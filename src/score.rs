//! Productivity score: each metric over the team median, capped, weighted,
//! scaled so 50 is "a typical member this period" and 100 the cap.

use chrono::{Datelike, NaiveDate, Weekday};

use crate::config::ScoreConfig;

pub fn weights(cfg: &ScoreConfig) -> [f64; 6] {
    let w = &cfg.weights;
    [
        w.prs_merged,
        w.reviews_given,
        w.review_comments,
        w.prs_opened,
        w.commits,
        w.conv_comments,
    ]
}

pub struct Input {
    pub user: String,
    pub counts: [f64; 6],
    pub working_days: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub user: String,
    /// Points each metric contributes; they sum to `total`.
    pub parts: [f64; 6],
    pub total: f64,
    /// Adjusted count ÷ team median, before the cap.
    pub ratios: [f64; 6],
    /// The team medians this member was measured against (floored at 1).
    pub medians: [f64; 6],
}

/// Mon-Fri dates in `from..=to`.
pub fn weekdays(from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    from.iter_days()
        .take_while(|d| *d <= to)
        .filter(|d| !matches!(d.weekday(), Weekday::Sat | Weekday::Sun))
        .collect()
}

pub fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    })
}

/// Scores the in-score members. Counts are scaled to the full period by each
/// member's working days, so leave does not read as low output; a member with
/// no working days in the period gets no score and stays out of the median.
pub fn score(inputs: &[Input], period_days: u32, cfg: &ScoreConfig) -> Vec<Scored> {
    let w = weights(cfg);
    let wsum: f64 = w.iter().sum();
    let adjusted: Vec<(&str, [f64; 6])> = inputs
        .iter()
        .filter(|i| i.working_days > 0)
        .map(|i| {
            let f = period_days as f64 / i.working_days as f64;
            (i.user.as_str(), i.counts.map(|c| c * f))
        })
        .collect();
    let medians: [f64; 6] = std::array::from_fn(|k| {
        median(adjusted.iter().map(|(_, x)| x[k]).collect())
            .unwrap_or(0.0)
            .max(1.0)
    });
    adjusted
        .iter()
        .map(|(user, x)| {
            let parts: [f64; 6] = std::array::from_fn(|k| {
                50.0 * w[k] * (x[k] / medians[k]).min(cfg.cap_multiple) / wsum
            });
            Scored {
                user: user.to_string(),
                parts,
                total: parts.iter().sum(),
                ratios: std::array::from_fn(|k| x[k] / medians[k]),
                medians,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Weights;

    fn cfg() -> ScoreConfig {
        ScoreConfig {
            cap_multiple: 2.0,
            weights: Weights::default(),
        }
    }

    fn input(user: &str, c: f64, days: u32) -> Input {
        Input {
            user: user.into(),
            counts: [c; 6],
            working_days: days,
        }
    }

    #[test]
    fn median_member_scores_50_and_cap_is_100() {
        let s = score(
            &[
                input("a", 2.0, 10),
                input("b", 4.0, 10),
                input("c", 100.0, 10),
            ],
            10,
            &cfg(),
        );
        assert!((s[1].total - 50.0).abs() < 1e-9, "{s:?}");
        assert!((s[2].total - 100.0).abs() < 1e-9);
        assert!((s[0].total - 25.0).abs() < 1e-9);
        // Ratios are uncapped; the median of 2, 4, 100 is 4.
        assert_eq!(s[2].ratios[0], 25.0);
        assert_eq!(s[0].medians, [4.0; 6]);
    }

    #[test]
    fn quiet_period_floors_the_median_at_one() {
        let s = score(&[input("a", 0.0, 10), input("b", 1.0, 10)], 10, &cfg());
        assert!((s[1].total - 50.0).abs() < 1e-9, "{s:?}");
        assert_eq!(s[0].total, 0.0);
    }

    #[test]
    fn away_days_scale_counts_up() {
        // b did 2 in half the working days: reads as 4, same as a.
        let s = score(
            &[input("a", 4.0, 10), input("b", 2.0, 5), input("c", 0.0, 0)],
            10,
            &cfg(),
        );
        assert_eq!(s.len(), 2);
        assert!((s[0].total - s[1].total).abs() < 1e-9);
    }

    #[test]
    fn weekdays_skip_the_weekend() {
        let d = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(weekdays(d("2026-10-16"), d("2026-10-19")).len(), 2);
    }
}
