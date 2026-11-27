use anyhow::Result;
use clap::ValueEnum;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::context::RuntimeContext;
use crate::crawl::{self, CrawlOptions, MapOptions};
use crate::daemon::protocol::read_bounded_line;
use crate::extract::{ExtractMode, ExtractOptions, OutputFormat};
use crate::fetch::{self, FetchError, FetchOptions};
use crate::policy::{RenderMode, SourceMode};
use crate::search::{self, Freshness, SearchMode, SearchOptions, SearchProvider};
use crate::transport::cef::{self, RendererProbe};

const MAX_MCP_LINE_BYTES: usize = 4 * 1024 * 1024;
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_PROTOCOL: &str = "2025-03-26";

pub async fn serve_stdio(ctx: Arc<RuntimeContext>) -> Result<()> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_io(ctx, stdin, stdout).await
}

pub async fn serve_io<R, W>(ctx: Arc<RuntimeContext>, mut reader: R, mut writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    while let Some(line) = read_bounded_line(&mut reader, MAX_MCP_LINE_BYTES).await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                write_json_error(
                    &mut writer,
                    Value::Null,
                    -32700,
                    format!("parse error: {err}"),
                )
                .await?;
                continue;
            }
        };

        let Some(obj) = req.as_object() else {
            write_json_error(
                &mut writer,
                Value::Null,
                -32600,
                "invalid request: expected JSON object",
            )
            .await?;
            continue;
        };

        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            if let Some(req_id) = id {
                write_json_error(
                    &mut writer,
                    req_id,
                    -32600,
                    "invalid request: missing string method",
                )
                .await?;
            }
            continue;
        };

        if id.is_none() || method.starts_with("notifications/") {
            continue;
        }
        let req_id = id.expect("checked id.is_some()");
        let params = obj.get("params");

        match dispatch_method(&ctx, method, params).await {
            Ok(result) => write_json_result(&mut writer, req_id, result).await?,
            Err(rpc_err) => {
                write_json_error(&mut writer, req_id, rpc_err.code, rpc_err.message).await?;
            }
        }
    }

    Ok(())
}

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }
}

async fn write_json_result<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Value,
    result: Value,
) -> Result<()> {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    write_json_line(writer, &resp).await
}

async fn write_json_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: Value,
    code: i64,
    message: impl Into<String>,
) -> Result<()> {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        },
    });
    write_json_line(writer, &resp).await
}

async fn write_json_line<W: AsyncWrite + Unpin>(writer: &mut W, payload: &Value) -> Result<()> {
    let mut encoded = serde_json::to_string(payload)?;
    encoded.push('\n');
    writer.write_all(encoded.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn dispatch_method(
    ctx: &RuntimeContext,
    method: &str,
    params: Option<&Value>,
) -> std::result::Result<Value, RpcError> {
    match method {
        "initialize" => {
            let requested = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str);
            let negotiated = requested
                .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
                .unwrap_or(DEFAULT_PROTOCOL);
            Ok(json!({
                "protocolVersion": negotiated,
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "pleb",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "pleb fetches web pages locally via Chrome's network stack (Cronet) and offscreen Chromium (CEF) to resolve datacenter-IP and bot-User-Agent 403 blocks cleanly. Use pleb_fetch whenever a remote fetch is blocked or returns an empty JS shell."
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params_obj = params
                .and_then(Value::as_object)
                .ok_or_else(|| RpcError::invalid_params("tools/call requires an object params"))?;
            let name = params_obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RpcError::invalid_params("missing tool name"))?;
            let args = match params_obj.get("arguments") {
                None => serde_json::Map::new(),
                Some(Value::Object(map)) => map.clone(),
                Some(_) => {
                    return Err(RpcError::invalid_params(
                        "tool arguments must be a JSON object",
                    ))
                }
            };
            call_tool(ctx, name, &args).await
        }
        other => Err(RpcError::method_not_found(other)),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "pleb_fetch",
            "description": "Fetch a public URL from this machine using Chrome's TLS/HTTP2 stack (Cronet) with automatic CEF/CDP escalation on 403 blocks and JS shells, returning clean Markdown or text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "http or https URL to fetch" },
                    "format": { "type": "string", "enum": ["markdown", "html", "text", "axtree"] },
                    "extract": { "type": "string", "enum": ["clean", "article", "full"] },
                    "render": { "type": "string", "enum": ["auto", "cef", "cdp", "cronet"] },
                    "source": { "type": "string", "enum": ["auto", "negotiate", "none"] },
                    "focus": { "type": "string", "description": "Optional query for BM25 section projection" },
                    "max_bytes": { "type": "integer", "minimum": 1 },
                    "wait_for_selector": { "type": "string", "description": "CSS selector to wait for when CEF/CDP renders" },
                    "respect_robots": { "type": "boolean" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "pleb_search",
            "description": "Search the web using configured search providers (Brave, Tavily, Serper, Exa, SearXNG) with DuckDuckGo fallback.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 },
                    "provider": { "type": "string", "enum": ["auto", "brave", "tavily", "serper", "exa", "searxng", "duckduckgo", "federated"] },
                    "mode": { "type": "string", "enum": ["auto", "exact", "semantic"] },
                    "freshness": { "type": "string", "enum": ["day", "week", "month", "year"] },
                    "site": { "type": "string", "description": "Restrict results to domain" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "pleb_crawl",
            "description": "Crawl same-origin pages from a seed URL and return clean Markdown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Starting http or https URL" },
                    "depth": { "type": "integer", "minimum": 0, "maximum": 5 },
                    "max_pages": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "extract": { "type": "string", "enum": ["clean", "article", "full"] },
                    "path_filter": { "type": "string" },
                    "respect_robots": { "type": "boolean" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "pleb_map",
            "description": "Discover URLs on a website via sitemap.xml and link traversal.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Starting http or https URL" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 5000 },
                    "use_sitemap": { "type": "boolean" },
                    "path_filter": { "type": "string" },
                    "respect_robots": { "type": "boolean" }
                },
                "required": ["url"]
            }
        },
        {
            "name": "pleb_status",
            "description": "Inspect local pleb transport readiness (Cronet TLS, CEF v5-meta renderer, CDP endpoint, and state dir).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}

async fn call_tool(
    ctx: &RuntimeContext,
    name: &str,
    args: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, RpcError> {
    match name {
        "pleb_fetch" => call_fetch(ctx, args).await,
        "pleb_search" => call_search(ctx, args).await,
        "pleb_crawl" => call_crawl(ctx, args).await,
        "pleb_map" => call_map(ctx, args).await,
        "pleb_status" => Ok(tool_ok(build_mcp_status_text(ctx))),
        _ => Err(RpcError::invalid_params(format!("unknown tool: {name}"))),
    }
}

async fn call_fetch(
    ctx: &RuntimeContext,
    args: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, RpcError> {
    let url = required_str(args, "url")?;
    let format = parse_opt_enum::<OutputFormat>(args, "format")?.unwrap_or(OutputFormat::Markdown);
    if format == OutputFormat::Raw {
        return Err(RpcError::invalid_params(
            "format 'raw' is not supported over MCP; use markdown, html, text, or axtree",
        ));
    }
    let mode = parse_opt_enum::<ExtractMode>(args, "extract")?.unwrap_or(ExtractMode::Clean);
    let render = parse_opt_enum::<RenderMode>(args, "render")?.unwrap_or(RenderMode::Auto);
    let source = parse_opt_enum::<SourceMode>(args, "source")?.unwrap_or(SourceMode::Auto);
    let focus = opt_nonempty_str(args, "focus")?;
    let max_bytes = opt_positive_usize(args, "max_bytes")?;
    let wait_for_selector = opt_nonempty_str(args, "wait_for_selector")?;
    let respect_robots = opt_bool(args, "respect_robots")?.unwrap_or(false);

    let opts = FetchOptions {
        extract: ExtractOptions {
            format,
            mode,
            include_images: true,
            include_links: true,
            max_bytes,
            focus,
        },
        render,
        source,
        wait_for_selector,
        respect_robots,
        ..Default::default()
    };

    match fetch::fetch_document(ctx, &url, &opts).await {
        Ok(res) => {
            let mut lines = Vec::new();
            if let Some(ref title) = res.title {
                lines.push(format!("Title: {title}"));
            }
            lines.push(format!("URL: {}", res.final_url));
            lines.push(format!(
                "Status: {} | Strategy: {} | Time: {}ms",
                res.status_code, res.strategy, res.timing_ms
            ));
            if let Some(cue) = fetch::format_resource_cue(&res.resources) {
                lines.push(cue);
            }
            if let Some(ref warning) = res.warning {
                lines.push(format!("Warning: {warning}"));
            }
            let text = format!("{}\n\n{}", lines.join("\n"), res.content);
            Ok(tool_ok(text))
        }
        Err(err) => Ok(tool_error_from_fetch(&err)),
    }
}

async fn call_search(
    ctx: &RuntimeContext,
    args: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, RpcError> {
    let query = required_str(args, "query")?;
    let limit = opt_u32_range(args, "limit", 1, 20)?.unwrap_or(8) as usize;
    let provider =
        parse_opt_enum::<SearchProvider>(args, "provider")?.unwrap_or(SearchProvider::Auto);
    let mode = parse_opt_enum::<SearchMode>(args, "mode")?.unwrap_or(SearchMode::Auto);
    let freshness = parse_opt_enum::<Freshness>(args, "freshness")?;
    let site = opt_nonempty_str(args, "site")?;

    let opts = SearchOptions {
        limit,
        provider,
        mode,
        freshness,
        site,
        extra_snippets: false,
    };
    match search::search(ctx, &query, &opts).await {
        Ok(resp) => Ok(tool_ok(search::format_results(&resp))),
        Err(err) => Ok(tool_error(json!({
            "error": err.to_string(),
            "kind": "search_error",
            "exit_code": 1,
        }))),
    }
}

async fn call_crawl(
    ctx: &RuntimeContext,
    args: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, RpcError> {
    let url = required_str(args, "url")?;
    let max_depth = opt_u32_range(args, "depth", 0, 5)?.unwrap_or(2);
    let max_pages = opt_u32_range(args, "max_pages", 1, 50)?.unwrap_or(10);
    let extract_mode =
        parse_opt_enum::<ExtractMode>(args, "extract")?.unwrap_or(ExtractMode::Clean);
    let path_filter = opt_nonempty_str(args, "path_filter")?;
    let respect_robots = opt_bool(args, "respect_robots")?.unwrap_or(true);

    let opts = CrawlOptions {
        max_depth,
        max_pages,
        format: OutputFormat::Markdown,
        extract_mode,
        respect_robots,
        path_filter,
    };

    match crawl::crawl(ctx, &url, opts).await {
        Ok(result) => {
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| url.clone());
            Ok(tool_ok(crawl::format_crawl_output(&result, &host)))
        }
        Err(err) => match err.downcast::<FetchError>() {
            Ok(fetch_err) => Ok(tool_error_from_fetch(&fetch_err)),
            Err(other) => Ok(tool_error(json!({
                "error": other.to_string(),
                "kind": "crawl_error",
                "exit_code": 1,
            }))),
        },
    }
}

async fn call_map(
    ctx: &RuntimeContext,
    args: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, RpcError> {
    let url = required_str(args, "url")?;
    let limit = opt_u32_range(args, "limit", 1, 5000)?.unwrap_or(100);
    let use_sitemap = opt_bool(args, "use_sitemap")?.unwrap_or(true);
    let path_filter = opt_nonempty_str(args, "path_filter")?;
    let respect_robots = opt_bool(args, "respect_robots")?.unwrap_or(true);

    let opts = MapOptions {
        limit,
        use_sitemap,
        respect_robots,
        path_filter,
    };

    match crawl::map(ctx, &url, opts).await {
        Ok(result) => {
            let host = url::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| url.clone());
            Ok(tool_ok(crawl::format_map_output(&result, &host)))
        }
        Err(err) => Ok(tool_error(json!({
            "error": err.to_string(),
            "kind": "map_error",
            "exit_code": 1,
        }))),
    }
}

fn build_mcp_status_text(ctx: &RuntimeContext) -> String {
    let cef_probe = cef::probe_renderer_with_override(
        &ctx.config.state_dir,
        ctx.config.cef_renderer_executable.as_deref(),
    );
    let cef_renderer = match cef_probe {
        RendererProbe::Ready(path) => json!({
            "status": "ready",
            "path": path.display().to_string(),
            "expected_protocol": cef::EXPECTED_PROTOCOL,
        }),
        RendererProbe::ProtocolMismatch {
            path,
            found,
            expected,
        } => json!({
            "status": "protocol_mismatch",
            "path": path.display().to_string(),
            "found_protocol": found,
            "expected_protocol": expected,
        }),
        RendererProbe::NotInstalled => json!({
            "status": "not_installed",
            "path": null,
            "expected_protocol": cef::EXPECTED_PROTOCOL,
        }),
    };

    let cdp_url = ctx
        .configured_cdp_url()
        .map(|u| crate::transport::cdp::sanitize_cdp_url_for_display(&u));
    let payload = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "cronet_enabled": cfg!(feature = "cronet"),
        "cef_renderer": cef_renderer,
        "cdp_renderer": {
            "configured": cdp_url.is_some(),
            "url": cdp_url,
            "prefer_cdp": ctx.config.prefer_cdp,
        },
        "state_dir": ctx.config.state_dir.display().to_string(),
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
}

fn tool_ok(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn tool_error(payload: Value) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

fn tool_error_from_fetch(err: &FetchError) -> Value {
    let mut payload = json!({
        "error": err.to_string(),
        "kind": err.kind(),
        "exit_code": err.exit_code(),
    });
    if let Some(reason) = err.block_reason() {
        payload["block_reason"] = json!(reason);
    }
    tool_error(payload)
}

fn required_str(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<String, RpcError> {
    let val = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params(format!("missing or empty string '{key}'")))?;
    Ok(val.to_string())
}

fn opt_nonempty_str(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<String>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|v| !v.is_empty())),
        Some(_) => Err(RpcError::invalid_params(format!(
            "'{key}' must be a string"
        ))),
    }
}

fn opt_bool(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<bool>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!(
            "'{key}' must be a boolean"
        ))),
    }
}

fn opt_positive_usize(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<usize>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                RpcError::invalid_params(format!("'{key}' must be an integer >= 1"))
            })?;
            if n == 0 {
                return Err(RpcError::invalid_params(format!("'{key}' must be >= 1")));
            }
            Ok(Some(n.min(usize::MAX as u64) as usize))
        }
    }
}

fn opt_u32_range(
    args: &serde_json::Map<String, Value>,
    key: &str,
    min: u32,
    max: u32,
) -> std::result::Result<Option<u32>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params(format!("'{key}' must be an integer")))?;
            if n < u64::from(min) || n > u64::from(max) {
                return Err(RpcError::invalid_params(format!(
                    "'{key}' must be between {min} and {max}"
                )));
            }
            Ok(Some(n as u32))
        }
    }
}

fn parse_opt_enum<T: ValueEnum>(
    args: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<Option<T>, RpcError> {
    let Some(raw) = opt_nonempty_str(args, key)? else {
        return Ok(None);
    };
    T::from_str(&raw, true)
        .map(Some)
        .map_err(|msg| RpcError::invalid_params(format!("invalid '{key}': {msg}")))
}
