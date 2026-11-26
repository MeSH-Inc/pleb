use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{watch, Mutex as AsyncMutex, Notify};
use tokio::task::JoinSet;

use super::protocol::{
    lock_path, read_bounded_line, socket_path, storage_key, write_frame, DaemonRequest,
    DaemonResponseHeader, MAX_HEADER_LINE_BYTES,
};
use crate::context::ensure_private_dir;
use crate::transport::cef::{CefProtocolError, CefRenderOptions, CefRenderer, CefTimeoutError};
use crate::transport::{ResponseTooLargeError, TransportClient};

const MAX_PROXY_SLOTS: usize = 4;
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

struct SocketCleanupGuard {
    sock_path: PathBuf,
}

impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

struct ActiveRequestGuard {
    counter: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ActiveRequestGuard {
    fn new(counter: Arc<AtomicUsize>, notify: Arc<Notify>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter, notify }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.notify.notify_one();
        }
    }
}

struct SlotLease {
    in_flight: Arc<AtomicUsize>,
}

impl SlotLease {
    fn acquire(in_flight: &Arc<AtomicUsize>) -> Self {
        in_flight.fetch_add(1, Ordering::AcqRel);
        Self {
            in_flight: Arc::clone(in_flight),
        }
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ProxySlot {
    client: Arc<TransportClient>,
    cef: Arc<AsyncMutex<CefRenderer>>,
    in_flight: Arc<AtomicUsize>,
    last_used: Mutex<Instant>,
}

#[derive(Default)]
struct SlotPool {
    slots: Mutex<HashMap<String, Arc<ProxySlot>>>,
}

impl SlotPool {
    fn acquire(
        &self,
        state_dir: &Path,
        proxy: Option<&str>,
        default_timeout: Duration,
        verbose: bool,
    ) -> Result<(Arc<ProxySlot>, SlotLease)> {
        let normalized_proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        let key = storage_key(normalized_proxy);

        let evicted = {
            let mut guard = self
                .slots
                .lock()
                .map_err(|_| anyhow::anyhow!("daemon slot pool lock poisoned"))?;

            if let Some(slot) = guard.get(&key) {
                if let Ok(mut ts) = slot.last_used.lock() {
                    *ts = Instant::now();
                }
                let lease = SlotLease::acquire(&slot.in_flight);
                return Ok((Arc::clone(slot), lease));
            }

            let mut evicted_slot = None;
            if guard.len() >= MAX_PROXY_SLOTS {
                let lru_key = guard
                    .iter()
                    .filter(|(_, s)| s.in_flight.load(Ordering::Acquire) == 0)
                    .min_by_key(|(_, s)| {
                        s.last_used
                            .lock()
                            .map(|ts| *ts)
                            .unwrap_or_else(|_| Instant::now())
                    })
                    .map(|(k, _)| k.clone());

                let Some(evict_key) = lru_key else {
                    bail!(
                        "all {} proxy transport slots are busy; retry after active requests finish",
                        MAX_PROXY_SLOTS
                    );
                };
                evicted_slot = guard.remove(&evict_key);
            }

            let data_dir = state_dir.join("data").join(&key);
            ensure_private_dir(&data_dir)?;
            let cef_cache_dir = state_dir.join("cef-cache").join(&key);
            let client = Arc::new(TransportClient::new_direct(
                &data_dir,
                normalized_proxy,
                default_timeout,
            )?);
            let cef = Arc::new(AsyncMutex::new(
                CefRenderer::new_direct(
                    state_dir.to_path_buf(),
                    normalized_proxy.map(str::to_string),
                    verbose,
                )
                .with_cache_dir(cef_cache_dir),
            ));
            let slot = Arc::new(ProxySlot {
                client,
                cef,
                in_flight: Arc::new(AtomicUsize::new(0)),
                last_used: Mutex::new(Instant::now()),
            });
            let lease = SlotLease::acquire(&slot.in_flight);
            guard.insert(key, Arc::clone(&slot));
            (slot, lease, evicted_slot)
        };

        drop(evicted.2);
        Ok((evicted.0, evicted.1))
    }

    fn stats(&self) -> (usize, usize) {
        let Ok(guard) = self.slots.lock() else {
            return (0, 0);
        };
        let cronet_slots = guard.len();
        let cef_alive = guard
            .values()
            .filter(|slot| {
                slot.cef
                    .try_lock()
                    .is_ok_and(|renderer| renderer.has_live_process())
            })
            .count();
        (cronet_slots, cef_alive)
    }
}

pub async fn stop_legacy_cef_daemon(state_dir: &Path) {
    let legacy_sock = state_dir.join("cef.sock");
    let legacy_lock = state_dir.join("cef.lock");
    if legacy_sock.exists() {
        if let Ok(Ok(mut stream)) =
            tokio::time::timeout(Duration::from_millis(500), UnixStream::connect(&legacy_sock)).await
        {
            let _ = stream.write_all(b"{\"op\":\"shutdown\"}\n").await;
            let _ = stream.flush().await;
            let mut sink = [0u8; 256];
            let _ = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut sink)).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while legacy_sock.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if legacy_sock.exists() {
            let _ = std::fs::remove_file(&legacy_sock);
        }
    }
    if legacy_lock.exists() {
        let _ = std::fs::remove_file(&legacy_lock);
    }
}

pub async fn run_daemon(state_dir: PathBuf, idle_timeout: Duration, verbose: bool) -> Result<()> {
    ensure_private_dir(&state_dir)?;
    stop_legacy_cef_daemon(&state_dir).await;

    let lp = lock_path(&state_dir);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lp)
        .with_context(|| format!("failed to open daemon lock file {}", lp.display()))?;

    let lock_deadline = tokio::time::Instant::now() + LOCK_ACQUIRE_TIMEOUT;
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= lock_deadline {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(err) => {
                return Err(err).context("failed to acquire daemon lock");
            }
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
        .with_context(|| format!("failed to bind daemon socket {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    let sock_guard = SocketCleanupGuard {
        sock_path: sock_path.clone(),
    };

    let pool = Arc::new(SlotPool::default());
    let active_requests = Arc::new(AtomicUsize::new(0));
    let idle_notify = Arc::new(Notify::new());
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::new();

    let idle_sleep = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle_sleep);

    loop {
        while tasks.try_join_next().is_some() {}

        let is_idle = active_requests.load(Ordering::Acquire) == 0;
        tokio::select! {
            _ = shutdown_signal() => {
                let _ = shutdown_tx.send(true);
                break;
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = idle_notify.notified() => {
                if active_requests.load(Ordering::Acquire) == 0 {
                    idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                }
            }
            _ = &mut idle_sleep, if is_idle => {
                break;
            }
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        if active_requests.load(Ordering::Acquire) == 0 {
                            idle_sleep.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                        }
                        let state_dir_c = state_dir.clone();
                        let pool_c = Arc::clone(&pool);
                        let active_c = Arc::clone(&active_requests);
                        let notify_c = Arc::clone(&idle_notify);
                        let shutdown_tx_c = shutdown_tx.clone();
                        tasks.spawn(async move {
                            if let Err(err) = handle_connection(
                                stream,
                                state_dir_c,
                                pool_c,
                                active_c,
                                notify_c,
                                shutdown_tx_c,
                                verbose,
                            )
                            .await
                            {
                                tracing::debug!("daemon connection ended with error: {}", err);
                            }
                        });
                    }
                    Err(err) => {
                        tracing::error!("daemon accept failed: {}", err);
                        break;
                    }
                }
            }
        }
    }

    // Unlink socket before dropping listener so clients see NotFound immediately.
    drop(sock_guard);
    drop(listener);

    let drain_fut = async {
        while tasks.join_next().await.is_some() {}
    };
    if tokio::time::timeout(DRAIN_TIMEOUT, drain_fut).await.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    drop(pool);
    drop(lock_file);
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    state_dir: PathBuf,
    pool: Arc<SlotPool>,
    active_requests: Arc<AtomicUsize>,
    idle_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    verbose: bool,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let line = match tokio::time::timeout(
        HEADER_READ_TIMEOUT,
        read_bounded_line(&mut reader, MAX_HEADER_LINE_BYTES),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(err)) => {
            let hdr = DaemonResponseHeader::err(
                format!("invalid daemon request frame: {}", err),
                Some("protocol"),
                None,
            );
            let _ = write_frame(&mut write_half, &hdr, &[]).await;
            return Ok(());
        }
        Err(_) => {
            let hdr = DaemonResponseHeader::err(
                "timed out reading daemon request",
                Some("timeout"),
                None,
            );
            let _ = write_frame(&mut write_half, &hdr, &[]).await;
            return Ok(());
        }
    };

    let req: DaemonRequest = match serde_json::from_str(line.trim()) {
        Ok(req) => req,
        Err(err) => {
            let hdr = DaemonResponseHeader::err(
                format!("invalid daemon request: {}", err),
                Some("protocol"),
                None,
            );
            let _ = write_frame(&mut write_half, &hdr, &[]).await;
            return Ok(());
        }
    };

    match req {
        DaemonRequest::Ping => {
            let (cronet_slots, cef_renderers_alive) = pool.stats();
            let mut hdr = DaemonResponseHeader::ok_empty();
            hdr.pid = Some(std::process::id());
            hdr.active_requests = Some(active_requests.load(Ordering::Acquire));
            hdr.cronet_slots = Some(cronet_slots);
            hdr.cef_renderers_alive = Some(cef_renderers_alive);
            write_frame(&mut write_half, &hdr, &[]).await?;
            Ok(())
        }
        DaemonRequest::Shutdown => {
            let hdr = DaemonResponseHeader::ok_empty();
            let _ = write_frame(&mut write_half, &hdr, &[]).await;
            let _ = shutdown_tx.send(true);
            Ok(())
        }
        DaemonRequest::HttpGet {
            url,
            proxy,
            timeout_ms,
            max_body_bytes,
        } => {
            let _active = ActiveRequestGuard::new(active_requests, idle_notify);
            let timeout = Duration::from_millis(timeout_ms.max(1));
            let (slot, _lease) = match pool.acquire(&state_dir, proxy.as_deref(), timeout, verbose)
            {
                Ok(pair) => pair,
                Err(err) => {
                    let hdr = DaemonResponseHeader::err(err.to_string(), None, None);
                    let _ = write_frame(&mut write_half, &hdr, &[]).await;
                    return Ok(());
                }
            };

            let get_fut = slot.client.get_with_limits(&url, timeout, max_body_bytes);
            tokio::select! {
                biased;
                _ = wait_for_client_disconnect(&mut reader) => {
                    return Ok(());
                }
                res = get_fut => {
                    match res {
                        Ok(resp) => {
                            let mut hdr = DaemonResponseHeader::ok_empty();
                            hdr.status_code = Some(resp.status);
                            hdr.final_url = Some(resp.final_url);
                            hdr.content_type = resp.content_type;
                            hdr.body_bytes = resp.body.len();
                            let _ = write_frame(&mut write_half, &hdr, &resp.body).await;
                        }
                        Err(err) => {
                            let hdr = if let Some(too_large) = err.downcast_ref::<ResponseTooLargeError>() {
                                DaemonResponseHeader::err(
                                    err.to_string(),
                                    Some("response_too_large"),
                                    Some(too_large.limit_bytes),
                                )
                            } else {
                                DaemonResponseHeader::err(err.to_string(), None, None)
                            };
                            let _ = write_frame(&mut write_half, &hdr, &[]).await;
                        }
                    }
                }
            }
            Ok(())
        }
        DaemonRequest::CefRender {
            url,
            wait_for_selector,
            proxy,
            timeout_ms,
        } => {
            let _active = ActiveRequestGuard::new(active_requests, idle_notify);
            let timeout = Duration::from_millis(timeout_ms.max(1));
            let deadline = tokio::time::Instant::now() + timeout;

            let (slot, _lease) = match pool.acquire(&state_dir, proxy.as_deref(), timeout, verbose)
            {
                Ok(pair) => pair,
                Err(err) => {
                    let hdr = DaemonResponseHeader::err(err.to_string(), None, None);
                    let _ = write_frame(&mut write_half, &hdr, &[]).await;
                    return Ok(());
                }
            };

            let renderer_guard = tokio::select! {
                biased;
                _ = wait_for_client_disconnect(&mut reader) => {
                    return Ok(());
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let hdr = DaemonResponseHeader::err(
                        format!("CEF render timed out waiting for browser slot after {:?}", timeout),
                        Some("timeout"),
                        None,
                    );
                    let _ = write_frame(&mut write_half, &hdr, &[]).await;
                    return Ok(());
                }
                guard = slot.cef.lock() => guard,
            };

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let hdr = DaemonResponseHeader::err(
                    format!("CEF render timed out after {:?}", timeout),
                    Some("timeout"),
                    None,
                );
                let _ = write_frame(&mut write_half, &hdr, &[]).await;
                return Ok(());
            }

            let opts = CefRenderOptions { wait_for_selector };
            match renderer_guard.render(&url, &opts, remaining).await {
                Ok(result) => {
                    let bytes = result.html.as_bytes();
                    let mut hdr = DaemonResponseHeader::ok_empty();
                    hdr.status_code = Some(result.status_code);
                    hdr.final_url = Some(result.final_url);
                    hdr.content_type = Some("text/html".to_string());
                    hdr.body_bytes = bytes.len();
                    let _ = write_frame(&mut write_half, &hdr, bytes).await;
                }
                Err(err) => {
                    let kind = if err.is::<CefTimeoutError>() {
                        Some("timeout")
                    } else if err.is::<CefProtocolError>() {
                        Some("protocol")
                    } else {
                        None
                    };
                    let hdr = DaemonResponseHeader::err(err.to_string(), kind, None);
                    let _ = write_frame(&mut write_half, &hdr, &[]).await;
                }
            }
            Ok(())
        }
    }
}

async fn wait_for_client_disconnect<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) {
    let mut buf = [0u8; 64];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

async fn shutdown_signal() {
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
