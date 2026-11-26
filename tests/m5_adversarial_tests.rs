use mesh_wick::auth::{
    remove_credential, resolve_credential, set_credential, CredentialProvider, CredentialSource,
    CredentialStorage,
};
use mesh_wick::context::clear_state_dir;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static ADV_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

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
        let seq = ADV_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/wick-t-adv-{pid}-{seq}"));
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

#[cfg(unix)]
#[test]
fn adversarial_auth_rejects_insecure_modes_and_symlinks_across_all_ops() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let _env = EnvGuard::new(&[("BRAVE_API_KEY", None), ("TAVILY_API_KEY", None)]);
    let dir = TempStateDir::new();
    let creds_path = dir.path.join("credentials.json");

    // Any group/other bit (0o644, 0o640, 0o604) must fail loudly on read, set, and remove.
    for mode in [0o644u32, 0o640, 0o604] {
        std::fs::write(&creds_path, r#"{"brave":"secret-mode-test"}"#).unwrap();
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(mode)).unwrap();

        let err = resolve_credential(&dir.path, CredentialProvider::Brave).unwrap_err();
        assert!(
            !err.to_string().contains("secret-mode-test"),
            "error must not echo secret: {err}"
        );
        assert!(set_credential(
            &dir.path,
            CredentialProvider::Tavily,
            "tavily-new",
            CredentialStorage::File
        )
        .is_err());
        assert!(remove_credential(
            &dir.path,
            CredentialProvider::Brave,
            CredentialStorage::File
        )
        .is_err());
    }

    // Symlink credentials.json must fail via O_NOFOLLOW even if target is 0o600.
    let target = dir.path.join("real.json");
    std::fs::write(&target, r#"{"brave":"symlink-secret"}"#).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&creds_path).unwrap();
    symlink(&target, &creds_path).unwrap();

    assert!(resolve_credential(&dir.path, CredentialProvider::Brave).is_err());
    assert!(remove_credential(
        &dir.path,
        CredentialProvider::Brave,
        CredentialStorage::File
    )
    .is_err());

    // Symlink credentials.lock must also be rejected via O_NOFOLLOW.
    std::fs::remove_file(&creds_path).unwrap();
    let lock_path = dir.path.join("credentials.lock");
    let _ = std::fs::remove_file(&lock_path);
    symlink(&target, &lock_path).unwrap();
    assert!(set_credential(
        &dir.path,
        CredentialProvider::Brave,
        "after-symlink-lock",
        CredentialStorage::File
    )
    .is_err());
}

#[test]
fn adversarial_auth_whitespace_env_falls_through_and_concurrent_writes_preserve_all_providers() {
    let _env = EnvGuard::new(&[
        ("BRAVE_API_KEY", Some("   \r\n  ")),
        ("TAVILY_API_KEY", None),
        ("SERPER_API_KEY", None),
        ("EXA_API_KEY", None),
        ("SEARXNG_URL", None),
    ]);
    let dir = TempStateDir::new();

    let providers = [
        (CredentialProvider::Brave, "brave-concurrent-key"),
        (CredentialProvider::Tavily, "tavily-concurrent-key"),
        (CredentialProvider::Serper, "serper-concurrent-key"),
        (CredentialProvider::Exa, "exa-concurrent-key"),
        (CredentialProvider::Searxng, "http://127.0.0.1:8888"),
    ];

    std::thread::scope(|s| {
        for (provider, value) in providers {
            let path = dir.path.clone();
            s.spawn(move || {
                let source =
                    set_credential(&path, provider, value, CredentialStorage::File).unwrap();
                assert_eq!(source, CredentialSource::File);
            });
        }
    });

    // Whitespace-only BRAVE_API_KEY must fall through to File tier.
    for (provider, expected) in providers {
        let resolved = resolve_credential(&dir.path, provider)
            .unwrap()
            .expect("provider must be preserved across concurrent writes");
        assert_eq!(resolved.source, CredentialSource::File);
        assert_eq!(resolved.value.as_str(), expected);
    }
}

#[test]
fn adversarial_session_clear_preserves_credentials_and_lock_files() {
    let _env = EnvGuard::new(&[("TAVILY_API_KEY", None)]);
    let dir = TempStateDir::new();

    set_credential(
        &dir.path,
        CredentialProvider::Tavily,
        "tavily-survives-clear",
        CredentialStorage::File,
    )
    .unwrap();

    let creds_path = dir.path.join("credentials.json");
    let lock_path = dir.path.join("credentials.lock");
    let rules_path = dir.path.join("site-rules.json");
    let search_state_path = dir.path.join("search-state.json");
    std::fs::write(&rules_path, "{}").unwrap();
    std::fs::write(&search_state_path, r#"{"tavily":{"month_requests":7}}"#).unwrap();

    clear_state_dir(&dir.path).unwrap();

    assert!(creds_path.exists(), "credentials.json must survive clear");
    assert!(lock_path.exists(), "credentials.lock must survive clear");
    assert!(rules_path.exists(), "site-rules.json must survive clear");
    assert!(
        !search_state_path.exists(),
        "search-state.json must be removed by clear"
    );

    let resolved = resolve_credential(&dir.path, CredentialProvider::Tavily)
        .unwrap()
        .expect("credential must still resolve after clear");
    assert_eq!(resolved.value.as_str(), "tavily-survives-clear");
}
