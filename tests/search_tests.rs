use mesh_wick::auth::CredentialProvider;
use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::search::{
    build_brave_search_url, build_ddg_search_url, build_effective_query, civil_from_days,
    clean_ddg_url, clear_provider_cooldown, clear_search_state, days_from_civil, format_results,
    format_urls_only, is_lexical_needle, normalize_site, parse_brave_json, parse_ddg_html,
    parse_retry_after, provider_health_summary, search_provider_statuses, search_with_endpoints,
    Freshness, ProviderEndpoints, SearchMode, SearchOptions, SearchProvider, SearchResponse,
    SearchResult,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);
static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

const ALL_CRED_ENV_VARS: &[&str] = &[
    "BRAVE_API_KEY",
    "TAVILY_API_KEY",
    "SERPER_API_KEY",
    "EXA_API_KEY",
    "SEARXNG_URL",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn new(overrides: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::new();
        for &key in ALL_CRED_ENV_VARS
            .iter()
            .chain(&["WICK_NO_KEYCHAIN", "WICK_NO_HOME_CEF"])
        {
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

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn temp_state_dir() -> PathBuf {
    let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/wick-t-{}-s{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn test_ctx(state_dir: PathBuf) -> RuntimeContext {
    let cfg = RuntimeConfig {
        state_dir,
        timeout: Duration::from_secs(5),
        use_daemon: false,
        ..RuntimeConfig::default()
    };
    RuntimeContext::new(cfg).unwrap()
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Debug, Clone)]
struct MockHttpResponse {
    status_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl MockHttpResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status_line: format!("HTTP/1.1 {status} OK"),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.into(),
        }
    }

    fn html(status: u16, body: impl Into<String>) -> Self {
        Self {
            status_line: format!("HTTP/1.1 {status} OK"),
            headers: vec![(
                "Content-Type".to_string(),
                "text/html; charset=utf-8".to_string(),
            )],
            body: body.into(),
        }
    }

    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

async fn spawn_mock_server<F>(
    handler: F,
) -> (
    String,
    Arc<StdMutex<Vec<RecordedRequest>>>,
    tokio::task::JoinHandle<()>,
)
where
    F: Fn(&RecordedRequest, usize) -> MockHttpResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded = Arc::new(StdMutex::new(Vec::new()));
    let recorded_clone = recorded.clone();
    let handler = Arc::new(handler);

    let handle = tokio::spawn(async move {
        let mut count = 0usize;
        while let Ok((mut stream, _)) = listener.accept().await {
            let recorded_req = recorded_clone.clone();
            let handler_req = handler.clone();
            let idx = count;
            count += 1;
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end;
                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = pos + 4;
                        break;
                    }
                }
                let header_str = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = header_str.lines();
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                let content_length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + content_length {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(
                    &buf[header_end..(header_end + content_length).min(buf.len())],
                )
                .to_string();

                let req = RecordedRequest {
                    method,
                    target,
                    headers,
                    body,
                };
                let resp = handler_req(&req, idx);
                recorded_req.lock().unwrap().push(req);

                let mut raw = format!(
                    "{}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status_line,
                    resp.body.len()
                );
                for (k, v) in resp.headers {
                    raw.push_str(&format!("{k}: {v}\r\n"));
                }
                raw.push_str("\r\n");
                let _ = stream.write_all(raw.as_bytes()).await;
                let _ = stream.write_all(resp.body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (
        format!("http://127.0.0.1:{}", addr.port()),
        recorded,
        handle,
    )
}

const DDG_FIXTURE: &str = r#"
<html>
<body>
<div class="results">
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/y.js?ad_domain=ad.example">
          Sponsored Result
        </a>
      </h2>
      <div class="result__snippet">Ad copy</div>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc123">
          Rust   Programming
          Language
        </a>
      </h2>
      <div class="result__snippet">
        A   language empowering   everyone to build reliable software.
      </div>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="https://doc.rust-lang.org/book/">
          The Rust Programming Language - Book
        </a>
      </h2>
      <div class="result__snippet">
        The official book on the Rust programming language.
      </div>
    </div>
  </div>
</div>
</body>
</html>
"#;

#[test]
fn parse_brave_json_extracts_extra_snippets_and_limits_results() {
    let payload = r#"{
      "web": {
        "results": [
          {
            "title": "<strong>First</strong> &amp; Best",
            "url": "https://a.example",
            "description": "<strong>Rust</strong> &amp; <em>&#x27;Cargo&#x27;</em> guide",
            "extra_snippets": ["  First <b>extra</b> line  ", ""]
          },
          { "title": "<b></b>", "url": "https://empty-title.example", "description": "skip" },
          { "title": "Second", "url": "https://b.example", "description": "Snippet B" },
          { "title": "Third", "url": "https://c.example", "description": "Snippet C" }
        ]
      }
    }"#;

    let results = parse_brave_json(payload, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First & Best");
    assert_eq!(results[0].url, "https://a.example");
    assert_eq!(results[0].snippet, "Rust & 'Cargo' guide");
    assert_eq!(results[0].extra_snippets, vec!["First extra line"]);
    assert_eq!(results[1].title, "Second");
    assert!(results[1].extra_snippets.is_empty());

    let json = serde_json::to_string(&results).unwrap();
    assert!(json.contains("extra_snippets"));
    let second_only = serde_json::to_string(&results[1]).unwrap();
    assert!(!second_only.contains("extra_snippets"));
}

#[test]
fn parse_ddg_html_collapses_whitespace_filters_internal_links_and_unwraps_uddg() {
    let results = parse_ddg_html(DDG_FIXTURE, 5).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Rust Programming Language");
    assert_eq!(results[0].url, "https://rust-lang.org/");
    assert_eq!(
        results[0].snippet,
        "A language empowering everyone to build reliable software."
    );
    assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");

    assert_eq!(
        clean_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_%28programming_language%29&rut=xyz"),
        "https://en.wikipedia.org/wiki/Rust_(programming_language)"
    );
}

#[test]
fn search_query_normalization_freshness_urls_and_formatting() {
    assert_eq!(
        normalize_site("  HTTPS://docs.rs/tokio/ "),
        Some("docs.rs/tokio".to_string())
    );
    assert_eq!(normalize_site("  "), None);
    assert_eq!(
        build_effective_query("async spawn", Some("https://docs.rs/")),
        "site:docs.rs async spawn"
    );

    let brave_url = build_brave_search_url("rust", 5, Some(Freshness::Week), true);
    assert!(brave_url.contains("count=5"));
    assert!(brave_url.contains("freshness=pw"));
    assert!(brave_url.contains("extra_snippets=true"));

    let ddg_url = build_ddg_search_url("rust", Some(Freshness::Month));
    assert!(ddg_url.contains("df=m"));

    let empty = SearchResponse {
        provider: "brave",
        providers_used: vec![],
        results: vec![],
    };
    assert_eq!(format_results(&empty), "No results found.");
    assert_eq!(format_urls_only(&empty), "");

    let populated = SearchResponse {
        provider: "brave",
        providers_used: vec![],
        results: vec![
            SearchResult {
                title: "Example".to_string(),
                url: "https://example.com".to_string(),
                snippet: "A sample result".to_string(),
                extra_snippets: vec!["More context".to_string()],
            },
            SearchResult {
                title: "Two".to_string(),
                url: "https://two.example".to_string(),
                snippet: "Second".to_string(),
                extra_snippets: vec![],
            },
        ],
    };
    let formatted = format_results(&populated);
    assert!(formatted.contains("1. [Example](https://example.com)"));
    assert!(formatted.contains("A sample result"));
    assert!(formatted.contains("> More context"));
    assert_eq!(
        format_urls_only(&populated),
        "https://example.com\nhttps://two.example"
    );
}

#[test]
fn pure_std_utc_calendar_and_retry_after_parsing() {
    for &(y, m, d) in &[
        (1970, 1, 1),
        (2000, 2, 29),
        (2024, 2, 29),
        (2026, 11, 26),
        (2030, 12, 31),
    ] {
        let days = days_from_civil(y, m, d);
        assert_eq!(civil_from_days(days), (y, m, d));
    }
    assert_eq!(days_from_civil(1970, 1, 1), 0);

    // Integer Retry-After clamped 5..=300
    assert_eq!(parse_retry_after("0", 1_000), Some(5));
    assert_eq!(parse_retry_after("42", 1_000), Some(42));
    assert_eq!(parse_retry_after("999", 1_000), Some(300));

    // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT == 784111777
    let target = (days_from_civil(1994, 11, 6) as u64) * 86_400 + 8 * 3600 + 49 * 60 + 37;
    assert_eq!(target, 784_111_777);
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", target - 90),
        Some(90)
    );
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", target - 500),
        Some(300)
    );
    assert_eq!(
        parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", target + 100),
        Some(5)
    );
    assert_eq!(parse_retry_after("not-a-date", target), None);
    assert_eq!(
        parse_retry_after("Sun, 31 Feb 2026 08:49:37 GMT", target),
        None
    );
}

#[test]
fn strict_lexical_needle_detector_boundaries() {
    // Positive lexical needles
    assert!(is_lexical_needle("\"exact phrase\""));
    assert!(is_lexical_needle("find std::sync::Arc usage"));
    assert!(is_lexical_needle("rust E0382 borrow checker"));
    assert!(is_lexical_needle("socket ERR_CONNECTION_REFUSED"));
    assert!(is_lexical_needle("crash at 0xdeadbeef"));
    assert!(is_lexical_needle("check src/search/mod.rs"));
    assert!(is_lexical_needle("tokio-rs/tokio spawn"));

    // Negative (must NOT trigger Exact)
    assert!(!is_lexical_needle("\"\""));
    assert!(!is_lexical_needle("\"   \""));
    assert!(!is_lexical_needle("release 2026/11 notes"));
    assert!(!is_lexical_needle("50/50 split test"));
    assert!(!is_lexical_needle("pros/cons of rust"));
    assert!(!is_lexical_needle("and/or semantics"));
    assert!(!is_lexical_needle("ci/cd pipeline"));
    assert!(!is_lexical_needle("true/false flag"));
    assert!(!is_lexical_needle("yes/no prompt"));
    assert!(!is_lexical_needle("read/write lock"));
    assert!(!is_lexical_needle("input/output stream"));
    assert!(!is_lexical_needle("20x20 grid matrix"));
    assert!(!is_lexical_needle("how does rust async work"));
    assert!(!is_lexical_needle("code E03821 extra digit"));
}

#[tokio::test]
async fn tavily_serper_exa_searxng_protocol_and_mapping() {
    let _env_lock = ENV_LOCK.lock().await;
    let state_dir = temp_state_dir();

    let (tavily_base, tavily_reqs, tavily_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "results": [
                {
                  "title": "<b>Tokio</b> Spawn",
                  "url": "https://docs.rs/tokio/latest/tokio/fn.spawn.html",
                  "content": "First chunk text [...] Second <em>chunk</em> [...] Third chunk"
                }
              ]
            }"#,
        )
    })
    .await;

    let (serper_base, serper_reqs, serper_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "organic": [
                {
                  "title": "Rust E0382",
                  "link": "https://doc.rust-lang.org/error_codes/E0382.html",
                  "snippet": "Borrow of moved value",
                  "sitelinks": [
                    { "title": "Ownership", "link": "https://doc.rust-lang.org/book/ch04.html", "snippet": "Move semantics" },
                    { "title": "Clone", "link": "https://doc.rust-lang.org/std/clone/" }
                  ]
                }
              ]
            }"#,
        )
    })
    .await;

    let (exa_base, exa_reqs, exa_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "results": [
                {
                  "title": "Local-First CRDTs",
                  "url": "https://inkandswitch.com/local-first/",
                  "highlights": ["Primary highlight", "Secondary highlight"],
                  "text": "Fallback body text"
                },
                {
                  "url": "https://crdt.tech/",
                  "text": "Fallback text only"
                }
              ]
            }"#,
        )
    })
    .await;

    let (searx_base, searx_reqs, searx_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "results": [
                { "title": "One", "url": "https://one.example", "content": "First" },
                { "title": "Two", "url": "https://two.example", "content": "Second" },
                { "title": "Three", "url": "https://three.example", "content": "Third" }
              ]
            }"#,
        )
    })
    .await;

    // Pass trailing /search/ on SEARXNG_URL to verify normalization avoids /search/search
    let searx_with_suffix = format!("{searx_base}/search/");
    let _env = EnvGuard::new(&[
        ("TAVILY_API_KEY", "tvly-test-key"),
        ("SERPER_API_KEY", "serper-test-key"),
        ("EXA_API_KEY", "exa-test-key"),
        ("SEARXNG_URL", &searx_with_suffix),
    ]);

    let endpoints = ProviderEndpoints {
        tavily_url: format!("{tavily_base}/search"),
        serper_url: format!("{serper_base}/search"),
        exa_url: format!("{exa_base}/search"),
        ..ProviderEndpoints::default()
    };
    let ctx = test_ctx(state_dir.clone());

    // 1. Tavily with --site path + --extra-snippets + --freshness
    let resp = search_with_endpoints(
        &ctx,
        "spawn task",
        &SearchOptions {
            limit: 5,
            provider: SearchProvider::Tavily,
            freshness: Some(Freshness::Week),
            site: Some("https://docs.rs/tokio/".to_string()),
            extra_snippets: true,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(resp.provider, "tavily");
    assert_eq!(resp.results.len(), 1);
    assert_eq!(resp.results[0].title, "Tokio Spawn");
    assert_eq!(resp.results[0].snippet, "First chunk text");
    assert_eq!(
        resp.results[0].extra_snippets,
        vec!["Second chunk", "Third chunk"]
    );
    let treq = tavily_reqs.lock().unwrap()[0].clone();
    assert_eq!(treq.method, "POST");
    assert_eq!(
        treq.headers.get("authorization").map(String::as_str),
        Some("Bearer tvly-test-key")
    );
    let tbody: serde_json::Value = serde_json::from_str(&treq.body).unwrap();
    assert_eq!(tbody["max_results"], 5);
    assert_eq!(tbody["search_depth"], "advanced");
    assert_eq!(tbody["chunks_per_source"], 3);
    assert_eq!(tbody["time_range"], "week");
    assert_eq!(tbody["include_domains"], serde_json::json!(["docs.rs"]));
    assert_eq!(tbody["query"], "site:docs.rs/tokio spawn task");

    // 2. Serper with --extra-snippets + --freshness
    let resp = search_with_endpoints(
        &ctx,
        "E0382",
        &SearchOptions {
            limit: 4,
            provider: SearchProvider::Serper,
            freshness: Some(Freshness::Month),
            extra_snippets: true,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(resp.provider, "serper");
    assert_eq!(resp.results.len(), 1);
    assert_eq!(
        resp.results[0].url,
        "https://doc.rust-lang.org/error_codes/E0382.html"
    );
    assert_eq!(
        resp.results[0].extra_snippets,
        vec![
            "Ownership: Move semantics",
            "Clone (https://doc.rust-lang.org/std/clone/)"
        ]
    );
    let sreq = serper_reqs.lock().unwrap()[0].clone();
    assert_eq!(
        sreq.headers.get("x-api-key").map(String::as_str),
        Some("serper-test-key")
    );
    let sbody: serde_json::Value = serde_json::from_str(&sreq.body).unwrap();
    assert_eq!(sbody["num"], 4);
    assert_eq!(sbody["tbs"], "qdr:m");

    // 3. Exa with --extra-snippets + --freshness + host-only --site
    let resp = search_with_endpoints(
        &ctx,
        "local-first crdt",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Exa,
            freshness: Some(Freshness::Day),
            site: Some("https://inkandswitch.com/".to_string()),
            extra_snippets: true,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(resp.provider, "exa");
    assert_eq!(resp.results.len(), 2);
    assert_eq!(resp.results[0].snippet, "Primary highlight");
    assert_eq!(resp.results[0].extra_snippets, vec!["Secondary highlight"]);
    assert_eq!(resp.results[1].title, "https://crdt.tech/");
    assert_eq!(resp.results[1].snippet, "Fallback text only");
    let ereq = exa_reqs.lock().unwrap()[0].clone();
    assert_eq!(
        ereq.headers.get("x-api-key").map(String::as_str),
        Some("exa-test-key")
    );
    let ebody: serde_json::Value = serde_json::from_str(&ereq.body).unwrap();
    assert_eq!(ebody["numResults"], 3);
    assert_eq!(ebody["contents"]["highlights"]["maxCharacters"], 1200);
    assert!(ebody["contents"]["highlights"]
        .get("numSentences")
        .is_none());
    assert!(ebody["startPublishedDate"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        ebody["includeDomains"],
        serde_json::json!(["inkandswitch.com"])
    );
    assert_eq!(ebody["query"], "local-first crdt");

    // 4. SearXNG URL normalization + client-side .take(limit)
    let resp = search_with_endpoints(
        &ctx,
        "rust",
        &SearchOptions {
            limit: 2,
            provider: SearchProvider::Searxng,
            freshness: Some(Freshness::Day),
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(resp.provider, "searxng");
    assert_eq!(resp.results.len(), 2);
    let xreq = &searx_reqs.lock().unwrap()[0];
    assert!(xreq.target.starts_with("/search?format=json&"));
    assert!(!xreq.target.contains("/search/search"));
    assert!(xreq.target.contains("pageno=1"));
    assert!(xreq.target.contains("time_range=day"));

    tavily_srv.abort();
    serper_srv.abort();
    exa_srv.abort();
    searx_srv.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn ddg_202_and_anomaly_challenge_record_15s_cooldown() {
    let _env_lock = ENV_LOCK.lock().await;
    let state_dir = temp_state_dir();
    let _env = EnvGuard::new(&[]);

    let (ddg_base, _reqs, ddg_srv) = spawn_mock_server(|_, idx| {
        if idx == 0 {
            MockHttpResponse::html(202, "<html><body>accepted</body></html>")
        } else {
            MockHttpResponse::html(
                200,
                "<html><body><div class='anomaly-modal'>Unfortunately, bots use DuckDuckGo too.</div></body></html>",
            )
        }
    })
    .await;

    let endpoints = ProviderEndpoints {
        ddg_url: format!("{ddg_base}/html/"),
        ..ProviderEndpoints::default()
    };
    let ctx = test_ctx(state_dir.clone());

    // HTTP 202 -> error + 15s cooldown
    let err = search_with_endpoints(
        &ctx,
        "rust",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::DuckDuckGo,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("202"));
    let summary = provider_health_summary(&state_dir).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ddg_entry = summary.get("duckduckgo").unwrap();
    assert_eq!(ddg_entry.last_error_status, Some(202));
    let rem = ddg_entry.cooldown_until_epoch_secs.saturating_sub(now);
    assert!(
        (10..=16).contains(&rem),
        "expected ~15s cooldown, got {rem}s"
    );

    clear_search_state(&state_dir).unwrap();

    // HTTP 200 anomaly challenge with empty results -> error + 15s cooldown
    let err = search_with_endpoints(
        &ctx,
        "rust",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::DuckDuckGo,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("challenge"));
    let summary = provider_health_summary(&state_dir).unwrap();
    let ddg_entry = summary.get("duckduckgo").unwrap();
    let rem = ddg_entry.cooldown_until_epoch_secs.saturating_sub(now);
    assert!(
        (10..=16).contains(&rem),
        "expected ~15s cooldown, got {rem}s"
    );

    ddg_srv.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn cooldown_classification_retry_after_and_key_fingerprint_invalidation() {
    let _env_lock = ENV_LOCK.lock().await;
    let state_dir = temp_state_dir();

    let (brave_base, brave_reqs, brave_srv) = spawn_mock_server(|req, _| {
        let token = req
            .headers
            .get("x-subscription-token")
            .map(String::as_str)
            .unwrap_or("");
        match token {
            "key-429-int" => MockHttpResponse::json(429, r#"{"error":"rate"}"#)
                .with_header("Retry-After", "45"),
            "key-429-date" => {
                // Target ~120s in the future as IMF-fixdate
                let target = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 120;
                let (y, m, d) = civil_from_days((target / 86_400) as i64);
                let rem = target % 86_400;
                let months = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                let imf = format!(
                    "Thu, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
                    months[(m - 1) as usize],
                    rem / 3600,
                    (rem % 3600) / 60,
                    rem % 60
                );
                MockHttpResponse::json(429, r#"{"error":"rate"}"#).with_header("Retry-After", imf)
            }
            "key-401" => MockHttpResponse::json(401, r#"{"error":"unauthorized"}"#),
            "key-432" => MockHttpResponse::json(432, r#"{"error":"plan limit"}"#),
            "key-403-waf" => MockHttpResponse::html(403, "<html>Cloudflare WAF</html>"),
            "key-400" => MockHttpResponse::json(400, r#"{"error":"bad query"}"#),
            _ => MockHttpResponse::json(
                200,
                r#"{"web":{"results":[{"title":"Brave OK","url":"https://brave.example","description":"ok"}]}}"#,
            ),
        }
    })
    .await;

    let (tavily_base, _tavily_reqs, tavily_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{"results":[{"title":"Tavily Fallback","url":"https://tavily.example","content":"ok"}]}"#,
        )
    })
    .await;

    let endpoints = ProviderEndpoints {
        brave_url: format!("{brave_base}/web/search"),
        tavily_url: format!("{tavily_base}/search"),
        ..ProviderEndpoints::default()
    };
    let ctx = test_ctx(state_dir.clone());

    // 1. 429 with integer Retry-After: 45
    {
        let _env = EnvGuard::new(&[
            ("BRAVE_API_KEY", "key-429-int"),
            ("TAVILY_API_KEY", "tvly-ok"),
        ]);
        let resp = search_with_endpoints(
            &ctx,
            "general query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Auto,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp.provider, "tavily");
        let statuses = search_provider_statuses(&state_dir).unwrap();
        let rem = statuses["brave"].cooldown_remaining_secs;
        assert!(
            (40..=46).contains(&rem),
            "expected ~45s cooldown, got {rem}s"
        );
    }

    // 1b. 429 with IMF-fixdate Retry-After (~120s)
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", "key-429-date")]);
        let _ = search_with_endpoints(
            &ctx,
            "date retry",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Brave,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap_err();
        let statuses = search_provider_statuses(&state_dir).unwrap();
        let rem = statuses["brave"].cooldown_remaining_secs;
        assert!(
            (110..=125).contains(&rem),
            "expected ~120s IMF-fixdate cooldown, got {rem}s"
        );
    }

    // 1c. 432 plan limit -> 86_400s cooldown
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", "key-432")]);
        let _ = search_with_endpoints(
            &ctx,
            "quota limit",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Brave,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap_err();
        let statuses = search_provider_statuses(&state_dir).unwrap();
        assert!(statuses["brave"].cooldown_remaining_secs > 80_000);
        assert_eq!(statuses["brave"].last_error_status, Some(432));
    }

    // 2. 401 -> 86_400s cooldown; Auto skips Brave; rotating key invalidates cooldown immediately!
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", "key-401"), ("TAVILY_API_KEY", "tvly-ok")]);
        let resp = search_with_endpoints(
            &ctx,
            "general query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Auto,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp.provider, "tavily");
        let statuses = search_provider_statuses(&state_dir).unwrap();
        assert!(statuses["brave"].cooldown_remaining_secs > 80_000);
        assert_eq!(statuses["brave"].last_error_status, Some(401));

        let reqs_before = brave_reqs.lock().unwrap().len();
        // Same key -> Brave skipped without HTTP request
        let resp2 = search_with_endpoints(
            &ctx,
            "another query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Auto,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp2.provider, "tavily");
        assert_eq!(brave_reqs.lock().unwrap().len(), reqs_before);

        // Rotate key -> fingerprint changes, cooldown ignored immediately!
        std::env::set_var("BRAVE_API_KEY", "key-good-rotated");
        let statuses_after = search_provider_statuses(&state_dir).unwrap();
        assert_eq!(statuses_after["brave"].cooldown_remaining_secs, 0);
        // Clear last_used burst timestamp via explicit override check: first wait or test explicit
        let resp3 = search_with_endpoints(
            &ctx,
            "rotated query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Brave,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp3.provider, "brave");
        let statuses_cleared = search_provider_statuses(&state_dir).unwrap();
        assert_eq!(statuses_cleared["brave"].cooldown_remaining_secs, 0);
        assert_eq!(statuses_cleared["brave"].last_error_status, None);
    }

    // 3. Non-JSON HTML WAF 403 -> downgraded to 300s (not 86_400s)
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", "key-403-waf")]);
        let _ = search_with_endpoints(
            &ctx,
            "waf query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Brave,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap_err();
        let statuses = search_provider_statuses(&state_dir).unwrap();
        let rem = statuses["brave"].cooldown_remaining_secs;
        assert!(
            (290..=301).contains(&rem),
            "expected ~300s WAF cooldown, got {rem}s"
        );
        clear_provider_cooldown(&state_dir, CredentialProvider::Brave).unwrap();
        let statuses2 = search_provider_statuses(&state_dir).unwrap();
        assert_eq!(statuses2["brave"].cooldown_remaining_secs, 0);
    }

    // 4. 400 does NOT poison provider cooldown
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[("BRAVE_API_KEY", "key-400")]);
        let _ = search_with_endpoints(
            &ctx,
            "bad query",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Brave,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap_err();
        let statuses = search_provider_statuses(&state_dir).unwrap();
        assert_eq!(statuses["brave"].cooldown_remaining_secs, 0);
    }

    brave_srv.abort();
    tavily_srv.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn brave_burst_spillover_and_intent_routing() {
    let _env_lock = ENV_LOCK.lock().await;
    let state_dir = temp_state_dir();

    let (brave_base, _, brave_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{"web":{"results":[{"title":"From Brave","url":"https://brave.example","description":"b"}]}}"#,
        )
    })
    .await;
    let (tavily_base, _, tavily_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{"results":[{"title":"From Tavily","url":"https://tavily.example","content":"t"}]}"#,
        )
    })
    .await;
    let (serper_base, _, serper_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{"organic":[{"title":"From Serper","link":"https://serper.example","snippet":"s"}]}"#,
        )
    })
    .await;
    let (exa_base, _, exa_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{"results":[{"title":"From Exa","url":"https://exa.example","highlights":["e"]}]}"#,
        )
    })
    .await;

    let endpoints = ProviderEndpoints {
        brave_url: format!("{brave_base}/web/search"),
        tavily_url: format!("{tavily_base}/search"),
        serper_url: format!("{serper_base}/search"),
        exa_url: format!("{exa_base}/search"),
        ..ProviderEndpoints::default()
    };
    let ctx = test_ctx(state_dir.clone());

    let _env = EnvGuard::new(&[
        ("BRAVE_API_KEY", "b-key"),
        ("TAVILY_API_KEY", "t-key"),
        ("SERPER_API_KEY", "s-key"),
        ("EXA_API_KEY", "e-key"),
    ]);

    // 1. General query with --site alone does NOT force Exact -> picks Brave first
    let r1 = search_with_endpoints(
        &ctx,
        "how does rust async work",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Auto,
            site: Some("docs.rs".to_string()),
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(r1.provider, "brave");

    // 2. Immediate second general query (< 1100ms) -> Brave burst demoted -> spills to Tavily!
    let r2 = search_with_endpoints(
        &ctx,
        "rust tokio tutorial",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Auto,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(r2.provider, "tavily");

    // 3. Lexical needle query -> routes to Serper (Exact order: Searxng -> Serper -> ...)
    let r3 = search_with_endpoints(
        &ctx,
        "E0382 borrow of moved value",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Auto,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(r3.provider, "serper");

    // 4. Semantic mode -> routes to Exa
    let r4 = search_with_endpoints(
        &ctx,
        "local first collaborative software",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Auto,
            mode: SearchMode::Semantic,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(r4.provider, "exa");

    // 5. --extra-snippets with clean state -> routes to Brave (even if SearXNG were configured)
    clear_search_state(&state_dir).unwrap();
    let r5 = search_with_endpoints(
        &ctx,
        "rust async streams",
        &SearchOptions {
            limit: 3,
            provider: SearchProvider::Auto,
            extra_snippets: true,
            ..SearchOptions::default()
        },
        &endpoints,
    )
    .await
    .unwrap();
    assert_eq!(r5.provider, "brave");

    brave_srv.abort();
    tavily_srv.abort();
    serper_srv.abort();
    exa_srv.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn federated_rrf_merging_partial_failure_tie_breaking_and_zero_key_degradation() {
    let _env_lock = ENV_LOCK.lock().await;
    let state_dir = temp_state_dir();

    // Brave (slot 0): shared at rank 1 (plus intra-provider dup at rank 2), brave-only at rank 3
    let (brave_base, brave_reqs, brave_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "web": {
                "results": [
                  { "title": "Shared Guide", "url": "https://shared.example/guide/", "description": "Short", "extra_snippets": ["E1", "E2", "E3", "E4"] },
                  { "title": "Shared Dup", "url": "https://shared.example/guide?utm_source=brave", "description": "Dup" },
                  { "title": "Brave Unique", "url": "https://tie-b.example", "description": "Brave tie" }
                ]
              }
            }"#,
        )
    })
    .await;

    // Serper (slot 1): tie-a at rank 1, shared at rank 2 (richer snippet!)
    let (serper_base, serper_reqs, serper_srv) = spawn_mock_server(|_, idx| {
        if idx == 1 {
            // Second test call simulates partial failure (HTTP 500)
            return MockHttpResponse::json(500, r#"{"error":"upstream"}"#);
        }
        MockHttpResponse::json(
            200,
            r#"{
              "organic": [
                { "title": "Serper Unique", "link": "https://tie-s.example", "snippet": "Serper tie" },
                { "title": "Shared Guide", "link": "https://shared.example/guide", "snippet": "Much richer and longer primary snippet from Serper", "sitelinks": [
                  { "title": "S1", "snippet": "Extra 5" },
                  { "title": "S2", "snippet": "Extra 6 should be capped" }
                ]}
              ]
            }"#,
        )
    })
    .await;

    // Exa (slot 2): exa-only at rank 1
    let (exa_base, exa_reqs, exa_srv) = spawn_mock_server(|_, _| {
        MockHttpResponse::json(
            200,
            r#"{
              "results": [
                { "title": "Exa Unique", "url": "https://tie-e.example", "highlights": ["Exa tie"] }
              ]
            }"#,
        )
    })
    .await;

    let (ddg_base, _, ddg_srv) =
        spawn_mock_server(|_, _| MockHttpResponse::html(200, DDG_FIXTURE)).await;

    let endpoints = ProviderEndpoints {
        brave_url: format!("{brave_base}/web/search"),
        serper_url: format!("{serper_base}/search"),
        exa_url: format!("{exa_base}/search"),
        ddg_url: format!("{ddg_base}/html/"),
        ..ProviderEndpoints::default()
    };
    let ctx = test_ctx(state_dir.clone());

    // 1. Full 3-provider federated merge with limit=3 (sub-provider limit must be 10)
    {
        let _env = EnvGuard::new(&[
            ("BRAVE_API_KEY", "b-key"),
            ("SERPER_API_KEY", "s-key"),
            ("EXA_API_KEY", "e-key"),
        ]);

        let resp = search_with_endpoints(
            &ctx,
            "rust reqwest",
            &SearchOptions {
                limit: 3,
                provider: SearchProvider::Federated,
                extra_snippets: true,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();

        assert_eq!(resp.provider, "federated");
        assert_eq!(resp.providers_used, vec!["brave", "serper", "exa"]);
        assert_eq!(resp.results.len(), 3);

        // Shared URL ranked #1 (present in Brave rank 1 + Serper rank 2)
        assert_eq!(resp.results[0].url, "https://shared.example/guide/");
        assert_eq!(
            resp.results[0].snippet,
            "Much richer and longer primary snippet from Serper"
        );
        // Capped at MAX_EXTRA_SNIPPETS = 5
        assert_eq!(resp.results[0].extra_snippets.len(), 5);

        // Tie at score 1/61 between Serper (slot 1, rank 1) and Exa (slot 2, rank 1):
        // Serper wins on first_provider_slot (1 < 2)!
        assert_eq!(resp.results[1].url, "https://tie-s.example");
        assert_eq!(resp.results[2].url, "https://tie-e.example");

        // Sub-provider limit was limit.max(10).min(20) == 10
        assert!(brave_reqs.lock().unwrap()[0].target.contains("count=10"));
        let sbody: serde_json::Value =
            serde_json::from_str(&serper_reqs.lock().unwrap()[0].body).unwrap();
        assert_eq!(sbody["num"], 10);
        let ebody: serde_json::Value =
            serde_json::from_str(&exa_reqs.lock().unwrap()[0].body).unwrap();
        assert_eq!(ebody["numResults"], 10);

        // 2. Partial failure: Serper returns 500, Brave + Exa succeed!
        clear_search_state(&state_dir).unwrap();
        let resp2 = search_with_endpoints(
            &ctx,
            "rust reqwest",
            &SearchOptions {
                limit: 5,
                provider: SearchProvider::Federated,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp2.provider, "federated");
        assert_eq!(resp2.providers_used, vec!["brave", "exa"]);
        let statuses = search_provider_statuses(&state_dir).unwrap();
        assert!(statuses["serper"].cooldown_remaining_secs > 0);
        assert_eq!(statuses["serper"].last_error_status, Some(500));
    }

    // 3. Zero credentials -> degrades cleanly to [DuckDuckGo]
    clear_search_state(&state_dir).unwrap();
    {
        let _env = EnvGuard::new(&[]);
        let resp = search_with_endpoints(
            &ctx,
            "rust",
            &SearchOptions {
                limit: 5,
                provider: SearchProvider::Federated,
                ..SearchOptions::default()
            },
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(resp.provider, "federated");
        assert_eq!(resp.providers_used, vec!["duckduckgo"]);
        assert_eq!(resp.results.len(), 2);
    }

    brave_srv.abort();
    serper_srv.abort();
    exa_srv.abort();
    ddg_srv.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}
