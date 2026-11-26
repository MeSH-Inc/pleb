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
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

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
        match self.content_type.as_deref() {
            Some(ct) => {
                let mime = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                mime.is_empty() || mime == "text/html" || mime == "application/xhtml+xml"
            }
            None => true,
        }
    }
}

/// Network transport client backed by Cronet (when enabled) or reqwest.
pub struct TransportClient {
    #[cfg(feature = "cronet")]
    engine: cronet::Engine,
    #[cfg(not(feature = "cronet"))]
    inner: reqwest::Client,
}

impl TransportClient {
    pub fn new(storage_path: &Path, proxy: Option<&str>, timeout: Duration) -> Result<Self> {
        #[cfg(feature = "cronet")]
        {
            let engine = cronet::Engine::new(storage_path, &chrome_user_agent(), proxy, timeout)?;
            Ok(Self { engine })
        }
        #[cfg(not(feature = "cronet"))]
        {
            let _ = storage_path;
            let mut builder = reqwest::Client::builder()
                .user_agent(chrome_user_agent())
                .default_headers(chrome_headers_reqwest())
                .gzip(true)
                .brotli(true)
                .deflate(true)
                .timeout(timeout);

            if let Some(proxy_url) = proxy {
                builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
            }

            Ok(Self {
                inner: builder.build()?,
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
        #[cfg(feature = "cronet")]
        {
            let headers = chrome_header_pairs();
            let resp = self
                .engine
                .get(url, &headers, DEFAULT_MAX_RESPONSE_BYTES)
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
            let mut resp = self.inner.get(url).send().await?;
            let final_url = resp.url().to_string();
            let status = resp.status().as_u16();
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if body.len().saturating_add(chunk.len()) > DEFAULT_MAX_RESPONSE_BYTES {
                    anyhow::bail!(
                        "response body exceeded {} byte limit",
                        DEFAULT_MAX_RESPONSE_BYTES
                    );
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpResponse {
                status,
                final_url,
                content_type,
                body,
            })
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

#[cfg(feature = "cronet")]
fn chrome_header_pairs() -> Vec<(&'static str, String)> {
    vec![
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8"
                .to_string(),
        ),
        ("Accept-Language", "en-US,en;q=0.9".to_string()),
        ("Accept-Encoding", "gzip, deflate, br, zstd".to_string()),
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
fn chrome_headers_reqwest() -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut h = HeaderMap::new();
    h.insert(
        "Accept",
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
        ),
    );
    h.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.9"));
    h.insert(
        "Accept-Encoding",
        HeaderValue::from_static("gzip, deflate, br"),
    );
    h.insert("Cache-Control", HeaderValue::from_static("max-age=0"));
    h.insert(
        "Sec-Ch-Ua",
        HeaderValue::from_str(&sec_ch_ua_value()).unwrap(),
    );
    h.insert("Sec-Ch-Ua-Mobile", HeaderValue::from_static("?0"));
    h.insert(
        "Sec-Ch-Ua-Platform",
        HeaderValue::from_static(sec_ch_ua_platform()),
    );
    h.insert("Sec-Fetch-Dest", HeaderValue::from_static("document"));
    h.insert("Sec-Fetch-Mode", HeaderValue::from_static("navigate"));
    h.insert("Sec-Fetch-Site", HeaderValue::from_static("none"));
    h.insert("Sec-Fetch-User", HeaderValue::from_static("?1"));
    h.insert("Upgrade-Insecure-Requests", HeaderValue::from_static("1"));
    h
}
