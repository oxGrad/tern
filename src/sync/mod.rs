//! One sync run: refresh members and repos, fetch each repo's changes since
//! its cursors, write them (and the advanced cursors) per repo in one
//! transaction, rebuild the touched rollup days, log the run.

mod fetch;
pub mod rollup;

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, Utc};
use rusqlite::{Connection, params};

use crate::config::Config;
use crate::github::auth::AppKey;
use crate::github::client::{Api, RateLimited};
use fetch::{Known, RepoData};

/// Without a cursor or `--since`, a first sync looks back this far.
const DEFAULT_LOOKBACK_DAYS: i64 = 14;

#[derive(Debug)]
pub struct Summary {
    pub status: &'static str,
    pub repos: usize,
    pub requests: u32,
    pub remaining: Option<i64>,
}

fn now_ts() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Runs one sync and records it in `sync_runs`, whatever the outcome.
pub async fn run(
    db: &Mutex<Connection>,
    config: &Config,
    since: Option<NaiveDate>,
) -> Result<Summary> {
    let run_id = {
        let c = db.lock().unwrap();
        c.execute("INSERT INTO sync_runs (started_at) VALUES (?1)", [now_ts()])?;
        c.last_insert_rowid()
    };
    let mut api = None;
    let result = run_inner(db, config, since, &mut api).await;
    let (requests, remaining) = api
        .as_ref()
        .map_or((0, None), |a: &Api| (a.requests, a.remaining));
    let (status, error) = match &result {
        Ok((status, _)) => (*status, None),
        Err(e) => ("failed", Some(format!("{e:#}"))),
    };
    db.lock().unwrap().execute(
        "UPDATE sync_runs SET finished_at = ?1, requests_used = ?2, rate_remaining = ?3,
                              status = ?4, error = COALESCE(error, ?5)
         WHERE id = ?6",
        params![now_ts(), requests, remaining, status, error, run_id],
    )?;
    let (status, repos) = result?;
    Ok(Summary {
        status,
        repos,
        requests,
        remaining,
    })
}

async fn run_inner(
    db: &Mutex<Connection>,
    config: &Config,
    since: Option<NaiveDate>,
    api_slot: &mut Option<Api>,
) -> Result<(&'static str, usize)> {
    let Some(gh) = &config.github else {
        bail!("[github] is not configured - set app_id, org and team in config.toml");
    };
    let pem = crate::github::app_key_pem().with_context(|| {
        format!(
            "no App key - set {} to the .pem path",
            crate::github::KEY_PATH_ENV
        )
    })?;
    let key = AppKey::from_pem(&pem)?;
    let api = api_slot.insert(Api::connect(gh.app_id, &gh.org, key).await?);
    let fallback = since
        .map(|d| rollup::local_midnight(d, config.timezone))
        .unwrap_or_else(|| Utc::now() - Duration::days(DEFAULT_LOOKBACK_DAYS));

    // 1. Members (team endpoint) and repos: small, full refreshes.
    let team = api
        .list(
            &format!("/orgs/{}/teams/{}/members?per_page=100", gh.org, gh.team),
            None,
            |_| false,
        )
        .await?
        .map(|(v, _)| v)
        .unwrap_or_default();
    let repos = api
        .list("/installation/repositories?per_page=100", None, |_| false)
        .await?
        .map(|(v, _)| v)
        .unwrap_or_default();
    let repos: Vec<(i64, String, String)> = {
        let c = db.lock().unwrap();
        let tx = c.unchecked_transaction()?;
        // The list call succeeded, so it is the whole team: whoever is missing left.
        tx.execute("UPDATE members SET in_team = FALSE", [])?;
        for m in &team {
            let (Some(login), avatar) = (m["login"].as_str(), m["avatar_url"].as_str()) else {
                continue;
            };
            // Same lowercase key members.toml and fetch use.
            let login = login.to_ascii_lowercase();
            tx.execute(
                "INSERT INTO members (github_user, name, avatar_url, in_team) VALUES (?1, ?1, ?2, TRUE)
                 ON CONFLICT(github_user) DO UPDATE SET avatar_url = ?2, in_team = TRUE",
                params![login, avatar],
            )?;
        }
        let mut out = Vec::new();
        for r in &repos {
            let (Some(id), Some(full), Some(name)) = (
                r["id"].as_i64(),
                r["full_name"].as_str(),
                r["name"].as_str(),
            ) else {
                continue;
            };
            let archived = r["archived"].as_bool().unwrap_or(false);
            let branch = r["default_branch"].as_str().unwrap_or("main");
            tx.execute(
                "INSERT INTO repos (id, full_name, default_branch, archived, pushed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET full_name = ?2, default_branch = ?3,
                                               archived = ?4, pushed_at = ?5",
                params![id, full, branch, archived, r["pushed_at"].as_str()],
            )?;
            let wanted = gh.repos.is_empty() || gh.repos.iter().any(|w| w == full || w == name);
            if !archived && wanted {
                out.push((id, full.to_string(), branch.to_string()));
            }
        }
        tx.commit()?;
        out
    };

    // 2-6 per repo, 8 (cursors) inside `store`.
    let mut status = "ok";
    let mut touched: Option<(String, String)> = None;
    for (repo_id, full, branch) in &repos {
        let mut known = { known(&db.lock().unwrap(), *repo_id)? };
        // An explicit --since re-pulls from that date; stored cursors and
        // ETags would otherwise skip everything older than the last sync.
        if since.is_some() {
            known.state.clear();
        }
        let data = match fetch::repo(api, full, branch, &known, fallback).await {
            Ok(d) => d,
            Err(e) if e.is::<RateLimited>() => {
                eprintln!("failed to sync {full}: {e:#}");
                status = "partial";
                break;
            }
            Err(e) => {
                eprintln!("failed to sync {full}: {e:#}");
                status = "partial";
                continue;
            }
        };
        let range = { store(&db.lock().unwrap(), *repo_id, &data)? };
        touched = match (touched, range) {
            (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
            (a, b) => a.or(b),
        };
    }

    // 7. Rollup for every day this run touched.
    if let Some((lo, hi)) = touched {
        rollup::rebuild(&db.lock().unwrap(), config.timezone, &lo, &hi)?;
    }
    if status == "partial" {
        db.lock().unwrap().execute(
            "UPDATE sync_runs SET error = 'some repos failed or rate limit hit - see daemon log'
             WHERE id = (SELECT MAX(id) FROM sync_runs)",
            [],
        )?;
    }
    Ok((status, repos.len()))
}

fn known(c: &Connection, repo_id: i64) -> Result<Known> {
    let mut stmt =
        c.prepare("SELECT entity, cursor_at, etag FROM sync_state WHERE repo_id = ?1")?;
    let state = stmt
        .query_map([repo_id], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    let mut stmt = c.prepare("SELECT email, github_user FROM member_emails")?;
    let emails = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(Known { state, emails })
}

/// Writes one repo's changes and its advanced cursors atomically. Returns the
/// oldest and newest event timestamps written, for the rollup.
fn store(c: &Connection, repo_id: i64, d: &RepoData) -> Result<Option<(String, String)>> {
    let tx = c.unchecked_transaction()?;
    let mut ts: Vec<&str> = Vec::new();
    for bot in &d.bots {
        tx.execute(
            "INSERT INTO members (github_user, name, in_score, is_bot) VALUES (?1, ?1, 0, 1)
             ON CONFLICT(github_user) DO UPDATE SET is_bot = 1, in_score = 0",
            [bot],
        )?;
    }
    for p in &d.prs {
        tx.execute(
            "INSERT INTO pull_requests (id, repo_id, number, author, title, state, draft,
                 created_at, first_review_at, merged_at, closed_at, updated_at,
                 additions, deletions, changed_files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(id) DO UPDATE SET author = ?4, title = ?5, state = ?6, draft = ?7,
                 created_at = ?8, first_review_at = ?9, merged_at = ?10, closed_at = ?11,
                 updated_at = ?12, additions = ?13, deletions = ?14, changed_files = ?15",
            params![
                p.id,
                repo_id,
                p.number,
                p.author,
                p.title,
                p.state,
                p.draft,
                p.created_at,
                p.first_review_at,
                p.merged_at,
                p.closed_at,
                p.updated_at,
                p.additions,
                p.deletions,
                p.changed_files
            ],
        )?;
        ts.push(&p.created_at);
        ts.extend(p.merged_at.as_deref());
    }
    for r in &d.reviews {
        tx.execute(
            "INSERT INTO reviews (id, repo_id, pr_number, reviewer, state, submitted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET state = ?5, submitted_at = ?6",
            params![
                r.id,
                repo_id,
                r.pr_number,
                r.reviewer,
                r.state,
                r.submitted_at
            ],
        )?;
        ts.push(&r.submitted_at);
    }
    for m in &d.comments {
        tx.execute(
            "INSERT INTO comments (id, repo_id, pr_number, author, kind, body_len, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET body_len = ?6",
            params![
                m.id,
                repo_id,
                m.pr_number,
                m.author,
                m.kind,
                m.body_len,
                m.created_at
            ],
        )?;
        ts.push(&m.created_at);
    }
    for k in &d.commits {
        tx.execute(
            "INSERT INTO commits (sha, repo_id, author_login, author_email, authored_at,
                                  committed_at, pr_number)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(sha) DO UPDATE SET
                 author_login = COALESCE(excluded.author_login, author_login),
                 pr_number = COALESCE(excluded.pr_number, pr_number)",
            params![
                k.sha,
                repo_id,
                k.login,
                k.email,
                k.authored_at,
                k.committed_at,
                k.pr_number
            ],
        )?;
        ts.push(&k.authored_at);
    }
    for (entity, cursor, etag) in &d.state {
        tx.execute(
            "INSERT INTO sync_state (repo_id, entity, cursor_at, etag) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(repo_id, entity) DO UPDATE SET
                 cursor_at = COALESCE(?3, cursor_at), etag = COALESCE(?4, etag)",
            params![repo_id, entity, cursor, etag],
        )?;
    }
    tx.commit()?;
    let lo = ts.iter().min().map(|s| s.to_string());
    let hi = ts.iter().max().map(|s| s.to_string());
    Ok(lo.zip(hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fetch::{Commit, Pr, Review};

    #[test]
    fn store_is_idempotent_and_keeps_pr_links() {
        let c = crate::db::open(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        c.execute(
            "INSERT INTO repos (id, full_name, default_branch) VALUES (1, 'o/r', 'main')",
            [],
        )
        .unwrap();
        let data = RepoData {
            prs: vec![Pr {
                id: 10,
                number: 1,
                author: "a".into(),
                title: None,
                state: "closed".into(),
                draft: false,
                created_at: "2026-10-01T00:00:00Z".into(),
                first_review_at: None,
                merged_at: Some("2026-10-02T00:00:00Z".into()),
                closed_at: None,
                updated_at: "2026-10-02T00:00:00Z".into(),
                additions: Some(1),
                deletions: Some(2),
                changed_files: Some(1),
            }],
            reviews: vec![Review {
                id: 5,
                pr_number: 1,
                reviewer: "b".into(),
                state: "APPROVED".into(),
                submitted_at: "2026-10-01T05:00:00Z".into(),
            }],
            commits: vec![Commit {
                sha: "s".into(),
                login: Some("a".into()),
                email: None,
                authored_at: "2026-09-30T00:00:00Z".into(),
                committed_at: "2026-09-30T00:00:00Z".into(),
                pr_number: Some(1),
            }],
            state: vec![(
                "pulls",
                Some("2026-10-02T00:00:00Z".into()),
                Some("e1".into()),
            )],
            ..Default::default()
        };
        let range = store(&c, 1, &data).unwrap();
        assert_eq!(
            range,
            Some(("2026-09-30T00:00:00Z".into(), "2026-10-02T00:00:00Z".into()))
        );

        // A direct-push listing of the same sha must not drop its PR link,
        // and a 304 (no cursor, no etag) must keep the stored state.
        let again = RepoData {
            commits: vec![Commit {
                pr_number: None,
                ..data.commits.into_iter().next().unwrap()
            }],
            state: vec![("pulls", None, None)],
            ..Default::default()
        };
        store(&c, 1, &again).unwrap();
        store(&c, 1, &again).unwrap();
        let (n, pr): (i64, Option<i64>) = c
            .query_row("SELECT COUNT(*), MAX(pr_number) FROM commits", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((n, pr), (1, Some(1)));
        let etag: String = c
            .query_row("SELECT etag FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(etag, "e1");
    }
}
