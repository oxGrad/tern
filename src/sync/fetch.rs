//! Fetch phase of a sync: talks to GitHub, touches no database. One repo's
//! worth of changes comes back as `RepoData`, which `sync::store` then writes
//! in a single transaction together with the advanced cursors.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::github::client::Api;

#[derive(Debug, Default)]
pub struct RepoData {
    pub prs: Vec<Pr>,
    pub reviews: Vec<Review>,
    pub comments: Vec<Comment>,
    pub commits: Vec<Commit>,
    /// (entity, new cursor, new etag); `None` keeps the stored value.
    pub state: Vec<(&'static str, Option<String>, Option<String>)>,
    pub bots: BTreeSet<String>,
}

#[derive(Debug)]
pub struct Pr {
    pub id: i64,
    pub number: i64,
    pub author: String,
    pub title: Option<String>,
    pub state: String,
    pub draft: bool,
    pub created_at: String,
    pub first_review_at: Option<String>,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub updated_at: String,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
}

#[derive(Debug)]
pub struct Review {
    pub id: i64,
    pub pr_number: i64,
    pub reviewer: String,
    pub state: String,
    pub submitted_at: String,
}

#[derive(Debug)]
pub struct Comment {
    pub id: i64,
    pub pr_number: i64,
    pub author: String,
    pub kind: &'static str,
    pub body_len: i64,
    pub created_at: String,
}

#[derive(Debug)]
pub struct Commit {
    pub sha: String,
    pub login: Option<String>,
    pub email: Option<String>,
    pub authored_at: String,
    pub committed_at: String,
    pub pr_number: Option<i64>,
}

/// What `store` already knows about a repo before fetching.
pub struct Known {
    /// entity -> (cursor_at, etag)
    pub state: HashMap<String, (Option<String>, Option<String>)>,
    /// lowercase commit email -> github login
    pub emails: HashMap<String, String>,
}

const TS: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Cursor minus the 5 minute overlap that absorbs clock skew, or `fallback`.
pub fn since_of(cursor: Option<&str>, fallback: DateTime<Utc>) -> String {
    cursor
        .and_then(|c| DateTime::parse_from_rfc3339(c).ok())
        .map_or(fallback, |c| c.with_timezone(&Utc) - Duration::minutes(5))
        .format(TS)
        .to_string()
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)?.as_str().map(String::from)
}

/// A GitHub login, lowercased: logins are case-insensitive, and members.toml
/// may spell one differently from the API.
fn login(v: &Value, ptr: &str) -> Option<String> {
    s(v, ptr).map(|l| l.to_ascii_lowercase())
}

fn n(v: &Value, ptr: &str) -> Option<i64> {
    v.pointer(ptr)?.as_i64()
}

fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]")
}

fn note(bots: &mut BTreeSet<String>, login: &str) {
    if is_bot(login) {
        bots.insert(login.to_string());
    }
}

/// Trailing number of a URL such as `.../pulls/12`.
fn last_number(url: &str) -> Option<i64> {
    url.rsplit('/').next()?.parse().ok()
}

fn newest(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

pub async fn repo(
    api: &mut Api,
    full: &str,
    branch: &str,
    known: &Known,
    fallback: DateTime<Utc>,
) -> Result<RepoData> {
    let mut data = RepoData::default();
    let cur = |entity: &str| known.state.get(entity).cloned().unwrap_or_default();
    let mut merge_shas = HashSet::new();

    // Pull requests, newest update first; stop at the first one older than the cursor.
    let (cursor, etag) = cur("pulls");
    let since = since_of(cursor.as_deref(), fallback);
    let path = format!("/repos/{full}/pulls?state=all&sort=updated&direction=desc&per_page=100");
    if let Some((items, new_etag)) = api
        .list(&path, etag.as_deref(), |p| {
            s(p, "/updated_at").is_none_or(|u| u < since)
        })
        .await?
    {
        let newest_seen = items.iter().filter_map(|p| s(p, "/updated_at")).max();
        for item in &items {
            let Some(number) = n(item, "/number") else {
                continue;
            };
            if let Some(sha) = s(item, "/merge_commit_sha") {
                merge_shas.insert(sha);
            }
            pr(api, full, number, known, &mut data).await?;
        }
        data.state.push(("pulls", newest_seen, new_etag));
    }

    // Inline review comments, repo-wide.
    let (cursor, etag) = cur("review_comments");
    let since = since_of(cursor.as_deref(), fallback);
    let path = format!("/repos/{full}/pulls/comments?since={since}&per_page=100");
    if let Some((items, new_etag)) = api.list(&path, etag.as_deref(), |_| false).await? {
        let mut newest_seen = None;
        for c in &items {
            newest_seen = newest(newest_seen, s(c, "/updated_at"));
            let (Some(id), Some(author), Some(created_at)) =
                (n(c, "/id"), login(c, "/user/login"), s(c, "/created_at"))
            else {
                continue;
            };
            let Some(pr_number) = s(c, "/pull_request_url").as_deref().and_then(last_number) else {
                continue;
            };
            note(&mut data.bots, &author);
            data.comments.push(Comment {
                id,
                pr_number,
                author,
                kind: "review",
                body_len: s(c, "/body").map_or(0, |b| b.len() as i64),
                created_at,
            });
        }
        data.state.push(("review_comments", newest_seen, new_etag));
    }

    // Conversation comments, repo-wide; keep only those on PRs.
    let (cursor, etag) = cur("issue_comments");
    let since = since_of(cursor.as_deref(), fallback);
    let path = format!("/repos/{full}/issues/comments?since={since}&per_page=100");
    if let Some((items, new_etag)) = api.list(&path, etag.as_deref(), |_| false).await? {
        let mut newest_seen = None;
        for c in &items {
            newest_seen = newest(newest_seen, s(c, "/updated_at"));
            if !s(c, "/html_url").is_some_and(|u| u.contains("/pull/")) {
                continue;
            }
            let (Some(id), Some(author), Some(created_at)) =
                (n(c, "/id"), login(c, "/user/login"), s(c, "/created_at"))
            else {
                continue;
            };
            let Some(pr_number) = s(c, "/issue_url").as_deref().and_then(last_number) else {
                continue;
            };
            note(&mut data.bots, &author);
            data.comments.push(Comment {
                id,
                pr_number,
                author,
                kind: "conversation",
                body_len: s(c, "/body").map_or(0, |b| b.len() as i64),
                created_at,
            });
        }
        data.state.push(("issue_comments", newest_seen, new_etag));
    }

    // Default-branch commits (direct pushes). A squash merge's commit is the
    // PR's `merge_commit_sha`; skip it so the work isn't counted twice.
    // ponytail: only skips merges seen in this run's PR window.
    let (cursor, etag) = cur("commits");
    let since = since_of(cursor.as_deref(), fallback);
    let path = format!("/repos/{full}/commits?sha={branch}&since={since}&per_page=100");
    if let Some((items, new_etag)) = api.list(&path, etag.as_deref(), |_| false).await? {
        let mut newest_seen = None;
        for c in &items {
            newest_seen = newest(newest_seen, s(c, "/commit/committer/date"));
            if s(c, "/sha").is_some_and(|sha| merge_shas.contains(&sha)) {
                continue;
            }
            commit(c, None, known, &mut data);
        }
        data.state.push(("commits", newest_seen, new_etag));
    }
    Ok(data)
}

/// One touched PR: its details, reviews and commits.
async fn pr(
    api: &mut Api,
    full: &str,
    number: i64,
    known: &Known,
    data: &mut RepoData,
) -> Result<()> {
    let crate::github::client::Page::Body { json: d, .. } = api
        .get(&format!("/repos/{full}/pulls/{number}"), None)
        .await?
    else {
        return Ok(());
    };
    let (Some(id), Some(author), Some(created_at), Some(updated_at)) = (
        n(&d, "/id"),
        login(&d, "/user/login"),
        s(&d, "/created_at"),
        s(&d, "/updated_at"),
    ) else {
        return Ok(());
    };
    note(&mut data.bots, &author);

    let mut first_review_at: Option<String> = None;
    if let Some((reviews, _)) = api
        .list(
            &format!("/repos/{full}/pulls/{number}/reviews?per_page=100"),
            None,
            |_| false,
        )
        .await?
    {
        for r in &reviews {
            // Pending reviews have no submitted_at; self-reviews don't count.
            let (Some(rid), Some(reviewer), Some(at)) =
                (n(r, "/id"), login(r, "/user/login"), s(r, "/submitted_at"))
            else {
                continue;
            };
            if reviewer == author {
                continue;
            }
            note(&mut data.bots, &reviewer);
            first_review_at = Some(first_review_at.map_or(at.clone(), |f| f.min(at.clone())));
            if !counts_as_review(r) {
                continue;
            }
            data.reviews.push(Review {
                id: rid,
                pr_number: number,
                reviewer,
                state: s(r, "/state").unwrap_or_default(),
                submitted_at: at,
            });
        }
    }
    if let Some((commits, _)) = api
        .list(
            &format!("/repos/{full}/pulls/{number}/commits?per_page=100"),
            None,
            |_| false,
        )
        .await?
    {
        for c in &commits {
            commit(c, Some(number), known, data);
        }
    }
    data.prs.push(Pr {
        id,
        number,
        author,
        title: s(&d, "/title"),
        state: s(&d, "/state").unwrap_or_else(|| "open".into()),
        draft: d["draft"].as_bool().unwrap_or(false),
        created_at,
        first_review_at,
        merged_at: s(&d, "/merged_at"),
        closed_at: s(&d, "/closed_at"),
        updated_at,
        additions: n(&d, "/additions"),
        deletions: n(&d, "/deletions"),
        changed_files: n(&d, "/changed_files"),
    });
    Ok(())
}

/// A reply in an inline-comment thread is stored by GitHub as its own
/// COMMENTED review with an empty body. It counts as a review comment (via
/// `pulls/comments`), not as another review; a COMMENTED review with a summary
/// body, an approval or a change request does count. It still sets
/// `first_review_at`, since it is feedback the author received.
fn counts_as_review(r: &Value) -> bool {
    s(r, "/state").as_deref() != Some("COMMENTED")
        || s(r, "/body").is_some_and(|b| !b.trim().is_empty())
}

fn commit(c: &Value, pr_number: Option<i64>, known: &Known, data: &mut RepoData) {
    let (Some(sha), Some(authored_at), Some(committed_at)) = (
        s(c, "/sha"),
        s(c, "/commit/author/date"),
        s(c, "/commit/committer/date"),
    ) else {
        return;
    };
    let email = s(c, "/commit/author/email");
    // `author.login` is null for an unlinked email; fall back to members.toml.
    let login = login(c, "/author/login").or_else(|| {
        known
            .emails
            .get(&email.as_deref()?.to_ascii_lowercase())
            .cloned()
    });
    if let Some(l) = &login {
        note(&mut data.bots, l);
    }
    data.commits.push(Commit {
        sha,
        login,
        email,
        authored_at,
        committed_at,
        pr_number,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_the_overlap_or_uses_the_fallback() {
        let fb = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .into();
        assert_eq!(
            since_of(Some("2026-10-16T07:10:00Z"), fb),
            "2026-10-16T07:05:00Z"
        );
        assert_eq!(since_of(None, fb), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn thread_replies_are_not_reviews() {
        let review = |state: &str, body: &str| serde_json::json!({"state": state, "body": body});
        assert!(!counts_as_review(&review("COMMENTED", "")));
        assert!(!counts_as_review(&review("COMMENTED", "  \n")));
        assert!(counts_as_review(&review(
            "COMMENTED",
            "Looks close, two nits"
        )));
        assert!(counts_as_review(&review("APPROVED", "")));
        assert!(counts_as_review(&review("CHANGES_REQUESTED", "")));
    }

    #[test]
    fn commit_login_falls_back_to_the_email_map() {
        let known = Known {
            state: HashMap::new(),
            emails: HashMap::from([("me@x.dev".to_string(), "budi".to_string())]),
        };
        let c = serde_json::json!({"sha": "a", "author": null,
            "commit": {"author": {"email": "Me@X.dev", "date": "2026-01-01T00:00:00Z"},
                       "committer": {"date": "2026-01-01T00:00:00Z"}}});
        let mut data = RepoData::default();
        commit(&c, Some(3), &known, &mut data);
        assert_eq!(data.commits[0].login.as_deref(), Some("budi"));
        assert_eq!(data.commits[0].pr_number, Some(3));
    }
}
