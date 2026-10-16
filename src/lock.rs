use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result};

/// Holds an exclusive OS-level lock (`std::fs::File::try_lock`, `flock` on
/// Unix) on the file `acquire` opened, for as long as this guard is alive.
/// The lock is released automatically when the file handle closes - on
/// drop, or on any process exit including a crash or `kill -9` - so
/// there's never a stale lock file left behind to clean up by hand, unlike
/// a PID-file scheme.
pub struct SingletonGuard {
    _file: File,
}

/// Ensures only one `tern` process runs against a given data directory at a
/// time, so two daemons never race the sync worker on one database.
pub fn acquire(lock_path: &Path) -> Result<SingletonGuard> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(SingletonGuard { _file: file }),
        Err(TryLockError::WouldBlock) => Err(anyhow::anyhow!(
            "another tern instance is already running (lock held at {})",
            lock_path.display()
        )),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("failed to lock {}", lock_path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_the_same_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tern.lock");

        let _first = acquire(&path).unwrap();
        let second = acquire(&path);

        assert!(second.is_err());
    }

    #[test]
    fn dropping_the_guard_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tern.lock");

        let first = acquire(&path).unwrap();
        drop(first);

        assert!(acquire(&path).is_ok());
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("tern.lock");

        assert!(acquire(&path).is_ok());
    }
}
