use anyhow::{bail, Result};
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

use super::cronet_ffi as ffi;

const READ_BUFFER_SIZE: u64 = 32 * 1024;

#[derive(Clone, Copy)]
struct RequestHandle(ffi::Cronet_UrlRequestPtr);

unsafe impl Send for RequestHandle {}
unsafe impl Sync for RequestHandle {}

struct CancelGuard {
    active_request: Arc<Mutex<Option<RequestHandle>>>,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Ok(guard) = self.active_request.lock() {
            if let Some(handle) = *guard {
                unsafe {
                    ffi::Cronet_UrlRequest_Cancel(handle.0);
                }
            }
        }
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
        std::fs::create_dir_all(storage_path)?;

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
            })
        }
    }

    pub async fn get(&self, url: &str, headers: &[(&str, String)]) -> Result<CronetResponse> {
        let (tx, rx) = oneshot::channel();
        let active_request = Arc::new(Mutex::new(None));
        let guard = CancelGuard {
            active_request: Arc::clone(&active_request),
        };

        self.start_request(url, headers, tx, active_request)?;

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
        tx: oneshot::Sender<Result<CronetResponse>>,
        active_request: Arc<Mutex<Option<RequestHandle>>>,
    ) -> Result<()> {
        let state = Box::new(RequestState {
            body: Mutex::new(Vec::new()),
            status_code: AtomicI32::new(0),
            final_url: Mutex::new(url.to_string()),
            content_type: Mutex::new(None),
            sender: Mutex::new(Some(tx)),
            request: Mutex::new(std::ptr::null_mut()),
            callback: Mutex::new(std::ptr::null_mut()),
            active_request,
        });
        let state_ptr = Box::into_raw(state) as *mut c_void;
        let c_url = CString::new(url)?;

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
            let method = CString::new("GET").unwrap();
            ffi::Cronet_UrlRequestParams_http_method_set(params, method.as_ptr());

            for (name, value) in headers {
                let h = ffi::Cronet_HttpHeader_Create();
                let cn = CString::new(*name)?;
                let cv = CString::new(value.as_str())?;
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
                let _ = Box::from_raw(state_ptr as *mut RequestState);
                ffi::Cronet_UrlRequest_Destroy(request);
                ffi::Cronet_UrlRequestCallback_Destroy(callback);
                bail!("Cronet request init failed: result={}", result);
            }

            let state_ref = &*(state_ptr as *const RequestState);
            *state_ref.request.lock().unwrap() = request;
            *state_ref.callback.lock().unwrap() = callback;
            *state_ref.active_request.lock().unwrap() = Some(RequestHandle(request));

            let start_result = ffi::Cronet_UrlRequest_Start(request);
            if start_result != ffi::RESULT_SUCCESS {
                *state_ref.active_request.lock().unwrap() = None;
                let _ = Box::from_raw(state_ptr as *mut RequestState);
                ffi::Cronet_UrlRequest_Destroy(request);
                ffi::Cronet_UrlRequestCallback_Destroy(callback);
                bail!("Cronet request start failed: result={}", start_result);
            }

            Ok(())
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            ffi::Cronet_Engine_Shutdown(self.engine);
            ffi::Cronet_Engine_Destroy(self.engine);
            ffi::Cronet_Executor_Destroy(self.executor);
        }
    }
}

struct RequestState {
    body: Mutex<Vec<u8>>,
    status_code: AtomicI32,
    final_url: Mutex<String>,
    content_type: Mutex<Option<String>>,
    sender: Mutex<Option<oneshot::Sender<Result<CronetResponse>>>>,
    request: Mutex<ffi::Cronet_UrlRequestPtr>,
    callback: Mutex<ffi::Cronet_UrlRequestCallbackPtr>,
    active_request: Arc<Mutex<Option<RequestHandle>>>,
}

fn get_state(callback: ffi::Cronet_UrlRequestCallbackPtr) -> &'static RequestState {
    unsafe {
        let ctx = ffi::Cronet_UrlRequestCallback_GetClientContext(callback);
        &*(ctx as *const RequestState)
    }
}

fn complete(state: &RequestState, result: Result<CronetResponse>) {
    if let Some(tx) = state.sender.lock().unwrap().take() {
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
            *state.final_url.lock().unwrap() = final_url;
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
                *state.content_type.lock().unwrap() = Some(value);
                break;
            }
        }
    }

    let buffer = ffi::Cronet_Buffer_Create();
    ffi::Cronet_Buffer_InitWithAlloc(buffer, READ_BUFFER_SIZE);
    ffi::Cronet_UrlRequest_Read(request, buffer);
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
        let data_ptr = ffi::Cronet_Buffer_GetData(buffer) as *const u8;
        let slice = std::slice::from_raw_parts(data_ptr, bytes_read as usize);
        state.body.lock().unwrap().extend_from_slice(slice);
    }

    // Reuse the existing allocated buffer directly.
    ffi::Cronet_UrlRequest_Read(request, buffer);
}

unsafe fn finish_request(callback: ffi::Cronet_UrlRequestCallbackPtr) -> Box<RequestState> {
    let ctx = ffi::Cronet_UrlRequestCallback_GetClientContext(callback);
    let boxed = Box::from_raw(ctx as *mut RequestState);

    // Clear active handle while holding lock so CancelGuard cannot use a destroyed request.
    let _ = boxed.active_request.lock().unwrap().take();

    let req = *boxed.request.lock().unwrap();
    let cb = *boxed.callback.lock().unwrap();
    if !req.is_null() {
        ffi::Cronet_UrlRequest_Destroy(req);
    }
    if !cb.is_null() {
        ffi::Cronet_UrlRequestCallback_Destroy(cb);
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
    let body = std::mem::take(&mut *boxed.body.lock().unwrap());
    let final_url = std::mem::take(&mut *boxed.final_url.lock().unwrap());
    let content_type = boxed.content_type.lock().unwrap().take();

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
    complete(&boxed, Err(anyhow::anyhow!("cronet request canceled")));
}
