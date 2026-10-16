//! The GitHub REST calls behind App authentication: who the App is, where
//! it's installed, minting an installation token, and how many repos that
//! token can see. The response parsing (`parse_*`, `describe_status`) is
//! pure and unit-tested; the `reqwest` calls themselves need a real App and
//! org, same "confirm end-to-end yourself" rule as `upgrade.rs`'s I/O.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde::Deserialize;

use super::auth::{AppKey, InstallationToken};

const API: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";

fn user_agent() -> String {
    format!("tern/{}", env!("TERN_VERSION"))
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client builds")
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AppInfo {
    pub id: u64,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Installation {
    pub id: u64,
    /// `"all"` or `"selected"` - which repos the org admin granted.
    pub repository_selection: String,
}

#[derive(Deserialize)]
struct RawAccessToken {
    token: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct RawRepoCount {
    total_count: u64,
}

/// Everything `tern status` reports about the App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCheck {
    pub app: AppInfo,
    pub installation: Installation,
    pub repo_count: u64,
    pub latency_ms: u128,
}

pub fn parse_app(body: &str) -> Result<AppInfo> {
    serde_json::from_str(body).context("unexpected response from GET /app")
}

pub fn parse_installation(body: &str) -> Result<Installation> {
    serde_json::from_str(body).context("unexpected response from GET /orgs/{org}/installation")
}

pub fn parse_access_token(body: &str) -> Result<InstallationToken> {
    let raw: RawAccessToken = serde_json::from_str(body)
        .context("unexpected response from POST /app/installations/{id}/access_tokens")?;
    let expires_at = DateTime::parse_from_rfc3339(&raw.expires_at)
        .with_context(|| format!("unexpected token expiry {:?}", raw.expires_at))?
        .timestamp();
    Ok(InstallationToken {
        token: raw.token,
        expires_at,
    })
}

pub fn parse_repo_count(body: &str) -> Result<u64> {
    let raw: RawRepoCount = serde_json::from_str(body)
        .context("unexpected response from GET /installation/repositories")?;
    Ok(raw.total_count)
}

/// Turns a non-2xx status from one of the calls above into an operator-
/// facing explanation - the raw "401 Unauthorized" alone doesn't say
/// whether the key, the App ID, or the clock is wrong.
pub fn describe_status(step: Step, status: u16, org: &str) -> String {
    match (step, status) {
        (Step::App, 401) => "GitHub rejected the App JWT (401) - check app_id in config.toml \
                             matches the App the key was generated for, and that this \
                             machine's clock is correct"
            .to_string(),
        (Step::Installation, 404) => format!(
            "the App is not installed on the {org:?} org (404) - install it from the App's \
             settings page (\"Install App\"), or fix `org` in config.toml"
        ),
        (Step::Token, 403) | (Step::Token, 404) => format!(
            "GitHub refused to issue an installation token ({status}) - the installation may \
             be suspended; check the App's installation on {org:?}"
        ),
        (Step::Repos, 403) => "the installation token can't list repositories (403) - make \
                               sure the App has the Metadata: read permission"
            .to_string(),
        (_, status) => format!("{} failed with HTTP {status}", step.endpoint()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    App,
    Installation,
    Token,
    Repos,
}

impl Step {
    fn endpoint(self) -> &'static str {
        match self {
            Step::App => "GET /app",
            Step::Installation => "GET /orgs/{org}/installation",
            Step::Token => "POST /app/installations/{id}/access_tokens",
            Step::Repos => "GET /installation/repositories",
        }
    }
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Client {
        Client {
            http: http_client(Duration::from_secs(20)),
        }
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        bearer: &str,
        step: Step,
        org: &str,
    ) -> Result<String> {
        let response = request
            .header(reqwest::header::USER_AGENT, user_agent())
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(bearer)
            .send()
            .await
            .with_context(|| format!("{} failed - is GitHub reachable?", step.endpoint()))?;
        let status = response.status();
        if !status.is_success() {
            bail!(describe_status(step, status.as_u16(), org));
        }
        response
            .text()
            .await
            .with_context(|| format!("failed to read the {} response", step.endpoint()))
    }

    pub async fn app(&self, jwt: &str) -> Result<AppInfo> {
        let body = self
            .send(self.http.get(format!("{API}/app")), jwt, Step::App, "")
            .await?;
        parse_app(&body)
    }

    pub async fn org_installation(&self, jwt: &str, org: &str) -> Result<Installation> {
        let url = format!("{API}/orgs/{org}/installation");
        let body = self
            .send(self.http.get(url), jwt, Step::Installation, org)
            .await?;
        parse_installation(&body)
    }

    pub async fn installation_token(
        &self,
        jwt: &str,
        installation_id: u64,
        org: &str,
    ) -> Result<InstallationToken> {
        let url = format!("{API}/app/installations/{installation_id}/access_tokens");
        let body = self
            .send(self.http.post(url), jwt, Step::Token, org)
            .await?;
        parse_access_token(&body)
    }

    pub async fn repo_count(&self, token: &str, org: &str) -> Result<u64> {
        let url = format!("{API}/installation/repositories?per_page=1");
        let body = self
            .send(self.http.get(url), token, Step::Repos, org)
            .await?;
        parse_repo_count(&body)
    }
}

/// The full round-trip `tern status` reports: App JWT -> `/app` -> the org's installation -> an
/// installation token -> how many repos it sees. Proves the key, the App
/// ID, the org, the installation, and the Metadata permission in one go.
pub async fn check(app_id: u64, org: &str, key: &AppKey) -> Result<AppCheck> {
    let started = Instant::now();
    let client = Client::new();
    let jwt = key.app_jwt(app_id, chrono::Utc::now().timestamp())?;
    let app = client.app(&jwt).await?;
    let installation = client.org_installation(&jwt, org).await?;
    let token = client
        .installation_token(&jwt, installation.id, org)
        .await?;
    let repo_count = client.repo_count(&token.token, org).await?;
    Ok(AppCheck {
        app,
        installation,
        repo_count,
        latency_ms: started.elapsed().as_millis(),
    })
}

/// Returned when GitHub says to slow down; the sync stops and marks the run
/// `partial`, the next tick retries.
#[derive(Debug)]
pub struct RateLimited(pub String);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited: {}", self.0)
    }
}
impl std::error::Error for RateLimited {}

pub enum Page {
    /// 304: nothing changed since the supplied ETag.
    NotModified,
    Body {
        json: serde_json::Value,
        etag: Option<String>,
        next: Option<String>,
    },
}

/// Authenticated, serial REST client for the sync: mints and refreshes the
/// installation token, counts requests, and tracks the rate-limit headroom.
pub struct Api {
    http: reqwest::Client,
    app_id: u64,
    org: String,
    key: AppKey,
    installation_id: u64,
    token: Option<InstallationToken>,
    pub requests: u32,
    pub remaining: Option<i64>,
}

impl Api {
    pub async fn connect(app_id: u64, org: &str, key: AppKey) -> Result<Api> {
        let client = Client::new();
        let jwt = key.app_jwt(app_id, chrono::Utc::now().timestamp())?;
        let installation = client.org_installation(&jwt, org).await?;
        Ok(Api {
            http: http_client(Duration::from_secs(30)),
            app_id,
            org: org.to_string(),
            key,
            installation_id: installation.id,
            token: None,
            requests: 0,
            remaining: None,
        })
    }

    async fn token(&mut self) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        if let Some(t) = &self.token
            && t.expires_at - now > 300
        {
            return Ok(t.token.clone());
        }
        let jwt = self.key.app_jwt(self.app_id, now)?;
        let t = Client::new()
            .installation_token(&jwt, self.installation_id, &self.org)
            .await?;
        self.token = Some(t.clone());
        Ok(t.token)
    }

    /// One GET. `path` is either `/repos/...` or a full `next` URL.
    pub async fn get(&mut self, path: &str, etag: Option<&str>) -> Result<Page> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{API}{path}")
        };
        let token = self.token().await?;
        let mut req = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, user_agent())
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(token);
        if let Some(e) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, e);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        self.requests += 1;
        let h = resp.headers();
        let num = |name: &str| h.get(name)?.to_str().ok()?.parse::<i64>().ok();
        if let Some(r) = num("x-ratelimit-remaining") {
            self.remaining = Some(r);
        }
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(Page::NotModified);
        }
        if status == 429 || status == 403 {
            let wait = num("retry-after")
                .map(|s| format!("retry after {s}s"))
                .or_else(|| num("x-ratelimit-reset").map(|t| format!("resets at unix {t}")))
                .unwrap_or_default();
            let limited =
                status == 429 || num("retry-after").is_some() || self.remaining == Some(0);
            // A secondary rate limit can be a bare 403 that only says so in its body.
            if limited || resp.text().await.unwrap_or_default().contains("rate limit") {
                return Err(RateLimited(wait).into());
            }
            bail!("GET {url} failed with HTTP {}", status.as_u16());
        }
        if !status.is_success() {
            bail!("GET {url} failed with HTTP {}", status.as_u16());
        }
        let etag = h
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let next = h
            .get("link")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_next_link);
        let json = resp
            .json()
            .await
            .with_context(|| format!("unexpected body from GET {url}"))?;
        Ok(Page::Body { json, etag, next })
    }

    /// Pages through a list endpoint, stopping when `stop` says an item is past
    /// the cursor or there is no next page. `Ok(None)` = the first page was a
    /// 304. A 409 (empty repository) reads as an empty list.
    pub async fn list(
        &mut self,
        path: &str,
        etag: Option<&str>,
        stop: impl Fn(&serde_json::Value) -> bool,
    ) -> Result<Option<(Vec<serde_json::Value>, Option<String>)>> {
        let mut out = Vec::new();
        let mut first_etag = None;
        let mut url = path.to_string();
        let mut first = true;
        loop {
            let page = match self.get(&url, if first { etag } else { None }).await {
                Ok(p) => p,
                Err(e) if e.to_string().ends_with("HTTP 409") => break,
                Err(e) => return Err(e),
            };
            let Page::Body { json, etag, next } = page else {
                return Ok(None);
            };
            if first {
                first_etag = etag;
                first = false;
            }
            let mut done = false;
            // `/installation/repositories` wraps its list in {"repositories": [...]}.
            let items = json.as_array().or_else(|| json["repositories"].as_array());
            for item in items.cloned().unwrap_or_default() {
                if stop(&item) {
                    done = true;
                    break;
                }
                out.push(item);
            }
            match next {
                Some(n) if !done => url = n,
                _ => break,
            }
        }
        Ok(Some((out, first_etag)))
    }
}

/// The `rel="next"` URL of a `Link` header, if any.
pub fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_next_link_finds_the_next_page() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#;
        assert_eq!(
            parse_next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(parse_next_link(r#"<https://a/x?page=1>; rel="prev""#), None);
    }

    #[test]
    fn parse_app_reads_id_slug_and_name() {
        let body = r#"{"id": 123456, "slug": "tern-oxgrad", "name": "tern (oxgrad)",
                       "owner": {"login": "oxgrad"}, "permissions": {"issues": "read"}}"#;
        assert_eq!(
            parse_app(body).unwrap(),
            AppInfo {
                id: 123456,
                slug: "tern-oxgrad".into(),
                name: "tern (oxgrad)".into(),
            }
        );
    }

    #[test]
    fn parse_installation_reads_id_and_selection() {
        let body =
            r#"{"id": 987, "account": {"login": "oxgrad"}, "repository_selection": "selected"}"#;
        assert_eq!(
            parse_installation(body).unwrap(),
            Installation {
                id: 987,
                repository_selection: "selected".into(),
            }
        );
    }

    #[test]
    fn parse_access_token_converts_expiry_to_unix_seconds() {
        let body = r#"{"token": "ghs_abc", "expires_at": "2026-10-15T10:00:00Z",
                       "permissions": {"issues": "read"}}"#;
        assert_eq!(
            parse_access_token(body).unwrap(),
            InstallationToken {
                token: "ghs_abc".into(),
                expires_at: 1_792_058_400,
            }
        );
    }

    #[test]
    fn parse_access_token_rejects_a_bad_expiry() {
        let body = r#"{"token": "ghs_abc", "expires_at": "soon"}"#;
        assert!(parse_access_token(body).is_err());
    }

    #[test]
    fn parse_repo_count_reads_total_count() {
        let body = r#"{"total_count": 14, "repositories": [{"name": "api"}]}"#;
        assert_eq!(parse_repo_count(body).unwrap(), 14);
    }

    #[test]
    fn describe_status_explains_the_common_failures() {
        assert!(describe_status(Step::App, 401, "oxgrad").contains("app_id"));
        assert!(describe_status(Step::Installation, 404, "oxgrad").contains("not installed"));
        assert!(describe_status(Step::Token, 403, "oxgrad").contains("suspended"));
        assert!(describe_status(Step::Repos, 403, "oxgrad").contains("Metadata"));
        assert_eq!(
            describe_status(Step::App, 500, "oxgrad"),
            "GET /app failed with HTTP 500"
        );
    }
}
