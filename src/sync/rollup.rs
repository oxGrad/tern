//! Rebuilds `daily_member_stats` for a range of local days from the raw tables.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;

const TS: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Column order of `daily_member_stats` after (github_user, date).
const COLS: [&str; 7] = [
    "commits",
    "prs_opened",
    "prs_merged",
    "reviews_given",
    "review_comments",
    "conv_comments",
    "lines_changed",
];

/// One row per counted event: (user, column, utc ts, amount).
const EVENTS: &str = "
    SELECT author_login, 'commits', authored_at, 1 FROM commits
     WHERE author_login IS NOT NULL AND authored_at >= ?1 AND authored_at < ?2
    UNION ALL SELECT author, 'prs_opened', created_at, 1 FROM pull_requests
     WHERE draft = 0 AND created_at >= ?1 AND created_at < ?2
    UNION ALL SELECT author, 'prs_merged', merged_at, 1 FROM pull_requests
     WHERE merged_at >= ?1 AND merged_at < ?2
    UNION ALL SELECT author, 'lines_changed', merged_at,
                     COALESCE(additions, 0) + COALESCE(deletions, 0) FROM pull_requests
     WHERE merged_at >= ?1 AND merged_at < ?2
    UNION ALL SELECT reviewer, 'reviews_given', submitted_at, 1 FROM reviews
     WHERE submitted_at >= ?1 AND submitted_at < ?2
    UNION ALL SELECT c.author, 'review_comments', c.created_at, 1 FROM comments c
      JOIN pull_requests p ON p.repo_id = c.repo_id AND p.number = c.pr_number
     WHERE c.kind = 'review' AND c.author <> p.author
       AND c.created_at >= ?1 AND c.created_at < ?2
    UNION ALL SELECT author, 'conv_comments', created_at, 1 FROM comments
     WHERE kind = 'conversation' AND created_at >= ?1 AND created_at < ?2";

pub fn local_date(ts: &str, tz: Tz) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&tz).date_naive())
}

/// The instant local midnight starts `date`.
pub fn local_midnight(date: NaiveDate, tz: Tz) -> DateTime<Utc> {
    let midnight = date.and_hms_opt(0, 0, 0).unwrap();
    tz.from_local_datetime(&midnight)
        .earliest()
        .map_or_else(|| midnight.and_utc(), |d| d.with_timezone(&Utc))
}

/// UTC timestamp string of local midnight starting `date`.
pub fn local_midnight_utc(date: NaiveDate, tz: Tz) -> String {
    local_midnight(date, tz).format(TS).to_string()
}

/// Rebuilds every day that has raw activity, so roster, email and timezone
/// changes reach history the incremental rebuilds never touch again.
/// ponytail: full rebuild on every start; track a roster hash if startup gets slow.
pub fn rebuild_all(conn: &Connection, tz: Tz) -> Result<usize> {
    let (lo, hi): (Option<String>, Option<String>) =
        conn.query_row("SELECT MIN(ts), MAX(ts) FROM activity", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    match (lo, hi) {
        (Some(lo), Some(hi)) => rebuild(conn, tz, &lo, &hi),
        _ => Ok(0),
    }
}

/// Recomputes every (member, day) whose local day falls between the local days
/// of `lo_ts` and `hi_ts` (UTC timestamps), inclusive.
pub fn rebuild(conn: &Connection, tz: Tz, lo_ts: &str, hi_ts: &str) -> Result<usize> {
    let (Some(lo), Some(hi)) = (local_date(lo_ts, tz), local_date(hi_ts, tz)) else {
        return Ok(0);
    };
    rebuild_days(conn, tz, lo, hi)
}

pub fn rebuild_days(conn: &Connection, tz: Tz, lo: NaiveDate, hi: NaiveDate) -> Result<usize> {
    let start = local_midnight_utc(lo, tz);
    let end = local_midnight_utc(hi + Duration::days(1), tz);
    let members: HashSet<String> = conn
        .prepare("SELECT github_user FROM members")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut acc: HashMap<(String, NaiveDate), [i64; 7]> = HashMap::new();
    let mut stmt = conn.prepare(EVENTS)?;
    let rows = stmt.query_map([&start, &end], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (user, col, ts, n) = row?;
        let (Some(date), Some(i)) = (local_date(&ts, tz), COLS.iter().position(|c| *c == col))
        else {
            continue;
        };
        if members.contains(&user) {
            acc.entry((user, date)).or_default()[i] += n;
        }
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM daily_member_stats WHERE date BETWEEN ?1 AND ?2",
        [lo.to_string(), hi.to_string()],
    )?;
    for ((user, date), v) in &acc {
        tx.execute(
            "INSERT INTO daily_member_stats (github_user, date, commits, prs_opened, prs_merged,
                 reviews_given, review_comments, conv_comments, lines_changed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                user,
                date.to_string(),
                v[0],
                v[1],
                v[2],
                v[3],
                v[4],
                v[5],
                v[6]
            ],
        )?;
    }
    tx.commit()?;
    Ok(acc.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_local_day_and_skips_self_review_comments() {
        let c = crate::db::open(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let tz: Tz = "Asia/Jakarta".parse().unwrap();
        c.execute_batch(
            "INSERT INTO members (github_user, name) VALUES ('a', 'A'), ('b', 'B');
             INSERT INTO repos (id, full_name, default_branch) VALUES (1, 'o/r', 'main');
             INSERT INTO pull_requests (id, repo_id, number, author, state, created_at,
                 merged_at, updated_at, additions, deletions)
             VALUES (1, 1, 7, 'a', 'closed', '2026-10-01T18:00:00Z', '2026-10-02T03:00:00Z',
                     '2026-10-02T03:00:00Z', 10, 5);
             INSERT INTO comments (id, repo_id, pr_number, author, kind, created_at) VALUES
                 (1, 1, 7, 'a', 'review', '2026-10-02T01:00:00Z'),
                 (2, 1, 7, 'b', 'review', '2026-10-02T01:00:00Z'),
                 (3, 1, 7, 'stranger', 'conversation', '2026-10-02T01:00:00Z');",
        )
        .unwrap();
        rebuild(&c, tz, "2026-10-01T18:00:00Z", "2026-10-02T03:00:00Z").unwrap();
        let get = |user: &str, date: &str| -> (i64, i64, i64, i64) {
            c.query_row(
                "SELECT prs_opened, prs_merged, review_comments, lines_changed
                 FROM daily_member_stats WHERE github_user = ?1 AND date = ?2",
                [user, date],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        // 18:00Z on Oct 1 is 01:00 on Oct 2 in Jakarta.
        assert_eq!(get("a", "2026-10-02"), (1, 1, 0, 15));
        assert_eq!(get("b", "2026-10-02"), (0, 0, 1, 0));
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM daily_member_stats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }
}
