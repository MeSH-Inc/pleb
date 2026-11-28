pub mod cdp;
pub mod cef;
#[cfg(feature = "cronet")]
pub mod cronet;
#[cfg(feature = "cronet")]
pub mod cronet_ffi;
pub mod web_bot_auth;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHROME_MAJOR: usize = 154;
const CHROME_FULL: &str = "154.0.8037.49";
pub const MARKDOWN_ACCEPT_HEADER: &str =
    "text/markdown, text/x-markdown;q=0.95, text/html;q=0.9, application/xhtml+xml;q=0.8, */*;q=0.7";
pub const IMAGE_ACCEPT_HEADER: &str =
    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
pub const ASSET_ACCEPT_HEADER: &str = "*/*";
pub const DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RAW_MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticHeaders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_mitigated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_ray: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub www_authenticate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waf_action: Option<String>,
}

fn sanitize_header_value(raw: &str) -> Option<String> {
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= 128 {
        return Some(trimmed.to_string());
    }
    let mut end = 128;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(trimmed[..end].to_string())
}

impl DiagnosticHeaders {
    pub fn is_empty(&self) -> bool {
        self.server.is_none()
            && self.cf_mitigated.is_none()
            && self.cf_ray.is_none()
            && self.retry_after.is_none()
            && self.www_authenticate.is_none()
            && self.waf_action.is_none()
    }

    pub fn capture_header(&mut self, name: &str, value: &str) {
        let Some(sanitized) = sanitize_header_value(value) else {
            return;
        };
        if name.eq_ignore_ascii_case("server") {
            if self.server.is_none() {
                self.server = Some(sanitized);
            }
        } else if name.eq_ignore_ascii_case("cf-mitigated") {
            if self.cf_mitigated.is_none() {
                self.cf_mitigated = Some(sanitized);
            }
        } else if name.eq_ignore_ascii_case("cf-ray") {
            if self.cf_ray.is_none() {
                self.cf_ray = Some(sanitized);
            }
        } else if name.eq_ignore_ascii_case("retry-after") {
            if self.retry_after.is_none() {
                self.retry_after = Some(sanitized);
            }
        } else if name.eq_ignore_ascii_case("www-authenticate") {
            if self.www_authenticate.is_none() {
                self.www_authenticate = Some(sanitized);
            }
        } else if name.eq_ignore_ascii_case("x-amzn-waf-action") {
            if self
                .waf_action
                .as_deref()
                .is_none_or(|v| !v.starts_with("aws-waf:"))
            {
                self.waf_action = sanitize_header_value(&format!("aws-waf:{sanitized}"));
            }
        } else if name.eq_ignore_ascii_case("x-datadome") {
            if self.waf_action.is_none() {
                self.waf_action = Some("datadome".to_string());
            }
        } else if name.eq_ignore_ascii_case("x-sucuri-id") {
            if self.waf_action.is_none() {
                self.waf_action = Some("sucuri".to_string());
            }
        } else if name.eq_ignore_ascii_case("x-iinfo") && self.waf_action.is_none() {
            self.waf_action = Some("imperva".to_string());
        }
    }

    pub fn is_edge_waf(&self) -> bool {
        if self.cf_ray.is_some() || self.cf_mitigated.is_some() || self.waf_action.is_some() {
            return true;
        }
        self.server.as_deref().is_some_and(|s| {
            let lower = s.to_ascii_lowercase();
            [
                "cloudflare",
                "akamai",
                "cloudfront",
                "fastly",
                "sucuri",
                "imperva",
                "incapsula",
                "ddos-guard",
                "datadome",
                "varnish",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
        })
    }

    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(ref v) = self.server {
            parts.push(format!("server={v}"));
        }
        if let Some(ref v) = self.cf_mitigated {
            parts.push(format!("cf-mitigated={v}"));
        }
        if let Some(ref v) = self.cf_ray {
            parts.push(format!("cf-ray={v}"));
        }
        if let Some(ref v) = self.waf_action {
            parts.push(format!("waf={v}"));
        }
        if let Some(ref v) = self.retry_after {
            parts.push(format!("retry-after={v}"));
        }
        if let Some(ref v) = self.www_authenticate {
            parts.push(format!("www-authenticate={v}"));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubresourceDest {
    Image,
    Asset,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebBotAuthHeaders {
    pub signature: String,
    pub signature_input: String,
    pub signature_agent: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RequestHeaderOptions {
    #[serde(default)]
    pub accept_markdown: bool,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub subresource_dest: Option<SubresourceDest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_bot_auth: Option<WebBotAuthHeaders>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_redirects: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportErrorKind {
    Timeout,
    LoadError,
    InvalidSelector,
    ResponseTooLarge,
    Canceled,
    ProtocolMismatch,
    RedirectBlocked,
    Network,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

pub fn is_retryable_network_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TransportError>()
        .is_some_and(|te| te.kind == TransportErrorKind::Network && te.retryable)
}

pub fn transport_error_kind(err: &anyhow::Error) -> Option<TransportErrorKind> {
    if let Some(te) = err.downcast_ref::<TransportError>() {
        return Some(te.kind);
    }
    if err.is::<ResponseTooLargeError>() {
        return Some(TransportErrorKind::ResponseTooLarge);
    }
    if err.is::<cef::CefTimeoutError>() {
        return Some(TransportErrorKind::Timeout);
    }
    if err.is::<cef::CefProtocolError>() {
        return Some(TransportErrorKind::ProtocolMismatch);
    }
    None
}

#[derive(Debug, thiserror::Error)]
#[error("response body exceeded {limit_bytes} byte limit")]
pub struct ResponseTooLargeError {
    pub limit_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub link_header: Option<String>,
    pub location_header: Option<String>,
    pub diagnostics: DiagnosticHeaders,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        crate::extract::decode_text_bytes(&self.body, self.content_type.as_deref())
    }

    pub fn is_html(&self) -> bool {
        crate::extract::is_html_content_type(self.content_type.as_deref())
    }
}

enum TransportBackend {
    #[cfg(feature = "cronet")]
    Cronet { engine: cronet::Engine },
    Reqwest {
        inner: reqwest::Client,
        no_redirect_client: reqwest::Client,
    },
    #[cfg(unix)]
    Daemon {
        state_dir: PathBuf,
        proxy: Option<String>,
        idle_timeout: Duration,
        verbose: bool,
        daemon_executable: Option<PathBuf>,
    },
}

/// Network transport client backed by Cronet/reqwest directly or the warm daemon.
pub struct TransportClient {
    backend: TransportBackend,
    default_timeout: Duration,
}

fn build_reqwest_client(
    proxy: Option<&str>,
    redirect_policy: reqwest::redirect::Policy,
    http1_only: bool,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(chrome_user_agent())
        .redirect(redirect_policy)
        .cookie_store(true)
        .gzip(true)
        .brotli(true)
        .deflate(true);

    if http1_only {
        builder = builder.http1_only();
    }

    if let Some(proxy_url) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
    }

    Ok(builder.build()?)
}

impl TransportClient {
    pub fn new(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        Self::new_direct(storage_path, proxy, timeout)
    }

    pub fn new_direct(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        #[cfg(feature = "cronet")]
        {
            let engine = cronet::Engine::new(storage_path, &chrome_user_agent(), proxy)?;
            Ok(Self {
                backend: TransportBackend::Cronet { engine },
                default_timeout: timeout,
            })
        }
        #[cfg(not(feature = "cronet"))]
        {
            let _ = storage_path;
            Self::new_reqwest(proxy, timeout, false)
        }
    }

    /// Build a direct HTTP/1.1-only `reqwest` client used as a bounded fallback when
    /// primary HTTP/2 or QUIC connections reset across retries.
    pub fn new_http1(proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        Self::new_reqwest(proxy, timeout, true)
    }

    fn new_reqwest(proxy: Option<&str>, timeout: Duration, http1_only: bool) -> Result<Self> {
        let inner = build_reqwest_client(proxy, reqwest_redirect_policy(), http1_only)?;
        let no_redirect_client =
            build_reqwest_client(proxy, reqwest::redirect::Policy::none(), http1_only)?;

        Ok(Self {
            backend: TransportBackend::Reqwest {
                inner,
                no_redirect_client,
            },
            default_timeout: timeout,
        })
    }

    #[cfg(unix)]
    pub fn new_daemon(
        state_dir: PathBuf,
        proxy: Option<String>,
        timeout: Duration,
        idle_timeout: Duration,
        verbose: bool,
    ) -> Self {
        Self {
            backend: TransportBackend::Daemon {
                state_dir,
                proxy,
                idle_timeout,
                verbose,
                daemon_executable: None,
            },
            default_timeout: timeout,
        }
    }

    #[cfg(unix)]
    pub fn with_daemon_executable(mut self, daemon_executable: Option<PathBuf>) -> Self {
        if let TransportBackend::Daemon {
            daemon_executable: ref mut slot,
            ..
        } = self.backend
        {
            *slot = daemon_executable;
        }
        self
    }

    pub fn transport_name(&self) -> &'static str {
        match self.backend {
            #[cfg(feature = "cronet")]
            TransportBackend::Cronet { .. } => "cronet",
            TransportBackend::Reqwest { .. } => "reqwest",
            #[cfg(unix)]
            TransportBackend::Daemon { .. } => {
                if cfg!(feature = "cronet") {
                    "cronet"
                } else {
                    "reqwest"
                }
            }
        }
    }

    pub async fn get(&self, url: &str) -> Result<HttpResponse> {
        self.get_with_limits(
            url,
            self.default_timeout,
            DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES,
        )
        .await
    }

    pub async fn get_with_limits(
        &self,
        url: &str,
        timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<HttpResponse> {
        self.get_with_options(url, timeout, max_body_bytes, false)
            .await
    }

    pub async fn get_with_request_options(
        &self,
        url: &str,
        timeout: Duration,
        max_body_bytes: usize,
        opts: &RequestHeaderOptions,
    ) -> Result<HttpResponse> {
        if let Some(ref wba) = opts.web_bot_auth {
            validate_web_bot_auth_headers(wba)?;
        }

        match &self.backend {
            #[cfg(unix)]
            TransportBackend::Daemon {
                state_dir,
                proxy,
                idle_timeout,
                verbose,
                daemon_executable,
            } => {
                crate::daemon::client::daemon_http_get_with_request_options(
                    state_dir,
                    proxy.as_deref(),
                    timeout,
                    max_body_bytes,
                    opts,
                    *idle_timeout,
                    *verbose,
                    url,
                    daemon_executable.as_deref(),
                )
                .await
            }
            #[cfg(feature = "cronet")]
            TransportBackend::Cronet { engine } => {
                let headers = request_header_pairs(true, url, opts)?;
                let disable_redirects = opts.disable_redirects || opts.web_bot_auth.is_some();
                let resp = engine
                    .get_with_redirects(url, &headers, timeout, max_body_bytes, disable_redirects)
                    .await?;
                Ok(HttpResponse {
                    status: resp.status_code,
                    final_url: resp.final_url,
                    content_type: resp.content_type,
                    link_header: resp.link_header,
                    location_header: resp.location_header,
                    diagnostics: resp.diagnostics,
                    body: resp.body,
                })
            }
            TransportBackend::Reqwest {
                inner,
                no_redirect_client,
            } => {
                let headers = request_headers_reqwest(url, opts)?;
                let no_redirects = opts.disable_redirects || opts.web_bot_auth.is_some();
                let client = if no_redirects {
                    no_redirect_client
                } else {
                    inner
                };
                execute_reqwest_get(client, url, headers, timeout, max_body_bytes).await
            }
        }
    }

    pub async fn get_with_options(
        &self,
        url: &str,
        timeout: Duration,
        max_body_bytes: usize,
        accept_markdown: bool,
    ) -> Result<HttpResponse> {
        let opts = RequestHeaderOptions {
            accept_markdown,
            ..Default::default()
        };
        self.get_with_request_options(url, timeout, max_body_bytes, &opts)
            .await
    }
}

pub fn chrome_user_agent() -> String {
    let os_token = if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else if cfg!(target_os = "linux") {
        "X11; Linux x86_64"
    } else {
        "Windows NT 10.0; Win64; x64"
    };

    format!(
        "Mozilla/5.0 ({os_token}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{CHROME_FULL} Safari/537.36"
    )
}

fn sec_ch_ua_value() -> String {
    sec_ch_ua_for_major(CHROME_MAJOR)
}

fn sec_ch_ua_for_major(major: usize) -> String {
    const GREASE_CHARS: [char; 11] = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'];
    const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let major_str = major.to_string();
    let grease_brand = format!(
        "Not{}A{}Brand",
        GREASE_CHARS[major % GREASE_CHARS.len()],
        GREASE_CHARS[(major + 1) % GREASE_CHARS.len()]
    );
    let brands = [
        (
            grease_brand.as_str(),
            GREASE_VERSIONS[major % GREASE_VERSIONS.len()],
        ),
        ("Chromium", major_str.as_str()),
        ("Google Chrome", major_str.as_str()),
    ];
    let order = ORDERS[major % ORDERS.len()];
    let mut shuffled = [("", ""); 3];
    for (idx, &slot) in order.iter().enumerate() {
        shuffled[slot] = brands[idx];
    }
    shuffled
        .iter()
        .map(|(brand, ver)| format!(r#""{brand}";v="{ver}""#))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sec_ch_ua_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "\"macOS\""
    } else if cfg!(target_os = "linux") {
        "\"Linux\""
    } else {
        "\"Windows\""
    }
}

fn compute_sec_fetch_site(referer: Option<&str>, target_url: &str) -> &'static str {
    let Some(referer_str) = referer.filter(|s| !s.is_empty()) else {
        return "none";
    };
    let (Ok(ref_url), Ok(tgt_url)) = (url::Url::parse(referer_str), url::Url::parse(target_url))
    else {
        return "none";
    };
    if ref_url.origin() == tgt_url.origin() {
        return "same-origin";
    }
    if ref_url.scheme() == tgt_url.scheme() {
        if let (Some(ref_host), Some(tgt_host)) = (ref_url.host_str(), tgt_url.host_str()) {
            if crate::policy::source::same_origin_or_parent_domain(ref_host, tgt_host) {
                return "same-site";
            }
        }
    }
    "cross-site"
}

fn validate_web_bot_auth_headers(wba: &WebBotAuthHeaders) -> Result<()> {
    for value in [&wba.signature, &wba.signature_input, &wba.signature_agent] {
        if value.bytes().any(|b| b.is_ascii_control() || !b.is_ascii()) {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Network,
                "Web Bot Auth header contains disallowed control or non-ASCII characters",
            )));
        }
    }
    Ok(())
}

fn request_header_pairs(
    include_zstd: bool,
    url: &str,
    opts: &RequestHeaderOptions,
) -> Result<Vec<(&'static str, String)>> {
    let accept_encoding = if include_zstd {
        "gzip, deflate, br, zstd"
    } else {
        "gzip, deflate, br"
    };
    let mut pairs = if let Some(dest) = opts.subresource_dest {
        let (accept, sec_fetch_dest) = match dest {
            SubresourceDest::Image => (IMAGE_ACCEPT_HEADER, "image"),
            SubresourceDest::Asset => (ASSET_ACCEPT_HEADER, "empty"),
        };
        vec![
            ("Accept", accept.to_string()),
            ("Accept-Language", "en-US,en;q=0.9".to_string()),
            ("Accept-Encoding", accept_encoding.to_string()),
            ("Sec-Ch-Ua", sec_ch_ua_value()),
            ("Sec-Ch-Ua-Mobile", "?0".to_string()),
            ("Sec-Ch-Ua-Platform", sec_ch_ua_platform().to_string()),
            ("Sec-Fetch-Dest", sec_fetch_dest.to_string()),
            ("Sec-Fetch-Mode", "no-cors".to_string()),
            (
                "Sec-Fetch-Site",
                compute_sec_fetch_site(opts.referer.as_deref(), url).to_string(),
            ),
        ]
    } else {
        let mut nav = chrome_header_pairs(include_zstd);
        if opts.accept_markdown {
            if let Some((_, value)) = nav.iter_mut().find(|(name, _)| *name == "Accept") {
                *value = MARKDOWN_ACCEPT_HEADER.to_string();
            }
        }
        nav
    };

    if let Some(referer) = opts.referer.as_deref().filter(|s| !s.is_empty()) {
        pairs.push(("Referer", referer.to_string()));
    }
    if let Some(ref wba) = opts.web_bot_auth {
        validate_web_bot_auth_headers(wba)?;
        pairs.push(("Signature", wba.signature.clone()));
        pairs.push(("Signature-Input", wba.signature_input.clone()));
        pairs.push(("Signature-Agent", wba.signature_agent.clone()));
    }
    Ok(pairs)
}

fn chrome_header_pairs(include_zstd: bool) -> Vec<(&'static str, String)> {
    let accept_encoding = if include_zstd {
        "gzip, deflate, br, zstd"
    } else {
        "gzip, deflate, br"
    };
    vec![
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8"
                .to_string(),
        ),
        ("Accept-Language", "en-US,en;q=0.9".to_string()),
        ("Accept-Encoding", accept_encoding.to_string()),
        ("Cache-Control", "max-age=0".to_string()),
        ("Sec-Ch-Ua", sec_ch_ua_value()),
        ("Sec-Ch-Ua-Mobile", "?0".to_string()),
        ("Sec-Ch-Ua-Platform", sec_ch_ua_platform().to_string()),
        ("Sec-Fetch-Dest", "document".to_string()),
        ("Sec-Fetch-Mode", "navigate".to_string()),
        ("Sec-Fetch-Site", "none".to_string()),
        ("Sec-Fetch-User", "?1".to_string()),
        ("Upgrade-Insecure-Requests", "1".to_string()),
    ]
}

async fn execute_reqwest_get(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
    max_body_bytes: usize,
) -> Result<HttpResponse> {
    let fut = async {
        let mut resp = client
            .get(url)
            .headers(headers)
            .timeout(timeout)
            .send()
            .await
            .map_err(map_reqwest_send_error)?;
        let final_url = resp.url().to_string();
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let link_values: Vec<&str> = resp
            .headers()
            .get_all(reqwest::header::LINK)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .collect();
        let link_header = (!link_values.is_empty()).then(|| link_values.join(", "));
        let location_header = if (300..400).contains(&status) {
            resp.headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        } else {
            None
        };
        let mut diagnostics = DiagnosticHeaders::default();
        for (name, value) in resp.headers() {
            if let Ok(val_str) = value.to_str() {
                diagnostics.capture_header(name.as_str(), val_str);
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(map_reqwest_send_error)? {
            if body.len().saturating_add(chunk.len()) > max_body_bytes {
                return Err(anyhow::Error::new(ResponseTooLargeError {
                    limit_bytes: max_body_bytes,
                }));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            final_url,
            content_type,
            link_header,
            location_header,
            diagnostics,
            body,
        })
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(anyhow::Error::new(TransportError::new(
            TransportErrorKind::Timeout,
            format!("request timed out after {:?}", timeout),
        ))),
    }
}

fn request_headers_reqwest(
    url: &str,
    opts: &RequestHeaderOptions,
) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    for (name, value) in request_header_pairs(false, url, opts)? {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(headers)
}

#[derive(Debug, thiserror::Error)]
#[error("blocked unsafe redirect to {target}")]
struct BlockedRedirectError {
    target: String,
}

fn reqwest_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let origin = attempt.previous().first().unwrap_or(attempt.url());
        if !crate::policy::is_safe_redirect_url(origin, attempt.url()) {
            let target = attempt.url().to_string();
            return attempt.error(BlockedRedirectError { target });
        }
        attempt.follow()
    })
}

fn map_reqwest_send_error(err: reqwest::Error) -> anyhow::Error {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(source) = cur {
        if let Some(blocked) = source.downcast_ref::<BlockedRedirectError>() {
            return anyhow::Error::new(TransportError::new(
                TransportErrorKind::RedirectBlocked,
                blocked.to_string(),
            ));
        }
        cur = source.source();
    }
    if err.is_timeout() {
        return anyhow::Error::new(TransportError::new(
            TransportErrorKind::Timeout,
            err.to_string(),
        ));
    }
    let retryable = !err.is_builder() && !err.is_connect();
    anyhow::Error::new(
        TransportError::new(TransportErrorKind::Network, err.to_string()).with_retryable(retryable),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "cronet"))]
    #[tokio::test]
    async fn reqwest_blocks_ssrf_redirects_and_allows_same_host_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let resp = match path {
                        "/ok" => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        "/redir-ok" => "HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        "/redir-imds" => "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let client = TransportClient::new_direct(Path::new("/tmp"), None, Duration::from_secs(5))
            .expect("reqwest client should build");
        let ok = client
            .get(&format!("http://{addr}/redir-ok"))
            .await
            .expect("same-host local redirect should succeed");
        assert_eq!(ok.status, 200);
        assert_eq!(ok.body, b"ok");

        let err = client
            .get(&format!("http://{addr}/redir-imds"))
            .await
            .expect_err("IMDS redirect must be blocked");
        assert_eq!(
            transport_error_kind(&err),
            Some(TransportErrorKind::RedirectBlocked)
        );
        server.abort();
    }

    #[test]
    fn generates_chromium_greased_sec_ch_ua_for_chrome_154() {
        assert_eq!(
            sec_ch_ua_for_major(154),
            r#""Chromium";v="154", "Google Chrome";v="154", "Not A(Brand";v="99""#
        );
    }

    #[test]
    fn computes_sec_fetch_site_for_same_origin_same_site_cross_site_and_none() {
        assert_eq!(
            compute_sec_fetch_site(None, "https://cdn.example.com/a.png"),
            "none"
        );
        assert_eq!(
            compute_sec_fetch_site(
                Some("https://docs.example.com/guide"),
                "https://docs.example.com/img/arch.png"
            ),
            "same-origin"
        );
        assert_eq!(
            compute_sec_fetch_site(
                Some("https://docs.example.com/"),
                "https://cdn.example.com/img/arch.png"
            ),
            "same-site"
        );
        assert_eq!(
            compute_sec_fetch_site(
                Some("http://docs.example.com/"),
                "https://cdn.example.com/img/arch.png"
            ),
            "cross-site"
        );
        assert_eq!(
            compute_sec_fetch_site(
                Some("https://a.github.io/"),
                "https://b.github.io/diagram.png"
            ),
            "cross-site"
        );
        assert_eq!(
            compute_sec_fetch_site(
                Some("https://example.com/"),
                "https://user-images.githubusercontent.com/123/arch.png"
            ),
            "cross-site"
        );
    }

    #[test]
    fn diagnostic_headers_captures_sanitizes_and_detects_waf() {
        let mut diag = DiagnosticHeaders::default();
        assert!(diag.is_empty());

        diag.capture_header("Server", "  cloudflare\r\n  ");
        diag.capture_header("CF-Ray", "90123abc-SJC");
        diag.capture_header("CF-Mitigated", "challenge");
        diag.capture_header("Retry-After", " 15 ");
        diag.capture_header("WWW-Authenticate", "Bearer");
        diag.capture_header("X-Amzn-Waf-Action", "captcha");
        // First-seen wins for duplicate server header and oversized value truncates at char boundary <= 128 bytes.
        diag.capture_header("Server", "nginx");
        let mut long_diag = DiagnosticHeaders::default();
        let long_val = "é".repeat(80);
        long_diag.capture_header("Server", &long_val);
        assert!(long_diag.server.as_ref().unwrap().len() <= 128);

        assert!(!diag.is_empty());
        assert!(diag.is_edge_waf());
        assert_eq!(diag.server.as_deref(), Some("cloudflare"));
        assert_eq!(diag.cf_ray.as_deref(), Some("90123abc-SJC"));
        assert_eq!(diag.cf_mitigated.as_deref(), Some("challenge"));
        assert_eq!(diag.retry_after.as_deref(), Some("15"));
        assert_eq!(diag.www_authenticate.as_deref(), Some("Bearer"));
        assert_eq!(diag.waf_action.as_deref(), Some("aws-waf:captcha"));
        let summary = diag.summary().unwrap();
        assert!(summary.contains("server=cloudflare"));
        assert!(summary.contains("cf-mitigated=challenge"));
        assert!(summary.contains("cf-ray=90123abc-SJC"));
        assert!(summary.contains("waf=aws-waf:captcha"));
        assert!(summary.contains("retry-after=15"));

        let mut origin_only = DiagnosticHeaders::default();
        origin_only.capture_header("Server", "nginx/1.26");
        assert!(!origin_only.is_edge_waf());

        let mut datadome = DiagnosticHeaders::default();
        datadome.capture_header("X-DataDome", "protected");
        assert!(datadome.is_edge_waf());
        assert_eq!(datadome.waf_action.as_deref(), Some("datadome"));
    }

    #[cfg(not(feature = "cronet"))]
    #[tokio::test]
    async fn reqwest_persists_session_cookie_across_redirect_to_avoid_403() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
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
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let has_cookie = req.lines().any(|l| {
                        l.to_ascii_lowercase().starts_with("cookie:") && l.contains("pleb_sess=ok")
                    });
                    let resp = match path {
                        "/start" => "HTTP/1.1 302 Found\r\nLocation: /protected\r\nSet-Cookie: pleb_sess=ok; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        "/protected" if has_cookie => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nServer: cloudflare\r\nContent-Length: 10\r\nConnection: close\r\n\r\ncookie_ok\n",
                        "/protected" => "HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nServer: cloudflare\r\nContent-Length: 9\r\nConnection: close\r\n\r\nForbidden",
                        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        let client = TransportClient::new_direct(Path::new("/tmp"), None, Duration::from_secs(5))
            .expect("reqwest client should build");
        let resp = client
            .get(&format!("http://{addr}/start"))
            .await
            .expect("cookie redirect should succeed");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"cookie_ok\n");
        assert_eq!(resp.diagnostics.server.as_deref(), Some("cloudflare"));
        server.abort();
    }

    #[tokio::test]
    async fn http1_fallback_client_enforces_redirects_and_classifies_retryable_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");
                    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
                    match path {
                        "/h1" => {
                            assert!(
                                first_line.ends_with("HTTP/1.1"),
                                "expected HTTP/1.1 request line, got: {first_line}"
                            );
                            let _ = socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nh1")
                                .await;
                        }
                        "/drop" => drop(socket),
                        "/redir-imds" => {
                            let _ = socket
                                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
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

        let h1 = TransportClient::new_http1(None, Duration::from_secs(5))
            .expect("http1 fallback client should build");
        assert_eq!(h1.transport_name(), "reqwest");

        let ok = h1.get(&format!("http://{addr}/h1")).await.unwrap();
        assert_eq!(ok.status, 200);
        assert_eq!(ok.body, b"h1");

        let blocked_redir = h1
            .get(&format!("http://{addr}/redir-imds"))
            .await
            .unwrap_err();
        assert_eq!(
            transport_error_kind(&blocked_redir),
            Some(TransportErrorKind::RedirectBlocked)
        );
        assert!(!is_retryable_network_error(&blocked_redir));

        let drop_err = h1.get(&format!("http://{addr}/drop")).await.unwrap_err();
        assert_eq!(
            transport_error_kind(&drop_err),
            Some(TransportErrorKind::Network)
        );
        assert!(is_retryable_network_error(&drop_err));

        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        let refused_err = h1
            .get(&format!("http://{closed_addr}/refused"))
            .await
            .unwrap_err();
        assert_eq!(
            transport_error_kind(&refused_err),
            Some(TransportErrorKind::Network)
        );
        assert!(
            !is_retryable_network_error(&refused_err),
            "connection refused must not be marked retryable"
        );

        server.abort();
    }
}
