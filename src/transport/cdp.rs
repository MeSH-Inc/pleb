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
use url::Url;

use super::{ResponseTooLargeError, TransportError, TransportErrorKind};
use crate::auth::{resolve_credentials, CredentialProvider};
use crate::extract::is_html_content_type;
use crate::extract::webmcp::sanitize_webmcp_tool;
use crate::extract::{WebMcpToolAnnotations, WebMcpToolDescriptor, WebMcpToolSource};
use crate::policy::source::canonical_url_host;
use crate::policy::{
    classify_response_with_headers, detect_interstitial_challenge, is_local_or_private_host,
    is_loopback_host, is_safe_redirect_url, ResponseClassification,
};
use crate::transport::reveal::{
    build_cdp_reveal_eval_js, is_indexable_json_mime, BrowserCaptureMetadata, CapturedDataset,
    RevealPolicy, RevealedPanelSnapshot, MAX_RAW_CAPTURED_DATASETS,
};
use crate::transport::DiagnosticHeaders;

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
    pub reveal: RevealPolicy,
    pub scroll_steps: usize,
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
    pub capture_meta: BrowserCaptureMetadata,
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
        let mut guard = self
            .tls_config
            .lock()
            .map_err(|_| cdp_err(TransportErrorKind::Network, "CDP TLS config lock poisoned"))?;
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
            return Err(cdp_err(
                TransportErrorKind::Network,
                "no native root CA certificates available for wss:// CDP connection",
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| {
                cdp_err(
                    TransportErrorKind::Network,
                    format!("failed to initialize CDP TLS protocol versions: {e}"),
                )
            })?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let arc_cfg = Arc::new(cfg);
        *guard = Some(Arc::clone(&arc_cfg));
        Ok(arc_cfg)
    }

    async fn connect_cdp_ws(
        &self,
        ws_url: &Url,
        target_url: Option<&Url>,
        display_target: &str,
        deadline: Instant,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, IpAddr, u16)> {
        let access_headers = if ws_url.scheme() == "wss" {
            resolve_cdp_access_headers(&self.state_dir, ws_url)?
        } else {
            None
        };

        let host = ws_url
            .host_str()
            .ok_or_else(|| cdp_err(TransportErrorKind::Network, "CDP URL must include a host"))?;
        let port = ws_url.port_or_known_default().ok_or_else(|| {
            cdp_err(
                TransportErrorKind::Network,
                "CDP URL must include a valid port",
            )
        })?;
        let connect_addr = format!("{host}:{port}");

        let tcp_stream = run_before(
            deadline,
            TcpStream::connect(&connect_addr),
            "CDP TCP connect",
            display_target,
        )
        .await
        .map_err(|e| map_connect_io_or_transport(e, display_target))?;

        let peer_ip = tcp_stream
            .peer_addr()
            .map_err(|e| {
                cdp_err(
                    TransportErrorKind::Network,
                    format!("failed to inspect CDP socket peer address: {e}"),
                )
            })?
            .ip();

        if ws_url.scheme() == "ws" && !is_loopback_ip(peer_ip) {
            return Err(cdp_err(
                TransportErrorKind::RedirectBlocked,
                "plaintext ws:// CDP connection resolved to non-loopback IP address",
            ));
        }

        if let Some(target) = target_url {
            if is_ip_self_ssrf(target, peer_ip, port) {
                return Err(cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "target URL resolves to the configured CDP endpoint (self-SSRF blocked)",
                ));
            }
        }

        let mut request = ws_url.as_str().into_client_request().map_err(|_| {
            cdp_err(
                TransportErrorKind::Network,
                "invalid CDP WebSocket upgrade request",
            )
        })?;
        if let Some((client_id, client_secret)) = access_headers {
            let id_val = HeaderValue::from_str(&client_id).map_err(|_| {
                cdp_err(
                    TransportErrorKind::Network,
                    "invalid CF-Access-Client-Id header value",
                )
            })?;
            let secret_val = HeaderValue::from_str(&client_secret).map_err(|_| {
                cdp_err(
                    TransportErrorKind::Network,
                    "invalid CF-Access-Client-Secret header value",
                )
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
            display_target,
        )
        .await
        .map_err(|e| map_ws_or_transport_error(e, display_target))?;

        Ok((ws_stream, peer_ip, port))
    }

    pub async fn render(
        &self,
        url: &str,
        options: &CdpRenderOptions,
        timeout: Duration,
    ) -> Result<CdpRenderResult> {
        if self.proxy.is_some() {
            return Err(cdp_err(
                TransportErrorKind::Network,
                "CDP renderer refuses to connect while --proxy is configured",
            ));
        }

        let cdp_url = validate_cdp_url(&self.cdp_url)?;
        let target_url = validate_target_url(url, &cdp_url, options)?;

        if let Some(selector) = options.wait_for_selector.as_deref() {
            scraper::Selector::parse(selector).map_err(|_| {
                cdp_err(
                    TransportErrorKind::InvalidSelector,
                    format!("invalid CSS selector for --wait-for-selector: {selector}"),
                )
            })?;
        }

        if timeout.is_zero() {
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                format!("CDP render timed out before connecting for {url}"),
            ));
        }

        let deadline = Instant::now() + timeout;
        let is_page_endpoint = cdp_url.path().starts_with("/devtools/page/");
        let needs_layout = options.capture_axtree
            || options.reveal != RevealPolicy::None
            || options.scroll_steps > 0;
        let ws_url = prepare_cdp_ws_url(&cdp_url, needs_layout, is_page_endpoint);

        let (ws_stream, peer_ip, port) = self
            .connect_cdp_ws(&ws_url, Some(&target_url), url, deadline)
            .await?;

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

    /// Snapshot an already-loaded loopback `/devtools/page/<id>` tab passively without navigating,
    /// mutating layout, intercepting requests, or closing the target.
    pub async fn render_attached(
        &self,
        expected_origin: Option<&str>,
        options: &CdpRenderOptions,
        timeout: Duration,
    ) -> Result<CdpRenderResult> {
        if self.proxy.is_some() {
            return Err(cdp_err(
                TransportErrorKind::Network,
                "CDP renderer refuses to connect while --proxy is configured",
            ));
        }

        let cdp_url = validate_loopback_page_cdp_url(&self.cdp_url)?;
        let expected_url = match expected_origin {
            Some(raw) => Some(validate_target_url(raw, &cdp_url, options)?),
            None => None,
        };

        if let Some(selector) = options.wait_for_selector.as_deref() {
            scraper::Selector::parse(selector).map_err(|_| {
                cdp_err(
                    TransportErrorKind::InvalidSelector,
                    format!("invalid CSS selector for --wait-for-selector: {selector}"),
                )
            })?;
        }

        if timeout.is_zero() {
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                "CDP passive attach timed out before connecting",
            ));
        }

        let display_target = expected_origin.unwrap_or("attached-cdp-page");
        let deadline = Instant::now() + timeout;
        let (ws_stream, peer_ip, port) = self
            .connect_cdp_ws(&cdp_url, expected_url.as_ref(), display_target, deadline)
            .await?;

        let mut session =
            PassiveAttachSession::new(ws_stream, deadline, cdp_url, peer_ip, port, expected_url);

        let outcome = session.execute(options).await;
        session.teardown().await;
        outcome
    }
}

/// Validate a loopback `ws://` `/devtools/page/<nonempty-id>` CDP endpoint for passive attach.
pub fn validate_loopback_page_cdp_url(raw: &str) -> Result<Url> {
    let parsed = validate_cdp_url(raw)?;
    if parsed.scheme() != "ws" {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "passive --no-navigate attach supports only loopback ws:// CDP endpoints",
        ));
    }
    let Some(page_id) = parsed.path().strip_prefix("/devtools/page/") else {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "passive --no-navigate attach requires a /devtools/page/<id> CDP endpoint",
        ));
    };
    if page_id.is_empty() || page_id.contains('/') {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "passive --no-navigate attach requires a non-empty single-segment page ID in /devtools/page/<id>",
        ));
    }
    Ok(parsed)
}

pub fn validate_cdp_url(raw: &str) -> Result<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "CDP URL must not be empty",
        ));
    }
    let parsed =
        Url::parse(trimmed).map_err(|_| cdp_err(TransportErrorKind::Network, "invalid CDP URL"))?;
    let scheme = parsed.scheme();
    if scheme != "ws" && scheme != "wss" {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "CDP URL scheme must be ws:// or wss://",
        ));
    }
    let Some(host) = parsed.host_str() else {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "CDP URL must include a host",
        ));
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "CDP URL must not embed userinfo credentials",
        ));
    }
    if parsed.fragment().is_some() {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "CDP URL must not include a fragment",
        ));
    }
    if scheme == "ws" && !is_loopback_host(host) {
        return Err(cdp_err(
            TransportErrorKind::Network,
            "plaintext ws:// CDP URL is permitted for loopback hosts only; use wss:// for remote endpoints",
        ));
    }
    Ok(parsed)
}

pub fn sanitize_cdp_url_for_display(raw: &str) -> String {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return "[invalid-cdp-url]".to_string();
    };
    let Some(formatted_host) = canonical_url_host(&parsed) else {
        return "[invalid-cdp-url]".to_string();
    };
    match parsed.port() {
        Some(port) => format!(
            "{}://{formatted_host}:{port}{}",
            parsed.scheme(),
            parsed.path()
        ),
        None => format!("{}://{formatted_host}{}", parsed.scheme(), parsed.path()),
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

fn prepare_cdp_ws_url(cdp_url: &Url, needs_layout: bool, is_page_endpoint: bool) -> Url {
    let mut ws_url = cdp_url.clone();
    if !needs_layout && !is_page_endpoint {
        let has_layout = ws_url.query_pairs().any(|(k, _)| k == "layout");
        if !has_layout {
            ws_url.query_pairs_mut().append_pair("layout", "off");
        }
    }
    ws_url
}

fn origin_and_path_changed(before: &str, after: &str) -> bool {
    let Ok(b) = Url::parse(before) else {
        return before != after;
    };
    let Ok(a) = Url::parse(after) else {
        return true;
    };
    b.scheme() != a.scheme()
        || b.host_str() != a.host_str()
        || b.port_or_known_default() != a.port_or_known_default()
        || b.path() != a.path()
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
        cdp_err(
            TransportErrorKind::Network,
            format!("failed to resolve CDP Access credentials: {e}"),
        )
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
            Err(cdp_err(
                TransportErrorKind::Network,
                format!(
                    "incomplete CDP Access configuration; missing: {}",
                    missing.join(", ")
                ),
            ))
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
    let (Some(allowed_host_norm), Some(cdp_host_norm)) = (
        canonical_url_host(&allowed_parsed),
        canonical_url_host(cdp_url),
    ) else {
        return false;
    };
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
        cdp_err(
            TransportErrorKind::LoadError,
            format!("invalid target URL: {e}"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(cdp_err(
            TransportErrorKind::LoadError,
            format!(
                "unsupported URL scheme {:?} for CDP render (only http and https)",
                parsed.scheme()
            ),
        ));
    }
    let Some(target_host) = parsed.host_str() else {
        return Err(cdp_err(
            TransportErrorKind::LoadError,
            "target URL must include a host",
        ));
    };
    if is_url_self_ssrf(&parsed, cdp_url) {
        return Err(cdp_err(
            TransportErrorKind::RedirectBlocked,
            "target URL matches the configured CDP endpoint (self-SSRF blocked)",
        ));
    }
    if cdp_url.scheme() == "wss"
        && !options.allow_local_targets
        && is_local_or_private_host(target_host)
    {
        return Err(cdp_err(
            TransportErrorKind::RedirectBlocked,
            "remote wss:// CDP renderer refuses local or private target URLs",
        ));
    }
    Ok(parsed)
}

fn is_url_self_ssrf(target: &Url, cdp_url: &Url) -> bool {
    let (Some(target_norm), Some(cdp_norm)) =
        (canonical_url_host(target), canonical_url_host(cdp_url))
    else {
        return false;
    };
    let (Some(target_port), Some(cdp_port)) = (
        target.port_or_known_default(),
        cdp_url.port_or_known_default(),
    ) else {
        return false;
    };
    target_port == cdp_port
        && (target_norm == cdp_norm
            || (is_loopback_host(&target_norm) && is_loopback_host(&cdp_norm)))
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

fn cdp_err(kind: TransportErrorKind, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(TransportError::new(kind, msg))
}

fn rpc_failure_err(method: &str, err: &RpcError, other_kind: TransportErrorKind) -> anyhow::Error {
    let kind = if err.code == CDP_ERR_METHOD_NOT_FOUND {
        TransportErrorKind::ProtocolMismatch
    } else {
        other_kind
    };
    cdp_err(
        kind,
        format!("{method} failed ({}): {}", err.code, err.message),
    )
}

fn parse_event_params<T: for<'de> Deserialize<'de>>(raw: &str) -> Option<T> {
    serde_json::from_str::<RpcEventEnvelope<T>>(raw)
        .ok()
        .and_then(|ev| ev.params)
}

fn ingest_webmcp_tools_added(
    raw: &str,
    main_frame_id: Option<&str>,
    cdp_tools: &mut Vec<WebMcpToolDescriptor>,
) {
    let Some(main_fid) = main_frame_id else {
        return;
    };
    let Some(params) = parse_event_params::<WebMcpToolsAddedParams>(raw) else {
        return;
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
            if let Some(existing) = cdp_tools.iter_mut().find(|t| t.name == clean.name) {
                *existing = clean;
            } else {
                cdp_tools.push(clean);
            }
        }
    }
}

fn remove_webmcp_tools(
    raw: &str,
    main_frame_id: Option<&str>,
    cdp_tools: &mut Vec<WebMcpToolDescriptor>,
) {
    let Some(main_fid) = main_frame_id else {
        return;
    };
    let Some(params) = parse_event_params::<WebMcpToolsRemovedParams>(raw) else {
        return;
    };
    for removed in params.tools {
        if removed.frame_id.as_deref() == Some(main_fid) {
            cdp_tools.retain(|t| t.name != removed.name);
        }
    }
}

fn format_bounded_axtree_outcome(
    outcome: RpcCallOutcome<serde_json::Value>,
    max_bytes: usize,
) -> Result<String> {
    match outcome {
        RpcCallOutcome::Ok(val) => {
            let raw_json = serde_json::to_string(&val).map_err(|e| {
                cdp_err(
                    TransportErrorKind::ProtocolMismatch,
                    format!("failed to serialize Accessibility.getFullAXTree payload: {e}"),
                )
            })?;
            if raw_json.len() > max_bytes {
                return Err(anyhow::Error::new(ResponseTooLargeError {
                    limit_bytes: max_bytes,
                }));
            }
            crate::extract::axtree::format_axtree_from_cdp_json(&raw_json)
        }
        RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => {
            Err(anyhow::Error::new(ResponseTooLargeError {
                limit_bytes: max_bytes,
            }))
        }
        RpcCallOutcome::RpcErr(err) => Err(rpc_failure_err(
            "Accessibility.getFullAXTree",
            &err,
            TransportErrorKind::LoadError,
        )),
    }
}

fn map_connect_io_or_transport(err: anyhow::Error, _url: &str) -> anyhow::Error {
    if err.is::<TransportError>() {
        return err;
    }
    cdp_err(
        TransportErrorKind::Network,
        format!("CDP TCP connect failed: {err}"),
    )
}

fn map_ws_or_transport_error(err: anyhow::Error, url: &str) -> anyhow::Error {
    if err.is::<TransportError>() {
        return err;
    }
    match err.downcast::<WsError>() {
        Ok(ws_err) => map_tungstenite_error(ws_err, url),
        Err(other) => cdp_err(
            TransportErrorKind::Network,
            format!("CDP WebSocket error: {other}"),
        ),
    }
}

fn map_tungstenite_error(err: WsError, _url: &str) -> anyhow::Error {
    match err {
        WsError::Http(resp) => cdp_err(
            TransportErrorKind::Network,
            format!(
                "CDP WebSocket upgrade failed with HTTP {}",
                resp.status().as_u16()
            ),
        ),
        WsError::Capacity(_) => cdp_err(
            TransportErrorKind::ResponseTooLarge,
            "CDP WebSocket message or frame exceeded 64 MiB limit",
        ),
        WsError::Protocol(proto) => cdp_err(
            TransportErrorKind::ProtocolMismatch,
            format!("CDP WebSocket protocol error: {proto}"),
        ),
        WsError::Tls(_) => cdp_err(
            TransportErrorKind::Network,
            "CDP WebSocket TLS handshake or transport failed",
        ),
        WsError::Io(io_err) => cdp_err(
            TransportErrorKind::Network,
            format!("CDP WebSocket I/O error: {}", io_err.kind()),
        ),
        WsError::Url(_) | WsError::HttpFormat(_) => cdp_err(
            TransportErrorKind::Network,
            "invalid CDP WebSocket URL or handshake headers",
        ),
        WsError::ConnectionClosed | WsError::AlreadyClosed => cdp_err(
            TransportErrorKind::Network,
            "CDP WebSocket connection closed",
        ),
        WsError::Utf8 | WsError::AttackAttempt | WsError::WriteBufferFull(_) => {
            cdp_err(TransportErrorKind::Network, "CDP WebSocket transport error")
        }
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
    xhr_get_requests: HashSet<String>,
    captured_datasets: Vec<CapturedDataset>,
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
    #[serde(rename = "loaderId", default)]
    loader_id: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "urlFragment", default)]
    url_fragment: Option<String>,
    #[serde(rename = "securityOrigin", default)]
    security_origin: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "unreachableUrl", default)]
    unreachable_url: Option<String>,
}

#[derive(Deserialize)]
struct FrameStartedLoadingParams {
    #[serde(rename = "frameId")]
    frame_id: String,
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
struct RuntimeEvaluateReply {
    result: RuntimeRemoteObject,
    #[serde(rename = "exceptionDetails")]
    exception_details: Option<RuntimeExceptionDetails>,
}

#[derive(Deserialize)]
struct RuntimeRemoteObject {
    value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RuntimeExceptionDetails {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RevealScriptOutcome {
    #[serde(default)]
    revealed_panels: Vec<RevealedPanelSnapshot>,
    #[serde(default)]
    reveal_capped: bool,
    #[serde(default)]
    scroll_steps_performed: usize,
    #[serde(default)]
    navigated: bool,
    #[serde(default)]
    challenge_detected: bool,
    #[serde(default)]
    warning: Option<String>,
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
    #[serde(default)]
    method: Option<String>,
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
    #[serde(rename = "encodedDataLength", default)]
    encoded_data_length: Option<f64>,
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
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("failed to serialize CDP request {method}: {e}"),
            )
        })?;
        let now = Instant::now();
        if now >= deadline {
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                format!("CDP send timed out for {method}"),
            ));
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
            Err(_) => Err(cdp_err(
                TransportErrorKind::Timeout,
                format!("CDP send timed out for {method}"),
            )),
        }
    }

    async fn send_ignored_session_cmd<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
        deadline: Instant,
    ) -> Result<()> {
        let cmd_id = self.alloc_id();
        self.ignored_reply_ids.insert(cmd_id);
        let sid = self.session_id.clone();
        self.send_raw(cmd_id, method, params, sid.as_deref(), deadline)
            .await
    }

    async fn call_typed<P: Serialize, R: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        params: P,
        use_session: bool,
    ) -> Result<RpcCallOutcome<R>> {
        let id = self.alloc_id();
        let sid = use_session.then(|| self.session_id.clone()).flatten();
        self.send_raw(id, method, params, sid.as_deref(), self.deadline)
            .await?;

        if let Some(err) = self.state.terminal_error.take() {
            return Err(anyhow::Error::new(err));
        }
        let text = self.read_until_reply(id, self.deadline).await?;
        let envelope: RpcEnvelope<R> = serde_json::from_str(&text).map_err(|e| {
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("malformed CDP reply for {method}: {e}"),
            )
        })?;
        if let Some(rpc_err) = envelope.error {
            return Ok(RpcCallOutcome::RpcErr(rpc_err));
        }
        let result = envelope.result.ok_or_else(|| {
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("CDP reply for {method} missing result payload"),
            )
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
            RpcCallOutcome::RpcErr(err) => Err(rpc_failure_err(
                &format!("CDP {method}"),
                &err,
                TransportErrorKind::LoadError,
            )),
        }
    }

    async fn poll_ws_step(
        &mut self,
        awaited_id: Option<i64>,
        deadline: Instant,
        allow_slice_timeout: bool,
    ) -> Result<Option<Option<String>>> {
        if let Some(err) = self.state.terminal_error.take() {
            return Err(anyhow::Error::new(err));
        }
        if Instant::now() >= deadline {
            if allow_slice_timeout && Instant::now() < self.deadline {
                return Ok(None);
            }
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                format!("CDP operation timed out for {}", self.initial_target_url),
            ));
        }
        match tokio::time::timeout_at(deadline, self.ws.next()).await {
            Ok(Some(Ok(msg))) => {
                let matched = self.process_ws_message(msg, awaited_id, deadline).await?;
                if let Some(err) = self.state.terminal_error.take() {
                    return Err(anyhow::Error::new(err));
                }
                Ok(Some(matched))
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
                Err(cdp_err(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed before response completed",
                ))
            }
            Err(_) => {
                if allow_slice_timeout && Instant::now() < self.deadline {
                    Ok(None)
                } else {
                    Err(cdp_err(
                        TransportErrorKind::Timeout,
                        format!("CDP operation timed out for {}", self.initial_target_url),
                    ))
                }
            }
        }
    }

    async fn pump_until(&mut self, stop_at: Instant) -> Result<()> {
        let target_deadline = self.deadline.min(stop_at);
        while self
            .poll_ws_step(None, target_deadline, true)
            .await?
            .is_some()
        {}
        Ok(())
    }

    async fn read_next_message(
        &mut self,
        awaited_id: Option<i64>,
        deadline: Instant,
    ) -> Result<Option<String>> {
        Ok(self
            .poll_ws_step(awaited_id, deadline, false)
            .await?
            .flatten())
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
                    cdp_err(
                        TransportErrorKind::ProtocolMismatch,
                        format!("malformed JSON from CDP endpoint: {e}"),
                    )
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
                    return Err(cdp_err(
                        TransportErrorKind::ProtocolMismatch,
                        format!("CDP error ({}): {}", err.code, err.message),
                    ));
                }
                Ok(None)
            }
            Message::Binary(bin) => {
                self.account_unsolicited(bin.len())?;
                Ok(None)
            }
            Message::Ping(payload) | Message::Pong(payload) => {
                self.account_unsolicited(payload.len())?;
                Ok(None)
            }
            Message::Close(_) => {
                self.ws_broken = true;
                Err(cdp_err(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed by remote endpoint",
                ))
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
            return Err(cdp_err(
                TransportErrorKind::ResponseTooLarge,
                "CDP event stream exceeded safety bounds",
            ));
        }
        Ok(())
    }

    fn validate_event_url(&self, raw_url: &str, context: &str) -> Result<Url> {
        let parsed = Url::parse(raw_url).map_err(|_| {
            cdp_err(
                TransportErrorKind::RedirectBlocked,
                format!("CDP blocked invalid URL during {context}"),
            )
        })?;
        if !is_safe_redirect_url(&self.initial_target_url, &parsed) {
            return Err(cdp_err(
                TransportErrorKind::RedirectBlocked,
                format!("CDP blocked unsafe URL pivot during {context}"),
            ));
        }
        if is_url_self_ssrf(&parsed, &self.cdp_url)
            || is_ip_self_ssrf(&parsed, self.cdp_peer_ip, self.cdp_port)
        {
            return Err(cdp_err(
                TransportErrorKind::RedirectBlocked,
                "CDP blocked self-SSRF request to CDP endpoint",
            ));
        }
        if self.cdp_url.scheme() == "wss"
            && !self.allow_local_targets
            && parsed.host_str().is_some_and(is_local_or_private_host)
        {
            return Err(cdp_err(
                TransportErrorKind::RedirectBlocked,
                "remote wss:// CDP blocked local or private URL",
            ));
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
                let Some(params) = parse_event_params::<FetchRequestPausedParams>(raw) else {
                    return Ok(());
                };
                let is_main_frame = params.resource_type.as_deref() == Some("Document")
                    || (params.frame_id.is_some()
                        && params.frame_id.as_deref() == self.state.main_frame_id.as_deref());

                match self.validate_event_url(&params.request.url, "Fetch.requestPaused") {
                    Ok(_) => {
                        self.send_ignored_session_cmd(
                            "Fetch.continueRequest",
                            serde_json::json!({ "requestId": params.request_id }),
                            reply_deadline,
                        )
                        .await?;
                    }
                    Err(err) => {
                        self.state
                            .blocked_interception_ids
                            .insert(params.request_id.clone());
                        self.send_ignored_session_cmd(
                            "Fetch.failRequest",
                            serde_json::json!({
                                "requestId": params.request_id,
                                "errorReason": "BlockedByClient"
                            }),
                            reply_deadline,
                        )
                        .await?;
                        if is_main_frame {
                            self.set_terminal_error(err);
                        }
                    }
                }
            }
            "Network.requestWillBeSent" => {
                let Some(params) = parse_event_params::<RequestWillBeSentParams>(raw) else {
                    return Ok(());
                };
                if params
                    .resource_type
                    .as_deref()
                    .is_some_and(|rt| rt == "XHR" || rt == "Fetch")
                    && params
                        .request
                        .method
                        .as_deref()
                        .unwrap_or("GET")
                        .eq_ignore_ascii_case("GET")
                {
                    self.state
                        .xhr_get_requests
                        .insert(params.request_id.clone());
                }
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
                let Some(params) = parse_event_params::<ResponseReceivedParams>(raw) else {
                    return Ok(());
                };
                let is_xhr_get = self.state.xhr_get_requests.remove(&params.request_id);
                let matches_main = self.state.main_frame_id.as_deref().is_some_and(|mf| {
                    params.frame_id.as_deref() == Some(mf)
                        && params.resource_type.as_deref() == Some("Document")
                }) || (self.state.main_request_id.as_deref()
                    == Some(params.request_id.as_str())
                    && params.resource_type.as_deref() == Some("Document"));
                if !matches_main {
                    if is_xhr_get
                        && (200..300).contains(&params.response.status)
                        && self.state.captured_datasets.len() < MAX_RAW_CAPTURED_DATASETS
                    {
                        let mime_type =
                            header_case_insensitive(&params.response.headers, "content-type")
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    (!params.response.mime_type.trim().is_empty())
                                        .then(|| params.response.mime_type.trim().to_string())
                                });
                        if mime_type.as_deref().is_some_and(is_indexable_json_mime) {
                            let byte_count =
                                header_case_insensitive(&params.response.headers, "content-length")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .or_else(|| {
                                        params
                                            .response
                                            .encoded_data_length
                                            .filter(|v| *v > 0.0 && v.is_finite())
                                            .map(|v| v as usize)
                                    });
                            self.state.captured_datasets.push(CapturedDataset {
                                url: params.response.url,
                                mime_type,
                                byte_count,
                            });
                        }
                    }
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
                let Some(params) = parse_event_params::<FrameNavigatedParams>(raw) else {
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
                let Some(params) = parse_event_params::<NavigatedWithinDocumentParams>(raw) else {
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
                let Some(params) = parse_event_params::<LoadingFailedParams>(raw) else {
                    return Ok(());
                };
                self.state.xhr_get_requests.remove(&params.request_id);
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
                ingest_webmcp_tools_added(
                    raw,
                    self.state.main_frame_id.as_deref(),
                    &mut self.state.cdp_tools,
                );
            }
            "WebMCP.toolsRemoved" => {
                remove_webmcp_tools(
                    raw,
                    self.state.main_frame_id.as_deref(),
                    &mut self.state.cdp_tools,
                );
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
        let needs_layout = options.capture_axtree
            || options.reveal != RevealPolicy::None
            || options.scroll_steps > 0;
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
        } else if !needs_layout {
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

        let max_bytes = if options.max_response_bytes > 0 {
            options.max_response_bytes
        } else {
            MAX_HTML_BYTES
        };
        let mut outer_html = self.read_bounded_outer_html(max_bytes).await?;

        let mut capture_meta = BrowserCaptureMetadata::default();
        let needs_reveal_or_scroll =
            options.reveal != RevealPolicy::None || options.scroll_steps > 0;
        if needs_reveal_or_scroll {
            let pre_state_final_url = self.state.final_url.clone();
            let pre_url = pre_state_final_url
                .as_deref()
                .unwrap_or_else(|| self.initial_target_url.as_str())
                .to_string();
            let pre_status = if self.state.status_code == 0 {
                200
            } else {
                self.state.status_code
            };
            let pre_is_html = self
                .state
                .content_type
                .as_deref()
                .is_none_or(|c| is_html_content_type(Some(c)));
            let mut pre_parsed = None;
            let empty_headers = DiagnosticHeaders::default();
            let pre_class = classify_response_with_headers(
                pre_status,
                pre_is_html,
                Some(self.initial_target_url.as_str()),
                Some(&pre_url),
                &outer_html,
                &empty_headers,
                &mut pre_parsed,
            );
            let skip_for_gate = detect_interstitial_challenge(&outer_html).is_some()
                || matches!(
                    pre_class,
                    ResponseClassification::Challenge { .. }
                        | ResponseClassification::Blocked { .. }
                );
            if !skip_for_gate {
                let remaining_ms = self
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64;
                let eval_js = build_cdp_reveal_eval_js(
                    options.reveal,
                    options.scroll_steps,
                    remaining_ms.clamp(200, 15_000),
                );
                let reply = self
                    .call_typed::<_, RuntimeEvaluateReply>(
                        "Runtime.evaluate",
                        serde_json::json!({
                            "expression": eval_js,
                            "returnByValue": true,
                            "awaitPromise": true
                        }),
                        true,
                    )
                    .await?;
                match reply {
                    RpcCallOutcome::Ok(eval_ok) => {
                        if let Some(ex) = eval_ok.exception_details {
                            capture_meta.reveal_warning =
                                Some(format!("reveal script exception: {}", ex.text));
                        } else if let Some(val) = eval_ok.result.value {
                            match serde_json::from_value::<RevealScriptOutcome>(val) {
                                Ok(outcome) => {
                                    let event_navigated = self
                                        .state
                                        .final_url
                                        .as_deref()
                                        .is_some_and(|u| origin_and_path_changed(&pre_url, u));
                                    if outcome.navigated || event_navigated {
                                        self.state.final_url = pre_state_final_url;
                                        capture_meta.reveal_warning =
                                            Some(outcome.warning.unwrap_or_else(|| {
                                                "aborted --reveal because a control triggered page navigation".to_string()
                                            }));
                                    } else {
                                        if !outcome.challenge_detected {
                                            capture_meta.revealed_panels = outcome.revealed_panels;
                                            capture_meta.reveal_capped |= outcome.reveal_capped;
                                            capture_meta.enforce_bounds();
                                        }
                                        capture_meta.scroll_steps_performed =
                                            outcome.scroll_steps_performed;
                                        capture_meta.reveal_warning = outcome.warning;
                                        outer_html =
                                            self.read_bounded_outer_html(max_bytes).await?;
                                    }
                                }
                                Err(err) => {
                                    capture_meta.reveal_warning =
                                        Some(format!("invalid reveal script payload: {err}"));
                                }
                            }
                        }
                    }
                    RpcCallOutcome::RpcErr(err) => {
                        if err.code == CDP_ERR_METHOD_NOT_FOUND {
                            return Err(rpc_failure_err(
                                "CDP Runtime.evaluate",
                                &err,
                                TransportErrorKind::ProtocolMismatch,
                            ));
                        }
                        capture_meta.reveal_warning = Some(format!(
                            "Runtime.evaluate failed ({}): {}",
                            err.code, err.message
                        ));
                    }
                }
            }
        }
        capture_meta.datasets = std::mem::take(&mut self.state.captured_datasets);

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
            capture_meta,
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
                    return Err(rpc_failure_err(
                        "DOM.getDocument",
                        &err,
                        TransportErrorKind::LoadError,
                    ));
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
                            return Err(cdp_err(
                                TransportErrorKind::InvalidSelector,
                                format!("invalid CSS selector {selector:?}: {err_msg}"),
                            ));
                        }
                        if res.node_id > 0 {
                            return Ok(());
                        }
                    }
                    RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => {
                        if err.message.contains("DOM Error while querying") {
                            return Err(cdp_err(
                                TransportErrorKind::InvalidSelector,
                                format!("invalid CSS selector {selector:?}: {}", err.message),
                            ));
                        }
                        // Otherwise transient stale node id; retry DOM.getDocument next tick.
                    }
                    RpcCallOutcome::RpcErr(err) => {
                        return Err(rpc_failure_err(
                            "DOM.querySelector",
                            &err,
                            TransportErrorKind::InvalidSelector,
                        ));
                    }
                }
            }

            if Instant::now() >= self.deadline {
                return Err(cdp_err(
                    TransportErrorKind::Timeout,
                    format!(
                        "timed out waiting for selector {selector:?} on {}",
                        self.initial_target_url
                    ),
                ));
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

    async fn read_bounded_outer_html(&mut self, max_bytes: usize) -> Result<String> {
        let html = self.read_outer_html().await?;
        if html.len() > max_bytes {
            return Err(anyhow::Error::new(ResponseTooLargeError {
                limit_bytes: max_bytes,
            }));
        }
        Ok(html)
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
                return Err(cdp_err(
                    TransportErrorKind::ProtocolMismatch,
                    "DOM.getDocument returned nodeId 0",
                ));
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
                    return Err(rpc_failure_err(
                        "DOM.getOuterHTML",
                        &err,
                        TransportErrorKind::LoadError,
                    ));
                }
            }
        }
        Err(cdp_err(
            TransportErrorKind::LoadError,
            "DOM.getOuterHTML failed after retry",
        ))
    }

    async fn read_axtree(&mut self, max_bytes: usize) -> Result<String> {
        let outcome = self
            .call_typed::<_, serde_json::Value>(
                "Accessibility.getFullAXTree",
                serde_json::json!({}),
                true,
            )
            .await?;
        format_bounded_axtree_outcome(outcome, max_bytes)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachedDocumentBinding {
    frame_id: String,
    loader_id: String,
    url: String,
    origin: (String, String, u16),
    security_origin: Option<String>,
    mime_type: Option<String>,
}

struct PassiveAttachSession {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    deadline: Instant,
    cdp_url: Url,
    cdp_peer_ip: IpAddr,
    cdp_port: u16,
    expected_origin: Option<Url>,
    next_id: i64,
    unsolicited_events: usize,
    unsolicited_event_bytes: usize,
    ws_broken: bool,
    binding: Option<AttachedDocumentBinding>,
    document_invalidated: bool,
    invalidation_reason: Option<String>,
    cdp_tools: Vec<WebMcpToolDescriptor>,
}

impl PassiveAttachSession {
    fn new(
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        deadline: Instant,
        cdp_url: Url,
        cdp_peer_ip: IpAddr,
        cdp_port: u16,
        expected_origin: Option<Url>,
    ) -> Self {
        Self {
            ws,
            deadline,
            cdp_url,
            cdp_peer_ip,
            cdp_port,
            expected_origin,
            next_id: 1,
            unsolicited_events: 0,
            unsolicited_event_bytes: 0,
            ws_broken: false,
            binding: None,
            document_invalidated: false,
            invalidation_reason: None,
            cdp_tools: Vec::new(),
        }
    }

    fn is_allowed_method(method: &str) -> bool {
        matches!(
            method,
            "Page.enable"
                | "Page.getFrameTree"
                | "DOM.enable"
                | "DOM.getDocument"
                | "DOM.querySelector"
                | "DOM.getOuterHTML"
                | "Accessibility.getFullAXTree"
                | "WebMCP.enable"
        )
    }

    async fn call_typed<P: Serialize, R: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<RpcCallOutcome<R>> {
        debug_assert!(Self::is_allowed_method(method));
        if !Self::is_allowed_method(method) {
            return Err(cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("forbidden outbound CDP method in passive attach: {method}"),
            ));
        }

        let id = self.next_id;
        self.next_id += 1;
        let req = RpcRequest {
            id,
            method,
            params,
            session_id: None,
        };
        let encoded = serde_json::to_string(&req).map_err(|e| {
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("failed to serialize CDP request {method}: {e}"),
            )
        })?;
        if Instant::now() >= self.deadline {
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                format!("CDP passive attach send timed out for {method}"),
            ));
        }
        match tokio::time::timeout_at(self.deadline, self.ws.send(Message::Text(encoded.into())))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(ws_err)) => {
                self.ws_broken = true;
                return Err(map_tungstenite_error(ws_err, "attached-cdp-page"));
            }
            Err(_) => {
                return Err(cdp_err(
                    TransportErrorKind::Timeout,
                    format!("CDP passive attach send timed out for {method}"),
                ));
            }
        }

        let text = self.read_until_reply(id, self.deadline).await?;
        let envelope: RpcEnvelope<R> = serde_json::from_str(&text).map_err(|e| {
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("malformed CDP reply for {method}: {e}"),
            )
        })?;
        if let Some(rpc_err) = envelope.error {
            return Ok(RpcCallOutcome::RpcErr(rpc_err));
        }
        let result = envelope.result.ok_or_else(|| {
            cdp_err(
                TransportErrorKind::ProtocolMismatch,
                format!("CDP reply for {method} missing result payload"),
            )
        })?;
        Ok(RpcCallOutcome::Ok(result))
    }

    async fn call_required<P: Serialize, R: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<R> {
        match self.call_typed(method, params).await? {
            RpcCallOutcome::Ok(r) => Ok(r),
            RpcCallOutcome::RpcErr(err) => Err(rpc_failure_err(
                &format!("CDP {method}"),
                &err,
                TransportErrorKind::LoadError,
            )),
        }
    }

    async fn call_best_effort<P: Serialize>(&mut self, method: &str, params: P) -> Result<bool> {
        match self
            .call_typed::<_, serde_json::Value>(method, params)
            .await?
        {
            RpcCallOutcome::Ok(_) => Ok(true),
            RpcCallOutcome::RpcErr(err) => {
                if err.code != CDP_ERR_METHOD_NOT_FOUND {
                    tracing::debug!(
                        "passive CDP best-effort method {method} returned error ({}): {}",
                        err.code,
                        err.message
                    );
                }
                Ok(false)
            }
        }
    }

    fn invalidate(&mut self, reason: impl Into<String>) {
        if !self.document_invalidated {
            self.document_invalidated = true;
            self.invalidation_reason = Some(reason.into());
        }
    }

    fn check_not_invalidated(&self) -> Result<()> {
        if self.document_invalidated {
            return Err(cdp_err(
                TransportErrorKind::LoadError,
                self.invalidation_reason
                    .as_deref()
                    .unwrap_or("attached CDP page navigated during capture"),
            ));
        }
        Ok(())
    }

    fn account_unsolicited(&mut self, byte_len: usize) -> Result<()> {
        self.unsolicited_events = self.unsolicited_events.saturating_add(1);
        self.unsolicited_event_bytes = self.unsolicited_event_bytes.saturating_add(byte_len);
        if self.unsolicited_events > MAX_UNSOLICITED_EVENTS
            || self.unsolicited_event_bytes > MAX_EVENT_BYTES
        {
            return Err(cdp_err(
                TransportErrorKind::ResponseTooLarge,
                "CDP event stream exceeded safety bounds",
            ));
        }
        Ok(())
    }

    async fn read_until_reply(&mut self, awaited_id: i64, deadline: Instant) -> Result<String> {
        loop {
            if let Some(matched) = self.poll_ws_step(Some(awaited_id), deadline, false).await? {
                return Ok(matched);
            }
        }
    }

    async fn pump_until(&mut self, stop_at: Instant) -> Result<()> {
        let target_deadline = self.deadline.min(stop_at);
        loop {
            self.check_not_invalidated()?;
            if Instant::now() >= target_deadline {
                return Ok(());
            }
            match self.poll_ws_step(None, target_deadline, true).await? {
                Some(_) => {}
                None => return Ok(()),
            }
        }
    }

    async fn poll_ws_step(
        &mut self,
        awaited_id: Option<i64>,
        deadline: Instant,
        allow_slice_timeout: bool,
    ) -> Result<Option<String>> {
        self.check_not_invalidated()?;
        if Instant::now() >= deadline {
            if allow_slice_timeout && Instant::now() < self.deadline {
                return Ok(None);
            }
            return Err(cdp_err(
                TransportErrorKind::Timeout,
                "CDP passive attach timed out",
            ));
        }
        match tokio::time::timeout_at(deadline, self.ws.next()).await {
            Ok(Some(Ok(msg))) => self.process_ws_message(msg, awaited_id),
            Ok(Some(Err(ws_err))) => {
                self.ws_broken = true;
                Err(map_tungstenite_error(ws_err, "attached-cdp-page"))
            }
            Ok(None) => {
                self.ws_broken = true;
                Err(cdp_err(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed before passive capture completed",
                ))
            }
            Err(_) => {
                if allow_slice_timeout && Instant::now() < self.deadline {
                    Ok(None)
                } else {
                    Err(cdp_err(
                        TransportErrorKind::Timeout,
                        "CDP passive attach timed out",
                    ))
                }
            }
        }
    }

    fn process_ws_message(
        &mut self,
        msg: Message,
        awaited_id: Option<i64>,
    ) -> Result<Option<String>> {
        match msg {
            Message::Text(text) => {
                let raw = text.as_str();
                let header: RpcHeader = serde_json::from_str(raw).map_err(|e| {
                    cdp_err(
                        TransportErrorKind::ProtocolMismatch,
                        format!("malformed JSON from CDP endpoint: {e}"),
                    )
                })?;

                if let Some(msg_id) = header.id {
                    if awaited_id == Some(msg_id) {
                        return Ok(Some(raw.to_string()));
                    }
                    return Ok(None);
                }

                self.account_unsolicited(raw.len())?;

                if let Some(method) = header.method.as_deref() {
                    if header.session_id.is_none() {
                        self.handle_passive_event(method, raw);
                        self.check_not_invalidated()?;
                    }
                } else if let Some(err) = header.error {
                    return Err(cdp_err(
                        TransportErrorKind::ProtocolMismatch,
                        format!("CDP error ({}): {}", err.code, err.message),
                    ));
                }
                Ok(None)
            }
            Message::Binary(bin) => {
                self.account_unsolicited(bin.len())?;
                Ok(None)
            }
            Message::Ping(payload) | Message::Pong(payload) => {
                self.account_unsolicited(payload.len())?;
                Ok(None)
            }
            Message::Close(_) => {
                self.ws_broken = true;
                Err(cdp_err(
                    TransportErrorKind::Network,
                    "CDP WebSocket closed by remote endpoint",
                ))
            }
            Message::Frame(_) => Ok(None),
        }
    }

    fn handle_passive_event(&mut self, method: &str, raw: &str) {
        match method {
            "Page.frameNavigated" => {
                if let Some(params) = parse_event_params::<FrameNavigatedParams>(raw) {
                    if self
                        .binding
                        .as_ref()
                        .is_some_and(|b| b.frame_id == params.frame.id)
                    {
                        self.invalidate("attached CDP page navigated during capture");
                    }
                }
            }
            "Page.navigatedWithinDocument" => {
                if let Some(params) = parse_event_params::<NavigatedWithinDocumentParams>(raw) {
                    if self
                        .binding
                        .as_ref()
                        .is_some_and(|b| b.frame_id == params.frame_id && b.url != params.url)
                    {
                        self.invalidate(
                            "attached CDP page navigated within document during capture",
                        );
                    }
                }
            }
            "Page.frameStartedLoading" => {
                if let Some(params) = parse_event_params::<FrameStartedLoadingParams>(raw) {
                    if self
                        .binding
                        .as_ref()
                        .is_some_and(|b| b.frame_id == params.frame_id)
                    {
                        self.invalidate("attached CDP page started loading during capture");
                    }
                }
            }
            "DOM.documentUpdated" => {
                if self.binding.is_some() {
                    self.invalidate("attached CDP document was replaced during capture");
                }
            }
            "WebMCP.toolsAdded" => {
                let main_fid = self.binding.as_ref().map(|b| b.frame_id.as_str());
                ingest_webmcp_tools_added(raw, main_fid, &mut self.cdp_tools);
            }
            "WebMCP.toolsRemoved" => {
                let main_fid = self.binding.as_ref().map(|b| b.frame_id.as_str());
                remove_webmcp_tools(raw, main_fid, &mut self.cdp_tools);
            }
            _ => {}
        }
    }

    fn validate_frame_binding(
        &self,
        frame: &FrameTreeFrame,
        options: &CdpRenderOptions,
    ) -> Result<AttachedDocumentBinding> {
        if frame.id.is_empty() {
            return Err(cdp_err(
                TransportErrorKind::ProtocolMismatch,
                "Page.getFrameTree returned an empty main frame id",
            ));
        }
        if frame.loader_id.is_empty() {
            return Err(cdp_err(
                TransportErrorKind::ProtocolMismatch,
                "Page.getFrameTree returned an empty loaderId; cannot bind attached document identity",
            ));
        }
        if frame
            .unreachable_url
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
        {
            return Err(cdp_err(
                TransportErrorKind::LoadError,
                "attached CDP page has an unreachableUrl error state",
            ));
        }
        let mime_type = frame
            .mime_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if mime_type
            .as_deref()
            .is_some_and(|m| crate::extract::detect_binary_content(Some(m), &[]).is_some())
        {
            return Err(cdp_err(
                TransportErrorKind::LoadError,
                "attached CDP page has an unsupported binary MIME type",
            ));
        }

        let reconstructed = format!(
            "{}{}",
            frame.url,
            frame.url_fragment.as_deref().unwrap_or("")
        );
        let live_url = validate_target_url(&reconstructed, &self.cdp_url, options)?;
        if is_ip_self_ssrf(&live_url, self.cdp_peer_ip, self.cdp_port) {
            return Err(cdp_err(
                TransportErrorKind::RedirectBlocked,
                "attached CDP page URL resolves to the CDP endpoint (self-SSRF blocked)",
            ));
        }
        let live_origin = origin_tuple(&live_url).ok_or_else(|| {
            cdp_err(
                TransportErrorKind::LoadError,
                "attached CDP page URL lacks a valid origin",
            )
        })?;

        if let Some(ref exp) = self.expected_origin {
            let exp_origin = origin_tuple(exp).ok_or_else(|| {
                cdp_err(
                    TransportErrorKind::LoadError,
                    "expected origin URL lacks a valid origin",
                )
            })?;
            if live_origin != exp_origin {
                return Err(cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "attached CDP page origin does not match expected URL origin",
                ));
            }
        }

        let sec_origin = frame
            .security_origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(ref sec) = sec_origin {
            if sec == "null" {
                return Err(cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "attached CDP page has an opaque null securityOrigin",
                ));
            }
            let sec_parsed = Url::parse(sec).map_err(|_| {
                cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "attached CDP page reported an invalid securityOrigin",
                )
            })?;
            let sec_tuple = origin_tuple(&sec_parsed).ok_or_else(|| {
                cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "attached CDP page reported an incomplete securityOrigin",
                )
            })?;
            if sec_tuple != live_origin {
                return Err(cdp_err(
                    TransportErrorKind::RedirectBlocked,
                    "attached CDP page securityOrigin does not match frame URL origin",
                ));
            }
        }

        Ok(AttachedDocumentBinding {
            frame_id: frame.id.clone(),
            loader_id: frame.loader_id.clone(),
            url: live_url.to_string(),
            origin: live_origin,
            security_origin: sec_origin,
            mime_type,
        })
    }

    async fn verify_binding(
        &mut self,
        expected: &AttachedDocumentBinding,
        options: &CdpRenderOptions,
    ) -> Result<()> {
        self.check_not_invalidated()?;
        let tree: FrameTreeReply = self
            .call_required("Page.getFrameTree", serde_json::json!({}))
            .await?;
        self.check_not_invalidated()?;
        let current = self.validate_frame_binding(&tree.frame_tree.frame, options)?;
        if current.frame_id != expected.frame_id
            || current.loader_id != expected.loader_id
            || current.url != expected.url
            || current.origin != expected.origin
            || current.security_origin != expected.security_origin
        {
            return Err(cdp_err(
                TransportErrorKind::LoadError,
                "attached CDP page identity changed during capture",
            ));
        }
        Ok(())
    }

    async fn read_outer_html(&mut self, max_bytes: usize) -> Result<String> {
        for attempt in 0..2 {
            self.check_not_invalidated()?;
            let doc: GetDocumentReply = self
                .call_required("DOM.getDocument", serde_json::json!({ "depth": 0 }))
                .await?;
            self.check_not_invalidated()?;
            if doc.root.node_id <= 0 {
                if attempt == 0 {
                    continue;
                }
                return Err(cdp_err(
                    TransportErrorKind::ProtocolMismatch,
                    "DOM.getDocument returned nodeId 0",
                ));
            }
            match self
                .call_typed::<_, OuterHtmlReply>(
                    "DOM.getOuterHTML",
                    serde_json::json!({ "nodeId": doc.root.node_id }),
                )
                .await?
            {
                RpcCallOutcome::Ok(reply) => {
                    self.check_not_invalidated()?;
                    if reply.outer_html.len() > max_bytes {
                        return Err(anyhow::Error::new(ResponseTooLargeError {
                            limit_bytes: max_bytes,
                        }));
                    }
                    return Ok(reply.outer_html);
                }
                RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR && attempt == 0 => {
                    continue;
                }
                RpcCallOutcome::RpcErr(err) => {
                    return Err(rpc_failure_err(
                        "DOM.getOuterHTML",
                        &err,
                        TransportErrorKind::LoadError,
                    ));
                }
            }
        }
        Err(cdp_err(
            TransportErrorKind::LoadError,
            "DOM.getOuterHTML failed after retry",
        ))
    }

    async fn wait_for_selector(
        &mut self,
        selector: &str,
        binding: &AttachedDocumentBinding,
        options: &CdpRenderOptions,
    ) -> Result<()> {
        loop {
            self.check_not_invalidated()?;
            let doc_reply = self
                .call_typed::<_, GetDocumentReply>(
                    "DOM.getDocument",
                    serde_json::json!({ "depth": 0 }),
                )
                .await?;
            self.check_not_invalidated()?;
            let root_id = match doc_reply {
                RpcCallOutcome::Ok(doc) => doc.root.node_id,
                RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => 0,
                RpcCallOutcome::RpcErr(err) => {
                    return Err(rpc_failure_err(
                        "DOM.getDocument",
                        &err,
                        TransportErrorKind::LoadError,
                    ));
                }
            };

            if root_id > 0 {
                let query_reply = self
                    .call_typed::<_, QuerySelectorReply>(
                        "DOM.querySelector",
                        serde_json::json!({ "nodeId": root_id, "selector": selector }),
                    )
                    .await?;
                self.check_not_invalidated()?;
                match query_reply {
                    RpcCallOutcome::Ok(res) => {
                        if let Some(err_msg) = res.error.filter(|s| !s.is_empty()) {
                            return Err(cdp_err(
                                TransportErrorKind::InvalidSelector,
                                format!("invalid CSS selector {selector:?}: {err_msg}"),
                            ));
                        }
                        if res.node_id > 0 {
                            self.verify_binding(binding, options).await?;
                            return Ok(());
                        }
                    }
                    RpcCallOutcome::RpcErr(err) if err.code == CDP_ERR_SERVER_ERROR => {
                        // Syntax was pre-validated by scraper::Selector::parse; retry on stale node.
                    }
                    RpcCallOutcome::RpcErr(err) => {
                        return Err(rpc_failure_err(
                            "DOM.querySelector",
                            &err,
                            TransportErrorKind::InvalidSelector,
                        ));
                    }
                }
            }

            if Instant::now() >= self.deadline {
                return Err(cdp_err(
                    TransportErrorKind::Timeout,
                    format!(
                        "timed out waiting for selector {selector:?} on {}",
                        binding.url
                    ),
                ));
            }
            self.pump_until(Instant::now() + SELECTOR_POLL_INTERVAL)
                .await?;
        }
    }

    async fn read_axtree(&mut self, frame_id: &str, max_bytes: usize) -> Result<String> {
        let outcome = self
            .call_typed::<_, serde_json::Value>(
                "Accessibility.getFullAXTree",
                serde_json::json!({ "frameId": frame_id }),
            )
            .await?;
        self.check_not_invalidated()?;
        format_bounded_axtree_outcome(outcome, max_bytes)
    }

    async fn execute(&mut self, options: &CdpRenderOptions) -> Result<CdpRenderResult> {
        let _: EmptyReply = self
            .call_required("Page.enable", serde_json::json!({}))
            .await?;
        let _ = self
            .call_best_effort("DOM.enable", serde_json::json!({}))
            .await?;

        let frame_tree: FrameTreeReply = self
            .call_required("Page.getFrameTree", serde_json::json!({}))
            .await?;
        let binding = self.validate_frame_binding(&frame_tree.frame_tree.frame, options)?;
        self.binding = Some(binding.clone());

        if self
            .call_best_effort("WebMCP.enable", serde_json::json!({}))
            .await?
        {
            self.pump_until(Instant::now() + Duration::from_millis(100))
                .await?;
        }

        let max_bytes = if options.max_response_bytes > 0 {
            options.max_response_bytes
        } else {
            MAX_HTML_BYTES
        };

        let mut outer_html = self.read_outer_html(max_bytes).await?;
        self.verify_binding(&binding, options).await?;

        // Fail fast on active challenges or walls before waiting on `--wait-for-selector`.
        let mut parsed_pre = None;
        let pre_cls = crate::policy::classify_response_with_context(
            200,
            true,
            self.expected_origin.as_ref().map(Url::as_str),
            Some(&binding.url),
            &outer_html,
            &mut parsed_pre,
        );
        let early_block = crate::policy::detect_interstitial_challenge(&outer_html).is_some()
            || matches!(
                pre_cls,
                crate::policy::ResponseClassification::Challenge { .. }
                    | crate::policy::ResponseClassification::Blocked { .. }
            );
        if early_block {
            let imperative_tools = self
                .cdp_tools
                .iter()
                .filter(|t| t.source == WebMcpToolSource::Imperative)
                .cloned()
                .collect();
            return Ok(CdpRenderResult {
                html: outer_html,
                final_url: binding.url,
                status_code: 0,
                content_type: binding.mime_type.or_else(|| Some("text/html".to_string())),
                link_header: None,
                axtree_text: None,
                imperative_tools,
                capture_meta: BrowserCaptureMetadata::default(),
            });
        }

        if let Some(selector) = options.wait_for_selector.as_deref() {
            self.wait_for_selector(selector, &binding, options).await?;
            outer_html = self.read_outer_html(max_bytes).await?;
        }

        let axtree_text = if options.capture_axtree {
            Some(self.read_axtree(&binding.frame_id, max_bytes).await?)
        } else {
            None
        };

        self.verify_binding(&binding, options).await?;

        let imperative_tools = self
            .cdp_tools
            .iter()
            .filter(|t| t.source == WebMcpToolSource::Imperative)
            .cloned()
            .collect();
        Ok(CdpRenderResult {
            html: outer_html,
            final_url: binding.url,
            status_code: 0,
            content_type: binding.mime_type.or_else(|| Some("text/html".to_string())),
            link_header: None,
            axtree_text,
            imperative_tools,
            capture_meta: BrowserCaptureMetadata::default(),
        })
    }

    async fn teardown(&mut self) {
        if self.ws_broken {
            return;
        }
        let teardown_deadline = Instant::now() + TEARDOWN_GRACE;
        let _ = tokio::time::timeout_at(teardown_deadline, self.ws.close(None)).await;
    }
}

fn origin_tuple(url: &Url) -> Option<(String, String, u16)> {
    let host = canonical_url_host(url)?;
    let port = url.port_or_known_default()?;
    Some((url.scheme().to_ascii_lowercase(), host, port))
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
