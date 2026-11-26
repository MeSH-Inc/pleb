use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::protocol::{
    lock_path, log_path, read_frame, socket_path, DaemonRequest, DaemonResponseHeader,
    DaemonStatus, BUILD_ID, PROTOCOL_VERSION,
};
use super::server::stop_legacy_cef_daemon;
use crate::context::ensure_private_dir;
use crate::transport::cef::{CefProtocolError, CefRenderOptions, CefRenderResult, CefTimeoutError};
use crate::transport::{
    HttpResponse, RequestHeaderOptions, ResponseTooLargeError, TransportError, TransportErrorKind,
};

const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_IO_GRACE: Duration = Duration::from_secs(5);

pub fn is_daemon_running(state_dir: &Path) -> bool {
    let lp = lock_path(state_dir);
    if !lp.exists() {
        return false;
    }
    let Ok(file) = OpenOptions::new().read(true).open(&lp) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

pub async fn daemon_ping(state_dir: &Path) -> Option<DaemonStatus> {
    let sock = socket_path(state_dir);
    let (hdr, _) = try_send_once(&sock, &DaemonRequest::Ping, Duration::from_secs(2))
        .await
        .ok()?;
    if !hdr.ok {
        return None;
    }
    Some(DaemonStatus {
        pid: hdr.pid?,
        protocol_version: hdr.protocol_version,
        build_id: hdr.build_id,
        active_requests: hdr.active_requests.unwrap_or(0),
        cronet_slots: hdr.cronet_slots.unwrap_or(0),
        cef_renderers_alive: hdr.cef_renderers_alive.unwrap_or(0),
    })
}

pub async fn daemon_stop(state_dir: &Path) -> Result<bool> {
    stop_legacy_cef_daemon(state_dir).await;
    let sock = socket_path(state_dir);
    match try_send_once(&sock, &DaemonRequest::Shutdown, Duration::from_secs(3)).await {
        Ok((hdr, _)) => {
            wait_for_daemon_exit(state_dir, &sock, Duration::from_secs(3)).await;
            if is_daemon_running(state_dir) {
                bail!(
                    "daemon at {} did not release lock after shutdown",
                    state_dir.display()
                );
            }
            Ok(hdr.ok)
        }
        Err(err) if is_retryable_pre_response_error(&err) => {
            if is_daemon_running(state_dir) {
                wait_for_daemon_exit(state_dir, &sock, Duration::from_secs(1)).await;
                if is_daemon_running(state_dir) {
                    return Err(err);
                }
            }
            if sock.exists() {
                let _ = std::fs::remove_file(&sock);
            }
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn daemon_http_get(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    max_body_bytes: usize,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
) -> Result<HttpResponse> {
    daemon_http_get_with_executable(
        state_dir,
        proxy,
        timeout,
        max_body_bytes,
        idle_timeout,
        verbose,
        url,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn daemon_http_get_with_executable(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    max_body_bytes: usize,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
    daemon_executable: Option<&Path>,
) -> Result<HttpResponse> {
    daemon_http_get_with_request_options(
        state_dir,
        proxy,
        timeout,
        max_body_bytes,
        &RequestHeaderOptions::default(),
        idle_timeout,
        verbose,
        url,
        daemon_executable,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn daemon_http_get_with_request_options(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    max_body_bytes: usize,
    opts: &RequestHeaderOptions,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
    daemon_executable: Option<&Path>,
) -> Result<HttpResponse> {
    let req = DaemonRequest::HttpGet {
        url: url.to_string(),
        proxy: proxy.map(str::to_string),
        timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
        max_body_bytes,
        accept_markdown: opts.accept_markdown,
        referer: opts.referer.clone(),
        subresource_dest: opts.subresource_dest,
    };
    let io_timeout = timeout.saturating_add(CLIENT_IO_GRACE);
    let (hdr, body) = send_daemon_rpc(
        state_dir,
        &req,
        io_timeout,
        idle_timeout,
        verbose,
        daemon_executable,
    )
    .await?;

    if hdr.ok {
        Ok(HttpResponse {
            status: hdr.status_code.unwrap_or(200),
            final_url: hdr.final_url.unwrap_or_else(|| url.to_string()),
            content_type: hdr.content_type,
            link_header: hdr.link_header,
            body,
        })
    } else {
        Err(map_daemon_error(&hdr, "daemon HTTP request failed"))
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn daemon_cef_render(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
    opts: &CefRenderOptions,
) -> Result<CefRenderResult> {
    daemon_cef_render_with_executable(
        state_dir,
        proxy,
        timeout,
        idle_timeout,
        verbose,
        url,
        opts,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn daemon_cef_render_with_executable(
    state_dir: &Path,
    proxy: Option<&str>,
    timeout: Duration,
    idle_timeout: Duration,
    verbose: bool,
    url: &str,
    opts: &CefRenderOptions,
    daemon_executable: Option<&Path>,
) -> Result<CefRenderResult> {
    let req = DaemonRequest::CefRender {
        url: url.to_string(),
        wait_for_selector: opts.wait_for_selector.clone(),
        proxy: proxy.map(str::to_string),
        timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
    };
    let io_timeout = timeout.saturating_add(CLIENT_IO_GRACE);
    let (hdr, body) = send_daemon_rpc(
        state_dir,
        &req,
        io_timeout,
        idle_timeout,
        verbose,
        daemon_executable,
    )
    .await?;

    if hdr.ok {
        let html = String::from_utf8_lossy(&body).into_owned();
        Ok(CefRenderResult {
            html,
            final_url: hdr.final_url.unwrap_or_else(|| url.to_string()),
            status_code: hdr.status_code.unwrap_or(200),
        })
    } else {
        Err(map_daemon_error(&hdr, "daemon CEF render failed"))
    }
}

fn map_daemon_error(hdr: &DaemonResponseHeader, default_msg: &str) -> anyhow::Error {
    let msg = hdr.error.as_deref().unwrap_or(default_msg).to_string();
    match hdr.error_kind {
        Some(TransportErrorKind::Timeout) => anyhow::Error::new(CefTimeoutError(msg)),
        Some(TransportErrorKind::ProtocolMismatch) => anyhow::Error::new(CefProtocolError(msg)),
        Some(TransportErrorKind::ResponseTooLarge) => anyhow::Error::new(ResponseTooLargeError {
            limit_bytes: hdr.limit_bytes.unwrap_or(0),
        }),
        Some(kind) => anyhow::Error::new(TransportError::new(kind, msg)),
        None => anyhow::Error::new(TransportError::new(TransportErrorKind::Network, msg)),
    }
}

async fn send_daemon_rpc(
    state_dir: &Path,
    req: &DaemonRequest,
    io_timeout: Duration,
    idle_timeout: Duration,
    verbose: bool,
    daemon_executable: Option<&Path>,
) -> Result<(DaemonResponseHeader, Vec<u8>)> {
    let sock = socket_path(state_dir);
    let (hdr, body) = match try_send_once(&sock, req, io_timeout).await {
        Ok(res) => res,
        Err(err) if is_retryable_pre_response_error(&err) => {
            if !is_daemon_running(state_dir) {
                spawn_detached_daemon(state_dir, idle_timeout, verbose, daemon_executable)?;
            }
            send_with_start_retry(&sock, req, io_timeout).await?
        }
        Err(err) => return Err(err),
    };

    if hdr.protocol_version != PROTOCOL_VERSION || hdr.build_id != BUILD_ID {
        let _ = daemon_stop(state_dir).await;
        spawn_detached_daemon(state_dir, idle_timeout, verbose, daemon_executable)?;
        let (restarted_hdr, restarted_body) = send_with_start_retry(&sock, req, io_timeout).await?;
        if restarted_hdr.protocol_version != PROTOCOL_VERSION || restarted_hdr.build_id != BUILD_ID
        {
            bail!(
                "daemon version mismatch after restart (got v{}/{}, expected v{}/{})",
                restarted_hdr.protocol_version,
                restarted_hdr.build_id,
                PROTOCOL_VERSION,
                BUILD_ID
            );
        }
        return Ok((restarted_hdr, restarted_body));
    }

    Ok((hdr, body))
}

async fn send_with_start_retry(
    sock: &Path,
    req: &DaemonRequest,
    io_timeout: Duration,
) -> Result<(DaemonResponseHeader, Vec<u8>)> {
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    loop {
        match try_send_once(sock, req, io_timeout).await {
            Ok(res) => return Ok(res),
            Err(err) if is_retryable_pre_response_error(&err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err).with_context(|| {
                        format!("timed out connecting to daemon socket {}", sock.display())
                    });
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub fn resolve_daemon_executable(override_exe: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_exe {
        if !path.as_os_str().is_empty() {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(path) = std::env::var_os("WICK_DAEMON_EXECUTABLE") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    if exe
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "deps")
    {
        bail!(
            "refusing to spawn Cargo test binary {} as wick daemon; set RuntimeConfig::daemon_executable or WICK_DAEMON_EXECUTABLE",
            exe.display()
        );
    }
    Ok(exe)
}

fn spawn_detached_daemon(
    state_dir: &Path,
    idle_timeout: Duration,
    verbose: bool,
    daemon_executable: Option<&Path>,
) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let exe = resolve_daemon_executable(daemon_executable)?;
    let idle_secs = idle_timeout.as_secs().max(1).to_string();

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path(state_dir))
        .context("failed to open daemon.log")?;

    let mut cmd = Command::new(exe);
    cmd.arg("--state-dir")
        .arg(state_dir)
        .arg("--daemon-idle-timeout")
        .arg(&idle_secs)
        .arg("daemon")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .process_group(0);

    if verbose {
        cmd.arg("--verbose");
    }

    cmd.spawn()
        .context("failed to spawn detached transport daemon")?;
    Ok(())
}

async fn wait_for_daemon_exit(state_dir: &Path, sock_path: &Path, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while (is_daemon_running(state_dir) || sock_path.exists())
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn try_send_once(
    sock_path: &Path,
    req: &DaemonRequest,
    timeout: Duration,
) -> Result<(DaemonResponseHeader, Vec<u8>)> {
    let fut = async {
        let stream = UnixStream::connect(sock_path).await?;
        let (read_half, mut write_half) = stream.into_split();

        let mut encoded = serde_json::to_string(req)?;
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half);
        let res = read_frame(&mut reader).await?;
        drop(write_half);
        Ok::<(DaemonResponseHeader, Vec<u8>), anyhow::Error>(res)
    };

    tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| CefTimeoutError(format!("daemon request timed out after {:?}", timeout)))?
}

fn is_retryable_pre_response_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        )
    })
}
