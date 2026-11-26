use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::transport::TransportErrorKind;

pub const PROTOCOL_VERSION: u32 = 2;
pub const BUILD_ID: &str = env!("WICK_BUILD_ID");
pub const SOCKET_NAME: &str = "daemon.sock";
pub const LOCK_NAME: &str = "daemon.lock";
pub const LOG_NAME: &str = "daemon.log";
pub const MAX_HEADER_LINE_BYTES: usize = 64 * 1024;
pub const MAX_FRAME_BODY_BYTES: usize = 256 * 1024 * 1024;

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SOCKET_NAME)
}

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOCK_NAME)
}

pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LOG_NAME)
}

pub fn storage_key(proxy: Option<&str>) -> String {
    let trimmed = proxy.map(str::trim).filter(|s| !s.is_empty());
    let Some(proxy_url) = trimmed else {
        return "direct".to_string();
    };
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in proxy_url.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("proxy-{hash:016x}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub pid: u32,
    pub protocol_version: u32,
    pub build_id: String,
    pub active_requests: usize,
    pub cronet_slots: usize,
    pub cef_renderers_alive: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    Ping,
    Shutdown,
    HttpGet {
        url: String,
        proxy: Option<String>,
        timeout_ms: u64,
        max_body_bytes: usize,
        #[serde(default)]
        accept_markdown: bool,
    },
    CefRender {
        url: String,
        wait_for_selector: Option<String>,
        proxy: Option<String>,
        timeout_ms: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponseHeader {
    pub ok: bool,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub build_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub active_requests: Option<usize>,
    #[serde(default)]
    pub cronet_slots: Option<usize>,
    #[serde(default)]
    pub cef_renderers_alive: Option<usize>,
    #[serde(default)]
    pub status_code: Option<u16>,
    #[serde(default)]
    pub final_url: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub link_header: Option<String>,
    #[serde(default)]
    pub body_bytes: usize,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_kind: Option<TransportErrorKind>,
    #[serde(default)]
    pub limit_bytes: Option<usize>,
}

impl DaemonResponseHeader {
    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            protocol_version: PROTOCOL_VERSION,
            build_id: BUILD_ID.to_string(),
            pid: None,
            active_requests: None,
            cronet_slots: None,
            cef_renderers_alive: None,
            status_code: None,
            final_url: None,
            content_type: None,
            link_header: None,
            body_bytes: 0,
            error: None,
            error_kind: None,
            limit_bytes: None,
        }
    }

    pub fn err(
        message: impl Into<String>,
        kind: Option<TransportErrorKind>,
        limit_bytes: Option<usize>,
    ) -> Self {
        Self {
            ok: false,
            protocol_version: PROTOCOL_VERSION,
            build_id: BUILD_ID.to_string(),
            pid: None,
            active_requests: None,
            cronet_slots: None,
            cef_renderers_alive: None,
            status_code: None,
            final_url: None,
            content_type: None,
            link_header: None,
            body_bytes: 0,
            error: Some(message.into()),
            error_kind: kind,
            limit_bytes,
        }
    }
}

pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(nl_pos) = chunk.iter().position(|&b| b == b'\n') {
            if buf.len() + nl_pos > max_bytes {
                bail!("daemon frame header exceeded {max_bytes} byte limit");
            }
            buf.extend_from_slice(&chunk[..nl_pos]);
            reader.consume(nl_pos + 1);
            break;
        }
        if buf.len() + chunk.len() > max_bytes {
            bail!("daemon frame header exceeded {max_bytes} byte limit");
        }
        buf.extend_from_slice(chunk);
        let consumed = chunk.len();
        reader.consume(consumed);
    }

    while buf.last() == Some(&b'\r') {
        buf.pop();
    }
    let line = String::from_utf8(buf)?;
    Ok(Some(line))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: &DaemonResponseHeader,
    body: &[u8],
) -> Result<()> {
    let mut encoded = serde_json::to_string(header)?;
    encoded.push('\n');
    writer.write_all(encoded.as_bytes()).await?;
    if !body.is_empty() {
        writer.write_all(body).await?;
    }
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<(DaemonResponseHeader, Vec<u8>)> {
    let Some(line) = read_bounded_line(reader, MAX_HEADER_LINE_BYTES).await? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed connection without a response",
        )
        .into());
    };
    let header: DaemonResponseHeader = serde_json::from_str(line.trim())?;
    if header.body_bytes > MAX_FRAME_BODY_BYTES {
        bail!(
            "daemon response body exceeded {} byte frame limit ({} bytes)",
            MAX_FRAME_BODY_BYTES,
            header.body_bytes
        );
    }
    let mut body = vec![0u8; header.body_bytes];
    if header.body_bytes > 0 {
        reader.read_exact(&mut body).await?;
    }
    Ok((header, body))
}
