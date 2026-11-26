pub mod cef;
#[cfg(feature = "cronet")]
pub mod cronet;
#[cfg(feature = "cronet")]
pub mod cronet_ffi;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHROME_MAJOR: &str = "143";
const CHROME_FULL: &str = "143.0.7499.109";
pub const MARKDOWN_ACCEPT_HEADER: &str =
    "text/markdown, text/x-markdown;q=0.95, text/html;q=0.9, application/xhtml+xml;q=0.8, */*;q=0.7";
pub const IMAGE_ACCEPT_HEADER: &str =
    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";
pub const ASSET_ACCEPT_HEADER: &str = "*/*";
pub const DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RAW_MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubresourceDest {
    Image,
    Asset,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RequestHeaderOptions {
    #[serde(default)]
    pub accept_markdown: bool,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub subresource_dest: Option<SubresourceDest>,
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
    Network,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub message: String,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
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
    Direct {
        #[cfg(feature = "cronet")]
        engine: cronet::Engine,
        #[cfg(not(feature = "cronet"))]
        inner: reqwest::Client,
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

impl TransportClient {
    pub fn new(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        Self::new_direct(storage_path, proxy, timeout)
    }

    pub fn new_direct(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        #[cfg(feature = "cronet")]
        {
            let engine = cronet::Engine::new(storage_path, &chrome_user_agent(), proxy)?;
            Ok(Self {
                backend: TransportBackend::Direct { engine },
                default_timeout: timeout,
            })
        }
        #[cfg(not(feature = "cronet"))]
        {
            let _ = storage_path;
            let mut builder = reqwest::Client::builder()
                .user_agent(chrome_user_agent())
                .gzip(true)
                .brotli(true)
                .deflate(true);

            if let Some(proxy_url) = proxy {
                builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
            }

            Ok(Self {
                backend: TransportBackend::Direct {
                    inner: builder.build()?,
                },
                default_timeout: timeout,
            })
        }
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
        if cfg!(feature = "cronet") {
            "cronet"
        } else {
            "reqwest"
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
            TransportBackend::Direct {
                #[cfg(feature = "cronet")]
                engine,
                #[cfg(not(feature = "cronet"))]
                inner,
            } => {
                #[cfg(feature = "cronet")]
                {
                    let headers = request_header_pairs(true, url, opts);
                    let resp = engine.get(url, &headers, timeout, max_body_bytes).await?;
                    Ok(HttpResponse {
                        status: resp.status_code,
                        final_url: resp.final_url,
                        content_type: resp.content_type,
                        link_header: resp.link_header,
                        body: resp.body,
                    })
                }
                #[cfg(not(feature = "cronet"))]
                {
                    let headers = request_headers_reqwest(url, opts)?;
                    let fut = async {
                        let mut resp = inner
                            .get(url)
                            .headers(headers)
                            .timeout(timeout)
                            .send()
                            .await?;
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
                        let link_header = if link_values.is_empty() {
                            None
                        } else {
                            Some(link_values.join(", "))
                        };
                        let mut body = Vec::new();
                        while let Some(chunk) = resp.chunk().await? {
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
    format!(
        r#""Chromium";v="{CHROME_MAJOR}", "Google Chrome";v="{CHROME_MAJOR}", "Not:A-Brand";v="24""#
    )
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

fn request_header_pairs(
    include_zstd: bool,
    url: &str,
    opts: &RequestHeaderOptions,
) -> Vec<(&'static str, String)> {
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
    pairs
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

#[cfg(not(feature = "cronet"))]
fn request_headers_reqwest(
    url: &str,
    opts: &RequestHeaderOptions,
) -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    for (name, value) in request_header_pairs(false, url, opts) {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
