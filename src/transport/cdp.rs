use anyhow::{bail, Result};
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

use crate::extract::WebMcpToolDescriptor;

pub const MAX_HTML_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_UNSOLICITED_EVENTS: usize = 16_384;
pub const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

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

#[derive(Debug, Clone)]
pub struct CdpRenderer {
    cdp_url: String,
    _state_dir: PathBuf,
    _proxy: Option<String>,
}

impl CdpRenderer {
    pub fn new(cdp_url: impl Into<String>, state_dir: PathBuf, proxy: Option<String>) -> Self {
        Self {
            cdp_url: cdp_url.into(),
            _state_dir: state_dir,
            _proxy: proxy,
        }
    }

    pub fn cdp_url_display(&self) -> String {
        sanitize_cdp_url_for_display(&self.cdp_url)
    }

    pub async fn render(
        &self,
        _url: &str,
        _options: &CdpRenderOptions,
        _timeout: Duration,
    ) -> Result<CdpRenderResult> {
        bail!("CDP renderer is not yet implemented")
    }
}

pub fn validate_cdp_url(raw: &str) -> Result<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("CDP URL must not be empty");
    }
    let parsed = Url::parse(trimmed).map_err(|_| anyhow::anyhow!("invalid CDP URL"))?;
    let scheme = parsed.scheme();
    if scheme != "ws" && scheme != "wss" {
        bail!("CDP URL scheme must be ws:// or wss://");
    }
    let Some(host) = parsed.host_str() else {
        bail!("CDP URL must include a host");
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("CDP URL must not embed userinfo credentials");
    }
    if parsed.fragment().is_some() {
        bail!("CDP URL must not include a fragment");
    }
    if scheme == "ws" && !crate::policy::is_loopback_host(host) {
        bail!("plaintext ws:// CDP URL is permitted for loopback hosts only; use wss:// for remote endpoints");
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
    let formatted_host = match parsed.host() {
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        _ => host.to_ascii_lowercase(),
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
