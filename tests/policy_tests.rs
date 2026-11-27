use pleb::extract::ParsedHtml;
use pleb::policy::{
    classify_response, classify_response_with_headers, classify_response_with_parsed,
    plan_strategy, BlockReason, EscalationBackend, PrimaryTransport, RenderMode,
    ResponseClassification, SiteCache, SiteCacheEntry, SiteRule, SiteRules,
};
use pleb::transport::DiagnosticHeaders;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(_name: &str) -> std::path::PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::path::PathBuf::from(format!("/tmp/pleb-t-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

#[test]
fn strategy_router_precedence() {
    let cef_rule = SiteRule {
        render: Some("cef".to_string()),
        ..Default::default()
    };
    let cronet_rule = SiteRule {
        render: Some("cronet".to_string()),
        ..Default::default()
    };
    let cef_cached = SiteCacheEntry {
        strategy: "cef".to_string(),
        avg_time_ms: 1500,
        successes: 2,
        updated_at_epoch_secs: 100,
    };
    let timeout_cached = SiteCacheEntry {
        strategy: "cef_timeout".to_string(),
        avg_time_ms: 30_000,
        successes: 1,
        updated_at_epoch_secs: 100,
    };

    // Forced modes win over rules and cache.
    let p_forced_cronet = plan_strategy(
        RenderMode::Cronet,
        Some(&cef_rule),
        Some(&cef_cached),
        true,
        true,
        true,
    );
    assert_eq!(p_forced_cronet.primary, PrimaryTransport::Cronet);
    assert_eq!(p_forced_cronet.escalation, EscalationBackend::None);

    let p_forced_cef = plan_strategy(RenderMode::Cef, Some(&cronet_rule), None, true, true, true);
    assert_eq!(p_forced_cef.primary, PrimaryTransport::Cef);
    assert!(!p_forced_cef.allow_cronet_fallback);

    // Auto without CEF or CDP installed never picks CEF or CDP.
    let p_no_cef = plan_strategy(
        RenderMode::Auto,
        Some(&cef_rule),
        Some(&cef_cached),
        false,
        false,
        false,
    );
    assert_eq!(p_no_cef.primary, PrimaryTransport::Cronet);
    assert_eq!(p_no_cef.escalation, EscalationBackend::None);

    // Auto: curated rule beats local cache.
    let p_rule_cef = plan_strategy(RenderMode::Auto, Some(&cef_rule), None, true, false, false);
    assert_eq!(p_rule_cef.primary, PrimaryTransport::Cef);
    assert!(p_rule_cef.allow_cronet_fallback);

    let p_rule_cronet = plan_strategy(
        RenderMode::Auto,
        Some(&cronet_rule),
        Some(&cef_cached),
        true,
        false,
        false,
    );
    assert_eq!(p_rule_cronet.primary, PrimaryTransport::Cronet);
    assert_eq!(p_rule_cronet.escalation, EscalationBackend::Cef);

    // Auto with no rule uses cache ("cef" -> Cef, "cef_timeout" -> Cronet).
    let p_cached_cef = plan_strategy(
        RenderMode::Auto,
        None,
        Some(&cef_cached),
        true,
        false,
        false,
    );
    assert_eq!(p_cached_cef.primary, PrimaryTransport::Cef);

    let p_cached_timeout = plan_strategy(
        RenderMode::Auto,
        None,
        Some(&timeout_cached),
        true,
        false,
        false,
    );
    assert_eq!(p_cached_timeout.primary, PrimaryTransport::Cronet);
    assert_eq!(p_cached_timeout.escalation, EscalationBackend::Cef);

    // Explicit CDP render never falls back to Cronet.
    let p_cdp = plan_strategy(RenderMode::Cdp, None, None, true, true, false);
    assert_eq!(p_cdp.primary, PrimaryTransport::Cdp);
    assert_eq!(p_cdp.escalation, EscalationBackend::None);
    assert!(!p_cdp.allow_cronet_fallback);

    // site-rules "render": "cdp" uses Cdp when configured, otherwise falls back cleanly.
    let cdp_rule = SiteRule {
        render: Some("cdp".to_string()),
        ..Default::default()
    };
    let p_rule_cdp = plan_strategy(RenderMode::Auto, Some(&cdp_rule), None, true, true, false);
    assert_eq!(p_rule_cdp.primary, PrimaryTransport::Cdp);
    assert!(p_rule_cdp.allow_cronet_fallback);

    let p_rule_cdp_missing =
        plan_strategy(RenderMode::Auto, Some(&cdp_rule), None, true, false, false);
    assert_eq!(p_rule_cdp_missing.primary, PrimaryTransport::Cronet);
    assert_eq!(p_rule_cdp_missing.escalation, EscalationBackend::Cef);

    // prefer_cdp redirects CEF-preferred rules and caches toward CDP.
    let p_rule_cef_prefer_cdp =
        plan_strategy(RenderMode::Auto, Some(&cef_rule), None, true, true, true);
    assert_eq!(p_rule_cef_prefer_cdp.primary, PrimaryTransport::Cdp);
    assert!(p_rule_cef_prefer_cdp.allow_cronet_fallback);

    // Cached "cdp" strategy selects Cdp primary when available, else Cronet.
    let cdp_cached = SiteCacheEntry {
        strategy: "cdp".to_string(),
        avg_time_ms: 420,
        successes: 1,
        updated_at_epoch_secs: 1_000,
    };
    let p_cached_cdp = plan_strategy(RenderMode::Auto, None, Some(&cdp_cached), true, true, false);
    assert_eq!(p_cached_cdp.primary, PrimaryTransport::Cdp);
    assert!(p_cached_cdp.allow_cronet_fallback);

    let p_cached_cef_prefer_cdp =
        plan_strategy(RenderMode::Auto, None, Some(&cef_cached), true, true, true);
    assert_eq!(p_cached_cef_prefer_cdp.primary, PrimaryTransport::Cdp);
}

#[test]
fn site_cache_atomic_save_and_ttl_expiry() {
    let dir = temp_dir("site-cache");
    let cache_path = dir.join("site-cache.json");

    let cache = SiteCache::load(cache_path.clone());
    cache.record_at("example.com", "cef", 1200, 1_000_000);
    cache.record_at("slow.example.org", "cef_timeout", 60_000, 1_000_000);

    let reloaded = SiteCache::load(cache_path.clone());
    assert_eq!(
        reloaded
            .get_at("www.example.com", 1_000_000 + 3600)
            .map(|e| e.strategy),
        Some("cef".to_string())
    );
    assert!(reloaded
        .get_at("www.example.com", 1_000_000 + 7 * 24 * 3600 + 1)
        .is_none());

    assert!(reloaded
        .get_at("slow.example.org", 1_000_000 + 12 * 3600)
        .is_some());
    assert!(reloaded
        .get_at("slow.example.org", 1_000_000 + 24 * 3600 + 1)
        .is_none());

    cache.record_at("slow-cdp.example.org", "cdp_timeout", 60_000, 2_000_000);
    cache.record_at("ok-cdp.example.org", "cdp", 350, 2_000_000);
    let reloaded_cdp = SiteCache::load(cache_path);
    assert!(reloaded_cdp
        .get_at("slow-cdp.example.org", 2_000_000 + 12 * 3600)
        .is_some());
    assert!(reloaded_cdp
        .get_at("slow-cdp.example.org", 2_000_000 + 24 * 3600 + 1)
        .is_none());
    assert_eq!(
        reloaded_cdp
            .get_at("ok-cdp.example.org", 2_000_000 + 48 * 3600)
            .map(|e| e.strategy),
        Some("cdp".to_string())
    );

    // Repeated same-strategy record within the refresh interval skips disk rewrite,
    // while a strategy change or post-interval refresh persists immediately.
    let before_same = std::fs::read_to_string(dir.join("site-cache.json")).unwrap();
    cache.record_at("example.com", "cef", 900, 1_000_000 + 60);
    let after_same = std::fs::read_to_string(dir.join("site-cache.json")).unwrap();
    assert_eq!(
        before_same, after_same,
        "unchanged fresh strategy must not rewrite site-cache.json"
    );

    cache.record_at("example.com", "cronet", 50, 1_000_000 + 61);
    let after_change = SiteCache::load(dir.join("site-cache.json"));
    assert_eq!(
        after_change
            .get_at("example.com", 1_000_000 + 120)
            .map(|e| e.strategy),
        Some("cronet".to_string())
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_rules_bundled_parent_rewrite_and_local_overlay() {
    let dir = temp_dir("site-rules");
    let overlay = r#"{"rules":{"x.com":{"render":"cronet"},"sub.example.com":{"render":"cef"},"custom.dev":{"render":"cef"}}}"#;
    std::fs::write(dir.join("site-rules.json"), overlay).unwrap();

    let rules = SiteRules::load(&dir);

    // Local overlay overrides bundled x.com rule, preserving www. parent lookup.
    assert_eq!(
        rules.get("www.x.com").and_then(|r| r.render.as_deref()),
        Some("cronet")
    );
    assert_eq!(
        rules.get("custom.dev").and_then(|r| r.render.as_deref()),
        Some("cef")
    );

    // Exact subdomain does not overreach.
    assert!(rules.get("sub.example.com").is_some());
    assert!(rules.get("other.example.com").is_none());

    // Bundled Reddit rewrite works once and does not rewrite old.reddit.com again.
    let reddit = Url::parse("https://www.reddit.com/r/rust/").unwrap();
    let rewritten = rules.rewrite_url(&reddit).expect("reddit should rewrite");
    assert_eq!(rewritten.as_str(), "https://old.reddit.com/r/rust/");
    assert!(rules.rewrite_url(&rewritten).is_none());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn response_classifier_covers_challenges_js_shells_and_normal_pages() {
    let cf_403 = r#"<html><head><title>Just a moment...</title></head>
        <body><script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script></body></html>"#;
    assert!(matches!(
        classify_response(403, true, cf_403),
        ResponseClassification::Challenge { status: 403, .. }
    ));

    assert_eq!(
        classify_response(403, true, "Forbidden"),
        ResponseClassification::Blocked {
            status: 403,
            reason: BlockReason::OriginForbidden,
        }
    );
    assert_eq!(
        classify_response(404, true, "Not Found"),
        ResponseClassification::HttpError { status: 404 }
    );

    let x_shell = r#"<h1>JavaScript is not available.</h1><p>We've detected that JavaScript is disabled.</p>"#;
    assert!(matches!(
        classify_response(200, true, x_shell),
        ResponseClassification::JavaScriptShell { .. }
    ));

    let next_spa_shell =
        r#"<html><body><div id="__next"></div><script src="/a.js"></script></body></html>"#;
    assert!(matches!(
        classify_response(200, true, next_spa_shell),
        ResponseClassification::JavaScriptShell { .. }
    ));

    let normal_page = "<html><body><h1>Welcome</h1><p>Normal prose content here.</p></body></html>";
    assert_eq!(
        classify_response(200, true, normal_page),
        ResponseClassification::Ok
    );

    let huge = format!(
        "javascript is not available {}",
        "<p>article</p>".repeat(200_000)
    );
    assert!(huge.len() > 2_000_000);
    assert_eq!(
        classify_response(200, true, &huge),
        ResponseClassification::Ok
    );

    // Lazy parsing only populates ParsedHtml when DOM metrics are actually evaluated.
    let mut parsed: Option<ParsedHtml> = None;
    assert_eq!(
        classify_response_with_parsed(403, true, "Forbidden", &mut parsed),
        ResponseClassification::Blocked {
            status: 403,
            reason: BlockReason::OriginForbidden,
        }
    );
    assert!(parsed.is_none());

    assert!(matches!(
        classify_response_with_parsed(200, true, x_shell, &mut parsed),
        ResponseClassification::JavaScriptShell { .. }
    ));
    assert!(parsed.is_none());

    assert_eq!(
        classify_response_with_parsed(200, true, normal_page, &mut parsed),
        ResponseClassification::Ok
    );
    assert!(parsed.is_some());
}

#[cfg(unix)]
#[test]
fn site_cache_file_has_private_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("cache-perms");
    let cache_path = dir.join("site-cache.json");
    let cache = SiteCache::load(cache_path.clone());
    cache.record_at("example.com", "cronet", 42, 1_000_000);

    let mode = std::fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_cache_concurrent_multi_writer_merges_without_loss() {
    let dir = temp_dir("cache-flock");
    let cache_path = dir.join("site-cache.json");

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = cache_path.clone();
            std::thread::spawn(move || {
                let cache = SiteCache::load(path);
                cache.record_at(
                    &format!("host-{i}.example"),
                    "cronet",
                    10 + i as u64,
                    1_000_000,
                );
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    let reloaded = SiteCache::load(cache_path);
    for i in 0..8 {
        assert_eq!(
            reloaded
                .get_at(&format!("host-{i}.example"), 1_000_000 + 60)
                .map(|e| e.strategy),
            Some("cronet".to_string()),
            "missing concurrent writer entry for host-{i}.example"
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_cache_recovers_corrupt_json_and_preserves_lock_on_clear() {
    let dir = temp_dir("cache-corrupt");
    let cache_path = dir.join("site-cache.json");
    let lock_path = dir.join("site-cache.lock");
    std::fs::write(&cache_path, "{not valid json").unwrap();

    let cache = SiteCache::load(cache_path.clone());
    assert!(cache.get_at("example.com", 1_000_000).is_none());

    cache.record_at("example.com", "cronet", 25, 1_000_000);
    let reloaded = SiteCache::load(cache_path.clone());
    assert_eq!(
        reloaded
            .get_at("example.com", 1_000_060)
            .map(|e| e.strategy),
        Some("cronet".to_string())
    );

    reloaded.clear().unwrap();
    assert!(!cache_path.exists());
    assert!(
        lock_path.exists(),
        "clear must never unlink site-cache.lock"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_cache_does_not_overwrite_unreadable_file() {
    let dir = temp_dir("cache-io-err");
    // Place a directory at the cache path so read_to_string fails with an IO error (IsADirectory).
    let cache_path = dir.join("site-cache.json");
    std::fs::create_dir(&cache_path).unwrap();

    let cache = SiteCache::load(cache_path.clone());
    cache.record_at("example.com", "cronet", 25, 1_000_000);

    // Must still be a directory (not overwritten).
    assert!(cache_path.is_dir());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn site_cache_subprocess_worker() {
    let cache_path = std::env::var("PLEB_TEST_CACHE_PATH").expect("PLEB_TEST_CACHE_PATH");
    let host = std::env::var("PLEB_TEST_CACHE_HOST").expect("PLEB_TEST_CACHE_HOST");
    let cache = SiteCache::load(std::path::PathBuf::from(cache_path));
    cache.record_at(&host, "cronet", 40, 1_000_000);
}

#[test]
fn site_cache_eight_subprocess_contention_preserves_all_entries() {
    let dir = temp_dir("cache-subproc");
    let cache_path = dir.join("site-cache.json");
    let exe = std::env::current_exe().unwrap();

    let mut children: Vec<_> = (0..8)
        .map(|i| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "site_cache_subprocess_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PLEB_TEST_CACHE_PATH", &cache_path)
                .env("PLEB_TEST_CACHE_HOST", format!("subproc-{i}.example"))
                .spawn()
                .expect("spawn subprocess worker")
        })
        .collect();

    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().expect("wait subprocess worker");
        assert!(status.success(), "subprocess {i} exited with {status}");
    }

    let reloaded = SiteCache::load(cache_path);
    for i in 0..8 {
        let host = format!("subproc-{i}.example");
        assert_eq!(
            reloaded.get_at(&host, 1_000_060).map(|e| e.strategy),
            Some("cronet".to_string()),
            "missing subprocess entry for {host}"
        );
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn classify_response_with_context_gates_http200_on_prose_and_detects_soft_blocks() {
    use pleb::policy::classify_response_with_context;

    // Long technical article (>= 600 prose bytes) quoting challenge text must stay Ok.
    let long_prose =
        "Detailed engineering explanation of reverse proxy behavior and authentication headers. "
            .repeat(12);
    let article = format!(
        "<html><head><title>Debugging</title></head><body><main><p>Just a moment... checking your browser</p><p>{long_prose}</p></main></body></html>"
    );
    let mut parsed = None;
    assert_eq!(
        classify_response_with_context(
            200,
            true,
            Some("https://docs.example.com/guide"),
            Some("https://docs.example.com/guide"),
            &article,
            &mut parsed
        ),
        ResponseClassification::Ok
    );

    // Login redirect (non-auth -> /login) with low prose classifies as Blocked.
    let login_page = "<html><head><title>Sign in</title></head><body><main><p>Please sign in.</p></main></body></html>";
    let mut parsed_login = None;
    assert_eq!(
        classify_response_with_context(
            200,
            true,
            Some("https://app.example.com/dashboard"),
            Some("https://app.example.com/login?next=/dashboard"),
            login_page,
            &mut parsed_login
        ),
        ResponseClassification::Blocked {
            status: 200,
            reason: BlockReason::LoginWall,
        }
    );

    // Password-form wall without redirect and prose < 400 classifies as Blocked.
    let form_wall = "<html><body><form><input type=\"password\" name=\"pw\"></form></body></html>";
    let mut parsed_form = None;
    assert_eq!(
        classify_response_with_context(200, true, None, None, form_wall, &mut parsed_form),
        ResponseClassification::Blocked {
            status: 200,
            reason: BlockReason::LoginWall,
        }
    );

    // Low-prose soft-404 title classifies as HttpError { status: 404 }.
    let soft_404 =
        "<html><head><title>404 - Page Not Found</title></head><body><p>Missing.</p></body></html>";
    let mut parsed_404 = None;
    assert_eq!(
        classify_response_with_context(200, true, None, None, soft_404, &mut parsed_404),
        ResponseClassification::HttpError { status: 404 }
    );
}

#[test]
fn adversarial_auth_path_segment_boundary_does_not_flag_login_guide_or_oauth2_docs() {
    use pleb::policy::classify_response_with_context;

    let short_doc = "<html><head><title>Migration Guide</title></head><body><main><p>Short technical guide with no password form.</p></main></body></html>";

    // Redirecting to `/login-guide` or `/oauth2/docs` must NOT match `/login` or `/oauth`.
    for final_url in [
        "https://docs.example.com/login-guide",
        "https://docs.example.com/oauth2/docs",
    ] {
        let mut parsed = None;
        assert_eq!(
            classify_response_with_context(
                200,
                true,
                Some("https://docs.example.com/old"),
                Some(final_url),
                short_doc,
                &mut parsed
            ),
            ResponseClassification::Ok,
            "path boundary false positive for {final_url}"
        );
    }
}

#[test]
fn adversarial_sveltekit_hydrated_and_zero_script_pages_not_flagged_as_js_shell() {
    use pleb::policy::classify_response_with_context;

    // Empty HTML with zero <script> tags must stay Ok (not JavaScriptShell).
    let no_script = "<html><head><title>Blank</title></head><body></body></html>";
    let mut p1 = None;
    assert_eq!(
        classify_response_with_context(200, true, None, None, no_script, &mut p1),
        ResponseClassification::Ok
    );

    // Hydrated SvelteKit page with >= 200 visible prose bytes must stay Ok.
    let prose = "Hydrated SvelteKit documentation content rendered on the server side. ".repeat(5);
    let hydrated = format!(
        "<html><head><title>Docs</title></head><body data-sveltekit-preload-data=\"hover\"><main><h1>Docs</h1><p>{prose}</p></main><script>window.__kit = 1;</script></body></html>"
    );
    let mut p2 = None;
    assert_eq!(
        classify_response_with_context(200, true, None, None, &hydrated, &mut p2),
        ResponseClassification::Ok
    );
}

#[test]
fn is_local_or_private_host_blocks_hardened_ipv4_and_ipv6_ranges() {
    use pleb::policy::is_local_or_private_host;

    for blocked in [
        "localhost",
        "api.localhost",
        "printer.local",
        "metadata.internal",
        "0.0.0.0",
        "0.12.34.56",
        "127.0.0.1",
        "10.0.0.1",
        "100.64.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.1.1",
        "224.0.0.1",
        "240.0.0.1",
        "::",
        "::1",
        "[::ffff:127.0.0.1]",
        "64:ff9b::1",
        "ff02::1",
        "fec0::1",
        "fc00::1",
        "fe80::1",
    ] {
        assert!(
            is_local_or_private_host(blocked),
            "expected private/local block for {blocked}"
        );
    }

    for public in [
        "1.1.1.1",
        "93.184.216.34",
        "2606:4700:4700::1111",
        "example.com",
    ] {
        assert!(
            !is_local_or_private_host(public),
            "expected public host allowance for {public}"
        );
    }
}

#[test]
fn classify_response_with_headers_distinguishes_waf_challenges_cookie_walls_and_429() {
    let mut cf_challenge = DiagnosticHeaders::default();
    cf_challenge.capture_header("Server", "cloudflare");
    cf_challenge.capture_header("CF-Mitigated", "challenge");
    let mut parsed = None;
    assert!(matches!(
        classify_response_with_headers(403, true, None, None, "Access denied", &cf_challenge, &mut parsed),
        ResponseClassification::Challenge { status: 403, ref reason } if reason.contains("cf-mitigated")
    ));

    let mut aws_challenge = DiagnosticHeaders::default();
    aws_challenge.capture_header("x-amzn-waf-action", "captcha");
    assert!(matches!(
        classify_response_with_headers(403, true, None, None, "Blocked", &aws_challenge, &mut parsed),
        ResponseClassification::Challenge { status: 403, ref reason } if reason.contains("x-amzn-waf-action")
    ));

    let mut cf_edge = DiagnosticHeaders::default();
    cf_edge.capture_header("Server", "cloudflare");
    cf_edge.capture_header("CF-Ray", "123abc-SJC");
    assert_eq!(
        classify_response_with_headers(403, true, None, None, "Forbidden", &cf_edge, &mut parsed),
        ResponseClassification::Blocked {
            status: 403,
            reason: BlockReason::EdgeWafBlock,
        }
    );

    let mut rl_headers = DiagnosticHeaders::default();
    rl_headers.capture_header("Retry-After", "45");
    assert_eq!(
        classify_response_with_headers(
            429,
            true,
            None,
            None,
            "Slow down",
            &rl_headers,
            &mut parsed
        ),
        ResponseClassification::RateLimited {
            status: 429,
            retry_after_secs: Some(45),
        }
    );

    let cookie_wall =
        "<html><body><p>Before you continue to Example, please accept cookies.</p></body></html>";
    assert_eq!(
        classify_response_with_headers(
            200,
            true,
            None,
            None,
            cookie_wall,
            &DiagnosticHeaders::default(),
            &mut parsed
        ),
        ResponseClassification::Blocked {
            status: 200,
            reason: BlockReason::CookieWall,
        }
    );
}
