use anyhow::Result;
use rusqlite::Connection;

/// A default, not a requirement: sync_runs is bookkeeping only.
const RETENTION_DAYS: u32 = 90;

/// Prunes old `sync_runs` rows and reclaims space with `VACUUM`. Raw activity
/// and `daily_member_stats` are never touched - they are the history the
/// score and trends depend on. Returns the number of rows pruned.
pub fn run(conn: &Connection) -> Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM sync_runs WHERE started_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)",
        [format!("-{RETENTION_DAYS} days")],
    )?;
    // VACUUM can't run inside a transaction; the DELETE above autocommitted.
    conn.execute("VACUUM", [])?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_only_old_sync_runs() {
        let conn = crate::db::open(&tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO sync_runs (started_at) VALUES ('2020-01-01T00:00:00Z');
             INSERT INTO sync_runs (started_at) VALUES (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));",
        )
        .unwrap();
        assert_eq!(run(&conn).unwrap(), 1);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }
}
