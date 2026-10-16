# Metrics and the productivity score

How every number on the dashboard is counted, and how the score is computed
from those counts. The code is the source of truth:

- `src/sync/fetch.rs`: what the sync stores
- `src/sync/rollup.rs`: the daily counts
- `src/score.rs`: the score
- `src/web/`: the charts and tables

All times are stored in UTC and bucketed into days, weekdays and hours in the
`timezone` from `config.toml` (default `Asia/Jakarta`).

## What gets counted

The sync stores raw activity from every non-archived repo the App can see
(narrowed by `repos` in `config.toml`), with these rules:

| Activity | Counted when | Not counted |
| --- | --- | --- |
| **Commit** | Authored by the member, on a PR's branch or pushed straight to the default branch. Dated by `authored_at`. | A squash merge's commit on the default branch (its work is already counted from the PR's own commits). Commits whose author can't be matched to a login. |
| **PR opened** | The member authored it and it is not a draft. Dated by `created_at`. | Drafts (if a draft is later marked ready, it counts on its creation date). |
| **PR merged** | The member authored it and it was merged. Dated by `merged_at`. | Closed without merging. |
| **Review given** | The member submitted an approval, a change request, or a comment review with a written summary on someone else's PR. Dated by `submitted_at`. | Reviews on their own PR. Pending (unsubmitted) reviews. A reply in an inline-comment thread, which GitHub stores as an empty `COMMENTED` review: it counts as a review comment instead. |
| **Review comment** | An inline comment on code, on someone else's PR. | Inline comments on their own PR. |
| **Conversation comment** | A comment in a PR's main thread, on any PR. | Comments on issues. |
| **Lines changed** | `additions + deletions` of the member's PRs, on the day each was merged. | Shown in tables, never scored. |

Logins are matched case-insensitively. A commit made with an email GitHub
can't link to an account is credited through that member's `emails` list in
`members.toml`; adding an email later re-credits commits already synced.

**Bots** (any login ending in `[bot]`, such as `dependabot[bot]`) are stored
like anyone else but are never scored, and are hidden from every chart and
table unless you tick **Include bots**.

## The daily rollup

After each sync, tern rebuilds `daily_member_stats` for every day the sync
touched, and the whole table on every start. Each row is one member on one
local day:

| Column | From the table above |
| --- | --- |
| `commits` | Commits |
| `prs_opened` | PRs opened (non-draft) |
| `prs_merged` | PRs merged |
| `reviews_given` | Reviews given |
| `review_comments` | Review comments |
| `conv_comments` | Conversation comments |
| `lines_changed` | Lines changed |

The score, the member table and the member page's score trend read this
table. It has no repo column, so **the repo filter does not apply to them**;
the page says so.

## The productivity score

The score compares each member with the **team median for the same period**.
50 means "a typical member this period", 100 is the ceiling. It is always
shown with its breakdown: each segment of the score bar is one metric's
share of the points.

### Inputs and weights

| Metric | Weight (default) |
| --- | --- |
| PRs merged | 3 |
| Reviews given | 3 |
| Review comments | 1 |
| PRs opened | 1 |
| Commits | 0.5 |
| Conversation comments | 0.5 |

The weights add up to 9. Change them under `[score.weights]` in
`config.toml`; they apply on the next restart. Lines changed is not an input:
it swings with generated files and refactors.

### Step by step

For a date range (default: the last 14 days), for each metric *k*:

1. **Who is scored.** Members with `in_score = true` who are still active (in
   `members.toml` or currently on the GitHub team), excluding bots. Only they
   get a score, and only they count toward the median. Everyone else is still
   shown in the member table, as `not scored` or `left team`.
2. **Working days.** Weekdays (Mon-Fri) in the range, minus the member's rows
   in `away_days`. Weekend activity still counts; weekend days just aren't
   working days. A member with no working days in the range gets no score and
   stays out of the median.
3. **Adjust for leave.** Each count is scaled up to the full period:
   `x = count * weekdays_in_range / working_days`. Someone away for half the
   period who did half as much reads the same as a full-time member.
4. **Team median.** `m` = the median of the adjusted counts across the scored
   members (the average of the middle two for an even number), **floored at
   1** so a quiet period can't divide by zero or blow up tiny counts.
5. **Ratio and cap.** `s = min(x / m, cap_multiple)`.
6. **Weight and scale.** The metric contributes `50 * w * s / 9` points
   (9 = the sum of the weights).

The score is the sum of the six contributions:

```text
score = 50 * Σ(w * min(x / max(m, 1), cap_multiple)) / Σw
```

### What `cap_multiple` does

`cap_multiple` (default `2`, under `[score]` in `config.toml`) is the most a
member can be credited for one metric, as a multiple of the team median.
With the default, doing **twice the median or more** of something earns that
metric's full share and nothing beyond it.

It does two things:

- **Stops gaming by splitting.** Ten one-line PRs in place of one real PR
  stop paying once you pass double the typical count.
- **Sets the ceiling.** A member at the cap on every metric scores
  `50 * cap_multiple`, so the default caps the score at 100. With
  `cap_multiple = 3` the scale runs to 150 while 50 still means "typical";
  with `1.5` it tops out at 75 and the bars bunch closer together.

### Worked example

Ten working days, four scored members, nobody away. PRs merged (weight 3):

| Member | PRs merged | ÷ median 5 | Capped at 2 | Points (`50 * 3 * s / 9`) |
| --- | --- | --- | --- | --- |
| A | 2 | 0.4 | 0.4 | 6.7 |
| B | 4 | 0.8 | 0.8 | 13.3 |
| C | 6 | 1.2 | 1.2 | 20.0 |
| D | 20 | 4.0 | **2.0** | 33.3 (the most this metric can give) |

The median of 2, 4, 6 and 20 is 5. D merged ten times as much as A but gets
5 times the points, and anything past 10 PRs would have earned D nothing more.
Each of the other five metrics adds its own column the same way, and the
score is the row total. A member exactly at the median on all six scores 50.

If B had been away for 2 of the 10 days, B's 4 PRs would be adjusted to
`4 * 10 / 8 = 5` before the median is taken.

### Score trend (member page)

The same calculation run once per calendar week (Monday to Sunday, clipped to
the selected range), against that week's team median. The dashed line at 50
is the median; the shaded band is 40-60. A week with no working days in the
range (for example a range that starts on a Saturday) shows no point.

## Flow metrics

Shown beside the score, never part of it. These read the raw PR tables, so
the repo filter and the bots toggle both apply.

| Metric | Definition |
| --- | --- |
| **PRs merged** (tile) | PRs merged in the range. |
| **Reviews** (tile) | Reviews submitted in the range, per the counting rules above. |
| **Median cycle time** (tile) | Median of `merged_at - created_at`, in hours, over PRs merged in the range. Includes any time spent as a draft. |
| **Median time to first review** (tile) | Median of `first_review_at - created_at`, in hours, over PRs opened in the range that have been reviewed. The first review is the earliest review by anyone other than the author, including a thread reply. |
| **Size vs cycle time** (PR flow) | One dot per PR merged in the range: size (`additions + deletions`, log scale) against cycle time. The note shows the median size. |
| **Waiting for review** (PR flow) | Open, non-draft PRs with no review yet, right now, bucketed by age: under 1 day, 1-3 days, 3-7 days, over 7 days. Ignores the date range. |

Each tile compares with the previous period of the same length, the days
just before the selected range.

## Activity charts and heatmaps

These count individual events from raw activity (the `activity` view), so
the repo filter applies.

- **Activity over time**: events per member per calendar week. The type
  selector narrows it to commits, PRs (opened + merged), reviews, or comments
  (review + conversation).
- **Activity by type**: one stacked chart per member, a segment per event
  type, all sharing one y-axis so members compare directly.
- **When work happens**: events by weekday and hour. **Commits are left
  out**: their time comes from the author's machine and changes on rebase.
  PR, review and comment times are recorded by GitHub. Core hours (09-12 and
  16-18) are named in the note.

Unlike the score, these count draft PRs when they're opened, and count
inline comments on a member's own PR. They show what happened, not what is
scored.

## Known gaps

- **Rebase merges** count a PR's commits twice: once from the PR, once as
  the rebased copies on the default branch, which get new SHAs. Squash
  merges are handled.
- **Late pushes** to the default branch can be missed: GitHub filters
  commits by their own date, so a commit made Monday and pushed Wednesday
  falls before Wednesday's cursor.
- **Review load** (reviews given ÷ PRs opened), listed in the design doc, is
  not computed yet. Both inputs are in the member table.
- **Bot reviews** (for example an AI reviewer) count as a PR's first review,
  so they can shorten time to first review.
- **People who leave** stop being scored as soon as they're in neither
  `members.toml` nor the team. That also removes them from past periods'
  scores, even though their activity stays visible.
