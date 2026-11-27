use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

pub const EXPECTED_PROTOCOL: &str = "v5-meta";
const PROTOCOL_MARKER_PREFIX: &[u8] = b"PLEB_RENDERER_PROTOCOL=";
const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;
const MAX_HTML_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct CefTimeoutError(pub String);

#[derive(Debug, Error)]
#[error("{0}")]
pub struct CefProtocolError(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RendererProbe {
    Ready(PathBuf),
    ProtocolMismatch {
        path: PathBuf,
        found: Option<String>,
        expected: &'static str,
    },
    NotInstalled,
}

/// Serializable status snapshot of the CEF renderer probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RendererStatus {
    pub status: &'static str,
    pub path: Option<String>,
    pub found_protocol: Option<String>,
    pub expected_protocol: &'static str,
}

impl RendererProbe {
    /// Converts this probe result into a structured JSON status payload.
    pub fn to_status(&self) -> RendererStatus {
        match self {
            Self::Ready(path) => RendererStatus {
                status: "ready",
                path: Some(path.display().to_string()),
                found_protocol: Some(EXPECTED_PROTOCOL.to_string()),
                expected_protocol: EXPECTED_PROTOCOL,
            },
            Self::ProtocolMismatch {
                path,
                found,
                expected,
            } => RendererStatus {
                status: "protocol_mismatch",
                path: Some(path.display().to_string()),
                found_protocol: found.clone(),
                expected_protocol: expected,
            },
            Self::NotInstalled => RendererStatus {
                status: "not_installed",
                path: None,
                found_protocol: None,
                expected_protocol: EXPECTED_PROTOCOL,
            },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CefRenderOptions {
    pub wait_for_selector: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefRenderResult {
    pub html: String,
    pub final_url: String,
    pub status_code: u16,
}

#[derive(Serialize)]
struct RendererRequest<'a> {
    url: &'a str,
    wait_for_selector: Option<&'a str>,
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct ReadyBanner {
    ready: bool,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct RendererResponseHeader {
    status_code: u16,
    #[serde(default)]
    final_url: String,
    #[serde(default)]
    html_bytes: usize,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_kind: Option<crate::transport::TransportErrorKind>,
}

struct DaemonProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

struct KillOnDrop(Option<Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Manages a persistent `pleb-renderer` CEF child process.
pub struct CefRenderer {
    state_dir: PathBuf,
    cache_dir: Option<PathBuf>,
    daemon_executable: Option<PathBuf>,
    renderer_executable: Option<PathBuf>,
    proxy: Option<String>,
    idle_timeout: Duration,
    use_daemon: bool,
    verbose: bool,
    daemon: AsyncMutex<Option<DaemonProcess>>,
}

impl CefRenderer {
    pub fn new(
        state_dir: PathBuf,
        proxy: Option<String>,
        idle_timeout: Duration,
        use_daemon: bool,
        verbose: bool,
    ) -> Self {
        Self {
            state_dir,
            cache_dir: None,
            daemon_executable: None,
            renderer_executable: None,
            proxy,
            idle_timeout,
            use_daemon,
            verbose,
            daemon: AsyncMutex::new(None),
        }
    }

    pub fn new_direct(state_dir: PathBuf, proxy: Option<String>, verbose: bool) -> Self {
        Self::new(state_dir, proxy, Duration::ZERO, false, verbose)
    }

    pub fn with_cache_dir(mut self, cache_dir: PathBuf) -> Self {
        self.cache_dir = Some(cache_dir);
        self
    }

    pub fn with_daemon_executable(mut self, daemon_executable: Option<PathBuf>) -> Self {
        self.daemon_executable = daemon_executable;
        self
    }

    pub fn with_renderer_executable(mut self, renderer_executable: Option<PathBuf>) -> Self {
        self.renderer_executable = renderer_executable;
        self
    }

    pub fn probe(&self) -> RendererProbe {
        probe_renderer_with_override(&self.state_dir, self.renderer_executable.as_deref())
    }

    pub fn is_available(&self) -> bool {
        !matches!(self.probe(), RendererProbe::NotInstalled)
    }

    pub fn renderer_path(&self) -> Option<PathBuf> {
        find_renderer_with_override(&self.state_dir, self.renderer_executable.as_deref())
    }

    pub fn has_live_process(&self) -> bool {
        match self.daemon.try_lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(proc) => matches!(proc.child.try_wait(), Ok(None)),
                None => false,
            },
            Err(_) => true,
        }
    }

    pub async fn render(
        &self,
        url: &str,
        opts: &CefRenderOptions,
        timeout: Duration,
    ) -> Result<CefRenderResult> {
        #[cfg(unix)]
        if self.use_daemon {
            return crate::daemon::daemon_cef_render_with_executable(
                &self.state_dir,
                self.proxy.as_deref(),
                timeout,
                self.idle_timeout,
                self.verbose,
                url,
                opts,
                self.daemon_executable.as_deref(),
            )
            .await;
        }

        self.render_direct(url, opts, timeout).await
    }

    async fn render_direct(
        &self,
        url: &str,
        opts: &CefRenderOptions,
        timeout: Duration,
    ) -> Result<CefRenderResult> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.daemon.lock().await;

        if let Some(ref mut proc) = *guard {
            if !matches!(proc.child.try_wait(), Ok(None)) {
                *guard = None;
            }
        }

        if guard.is_none() {
            let start_timeout = deadline.saturating_duration_since(Instant::now());
            if start_timeout.is_zero() {
                return Err(anyhow::Error::new(CefTimeoutError(format!(
                    "CEF render timed out before starting renderer for {}",
                    url
                ))));
            }
            let spawned = self.spawn_and_await_ready(start_timeout).await?;
            *guard = Some(spawned);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::Error::new(CefTimeoutError(format!(
                "CEF render timed out waiting for renderer startup for {}",
                url
            ))));
        }

        let proc = guard
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF renderer failed to initialize"))?;

        let DaemonProcess {
            child,
            mut stdin,
            mut stdout,
            stderr_tail,
        } = proc;

        let mut kill_guard = KillOnDrop(Some(child));
        let clean_url = url.to_string();
        let clean_selector = opts.wait_for_selector.clone();
        let timeout_ms = remaining.as_millis().min(u128::from(u64::MAX)) as u64;
        let url_for_err = clean_url.clone();

        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("pleb-cef-io".into())
            .spawn(move || {
                let res = perform_render(
                    &mut stdin,
                    &mut stdout,
                    &clean_url,
                    clean_selector.as_deref(),
                    timeout_ms,
                );
                let _ = tx.send((stdin, stdout, res));
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn CEF I/O thread: {}", e))?;

        match tokio::time::timeout(remaining, rx).await {
            Ok(Ok((stdin, stdout, Ok(page_res)))) => {
                let child = kill_guard.0.take().expect("child still owned");
                *guard = Some(DaemonProcess {
                    child,
                    stdin,
                    stdout,
                    stderr_tail,
                });
                page_res
            }
            Ok(Ok((_, _, Err(e)))) => {
                drop(kill_guard);
                Err(anyhow::anyhow!(append_stderr_tail(
                    e.to_string(),
                    &stderr_tail
                )))
            }
            Ok(Err(_)) => {
                drop(kill_guard);
                bail!(
                    "{}",
                    append_stderr_tail(
                        format!(
                            "CEF renderer I/O worker terminated unexpectedly for {}",
                            url_for_err
                        ),
                        &stderr_tail,
                    )
                )
            }
            Err(_) => {
                drop(kill_guard);
                let msg = append_stderr_tail(
                    format!(
                        "CEF render timed out after {:?} for {}",
                        timeout, url_for_err
                    ),
                    &stderr_tail,
                );
                Err(anyhow::Error::new(CefTimeoutError(msg)))
            }
        }
    }

    async fn spawn_and_await_ready(&self, timeout: Duration) -> Result<DaemonProcess> {
        let renderer_path = match self.probe() {
            RendererProbe::Ready(path) => path,
            RendererProbe::ProtocolMismatch {
                path,
                found,
                expected,
            } => {
                return Err(anyhow::Error::new(CefProtocolError(format!(
                    "pleb-renderer at {} has protocol {} (expected {}); rebuild with: {}",
                    path.display(),
                    found.as_deref().unwrap_or("unknown"),
                    expected,
                    rebuild_command_hint()
                ))));
            }
            RendererProbe::NotInstalled => {
                return Err(anyhow::Error::new(CefProtocolError(format!(
                    "pleb-renderer not found; install with: {}",
                    rebuild_command_hint()
                ))));
            }
        };

        let renderer_dir = renderer_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("renderer path has no parent directory"))?
            .to_path_buf();

        let mut cmd = Command::new(&renderer_path);
        crate::auth::scrub_sensitive_env(&mut cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PLEB_HOME", &self.state_dir);

        if let Some(ref cache_dir) = self.cache_dir {
            cmd.arg(format!("--cache-dir={}", cache_dir.display()));
        }

        if let Some(ref proxy) = self.proxy {
            let trimmed = proxy.trim();
            if !trimmed.is_empty() {
                cmd.arg(format!("--proxy-server={}", trimmed));
            }
        }

        #[cfg(target_os = "linux")]
        if std::env::var("DISPLAY").is_ok_and(|d| d.trim().is_empty()) {
            cmd.env_remove("DISPLAY");
        }

        let lib_path = match std::env::var("LD_LIBRARY_PATH") {
            Ok(existing) => {
                #[cfg(target_os = "linux")]
                {
                    let mut parts = Vec::new();
                    if std::env::var("DISPLAY").is_ok_and(|d| !d.trim().is_empty()) {
                        if let Some(multiarch) = linux_multiarch_lib_dir() {
                            parts.push(multiarch.display().to_string());
                        }
                    }
                    parts.push(renderer_dir.display().to_string());
                    parts.push(existing);
                    parts.join(":")
                }
                #[cfg(not(target_os = "linux"))]
                format!("{}:{}", renderer_dir.display(), existing)
            }
            Err(_) => renderer_dir.display().to_string(),
        };
        cmd.env("LD_LIBRARY_PATH", &lib_path);

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("failed to start CEF renderer {:?}: {}", renderer_path, e)
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF renderer stdin unavailable"))?;
        let mut stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("CEF renderer stdout unavailable"))?,
        );
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF renderer stderr unavailable"))?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        spawn_stderr_drain(stderr, Arc::clone(&stderr_tail), self.verbose);

        let mut kill_guard = KillOnDrop(Some(child));
        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("pleb-cef-ready".into())
            .spawn(move || {
                let res = read_ready_banner(&mut stdout);
                let _ = tx.send((stdout, res));
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn CEF ready reader: {}", e))?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok((stdout, Ok(())))) => {
                let child = kill_guard.0.take().expect("child still owned");
                Ok(DaemonProcess {
                    child,
                    stdin,
                    stdout,
                    stderr_tail,
                })
            }
            Ok(Ok((_, Err(e)))) => {
                drop(kill_guard);
                Err(anyhow::anyhow!(append_stderr_tail(
                    e.to_string(),
                    &stderr_tail
                )))
            }
            Ok(Err(_)) => {
                drop(kill_guard);
                bail!(
                    "{}",
                    append_stderr_tail(
                        "CEF renderer exited before ready handshake".to_string(),
                        &stderr_tail,
                    )
                )
            }
            Err(_) => {
                drop(kill_guard);
                let msg = append_stderr_tail(
                    format!("CEF renderer ready handshake timed out after {:?}", timeout),
                    &stderr_tail,
                );
                Err(anyhow::Error::new(CefTimeoutError(msg)))
            }
        }
    }
}

impl Drop for CefRenderer {
    fn drop(&mut self) {
        if let Some(mut proc) = self.daemon.get_mut().take() {
            // Close stdin first so pleb-renderer observes EOF and runs cef_shutdown().
            drop(proc.stdin);
            let deadline = Instant::now() + Duration::from_millis(300);
            loop {
                match proc.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => break,
                }
            }
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        }
    }
}

fn read_ready_banner(stdout: &mut BufReader<ChildStdout>) -> Result<()> {
    let mut line = String::new();
    let n = stdout.read_line(&mut line)?;
    if n == 0 {
        bail!("pleb-renderer closed stdout before sending ready handshake");
    }
    let banner: ReadyBanner = serde_json::from_str(line.trim()).map_err(|e| {
        anyhow::anyhow!(
            "invalid pleb-renderer ready banner '{}': {}",
            line.trim(),
            e
        )
    })?;
    if banner.protocol != EXPECTED_PROTOCOL {
        return Err(anyhow::Error::new(CefProtocolError(format!(
            "pleb-renderer reported protocol '{}' (expected {}); rebuild with: {}",
            banner.protocol,
            EXPECTED_PROTOCOL,
            rebuild_command_hint()
        ))));
    }
    if !banner.ready {
        bail!(
            "pleb-renderer failed startup: {}",
            banner
                .error
                .unwrap_or_else(|| "unknown initialization error".to_string())
        );
    }
    Ok(())
}

/// Returns `Err` on unrecoverable framing/IO failure (caller kills child) and
/// `Ok(Err(...))` when framing stayed intact and the child remains reusable.
fn perform_render(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    url: &str,
    wait_for_selector: Option<&str>,
    timeout_ms: u64,
) -> Result<Result<CefRenderResult>> {
    let req = RendererRequest {
        url,
        wait_for_selector: wait_for_selector.filter(|s| !s.trim().is_empty()),
        timeout_ms: timeout_ms.max(1),
    };
    let encoded = serde_json::to_string(&req)?;
    writeln!(stdin, "{}", encoded)?;
    stdin.flush()?;

    let mut header_line = String::new();
    let n = stdout.read_line(&mut header_line)?;
    if n == 0 {
        bail!("pleb-renderer closed stdout without a response");
    }

    let header: RendererResponseHeader = serde_json::from_str(header_line.trim())
        .map_err(|e| anyhow::anyhow!("bad CEF response header '{}': {}", header_line.trim(), e))?;

    if header.html_bytes > MAX_HTML_BYTES {
        bail!(
            "pleb-renderer response exceeded {} byte limit ({} bytes)",
            MAX_HTML_BYTES,
            header.html_bytes
        );
    }

    let mut html_buf = vec![0u8; header.html_bytes];
    if header.html_bytes > 0 {
        stdout.read_exact(&mut html_buf)?;
    }

    if let Some(err_msg) = header.error {
        let formatted = format!("pleb-renderer failed for {}: {}", url, err_msg);
        let kind = header
            .error_kind
            .unwrap_or(crate::transport::TransportErrorKind::Network);
        return Ok(Err(match kind {
            crate::transport::TransportErrorKind::Timeout => {
                anyhow::Error::new(CefTimeoutError(formatted))
            }
            crate::transport::TransportErrorKind::ProtocolMismatch => {
                anyhow::Error::new(CefProtocolError(formatted))
            }
            other => anyhow::Error::new(crate::transport::TransportError::new(other, formatted)),
        }));
    }

    let final_url = if header.final_url.is_empty() {
        url.to_string()
    } else {
        header.final_url
    };

    Ok(Ok(CefRenderResult {
        html: String::from_utf8_lossy(&html_buf).into_owned(),
        final_url,
        status_code: header.status_code,
    }))
}

fn spawn_stderr_drain(
    mut stderr: std::process::ChildStderr,
    tail: Arc<Mutex<Vec<u8>>>,
    verbose: bool,
) {
    let _ = std::thread::Builder::new()
        .name("pleb-cef-stderr".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if verbose {
                            let _ = std::io::stderr().write_all(&buf[..n]);
                        }
                        if let Ok(mut guard) = tail.lock() {
                            guard.extend_from_slice(&buf[..n]);
                            if guard.len() > STDERR_TAIL_MAX_BYTES {
                                let excess = guard.len() - STDERR_TAIL_MAX_BYTES;
                                guard.drain(..excess);
                            }
                        }
                    }
                }
            }
        });
}

fn append_stderr_tail(msg: String, tail: &Arc<Mutex<Vec<u8>>>) -> String {
    let stderr = tail
        .lock()
        .map(|buf| String::from_utf8_lossy(&buf).trim().to_string())
        .unwrap_or_default();
    if stderr.is_empty() {
        msg
    } else {
        format!("{}\nRenderer stderr:\n{}", msg, stderr)
    }
}

pub fn rebuild_command_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "bash scripts/install-cef-mac.sh --force"
    } else {
        "bash scripts/install-cef-linux.sh --force"
    }
}

pub fn probe_renderer_in(state_dir: &Path) -> RendererProbe {
    probe_renderer_with_override(state_dir, None)
}

pub fn probe_renderer_with_override(
    state_dir: &Path,
    renderer_executable: Option<&Path>,
) -> RendererProbe {
    match find_renderer_with_override(state_dir, renderer_executable) {
        Some(path) => probe_renderer_binary(&path),
        None => RendererProbe::NotInstalled,
    }
}

pub fn probe_renderer_binary(path: &Path) -> RendererProbe {
    let Ok(bytes) = std::fs::read(path) else {
        return RendererProbe::NotInstalled;
    };

    let found = extract_protocol_version(&bytes);
    match found {
        Some(ref ver) if ver == EXPECTED_PROTOCOL => RendererProbe::Ready(path.to_path_buf()),
        other => RendererProbe::ProtocolMismatch {
            path: path.to_path_buf(),
            found: other,
            expected: EXPECTED_PROTOCOL,
        },
    }
}

fn extract_protocol_version(bytes: &[u8]) -> Option<String> {
    bytes
        .windows(PROTOCOL_MARKER_PREFIX.len())
        .position(|w| w == PROTOCOL_MARKER_PREFIX)
        .and_then(|pos| {
            let tail = &bytes[pos + PROTOCOL_MARKER_PREFIX.len()..];
            let end = tail
                .iter()
                .position(|&b| {
                    b == 0
                        || b == b'\n'
                        || b == b'\r'
                        || b == b'"'
                        || b == b'\''
                        || b.is_ascii_whitespace()
                })
                .unwrap_or(tail.len());
            let ver = std::str::from_utf8(&tail[..end]).ok()?.trim();
            (!ver.is_empty()).then(|| ver.to_string())
        })
}

fn is_no_home_cef_enabled() -> bool {
    std::env::var("PLEB_NO_HOME_CEF")
        .is_ok_and(|val| val == "1" || val.eq_ignore_ascii_case("true"))
}

pub fn find_renderer_in(state_dir: &Path) -> Option<PathBuf> {
    find_renderer_with_override(state_dir, None)
}

pub fn find_renderer_with_override(
    state_dir: &Path,
    renderer_executable: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(explicit) = renderer_executable {
        if !explicit.as_os_str().is_empty() {
            return explicit.exists().then(|| explicit.to_path_buf());
        }
    }

    if let Some(env_path) = std::env::var_os("PLEB_CEF_RENDERER") {
        if !env_path.is_empty() {
            let path = PathBuf::from(env_path);
            return path.exists().then_some(path);
        }
    }

    let no_home_cef = is_no_home_cef_enabled();
    let mut candidates = Vec::new();

    if !no_home_cef {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("pleb-renderer.app/Contents/MacOS/pleb-renderer"));
                candidates.push(dir.join("pleb-renderer"));
            }
        }
    }

    let state_cef = state_dir.join("cef");
    candidates.push(state_cef.join("pleb-renderer.app/Contents/MacOS/pleb-renderer"));
    candidates.push(state_cef.join("pleb-renderer"));

    if !no_home_cef {
        if let Some(home) = std::env::var_os("HOME") {
            let pleb_cef = PathBuf::from(home).join(".pleb").join("cef");
            candidates.push(pleb_cef.join("pleb-renderer.app/Contents/MacOS/pleb-renderer"));
            candidates.push(pleb_cef.join("pleb-renderer"));
        }

        candidates.push(PathBuf::from("/opt/pleb/pleb-renderer"));
    }

    candidates.into_iter().find(|path| path.exists())
}

#[cfg(target_os = "linux")]
fn linux_multiarch_lib_dir() -> Option<PathBuf> {
    if let Ok(output) = Command::new("dpkg-architecture")
        .arg("-qDEB_HOST_MULTIARCH")
        .output()
    {
        if output.status.success() {
            let triplet = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !triplet.is_empty() {
                let path = PathBuf::from(format!("/usr/lib/{}", triplet));
                if path.is_dir() {
                    return Some(path);
                }
            }
        }
    }

    let entries = std::fs::read_dir("/usr/lib").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with("-linux-gnu") {
                    return Some(path);
                }
            }
        }
    }
    None
}
