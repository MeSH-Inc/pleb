use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::cef::{CefRenderOptions, CefRenderer};

const SOCKET_NAME: &str = "cef.sock";
const LOCK_NAME: &str = "cef.lock";
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_IO_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub pid: u32,
    pub renderer_alive: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum DaemonRequest {
    Ping,
    Shutdown,
    Render {
        url: String,
        wait_for_selector: Option<String>,
        proxy: Option<String>,
        timeout_ms: u64,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DaemonResponse {
    ok: bool,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    renderer_alive: Option<bool>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

struct SocketCleanupGuard {
    sock_path: PathBuf,
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_NAME)
}

fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOCK_NAME)
}

pub async fn run_daemon(state_dir: PathBuf, idle_timeout: Duration, verbose: bool) -> Result<()> {
    std::fs::create_dir_all(&state_dir)?;

    let lp = lock_path(&state_dir);
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lp)
        .with_context(|| format!("failed to open CEF daemon lock file {}", lp.display()))?;

    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            // Another daemon instance already holds the lock for this state_dir.
            return Ok(());
        }
        Err(err) => {
            return Err(err).context("failed to acquire CEF daemon lock");
        }
    }

    let sock_path = socket_path(&state_dir);
    if sock_path.as_os_str().len() >= 100 {
        bail!(
            "state_dir socket path too long for Unix domain socket ({} bytes): {}",
            sock_path.as_os_str().len(),
            sock_path.display()
        );
    }

    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind CEF daemon socket {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    let _sock_guard = SocketCleanupGuard {
        sock_path: sock_path.clone(),
    };

    let mut active_renderer: Option<(Option<String>, Duration, CefRenderer)> = None;

    loop {
        let stream = tokio::select! {
            _ = shutdown_signal() => break,
            res = tokio::time::timeout(idle_timeout, listener.accept()) => match res {
                Ok(Ok((stream, _))) => stream,
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => break,
            },
        };

        let should_stop = tokio::select! {
            _ = shutdown_signal() => true,
            res = handle_connection(stream, &state_dir, &mut active_renderer, verbose) => res?,
        };

        if should_stop {
            break;
        }
    }

    drop(active_renderer);
    drop(_sock_guard);
    drop(lock_file);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    state_dir: &Path,
    active_renderer: &mut Option<(Option<String>, Duration, CefRenderer)>,
    verbose: bool,
) -> Result<bool> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => return Ok(false),
        Ok(Ok(_)) => {}
        Ok(Err(err)) => return Err(err.into()),
        Err(_) => {
            let resp = DaemonResponse {
                ok: false,
                error: Some("timed out reading daemon request".to_string()),
                ..Default::default()
            };
            let _ = write_response(&mut write_half, &resp).await;
            return Ok(false);
        }
    }

    let req: DaemonRequest = match serde_json::from_str(line.trim()) {
        Ok(req) => req,
        Err(err) => {
            let resp = DaemonResponse {
                ok: false,
                error: Some(format!("invalid daemon request: {}", err)),
                ..Default::default()
            };
            let _ = write_response(&mut write_half, &resp).await;
            return Ok(false);
        }
    };

    match req {
        DaemonRequest::Ping => {
            let renderer_alive = active_renderer
                .as_ref()
                .is_some_and(|(_, _, r)| r.has_live_process());
            let resp = DaemonResponse {
                ok: true,
                pid: Some(std::process::id()),
                renderer_alive: Some(renderer_alive),
                ..Default::default()
            };
            write_response(&mut write_half, &resp).await?;
            Ok(false)
        }
        DaemonRequest::Shutdown => {
            let resp = DaemonResponse {
                ok: true,
                ..Default::default()
            };
            let _ = write_response(&mut write_half, &resp).await;
            Ok(true)
        }
        DaemonRequest::Render {
            url,
            wait_for_selector,
            proxy,
            timeout_ms,
        } => {
            let timeout = Duration::from_millis(timeout_ms.max(1));
            let needs_new = match active_renderer {
                Some((ref p, t, _)) => p != &proxy || *t != timeout,
                None => true,
            };
            if needs_new {
                *active_renderer = Some((
                    proxy.clone(),
                    timeout,
                    CefRenderer::new_direct(state_dir.to_path_buf(), proxy, timeout, verbose),
                ));
            }

            let (_, _, renderer) = active_renderer.as_ref().expect("renderer initialized");
            let opts = CefRenderOptions { wait_for_selector };
            let resp = match renderer.render(&url, &opts).await {
                Ok(html) => DaemonResponse {
                    ok: true,
                    html: Some(html),
                    ..Default::default()
                },
                Err(err) => DaemonResponse {
                    ok: false,
                    error: Some(err.to_string()),
                    ..Default::default()
                },
            };
            let _ = write_response(&mut write_half, &resp).await;
            Ok(false)
        }
    }
}

async fn write_response(write_half: &mut tokio::net::unix::OwnedWriteHalf, resp: &DaemonResponse) -> Result<()> {
    let mut encoded = serde_json::to_string(resp)?;
    encoded.push('\n');
    write_half.write_all(encoded.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        match sigterm {
            Some(ref mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn daemon_ping(state_dir: &Path) -> Option<DaemonStatus> {
    let sock = socket_path(state_dir);
    let resp = send_request(&sock, &DaemonRequest::Ping, Duration::from_secs(2))
        .await
        .ok()?;
    if !resp.ok {
        return None;
    }
    Some(DaemonStatus {
        pid: resp.pid?,
        renderer_alive: resp.renderer_alive.unwrap_or(false),
    })
}

pub async fn daemon_stop(state_dir: &Path) -> Result<bool> {
    let sock = socket_path(state_dir);
    match send_request(&sock, &DaemonRequest::Shutdown, Duration::from_secs(3)).await {
        Ok(resp) => {
            wait_for_socket_gone(&sock, Duration::from_secs(2)).await;
            Ok(resp.ok)
        }
        Err(err) if is_not_running_error(&err) => {
            if sock.exists() {
                let _ = std::fs::remove_file(&sock);
            }
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

pub async fn daemon_render(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
    opts: &CefRenderOptions,
) -> Result<String> {
    let sock = socket_path(state_dir);
    let req = DaemonRequest::Render {
        url: url.to_string(),
        wait_for_selector: opts.wait_for_selector.clone(),
        proxy: proxy.map(str::to_string),
        timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
    };
    let io_timeout = timeout.saturating_add(CLIENT_IO_GRACE);

    let resp = match send_request(&sock, &req, io_timeout).await {
        Ok(r) => r,
        Err(err) if is_not_running_error(&err) => {
            spawn_detached_daemon(state_dir, idle_timeout, verbose)?;
            wait_for_socket_ready(&sock, DAEMON_START_TIMEOUT).await?;
            send_request(&sock, &req, io_timeout).await?
        }
        Err(err) => return Err(err),
    };

    if resp.ok {
        resp.html
            .ok_or_else(|| anyhow::anyhow!("CEF daemon returned ok=true with no HTML"))
    } else {
        bail!(
            "{}",
            resp.error
                .unwrap_or_else(|| "CEF daemon render failed".to_string())
        )
    }
}

fn spawn_detached_daemon(state_dir: &Path, idle_timeout: Duration, verbose: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let idle_secs = idle_timeout.as_secs().max(1).to_string();

    let mut cmd = Command::new(exe);
    cmd.arg("--state-dir")
        .arg(state_dir)
        .arg("daemon")
        .arg("run")
        .arg("--idle-secs")
        .arg(&idle_secs)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);

    if verbose {
        cmd.arg("--verbose");
    }

    cmd.spawn()
        .context("failed to spawn detached CEF daemon")?;
    Ok(())
}

async fn wait_for_socket_ready(sock_path: &Path, limit: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Ok(stream) = UnixStream::connect(sock_path).await {
            drop(stream);
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for CEF daemon socket {}",
                sock_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_socket_gone(sock_path: &Path, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while sock_path.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn send_request(sock_path: &Path, req: &DaemonRequest, timeout: Duration) -> Result<DaemonResponse> {
    let fut = async {
        let stream = UnixStream::connect(sock_path).await?;
        let (read_half, mut write_half) = stream.into_split();

        let mut encoded = serde_json::to_string(req)?;
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half);
        let mut resp_line = String::new();
        let n = reader.read_line(&mut resp_line).await?;
        if n == 0 {
            bail!("CEF daemon closed connection without a response");
        }
        let resp: DaemonResponse = serde_json::from_str(resp_line.trim())?;
        Ok::<DaemonResponse, anyhow::Error>(resp)
    };

    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| anyhow::anyhow!("CEF daemon request timed out after {:?}", timeout))?
}

fn is_not_running_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    })
}
