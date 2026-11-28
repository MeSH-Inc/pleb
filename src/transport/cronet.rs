use anyhow::{bail, Result};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::oneshot;

use super::cronet_ffi as ffi;
use super::{TransportError, TransportErrorKind};

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
    pub link_header: Option<String>,
    pub location_header: Option<String>,
    pub diagnostics: super::DiagnosticHeaders,
    pub body: Vec<u8>,
}

/// Cronet-backed HTTP engine with Chrome-identical TLS/HTTP2/QUIC.
pub struct Engine {
    engine: ffi::Cronet_EnginePtr,
    executor: ffi::Cronet_ExecutorPtr,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn new(storage_path: &Path, user_agent: &str, proxy: Option<&str>) -> Result<Self> {
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
                in_flight: Arc::new((Mutex::new(0), Condvar::new())),
            })
        }
    }

    pub async fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
        timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<CronetResponse> {
        self.get_with_redirects(url, headers, timeout, max_body_bytes, false)
            .await
    }

    pub async fn get_with_redirects(
        &self,
        url: &str,
        headers: &[(&str, String)],
        timeout: Duration,
        max_body_bytes: usize,
        disable_redirects: bool,
    ) -> Result<CronetResponse> {
        let deadline = tokio::time::Instant::now() + timeout;
        for attempt in 0..=1 {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Timeout,
                    format!("Cronet request timed out after {:?}", timeout),
                )));
            }

            let (tx, rx) = oneshot::channel();
            let active_request =
                Arc::new((Mutex::new(ActiveRequestInner::default()), Condvar::new()));
            let guard = CancelGuard {
                active_request: Arc::clone(&active_request),
            };

            self.start_request(
                url,
                headers,
                max_body_bytes,
                disable_redirects,
                tx,
                active_request,
            )?;

            match tokio::time::timeout(remaining, rx).await {
                Ok(Ok(Ok(resp))) => {
                    drop(guard);
                    return Ok(resp);
                }
                Ok(Ok(Err(failure))) => {
                    drop(guard);
                    let remaining_after =
                        deadline.saturating_duration_since(tokio::time::Instant::now());
                    if attempt == 0
                        && failure.retryable
                        && remaining_after >= Duration::from_millis(250)
                    {
                        tracing::debug!(
                            url,
                            error = %failure.err,
                            "retrying transient Cronet error once"
                        );
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                    return Err(failure.err);
                }
                Ok(Err(_)) => {
                    drop(guard);
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::Canceled,
                        "Cronet request canceled",
                    )));
                }
                Err(_) => {
                    drop(guard);
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::Timeout,
                        format!("Cronet request timed out after {:?}", timeout),
                    )));
                }
            }
        }
        unreachable!("attempt loop always returns within two attempts")
    }

    fn start_request(
        &self,
        url: &str,
        headers: &[(&str, String)],
        max_body_bytes: usize,
        disable_redirects: bool,
        tx: oneshot::Sender<std::result::Result<CronetResponse, CronetAttemptError>>,
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
            disable_redirects,
            overflowed: AtomicBool::new(false),
            blocked_redirect: Mutex::new(None),
            intercepted_redirect: Mutex::new(None),
            initial_url: url::Url::parse(url).ok(),
            status_code: AtomicI32::new(0),
            final_url: Mutex::new(url.to_string()),
            content_type: Mutex::new(None),
            link_header: Mutex::new(None),
            location_header: Mutex::new(None),
            diagnostics: Mutex::new(super::DiagnosticHeaders::default()),
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

struct InterceptedRedirect {
    status_code: u16,
    final_url: String,
    content_type: Option<String>,
    link_header: Option<String>,
    location_header: Option<String>,
    diagnostics: super::DiagnosticHeaders,
}

struct CronetAttemptError {
    err: anyhow::Error,
    retryable: bool,
}

struct RequestState {
    body: Mutex<Vec<u8>>,
    max_body_bytes: usize,
    disable_redirects: bool,
    overflowed: AtomicBool,
    blocked_redirect: Mutex<Option<String>>,
    intercepted_redirect: Mutex<Option<InterceptedRedirect>>,
    initial_url: Option<url::Url>,
    status_code: AtomicI32,
    final_url: Mutex<String>,
    content_type: Mutex<Option<String>>,
    link_header: Mutex<Option<String>>,
    location_header: Mutex<Option<String>>,
    diagnostics: Mutex<super::DiagnosticHeaders>,
    sender: Mutex<Option<oneshot::Sender<std::result::Result<CronetResponse, CronetAttemptError>>>>,
    request: Mutex<ffi::Cronet_UrlRequestPtr>,
    callback: Mutex<ffi::Cronet_UrlRequestCallbackPtr>,
    active_request: Arc<(Mutex<ActiveRequestInner>, Condvar)>,
    in_flight: Option<InFlightGuard>,
}

struct ParsedCronetHeaders {
    content_type: Option<String>,
    link_header: Option<String>,
    location_header: Option<String>,
    diagnostics: super::DiagnosticHeaders,
}

unsafe fn parse_cronet_response_headers(
    info: ffi::Cronet_UrlResponseInfoPtr,
    capture_location: bool,
) -> ParsedCronetHeaders {
    let mut content_type: Option<String> = None;
    let mut link_values: Vec<String> = Vec::new();
    let mut location_header: Option<String> = None;
    let mut diagnostics = super::DiagnosticHeaders::default();
    if info.is_null() {
        return ParsedCronetHeaders {
            content_type,
            link_header: None,
            location_header,
            diagnostics,
        };
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
            let value = CStr::from_ptr(value_ptr).to_string_lossy();
            diagnostics.capture_header(&name, &value);
            if name.eq_ignore_ascii_case("content-type") && content_type.is_none() {
                content_type = Some(value.into_owned());
            } else if name.eq_ignore_ascii_case("link") {
                if !value.is_empty() {
                    link_values.push(value.into_owned());
                }
            } else if capture_location
                && name.eq_ignore_ascii_case("location")
                && location_header.is_none()
                && !value.is_empty()
            {
                location_header = Some(value.into_owned());
            }
        }
    }

    ParsedCronetHeaders {
        content_type,
        link_header: (!link_values.is_empty()).then(|| link_values.join(", ")),
        location_header,
        diagnostics,
    }
}

fn get_state(callback: ffi::Cronet_UrlRequestCallbackPtr) -> &'static RequestState {
    unsafe {
        let ctx = ffi::Cronet_UrlRequestCallback_GetClientContext(callback);
        &*(ctx as *const RequestState)
    }
}

fn complete(state: &RequestState, result: std::result::Result<CronetResponse, CronetAttemptError>) {
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
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    request: ffi::Cronet_UrlRequestPtr,
    info: ffi::Cronet_UrlResponseInfoPtr,
    new_location: ffi::Cronet_String,
) {
    let state = get_state(callback);
    let raw_loc = if new_location.is_null() {
        String::new()
    } else {
        CStr::from_ptr(new_location).to_string_lossy().into_owned()
    };

    if state.disable_redirects {
        let status_code = if info.is_null() {
            302
        } else {
            let code = ffi::Cronet_UrlResponseInfo_http_status_code_get(info);
            if (300..400).contains(&code) {
                code as u16
            } else {
                302
            }
        };
        let final_url = if !info.is_null() {
            let url_ptr = ffi::Cronet_UrlResponseInfo_url_get(info);
            if !url_ptr.is_null() {
                let s = CStr::from_ptr(url_ptr).to_string_lossy().into_owned();
                if !s.is_empty() {
                    s
                } else {
                    lock_unpoisoned(&state.final_url).clone()
                }
            } else {
                lock_unpoisoned(&state.final_url).clone()
            }
        } else {
            lock_unpoisoned(&state.final_url).clone()
        };
        let headers = parse_cronet_response_headers(info, true);
        let location_header = (!raw_loc.is_empty())
            .then_some(raw_loc)
            .or(headers.location_header);
        *lock_unpoisoned(&state.intercepted_redirect) = Some(InterceptedRedirect {
            status_code,
            final_url,
            content_type: headers.content_type,
            link_header: headers.link_header,
            location_header,
            diagnostics: headers.diagnostics,
        });
        let active = Arc::clone(&state.active_request);
        cancel_active_request(&active);
        return;
    }

    let resolved = url::Url::parse(&raw_loc).ok().or_else(|| {
        let current = lock_unpoisoned(&state.final_url).clone();
        url::Url::parse(&current)
            .ok()
            .and_then(|u| u.join(&raw_loc).ok())
    });
    let allowed = match (&state.initial_url, &resolved) {
        (Some(origin), Some(target)) => crate::policy::is_safe_redirect_url(origin, target),
        _ => false,
    };
    if !allowed {
        let target_desc = resolved
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or(raw_loc);
        *lock_unpoisoned(&state.blocked_redirect) = Some(target_desc);
        let active = Arc::clone(&state.active_request);
        cancel_active_request(&active);
        return;
    }
    if let Some(target) = resolved {
        *lock_unpoisoned(&state.final_url) = target.into();
    }
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

    let headers = parse_cronet_response_headers(info, (300..400).contains(&code));
    if headers.content_type.is_some() {
        *lock_unpoisoned(&state.content_type) = headers.content_type;
    }
    if headers.link_header.is_some() {
        *lock_unpoisoned(&state.link_header) = headers.link_header;
    }
    if headers.location_header.is_some() {
        *lock_unpoisoned(&state.location_header) = headers.location_header;
    }
    if !headers.diagnostics.is_empty() {
        *lock_unpoisoned(&state.diagnostics) = headers.diagnostics;
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
    let link_header = lock_unpoisoned(&boxed.link_header).take();
    let location_header = lock_unpoisoned(&boxed.location_header).take();
    let diagnostics = std::mem::take(&mut *lock_unpoisoned(&boxed.diagnostics));

    complete(
        &boxed,
        Ok(CronetResponse {
            status_code: code as u16,
            final_url,
            content_type,
            link_header,
            location_header,
            diagnostics,
            body,
        }),
    );
}

fn sanitize_cronet_error_detail(raw: &str) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !out.is_empty() && !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
        if out.len() >= 256 {
            while out.len() > 256 {
                out.pop();
            }
            break;
        }
    }
    out.trim_end().to_string()
}

fn format_cronet_error_message(
    code_name: &str,
    internal_code: i32,
    quic_code: i32,
    raw_message: Option<&str>,
    retryable: bool,
) -> String {
    let mut out = format!("cronet request failed [{code_name}]");
    let mut meta = Vec::new();
    if internal_code != 0 {
        meta.push(format!("net_error={internal_code}"));
    }
    if quic_code != 0 {
        meta.push(format!("quic_error={quic_code}"));
    }
    if retryable {
        meta.push("retryable=true".to_string());
    }
    if !meta.is_empty() {
        out.push_str(" (");
        out.push_str(&meta.join(", "));
        out.push(')');
    }
    if let Some(raw) = raw_message {
        let detail = sanitize_cronet_error_detail(raw);
        if !detail.is_empty() {
            out.push_str(": ");
            out.push_str(&detail);
        }
    }
    out
}

fn is_retryable_cronet_error(
    code: std::os::raw::c_int,
    internal_code: i32,
    immediately_retryable: bool,
) -> bool {
    immediately_retryable
        || matches!(code, 5 | 8 | 10)
        || matches!(internal_code, -337 | -324 | -103)
}

unsafe extern "C" fn on_failed(
    callback: ffi::Cronet_UrlRequestCallbackPtr,
    _request: ffi::Cronet_UrlRequestPtr,
    _info: ffi::Cronet_UrlResponseInfoPtr,
    error: ffi::Cronet_ErrorPtr,
) {
    let (msg, kind, retryable_err) = if error.is_null() {
        (
            format_cronet_error_message("ERROR_OTHER", 0, 0, None, false),
            TransportErrorKind::Network,
            false,
        )
    } else {
        let code = ffi::Cronet_Error_error_code_get(error);
        let internal_code = ffi::Cronet_Error_internal_error_code_get(error);
        let quic_code = ffi::Cronet_Error_quic_detailed_error_code_get(error);
        let immediately_retryable = ffi::Cronet_Error_immediately_retryable_get(error);
        let msg_ptr = ffi::Cronet_Error_message_get(error);
        let raw_message =
            (!msg_ptr.is_null()).then(|| CStr::from_ptr(msg_ptr).to_string_lossy().into_owned());
        let kind = match code {
            4 | 6 => TransportErrorKind::Timeout,
            _ => TransportErrorKind::Network,
        };
        let is_retryable = is_retryable_cronet_error(code, internal_code, immediately_retryable);
        (
            format_cronet_error_message(
                net_error_name(code),
                internal_code,
                quic_code,
                raw_message.as_deref(),
                immediately_retryable,
            ),
            kind,
            is_retryable,
        )
    };
    let boxed = finish_request(callback);
    complete(
        &boxed,
        Err(CronetAttemptError {
            err: anyhow::Error::new(TransportError::new(kind, msg)),
            retryable: retryable_err,
        }),
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
    let result = if let Some(redir) = lock_unpoisoned(&boxed.intercepted_redirect).take() {
        Ok(CronetResponse {
            status_code: redir.status_code,
            final_url: redir.final_url,
            content_type: redir.content_type,
            link_header: redir.link_header,
            location_header: redir.location_header,
            diagnostics: redir.diagnostics,
            body: Vec::new(),
        })
    } else if let Some(target) = lock_unpoisoned(&boxed.blocked_redirect).take() {
        Err(CronetAttemptError {
            err: anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                format!("blocked unsafe redirect to {target}"),
            )),
            retryable: false,
        })
    } else if boxed.overflowed.load(Ordering::SeqCst) {
        Err(CronetAttemptError {
            err: anyhow::Error::new(super::ResponseTooLargeError {
                limit_bytes: boxed.max_body_bytes,
            }),
            retryable: false,
        })
    } else {
        Err(CronetAttemptError {
            err: anyhow::Error::new(TransportError::new(
                TransportErrorKind::Canceled,
                "cronet request canceled",
            )),
            retryable: false,
        })
    };
    complete(&boxed, result);
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
            "pleb-cronet-{}-{}-{}",
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
        let drop_once_seen = Arc::new(AtomicBool::new(false));

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let drop_once_seen = Arc::clone(&drop_once_seen);
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
                        "/ok" => {
                            let body = b"redirect ok";
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(body).await;
                        }
                        "/drop-once" => {
                            if !drop_once_seen.swap(true, Ordering::SeqCst) {
                                drop(socket);
                                return;
                            }
                            let body = b"recovered after drop";
                            let hdr = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(hdr.as_bytes()).await;
                            let _ = socket.write_all(body).await;
                        }
                        "/drop-always" => {
                            drop(socket);
                        }
                        "/redir-ok" => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                        }
                        "/redir-imds" => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                        }
                        "/redir-localhost" => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://localhost:1/internal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
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

        let engine = Engine::new(&storage, "pleb-test", None).expect("cronet engine should start");

        let err_large = engine
            .get(&format!("{base}/large"), &[], Duration::from_secs(5), 1024)
            .await
            .err()
            .expect("64 KiB payload should exceed 1024 byte limit");
        assert!(
            err_large.is::<crate::transport::ResponseTooLargeError>(),
            "expected ResponseTooLargeError, got: {err_large}"
        );

        let err_slow = engine
            .get(
                &format!("{base}/slow"),
                &[],
                Duration::from_millis(150),
                65536,
            )
            .await
            .err()
            .expect("slow endpoint should time out");
        let slow_te = err_slow
            .downcast_ref::<TransportError>()
            .expect("slow endpoint error must be a structured TransportError");
        assert_eq!(slow_te.kind, TransportErrorKind::Timeout);

        let ok_redir = engine
            .get(
                &format!("{base}/redir-ok"),
                &[],
                Duration::from_secs(5),
                65536,
            )
            .await
            .expect("same-host local redirect should succeed");
        assert_eq!(ok_redir.status_code, 200);
        assert_eq!(ok_redir.body, b"redirect ok");

        for path in ["/redir-imds", "/redir-localhost"] {
            let err = engine
                .get(&format!("{base}{path}"), &[], Duration::from_secs(5), 65536)
                .await
                .err()
                .unwrap_or_else(|| panic!("{path} redirect must be blocked"));
            let te = err
                .downcast_ref::<TransportError>()
                .expect("blocked redirect must return a structured TransportError");
            assert_eq!(te.kind, TransportErrorKind::RedirectBlocked);
        }

        let recovered = engine
            .get(
                &format!("{base}/drop-once"),
                &[],
                Duration::from_secs(5),
                65536,
            )
            .await
            .expect("transient connection drop should recover on 1x retry");
        assert_eq!(recovered.status_code, 200);
        assert_eq!(recovered.body, b"recovered after drop");

        let err_drop = engine
            .get(
                &format!("{base}/drop-always"),
                &[],
                Duration::from_secs(5),
                65536,
            )
            .await
            .err()
            .expect("persistent drop should fail with TransportError");
        let drop_te = err_drop
            .downcast_ref::<TransportError>()
            .expect("persistent drop must return TransportError");
        assert_eq!(drop_te.kind, TransportErrorKind::Network);
        assert!(
            drop_te.message.contains("cronet request failed [")
                && drop_te.message.contains("net_error=-"),
            "expected enriched Cronet error details, got: {}",
            drop_te.message
        );

        drop(engine);
        let _ = std::fs::remove_dir_all(&storage);
    }

    #[test]
    fn cronet_error_formatting_and_retry_classification() {
        assert!(is_retryable_cronet_error(11, -337, false));
        assert!(is_retryable_cronet_error(5, 0, false));
        assert!(is_retryable_cronet_error(11, 0, true));
        assert!(!is_retryable_cronet_error(1, -105, false));

        let msg = format_cronet_error_message(
            "ERROR_OTHER",
            -337,
            0,
            Some("net::ERR_HTTP2_PROTOCOL_ERROR\n\textra"),
            false,
        );
        assert_eq!(
            msg,
            "cronet request failed [ERROR_OTHER] (net_error=-337): net::ERR_HTTP2_PROTOCOL_ERROR extra"
        );
    }
}
