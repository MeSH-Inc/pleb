use serde::Serialize;
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::context::RuntimeContext;
use crate::extract::{self, ExtractOptions, OutputFormat, ParsedHtml};
use crate::policy::{
    classify_response_with_parsed, detect_interstitial_challenge, plan_strategy, PrimaryTransport,
    RenderMode, ResponseClassification,
};
use crate::transport::cef::{
    CefProtocolError, CefRenderOptions, CefRenderResult, CefRenderer, CefTimeoutError,
};

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("blocked by robots.txt on {host}: {url}")]
    RobotsDisallowed { host: String, url: String },

    #[error("HTTP {status} blocked fetching {url}: {detail}")]
    Blocked {
        url: String,
        status: u16,
        detail: String,
    },

    #[error("challenge encountered (HTTP {status}) on {url}: {reason}")]
    Challenge {
        url: String,
        status: u16,
        reason: String,
    },

    #[error("HTTP {status} error fetching {url}")]
    HttpError { url: String, status: u16 },

    #[error(
        "unsupported binary content ({}) on {url} ({byte_count} bytes); use --format raw --output <path>",
        content_type.as_deref().unwrap_or("unknown")
    )]
    UnsupportedContentType {
        url: String,
        content_type: Option<String>,
        byte_count: usize,
    },

    #[error("CEF renderer unavailable: {0}")]
    CefUnavailable(String),

    #[error("timed out fetching {url}: {detail}")]
    Timeout { url: String, detail: String },

    #[error(transparent)]
    Transport(#[from] anyhow::Error),
}

impl FetchError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::RobotsDisallowed { .. } | Self::Blocked { .. } | Self::Challenge { .. } => 2,
            Self::HttpError { .. } => 3,
            Self::UnsupportedContentType { .. } => 5,
            Self::InvalidUrl(_)
            | Self::CefUnavailable(_)
            | Self::Timeout { .. }
            | Self::Transport(_) => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub extract: ExtractOptions,
    pub render: RenderMode,
    pub wait_for_selector: Option<String>,
    pub respect_robots: bool,
    pub apply_rewrites: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            extract: ExtractOptions::default(),
            render: RenderMode::Auto,
            wait_for_selector: None,
            respect_robots: false,
            apply_rewrites: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchOutput {
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub strategy: String,
    pub escalated: bool,
    pub truncated: bool,
    pub timing_ms: u64,
}

pub struct FetchHtmlResult {
    pub html: String,
    pub url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub parsed: Option<ParsedHtml>,
}

#[derive(Debug, Clone)]
pub struct FetchRawResult {
    pub bytes: Vec<u8>,
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub timing_ms: u64,
}

struct AcquiredPage {
    body: String,
    final_url: String,
    status_code: u16,
    content_type: Option<String>,
    strategy: String,
    escalated: bool,
    parsed: Option<ParsedHtml>,
}

pub async fn fetch_document(
    ctx: &RuntimeContext,
    url: &str,
    options: &FetchOptions,
) -> std::result::Result<FetchOutput, FetchError> {
    let start = Instant::now();
    if options.extract.format == OutputFormat::Raw {
        return Err(FetchError::Transport(anyhow::anyhow!(
            "use fetch_raw for OutputFormat::Raw"
        )));
    }

    let parsed = parse_http_url(url)?;
    let effective = resolve_target_url(ctx, &parsed, options.apply_rewrites);
    let effective_str = effective.as_str();
    let host = effective
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?;

    if options.respect_robots && !ctx.robots.is_allowed(&ctx.client, effective_str).await {
        return Err(FetchError::RobotsDisallowed {
            host: host.to_string(),
            url: effective_str.to_string(),
        });
    }

    let page = acquire_page(
        ctx,
        &effective,
        options.render,
        options.wait_for_selector.as_deref(),
    )
    .await?;

    let final_url_parsed = url::Url::parse(&page.final_url).unwrap_or_else(|_| effective.clone());
    let extracted = extract::extract_document_parsed(
        &page.body,
        page.parsed,
        &final_url_parsed,
        page.content_type.as_deref(),
        &options.extract,
    )?;

    Ok(FetchOutput {
        url: url.to_string(),
        final_url: page.final_url,
        status_code: page.status_code,
        content_type: page.content_type,
        title: extracted.title,
        content: extracted.content,
        strategy: page.strategy,
        escalated: page.escalated,
        truncated: extracted.truncated,
        timing_ms: start.elapsed().as_millis() as u64,
    })
}

pub async fn fetch_html(
    ctx: &RuntimeContext,
    url: &str,
    respect_robots: bool,
) -> std::result::Result<FetchHtmlResult, FetchError> {
    let parsed = parse_http_url(url)?;
    let effective = resolve_target_url(ctx, &parsed, true);
    let effective_str = effective.as_str();
    let host = effective
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?;

    if respect_robots && !ctx.robots.is_allowed(&ctx.client, effective_str).await {
        return Err(FetchError::RobotsDisallowed {
            host: host.to_string(),
            url: effective_str.to_string(),
        });
    }

    let page = acquire_page(ctx, &effective, RenderMode::Auto, None).await?;
    Ok(FetchHtmlResult {
        html: page.body,
        url: page.final_url,
        status_code: page.status_code,
        content_type: page.content_type,
        parsed: page.parsed,
    })
}

pub async fn fetch_raw(
    ctx: &RuntimeContext,
    url: &str,
    respect_robots: bool,
) -> std::result::Result<FetchRawResult, FetchError> {
    let start = Instant::now();
    let parsed = parse_http_url(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?;

    if respect_robots && !ctx.robots.is_allowed(&ctx.client, parsed.as_str()).await {
        return Err(FetchError::RobotsDisallowed {
            host: host.to_string(),
            url: url.to_string(),
        });
    }

    let resp = ctx.client.get(parsed.as_str()).await?;
    let timing_ms = start.elapsed().as_millis() as u64;

    if resp.status >= 400 {
        return Err(FetchError::HttpError {
            url: resp.final_url,
            status: resp.status,
        });
    }

    ctx.site_cache
        .record(host, ctx.client.transport_name(), timing_ms);

    Ok(FetchRawResult {
        bytes: resp.body,
        url: url.to_string(),
        final_url: resp.final_url,
        status_code: resp.status,
        content_type: resp.content_type,
        timing_ms,
    })
}

async fn acquire_page(
    ctx: &RuntimeContext,
    parsed: &url::Url,
    render_mode: RenderMode,
    wait_for_selector: Option<&str>,
) -> std::result::Result<AcquiredPage, FetchError> {
    let start = Instant::now();
    let url_str = parsed.as_str();
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?;

    let rule = ctx.rules.get(host);
    let cached = ctx.site_cache.get(host);
    let cef_available = ctx.cef.is_available();
    let effective_selector =
        wait_for_selector.or_else(|| rule.and_then(|r| r.wait_for_selector.as_deref()));

    if render_mode == RenderMode::Cef && !cef_available {
        return Err(FetchError::CefUnavailable(
            "render=cef requested but wick-renderer was not found".to_string(),
        ));
    }

    let plan = plan_strategy(render_mode, rule, cached.as_ref(), cef_available);

    let mut cef_fallback = false;
    if plan.primary == PrimaryTransport::Cef {
        match render_cef_checked(&ctx.cef, url_str, effective_selector, ctx.config.cef_timeout)
            .await
        {
            Ok(cef_res) => return classify_cef_result(ctx, host, start, cef_res, false),
            Err(err) => {
                let timing_ms = start.elapsed().as_millis() as u64;
                if matches!(err, FetchError::Timeout { .. }) {
                    ctx.site_cache.record(host, "cef_timeout", timing_ms);
                }
                if matches!(err, FetchError::CefUnavailable(_)) || !plan.allow_cronet_fallback {
                    return Err(err);
                }
                tracing::warn!(
                    "CEF render failed for {}: {}; falling back to network transport",
                    host,
                    err
                );
                cef_fallback = true;
            }
        }
    }

    let resp = ctx.client.get(url_str).await?;
    let status = resp.status;
    let final_url = resp.final_url.clone();
    let content_type = resp.content_type.clone();
    let is_html = resp.is_html();

    if status < 400 {
        if let Some(detected_mime) =
            extract::detect_binary_content(content_type.as_deref(), &resp.body)
        {
            return Err(FetchError::UnsupportedContentType {
                url: final_url,
                content_type: detected_mime.or(content_type),
                byte_count: resp.body.len(),
            });
        }
    }

    let body = resp.text();
    let mut parsed_html = None;
    let classification =
        classify_response_with_parsed(status, is_html, &body, &mut parsed_html);

    match classification {
        ResponseClassification::Ok => {
            let timing_ms = start.elapsed().as_millis() as u64;
            let strategy = ctx.client.transport_name().to_string();
            ctx.site_cache.record(host, &strategy, timing_ms);
            Ok(AcquiredPage {
                body,
                final_url,
                status_code: status,
                content_type,
                strategy,
                escalated: cef_fallback,
                parsed: parsed_html,
            })
        }
        ResponseClassification::JavaScriptShell { reason } => {
            if plan.allow_cef_escalation {
                match render_cef_checked(
                    &ctx.cef,
                    &final_url,
                    effective_selector,
                    ctx.config.cef_timeout,
                )
                .await
                {
                    Ok(cef_res) => return classify_cef_result(ctx, host, start, cef_res, true),
                    Err(err) if matches!(err, FetchError::CefUnavailable(_)) => return Err(err),
                    Err(err) => {
                        tracing::warn!(
                            "JS shell detected ({}) on {} and CEF escalation failed: {}",
                            reason,
                            host,
                            err
                        );
                    }
                }
            }
            let timing_ms = start.elapsed().as_millis() as u64;
            let strategy = ctx.client.transport_name().to_string();
            ctx.site_cache.record(host, &strategy, timing_ms);
            Ok(AcquiredPage {
                body,
                final_url,
                status_code: status,
                content_type,
                strategy,
                escalated: cef_fallback,
                parsed: parsed_html,
            })
        }
        ResponseClassification::Blocked { status } => {
            if plan.allow_cef_escalation {
                match render_cef_checked(
                    &ctx.cef,
                    &final_url,
                    effective_selector,
                    ctx.config.cef_timeout,
                )
                .await
                {
                    Ok(cef_res) => return classify_cef_result(ctx, host, start, cef_res, true),
                    Err(err) if matches!(err, FetchError::CefUnavailable(_)) => return Err(err),
                    Err(err) => {
                        return Err(FetchError::Blocked {
                            url: final_url,
                            status,
                            detail: format!("network blocked and CEF escalation failed: {}", err),
                        });
                    }
                }
            }
            let detail = if !cef_available && render_mode != RenderMode::Cronet {
                "site blocked the request and CEF renderer is not installed".to_string()
            } else {
                "site blocked the request".to_string()
            };
            Err(FetchError::Blocked {
                url: final_url,
                status,
                detail,
            })
        }
        ResponseClassification::Challenge { status, reason } => {
            if plan.allow_cef_escalation {
                match render_cef_checked(
                    &ctx.cef,
                    &final_url,
                    effective_selector,
                    ctx.config.cef_timeout,
                )
                .await
                {
                    Ok(cef_res) => return classify_cef_result(ctx, host, start, cef_res, true),
                    Err(err) if matches!(err, FetchError::CefUnavailable(_)) => return Err(err),
                    Err(err) => {
                        return Err(FetchError::Challenge {
                            url: final_url,
                            status,
                            reason: format!("{} (CEF escalation failed: {})", reason, err),
                        });
                    }
                }
            }
            Err(FetchError::Challenge {
                url: final_url,
                status,
                reason,
            })
        }
        ResponseClassification::HttpError { status } => Err(FetchError::HttpError {
            url: final_url,
            status,
        }),
    }
}

fn classify_cef_result(
    ctx: &RuntimeContext,
    host: &str,
    start: Instant,
    res: CefRenderResult,
    escalated: bool,
) -> std::result::Result<AcquiredPage, FetchError> {
    if res.status_code == 0 {
        return Err(FetchError::Transport(anyhow::anyhow!(
            "CEF render did not capture an HTTP status code for {}",
            res.final_url
        )));
    }

    let mut parsed_html = None;
    match classify_response_with_parsed(res.status_code, true, &res.html, &mut parsed_html) {
        ResponseClassification::Ok | ResponseClassification::JavaScriptShell { .. } => {
            let timing_ms = start.elapsed().as_millis() as u64;
            ctx.site_cache.record(host, "cef", timing_ms);
            Ok(AcquiredPage {
                body: res.html,
                final_url: res.final_url,
                status_code: res.status_code,
                content_type: Some("text/html".to_string()),
                strategy: "cef".to_string(),
                escalated,
                parsed: parsed_html,
            })
        }
        ResponseClassification::Blocked { status } => Err(FetchError::Blocked {
            url: res.final_url,
            status,
            detail: "site blocked the request after CEF render".to_string(),
        }),
        ResponseClassification::Challenge { status, reason } => Err(FetchError::Challenge {
            url: res.final_url,
            status,
            reason,
        }),
        ResponseClassification::HttpError { status } => Err(FetchError::HttpError {
            url: res.final_url,
            status,
        }),
    }
}

async fn render_cef_checked(
    cef: &CefRenderer,
    url: &str,
    wait_for_selector: Option<&str>,
    timeout: Duration,
) -> std::result::Result<CefRenderResult, FetchError> {
    let deadline = Instant::now() + timeout;
    let opts = CefRenderOptions {
        wait_for_selector: wait_for_selector.map(|s| s.to_string()),
    };

    let first_attempt = cef.render(url, &opts, timeout).await;
    let result = match first_attempt {
        Ok(first) if is_acceptable_cef_render(&first) => first,
        Ok(first) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining > Duration::from_secs(2) {
                cef.render(url, &opts, remaining)
                    .await
                    .map_err(|err| map_cef_error(url, err))?
            } else {
                first
            }
        }
        Err(err) => {
            if err.is::<CefProtocolError>() || err.is::<CefTimeoutError>() {
                return Err(map_cef_error(url, err));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining > Duration::from_secs(2) {
                cef.render(url, &opts, remaining)
                    .await
                    .map_err(|retry_err| map_cef_error(url, retry_err))?
            } else {
                return Err(map_cef_error(url, err));
            }
        }
    };

    if let Some(reason) = detect_interstitial_challenge(&result.html) {
        let status = if result.status_code != 0 {
            result.status_code
        } else {
            403
        };
        return Err(FetchError::Challenge {
            url: result.final_url,
            status,
            reason,
        });
    }

    Ok(result)
}

fn map_cef_error(url: &str, err: anyhow::Error) -> FetchError {
    if err.is::<CefProtocolError>() {
        FetchError::CefUnavailable(err.to_string())
    } else if err.is::<CefTimeoutError>() {
        FetchError::Timeout {
            url: url.to_string(),
            detail: err.to_string(),
        }
    } else {
        FetchError::Transport(err)
    }
}

fn is_acceptable_cef_render(res: &CefRenderResult) -> bool {
    !res.html.trim().is_empty()
        && res.status_code != 0
        && detect_interstitial_challenge(&res.html).is_none()
}

fn parse_http_url(url: &str) -> std::result::Result<url::Url, FetchError> {
    let parsed = url::Url::parse(url).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(FetchError::InvalidUrl(format!(
            "unsupported scheme {:?} (only http and https)",
            other
        ))),
    }
}

fn resolve_target_url(ctx: &RuntimeContext, parsed: &url::Url, apply_rewrites: bool) -> url::Url {
    if !apply_rewrites {
        return parsed.clone();
    }
    ctx.rules
        .rewrite_url(parsed)
        .unwrap_or_else(|| parsed.clone())
}
