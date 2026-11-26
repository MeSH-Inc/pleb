use mesh_wick::auth::{
    remove_credential, resolve_credential, set_credential, CredentialProvider, CredentialSource,
    CredentialStorage,
};
use mesh_wick::context::{clear_state_dir, RuntimeConfig, RuntimeContext};
use mesh_wick::search::{
    civil_from_days, days_from_civil, parse_retry_after, provider_health_summary,
    search_with_endpoints, ProviderEndpoints, SearchMode, SearchOptions, SearchProvider,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
static ADV_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

struct EnvGuard {
    _lock: AsyncMutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.blocking_lock();
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

struct SearchEnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl SearchEnvGuard {
    fn new(overrides: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::new();
        for key in [
            "BRAVE_API_KEY",
            "TAVILY_API_KEY",
            "SERPER_API_KEY",
            "EXA_API_KEY",
            "SEARXNG_URL",
            "WICK_NO_KEYCHAIN",
            "WICK_NO_HOME_CEF",
        ] {
            saved.push((key, std::env::var(key).ok()));
            std::env::remove_var(key);
        }
        std::env::set_var("WICK_NO_KEYCHAIN", "1");
        std::env::set_var("WICK_NO_HOME_CEF", "1");
        for &(k, v) in overrides {
            std::env::set_var(k, v);
        }
        Self { saved }
    }
}

impl Drop for SearchEnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
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

#[tokio::test]
async fn adversarial_cli_zero_secret_leakage_in_json_and_verbose_logs() {
    let wick_bin = env!("CARGO_BIN_EXE_wick");
    let state_dir = TempStateDir::new();

    let brave_secret = "BSA-adversarial-top-secret-918273645";
    let tavily_secret = "tvly-adversarial-secret-5544332211";
    let searxng_url = "http://127.0.0.1:19999";

    // 1. Positional secret argument must be rejected by clap (exit 2).
    let positional = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir.path)
        .args(["auth", "set", "brave", brave_secret])
        .output()
        .await
        .unwrap();
    assert_eq!(positional.status.code(), Some(2));

    // 2. `wick status --json --verbose` and `wick auth status --json --verbose` must never leak
    //    configured secrets, prefixes, or SEARXNG_URL on stdout or stderr.
    for args in [
        vec!["--verbose", "--json", "status"],
        vec!["--verbose", "auth", "status", "--json"],
    ] {
        let out = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .env("BRAVE_API_KEY", brave_secret)
            .env("TAVILY_API_KEY", tavily_secret)
            .env("SEARXNG_URL", searxng_url)
            .env_remove("SERPER_API_KEY")
            .env_remove("EXA_API_KEY")
            .args(["--state-dir"])
            .arg(&state_dir.path)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "args={args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for forbidden in [
            brave_secret,
            "918273645",
            tavily_secret,
            "5544332211",
            searxng_url,
            "19999",
        ] {
            assert!(
                !stdout.contains(forbidden),
                "leaked {forbidden} on stdout for {args:?}"
            );
            assert!(
                !stderr.contains(forbidden),
                "leaked {forbidden} on stderr for {args:?}"
            );
        }
    }

    // 3. Rejected `--stdin` input with `--verbose --json` must not echo any part of the rejected secret.
    let bad_secret = "secret-with-\x07-control-byte-998877";
    let mut child = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir.path)
        .args([
            "--verbose",
            "--json",
            "auth",
            "set",
            "brave",
            "--stdin",
            "--storage",
            "file",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(bad_secret.as_bytes())
        .await
        .unwrap();
    let out = child.wait_with_output().await.unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("998877"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("998877"));
}

async fn spawn_simple_json_server(
    status_line: &'static str,
    body: &'static str,
    delay_ms: u64,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let hits_req = hits_clone.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                hits_req.fetch_add(1, Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://{addr}"), hits)
}

#[test]
fn adversarial_retry_after_imf_fixdate_clamping_and_utc_calendar_edges() {
    // Pure-std Howard Hinnant calendar edge cases (epoch + leap years).
    for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2024, 2, 29), (2026, 11, 26)] {
        let days = days_from_civil(y, m, d);
        assert_eq!(civil_from_days(days), (y, m, d));
    }

    // Integer Retry-After clamping to 5..=300.
    assert_eq!(parse_retry_after("0", 1_700_000_000), Some(5));
    assert_eq!(parse_retry_after("42", 1_700_000_000), Some(42));
    assert_eq!(parse_retry_after("9999", 1_700_000_000), Some(300));

    // IMF-fixdate parsing + clamping (2026-11-26T15:00:00Z).
    let base_secs = (days_from_civil(2026, 11, 26) as u64) * 86_400 + 15 * 3_600;
    assert_eq!(
        parse_retry_after("Thu, 26 Nov 2026 15:02:00 GMT", base_secs),
        Some(120)
    );
    assert_eq!(
        parse_retry_after("Thu, 26 Nov 2026 14:50:00 GMT", base_secs),
        Some(5)
    );
    assert_eq!(
        parse_retry_after("Thu, 26 Nov 2026 16:00:00 GMT", base_secs),
        Some(300)
    );
    // Invalid calendar dates and malformed headers return None.
    assert_eq!(
        parse_retry_after("Sat, 30 Feb 2026 15:02:00 GMT", base_secs),
        None
    );
    assert_eq!(parse_retry_after("not-a-date", base_secs), None);
}

#[tokio::test]
async fn adversarial_concurrent_brave_burst_spills_over_atomically_to_tavily() {
    let _lock = ENV_LOCK.lock().await;
    let _env = SearchEnvGuard::new(&[
        ("BRAVE_API_KEY", "brave-burst-secret"),
        ("TAVILY_API_KEY", "tavily-burst-secret"),
    ]);
    let dir = TempStateDir::new();

    let (brave_url, brave_hits) = spawn_simple_json_server(
        "200 OK",
        r#"{"web":{"results":[{"title":"Brave Hit","url":"https://brave.example/1","description":"from brave"}]}}"#,
        40,
    )
    .await;
    let (tavily_url, tavily_hits) = spawn_simple_json_server(
        "200 OK",
        r#"{"results":[{"title":"Tavily Hit","url":"https://tavily.example/1","content":"from tavily"}]}"#,
        10,
    )
    .await;

    let endpoints = ProviderEndpoints {
        brave_url,
        tavily_url,
        ..ProviderEndpoints::default()
    };

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: dir.path.clone(),
        timeout: Duration::from_secs(5),
        use_daemon: false,
        ..RuntimeConfig::default()
    })
    .unwrap();

    let opts = SearchOptions {
        limit: 5,
        provider: SearchProvider::Auto,
        mode: SearchMode::Auto,
        freshness: None,
        site: None,
        extra_snippets: false,
    };

    let (res1, res2) = tokio::join!(
        search_with_endpoints(&ctx, "rust async runtime", &opts, &endpoints),
        search_with_endpoints(&ctx, "rust async runtime", &opts, &endpoints),
    );
    let r1 = res1.unwrap();
    let r2 = res2.unwrap();
    let mut used = vec![r1.provider, r2.provider];
    used.sort();
    assert_eq!(
        used,
        vec!["brave", "tavily"],
        "concurrent Auto burst must spill second call to tavily"
    );
    assert_eq!(brave_hits.load(Ordering::SeqCst), 1);
    assert_eq!(tavily_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn adversarial_federated_partial_failure_dedup_and_deterministic_tie_breaking() {
    let _lock = ENV_LOCK.lock().await;
    let _env = SearchEnvGuard::new(&[
        ("BRAVE_API_KEY", "brave-fed-secret-xyz"),
        ("SERPER_API_KEY", "serper-fed-secret-xyz"),
        ("EXA_API_KEY", "exa-fed-secret-xyz"),
    ]);
    let dir = TempStateDir::new();

    // Brave (slot 0) returns a duplicate URL within its own list + a unique rank-2 hit.
    let (brave_url, _) = spawn_simple_json_server(
        "200 OK",
        r#"{"web":{"results":[
            {"title":"Shared","url":"https://shared.example/doc","description":"short"},
            {"title":"Shared Dup","url":"https://shared.example/doc#frag","description":"dup should be ignored"},
            {"title":"Tie Brave","url":"https://tie-z.example/page","description":"brave rank 3"}
        ]}}"#,
        10,
    )
    .await;
    // Serper (slot 1) fails with HTTP 503 -> must record 60s cooldown and not fail federation.
    let (serper_url, _) =
        spawn_simple_json_server("503 Service Unavailable", "upstream error", 5).await;
    // Exa (slot 2) returns the shared URL at rank 1 + a tied unique hit at rank 2.
    let (exa_url, _) = spawn_simple_json_server(
        "200 OK",
        r#"{"results":[
            {"title":"Shared","url":"https://shared.example/doc","highlights":["much richer exa snippet for shared doc"]},
            {"title":"Tie Exa","url":"https://tie-a.example/page","highlights":["exa rank 2"]}
        ]}"#,
        10,
    )
    .await;

    let endpoints = ProviderEndpoints {
        brave_url,
        serper_url,
        exa_url,
        ..ProviderEndpoints::default()
    };
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: dir.path.clone(),
        timeout: Duration::from_secs(5),
        use_daemon: false,
        ..RuntimeConfig::default()
    })
    .unwrap();

    let opts = SearchOptions {
        limit: 5,
        provider: SearchProvider::Federated,
        mode: SearchMode::Auto,
        freshness: None,
        site: None,
        extra_snippets: true,
    };

    let resp = search_with_endpoints(&ctx, "federated test", &opts, &endpoints)
        .await
        .unwrap();
    assert_eq!(resp.provider, "federated");
    assert_eq!(resp.providers_used, vec!["brave", "exa"]);
    // Shared URL gets 2 provider hits (rank 1 in brave + rank 1 in exa; intra-provider dup ignored).
    assert_eq!(resp.results[0].url, "https://shared.example/doc");
    assert_eq!(
        resp.results[0].snippet,
        "much richer exa snippet for shared doc"
    );
    // Exa rank-2 (1/62) beats Brave rank-3 (1/63).
    assert_eq!(resp.results[1].url, "https://tie-a.example/page");
    assert_eq!(resp.results[2].url, "https://tie-z.example/page");

    let health = provider_health_summary(&dir.path).unwrap();
    let serper_health = health.get("serper").expect("serper health entry recorded");
    assert_eq!(serper_health.last_error_status, Some(503));
    assert!(serper_health.cooldown_until_epoch_secs > 0);
}
