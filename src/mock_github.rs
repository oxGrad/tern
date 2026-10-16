//! A canned GitHub REST API on 127.0.0.1 for tests: org `o`, team `t`, repo
//! `o/api` (plus an archived `o/old`) holding one merged PR, #7 by budi. Every
//! list endpoint answers 304 to its own ETag, and `rate_limited` turns the
//! pulls listing into a primary rate-limit 403.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Mock {
    pub url: String,
    /// Every request as "METHOD /path?query".
    pub hits: Arc<Mutex<Vec<String>>>,
    pub rate_limited: Arc<AtomicBool>,
}

impl Mock {
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.hits.lock().unwrap().clear();
    }
}

pub async fn start() -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        url: format!("http://{}", listener.local_addr().unwrap()),
        hits: Arc::default(),
        rate_limited: Arc::default(),
    };
    let app = Router::new().fallback(handle).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    mock
}

/// The test App key from `src/github/testdata`, for `TERN_GITHUB_KEY_PATH`.
pub const KEY_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/github/testdata/test_app_key.pem"
);

fn reply(status: StatusCode, body: Value, extra: &[(&'static str, String)]) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    let h = resp.headers_mut();
    h.insert("x-ratelimit-remaining", "4999".parse().unwrap());
    for (k, v) in extra {
        h.insert(*k, v.parse().unwrap());
    }
    resp
}

fn commit(sha: &str, login: Option<&str>, email: &str, date: &str) -> Value {
    json!({
        "sha": sha,
        "author": login.map(|l| json!({"login": l})),
        "commit": {"author": {"email": email, "date": date}, "committer": {"date": date}},
    })
}

async fn handle(State(m): State<Mock>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("");
    m.hits
        .lock()
        .unwrap()
        .push(format!("{method} {path}?{query}"));
    let ok = |body: Value| reply(StatusCode::OK, body, &[]);

    // Conditional requests on the list endpoints the sync stores ETags for.
    let etag = format!("\"{path}\"");
    let listed = matches!(
        path.as_str(),
        "/repos/o/api/pulls"
            | "/repos/o/api/pulls/comments"
            | "/repos/o/api/issues/comments"
            | "/repos/o/api/commits"
    );
    if listed && headers.get("if-none-match").is_some_and(|v| *v == *etag) {
        return reply(StatusCode::NOT_MODIFIED, json!(null), &[]);
    }
    let listing = |body: Value| reply(StatusCode::OK, body, &[("etag", etag.clone())]);

    match (method, path.as_str()) {
        (Method::POST, "/app/installations/1/access_tokens") => {
            ok(json!({"token": "ghs_test", "expires_at": "2999-01-01T00:00:00Z"}))
        }
        (Method::GET, "/app") => ok(json!({"id": 1, "slug": "tern-test", "name": "tern test"})),
        (Method::GET, "/orgs/o/installation") => {
            ok(json!({"id": 1, "repository_selection": "selected"}))
        }
        (Method::GET, "/installation/repositories") => ok(json!({
            "total_count": 2,
            "repositories": [
                {"id": 1, "full_name": "o/api", "name": "api", "default_branch": "main",
                 "archived": false, "pushed_at": "2026-10-15T10:00:00Z"},
                {"id": 2, "full_name": "o/old", "name": "old", "default_branch": "main",
                 "archived": true},
            ],
        })),
        (Method::GET, "/orgs/o/teams/t/members") => ok(json!([
            {"login": "Budi", "avatar_url": "https://avatars/budi"},
            {"login": "sari", "avatar_url": "https://avatars/sari"},
        ])),
        (Method::GET, "/repos/o/api/pulls") if m.rate_limited.load(Ordering::SeqCst) => reply(
            StatusCode::FORBIDDEN,
            json!({"message": "API rate limit exceeded"}),
            &[
                ("x-ratelimit-remaining", "0".into()),
                ("x-ratelimit-reset", "1792000000".into()),
            ],
        ),
        (Method::GET, "/repos/o/api/pulls") if query.contains("page=2") => {
            // Older than any cursor or --since a test uses: paging stops here.
            ok(json!([{"number": 3, "updated_at": "2026-09-01T00:00:00Z"}]))
        }
        (Method::GET, "/repos/o/api/pulls") => reply(
            StatusCode::OK,
            json!([{"number": 7, "updated_at": "2026-10-15T10:00:00Z", "merge_commit_sha": "m7"}]),
            &[
                ("etag", etag.clone()),
                (
                    "link",
                    format!(
                        "<{}/repos/o/api/pulls?state=all&page=2>; rel=\"next\"",
                        m.url
                    ),
                ),
            ],
        ),
        (Method::GET, "/repos/o/api/pulls/7") => ok(json!({
            "id": 70, "number": 7, "title": "Add the thing", "state": "closed", "draft": false,
            "user": {"login": "Budi"},
            "created_at": "2026-10-14T03:00:00Z", "updated_at": "2026-10-15T10:00:00Z",
            "merged_at": "2026-10-15T10:00:00Z", "closed_at": "2026-10-15T10:00:00Z",
            "additions": 10, "deletions": 5, "changed_files": 2,
        })),
        (Method::GET, "/repos/o/api/pulls/7/reviews") => ok(json!([
            // A thread reply: sets first_review_at, but is not stored as a review.
            {"id": 2, "user": {"login": "sari"}, "state": "COMMENTED", "body": "",
             "submitted_at": "2026-10-14T04:00:00Z"},
            {"id": 1, "user": {"login": "sari"}, "state": "APPROVED", "body": "",
             "submitted_at": "2026-10-14T05:00:00Z"},
            {"id": 3, "user": {"login": "budi"}, "state": "COMMENTED", "body": "self",
             "submitted_at": "2026-10-14T06:00:00Z"},
            {"id": 4, "user": {"login": "dependabot[bot]"}, "state": "COMMENTED",
             "body": "bump", "submitted_at": "2026-10-14T07:00:00Z"},
            {"id": 5, "user": {"login": "sari"}, "state": "PENDING", "body": ""},
        ])),
        (Method::GET, "/repos/o/api/pulls/7/commits") => ok(json!([
            commit("c1", Some("Budi"), "budi@work.dev", "2026-10-14T02:00:00Z"),
            commit("c2", None, "Budi@Personal.dev", "2026-10-14T02:30:00Z"),
        ])),
        (Method::GET, "/repos/o/api/pulls/comments") => listing(json!([
            {"id": 11, "user": {"login": "sari"}, "body": "nit: rename",
             "created_at": "2026-10-14T04:00:00Z", "updated_at": "2026-10-14T04:00:00Z",
             "pull_request_url": format!("{}/repos/o/api/pulls/7", m.url)},
        ])),
        (Method::GET, "/repos/o/api/issues/comments") => listing(json!([
            {"id": 21, "user": {"login": "budi"}, "body": "thanks",
             "created_at": "2026-10-14T08:00:00Z", "updated_at": "2026-10-14T08:00:00Z",
             "html_url": "https://github.com/o/api/pull/7#issuecomment-21",
             "issue_url": format!("{}/repos/o/api/issues/7", m.url)},
            {"id": 22, "user": {"login": "budi"}, "body": "on an issue",
             "created_at": "2026-10-14T08:00:00Z", "updated_at": "2026-10-14T09:00:00Z",
             "html_url": "https://github.com/o/api/issues/9#issuecomment-22",
             "issue_url": format!("{}/repos/o/api/issues/9", m.url)},
        ])),
        (Method::GET, "/repos/o/api/commits") => listing(json!([
            // The squash merge of #7: skipped, its work is already counted.
            commit("m7", Some("budi"), "budi@work.dev", "2026-10-15T10:00:00Z"),
            commit("d1", Some("sari"), "sari@work.dev", "2026-10-15T11:00:00Z"),
        ])),
        _ => reply(StatusCode::NOT_FOUND, json!({"message": "Not Found"}), &[]),
    }
}
