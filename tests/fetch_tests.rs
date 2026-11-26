use flate2::write::GzEncoder;
use flate2::Compression;
use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::crawl::{crawl, map, CrawlOptions, MapOptions};
use mesh_wick::extract::{ExtractMode, ExtractOptions, OutputFormat};
use mesh_wick::fetch::{
    batch_exit_code, fetch_batch, fetch_document, fetch_raw, parse_batch_stdin, BatchFetchOptions,
    BatchItemResult, FetchError, FetchOptions,
};
use mesh_wick::policy::RenderMode;
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
        let dir = PathBuf::from(format!("/tmp/wick-t-{}-{}", std::process::id(), seq));
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
