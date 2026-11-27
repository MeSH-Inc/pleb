use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::artifacts::stage_artifact;
use crate::context::RuntimeContext;
use crate::extract::{
    self, ExtractMode, ExtractOptions, OutputFormat, ParsedHtml, ResourceDescriptor, ResourceKind,
    ResourceSalience,
};
use crate::policy::{
    classify_response_with_context, compute_safe_referer, detect_interstitial_challenge,
    is_markdown_content_type, is_mirror_acceptable_content_type, is_safe_subresource_url,
    parse_markdown_link_header, plan_strategy, resolve_raw_source_candidate, EscalationBackend,
    PrimaryTransport, RawSourceCandidate, RawSourceKind, RenderMode, ResourcePolicy,
    ResponseClassification, SourceMode, ToolPolicy,
};
use crate::transport::cdp::{CdpRenderOptions, CdpRenderResult, CdpRenderer};
use crate::transport::cef::{CefRenderOptions, CefRenderResult, CefRenderer};
use crate::transport::web_bot_auth::execute_signed_get;
use crate::transport::{
    transport_error_kind, RequestHeaderOptions, ResponseTooLargeError, SubresourceDest,
    TransportErrorKind, DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES, DEFAULT_RAW_MAX_RESPONSE_BYTES,
};

const MAX_TRANSCRIPT_FETCHES: usize = 4;
const MAX_STAGED_ARTIFACTS: usize = 8;
const MAX_SUBRESOURCE_BYTES: usize = 16 * 1024 * 1024;

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
        "unsupported binary content ({}) on {url} ({byte_count} bytes); use --stage or --format raw --output <path>",
        content_type.as_deref().unwrap_or("unknown")
    )]
    UnsupportedContentType {
        url: String,
        content_type: Option<String>,
        byte_count: usize,
    },

    #[error("CEF renderer unavailable: {0}")]
    CefUnavailable(String),

    #[error("CDP renderer unavailable: {0}")]
    CdpUnavailable(String),

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
            | Self::CdpUnavailable(_)
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
            Self::CdpUnavailable(_) => "cdp_unavailable",
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
    pub source: SourceMode,
    pub resources: ResourcePolicy,
    pub tools: ToolPolicy,
    pub stage_resources: Vec<String>,
    pub referer: Option<String>,
    pub wait_for_selector: Option<String>,
    pub respect_robots: bool,
    pub apply_rewrites: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            extract: ExtractOptions::default(),
            render: RenderMode::Auto,
            source: SourceMode::Auto,
            resources: ResourcePolicy::None,
            tools: ToolPolicy::None,
            stage_resources: Vec::new(),
            referer: None,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<crate::extract::ResourceDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::extract::WebMcpToolDescriptor>,
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

#[derive(Debug, Clone, Serialize)]
pub struct StagedAssetOutput {
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub byte_count: usize,
    pub staged_path: String,
    pub timing_ms: u64,
}

struct AcquiredPage {
    body: String,
    final_url: String,
    extract_base_url: Option<url::Url>,
    status_code: u16,
    content_type: Option<String>,
    strategy: String,
    escalated: bool,
    warning: Option<String>,
    parsed: Option<ParsedHtml>,
    axtree_text: Option<String>,
    imperative_tools: Vec<crate::extract::WebMcpToolDescriptor>,
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

    let is_axtree = options.extract.format == OutputFormat::Axtree;
    if is_axtree {
        if matches!(options.render, RenderMode::Cronet | RenderMode::Cef) {
            return Err(FetchError::CdpUnavailable(
                "--format axtree requires CDP rendering (cannot be used with --render cronet or --render cef)"
                    .to_string(),
            ));
        }
        if ctx.config.proxy.is_some() {
            return Err(FetchError::CdpUnavailable(
                "CDP renderer cannot be used with --proxy because WebSocket CDP connections would bypass the proxy"
                    .to_string(),
            ));
        }
        if ctx.configured_cdp_url().is_none() {
            return Err(FetchError::CdpUnavailable(
                "--format axtree requires --cdp-url or PLEB_CDP_URL".to_string(),
            ));
        }
    }

    let effective_render = if is_axtree && options.render == RenderMode::Auto {
        RenderMode::Cdp
    } else {
        options.render
    };

    let (effective, _) =
        resolve_and_check_robots(ctx, url, options.apply_rewrites, options.respect_robots).await?;

    let effective_source_mode = if options.source == SourceMode::None
        || matches!(
            options.extract.format,
            OutputFormat::Html | OutputFormat::Raw | OutputFormat::Axtree
        )
        || matches!(effective_render, RenderMode::Cef | RenderMode::Cdp)
    {
        SourceMode::None
    } else {
        options.source
    };

    if options.apply_rewrites && effective_source_mode != SourceMode::None {
        if let Some(candidate) = resolve_raw_source_candidate(&effective) {
            if let Some(output) =
                try_fetch_raw_source(ctx, url, &effective, &candidate, options, start).await?
            {
                return Ok(output);
            }
        }
    }

    let page = acquire_page(
        ctx,
        &effective,
        effective_render,
        effective_source_mode,
        options.wait_for_selector.as_deref(),
        options.respect_robots,
        is_axtree,
    )
    .await?;

    let final_url_parsed = url::Url::parse(&page.final_url).unwrap_or_else(|_| effective.clone());
    let extract_url = page.extract_base_url.as_ref().unwrap_or(&final_url_parsed);
    let needs_enrichment = options.resources != ResourcePolicy::None
        || !options.stage_resources.is_empty()
        || options.tools == ToolPolicy::Manifest;
    let initial_extract_opts = if is_axtree {
        ExtractOptions {
            format: OutputFormat::Html,
            max_bytes: None,
            ..options.extract.clone()
        }
    } else if needs_enrichment && options.extract.max_bytes.is_some() {
        ExtractOptions {
            max_bytes: None,
            ..options.extract.clone()
        }
    } else {
        options.extract.clone()
    };
    let mut extracted = extract::extract_document_parsed(
        &page.body,
        page.parsed,
        extract_url,
        page.content_type.as_deref(),
        &initial_extract_opts,
    )?;

    if is_axtree {
        extracted.content = page.axtree_text.ok_or_else(|| {
            FetchError::CdpUnavailable("CDP render did not capture accessibility tree".to_string())
        })?;
        extracted.truncated = false;
    }

    if !page.imperative_tools.is_empty() {
        let imperative: Vec<_> = page
            .imperative_tools
            .into_iter()
            .filter(|t| t.source == extract::WebMcpToolSource::Imperative)
            .collect();
        extracted.tools = extract::webmcp::merge_webmcp_tools(extracted.tools, imperative);
    }

    if needs_enrichment {
        enrich_document_resources(
            ctx,
            &final_url_parsed,
            &mut extracted.content,
            &mut extracted.resources,
            options,
        )
        .await?;
        if options.tools == ToolPolicy::Manifest && !extracted.tools.is_empty() {
            let manifest = extract::webmcp::format_webmcp_manifest_markdown(&extracted.tools);
            if !manifest.trim().is_empty() {
                if !extracted.content.trim().is_empty() {
                    extracted.content.push_str("\n\n");
                }
                extracted.content.push_str(manifest.trim());
            }
        }
    }

    if (needs_enrichment || is_axtree) && options.extract.max_bytes.is_some() {
        let (truncated_content, did_truncate) = extract::apply_format_limit(
            &extracted.content,
            options.extract.format,
            options.extract.max_bytes,
        );
        extracted.content = truncated_content;
        extracted.truncated |= did_truncate;
    }

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
        warning: page.warning,
        resources: extracted.resources,
        tools: extracted.tools,
    })
}

pub async fn fetch_html(
    ctx: &RuntimeContext,
    url: &str,
    respect_robots: bool,
) -> std::result::Result<FetchHtmlResult, FetchError> {
    let (effective, _) = resolve_and_check_robots(ctx, url, true, respect_robots).await?;
    let page = acquire_page(
        ctx,
        &effective,
        RenderMode::Auto,
        SourceMode::None,
        None,
        respect_robots,
        false,
    )
    .await?;
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
    let signer = ctx
        .web_bot_auth_signer_for_url(&parsed)
        .map_err(FetchError::Transport)?;
    let req_opts = RequestHeaderOptions::default();
    let resp = execute_signed_get(
        &ctx.client,
        parsed.as_str(),
        ctx.config.timeout,
        max_bytes,
        &req_opts,
        signer.as_deref(),
    )
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

pub async fn fetch_staged_asset(
    ctx: &RuntimeContext,
    url: &str,
    referer: Option<&str>,
    respect_robots: bool,
) -> std::result::Result<StagedAssetOutput, FetchError> {
    let start = Instant::now();
    let (parsed, host) = resolve_and_check_robots(ctx, url, false, respect_robots).await?;

    let safe_referer = match referer {
        Some(raw) if !raw.trim().is_empty() => {
            let parent = parse_http_url(raw.trim())?;
            compute_safe_referer(&parent, &parsed)
        }
        _ => None,
    };
    let req_opts = RequestHeaderOptions {
        accept_markdown: false,
        referer: safe_referer,
        subresource_dest: Some(infer_subresource_dest(&parsed)),
        ..Default::default()
    };

    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_RAW_MAX_RESPONSE_BYTES);
    let signer = ctx
        .web_bot_auth_signer_for_url(&parsed)
        .map_err(FetchError::Transport)?;
    let resp = execute_signed_get(
        &ctx.client,
        parsed.as_str(),
        ctx.config.timeout,
        max_bytes,
        &req_opts,
        signer.as_deref(),
    )
    .await
    .map_err(|e| map_transport_error(parsed.as_str(), e, max_bytes))?;
    let timing_ms = start.elapsed().as_millis() as u64;

    if resp.status >= 400 {
        return Err(FetchError::HttpError {
            url: resp.final_url,
            status: resp.status,
        });
    }

    let final_url_parsed = url::Url::parse(&resp.final_url).unwrap_or(parsed);
    let staged = stage_artifact(
        &ctx.config.state_dir,
        &final_url_parsed,
        resp.content_type.as_deref(),
        &resp.body,
        None,
    )
    .map_err(FetchError::Transport)?;

    ctx.site_cache
        .record(&host, ctx.client.transport_name(), timing_ms);

    Ok(StagedAssetOutput {
        url: url.to_string(),
        final_url: resp.final_url,
        status_code: resp.status,
        content_type: staged.content_type,
        byte_count: staged.byte_count,
        staged_path: staged.path.display().to_string(),
        timing_ms,
    })
}

pub fn format_resource_cue(resources: &[ResourceDescriptor]) -> Option<String> {
    let kind_order = [
        (ResourceKind::Diagram, "diagram", "diagrams"),
        (ResourceKind::Transcript, "transcript", "transcripts"),
        (ResourceKind::Document, "document", "documents"),
        (ResourceKind::Dataset, "dataset", "datasets"),
        (ResourceKind::Schema, "schema", "schemas"),
        (ResourceKind::Image, "image", "images"),
    ];

    let mut parts = Vec::new();
    for (kind, singular, plural) in kind_order {
        let ids: Vec<&str> = resources
            .iter()
            .filter(|r| r.salience == ResourceSalience::High && r.kind == kind)
            .map(|r| r.identifier.as_str())
            .collect();
        if !ids.is_empty() {
            let word = if ids.len() == 1 { singular } else { plural };
            parts.push(format!("{} {} ({})", ids.len(), word, ids.join(", ")));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(format!(
            "Resources: {} — pass --resources manifest or --stage-resources <ids>",
            parts.join(", ")
        ))
    }
}

fn infer_subresource_dest(url: &url::Url) -> SubresourceDest {
    let lower = url.path().to_ascii_lowercase();
    if [
        ".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg", ".avif", ".bmp", ".ico",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
    {
        SubresourceDest::Image
    } else {
        SubresourceDest::Asset
    }
}

fn record_staged_artifact(
    res: &mut ResourceDescriptor,
    staged: crate::artifacts::StagedArtifact,
    staged_lines: &mut Vec<String>,
) -> String {
    let staged_path = staged.path.display().to_string();
    res.staged_path = Some(staged_path.clone());
    res.byte_count = Some(staged.byte_count);
    res.content_type = staged.content_type;
    staged_lines.push(format!(
        "- **{}** ({}, {} bytes): `{}`",
        res.identifier,
        res.kind.as_str(),
        staged.byte_count,
        staged_path
    ));
    staged_path
}

async fn join_all_futures<T, F>(futs: Vec<F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut results: Vec<Option<T>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, fut) in pinned.iter_mut().enumerate() {
            if results[i].is_none() {
                match fut.as_mut().poll(cx) {
                    std::task::Poll::Ready(val) => results[i] = Some(val),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    results.into_iter().flatten().collect()
}

fn host_semaphore(map: &Mutex<HashMap<String, Arc<Semaphore>>>, host: String) -> Arc<Semaphore> {
    let mut guard = map.lock().expect("host semaphore map poisoned");
    Arc::clone(
        guard
            .entry(host)
            .or_insert_with(|| Arc::new(Semaphore::new(2))),
    )
}

struct SelectedSubresource {
    index: usize,
    url: url::Url,
    kind: ResourceKind,
    explicit: bool,
}

async fn enrich_document_resources(
    ctx: &RuntimeContext,
    final_url: &url::Url,
    content: &mut String,
    resources: &mut [ResourceDescriptor],
    options: &FetchOptions,
) -> std::result::Result<(), FetchError> {
    if options.resources == ResourcePolicy::Manifest && !resources.is_empty() {
        let manifest = extract::format_resource_manifest_markdown(resources);
        if !manifest.trim().is_empty() {
            content.push_str("\n\n");
            content.push_str(manifest.trim());
        }
    }

    let explicit_ids: HashSet<&str> = options
        .stage_resources
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if matches!(
        options.resources,
        ResourcePolicy::None | ResourcePolicy::Manifest
    ) && explicit_ids.is_empty()
    {
        return Ok(());
    }

    let mut selected = Vec::new();
    let mut transcript_count = 0usize;
    let mut staged_count = 0usize;

    for (index, res) in resources.iter().enumerate() {
        let explicit = explicit_ids.contains(res.identifier.as_str());
        let policy_match = match options.resources {
            ResourcePolicy::Transcripts => res.kind == ResourceKind::Transcript,
            ResourcePolicy::Diagrams => {
                res.kind == ResourceKind::Diagram && res.salience == ResourceSalience::High
            }
            ResourcePolicy::All => {
                res.kind == ResourceKind::Transcript
                    || (res.salience == ResourceSalience::High
                        && matches!(
                            res.kind,
                            ResourceKind::Diagram
                                | ResourceKind::Document
                                | ResourceKind::Dataset
                                | ResourceKind::Schema
                        ))
            }
            ResourcePolicy::None | ResourcePolicy::Manifest => false,
        };

        if !(explicit || policy_match) {
            continue;
        }
        let Ok(res_url) = url::Url::parse(&res.url) else {
            continue;
        };
        if !is_safe_subresource_url(final_url, &res_url) {
            continue;
        }

        if res.kind == ResourceKind::Transcript {
            if transcript_count >= MAX_TRANSCRIPT_FETCHES {
                continue;
            }
            if explicit && staged_count >= MAX_STAGED_ARTIFACTS {
                continue;
            }
            transcript_count += 1;
            if explicit {
                staged_count += 1;
            }
        } else {
            if staged_count >= MAX_STAGED_ARTIFACTS {
                continue;
            }
            staged_count += 1;
        }

        selected.push(SelectedSubresource {
            index,
            url: res_url,
            kind: res.kind,
            explicit,
        });
    }

    if selected.is_empty() {
        return Ok(());
    }

    let mut subresource_signer: Option<Arc<crate::transport::web_bot_auth::WebBotAuthSigner>> =
        None;
    for item in &selected {
        if let Some(signer) = ctx
            .web_bot_auth_signer_for_url(&item.url)
            .map_err(FetchError::Transport)?
        {
            subresource_signer = Some(signer);
            break;
        }
    }

    let max_sub_bytes = ctx
        .config
        .max_response_bytes
        .map(|limit| limit.min(MAX_SUBRESOURCE_BYTES))
        .unwrap_or(MAX_SUBRESOURCE_BYTES);
    let host_sems: Mutex<HashMap<String, Arc<Semaphore>>> = Mutex::new(HashMap::new());
    let respect_robots = options.respect_robots;

    let futs = selected
        .into_iter()
        .map(|item| {
            let host_key = item
                .url
                .host_str()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            let sem = host_semaphore(&host_sems, host_key);
            let signer = subresource_signer.clone();
            async move {
                let _permit = sem.acquire_owned().await.ok()?;
                if respect_robots && !ctx.robots.is_allowed(&ctx.client, item.url.as_str()).await {
                    return None;
                }
                let req_opts = RequestHeaderOptions {
                    accept_markdown: false,
                    referer: compute_safe_referer(final_url, &item.url),
                    subresource_dest: Some(match item.kind {
                        ResourceKind::Diagram | ResourceKind::Image => SubresourceDest::Image,
                        _ => SubresourceDest::Asset,
                    }),
                    ..Default::default()
                };
                match execute_signed_get(
                    &ctx.client,
                    item.url.as_str(),
                    ctx.config.timeout,
                    max_sub_bytes,
                    &req_opts,
                    signer.as_deref(),
                )
                .await
                {
                    Ok(resp) if resp.status < 400 => Some((item, resp)),
                    Ok(resp) => {
                        tracing::debug!(
                            "Skipping subresource {} with HTTP {}",
                            item.url,
                            resp.status
                        );
                        None
                    }
                    Err(err) => {
                        tracing::debug!("Skipping subresource {} on error: {}", item.url, err);
                        None
                    }
                }
            }
        })
        .collect();

    let fetched = join_all_futures(futs).await;
    let mut staged_lines: Vec<String> = Vec::new();
    let transcript_opts = ExtractOptions {
        format: if options.extract.format == OutputFormat::Text {
            OutputFormat::Text
        } else {
            OutputFormat::Markdown
        },
        mode: ExtractMode::Clean,
        include_images: false,
        include_links: false,
        max_bytes: None,
    };

    for entry in fetched.into_iter().flatten() {
        let (item, resp) = entry;
        let res_final_url = url::Url::parse(&resp.final_url).unwrap_or_else(|_| item.url.clone());

        if item.kind == ResourceKind::Transcript {
            let ct = match resp.content_type.as_deref() {
                Some(raw)
                    if raw.to_ascii_lowercase().contains("vtt")
                        || raw.to_ascii_lowercase().contains("subrip")
                        || raw.to_ascii_lowercase().contains("srt") =>
                {
                    Some(raw)
                }
                _ => Some("text/vtt"),
            };
            if let Ok(doc) =
                extract::extract_document_bytes(&resp.body, &res_final_url, ct, &transcript_opts)
            {
                let text = doc.content.trim();
                if !text.is_empty() {
                    let label = resources[item.index]
                        .label
                        .as_deref()
                        .unwrap_or(&resources[item.index].identifier);
                    content.push_str(&format!("\n\n## Media Transcript: {label}\n\n{text}"));
                }
            }
            if item.explicit {
                if let Ok(staged) = stage_artifact(
                    &ctx.config.state_dir,
                    &res_final_url,
                    ct,
                    &resp.body,
                    Some(ResourceKind::Transcript),
                ) {
                    record_staged_artifact(&mut resources[item.index], staged, &mut staged_lines);
                }
            }
            continue;
        }

        match stage_artifact(
            &ctx.config.state_dir,
            &res_final_url,
            resp.content_type.as_deref(),
            &resp.body,
            Some(item.kind),
        ) {
            Ok(staged) => {
                let res = &mut resources[item.index];
                let staged_path = record_staged_artifact(res, staged, &mut staged_lines);
                if options.extract.format == OutputFormat::Markdown {
                    let needle = format!("({})", res.url);
                    if content.contains(&needle) {
                        *content = content.replace(&needle, &format!("({staged_path})"));
                    }
                }
            }
            Err(err) => {
                tracing::debug!("Skipping staging for {}: {}", item.url, err);
            }
        }
    }

    if !staged_lines.is_empty() {
        content.push_str("\n\n## Staged Resources\n\n");
        content.push_str(&staged_lines.join("\n"));
    }
    Ok(())
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

async fn try_fetch_raw_source(
    ctx: &RuntimeContext,
    original_url: &str,
    effective_url: &url::Url,
    candidate: &RawSourceCandidate,
    options: &FetchOptions,
    start: Instant,
) -> std::result::Result<Option<FetchOutput>, FetchError> {
    let raw_str = candidate.raw_url.as_str();
    if options.respect_robots && !ctx.robots.is_allowed(&ctx.client, raw_str).await {
        return Ok(None);
    }

    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES);
    let signer = ctx
        .web_bot_auth_signer_for_url(&candidate.raw_url)
        .map_err(FetchError::Transport)?;
    let req_opts = RequestHeaderOptions::default();
    let Ok(resp) = execute_signed_get(
        &ctx.client,
        raw_str,
        ctx.config.timeout,
        max_bytes,
        &req_opts,
        signer.as_deref(),
    )
    .await
    else {
        return Ok(None);
    };
    if resp.status >= 400 || resp.is_html() {
        return Ok(None);
    }
    if extract::detect_binary_content(resp.content_type.as_deref(), &resp.body).is_some()
        || extract::detect_binary_content(None, &resp.body).is_some()
    {
        return Ok(None);
    }

    let body = resp.text();
    let raw_final_url =
        url::Url::parse(&resp.final_url).unwrap_or_else(|_| candidate.raw_url.clone());
    let extracted = match candidate.kind {
        RawSourceKind::Code { language }
            if !is_markdown_content_type(resp.content_type.as_deref()) =>
        {
            extract::wrap_raw_code_document(&candidate.filename, &body, language, &options.extract)
        }
        _ => match extract::extract_document_parsed(
            &body,
            None,
            &raw_final_url,
            Some("text/markdown"),
            &options.extract,
        ) {
            Ok(doc) => doc,
            Err(_) => return Ok(None),
        },
    };

    let timing_ms = start.elapsed().as_millis() as u64;
    if let Some(host) = effective_url.host_str() {
        ctx.site_cache
            .record(host, ctx.client.transport_name(), timing_ms);
    }
    let content_bytes = extracted.content.len();
    Ok(Some(FetchOutput {
        url: original_url.to_string(),
        final_url: resp.final_url,
        status_code: resp.status,
        content_type: resp.content_type,
        title: extracted.title.or_else(|| Some(candidate.filename.clone())),
        content: extracted.content,
        content_bytes,
        strategy: "raw_source".to_string(),
        escalated: false,
        truncated: extracted.truncated,
        timing_ms,
        warning: None,
        resources: extracted.resources,
        tools: extracted.tools,
    }))
}

async fn fetch_validated_mirror(
    ctx: &RuntimeContext,
    mirror_url: url::Url,
    respect_robots: bool,
    max_bytes: usize,
    allow_plain_text: bool,
) -> std::result::Result<Option<(String, url::Url)>, FetchError> {
    if respect_robots
        && !ctx
            .robots
            .is_allowed(&ctx.client, mirror_url.as_str())
            .await
    {
        return Ok(None);
    }
    let signer = ctx
        .web_bot_auth_signer_for_url(&mirror_url)
        .map_err(FetchError::Transport)?;
    let req_opts = RequestHeaderOptions {
        accept_markdown: true,
        ..Default::default()
    };
    let Ok(resp) = execute_signed_get(
        &ctx.client,
        mirror_url.as_str(),
        ctx.config.timeout,
        max_bytes,
        &req_opts,
        signer.as_deref(),
    )
    .await
    else {
        return Ok(None);
    };
    let mime_ok = if allow_plain_text {
        !resp.is_html() && is_mirror_acceptable_content_type(resp.content_type.as_deref())
    } else {
        is_markdown_content_type(resp.content_type.as_deref())
    };
    if resp.status != 200 || !mime_ok || extract::detect_binary_content(None, &resp.body).is_some()
    {
        return Ok(None);
    }
    let text = resp.text();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let base = url::Url::parse(&resp.final_url).unwrap_or(mirror_url);
    Ok(Some((text, base)))
}

#[allow(clippy::too_many_arguments)]
async fn try_fetch_markdown_mirror(
    ctx: &RuntimeContext,
    requested: &url::Url,
    final_url: &str,
    link_header: Option<&str>,
    html_body: &str,
    parsed_html: &mut Option<ParsedHtml>,
    source_mode: SourceMode,
    respect_robots: bool,
    max_bytes: usize,
) -> std::result::Result<Option<(String, url::Url)>, FetchError> {
    let final_url_parsed = url::Url::parse(final_url).unwrap_or_else(|_| requested.clone());

    let advertised = link_header
        .and_then(|h| parse_markdown_link_header(h, &final_url_parsed))
        .or_else(|| {
            let doc = parsed_html.get_or_insert_with(|| ParsedHtml::parse(html_body));
            doc.markdown_alternate_url(&final_url_parsed)
        });

    if let Some(mirror_url) = advertised {
        if mirror_url == final_url_parsed {
            return Ok(None);
        }
        return fetch_validated_mirror(ctx, mirror_url, respect_robots, max_bytes, true).await;
    }

    if source_mode == SourceMode::Negotiate {
        let path = final_url_parsed.path();
        let lower_path = path.to_ascii_lowercase();
        if !path.is_empty()
            && !lower_path.ends_with('/')
            && !lower_path.ends_with(".html")
            && !lower_path.ends_with(".htm")
            && !lower_path.ends_with(".md")
        {
            let mut probe = final_url_parsed.clone();
            probe.set_path(&format!("{path}.md"));
            probe.set_fragment(None);
            return fetch_validated_mirror(ctx, probe, respect_robots, max_bytes, false).await;
        }
    }

    Ok(None)
}

async fn acquire_page(
    ctx: &RuntimeContext,
    parsed: &url::Url,
    render_mode: RenderMode,
    source_mode: SourceMode,
    wait_for_selector: Option<&str>,
    respect_robots: bool,
    capture_axtree: bool,
) -> std::result::Result<AcquiredPage, FetchError> {
    let start = Instant::now();
    let url_str = parsed.as_str();
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".to_string()))?;

    let rule = ctx.rules.get(host);
    let cached = ctx.site_cache.get(host);
    let cef_available = ctx.cef.is_available();
    let cdp_renderer = ctx.cdp_renderer();
    let cdp_available = cdp_renderer.is_some();
    let effective_selector =
        wait_for_selector.or_else(|| rule.and_then(|r| r.wait_for_selector.as_deref()));

    if render_mode == RenderMode::Cef && !cef_available {
        return Err(FetchError::CefUnavailable(format!(
            "render=cef requested but pleb-renderer was not found; install CEF ({}) or use --render cdp with --cdp-url",
            crate::transport::cef::rebuild_command_hint()
        )));
    }
    if render_mode == RenderMode::Cdp {
        if ctx.config.proxy.is_some() {
            return Err(FetchError::CdpUnavailable(
                "CDP renderer cannot be used with --proxy because WebSocket CDP connections would bypass the proxy"
                    .to_string(),
            ));
        }
        if !cdp_available {
            return Err(FetchError::CdpUnavailable(
                "render=cdp requested but --cdp-url / PLEB_CDP_URL is not configured".to_string(),
            ));
        }
    }

    let plan = plan_strategy(
        render_mode,
        rule,
        cached.as_ref(),
        cef_available,
        cdp_available,
        ctx.config.prefer_cdp,
    );

    let mut fallback_used = false;
    if plan.primary == PrimaryTransport::Cdp {
        let Some(cdp) = cdp_renderer.as_deref() else {
            return Err(FetchError::CdpUnavailable(
                "CDP renderer is not configured".to_string(),
            ));
        };
        match acquire_via_cdp(
            ctx,
            cdp,
            host,
            url_str,
            effective_selector,
            capture_axtree,
            start,
            false,
        )
        .await
        {
            Ok(page) => return Ok(page),
            Err(err) => {
                let timing_ms = start.elapsed().as_millis() as u64;
                if matches!(err, FetchError::Timeout { .. }) {
                    ctx.site_cache.record(host, "cdp_timeout", timing_ms);
                }
                if is_non_fallback_cdp_fetch_error(&err) || !plan.allow_cronet_fallback {
                    return Err(err);
                }
                tracing::warn!(
                    "CDP render failed for {}: {}; falling back to network transport",
                    host,
                    err
                );
                fallback_used = true;
            }
        }
    } else if plan.primary == PrimaryTransport::Cef {
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
                fallback_used = true;
            }
        }
    }

    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES);
    let accept_markdown = source_mode == SourceMode::Negotiate
        || (source_mode == SourceMode::Auto && rule.is_some_and(|r| r.accept_markdown));
    let mut current_url = url_str.to_string();
    let mut current_parsed = parsed.clone();
    let mut visited_refresh = std::collections::HashSet::from([current_url.clone()]);
    let (status, final_url, content_type, link_header, is_html, body, mut parsed_html) = {
        const MAX_META_REFRESH_HOPS: usize = 3;
        let mut hop = 0usize;
        loop {
            let signer = ctx
                .web_bot_auth_signer_for_url(&current_parsed)
                .map_err(FetchError::Transport)?;
            let req_opts = RequestHeaderOptions {
                accept_markdown,
                ..Default::default()
            };
            let resp = execute_signed_get(
                &ctx.client,
                &current_url,
                ctx.config.timeout,
                max_bytes,
                &req_opts,
                signer.as_deref(),
            )
            .await
            .map_err(|e| map_transport_error(&current_url, e, max_bytes))?;
            let status = resp.status;
            let final_url = resp.final_url.clone();
            let content_type = resp.content_type.clone();
            let link_header = resp.link_header.clone();
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
            if status == 200
                && is_html
                && hop < MAX_META_REFRESH_HOPS
                && extract::dom::may_contain_meta_refresh(&body)
            {
                let page_url = url::Url::parse(&final_url).unwrap_or_else(|_| parsed.clone());
                let refresh_target = parsed_html
                    .get_or_insert_with(|| ParsedHtml::parse(&body))
                    .meta_refresh_url(&page_url);
                if let Some(next_url) = refresh_target {
                    let next_str = next_url.to_string();
                    if visited_refresh.insert(next_str.clone()) {
                        if respect_robots && !ctx.robots.is_allowed(&ctx.client, &next_str).await {
                            return Err(FetchError::RobotsDisallowed {
                                host: next_url.host_str().unwrap_or(host).to_string(),
                                url: next_str,
                            });
                        }
                        current_url = next_str;
                        current_parsed = next_url;
                        hop += 1;
                        continue;
                    }
                }
            }

            break (
                status,
                final_url,
                content_type,
                link_header,
                is_html,
                body,
                parsed_html,
            );
        }
    };
    let classification = classify_response_with_context(
        status,
        is_html,
        Some(url_str),
        Some(&final_url),
        &body,
        &mut parsed_html,
    );

    let record_network_page =
        |body, final_url, extract_base_url, content_type, strategy: &str, parsed| -> AcquiredPage {
            ctx.site_cache.record(
                host,
                ctx.client.transport_name(),
                start.elapsed().as_millis() as u64,
            );
            AcquiredPage {
                body,
                final_url,
                extract_base_url,
                status_code: status,
                content_type,
                strategy: strategy.to_string(),
                escalated: fallback_used,
                warning: None,
                parsed,
                axtree_text: None,
                imperative_tools: Vec::new(),
            }
        };

    match classification {
        ResponseClassification::Ok => {
            if accept_markdown && status < 400 && is_markdown_content_type(content_type.as_deref())
            {
                return Ok(record_network_page(
                    body,
                    final_url,
                    None,
                    content_type,
                    "markdown_negotiated",
                    None,
                ));
            }

            if source_mode != SourceMode::None && status == 200 && is_html {
                if let Some((mirror_body, mirror_base_url)) = try_fetch_markdown_mirror(
                    ctx,
                    parsed,
                    &final_url,
                    link_header.as_deref(),
                    &body,
                    &mut parsed_html,
                    source_mode,
                    respect_robots,
                    max_bytes,
                )
                .await?
                {
                    return Ok(record_network_page(
                        mirror_body,
                        final_url,
                        Some(mirror_base_url),
                        Some("text/markdown".to_string()),
                        "markdown_mirror",
                        None,
                    ));
                }
            }

            Ok(record_network_page(
                body,
                final_url,
                None,
                content_type,
                ctx.client.transport_name(),
                parsed_html,
            ))
        }
        ResponseClassification::JavaScriptShell { reason } => {
            let mut escalation_failure: Option<(&'static str, String)> = None;
            match plan.escalation {
                EscalationBackend::Cef => {
                    match try_cef_escalation(ctx, host, start, &final_url, effective_selector).await
                    {
                        RendererEscalation::Completed(res) => return *res,
                        RendererEscalation::Unavailable(err) => return Err(err),
                        RendererEscalation::RenderFailed(err) => {
                            tracing::warn!(
                                "JS shell detected ({}) on {} and CEF escalation failed: {}",
                                reason,
                                host,
                                err
                            );
                            escalation_failure = Some(("CEF", err.to_string()));
                        }
                    }
                }
                EscalationBackend::Cdp => {
                    if let Some(cdp) = cdp_renderer.as_deref() {
                        match try_cdp_escalation(
                            ctx,
                            cdp,
                            host,
                            start,
                            &final_url,
                            effective_selector,
                            capture_axtree,
                        )
                        .await
                        {
                            RendererEscalation::Completed(res) => return *res,
                            RendererEscalation::Unavailable(err) => return Err(err),
                            RendererEscalation::RenderFailed(err) => {
                                if is_non_fallback_cdp_fetch_error(&err) {
                                    return Err(err);
                                }
                                tracing::warn!(
                                    "JS shell detected ({}) on {} and CDP escalation failed: {}",
                                    reason,
                                    host,
                                    err
                                );
                                escalation_failure = Some(("CDP", err.to_string()));
                            }
                        }
                    }
                }
                EscalationBackend::None => {}
            }
            let mut page = record_network_page(
                body,
                final_url,
                None,
                content_type,
                ctx.client.transport_name(),
                parsed_html,
            );
            page.warning = Some(match escalation_failure {
                Some((backend, err)) => format!(
                    "JavaScript shell detected ({reason}) and {backend} escalation failed ({err}); returning unrendered HTML"
                ),
                None if render_mode == RenderMode::Cronet => format!(
                    "JavaScript shell detected ({reason}) but --render cronet disabled browser escalation; returning unrendered HTML"
                ),
                None if !cef_available && !cdp_available => format!(
                    "JavaScript shell detected ({reason}) but no browser renderer (CEF or CDP) is available; install CEF ({}) or pass --cdp-url",
                    crate::transport::cef::rebuild_command_hint()
                ),
                None => format!(
                    "JavaScript shell detected ({reason}) and browser escalation was skipped; returning unrendered HTML"
                ),
            });
            Ok(page)
        }
        ResponseClassification::Blocked { status } => {
            match plan.escalation {
                EscalationBackend::Cef => {
                    match try_cef_escalation(ctx, host, start, &final_url, effective_selector).await
                    {
                        RendererEscalation::Completed(res) => return *res,
                        RendererEscalation::Unavailable(err) => return Err(err),
                        RendererEscalation::RenderFailed(err) => {
                            return Err(FetchError::Blocked {
                                url: final_url,
                                status,
                                detail: format!(
                                    "network blocked and CEF escalation failed: {}",
                                    err
                                ),
                            });
                        }
                    }
                }
                EscalationBackend::Cdp => {
                    if let Some(cdp) = cdp_renderer.as_deref() {
                        match try_cdp_escalation(
                            ctx,
                            cdp,
                            host,
                            start,
                            &final_url,
                            effective_selector,
                            capture_axtree,
                        )
                        .await
                        {
                            RendererEscalation::Completed(res) => return *res,
                            RendererEscalation::Unavailable(err) => return Err(err),
                            RendererEscalation::RenderFailed(err) => {
                                if matches!(
                                    err,
                                    FetchError::Challenge { .. } | FetchError::HttpError { .. }
                                ) {
                                    return Err(err);
                                }
                                return Err(FetchError::Blocked {
                                    url: final_url,
                                    status,
                                    detail: format!(
                                        "network blocked and CDP escalation failed: {}",
                                        err
                                    ),
                                });
                            }
                        }
                    }
                }
                EscalationBackend::None => {}
            }
            let detail = if !cef_available && !cdp_available && render_mode != RenderMode::Cronet {
                format!(
                    "site blocked the request and no browser renderer (CEF or CDP) is available; install CEF ({}) or pass --cdp-url",
                    crate::transport::cef::rebuild_command_hint()
                )
            } else {
                "site blocked the request".to_string()
            };
            Err(FetchError::Blocked {
                url: final_url,
                status,
                detail,
            })
        }
        ResponseClassification::Challenge { status, reason } => Err(FetchError::Challenge {
            url: final_url,
            status,
            reason,
        }),
        ResponseClassification::HttpError { status } => Err(FetchError::HttpError {
            url: final_url,
            status,
        }),
    }
}

enum RendererEscalation {
    Completed(Box<std::result::Result<AcquiredPage, FetchError>>),
    Unavailable(FetchError),
    RenderFailed(FetchError),
}

async fn try_cef_escalation(
    ctx: &RuntimeContext,
    host: &str,
    start: Instant,
    url: &str,
    wait_for_selector: Option<&str>,
) -> RendererEscalation {
    match render_cef_checked(&ctx.cef, url, wait_for_selector, ctx.config.cef_timeout).await {
        Ok(cef_res) => RendererEscalation::Completed(Box::new(classify_cef_result(
            ctx, host, url, start, cef_res, true,
        ))),
        Err(err @ FetchError::CefUnavailable(_)) => RendererEscalation::Unavailable(err),
        Err(err) => RendererEscalation::RenderFailed(err),
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_cdp_escalation(
    ctx: &RuntimeContext,
    cdp: &CdpRenderer,
    host: &str,
    start: Instant,
    url: &str,
    wait_for_selector: Option<&str>,
    capture_axtree: bool,
) -> RendererEscalation {
    match acquire_via_cdp(
        ctx,
        cdp,
        host,
        url,
        wait_for_selector,
        capture_axtree,
        start,
        true,
    )
    .await
    {
        Ok(page) => RendererEscalation::Completed(Box::new(Ok(page))),
        Err(err @ FetchError::CdpUnavailable(_)) => RendererEscalation::Unavailable(err),
        Err(err) => {
            if matches!(err, FetchError::Timeout { .. }) {
                ctx.site_cache
                    .record(host, "cdp_timeout", start.elapsed().as_millis() as u64);
            }
            RendererEscalation::RenderFailed(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn acquire_via_cdp(
    ctx: &RuntimeContext,
    cdp: &CdpRenderer,
    host: &str,
    url: &str,
    wait_for_selector: Option<&str>,
    capture_axtree: bool,
    start: Instant,
    escalated: bool,
) -> std::result::Result<AcquiredPage, FetchError> {
    let max_bytes = ctx
        .config
        .max_response_bytes
        .unwrap_or(DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES);
    let opts = CdpRenderOptions {
        wait_for_selector: wait_for_selector.map(|s| s.to_string()),
        capture_axtree,
        max_response_bytes: max_bytes,
        allow_local_targets: false,
    };
    let res = cdp
        .render(url, &opts, ctx.config.cdp_timeout)
        .await
        .map_err(|err| map_cdp_error(url, err, max_bytes))?;
    if let Some(reason) = detect_interstitial_challenge(&res.html) {
        return Err(challenge_error_from_cdp(res, reason));
    }
    classify_cdp_result(ctx, host, url, start, res, escalated)
}

#[allow(clippy::too_many_arguments)]
fn classify_rendered_result(
    ctx: &RuntimeContext,
    host: &str,
    requested_url: &str,
    start: Instant,
    strategy: &'static str,
    final_url: String,
    status_code: u16,
    html: String,
    content_type: Option<String>,
    axtree_text: Option<String>,
    imperative_tools: Vec<crate::extract::WebMcpToolDescriptor>,
    escalated: bool,
) -> std::result::Result<AcquiredPage, FetchError> {
    if status_code == 0 {
        return Err(FetchError::Transport(anyhow::anyhow!(
            "{} render did not capture an HTTP status code for {final_url}",
            strategy.to_ascii_uppercase()
        )));
    }

    let mut parsed_html = None;
    match classify_response_with_context(
        status_code,
        true,
        Some(requested_url),
        Some(&final_url),
        &html,
        &mut parsed_html,
    ) {
        ResponseClassification::Ok | ResponseClassification::JavaScriptShell { .. } => {
            let timing_ms = start.elapsed().as_millis() as u64;
            ctx.site_cache.record(host, strategy, timing_ms);
            Ok(AcquiredPage {
                body: html,
                final_url,
                extract_base_url: None,
                status_code,
                content_type,
                strategy: strategy.to_string(),
                escalated,
                warning: None,
                parsed: parsed_html,
                axtree_text,
                imperative_tools,
            })
        }
        ResponseClassification::Blocked { status } => Err(FetchError::Blocked {
            url: final_url,
            status,
            detail: format!(
                "site blocked the request after {} render",
                strategy.to_ascii_uppercase()
            ),
        }),
        ResponseClassification::Challenge { status, reason } => Err(FetchError::Challenge {
            url: final_url,
            status,
            reason,
        }),
        ResponseClassification::HttpError { status } => Err(FetchError::HttpError {
            url: final_url,
            status,
        }),
    }
}

fn classify_cdp_result(
    ctx: &RuntimeContext,
    host: &str,
    requested_url: &str,
    start: Instant,
    res: CdpRenderResult,
    escalated: bool,
) -> std::result::Result<AcquiredPage, FetchError> {
    let content_type = res
        .content_type
        .filter(|c| extract::is_html_content_type(Some(c)))
        .or_else(|| Some("text/html".to_string()));
    classify_rendered_result(
        ctx,
        host,
        requested_url,
        start,
        "cdp",
        res.final_url,
        res.status_code,
        res.html,
        content_type,
        res.axtree_text,
        res.imperative_tools,
        escalated,
    )
}

fn challenge_error_from_cdp(result: CdpRenderResult, reason: String) -> FetchError {
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

fn is_non_fallback_cdp_fetch_error(err: &FetchError) -> bool {
    match err {
        FetchError::CdpUnavailable(_)
        | FetchError::Challenge { .. }
        | FetchError::Blocked { .. }
        | FetchError::HttpError { .. }
        | FetchError::ResponseTooLarge { .. } => true,
        FetchError::Transport(inner) => matches!(
            transport_error_kind(inner),
            Some(TransportErrorKind::InvalidSelector | TransportErrorKind::RedirectBlocked)
        ),
        _ => false,
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
    classify_rendered_result(
        ctx,
        host,
        requested_url,
        start,
        "cef",
        res.final_url,
        res.status_code,
        res.html,
        Some("text/html".to_string()),
        None,
        Vec::new(),
        escalated,
    )
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
    matches!(
        transport_error_kind(err),
        Some(
            TransportErrorKind::Timeout
                | TransportErrorKind::ProtocolMismatch
                | TransportErrorKind::InvalidSelector
                | TransportErrorKind::Canceled
                | TransportErrorKind::ResponseTooLarge
                | TransportErrorKind::RedirectBlocked
        )
    )
}

fn map_transport_error(url: &str, err: anyhow::Error, max_response_bytes: usize) -> FetchError {
    if let Some(too_large) = err.downcast_ref::<ResponseTooLargeError>() {
        return FetchError::ResponseTooLarge {
            url: url.to_string(),
            limit_bytes: too_large.limit_bytes,
        };
    }
    match transport_error_kind(&err) {
        Some(TransportErrorKind::Timeout) => FetchError::Timeout {
            url: url.to_string(),
            detail: err.to_string(),
        },
        Some(TransportErrorKind::ResponseTooLarge) => FetchError::ResponseTooLarge {
            url: url.to_string(),
            limit_bytes: max_response_bytes,
        },
        Some(TransportErrorKind::ProtocolMismatch) => FetchError::CefUnavailable(err.to_string()),
        _ => FetchError::Transport(err),
    }
}

fn map_cef_error(url: &str, err: anyhow::Error) -> FetchError {
    map_transport_error(url, err, DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES)
}

fn map_cdp_error(url: &str, err: anyhow::Error, max_response_bytes: usize) -> FetchError {
    if let Some(too_large) = err.downcast_ref::<ResponseTooLargeError>() {
        return FetchError::ResponseTooLarge {
            url: url.to_string(),
            limit_bytes: too_large.limit_bytes,
        };
    }
    match transport_error_kind(&err) {
        Some(TransportErrorKind::Timeout) => FetchError::Timeout {
            url: url.to_string(),
            detail: err.to_string(),
        },
        Some(TransportErrorKind::ResponseTooLarge) => FetchError::ResponseTooLarge {
            url: url.to_string(),
            limit_bytes: max_response_bytes,
        },
        Some(TransportErrorKind::ProtocolMismatch) => FetchError::CdpUnavailable(err.to_string()),
        _ => FetchError::Transport(err),
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<crate::extract::ResourceDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<crate::extract::WebMcpToolDescriptor>,
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
                warning: out.warning,
                content: (!metadata_only).then_some(out.content),
                resources: out.resources,
                tools: out.tools,
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
            let host_sem = host_semaphore(&host_sems_c, host_key);

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
    use crate::transport::TransportError;

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

        let cdp_mismatch = map_cdp_error(
            "https://example.com",
            anyhow::Error::new(TransportError::new(
                TransportErrorKind::ProtocolMismatch,
                "CDP Target.createTarget missing",
            )),
            1024,
        );
        assert!(matches!(cdp_mismatch, FetchError::CdpUnavailable(_)));
        assert_eq!(cdp_mismatch.kind(), "cdp_unavailable");
        assert_eq!(cdp_mismatch.exit_code(), 1);

        let invalid_selector_err = anyhow::Error::new(TransportError::new(
            TransportErrorKind::InvalidSelector,
            "bad css selector",
        ));
        assert!(is_non_retryable_cef_error(&invalid_selector_err));
        let mapped = map_transport_error("https://example.com", invalid_selector_err, 1024);
        assert!(matches!(mapped, FetchError::Transport(_)));
    }

    #[tokio::test]
    async fn try_fetch_raw_source_handles_markdown_code_404_and_binary_fallback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());

        tokio::spawn(async move {
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
                    let (status, ct, body): (&str, &str, &[u8]) = match path {
                        "/README.md" => (
                            "200 OK",
                            "text/plain; charset=utf-8",
                            b"# Raw Readme\n\nHello raw.",
                        ),
                        "/lib.rs" => (
                            "200 OK",
                            "text/plain; charset=utf-8",
                            b"pub fn answer() -> u32 { 42 }\n",
                        ),
                        "/binary.bin" => ("200 OK", "application/pdf", b"%PDF-1.7\n"),
                        "/missing.rs" => ("404 Not Found", "text/plain", b"not found"),
                        "/robots.txt" => (
                            "200 OK",
                            "text/plain",
                            b"User-agent: *\nDisallow: /lib.rs\n",
                        ),
                        _ => ("404 Not Found", "text/plain", b"unknown"),
                    };
                    let hdr = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });

        let state_dir =
            std::path::PathBuf::from(format!("/tmp/pleb-t-{}-rawsrc", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        let ctx = RuntimeContext::new(crate::context::RuntimeConfig {
            state_dir: state_dir.clone(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            use_daemon: false,
            ..Default::default()
        })
        .unwrap();

        let effective = url::Url::parse("https://github.com/org/repo/blob/main/README.md").unwrap();
        let opts = FetchOptions {
            render: RenderMode::Cronet,
            ..Default::default()
        };

        // 1. Markdown raw source succeeds with strategy == "raw_source".
        let md_cand = RawSourceCandidate {
            raw_url: url::Url::parse(&format!("{base}/README.md")).unwrap(),
            filename: "README.md".to_string(),
            kind: RawSourceKind::Markdown,
        };
        let md_out = try_fetch_raw_source(
            &ctx,
            effective.as_str(),
            &effective,
            &md_cand,
            &opts,
            Instant::now(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(md_out.strategy, "raw_source");
        assert!(md_out.content.contains("Hello raw."));

        // 2. Code raw source succeeds with strategy == "raw_source".
        let rs_cand = RawSourceCandidate {
            raw_url: url::Url::parse(&format!("{base}/lib.rs")).unwrap(),
            filename: "lib.rs".to_string(),
            kind: RawSourceKind::Code {
                language: Some("rust"),
            },
        };
        let rs_out = try_fetch_raw_source(
            &ctx,
            effective.as_str(),
            &effective,
            &rs_cand,
            &opts,
            Instant::now(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rs_out.strategy, "raw_source");
        assert!(rs_out.content.contains("```rust"));
        assert!(rs_out.content.contains("pub fn answer()"));

        // 3. 404 returns None so fetch_document falls back to HTML.
        let missing_cand = RawSourceCandidate {
            raw_url: url::Url::parse(&format!("{base}/missing.rs")).unwrap(),
            filename: "missing.rs".to_string(),
            kind: RawSourceKind::Code {
                language: Some("rust"),
            },
        };
        assert!(try_fetch_raw_source(
            &ctx,
            effective.as_str(),
            &effective,
            &missing_cand,
            &opts,
            Instant::now()
        )
        .await
        .unwrap()
        .is_none());

        // 4. Binary content returns None so fetch_document falls back cleanly.
        let bin_cand = RawSourceCandidate {
            raw_url: url::Url::parse(&format!("{base}/binary.bin")).unwrap(),
            filename: "binary.bin".to_string(),
            kind: RawSourceKind::Code { language: None },
        };
        assert!(try_fetch_raw_source(
            &ctx,
            effective.as_str(),
            &effective,
            &bin_cand,
            &opts,
            Instant::now()
        )
        .await
        .unwrap()
        .is_none());

        // 5. respect_robots=true skips disallowed raw_url.
        let robots_opts = FetchOptions {
            respect_robots: true,
            ..opts
        };
        assert!(try_fetch_raw_source(
            &ctx,
            effective.as_str(),
            &effective,
            &rs_cand,
            &robots_opts,
            Instant::now()
        )
        .await
        .unwrap()
        .is_none());

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn format_resource_cue_summarizes_high_salience_only() {
        let resources = vec![
            ResourceDescriptor {
                identifier: "r1".to_string(),
                kind: ResourceKind::Diagram,
                url: "https://example.com/arch.png".to_string(),
                mime_hint: Some("image/png".to_string()),
                section_heading: None,
                label: Some("Arch".to_string()),
                dimensions: Some((800, 400)),
                salience: ResourceSalience::High,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
            ResourceDescriptor {
                identifier: "r2".to_string(),
                kind: ResourceKind::Image,
                url: "https://example.com/photo.jpg".to_string(),
                mime_hint: None,
                section_heading: None,
                label: Some("Photo".to_string()),
                dimensions: None,
                salience: ResourceSalience::Medium,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
            ResourceDescriptor {
                identifier: "r3".to_string(),
                kind: ResourceKind::Transcript,
                url: "https://example.com/captions.vtt".to_string(),
                mime_hint: Some("text/vtt".to_string()),
                section_heading: None,
                label: Some("English".to_string()),
                dimensions: None,
                salience: ResourceSalience::High,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
        ];
        let cue = format_resource_cue(&resources).unwrap();
        assert_eq!(
            cue,
            "Resources: 1 diagram (r1), 1 transcript (r3) — pass --resources manifest or --stage-resources <ids>"
        );
        assert!(format_resource_cue(&resources[1..2]).is_none());
    }

    #[tokio::test]
    async fn enrich_document_resources_stages_diagrams_rewrites_markdown_and_inlines_transcripts() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());

        tokio::spawn(async move {
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
                    let (status, ct, body): (&str, &str, &[u8]) = match path {
                        "/arch.png" => (
                            "200 OK",
                            "image/png",
                            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
                        ),
                        "/talk.vtt" => (
                            "200 OK",
                            "text/vtt; charset=utf-8",
                            b"WEBVTT\n\n00:00:01.000 --> 00:00:03.000\n<v Alice>Welcome to the architecture talk.\n",
                        ),
                        "/bad.png" => (
                            "200 OK",
                            "text/html",
                            b"<!DOCTYPE html><html><body>soft 404</body></html>",
                        ),
                        _ => ("404 Not Found", "text/plain", b"not found"),
                    };
                    let hdr = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(hdr.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });

        let state_dir =
            std::path::PathBuf::from(format!("/tmp/pleb-t-{}-enrich", std::process::id()));
        let _ = std::fs::remove_dir_all(&state_dir);
        std::fs::create_dir_all(&state_dir).unwrap();
        let ctx = RuntimeContext::new(crate::context::RuntimeConfig {
            state_dir: state_dir.clone(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            use_daemon: false,
            ..Default::default()
        })
        .unwrap();

        let page_url = url::Url::parse(&format!("{base}/post")).unwrap();
        let png_url = format!("{base}/arch.png");
        let vtt_url = format!("{base}/talk.vtt");
        let bad_url = format!("{base}/bad.png");
        let mut content = format!("# Architecture\n\n![System Diagram]({png_url})\n");
        let mut resources = vec![
            ResourceDescriptor {
                identifier: "r1".to_string(),
                kind: ResourceKind::Diagram,
                url: png_url.clone(),
                mime_hint: Some("image/png".to_string()),
                section_heading: Some("Architecture".to_string()),
                label: Some("System Diagram".to_string()),
                dimensions: Some((800, 400)),
                salience: ResourceSalience::High,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
            ResourceDescriptor {
                identifier: "r2".to_string(),
                kind: ResourceKind::Transcript,
                url: vtt_url,
                mime_hint: Some("text/vtt".to_string()),
                section_heading: Some("Architecture".to_string()),
                label: Some("English Captions".to_string()),
                dimensions: None,
                salience: ResourceSalience::High,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
            ResourceDescriptor {
                identifier: "r3".to_string(),
                kind: ResourceKind::Diagram,
                url: bad_url,
                mime_hint: Some("image/png".to_string()),
                section_heading: None,
                label: Some("Broken Soft 404".to_string()),
                dimensions: Some((500, 300)),
                salience: ResourceSalience::High,
                staged_path: None,
                byte_count: None,
                content_type: None,
            },
        ];

        let opts = FetchOptions {
            resources: ResourcePolicy::All,
            stage_resources: vec!["r2".to_string()],
            ..Default::default()
        };

        enrich_document_resources(&ctx, &page_url, &mut content, &mut resources, &opts)
            .await
            .unwrap();

        let staged_png = resources[0].staged_path.as_deref().unwrap();
        assert!(std::path::Path::new(staged_png).exists());
        assert_eq!(resources[0].byte_count, Some(16));
        assert!(content.contains(&format!("![System Diagram]({staged_png})")));
        assert!(content.contains("## Staged Resources"));
        assert!(content.contains("- **r1** (diagram, 16 bytes):"));

        // Transcript was inlined and also staged because r2 was explicitly in --stage-resources.
        assert!(content.contains("## Media Transcript: English Captions"));
        assert!(content.contains("Welcome to the architecture talk."));
        assert!(resources[1].staged_path.is_some());

        // Disguised HTML-as-PNG was safely rejected without failing enrichment.
        assert!(resources[2].staged_path.is_none());

        let _ = std::fs::remove_dir_all(&state_dir);
    }
}
