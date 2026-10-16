mod config;
mod db;
mod github;
mod init;
mod lock;
mod maintenance;
mod members;
mod score;
mod status;
mod sync;
mod web;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use config::Config;

#[cfg(test)]
pub(crate) mod test_support {
    /// Shared by every test that mutates process env vars (`TERN_*`):
    /// `cargo test` runs tests in parallel within one process.
    pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes `ENV_LOCK` even if an earlier test panicked while holding it.
    pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod mock_github;

#[derive(Parser)]
#[command(version = env!("TERN_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write default config.toml and members.toml templates if missing
    Init,
    /// Check config.toml, members.toml and the key path without opening the DB
    Validate,
    /// GitHub App ping and the last sync run
    Status,
    /// Database maintenance
    Maintenance {
        #[command(subcommand)]
        action: MaintenanceCommand,
    },
    /// One-off history pull from a start date
    Backfill {
        /// First day to pull, e.g. 2026-01-01
        #[arg(long, value_name = "YYYY-MM-DD")]
        since: NaiveDate,
    },
    /// Run one sync, then exit
    Sync {
        #[arg(long)]
        once: bool,
    },
}

#[derive(Subcommand)]
enum MaintenanceCommand {
    /// Prune sync_runs rows older than 90 days, then VACUUM
    Run,
}

/// How often the daemon's ticker runs maintenance (no systemd timer here).
const MAINTENANCE_EVERY: Duration = Duration::from_secs(7 * 24 * 3600);

fn run_validate() -> anyhow::Result<()> {
    match config::config_file_path()? {
        Some(path) => println!("config.toml: ok ({})", path.display()),
        None => println!("config.toml: not configured (using defaults) - run: tern init"),
    }
    let config = Config::load()?;
    match &config.github {
        Some(gh) => println!(
            "github: configured (app {} on {}, team {}, sync every {} min)",
            gh.app_id, gh.org, gh.team, gh.sync_interval_minutes
        ),
        None => {
            println!("github: not configured - set [github] app_id, org and team in config.toml")
        }
    }
    match std::env::var(github::KEY_PATH_ENV) {
        Err(_) => println!(
            "key: not set - run: export {}=<path to app .pem>",
            github::KEY_PATH_ENV
        ),
        Ok(path) => match github::app_key_pem() {
            Some(pem) => match github::auth::AppKey::from_pem(&pem) {
                Ok(_) => println!("key: ok ({path})"),
                Err(e) => println!("key: failed - {e:#} ({path})"),
            },
            None => println!(
                "key: failed - {path} is missing or empty - set {} to the App's .pem",
                github::KEY_PATH_ENV
            ),
        },
    }
    match members::resolve_path() {
        Some(path) => {
            let r = members::validate()?;
            println!(
                "members.toml: ok ({} member{}, {} in score - {})",
                r.members,
                if r.members == 1 { "" } else { "s" },
                r.in_score,
                path.display()
            );
        }
        None => println!("members.toml: not configured (no roster to seed) - run: tern init"),
    }
    Ok(())
}

fn run_maintenance(config: &Config, conn: &rusqlite::Connection) -> anyhow::Result<()> {
    let _singleton = lock::acquire(&config.db_path.with_extension("maintenance.lock"))?;
    let pruned = maintenance::run(conn)?;
    std::fs::write(stamp_path(&config.db_path), "")?;
    println!("maintenance done: {pruned} old sync_runs row(s) pruned, database vacuumed");
    Ok(())
}

/// Its mtime records the last maintenance run.
fn stamp_path(db: &Path) -> std::path::PathBuf {
    db.with_extension("maintenance.stamp")
}

fn maintenance_due(db: &Path) -> bool {
    std::fs::metadata(stamp_path(db))
        .and_then(|m| m.modified())
        .map_or(true, |t| {
            SystemTime::now().duration_since(t).unwrap_or_default() >= MAINTENANCE_EVERY
        })
}

fn open_state(config: Config) -> anyhow::Result<(web::AppState, lock::SingletonGuard)> {
    // Guards against two tern processes racing the sync on one database.
    let guard = lock::acquire(&config.db_path.with_extension("lock"))?;
    let conn = db::open(&config.db_path)?;
    members::seed(&conn)?;
    sync::rollup::rebuild_all(&conn, config.timezone)?;
    Ok((
        web::AppState {
            db: Arc::new(Mutex::new(conn)),
            config: Arc::new(config),
            sync: Arc::new(tokio::sync::Mutex::new(())),
        },
        guard,
    ))
}

async fn run_sync(since: Option<NaiveDate>) -> anyhow::Result<()> {
    let (state, _guard) = open_state(Config::load()?)?;
    let s = sync::run(&state.db, &state.config, since).await?;
    println!(
        "sync {} - {} repo{}, {} requests, {} remaining",
        s.status,
        s.repos,
        if s.repos == 1 { "" } else { "s" },
        s.requests,
        s.remaining.map_or("?".into(), |r| r.to_string())
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Init) => return init::run(),
        Some(Command::Validate) => return run_validate(),
        Some(Command::Status) => return status::run().await,
        Some(Command::Maintenance {
            action: MaintenanceCommand::Run,
        }) => {
            let config = Config::load()?;
            let conn = db::open(&config.db_path)?;
            return run_maintenance(&config, &conn);
        }
        Some(Command::Backfill { since }) => return run_sync(Some(since)).await,
        Some(Command::Sync { once: true }) => return run_sync(None).await,
        Some(Command::Sync { once: false }) => {
            anyhow::bail!("tern sync needs --once - the bare `tern` runs the sync ticker")
        }
        None => {}
    }

    let config = Config::load()?;
    let (state, _guard) = open_state(config)?;
    let c = &state.config;
    let seeded: i64 = state.db.lock().unwrap().query_row(
        "SELECT COUNT(*) FROM members WHERE is_bot = 0",
        [],
        |r| r.get(0),
    )?;
    println!("tern effective config:");
    println!("  timezone:                     {}", c.timezone);
    match &c.github {
        Some(gh) => {
            println!("  org / team:                   {} / {}", gh.org, gh.team);
            println!(
                "  sync_interval_minutes:        {}",
                gh.sync_interval_minutes
            );
        }
        None => println!(
            "  github:                       not configured - set [github] in config.toml, dashboard only"
        ),
    }
    println!("  listen:                       {}", c.listen);
    println!("  score cap_multiple:           {}", c.score.cap_multiple);
    println!("  db_path:                      {}", c.db_path.display());
    println!("  members seeded:               {seeded}");

    if let Some(gh) = c.github.clone() {
        let st = state.clone();
        tokio::spawn(async move {
            // The first tick fires immediately: one sync on startup.
            let mut tick =
                tokio::time::interval(Duration::from_secs(gh.sync_interval_minutes as u64 * 60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match web::sync_once(&st).await {
                    Ok(Some(s)) if s.status != "ok" => {
                        eprintln!("failed to sync fully: run was {}", s.status)
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("failed to sync: {e:#}"),
                }
                if maintenance_due(&st.config.db_path) {
                    let conn = st.db.lock().unwrap();
                    if let Err(e) = run_maintenance(&st.config, &conn) {
                        eprintln!("failed to run maintenance: {e:#}");
                    }
                }
            }
        });
    }

    let listen = c.listen.clone();
    println!("dashboard on http://{listen}");
    web::serve(state)
        .await
        .with_context(|| format!("failed to serve the dashboard on {listen}"))
}
