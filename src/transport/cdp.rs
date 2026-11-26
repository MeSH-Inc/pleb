use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream};
use url::{Host, Url};

use super::{ResponseTooLargeError, TransportError, TransportErrorKind};
use crate::auth::{resolve_credentials, CredentialProvider};
use crate::extract::webmcp::sanitize_webmcp_tool;
use crate::extract::{WebMcpToolAnnotations, WebMcpToolDescriptor, WebMcpToolSource};
use crate::policy::{is_local_or_private_host, is_loopback_host, is_safe_redirect_url};

pub const MAX_HTML_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_UNSOLICITED_EVENTS: usize = 16_384;
pub const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

const TEARDOWN_GRACE: Duration = Duration::from_millis(500);
const SELECTOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PAGE_STATE_POLL_INTERVAL: Duration = Duration::from_millis(125);
const PAGE_STATE_MAX_WAIT: Duration = Duration::from_millis(1000);
const PAGE_STATE_FALLBACK_SETTLE: Duration = Duration::from_millis(250);
const CDP_ERR_METHOD_NOT_FOUND: i64 = -32601;
const CDP_ERR_SERVER_ERROR: i64 = -32000;

#[derive(Debug, Clone, Default)]
pub struct CdpRenderOptions {
    pub wait_for_selector: Option<String>,
    pub capture_axtree: bool,
    pub max_response_bytes: usize,
    pub allow_local_targets: bool,
}

#[derive(Debug, Clone)]
pub struct CdpRenderResult {
    pub html: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub link_header: Option<String>,
    pub axtree_text: Option<String>,
    pub imperative_tools: Vec<WebMcpToolDescriptor>,
}

#[derive(Clone)]
pub struct CdpRenderer {
    cdp_url: String,
    state_dir: PathBuf,
    proxy: Option<String>,
    tls_config: Arc<Mutex<Option<Arc<rustls::ClientConfig>>>>,
}

impl fmt::Debug for CdpRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CdpRenderer")
            .field("cdp_url", &self.cdp_url_display())
            .field("state_dir", &self.state_dir)
            .field("has_proxy", &self.proxy.is_some())
            .finish_non_exhaustive()
    }
}

impl CdpRenderer {
    pub fn new(cdp_url: impl Into<String>, state_dir: PathBuf, proxy: Option<String>) -> Self {
        Self {
            cdp_url: cdp_url.into(),
            state_dir,
            proxy,
            tls_config: Arc::new(Mutex::new(None)),
        }
    }

    pub fn cdp_url_display(&self) -> String {
        sanitize_cdp_url_for_display(&self.cdp_url)
    }

    fn rustls_client_config(&self) -> Result<Arc<rustls::ClientConfig>> {
        let mut guard = self.tls_config.lock().map_err(|_| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "CDP TLS config lock poisoned",
            ))
        })?;
        if let Some(ref cfg) = *guard {
            return Ok(Arc::clone(cfg));
        }

        let mut root_store = rustls::RootCertStore::empty();
        let cert_res = rustls_native_certs::load_native_certs();
        if !cert_res.errors.is_empty() {
            tracing::warn!(
                "native root CA certificate loading errors for CDP TLS: {:?}",
                cert_res.errors
            );
        }
        let (added, _) = root_store.add_parsable_certificates(cert_res.certs);
        if added == 0 {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "no native root CA certificates available for wss:// CDP connection",
            )));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    format!("failed to initialize CDP TLS protocol versions: {e}"),
                ))
            })?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let arc_cfg = Arc::new(cfg);
        *guard = Some(Arc::clone(&arc_cfg));
        Ok(arc_cfg)
    }

    pub async fn render(
        &self,
        url: &str,
        options: &CdpRenderOptions,
        timeout: Duration,
    ) -> Result<CdpRenderResult> {
        if self.proxy.is_some() {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "CDP renderer refuses to connect while --proxy is configured",
            )));
        }

        let cdp_url = validate_cdp_url(&self.cdp_url)?;
        let target_url = validate_target_url(url, &cdp_url, options)?;

        if let Some(selector) = options.wait_for_selector.as_deref() {
            scraper::Selector::parse(selector).map_err(|_| {
                anyhow::Error::new(TransportError::new(
                    TransportErrorKind::InvalidSelector,
                    format!("invalid CSS selector for --wait-for-selector: {selector}"),
                ))
            })?;
        }

        if timeout.is_zero() {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("CDP render timed out before connecting for {url}"),
            )));
        }

        let deadline = Instant::now() + timeout;
        let is_page_endpoint = cdp_url.path().starts_with("/devtools/page/");
        let ws_url = prepare_cdp_ws_url(&cdp_url, options.capture_axtree, is_page_endpoint);

        let access_headers = if ws_url.scheme() == "wss" {
            resolve_cdp_access_headers(&self.state_dir, &cdp_url)?
        } else {
            None
        };

        let host = ws_url.host_str().ok_or_else(|| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "CDP URL must include a host",
            ))
        })?;
        let port = ws_url.port_or_known_default().ok_or_else(|| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "CDP URL must include a valid port",
            ))
        })?;
        let connect_addr = format!("{host}:{port}");

        let tcp_stream = run_before(
            deadline,
            TcpStream::connect(&connect_addr),
            "CDP TCP connect",
            url,
        )
        .await
        .map_err(|e| map_connect_io_or_transport(e, url))?;

        let peer_ip = tcp_stream
            .peer_addr()
            .map_err(|e| {
                anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    format!("failed to inspect CDP socket peer address: {e}"),
                ))
            })?
            .ip();

        if ws_url.scheme() == "ws" && !is_loopback_ip(peer_ip) {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                "plaintext ws:// CDP connection resolved to non-loopback IP address",
            )));
        }

        if is_ip_self_ssrf(&target_url, peer_ip, port) {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                "target URL resolves to the configured CDP endpoint (self-SSRF blocked)",
            )));
        }

        let mut request = ws_url.as_str().into_client_request().map_err(|_| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "invalid CDP WebSocket upgrade request",
            ))
        })?;
        if let Some((client_id, client_secret)) = access_headers {
            let id_val = HeaderValue::from_str(&client_id).map_err(|_| {
                anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    "invalid CF-Access-Client-Id header value",
                ))
            })?;
            let secret_val = HeaderValue::from_str(&client_secret).map_err(|_| {
                anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    "invalid CF-Access-Client-Secret header value",
                ))
            })?;
            request.headers_mut().insert("CF-Access-Client-Id", id_val);
            request
                .headers_mut()
                .insert("CF-Access-Client-Secret", secret_val);
        }

        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(MAX_HTML_BYTES);
        ws_config.max_frame_size = Some(MAX_HTML_BYTES);

        let connector = if ws_url.scheme() == "wss" {
            Connector::Rustls(self.rustls_client_config()?)
        } else {
            Connector::Plain
        };

        let (ws_stream, _) = run_before(
            deadline,
            client_async_tls_with_config(request, tcp_stream, Some(ws_config), Some(connector)),
            "CDP WebSocket handshake",
            url,
        )
        .await
        .map_err(|e| map_ws_or_transport_error(e, url))?;

        let mut session = CdpSession::new(
            ws_stream,
            deadline,
            cdp_url,
            peer_ip,
            port,
            target_url,
            options.allow_local_targets,
            is_page_endpoint,
        );

        let outcome = session.execute(options).await;
        session.teardown().await;
        outcome
    }
}

pub fn validate_cdp_url(raw: &str) -> Result<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP URL must not be empty",
        )));
    }
    let parsed = Url::parse(trimmed).map_err(|_| {
        anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "invalid CDP URL",
        ))
    })?;
    let scheme = parsed.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP URL scheme must be ws:// or wss://",
        )));
    }
    let Some(host) = parsed.host_str() else {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP URL must include a host",
        )));
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP URL must not embed userinfo credentials",
        )));
    }
    if parsed.fragment().is_some() {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP URL must not include a fragment",
        )));
    }
    if scheme == "ws" && !is_loopback_host(host) {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "plaintext ws:// CDP URL is permitted for loopback hosts only; use wss:// for remote endpoints",
        )));
    }
    Ok(parsed)
}

pub fn sanitize_cdp_url_for_display(raw: &str) -> String {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return "[invalid-cdp-url]".to_string();
    };
    let Some(host) = parsed.host_str() else {
        return "[invalid-cdp-url]".to_string();
    };
    let formatted_host = format_url_host(&parsed, host);
    match parsed.port() {
        Some(port) => format!(
            "{}://{formatted_host}:{port}{}",
            parsed.scheme(),
            parsed.path()
        ),
        None => format!("{}://{formatted_host}{}", parsed.scheme(), parsed.path()),
    }
}

fn format_url_host(url: &Url, fallback_host: &str) -> String {
    match url.host() {
        Some(Host::Ipv6(ip)) => format!("[{ip}]"),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Domain(d)) => d.to_ascii_lowercase(),
        None => fallback_host.to_ascii_lowercase(),
    }
}

fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn prepare_cdp_ws_url(cdp_url: &Url, capture_axtree: bool, is_page_endpoint: bool) -> Url {
    let mut ws_url = cdp_url.clone();
    if !capture_axtree && !is_page_endpoint {
        let has_layout = ws_url.query_pairs().any(|(k, _)| k == "layout");
        if !has_layout {
            ws_url.query_pairs_mut().append_pair("layout", "off");
        }
    }
    ws_url
}

fn resolve_cdp_access_headers(state_dir: &Path, cdp_url: &Url) -> Result<Option<(String, String)>> {
    let resolved = resolve_credentials(
        state_dir,
        &[
            CredentialProvider::CdpAccessClientId,
            CredentialProvider::CdpAccessClientSecret,
            CredentialProvider::CdpAccessHost,
        ],
    )
    .map_err(|e| {
        anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            format!("failed to resolve CDP Access credentials: {e}"),
        ))
    })?;

    if resolved.is_empty() {
        return Ok(None);
    }

    let id = resolved.get(&CredentialProvider::CdpAccessClientId);
    let secret = resolved.get(&CredentialProvider::CdpAccessClientSecret);
    let host = resolved.get(&CredentialProvider::CdpAccessHost);

    match (id, secret, host) {
        (Some(id), Some(secret), Some(host)) => {
            if cdp_access_host_matches(cdp_url, host.value.as_str()) {
                Ok(Some((
                    id.value.as_str().to_string(),
                    secret.value.as_str().to_string(),
                )))
            } else {
                Ok(None)
            }
        }
        _ => {
            let mut missing = Vec::new();
            if id.is_none() {
                missing.push(CredentialProvider::CdpAccessClientId.as_str());
            }
            if secret.is_none() {
                missing.push(CredentialProvider::CdpAccessClientSecret.as_str());
            }
            if host.is_none() {
                missing.push(CredentialProvider::CdpAccessHost.as_str());
            }
            Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                format!(
                    "incomplete CDP Access configuration; missing: {}",
                    missing.join(", ")
                ),
            )))
        }
    }
}

fn cdp_access_host_matches(cdp_url: &Url, allowed_host: &str) -> bool {
    let trimmed = allowed_host.trim().to_ascii_lowercase();
    if trimmed.is_empty()
        || trimmed.contains("://")
        || trimmed.contains('/')
        || trimmed.contains('@')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.bytes().any(|b| b.is_ascii_whitespace())
    {
        return false;
    }

    let Ok(allowed_parsed) = Url::parse(&format!("wss://{trimmed}")) else {
        return false;
    };
    let Some(allowed_host_raw) = allowed_parsed.host_str() else {
        return false;
    };
    let allowed_host_norm = format_url_host(&allowed_parsed, allowed_host_raw);

    let Some(cdp_host_raw) = cdp_url.host_str() else {
        return false;
    };
    let cdp_host_norm = format_url_host(cdp_url, cdp_host_raw);
    if cdp_host_norm != allowed_host_norm {
        return false;
    }

    match allowed_parsed.port() {
        Some(expected_port) => cdp_url.port_or_known_default() == Some(expected_port),
        None => true,
    }
}

fn validate_target_url(url: &str, cdp_url: &Url, options: &CdpRenderOptions) -> Result<Url> {
    let parsed = Url::parse(url).map_err(|e| {
        anyhow::Error::new(TransportError::new(
            TransportErrorKind::LoadError,
            format!("invalid target URL: {e}"),
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::LoadError,
            format!(
                "unsupported URL scheme {:?} for CDP render (only http and https)",
                parsed.scheme()
            ),
        )));
    }
    let Some(target_host) = parsed.host_str() else {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::LoadError,
            "target URL must include a host",
        )));
    };
    if is_url_self_ssrf(&parsed, cdp_url) {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::RedirectBlocked,
            "target URL matches the configured CDP endpoint (self-SSRF blocked)",
        )));
    }
    if cdp_url.scheme() == "wss"
        && !options.allow_local_targets
        && is_local_or_private_host(target_host)
    {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::RedirectBlocked,
            "remote wss:// CDP renderer refuses local or private target URLs",
        )));
    }
    Ok(parsed)
}

fn is_url_self_ssrf(target: &Url, cdp_url: &Url) -> bool {
    let (Some(target_raw), Some(cdp_raw)) = (target.host_str(), cdp_url.host_str()) else {
        return false;
    };
    let (Some(target_port), Some(cdp_port)) = (
        target.port_or_known_default(),
        cdp_url.port_or_known_default(),
    ) else {
        return false;
    };
    if target_port != cdp_port {
        return false;
    }
    let target_norm = format_url_host(target, target_raw);
    let cdp_norm = format_url_host(cdp_url, cdp_raw);
    target_norm == cdp_norm || (is_loopback_host(&target_norm) && is_loopback_host(&cdp_norm))
}

fn is_ip_self_ssrf(target: &Url, cdp_peer_ip: IpAddr, cdp_port: u16) -> bool {
    if target.port_or_known_default() != Some(cdp_port) {
        return false;
    }
    let Some(host) = target.host_str() else {
        return false;
    };
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(target_ip) = trimmed.parse::<IpAddr>() {
        return canonical_ip(target_ip) == canonical_ip(cdp_peer_ip);
    }
    false
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        other => other,
    }
}

async fn run_before<F, T, E>(
    deadline: Instant,
    fut: F,
    phase: &str,
    url: &str,
) -> std::result::Result<T, anyhow::Error>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: Into<anyhow::Error>,
{
    let now = Instant::now();
    if now >= deadline {
        return Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Timeout,
            format!("{phase} timed out for {url}"),
        )));
    }
    match tokio::time::timeout_at(deadline, fut).await {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(err)) => Err(err.into()),
        Err(_) => Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Timeout,
            format!("{phase} timed out for {url}"),
        ))),
    }
}

fn map_connect_io_or_transport(err: anyhow::Error, _url: &str) -> anyhow::Error {
    if err.is::<TransportError>() {
        return err;
    }
    anyhow::Error::new(TransportError::new(
        TransportErrorKind::Network,
        format!("CDP TCP connect failed: {err}"),
    ))
}

fn map_ws_or_transport_error(err: anyhow::Error, url: &str) -> anyhow::Error {
    if err.is::<TransportError>() {
        return err;
    }
    match err.downcast::<WsError>() {
        Ok(ws_err) => map_tungstenite_error(ws_err, url),
        Err(other) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            format!("CDP WebSocket error: {other}"),
        )),
    }
}

fn map_tungstenite_error(err: WsError, _url: &str) -> anyhow::Error {
    match err {
        WsError::Http(resp) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            format!(
                "CDP WebSocket upgrade failed with HTTP {}",
                resp.status().as_u16()
            ),
        )),
        WsError::Capacity(_) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::ResponseTooLarge,
            "CDP WebSocket message or frame exceeded 64 MiB limit",
        )),
        WsError::Protocol(proto) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::ProtocolMismatch,
            format!("CDP WebSocket protocol error: {proto}"),
        )),
        WsError::Tls(_) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "CDP WebSocket TLS handshake or transport failed",
        )),
        WsError::Io(io_err) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            format!("CDP WebSocket I/O error: {}", io_err.kind()),
        )),
        WsError::Url(_) | WsError::HttpFormat(_) => anyhow::Error::new(TransportError::new(
            TransportErrorKind::Network,
            "invalid CDP WebSocket URL or handshake headers",
        )),
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "CDP WebSocket connection closed",
            ))
        }
        WsError::Utf8 | WsError::AttackAttempt | WsError::WriteBufferFull(_) => anyhow::Error::new(
            TransportError::new(TransportErrorKind::Network, "CDP WebSocket transport error"),
        ),
    }
}

struct CdpSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    deadline: Instant,
    cdp_url: Url,
    cdp_peer_ip: IpAddr,
    cdp_port: u16,
    initial_target_url: Url,
    allow_local_targets: bool,
    is_page_endpoint: bool,
    next_id: i64,
    owns_target: bool,
    created_target_id: Option<String>,
    session_id: Option<String>,
    ignored_reply_ids: HashSet<i64>,
    unsolicited_events: usize,
    unsolicited_event_bytes: usize,
    state: SessionEventState,
    ws_broken: bool,
}

#[derive(Default)]
struct SessionEventState {
    main_frame_id: Option<String>,
    main_request_id: Option<String>,
    final_url: Option<String>,
    status_code: u16,
    content_type: Option<String>,
    link_header: Option<String>,
    load_fired: bool,
    terminal_error: Option<TransportError>,
    cdp_tools: Vec<WebMcpToolDescriptor>,
    blocked_interception_ids: HashSet<String>,
}

#[derive(Serialize)]
struct RpcRequest<'a, P: Serialize> {
    id: i64,
    method: &'a str,
    params: P,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct RpcHeader {
    id: Option<i64>,
    method: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcEnvelope<R> {
    result: Option<R>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcEventEnvelope<P> {
    params: Option<P>,
}

#[derive(Deserialize)]
struct EmptyReply {}

#[derive(Deserialize)]
struct CreateTargetReply {
    #[serde(rename = "targetId")]
    target_id: String,
}

#[derive(Deserialize)]
struct AttachTargetReply {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Deserialize)]
struct FrameTreeReply {
    #[serde(rename = "frameTree")]
    frame_tree: FrameTreeNode,
}

#[derive(Deserialize)]
struct FrameTreeNode {
    frame: FrameTreeFrame,
}

#[derive(Deserialize)]
struct FrameTreeFrame {
    id: String,
}

#[derive(Deserialize)]
struct NavigateReply {
    #[serde(rename = "loaderId")]
    loader_id: Option<String>,
    #[serde(rename = "errorText")]
    error_text: Option<String>,
}

#[derive(Deserialize)]
struct GetDocumentReply {
    root: DocumentRootNode,
}

#[derive(Deserialize)]
struct DocumentRootNode {
    #[serde(rename = "nodeId")]
    node_id: i64,
}

#[derive(Deserialize)]
struct QuerySelectorReply {
    #[serde(rename = "nodeId", default)]
    node_id: i64,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct PageStateReply {
    seq: Option<i64>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct OuterHtmlReply {
    #[serde(rename = "outerHTML")]
    outer_html: String,
}

#[derive(Deserialize)]
struct FetchRequestPausedParams {
    #[serde(rename = "requestId")]
    request_id: String,
    request: FetchPausedRequest,
    #[serde(rename = "frameId")]
    frame_id: Option<String>,
    #[serde(rename = "resourceType")]
    resource_type: Option<String>,
}

#[derive(Deserialize)]
struct FetchPausedRequest {
    url: String,
}

#[derive(Deserialize)]
struct RequestWillBeSentParams {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "frameId")]
    frame_id: Option<String>,
    #[serde(rename = "type")]
    resource_type: Option<String>,
    request: RequestInfo,
    #[serde(rename = "redirectResponse")]
    redirect_response: Option<ResponseUrlOnly>,
}

#[derive(Deserialize)]
struct RequestInfo {
    url: String,
}

#[derive(Deserialize)]
struct ResponseUrlOnly {
    url: String,
}

#[derive(Deserialize)]
struct ResponseReceivedParams {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "frameId")]
    frame_id: Option<String>,
    #[serde(rename = "type")]
    resource_type: Option<String>,
    response: NetworkResponsePayload,
}

#[derive(Deserialize)]
struct NetworkResponsePayload {
    url: String,
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(rename = "mimeType", default)]
    mime_type: String,
}

#[derive(Deserialize)]
struct LoadingFailedParams {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "errorText", default)]
    error_text: String,
}

#[derive(Deserialize)]
struct FrameNavigatedParams {
    frame: NavigatedFrame,
}

#[derive(Deserialize)]
struct NavigatedFrame {
    id: String,
    url: String,
    #[serde(rename = "urlFragment")]
    url_fragment: Option<String>,
}

#[derive(Deserialize)]
struct NavigatedWithinDocumentParams {
    #[serde(rename = "frameId")]
    frame_id: String,
    url: String,
}

#[derive(Deserialize)]
struct WebMcpToolsAddedParams {
    #[serde(default)]
    tools: Vec<CdpWebMcpTool>,
}

#[derive(Deserialize)]
struct CdpWebMcpTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "frameId")]
    frame_id: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Option<serde_json::Value>,
    #[serde(rename = "backendNodeId")]
    backend_node_id: Option<i64>,
    #[serde(default)]
    annotations: Option<CdpWebMcpAnnotations>,
}

#[derive(Deserialize)]
struct CdpWebMcpAnnotations {
    #[serde(rename = "readOnly")]
    read_only: Option<bool>,
    autosubmit: Option<bool>,
}

#[derive(Deserialize)]
struct WebMcpToolsRemovedParams {
    #[serde(default)]
    tools: Vec<CdpWebMcpRemovedTool>,
}

#[derive(Deserialize)]
struct CdpWebMcpRemovedTool {
    name: String,
    #[serde(rename = "frameId")]
    frame_id: Option<String>,
}

enum RpcCallOutcome<R> {
    Ok(R),
    RpcErr(RpcError),
}

impl CdpSession {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        deadline: Instant,
        cdp_url: Url,
        cdp_peer_ip: IpAddr,
        cdp_port: u16,
        initial_target_url: Url,
        allow_local_targets: bool,
        is_page_endpoint: bool,
    ) -> Self {
        Self {
            ws,
            deadline,
            cdp_url,
            cdp_peer_ip,
            cdp_port,
            initial_target_url,
            allow_local_targets,
            is_page_endpoint,
            next_id: 1,
            owns_target: false,
            created_target_id: None,
            session_id: None,
            ignored_reply_ids: HashSet::new(),
            unsolicited_events: 0,
            unsolicited_event_bytes: 0,
            state: SessionEventState::default(),
            ws_broken: false,
        }
    }

    fn alloc_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send_raw<P: Serialize>(
        &mut self,
        id: i64,
        method: &str,
        params: P,
        session_id: Option<&str>,
        deadline: Instant,
    ) -> Result<()> {
        let req = RpcRequest {
            id,
            method,
            params,
            session_id,
        };
        let encoded = serde_json::to_string(&req).map_err(|e| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                format!("failed to serialize CDP request {method}: {e}"),
            ))
        })?;
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("CDP send timed out for {method}"),
            )));
        }
        match tokio::time::timeout_at(deadline, self.ws.send(Message::Text(encoded.into()))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(ws_err)) => {
                self.ws_broken = true;
                Err(map_tungstenite_error(
                    ws_err,
                    self.initial_target_url.as_str(),
                ))
            }
            Err(_) => Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("CDP send timed out for {method}"),
            ))),
        }
    }

    async fn call_typed<P: Serialize, R: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        params: P,
        use_session: bool,
    ) -> Result<RpcCallOutcome<R>> {
        let id = self.alloc_id();
        let sid = if use_session {
            self.session_id.clone()
        } else {
            None
        };
        self.send_raw(id, method, params, sid.as_deref(), self.deadline)
            .await?;

        if let Some(err) = self.state.terminal_error.take() {
            return Err(anyhow::Error::new(err));
        }
        let text = self.read_until_reply(id, self.deadline).await?;
        let envelope: RpcEnvelope<R> = serde_json::from_str(&text).map_err(|e| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                format!("malformed CDP reply for {method}: {e}"),
            ))
        })?;
        if let Some(rpc_err) = envelope.error {
            return Ok(RpcCallOutcome::RpcErr(rpc_err));
        }
        let result = envelope.result.ok_or_else(|| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                format!("CDP reply for {method} missing result payload"),
            ))
        })?;
        Ok(RpcCallOutcome::Ok(result))
    }

    async fn call_required<P: Serialize, R: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        params: P,
        use_session: bool,
    ) -> Result<R> {
        match self.call_typed(method, params, use_session).await? {
            RpcCallOutcome::Ok(r) => Ok(r),
            RpcCallOutcome::RpcErr(err) => {
                let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
                    TransportErrorKind::ProtocolMismatch
                } else {
                    TransportErrorKind::LoadError
                };
                Err(anyhow::Error::new(TransportError::new(
                    kind,
                    format!("CDP {method} failed ({}): {}", err.code, err.message),
                )))
            }
        }
    }

    async fn pump_until(&mut self, stop_at: Instant) -> Result<()> {
        let target_deadline = self.deadline.min(stop_at);
        loop {
            if let Some(err) = self.state.terminal_error.take() {
                return Err(anyhow::Error::new(err));
            }
            if Instant::now() >= target_deadline {
                return Ok(());
            }
            match tokio::time::timeout_at(target_deadline, self.ws.next()).await {
                Ok(Some(Ok(msg))) => {
                    self.process_ws_message(msg, None, target_deadline).await?;
                }
                Ok(Some(Err(ws_err))) => {
                    self.ws_broken = true;
                    return Err(map_tungstenite_error(
                        ws_err,
                        self.initial_target_url.as_str(),
                    ));
                }
                Ok(None) => {
                    self.ws_broken = true;
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::Network,
                        "CDP WebSocket closed unexpectedly",
                    )));
                }
                Err(_) => {
                    if Instant::now() >= self.deadline {
                        return Err(anyhow::Error::new(TransportError::new(
                            TransportErrorKind::Timeout,
                            format!("CDP render timed out for {}", self.initial_target_url),
                        )));
                    }
                    return Ok(());
                }
            }
        }
    }

    async fn read_next_message(
        &mut self,
        awaited_id: Option<i64>,
        deadline: Instant,
    ) -> Result<Option<String>> {
        if let Some(err) = self.state.terminal_error.take() {
            return Err(anyhow::Error::new(err));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("CDP operation timed out for {}", self.initial_target_url),
            )));
        }
        match tokio::time::timeout_at(deadline, self.ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let matched = self.process_ws_message(msg, awaited_id, deadline).await?;
                if let Some(err) = self.state.terminal_error.take() {
                    return Err(anyhow::Error::new(err));
                }
                Ok(matched)
            }
            Ok(Some(Err(ws_err))) => {
                self.ws_broken = true;
                Err(map_tungstenite_error(
                    ws_err,
                    self.initial_target_url.as_str(),
                ))
            }
            Ok(None) => {
                self.ws_broken = true;
                Err(anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed before response completed",
                )))
            }
            Err(_) => Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("CDP operation timed out for {}", self.initial_target_url),
            ))),
        }
    }

    async fn read_until_reply(&mut self, awaited_id: i64, deadline: Instant) -> Result<String> {
        loop {
            if let Some(matched) = self.read_next_message(Some(awaited_id), deadline).await? {
                return Ok(matched);
            }
        }
    }

    async fn process_ws_message(
        &mut self,
        msg: Message,
        awaited_id: Option<i64>,
        reply_deadline: Instant,
    ) -> Result<Option<String>> {
        match msg {
            Message::Text(text) => {
                let raw = text.as_str();
                let header: RpcHeader = serde_json::from_str(raw).map_err(|e| {
                    anyhow::Error::new(TransportError::new(
                        TransportErrorKind::ProtocolMismatch,
                        format!("malformed JSON from CDP endpoint: {e}"),
                    ))
                })?;

                if let Some(msg_id) = header.id {
                    if self.ignored_reply_ids.remove(&msg_id) {
                        return Ok(None);
                    }
                    if awaited_id == Some(msg_id) {
                        return Ok(Some(raw.to_string()));
                    }
                    return Ok(None);
                }

                self.account_unsolicited(raw.len())?;

                if let Some(method) = header.method.as_deref() {
                    if self.session_id.as_deref() == header.session_id.as_deref() {
                        self.handle_event(method, raw, reply_deadline).await?;
                    }
                } else if let Some(err) = header.error {
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::ProtocolMismatch,
                        format!("CDP error ({}): {}", err.code, err.message),
                    )));
                }
                Ok(None)
            }
            Message::Binary(bin) => {
                self.account_unsolicited(bin.len())?;
                Ok(None)
            }
            Message::Ping(payload) => {
                self.account_unsolicited(payload.len())?;
                Ok(None)
            }
            Message::Pong(payload) => {
                self.account_unsolicited(payload.len())?;
                Ok(None)
            }
            Message::Close(_) => {
                self.ws_broken = true;
                Err(anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed by remote endpoint",
                )))
            }
            Message::Frame(_) => Ok(None),
        }
    }

    fn account_unsolicited(&mut self, byte_len: usize) -> Result<()> {
        self.unsolicited_events = self.unsolicited_events.saturating_add(1);
        self.unsolicited_event_bytes = self.unsolicited_event_bytes.saturating_add(byte_len);
        if self.unsolicited_events > MAX_UNSOLICITED_EVENTS
            || self.unsolicited_event_bytes > MAX_EVENT_BYTES
        {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::ResponseTooLarge,
                "CDP event stream exceeded safety bounds",
            )));
        }
        Ok(())
    }

    fn validate_event_url(&self, raw_url: &str, context: &str) -> Result<Url> {
        let parsed = Url::parse(raw_url).map_err(|_| {
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                format!("CDP blocked invalid URL during {context}"),
            ))
        })?;
        if !is_safe_redirect_url(&self.initial_target_url, &parsed) {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                format!("CDP blocked unsafe URL pivot during {context}"),
            )));
        }
        if is_url_self_ssrf(&parsed, &self.cdp_url)
            || is_ip_self_ssrf(&parsed, self.cdp_peer_ip, self.cdp_port)
        {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                "CDP blocked self-SSRF request to CDP endpoint",
            )));
        }
        if self.cdp_url.scheme() == "wss"
            && !self.allow_local_targets
            && parsed.host_str().is_some_and(is_local_or_private_host)
        {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                "remote wss:// CDP blocked local or private URL",
            )));
        }
        Ok(parsed)
    }

    async fn handle_event(
        &mut self,
        method: &str,
        raw: &str,
        reply_deadline: Instant,
    ) -> Result<()> {
        match method {
            "Fetch.requestPaused" => {
                let Ok(ev) =
                    serde_json::from_str::<RpcEventEnvelope<FetchRequestPausedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                let is_main_frame = params.resource_type.as_deref() == Some("Document")
                    || (params.frame_id.is_some()
                        && params.frame_id.as_deref() == self.state.main_frame_id.as_deref());

                match self.validate_event_url(&params.request.url, "Fetch.requestPaused") {
                    Ok(_) => {
                        let cmd_id = self.alloc_id();
                        self.ignored_reply_ids.insert(cmd_id);
                        let sid = self.session_id.clone();
                        self.send_raw(
                            cmd_id,
                            "Fetch.continueRequest",
                            serde_json::json!({ "requestId": params.request_id }),
                            sid.as_deref(),
                            reply_deadline,
                        )
                        .await?;
                    }
                    Err(err) => {
                        self.state
                            .blocked_interception_ids
                            .insert(params.request_id.clone());
                        let cmd_id = self.alloc_id();
                        self.ignored_reply_ids.insert(cmd_id);
                        let sid = self.session_id.clone();
                        self.send_raw(
                            cmd_id,
                            "Fetch.failRequest",
                            serde_json::json!({
                                "requestId": params.request_id,
                                "errorReason": "BlockedByClient"
                            }),
                            sid.as_deref(),
                            reply_deadline,
                        )
                        .await?;
                        if is_main_frame {
                            let te = err.downcast::<TransportError>().unwrap_or_else(|e| {
                                TransportError::new(
                                    TransportErrorKind::RedirectBlocked,
                                    e.to_string(),
                                )
                            });
                            self.state.terminal_error = Some(te);
                        }
                    }
                }
            }
            "Network.requestWillBeSent" => {
                let Ok(ev) = serde_json::from_str::<RpcEventEnvelope<RequestWillBeSentParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                let matches_main = self.state.main_frame_id.as_deref().is_some_and(|mf| {
                    params.frame_id.as_deref() == Some(mf)
                        && params.resource_type.as_deref() == Some("Document")
                });
                if !matches_main {
                    return Ok(());
                }
                self.state.main_request_id = Some(params.request_id);
                if let Some(redir) = params.redirect_response {
                    if let Err(err) =
                        self.validate_event_url(&redir.url, "Network.requestWillBeSent redirect")
                    {
                        self.set_terminal_error(err);
                        return Ok(());
                    }
                }
                if let Err(err) =
                    self.validate_event_url(&params.request.url, "Network.requestWillBeSent")
                {
                    self.set_terminal_error(err);
                }
            }
            "Network.responseReceived" => {
                let Ok(ev) = serde_json::from_str::<RpcEventEnvelope<ResponseReceivedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                let matches_main = self.state.main_frame_id.as_deref().is_some_and(|mf| {
                    params.frame_id.as_deref() == Some(mf)
                        && params.resource_type.as_deref() == Some("Document")
                }) || (self.state.main_request_id.as_deref()
                    == Some(params.request_id.as_str())
                    && params.resource_type.as_deref() == Some("Document"));
                if !matches_main {
                    return Ok(());
                }

                if let Err(err) =
                    self.validate_event_url(&params.response.url, "Network.responseReceived")
                {
                    self.set_terminal_error(err);
                    return Ok(());
                }

                self.state.status_code = params.response.status;
                if self.state.final_url.is_none() && params.response.url != "about:blank" {
                    self.state.final_url = Some(params.response.url);
                }

                let ct = header_case_insensitive(&params.response.headers, "content-type")
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        (!params.response.mime_type.trim().is_empty())
                            .then(|| params.response.mime_type.trim().to_string())
                    });
                if ct.is_some() {
                    self.state.content_type = ct;
                }

                if let Some(link) = header_case_insensitive(&params.response.headers, "link") {
                    let normalized = normalize_link_header(&link);
                    if !normalized.is_empty() {
                        self.state.link_header = Some(normalized);
                    }
                }
            }
            "Page.frameNavigated" => {
                let Ok(ev) = serde_json::from_str::<RpcEventEnvelope<FrameNavigatedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                if self.state.main_frame_id.as_deref() != Some(params.frame.id.as_str()) {
                    return Ok(());
                }
                let reconstructed = format!(
                    "{}{}",
                    params.frame.url,
                    params.frame.url_fragment.as_deref().unwrap_or("")
                );
                if reconstructed != "about:blank" {
                    if let Err(err) = self.validate_event_url(&reconstructed, "Page.frameNavigated")
                    {
                        self.set_terminal_error(err);
                        return Ok(());
                    }
                    self.state.final_url = Some(reconstructed);
                }
            }
            "Page.navigatedWithinDocument" => {
                let Ok(ev) =
                    serde_json::from_str::<RpcEventEnvelope<NavigatedWithinDocumentParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                if self.state.main_frame_id.as_deref() != Some(params.frame_id.as_str()) {
                    return Ok(());
                }
                if let Err(err) =
                    self.validate_event_url(&params.url, "Page.navigatedWithinDocument")
                {
                    self.set_terminal_error(err);
                    return Ok(());
                }
                self.state.final_url = Some(params.url);
            }
            "Page.loadEventFired" => {
                self.state.load_fired = true;
            }
            "Network.loadingFailed" => {
                let Ok(ev) = serde_json::from_str::<RpcEventEnvelope<LoadingFailedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                if self.state.main_request_id.as_deref() == Some(params.request_id.as_str())
                    && self.state.terminal_error.is_none()
                {
                    let kind = if params.error_text == "net::ERR_BLOCKED_BY_CLIENT"
                        && !self.state.blocked_interception_ids.is_empty()
                    {
                        TransportErrorKind::RedirectBlocked
                    } else {
                        TransportErrorKind::LoadError
                    };
                    self.state.terminal_error = Some(TransportError::new(
                        kind,
                        format!("CDP document load failed: {}", params.error_text),
                    ));
                }
            }
            "WebMCP.toolsAdded" => {
                let Ok(ev) = serde_json::from_str::<RpcEventEnvelope<WebMcpToolsAddedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                let Some(main_fid) = self.state.main_frame_id.as_deref() else {
                    return Ok(());
                };
                for tool in params.tools {
                    if tool.frame_id.as_deref() != Some(main_fid) {
                        continue;
                    }
                    let source = if tool.backend_node_id.is_some_and(|id| id > 0) {
                        WebMcpToolSource::Declarative
                    } else {
                        WebMcpToolSource::Imperative
                    };
                    let annotations = WebMcpToolAnnotations {
                        read_only: tool.annotations.as_ref().and_then(|a| a.read_only),
                        autosubmit: tool.annotations.as_ref().and_then(|a| a.autosubmit),
                        untrusted_content: Some(true),
                    };
                    let descriptor = WebMcpToolDescriptor {
                        name: tool.name,
                        description: tool.description,
                        title: tool.title,
                        source,
                        input_schema: tool.input_schema,
                        annotations: Some(annotations),
                        action_url: None,
                        method: None,
                    };
                    if let Some(clean) = sanitize_webmcp_tool(descriptor) {
                        if let Some(existing) = self
                            .state
                            .cdp_tools
                            .iter_mut()
                            .find(|t| t.name == clean.name)
                        {
                            *existing = clean;
                        } else {
                            self.state.cdp_tools.push(clean);
                        }
                    }
                }
            }
            "WebMCP.toolsRemoved" => {
                let Ok(ev) =
                    serde_json::from_str::<RpcEventEnvelope<WebMcpToolsRemovedParams>>(raw)
                else {
                    return Ok(());
                };
                let Some(params) = ev.params else {
                    return Ok(());
                };
                let Some(main_fid) = self.state.main_frame_id.as_deref() else {
                    return Ok(());
                };
                for removed in params.tools {
                    if removed.frame_id.as_deref() == Some(main_fid) {
                        self.state.cdp_tools.retain(|t| t.name != removed.name);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn set_terminal_error(&mut self, err: anyhow::Error) {
        if self.state.terminal_error.is_none() {
            let te = err.downcast::<TransportError>().unwrap_or_else(|e| {
                TransportError::new(TransportErrorKind::RedirectBlocked, e.to_string())
            });
            self.state.terminal_error = Some(te);
        }
    }

    async fn execute(&mut self, options: &CdpRenderOptions) -> Result<CdpRenderResult> {
        if !self.is_page_endpoint {
            let created: CreateTargetReply = self
                .call_required(
                    "Target.createTarget",
                    serde_json::json!({ "url": "about:blank" }),
                    false,
                )
                .await?;
            self.created_target_id = Some(created.target_id.clone());
            self.owns_target = true;

            let attached: AttachTargetReply = self
                .call_required(
                    "Target.attachToTarget",
                    serde_json::json!({
                        "targetId": created.target_id,
                        "flatten": true
                    }),
                    false,
                )
                .await?;
            self.session_id = Some(attached.session_id);
        } else if !options.capture_axtree {
            let _ = self
                .call_best_effort(
                    "Kitesurf.setLayoutMode",
                    serde_json::json!({ "mode": "off" }),
                    false,
                )
                .await?;
        }

        // Initialize domains sequentially before Page.navigate.
        let _: EmptyReply = self
            .call_required("Page.enable", serde_json::json!({}), true)
            .await?;
        let _: EmptyReply = self
            .call_required("Network.enable", serde_json::json!({}), true)
            .await?;

        let frame_tree: FrameTreeReply = self
            .call_required("Page.getFrameTree", serde_json::json!({}), true)
            .await?;
        if frame_tree.frame_tree.frame.id.is_empty() {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                "Page.getFrameTree returned an empty main frame id",
            )));
        }
        self.state.main_frame_id = Some(frame_tree.frame_tree.frame.id);

        let _ = self
            .call_best_effort(
                "Fetch.enable",
                serde_json::json!({
                    "patterns": [{ "urlPattern": "*", "requestStage": "Request" }]
                }),
                true,
            )
            .await?;
        let _ = self
            .call_best_effort("WebMCP.enable", serde_json::json!({}), true)
            .await?;

        // Navigate and process events.
        let nav: NavigateReply = self
            .call_required(
                "Page.navigate",
                serde_json::json!({ "url": self.initial_target_url.as_str() }),
                true,
            )
            .await?;

        if let Some(err) = self.state.terminal_error.take() {
            return Err(anyhow::Error::new(err));
        }

        if let Some(err_text) = nav.error_text.filter(|s| !s.is_empty()) {
            let kind = if err_text == "net::ERR_BLOCKED_BY_CLIENT"
                || err_text == "net::ERR_BLOCKED_BY_RESPONSE"
                || err_text == "net::ERR_ACCESS_DENIED"
            {
                if !self.state.blocked_interception_ids.is_empty() {
                    TransportErrorKind::RedirectBlocked
                } else {
                    TransportErrorKind::LoadError
                }
            } else {
                TransportErrorKind::LoadError
            };
            return Err(anyhow::Error::new(TransportError::new(
                kind,
                format!("CDP navigation failed: {err_text}"),
            )));
        }

        if self.state.main_request_id.is_none() {
            self.state.main_request_id = nav.loader_id;
        }

        while !self.state.load_fired {
            if let Some(err) = self.state.terminal_error.take() {
                return Err(anyhow::Error::new(err));
            }
            let _ = self.read_next_message(None, self.deadline).await?;
        }

        if let Some(selector) = options.wait_for_selector.as_deref() {
            self.wait_for_selector(selector).await?;
        }

        self.settle_page().await?;

        let outer_html = self.read_outer_html().await?;
        let max_bytes = if options.max_response_bytes > 0 {
            options.max_response_bytes
        } else {
            MAX_HTML_BYTES
        };
        if outer_html.len() > max_bytes {
            return Err(anyhow::Error::new(ResponseTooLargeError {
                limit_bytes: max_bytes,
            }));
        }

        let axtree_text = if options.capture_axtree {
            Some(self.read_axtree(max_bytes).await?)
        } else {
            None
        };

        let final_url = self
            .state
            .final_url
            .clone()
            .unwrap_or_else(|| self.initial_target_url.to_string());
        let imperative_tools: Vec<WebMcpToolDescriptor> = self
            .state
            .cdp_tools
            .iter()
            .filter(|t| t.source == WebMcpToolSource::Imperative)
            .cloned()
            .collect();

        Ok(CdpRenderResult {
            html: outer_html,
            final_url,
            status_code: self.state.status_code,
            content_type: self.state.content_type.clone(),
            link_header: self.state.link_header.clone(),
            axtree_text,
            imperative_tools,
        })
    }

    async fn call_best_effort<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
        use_session: bool,
    ) -> Result<bool> {
        match self
            .call_typed::<_, serde_json::Value>(method, params, use_session)
            .await?
        {
            RpcCallOutcome::Ok(_) => Ok(true),
            RpcCallOutcome::RpcErr(err) => {
                if err.code == CDP_ERR_METHOD_NOT_FOUND {
                    Ok(false)
                } else {
                    tracing::debug!(
                        "CDP best-effort method {method} returned error ({}): {}",
                        err.code,
                        err.message
                    );
                    Ok(false)
                }
            }
        }
    }

    async fn wait_for_selector(&mut self, selector: &str) -> Result<()> {
        loop {
            let doc_reply = self
                .call_typed::<_, GetDocumentReply>(
                    "DOM.getDocument",
                    serde_json::json!({ "depth": 0 }),
                    true,
                )
                .await?;
            let root_id = match doc_reply {
                RpcCallOutcome::Ok(doc) => doc.root.node_id,
                RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => 0,
                RpcCallOutcome::RpcErr(err) => {
                    let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
                        TransportErrorKind::ProtocolMismatch
                    } else {
                        TransportErrorKind::LoadError
                    };
                    return Err(anyhow::Error::new(TransportError::new(
                        kind,
                        format!("DOM.getDocument failed ({}): {}", err.code, err.message),
                    )));
                }
            };

            if root_id > 0 {
                let query_reply = self
                    .call_typed::<_, QuerySelectorReply>(
                        "DOM.querySelector",
                        serde_json::json!({ "nodeId": root_id, "selector": selector }),
                        true,
                    )
                    .await?;
                match query_reply {
                    RpcCallOutcome::Ok(res) => {
                        if let Some(err_msg) = res.error.filter(|s| !s.is_empty()) {
                            return Err(anyhow::Error::new(TransportError::new(
                                TransportErrorKind::InvalidSelector,
                                format!("invalid CSS selector {selector:?}: {err_msg}"),
                            )));
                        }
                        if res.node_id > 0 {
                            return Ok(());
                        }
                    }
                    RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => {
                        if err.message.contains("DOM Error while querying") {
                            return Err(anyhow::Error::new(TransportError::new(
                                TransportErrorKind::InvalidSelector,
                                format!("invalid CSS selector {selector:?}: {}", err.message),
                            )));
                        }
                        // Otherwise transient stale node id; retry DOM.getDocument next tick.
                    }
                    RpcCallOutcome::RpcErr(err) => {
                        let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
                            TransportErrorKind::ProtocolMismatch
                        } else {
                            TransportErrorKind::InvalidSelector
                        };
                        return Err(anyhow::Error::new(TransportError::new(
                            kind,
                            format!("DOM.querySelector failed ({}): {}", err.code, err.message),
                        )));
                    }
                }
            }

            if Instant::now() >= self.deadline {
                return Err(anyhow::Error::new(TransportError::new(
                    TransportErrorKind::Timeout,
                    format!(
                        "timed out waiting for selector {selector:?} on {}",
                        self.initial_target_url
                    ),
                )));
            }
            self.pump_until(Instant::now() + SELECTOR_POLL_INTERVAL)
                .await?;
        }
    }

    async fn settle_page(&mut self) -> Result<()> {
        let params = match &self.created_target_id {
            Some(tid) if self.owns_target => serde_json::json!({ "targetId": tid }),
            _ => serde_json::json!({}),
        };

        let first = self
            .call_typed::<_, PageStateReply>("Kitesurf.getPageState", params.clone(), true)
            .await?;
        let mut last_seq = match first {
            RpcCallOutcome::Ok(reply)
                if reply.error.as_deref().is_none_or(str::is_empty) && reply.seq.is_some() =>
            {
                reply.seq.expect("checked is_some")
            }
            _ => {
                self.pump_until(Instant::now() + PAGE_STATE_FALLBACK_SETTLE)
                    .await?;
                return Ok(());
            }
        };

        let settle_deadline = self.deadline.min(Instant::now() + PAGE_STATE_MAX_WAIT);
        let mut unchanged_ticks = 0usize;
        while Instant::now() < settle_deadline {
            let next_tick = settle_deadline.min(Instant::now() + PAGE_STATE_POLL_INTERVAL);
            self.pump_until(next_tick).await?;
            if Instant::now() >= settle_deadline {
                break;
            }
            match self
                .call_typed::<_, PageStateReply>("Kitesurf.getPageState", params.clone(), true)
                .await?
            {
                RpcCallOutcome::Ok(reply)
                    if reply.error.as_deref().is_none_or(str::is_empty) && reply.seq.is_some() =>
                {
                    let seq = reply.seq.expect("checked is_some");
                    if seq == last_seq {
                        unchanged_ticks += 1;
                        if unchanged_ticks >= 2 {
                            break;
                        }
                    } else {
                        last_seq = seq;
                        unchanged_ticks = 0;
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    async fn read_outer_html(&mut self) -> Result<String> {
        for attempt in 0..2 {
            let doc: GetDocumentReply = self
                .call_required("DOM.getDocument", serde_json::json!({ "depth": 0 }), true)
                .await?;
            if doc.root.node_id <= 0 {
                if attempt == 0 {
                    continue;
                }
                return Err(anyhow::Error::new(TransportError::new(
                    TransportErrorKind::ProtocolMismatch,
                    "DOM.getDocument returned nodeId 0",
                )));
            }
            match self
                .call_typed::<_, OuterHtmlReply>(
                    "DOM.getOuterHTML",
                    serde_json::json!({ "nodeId": doc.root.node_id }),
                    true,
                )
                .await?
            {
                RpcCallOutcome::Ok(reply) => return Ok(reply.outer_html),
                RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR && attempt == 0 => {
                    continue;
                }
                RpcCallOutcome::RpcErr(err) => {
                    let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
                        TransportErrorKind::ProtocolMismatch
                    } else {
                        TransportErrorKind::LoadError
                    };
                    return Err(anyhow::Error::new(TransportError::new(
                        kind,
                        format!("DOM.getOuterHTML failed ({}): {}", err.code, err.message),
                    )));
                }
            }
        }
        Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::LoadError,
            "DOM.getOuterHTML failed after retry",
        )))
    }

    async fn read_axtree(&mut self, max_bytes: usize) -> Result<String> {
        match self
            .call_typed::<_, serde_json::Value>(
                "Accessibility.getFullAXTree",
                serde_json::json!({}),
                true,
            )
            .await?
        {
            RpcCallOutcome::Ok(val) => {
                let raw_json = serde_json::to_string(&val).map_err(|e| {
                    anyhow::Error::new(TransportError::new(
                        TransportErrorKind::ProtocolMismatch,
                        format!("failed to serialize Accessibility.getFullAXTree payload: {e}"),
                    ))
                })?;
                crate::extract::axtree::format_axtree_from_cdp_json(&raw_json)
            }
            RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => {
                Err(anyhow::Error::new(ResponseTooLargeError {
                    limit_bytes: max_bytes,
                }))
            }
            RpcCallOutcome::RpcErr(err) => {
                let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
                    TransportErrorKind::ProtocolMismatch
                } else {
                    TransportErrorKind::LoadError
                };
                Err(anyhow::Error::new(TransportError::new(
                    kind,
                    format!(
                        "Accessibility.getFullAXTree failed ({}): {}",
                        err.code, err.message
                    ),
                )))
            }
        }
    }

    async fn teardown(&mut self) {
        if self.ws_broken {
            return;
        }
        let teardown_deadline = Instant::now() + TEARDOWN_GRACE;
        if self.owns_target {
            if let Some(tid) = self.created_target_id.take() {
                let id = self.alloc_id();
                if self
                    .send_raw(
                        id,
                        "Target.closeTarget",
                        serde_json::json!({ "targetId": tid }),
                        None,
                        teardown_deadline,
                    )
                    .await
                    .is_ok()
                {
                    let _ = self.read_until_reply(id, teardown_deadline).await;
                }
            }
        }
        let _ = tokio::time::timeout_at(teardown_deadline, self.ws.close(None)).await;
    }
}

fn header_case_insensitive(headers: &HashMap<String, String>, needle: &str) -> Option<String> {
    let mut matches = Vec::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(needle) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                matches.push(trimmed);
            }
        }
    }
    if matches.is_empty() {
        None
    } else {
        Some(matches.join(", "))
    }
}

fn normalize_link_header(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{set_credential, CredentialStorage};
    use crate::transport::transport_error_kind;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_state_dir() -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!(
            "/tmp/pleb-t-{}-cdpunit-{}",
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir
    }

    #[test]
    fn cdp_url_validation_redaction_and_layout_param_rules() {
        assert!(validate_cdp_url("ws://127.0.0.1:8787/devtools/browser?token=secret").is_ok());
        assert!(validate_cdp_url("ws://localhost:9222/devtools/page/abc").is_ok());
        assert!(validate_cdp_url("ws://[::1]:8787/devtools/browser").is_ok());
        assert!(validate_cdp_url("wss://kitesurf.example.com/devtools/browser?token=abc").is_ok());

        for bad in [
            "",
            "http://127.0.0.1:8787/devtools/browser",
            "ws://example.com/devtools/browser",
            "ws://10.0.0.5:8787/devtools/browser",
            "wss://user:pass@kitesurf.example.com/devtools/browser",
            "wss://kitesurf.example.com/devtools/browser#frag",
        ] {
            assert!(validate_cdp_url(bad).is_err(), "expected invalid: {bad}");
        }

        let renderer = CdpRenderer::new(
            "wss://Kitesurf.Example.com:8443/devtools/browser?token=supersecret",
            PathBuf::from("/tmp"),
            None,
        );
        let displayed = renderer.cdp_url_display();
        assert_eq!(
            displayed,
            "wss://kitesurf.example.com:8443/devtools/browser"
        );
        assert!(!displayed.contains("supersecret"));
        let dbg = format!("{renderer:?}");
        assert!(!dbg.contains("supersecret"));

        let browser_url = Url::parse("ws://127.0.0.1:8787/devtools/browser").unwrap();
        assert_eq!(
            prepare_cdp_ws_url(&browser_url, false, false).as_str(),
            "ws://127.0.0.1:8787/devtools/browser?layout=off"
        );
        assert_eq!(
            prepare_cdp_ws_url(&browser_url, true, false).as_str(),
            "ws://127.0.0.1:8787/devtools/browser"
        );
        let page_url = Url::parse("ws://127.0.0.1:8787/devtools/page/123").unwrap();
        assert_eq!(
            prepare_cdp_ws_url(&page_url, false, true).as_str(),
            "ws://127.0.0.1:8787/devtools/page/123"
        );
    }

    #[test]
    fn cdp_access_headers_are_host_scoped_and_fail_on_partial_tuple() {
        let dir = temp_state_dir();
        let wss_match = Url::parse("wss://worker.example.com/devtools/browser").unwrap();
        let wss_other = Url::parse("wss://other.example.com/devtools/browser").unwrap();
        let wss_port_match = Url::parse("wss://worker.example.com:8443/devtools/browser").unwrap();

        // None set -> Ok(None).
        assert!(resolve_cdp_access_headers(&dir, &wss_match)
            .unwrap()
            .is_none());

        // Partial tuple -> fail loud.
        set_credential(
            &dir,
            CredentialProvider::CdpAccessClientId,
            "id-123",
            CredentialStorage::File,
        )
        .unwrap();
        let err = resolve_cdp_access_headers(&dir, &wss_match).unwrap_err();
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::Network)
        );

        // Complete tuple -> attaches only on matching host (case-insensitive).
        set_credential(
            &dir,
            CredentialProvider::CdpAccessClientSecret,
            "secret-456",
            CredentialStorage::File,
        )
        .unwrap();
        set_credential(
            &dir,
            CredentialProvider::CdpAccessHost,
            "Worker.Example.Com",
            CredentialStorage::File,
        )
        .unwrap();

        let headers = resolve_cdp_access_headers(&dir, &wss_match)
            .unwrap()
            .unwrap();
        assert_eq!(headers.0, "id-123");
        assert_eq!(headers.1, "secret-456");

        assert!(resolve_cdp_access_headers(&dir, &wss_other)
            .unwrap()
            .is_none());

        // Host:port restriction checks port too.
        set_credential(
            &dir,
            CredentialProvider::CdpAccessHost,
            "worker.example.com:8443",
            CredentialStorage::File,
        )
        .unwrap();
        assert!(resolve_cdp_access_headers(&dir, &wss_port_match)
            .unwrap()
            .is_some());
        assert!(resolve_cdp_access_headers(&dir, &wss_match)
            .unwrap()
            .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_101_http_upgrade_error_omits_headers_and_body() {
        let mut resp = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .header("Set-Cookie", "CF_Authorization=super-secret-cookie")
            .body(Some(
                b"<html><body>Access Denied Secret Body</body></html>".to_vec(),
            ))
            .unwrap();
        *resp.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
        let mapped = map_tungstenite_error(
            WsError::Http(resp),
            "https://example.com?token=secret-query",
        );
        assert_eq!(
            transport_error_kind(&mapped),
            Some(TransportErrorKind::Network)
        );
        let msg = mapped.to_string();
        assert!(msg.contains("403"));
        assert!(!msg.contains("super-secret-cookie"));
        assert!(!msg.contains("Access Denied"));
        assert!(!msg.contains("secret-query"));
    }
}
