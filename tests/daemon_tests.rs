#![cfg(unix)]

use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::daemon::{
    daemon_http_get, daemon_http_get_with_executable, daemon_ping, daemon_stop, lock_path,
    resolve_daemon_executable, run_daemon, socket_path, storage_key, DaemonResponseHeader,
    BUILD_ID, PROTOCOL_VERSION,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Notify;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempStateDir(PathBuf);

impl TempStateDir {
    fn new() -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/wick-t-{}-{}", std::process::id(), seq));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TempStateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn wait_for_ping(dir: &Path) -> mesh_wick::daemon::DaemonStatus {
    for _ in 0..80 {
        if let Some(status) = daemon_ping(dir).await {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for daemon ping at {}", dir.display());
}

async fn spawn_daemon_test_server() -> (String, Arc<AtomicUsize>, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let active_slow = Arc::new(AtomicUsize::new(0));
    let slow_started = Arc::new(Notify::new());
    let active_slow_c = Arc::clone(&active_slow);
    let slow_started_c = Arc::clone(&slow_started);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let active_slow = Arc::clone(&active_slow_c);
            let slow_started = Arc::clone(&slow_started_c);
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

                match path {
                    "/fast-a" => {
                        let body = b"alpha";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    }
                    "/fast-b" => {
                        let body = b"beta";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    }
                    "/slow" => {
                        active_slow.fetch_add(1, Ordering::AcqRel);
                        slow_started.notify_waiters();
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        let _ = socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone")
                            .await;
                        active_slow.fetch_sub(1, Ordering::AcqRel);
                    }
                    _ => {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                }
            });
        }
    });

    (format!("http://{}", addr), active_slow, slow_started)
}

#[tokio::test]
async fn daemon_concurrent_http_ping_during_slow_and_disconnect_cancel() {
    let (base, _active_slow, slow_started) = spawn_daemon_test_server().await;
    let dir = TempStateDir::new();
    let dir_clone = dir.to_path_buf();
    let handle =
        tokio::spawn(async move { run_daemon(dir_clone, Duration::from_secs(30), false).await });

    let init_status = wait_for_ping(dir.path()).await;
    assert_eq!(init_status.pid, std::process::id());
    assert_eq!(init_status.protocol_version, PROTOCOL_VERSION);
    assert_eq!(init_status.build_id, BUILD_ID);

    // Launch a slow HttpGet in the background and wait for it to reach the server.
    let dir_slow = dir.to_path_buf();
    let slow_url = format!("{base}/slow");
    let slow_task = tokio::spawn(async move {
        daemon_http_get(
            &dir_slow,
            None,
            Duration::from_secs(10),
            1024 * 1024,
            Duration::from_secs(30),
            false,
            &slow_url,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(2), slow_started.notified())
        .await
        .expect("slow request should reach mock server");

    // Ping must return immediately while slow request is active, reporting active_requests >= 1.
    let mid_ping = tokio::time::timeout(Duration::from_millis(400), daemon_ping(dir.path()))
        .await
        .expect("ping must not block on slow request")
        .expect("ping should succeed");
    assert!(mid_ping.active_requests >= 1);

    // Concurrent HttpGet requests succeed while slow request is in flight.
    let url_a = format!("{base}/fast-a");
    let url_b = format!("{base}/fast-b");
    let (r_a, r_b) = tokio::join!(
        daemon_http_get(
            dir.path(),
            None,
            Duration::from_secs(5),
            1024 * 1024,
            Duration::from_secs(30),
            false,
            &url_a,
        ),
        daemon_http_get(
            dir.path(),
            None,
            Duration::from_secs(5),
            1024 * 1024,
            Duration::from_secs(30),
            false,
            &url_b,
        ),
    );
    assert_eq!(r_a.unwrap().text(), "alpha");
    assert_eq!(r_b.unwrap().text(), "beta");

    // Abort the client holding the slow request (simulating client disconnect) -> server drops HttpGet.
    slow_task.abort();
    let _ = slow_task.await;

    let mut settled = false;
    for _ in 0..40 {
        if let Some(st) = daemon_ping(dir.path()).await {
            if st.active_requests == 0 {
                settled = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        settled,
        "active_requests should drop to 0 after client disconnect"
    );

    // --no-daemon refuses to run against a state_dir owned by a live daemon.
    let no_daemon_err = RuntimeContext::new(RuntimeConfig {
        state_dir: dir.to_path_buf(),
        use_daemon: false,
        ..Default::default()
    })
    .err()
    .expect("--no-daemon should fail when daemon owns state_dir");
    assert!(no_daemon_err.to_string().contains("daemon stop"));

    assert!(daemon_stop(dir.path()).await.unwrap());
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert!(!socket_path(dir.path()).exists());
    assert!(!daemon_stop(dir.path()).await.unwrap());
}

#[tokio::test]
async fn daemon_proxy_partitioning_stop_restart_idle_and_legacy_cleanup() {
    let (base, _, _) = spawn_daemon_test_server().await;
    let dir = TempStateDir::new();

    // Seed legacy cef.sock and cef.lock to verify run_daemon cleans them up.
    let legacy_sock = dir.path().join("cef.sock");
    let legacy_lock = dir.path().join("cef.lock");
    std::fs::write(&legacy_sock, b"").unwrap();
    std::fs::write(&legacy_lock, b"").unwrap();

    let dir_clone = dir.to_path_buf();
    let handle =
        tokio::spawn(async move { run_daemon(dir_clone, Duration::from_secs(30), false).await });
    let _ = wait_for_ping(dir.path()).await;
    assert!(!legacy_sock.exists());
    assert!(!legacy_lock.exists());

    // Direct request creates data/direct; proxy key creates separate proxy-* slot directory.
    let direct = daemon_http_get(
        dir.path(),
        None,
        Duration::from_secs(5),
        1024 * 1024,
        Duration::from_secs(30),
        false,
        &format!("{base}/fast-a"),
    )
    .await
    .unwrap();
    assert_eq!(direct.text(), "alpha");

    let proxy = "http://127.0.0.1:9";
    let _ = daemon_http_get(
        dir.path(),
        Some(proxy),
        Duration::from_millis(500),
        1024 * 1024,
        Duration::from_secs(30),
        false,
        &format!("{base}/fast-a"),
    )
    .await;

    let ping = daemon_ping(dir.path()).await.unwrap();
    assert_eq!(ping.cronet_slots, 2);
    assert!(dir.path().join("data").join(storage_key(None)).exists());
    assert!(dir
        .path()
        .join("data")
        .join(storage_key(Some(proxy)))
        .exists());

    // Stop and immediately restart (testing 2s lock retry).
    assert!(daemon_stop(dir.path()).await.unwrap());
    let _ = handle.await;

    let dir_idle = dir.to_path_buf();
    let handle2 =
        tokio::spawn(async move { run_daemon(dir_idle, Duration::from_millis(250), false).await });
    let _ = wait_for_ping(dir.path()).await;
    tokio::time::timeout(Duration::from_secs(3), handle2)
        .await
        .expect("daemon should exit after idle timeout")
        .unwrap()
        .unwrap();
    assert!(!socket_path(dir.path()).exists());
}

#[tokio::test]
async fn daemon_restarts_automatically_on_build_id_mismatch() {
    let (base, _, _) = spawn_daemon_test_server().await;
    let dir = TempStateDir::new();
    let sock = socket_path(dir.path());

    // Create a mock daemon listener returning a stale build_id and recording Shutdown.
    let listener = UnixListener::bind(&sock).unwrap();
    let lp = lock_path(dir.path());
    let _lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lp)
        .unwrap();

    let saw_shutdown = Arc::new(AtomicUsize::new(0));
    let saw_shutdown_c = Arc::clone(&saw_shutdown);
    let sock_c = sock.clone();

    let mock_task = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            let val: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            if val["op"] == "shutdown" {
                saw_shutdown_c.fetch_add(1, Ordering::AcqRel);
                let mut hdr = DaemonResponseHeader::ok_empty();
                hdr.build_id = "stale-build-id".to_string();
                let mut enc = serde_json::to_string(&hdr).unwrap();
                enc.push('\n');
                let _ = write_half.write_all(enc.as_bytes()).await;
                let _ = write_half.flush().await;
                drop(listener);
                let _ = std::fs::remove_file(&sock_c);
                break;
            } else {
                let mut hdr = DaemonResponseHeader::ok_empty();
                hdr.build_id = "stale-build-id".to_string();
                hdr.status_code = Some(200);
                hdr.final_url = Some("http://mock/stale".to_string());
                let mut enc = serde_json::to_string(&hdr).unwrap();
                enc.push('\n');
                let _ = write_half.write_all(enc.as_bytes()).await;
                let _ = write_half.flush().await;
            }
        }
    });

    // Drop mock lock when mock_task shuts down; pass explicit CARGO_BIN_EXE_wick path.
    drop(_lock);

    let resp = daemon_http_get_with_executable(
        dir.path(),
        None,
        Duration::from_secs(5),
        1024 * 1024,
        Duration::from_secs(5),
        false,
        &format!("{base}/fast-a"),
        Some(Path::new(env!("CARGO_BIN_EXE_wick"))),
    )
    .await
    .unwrap();

    assert_eq!(saw_shutdown.load(Ordering::Acquire), 1);
    assert_eq!(resp.text(), "alpha");

    let _ = mock_task.await;
    let _ = daemon_stop(dir.path()).await;
}

#[test]
fn resolve_daemon_executable_rejects_cargo_test_runner_and_accepts_override() {
    let explicit = Path::new(env!("CARGO_BIN_EXE_wick"));
    assert_eq!(resolve_daemon_executable(Some(explicit)).unwrap(), explicit);

    if std::env::var_os("WICK_DAEMON_EXECUTABLE").is_none() {
        let err = resolve_daemon_executable(None)
            .expect_err("Cargo test runner under deps/ must not be spawned as daemon");
        assert!(err.to_string().contains("WICK_DAEMON_EXECUTABLE"));
    }
}

#[tokio::test]
async fn run_daemon_exits_immediately_when_live_daemon_responds_to_ping() {
    let dir = TempStateDir::new();
    let dir_clone = dir.to_path_buf();
    let handle =
        tokio::spawn(async move { run_daemon(dir_clone, Duration::from_secs(30), false).await });
    let _ = wait_for_ping(dir.path()).await;

    let started = Instant::now();
    run_daemon(dir.to_path_buf(), Duration::from_secs(30), false)
        .await
        .expect("second concurrent run_daemon should exit cleanly");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "second run_daemon spun for {:?} instead of exiting immediately on live ping",
        started.elapsed()
    );

    assert!(daemon_stop(dir.path()).await.unwrap());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_round_trips_structured_transport_error_kinds() {
    use mesh_wick::transport::cef::{CefProtocolError, CefTimeoutError};
    use mesh_wick::transport::{ResponseTooLargeError, TransportError, TransportErrorKind};

    let dir = TempStateDir::new();
    let sock = socket_path(dir.path());
    let listener = UnixListener::bind(&sock).unwrap();

    let cases: Vec<(TransportErrorKind, Option<usize>)> = vec![
        (TransportErrorKind::LoadError, None),
        (TransportErrorKind::InvalidSelector, None),
        (TransportErrorKind::Canceled, None),
        (TransportErrorKind::Network, None),
        (TransportErrorKind::Timeout, None),
        (TransportErrorKind::ProtocolMismatch, None),
        (TransportErrorKind::ResponseTooLarge, Some(4096)),
    ];

    let server_cases = cases.clone();
    let server = tokio::spawn(async move {
        for (kind, limit) in server_cases {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            let hdr = DaemonResponseHeader::err("structured failure", Some(kind), limit);
            let mut enc = serde_json::to_string(&hdr).unwrap();
            enc.push('\n');
            write_half.write_all(enc.as_bytes()).await.unwrap();
            write_half.flush().await.unwrap();
        }
    });

    for (kind, limit) in cases {
        let err = daemon_http_get_with_executable(
            dir.path(),
            None,
            Duration::from_secs(5),
            1024 * 1024,
            Duration::from_secs(5),
            false,
            "http://127.0.0.1/err",
            Some(Path::new(env!("CARGO_BIN_EXE_wick"))),
        )
        .await
        .unwrap_err();

        match kind {
            TransportErrorKind::Timeout => assert!(err.is::<CefTimeoutError>()),
            TransportErrorKind::ProtocolMismatch => assert!(err.is::<CefProtocolError>()),
            TransportErrorKind::ResponseTooLarge => {
                let rtl = err
                    .downcast_ref::<ResponseTooLargeError>()
                    .expect("expected ResponseTooLargeError");
                assert_eq!(rtl.limit_bytes, limit.unwrap());
            }
            other => {
                let te = err
                    .downcast_ref::<TransportError>()
                    .expect("expected TransportError");
                assert_eq!(te.kind, other);
            }
        }
    }

    server.await.unwrap();
}

#[test]
fn adversarial_zero_untyped_error_matches_in_transport_and_daemon() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dirs = [manifest.join("src/transport"), manifest.join("src/daemon")];
    let forbidden = [
        ".to_string().contains(",
        ".to_string().starts_with(",
        ".to_string().ends_with(",
        "err.starts_with(",
        "error.starts_with(",
        "msg.starts_with(",
    ];
    let mut violations = Vec::new();
    for dir in dirs {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let non_test = src.split("#[cfg(test)]").next().unwrap_or(&src);
            for (idx, line) in non_test.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                for pat in forbidden {
                    if trimmed.contains(pat) {
                        violations.push(format!("{}:{}: {}", path.display(), idx + 1, trimmed));
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "untyped_error_matches must be 0 in src/transport & src/daemon:\n{}",
        violations.join("\n")
    );
}

async fn spawn_link_and_accept_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let mut lines = req.lines();
                let path = lines
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let mut accept_val = String::new();
                let mut sec_fetch_mode = String::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, val)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("accept") {
                            accept_val = val.trim().to_string();
                        } else if name.eq_ignore_ascii_case("sec-fetch-mode") {
                            sec_fetch_mode = val.trim().to_string();
                        }
                    }
                }

                match path {
                    "/links" => {
                        let body = b"<html><body>ok</body></html>";
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nLink: </docs/page.md>; rel=\"alternate\"; type=\"text/markdown\"\r\nlink: </feed.xml>; rel=\"alternate\"; type=\"application/atom+xml\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    }
                    "/echo-accept" => {
                        let body = format!("accept={accept_val}\nsec-fetch-mode={sec_fetch_mode}");
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(hdr.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                    _ => {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                }
            });
        }
    });

    format!("http://{}", addr)
}

#[tokio::test]
async fn transport_captures_multiple_link_headers_and_accept_markdown_direct_and_daemon() {
    use mesh_wick::transport::{TransportClient, MARKDOWN_ACCEPT_HEADER};

    let base = spawn_link_and_accept_server().await;
    let expected_link = "</docs/page.md>; rel=\"alternate\"; type=\"text/markdown\", </feed.xml>; rel=\"alternate\"; type=\"application/atom+xml\"";

    // Direct (non-daemon) TransportClient
    let direct_dir = TempStateDir::new();
    let direct = TransportClient::new_direct(direct_dir.path(), None, Duration::from_secs(5))
        .expect("direct transport should initialize");

    let link_resp = direct.get(&format!("{base}/links")).await.unwrap();
    assert_eq!(link_resp.link_header.as_deref(), Some(expected_link));

    let default_accept = direct
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            false,
        )
        .await
        .unwrap()
        .text();
    assert!(
        default_accept.contains("accept=text/html,application/xhtml+xml"),
        "expected Chrome default Accept header, got: {default_accept}"
    );
    assert!(default_accept.contains("sec-fetch-mode=navigate"));

    let md_accept = direct
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            true,
        )
        .await
        .unwrap()
        .text();
    assert!(
        md_accept.contains(&format!("accept={MARKDOWN_ACCEPT_HEADER}")),
        "expected Markdown Accept header, got: {md_accept}"
    );
    assert!(md_accept.contains("sec-fetch-mode=navigate"));

    // Daemon-backed TransportClient over daemon.sock
    let daemon_dir = TempStateDir::new();
    let daemon_dir_clone = daemon_dir.to_path_buf();
    let handle =
        tokio::spawn(
            async move { run_daemon(daemon_dir_clone, Duration::from_secs(30), false).await },
        );
    let _ = wait_for_ping(daemon_dir.path()).await;

    let daemon_client = TransportClient::new_daemon(
        daemon_dir.to_path_buf(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(30),
        false,
    );

    let daemon_link_resp = daemon_client.get(&format!("{base}/links")).await.unwrap();
    assert_eq!(daemon_link_resp.link_header.as_deref(), Some(expected_link));

    let daemon_default_accept = daemon_client
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            false,
        )
        .await
        .unwrap()
        .text();
    assert!(
        daemon_default_accept.contains("accept=text/html,application/xhtml+xml"),
        "expected Chrome default Accept header over daemon.sock, got: {daemon_default_accept}"
    );
    assert!(daemon_default_accept.contains("sec-fetch-mode=navigate"));

    let daemon_md_accept = daemon_client
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            true,
        )
        .await
        .unwrap()
        .text();
    assert!(
        daemon_md_accept.contains(&format!("accept={MARKDOWN_ACCEPT_HEADER}")),
        "expected Markdown Accept header over daemon.sock, got: {daemon_md_accept}"
    );
    assert!(daemon_md_accept.contains("sec-fetch-mode=navigate"));

    assert!(daemon_stop(daemon_dir.path()).await.unwrap());
    let _ = handle.await;
}

#[tokio::test]
async fn daemon_restarts_automatically_on_protocol_version_mismatch() {
    assert_eq!(PROTOCOL_VERSION, 2);
    let (base, _, _) = spawn_daemon_test_server().await;
    let dir = TempStateDir::new();
    let sock = socket_path(dir.path());

    let listener = UnixListener::bind(&sock).unwrap();
    let saw_shutdown = Arc::new(AtomicUsize::new(0));
    let saw_shutdown_c = Arc::clone(&saw_shutdown);
    let sock_c = sock.clone();

    let mock_task = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            let val: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let mut hdr = DaemonResponseHeader::ok_empty();
            hdr.protocol_version = 1;
            hdr.build_id = BUILD_ID.to_string();
            if val["op"] == "shutdown" {
                saw_shutdown_c.fetch_add(1, Ordering::AcqRel);
                let mut enc = serde_json::to_string(&hdr).unwrap();
                enc.push('\n');
                let _ = write_half.write_all(enc.as_bytes()).await;
                let _ = write_half.flush().await;
                drop(listener);
                let _ = std::fs::remove_file(&sock_c);
                break;
            } else {
                hdr.status_code = Some(200);
                hdr.final_url = Some("http://mock/v1".to_string());
                let mut enc = serde_json::to_string(&hdr).unwrap();
                enc.push('\n');
                let _ = write_half.write_all(enc.as_bytes()).await;
                let _ = write_half.flush().await;
            }
        }
    });

    let resp = daemon_http_get_with_executable(
        dir.path(),
        None,
        Duration::from_secs(5),
        1024 * 1024,
        Duration::from_secs(5),
        false,
        &format!("{base}/fast-a"),
        Some(Path::new(env!("CARGO_BIN_EXE_wick"))),
    )
    .await
    .unwrap();

    assert_eq!(saw_shutdown.load(Ordering::Acquire), 1);
    assert_eq!(resp.text(), "alpha");
    let status = daemon_ping(dir.path())
        .await
        .expect("v2 daemon should be running");
    assert_eq!(status.protocol_version, 2);

    let _ = mock_task.await;
    let _ = daemon_stop(dir.path()).await;
}

#[tokio::test]
async fn adversarial_accept_markdown_no_sticky_leak_and_absent_link_is_none() {
    use mesh_wick::transport::{TransportClient, MARKDOWN_ACCEPT_HEADER};

    let base = spawn_link_and_accept_server().await;

    // Direct client: accept_markdown=true followed by accept_markdown=false must restore Chrome Accept
    let direct_dir = TempStateDir::new();
    let direct = TransportClient::new_direct(direct_dir.path(), None, Duration::from_secs(5))
        .expect("direct transport should initialize");

    let first = direct
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            true,
        )
        .await
        .unwrap();
    assert_eq!(first.link_header, None);
    assert!(first
        .text()
        .contains(&format!("accept={MARKDOWN_ACCEPT_HEADER}")));

    let second = direct
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            false,
        )
        .await
        .unwrap();
    assert_eq!(second.link_header, None);
    let second_text = second.text();
    assert!(
        second_text.contains("accept=text/html,application/xhtml+xml")
            && !second_text.contains("text/markdown"),
        "accept_markdown leaked into subsequent direct request: {second_text}"
    );

    // Daemon client: same non-sticky invariant across pooled daemon.sock slot
    let daemon_dir = TempStateDir::new();
    let daemon_dir_clone = daemon_dir.to_path_buf();
    let handle =
        tokio::spawn(
            async move { run_daemon(daemon_dir_clone, Duration::from_secs(30), false).await },
        );
    let _ = wait_for_ping(daemon_dir.path()).await;

    let daemon_client = TransportClient::new_daemon(
        daemon_dir.to_path_buf(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(30),
        false,
    );
    let d1 = daemon_client
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            true,
        )
        .await
        .unwrap();
    assert_eq!(d1.link_header, None);
    assert!(d1
        .text()
        .contains(&format!("accept={MARKDOWN_ACCEPT_HEADER}")));

    let d2 = daemon_client
        .get_with_options(
            &format!("{base}/echo-accept"),
            Duration::from_secs(5),
            64 * 1024,
            false,
        )
        .await
        .unwrap();
    assert_eq!(d2.link_header, None);
    let d2_text = d2.text();
    assert!(
        d2_text.contains("accept=text/html,application/xhtml+xml")
            && !d2_text.contains("text/markdown"),
        "accept_markdown leaked into subsequent daemon request: {d2_text}"
    );

    assert!(daemon_stop(daemon_dir.path()).await.unwrap());
    let _ = handle.await;
}
