use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono_tz::Tz;
use serde::Deserialize;

/// Points directly at the config file to load, bypassing XDG lookup.
pub const CONFIG_PATH_OVERRIDE_ENV: &str = "TERN_CONFIG_PATH";
/// Points directly at the SQLite DB file, bypassing the XDG data-dir default.
const DB_PATH_OVERRIDE_ENV: &str = "TERN_DB_PATH";

const DEFAULT_TIMEZONE: &str = "Asia/Jakarta";
const DEFAULT_DB_FILE_NAME: &str = "tern.db";
const DEFAULT_API_URL: &str = "https://api.github.com";
const DEFAULT_SYNC_INTERVAL_MINUTES: u32 = 15;
const DEFAULT_LISTEN: &str = "127.0.0.1:8088";
const DEFAULT_CAP_MULTIPLE: f64 = 2.0;

/// The resolved config used by the rest of the program. Every field is valid.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub timezone: Tz,
    pub db_path: PathBuf,
    /// `None` = no `[github]` section: the dashboard serves, nothing syncs.
    pub github: Option<GithubConfig>,
    pub listen: String,
    pub score: ScoreConfig,
}

/// `[github]` - the App tern authenticates as. Neither value is a secret;
/// the private key is (see `TERN_GITHUB_KEY_PATH`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubConfig {
    pub app_id: u64,
    pub org: String,
    pub team: String,
    /// Empty = every non-archived repo the App can see.
    pub repos: Vec<String>,
    pub sync_interval_minutes: u32,
    /// REST API root: api.github.com, or `https://<host>/api/v3` for GitHub Enterprise.
    pub api_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoreConfig {
    pub cap_multiple: f64,
    pub weights: Weights,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub prs_merged: f64,
    pub reviews_given: f64,
    pub review_comments: f64,
    pub prs_opened: f64,
    pub commits: f64,
    pub conv_comments: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Weights {
            prs_merged: 3.0,
            reviews_given: 3.0,
            review_comments: 1.0,
            prs_opened: 1.0,
            commits: 0.5,
            conv_comments: 0.5,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            timezone: DEFAULT_TIMEZONE.parse().expect("default timezone is valid"),
            db_path: xdg_default_db_path().expect("default db_path is resolvable"),
            github: None,
            listen: DEFAULT_LISTEN.to_string(),
            score: ScoreConfig {
                cap_multiple: DEFAULT_CAP_MULTIPLE,
                weights: Weights::default(),
            },
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    timezone: Option<String>,
    db_path: Option<String>,
    #[serde(default)]
    github: RawGithub,
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    score: RawScore,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGithub {
    app_id: Option<u64>,
    org: Option<String>,
    team: Option<String>,
    repos: Option<Vec<String>>,
    sync_interval_minutes: Option<u32>,
    api_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScore {
    cap_multiple: Option<f64>,
    #[serde(default)]
    weights: Weights,
}

/// `app_id`, `org` and `team` switch the integration on together; none = off.
/// A partial set is a half-finished edit, so it is an error.
fn github_from_raw(raw: RawGithub) -> Result<Option<GithubConfig>> {
    let trim = |s: Option<String>| s.map(|s| s.trim().to_string());
    let (app_id, org, team) = match (raw.app_id, trim(raw.org), trim(raw.team)) {
        (None, None, None) => return Ok(None),
        (Some(a), Some(o), Some(t)) => (a, o, t),
        _ => anyhow::bail!("github.app_id, github.org and github.team must be set together"),
    };
    if org.is_empty() || team.is_empty() {
        anyhow::bail!("github.org and github.team must not be empty");
    }
    let sync_interval_minutes = raw
        .sync_interval_minutes
        .unwrap_or(DEFAULT_SYNC_INTERVAL_MINUTES);
    if sync_interval_minutes == 0 {
        anyhow::bail!("github.sync_interval_minutes must be at least 1");
    }
    Ok(Some(GithubConfig {
        app_id,
        org,
        team,
        repos: raw.repos.unwrap_or_default(),
        sync_interval_minutes,
        api_url: raw
            .api_url
            .unwrap_or_else(|| DEFAULT_API_URL.into())
            .trim_end_matches('/')
            .to_string(),
    }))
}

impl Config {
    /// Defaults merged with the XDG-located (or explicitly pointed-at) config
    /// file. A missing file is not an error - it means "use defaults".
    pub fn load() -> Result<Config> {
        let mut raw: RawConfig = match config_file_path()? {
            Some(path) => {
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read config file {}", path.display()))?;
                toml::from_str(&contents)
                    .with_context(|| format!("failed to parse config file {}", path.display()))?
            }
            None => RawConfig::default(),
        };
        // Applied here, not in `from_raw`, to keep that env-free for tests.
        // Before `from_raw`, so the XDG default is never resolved (or needed).
        if let Ok(path) = env::var(DB_PATH_OVERRIDE_ENV) {
            raw.db_path = Some(path);
        }
        Config::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Config> {
        let tz = raw.timezone.as_deref().unwrap_or(DEFAULT_TIMEZONE);
        let timezone = tz
            .parse::<Tz>()
            .map_err(|e| anyhow::anyhow!("invalid timezone {tz:?}: {e}"))?;
        let cap_multiple = raw.score.cap_multiple.unwrap_or(DEFAULT_CAP_MULTIPLE);
        if !(cap_multiple.is_finite() && cap_multiple > 0.0) {
            anyhow::bail!("score.cap_multiple must be positive");
        }
        let w = &raw.score.weights;
        let all = [
            w.prs_merged,
            w.reviews_given,
            w.review_comments,
            w.prs_opened,
            w.commits,
            w.conv_comments,
        ];
        if all.iter().any(|v| !v.is_finite() || *v < 0.0) || all.iter().sum::<f64>() <= 0.0 {
            anyhow::bail!("score.weights must be non-negative and not all zero");
        }
        Ok(Config {
            timezone,
            db_path: match raw.db_path {
                Some(p) => PathBuf::from(p),
                None => xdg_default_db_path()?,
            },
            github: github_from_raw(raw.github)?,
            listen: raw
                .server
                .listen
                .unwrap_or_else(|| DEFAULT_LISTEN.to_string()),
            score: ScoreConfig {
                cap_multiple,
                weights: raw.score.weights,
            },
        })
    }
}

pub(crate) fn xdg_dirs() -> xdg::BaseDirectories {
    xdg::BaseDirectories::with_prefix("tern")
}

/// The config file to load, without reading it. `None` when no override env
/// var is set and no XDG config file exists.
pub fn config_file_path() -> Result<Option<PathBuf>> {
    if let Ok(path) = env::var(CONFIG_PATH_OVERRIDE_ENV) {
        return Ok(Some(PathBuf::from(path)));
    }
    Ok(xdg_dirs().find_config_file("config.toml"))
}

/// Where `tern init` should create config.toml.
pub fn config_target_path() -> Result<PathBuf> {
    if let Ok(path) = env::var(CONFIG_PATH_OVERRIDE_ENV) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        return Ok(path);
    }
    xdg_dirs()
        .place_config_file("config.toml")
        .context("failed to resolve config.toml target path (is $HOME set?)")
}

fn xdg_default_db_path() -> Result<PathBuf> {
    xdg_dirs()
        .get_data_file(DEFAULT_DB_FILE_NAME)
        .context("could not determine data directory (is $HOME set?); set TERN_DB_PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config> {
        Config::from_raw(toml::from_str(s)?)
    }

    #[test]
    fn empty_file_yields_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_weights_keep_other_defaults() {
        let c = parse("[score.weights]\ncommits = 1.5\n").unwrap();
        assert_eq!(c.score.weights.commits, 1.5);
        assert_eq!(c.score.weights.prs_merged, 3.0);
    }

    #[test]
    fn github_needs_all_three_keys() {
        assert!(parse("[github]\napp_id = 1\norg = \"o\"\n").is_err());
        let c = parse("[github]\napp_id = 1\norg = \"o\"\nteam = \"t\"\n").unwrap();
        assert_eq!(c.github.unwrap().sync_interval_minutes, 15);
    }

    #[test]
    fn bad_timezone_and_weights_are_rejected() {
        assert!(parse("timezone = \"Mars/Base\"").is_err());
        assert!(parse("[score.weights]\ncommits = -1\n").is_err());
        assert!(parse("[score]\ncap_multiple = nan\n").is_err());
        // A top-level key written after a [table] header lands in that table.
        assert!(parse("[server]\ntimezone = \"UTC\"\n").is_err());
    }
}
