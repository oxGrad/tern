//! GitHub App integration: `auth` (key parsing, JWT, token), `client` (App
//! checks plus the authenticated REST client the sync uses).

pub mod auth;
pub mod client;

use std::env;

/// Path to the plaintext App private key `.pem` (chmod 600, outside the repo).
pub const KEY_PATH_ENV: &str = "TERN_GITHUB_KEY_PATH";

/// Reads the App private key PEM from `TERN_GITHUB_KEY_PATH`. `None` when the
/// variable is unset, or the file is missing or empty.
pub fn app_key_pem() -> Option<String> {
    std::fs::read_to_string(env::var(KEY_PATH_ENV).ok()?)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_key_pem_reads_the_env_path_and_ignores_empty_files() {
        let _guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.pem");
        // SAFETY: held under ENV_LOCK; removed before returning.
        unsafe { env::set_var(KEY_PATH_ENV, &key) };
        assert_eq!(app_key_pem(), None);
        std::fs::write(&key, "  \n").unwrap();
        assert_eq!(app_key_pem(), None);
        std::fs::write(&key, "from-env\n").unwrap();
        assert_eq!(app_key_pem().as_deref(), Some("from-env\n"));
        unsafe { env::remove_var(KEY_PATH_ENV) };
        assert_eq!(app_key_pem(), None);
    }
}
