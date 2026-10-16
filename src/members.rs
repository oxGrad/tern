use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Deserialize;

use crate::config::xdg_dirs;

pub(crate) const MEMBERS_PATH_OVERRIDE_ENV: &str = "TERN_MEMBERS_PATH";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersFile {
    #[serde(default)]
    members: Vec<MemberSeed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberSeed {
    github_user: String,
    name: String,
    /// `false` keeps them out of the score and the team median.
    #[serde(default = "yes")]
    in_score: bool,
    /// Unlinked commit emails that belong to this member.
    #[serde(default)]
    emails: Vec<String>,
}

fn yes() -> bool {
    true
}

/// What `tern validate` reports about the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RosterSummary {
    pub members: usize,
    pub in_score: usize,
}

/// GitHub's username rules: 1-39 ASCII letters, digits or hyphens, not
/// starting with a hyphen.
fn is_valid_github_user(name: &str) -> bool {
    (1..=39).contains(&name.len())
        && !name.starts_with('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The roster file to read, without reading it. `None` = nothing to seed.
pub fn resolve_path() -> Option<PathBuf> {
    if let Ok(path) = env::var(MEMBERS_PATH_OVERRIDE_ENV) {
        return Some(PathBuf::from(path));
    }
    xdg_dirs().find_config_file("members.toml")
}

/// Where `tern init` should create members.toml.
pub fn target_path() -> Result<PathBuf> {
    if let Ok(path) = env::var(MEMBERS_PATH_OVERRIDE_ENV) {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        return Ok(path);
    }
    xdg_dirs()
        .place_config_file("members.toml")
        .context("failed to resolve members.toml target path (is $HOME set?)")
}

fn load_and_validate() -> Result<Option<MembersFile>> {
    let Some(path) = resolve_path() else {
        return Ok(None);
    };
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read members file {}", path.display()))?;
    let file: MembersFile = toml::from_str(&contents)
        .with_context(|| format!("failed to parse members file {}", path.display()))?;
    check(&file)?;
    Ok(Some(file))
}

fn check(file: &MembersFile) -> Result<()> {
    let mut users = std::collections::HashSet::new();
    let mut emails = std::collections::HashSet::new();
    for m in &file.members {
        if !is_valid_github_user(&m.github_user) {
            let hint = if m.github_user.starts_with('@') {
                " - drop the leading @"
            } else {
                ""
            };
            bail!(
                "invalid github_user {:?} for member {:?}: expected a GitHub username{hint}",
                m.github_user,
                m.name
            );
        }
        // GitHub usernames are case-insensitive.
        if !users.insert(m.github_user.to_ascii_lowercase()) {
            bail!(
                "github_user {:?} appears twice in members.toml",
                m.github_user
            );
        }
        for e in &m.emails {
            if !e.contains('@') {
                bail!("invalid email {e:?} for member {:?}", m.name);
            }
            if !emails.insert(e.to_ascii_lowercase()) {
                bail!("email {e:?} is listed for more than one member");
            }
        }
    }
    Ok(())
}

/// Parses and validates members.toml without touching the DB.
pub fn validate() -> Result<RosterSummary> {
    Ok(
        load_and_validate()?.map_or_else(RosterSummary::default, |f| RosterSummary {
            members: f.members.len(),
            in_score: f.members.iter().filter(|m| m.in_score).count(),
        }),
    )
}

/// Upserts members.toml into `members` and replaces `member_emails`, atomically.
/// Never deletes member rows: their activity history references them, and the
/// team endpoint adds members too. Returns the number of roster entries.
pub fn seed(conn: &Connection) -> Result<usize> {
    let members = load_and_validate()?.map_or_else(Vec::new, |f| f.members);
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM member_emails", [])?;
    tx.execute("UPDATE members SET in_toml = FALSE", [])?;
    for m in &members {
        // Lowercase, like every login the sync stores: GitHub's casing may differ.
        let user = m.github_user.to_ascii_lowercase();
        tx.execute(
            "INSERT INTO members (github_user, name, in_score, in_toml) VALUES (?1, ?2, ?3, TRUE)
             ON CONFLICT(github_user) DO UPDATE SET name = ?2, in_score = ?3, in_toml = TRUE",
            rusqlite::params![user, m.name, m.in_score],
        )
        .with_context(|| format!("failed to insert member {:?}", m.github_user))?;
        for e in &m.emails {
            tx.execute(
                "INSERT INTO member_emails (email, github_user) VALUES (?1, ?2)",
                rusqlite::params![e.to_ascii_lowercase(), user],
            )?;
        }
    }
    // Emails added since a commit was synced: link the commits already stored.
    tx.execute(
        "UPDATE commits SET author_login =
             (SELECT github_user FROM member_emails WHERE email = lower(commits.author_email))
         WHERE author_login IS NULL
           AND lower(author_email) IN (SELECT email FROM member_emails)",
        [],
    )?;
    tx.commit()?;
    Ok(members.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;

    fn seed_from(conn: &Connection, contents: &str) -> Result<usize> {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("members.toml");
        std::fs::write(&path, contents).unwrap();
        // SAFETY: held under ENV_LOCK; removed before returning.
        unsafe { env::set_var(MEMBERS_PATH_OVERRIDE_ENV, &path) };
        let r = seed(conn);
        unsafe { env::remove_var(MEMBERS_PATH_OVERRIDE_ENV) };
        r
    }

    fn db() -> Connection {
        crate::db::open(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    #[test]
    fn seed_upserts_and_replaces_emails() {
        let conn = db();
        let toml = "[[members]]\ngithub_user = \"budi\"\nname = \"Budi\"\nemails = [\"B@x.dev\"]\n";
        assert_eq!(seed_from(&conn, toml).unwrap(), 1);
        let toml2 = "[[members]]\ngithub_user = \"budi\"\nname = \"Budi S\"\nin_score = false\n";
        seed_from(&conn, toml2).unwrap();
        let (name, in_score): (String, bool) = conn
            .query_row("SELECT name, in_score FROM members", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((name.as_str(), in_score), ("Budi S", false));
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM member_emails", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn seed_lowercases_logins_and_links_stored_commits_by_email() {
        let conn = db();
        conn.execute_batch(
            "INSERT INTO repos (id, full_name, default_branch) VALUES (1, 'o/r', 'main');
             INSERT INTO commits (sha, repo_id, author_email, authored_at, committed_at)
             VALUES ('s', 1, 'Me@X.dev', '2026-10-01T00:00:00Z', '2026-10-01T00:00:00Z');",
        )
        .unwrap();
        let toml =
            "[[members]]\ngithub_user = \"BudiS\"\nname = \"Budi\"\nemails = [\"me@x.dev\"]\n";
        seed_from(&conn, toml).unwrap();
        let (user, login): (String, String) = conn
            .query_row(
                "SELECT m.github_user, c.author_login FROM members m, commits c",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((user.as_str(), login.as_str()), ("budis", "budis"));
    }

    #[test]
    fn rejects_bad_and_duplicate_users() {
        let conn = db();
        assert!(seed_from(&conn, "[[members]]\ngithub_user = \"@x\"\nname = \"X\"\n").is_err());
        let dup = "[[members]]\ngithub_user = \"A\"\nname = \"1\"\n\
                   [[members]]\ngithub_user = \"a\"\nname = \"2\"\n";
        assert!(seed_from(&conn, dup).is_err());
    }
}
