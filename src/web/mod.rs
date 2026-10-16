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
use views::{Hero, Section};

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
    /// Row count before paging, for the collapsed summary.
    total: usize,
    /// Shown expanded: the section asks for it, or the viewer sorted,
    /// filtered or paged it.
    open: bool,
}

#[derive(Template)]
#[template(path = "base.html")]
struct PageT<'a> {
    title: &'a str,
    nav: &'a str,
    path: &'a str,
    f: &'a F,
    repos: Vec<String>,
    /// Date-range presets: (label, url, current).
    ranges: Vec<(String, String, bool)>,
    /// None of the presets matches: the custom dates are open.
    custom: bool,
    /// Extra sub-nav links, e.g. the member switcher: (label, url, current).
    chips: Vec<(String, String, bool)>,
    hero: Hero,
    body: String,
    brief: String,
    last_sync: String,
    listen: &'a str,
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
    let total = rows.len();
    let open = s.open_table
        || q.sort.is_some()
        || q.limit.is_some()
        || q.filter.as_deref().is_some_and(|f| !f.is_empty());
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
        total,
        open,
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

/// The sub-nav's date-range presets, each ending today.
fn ranges(path: &str, f: &F) -> Vec<(String, String, bool)> {
    let today = Utc::now().with_timezone(&f.tz).date_naive();
    [7, 14, 30, 90]
        .into_iter()
        .map(|n| {
            let from = today - Duration::days(n - 1);
            let qs = f.with_range(from, today).qs();
            (
                format!("{n} days"),
                format!("{path}?{qs}"),
                f.to == today && f.from == from,
            )
        })
        .collect()
}

/// Sections on a full page start unsorted and unfiltered.
fn page(
    st: &AppState,
    conn: &Connection,
    f: &F,
    (title, nav, path): (&str, &str, &str),
    hero: Hero,
    chips: Vec<(String, String, bool)>,
    sections: Vec<Section>,
) -> R {
    let ranges = ranges(path, f);
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
        custom: !ranges.iter().any(|r| r.2),
        ranges,
        chips,
        hero,
        body,
        brief: data::sync_brief(conn, f.tz)?,
        last_sync: data::last_sync(conn, f.tz)?,
        listen: &st.config.listen,
        sync_enabled: st.config.github.is_some(),
    }
    .render()?;
    Ok(Html(html).into_response())
}

async fn overview(State(st): State<AppState>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let mut sections = vec![];
    for name in ["score", "activity", "mix", "heatmap", "member-table"] {
        sections.extend(section_by_name(&conn, &st.config, &f, name, &Q::default())?);
    }
    let hero = views::overview_hero(&conn, &f)?;
    page(
        &st,
        &conn,
        &f,
        ("Team overview", "overview", "/"),
        hero,
        vec![],
        sections,
    )
}

async fn prflow(State(st): State<AppState>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let sections = vec![views::scatter(&conn, &f)?, views::waiting(&conn, &f)?];
    let hero = views::prflow_hero(&conn, &f)?;
    page(
        &st,
        &conn,
        &f,
        ("PR flow", "prs", "/prs"),
        hero,
        vec![],
        sections,
    )
}

/// The member page's headline: who they are, their score and rank.
fn member_hero(
    conn: &Connection,
    config: &Config,
    f: &F,
    m: &data::Member,
) -> anyhow::Result<Hero> {
    let rows = data::rows(conn, config, f)?;
    let mut scored: Vec<&data::Row> = rows.iter().filter(|r| r.score.is_some()).collect();
    scored.sort_by(|a, b| {
        let t = |r: &data::Row| r.score.as_ref().map_or(0.0, |s| s.total);
        t(b).total_cmp(&t(a))
    });
    let row = rows.iter().find(|r| r.login == m.login);
    let weekdays = crate::score::weekdays(f.from, f.to).len();
    let status = if m.is_bot {
        "bot"
    } else if !m.active {
        "left the team"
    } else if !m.in_score {
        "not scored"
    } else {
        "in score"
    };
    let days = row.map_or(0, |r| r.working_days as usize);
    let away = weekdays.saturating_sub(days);
    let lead = format!(
        "@{} · {status} · {days} of {weekdays} working days{}",
        m.login,
        if away > 0 {
            format!(", {away} away")
        } else {
            String::new()
        }
    );
    let (big, big_note) = match row.and_then(|r| r.score.as_ref()) {
        Some(s) => {
            let rank = scored.iter().position(|r| r.login == m.login).unwrap_or(0) + 1;
            (
                format!("{:.1}", s.total),
                format!("#{rank} of {} this period. Typical is 50.", scored.len()),
            )
        }
        None => ("n/a".into(), "No score this period.".into()),
    };
    Ok(Hero {
        eyebrow: views::eyebrow(f),
        title: m.name.clone(),
        lead,
        avatar: m
            .name
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .collect(),
        big,
        big_note,
        ..Hero::default()
    })
}

async fn member(State(st): State<AppState>, Path(login): Path<String>, Query(q): Query<Q>) -> R {
    let conn = st.db.lock().unwrap();
    let f = filters(&conn, &st.config, &q)?;
    let all = data::members(&conn, true)?;
    let Some(m) = all.iter().find(|m| m.login == login) else {
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
    heat.title = "Working rhythm".into();
    heat.note = format!(
        "Their PRs, reviews and comments by weekday and hour, {}. The table lists their reviews and comments, newest first.",
        f.tz
    );
    heat.charts[0].click = None;
    heat.table = Some(views::feedback_rows(&conn, &f, &login)?);
    let sections = vec![
        views::breakdown(&conn, &st.config, &f, &login, base("breakdown"))?,
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
    let chips = all
        .iter()
        .filter(|o| (o.active && !o.is_bot) || o.login == m.login)
        .map(|o| {
            (
                o.name.clone(),
                format!("/members/{}?{}", enc(&o.login), f.qs()),
                o.login == m.login,
            )
        })
        .collect();
    let hero = member_hero(&conn, &st.config, &f, m)?;
    let path = format!("/members/{}", enc(&login));
    page(
        &st,
        &conn,
        &f,
        (&m.name, "member", &path),
        hero,
        chips,
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

    /// Two repos, five members (one left the team, one opted out, one bot) and
    /// a handful of PRs across 2026-10-01..16.
    const FIXTURE: &str = "
        INSERT INTO members (github_user, name, in_score, is_bot, in_toml, in_team) VALUES
            ('budi', 'Budi', 1, 0, 1, 1), ('sari', 'Sari', 1, 0, 1, 0), ('ayu', 'Ayu', 0, 0, 1, 0),
            ('gone', 'Gone', 1, 0, 0, 0), ('dependabot[bot]', 'dependabot[bot]', 0, 1, 0, 0);
        INSERT INTO repos (id, full_name, default_branch) VALUES (1, 'o/api', 'main'), (2, 'o/web', 'main');
        INSERT INTO pull_requests (id, repo_id, number, author, title, state, draft, created_at,
                                   first_review_at, merged_at, updated_at, additions, deletions) VALUES
            (1, 1, 1, 'budi', 'Big change', 'closed', 0, '2026-10-06T02:00:00Z',
             '2026-10-06T05:00:00Z', '2026-10-08T02:00:00Z', '2026-10-08T02:00:00Z', 100, 20),
            (2, 2, 2, 'sari', 'Tiny, \"quoted\" fix', 'closed', 0, '2026-10-13T03:00:00Z',
             '2026-10-13T04:00:00Z', '2026-10-14T03:00:00Z', '2026-10-14T03:00:00Z', 0, 0),
            (3, 1, 3, 'budi', 'Waiting one', 'open', 0, '2026-10-15T01:00:00Z',
             NULL, NULL, '2026-10-15T01:00:00Z', 5, 5),
            (4, 1, 4, 'sari', 'Draft one', 'open', 1, '2026-10-15T01:00:00Z',
             NULL, NULL, '2026-10-15T01:00:00Z', 5, 5),
            (5, 1, 5, 'dependabot[bot]', 'Bump dep', 'open', 0, '2026-10-10T01:00:00Z',
             NULL, NULL, '2026-10-10T01:00:00Z', 1, 1),
            (6, 2, 6, 'gone', 'Old work', 'closed', 0, '2026-10-06T01:00:00Z',
             '2026-10-06T02:00:00Z', '2026-10-07T01:00:00Z', '2026-10-07T01:00:00Z', 30, 0);
        INSERT INTO reviews (id, repo_id, pr_number, reviewer, state, submitted_at) VALUES
            (1, 1, 1, 'sari', 'APPROVED', '2026-10-06T05:00:00Z'),
            (2, 2, 2, 'budi', 'CHANGES_REQUESTED', '2026-10-13T04:00:00Z');
        INSERT INTO comments (id, repo_id, pr_number, author, kind, body_len, created_at) VALUES
            (1, 1, 1, 'sari', 'review', 10, '2026-10-06T05:00:00Z'),
            (2, 2, 2, 'budi', 'conversation', 5, '2026-10-13T06:00:00Z'),
            (3, 1, 1, 'ayu', 'conversation', 3, '2026-10-07T03:00:00Z');
        INSERT INTO commits (sha, repo_id, author_login, authored_at, committed_at, pr_number) VALUES
            ('a', 1, 'budi', '2026-10-06T01:00:00Z', '2026-10-06T01:00:00Z', 1),
            ('b', 2, 'sari', '2026-10-13T02:00:00Z', '2026-10-13T02:00:00Z', 2);
        INSERT INTO away_days (github_user, date) VALUES ('sari', '2026-10-12');
        INSERT INTO sync_runs (started_at, status, requests_used, rate_remaining)
            VALUES ('2026-10-16T07:15:00Z', 'ok', 23, 4812);";

    const Q: &str = "from=2026-10-01&to=2026-10-16";

    struct Srv {
        base: String,
        http: reqwest::Client,
    }

    impl Srv {
        async fn start() -> Srv {
            let config = Config {
                db_path: tempfile::tempdir().unwrap().keep().join("tern.db"),
                ..Config::default()
            };
            let conn = crate::db::open(&config.db_path).unwrap();
            conn.execute_batch(FIXTURE).unwrap();
            crate::sync::rollup::rebuild_all(&conn, config.timezone).unwrap();
            let state = AppState {
                db: Arc::new(Mutex::new(conn)),
                config: Arc::new(config),
                sync: Arc::default(),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
            Srv {
                base,
                http: reqwest::Client::new(),
            }
        }

        async fn get(&self, path: &str) -> (u16, String) {
            let r = self
                .http
                .get(format!("{}{path}", self.base))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }

        /// A CSV export, as rows of cells (no quoting in the fixture's hot paths).
        async fn csv(&self, path: &str) -> Vec<Vec<String>> {
            let (status, body) = self.get(&format!("{path}&format=csv")).await;
            assert_eq!(status, 200, "{body}");
            body.lines()
                .skip(1)
                .map(|l| l.split(',').map(String::from).collect())
                .collect()
        }
    }

    fn col(rows: &[Vec<String>], i: usize) -> Vec<&str> {
        rows.iter().map(|r| r[i].as_str()).collect()
    }

    #[tokio::test]
    async fn full_pages_render_every_section() {
        let srv = Srv::start().await;
        let (status, body) = srv.get(&format!("/?{Q}")).await;
        assert_eq!(status, 200);
        for want in [
            "Team overview",
            // Hero: 3 merged in range, none in the 16 days before.
            "3 PRs merged. 2 reviews given.",
            "3 more merges than the 16 days before",
            "Median cycle time",
            // Bands in their tones, the score as ranked rows.
            "class=\"band band-parchment\"",
            "class=\"band band-dark\"",
            "Productivity score",
            "Not scored: in_score = false",
            "Left the team",
            "Activity",
            "By type",
            "When work happens",
            "Members",
            // Range presets and the footer.
            "14 days",
            "90 days",
            "last sync: ok - 2026-10-16 14:15, 23 requests",
            "data-chart=",
        ] {
            assert!(body.contains(want), "overview lacks {want:?}");
        }
        // Score rows filter the members table instead of drawing a chart.
        assert!(
            body.contains("filter=budi\" hx-target=\"#member-table\""),
            "score rows must filter the members table"
        );
        // No GitHub configured: no Sync now button.
        assert!(!body.contains("hx-post=\"/sync\""));

        let (status, body) = srv.get(&format!("/prs?{Q}")).await;
        assert_eq!(status, 200);
        assert!(body.contains("Do big PRs slow down?") && body.contains("Waiting for review"));

        let (status, body) = srv.get(&format!("/members/budi?{Q}")).await;
        assert_eq!(status, 200);
        for want in [
            "<h1 id=\"hero-h\">Budi</h1>",
            "@budi · in score · 12 of 12 working days",
            "this period. Typical is 50.",
            "Where the ",
            "× the median of ",
            "Score trend",
            "Shipping vs reviewing",
            "Working rhythm",
            "Big change",
            "changes_requested",
            // The member switcher marks the current member.
            "aria-current=\"page\">Budi</a>",
        ] {
            assert!(body.contains(want), "member page lacks {want:?}");
        }

        let (status, body) = srv.get(&format!("/members/budi?{Q}&section=trend")).await;
        assert_eq!(status, 200);
        assert!(body.starts_with("<section id=\"trend\"") && !body.contains("<html"));
        assert_eq!(
            srv.get(&format!("/members/budi?{Q}&section=nope")).await.0,
            404
        );
        // Ayu opted out: no breakdown, and the reason instead.
        let (_, body) = srv
            .get(&format!("/members/ayu?{Q}&section=breakdown"))
            .await;
        assert!(body.contains("No score this period") && body.contains("in_score is false"));

        let (_, body) = srv.get(&format!("/prs?{Q}")).await;
        assert!(
            body.contains("hours from open to merge.") && body.contains("1 PR</p>"),
            "{body}"
        );
        assert_eq!(srv.get("/members/nobody").await.0, 404);
        // Out-of-range dates fall back to the default range instead of panicking.
        assert_eq!(srv.get("/?from=garbage&to=%2B262143-12-31").await.0, 200);
    }

    #[tokio::test]
    async fn member_table_scores_sorts_filters_and_exports() {
        let srv = Srv::start().await;
        let rows = srv.csv(&format!("/fragments/member-table?{Q}")).await;
        // Bots are hidden by default; names sort case-insensitively.
        assert_eq!(col(&rows, 0), ["ayu", "budi", "gone", "sari"]);
        let cell = |u: &str, i: usize| rows.iter().find(|r| r[0] == u).unwrap()[i].clone();
        let score = |u: &str| cell(u, 11);
        assert_eq!(score("ayu"), "not scored");
        assert_eq!(score("gone"), "left team");
        assert!(score("budi").parse::<f64>().is_ok() && score("sari").parse::<f64>().is_ok());
        // sari was away one weekday out of twelve.
        let days = |u: &str| cell(u, 10);
        assert_eq!((days("budi"), days("sari")), ("12".into(), "11".into()));
        // Review load: budi 1 review / 2 PRs, sari 1 / 1 (her draft doesn't
        // count), gone 0 / 1, ayu opened nothing.
        let load = |u: &str| cell(u, 8);
        assert_eq!(
            [load("budi"), load("sari"), load("gone"), load("ayu")],
            ["0.50", "1.00", "0.00", "-"]
        );

        let sorted = srv
            .csv(&format!("/fragments/member-table?{Q}&sort=0&dir=desc"))
            .await;
        assert_eq!(col(&sorted, 0), ["sari", "gone", "budi", "ayu"]);
        let filtered = srv
            .csv(&format!("/fragments/member-table?{Q}&filter=budi"))
            .await;
        assert_eq!(col(&filtered, 0), ["budi"]);
        let bots = srv
            .csv(&format!("/fragments/member-table?{Q}&bots=1"))
            .await;
        assert!(col(&bots, 0).contains(&"dependabot[bot]"));

        let (status, html) = srv
            .get(&format!(
                "/fragments/member-table?{Q}&filter=budi&sort=2&dir=asc"
            ))
            .await;
        assert_eq!(status, 200);
        assert!(html.contains("Filtered: budi") && html.contains("class=\"sort asc\""));
    }

    #[tokio::test]
    async fn fragments_render_and_follow_their_params() {
        let srv = Srv::start().await;
        for frag in ["score", "activity", "mix", "heatmap", "prs"] {
            let (status, body) = srv.get(&format!("/fragments/{frag}?{Q}")).await;
            assert_eq!(status, 200, "{frag}");
            assert!(body.starts_with("<section"), "{frag}");
        }
        assert_eq!(srv.get(&format!("/fragments/nope?{Q}")).await.0, 404);
        // Rows behind a chart start collapsed; the members table and any
        // sorted or filtered table start open.
        let collapsed = "<details class=\"card table-card\">";
        let open = "<details class=\"card table-card\" open>";
        assert!(
            srv.get(&format!("/fragments/activity?{Q}"))
                .await
                .1
                .contains(collapsed)
        );
        assert!(
            srv.get(&format!("/fragments/activity?{Q}&sort=2&dir=desc"))
                .await
                .1
                .contains(open)
        );
        assert!(
            srv.get(&format!("/fragments/member-table?{Q}"))
                .await
                .1
                .contains(open)
        );
        // The score chart has no table to export.
        assert_eq!(
            srv.get(&format!("/fragments/score?{Q}&format=csv")).await.0,
            404
        );

        let (_, body) = srv
            .get(&format!("/fragments/activity?{Q}&type=reviews"))
            .await;
        assert!(body.contains("aria-pressed=\"true\">Reviews<"), "{body}");
        let (_, body) = srv.get(&format!("/fragments/activity?{Q}&limit=2")).await;
        assert!(body.contains("Load more"));

        // Heatmap rows are weekday/hour cells; commits are left out.
        let cells = srv.csv(&format!("/fragments/heatmap?{Q}")).await;
        let total: i64 = cells.iter().map(|r| r[2].parse::<i64>().unwrap()).sum();
        // 5 PRs opened, 3 merged, 2 reviews, 3 comments; the bot's PR is hidden.
        assert_eq!(total, 13, "{cells:?}");
        let tue = srv.csv(&format!("/fragments/heatmap?{Q}&filter=Tue")).await;
        assert!(!tue.is_empty() && tue.iter().all(|r| r[0] == "Tue"));

        let mix = srv.csv(&format!("/fragments/mix?{Q}")).await;
        assert!(mix.iter().any(|r| r[0] == "budi" && r[2] == "Commits"));
    }

    #[tokio::test]
    async fn pr_flow_lists_respect_repo_bots_and_drafts() {
        let srv = Srv::start().await;
        let waiting = srv
            .csv(&format!("/fragments/prs?{Q}&state=open&reviewed=false"))
            .await;
        // Not the draft, not the bot's.
        assert_eq!(col(&waiting, 2), ["#3"]);
        let with_bots = srv
            .csv(&format!(
                "/fragments/prs?{Q}&state=open&reviewed=false&bots=1"
            ))
            .await;
        assert_eq!(col(&with_bots, 2), ["#5", "#3"]);

        let merged = srv.csv(&format!("/fragments/prs?{Q}&state=merged")).await;
        assert_eq!(col(&merged, 1), ["#2", "#1", "#6"]);
        let web = srv
            .csv(&format!("/fragments/prs?{Q}&state=merged&repo=o%2Fweb"))
            .await;
        assert_eq!(col(&web, 1), ["#2", "#6"]);
        let unknown = srv
            .csv(&format!("/fragments/prs?{Q}&state=merged&repo=o%2Fnope"))
            .await;
        assert!(unknown.is_empty());

        let (_, raw) = srv
            .get(&format!("/fragments/prs?{Q}&state=merged&format=csv"))
            .await;
        assert!(
            raw.contains("\"Tiny, \"\"quoted\"\" fix\""),
            "CSV must quote: {raw}"
        );
    }

    #[tokio::test]
    async fn assets_and_the_request_guard() {
        let srv = Srv::start().await;
        let r = srv
            .http
            .get(format!("{}/static/charts.js", srv.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "text/javascript");
        let r = srv
            .http
            .get(format!("{}/static/style.css", srv.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()[header::CONTENT_TYPE], "text/css");
        assert_eq!(srv.get("/static/nope.js").await.0, 404);

        let rebound = srv
            .http
            .get(format!("{}/", srv.base))
            .header(header::HOST, "evil.example:8088")
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), 403);

        let sync = |hx: bool, origin: &str| {
            let mut req = srv
                .http
                .post(format!("{}/sync", srv.base))
                .header(header::ORIGIN, origin);
            if hx {
                req = req.header("HX-Request", "true");
            }
            req.send()
        };
        assert_eq!(sync(false, &srv.base).await.unwrap().status(), 403);
        assert_eq!(
            sync(true, "http://evil.example").await.unwrap().status(),
            403
        );
        let ok = sync(true, &srv.base).await.unwrap();
        assert_eq!(ok.status(), 200);
        assert!(
            ok.text()
                .await
                .unwrap()
                .starts_with("sync failed - [github] is not configured")
        );
    }

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
