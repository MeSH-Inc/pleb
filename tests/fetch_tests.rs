use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::extract::{ExtractOptions, OutputFormat};
use mesh_wick::fetch::{fetch_document, fetch_raw, FetchError, FetchOptions};
use mesh_wick::policy::RenderMode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn temp_state_dir(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mesh-wick-fetch-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
    let state_dir = temp_state_dir("e2e");
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.clone(),
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
    assert!(redir.content.contains(&format!("[Guide]({base}/docs/sub/guide?v=1)")));

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
    assert!(matches!(err_404_png, FetchError::HttpError { status: 404, .. }));
    assert_eq!(err_404_png.exit_code(), 3);

    let _ = std::fs::remove_dir_all(state_dir);
}

#[tokio::test]
async fn fetch_concurrent_requests_and_clean_teardown() {
    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir("concurrent");
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.clone(),
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
    let _ = std::fs::remove_dir_all(state_dir);
}

#[tokio::test]
async fn fetch_enforces_max_response_bytes() {
    let base = spawn_mock_server().await;
    let state_dir = temp_state_dir("max-bytes");
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.clone(),
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
        matches!(err_doc, FetchError::ResponseTooLarge { limit_bytes: 16, .. }),
        "unexpected document error: {err_doc:?}"
    );
    assert_eq!(err_doc.exit_code(), 1);
    assert_eq!(err_doc.kind(), "response_too_large");

    let err_raw = fetch_raw(&ctx, &format!("{base}/page"), false)
        .await
        .unwrap_err();
    assert!(
        matches!(err_raw, FetchError::ResponseTooLarge { limit_bytes: 16, .. }),
        "unexpected raw error: {err_raw:?}"
    );

    drop(ctx);
    let _ = std::fs::remove_dir_all(state_dir);
}
