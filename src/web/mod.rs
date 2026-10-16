//! The dashboard: axum routes returning server-rendered HTML pages and the
//! fragments htmx swaps in. Reads SQLite only; `POST /sync` is the one route
//! that triggers GitHub traffic.

mod data;
mod views;

use std::sync::{Arc, Mutex};

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use rusqlite::Connection;
use serde::Deserialize;

use crate::config::Config;
use data::{F, enc};
use views::{Section, Tile};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    pub config: Arc<Config>,
    /// Held while a sync runs, so the ticker and "Sync now" never overlap.
    pub sync: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Q {
    from: Option<String>,
    to: Option<String>,
    repo: Option<String>,
    bots: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
    filter: Option<String>,
    sort: Option<usize>,
    dir: Option<String>,
    format: Option<String>,
    state: Option<String>,
    section: Option<String>,
    limit: Option<usize>,
}

const PAGE_ROWS: usize = 50;

#[derive(rust_embed::RustEmbed)]
#[folder = "static/"]
struct Assets;

#[derive(Template)]
#[template(path = "section.html")]
struct SectionT<'a> {
    s: &'a Section,
    t: Option<TableT>,
}

/// A table after filter, sort and paging, with every link precomputed.
struct TableT {
    headers: Vec<(String, String, &'static str)>,
    rows: Vec<Vec<(String, Option<String>)>>,
    csv: String,
    filter: Option<(String, String)>,
    more: Option<String>,
}

#[derive(Template)]
#[template(path = "base.html")]
struct PageT<'a> {
    title: &'a str,
    nav: &'a str,
    path: &'a str,
    f: &'a F,
    repos: Vec<String>,
    last_sync: String,
    tiles: Vec<Tile>,
    body: String,
    sync_enabled: bool,
}

pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("failed to serve request: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {:#}", self.0),
        )
            .into_response()
    }
}

type R = Result<Response, AppError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/members/{login}", get(member))
        .route("/prs", get(prflow))
        .route("/fragments/{name}", get(fragment))
        .route("/sync", post(sync_now))
        .route("/static/{*path}", get(asset))
        .layer(middleware::from_fn(local_only))
        .with_state(state)
}

/// `true` for a Host header naming this machine: localhost, 127.0.0.1 or [::1],
/// with or without a port.
fn is_local_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().map(|h| format!("[{h}]")),
        None => host.split(':').next().map(String::from),
    };
    matches!(name.as_deref(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// Refuses requests addressed to any other host name, which blocks DNS
/// rebinding (an attacker's domain resolving to 127.0.0.1 still sends its own
/// name as Host). Writes must also come from htmx: a cross-site form or
/// `fetch` can't set `HX-Request` without a CORS preflight this server never
/// answers, which blocks CSRF on `POST /sync`.
async fn local_only(req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !is_local_host(host) {
        return (StatusCode::FORBIDDEN, "tern only answers on localhost").into_response();
    }
    let safe = matches!(*req.method(), Method::GET | Method::HEAD);
    let from_htmx = headers.get("hx-request").is_some_and(|v| v == "true");
    let same_origin = headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_none_or(|o| o == format!("http://{host}"));
    if !safe && !(from_htmx && same_origin) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    next.run(req).await
}

pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&state.config.listen).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Years far outside this window overflow the date math the views do, and a
/// panic there poisons the DB mutex for every later request and the sync.
fn date(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?, "%Y-%m-%d")
        .ok()
        .filter(|d| (1970..=9999).contains(&d.year()))
}

fn filters(conn: &Connection, config: &Config, q: &Q) -> anyhow::Result<F> {
    let today = Utc::now().with_timezone(&config.timezone).date_naive();
    let to = date(q.to.as_deref()).unwrap_or(today);
    let from = date(q.from.as_deref())
        .filter(|f| *f <= to)
        .unwrap_or(to - Duration::days(13));
    let repo = q.repo.clone().filter(|r| !r.is_empty());
    let repo_id = match &repo {
        Some(r) => data::repo_id(conn, r)?.or(Some(-1)),
        None => None,
    };
    Ok(F {
        from,
        to,
        repo,
        repo_id,
        bots: q.bots.as_deref().is_some_and(|b| !b.is_empty() && b != "0"),
        tz: config.timezone,
    })
}

/// Numeric-aware cell comparison, so "10" sorts after "9".
fn cmp_cells(a: &str, b: &str) -> std::cmp::Ordering {
    let num = |s: &str| s.trim_start_matches('#').parse::<f64>().ok();
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

fn shape(s: &Section, q: &Q) -> Option<TableT> {
    let t = s.table.as_ref()?;
    let mut rows = t.rows.clone();
    if let Some(f) = q.filter.as_deref().filter(|f| !f.is_empty()) {
        rows.retain(|r| r.iter().any(|(c, _)| c == f));
    }
    if let Some(i) = q.sort.filter(|i| *i < t.headers.len()) {
        rows.sort_by(|a, b| cmp_cells(&a[i].0, &b[i].0));
        if q.dir.as_deref() == Some("desc") {
            rows.reverse();
        }
    }
    let keep = |extra: &str| {
        let mut u = s.base.clone();
        if let Some(f) = q.filter.as_deref().filter(|f| !f.is_empty()) {
            u.push_str(&format!("&filter={}", enc(f)));
        }
        u + extra
    };
    let sorted = |u: String| match q.sort {
        Some(i) => format!("{u}&sort={i}&dir={}", q.dir.as_deref().unwrap_or("asc")),
        None => u,
    };
    let headers = t
        .headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let (dir, mark) = match (q.sort == Some(i), q.dir.as_deref()) {
                (true, Some("desc")) => ("asc", " desc"),
                (true, _) => ("desc", " asc"),
                _ => ("asc", ""),
            };
            (h.clone(), keep(&format!("&sort={i}&dir={dir}")), mark)
        })
        .collect();
    let limit = q.limit.unwrap_or(PAGE_ROWS);
    let more = (rows.len() > limit)
        .then(|| sorted(keep(&format!("&limit={}", limit.saturating_add(PAGE_ROWS)))));
    rows.truncate(limit);
    Some(TableT {
        headers,
        rows,
        csv: sorted(keep("&format=csv&limit=1000000")),
        filter: q
            .filter
            .clone()
            .filter(|f| !f.is_empty())
            .map(|f| (f, sorted(s.base.clone()))),
        more,
    })
}

fn csv(s: &Section, q: &Q) -> Response {
    let Some(t) = shape(s, q) else {
        return (StatusCode::NOT_FOUND, "this section has no table").into_response();
    };
    let field = |v: &str| {
        if v.contains([',', '"', '\n']) {
            format!("\"{}\"", v.replace('"', "\"\""))
        } else {
            v.to_string()
        }
    };
    let mut out = t
        .headers
        .iter()
        .map(|h| field(&h.0))
        .collect::<Vec<_>>()
        .join(",")
        + "\n";
    for r in &t.rows {
        out += &(r.iter().map(|c| field(&c.0)).collect::<Vec<_>>().join(",") + "\n");
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"tern-{}.csv\"", s.id),
            ),
        ],
        out,
    )
        .into_response()
}

fn render_section(s: &Section, q: &Q) -> anyhow::Result<String> {
    Ok(SectionT { s, t: shape(s, q) }.render()?)
}

/// A fragment response: the section's HTML, or its table as CSV.
fn respond(s: Section, q: &Q) -> R {
    if q.format.as_deref() == Some("csv") {
        return Ok(csv(&s, q));
    }
    Ok(Html(render_section(&s, q)?).into_response())
}

fn section_by_name(
    conn: &Connection,
    config: &Config,
    f: &F,
    name: &str,
    q: &Q,
) -> anyhow::Result<Option<Section>> {
    Ok(Some(match name {
        "score" => views::score(conn, config, f)?,
        "member-table" => views::member_table(conn, config, f)?,
        "activity" => views::activity(conn, f, q.ty.as_deref().unwrap_or("all"))?,
        "mix" => views::mix(conn, f)?,
        "heatmap" => views::heatmap(
            conn,
            f,
            None,
            "heatmap",
            format!("/fragments/heatmap?{}", f.qs()),
        )?,
        "prs" if q.state.as_deref() == Some("open") => views::waiting(conn, f)?,
        "prs" => views::scatter(conn, f)?,
        _ => return Ok(None),
    }))
}

async fn fragment(State(st): State<AppState>, Path(name): Path<String>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    match section_by_name(&conn, &st.config, &f, &name, &q)? {
        Some(s) => respond(s, &q),
        None => Ok((StatusCode::NOT_FOUND, "no such fragment").into_response()),
    }
}

/// Sections on a full page start unsorted and unfiltered.
fn page(
    st: &AppState,
    conn: &Connection,
    f: &F,
    (title, nav, path): (&str, &str, &str),
    tiles: Vec<Tile>,
    sections: Vec<Section>,
) -> R {
    let mut body = String::new();
    for s in &sections {
        body.push_str(&render_section(s, &Q::default())?);
    }
    let html = PageT {
        title,
        nav,
        path,
        f,
        repos: data::repo_names(conn)?,
        last_sync: data::last_sync(conn, f.tz)?,
        tiles,
        body,
        sync_enabled: st.config.github.is_some(),
    }
    .render()?;
    Ok(Html(html).into_response())
}

async fn overview(State(st): State<AppState>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let mut sections = vec![];
    for name in ["score", "member-table", "activity", "mix", "heatmap"] {
        sections.extend(section_by_name(&conn, &st.config, &f, name, &Q::default())?);
    }
    let tiles = views::tiles(&conn, &f)?;
    page(
        &st,
        &conn,
        &f,
        ("Team overview", "overview", "/"),
        tiles,
        sections,
    )
}

async fn prflow(State(st): State<AppState>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let sections = vec![views::scatter(&conn, &f)?, views::waiting(&conn, &f)?];
    page(&st, &conn, &f, ("PR flow", "prs", "/prs"), vec![], sections)
}

async fn member(State(st): State<AppState>, Path(login): Path<String>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let Some(m) = data::members(&conn, true)?
        .into_iter()
        .find(|m| m.login == login)
    else {
        return Ok((
            StatusCode::NOT_FOUND,
            format!("{login} is not in members.toml or the team"),
        )
            .into_response());
    };
    let base = |sec: &str| format!("/members/{}?{}&section={sec}", enc(&login), f.qs());
    let mut heat = views::heatmap(
        &conn,
        &f,
        Some(&login),
        "member-heatmap",
        base("member-heatmap"),
    )?;
    heat.title = "Personal heatmap".into();
    heat.note = format!(
        "Their PR, review and comment events by weekday and hour ({}). The table lists their reviews and comments, newest first.",
        f.tz
    );
    heat.charts[0].click = None;
    heat.table = Some(views::feedback_rows(&conn, &f, &login)?);
    let sections = vec![
        views::trend(&conn, &st.config, &f, &login, base("trend"))?,
        views::member_mix(&conn, &f, &login, base("member-mix"))?,
        heat,
    ];
    if let Some(sec) = q.section.as_deref() {
        return match sections.into_iter().find(|s| s.id == sec) {
            Some(s) => respond(s, &q),
            None => Ok((StatusCode::NOT_FOUND, "no such section").into_response()),
        };
    }
    let title = format!("{} ({})", m.name, m.login);
    page(
        &st,
        &conn,
        &f,
        (&title, "member", &format!("/members/{}", enc(&login))),
        vec![],
        sections,
    )
}

/// Runs one sync unless one is already running. Shared by the ticker and
/// the "Sync now" button.
pub async fn sync_once(st: &AppState) -> anyhow::Result<Option<crate::sync::Summary>> {
    let Ok(_guard) = st.sync.try_lock() else {
        return Ok(None);
    };
    crate::sync::run(&st.db, &st.config, None).await.map(Some)
}

async fn sync_now(State(st): State<AppState>) -> Response {
    match sync_once(&st).await {
        // HX-Refresh reloads the page so every chart shows the new data.
        Ok(Some(s)) => (
            [("HX-Refresh", "true")],
            format!(
                "sync {} - {} repos, {} requests",
                s.status, s.repos, s.requests
            ),
        )
            .into_response(),
        Ok(None) => "sync already running".into_response(),
        Err(e) => format!("sync failed - {e:#}").into_response(),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let ty = match path.rsplit('.').next() {
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                _ => "application/octet-stream",
            };
            (
                [
                    (header::CONTENT_TYPE, ty),
                    (header::CACHE_CONTROL, "max-age=3600"),
                ],
                file.data,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_host_names_pass() {
        for ok in [
            "127.0.0.1:8088",
            "localhost:8088",
            "localhost",
            "[::1]:8088",
            "[::1]",
        ] {
            assert!(is_local_host(ok), "{ok}");
        }
        for bad in [
            "evil.example:8088",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "",
            "[::2]:8088",
        ] {
            assert!(!is_local_host(bad), "{bad}");
        }
    }

    #[test]
    fn cells_sort_numerically() {
        let mut v = vec!["10", "9", "#100", "abc"];
        v.sort_by(|a, b| cmp_cells(a, b));
        assert_eq!(v, vec!["9", "10", "#100", "abc"]);
    }
}
