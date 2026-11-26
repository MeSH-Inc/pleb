use flate2::write::GzEncoder;
use flate2::Compression;
use pleb::context::{RuntimeConfig, RuntimeContext};
use pleb::crawl::{crawl, map, CrawlOptions, MapOptions};
use pleb::extract::{ExtractMode, ExtractOptions, OutputFormat};
use pleb::fetch::{
    batch_exit_code, fetch_batch, fetch_document, fetch_raw, parse_batch_stdin, BatchFetchOptions,
    BatchItemResult, FetchError, FetchOptions,
};
use pleb::policy::{
    parse_markdown_link_header, resolve_raw_source_candidate, RawSourceKind, RenderMode,
    SourceMode, ToolPolicy,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{}", std::process::id(), seq));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_state_dir() -> TempDir {
    TempDir::new()
}

async fn spawn_mock_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let response: Vec<u8> = match path {
                    "/page" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Mock Page</title></head><body><nav>NavLinks</nav><main><h1>Hello</h1><p>Main body content.</p></main></body></html>",
                    ),
                    "/docs/sub/entry" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Nested</title></head><body><main><a href=\"guide?utm_source=x&amp;v=1\">Guide</a></main></body></html>",
                    ),
                    "/redirect" => {
                        b"HTTP/1.1 302 Found\r\nLocation: /docs/sub/entry\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                    }
                    "/data.json" => http_response(
                        "200 OK",
                        "application/json",
                        br#"{"ok":true,"count":42}"#,
                    ),
                    "/doc.pdf" => http_response(
                        "200 OK",
                        "application/pdf",
                        b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n",
                    ),
                    "/logo.svg" => http_response(
                        "200 OK",
                        "image/svg+xml",
                        b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
                    ),
                    "/latin1" => http_response(
                        "200 OK",
                        "text/html; charset=iso-8859-1",
                        b"<html><body><p>Caf\xe9 menu</p></body></html>",
                    ),
                    "/missing.png" => http_response(
                        "404 Not Found",
                        "image/png",
                        b"\x89PNG\r\n\x1a\n",
                    ),
                    "/blocked" => http_response("403 Forbidden", "text/html", b"Access denied"),
                    "/missing" => http_response("404 Not Found", "text/plain", b"Not found"),
                    "/robots.txt" => http_response(
                        "200 OK",
                        "text/plain",
                        b"User-agent: *\nDisallow: /private\n",
                    ),
                    "/private" => http_response(
                        "200 OK",
                        "text/html",
                        b"<html><body><p>Private content</p></body></html>",
                    ),
                    "/binary" => http_response(
                        "200 OK",
                        "application/octet-stream",
                        &[0x00, 0xFF, 0xFE, 0x41, 0x42],
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"unknown"),
                };

                let _ = socket.write_all(&response).await;
            });
        }
    });

    format!("http://{}", addr)
}

fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

#[tokio::test]
async fn fetch_pipeline_end_to_end_against_local_server() {
    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let cronet_opts = FetchOptions {
        render: RenderMode::Cronet,
        ..Default::default()
    };

    // 1. HTML fetch strips nav and keeps main content.
    let page = fetch_document(&ctx, &format!("{base}/page"), &cronet_opts)
        .await
        .unwrap();
    assert_eq!(page.status_code, 200);
    assert_eq!(page.title.as_deref(), Some("Mock Page"));
    assert!(page.content.contains("Main body content."));
    assert!(!page.content.contains("NavLinks"));

    // 2. Redirect captures final_url and resolves relative links against post-redirect path.
    let redir = fetch_document(&ctx, &format!("{base}/redirect"), &cronet_opts)
        .await
        .unwrap();
    assert!(redir.final_url.ends_with("/docs/sub/entry"));
    assert!(redir
        .content
        .contains(&format!("[Guide]({base}/docs/sub/guide?v=1)")));

    // 3. JSON passes through unmodified.
    let json_doc = fetch_document(&ctx, &format!("{base}/data.json"), &cronet_opts)
        .await
        .unwrap();
    assert_eq!(json_doc.content, r#"{"ok":true,"count":42}"#);
    assert!(json_doc.title.is_none());

    // 4. 403 returns FetchError::Blocked with exit code 2.
    let err_403 = fetch_document(&ctx, &format!("{base}/blocked"), &cronet_opts)
        .await
        .unwrap_err();
    assert!(matches!(err_403, FetchError::Blocked { status: 403, .. }));
    assert_eq!(err_403.exit_code(), 2);

    // 5. 404 returns FetchError::HttpError with exit code 3.
    let err_404 = fetch_document(&ctx, &format!("{base}/missing"), &cronet_opts)
        .await
        .unwrap_err();
    assert!(matches!(err_404, FetchError::HttpError { status: 404, .. }));
    assert_eq!(err_404.exit_code(), 3);

    // 6. robots.txt is skipped by default and enforced when respect_robots=true.
    let allowed = fetch_document(&ctx, &format!("{base}/private"), &cronet_opts)
        .await
        .unwrap();
    assert!(allowed.content.contains("Private content"));

    let robots_opts = FetchOptions {
        respect_robots: true,
        extract: ExtractOptions {
            format: OutputFormat::Markdown,
            ..Default::default()
        },
        ..cronet_opts.clone()
    };
    let err_robots = fetch_document(&ctx, &format!("{base}/private"), &robots_opts)
        .await
        .unwrap_err();
    assert!(matches!(err_robots, FetchError::RobotsDisallowed { .. }));
    assert_eq!(err_robots.exit_code(), 2);

    // 7. Raw fetch preserves arbitrary binary bytes cleanly.
    let raw = fetch_raw(&ctx, &format!("{base}/binary"), false)
        .await
        .unwrap();
    assert_eq!(raw.bytes, vec![0x00, 0xFF, 0xFE, 0x41, 0x42]);

    // 8. Binary guard rejects PDF and NUL octet-stream with exit code 5,
    // allows SVG passthrough and Latin-1 HTML, and keeps 404 PNG as HttpError (exit 3).
    let err_pdf = fetch_document(&ctx, &format!("{base}/doc.pdf"), &cronet_opts)
        .await
        .unwrap_err();
    assert!(matches!(err_pdf, FetchError::UnsupportedContentType { .. }));
    assert_eq!(err_pdf.exit_code(), 5);

    let err_bin = fetch_document(&ctx, &format!("{base}/binary"), &cronet_opts)
        .await
        .unwrap_err();
    assert_eq!(err_bin.exit_code(), 5);

    let svg = fetch_document(&ctx, &format!("{base}/logo.svg"), &cronet_opts)
        .await
        .unwrap();
    assert!(svg.content.contains("<svg"));

    let latin1 = fetch_document(&ctx, &format!("{base}/latin1"), &cronet_opts)
        .await
        .unwrap();
    assert!(latin1.content.contains("Caf"));

    let err_404_png = fetch_document(&ctx, &format!("{base}/missing.png"), &cronet_opts)
        .await
        .unwrap_err();
    assert!(matches!(
        err_404_png,
        FetchError::HttpError { status: 404, .. }
    ));
    assert_eq!(err_404_png.exit_code(), 3);

    drop(ctx);
}

#[tokio::test]
async fn fetch_concurrent_requests_and_clean_teardown() {
    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let opts = FetchOptions {
        render: RenderMode::Cronet,
        ..Default::default()
    };

    let u1 = format!("{base}/page");
    let u2 = format!("{base}/data.json");
    let u3 = format!("{base}/redirect");
    let u4 = format!("{base}/logo.svg");
    let (r1, r2, r3, r4) = tokio::join!(
        fetch_document(&ctx, &u1, &opts),
        fetch_document(&ctx, &u2, &opts),
        fetch_document(&ctx, &u3, &opts),
        fetch_document(&ctx, &u4, &opts),
    );

    assert_eq!(r1.unwrap().status_code, 200);
    assert_eq!(r2.unwrap().content, r#"{"ok":true,"count":42}"#);
    assert!(r3.unwrap().final_url.ends_with("/docs/sub/entry"));
    assert!(r4.unwrap().content.contains("<svg"));

    drop(ctx);
}

#[tokio::test]
async fn fetch_enforces_max_response_bytes() {
    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        max_response_bytes: Some(16),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let opts = FetchOptions {
        render: RenderMode::Cronet,
        ..Default::default()
    };

    let err_doc = fetch_document(&ctx, &format!("{base}/page"), &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err_doc,
            FetchError::ResponseTooLarge {
                limit_bytes: 16,
                ..
            }
        ),
        "unexpected document error: {err_doc:?}"
    );
    assert_eq!(err_doc.exit_code(), 1);
    assert_eq!(err_doc.kind(), "response_too_large");

    let err_raw = fetch_raw(&ctx, &format!("{base}/page"), false)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err_raw,
            FetchError::ResponseTooLarge {
                limit_bytes: 16,
                ..
            }
        ),
        "unexpected raw error: {err_raw:?}"
    );

    drop(ctx);
}

#[tokio::test]
async fn fetch_batch_enforces_concurrency_and_exit_codes() {
    assert_eq!(
        parse_batch_stdin("\n# comment\n  https://a.example  \n\nhttps://b.example\n"),
        vec![
            "https://a.example".to_string(),
            "https://b.example".to_string()
        ]
    );

    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir();
    let ctx = Arc::new(
        RuntimeContext::new(RuntimeConfig {
            state_dir: state_dir.path().to_path_buf(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            ..Default::default()
        })
        .unwrap(),
    );

    let batch_opts = BatchFetchOptions {
        fetch: FetchOptions {
            render: RenderMode::Cronet,
            ..Default::default()
        },
        concurrency: 4,
        metadata_only: true,
    };

    let mut streamed = 0usize;
    let mut mixed = fetch_batch(
        Arc::clone(&ctx),
        vec![
            format!("{base}/page"),
            format!("{base}/missing"),
            format!("{base}/data.json"),
        ],
        batch_opts.clone(),
        |_, item| {
            streamed += 1;
            if let BatchItemResult::Success(s) = item {
                assert!(s.content.is_none());
            }
        },
    )
    .await;
    assert_eq!(streamed, 3);
    mixed.sort_by_key(|(idx, _)| *idx);
    let mixed_items: Vec<BatchItemResult> = mixed.into_iter().map(|(_, item)| item).collect();
    assert_eq!(batch_exit_code(&mixed_items), 4);

    let all_404 = fetch_batch(
        Arc::clone(&ctx),
        vec![format!("{base}/missing"), format!("{base}/missing2")],
        batch_opts.clone(),
        |_, _| {},
    )
    .await;
    let all_404_items: Vec<BatchItemResult> = all_404.into_iter().map(|(_, i)| i).collect();
    assert_eq!(batch_exit_code(&all_404_items), 3);

    let hetero = fetch_batch(
        Arc::clone(&ctx),
        vec![format!("{base}/blocked"), format!("{base}/missing")],
        batch_opts,
        |_, _| {},
    )
    .await;
    let hetero_items: Vec<BatchItemResult> = hetero.into_iter().map(|(_, i)| i).collect();
    assert_eq!(batch_exit_code(&hetero_items), 1);

    drop(ctx);
}

#[tokio::test]
async fn crawl_updates_origin_after_seed_redirect() {
    // Target origin serves /home linking to /child and back to /home.
    let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_base = format!("http://{}", target_listener.local_addr().unwrap());
    let target_base_srv = target_base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = target_listener.accept().await else {
                break;
            };
            let base = target_base_srv.clone();
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
                let resp = match path {
                    "/home" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        format!(
                            "<html><head><title>Home</title></head><body><main>\
                             <a href=\"{base}/home\">Self</a>\
                             <a href=\"/child\">Child</a>\
                             </main></body></html>"
                        )
                        .as_bytes(),
                    ),
                    "/child" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Child</title></head><body><main><p>Child body</p></main></body></html>",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    // Seed origin redirects /start -> target_base/home (different origin).
    let seed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let seed_base = format!("http://{}", seed_listener.local_addr().unwrap());
    let redirect_target = format!("{target_base}/home");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = seed_listener.accept().await else {
                break;
            };
            let loc = redirect_target.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 301 Moved Permanently\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let res = crawl(
        &ctx,
        &format!("{seed_base}/start"),
        CrawlOptions {
            max_depth: 1,
            max_pages: 5,
            format: OutputFormat::Markdown,
            extract_mode: ExtractMode::Clean,
            respect_robots: false,
            path_filter: None,
        },
    )
    .await
    .unwrap();

    let urls: Vec<&str> = res.pages.iter().map(|p| p.url.as_str()).collect();
    assert_eq!(
        urls,
        vec![
            format!("{target_base}/home"),
            format!("{target_base}/child")
        ]
    );
}

#[tokio::test]
async fn crawl_enforces_max_pages_and_deterministic_order_under_concurrency() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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

                // Stagger delays so /p1 finishes AFTER /p2 and /p3 to test deterministic sorting.
                let delay_ms = match path {
                    "/p1" => 180,
                    "/p2" => 60,
                    "/p3" => 10,
                    _ => 0,
                };
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }

                let resp = match path {
                    "/" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Seed</title></head><body><main>\
                          <a href=\"/p1\">P1</a>\
                          <a href=\"/p2\">P2</a>\
                          <a href=\"/p3\">P3</a>\
                          <a href=\"/p4\">P4</a>\
                          <a href=\"/p5\">P5</a>\
                          </main></body></html>",
                    ),
                    "/p1" | "/p2" | "/p3" | "/p4" | "/p5" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        format!("<html><head><title>{path}</title></head><body><main><p>Body {path}</p></main></body></html>")
                            .as_bytes(),
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let res = crawl(
        &ctx,
        &format!("{base}/"),
        CrawlOptions {
            max_depth: 1,
            max_pages: 4,
            format: OutputFormat::Markdown,
            extract_mode: ExtractMode::Clean,
            respect_robots: false,
            path_filter: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(res.pages.len(), 4);
    let urls: Vec<&str> = res.pages.iter().map(|p| p.url.as_str()).collect();
    assert_eq!(
        urls,
        vec![
            format!("{base}/"),
            format!("{base}/p1"),
            format!("{base}/p2"),
            format!("{base}/p3"),
        ]
    );
}

#[tokio::test]
async fn robots_cache_coalesces_concurrent_cold_requests() {
    let robots_hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&robots_hits);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let base_srv = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let hits = Arc::clone(&hits_srv);
            let origin = base_srv.clone();
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
                if path == "/robots.txt" {
                    hits.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    let body = format!(
                        "User-agent: *\nDisallow: /secret\nSitemap: {origin}/custom-map.xml\n"
                    );
                    let _ = socket
                        .write_all(&http_response("200 OK", "text/plain", body.as_bytes()))
                        .await;
                } else {
                    let _ = socket
                        .write_all(&http_response("404 Not Found", "text/plain", b"missing"))
                        .await;
                }
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = Arc::new(
        RuntimeContext::new(RuntimeConfig {
            state_dir: state_dir.path().to_path_buf(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            ..Default::default()
        })
        .unwrap(),
    );

    let mut handles = Vec::new();
    for i in 0..8 {
        let ctx_clone = Arc::clone(&ctx);
        let url = if i % 2 == 0 {
            format!("{base}/public/{i}")
        } else {
            format!("{base}/secret/{i}")
        };
        handles.push(tokio::spawn(async move {
            ctx_clone.robots.is_allowed(&ctx_clone.client, &url).await
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    for (i, allowed) in results.into_iter().enumerate() {
        assert_eq!(allowed, i % 2 == 0);
    }

    let sitemaps = ctx
        .robots
        .sitemap_urls(&ctx.client, &format!("{base}/"))
        .await;
    assert_eq!(sitemaps, vec![format!("{base}/custom-map.xml")]);
    assert_eq!(
        robots_hits.load(Ordering::SeqCst),
        1,
        "concurrent cold requests must coalesce into a single /robots.txt fetch"
    );
}

#[tokio::test]
async fn map_resolves_recursive_sitemapindex_guards_and_gzip_sitemaps() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let base_srv = base.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let origin = base_srv.clone();
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

                let resp = match path {
                    "/robots.txt" => http_response(
                        "200 OK",
                        "text/plain",
                        format!(
                            "User-agent: *\nDisallow: /blocked-by-robots\nSitemap: {origin}/gz-sitemap.xml.gz\n"
                        )
                        .as_bytes(),
                    ),
                    // Depth 0: namespaced sitemapindex with a cycle back to itself + depth chain.
                    "/sitemap.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<?xml version="1.0" encoding="UTF-8"?>
                            <sm:sitemapindex xmlns:sm="http://www.sitemaps.org/schemas/sitemap/0.9">
                              <sm:sitemap><sm:loc>{origin}/index-d1.xml</sm:loc></sm:sitemap>
                              <sm:sitemap><sm:loc>{origin}/sitemap.xml</sm:loc></sm:sitemap>
                            </sm:sitemapindex>"#
                        )
                        .as_bytes(),
                    ),
                    // Depth 1: points back to /sitemap.xml (cycle) and forward to depth 2 + leaf urlset.
                    "/index-d1.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<sitemapindex>
                              <sitemap><loc>{origin}/sitemap.xml</loc></sitemap>
                              <sitemap><loc>{origin}/leaf-d2.xml</loc></sitemap>
                              <sitemap><loc>{origin}/index-d2.xml</loc></sitemap>
                            </sitemapindex>"#
                        )
                        .as_bytes(),
                    ),
                    // Depth 2 leaf: CDATA, &amp; entity, cross-origin URL, robots-disallowed URL.
                    "/leaf-d2.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<sm:urlset xmlns:sm="http://www.sitemaps.org/schemas/sitemap/0.9">
                              <sm:url><sm:loc><![CDATA[{origin}/from-cdata?a=1&b=2]]></sm:loc></sm:url>
                              <sm:url><sm:loc>{origin}/from-entity?x=1&amp;y=2</sm:loc></sm:url>
                              <sm:url><sm:loc>{origin}/blocked-by-robots</sm:loc></sm:url>
                              <sm:url><sm:loc>https://other.example.com/offsite</sm:loc></sm:url>
                            </sm:urlset>"#
                        )
                        .as_bytes(),
                    ),
                    // Depth 2 -> Depth 3 -> Depth 4 (depth 4 must NOT be fetched because depth <= 3).
                    "/index-d2.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<sitemapindex><sitemap><loc>{origin}/index-d3.xml</loc></sitemap></sitemapindex>"#
                        )
                        .as_bytes(),
                    ),
                    "/index-d3.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<sitemapindex><sitemap><loc>{origin}/too-deep-d4.xml</loc></sitemap></sitemapindex>"#
                        )
                        .as_bytes(),
                    ),
                    "/too-deep-d4.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(r#"<urlset><url><loc>{origin}/too-deep-page</loc></url></urlset>"#)
                            .as_bytes(),
                    ),
                    // Raw gzip-compressed sitemap discovered from robots.txt Sitemap: directive.
                    "/gz-sitemap.xml.gz" => {
                        let xml = format!(
                            r#"<urlset><url><loc>{origin}/from-gzip</loc></url></urlset>"#
                        );
                        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                        encoder.write_all(xml.as_bytes()).unwrap();
                        let gz_bytes = encoder.finish().unwrap();
                        http_response("200 OK", "application/gzip", &gz_bytes)
                    }
                    "/" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><body><main><p>Home</p></main></body></html>",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let res = map(
        &ctx,
        &format!("{base}/"),
        MapOptions {
            limit: 50,
            use_sitemap: true,
            respect_robots: true,
            path_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(res.urls.contains(&format!("{base}/from-gzip")));
    assert!(res.urls.contains(&format!("{base}/from-cdata?a=1&b=2")));
    assert!(res.urls.contains(&format!("{base}/from-entity?x=1&y=2")));
    assert!(!res.urls.iter().any(|u| u.contains("blocked-by-robots")));
    assert!(!res.urls.iter().any(|u| u.contains("other.example.com")));
    assert!(!res.urls.iter().any(|u| u.contains("too-deep-page")));
    assert_eq!(res.from_sitemap, 3);
}

#[tokio::test]
async fn adversarial_corrupt_gzip_sitemap_is_skipped_without_breaking_valid_sitemap() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let base_srv = base.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let origin = base_srv.clone();
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
                let resp = match path {
                    "/robots.txt" => http_response(
                        "200 OK",
                        "text/plain",
                        format!("User-agent: *\nSitemap: {origin}/corrupt.xml.gz\n").as_bytes(),
                    ),
                    // Valid gzip magic bytes followed by corrupted deflate stream.
                    "/corrupt.xml.gz" => http_response(
                        "200 OK",
                        "application/gzip",
                        b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\xffCORRUPT_DEFLATE_PAYLOAD",
                    ),
                    "/sitemap.xml" => http_response(
                        "200 OK",
                        "application/xml",
                        format!(
                            r#"<urlset><url><loc>{origin}/survived-corrupt-gz</loc></url></urlset>"#
                        )
                        .as_bytes(),
                    ),
                    "/" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><body><main><p>Home</p></main></body></html>",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let res = map(
        &ctx,
        &format!("{base}/"),
        MapOptions {
            limit: 10,
            use_sitemap: true,
            respect_robots: true,
            path_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(res.urls.contains(&format!("{base}/survived-corrupt-gz")));
    assert_eq!(res.from_sitemap, 1);
}

#[tokio::test]
async fn adversarial_fetch_document_decodes_windows1252_over_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let body: &[u8] = b"<!DOCTYPE html><html><head><title>Caf\xe9</title></head><body><main><h1>Caf\xe9</h1><p>Price \x8012 \x97 \x93ok\x94</p></main></body></html>";
            let resp = http_response("200 OK", "text/html; charset=windows-1252", body);
            let _ = socket.write_all(&resp).await;
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();

    let out = fetch_document(&ctx, &format!("{base}/cafe"), &FetchOptions::default())
        .await
        .unwrap();
    assert_eq!(out.title.as_deref(), Some("Café"));
    assert!(out.content.contains("Price €12 — “ok”"));
    assert!(!out.content.contains('\u{FFFD}'));
}

#[cfg(unix)]
#[tokio::test]
async fn cef_challenge_interstitial_fails_fast_without_retry() {
    use std::os::unix::fs::PermissionsExt;

    let state_dir = temp_state_dir();
    let cef_dir = state_dir.path().join("cef");
    std::fs::create_dir_all(&cef_dir).unwrap();
    let renderer_bin = cef_dir.join("pleb-renderer");
    let count_file = state_dir.path().join("invocations.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# PLEB_RENDERER_PROTOCOL=v5-meta
import json, sys

count_path = {count_path:?}
sys.stdout.write(json.dumps({{"ready": True, "protocol": "v5-meta", "error": None}}) + "\n")
sys.stdout.flush()

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        break
    req = json.loads(line)
    with open(count_path, "a") as f:
        f.write(req["url"] + "\n")
    html = b"<html><head><title>Just a moment...</title></head><body><div id='cf-challenge-running'>Checking your browser before accessing</div></body></html>"
    hdr = {{
        "status_code": 403,
        "final_url": req["url"],
        "html_bytes": len(html),
        "error": None,
        "error_kind": None,
    }}
    sys.stdout.write(json.dumps(hdr) + "\n")
    sys.stdout.flush()
    sys.stdout.buffer.write(html)
    sys.stdout.buffer.flush()
"#,
        count_path = count_file
    );

    std::fs::write(&renderer_bin, script).unwrap();
    std::fs::set_permissions(&renderer_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(10),
        cef_timeout: Duration::from_secs(10),
        cef_renderer_executable: Some(renderer_bin),
        ..Default::default()
    })
    .unwrap();

    let err = fetch_document(
        &ctx,
        "https://protected.example/challenge",
        &FetchOptions {
            render: RenderMode::Cef,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, FetchError::Challenge { status: 403, .. }),
        "expected FetchError::Challenge, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2);

    let invocations = std::fs::read_to_string(&count_file).unwrap();
    assert_eq!(
        invocations.lines().count(),
        1,
        "CEF must fail fast on challenge interstitial without retrying"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn auto_mode_fails_fast_on_cronet_challenge_without_escalating_to_cef() {
    use std::os::unix::fs::PermissionsExt;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let body = b"<html><head><title>Just a moment...</title></head><body><script src=\"https://challenges.cloudflare.com/turnstile/v0/api.js\"></script></body></html>";
                let _ = socket
                    .write_all(&http_response("403 Forbidden", "text/html", body))
                    .await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let cef_dir = state_dir.path().join("cef");
    std::fs::create_dir_all(&cef_dir).unwrap();
    let renderer_bin = cef_dir.join("pleb-renderer");
    let count_file = state_dir.path().join("auto_challenge_cef.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# PLEB_RENDERER_PROTOCOL=v5-meta
import json, sys

with open({count_path:?}, "a") as f:
    f.write("spawned\n")
sys.stdout.write(json.dumps({{"ready": True, "protocol": "v5-meta", "error": None}}) + "\n")
sys.stdout.flush()
"#,
        count_path = count_file
    );
    std::fs::write(&renderer_bin, script).unwrap();
    std::fs::set_permissions(&renderer_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        cef_renderer_executable: Some(renderer_bin),
        ..Default::default()
    })
    .unwrap();

    let err = fetch_document(
        &ctx,
        &format!("{base}/cf"),
        &FetchOptions {
            render: RenderMode::Auto,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(err, FetchError::Challenge { status: 403, .. }),
        "expected FetchError::Challenge, got {err:?}"
    );
    assert_eq!(err.exit_code(), 2);
    assert!(
        !count_file.exists(),
        "RenderMode::Auto must fail fast on Cronet challenge without spawning CEF"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn adversarial_cef_transient_empty_render_retries_once_and_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let state_dir = temp_state_dir();
    let cef_dir = state_dir.path().join("cef");
    std::fs::create_dir_all(&cef_dir).unwrap();
    let renderer_bin = cef_dir.join("pleb-renderer");
    let count_file = state_dir.path().join("retry_invocations.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# PLEB_RENDERER_PROTOCOL=v5-meta
import json, sys

count_path = {count_path:?}
attempt = 0
sys.stdout.write(json.dumps({{"ready": True, "protocol": "v5-meta", "error": None}}) + "\n")
sys.stdout.flush()

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        break
    attempt += 1
    req = json.loads(line)
    with open(count_path, "a") as f:
        f.write(str(attempt) + "\n")
    if attempt == 1:
        html = b""
        status = 0
    else:
        html = b"<html><head><title>Recovered</title></head><body><main><h1>Recovered</h1><p>Second CEF attempt succeeded cleanly.</p></main></body></html>"
        status = 200
    hdr = {{
        "status_code": status,
        "final_url": req["url"],
        "html_bytes": len(html),
        "error": None,
        "error_kind": None,
    }}
    sys.stdout.write(json.dumps(hdr) + "\n")
    sys.stdout.flush()
    sys.stdout.buffer.write(html)
    sys.stdout.buffer.flush()
"#,
        count_path = count_file
    );

    std::fs::write(&renderer_bin, script).unwrap();
    std::fs::set_permissions(&renderer_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(10),
        cef_timeout: Duration::from_secs(10),
        cef_renderer_executable: Some(renderer_bin),
        ..Default::default()
    })
    .unwrap();

    let out = fetch_document(
        &ctx,
        "https://transient.example/page",
        &FetchOptions {
            render: RenderMode::Cef,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(out.status_code, 200);
    assert_eq!(out.title.as_deref(), Some("Recovered"));
    assert!(out
        .content
        .contains("Second CEF attempt succeeded cleanly."));

    let invocations = std::fs::read_to_string(&count_file).unwrap();
    assert_eq!(
        invocations.lines().count(),
        2,
        "non-challenge empty CEF render should still retry once"
    );
}

#[tokio::test]
async fn source_negotiation_dot_md_probe_and_bypass_modes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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

                let resp = match path {
                    "/docs/guide" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>HTML Guide</title></head><body><main><p>HTML body</p></main></body></html>",
                    ),
                    "/docs/guide.md" => http_response(
                        "200 OK",
                        "text/markdown; charset=utf-8",
                        b"# Markdown Guide\n\nClean markdown mirror from dot-md probe.",
                    ),
                    "/docs/soft404" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Real Page</title></head><body><main><p>Real HTML content</p></main></body></html>",
                    ),
                    "/docs/soft404.md" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><body>Soft 404 HTML page</body></html>",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    // Verify public forge raw-source & RFC 8288 Link header resolvers.
    let gh_url =
        url::Url::parse("https://github.com/tokio-rs/tokio/blob/master/tokio/src/lib.rs").unwrap();
    let cand = resolve_raw_source_candidate(&gh_url).unwrap();
    assert_eq!(
        cand.raw_url.as_str(),
        "https://raw.githubusercontent.com/tokio-rs/tokio/master/tokio/src/lib.rs"
    );
    assert_eq!(
        cand.kind,
        RawSourceKind::Code {
            language: Some("rust")
        }
    );
    let link_base = url::Url::parse("https://docs.example.com/guide").unwrap();
    let parsed_link = parse_markdown_link_header(
        r#"</guide.md>; rel="alternate"; type="text/markdown""#,
        &link_base,
    )
    .unwrap();
    assert_eq!(parsed_link.as_str(), "https://docs.example.com/guide.md");

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        ..Default::default()
    })
    .unwrap();

    // 1. SourceMode::Negotiate probes <path>.md when HTML lacks an alternate link.
    let negotiate_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Negotiate,
        ..Default::default()
    };
    let res = fetch_document(&ctx, &format!("{base}/docs/guide"), &negotiate_opts)
        .await
        .unwrap();
    assert_eq!(res.strategy, "markdown_mirror");
    assert!(res.final_url.ends_with("/docs/guide"));
    assert!(res
        .content
        .contains("Clean markdown mirror from dot-md probe."));

    // 2. Soft-404 HTML returned for <path>.md is rejected; original HTML is kept.
    let soft404 = fetch_document(&ctx, &format!("{base}/docs/soft404"), &negotiate_opts)
        .await
        .unwrap();
    assert_ne!(soft404.strategy, "markdown_mirror");
    assert!(soft404.content.contains("Real HTML content"));

    // 3. SourceMode::None bypasses structured source resolution.
    let none_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::None,
        ..Default::default()
    };
    let none_res = fetch_document(&ctx, &format!("{base}/docs/guide"), &none_opts)
        .await
        .unwrap();
    assert_ne!(none_res.strategy, "markdown_mirror");
    assert!(none_res.content.contains("HTML body"));

    // 4. OutputFormat::Html bypasses structured source resolution even with SourceMode::Negotiate.
    let html_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Negotiate,
        extract: ExtractOptions {
            format: OutputFormat::Html,
            ..Default::default()
        },
        ..Default::default()
    };
    let html_res = fetch_document(&ctx, &format!("{base}/docs/guide"), &html_opts)
        .await
        .unwrap();
    assert_ne!(html_res.strategy, "markdown_mirror");
    assert!(html_res.content.contains("<p>HTML body</p>"));
}

#[tokio::test]
async fn source_negotiation_follows_http_link_html_alternate_and_accept_markdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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
                let accepts_markdown = req.lines().any(|line| {
                    line.split_once(':').is_some_and(|(k, v)| {
                        k.trim().eq_ignore_ascii_case("accept")
                            && v.to_ascii_lowercase().contains("text/markdown")
                    })
                });

                let resp: Vec<u8> = match path {
                    "/link-header" => {
                        let body = b"<html><head><title>HTML Chrome</title></head><body><main><p>Heavy HTML page.</p></main></body></html>";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLink: </mirrors/link-header.md>; rel=\"alternate\"; type=\"text/markdown\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let mut out = hdr.into_bytes();
                        out.extend_from_slice(body);
                        out
                    }
                    "/mirrors/link-header.md" => http_response(
                        "200 OK",
                        "text/markdown; charset=utf-8",
                        b"---\ntitle: Link Mirror Title\n---\n\nFrom HTTP Link header with [Sibling](./sibling.md).\n",
                    ),
                    "/html-alternate" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>HTML Alternate Page</title><link rel=\"alternate\" type=\"text/markdown\" href=\"/mirrors/html-alt.md\"></head><body><main><p>HTML fallback body.</p></main></body></html>",
                    ),
                    "/mirrors/html-alt.md" => http_response(
                        "200 OK",
                        "text/markdown; charset=utf-8",
                        b"---\ntitle: HTML Alt Mirror\n---\n\nFrom HTML head link alternate.\n",
                    ),
                    "/binary-mirror" => http_response(
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>Binary Mirror Fallback</title><link rel=\"alternate\" type=\"text/markdown\" href=\"/mirrors/fake-png.md\"></head><body><main><p>Safe HTML fallback.</p></main></body></html>",
                    ),
                    "/mirrors/fake-png.md" => http_response(
                        "200 OK",
                        "text/markdown; charset=utf-8",
                        b"\x89PNG\r\n\x1a\n\x00\x00binary-disguised-as-markdown",
                    ),
                    "/negotiable" => {
                        if accepts_markdown {
                            http_response(
                                "200 OK",
                                "text/markdown; charset=utf-8",
                                b"# Negotiated Markdown\n\nServed directly via Accept header.\n",
                            )
                        } else {
                            http_response(
                                "200 OK",
                                "text/html; charset=utf-8",
                                b"<html><head><title>Default Chrome HTML</title></head><body><main><p>Standard HTML response.</p></main></body></html>",
                            )
                        }
                    }
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        ..Default::default()
    })
    .unwrap();

    let auto_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Auto,
        ..Default::default()
    };

    // 1. HTTP Link header Markdown mirror follow in SourceMode::Auto.
    let link_res = fetch_document(&ctx, &format!("{base}/link-header"), &auto_opts)
        .await
        .unwrap();
    assert_eq!(link_res.strategy, "markdown_mirror");
    assert!(link_res.final_url.ends_with("/link-header"));
    assert_eq!(link_res.title.as_deref(), Some("Link Mirror Title"));
    assert!(link_res.content.contains("From HTTP Link header"));
    assert!(
        link_res
            .content
            .contains(&format!("[Sibling]({base}/mirrors/sibling.md)")),
        "relative link in mirror should resolve against mirror URL, got: {}",
        link_res.content
    );
    assert!(!link_res.content.contains("Heavy HTML page."));

    // 2. HTML <link rel="alternate" type="text/markdown"> mirror follow in SourceMode::Auto.
    let alt_res = fetch_document(&ctx, &format!("{base}/html-alternate"), &auto_opts)
        .await
        .unwrap();
    assert_eq!(alt_res.strategy, "markdown_mirror");
    assert!(alt_res.final_url.ends_with("/html-alternate"));
    assert_eq!(alt_res.title.as_deref(), Some("HTML Alt Mirror"));
    assert!(alt_res.content.contains("From HTML head link alternate."));
    assert!(!alt_res.content.contains("HTML fallback body."));

    // 3. SourceMode::Auto preserves default Chrome Accept header (does NOT negotiate Markdown).
    let auto_neg = fetch_document(&ctx, &format!("{base}/negotiable"), &auto_opts)
        .await
        .unwrap();
    assert_ne!(auto_neg.strategy, "markdown_negotiated");
    assert!(auto_neg.content.contains("Standard HTML response."));

    // 4. SourceMode::Negotiate sends Accept: text/markdown and returns strategy == "markdown_negotiated".
    let negotiate_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Negotiate,
        ..Default::default()
    };
    let neg_res = fetch_document(&ctx, &format!("{base}/negotiable"), &negotiate_opts)
        .await
        .unwrap();
    assert_eq!(neg_res.strategy, "markdown_negotiated");
    assert_eq!(neg_res.title.as_deref(), Some("Negotiated Markdown"));
    assert!(neg_res
        .content
        .contains("Served directly via Accept header."));

    // 5. SourceMode::None and OutputFormat::Html bypass HTTP Link and HTML <link> mirrors.
    let none_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::None,
        ..Default::default()
    };
    let none_link = fetch_document(&ctx, &format!("{base}/link-header"), &none_opts)
        .await
        .unwrap();
    assert_ne!(none_link.strategy, "markdown_mirror");
    assert!(none_link.content.contains("Heavy HTML page."));

    let html_opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Auto,
        extract: ExtractOptions {
            format: OutputFormat::Html,
            ..Default::default()
        },
        ..Default::default()
    };
    let html_alt = fetch_document(&ctx, &format!("{base}/html-alternate"), &html_opts)
        .await
        .unwrap();
    assert_ne!(html_alt.strategy, "markdown_mirror");
    assert!(html_alt.content.contains("HTML fallback body."));

    // 6. Binary PNG/NUL payload disguised as text/markdown mirror is rejected and falls back to HTML.
    let bin_fallback = fetch_document(&ctx, &format!("{base}/binary-mirror"), &auto_opts)
        .await
        .unwrap();
    assert_ne!(bin_fallback.strategy, "markdown_mirror");
    assert!(bin_fallback.content.contains("Safe HTML fallback."));
}

#[tokio::test]
async fn adversarial_source_mirror_404_binary_fallback_ssrf_and_forge_binary_skip() {
    // 1. Forge raw-source resolver skips binary extensions & non-gist paths, resolves GitLab nested groups & Dockerfile.
    for skipped in [
        "https://github.com/org/repo/blob/main/assets/logo.png",
        "https://github.com/org/repo/blob/main/dist/archive.zip",
        "https://gitlab.com/group/sub/proj/-/blob/main/paper.pdf",
        "https://gist.github.com/user/abcd1234/revisions",
        "https://gist.github.com/user/short",
    ] {
        let u = url::Url::parse(skipped).unwrap();
        assert!(
            resolve_raw_source_candidate(&u).is_none(),
            "expected {skipped} to be skipped"
        );
    }
    let gl = url::Url::parse("https://gitlab.com/a/b/proj/-/blob/main/Dockerfile").unwrap();
    let gl_cand = resolve_raw_source_candidate(&gl).unwrap();
    assert_eq!(
        gl_cand.raw_url.as_str(),
        "https://gitlab.com/a/b/proj/-/raw/main/Dockerfile"
    );
    assert_eq!(
        gl_cand.kind,
        RawSourceKind::Code {
            language: Some("dockerfile")
        }
    );

    // 2. RFC 8288 Link header SSRF / cross-origin / HTTPS downgrade rejection.
    let public_base = url::Url::parse("https://docs.example.com/api/ref").unwrap();
    for bad_link in [
        r#"<http://127.0.0.1/secret.md>; rel="alternate"; type="text/markdown""#,
        r#"<https://169.254.169.254/latest/meta-data/>; rel="alternate"; type="text/markdown""#,
        r#"<https://[::1]/secret.md>; rel="alternate"; type="text/markdown""#,
        r#"<http://docs.example.com/api/ref.md>; rel="alternate"; type="text/markdown""#,
        r#"<https://evil.example.org/api/ref.md>; rel="alternate"; type="text/markdown""#,
    ] {
        assert!(
            parse_markdown_link_header(bad_link, &public_base).is_none(),
            "expected Link header {bad_link} to be rejected"
        );
    }

    // 3. End-to-end loopback: 404 or binary PNG mirror transparently falls back to original HTML.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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
                let resp: Vec<u8> = match path {
                    "/broken-mirror" => {
                        let body = b"<html><head><title>Fallback Page</title></head><body><main><p>Original HTML survives 404 mirror.</p></main></body></html>";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLink: </does-not-exist.md>; rel=\"alternate\"; type=\"text/markdown\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let mut out = hdr.into_bytes();
                        out.extend_from_slice(body);
                        out
                    }
                    "/binary-mirror" => {
                        let body = b"<html><head><title>Binary Mirror Fallback</title></head><body><main><p>Original HTML survives binary mirror.</p></main></body></html>";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLink: </png-as-md.md>; rel=\"alternate\"; type=\"text/markdown\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let mut out = hdr.into_bytes();
                        out.extend_from_slice(body);
                        out
                    }
                    "/png-as-md.md" => http_response(
                        "200 OK",
                        "text/markdown; charset=utf-8",
                        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"not found"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        ..Default::default()
    })
    .unwrap();
    let opts = FetchOptions {
        render: RenderMode::Cronet,
        source: SourceMode::Auto,
        ..Default::default()
    };

    let res404 = fetch_document(&ctx, &format!("{base}/broken-mirror"), &opts)
        .await
        .unwrap();
    assert_ne!(res404.strategy, "markdown_mirror");
    assert!(res404
        .content
        .contains("Original HTML survives 404 mirror."));

    let res_bin = fetch_document(&ctx, &format!("{base}/binary-mirror"), &opts)
        .await
        .unwrap();
    assert_ne!(res_bin.strategy, "markdown_mirror");
    assert!(res_bin
        .content
        .contains("Original HTML survives binary mirror."));
}

#[tokio::test]
async fn fetch_staged_asset_and_session_clear_artifact_lifecycle() {
    use pleb::artifacts::{evict_artifacts_over_budget, stage_artifact};
    use pleb::context::clear_state_dir;
    use pleb::fetch::fetch_staged_asset;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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
                let resp = match path {
                    "/chart.png" => http_response(
                        "200 OK",
                        "image/png",
                        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
                    ),
                    "/soft404.png" => http_response(
                        "200 OK",
                        "image/png",
                        b"<html><body>Not found</body></html>",
                    ),
                    _ => http_response("404 Not Found", "text/plain", b"missing"),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let state_dir = temp_state_dir();
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        ..Default::default()
    })
    .unwrap();

    // 1. Valid PNG stages cleanly.
    let staged = fetch_staged_asset(
        &ctx,
        &format!("{base}/chart.png"),
        Some(&format!("{base}/article")),
        false,
    )
    .await
    .unwrap();
    assert_eq!(staged.status_code, 200);
    assert_eq!(staged.byte_count, 16);
    assert_eq!(staged.content_type.as_deref(), Some("image/png"));
    let staged_path = PathBuf::from(&staged.staged_path);
    assert!(staged_path.exists());
    assert!(staged_path.starts_with(state_dir.path().join("artifacts")));

    // 2. Disguised HTML-as-PNG soft-404 fails cleanly.
    let err = fetch_staged_asset(&ctx, &format!("{base}/soft404.png"), None, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("disguised HTML"));

    // 3. 404 returns HttpError (exit 3).
    let err_404 = fetch_staged_asset(&ctx, &format!("{base}/nope.png"), None, false)
        .await
        .unwrap_err();
    assert!(matches!(err_404, FetchError::HttpError { status: 404, .. }));

    // 4. LRU eviction keeps protected file and removes older ones.
    let u2 = url::Url::parse(&format!("{base}/second.png")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let staged2 = stage_artifact(
        state_dir.path(),
        &u2,
        Some("image/png"),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR2",
        None,
    )
    .unwrap();
    let evicted =
        evict_artifacts_over_budget(&state_dir.path().join("artifacts"), 20, Some(&staged2.path))
            .unwrap();
    assert_eq!(evicted, 1);
    assert!(!staged_path.exists());
    assert!(staged2.path.exists());

    // 5. clear_state_dir removes artifacts/.
    clear_state_dir(state_dir.path()).unwrap();
    assert!(!state_dir.path().join("artifacts").exists());
}

#[test]
fn adversarial_artifact_staging_traversal_bom_html_magic_mismatch_and_perms() {
    use pleb::artifacts::stage_artifact;
    use pleb::extract::ResourceKind;

    let state_dir = temp_state_dir();
    let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

    // 1. Path traversal + oversized stem stays confined inside `<state_dir>/artifacts/` with <= 40-char stem and 0o600 perms.
    let nasty_url = url::Url::parse(
        "https://cdn.example.com/..%2F..%2Fetc%2Fpasswd_0123456789_0123456789_0123456789_0123456789.png",
    )
    .unwrap();
    let staged = stage_artifact(
        state_dir.path(),
        &nasty_url,
        Some("image/png"),
        png,
        Some(ResourceKind::Diagram),
    )
    .unwrap();
    assert_eq!(
        staged.path.parent().unwrap(),
        state_dir.path().join("artifacts")
    );
    let fname = staged.path.file_name().unwrap().to_str().unwrap();
    assert!(!fname.contains(".."));
    assert!(!fname.contains('/'));
    let (hash_part, rest) = fname.split_once('-').unwrap();
    assert_eq!(hash_part.len(), 16);
    let stem_part = rest.strip_suffix(".png").unwrap();
    assert!(stem_part.len() <= 40, "stem exceeded 40 chars: {stem_part}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&staged.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "staged artifact must be 0o600, got {mode:o}");
    }

    // 2. UTF-8 BOM-prefixed uppercase <HTML> disguised as application/pdf is rejected.
    let pdf_url = url::Url::parse("https://docs.example.com/spec.pdf").unwrap();
    let bom_html = b"\xEF\xBB\xBF  \n<HTML><body>Access Denied</body></HTML>";
    assert!(
        stage_artifact(
            state_dir.path(),
            &pdf_url,
            Some("application/pdf"),
            bom_html,
            Some(ResourceKind::Document),
        )
        .is_err(),
        "BOM-prefixed HTML disguised as PDF must be rejected"
    );

    // 3. JPEG magic bytes claiming image/png must be rejected by magic validation.
    let jpeg = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    assert!(
        stage_artifact(
            state_dir.path(),
            &nasty_url,
            Some("image/png"),
            jpeg,
            Some(ResourceKind::Image),
        )
        .is_err(),
        "JPEG payload claiming image/png must be rejected"
    );

    // 4. application/octet-stream with %PDF- magic resolves to .pdf and application/pdf.
    let pdf_bytes = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";
    let octet_url = url::Url::parse("https://cdn.example.com/download?id=42").unwrap();
    let pdf_staged = stage_artifact(
        state_dir.path(),
        &octet_url,
        Some("application/octet-stream"),
        pdf_bytes,
        None,
    )
    .unwrap();
    assert_eq!(
        pdf_staged.path.extension().and_then(|e| e.to_str()),
        Some("pdf")
    );
    assert_eq!(pdf_staged.content_type.as_deref(), Some("application/pdf"));
}

async fn spawn_fetch_mock_cdp_server(
    html_body: &'static str,
    status_code: u16,
    imperative_tool_name: Option<&'static str>,
) -> String {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let Ok(val) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let id = val["id"].as_i64().unwrap_or(0);
                    let method = val["method"].as_str().unwrap_or("");
                    let sid = val["sessionId"].as_str().map(|s| s.to_string());

                    let send_frame =
                        |id_opt: Option<i64>, ev_method: Option<&str>, result: Value| {
                            let mut obj = serde_json::Map::new();
                            if let Some(i) = id_opt {
                                obj.insert("id".into(), json!(i));
                                obj.insert("result".into(), result);
                            } else if let Some(m) = ev_method {
                                obj.insert("method".into(), json!(m));
                                obj.insert("params".into(), result);
                            }
                            if let Some(ref s) = sid {
                                obj.insert("sessionId".into(), json!(s));
                            }
                            Message::Text(Value::Object(obj).to_string().into())
                        };

                    let replies = match method {
                        "Target.createTarget" => {
                            vec![send_frame(Some(id), None, json!({ "targetId": "tgt-f" }))]
                        }
                        "Target.attachToTarget" => {
                            vec![send_frame(Some(id), None, json!({ "sessionId": "sess-f" }))]
                        }
                        "Target.closeTarget" | "Page.enable" | "Network.enable"
                        | "Fetch.enable" | "WebMCP.enable" => {
                            vec![send_frame(Some(id), None, json!({}))]
                        }
                        "Page.getFrameTree" => vec![send_frame(
                            Some(id),
                            None,
                            json!({ "frameTree": { "frame": { "id": "mf-f" } } }),
                        )],
                        "Page.navigate" => {
                            let nav_url = val["params"]["url"]
                                .as_str()
                                .unwrap_or("http://127.0.0.1/")
                                .to_string();
                            let mut out = vec![send_frame(
                                None,
                                Some("Network.responseReceived"),
                                json!({
                                    "requestId": "req-f",
                                    "frameId": "mf-f",
                                    "type": "Document",
                                    "response": {
                                        "url": nav_url,
                                        "status": status_code,
                                        "headers": { "Content-Type": "text/html; charset=utf-8" }
                                    }
                                }),
                            )];
                            if let Some(tool_name) = imperative_tool_name {
                                out.push(send_frame(
                                    None,
                                    Some("WebMCP.toolsAdded"),
                                    json!({
                                        "tools": [{
                                            "name": tool_name,
                                            "description": "Search catalog",
                                            "frameId": "mf-f",
                                            "inputSchema": { "type": "object" }
                                        }]
                                    }),
                                ));
                            }
                            out.push(send_frame(Some(id), None, json!({})));
                            out.push(send_frame(None, Some("Page.loadEventFired"), json!({})));
                            out
                        }
                        "DOM.getDocument" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({ "root": { "nodeId": 3 } }),
                            )]
                        }
                        "DOM.getOuterHTML" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({ "outerHTML": html_body }),
                            )]
                        }
                        _ => {
                            let mut obj = json!({
                                "id": id,
                                "error": { "code": -32601, "message": "not found" }
                            });
                            if let Some(ref s) = sid {
                                obj["sessionId"] = json!(s);
                            }
                            vec![Message::Text(obj.to_string().into())]
                        }
                    };
                    for msg in replies {
                        if ws.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    format!("ws://127.0.0.1:{}/devtools/browser", addr.port())
}

async fn spawn_escalation_origin_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
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
                let resp = match path {
                    "/blocked-page" => http_response(
                        "403 Forbidden",
                        "text/html",
                        b"<html><body>Access Denied</body></html>",
                    ),
                    "/js-shell" => http_response(
                        "200 OK",
                        "text/html",
                        b"<html><head><title>App</title></head><body><div id=\"root\"></div><script>window.__APP__=1</script><noscript>Please enable JavaScript</noscript></body></html>",
                    ),
                    "/challenge" => http_response(
                        "403 Forbidden",
                        "text/html",
                        b"<html><head><title>Just a moment...</title></head><body>Checking your browser before accessing... /cdn-cgi/challenge-platform/</body></html>",
                    ),
                    _ => http_response(
                        "200 OK",
                        "text/html",
                        b"<html><head><title>Origin</title></head><body><main>Plain HTML</main></body></html>",
                    ),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    format!("http://127.0.0.1:{}", addr.port())
}

#[tokio::test]
async fn fetch_cdp_render_escalation_challenge_proxy_and_batch_tools() {
    let state_dir = temp_state_dir();
    let origin = spawn_escalation_origin_server().await;
    let cdp_ws = spawn_fetch_mock_cdp_server(
        "<html><head><title>CDP Rendered</title></head><body><main><h1>CDP Content</h1><p>Hydrated article body with substantial text.</p></main></body></html>",
        200,
        Some("query_catalog"),
    )
    .await;

    let ctx = Arc::new(
        RuntimeContext::new(RuntimeConfig {
            state_dir: state_dir.path().to_path_buf(),
            cdp_url: Some(cdp_ws.clone()),
            cdp_timeout: Duration::from_secs(5),
            no_web_bot_auth: true,
            ..Default::default()
        })
        .unwrap(),
    );

    // 1. Direct RenderMode::Cdp returns CDP strategy, populated imperative tools, and records "cdp".
    let direct = fetch_document(
        &ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cdp,
            tools: ToolPolicy::Manifest,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(direct.strategy, "cdp");
    assert!(!direct.escalated);
    assert!(direct.content.contains("Hydrated article body"));
    assert_eq!(direct.tools.len(), 1);
    assert_eq!(direct.tools[0].name, "query_catalog");
    let manifest_md = pleb::extract::webmcp::format_webmcp_manifest_markdown(&direct.tools);
    if !manifest_md.is_empty() {
        assert!(direct.content.contains("query_catalog"));
    }

    // Cached "cdp" in ctx routes Auto directly to PrimaryTransport::Cdp without escalation.
    let cached_hit = fetch_document(
        &ctx,
        &format!("{origin}/blocked-page"),
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(cached_hit.strategy, "cdp");
    assert!(!cached_hit.escalated);

    // 2. Cold RenderMode::Auto escalates on 403 Blocked and 200 JavaScriptShell when CDP is available.
    let make_cold_ctx = |dir: &Path| {
        RuntimeContext::new(RuntimeConfig {
            state_dir: dir.to_path_buf(),
            cdp_url: Some(cdp_ws.clone()),
            cdp_timeout: Duration::from_secs(5),
            no_web_bot_auth: true,
            ..Default::default()
        })
        .unwrap()
    };

    let blocked_dir = temp_state_dir();
    let blocked_ctx = make_cold_ctx(blocked_dir.path());
    let blocked_esc = fetch_document(
        &blocked_ctx,
        &format!("{origin}/blocked-page"),
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(blocked_esc.strategy, "cdp");
    assert!(blocked_esc.escalated);

    let js_dir = temp_state_dir();
    let js_ctx = make_cold_ctx(js_dir.path());
    let js_esc = fetch_document(
        &js_ctx,
        &format!("{origin}/js-shell"),
        &FetchOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(js_esc.strategy, "cdp");
    assert!(js_esc.escalated);

    // 3. Challenge fast-fail exits with FetchError::Challenge without bypassing via CDP.
    let chal_dir = temp_state_dir();
    let chal_ctx = make_cold_ctx(chal_dir.path());
    let challenge_err = fetch_document(
        &chal_ctx,
        &format!("{origin}/challenge"),
        &FetchOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(challenge_err, FetchError::Challenge { .. }));
    assert_eq!(challenge_err.exit_code(), 2);

    // 4. --format axtree pre-flight validation errors.
    for (bad_render, bad_cdp) in [
        (RenderMode::Cronet, Some(cdp_ws.clone())),
        (RenderMode::Cef, Some(cdp_ws.clone())),
        (RenderMode::Auto, None),
    ] {
        let state_sub = temp_state_dir();
        let sub_ctx = RuntimeContext::new(RuntimeConfig {
            state_dir: state_sub.path().to_path_buf(),
            cdp_url: bad_cdp,
            no_web_bot_auth: true,
            ..Default::default()
        })
        .unwrap();
        let err = fetch_document(
            &sub_ctx,
            &format!("{origin}/plain"),
            &FetchOptions {
                render: bad_render,
                extract: ExtractOptions {
                    format: OutputFormat::Axtree,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, FetchError::CdpUnavailable(_)));
    }

    // 5. Proxy blocks CDP explicitly and suppresses CDP in Auto.
    let proxy_state = temp_state_dir();
    let proxy_ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: proxy_state.path().to_path_buf(),
        cdp_url: Some(cdp_ws.clone()),
        proxy: Some("http://127.0.0.1:18080".to_string()),
        no_web_bot_auth: true,
        ..Default::default()
    })
    .unwrap();
    let proxy_err = fetch_document(
        &proxy_ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cdp,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(proxy_err, FetchError::CdpUnavailable(_)));

    // 6. Batch fetch propagates tools to BatchItemSuccess.
    let batch = fetch_batch(
        Arc::clone(&ctx),
        vec![format!("{origin}/plain")],
        BatchFetchOptions {
            fetch: FetchOptions {
                render: RenderMode::Cdp,
                ..Default::default()
            },
            concurrency: 1,
            metadata_only: true,
        },
        |_, _| {},
    )
    .await;
    assert_eq!(batch.len(), 1);
    match &batch[0].1 {
        BatchItemResult::Success(item) => {
            assert_eq!(item.strategy, "cdp");
            assert_eq!(item.tools.len(), 1);
            assert_eq!(item.tools[0].name, "query_catalog");
            assert!(item.content.is_none());
        }
        BatchItemResult::Failure(f) => panic!("unexpected batch failure: {f:?}"),
    }
}

#[tokio::test]
async fn adversarial_fetch_cdp_webmcp_merge_axtree_manifest_timeout_cache_and_unsigned_wba() {
    use futures_util::{SinkExt, StreamExt};
    use pleb::auth::{set_credential, CredentialProvider, CredentialStorage};
    use pleb::extract::WebMcpToolSource;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::Message;

    const ADVERSARIAL_CDP_HTML: &str = concat!(
        "<html><head><title>Dining</title></head><body>",
        "<form toolname=\"reserve_table\" tooldescription=\"Reserve dinner table\" ",
        "action=\"/api/reserve\" method=\"post\">",
        "<label for=\"guests\">Party size</label>",
        "<input id=\"guests\" name=\"guests\" type=\"number\" min=\"1\" max=\"8\" required>",
        "<input type=\"password\" name=\"secret_pin\">",
        "</form>",
        "<form hidden toolname=\"hidden_admin\" tooldescription=\"Secret admin form\">",
        "<input name=\"x\">",
        "</form>",
        "<main><h1>Dining Guide</h1>",
        "<p>Reserve safely without form chrome. Our kitchen serves seasonal tasting menus ",
        "every evening from five until ten, with vegetarian and gluten-free pairings ",
        "prepared fresh to order by our culinary team in the main dining room.</p>",
        "<p>Walk-in seating remains available at the chef counter throughout dinner service, ",
        "and private dining tables for celebrations can accommodate larger parties upon ",
        "advance request with forty-eight hours notice before your preferred date.</p>",
        "</main>",
        "</body></html>"
    );

    type LoggedHttpRequests = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

    let http_requests: LoggedHttpRequests = Arc::new(Mutex::new(Vec::new()));
    let http_log = Arc::clone(&http_requests);
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = http_listener.accept().await else {
                break;
            };
            let log = Arc::clone(&http_log);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let mut lines = raw.lines();
                let req_line = lines.next().unwrap_or("");
                let path = req_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                log.lock().unwrap().push((path.clone(), headers));
                let resp = match path.as_str() {
                    "/robots.txt" => {
                        http_response("200 OK", "text/plain", b"User-agent: *\nAllow: /\n")
                    }
                    "/blocked" => http_response(
                        "403 Forbidden",
                        "text/html",
                        b"<html><body>Blocked Origin</body></html>",
                    ),
                    _ => http_response(
                        "200 OK",
                        "text/html",
                        b"<html><head><title>Plain</title></head><body><main>Plain Origin</main></body></html>",
                    ),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let ws_headers: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
    let nav_delay_ms = Arc::new(AtomicU64::new(0));
    let ws_headers_bg = Arc::clone(&ws_headers);
    let nav_delay_bg = Arc::clone(&nav_delay_ms);
    let cdp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cdp_port = cdp_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = cdp_listener.accept().await else {
                break;
            };
            let hdr_log = Arc::clone(&ws_headers_bg);
            let delay_ctl = Arc::clone(&nav_delay_bg);
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let on_headers = |req: &Request, resp: Response| {
                    let mut map = HashMap::new();
                    for (name, val) in req.headers() {
                        if let Ok(s) = val.to_str() {
                            map.insert(name.as_str().to_ascii_lowercase(), s.to_string());
                        }
                    }
                    hdr_log.lock().unwrap().push(map);
                    Ok(resp)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, on_headers).await
                else {
                    return;
                };
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let Ok(val) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let id = val["id"].as_i64().unwrap_or(0);
                    let method = val["method"].as_str().unwrap_or("");
                    let sid = val["sessionId"].as_str().map(|s| s.to_string());
                    let send_frame =
                        |id_opt: Option<i64>, ev_method: Option<&str>, result: Value| {
                            let mut obj = serde_json::Map::new();
                            if let Some(i) = id_opt {
                                obj.insert("id".into(), json!(i));
                                obj.insert("result".into(), result);
                            } else if let Some(m) = ev_method {
                                obj.insert("method".into(), json!(m));
                                obj.insert("params".into(), result);
                            }
                            if let Some(ref s) = sid {
                                obj.insert("sessionId".into(), json!(s));
                            }
                            Message::Text(Value::Object(obj).to_string().into())
                        };

                    let replies = match method {
                        "Target.createTarget" => {
                            vec![send_frame(Some(id), None, json!({ "targetId": "tgt-adv" }))]
                        }
                        "Target.attachToTarget" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({ "sessionId": "sess-adv" }),
                            )]
                        }
                        "Target.closeTarget" | "Page.enable" | "Network.enable"
                        | "Fetch.enable" | "WebMCP.enable" => {
                            vec![send_frame(Some(id), None, json!({}))]
                        }
                        "Page.getFrameTree" => vec![send_frame(
                            Some(id),
                            None,
                            json!({ "frameTree": { "frame": { "id": "mf-adv" } } }),
                        )],
                        "Page.navigate" => {
                            let ms = delay_ctl.load(Ordering::Relaxed);
                            if ms > 0 {
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                            }
                            let nav_url = val["params"]["url"]
                                .as_str()
                                .unwrap_or("http://127.0.0.1/")
                                .to_string();
                            vec![
                                send_frame(
                                    None,
                                    Some("Network.responseReceived"),
                                    json!({
                                        "requestId": "req-adv",
                                        "frameId": "mf-adv",
                                        "type": "Document",
                                        "response": {
                                            "url": nav_url,
                                            "status": 200,
                                            "headers": { "Content-Type": "text/html; charset=utf-8" }
                                        }
                                    }),
                                ),
                                send_frame(
                                    None,
                                    Some("WebMCP.toolsAdded"),
                                    json!({
                                        "tools": [
                                            {
                                                "name": "reserve_table",
                                                "description": "STALE_CDP_CLOBBER",
                                                "frameId": "mf-adv",
                                                "backendNodeId": 77
                                            },
                                            {
                                                "name": "hidden_admin",
                                                "description": "REINJECTED_HIDDEN",
                                                "frameId": "mf-adv",
                                                "backendNodeId": 88
                                            },
                                            {
                                                "name": "foreign_tool",
                                                "description": "SUBFRAME",
                                                "frameId": "sub-frame"
                                            },
                                            {
                                                "name": "ephemeral_tool",
                                                "description": "EPHEMERAL",
                                                "frameId": "mf-adv"
                                            },
                                            {
                                                "name": "live_slots",
                                                "description": "Check live ```slots\x1b[31m",
                                                "frameId": "mf-adv",
                                                "inputSchema": {
                                                    "type": "object",
                                                    "properties": {
                                                        "note": {
                                                            "type": "string",
                                                            "description": "Use ``` fence"
                                                        }
                                                    }
                                                }
                                            }
                                        ]
                                    }),
                                ),
                                send_frame(
                                    None,
                                    Some("WebMCP.toolsRemoved"),
                                    json!({
                                        "tools": [
                                            { "name": "ephemeral_tool", "frameId": "mf-adv" },
                                            { "name": "live_slots", "frameId": "sub-frame" }
                                        ]
                                    }),
                                ),
                                send_frame(Some(id), None, json!({})),
                                send_frame(None, Some("Page.loadEventFired"), json!({})),
                            ]
                        }
                        "DOM.getDocument" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({ "root": { "nodeId": 9 } }),
                            )]
                        }
                        "DOM.getOuterHTML" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({ "outerHTML": ADVERSARIAL_CDP_HTML }),
                            )]
                        }
                        "Accessibility.getFullAXTree" => {
                            vec![send_frame(
                                Some(id),
                                None,
                                json!({
                                    "nodes": [
                                        {
                                            "nodeId": "1",
                                            "ignored": false,
                                            "role": { "value": "RootWebArea" },
                                            "name": { "value": "Dining AX" },
                                            "childIds": ["2", "3"]
                                        },
                                        {
                                            "nodeId": "2",
                                            "ignored": false,
                                            "role": { "value": "generic" },
                                            "childIds": ["4"]
                                        },
                                        {
                                            "nodeId": "3",
                                            "ignored": true,
                                            "role": { "value": "button" },
                                            "name": { "value": "HiddenAX" },
                                            "childIds": []
                                        },
                                        {
                                            "nodeId": "4",
                                            "ignored": false,
                                            "role": { "value": "button" },
                                            "name": { "value": "Book\x1b[31m\nNow" },
                                            "childIds": []
                                        }
                                    ]
                                }),
                            )]
                        }
                        _ => {
                            let mut obj = json!({
                                "id": id,
                                "error": { "code": -32601, "message": "not found" }
                            });
                            if let Some(ref s) = sid {
                                obj["sessionId"] = json!(s);
                            }
                            vec![Message::Text(obj.to_string().into())]
                        }
                    };
                    for msg in replies {
                        if ws.send(msg).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    let cdp_ws = format!("ws://127.0.0.1:{cdp_port}/devtools/browser");
    let origin = format!("http://127.0.0.1:{http_port}");
    let state_dir = temp_state_dir();

    // Configure WBA credentials (file-backed, no global env mutation) and keep signing enabled.
    set_credential(
        state_dir.path(),
        CredentialProvider::WebBotAuthJwk,
        r#"{"crv":"Ed25519","d":"nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A","kty":"OKP","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#,
        CredentialStorage::File,
    )
    .unwrap();
    set_credential(
        state_dir.path(),
        CredentialProvider::WebBotAuthAgent,
        "https://agent.example.com",
        CredentialStorage::File,
    )
    .unwrap();

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.path().to_path_buf(),
        cdp_url: Some(cdp_ws.clone()),
        cdp_timeout: Duration::from_secs(5),
        no_web_bot_auth: false,
        ..Default::default()
    })
    .unwrap();

    // A. Declarative vs imperative WebMCP merge + manifest rendering + dynamic fence + unsigned CDP WS.
    let merged = fetch_document(
        &ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cdp,
            tools: ToolPolicy::Manifest,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(merged.strategy, "cdp");
    assert_eq!(merged.tools.len(), 2);
    let decl = merged
        .tools
        .iter()
        .find(|t| t.name == "reserve_table")
        .expect("declarative HTML tool must remain present");
    assert_eq!(decl.source, WebMcpToolSource::Declarative);
    assert_eq!(decl.description, "Reserve dinner table");
    assert_eq!(
        decl.action_url.as_deref(),
        Some(format!("{origin}/api/reserve").as_str())
    );
    assert_eq!(decl.method.as_deref(), Some("POST"));
    assert_eq!(
        decl.annotations.as_ref().and_then(|a| a.untrusted_content),
        Some(true)
    );
    let schema_str = decl.input_schema.as_ref().unwrap().to_string();
    assert!(schema_str.contains("guests"));
    assert!(!schema_str.contains("secret_pin"));

    let imp = merged
        .tools
        .iter()
        .find(|t| t.name == "live_slots")
        .expect("imperative CDP tool must merge");
    assert_eq!(imp.source, WebMcpToolSource::Imperative);
    assert!(!imp.description.contains('\x1b'));
    assert_eq!(
        imp.annotations.as_ref().and_then(|a| a.untrusted_content),
        Some(true)
    );

    assert!(merged.content.contains("Dining Guide"));
    assert!(merged.content.contains("## Page Tools (WebMCP)"));
    assert!(merged.content.contains("````json"));
    for forbidden in [
        "STALE_CDP_CLOBBER",
        "REINJECTED_HIDDEN",
        "SUBFRAME",
        "EPHEMERAL",
        "hidden_admin",
        "secret_pin",
    ] {
        assert!(
            !merged.content.contains(forbidden),
            "merged output unexpectedly contained {forbidden}"
        );
    }

    // Check --tools manifest + --max-bytes preserves merged tools and bounds final output.
    let capped_manifest = fetch_document(
        &ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cdp,
            tools: ToolPolicy::Manifest,
            extract: ExtractOptions {
                max_bytes: Some(140),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(capped_manifest.truncated);
    assert!(capped_manifest.content.len() <= 140);
    assert_eq!(capped_manifest.content_bytes, capped_manifest.content.len());
    assert_eq!(capped_manifest.tools.len(), 2);

    // B. Cold RenderMode::Auto + --format axtree promotes directly to CDP, promotes children across generic, omits ignored, escapes controls, and bounds --max-bytes.
    let ax_cold_dir = temp_state_dir();
    let ax_cold_ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: ax_cold_dir.path().to_path_buf(),
        cdp_url: Some(cdp_ws.clone()),
        cdp_timeout: Duration::from_secs(5),
        no_web_bot_auth: true,
        ..Default::default()
    })
    .unwrap();
    let ax_full = fetch_document(
        &ax_cold_ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Auto,
            extract: ExtractOptions {
                format: OutputFormat::Axtree,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ax_full.strategy, "cdp");
    assert!(!ax_full.escalated);
    assert!(!ax_full.content.contains('\x1b'));
    assert!(!ax_full.content.contains("HiddenAX"));
    assert!(!ax_full.content.contains("generic"));
    assert!(ax_full.content.contains("RootWebArea \"Dining AX\""));
    assert!(ax_full.content.contains("\n  button \"Book[31m\\nNow\""));
    assert_eq!(ax_full.tools.len(), 2);

    let ax_capped = fetch_document(
        &ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cdp,
            tools: ToolPolicy::Manifest,
            extract: ExtractOptions {
                format: OutputFormat::Axtree,
                max_bytes: Some(90),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(ax_capped.truncated);
    assert!(ax_capped.content.len() <= 90);
    assert_eq!(ax_capped.content_bytes, ax_capped.content.len());

    // C. Verify WBA never signs CDP handshakes, robots.txt fetches, or plaintext http:// origins.
    let _plain_ok = fetch_document(
        &ctx,
        &format!("{origin}/plain"),
        &FetchOptions {
            render: RenderMode::Cronet,
            respect_robots: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for hdrs in ws_headers.lock().unwrap().iter() {
        for forbidden in ["signature", "signature-input", "signature-agent"] {
            assert!(
                !hdrs.contains_key(forbidden),
                "CDP WebSocket handshake leaked WBA header {forbidden}"
            );
        }
    }
    {
        let http_log_guard = http_requests.lock().unwrap();
        assert!(http_log_guard.iter().any(|(p, _)| p == "/robots.txt"));
        assert!(http_log_guard.iter().any(|(p, _)| p == "/plain"));
        for (path, hdrs) in http_log_guard.iter() {
            for forbidden in ["signature", "signature-input", "signature-agent"] {
                assert!(
                    !hdrs.contains_key(forbidden),
                    "plaintext http://{path} leaked WBA header {forbidden}"
                );
            }
        }
    }

    // D. Escalation CDP timeout persists "cdp_timeout" into site_cache.
    let timeout_dir = temp_state_dir();
    let timeout_ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: timeout_dir.path().to_path_buf(),
        cdp_url: Some(cdp_ws),
        cdp_timeout: Duration::from_millis(150),
        no_web_bot_auth: true,
        ..Default::default()
    })
    .unwrap();
    nav_delay_ms.store(500, Ordering::Relaxed);
    let esc_err = fetch_document(
        &timeout_ctx,
        &format!("{origin}/blocked"),
        &FetchOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(esc_err, FetchError::Blocked { .. }));
    let cached = timeout_ctx
        .site_cache
        .get("127.0.0.1")
        .expect("cdp_timeout should be recorded in site_cache");
    assert_eq!(cached.strategy, "cdp_timeout");
}
