//! Each dashboard section: its charts (as ECharts options) and the table of
//! rows behind them. Colors are `@sN` tokens that `charts.js` resolves per theme.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Datelike, NaiveDate, Timelike};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::data::{self, Ev, F, enc, hours_between, local, median};
use crate::config::Config;

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

pub struct Section {
    pub id: String,
    pub title: String,
    pub note: String,
    /// Fragment URL with the view's query string; table links append to it.
    pub base: String,
    pub controls: Vec<(String, String, bool)>,
    /// Shared legend swatches: (label, color slot 1-8).
    pub legend: Vec<(String, usize)>,
    pub charts: Vec<Chart>,
    pub grid: bool,
    pub table: Option<Table>,
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

fn kind_slot(kind: &str) -> String {
    slot(KINDS.iter().position(|(k, _)| *k == kind).unwrap_or(9))
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
        "legend": {"top": 0, "type": "scroll"},
        "grid": {"left": 48, "right": 16, "top": 36, "bottom": 28, "containLabel": true},
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

pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub delta: String,
}

pub fn tiles(conn: &Connection, f: &F) -> Result<Vec<Tile>> {
    let measure = |f: &F| -> Result<[Option<f64>; 4]> {
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
    };
    let now = measure(f)?;
    let before = measure(&f.prev())?;
    let labels = [
        "PRs merged",
        "Reviews",
        "Median cycle time (h)",
        "Median time to first review (h)",
    ];
    Ok((0..4)
        .map(|i| {
            let fmt = |v: Option<f64>| match (i, v) {
                (0 | 1, Some(v)) => format!("{v:.0}"),
                (_, v) => h1(v),
            };
            let delta = match (now[i], before[i]) {
                (Some(a), Some(b)) => {
                    let d = a - b;
                    format!(
                        "{}{} vs previous {} days",
                        if d >= 0.0 { "+" } else { "" },
                        fmt(Some(d)),
                        f.days()
                    )
                }
                _ => format!("no data for previous {} days", f.days()),
            };
            Tile {
                label: labels[i],
                value: fmt(now[i]),
                delta,
            }
        })
        .collect())
}

pub fn score(conn: &Connection, cfg: &Config, f: &F) -> Result<Section> {
    let mut rows: Vec<_> = data::rows(conn, cfg, f)?
        .into_iter()
        .filter(|r| r.score.is_some())
        .collect();
    rows.sort_by(|a, b| {
        let t = |r: &data::Row| r.score.as_ref().map_or(0.0, |s| s.total);
        t(a).total_cmp(&t(b))
    });
    let logins: Vec<&str> = rows.iter().map(|r| r.login.as_str()).collect();
    let series: Vec<Value> = (0..6)
        .map(|k| {
            let d: Vec<Value> = rows
                .iter()
                .map(|r| json!((r.score.as_ref().unwrap().parts[k] * 10.0).round() / 10.0))
                .collect();
            let mut s = json!({"name": METRIC_LABEL[k], "type": "bar", "stack": "score",
                "data": d, "color": kind_slot(METRIC_KIND[k]), "barMaxWidth": 22,
                "itemStyle": {"borderColor": "@surface", "borderWidth": 1}});
            if k == 0 {
                s["markLine"] = json!({"symbol": "none", "silent": true, "label": {"formatter": "team median", "position": "insideEndTop"},
                    "lineStyle": {"type": "dashed", "color": "@ink2"}, "data": [{"xAxis": 50}]});
            }
            s
        })
        .collect();
    let opt = merge(
        base_opt(),
        json!({"tooltip": {"trigger": "axis", "axisPointer": {"type": "shadow"}},
               "xAxis": {"type": "value", "max": 100},
               "yAxis": {"type": "category", "data": logins},
               "series": series}),
    );
    let mut c = chart(opt, 60 + 36 * rows.len().max(1) as u32);
    c.click = Some(format!(
        "/fragments/member-table?{}&filter={{name}}",
        f.qs()
    ));
    c.target = "#member-table".into();
    Ok(Section {
        id: "score".into(),
        title: "Score by member".into(),
        note: "50 = the team median this period, 100 = the cap. Click a bar to filter the member table.".into(),
        base: format!("/fragments/score?{}", f.qs()),
        controls: vec![],
        legend: vec![],
        charts: vec![c],
        grid: false,
        table: None,
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
        title: "Members".into(),
        note: "From the daily rollup; the repo filter does not apply here.".into(),
        base: format!("/fragments/member-table?{}", f.qs()),
        controls: vec![],
        legend: vec![],
        charts: vec![],
        grid: false,
        table: Some(table),
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

fn week_label(w: NaiveDate) -> String {
    w.format("%b %d").to_string()
}

/// Members to chart: everyone active, plus anyone who left but has activity
/// in the period (their history stays visible).
fn charted_members(conn: &Connection, f: &F, grid: &Grid) -> Result<Vec<data::Member>> {
    let mut members = data::members(conn, f.bots)?;
    members.retain(|m| m.active || grid.keys().any(|(u, _)| *u == m.login));
    Ok(members)
}

/// (login, week) -> kind -> count
type Grid = BTreeMap<(String, NaiveDate), BTreeMap<String, i64>>;

fn by_week(evs: &[Ev]) -> Grid {
    let mut g = Grid::new();
    for e in evs {
        *g.entry((e.user.clone(), data::week_of(e.at.date_naive())))
            .or_default()
            .entry(e.kind.clone())
            .or_default() += 1;
    }
    g
}

pub fn activity(conn: &Connection, f: &F, ty: &str) -> Result<Section> {
    let (ty, _, kinds) = TYPES
        .iter()
        .find(|t| t.0 == ty)
        .copied()
        .unwrap_or(TYPES[0]);
    let evs = data::events(conn, f)?;
    let grid = by_week(&evs);
    let weeks = data::weeks(f);
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
            json!({"name": m.login, "type": "line", "symbolSize": 8, "lineStyle": {"width": 2},
                   "color": slot(i), "data": weeks.iter().map(|w| count(&m.login, *w)).collect::<Vec<_>>()})
        })
        .collect();
    let opt = merge(
        base_opt(),
        json!({"xAxis": {"type": "category", "data": weeks.iter().map(|w| week_label(*w)).collect::<Vec<_>>()},
               "yAxis": {"type": "value", "minInterval": 1}, "series": series}),
    );
    let base = format!("/fragments/activity?{}&type={ty}", f.qs());
    let mut c = chart(opt, 300);
    c.click = Some(format!("{base}&filter={{series}}"));
    c.target = "#activity".into();
    let mut rows = vec![];
    for m in &members {
        for w in &weeks {
            rows.push(vec![
                (m.login.clone(), member_link(&m.login, f)),
                cell(w),
                cell(count(&m.login, *w)),
            ]);
        }
    }
    Ok(Section {
        id: "activity".into(),
        title: "Activity over time".into(),
        note: "Weekly events per member. Click a line to filter the table.".into(),
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
        legend: vec![],
        base,
        charts: vec![c],
        grid: false,
        table: Some(Table {
            headers: vec!["member".into(), "week of".into(), "events".into()],
            rows,
        }),
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

fn mix_chart(grid: &Grid, login: &str, weeks: &[NaiveDate], ymax: i64, legend: bool) -> Value {
    let series: Vec<Value> = KINDS
        .iter()
        .map(|(k, label)| {
            json!({"name": label, "type": "bar", "stack": "t", "color": kind_slot(k), "barMaxWidth": 28,
                   "itemStyle": {"borderColor": "@surface", "borderWidth": 1},
                   "data": weeks.iter().map(|w| grid.get(&(login.to_string(), *w))
                        .and_then(|m| m.get(*k)).copied().unwrap_or(0)).collect::<Vec<_>>()})
        })
        .collect();
    merge(
        base_opt(),
        json!({"legend": {"show": legend, "top": 0, "type": "scroll"},
               "tooltip": {"trigger": "axis", "axisPointer": {"type": "shadow"}},
               "grid": {"left": 8, "right": 8, "top": if legend { 36 } else { 8 }, "bottom": 8, "containLabel": true},
               "xAxis": {"type": "category", "data": weeks.iter().map(|w| week_label(*w)).collect::<Vec<_>>()},
               "yAxis": {"type": "value", "max": nice(ymax), "minInterval": 1},
               "series": series}),
    )
}

pub fn mix(conn: &Connection, f: &F) -> Result<Section> {
    let grid = by_week(&data::events(conn, f)?);
    let weeks = data::weeks(f);
    let members = charted_members(conn, f, &grid)?;
    let ymax = grid.values().map(|m| m.values().sum()).max().unwrap_or(1);
    let charts = members
        .iter()
        .map(|m| {
            let mut c = chart(mix_chart(&grid, &m.login, &weeks, ymax, false), 180);
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
        title: "Activity by type".into(),
        note: "One chart per member, shared y-axis. Click one to filter the member table.".into(),
        base: format!("/fragments/mix?{}", f.qs()),
        controls: vec![],
        legend,
        charts,
        grid: true,
        table: Some(Table {
            headers: vec![
                "member".into(),
                "week of".into(),
                "type".into(),
                "events".into(),
            ],
            rows,
        }),
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
    let opt = json!({
        "tooltip": {"position": "top"},
        "grid": {"left": 8, "right": 8, "top": 8, "bottom": 56, "containLabel": true},
        "xAxis": {"type": "category", "data": (0..24).map(|h| format!("{h:02}")).collect::<Vec<_>>(), "splitArea": {"show": false}},
        "yAxis": {"type": "category", "data": WEEKDAYS, "inverse": true},
        "visualMap": {"min": 0, "max": max, "calculable": false, "orient": "horizontal", "left": "center",
                      "bottom": 0, "itemHeight": 120, "inRange": {"color": ["@seq0", "@seq1"]}},
        "series": [{"type": "heatmap", "data": points, "itemStyle": {"borderColor": "@surface", "borderWidth": 2}}]
    });
    let mut c = chart(opt, 300);
    c.click = Some(format!("{base}&filter={{name}}"));
    c.target = format!("#{id}");
    Ok(Section {
        id: id.into(),
        title: "When work happens".into(),
        note: format!(
            "PR, review and comment events by weekday and hour ({}); core hours are 09-12 and 16-18. Click a cell to filter by weekday.",
            f.tz
        ),
        base,
        controls: vec![],
        legend: vec![],
        charts: vec![c],
        grid: false,
        table: Some(Table {
            headers: vec!["weekday".into(), "hour".into(), "events".into()],
            rows,
        }),
    })
}

fn pr_link(repo: &str, number: i64) -> Option<String> {
    Some(format!("https://github.com/{repo}/pull/{number}"))
}

pub fn scatter(conn: &Connection, f: &F) -> Result<Section> {
    let prs = data::merged_prs(conn, f)?;
    let mut points = vec![];
    let mut rows = vec![];
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
            points.push(json!({"name": format!("{}#{} {}", p.repo, p.number, p.title), "value": [p.size, (c * 10.0).round() / 10.0]}));
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
    let opt = json!({
        "tooltip": {"trigger": "item"},
        "grid": {"left": 48, "right": 16, "top": 24, "bottom": 40, "containLabel": true},
        "xAxis": {"type": "log", "name": "lines changed", "nameLocation": "middle", "nameGap": 28, "min": 1},
        "yAxis": {"type": "value", "name": "cycle time (h)"},
        "series": [{"type": "scatter", "symbolSize": 9, "color": "@s1", "data": points,
                    "itemStyle": {"borderColor": "@surface", "borderWidth": 1}}]
    });
    Ok(Section {
        id: "scatter".into(),
        title: "Size vs cycle time".into(),
        note: format!(
            "One dot per PR merged in the period. Median size {} lines.",
            h1(median(prs.iter().map(|p| p.size as f64).collect()))
        ),
        base: format!("/fragments/prs?{}&state=merged", f.qs()),
        controls: vec![],
        legend: vec![],
        charts: vec![chart(opt, 320)],
        grid: false,
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
    })
}

pub fn waiting(conn: &Connection, f: &F) -> Result<Section> {
    let mut prs = data::prs(
        conn,
        f,
        "p.state = 'open' AND p.draft = 0 AND p.first_review_at IS NULL",
        &[],
    )?;
    prs.reverse(); // oldest first
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let buckets = ["< 1 day", "1-3 days", "3-7 days", "> 7 days"];
    let mut counts = [0; 4];
    let mut rows = vec![];
    for p in &prs {
        let age = hours_between(&p.created_at, &now).unwrap_or(0.0);
        counts[match age {
            a if a < 24.0 => 0,
            a if a < 72.0 => 1,
            a if a < 168.0 => 2,
            _ => 3,
        }] += 1;
        rows.push(vec![
            cell(format!("{age:.1}")),
            cell(&p.repo),
            (format!("#{}", p.number), pr_link(&p.repo, p.number)),
            (p.author.clone(), member_link(&p.author, f)),
            cell(&p.title),
            cell(local(&p.created_at, f.tz)),
        ]);
    }
    let opt = json!({
        "tooltip": {"trigger": "axis", "axisPointer": {"type": "shadow"}},
        "grid": {"left": 8, "right": 16, "top": 16, "bottom": 8, "containLabel": true},
        "xAxis": {"type": "category", "data": buckets},
        "yAxis": {"type": "value", "minInterval": 1},
        "series": [{"type": "bar", "data": counts, "color": "@s4", "barMaxWidth": 40,
                    "itemStyle": {"borderRadius": [4, 4, 0, 0]}, "label": {"show": true, "position": "top", "color": "@ink2"}}]
    });
    Ok(Section {
        id: "waiting".into(),
        title: "Waiting for review".into(),
        note: "Open, non-draft PRs with no review yet, right now (date range ignored).".into(),
        base: format!("/fragments/prs?{}&state=open&reviewed=false", f.qs()),
        controls: vec![],
        legend: vec![],
        charts: vec![chart(opt, 240)],
        grid: false,
        table: Some(Table {
            headers: ["age (h)", "repo", "pr", "author", "title", "opened"]
                .map(String::from)
                .to_vec(),
            rows,
        }),
    })
}

/// Member page: weekly score with the team median at 50.
pub fn trend(conn: &Connection, cfg: &Config, f: &F, login: &str, base: String) -> Result<Section> {
    let mut labels = vec![];
    let mut values = vec![];
    let mut rows = vec![];
    for w in data::weeks(f) {
        let wf = f.with_range(w.max(f.from), (w + chrono::Duration::days(6)).min(f.to));
        let s = data::rows(conn, cfg, &wf)?
            .into_iter()
            .find(|r| r.login == login)
            .and_then(|r| r.score);
        labels.push(week_label(w));
        values.push(s.as_ref().map(|s| (s.total * 10.0).round() / 10.0));
        let mut row = vec![cell(w), cell(h1(s.as_ref().map(|s| s.total)))];
        row.extend((0..6).map(|k| cell(h1(s.as_ref().map(|s| s.parts[k])))));
        rows.push(row);
    }
    let opt = merge(
        base_opt(),
        json!({"legend": {"show": false},
               "xAxis": {"type": "category", "data": labels},
               "yAxis": {"type": "value", "min": 0, "max": 100},
               "series": [{"name": "score", "type": "line", "symbolSize": 8, "lineStyle": {"width": 2}, "color": "@s1",
                           "data": values, "connectNulls": false,
                           "markArea": {"silent": true, "itemStyle": {"color": "@band"}, "data": [[{"yAxis": 40}, {"yAxis": 60}]]},
                           "markLine": {"symbol": "none", "silent": true, "label": {"formatter": "team median", "position": "insideEndTop"},
                                        "lineStyle": {"type": "dashed", "color": "@ink2"}, "data": [{"yAxis": 50}]}}]}),
    );
    let mut headers = vec!["week of".to_string(), "score".into()];
    headers.extend(
        METRIC_LABEL
            .iter()
            .map(|s| format!("{} pts", s.to_lowercase())),
    );
    Ok(Section {
        id: "trend".into(),
        title: "Score trend".into(),
        note: "Weekly score; the line at 50 is the team median, the band 40-60.".into(),
        base,
        controls: vec![],
        legend: vec![],
        charts: vec![chart(opt, 260)],
        grid: false,
        table: Some(Table { headers, rows }),
    })
}

pub fn member_mix(conn: &Connection, f: &F, login: &str, base: String) -> Result<Section> {
    let grid = by_week(&data::events(conn, f)?);
    let weeks = data::weeks(f);
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
        title: "Activity mix".into(),
        note: "Shipping vs reviewing, per week. The table lists their PRs opened or merged in the period.".into(),
        base,
        controls: vec![],
        legend: vec![],
        charts: vec![chart(mix_chart(&grid, login, &weeks, ymax, true), 280)],
        grid: false,
        table: Some(Table {
            headers: ["title", "repo", "opened", "merged", "cycle (h)", "size", "reviews"].map(String::from).to_vec(),
            rows,
        }),
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
}
