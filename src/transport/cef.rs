use anyhow::{bail, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct CefRenderOptions {
    pub wait_for_selector: Option<String>,
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

/// Manages a persistent `wick-renderer` CEF daemon scoped to a runtime context.
pub struct CefRenderer {
    proxy: Option<String>,
    timeout: Duration,
    verbose: bool,
    daemon: AsyncMutex<Option<DaemonProcess>>,
}

impl CefRenderer {
    pub fn new(proxy: Option<String>, timeout: Duration, verbose: bool) -> Self {
        Self {
            proxy,
            timeout,
            verbose,
            daemon: AsyncMutex::new(None),
        }
    }

    pub fn is_available(&self) -> bool {
        find_renderer().is_some()
    }

    pub fn renderer_path(&self) -> Option<PathBuf> {
        find_renderer()
    }

    pub async fn render(&self, url: &str, opts: &CefRenderOptions) -> Result<String> {
        let mut guard = self.daemon.lock().await;
        self.ensure_daemon(&mut guard)?;

        let proc = guard
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF daemon failed to initialize"))?;

        let DaemonProcess {
            child,
            mut stdin,
            mut stdout,
            stderr_tail,
        } = proc;

        let mut kill_guard = KillOnDrop(Some(child));
        let clean_url = sanitize_line(url);
        let clean_selector = opts.wait_for_selector.as_deref().map(sanitize_line);
        let url_for_err = clean_url.clone();

        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("mesh-wick-cef-io".into())
            .spawn(move || {
                let res = perform_render(&mut stdin, &mut stdout, &clean_url, clean_selector.as_deref());
                let _ = tx.send((stdin, stdout, res));
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn CEF I/O thread: {}", e))?;

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok((stdin, stdout, Ok(html)))) => {
                let child = kill_guard.0.take().expect("child still owned");
                *guard = Some(DaemonProcess {
                    child,
                    stdin,
                    stdout,
                    stderr_tail,
                });
                Ok(html)
            }
            Ok(Ok((_, _, Err(e)))) => {
                drop(kill_guard);
                let tail = read_stderr_tail(&stderr_tail);
                if tail.is_empty() {
                    Err(e)
                } else {
                    Err(anyhow::anyhow!("{}\nRenderer stderr:\n{}", e, tail))
                }
            }
            Ok(Err(_)) => {
                drop(kill_guard);
                let tail = read_stderr_tail(&stderr_tail);
                if tail.is_empty() {
                    bail!("CEF renderer I/O worker terminated unexpectedly for {}", url_for_err)
                } else {
                    bail!(
                        "CEF renderer I/O worker terminated unexpectedly for {}\nRenderer stderr:\n{}",
                        url_for_err,
                        tail
                    )
                }
            }
            Err(_) => {
                // Kill child first so the blocked read in the worker thread observes EOF.
                drop(kill_guard);
                let tail = read_stderr_tail(&stderr_tail);
                if tail.is_empty() {
                    bail!("CEF render timed out after {:?} for {}", self.timeout, url_for_err)
                } else {
                    bail!(
                        "CEF render timed out after {:?} for {}\nRenderer stderr:\n{}",
                        self.timeout,
                        url_for_err,
                        tail
                    )
                }
            }
        }
    }

    fn ensure_daemon(&self, slot: &mut Option<DaemonProcess>) -> Result<()> {
        if let Some(ref mut proc) = *slot {
            match proc.child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(_)) | Err(_) => {
                    *slot = None;
                }
            }
        }

        let renderer_path = find_renderer().ok_or_else(|| anyhow::anyhow!("wick-renderer not found"))?;
        let renderer_dir = renderer_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("renderer path has no parent directory"))?
            .to_path_buf();

        let mut cmd = Command::new(&renderer_path);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("WICK_PROXY");

        if let Some(ref proxy) = self.proxy {
            let trimmed = proxy.trim();
            if !trimmed.is_empty() {
                cmd.arg(format!("--proxy-server={}", trimmed));
            }
        }

        #[cfg(target_os = "linux")]
        if std::env::var("DISPLAY").is_err() {
            if !std::path::Path::new("/tmp/.X99-lock").exists() {
                let _ = Command::new("Xvfb")
                    .args([":99", "-screen", "0", "1920x1080x24"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                std::thread::sleep(Duration::from_secs(1));
            }
            cmd.env("DISPLAY", ":99");
        }

        let lib_path = match std::env::var("LD_LIBRARY_PATH") {
            Ok(existing) => {
                #[cfg(target_os = "linux")]
                {
                    let mut parts = Vec::new();
                    if std::env::var("DISPLAY").is_ok()
                        || std::path::Path::new("/tmp/.X99-lock").exists()
                    {
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

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to start CEF renderer {:?}: {}", renderer_path, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF daemon stdin unavailable"))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("CEF daemon stdout unavailable"))?,
        );
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("CEF daemon stderr unavailable"))?;

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        spawn_stderr_drain(stderr, Arc::clone(&stderr_tail), self.verbose);

        *slot = Some(DaemonProcess {
            child,
            stdin,
            stdout,
            stderr_tail,
        });
        Ok(())
    }
}

impl Drop for CefRenderer {
    fn drop(&mut self) {
        if let Some(mut proc) = self.daemon.get_mut().take() {
            let _ = proc.child.kill();
            let _ = proc.child.wait();
        }
    }
}

fn perform_render(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    url: &str,
    wait_for_selector: Option<&str>,
) -> Result<String> {
    match wait_for_selector {
        Some(sel) if !sel.is_empty() => writeln!(stdin, "{}\t{}", url, sel)?,
        _ => writeln!(stdin, "{}", url)?,
    }
    stdin.flush()?;

    let mut len_line = String::new();
    stdout.read_line(&mut len_line)?;
    let byte_count: usize = len_line
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("bad CEF response header '{}': {}", len_line.trim(), e))?;

    if byte_count == 0 {
        bail!("wick-renderer returned 0 bytes for {}", url);
    }

    let mut html_buf = vec![0u8; byte_count];
    stdout.read_exact(&mut html_buf)?;

    Ok(String::from_utf8_lossy(&html_buf).into_owned())
}

fn sanitize_line(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\n' && c != '\r' && c != '\t')
        .collect()
}

fn spawn_stderr_drain(
    mut stderr: std::process::ChildStderr,
    tail: Arc<Mutex<Vec<u8>>>,
    verbose: bool,
) {
    let _ = std::thread::Builder::new()
        .name("mesh-wick-cef-stderr".into())
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

fn read_stderr_tail(tail: &Arc<Mutex<Vec<u8>>>) -> String {
    tail.lock()
        .map(|buf| String::from_utf8_lossy(&buf).trim().to_string())
        .unwrap_or_default()
}

pub fn find_renderer() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("wick-renderer.app/Contents/MacOS/wick-renderer"));
            candidates.push(dir.join("wick-renderer"));
        }
    }

    if let Some(home) = std::env::var_os("MESH_WICK_HOME") {
        let base = PathBuf::from(home).join("cef");
        candidates.push(base.join("wick-renderer.app/Contents/MacOS/wick-renderer"));
        candidates.push(base.join("wick-renderer"));
    }

    if let Some(home) = std::env::var_os("HOME") {
        let mesh_cef = PathBuf::from(&home).join(".mesh-wick").join("cef");
        candidates.push(mesh_cef.join("wick-renderer.app/Contents/MacOS/wick-renderer"));
        candidates.push(mesh_cef.join("wick-renderer"));

        let wick_cef = PathBuf::from(home).join(".wick").join("cef");
        candidates.push(wick_cef.join("wick-renderer.app/Contents/MacOS/wick-renderer"));
        candidates.push(wick_cef.join("wick-renderer"));
    }

    candidates.push(PathBuf::from("/opt/wick/wick-renderer"));

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
