//! `tern status`: GitHub App ping and the last sync run.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::config::Config;
use crate::github::{self, auth::AppKey, client};

pub async fn run() -> Result<()> {
    print!("{}", report(&Config::load()?).await?);
    Ok(())
}

/// The whole `tern status` output.
pub async fn report(config: &Config) -> Result<String> {
    let mut out = github_section(config).await;
    out.push_str("sync:\n");
    if !config.db_path.exists() {
        out.push_str("  last run:             never - run: tern sync --once\n");
        return Ok(out);
    }
    let conn = crate::db::open(&config.db_path)?;
    out.push_str(&format!(
        "  last run:             {}\n",
        last_run(&conn, config.timezone)?
    ));
    let (members, in_score): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(in_score), 0) FROM members
          WHERE is_bot = 0 AND (in_toml OR in_team)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    out.push_str(&format!(
        "  members tracked:      {members} ({in_score} in score)\n"
    ));
    Ok(out)
}

async fn github_section(config: &Config) -> String {
    let mut out = String::from("github:\n");
    let Some(gh) = &config.github else {
        out.push_str("  app:                  not configured - set [github] app_id, org and team in config.toml\n");
        return out;
    };
    out.push_str(&format!(
        "  app:                  {} on {}\n",
        gh.app_id, gh.org
    ));
    let path = std::env::var(github::KEY_PATH_ENV).unwrap_or_default();
    let Some(pem) = github::app_key_pem() else {
        out.push_str(&format!(
            "  key:                  not set - run: export {}=~/.config/tern/app.pem\n",
            github::KEY_PATH_ENV
        ));
        return out;
    };
    let key = match AppKey::from_pem(&pem) {
        Ok(k) => k,
        Err(e) => {
            out.push_str(&format!(
                "  key:                  failed - {e:#} ({path})\n"
            ));
            return out;
        }
    };
    out.push_str(&format!("  key:                  ok ({path})\n"));
    match client::check(&gh.api_url, gh.app_id, &gh.org, &key).await {
        Ok(c) => out.push_str(&format!(
            "  api:                  ok - {:?} installed ({} repositories), {} repo{} visible, {}ms\n",
            c.app.name,
            c.installation.repository_selection,
            c.repo_count,
            if c.repo_count == 1 { "" } else { "s" },
            c.latency_ms
        )),
        Err(e) => out.push_str(&format!("  api:                  failed - {e:#}\n")),
    }
    out
}

pub fn last_run(conn: &Connection, tz: chrono_tz::Tz) -> Result<String> {
    type Run = (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    let run: Option<Run> = conn
        .query_row(
            "SELECT started_at, status, requests_used, rate_remaining, error
               FROM sync_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((at, status, requests, remaining, error)) = run else {
        return Ok("never - run: tern sync --once".into());
    };
    let at = chrono::DateTime::parse_from_rfc3339(&at)
        .map(|d| d.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or(at);
    Ok(match status.as_deref() {
        Some("ok") => format!(
            "ok - {at}, {} requests, {} remaining",
            requests.unwrap_or(0),
            remaining.map_or("?".into(), |r| r.to_string())
        ),
        Some(s) => format!(
            "{s} - {at}: {} - run: tern sync --once",
            error.unwrap_or_default()
        ),
        None => format!("running since {at}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GithubConfig;
    use crate::mock_github;

    fn config(api_url: &str, org: &str) -> Config {
        Config {
            db_path: tempfile::tempdir().unwrap().keep().join("tern.db"),
            github: Some(GithubConfig {
                app_id: 1,
                org: org.into(),
                team: "t".into(),
                repos: vec![],
                sync_interval_minutes: 15,
                api_url: api_url.into(),
            }),
            ..Config::default()
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the env lock must span the whole report
    async fn report_pings_the_app_and_summarises_the_last_run() {
        let _env = crate::test_support::env_lock();
        // SAFETY: held under ENV_LOCK; removed before returning.
        unsafe { std::env::set_var(github::KEY_PATH_ENV, mock_github::KEY_PATH) };
        let mock = mock_github::start().await;
        let config = config(&mock.url, "o");

        let out = report(&config).await.unwrap();
        assert!(
            out.contains("api:                  ok - \"tern test\" installed (selected repositories), 2 repos visible"),
            "{out}"
        );
        assert!(
            out.contains("last run:             never - run: tern sync --once"),
            "{out}"
        );

        let conn = crate::db::open(&config.db_path).unwrap();
        conn.execute_batch(
            "INSERT INTO members (github_user, name, in_score, in_toml) VALUES
                 ('a', 'A', 1, 1), ('d', 'D', 0, 1), ('gone', 'G', 1, 0);
             INSERT INTO sync_runs (started_at, status, requests_used, rate_remaining)
                 VALUES ('2026-10-16T07:15:00Z', 'ok', 23, 4812);",
        )
        .unwrap();
        let out = report(&config).await.unwrap();
        assert!(
            out.contains("ok - 2026-10-16 14:15, 23 requests, 4812 remaining"),
            "{out}"
        );
        assert!(
            out.contains("members tracked:      2 (1 in score)"),
            "{out}"
        );

        conn.execute(
            "INSERT INTO sync_runs (started_at, status, error) VALUES ('2026-10-16T08:00:00Z', 'partial', 'boom')",
            [],
        )
        .unwrap();
        let out = report(&config).await.unwrap();
        assert!(
            out.contains("partial - 2026-10-16 15:00: boom - run: tern sync --once"),
            "{out}"
        );

        // The App is not installed on that org: the 404 is explained.
        let out = report(&super::tests::config(&mock.url, "x")).await.unwrap();
        assert!(
            out.contains("api:                  failed - the App is not installed"),
            "{out}"
        );

        unsafe { std::env::remove_var(github::KEY_PATH_ENV) };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn report_says_how_to_fix_missing_config_and_keys() {
        let _env = crate::test_support::env_lock();
        let mut config = config("http://127.0.0.1:9", "o");

        unsafe { std::env::remove_var(github::KEY_PATH_ENV) };
        let out = report(&config).await.unwrap();
        assert!(
            out.contains("key:                  not set - run: export TERN_GITHUB_KEY_PATH"),
            "{out}"
        );

        let bad = config.db_path.with_file_name("bad.pem");
        std::fs::write(&bad, "not a key").unwrap();
        // SAFETY: held under ENV_LOCK; removed before returning.
        unsafe { std::env::set_var(github::KEY_PATH_ENV, &bad) };
        let out = report(&config).await.unwrap();
        assert!(
            out.contains("key:                  failed - not a PEM file"),
            "{out}"
        );
        unsafe { std::env::remove_var(github::KEY_PATH_ENV) };

        config.github = None;
        let out = report(&config).await.unwrap();
        assert!(
            out.contains("app:                  not configured"),
            "{out}"
        );
    }
}
