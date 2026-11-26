pub mod cef;
#[cfg(unix)]
pub mod cef_daemon;
#[cfg(feature = "cronet")]
pub mod cronet;
#[cfg(feature = "cronet")]
pub mod cronet_ffi;

use anyhow::Result;
use std::path::Path;
use std::time::Duration;

const CHROME_MAJOR: &str = "143";
const CHROME_FULL: &str = "143.0.7499.109";
pub const DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RAW_MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

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
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn is_html(&self) -> bool {
        crate::extract::is_html_content_type(self.content_type.as_deref())
    }
}

/// Network transport client backed by Cronet (when enabled) or reqwest.
pub struct TransportClient {
    #[cfg(feature = "cronet")]
    engine: cronet::Engine,
    #[cfg(not(feature = "cronet"))]
    inner: reqwest::Client,
    default_timeout: Duration,
}

impl TransportClient {
    pub fn new(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        #[cfg(feature = "cronet")]
        {
            let engine = cronet::Engine::new(storage_path, &chrome_user_agent(), proxy)?;
            Ok(Self {
                engine,
                default_timeout: timeout,
            })
        }
        #[cfg(not(feature = "cronet"))]
        {
            let _ = storage_path;
            let mut builder = reqwest::Client::builder()
                .user_agent(chrome_user_agent())
                .default_headers(chrome_headers_reqwest()?)
                .gzip(true)
                .brotli(true)
                .deflate(true);

            if let Some(proxy_url) = proxy {
                builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
            }

            Ok(Self {
                inner: builder.build()?,
                default_timeout: timeout,
            })
        }
    }

    pub fn transport_name(&self) -> &'static str {
        if cfg!(feature = "cronet") {
            "cronet"
        } else {
            "reqwest"
        }
    }

    pub async fn get(&self, url: &str) -> Result<HttpResponse> {
        self.get_with_limits(url, self.default_timeout, DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES)
            .await
    }

    pub async fn get_with_limits(
        &self,
        url: &str,
        timeout: Duration,
        max_body_bytes: usize,
    ) -> Result<HttpResponse> {
        #[cfg(feature = "cronet")]
        {
            let headers = chrome_header_pairs(true);
            let resp = self
                .engine
                .get(url, &headers, timeout, max_body_bytes)
                .await?;
            Ok(HttpResponse {
                status: resp.status_code,
                final_url: resp.final_url,
                content_type: resp.content_type,
                body: resp.body,
            })
        }
        #[cfg(not(feature = "cronet"))]
        {
            let fut = async {
                let mut resp = self.inner.get(url).timeout(timeout).send().await?;
                let final_url = resp.url().to_string();
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
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
                    body,
                })
            };

            match tokio::time::timeout(timeout, fut).await {
                Ok(res) => res,
                Err(_) => anyhow::bail!("request timed out after {:?}", timeout),
            }
        }
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
fn chrome_headers_reqwest() -> Result<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut headers = HeaderMap::new();
    for (name, value) in chrome_header_pairs(false) {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(headers)
}
