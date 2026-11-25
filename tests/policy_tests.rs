use mesh_wick::policy::{
    classify_response, plan_strategy, PrimaryTransport, RenderMode, ResponseClassification,
    SiteCache, SiteCacheEntry, SiteRule, SiteRules,
};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mesh-wick-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
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
    let p_forced_cronet = plan_strategy(RenderMode::Cronet, Some(&cef_rule), Some(&cef_cached), true);
    assert_eq!(p_forced_cronet.primary, PrimaryTransport::Cronet);
    assert!(!p_forced_cronet.allow_cef_escalation);

    let p_forced_cef = plan_strategy(RenderMode::Cef, Some(&cronet_rule), None, true);
    assert_eq!(p_forced_cef.primary, PrimaryTransport::Cef);
    assert!(!p_forced_cef.allow_cronet_fallback);

    // Auto without CEF installed never picks CEF.
    let p_no_cef = plan_strategy(RenderMode::Auto, Some(&cef_rule), Some(&cef_cached), false);
    assert_eq!(p_no_cef.primary, PrimaryTransport::Cronet);
    assert!(!p_no_cef.allow_cef_escalation);

    // Auto: curated rule beats local cache.
    let p_rule_cef = plan_strategy(RenderMode::Auto, Some(&cef_rule), None, true);
    assert_eq!(p_rule_cef.primary, PrimaryTransport::Cef);
    assert!(p_rule_cef.allow_cronet_fallback);

    let p_rule_cronet = plan_strategy(RenderMode::Auto, Some(&cronet_rule), Some(&cef_cached), true);
    assert_eq!(p_rule_cronet.primary, PrimaryTransport::Cronet);
    assert!(p_rule_cronet.allow_cef_escalation);

    // Auto with no rule uses cache ("cef" -> Cef, "cef_timeout" -> Cronet).
    let p_cached_cef = plan_strategy(RenderMode::Auto, None, Some(&cef_cached), true);
    assert_eq!(p_cached_cef.primary, PrimaryTransport::Cef);

    let p_cached_timeout = plan_strategy(RenderMode::Auto, None, Some(&timeout_cached), true);
    assert_eq!(p_cached_timeout.primary, PrimaryTransport::Cronet);
    assert!(p_cached_timeout.allow_cef_escalation);
}

#[test]
fn site_cache_atomic_save_and_ttl_expiry() {
    let dir = temp_dir("site-cache");
    let cache_path = dir.join("site-cache.json");

    let cache = SiteCache::load(cache_path.clone());
    cache.record_at("example.com", "cef", 1200, 1_000_000);
    cache.record_at("slow.example.org", "cef_timeout", 60_000, 1_000_000);

    let reloaded = SiteCache::load(cache_path);
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

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn site_rules_bundled_parent_rewrite_and_local_overlay() {
    let dir = temp_dir("site-rules");
    let overlay = r#"{"rules":{"reuters.com":{"render":"cronet"},"custom.dev":{"render":"cef"}}}"#;
    std::fs::write(dir.join("site-rules.json"), overlay).unwrap();

    let rules = SiteRules::load(&dir);

    // Local overlay overrides bundled reuters.com rule, preserving www. parent lookup.
    assert_eq!(
        rules.get("www.reuters.com").and_then(|r| r.render.as_deref()),
        Some("cronet")
    );
    assert_eq!(
        rules.get("custom.dev").and_then(|r| r.render.as_deref()),
        Some("cef")
    );

    // Exact subdomain does not overreach.
    assert!(rules.get("finance.yahoo.com").is_some());
    assert!(rules.get("mail.yahoo.com").is_none());

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
        ResponseClassification::Blocked { status: 403 }
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

    let next_spa_shell = r#"<html><body><div id="__next"></div><script src="/a.js"></script></body></html>"#;
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
    assert_eq!(classify_response(200, true, &huge), ResponseClassification::Ok);
}
