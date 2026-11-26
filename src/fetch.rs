use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::context::RuntimeContext;
use crate::extract::{self, ExtractOptions, OutputFormat, ParsedHtml};
use crate::policy::{
    classify_response_with_context, detect_interstitial_challenge, plan_strategy, PrimaryTransport,
    RenderMode, ResponseClassification,
};
use crate::transport::cef::{
    CefProtocolError, CefRenderOptions, CefRenderResult, CefRenderer, CefTimeoutError,
};
use crate::transport::{
    ResponseTooLargeError, TransportError, TransportErrorKind, DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES,
    DEFAULT_RAW_MAX_RESPONSE_BYTES,
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

    #[error("response body from {url} exceeded {limit_bytes} byte limit")]
    ResponseTooLarge { url: String, limit_bytes: usize },

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
            | Self::ResponseTooLarge { .. }
            | Self::Transport(_) => 1,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::RobotsDisallowed { .. } => "robots_disallowed",
            Self::Blocked { .. } => "blocked",
            Self::Challenge { .. } => "challenge",
            Self::HttpError { .. } => "http_error",
            Self::UnsupportedContentType { .. } => "unsupported_content_type",
            Self::CefUnavailable(_) => "cef_unavailable",
            Self::Timeout { .. } => "timeout",
            Self::ResponseTooLarge { .. } => "response_too_large",
            Self::Transport(_) => "transport",
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
    pub content_bytes: usize,
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

    let (effective, _) =
        resolve_and_check_robots(ctx, url, options.apply_rewrites, options.respect_robots).await?;

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

    let content_bytes = extracted.content.len();
    Ok(FetchOutput {
        url: url.to_string(),
        final_url: page.final_url,
        status_code: page.status_code,
        content_type: page.content_type,
        title: extracted.title,
        content: extracted.content,
        content_bytes,
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
    let (effective, _) = resolve_and_check_robots(ctx, url, true, respect_robots).await?;
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
    let (parsed, host) = resolve_and_check_robots(ctx, url, false, respect_robots).await?;

    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_RAW_MAX_RESPONSE_BYTES);
    let resp = ctx
        .client
        .get_with_limits(parsed.as_str(), ctx.config.timeout, max_bytes)
        .await
        .map_err(|e| map_transport_error(parsed.as_str(), e, max_bytes))?;
    let timing_ms = start.elapsed().as_millis() as u64;

    if resp.status >= 400 {
        return Err(FetchError::HttpError {
            url: resp.final_url,
            status: resp.status,
        });
    }

    ctx.site_cache
        .record(&host, ctx.client.transport_name(), timing_ms);

    Ok(FetchRawResult {
        bytes: resp.body,
        url: url.to_string(),
        final_url: resp.final_url,
        status_code: resp.status,
        content_type: resp.content_type,
        timing_ms,
    })
}

async fn resolve_and_check_robots(
    ctx: &RuntimeContext,
    url: &str,
    apply_rewrites: bool,
    respect_robots: bool,
) -> std::result::Result<(url::Url, String), FetchError> {
    let parsed = parse_http_url(url)?;
    let effective = resolve_target_url(ctx, &parsed, apply_rewrites);
    let host = effective
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?
        .to_string();

    if respect_robots && !ctx.robots.is_allowed(&ctx.client, effective.as_str()).await {
        return Err(FetchError::RobotsDisallowed {
            host,
            url: effective.to_string(),
        });
    }

    Ok((effective, host))
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
        match render_cef_checked(
            &ctx.cef,
            url_str,
            effective_selector,
            ctx.config.cef_timeout,
        )
        .await
        {
            Ok(cef_res) => return classify_cef_result(ctx, host, url_str, start, cef_res, false),
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

    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES);
    let resp = ctx
        .client
        .get_with_limits(url_str, ctx.config.timeout, max_bytes)
        .await
        .map_err(|e| map_transport_error(url_str, e, max_bytes))?;
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
    let classification = classify_response_with_context(
        status,
        is_html,
        Some(url_str),
        Some(&final_url),
        &body,
        &mut parsed_html,
    );

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
                match try_cef_escalation(ctx, host, start, &final_url, effective_selector).await {
                    CefEscalation::Completed(res) => return res,
                    CefEscalation::Unavailable(err) => return Err(err),
                    CefEscalation::RenderFailed(err) => {
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
                match try_cef_escalation(ctx, host, start, &final_url, effective_selector).await {
                    CefEscalation::Completed(res) => return res,
                    CefEscalation::Unavailable(err) => return Err(err),
                    CefEscalation::RenderFailed(err) => {
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
                match try_cef_escalation(ctx, host, start, &final_url, effective_selector).await {
                    CefEscalation::Completed(res) => return res,
                    CefEscalation::Unavailable(err) => return Err(err),
                    CefEscalation::RenderFailed(err) => {
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

enum CefEscalation {
    Completed(std::result::Result<AcquiredPage, FetchError>),
    Unavailable(FetchError),
    RenderFailed(FetchError),
}

async fn try_cef_escalation(
    ctx: &RuntimeContext,
    host: &str,
    start: Instant,
    url: &str,
    wait_for_selector: Option<&str>,
) -> CefEscalation {
    match render_cef_checked(&ctx.cef, url, wait_for_selector, ctx.config.cef_timeout).await {
        Ok(cef_res) => {
            CefEscalation::Completed(classify_cef_result(ctx, host, url, start, cef_res, true))
        }
        Err(err @ FetchError::CefUnavailable(_)) => CefEscalation::Unavailable(err),
        Err(err) => CefEscalation::RenderFailed(err),
    }
}

fn classify_cef_result(
    ctx: &RuntimeContext,
    host: &str,
    requested_url: &str,
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
    match classify_response_with_context(
        res.status_code,
        true,
        Some(requested_url),
        Some(&res.final_url),
        &res.html,
        &mut parsed_html,
    ) {
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
        Ok(first) => {
            if let Some(reason) = detect_interstitial_challenge(&first.html) {
                return Err(challenge_error_from_cef(first, reason));
            }
            if is_acceptable_cef_render(&first) {
                first
            } else {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining > Duration::from_secs(2) {
                    cef.render(url, &opts, remaining)
                        .await
                        .map_err(|err| map_cef_error(url, err))?
                } else {
                    first
                }
            }
        }
        Err(err) => {
            if is_non_retryable_cef_error(&err) {
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
        return Err(challenge_error_from_cef(result, reason));
    }

    Ok(result)
}

fn challenge_error_from_cef(result: CefRenderResult, reason: String) -> FetchError {
    let status = if result.status_code != 0 {
        result.status_code
    } else {
        403
    };
    FetchError::Challenge {
        url: result.final_url,
        status,
        reason,
    }
}

fn is_non_retryable_cef_error(err: &anyhow::Error) -> bool {
    if err.is::<CefProtocolError>() || err.is::<CefTimeoutError>() {
        return true;
    }
    err.downcast_ref::<TransportError>().is_some_and(|te| {
        matches!(
            te.kind,
            TransportErrorKind::Timeout
                | TransportErrorKind::ProtocolMismatch
                | TransportErrorKind::InvalidSelector
                | TransportErrorKind::Canceled
                | TransportErrorKind::ResponseTooLarge
        )
    })
}

fn map_transport_error(url: &str, err: anyhow::Error, max_response_bytes: usize) -> FetchError {
    if let Some(too_large) = err.downcast_ref::<ResponseTooLargeError>() {
        return FetchError::ResponseTooLarge {
            url: url.to_string(),
            limit_bytes: too_large.limit_bytes,
        };
    }
    if let Some(te) = err.downcast_ref::<TransportError>() {
        return match te.kind {
            TransportErrorKind::Timeout => FetchError::Timeout {
                url: url.to_string(),
                detail: te.message.clone(),
            },
            TransportErrorKind::ResponseTooLarge => FetchError::ResponseTooLarge {
                url: url.to_string(),
                limit_bytes: max_response_bytes,
            },
            TransportErrorKind::ProtocolMismatch => FetchError::CefUnavailable(te.message.clone()),
            TransportErrorKind::LoadError
            | TransportErrorKind::InvalidSelector
            | TransportErrorKind::Canceled
            | TransportErrorKind::Network => FetchError::Transport(err),
        };
    }
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

fn map_cef_error(url: &str, err: anyhow::Error) -> FetchError {
    map_transport_error(url, err, DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES)
}

fn is_acceptable_cef_render(res: &CefRenderResult) -> bool {
    !res.html.trim().is_empty() && res.status_code != 0
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

#[derive(Debug, Clone, Serialize)]
pub struct BatchItemSuccess {
    pub url: String,
    pub ok: bool,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub title: Option<String>,
    pub strategy: String,
    pub escalated: bool,
    pub truncated: bool,
    pub timing_ms: u64,
    pub content_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchItemFailure {
    pub url: String,
    pub ok: bool,
    pub error: String,
    pub kind: String,
    pub exit_code: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum BatchItemResult {
    Success(BatchItemSuccess),
    Failure(BatchItemFailure),
}

impl BatchItemResult {
    pub fn from_result(
        url: &str,
        res: std::result::Result<FetchOutput, FetchError>,
        metadata_only: bool,
    ) -> Self {
        match res {
            Ok(out) => Self::Success(BatchItemSuccess {
                url: out.url,
                ok: true,
                final_url: out.final_url,
                status_code: out.status_code,
                content_type: out.content_type,
                title: out.title,
                strategy: out.strategy,
                escalated: out.escalated,
                truncated: out.truncated,
                timing_ms: out.timing_ms,
                content_bytes: out.content_bytes,
                content: (!metadata_only).then_some(out.content),
            }),
            Err(err) => Self::Failure(BatchItemFailure {
                url: url.to_string(),
                ok: false,
                error: err.to_string(),
                kind: err.kind().to_string(),
                exit_code: err.exit_code().clamp(1, 255) as u8,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BatchFetchOptions {
    pub fetch: FetchOptions,
    pub concurrency: usize,
    pub metadata_only: bool,
}

pub fn parse_batch_stdin(input: &str) -> Vec<String> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

pub fn batch_exit_code(results: &[BatchItemResult]) -> u8 {
    if results.is_empty() {
        return 1;
    }
    let mut successes = 0usize;
    let mut failure_codes: HashSet<u8> = HashSet::new();
    for item in results {
        match item {
            BatchItemResult::Success(_) => successes += 1,
            BatchItemResult::Failure(f) => {
                failure_codes.insert(f.exit_code);
            }
        }
    }
    if failure_codes.is_empty() {
        0
    } else if successes > 0 {
        4
    } else if failure_codes.len() == 1 {
        *failure_codes.iter().next().unwrap_or(&1)
    } else {
        1
    }
}

pub async fn fetch_batch<F>(
    ctx: Arc<RuntimeContext>,
    urls: Vec<String>,
    options: BatchFetchOptions,
    mut on_complete: F,
) -> Vec<(usize, BatchItemResult)>
where
    F: FnMut(usize, &BatchItemResult),
{
    let concurrency = options.concurrency.clamp(1, 16);
    let global_sem = Arc::new(Semaphore::new(concurrency));
    let host_sems: Arc<Mutex<HashMap<String, Arc<Semaphore>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let fetch_opts = Arc::new(options.fetch);
    let metadata_only = options.metadata_only;

    let mut set = JoinSet::new();
    for (idx, raw_url) in urls.into_iter().enumerate() {
        let ctx_c = Arc::clone(&ctx);
        let global_c = Arc::clone(&global_sem);
        let host_sems_c = Arc::clone(&host_sems);
        let opts_c = Arc::clone(&fetch_opts);

        set.spawn(async move {
            let host_key = batch_host_key(&ctx_c, &raw_url, opts_c.apply_rewrites);
            let host_sem = {
                let mut map = host_sems_c.lock().expect("host semaphore map poisoned");
                Arc::clone(
                    map.entry(host_key)
                        .or_insert_with(|| Arc::new(Semaphore::new(2))),
                )
            };

            let _host_permit = host_sem
                .acquire_owned()
                .await
                .expect("host semaphore closed");
            let _global_permit = global_c
                .acquire_owned()
                .await
                .expect("global semaphore closed");

            let res = fetch_document(&ctx_c, &raw_url, &opts_c).await;
            let item = BatchItemResult::from_result(&raw_url, res, metadata_only);
            (idx, item)
        });
    }

    let mut collected = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((idx, item)) = joined {
            on_complete(idx, &item);
            collected.push((idx, item));
        }
    }
    collected
}

fn batch_host_key(ctx: &RuntimeContext, raw_url: &str, apply_rewrites: bool) -> String {
    let Ok(parsed) = parse_http_url(raw_url) else {
        return "__invalid__".to_string();
    };
    let effective = resolve_target_url(ctx, &parsed, apply_rewrites);
    effective
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "__invalid__".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_error_kind_maps_directly_to_fetch_error() {
        let timeout = map_transport_error(
            "https://example.com",
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                "timed out after 5s",
            )),
            1024,
        );
        assert!(matches!(timeout, FetchError::Timeout { .. }));
        assert_eq!(timeout.kind(), "timeout");

        let too_large = map_transport_error(
            "https://example.com",
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ResponseTooLarge,
                "limit exceeded",
            )),
            2048,
        );
        assert!(matches!(
            too_large,
            FetchError::ResponseTooLarge {
                limit_bytes: 2048,
                ..
            }
        ));

        let mismatch = map_transport_error(
            "https://example.com",
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                "expected v5-meta",
            )),
            1024,
        );
        assert!(matches!(mismatch, FetchError::CefUnavailable(_)));

        let invalid_selector_err = anyhow::Error::new(TransportError::new(
            TransportErrorKind::InvalidSelector,
            "bad css selector",
        ));
        assert!(is_non_retryable_cef_error(&invalid_selector_err));
        let mapped = map_transport_error("https://example.com", invalid_selector_err, 1024);
        assert!(matches!(mapped, FetchError::Transport(_)));
    }
}
