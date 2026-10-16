//! Each dashboard section: its charts (as ECharts options), any server-drawn
//! blocks, and the table of rows behind them. Colors are `@sN` tokens that
//! `charts.js` resolves for the band the chart sits on (light or dark).

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Datelike, Duration, NaiveDate, Timelike, Weekday};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::data::{self, Ev, F, enc, hours_between, local, median};
use crate::config::Config;
use crate::score;

pub struct Chart {
    pub option: String,
    pub click: Option<String>,
    pub target: String,
    pub height: u32,
    pub label: Option<String>,
}

pub struct Table {
    pub headers: Vec<String>,
    /// (text, link)
    pub rows: Vec<Vec<(String, Option<String>)>>,
}

/// One full-width band of a page, swapped as a whole by htmx.
#[derive(Default)]
pub struct Section {
    pub id: String,
    /// "white", "parchment" or "dark": the band's surface.
    pub tone: &'static str,
    /// Less top padding, for a band that continues the one above it.
    pub tight: bool,
    /// The band's headline; empty = the band only has a card title.
    pub title: String,
    pub note: String,
    /// Heading inside the chart card.
    pub card_title: String,
    /// Fragment URL with the view's query string; table links append to it.
    pub base: String,
    pub controls: Vec<(String, String, bool)>,
    /// Shared legend swatches: (label, color slot 1-8).
    pub legend: Vec<(String, usize)>,
    pub charts: Vec<Chart>,
    /// Lay the charts out as small multiples.
    pub grid: bool,
    pub score_rows: Vec<ScoreRow>,
    pub parts: Vec<Part>,
    /// A large number shown above the blocks (e.g. "5 PRs").
    pub big: String,
    pub aging: Vec<Aging>,
    pub table: Option<Table>,
    /// Show the table expanded; otherwise it starts collapsed under the chart.
    pub open_table: bool,
    /// Score bars: the top of the scale (50 × cap_multiple) and where the
    /// median (50) sits on it, in %.
    pub scale_max: String,
    pub mid: String,
}

/// Score bars run 0..50 × cap; returns (top of the scale, median position in %).
fn scale(cap: f64) -> (f64, String) {
    (50.0 * cap, format!("{:.2}", 100.0 / cap))
}

/// A ranked row of the score list.
pub struct ScoreRow {
    pub rank: String,
    pub name: String,
    pub login: String,
    pub href: String,
    /// Fragment URL that filters the members table to this member.
    pub filter: String,
    /// (width % of the 0-100 scale, color slot)
    pub segs: Vec<(String, usize)>,
    pub score: String,
    /// Why there is no score (empty when scored).
    pub why: String,
}

/// One metric's share of a member's score.
pub struct Part {
    pub label: &'static str,
    pub slot: usize,
    pub count: String,
    pub points: String,
    /// Points as % of the 0-100 scale, for the breakdown bar.
    pub width: String,
    pub ratio: String,
    /// Ratio drawn on a 0-cap scale.
    pub fill: String,
    pub median: String,
    pub weight: String,
}

/// One age bucket of the waiting-for-review bar.
pub struct Aging {
    pub label: &'static str,
    pub count: usize,
    /// 1 (youngest) .. 4 (oldest).
    pub step: usize,
}

pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub unit: &'static str,
    /// Change vs the previous period, bold; empty when there is none.
    pub delta: String,
    pub note: String,
}

/// The headline band at the top of a page.
#[derive(Default)]
pub struct Hero {
    pub eyebrow: String,
    pub title: String,
    pub lead: String,
    pub tiles: Vec<Tile>,
    /// Member pages: the avatar initial, the big score and its caption.
    pub avatar: String,
    pub big: String,
    pub big_note: String,
}

/// Event kinds of the `activity` view, in fixed color-slot order.
pub const KINDS: [(&str, &str); 6] = [
    ("commit", "Commits"),
    ("pr_opened", "PRs opened"),
    ("pr_merged", "PRs merged"),
    ("review", "Reviews"),
    ("comment_review", "Review comments"),
    ("comment_conversation", "Conversation comments"),
];

fn slot(i: usize) -> String {
    if i < 8 {
        format!("@s{}", i + 1)
    } else {
        "@s0".into()
    }
}

fn kind_index(kind: &str) -> usize {
    KINDS.iter().position(|(k, _)| *k == kind).unwrap_or(9)
}

fn kind_slot(kind: &str) -> String {
    slot(kind_index(kind))
}

/// Score metric -> the event kind whose color it wears.
const METRIC_KIND: [&str; 6] = [
    "pr_merged",
    "review",
    "comment_review",
    "pr_opened",
    "commit",
    "comment_conversation",
];
const METRIC_LABEL: [&str; 6] = [
    "PRs merged",
    "Reviews given",
    "Review comments",
    "PRs opened",
    "Commits",
    "Conversation comments",
];

fn cell(s: impl ToString) -> (String, Option<String>) {
    (s.to_string(), None)
}

fn h1(x: Option<f64>) -> String {
    x.map_or_else(|| "-".into(), |v| format!("{v:.1}"))
}

fn chart(option: Value, height: u32) -> Chart {
    Chart {
        option: option.to_string(),
        click: None,
        target: String::new(),
        height,
        label: None,
    }
}

fn base_opt() -> Value {
    json!({
        "tooltip": {"trigger": "axis"},
        "legend": {"top": 0, "type": "scroll", "icon": "circle"},
        "grid": {"left": 40, "right": 16, "top": 40, "bottom": 28, "containLabel": true},
    })
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}

fn member_link(login: &str, f: &F) -> Option<String> {
    Some(format!("/members/{}?{}", enc(login), f.qs()))
}

fn pr_link(repo: &str, number: i64) -> Option<String> {
    Some(format!("https://github.com/{repo}/pull/{number}"))
}

/// Merged count, reviews, median cycle time and median time to first review.
fn measure(conn: &Connection, f: &F) -> Result<[Option<f64>; 4]> {
    let merged = data::merged_prs(conn, f)?;
    let cycle = median(
        merged
            .iter()
            .filter_map(|p| hours_between(&p.created_at, p.merged_at.as_deref()?))
            .collect(),
    );
    let (s, e) = (f.start(), f.end());
    let created = data::prs(
        conn,
        f,
        "p.created_at >= ?3 AND p.created_at < ?4",
        &[&s, &e],
    )?;
    let first = median(
        created
            .iter()
            .filter_map(|p| hours_between(&p.created_at, p.first_review_at.as_deref()?))
            .collect(),
    );
    Ok([
        Some(merged.len() as f64),
        Some(data::reviews_count(conn, f)? as f64),
        cycle,
        first,
    ])
}

fn plural(n: f64, one: &str, many: &str) -> String {
    format!("{n:.0} {}", if n == 1.0 { one } else { many })
}

pub fn eyebrow(f: &F) -> String {
    format!(
        "{} to {} · {}",
        f.from.format("%b %-d"),
        f.to.format("%b %-d, %Y"),
        f.tz
    )
}

/// Overview headline: what shipped, and how it compares with the period before.
pub fn overview_hero(conn: &Connection, f: &F) -> Result<Hero> {
    let now = measure(conn, f)?;
    let before = measure(conn, &f.prev())?;
    let (merged, reviews) = (now[0].unwrap_or(0.0), now[1].unwrap_or(0.0));
    let days = f.days();
    let change = match before[0].map(|b| merged - b) {
        Some(d) if d > 0.0 => format!("{d:.0} more {}", if d == 1.0 { "merge" } else { "merges" }),
        Some(d) if d < 0.0 => format!(
            "{:.0} fewer {}",
            -d,
            if d == -1.0 { "merge" } else { "merges" }
        ),
        _ => "The same number of merges".into(),
    };
    let first = match (now[3], before[3]) {
        (Some(a), Some(b)) if a > b + 0.05 => ", and first reviews are arriving later",
        (Some(a), Some(b)) if a + 0.05 < b => ", and first reviews are arriving sooner",
        _ => "",
    };
    let labels = [
        "PRs merged",
        "Reviews given",
        "Median cycle time",
        "Median time to first review",
    ];
    let tiles = (0..4)
        .map(|i| {
            let hours = i >= 2;
            let fmt = |v: f64| {
                if hours {
                    format!("{v:.1}")
                } else {
                    format!("{v:.0}")
                }
            };
            let (delta, note) = match (now[i], before[i]) {
                (Some(a), Some(b)) => {
                    let d = a - b;
                    let sign = if d > 0.0 {
                        "+"
                    } else if d < 0.0 {
                        "−"
                    } else {
                        "±"
                    };
                    let unit = if hours { "h" } else { "" };
                    let note = if hours && d > 0.05 {
                        "slower than before".to_string()
                    } else if hours && d < -0.05 {
                        "faster than before".to_string()
                    } else {
                        format!("vs previous {days} days")
                    };
                    (format!("{sign}{}{unit}", fmt(d.abs())), note)
                }
                _ => (String::new(), format!("no data for previous {days} days")),
            };
            Tile {
                label: labels[i],
                value: now[i].map_or_else(|| "-".into(), fmt),
                unit: if hours && now[i].is_some() { "h" } else { "" },
                delta,
                note,
            }
        })
        .collect();
    Ok(Hero {
        eyebrow: eyebrow(f),
        title: format!(
            "{} merged. {} given.",
            plural(merged, "PR", "PRs"),
            plural(reviews, "review", "reviews")
        ),
        lead: format!("{change} than the {days} days before{first}."),
        tiles,
        ..Hero::default()
    })
}

/// PR flow headline: how long a PR takes, and how much of it is waiting.
pub fn prflow_hero(conn: &Connection, f: &F) -> Result<Hero> {
    let now = measure(conn, f)?;
    let merged = data::merged_prs(conn, f)?;
    let size = median(merged.iter().map(|p| p.size as f64).collect());
    let waiting = waiting_prs(conn, f)?;
    let oldest = waiting
        .first()
        .and_then(|p| hours_between(&p.created_at, &now_ts()));
    let title = match now[2] {
        Some(c) => format!("{c:.1} hours from open to merge."),
        None => "No PRs merged in this period.".into(),
    };
    let lead = match (now[2], now[3]) {
        (Some(c), Some(r)) if r >= c / 2.0 => {
            format!("Half of that is waiting: the first review arrives after {r:.1} hours.")
        }
        (_, Some(r)) => format!("The first review arrives after {r:.1} hours."),
        _ => "No reviewed PRs opened in this period.".into(),
    };
    let tiles = vec![
        Tile {
            label: "Median cycle time",
            value: h1(now[2]),
            unit: if now[2].is_some() { "h" } else { "" },
            delta: String::new(),
            note: "opened to merged".into(),
        },
        Tile {
            label: "Median time to first review",
            value: h1(now[3]),
            unit: if now[3].is_some() { "h" } else { "" },
            delta: String::new(),
            note: "by someone other than the author".into(),
        },
        Tile {
            label: "Median PR size",
            value: size.map_or_else(|| "-".into(), |s| format!("{s:.0}")),
            unit: if size.is_some() { " lines" } else { "" },
            delta: String::new(),
            note: "additions plus deletions".into(),
        },
        Tile {
            label: "Waiting for review",
            value: waiting.len().to_string(),
            unit: if waiting.len() == 1 { " PR" } else { " PRs" },
            delta: String::new(),
            note: oldest.map_or_else(
                || "none right now".into(),
                |h| format!("oldest for {}", age(h)),
            ),
        },
    ];
    Ok(Hero {
        eyebrow: format!(
            "{} · {}",
            eyebrow(f),
            plural(merged.len() as f64, "merged PR", "merged PRs")
        ),
        title,
        lead,
        tiles,
        ..Hero::default()
    })
}

/// "5 h" under a day, "3.4 d" after.
fn age(hours: f64) -> String {
    if hours < 24.0 {
        format!("{hours:.0} h")
    } else {
        format!("{:.1} d", hours / 24.0)
    }
}

fn now_ts() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn score(conn: &Connection, cfg: &Config, f: &F) -> Result<Section> {
    let (top, mid) = scale(cfg.score.cap_multiple);
    let rows = data::rows(conn, cfg, f)?;
    let mut scored: Vec<&data::Row> = rows.iter().filter(|r| r.score.is_some()).collect();
    scored.sort_by(|a, b| {
        let t = |r: &data::Row| r.score.as_ref().map_or(0.0, |s| s.total);
        t(b).total_cmp(&t(a))
    });
    let filter = |login: &str| format!("/fragments/member-table?{}&filter={}", f.qs(), enc(login));
    let mut score_rows: Vec<ScoreRow> = scored
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let s = r.score.as_ref().unwrap();
            ScoreRow {
                rank: (i + 1).to_string(),
                name: r.name.clone(),
                login: r.login.clone(),
                href: member_link(&r.login, f).unwrap(),
                filter: filter(&r.login),
                segs: (0..6)
                    .map(|k| {
                        (
                            format!("{:.2}", s.parts[k] / top * 100.0),
                            kind_index(METRIC_KIND[k]) + 1,
                        )
                    })
                    .collect(),
                score: format!("{:.1}", s.total),
                why: String::new(),
            }
        })
        .collect();
    for r in rows.iter().filter(|r| r.score.is_none()) {
        let why = if !r.active {
            "Left the team. Activity stays visible, the score doesn't count."
        } else if !r.in_score {
            "Not scored: in_score = false in members.toml."
        } else {
            "No working days in this period."
        };
        score_rows.push(ScoreRow {
            rank: String::new(),
            name: r.name.clone(),
            login: r.login.clone(),
            href: member_link(&r.login, f).unwrap(),
            filter: filter(&r.login),
            segs: vec![],
            score: "n/a".into(),
            why: why.into(),
        });
    }
    // Legend in the bar's segment order.
    let legend = (0..6)
        .map(|k| (METRIC_LABEL[k].to_string(), kind_index(METRIC_KIND[k]) + 1))
        .collect();
    Ok(Section {
        id: "score".into(),
        tone: "parchment",
        title: "Productivity score".into(),
        note: format!(
            "Each metric against the team median, capped at {}× the median. 50 is a typical member this period. Click a bar to filter the members table.",
            cfg.score.cap_multiple
        ),
        base: format!("/fragments/score?{}", f.qs()),
        legend,
        score_rows,
        scale_max: format!("{top:.0}"),
        mid,
        ..Section::default()
    })
}

/// Reviews given per PR opened: above 1, a member reviews more than they ship.
/// `-` when they opened no PRs, since the ratio has no meaning then.
fn review_load(reviews_given: i64, prs_opened: i64) -> String {
    if prs_opened == 0 {
        return "-".into();
    }
    format!("{:.2}", reviews_given as f64 / prs_opened as f64)
}

pub fn member_table(conn: &Connection, cfg: &Config, f: &F) -> Result<Section> {
    let rows = data::rows(conn, cfg, f)?;
    let mut headers = vec!["member".to_string(), "name".into()];
    headers.extend(METRIC_LABEL.iter().map(|s| s.to_lowercase()));
    headers.extend([
        "review load".into(),
        "lines changed".into(),
        "working days".into(),
        "score".into(),
    ]);
    let table = Table {
        headers,
        rows: rows
            .iter()
            .map(|r| {
                let mut v = vec![(r.login.clone(), member_link(&r.login, f)), cell(&r.name)];
                v.extend((0..6).map(|k| cell(r.metric(k))));
                v.push(cell(review_load(r.sums[3], r.sums[1])));
                v.push(cell(r.sums[6]));
                v.push(cell(r.working_days));
                v.push(cell(match &r.score {
                    Some(s) => format!("{:.1}", s.total),
                    None if !r.active => "left team".into(),
                    None if !r.in_score => "not scored".into(),
                    None => "-".into(),
                }));
                v
            })
            .collect(),
    };
    Ok(Section {
        id: "member-table".into(),
        tone: "white",
        title: "Members".into(),
        note:
            "The rows behind the score, from the daily rollup. The repo filter doesn't apply here."
                .into(),
        base: format!("/fragments/member-table?{}", f.qs()),
        table: Some(table),
        open_table: true,
        ..Section::default()
    })
}

const TYPES: [(&str, &str, &[&str]); 5] = [
    ("all", "All", &[]),
    ("commits", "Commits", &["commit"]),
    ("prs", "PRs", &["pr_opened", "pr_merged"]),
    ("reviews", "Reviews", &["review"]),
    (
        "comments",
        "Comments",
        &["comment_review", "comment_conversation"],
    ),
];

fn day_label(d: NaiveDate) -> String {
    d.format("%b %d").to_string()
}

/// Ranges up to this many days are charted per day; longer ones per week.
const DAILY_MAX_DAYS: u32 = 31;

/// The chart buckets for a range: every day, or every week's Monday.
fn buckets(f: &F) -> (Vec<NaiveDate>, bool) {
    if f.days() <= DAILY_MAX_DAYS {
        (f.from.iter_days().take(f.days() as usize).collect(), true)
    } else {
        (data::weeks(f), false)
    }
}

/// Members to chart: everyone active, plus anyone who left but has activity
/// in the period (their history stays visible).
fn charted_members(conn: &Connection, f: &F, grid: &Grid) -> Result<Vec<data::Member>> {
    let mut members = data::members(conn, f.bots)?;
    members.retain(|m| m.active || grid.keys().any(|(u, _)| *u == m.login));
    Ok(members)
}

/// (login, bucket) -> kind -> count
type Grid = BTreeMap<(String, NaiveDate), BTreeMap<String, i64>>;

fn by_bucket(evs: &[Ev], daily: bool) -> Grid {
    let mut g = Grid::new();
    for e in evs {
        let d = e.at.date_naive();
        let key = if daily { d } else { data::week_of(d) };
        *g.entry((e.user.clone(), key))
            .or_default()
            .entry(e.kind.clone())
            .or_default() += 1;
    }
    g
}

/// Weekend shading for a daily category axis: one [Sat, Sun] span per weekend.
fn weekend_areas(days: &[NaiveDate]) -> Value {
    let mut areas = vec![];
    let mut i = 0;
    while i < days.len() {
        let wd = days[i].weekday();
        if matches!(wd, Weekday::Sat | Weekday::Sun) {
            let start = i;
            while i + 1 < days.len() && days[i + 1].weekday() == Weekday::Sun {
                i += 1;
            }
            areas.push(json!([{"xAxis": day_label(days[start])}, {"xAxis": day_label(days[i])}]));
        }
        i += 1;
    }
    json!({"silent": true, "itemStyle": {"color": "@band"}, "data": areas})
}

pub fn activity(conn: &Connection, f: &F, ty: &str) -> Result<Section> {
    let (ty, _, kinds) = TYPES
        .iter()
        .find(|t| t.0 == ty)
        .copied()
        .unwrap_or(TYPES[0]);
    let (keys, daily) = buckets(f);
    let grid = by_bucket(&data::events(conn, f)?, daily);
    let members = charted_members(conn, f, &grid)?;
    let count = |login: &str, w: NaiveDate| -> i64 {
        grid.get(&(login.to_string(), w)).map_or(0, |m| {
            m.iter()
                .filter(|(k, _)| kinds.is_empty() || kinds.contains(&k.as_str()))
                .map(|(_, n)| n)
                .sum()
        })
    };
    let series: Vec<Value> = members
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut s = json!({"name": m.login, "type": "line", "symbol": "circle", "symbolSize": 7,
                   "showSymbol": !daily, "lineStyle": {"width": 2.5}, "color": slot(i),
                   "data": keys.iter().map(|w| count(&m.login, *w)).collect::<Vec<_>>()});
            if daily && i == 0 {
                s["markArea"] = weekend_areas(&keys);
            }
            s
        })
        .collect();
    let opt = merge(
        base_opt(),
        json!({"xAxis": {"type": "category", "boundaryGap": false, "data": keys.iter().map(|w| day_label(*w)).collect::<Vec<_>>()},
               "yAxis": {"type": "value", "minInterval": 1}, "series": series}),
    );
    let base = format!("/fragments/activity?{}&type={ty}", f.qs());
    let mut c = chart(opt, 320);
    c.click = Some(format!("{base}&filter={{series}}"));
    c.target = "#activity".into();
    let mut rows = vec![];
    for m in &members {
        for w in &keys {
            rows.push(vec![
                (m.login.clone(), member_link(&m.login, f)),
                cell(w),
                cell(count(&m.login, *w)),
            ]);
        }
    }
    let per = if daily { "day" } else { "week" };
    Ok(Section {
        id: "activity".into(),
        tone: "white",
        title: "Activity".into(),
        note: format!(
            "Every commit, PR, review and comment, {per} by {per}. Click a line to filter the table."
        ),
        card_title: "Over time".into(),
        controls: TYPES
            .iter()
            .map(|(k, label, _)| {
                (
                    label.to_string(),
                    format!("/fragments/activity?{}&type={k}", f.qs()),
                    *k == ty,
                )
            })
            .collect(),
        base,
        charts: vec![c],
        table: Some(Table {
            headers: vec![
                "member".into(),
                if daily { "day" } else { "week of" }.into(),
                "events".into(),
            ],
            rows,
        }),
        ..Section::default()
    })
}

/// Rounds a shared axis max up to a readable step.
fn nice(max: i64) -> i64 {
    let step = match max {
        m if m <= 10 => 2,
        m if m <= 50 => 10,
        m if m <= 200 => 25,
        _ => 100,
    };
    ((max.max(1) + step - 1) / step) * step
}

/// One member's stacked bars per bucket. Buckets before `dim_before` are
/// drawn faded (outside the selected range).
fn mix_chart(
    grid: &Grid,
    login: &str,
    keys: &[NaiveDate],
    ymax: i64,
    legend: bool,
    dim_before: Option<NaiveDate>,
) -> Value {
    let series: Vec<Value> = KINDS
        .iter()
        .map(|(k, label)| {
            let data: Vec<Value> = keys
                .iter()
                .map(|w| {
                    let n = grid
                        .get(&(login.to_string(), *w))
                        .and_then(|m| m.get(*k))
                        .copied()
                        .unwrap_or(0);
                    match dim_before {
                        Some(d) if *w < d => json!({"value": n, "itemStyle": {"opacity": 0.4}}),
                        _ => json!(n),
                    }
                })
                .collect();
            json!({"name": label, "type": "bar", "stack": "t", "color": kind_slot(k), "barMaxWidth": 28,
                   "itemStyle": {"borderColor": "@surface", "borderWidth": 1, "borderRadius": 2},
                   "data": data})
        })
        .collect();
    merge(
        base_opt(),
        json!({"legend": {"show": legend, "top": 0, "type": "scroll", "icon": "circle"},
               "tooltip": {"trigger": "axis", "axisPointer": {"type": "shadow"}},
               "grid": {"left": 8, "right": 8, "top": if legend { 40 } else { 8 }, "bottom": 8, "containLabel": true},
               "xAxis": {"type": "category", "data": keys.iter().map(|w| day_label(*w)).collect::<Vec<_>>()},
               "yAxis": {"type": "value", "max": nice(ymax), "minInterval": 1},
               "series": series}),
    )
}

pub fn mix(conn: &Connection, f: &F) -> Result<Section> {
    let (keys, daily) = buckets(f);
    let grid = by_bucket(&data::events(conn, f)?, daily);
    let members = charted_members(conn, f, &grid)?;
    let ymax = grid.values().map(|m| m.values().sum()).max().unwrap_or(1);
    let charts = members
        .iter()
        .map(|m| {
            let mut c = chart(mix_chart(&grid, &m.login, &keys, ymax, false, None), 180);
            c.label = Some(m.name.clone());
            c.click = Some(format!(
                "/fragments/member-table?{}&filter={}",
                f.qs(),
                enc(&m.login)
            ));
            c.target = "#member-table".into();
            c
        })
        .collect();
    let mut rows = vec![];
    for ((login, w), kinds) in &grid {
        for (k, n) in kinds {
            let label = KINDS.iter().find(|x| x.0 == k).map_or(k.as_str(), |x| x.1);
            rows.push(vec![
                (login.clone(), member_link(login, f)),
                cell(w),
                cell(label),
                cell(n),
            ]);
        }
    }
    let legend = KINDS
        .iter()
        .enumerate()
        .map(|(i, (_, label))| (label.to_string(), i + 1))
        .collect();
    Ok(Section {
        id: "mix".into(),
        tone: "white",
        tight: true,
        card_title: "By type".into(),
        note: "One chart per member on a shared scale, so the mix and the volume compare directly. Click one to filter the members table.".into(),
        base: format!("/fragments/mix?{}", f.qs()),
        legend,
        charts,
        grid: true,
        table: Some(Table {
            headers: vec![
                "member".into(),
                if daily { "day" } else { "week of" }.into(),
                "type".into(),
                "events".into(),
            ],
            rows,
        }),
        ..Section::default()
    })
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Weekday x hour heatmap. Commits are left out: `authored_at` comes from the
/// author's machine and moves on rebase.
pub fn heatmap(
    conn: &Connection,
    f: &F,
    member: Option<&str>,
    id: &str,
    base: String,
) -> Result<Section> {
    let mut cells = [[0i64; 24]; 7];
    for e in data::events(conn, f)? {
        if e.kind == "commit" || member.is_some_and(|m| m != e.user) {
            continue;
        }
        cells[e.at.weekday().num_days_from_monday() as usize][e.at.hour() as usize] += 1;
    }
    let max = cells.iter().flatten().copied().max().unwrap_or(0).max(1);
    let mut points = vec![];
    let mut rows = vec![];
    for (d, hours) in cells.iter().enumerate() {
        for (h, n) in hours.iter().enumerate() {
            // Empty cells stay out of the data so they show the surface.
            if *n > 0 {
                points.push(json!({"name": WEEKDAYS[d], "value": [h, d, n]}));
                rows.push(vec![cell(WEEKDAYS[d]), cell(format!("{h:02}:00")), cell(n)]);
            }
        }
    }
    let core =
        |a: u32, b: u32| json!([{"xAxis": format!("{a:02}")}, {"xAxis": format!("{:02}", b - 1)}]);
    let opt = json!({
        "tooltip": {"position": "top"},
        "grid": {"left": 8, "right": 8, "top": 8, "bottom": 56, "containLabel": true},
        "xAxis": {"type": "category", "data": (0..24).map(|h| format!("{h:02}")).collect::<Vec<_>>(), "splitArea": {"show": false}},
        "yAxis": {"type": "category", "data": WEEKDAYS, "inverse": true},
        "visualMap": {"min": 0, "max": max, "calculable": false, "orient": "horizontal", "left": "center",
                      "bottom": 0, "itemHeight": 140, "text": ["More", "Fewer"], "inRange": {"color": ["@seq0", "@seq1"]}},
        "series": [{"type": "heatmap", "data": points, "itemStyle": {"borderColor": "@surface", "borderWidth": 3, "borderRadius": 4},
                    "markArea": {"silent": true, "itemStyle": {"color": "transparent", "borderColor": "@ink", "borderWidth": 1, "borderType": "dashed"},
                                 "data": [core(9, 12), core(16, 18)]}}]
    });
    let mut c = chart(opt, 320);
    c.click = Some(format!("{base}&filter={{name}}"));
    c.target = format!("#{id}");
    Ok(Section {
        id: id.into(),
        tone: "dark",
        title: "When work happens".into(),
        note: format!(
            "PRs, reviews and comments by weekday and hour, {}. Commits are left out: their time comes from the author's machine. Core hours (09-12, 16-18) are outlined. Click a cell to filter by weekday.",
            f.tz
        ),
        base,
        charts: vec![c],
        table: Some(Table {
            headers: vec!["weekday".into(), "hour".into(), "events".into()],
            rows,
        }),
        ..Section::default()
    })
}

pub fn scatter(conn: &Connection, f: &F) -> Result<Section> {
    let prs = data::merged_prs(conn, f)?;
    let mut points = vec![];
    let mut rows = vec![];
    let mut cycles = vec![];
    for p in &prs {
        let cycle = p
            .merged_at
            .as_deref()
            .and_then(|m| hours_between(&p.created_at, m));
        let first = p
            .first_review_at
            .as_deref()
            .and_then(|m| hours_between(&p.created_at, m));
        if let Some(c) = cycle {
            cycles.push(c);
            // Log axis: a PR with no changed lines sits at 1.
            points.push(json!({"name": format!("{}#{} {}", p.repo, p.number, p.title), "value": [p.size.max(1), (c * 10.0).round() / 10.0]}));
        }
        rows.push(vec![
            cell(&p.repo),
            (format!("#{}", p.number), pr_link(&p.repo, p.number)),
            (p.author.clone(), member_link(&p.author, f)),
            cell(&p.title),
            cell(local(&p.created_at, f.tz)),
            cell(
                p.merged_at
                    .as_deref()
                    .map(|m| local(m, f.tz))
                    .unwrap_or_default(),
            ),
            cell(h1(cycle)),
            cell(h1(first)),
            cell(p.size),
            cell(p.reviews),
        ]);
    }
    let med_size = median(prs.iter().map(|p| p.size.max(1) as f64).collect());
    let med_cycle = median(cycles);
    let mut series = json!({"type": "scatter", "symbolSize": 11, "color": "@s1", "data": points,
                            "itemStyle": {"borderColor": "@surface", "borderWidth": 2}});
    if let (Some(x), Some(y)) = (med_size, med_cycle) {
        series["markLine"] = json!({"symbol": "none", "silent": true, "label": {"show": false},
            "lineStyle": {"type": "dashed", "color": "@ink", "width": 1.5}, "data": [{"xAxis": x}, {"yAxis": y}]});
        series["markArea"] = json!({"silent": true, "itemStyle": {"color": "@band"},
            "label": {"show": true, "position": "insideTopLeft", "distance": 8, "color": "@ink2", "formatter": "big and slow"},
            "data": [[{"xAxis": x, "yAxis": y}, {"xAxis": "max", "yAxis": "max"}]]});
    }
    let opt = json!({
        "tooltip": {"trigger": "item"},
        "grid": {"left": 48, "right": 16, "top": 24, "bottom": 44, "containLabel": true},
        "xAxis": {"type": "log", "name": "Lines changed, log scale", "nameLocation": "middle", "nameGap": 30, "min": 1},
        "yAxis": {"type": "value", "name": "Cycle time, hours"},
        "series": [series]
    });
    Ok(Section {
        id: "scatter".into(),
        tone: "parchment",
        title: "Do big PRs slow down?".into(),
        note: format!(
            "One dot per merged PR; the dashed lines are the medians (size {} lines, cycle {} h). The shaded corner holds the PRs that were both big and slow.",
            med_size.map_or_else(|| "-".into(), |s| format!("{s:.0}")),
            h1(med_cycle)
        ),
        base: format!("/fragments/prs?{}&state=merged", f.qs()),
        charts: vec![chart(opt, 380)],
        table: Some(Table {
            headers: [
                "repo",
                "pr",
                "author",
                "title",
                "opened",
                "merged",
                "cycle (h)",
                "to first review (h)",
                "size",
                "reviews",
            ]
            .map(String::from)
            .to_vec(),
            rows,
        }),
        ..Section::default()
    })
}

/// Open, non-draft PRs with no review yet, oldest first.
fn waiting_prs(conn: &Connection, f: &F) -> Result<Vec<data::Pr>> {
    let mut prs = data::prs(
        conn,
        f,
        "p.state = 'open' AND p.draft = 0 AND p.first_review_at IS NULL",
        &[],
    )?;
    prs.reverse();
    Ok(prs)
}

pub fn waiting(conn: &Connection, f: &F) -> Result<Section> {
    let prs = waiting_prs(conn, f)?;
    let now = now_ts();
    let labels = ["Under 1 day", "1 to 3 days", "3 to 7 days", "Over 7 days"];
    let mut counts = [0; 4];
    let mut rows = vec![];
    for p in &prs {
        let hours = hours_between(&p.created_at, &now).unwrap_or(0.0);
        counts[match hours {
            a if a < 24.0 => 0,
            a if a < 72.0 => 1,
            a if a < 168.0 => 2,
            _ => 3,
        }] += 1;
        rows.push(vec![
            cell(format!("{hours:.1}")),
            cell(&p.repo),
            (format!("#{}", p.number), pr_link(&p.repo, p.number)),
            (p.author.clone(), member_link(&p.author, f)),
            cell(&p.title),
            cell(local(&p.created_at, f.tz)),
        ]);
    }
    Ok(Section {
        id: "waiting".into(),
        tone: "dark",
        title: "Waiting for review".into(),
        note: "Open, not a draft, no review yet. Right now, whatever the date range.".into(),
        base: format!("/fragments/prs?{}&state=open&reviewed=false", f.qs()),
        big: format!(
            "{} {}",
            prs.len(),
            if prs.len() == 1 { "PR" } else { "PRs" }
        ),
        open_table: true,
        aging: labels
            .iter()
            .zip(counts)
            .enumerate()
            .map(|(i, (label, count))| Aging {
                label,
                count,
                step: i + 1,
            })
            .collect(),
        table: Some(Table {
            headers: ["age (h)", "repo", "pr", "author", "title", "opened"]
                .map(String::from)
                .to_vec(),
            rows,
        }),
        ..Section::default()
    })
}

/// Member pages chart this many weeks, ending with the selected range.
const TREND_WEEKS: i64 = 12;

/// Mondays of the member-page window: `TREND_WEEKS` weeks ending at `f.to`.
fn trend_weeks(f: &F) -> Vec<NaiveDate> {
    let last = data::week_of(f.to);
    (0..TREND_WEEKS)
        .rev()
        .map(|i| last - Duration::days(7 * i))
        .collect()
}

/// Member page: what each metric contributed to the score.
pub fn breakdown(
    conn: &Connection,
    cfg: &Config,
    f: &F,
    login: &str,
    base: String,
) -> Result<Section> {
    let rows = data::rows(conn, cfg, f)?;
    let row = rows.iter().find(|r| r.login == login);
    let weights = score::weights(&cfg.score);
    let cap = cfg.score.cap_multiple;
    let (top, mid) = scale(cap);
    let (title, note, parts) = match row.and_then(|r| r.score.as_ref().map(|s| (r, s))) {
        Some((r, s)) => {
            let parts = (0..6)
                .map(|k| Part {
                    label: METRIC_LABEL[k],
                    slot: kind_index(METRIC_KIND[k]) + 1,
                    count: r.metric(k).to_string(),
                    points: format!("{:.1}", s.parts[k]),
                    width: format!("{:.2}", s.parts[k] / top * 100.0),
                    ratio: format!("{:.2}", s.ratios[k]),
                    fill: format!("{:.1}", (s.ratios[k] / cap).min(1.0) * 100.0),
                    median: format!("{:.1}", s.medians[k]),
                    weight: format!("{}", weights[k]),
                })
                .collect();
            (
                format!("Where the {:.1} comes from", s.total),
                format!(
                    "Counts scaled to the period's {} weekdays, each divided by the team median and capped at {cap}×. The marker on each bar is the median.",
                    score::weekdays(f.from, f.to).len()
                ),
                parts,
            )
        }
        None => {
            let why = match row {
                Some(r) if !r.active => "They're no longer on the team or in members.toml.",
                Some(r) if !r.in_score => "in_score is false for them in members.toml.",
                Some(_) => "They have no working days in this period.",
                None => "They're not on the roster.",
            };
            ("No score this period".into(), why.into(), vec![])
        }
    };
    Ok(Section {
        id: "breakdown".into(),
        tone: "parchment",
        title,
        note,
        base,
        parts,
        scale_max: format!("{top:.0}"),
        mid,
        ..Section::default()
    })
}

/// Member page: weekly score over the last 12 weeks, the selected range shaded.
pub fn trend(conn: &Connection, cfg: &Config, f: &F, login: &str, base: String) -> Result<Section> {
    let weeks = trend_weeks(f);
    let mut labels = vec![];
    let mut values = vec![];
    let mut rows = vec![];
    for &w in &weeks {
        let wf = f.with_range(w, (w + Duration::days(6)).min(f.to));
        let s = data::rows(conn, cfg, &wf)?
            .into_iter()
            .find(|r| r.login == login)
            .and_then(|r| r.score);
        labels.push(day_label(w));
        values.push(s.as_ref().map(|s| (s.total * 10.0).round() / 10.0));
        let mut row = vec![cell(w), cell(h1(s.as_ref().map(|s| s.total)))];
        row.extend((0..6).map(|k| cell(h1(s.as_ref().map(|s| s.parts[k])))));
        rows.push(row);
    }
    let first_sel = data::week_of(f.from).max(weeks[0]);
    let opt = merge(
        base_opt(),
        json!({"legend": {"show": false},
               "xAxis": {"type": "category", "data": labels},
               "yAxis": {"type": "value", "min": 0, "max": 100},
               "series": [
                   {"name": "score", "type": "line", "symbol": "circle", "symbolSize": 9, "lineStyle": {"width": 2.5}, "color": "@s1",
                    "data": values, "connectNulls": false,
                    "markArea": {"silent": true, "data": [
                        [{"xAxis": day_label(first_sel), "itemStyle": {"color": "@sel"}}, {"xAxis": day_label(data::week_of(f.to))}],
                        [{"yAxis": 40, "itemStyle": {"color": "@band"}}, {"yAxis": 60}]]},
                    "markLine": {"symbol": "none", "silent": true, "label": {"formatter": "team median", "position": "insideEndTop", "color": "@ink"},
                                 "lineStyle": {"type": "dashed", "color": "@ink", "width": 1.5}, "data": [{"yAxis": 50}]}}]}),
    );
    let mut headers = vec!["week of".to_string(), "score".into()];
    headers.extend(
        METRIC_LABEL
            .iter()
            .map(|s| format!("{} pts", s.to_lowercase())),
    );
    Ok(Section {
        id: "trend".into(),
        tone: "white",
        title: "Score trend".into(),
        note: "The last 12 weeks, each scored against that week's team median. The band is 40 to 60; the selected range is shaded.".into(),
        base,
        charts: vec![chart(opt, 280)],
        table: Some(Table { headers, rows }),
        ..Section::default()
    })
}

/// Member page: shipping vs reviewing per week over the last 12 weeks, plus
/// their PRs in the selected range.
pub fn member_mix(conn: &Connection, f: &F, login: &str, base: String) -> Result<Section> {
    let weeks = trend_weeks(f);
    let window = f.with_range(weeks[0], f.to);
    let grid = by_bucket(&data::events(conn, &window)?, false);
    let ymax = grid
        .iter()
        .filter(|((u, _), _)| u == login)
        .map(|(_, m)| m.values().sum())
        .max()
        .unwrap_or(1);
    let (s, e) = (f.start(), f.end());
    let prs = data::prs(
        conn,
        f,
        "p.author = ?3 AND ((p.created_at >= ?4 AND p.created_at < ?5) OR (p.merged_at >= ?4 AND p.merged_at < ?5))",
        &[&login, &s, &e],
    )?;
    let rows = prs
        .iter()
        .map(|p| {
            vec![
                (p.title.clone(), pr_link(&p.repo, p.number)),
                cell(&p.repo),
                cell(local(&p.created_at, f.tz)),
                cell(
                    p.merged_at
                        .as_deref()
                        .map(|m| local(m, f.tz))
                        .unwrap_or_default(),
                ),
                cell(h1(p
                    .merged_at
                    .as_deref()
                    .and_then(|m| hours_between(&p.created_at, m)))),
                cell(p.size),
                cell(p.reviews),
            ]
        })
        .collect();
    Ok(Section {
        id: "member-mix".into(),
        tone: "white",
        tight: true,
        card_title: "Shipping vs reviewing".into(),
        open_table: true,
        note: "Events per week over the last 12 weeks; weeks before the selected range are faded. The table lists their PRs opened or merged in the range.".into(),
        base,
        charts: vec![chart(
            mix_chart(&grid, login, &weeks, ymax, true, Some(data::week_of(f.from))),
            300,
        )],
        table: Some(Table {
            headers: ["title", "repo", "opened", "merged", "cycle (h)", "size", "reviews"]
                .map(String::from)
                .to_vec(),
            rows,
        }),
        ..Section::default()
    })
}

/// Reviews given and comments by `login`, newest first.
pub fn feedback_rows(conn: &Connection, f: &F, login: &str) -> Result<Table> {
    let mut stmt = conn.prepare(
        "SELECT kind, full_name, pr_number, at FROM (
             SELECT 'review ' || lower(v.state) AS kind, v.repo_id, v.pr_number, v.submitted_at AS at
               FROM reviews v WHERE v.reviewer = ?1
             UNION ALL
             SELECT c.kind || ' comment', c.repo_id, c.pr_number, c.created_at FROM comments c WHERE c.author = ?1
         ) x JOIN repos r ON r.id = x.repo_id
         WHERE at >= ?2 AND at < ?3 AND (?4 IS NULL OR x.repo_id = ?4)
         ORDER BY at DESC",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![login, f.start(), f.end(), f.repo_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?
        .map(|r| {
            r.map(|(kind, repo, n, at)| {
                vec![
                    cell(local(&at, f.tz)),
                    cell(kind),
                    cell(&repo),
                    (format!("#{n}"), pr_link(&repo, n)),
                ]
            })
        })
        .collect::<rusqlite::Result<_>>()?;
    Ok(Table {
        headers: ["at", "what", "repo", "pr"].map(String::from).to_vec(),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn range(from: &str, to: &str) -> F {
        F {
            from: d(from),
            to: d(to),
            repo: None,
            repo_id: None,
            bots: false,
            tz: chrono_tz::Asia::Jakarta,
        }
    }

    #[test]
    fn review_load_is_reviews_per_pr_opened() {
        assert_eq!(review_load(3, 2), "1.50");
        assert_eq!(review_load(0, 4), "0.00");
        assert_eq!(review_load(5, 0), "-");
    }

    #[test]
    fn nice_rounds_up_to_a_step() {
        assert_eq!(nice(0), 2);
        assert_eq!(nice(55), 75);
        assert_eq!(nice(40), 40);
        assert_eq!(nice(7), 8);
    }

    #[test]
    fn short_ranges_chart_days_long_ones_weeks() {
        let (days, daily) = buckets(&range("2026-10-03", "2026-10-16"));
        assert!(daily);
        assert_eq!(
            (days.len(), days[0], days[13]),
            (14, d("2026-10-03"), d("2026-10-16"))
        );
        let (weeks, daily) = buckets(&range("2026-07-01", "2026-10-16"));
        assert!(!daily);
        assert_eq!(weeks[0], d("2026-06-29"));
    }

    #[test]
    fn weekends_shade_as_saturday_to_sunday_spans() {
        let (days, _) = buckets(&range("2026-10-03", "2026-10-16"));
        let areas = weekend_areas(&days);
        assert_eq!(
            areas["data"],
            json!([[{"xAxis": "Oct 03"}, {"xAxis": "Oct 04"}], [{"xAxis": "Oct 10"}, {"xAxis": "Oct 11"}]])
        );
    }

    #[test]
    fn member_charts_cover_twelve_weeks_ending_at_the_range() {
        let weeks = trend_weeks(&range("2026-10-03", "2026-10-16"));
        assert_eq!(weeks.len(), 12);
        assert_eq!((weeks[0], weeks[11]), (d("2026-07-27"), d("2026-10-12")));
    }

    #[test]
    fn score_scale_follows_the_cap() {
        assert_eq!(scale(2.0), (100.0, "50.00".into()));
        assert_eq!(scale(3.0), (150.0, "33.33".into()));
    }

    #[test]
    fn ages_read_in_hours_then_days() {
        assert_eq!(age(5.2), "5 h");
        assert_eq!(age(81.6), "3.4 d");
    }
}
