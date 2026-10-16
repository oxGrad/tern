//! Read-only queries behind the pages. Everything takes the shared filter `F`.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};

pub use crate::score::median;

use crate::config::Config;
use crate::score::{self, Input, Scored};
use crate::sync::rollup::local_midnight_utc;

/// The global bar: date range (local days, inclusive), repo, bots toggle.
#[derive(Debug, Clone)]
pub struct F {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub repo: Option<String>,
    pub repo_id: Option<i64>,
    pub bots: bool,
    pub tz: Tz,
}

impl F {
    pub fn start(&self) -> String {
        local_midnight_utc(self.from, self.tz)
    }
    pub fn end(&self) -> String {
        local_midnight_utc(self.to + Duration::days(1), self.tz)
    }
    pub fn days(&self) -> u32 {
        ((self.to - self.from).num_days() + 1).max(1) as u32
    }
    /// The same-length period just before this one.
    pub fn prev(&self) -> F {
        let len = Duration::days(self.days() as i64);
        F {
            from: self.from - len,
            to: self.to - len,
            ..self.clone()
        }
    }
    pub fn with_range(&self, from: NaiveDate, to: NaiveDate) -> F {
        F {
            from,
            to,
            ..self.clone()
        }
    }
    /// The filter as a query string, for links that keep the current view.
    pub fn qs(&self) -> String {
        let mut s = format!("from={}&to={}", self.from, self.to);
        if let Some(r) = &self.repo {
            s.push_str(&format!("&repo={}", enc(r)));
        }
        if self.bots {
            s.push_str("&bots=1");
        }
        s
    }
}

/// Minimal query-value encoding (logins, repo names, dates, plain text).
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn parse_utc(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

pub fn local(ts: &str, tz: Tz) -> String {
    parse_utc(ts).map_or_else(String::new, |d| {
        d.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
    })
}

pub fn hours_between(a: &str, b: &str) -> Option<f64> {
    Some((parse_utc(b)? - parse_utc(a)?).num_minutes() as f64 / 60.0)
}

/// Monday of the week `d` falls in.
pub fn week_of(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

pub fn weeks(f: &F) -> Vec<NaiveDate> {
    let mut out = vec![];
    let mut w = week_of(f.from);
    while w <= f.to {
        out.push(w);
        w += Duration::days(7);
    }
    out
}

pub fn repo_id(conn: &Connection, full_name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM repos WHERE full_name = ?1",
            [full_name],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn repo_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT full_name FROM repos WHERE archived = 0 ORDER BY full_name")?;
    Ok(stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// The top bar's sync status: "Synced 14:15", "Sync partial Oct 15 09:00", ...
pub fn sync_brief(conn: &Connection, tz: Tz) -> Result<String> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT started_at, status FROM sync_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((at, status)) = row else {
        return Ok("Never synced".into());
    };
    let when = parse_utc(&at).map_or_else(String::new, |d| {
        let d = d.with_timezone(&tz);
        if d.date_naive() == Utc::now().with_timezone(&tz).date_naive() {
            d.format("%H:%M").to_string()
        } else {
            d.format("%b %-d %H:%M").to_string()
        }
    });
    Ok(match status.as_deref() {
        Some("ok") => format!("Synced {when}"),
        Some(s) => format!("Sync {s} {when}"),
        None => format!("Syncing since {when}"),
    })
}

pub fn last_sync(conn: &Connection, tz: Tz) -> Result<String> {
    Ok(format!("last sync: {}", crate::status::last_run(conn, tz)?))
}

pub struct Member {
    pub login: String,
    pub name: String,
    pub in_score: bool,
    pub is_bot: bool,
    /// In members.toml or on the team right now.
    pub active: bool,
}

pub fn members(conn: &Connection, bots: bool) -> Result<Vec<Member>> {
    let mut stmt = conn.prepare(
        "SELECT github_user, name, in_score, is_bot, in_toml OR in_team FROM members
         WHERE ?1 OR is_bot = 0 ORDER BY lower(name)",
    )?;
    Ok(stmt
        .query_map([bots], |r| {
            Ok(Member {
                login: r.get(0)?,
                name: r.get(1)?,
                in_score: r.get(2)?,
                is_bot: r.get(3)?,
                active: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// One row of the `activity` view, its timestamp converted to local time.
pub struct Ev {
    pub user: String,
    pub kind: String,
    pub at: DateTime<Tz>,
}

pub fn events(conn: &Connection, f: &F) -> Result<Vec<Ev>> {
    let mut stmt = conn.prepare(
        "SELECT a.github_user, a.kind, a.ts FROM activity a
           JOIN members m ON m.github_user = a.github_user
          WHERE a.ts >= ?1 AND a.ts < ?2 AND (?3 IS NULL OR a.repo_id = ?3)
            AND (?4 OR m.is_bot = 0)",
    )?;
    let rows = stmt.query_map(params![f.start(), f.end(), f.repo_id, f.bots], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (user, kind, ts) = row?;
        if let Some(d) = parse_utc(&ts) {
            out.push(Ev {
                user,
                kind,
                at: d.with_timezone(&f.tz),
            });
        }
    }
    Ok(out)
}

/// Per-member totals for the period, from the rollup (no repo dimension).
pub struct Row {
    pub login: String,
    pub name: String,
    pub in_score: bool,
    pub active: bool,
    /// commits, prs_opened, prs_merged, reviews_given, review_comments,
    /// conv_comments, lines_changed
    pub sums: [i64; 7],
    pub working_days: u32,
    pub score: Option<Scored>,
}

impl Row {
    /// Counts in score order: prs_merged, reviews_given, review_comments,
    /// prs_opened, commits, conv_comments.
    pub fn metric(&self, k: usize) -> i64 {
        self.sums[[2, 3, 4, 1, 0, 5][k]]
    }
}

pub fn rows(conn: &Connection, cfg: &Config, f: &F) -> Result<Vec<Row>> {
    let mut sums: HashMap<String, [i64; 7]> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT github_user, SUM(commits), SUM(prs_opened), SUM(prs_merged), SUM(reviews_given),
                SUM(review_comments), SUM(conv_comments), SUM(lines_changed)
           FROM daily_member_stats WHERE date BETWEEN ?1 AND ?2 GROUP BY github_user",
    )?;
    let q = stmt.query_map([f.from.to_string(), f.to.to_string()], |r| {
        let mut v = [0i64; 7];
        for (i, x) in v.iter_mut().enumerate() {
            *x = r.get(i + 1)?;
        }
        Ok((r.get::<_, String>(0)?, v))
    })?;
    for row in q {
        let (u, v) = row?;
        sums.insert(u, v);
    }
    let mut away: HashMap<String, Vec<NaiveDate>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT github_user, date FROM away_days WHERE date BETWEEN ?1 AND ?2")?;
    let q = stmt.query_map([f.from.to_string(), f.to.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in q {
        let (u, d) = row?;
        if let Ok(d) = NaiveDate::parse_from_str(&d, "%Y-%m-%d") {
            away.entry(u).or_default().push(d);
        }
    }
    let days = score::weekdays(f.from, f.to);
    let mut out: Vec<Row> = members(conn, f.bots)?
        .into_iter()
        .map(|m| {
            let away = away.get(&m.login).cloned().unwrap_or_default();
            Row {
                sums: sums.get(&m.login).copied().unwrap_or_default(),
                working_days: days.iter().filter(|d| !away.contains(d)).count() as u32,
                in_score: m.in_score && !m.is_bot && m.active,
                active: m.active,
                login: m.login,
                name: m.name,
                score: None,
            }
        })
        .collect();
    let inputs: Vec<Input> = out
        .iter()
        .filter(|r| r.in_score)
        .map(|r| Input {
            user: r.login.clone(),
            counts: std::array::from_fn(|k| r.metric(k) as f64),
            working_days: r.working_days,
        })
        .collect();
    let scored = score::score(&inputs, days.len() as u32, &cfg.score);
    for r in &mut out {
        r.score = scored.iter().find(|s| s.user == r.login).cloned();
    }
    Ok(out)
}

pub struct Pr {
    pub number: i64,
    pub repo: String,
    pub author: String,
    pub title: String,
    pub created_at: String,
    pub merged_at: Option<String>,
    pub first_review_at: Option<String>,
    pub size: i64,
    pub reviews: i64,
}

/// PRs matching `cond` (an SQL fragment over `p`), newest first.
pub fn prs(conn: &Connection, f: &F, cond: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Pr>> {
    let sql = format!(
        "SELECT p.number, r.full_name, p.author, COALESCE(p.title, ''), p.created_at, p.merged_at,
                p.first_review_at, COALESCE(p.additions, 0) + COALESCE(p.deletions, 0),
                (SELECT COUNT(*) FROM reviews v WHERE v.repo_id = p.repo_id AND v.pr_number = p.number)
           FROM pull_requests p JOIN repos r ON r.id = p.repo_id
           LEFT JOIN members m ON m.github_user = p.author
          WHERE (?1 IS NULL OR p.repo_id = ?1) AND (?2 OR COALESCE(m.is_bot, 0) = 0) AND ({cond})
          ORDER BY p.created_at DESC"
    );
    let mut all: Vec<&dyn rusqlite::ToSql> = vec![&f.repo_id, &f.bots];
    all.extend_from_slice(args);
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_map(all.as_slice(), |r| {
            Ok(Pr {
                number: r.get(0)?,
                repo: r.get(1)?,
                author: r.get(2)?,
                title: r.get(3)?,
                created_at: r.get(4)?,
                merged_at: r.get(5)?,
                first_review_at: r.get(6)?,
                size: r.get(7)?,
                reviews: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn merged_prs(conn: &Connection, f: &F) -> Result<Vec<Pr>> {
    let (s, e) = (f.start(), f.end());
    prs(conn, f, "p.merged_at >= ?3 AND p.merged_at < ?4", &[&s, &e])
}

pub fn reviews_count(conn: &Connection, f: &F) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM reviews v LEFT JOIN members m ON m.github_user = v.reviewer
          WHERE v.submitted_at >= ?1 AND v.submitted_at < ?2
            AND (?3 IS NULL OR v.repo_id = ?3) AND (?4 OR COALESCE(m.is_bot, 0) = 0)",
        params![f.start(), f.end(), f.repo_id, f.bots],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_who_left_are_not_scored_or_in_the_median() {
        let conn = crate::db::open(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        // c left: in neither members.toml nor the team, with zero activity.
        conn.execute_batch(
            "INSERT INTO members (github_user, name, in_toml, in_team) VALUES
                 ('a', 'A', 1, 0), ('b', 'B', 0, 1), ('c', 'C', 0, 0);
             INSERT INTO daily_member_stats (github_user, date, prs_merged) VALUES
                 ('a', '2026-10-14', 2), ('b', '2026-10-14', 4);",
        )
        .unwrap();
        let d = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let f = F {
            from: d("2026-10-12"),
            to: d("2026-10-16"),
            repo: None,
            repo_id: None,
            bots: false,
            tz: chrono_tz::Asia::Jakarta,
        };
        let rows = rows(&conn, &crate::config::Config::default(), &f).unwrap();
        let get = |u: &str| rows.iter().find(|r| r.login == u).unwrap();
        assert!(get("c").score.is_none());
        // Median of a and b is 3; with c's zero it would be 2.
        let part = |u: &str| get(u).score.as_ref().unwrap().parts[0];
        let full = 50.0 * 3.0 / 9.0; // weight 3 of 9 total, at the median
        assert!((part("a") - full * 2.0 / 3.0).abs() < 1e-9);
        assert!((part("b") - full * 4.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn enc_escapes_slashes_and_spaces() {
        assert_eq!(enc("org/repo x"), "org%2Frepo%20x");
    }

    #[test]
    fn prev_period_has_the_same_length() {
        let d = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let f = F {
            from: d("2026-10-03"),
            to: d("2026-10-16"),
            repo: None,
            repo_id: None,
            bots: false,
            tz: chrono_tz::Asia::Jakarta,
        };
        let p = f.prev();
        assert_eq!((p.from, p.to), (d("2026-09-19"), d("2026-10-02")));
        assert_eq!(weeks(&f).len(), 3);
    }
}
