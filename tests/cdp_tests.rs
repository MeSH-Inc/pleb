use futures_util::{SinkExt, StreamExt};
use pleb::auth::{set_credential, CredentialProvider, CredentialStorage};
use pleb::context::{RuntimeConfig, RuntimeContext};
use pleb::fetch::{fetch_target, FetchError, FetchOptions, FetchTarget};
use pleb::policy::{BlockReason, RenderMode, ResourcePolicy};
use pleb::transport::cdp::{
    sanitize_cdp_url_for_display, validate_cdp_url, validate_loopback_page_cdp_url,
    CdpRenderOptions, CdpRenderer, MAX_UNSOLICITED_EVENTS,
};
use pleb::transport::{transport_error_kind, ResponseTooLargeError, TransportErrorKind};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{}", std::process::id(), seq));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone)]
struct RecordedRpc {
    method: String,
    session_id: Option<String>,
    params: Value,
}

#[derive(Default)]
struct ServerLog {
    request_uris: Vec<String>,
    request_headers: Vec<HashMap<String, String>>,
    rpcs: Vec<RecordedRpc>,
    received_close_frame: bool,
}

type RpcHandler = Arc<
    dyn Fn(i64, &str, &Option<String>, &Value, Arc<Mutex<ServerLog>>) -> Vec<Message> + Send + Sync,
>;

async fn wait_for_close_frame(log: &Arc<Mutex<ServerLog>>) {
    for _ in 0..50 {
        if log.lock().unwrap().received_close_frame {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn spawn_mock_cdp_server(handler: RpcHandler) -> (String, u16, Arc<Mutex<ServerLog>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();
    let log = Arc::new(Mutex::new(ServerLog::default()));
    let log_bg = Arc::clone(&log);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let log_conn = Arc::clone(&log_bg);

            tokio::spawn(async move {
                let log_hdr = Arc::clone(&log_conn);
                #[allow(clippy::result_large_err)]
                let on_headers = |req: &Request, resp: Response| {
                    let mut guard = log_hdr.lock().unwrap();
                    guard.request_uris.push(req.uri().to_string());
                    let mut headers = HashMap::new();
                    for (name, val) in req.headers() {
                        if let Ok(s) = val.to_str() {
                            headers.insert(name.as_str().to_ascii_lowercase(), s.to_string());
                        }
                    }
                    guard.request_headers.push(headers);
                    Ok(resp)
                };
                let ws_res = tokio_tungstenite::accept_hdr_async(stream, on_headers).await;

                let Ok(mut ws) = ws_res else {
                    return;
                };

                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            let Ok(val) = serde_json::from_str::<Value>(text.as_str()) else {
                                continue;
                            };
                            let id = val["id"].as_i64().unwrap_or(0);
                            let method = val["method"].as_str().unwrap_or("").to_string();
                            let session_id = val["sessionId"].as_str().map(|s| s.to_string());
                            let params = val["params"].clone();

                            log_conn.lock().unwrap().rpcs.push(RecordedRpc {
                                method: method.clone(),
                                session_id: session_id.clone(),
                                params: params.clone(),
                            });

                            let replies =
                                handler(id, &method, &session_id, &params, Arc::clone(&log_conn));
                            for reply in replies {
                                if ws.send(reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            log_conn.lock().unwrap().received_close_frame = true;
                            let _ = ws.send(Message::Close(None)).await;
                            break;
                        }
                        Ok(Message::Ping(payload)) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Err(_) => break,
                        _ => {}
                    }
                }
            });
        }
    });

    (format!("ws://127.0.0.1:{port}"), port, log)
}

fn json_msg(val: Value) -> Message {
    Message::Text(val.to_string().into())
}

fn rpc_ok(id: i64, session_id: &Option<String>, result: Value) -> Message {
    let mut obj = json!({
        "id": id,
        "result": result,
    });
    if let Some(sid) = session_id {
        obj["sessionId"] = json!(sid);
    }
    json_msg(obj)
}

fn rpc_err(id: i64, session_id: &Option<String>, code: i64, message: &str) -> Message {
    let mut obj = json!({
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    });
    if let Some(sid) = session_id {
        obj["sessionId"] = json!(sid);
    }
    json_msg(obj)
}

fn rpc_event(session_id: Option<&str>, method: &str, params: Value) -> Message {
    let mut obj = json!({
        "method": method,
        "params": params,
    });
    if let Some(sid) = session_id {
        obj["sessionId"] = json!(sid);
    }
    json_msg(obj)
}

fn basic_handler(html: String) -> RpcHandler {
    let target_url_state = Arc::new(Mutex::new("about:blank".to_string()));
    let page_state_calls = Arc::new(AtomicU64::new(0));

    Arc::new(move |id, method, session_id, params, _log| {
        let html = html.clone();
        let url_state = Arc::clone(&target_url_state);
        let ps_calls = Arc::clone(&page_state_calls);
        match method {
            "Target.createTarget" => {
                vec![rpc_ok(id, session_id, json!({ "targetId": "tgt-1" }))]
            }
            "Target.attachToTarget" => {
                vec![rpc_ok(id, session_id, json!({ "sessionId": "sess-1" }))]
            }
            "Target.closeTarget" => vec![rpc_ok(id, session_id, json!({ "success": true }))],
            "Page.enable" | "Network.enable" | "Fetch.enable" | "WebMCP.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Fetch.continueRequest" | "Fetch.failRequest" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "main-frame" } } }),
            )],
            "Page.navigate" => {
                let nav_url = params["url"]
                    .as_str()
                    .unwrap_or("https://example.com/")
                    .to_string();
                *url_state.lock().unwrap() = nav_url.clone();
                let sid = session_id.as_deref();
                vec![
                    rpc_event(
                        sid,
                        "Network.requestWillBeSent",
                        json!({
                            "requestId": "req-main",
                            "frameId": "main-frame",
                            "type": "Document",
                            "request": { "url": nav_url },
                        }),
                    ),
                    rpc_event(
                        sid,
                        "Network.responseReceived",
                        json!({
                            "requestId": "req-main",
                            "frameId": "main-frame",
                            "type": "Document",
                            "response": {
                                "url": "https://example.com/final",
                                "status": 200,
                                "mimeType": "text/html",
                                "headers": {
                                    "Content-Type": "text/html; charset=utf-8",
                                    "Link": "</first.css>; rel=\"stylesheet\"\n</final.md>; rel=\"alternate\"; type=\"text/markdown\""
                                }
                            }
                        }),
                    ),
                    rpc_event(
                        sid,
                        "Page.frameNavigated",
                        json!({
                            "frame": {
                                "id": "main-frame",
                                "url": "https://example.com/final",
                                "urlFragment": "#section-2"
                            }
                        }),
                    ),
                    rpc_event(
                        sid,
                        "Page.navigatedWithinDocument",
                        json!({
                            "frameId": "main-frame",
                            "url": "https://example.com/final-spa#section-2"
                        }),
                    ),
                    rpc_ok(
                        id,
                        session_id,
                        json!({ "frameId": "main-frame", "loaderId": "req-main" }),
                    ),
                    rpc_event(sid, "Page.loadEventFired", json!({ "timestamp": 1.0 })),
                ]
            }
            "Kitesurf.getPageState" => {
                let n = ps_calls.fetch_add(1, Ordering::Relaxed);
                let seq = if n == 0 { 1 } else { 2 };
                vec![rpc_ok(id, session_id, json!({ "seq": seq }))]
            }
            "DOM.getDocument" => {
                vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 42 } }))]
            }
            "DOM.getOuterHTML" => vec![rpc_ok(id, session_id, json!({ "outerHTML": html }))],
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    })
}

#[tokio::test]
async fn cdp_browser_mode_layout_off_domain_init_and_target_teardown() {
    let dir = TempDir::new();
    let html =
        "<html><head><title>Kitesurf</title></head><body><main>Rendered</main></body></html>"
            .to_string();
    let (base_ws, _port, log) = spawn_mock_cdp_server(basic_handler(html.clone())).await;

    let raw_cdp_url = format!("{base_ws}/devtools/browser?token=secret-tok");
    let renderer = CdpRenderer::new(raw_cdp_url.clone(), dir.path().to_path_buf(), None);
    assert_eq!(
        renderer.cdp_url_display(),
        format!("{base_ws}/devtools/browser")
    );
    assert!(!format!("{renderer:?}").contains("secret-tok"));
    assert_eq!(
        sanitize_cdp_url_for_display(&raw_cdp_url),
        format!("{base_ws}/devtools/browser")
    );

    let res = renderer
        .render(
            "https://example.com/start",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    assert_eq!(res.html, html);
    assert_eq!(res.status_code, 200);
    assert_eq!(res.final_url, "https://example.com/final-spa#section-2");
    assert_eq!(
        res.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert!(res
        .link_header
        .as_deref()
        .is_some_and(|l| l.contains("/final.md") && l.contains(", ")));

    wait_for_close_frame(&log).await;
    let guard = log.lock().unwrap();
    assert_eq!(
        guard.request_uris[0],
        "/devtools/browser?token=secret-tok&layout=off"
    );
    assert!(guard.received_close_frame);

    let methods: Vec<&str> = guard.rpcs.iter().map(|r| r.method.as_str()).collect();
    assert!(!methods.contains(&"DOM.enable"));
    assert!(!methods.contains(&"Browser.close"));
    assert_eq!(
        methods[..9],
        [
            "Target.createTarget",
            "Target.attachToTarget",
            "Page.enable",
            "Network.enable",
            "Page.getFrameTree",
            "Fetch.enable",
            "WebMCP.enable",
            "Page.navigate",
            "Kitesurf.getPageState",
        ]
    );
    assert_eq!(methods.last().copied(), Some("Target.closeTarget"));

    let ps_rpc = guard
        .rpcs
        .iter()
        .find(|r| r.method == "Kitesurf.getPageState")
        .unwrap();
    assert_eq!(ps_rpc.params["targetId"], "tgt-1");
}

#[tokio::test]
async fn cdp_page_endpoint_preserves_tab_and_uses_set_layout_mode() {
    let dir = TempDir::new();
    let page_handler: RpcHandler = Arc::new(|id, method, session_id, _params, _log| {
        match method {
            "Kitesurf.setLayoutMode"
            | "Page.enable"
            | "Network.enable"
            | "Fetch.enable"
            | "WebMCP.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "page-frame" } } }),
            )],
            "Page.navigate" => vec![
                rpc_event(
                    None,
                    "Network.responseReceived",
                    json!({
                        "requestId": "req-1",
                        "frameId": "page-frame",
                        "type": "Document",
                        "response": {
                            "url": "https://example.com/page",
                            "status": 200,
                            "headers": { "content-type": "text/html" }
                        }
                    }),
                ),
                rpc_ok(id, session_id, json!({ "frameId": "page-frame" })),
                rpc_event(None, "Page.loadEventFired", json!({ "timestamp": 1.0 })),
            ],
            "DOM.getDocument" => {
                vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 7 } }))]
            }
            "DOM.getOuterHTML" => vec![rpc_ok(
                id,
                session_id,
                json!({ "outerHTML": "<html><body><p>Page Mode</p></body></html>" }),
            )],
            // Kitesurf.getPageState returns -32601 -> triggers 250ms fallback cleanly.
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(page_handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/page/tab-abc"),
        dir.path().to_path_buf(),
        None,
    );

    let res = renderer
        .render(
            "https://example.com/page",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(res.status_code, 200);
    assert!(res.html.contains("Page Mode"));

    wait_for_close_frame(&log).await;
    let guard = log.lock().unwrap();
    assert_eq!(guard.request_uris[0], "/devtools/page/tab-abc");
    assert!(guard.received_close_frame);

    let methods: Vec<&str> = guard.rpcs.iter().map(|r| r.method.as_str()).collect();
    assert!(methods.contains(&"Kitesurf.setLayoutMode"));
    assert!(!methods.contains(&"Target.createTarget"));
    assert!(!methods.contains(&"Target.attachToTarget"));
    assert!(!methods.contains(&"Target.closeTarget"));
    assert!(!methods.contains(&"Browser.close"));
    for rpc in &guard.rpcs {
        assert!(rpc.session_id.is_none());
    }
    let set_layout = guard
        .rpcs
        .iter()
        .find(|r| r.method == "Kitesurf.setLayoutMode")
        .unwrap();
    assert_eq!(set_layout.params["mode"], "off");
}

#[tokio::test]
async fn cdp_navigate_error_text_fails_fast_and_still_closes_owned_target() {
    let dir = TempDir::new();
    let fail_handler: RpcHandler = Arc::new(|id, method, session_id, _params, _log| match method {
        "Target.createTarget" => {
            vec![rpc_ok(id, session_id, json!({ "targetId": "tgt-fail" }))]
        }
        "Target.attachToTarget" => {
            vec![rpc_ok(id, session_id, json!({ "sessionId": "sess-fail" }))]
        }
        "Target.closeTarget" => vec![rpc_ok(id, session_id, json!({ "success": true }))],
        "Page.enable" | "Network.enable" | "Fetch.enable" | "WebMCP.enable" => {
            vec![rpc_ok(id, session_id, json!({}))]
        }
        "Page.getFrameTree" => vec![rpc_ok(
            id,
            session_id,
            json!({ "frameTree": { "frame": { "id": "mf" } } }),
        )],
        "Page.navigate" => vec![rpc_ok(
            id,
            session_id,
            json!({ "errorText": "net::ERR_NAME_NOT_RESOLVED" }),
        )],
        _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(fail_handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    );

    let err = renderer
        .render(
            "https://does-not-exist.invalid/",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&err),
        Some(TransportErrorKind::LoadError)
    );

    wait_for_close_frame(&log).await;
    let guard = log.lock().unwrap();
    let methods: Vec<&str> = guard.rpcs.iter().map(|r| r.method.as_str()).collect();
    assert!(methods.contains(&"Target.closeTarget"));
    assert!(guard.received_close_frame);
}

#[tokio::test]
async fn cdp_wait_for_selector_polling_and_error_classification() {
    let dir = TempDir::new();
    let polls = Arc::new(AtomicU64::new(0));
    let selector_handler: RpcHandler = Arc::new(move |id, method, session_id, params, _log| {
        let polls = Arc::clone(&polls);
        match method {
            "Target.createTarget" => vec![rpc_ok(id, session_id, json!({ "targetId": "t1" }))],
            "Target.attachToTarget" => {
                vec![rpc_ok(id, session_id, json!({ "sessionId": "s1" }))]
            }
            "Target.closeTarget" | "Page.enable" | "Network.enable" | "Fetch.enable"
            | "WebMCP.enable" => vec![rpc_ok(id, session_id, json!({}))],
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "mf" } } }),
            )],
            "Page.navigate" => vec![
                rpc_event(
                    session_id.as_deref(),
                    "Network.responseReceived",
                    json!({
                        "requestId": "r1",
                        "frameId": "mf",
                        "type": "Document",
                        "response": { "url": "https://example.com/", "status": 200, "headers": {} }
                    }),
                ),
                rpc_ok(id, session_id, json!({})),
                rpc_event(session_id.as_deref(), "Page.loadEventFired", json!({})),
            ],
            "DOM.getDocument" => {
                vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 99 } }))]
            }
            "DOM.querySelector" => {
                let sel = params["selector"].as_str().unwrap_or("");
                match sel {
                    ".kitesurf-bad" => vec![rpc_ok(
                        id,
                        session_id,
                        json!({ "nodeId": 0, "error": "SyntaxError: bad selector" }),
                    )],
                    ".chrome-bad" => vec![rpc_err(
                        id,
                        session_id,
                        -32000,
                        "DOM Error while querying selector",
                    )],
                    _ => {
                        let n = polls.fetch_add(1, Ordering::Relaxed);
                        if n == 0 {
                            vec![rpc_ok(id, session_id, json!({ "nodeId": 0 }))]
                        } else if n == 1 {
                            // Transient stale-node error must retry cleanly.
                            vec![rpc_err(
                                id,
                                session_id,
                                -32000,
                                "Node with given id not found",
                            )]
                        } else {
                            vec![rpc_ok(id, session_id, json!({ "nodeId": 123 }))]
                        }
                    }
                }
            }
            "DOM.getOuterHTML" => vec![rpc_ok(
                id,
                session_id,
                json!({ "outerHTML": "<html><body><div class='ready'>OK</div></body></html>" }),
            )],
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(selector_handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    );

    // 1. Pre-flight invalid syntax must fail before opening a WebSocket.
    let invalid_err = renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions {
                wait_for_selector: Some("div[".to_string()),
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&invalid_err),
        Some(TransportErrorKind::InvalidSelector)
    );
    assert!(log.lock().unwrap().request_uris.is_empty());

    // 2. Transient retry -> success.
    let ok = renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions {
                wait_for_selector: Some(".ready".to_string()),
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(ok.html.contains("class='ready'"));

    // 3. Kitesurf in-band result.error -> InvalidSelector.
    let k_err = renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions {
                wait_for_selector: Some(".kitesurf-bad".to_string()),
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&k_err),
        Some(TransportErrorKind::InvalidSelector)
    );

    // 4. Chrome -32000 "DOM Error while querying" -> InvalidSelector.
    let c_err = renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions {
                wait_for_selector: Some(".chrome-bad".to_string()),
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&c_err),
        Some(TransportErrorKind::InvalidSelector)
    );
}

#[tokio::test]
async fn cdp_fetch_interception_blocks_ssrf_and_self_ssrf_and_wss_local_targets() {
    let dir = TempDir::new();
    let ssrf_url = Arc::new(Mutex::new(
        "http://169.254.169.254/latest/meta-data".to_string(),
    ));
    let ssrf_url_bg = Arc::clone(&ssrf_url);

    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| {
        let ssrf_url = Arc::clone(&ssrf_url_bg);
        match method {
            "Target.createTarget" => {
                vec![rpc_ok(id, session_id, json!({ "targetId": "t-ssrf" }))]
            }
            "Target.attachToTarget" => {
                vec![rpc_ok(id, session_id, json!({ "sessionId": "s-ssrf" }))]
            }
            "Target.closeTarget"
            | "Page.enable"
            | "Network.enable"
            | "Fetch.enable"
            | "WebMCP.enable"
            | "Fetch.failRequest"
            | "Fetch.continueRequest" => vec![rpc_ok(id, session_id, json!({}))],
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "mf" } } }),
            )],
            "Page.navigate" => {
                let blocked = ssrf_url.lock().unwrap().clone();
                vec![
                    rpc_event(
                        session_id.as_deref(),
                        "Fetch.requestPaused",
                        json!({
                            "requestId": "pause-1",
                            "frameId": "mf",
                            "resourceType": "Document",
                            "request": { "url": blocked }
                        }),
                    ),
                    rpc_ok(
                        id,
                        session_id,
                        json!({ "errorText": "net::ERR_BLOCKED_BY_CLIENT" }),
                    ),
                ]
            }
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, cdp_port, log) = spawn_mock_cdp_server(handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    );

    // 1. Fetch.requestPaused targeting cloud IMDS fails closed via Fetch.failRequest(BlockedByClient).
    let imds_err = renderer
        .render(
            "https://example.com/start",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&imds_err),
        Some(TransportErrorKind::RedirectBlocked)
    );
    {
        let guard = log.lock().unwrap();
        let fail_rpc = guard
            .rpcs
            .iter()
            .find(|r| r.method == "Fetch.failRequest")
            .unwrap();
        assert_eq!(fail_rpc.params["errorReason"], "BlockedByClient");
        assert_eq!(fail_rpc.params["requestId"], "pause-1");
    }

    // 2. Fetch.requestPaused targeting self CDP port is blocked even when initial URL was public.
    *ssrf_url.lock().unwrap() = format!("http://127.0.0.1:{cdp_port}/devtools/browser");
    let self_pause_err = renderer
        .render(
            "https://example.com/start",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&self_pause_err),
        Some(TransportErrorKind::RedirectBlocked)
    );

    // 3. Direct self-SSRF target URL rejected before opening WS, even if allow_local_targets=true.
    let direct_self_err = renderer
        .render(
            &format!("http://127.0.0.1:{cdp_port}/devtools/version"),
            &CdpRenderOptions {
                allow_local_targets: true,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&direct_self_err),
        Some(TransportErrorKind::RedirectBlocked)
    );

    // 4. Remote wss:// endpoint refuses local/private targets pre-flight.
    let wss_renderer = CdpRenderer::new(
        "wss://cdp.example.com/devtools/browser",
        dir.path().to_path_buf(),
        None,
    );
    let wss_local_err = wss_renderer
        .render(
            "http://10.0.0.5/internal",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&wss_local_err),
        Some(TransportErrorKind::RedirectBlocked)
    );
}

#[tokio::test]
async fn cdp_webmcp_enable_supported_and_unsupported_edge_cases() {
    let dir = TempDir::new();
    let webmcp_supported = Arc::new(Mutex::new(true));
    let webmcp_flag = Arc::clone(&webmcp_supported);

    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| {
        let supported = *webmcp_flag.lock().unwrap();
        match method {
            "Target.createTarget" => vec![rpc_ok(id, session_id, json!({ "targetId": "t-w" }))],
            "Target.attachToTarget" => {
                vec![rpc_ok(id, session_id, json!({ "sessionId": "s-w" }))]
            }
            "Target.closeTarget" | "Page.enable" | "Network.enable" | "Fetch.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "WebMCP.enable" => {
                if supported {
                    vec![rpc_ok(id, session_id, json!({}))]
                } else {
                    vec![rpc_err(id, session_id, -32601, "WebMCP not available")]
                }
            }
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "mf" } } }),
            )],
            "Page.navigate" => {
                let sid = session_id.as_deref();
                let mut msgs = vec![rpc_event(
                    sid,
                    "Network.responseReceived",
                    json!({
                        "requestId": "rw",
                        "frameId": "mf",
                        "type": "Document",
                        "response": { "url": "https://example.com/tools", "status": 200, "headers": {} }
                    }),
                )];
                if supported {
                    msgs.push(rpc_event(
                            sid,
                            "WebMCP.toolsAdded",
                            json!({
                                "tools": [
                                    {
                                        "name": "decl_form",
                                        "description": "Declarative form from CDP must be excluded from imperative_tools",
                                        "frameId": "mf",
                                        "backendNodeId": 77
                                    },
                                    {
                                        "name": "foreign_tool",
                                        "description": "Subframe tool must be ignored",
                                        "frameId": "sub-frame",
                                    },
                                    {
                                        "name": "temp_tool",
                                        "description": "Removed tool",
                                        "frameId": "mf"
                                    },
                                    {
                                        "name": "search_docs",
                                        "description": "Search API docs\x1b[31m",
                                        "title": "Search",
                                        "frameId": "mf",
                                        "inputSchema": { "type": "object" },
                                        "annotations": { "readOnly": true }
                                    }
                                ]
                            }),
                        ));
                    msgs.push(rpc_event(
                        sid,
                        "WebMCP.toolsRemoved",
                        json!({
                            "tools": [
                                { "name": "temp_tool", "frameId": "mf" },
                                { "name": "search_docs", "frameId": "sub-frame" }
                            ]
                        }),
                    ));
                }
                msgs.push(rpc_ok(id, session_id, json!({})));
                msgs.push(rpc_event(sid, "Page.loadEventFired", json!({})));
                msgs
            }
            "DOM.getDocument" => {
                vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 5 } }))]
            }
            "DOM.getOuterHTML" => vec![rpc_ok(
                id,
                session_id,
                json!({ "outerHTML": "<html><body><main>Tools page</main></body></html>" }),
            )],
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, _port, _log) = spawn_mock_cdp_server(handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    );

    let res = renderer
        .render(
            "https://example.com/tools",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(res.imperative_tools.len(), 1);
    let tool = &res.imperative_tools[0];
    assert_eq!(tool.name, "search_docs");
    assert_eq!(tool.source, pleb::extract::WebMcpToolSource::Imperative);
    assert_eq!(
        tool.annotations.as_ref().and_then(|a| a.untrusted_content),
        Some(true)
    );
    assert_eq!(
        tool.annotations.as_ref().and_then(|a| a.read_only),
        Some(true)
    );

    // Unsupported WebMCP (-32601) succeeds cleanly with empty tools.
    *webmcp_supported.lock().unwrap() = false;
    let res2 = renderer
        .render(
            "https://example.com/tools",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(res2.imperative_tools.is_empty());
}

#[tokio::test]
async fn cdp_dom_get_outer_html_retry_max_bytes_axtree_and_flood_guard() {
    let dir = TempDir::new();
    let html_attempts = Arc::new(AtomicU64::new(0));
    let html_payload = Arc::new(Mutex::new(
        "<html><body><main>Recovered</main></body></html>".to_string(),
    ));
    let ax_mode = Arc::new(Mutex::new("budget_err"));
    let flood = Arc::new(Mutex::new(false));

    let html_attempts_bg = Arc::clone(&html_attempts);
    let html_payload_bg = Arc::clone(&html_payload);
    let ax_mode_bg = Arc::clone(&ax_mode);
    let flood_bg = Arc::clone(&flood);

    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| {
        let html_attempts = Arc::clone(&html_attempts_bg);
        let html_payload = Arc::clone(&html_payload_bg);
        let ax_mode = Arc::clone(&ax_mode_bg);
        let flood = Arc::clone(&flood_bg);
        match method {
            "Target.createTarget" => vec![rpc_ok(id, session_id, json!({ "targetId": "t-a" }))],
            "Target.attachToTarget" => {
                vec![rpc_ok(id, session_id, json!({ "sessionId": "s-a" }))]
            }
            "Target.closeTarget" | "Page.enable" | "Network.enable" | "Fetch.enable"
            | "WebMCP.enable" => vec![rpc_ok(id, session_id, json!({}))],
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": { "id": "mf" } } }),
            )],
            "Page.navigate" => {
                let sid = session_id.as_deref();
                if *flood.lock().unwrap() {
                    let mut out = Vec::with_capacity(MAX_UNSOLICITED_EVENTS + 5);
                    for i in 0..=(MAX_UNSOLICITED_EVENTS + 1) {
                        out.push(rpc_event(
                            sid,
                            "Network.requestWillBeSent",
                            json!({
                                "requestId": format!("spam-{i}"),
                                "request": { "url": "https://example.com/a" }
                            }),
                        ));
                    }
                    out.push(rpc_ok(id, session_id, json!({})));
                    out
                } else {
                    vec![
                        rpc_event(
                            sid,
                            "Network.responseReceived",
                            json!({
                                "requestId": "r-a",
                                "frameId": "mf",
                                "type": "Document",
                                "response": { "url": "https://example.com/ax", "status": 200, "headers": {} }
                            }),
                        ),
                        rpc_ok(id, session_id, json!({})),
                        rpc_event(sid, "Page.loadEventFired", json!({})),
                    ]
                }
            }
            "DOM.getDocument" => {
                vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 10 } }))]
            }
            "DOM.getOuterHTML" => {
                let attempt = html_attempts.fetch_add(1, Ordering::Relaxed);
                if attempt == 0 {
                    vec![rpc_err(id, session_id, -32000, "Node disappeared")]
                } else {
                    let body = html_payload.lock().unwrap().clone();
                    vec![rpc_ok(id, session_id, json!({ "outerHTML": body }))]
                }
            }
            "Accessibility.getFullAXTree" => {
                if *ax_mode.lock().unwrap() == "budget_err" {
                    vec![rpc_err(
                        id,
                        session_id,
                        -32000,
                        "AXTree exceeded 32 MiB budget",
                    )]
                } else {
                    vec![rpc_ok(
                        id,
                        session_id,
                        json!({
                            "nodes": [
                                {
                                    "nodeId": "1",
                                    "ignored": false,
                                    "role": { "value": "RootWebArea" },
                                    "name": { "value": "AX Title" },
                                    "childIds": []
                                }
                            ]
                        }),
                    )]
                }
            }
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    );

    // 1. Stale-node retry succeeds on second attempt.
    let res = renderer
        .render(
            "https://example.com/ax",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(res.html.contains("Recovered"));

    // 2. Oversized HTML exceeds max_response_bytes -> ResponseTooLargeError.
    *html_payload.lock().unwrap() = "x".repeat(512);
    let large_err = renderer
        .render(
            "https://example.com/ax",
            &CdpRenderOptions {
                max_response_bytes: 128,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(large_err.is::<ResponseTooLargeError>());

    // 3. capture_axtree omits layout=off and maps -32000 AXTree budget error to ResponseTooLargeError.
    *html_payload.lock().unwrap() = "<html><body>OK</body></html>".to_string();
    let ax_budget_err = renderer
        .render(
            "https://example.com/ax",
            &CdpRenderOptions {
                capture_axtree: true,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(ax_budget_err.is::<ResponseTooLargeError>());
    {
        let guard = log.lock().unwrap();
        let ax_uri = guard.request_uris.last().unwrap();
        assert!(!ax_uri.contains("layout=off"));
        assert!(guard
            .rpcs
            .iter()
            .any(|r| r.method == "Accessibility.getFullAXTree"));
    }

    // 4. capture_axtree sends Accessibility.getFullAXTree and passes once sibling axtree formatter merges.
    *ax_mode.lock().unwrap() = "ok";
    match renderer
        .render(
            "https://example.com/ax",
            &CdpRenderOptions {
                capture_axtree: true,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
    {
        Ok(ok_ax) => {
            assert!(ok_ax.axtree_text.as_deref().is_some_and(|t| !t.is_empty()));
        }
        Err(err) => {
            // Standalone wave-header pre-merge stub returns not yet implemented.
            assert!(
                err.to_string().contains("not yet implemented"),
                "unexpected AXTree error: {err}"
            );
        }
    }

    // 5. Event flood guard -> TransportErrorKind::ResponseTooLarge.
    *flood.lock().unwrap() = true;
    let flood_err = renderer
        .render(
            "https://example.com/ax",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&flood_err),
        Some(TransportErrorKind::ResponseTooLarge)
    );
}

#[tokio::test]
async fn cdp_proxy_non_101_sanitization_access_credentials_and_url_validation() {
    let dir = TempDir::new();

    // 1. Proxy configured refuses CDP before connecting.
    let proxy_renderer = CdpRenderer::new(
        "ws://127.0.0.1:9222/devtools/browser",
        dir.path().to_path_buf(),
        Some("http://127.0.0.1:8888".to_string()),
    );
    let proxy_err = proxy_renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&proxy_err),
        Some(TransportErrorKind::Network)
    );

    // 2. Non-101 HTTP upgrade omits response headers and body.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nX-Secret-Access: internal-secret\r\nContent-Length: 23\r\nConnection: close\r\n\r\nSENSITIVE_BODY_LEAK_123",
                )
                .await;
        }
    });

    let non_101_renderer = CdpRenderer::new(
        format!("ws://127.0.0.1:{}/devtools/browser", http_addr.port()),
        dir.path().to_path_buf(),
        None,
    );
    let non_101_err = non_101_renderer
        .render(
            "https://example.com/",
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    let msg = non_101_err.to_string();
    assert_eq!(
        transport_error_kind(&non_101_err),
        Some(TransportErrorKind::Network)
    );
    assert!(msg.contains("403"));
    assert!(!msg.contains("X-Secret-Access"));
    assert!(!msg.contains("internal-secret"));
    assert!(!msg.contains("SENSITIVE_BODY_LEAK_123"));

    // 3. ws:// remote host is rejected; ws:// loopback ignores Access credentials;
    //    wss:// with partial Access credentials fails loud with missing secret key.
    assert!(validate_cdp_url("ws://remote.example.com/devtools/browser").is_err());
    assert!(validate_cdp_url("http://127.0.0.1:9222/devtools/browser").is_err());

    set_credential(
        dir.path(),
        CredentialProvider::CdpAccessClientId,
        "cid-only",
        CredentialStorage::File,
    )
    .unwrap();

    // ws:// loopback still works without touching Access headers even with partial Access creds.
    let (base_ws, _port, log) = spawn_mock_cdp_server(basic_handler(
        "<html><body><main>Loopback</main></body></html>".to_string(),
    ))
    .await;
    let ws_ok = CdpRenderer::new(
        format!("{base_ws}/devtools/browser"),
        dir.path().to_path_buf(),
        None,
    )
    .render(
        "https://example.com/start",
        &CdpRenderOptions::default(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(ws_ok.html.contains("Loopback"));
    assert!(!log.lock().unwrap().request_headers[0].contains_key("cf-access-client-id"));

    // wss:// with partial Access tuple fails loud before TCP connect.
    let wss_partial = CdpRenderer::new(
        "wss://cdp.example.com/devtools/browser",
        dir.path().to_path_buf(),
        None,
    )
    .render(
        "https://example.com/start",
        &CdpRenderOptions::default(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&wss_partial),
        Some(TransportErrorKind::Network)
    );
    assert!(wss_partial.to_string().contains("cdp-access-client-secret"));

    // Ensure unused RenderMode import isn't flagged if omitted.
    let _ = RenderMode::Cdp;
}

#[tokio::test]
async fn passive_attach_enforces_outbound_rpc_allowlist_and_ignores_fetch_pause_events() {
    let dir = TempDir::new();
    let html = concat!(
        "<!doctype html><html><head><title>Attached Doc</title></head>",
        "<body><main><h1>Attached</h1><p>Logged in dashboard content.</p>",
        "<div role=\"tablist\">",
        "<button role=\"tab\" aria-controls=\"p1\" aria-selected=\"true\">One</button>",
        "<button role=\"tab\" aria-controls=\"p2\" aria-selected=\"false\">Two</button>",
        "</div>",
        "<div id=\"p1\" role=\"tabpanel\"><p>Visible one.</p></div>",
        "<div id=\"p2\" role=\"tabpanel\" hidden><p>Hidden two.</p></div>",
        "</main></body></html>"
    )
    .to_string();

    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| {
        let html = html.clone();
        match method {
            "Page.enable" | "DOM.enable" | "WebMCP.enable" => {
                vec![
                    rpc_event(
                        session_id.as_deref(),
                        "Fetch.requestPaused",
                        json!({ "requestId": "req-unsolicited", "request": { "url": "https://portal.example.com/" } }),
                    ),
                    rpc_ok(id, session_id, json!({})),
                ]
            }
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({
                    "frameTree": {
                        "frame": {
                            "id": "main-frame-1",
                            "loaderId": "loader-alpha",
                            "url": "https://portal.example.com/app",
                            "urlFragment": "#panel",
                            "securityOrigin": "https://portal.example.com",
                            "mimeType": "text/html"
                        }
                    }
                }),
            )],
            "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 7 } }))],
            "DOM.getOuterHTML" => vec![rpc_ok(id, session_id, json!({ "outerHTML": html }))],
            "DOM.querySelector" => vec![rpc_ok(id, session_id, json!({ "nodeId": 42 }))],
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        }
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/page/tab-123"),
        dir.path().to_path_buf(),
        None,
    );
    let res = renderer
        .render_attached(
            Some("https://portal.example.com/"),
            &CdpRenderOptions {
                wait_for_selector: Some("main".to_string()),
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(res.final_url, "https://portal.example.com/app#panel");
    assert!(res.html.contains("Logged in dashboard content."));
    wait_for_close_frame(&log).await;

    {
        let guard = log.lock().unwrap();
        assert_eq!(guard.request_uris, vec!["/devtools/page/tab-123"]);
        let methods: Vec<&str> = guard.rpcs.iter().map(|r| r.method.as_str()).collect();
        for required in [
            "Page.enable",
            "Page.getFrameTree",
            "DOM.getDocument",
            "DOM.getOuterHTML",
        ] {
            assert!(
                methods.contains(&required),
                "missing required passive method {required} in {methods:?}"
            );
        }
        for forbidden in [
            "Fetch.enable",
            "Fetch.continueRequest",
            "Fetch.failRequest",
            "Network.enable",
            "Page.navigate",
            "Kitesurf.setLayoutMode",
            "Target.createTarget",
            "Target.attachToTarget",
            "Target.closeTarget",
            "Browser.close",
        ] {
            assert!(
                !methods.contains(&forbidden),
                "forbidden method {forbidden} sent during passive attach: {methods:?}"
            );
        }
        assert!(guard.received_close_frame);
    }

    // Abrupt WebSocket EOF mid-attach fails cleanly with zero forbidden RPCs.
    let drop_handler: RpcHandler = Arc::new(|id, method, session_id, _params, _log| match method {
        "Page.enable" | "DOM.enable" | "WebMCP.enable" => vec![rpc_ok(id, session_id, json!({}))],
        "Page.getFrameTree" => vec![rpc_ok(
            id,
            session_id,
            json!({
                "frameTree": {
                    "frame": {
                        "id": "main-frame-1",
                        "loaderId": "loader-alpha",
                        "url": "https://portal.example.com/app",
                        "securityOrigin": "https://portal.example.com"
                    }
                }
            }),
        )],
        "DOM.getDocument" => vec![Message::Close(None)],
        _ => vec![],
    });
    let (drop_ws, _p, drop_log) = spawn_mock_cdp_server(drop_handler).await;
    let drop_err = CdpRenderer::new(
        format!("{drop_ws}/devtools/page/drop-1"),
        dir.path().to_path_buf(),
        None,
    )
    .render_attached(None, &CdpRenderOptions::default(), Duration::from_secs(5))
    .await
    .unwrap_err();
    assert!(
        matches!(
            transport_error_kind(&drop_err),
            Some(TransportErrorKind::Network | TransportErrorKind::LoadError)
        ),
        "unexpected drop err: {drop_err}"
    );
    let drop_methods: Vec<String> = drop_log
        .lock()
        .unwrap()
        .rpcs
        .iter()
        .map(|r| r.method.clone())
        .collect();
    assert!(!drop_methods
        .iter()
        .any(|m| m.starts_with("Target.") || m.starts_with("Fetch.") || m == "Browser.close"));
}

#[tokio::test]
async fn passive_attach_aborts_on_all_three_toctou_document_swaps() {
    let dir = TempDir::new();

    // Case A: `Page.frameNavigated` arrives during `--wait-for-selector` polling.
    let selector_polls = Arc::new(AtomicU64::new(0));
    let polls_a = Arc::clone(&selector_polls);
    let handler_a: RpcHandler =
        Arc::new(move |id, method, session_id, _params, _log| match method {
            "Page.enable" | "DOM.enable" | "WebMCP.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Page.getFrameTree" => vec![rpc_ok(
                id,
                session_id,
                json!({
                    "frameTree": {
                        "frame": {
                            "id": "main-a",
                            "loaderId": "ldr-a1",
                            "url": "https://portal.example.com/step1",
                            "securityOrigin": "https://portal.example.com"
                        }
                    }
                }),
            )],
            "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 1 } }))],
            "DOM.getOuterHTML" => {
                vec![rpc_ok(
                    id,
                    session_id,
                    json!({ "outerHTML": "<html><body>Initial</body></html>" }),
                )]
            }
            "DOM.querySelector" => {
                if polls_a.fetch_add(1, Ordering::Relaxed) == 0 {
                    vec![
                        rpc_event(
                            session_id.as_deref(),
                            "Page.frameNavigated",
                            json!({
                                "frame": {
                                    "id": "main-a",
                                    "loaderId": "ldr-a2",
                                    "url": "https://portal.example.com/step2"
                                }
                            }),
                        ),
                        rpc_ok(id, session_id, json!({ "nodeId": 0 })),
                    ]
                } else {
                    vec![rpc_ok(id, session_id, json!({ "nodeId": 22 }))]
                }
            }
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        });
    let (ws_a, _p, _l) = spawn_mock_cdp_server(handler_a).await;
    let err_a = CdpRenderer::new(
        format!("{ws_a}/devtools/page/a"),
        dir.path().to_path_buf(),
        None,
    )
    .render_attached(
        Some("https://portal.example.com/step1"),
        &CdpRenderOptions {
            wait_for_selector: Some(".ready".to_string()),
            ..Default::default()
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&err_a),
        Some(TransportErrorKind::LoadError)
    );

    // Case B: `loaderId` swaps between `DOM.getOuterHTML` and `Accessibility.getFullAXTree`.
    let tree_calls_b = Arc::new(AtomicU64::new(0));
    let tree_b = Arc::clone(&tree_calls_b);
    let handler_b: RpcHandler = Arc::new(
        move |id, method, session_id, _params, _log| match method {
            "Page.enable" | "DOM.enable" | "WebMCP.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Page.getFrameTree" => {
                let n = tree_b.fetch_add(1, Ordering::Relaxed);
                let loader = if n < 2 { "ldr-b1" } else { "ldr-b2" };
                vec![rpc_ok(
                    id,
                    session_id,
                    json!({
                        "frameTree": {
                            "frame": {
                                "id": "main-b",
                                "loaderId": loader,
                                "url": "https://portal.example.com/doc",
                                "securityOrigin": "https://portal.example.com"
                            }
                        }
                    }),
                )]
            }
            "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 1 } }))],
            "DOM.getOuterHTML" => {
                vec![rpc_ok(
                    id,
                    session_id,
                    json!({ "outerHTML": "<html><body>Snap B</body></html>" }),
                )]
            }
            "Accessibility.getFullAXTree" => vec![rpc_ok(
                id,
                session_id,
                json!({
                    "nodes": [
                        { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "Doc" }, "childIds": [] }
                    ]
                }),
            )],
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        },
    );
    let (ws_b, _p, _l) = spawn_mock_cdp_server(handler_b).await;
    let err_b = CdpRenderer::new(
        format!("{ws_b}/devtools/page/b"),
        dir.path().to_path_buf(),
        None,
    )
    .render_attached(
        Some("https://portal.example.com/doc"),
        &CdpRenderOptions {
            capture_axtree: true,
            ..Default::default()
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&err_b),
        Some(TransportErrorKind::LoadError)
    );

    // Case C: Tab navigates away and back to the exact same URL (new `loaderId`) before final `Page.getFrameTree`.
    let tree_calls_c = Arc::new(AtomicU64::new(0));
    let tree_c = Arc::clone(&tree_calls_c);
    let handler_c: RpcHandler =
        Arc::new(move |id, method, session_id, _params, _log| match method {
            "Page.enable" | "DOM.enable" | "WebMCP.enable" => {
                vec![rpc_ok(id, session_id, json!({}))]
            }
            "Page.getFrameTree" => {
                let n = tree_calls_c_step(&tree_c);
                let loader = if n == 0 || n == 1 {
                    "ldr-c1"
                } else {
                    "ldr-c-reloaded"
                };
                vec![rpc_ok(
                    id,
                    session_id,
                    json!({
                        "frameTree": {
                            "frame": {
                                "id": "main-c",
                                "loaderId": loader,
                                "url": "https://portal.example.com/same",
                                "securityOrigin": "https://portal.example.com"
                            }
                        }
                    }),
                )]
            }
            "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 1 } }))],
            "DOM.getOuterHTML" => {
                vec![rpc_ok(
                    id,
                    session_id,
                    json!({ "outerHTML": "<html><body>Round trip</body></html>" }),
                )]
            }
            _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
        });
    let (ws_c, _p, _l) = spawn_mock_cdp_server(handler_c).await;
    let err_c = CdpRenderer::new(
        format!("{ws_c}/devtools/page/c"),
        dir.path().to_path_buf(),
        None,
    )
    .render_attached(
        Some("https://portal.example.com/same"),
        &CdpRenderOptions::default(),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert_eq!(
        transport_error_kind(&err_c),
        Some(TransportErrorKind::LoadError)
    );
}

fn tree_calls_c_step(counter: &Arc<AtomicU64>) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test]
async fn passive_attach_guards_endpoints_frame_binding_axtree_size_and_child_webmcp() {
    let dir = TempDir::new();

    // 1. Endpoint URL validations reject non-page/remote/ambiguous CDP endpoints.
    for bad in [
        "ws://127.0.0.1:9222/devtools/browser",
        "ws://127.0.0.1:9222/devtools/page/",
        "ws://127.0.0.1:9222/devtools/page/a/b",
        "wss://cdp.example.com/devtools/page/abc",
        "http://127.0.0.1:9222/devtools/page/abc",
    ] {
        assert!(
            validate_loopback_page_cdp_url(bad).is_err(),
            "expected validate_loopback_page_cdp_url to reject {bad}"
        );
    }
    assert!(validate_loopback_page_cdp_url("ws://127.0.0.1:9222/devtools/page/TAB-42").is_ok());

    // 2. Frame binding failures and child-frame WebMCP filtering.
    let frame_override = Arc::new(Mutex::new(json!({
        "id": "main-1",
        "loaderId": "ldr-1",
        "url": "https://portal.example.com/app",
        "securityOrigin": "https://portal.example.com"
    })));
    let ax_nodes = Arc::new(Mutex::new(json!([
        { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "OK" }, "childIds": [] }
    ])));

    let f_bg = Arc::clone(&frame_override);
    let ax_bg = Arc::clone(&ax_nodes);
    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| match method {
        "Page.enable" | "DOM.enable" => vec![rpc_ok(id, session_id, json!({}))],
        "WebMCP.enable" => vec![
            rpc_event(
                session_id.as_deref(),
                "WebMCP.toolsAdded",
                json!({
                    "tools": [
                        {
                            "name": "evil_child_tool",
                            "description": "from iframe",
                            "frameId": "child-iframe-99"
                        },
                        {
                            "name": "main_safe_tool",
                            "description": "main tool",
                            "frameId": "main-1"
                        }
                    ]
                }),
            ),
            rpc_ok(id, session_id, json!({})),
        ],
        "Page.getFrameTree" => {
            let f = f_bg.lock().unwrap().clone();
            vec![rpc_ok(
                id,
                session_id,
                json!({ "frameTree": { "frame": f } }),
            )]
        }
        "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 1 } }))],
        "DOM.getOuterHTML" => {
            vec![rpc_ok(
                id,
                session_id,
                json!({ "outerHTML": "<html><body><main><p>Bound OK</p></main></body></html>" }),
            )]
        }
        "Accessibility.getFullAXTree" => {
            let nodes = ax_bg.lock().unwrap().clone();
            vec![rpc_ok(id, session_id, json!({ "nodes": nodes }))]
        }
        _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
    });

    let (base_ws, _port, log) = spawn_mock_cdp_server(handler).await;
    let renderer = CdpRenderer::new(
        format!("{base_ws}/devtools/page/main-1"),
        dir.path().to_path_buf(),
        None,
    );

    // Child-frame WebMCP is ignored while main-frame tool is kept; `frameId` is sent on AXTree.
    let ok = renderer
        .render_attached(
            Some("https://portal.example.com/"),
            &CdpRenderOptions {
                capture_axtree: true,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(ok.imperative_tools.len(), 1);
    assert_eq!(ok.imperative_tools[0].name, "main_safe_tool");
    assert!(ok
        .axtree_text
        .as_deref()
        .is_some_and(|t| t.contains("RootWebArea")));
    {
        let guard = log.lock().unwrap();
        let ax_rpc = guard
            .rpcs
            .iter()
            .find(|r| r.method == "Accessibility.getFullAXTree")
            .expect("AXTree RPC");
        assert_eq!(ax_rpc.params["frameId"], "main-1");
    }

    // Oversized successful AXTree reply triggers `ResponseTooLarge`.
    let huge_label = "x".repeat(4096);
    *ax_nodes.lock().unwrap() = json!([
        { "nodeId": "1", "role": { "value": "StaticText" }, "name": { "value": huge_label }, "childIds": [] }
    ]);
    let ax_over = renderer
        .render_attached(
            Some("https://portal.example.com/"),
            &CdpRenderOptions {
                capture_axtree: true,
                max_response_bytes: 512,
                ..Default::default()
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(ax_over.is::<ResponseTooLargeError>());
    *ax_nodes.lock().unwrap() = json!([
        { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "OK" }, "childIds": [] }
    ]);

    // Missing loaderId -> ProtocolMismatch.
    *frame_override.lock().unwrap() = json!({
        "id": "main-1",
        "loaderId": "",
        "url": "https://portal.example.com/app",
        "securityOrigin": "https://portal.example.com"
    });
    let no_ldr = renderer
        .render_attached(None, &CdpRenderOptions::default(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&no_ldr),
        Some(TransportErrorKind::ProtocolMismatch)
    );

    // Non-empty unreachableUrl -> LoadError.
    *frame_override.lock().unwrap() = json!({
        "id": "main-1",
        "loaderId": "ldr-1",
        "url": "chrome-error://chromewebdata/",
        "unreachableUrl": "https://portal.example.com/app"
    });
    let unreach = renderer
        .render_attached(None, &CdpRenderOptions::default(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&unreach),
        Some(TransportErrorKind::LoadError)
    );

    // Non-HTTP URLs (`chrome://settings`, `about:blank`) rejected.
    for non_http in ["chrome://settings", "about:blank"] {
        *frame_override.lock().unwrap() = json!({
            "id": "main-1",
            "loaderId": "ldr-1",
            "url": non_http
        });
        let err = renderer
            .render_attached(None, &CdpRenderOptions::default(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::LoadError),
            "expected LoadError for {non_http}, got {err}"
        );
    }

    // Mismatched expected origin -> RedirectBlocked.
    *frame_override.lock().unwrap() = json!({
        "id": "main-1",
        "loaderId": "ldr-1",
        "url": "https://other.example.com/app",
        "securityOrigin": "https://other.example.com"
    });
    let origin_err = renderer
        .render_attached(
            Some("https://portal.example.com/app"),
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&origin_err),
        Some(TransportErrorKind::RedirectBlocked)
    );

    // Opaque `"null"` securityOrigin -> RedirectBlocked.
    *frame_override.lock().unwrap() = json!({
        "id": "main-1",
        "loaderId": "ldr-1",
        "url": "https://portal.example.com/sandbox",
        "securityOrigin": "null"
    });
    let opaque_err = renderer
        .render_attached(
            Some("https://portal.example.com/"),
            &CdpRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(
        transport_error_kind(&opaque_err),
        Some(TransportErrorKind::RedirectBlocked)
    );
}

#[tokio::test]
async fn passive_attach_classification_site_cache_isolation_and_cli_envelopes() {
    let dir = TempDir::new();
    let site_cache_path = dir.path().join("site-cache.json");
    let initial_cache_bytes =
        br#"{"https://portal.example.com":{"prefer_cef":true,"updated_at_epoch_s":1700000000}}"#;
    std::fs::write(&site_cache_path, initial_cache_bytes).unwrap();

    let mode = Arc::new(Mutex::new("ok"));
    let frame_url = Arc::new(Mutex::new("https://portal.example.com/dashboard"));
    let query_calls = Arc::new(AtomicU64::new(0));

    let mode_bg = Arc::clone(&mode);
    let url_bg = Arc::clone(&frame_url);
    let qc_bg = Arc::clone(&query_calls);
    let handler: RpcHandler = Arc::new(move |id, method, session_id, _params, _log| match method {
        "Page.enable" | "DOM.enable" | "WebMCP.enable" => vec![rpc_ok(id, session_id, json!({}))],
        "Page.getFrameTree" => {
            let url = *url_bg.lock().unwrap();
            let origin = if url.starts_with("https://other.") {
                "https://other.example.com"
            } else {
                "https://portal.example.com"
            };
            vec![rpc_ok(
                id,
                session_id,
                json!({
                    "frameTree": {
                        "frame": {
                            "id": "main-cls",
                            "loaderId": "ldr-cls",
                            "url": url,
                            "securityOrigin": origin,
                            "mimeType": "text/html"
                        }
                    }
                }),
            )]
        }
        "DOM.getDocument" => vec![rpc_ok(id, session_id, json!({ "root": { "nodeId": 1 } }))],
        "DOM.getOuterHTML" => {
            let m = *mode_bg.lock().unwrap();
            let html = match m {
                "cloudflare" => "<html><head><title>Just a moment...</title></head><body><div id=\"cf-wrapper\">Checking your browser before accessing... Enable JavaScript and cookies to continue</div><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1\"></script></body></html>",
                "login" => "<html><head><title>Sign In</title></head><body><main><form action=\"/login\"><label>Email<input type=\"email\"></label><label>Password<input type=\"password\"></label><button type=\"submit\">Sign in</button></form></main></body></html>",
                _ => "<html><head><title>Portal</title></head><body><main><h1>Portal</h1><p>Loaded portal metrics report.</p><div role=\"tablist\"><button role=\"tab\" aria-controls=\"t1\" aria-selected=\"true\">Overview</button><button role=\"tab\" aria-controls=\"t2\" aria-selected=\"false\">Audit</button></div><div id=\"t1\" role=\"tabpanel\"><p>Overview body.</p></div><div id=\"t2\" role=\"tabpanel\" hidden><p>Audit log rows.</p></div></main></body></html>",
            };
            vec![rpc_ok(id, session_id, json!({ "outerHTML": html }))]
        }
        "DOM.querySelector" => {
            qc_bg.fetch_add(1, Ordering::Relaxed);
            vec![rpc_ok(id, session_id, json!({ "nodeId": 0 }))]
        }
        _ => vec![rpc_err(id, session_id, -32601, "Method not found")],
    });

    let (base_ws, _port, _log) = spawn_mock_cdp_server(handler).await;
    let cdp_page_url = format!("{base_ws}/devtools/page/cls-1");

    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: dir.path().to_path_buf(),
        cdp_url: Some(cdp_page_url.clone()),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        no_web_bot_auth: true,
        cef_renderer_executable: Some(dir.path().join("missing-cef")),
        ..Default::default()
    })
    .unwrap();

    let attach_opts = FetchOptions {
        render: RenderMode::Cdp,
        ..Default::default()
    };

    // 1. Success attaches with `strategy = "cdp_attach"`, `status_code = 0`, `escalated = false`, and UI gates.
    let ok = fetch_target(
        &ctx,
        FetchTarget::AttachCdp {
            expected_origin: None,
        },
        &attach_opts,
    )
    .await
    .unwrap();
    assert_eq!(ok.strategy, "cdp_attach");
    assert_eq!(ok.status_code, 0);
    assert!(!ok.escalated);
    assert_eq!(ok.url, "https://portal.example.com/dashboard");
    assert_eq!(ok.ui_gates.len(), 1);
    assert_eq!(
        std::fs::read(&site_cache_path).unwrap(),
        initial_cache_bytes
    );

    // 2. Challenge interstitial fails fast BEFORE any `--wait-for-selector` polling expires!
    *mode.lock().unwrap() = "cloudflare";
    let t0 = std::time::Instant::now();
    let ch_err = fetch_target(
        &ctx,
        FetchTarget::AttachCdp {
            expected_origin: Some("https://portal.example.com/".to_string()),
        },
        &FetchOptions {
            render: RenderMode::Cdp,
            wait_for_selector: Some(".never-appears".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "challenge waited on selector timeout"
    );
    assert_eq!(query_calls.load(Ordering::Relaxed), 0);
    assert!(
        matches!(ch_err, FetchError::Challenge { status: 0, .. }),
        "expected Challenge(status=0), got {ch_err:?}"
    );
    assert_eq!(
        std::fs::read(&site_cache_path).unwrap(),
        initial_cache_bytes
    );

    // 3. Login wall classifies cleanly as LoginWall without touching site-cache.
    *mode.lock().unwrap() = "login";
    *frame_url.lock().unwrap() = "https://portal.example.com/login";
    let login_err = fetch_target(
        &ctx,
        FetchTarget::AttachCdp {
            expected_origin: None,
        },
        &attach_opts,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            login_err,
            FetchError::Blocked {
                status: 0,
                reason: BlockReason::LoginWall,
                ..
            }
        ),
        "expected Blocked LoginWall, got {login_err:?}"
    );
    assert_eq!(
        std::fs::read(&site_cache_path).unwrap(),
        initial_cache_bytes
    );

    // 4. Selector timeout and origin mismatch both leave site-cache untouched.
    *mode.lock().unwrap() = "ok";
    *frame_url.lock().unwrap() = "https://portal.example.com/dashboard";
    let timeout_dir = TempDir::new();
    let timeout_cache_path = timeout_dir.path().join("site-cache.json");
    std::fs::write(&timeout_cache_path, initial_cache_bytes).unwrap();
    let timeout_ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: timeout_dir.path().to_path_buf(),
        cdp_url: Some(cdp_page_url.clone()),
        timeout: Duration::from_millis(120),
        cef_timeout: Duration::from_millis(120),
        use_daemon: false,
        no_web_bot_auth: true,
        cef_renderer_executable: Some(timeout_dir.path().join("missing-cef")),
        ..Default::default()
    })
    .unwrap();
    let timeout_err = fetch_target(
        &timeout_ctx,
        FetchTarget::AttachCdp {
            expected_origin: None,
        },
        &FetchOptions {
            render: RenderMode::Cdp,
            wait_for_selector: Some(".never-appears".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(timeout_err, FetchError::Timeout { .. }),
        "got {timeout_err:?}"
    );
    assert_eq!(
        std::fs::read(&timeout_cache_path).unwrap(),
        initial_cache_bytes
    );
    assert_eq!(
        std::fs::read(&site_cache_path).unwrap(),
        initial_cache_bytes
    );

    *frame_url.lock().unwrap() = "https://other.example.com/dashboard";
    let mismatch = fetch_target(
        &ctx,
        FetchTarget::AttachCdp {
            expected_origin: Some("https://portal.example.com/".to_string()),
        },
        &attach_opts,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(mismatch, FetchError::Transport(_)),
        "got {mismatch:?}"
    );
    assert_eq!(
        std::fs::read(&site_cache_path).unwrap(),
        initial_cache_bytes
    );

    // 5. CLI integration: single-item `--json`, `--jsonl`, `--metadata-only`, and invalid flag rejections.
    *frame_url.lock().unwrap() = "https://portal.example.com/dashboard";
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    let cli_json = tokio::process::Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(dir.path())
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        cli_json.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&cli_json.stderr)
    );
    let parsed_json: Value = serde_json::from_slice(&cli_json.stdout).unwrap();
    assert_eq!(parsed_json["strategy"], "cdp_attach");
    assert_eq!(parsed_json["status_code"], 0);
    assert_eq!(parsed_json["ui_gates"][0]["kind"], "tablist");

    let cli_jsonl = tokio::process::Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(dir.path())
        .args([
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
            "--jsonl",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(cli_jsonl.status.code(), Some(0));
    let jsonl_str = String::from_utf8(cli_jsonl.stdout).unwrap();
    assert_eq!(jsonl_str.lines().count(), 1);
    let parsed_jsonl: Value = serde_json::from_str(jsonl_str.trim()).unwrap();
    assert_eq!(parsed_jsonl["strategy"], "cdp_attach");
    assert_eq!(parsed_jsonl["status_code"], 0);

    let cli_meta = tokio::process::Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(dir.path())
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
            "--metadata-only",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(cli_meta.status.code(), Some(0));
    let parsed_meta: Value = serde_json::from_slice(&cli_meta.stdout).unwrap();
    assert!(parsed_meta.get("content").is_none());
    assert_eq!(parsed_meta["strategy"], "cdp_attach");
    assert_eq!(parsed_meta["status_code"], 0);
    assert_eq!(parsed_meta["ui_gates"][0]["kind"], "tablist");

    for bad_args in [
        vec!["--no-daemon", "fetch", "--no-navigate"],
        vec![
            "--no-daemon",
            "--proxy",
            "http://127.0.0.1:8888",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
            "--resources",
            "transcripts",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
            "https://portal.example.com/a",
            "https://portal.example.com/b",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            &cdp_page_url,
            "--no-navigate",
            "-",
        ],
    ] {
        let out = tokio::process::Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
            .env("PLEB_NO_KEYCHAIN", "1")
            .args(["--state-dir"])
            .arg(dir.path())
            .args(&bad_args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected exit 1 for {bad_args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = ResourcePolicy::Manifest;
}
