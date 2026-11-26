use anyhow::{bail, Result};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::oneshot;

use super::cronet_ffi as ffi;

const READ_BUFFER_SIZE: u64 = 32 * 1024;
const ENGINE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone, Copy)]
struct RequestHandle(ffi::Cronet_UrlRequestPtr);

unsafe impl Send for RequestHandle {}
unsafe impl Sync for RequestHandle {}

struct DeferredCleanup {
    request: ffi::Cronet_UrlRequestPtr,
    callback: ffi::Cronet_UrlRequestCallbackPtr,
}

unsafe impl Send for DeferredCleanup {}

struct InFlightGuard(Arc<(Mutex<usize>, Condvar)>);

impl InFlightGuard {
    fn new(in_flight: &Arc<(Mutex<usize>, Condvar)>) -> Self {
        let (lock, _) = &**in_flight;
        let mut count = lock_unpoisoned(lock);
        *count += 1;
        Self(Arc::clone(in_flight))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        let mut count = lock_unpoisoned(lock);
        *count = count.saturating_sub(1);
        if *count == 0 {
            cvar.notify_all();
        }
    }
}

#[derive(Default)]
struct ActiveRequestInner {
    handle: Option<RequestHandle>,
    cancel_thread: Option<std::thread::ThreadId>,
    deferred_destroy: Option<(DeferredCleanup, Option<InFlightGuard>)>,
}

struct CancelGuard {
    active_request: Arc<(Mutex<ActiveRequestInner>, Condvar)>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        cancel_active_request(&self.active_request);
    }
}

unsafe fn destroy_native_handles(cleanup: DeferredCleanup) {
    // Cronet_UrlRequest_Read transfers buffer ownership to Cronet's IOBufferWithCronet_Buffer,
    // which calls Cronet_Buffer_Destroy when URLRequest tears down.
    if !cleanup.request.is_null() {
        ffi::Cronet_UrlRequest_Destroy(cleanup.request);
    }
    if !cleanup.callback.is_null() {
        ffi::Cronet_UrlRequestCallback_Destroy(cleanup.callback);
    }
}

fn cancel_active_request(active_request: &Arc<(Mutex<ActiveRequestInner>, Condvar)>) {
    let (lock, cvar) = &**active_request;
    let handle = {
        let mut guard = lock_unpoisoned(lock);
        let Some(handle) = guard.handle.take() else {
            return;
        };
        guard.cancel_thread = Some(std::thread::current().id());
        handle
    };

    unsafe {
        ffi::Cronet_UrlRequest_Cancel(handle.0);
    }

    let deferred = {
        let mut guard = lock_unpoisoned(lock);
        guard.cancel_thread = None;
        let deferred = guard.deferred_destroy.take();
        cvar.notify_all();
        deferred
    };

    if let Some((cleanup, in_flight)) = deferred {
        unsafe {
            destroy_native_handles(cleanup);
        }
        drop(in_flight);
    }
}

pub struct CronetResponse {
    pub status_code: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Cronet-backed HTTP engine with Chrome-identical TLS/HTTP2/QUIC.
pub struct Engine {
    engine: ffi::Cronet_EnginePtr,
    executor: ffi::Cronet_ExecutorPtr,
    timeout: Duration,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn new(
        storage_path: &Path,
        user_agent: &str,
        proxy: Option<&str>,
        timeout: Duration,
    ) -> Result<Self> {
        crate::context::ensure_private_dir(storage_path)?;

        let ua = CString::new(user_agent)?;
        let sp = CString::new(storage_path.to_string_lossy().as_ref())?;

        unsafe {
            let params = ffi::Cronet_EngineParams_Create();
            ffi::Cronet_EngineParams_enable_http2_set(params, true);
            ffi::Cronet_EngineParams_enable_quic_set(params, true);
            ffi::Cronet_EngineParams_enable_brotli_set(params, true);
            ffi::Cronet_EngineParams_user_agent_set(params, ua.as_ptr());
            ffi::Cronet_EngineParams_storage_path_set(params, sp.as_ptr());
            ffi::Cronet_EngineParams_http_cache_mode_set(params, ffi::HTTP_CACHE_MODE_DISK);
            ffi::Cronet_EngineParams_http_cache_max_size_set(params, 50 * 1024 * 1024);
            ffi::Cronet_EngineParams_enable_check_result_set(params, false);

            if let Some(proxy_url) = proxy {
                let opts = serde_json::json!({ "proxy_rules": proxy_url }).to_string();
                let c_opts = CString::new(opts)?;
                ffi::Cronet_EngineParams_experimental_options_set(params, c_opts.as_ptr());
            }

            let engine = ffi::Cronet_Engine_Create();
            let result = ffi::Cronet_Engine_StartWithParams(engine, params);
            ffi::Cronet_EngineParams_Destroy(params);

            if result != ffi::RESULT_SUCCESS {
                ffi::Cronet_Engine_Destroy(engine);
                bail!("Cronet engine start failed: result={}", result);
            }

            let executor = ffi::Cronet_Executor_CreateWith(executor_execute);

            Ok(Self {
                engine,
                executor,
                timeout,
                in_flight: Arc::new((Mutex::new(0), Condvar::new())),
            })
        }
    }

    pub async fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_body_bytes: usize,
    ) -> Result<CronetResponse> {
        let (tx, rx) = oneshot::channel();
        let active_request = Arc::new((Mutex::new(ActiveRequestInner::default()), Condvar::new()));
        let guard = CancelGuard {
            active_request: Arc::clone(&active_request),
        };

        self.start_request(url, headers, max_body_bytes, tx, active_request)?;

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => {
                drop(guard);
                result
            }
            Ok(Err(_)) => {
                drop(guard);
                bail!("Cronet request cancelled")
            }
            Err(_) => {
                drop(guard);
                bail!("Cronet request timed out after {:?}", self.timeout)
            }
        }
    }

    fn start_request(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_body_bytes: usize,
        tx: oneshot::Sender<Result<CronetResponse>>,
        active_request: Arc<(Mutex<ActiveRequestInner>, Condvar)>,
    ) -> Result<()> {
        let c_url = CString::new(url)?;
        let c_method = CString::new("GET").expect("static GET has no NUL");
        let mut c_headers = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            c_headers.push((CString::new(*name)?, CString::new(value.as_str())?));
        }

        let in_flight_guard = InFlightGuard::new(&self.in_flight);
        let state = Box::new(RequestState {
            body: Mutex::new(Vec::new()),
            max_body_bytes,
            overflowed: AtomicBool::new(false),
            status_code: AtomicI32::new(0),
            final_url: Mutex::new(url.to_string()),
            content_type: Mutex::new(None),
            sender: Mutex::new(Some(tx)),
            request: Mutex::new(std::ptr::null_mut()),
            callback: Mutex::new(std::ptr::null_mut()),
            active_request,
            in_flight: Some(in_flight_guard),
        });
        let state_ptr = Box::into_raw(state) as *mut c_void;

        unsafe {
            let callback = ffi::Cronet_UrlRequestCallback_CreateWith(
                on_redirect_received,
                on_response_started,
                on_read_completed,
                on_succeeded,
                on_failed,
                on_canceled,
            );
            ffi::Cronet_UrlRequestCallback_SetClientContext(callback, state_ptr);

            let params = ffi::Cronet_UrlRequestParams_Create();
            ffi::Cronet_UrlRequestParams_http_method_set(params, c_method.as_ptr());

            for (cn, cv) in &c_headers {
                let h = ffi::Cronet_HttpHeader_Create();
                ffi::Cronet_HttpHeader_name_set(h, cn.as_ptr());
                ffi::Cronet_HttpHeader_value_set(h, cv.as_ptr());
                ffi::Cronet_UrlRequestParams_request_headers_add(params, h);
                ffi::Cronet_HttpHeader_Destroy(h);
            }

            let request = ffi::Cronet_UrlRequest_Create();
            let result = ffi::Cronet_UrlRequest_InitWithParams(
                request,
                self.engine,
                c_url.as_ptr(),
                params,
                callback,
                self.executor,
            );
            ffi::Cronet_UrlRequestParams_Destroy(params);

            if result != ffi::RESULT_SUCCESS {
                let boxed = Box::from_raw(state_ptr as *mut RequestState);
                ffi::Cronet_UrlRequest_Destroy(request);
                ffi::Cronet_UrlRequestCallback_Destroy(callback);
                drop(boxed);
                bail!("Cronet request init failed: result={}", result);
            }

            let state_ref = &*(state_ptr as *const RequestState);
            *lock_unpoisoned(&state_ref.request) = request;
            *lock_unpoisoned(&state_ref.callback) = callback;
            {
                let (lock, _) = &*state_ref.active_request;
                lock_unpoisoned(lock).handle = Some(RequestHandle(request));
            }

            let start_result = ffi::Cronet_UrlRequest_Start(request);
            if start_result != ffi::RESULT_SUCCESS {
                {
                    let (lock, _) = &*state_ref.active_request;
                    lock_unpoisoned(lock).handle = None;
                }
                let boxed = Box::from_raw(state_ptr as *mut RequestState);
                ffi::Cronet_UrlRequest_Destroy(request);
                ffi::Cronet_UrlRequestCallback_Destroy(callback);
                drop(boxed);
                bail!("Cronet request start failed: result={}", start_result);
            }

            Ok(())
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.in_flight;
        let guard = lock_unpoisoned(lock);
        let (guard, timeout_res) =
            match cvar.wait_timeout_while(guard, ENGINE_DRAIN_TIMEOUT, |count| *count > 0) {
                Ok(res) => res,
                Err(poisoned) => poisoned.into_inner(),
            };
        if timeout_res.timed_out() && *guard > 0 {
            tracing::warn!(
                "Cronet Engine dropped with {} in-flight request(s) still active",
                *guard
            );
        }
        drop(guard);

        unsafe {
            let shutdown_rc = ffi::Cronet_Engine_Shutdown(self.engine);
            if shutdown_rc != ffi::RESULT_SUCCESS {
                tracing::warn!("Cronet_Engine_Shutdown returned {}", shutdown_rc);
            }
            ffi::Cronet_Engine_Destroy(self.engine);
            ffi::Cronet_Executor_Destroy(self.executor);
        }
    }
}

struct RequestState {
    body: Mutex<Vec<u8>>,
    max_body_bytes: usize,
    overflowed: AtomicBool,
    status_code: AtomicI32,
    final_url: Mutex<String>,
    content_type: Mutex<Option<String>>,
    sender: Mutex<Option<oneshot::Sender<Result<CronetResponse>>>>,
    request: Mutex<ffi::Cronet_UrlRequestPtr>,
    callback: Mutex<ffi::Cronet_UrlRequestCallbackPtr>,
    active_request: Arc<(Mutex<ActiveRequestInner>, Condvar)>,
    in_flight: Option<InFlightGuard>,
}

fn get_state(callback: ffi::Cronet_UrlRequestCallbackPtr) -> &'static RequestState {
    unsafe {
        let ctx = ffi::Cronet_UrlRequestCallback_GetClientContext(callback);
        &*(ctx as *const RequestState)
    }
}

fn complete(state: &RequestState, result: Result<CronetResponse>) {
    if let Some(tx) = lock_unpoisoned(&state.sender).take() {
        let _ = tx.send(result);
    }
}

unsafe extern "C" fn executor_execute(
    _executor: ffi::Cronet_ExecutorPtr,
    command: ffi::Cronet_RunnablePtr,
) {
    ffi::Cronet_Runnable_Run(command);
    ffi::Cronet_Runnable_Destroy(command);
}

unsafe extern "C" fn on_redirect_received(
    _callback: ffi::Cronet_UrlRequestCallbackPtr,
    request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
    _new_location: ffi::Cronet_String,
) {
    ffi::Cronet_UrlRequest_FollowRedirect(request);
}

unsafe extern "C" fn on_response_started(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    request: ffi::Cronet_UrlRequestPtr,
    info: ffi::Cronet_UrlResponseInfoPtr,
) {
    let state = get_state(callback);
    let code = ffi::Cronet_UrlResponseInfo_http_status_code_get(info);
    state.status_code.store(code, Ordering::SeqCst);

    let url_ptr = ffi::Cronet_UrlResponseInfo_url_get(info);
    if !url_ptr.is_null() {
        let final_url = CStr::from_ptr(url_ptr).to_string_lossy().into_owned();
        if !final_url.is_empty() {
            *lock_unpoisoned(&state.final_url) = final_url;
        }
    }

    let header_count = ffi::Cronet_UrlResponseInfo_all_headers_list_size(info);
    for i in 0..header_count {
        let header = ffi::Cronet_UrlResponseInfo_all_headers_list_at(info, i);
        if header.is_null() {
            continue;
        }
        let name_ptr = ffi::Cronet_HttpHeader_name_get(header);
        let value_ptr = ffi::Cronet_HttpHeader_value_get(header);
        if !name_ptr.is_null() && !value_ptr.is_null() {
            let name = CStr::from_ptr(name_ptr).to_string_lossy();
            if name.eq_ignore_ascii_case("content-type") {
                let value = CStr::from_ptr(value_ptr).to_string_lossy().into_owned();
                *lock_unpoisoned(&state.content_type) = Some(value);
                break;
            }
        }
    }

    let buffer = ffi::Cronet_Buffer_Create();
    ffi::Cronet_Buffer_InitWithAlloc(buffer, READ_BUFFER_SIZE);
    if ffi::Cronet_UrlRequest_Read(request, buffer) != ffi::RESULT_SUCCESS {
        ffi::Cronet_Buffer_Destroy(buffer);
    }
}

unsafe extern "C" fn on_read_completed(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
    buffer: ffi::Cronet_BufferPtr,
    bytes_read: u64,
) {
    let state = get_state(callback);
    if bytes_read > 0 {
        let exceeded = {
            let mut body = lock_unpoisoned(&state.body);
            let next_len = body.len().saturating_add(bytes_read as usize);
            if next_len > state.max_body_bytes {
                true
            } else {
                let data_ptr = ffi::Cronet_Buffer_GetData(buffer) as *const u8;
                let slice = std::slice::from_raw_parts(data_ptr, bytes_read as usize);
                body.extend_from_slice(slice);
                false
            }
        };
        if exceeded {
            state.overflowed.store(true, Ordering::SeqCst);
            let active = Arc::clone(&state.active_request);
            ffi::Cronet_Buffer_Destroy(buffer);
            cancel_active_request(&active);
            return;
        }
    }

    // Reuse the existing allocated buffer directly; Cronet takes ownership on success.
    if ffi::Cronet_UrlRequest_Read(request, buffer) != ffi::RESULT_SUCCESS {
        ffi::Cronet_Buffer_Destroy(buffer);
    }
}

unsafe fn finish_request(callback: ffi::Cronet_UrlRequestCallbackPtr) -> Box<RequestState> {
    let ctx = ffi::Cronet_UrlRequestCallback_GetClientContext(callback);
    let mut boxed = Box::from_raw(ctx as *mut RequestState);

    let cleanup = DeferredCleanup {
        request: *lock_unpoisoned(&boxed.request),
        callback: *lock_unpoisoned(&boxed.callback),
    };

    let this_tid = std::thread::current().id();
    let immediate_cleanup = {
        let (lock, cvar) = &*boxed.active_request;
        let mut guard = lock_unpoisoned(lock);
        guard.handle = None;

        if guard.cancel_thread == Some(this_tid) {
            let in_flight = boxed.in_flight.take();
            guard.deferred_destroy = Some((cleanup, in_flight));
            None
        } else {
            while guard.cancel_thread.is_some() {
                guard = match cvar.wait(guard) {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            Some(cleanup)
        }
    };

    if let Some(cleanup) = immediate_cleanup {
        destroy_native_handles(cleanup);
    }

    boxed
}

unsafe extern "C" fn on_succeeded(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    _request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
) {
    let boxed = finish_request(callback);
    let code = boxed.status_code.load(Ordering::SeqCst);
    let body = std::mem::take(&mut *lock_unpoisoned(&boxed.body));
    let final_url = std::mem::take(&mut *lock_unpoisoned(&boxed.final_url));
    let content_type = lock_unpoisoned(&boxed.content_type).take();

    complete(
        &boxed,
        Ok(CronetResponse {
            status_code: code as u16,
            final_url,
            content_type,
            body,
        }),
    );
}

unsafe extern "C" fn on_failed(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    _request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
    error: ffi::Cronet_ErrorPtr,
) {
    let code_name = if error.is_null() {
        "ERROR_OTHER"
    } else {
        net_error_name(ffi::Cronet_Error_error_code_get(error))
    };
    let boxed = finish_request(callback);
    complete(
        &boxed,
        Err(anyhow::anyhow!("cronet request failed [{}]", code_name)),
    );
}

fn net_error_name(code: std::os::raw::c_int) -> &'static str {
    match code {
        0 => "ERROR_CALLBACK",
        1 => "ERROR_HOSTNAME_NOT_RESOLVED",
        2 => "ERROR_INTERNET_DISCONNECTED",
        3 => "ERROR_NETWORK_CHANGED",
        4 => "ERROR_TIMED_OUT",
        5 => "ERROR_CONNECTION_CLOSED",
        6 => "ERROR_CONNECTION_TIMED_OUT",
        7 => "ERROR_CONNECTION_REFUSED",
        8 => "ERROR_CONNECTION_RESET",
        9 => "ERROR_ADDRESS_UNREACHABLE",
        10 => "ERROR_QUIC_PROTOCOL_FAILED",
        _ => "ERROR_OTHER",
    }
}

unsafe extern "C" fn on_canceled(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    _request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
) {
    let boxed = finish_request(callback);
    let err = if boxed.overflowed.load(Ordering::SeqCst) {
        anyhow::anyhow!(
            "response body exceeded {} byte limit",
            boxed.max_body_bytes
        )
    } else {
        anyhow::anyhow!("cronet request canceled")
    };
    complete(&boxed, Err(err));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn temp_storage(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mesh-wick-cronet-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn spawn_edge_server() -> String {
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

                    match path {
                        "/large" => {
                            let body = vec![b'x'; 64 * 1024];
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                        }
                        "/slow" => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
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
    async fn cronet_enforces_max_body_bytes_and_handles_timeout_cancel_on_drop() {
        let base = spawn_edge_server().await;
        let storage = temp_storage("edge");

        let engine = Engine::new(&storage, "mesh-wick-test", None, Duration::from_millis(150))
            .expect("cronet engine should start");

        let err_large = engine
            .get(&format!("{base}/large"), &[], 1024)
            .await
            .err()
            .expect("64 KiB payload should exceed 1024 byte limit");
        assert!(
            err_large.to_string().contains("exceeded"),
            "unexpected overflow error: {err_large}"
        );

        let err_slow = engine
            .get(&format!("{base}/slow"), &[], 65536)
            .await
            .err()
            .expect("slow endpoint should time out");
        assert!(
            err_slow.to_string().contains("timed out"),
            "unexpected timeout error: {err_slow}"
        );

        drop(engine);
        let _ = std::fs::remove_dir_all(&storage);
    }
}
