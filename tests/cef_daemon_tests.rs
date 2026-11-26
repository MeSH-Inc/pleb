#![cfg(unix)]

use mesh_wick::transport::cef_daemon::{daemon_ping, daemon_stop, run_daemon, socket_path};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn short_temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = PathBuf::from(format!("/tmp/mw-{}-{}-{}", tag, std::process::id(), nanos));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn wait_for_ping(dir: &Path) -> mesh_wick::transport::cef_daemon::DaemonStatus {
    for _ in 0..80 {
        if let Some(status) = daemon_ping(dir).await {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for CEF daemon ping at {}", dir.display());
}

#[tokio::test]
async fn daemon_ping_and_stop_lifecycle() {
    let dir = short_temp_dir("life");
    let dir_clone = dir.clone();
    let handle = tokio::spawn(async move {
        run_daemon(dir_clone, Duration::from_secs(30), false).await
    });

    let status = wait_for_ping(&dir).await;
    assert_eq!(status.pid, std::process::id());
    assert!(!status.renderer_alive);
    assert!(socket_path(&dir).exists());

    assert!(daemon_stop(&dir).await.unwrap());
    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("daemon task should exit on stop")
        .unwrap()
        .unwrap();

    assert!(!socket_path(&dir).exists());
    assert!(!daemon_stop(&dir).await.unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn daemon_exits_on_idle_timeout_and_cleans_socket() {
    let dir = short_temp_dir("idle");
    let dir_clone = dir.clone();
    let handle = tokio::spawn(async move {
        run_daemon(dir_clone, Duration::from_millis(250), false).await
    });

    let _ = wait_for_ping(&dir).await;

    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("daemon should exit after idle timeout")
        .unwrap()
        .unwrap();

    assert!(!socket_path(&dir).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn second_daemon_respects_exclusive_lock() {
    let dir = short_temp_dir("lock");
    let dir_clone = dir.clone();
    let handle = tokio::spawn(async move {
        run_daemon(dir_clone, Duration::from_secs(30), false).await
    });

    let _ = wait_for_ping(&dir).await;

    // A competing daemon for the same state_dir exits cleanly without disturbing cef.sock.
    tokio::time::timeout(
        Duration::from_secs(2),
        run_daemon(dir.clone(), Duration::from_secs(30), false),
    )
    .await
    .expect("competing daemon should return promptly")
    .unwrap();

    assert!(daemon_ping(&dir).await.is_some());
    assert!(daemon_stop(&dir).await.unwrap());
    let _ = handle.await;

    let _ = std::fs::remove_dir_all(&dir);
}
