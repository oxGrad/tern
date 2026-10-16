//! `tern status`: GitHub App ping and the last sync run.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::config::Config;
use crate::github::{self, auth::AppKey, client};

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    print!("{}", github_section(&config).await);
    println!("sync:");
    if !config.db_path.exists() {
        println!("  last run:             never - run: tern sync --once");
        return Ok(());
    }
    let conn = crate::db::open(&config.db_path)?;
    println!(
        "  last run:             {}",
        last_run(&conn, config.timezone)?
    );
    let (members, in_score): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(in_score), 0) FROM members WHERE is_bot = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    println!("  members tracked:      {members} ({in_score} in score)");
    Ok(())
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
    match client::check(gh.app_id, &gh.org, &key).await {
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
