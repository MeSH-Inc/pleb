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
pub const DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_RAW_MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

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
                .default_headers(chrome_headers_reqwest()?)
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
        match &self.backend {
            #[cfg(unix)]
            TransportBackend::Daemon {
                state_dir,
                proxy,
                idle_timeout,
                verbose,
                daemon_executable,
            } => {
                crate::daemon::daemon_http_get_with_executable(
                    state_dir,
                    proxy.as_deref(),
                    timeout,
                    max_body_bytes,
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
                    let headers = chrome_header_pairs(true);
                    let resp = engine.get(url, &headers, timeout, max_body_bytes).await?;
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
                        let mut resp = inner.get(url).timeout(timeout).send().await?;
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
