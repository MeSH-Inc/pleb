#![cfg(unix)]

use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::daemon::{
    daemon_http_get, daemon_ping, daemon_stop, lock_path, run_daemon, socket_path, storage_key,
    DaemonResponseHeader, BUILD_ID, PROTOCOL_VERSION,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Notify;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

fn short_temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!(
        "/tmp/mw-d-{}-{}-{}-{}",
        tag,
        std::process::id(),
        seq,
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
    let dir = short_temp_dir("conc");
    let dir_clone = dir.clone();
    let handle = tokio::spawn(async move {
        run_daemon(dir_clone, Duration::from_secs(30), false).await
    });

    let init_status = wait_for_ping(&dir).await;
    assert_eq!(init_status.pid, std::process::id());
    assert_eq!(init_status.protocol_version, PROTOCOL_VERSION);
    assert_eq!(init_status.build_id, BUILD_ID);

    // Launch a slow HttpGet in the background and wait for it to reach the server.
    let dir_slow = dir.clone();
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
    let mid_ping = tokio::time::timeout(Duration::from_millis(400), daemon_ping(&dir))
        .await
        .expect("ping must not block on slow request")
        .expect("ping should succeed");
    assert!(mid_ping.active_requests >= 1);

    // Concurrent HttpGet requests succeed while slow request is in flight.
    let url_a = format!("{base}/fast-a");
    let url_b = format!("{base}/fast-b");
    let (r_a, r_b) = tokio::join!(
        daemon_http_get(
            &dir,
            None,
            Duration::from_secs(5),
            1024 * 1024,
            Duration::from_secs(30),
            false,
            &url_a,
        ),
        daemon_http_get(
            &dir,
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
        if let Some(st) = daemon_ping(&dir).await {
            if st.active_requests == 0 {
                settled = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(settled, "active_requests should drop to 0 after client disconnect");

    // --no-daemon refuses to run against a state_dir owned by a live daemon.
    let no_daemon_err = RuntimeContext::new(RuntimeConfig {
        state_dir: dir.clone(),
        use_daemon: false,
        ..Default::default()
    })
    .err()
    .expect("--no-daemon should fail when daemon owns state_dir");
    assert!(no_daemon_err.to_string().contains("daemon stop"));

    assert!(daemon_stop(&dir).await.unwrap());
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert!(!socket_path(&dir).exists());
    assert!(!daemon_stop(&dir).await.unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn daemon_proxy_partitioning_stop_restart_idle_and_legacy_cleanup() {
    let (base, _, _) = spawn_daemon_test_server().await;
    let dir = short_temp_dir("part");

    // Seed legacy cef.sock and cef.lock to verify run_daemon cleans them up.
    let legacy_sock = dir.join("cef.sock");
    let legacy_lock = dir.join("cef.lock");
    std::fs::write(&legacy_sock, b"").unwrap();
    std::fs::write(&legacy_lock, b"").unwrap();

    let dir_clone = dir.clone();
    let handle = tokio::spawn(async move {
        run_daemon(dir_clone, Duration::from_secs(30), false).await
    });
    let _ = wait_for_ping(&dir).await;
    assert!(!legacy_sock.exists());
    assert!(!legacy_lock.exists());

    // Direct request creates data/direct; proxy key creates separate proxy-* slot directory.
    let direct = daemon_http_get(
        &dir,
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
        &dir,
        Some(proxy),
        Duration::from_millis(500),
        1024 * 1024,
        Duration::from_secs(30),
        false,
        &format!("{base}/fast-a"),
    )
    .await;

    let ping = daemon_ping(&dir).await.unwrap();
    assert_eq!(ping.cronet_slots, 2);
    assert!(dir.join("data").join(storage_key(None)).exists());
    assert!(dir.join("data").join(storage_key(Some(proxy))).exists());

    // Stop and immediately restart (testing 2s lock retry).
    assert!(daemon_stop(&dir).await.unwrap());
    let _ = handle.await;

    let dir_idle = dir.clone();
    let handle2 = tokio::spawn(async move {
        run_daemon(dir_idle, Duration::from_millis(250), false).await
    });
    let _ = wait_for_ping(&dir).await;
    tokio::time::timeout(Duration::from_secs(3), handle2)
        .await
        .expect("daemon should exit after idle timeout")
        .unwrap()
        .unwrap();
    assert!(!socket_path(&dir).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn daemon_restarts_automatically_on_build_id_mismatch() {
    let (base, _, _) = spawn_daemon_test_server().await;
    let dir = short_temp_dir("build-id");
    let sock = socket_path(&dir);

    // Create a mock daemon listener returning a stale build_id and recording Shutdown.
    let listener = UnixListener::bind(&sock).unwrap();
    let lp = lock_path(&dir);
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

    // Drop mock lock when mock_task shuts down; run client with WICK_DAEMON_EXE pointing to wick bin.
    drop(_lock);
    std::env::set_var("WICK_DAEMON_EXE", env!("CARGO_BIN_EXE_wick"));

    let resp = daemon_http_get(
        &dir,
        None,
        Duration::from_secs(5),
        1024 * 1024,
        Duration::from_secs(5),
        false,
        &format!("{base}/fast-a"),
    )
    .await
    .unwrap();

    assert_eq!(saw_shutdown.load(Ordering::Acquire), 1);
    assert_eq!(resp.text(), "alpha");

    let _ = mock_task.await;
    let _ = daemon_stop(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
}
