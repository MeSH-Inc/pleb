use pleb::context::{RuntimeConfig, RuntimeContext};
use pleb::fetch::{
    fetch_batch, fetch_document, BatchFetchOptions, BatchItemResult, FetchError, FetchOptions,
};
use pleb::policy::{BlockReason, RenderMode, SourceMode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

static CANARY_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

fn hermetic_state_dir(tag: &str) -> std::path::PathBuf {
    let seq = CANARY_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::path::PathBuf::from(format!("/tmp/pleb-t-{}-{tag}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

async fn spawn_diagnostic_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let rate_once_hits = Arc::new(AtomicUsize::new(0));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let rate_hits = Arc::clone(&rate_once_hits);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let accept_header = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("accept:"))
                    .map(|l| l.split_once(':').map(|x| x.1.trim()).unwrap_or(""))
                    .unwrap_or("");

                let response = match path {
                    "/cf-waf" => concat!(
                        "HTTP/1.1 403 Forbidden\r\n",
                        "Content-Type: text/html\r\n",
                        "Server: cloudflare\r\n",
                        "CF-Ray: 123abc-SJC\r\n",
                        "Content-Length: 13\r\n",
                        "Connection: close\r\n\r\n",
                        "Access denied"
                    )
                    .to_string(),
                    "/cf-mitigated" => concat!(
                        "HTTP/1.1 403 Forbidden\r\n",
                        "Content-Type: text/plain\r\n",
                        "Server: cloudflare\r\n",
                        "CF-Mitigated: challenge\r\n",
                        "Content-Length: 13\r\n",
                        "Connection: close\r\n\r\n",
                        "Access denied"
                    )
                    .to_string(),
                    "/aws-waf" => concat!(
                        "HTTP/1.1 403 Forbidden\r\n",
                        "Content-Type: text/plain\r\n",
                        "x-amzn-waf-action: challenge\r\n",
                        "Content-Length: 7\r\n",
                        "Connection: close\r\n\r\n",
                        "Blocked"
                    )
                    .to_string(),
                    "/neg-406" => {
                        if accept_header.starts_with("text/markdown") {
                            concat!(
                                "HTTP/1.1 406 Not Acceptable\r\n",
                                "Content-Type: text/plain\r\n",
                                "Content-Length: 14\r\n",
                                "Connection: close\r\n\r\n",
                                "Not Acceptable"
                            )
                            .to_string()
                        } else {
                            let body = "<html><head><title>Fallback</title></head><body><h1>Fallback HTML</h1><p>Content recovered after 406.</p></body></html>";
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        }
                    }
                    "/rate-once" => {
                        let attempt = rate_hits.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            concat!(
                                "HTTP/1.1 429 Too Many Requests\r\n",
                                "Content-Type: text/plain\r\n",
                                "Retry-After: 0\r\n",
                                "Content-Length: 10\r\n",
                                "Connection: close\r\n\r\n",
                                "Rate limit"
                            )
                            .to_string()
                        } else {
                            let body = "<html><head><title>Recovered</title></head><body><h1>Recovered 429</h1><p>Done.</p></body></html>";
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        }
                    }
                    "/rate-hard" => concat!(
                        "HTTP/1.1 429 Too Many Requests\r\n",
                        "Content-Type: text/plain\r\n",
                        "Retry-After: 60\r\n",
                        "Content-Length: 17\r\n",
                        "Connection: close\r\n\r\n",
                        "Too many requests"
                    )
                    .to_string(),
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn hermetic_403_block_diagnostics_and_challenge_headers() {
    let base = spawn_diagnostic_origin().await;
    let state_dir = hermetic_state_dir("diag");
    let ctx = Arc::new(
        RuntimeContext::new(RuntimeConfig {
            state_dir: state_dir.clone(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            use_daemon: false,
            no_web_bot_auth: true,
            cef_renderer_executable: Some(state_dir.join("missing-cef")),
            ..Default::default()
        })
        .unwrap(),
    );
    let cronet_opts = FetchOptions {
        render: RenderMode::Cronet,
        ..Default::default()
    };

    // 1. 403 with Server: cloudflare + CF-Ray -> EdgeWafBlock + diagnostic summary.
    let err_cf = fetch_document(&ctx, &format!("{base}/cf-waf"), &cronet_opts)
        .await
        .unwrap_err();
    match &err_cf {
        FetchError::Blocked {
            status,
            reason,
            detail,
            ..
        } => {
            assert_eq!(*status, 403);
            assert_eq!(*reason, BlockReason::EdgeWafBlock);
            assert!(detail.contains("server=cloudflare"), "detail={detail}");
            assert!(detail.contains("cf-ray=123abc-SJC"), "detail={detail}");
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
    assert_eq!(err_cf.block_reason(), Some(BlockReason::EdgeWafBlock));

    // CLI --json emits block_reason on stderr envelope.
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let cli_403 = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/cf-waf"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(cli_403.status.code(), Some(2));
    let err_json: serde_json::Value = serde_json::from_slice(&cli_403.stderr).unwrap();
    assert_eq!(err_json["kind"], "blocked");
    assert_eq!(err_json["block_reason"], "edge_waf_block");
    assert_eq!(err_json["exit_code"], 2);
    assert!(err_json["error"].as_str().unwrap().contains("cloudflare"));

    // 2. CF-Mitigated: challenge -> Challenge error with CloudflareChallenge block_reason.
    let err_mitigated = fetch_document(
        &ctx,
        &format!("{base}/cf-mitigated"),
        &FetchOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err_mitigated,
        FetchError::Challenge { status: 403, .. }
    ));
    assert_eq!(
        err_mitigated.block_reason(),
        Some(BlockReason::CloudflareChallenge)
    );

    // 3. x-amzn-waf-action: challenge -> Challenge error with BotChallenge block_reason.
    let err_aws = fetch_document(&ctx, &format!("{base}/aws-waf"), &FetchOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err_aws, FetchError::Challenge { status: 403, .. }));
    assert_eq!(err_aws.block_reason(), Some(BlockReason::BotChallenge));

    // 4. 406 Not Acceptable on Accept: text/markdown falls back cleanly to standard HTML Accept.
    let neg_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Negotiate,
        ..Default::default()
    };
    let ok_406 = fetch_document(&ctx, &format!("{base}/neg-406"), &neg_opts)
        .await
        .unwrap();
    assert_eq!(ok_406.status_code, 200);
    assert!(ok_406.content.contains("Content recovered after 406."));

    // 5. 429 Retry-After: 0 retries once and succeeds; Retry-After: 60 fails fast with RateLimited.
    let ok_429 = fetch_document(&ctx, &format!("{base}/rate-once"), &cronet_opts)
        .await
        .unwrap();
    assert_eq!(ok_429.status_code, 200);
    assert!(ok_429.content.contains("Recovered 429"));

    let err_429 = fetch_document(&ctx, &format!("{base}/rate-hard"), &cronet_opts)
        .await
        .unwrap_err();
    assert_eq!(err_429.exit_code(), 2);
    match &err_429 {
        FetchError::Blocked {
            status,
            reason,
            detail,
            ..
        } => {
            assert_eq!(*status, 429);
            assert_eq!(*reason, BlockReason::RateLimited);
            assert!(detail.contains("60s"), "detail={detail}");
        }
        other => panic!("expected RateLimited Blocked, got {other:?}"),
    }

    // 6. Batch failure carries block_reason.
    let batch_res = fetch_batch(
        Arc::clone(&ctx),
        vec![format!("{base}/cf-waf")],
        BatchFetchOptions {
            fetch: cronet_opts,
            concurrency: 1,
            metadata_only: true,
        },
        |_, _| {},
    )
    .await;
    match &batch_res[0].1 {
        BatchItemResult::Failure(f) => {
            assert_eq!(f.kind, "blocked");
            assert_eq!(f.block_reason, Some(BlockReason::EdgeWafBlock));
            assert_eq!(f.exit_code, 2);
        }
        other => panic!("expected failure item, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
#[ignore]
async fn live_harness_403_unblock_canary() {
    let state_dir = hermetic_state_dir("live-canary");
    std::env::set_var("PLEB_NO_KEYCHAIN", "1");

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.clone(),
        timeout: Duration::from_secs(20),
        cef_timeout: Duration::from_secs(30),
        use_daemon: false,
        no_web_bot_auth: true,
        ..Default::default()
    })
    .unwrap();

    let urls = [
        "https://developers.cloudflare.com/fundamentals/reference/markdown-for-agents/",
        "https://www.reddit.com/r/rust/",
        "https://github.com/rust-lang/rust/blob/master/README.md",
        "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        "https://sqlite.org/wal.html",
        "https://doc.rust-lang.org/std/sync/struct.Mutex.html",
    ];

    let mut passes = 0usize;
    eprintln!("url | outcome | status | strategy | escalated | timing_ms | block_reason");
    for url in urls {
        match fetch_document(&ctx, url, &FetchOptions::default()).await {
            Ok(out) => {
                let non_empty = !out.content.trim().is_empty();
                if non_empty {
                    passes += 1;
                }
                eprintln!(
                    "{url} | {} | {} | {} | {} | {}ms | -",
                    if non_empty { "ok" } else { "empty" },
                    out.status_code,
                    out.strategy,
                    out.escalated,
                    out.timing_ms
                );
            }
            Err(err) => {
                eprintln!(
                    "{url} | err ({}) | - | - | - | - | {:?}",
                    err.kind(),
                    err.block_reason()
                );
            }
        }
    }

    let _ = std::fs::remove_dir_all(&state_dir);
    assert!(
        passes >= 5,
        "live harness 403 canary passed only {passes}/{} URLs",
        urls.len()
    );
}
