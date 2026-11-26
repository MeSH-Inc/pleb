use mesh_wick::auth::{
    all_credential_statuses, remove_credential, resolve_credential, set_credential,
    CredentialProvider, CredentialSource, CredentialStorage, ResolvedCredential, SecretString,
    MAX_SECRET_BYTES,
};
use mesh_wick::context::{clear_state_dir, ensure_private_dir};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut saved = Vec::with_capacity(vars.len() + 1);
        saved.push(("WICK_NO_KEYCHAIN", std::env::var("WICK_NO_KEYCHAIN").ok()));
        std::env::set_var("WICK_NO_KEYCHAIN", "1");

        for &(key, value) in vars {
            saved.push((key, std::env::var(key).ok()));
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { _lock: lock, saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.saved.iter().rev() {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

struct TempStateDir {
    path: PathBuf,
}

impl TempStateDir {
    fn new() -> Self {
        let pid = std::process::id();
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/wick-t-{pid}-{seq}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp state dir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("chmod 0700 temp state dir");
        }
        Self { path }
    }
}

impl Drop for TempStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn env_takes_precedence_over_file_and_remove_is_idempotent() {
    let dir = TempStateDir::new();
    {
        let _env = EnvGuard::new(&[
            ("BRAVE_API_KEY", None),
            ("TAVILY_API_KEY", None),
            ("SERPER_API_KEY", None),
            ("EXA_API_KEY", None),
            ("SEARXNG_URL", None),
        ]);

        assert!(resolve_credential(&dir.path, CredentialProvider::Brave)
            .unwrap()
            .is_none());

        let stored = set_credential(
            &dir.path,
            CredentialProvider::Brave,
            "  brave-file-secret \n",
            CredentialStorage::Auto,
        )
        .unwrap();
        assert_eq!(stored, CredentialSource::File);

        let resolved = resolve_credential(&dir.path, CredentialProvider::Brave)
            .unwrap()
            .expect("file credential should resolve");
        assert_eq!(resolved.source, CredentialSource::File);
        assert_eq!(resolved.value.as_str(), "brave-file-secret");

        let statuses = all_credential_statuses(&dir.path).unwrap();
        let brave_status = statuses
            .iter()
            .find(|s| s.provider == CredentialProvider::Brave)
            .unwrap();
        assert!(brave_status.configured);
        assert_eq!(brave_status.source, Some(CredentialSource::File));
    }

    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", Some("brave-env-override"))]);
        let resolved = resolve_credential(&dir.path, CredentialProvider::Brave)
            .unwrap()
            .expect("env credential should resolve");
        assert_eq!(resolved.source, CredentialSource::Environment);
        assert_eq!(resolved.value.as_str(), "brave-env-override");
    }

    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", None)]);
        let removed_first = remove_credential(
            &dir.path,
            CredentialProvider::Brave,
            CredentialStorage::Auto,
        )
        .unwrap();
        assert_eq!(removed_first, vec![CredentialSource::File]);

        let removed_second = remove_credential(
            &dir.path,
            CredentialProvider::Brave,
            CredentialStorage::Auto,
        )
        .unwrap();
        assert!(removed_second.is_empty());

        assert!(resolve_credential(&dir.path, CredentialProvider::Brave)
            .unwrap()
            .is_none());
    }
}

#[cfg(unix)]
#[test]
fn rejects_loose_permissions_and_symlinks() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let _env = EnvGuard::new(&[("BRAVE_API_KEY", None)]);
    let dir = TempStateDir::new();

    // 0o644 credentials.json must be rejected.
    let creds_path = dir.path.join("credentials.json");
    std::fs::write(&creds_path, r#"{"brave":"secret-123"}"#).unwrap();
    std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let err = resolve_credential(&dir.path, CredentialProvider::Brave).unwrap_err();
    assert!(
        err.to_string().contains("insecure permissions"),
        "unexpected error: {err}"
    );
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Tavily,
        "tavily-secret",
        CredentialStorage::File
    )
    .is_err());

    // Symlink credentials.json must be rejected via O_NOFOLLOW.
    let target = dir.path.join("real-creds.json");
    std::fs::write(&target, r#"{"brave":"secret-123"}"#).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&creds_path).unwrap();
    symlink(&target, &creds_path).unwrap();

    assert!(resolve_credential(&dir.path, CredentialProvider::Brave).is_err());
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Brave,
        "new-secret",
        CredentialStorage::File
    )
    .is_err());

    // Symlink state_dir must be rejected by ensure_private_dir.
    let symlink_state_dir = PathBuf::from(format!("{}-link", dir.path.display()));
    let _ = std::fs::remove_file(&symlink_state_dir);
    symlink(&dir.path, &symlink_state_dir).unwrap();
    let dir_err = ensure_private_dir(&symlink_state_dir).unwrap_err();
    let _ = std::fs::remove_file(&symlink_state_dir);
    assert!(
        dir_err.to_string().contains("symlink"),
        "unexpected error: {dir_err}"
    );
}

#[test]
fn rejects_corrupt_json_without_overwriting() {
    let _env = EnvGuard::new(&[("BRAVE_API_KEY", None)]);
    let dir = TempStateDir::new();
    let creds_path = dir.path.join("credentials.json");
    let corrupt_contents = "{not valid json";
    std::fs::write(&creds_path, corrupt_contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    assert!(resolve_credential(&dir.path, CredentialProvider::Brave).is_err());
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Brave,
        "valid-secret",
        CredentialStorage::File
    )
    .is_err());
    assert!(remove_credential(
        &dir.path,
        CredentialProvider::Brave,
        CredentialStorage::File
    )
    .is_err());

    assert_eq!(
        std::fs::read_to_string(&creds_path).unwrap(),
        corrupt_contents
    );
}

#[test]
fn validates_secrets_and_searxng_urls_without_echoing_values() {
    let _env = EnvGuard::new(&[("BRAVE_API_KEY", None), ("SEARXNG_URL", None)]);
    let dir = TempStateDir::new();

    for bad_secret in [
        "",
        "   \r\n ",
        "bad\x07bell",
        "bad\nline",
        "bad\ttab",
        "bad\x00nul",
        "non-ascii-é",
    ] {
        let err = set_credential(
            &dir.path,
            CredentialProvider::Brave,
            bad_secret,
            CredentialStorage::File,
        )
        .unwrap_err();
        if !bad_secret.trim().is_empty() {
            assert!(
                !err.to_string().contains(bad_secret),
                "error echoed secret: {err}"
            );
        }
    }

    let too_long = "a".repeat(MAX_SECRET_BYTES + 1);
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Brave,
        &too_long,
        CredentialStorage::File
    )
    .is_err());

    let max_ok = "a".repeat(MAX_SECRET_BYTES);
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Brave,
        &max_ok,
        CredentialStorage::File
    )
    .is_ok());

    for bad_url in [
        "ftp://127.0.0.1:8888",
        "file:///etc/passwd",
        "http://user:topsecret@127.0.0.1:8888",
        "https://user@search.example.com",
        "http://127.0.0.1:8888/search?q=leak",
        "http://127.0.0.1:8888/#frag",
        "not-a-url",
    ] {
        let err = set_credential(
            &dir.path,
            CredentialProvider::Searxng,
            bad_url,
            CredentialStorage::File,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(bad_url), "error echoed bad URL: {msg}");
        assert!(
            !msg.contains("topsecret"),
            "error echoed userinfo password: {msg}"
        );
    }

    assert!(set_credential(
        &dir.path,
        CredentialProvider::Searxng,
        "http://127.0.0.1:8888",
        CredentialStorage::File
    )
    .is_ok());

    // Env SEARXNG_URL is also validated at resolve_credential.
    drop(_env);
    let _env_bad_searx = EnvGuard::new(&[("SEARXNG_URL", Some("http://user:pass@127.0.0.1:8888"))]);
    let err = resolve_credential(&dir.path, CredentialProvider::Searxng).unwrap_err();
    assert!(!err.to_string().contains("pass"));
}

#[test]
fn secret_string_and_resolved_credential_redact_debug() {
    let secret_value = "super-sensitive-api-key-987654";
    let secret = SecretString::new(secret_value);
    assert_eq!(format!("{secret:?}"), "[REDACTED]");
    assert!(!format!("{secret:?}").contains(secret_value));

    let resolved = ResolvedCredential {
        provider: CredentialProvider::Tavily,
        value: secret,
        source: CredentialSource::File,
    };
    let debug_str = format!("{resolved:?}");
    assert!(debug_str.contains("[REDACTED]"));
    assert!(!debug_str.contains(secret_value));
}

#[test]
fn no_keychain_env_rejects_explicit_keychain_storage() {
    let _env = EnvGuard::new(&[("EXA_API_KEY", None)]);
    let dir = TempStateDir::new();

    let set_err = set_credential(
        &dir.path,
        CredentialProvider::Exa,
        "exa-secret",
        CredentialStorage::Keychain,
    )
    .unwrap_err();
    assert!(
        set_err.to_string().contains("WICK_NO_KEYCHAIN=1")
            || set_err.to_string().contains("only supported on macOS"),
        "unexpected error: {set_err}"
    );

    let rm_err = remove_credential(
        &dir.path,
        CredentialProvider::Exa,
        CredentialStorage::Keychain,
    )
    .unwrap_err();
    assert!(
        rm_err.to_string().contains("WICK_NO_KEYCHAIN=1")
            || rm_err.to_string().contains("only supported on macOS"),
        "unexpected error: {rm_err}"
    );
}

#[test]
fn clear_state_dir_preserves_credentials_and_clears_search_state() {
    let _env = EnvGuard::new(&[("BRAVE_API_KEY", None)]);
    let dir = TempStateDir::new();

    set_credential(
        &dir.path,
        CredentialProvider::Brave,
        "keep-this-secret",
        CredentialStorage::File,
    )
    .unwrap();

    let creds_path = dir.path.join("credentials.json");
    let creds_lock_path = dir.path.join("credentials.lock");
    let site_rules_path = dir.path.join("site-rules.json");
    let search_state_path = dir.path.join("search-state.json");
    let site_cache_path = dir.path.join("site-cache.json");

    let creds_before = std::fs::read_to_string(&creds_path).unwrap();
    assert!(creds_lock_path.exists());

    std::fs::write(&site_rules_path, r#"{"rules":[]}"#).unwrap();
    std::fs::write(&search_state_path, r#"{"brave":{"month_requests":5}}"#).unwrap();
    std::fs::write(&site_cache_path, r#"{}"#).unwrap();
    std::fs::create_dir_all(dir.path.join("data")).unwrap();
    std::fs::write(dir.path.join("data/cookies"), b"abc").unwrap();
    std::fs::create_dir_all(dir.path.join("cef-cache")).unwrap();
    std::fs::write(dir.path.join("cef-cache/item"), b"def").unwrap();
    std::fs::create_dir_all(dir.path.join("artifacts")).unwrap();
    std::fs::write(dir.path.join("artifacts/file.bin"), b"ghi").unwrap();

    clear_state_dir(&dir.path).unwrap();

    // Preserved files
    assert!(creds_path.exists(), "credentials.json must be preserved");
    assert_eq!(std::fs::read_to_string(&creds_path).unwrap(), creds_before);
    assert!(
        creds_lock_path.exists(),
        "credentials.lock must be preserved"
    );
    assert!(
        site_rules_path.exists(),
        "site-rules.json must be preserved"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials.json must remain 0o600");
    }

    // Cleared files/dirs
    assert!(
        !search_state_path.exists(),
        "search-state.json must be cleared"
    );
    assert!(!site_cache_path.exists(), "site-cache.json must be cleared");
    assert!(!dir.path.join("cef-cache").exists());
    assert!(!dir.path.join("artifacts").exists());
    assert!(dir.path.join("data").is_dir());
    assert!(!dir.path.join("data/cookies").exists());

    let resolved = resolve_credential(&dir.path, CredentialProvider::Brave)
        .unwrap()
        .expect("credential should still resolve after clear_state_dir");
    assert_eq!(resolved.value.as_str(), "keep-this-secret");
}
