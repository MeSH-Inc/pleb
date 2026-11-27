use pleb::auth::{
    all_credential_statuses, remove_credential, resolve_agent_identity_status,
    resolve_cdp_access_status, resolve_credential, set_credential, CredentialProvider,
    CredentialSource, CredentialStorage, ResolvedCredential, SecretString, MAX_SECRET_BYTES,
};
use pleb::context::{clear_state_dir, ensure_private_dir};
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
        saved.push(("PLEB_NO_KEYCHAIN", std::env::var("PLEB_NO_KEYCHAIN").ok()));
        std::env::set_var("PLEB_NO_KEYCHAIN", "1");

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
        let path = PathBuf::from(format!("/tmp/pleb-t-{pid}-{seq}"));
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
        set_err.to_string().contains("PLEB_NO_KEYCHAIN=1")
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
        rm_err.to_string().contains("PLEB_NO_KEYCHAIN=1")
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
    let vault_path = dir.path.join("credentials.enc");
    let creds_lock_path = dir.path.join("credentials.lock");
    let site_rules_path = dir.path.join("site-rules.json");
    let search_state_path = dir.path.join("search-state.json");
    let site_cache_path = dir.path.join("site-cache.json");

    let creds_before = std::fs::read_to_string(&creds_path).unwrap();
    assert!(creds_lock_path.exists());

    std::fs::write(&vault_path, b"PLEBVLT1synthetic-vault-payload").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
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
    assert!(vault_path.exists(), "credentials.enc must be preserved");
    assert_eq!(
        std::fs::read(&vault_path).unwrap(),
        b"PLEBVLT1synthetic-vault-payload"
    );
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
        let vault_mode = std::fs::metadata(&vault_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(vault_mode, 0o600, "credentials.enc must remain 0o600");
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

#[test]
fn web_bot_auth_jwk_canonicalizes_pretty_json_and_never_leaks_secrets_on_error() {
    let _env = EnvGuard::new(&[
        ("PLEB_WEB_BOT_AUTH_JWK", None),
        ("PLEB_WEB_BOT_AUTH_AGENT", None),
    ]);
    let dir = TempStateDir::new();

    let d_b64 = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";
    let x_b64 = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    let thumbprint = "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k";

    let pretty_jwk = format!(
        "\n  {{\n\t\"kty\": \"OKP\",\n\t\"kid\": \"ignored-kid\",\n\t\"x\": \"{x_b64}\",\n\t\"crv\": \"Ed25519\",\n\t\"d\": \"{d_b64}\"\n  }}\r\n"
    );
    let stored = set_credential(
        &dir.path,
        CredentialProvider::WebBotAuthJwk,
        &pretty_jwk,
        CredentialStorage::File,
    )
    .unwrap();
    assert_eq!(stored, CredentialSource::File);

    let expected_canonical =
        format!(r#"{{"crv":"Ed25519","d":"{d_b64}","kty":"OKP","x":"{x_b64}"}}"#);
    let resolved = resolve_credential(&dir.path, CredentialProvider::WebBotAuthJwk)
        .unwrap()
        .expect("jwk should resolve");
    assert_eq!(resolved.value.as_str(), expected_canonical);

    // Verify credentials.json persisted canonical single-line JSON (without pretty newlines/kid).
    let raw_file = std::fs::read_to_string(dir.path.join("credentials.json")).unwrap();
    let parsed_map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&raw_file).unwrap();
    assert_eq!(
        parsed_map.get("web-bot-auth-jwk").map(String::as_str),
        Some(expected_canonical.as_str())
    );
    assert!(!raw_file.contains("ignored-kid"));

    // Invalid JWKs (corrupt JSON, wrong kty/crv, public-only, mismatched keypair, NUL byte, non-ASCII).
    let bad_jwks = [
        "{not-json-secret-111",
        "{}",
        r#"{"kty":"RSA","crv":"Ed25519","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo","d":"nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A"}"#,
        r#"{"kty":"OKP","crv":"Ed25519","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#,
        // Mismatched d/x pair
        r#"{"kty":"OKP","crv":"Ed25519","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","d":"nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A"}"#,
        // Padding '=' rejected by URL_SAFE_NO_PAD
        r#"{"kty":"OKP","crv":"Ed25519","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo=","d":"nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A="}"#,
        "{\"kty\":\"OKP\",\"crv\":\"Ed25519\",\"x\":\"11qY\",\"d\":\"nWGx\x00nul\"}",
        "{\"kty\":\"OKP\",\"crv\":\"Ed25519\",\"x\":\"é\",\"d\":\"nWGx\"}",
    ];
    for bad in bad_jwks {
        let err = set_credential(
            &dir.path,
            CredentialProvider::WebBotAuthJwk,
            bad,
            CredentialStorage::File,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains(d_b64), "error leaked d: {msg}");
        assert!(!msg.contains(x_b64), "error leaked x: {msg}");
        assert!(
            !msg.contains("secret-111"),
            "error leaked raw JSON fragment: {msg}"
        );
        assert!(
            !msg.contains("expected value at line"),
            "error leaked serde_json details: {msg}"
        );
    }

    // Status reporting: partial (only JWK), then complete, then disabled flag, then corrupt env.
    let partial = resolve_agent_identity_status(&dir.path, false);
    assert!(partial.web_bot_auth_enabled);
    assert!(partial.jwk_configured);
    assert_eq!(partial.jwk_source, Some(CredentialSource::File));
    assert!(!partial.agent_configured);
    assert_eq!(partial.keyid.as_deref(), Some(thumbprint));
    assert!(partial.status_error.is_some());

    set_credential(
        &dir.path,
        CredentialProvider::WebBotAuthAgent,
        "https://agent.example.com",
        CredentialStorage::File,
    )
    .unwrap();
    let ready = resolve_agent_identity_status(&dir.path, false);
    assert!(ready.web_bot_auth_enabled);
    assert!(ready.jwk_configured);
    assert!(ready.agent_configured);
    assert_eq!(ready.keyid.as_deref(), Some(thumbprint));
    assert_eq!(ready.status_error, None);

    let disabled = resolve_agent_identity_status(&dir.path, true);
    assert!(!disabled.web_bot_auth_enabled);
    assert_eq!(disabled.keyid.as_deref(), Some(thumbprint));

    // Corrupt env JWK overrides and surfaces sanitized status_error without leaking secret.
    drop(_env);
    let _corrupt = EnvGuard::new(&[(
        "PLEB_WEB_BOT_AUTH_JWK",
        Some(r#"{"kty":"OKP","crv":"Ed25519","d":"secret-leak-guard","x":"bad"}"#),
    )]);
    let corrupt_status = resolve_agent_identity_status(&dir.path, false);
    let err_msg = corrupt_status
        .status_error
        .expect("corrupt env JWK must populate status_error");
    assert!(!err_msg.contains("secret-leak-guard"));
}

#[test]
fn validates_web_bot_auth_agent_and_cdp_access_credentials_and_status() {
    let _env = EnvGuard::new(&[
        ("PLEB_WEB_BOT_AUTH_AGENT", None),
        ("PLEB_CDP_ACCESS_CLIENT_ID", None),
        ("PLEB_CDP_ACCESS_CLIENT_SECRET", None),
        ("PLEB_CDP_ACCESS_HOST", None),
    ]);
    let dir = TempStateDir::new();

    // WebBotAuthAgent rejections.
    for bad_agent in [
        "http://agent.example.com",
        "https://localhost/dir",
        "https://127.0.0.1/dir",
        "https://[::1]/dir",
        "https://169.254.169.254/dir",
        "https://10.0.0.1/dir",
        "https://user:secretpass@agent.example.com",
        "https://agent.example.com?q=1",
        "https://agent.example.com#frag",
        "https://agent.example.com/with space",
        "https://agent.example.com/with\"quote",
        "https://agent.example.com/with\\slash",
        "https://agent.example.com/\x07bell",
        &format!("https://agent.example.com/{}", "a".repeat(500)),
    ] {
        let err = set_credential(
            &dir.path,
            CredentialProvider::WebBotAuthAgent,
            bad_agent,
            CredentialStorage::File,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("secretpass"));
    }

    assert!(set_credential(
        &dir.path,
        CredentialProvider::WebBotAuthAgent,
        "https://agent.example.com/.well-known/http-message-signatures-directory",
        CredentialStorage::File,
    )
    .is_ok());

    // CdpAccessHost validation + canonicalization.
    for bad_host in [
        "https://worker.example.com",
        "worker.example.com/path",
        "worker.example.com?a=1",
        "worker.example.com#frag",
        "user@worker.example.com",
        "*.example.com",
        "worker example.com",
        "worker\"example.com",
        "worker\\example.com",
        "worker.example.com:0",
        "worker.example.com:99999",
        "worker.example.com:",
        "localhost",
        "127.0.0.1",
        "::1",
        "[::1]:8787",
        "2130706433",
        "10.0.0.5",
        "169.254.169.254",
        "100.64.0.1",
        "fc00::1",
    ] {
        assert!(
            set_credential(
                &dir.path,
                CredentialProvider::CdpAccessHost,
                bad_host,
                CredentialStorage::File,
            )
            .is_err(),
            "bad cdp-access-host must be rejected: {bad_host}"
        );
    }

    set_credential(
        &dir.path,
        CredentialProvider::CdpAccessHost,
        "Worker.Example.COM:8443",
        CredentialStorage::File,
    )
    .unwrap();
    assert_eq!(
        resolve_credential(&dir.path, CredentialProvider::CdpAccessHost)
            .unwrap()
            .unwrap()
            .value
            .as_str(),
        "worker.example.com:8443"
    );

    set_credential(
        &dir.path,
        CredentialProvider::CdpAccessHost,
        "2001:4860:4860::8888",
        CredentialStorage::File,
    )
    .unwrap();
    assert_eq!(
        resolve_credential(&dir.path, CredentialProvider::CdpAccessHost)
            .unwrap()
            .unwrap()
            .value
            .as_str(),
        "[2001:4860:4860::8888]"
    );

    // Partial CDP Access tuple surfaces status_error without marking configured.
    let partial = resolve_cdp_access_status(&dir.path);
    assert!(!partial.configured);
    assert!(partial.host_configured);
    assert_eq!(partial.host_source, Some(CredentialSource::File));
    assert_eq!(
        partial.access_host.as_deref(),
        Some("[2001:4860:4860::8888]")
    );
    assert!(partial.status_error.is_some());

    // Internal whitespace rejected on client id & client secret.
    assert!(set_credential(
        &dir.path,
        CredentialProvider::CdpAccessClientId,
        "bad id",
        CredentialStorage::File,
    )
    .is_err());
    assert!(set_credential(
        &dir.path,
        CredentialProvider::CdpAccessClientSecret,
        "bad\tsecret",
        CredentialStorage::File,
    )
    .is_err());

    set_credential(
        &dir.path,
        CredentialProvider::CdpAccessClientId,
        "cid-123.access",
        CredentialStorage::File,
    )
    .unwrap();
    set_credential(
        &dir.path,
        CredentialProvider::CdpAccessClientSecret,
        "csec-456",
        CredentialStorage::File,
    )
    .unwrap();
    let complete = resolve_cdp_access_status(&dir.path);
    assert!(complete.configured);
    assert!(complete.client_id_configured);
    assert!(complete.client_secret_configured);
    assert!(complete.host_configured);
    assert_eq!(complete.status_error, None);

    // Kebab-case serialization & search provider partitioning.
    assert_eq!(
        serde_json::to_string(&CredentialProvider::WebBotAuthJwk).unwrap(),
        "\"web-bot-auth-jwk\""
    );
    assert_eq!(
        serde_json::to_string(&CredentialProvider::CdpAccessHost).unwrap(),
        "\"cdp-access-host\""
    );
    assert!(!CredentialProvider::WebBotAuthJwk.is_search_provider());
    assert!(CredentialProvider::Brave.is_search_provider());
}
