use anyhow::Result;
use std::time::Instant;

use crate::extract::{self, Format};
use crate::robots;
use crate::site_cache;
use crate::site_rules;
use crate::transport::cef::{CefRenderOptions, CefRenderer};
use crate::transport::TransportClient;

pub struct FetchResult {
    pub content: String,
    pub title: Option<String>,
    pub url: String,
    pub status_code: u16,
    pub timing_ms: u64,
}

pub struct FetchHtmlResult {
    pub html: String,
    pub url: String,
    pub status_code: u16,
}

pub struct FetchRawResult {
    pub bytes: Vec<u8>,
    pub url: String,
    pub status_code: u16,
    pub timing_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    #[default]
    Auto,
    Cef,
    Cronet,
}

impl RenderMode {
    pub fn from_str(s: &str) -> RenderMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "cef" | "browser" | "render" | "js" => RenderMode::Cef,
            "cronet" | "network" | "fast" | "no-js" | "nojs" => RenderMode::Cronet,
            _ => RenderMode::Auto,
        }
    }
}

pub async fn fetch(
    client: &TransportClient,
    cef: &CefRenderer,
    url: &str,
    format: Format,
    respect_robots: bool,
    render: RenderMode,
    wait_for_selector: Option<&str>,
) -> Result<FetchResult> {
    let start = Instant::now();

    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL: {}", e))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("unsupported scheme {:?} (only http and https)", s),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;

    if host == "www.reddit.com" || host == "reddit.com" {
        let old_url = url
            .replace("://www.reddit.com", "://old.reddit.com")
            .replace("://reddit.com", "://old.reddit.com");
        return Box::pin(fetch(
            client,
            cef,
            &old_url,
            format,
            respect_robots,
            render,
            wait_for_selector,
        ))
        .await;
    }

    if respect_robots && !robots::check(client, url).await {
        return Ok(FetchResult {
            content: format!(
                "Blocked by robots.txt: {} disallows this path for automated agents.",
                host
            ),
            title: None,
            url: url.to_string(),
            status_code: 0,
            timing_ms: start.elapsed().as_millis() as u64,
        });
    }

    let cef_installed = cef.is_available();
    let cached = site_cache::get(host);
    let rule = site_rules::get(host);
    let effective_selector =
        wait_for_selector.or_else(|| rule.as_ref().and_then(|r| r.wait_for_selector.as_deref()));

    if render == RenderMode::Cef && !cef_installed {
        anyhow::bail!("render=cef requested but the CEF renderer is not installed.");
    }

    let cef_first = render == RenderMode::Cef
        || (render == RenderMode::Auto
            && should_use_cef_first(
                rule.as_ref().map(|r| r.render.as_str()),
                cached.as_ref().map(|s| s.strategy.as_str()),
                cef_installed,
            ));

    if cef_first {
        match cef_render_with_retry(cef, url, effective_selector).await {
            Ok(html) => {
                let extracted = extract::extract(&html, &parsed, format)?;
                let timing_ms = start.elapsed().as_millis() as u64;
                site_cache::record(host, "cef", false, timing_ms);
                return Ok(FetchResult {
                    content: extracted.content,
                    title: extracted.title,
                    url: url.to_string(),
                    status_code: 200,
                    timing_ms,
                });
            }
            Err(e) => {
                let timing_ms = start.elapsed().as_millis() as u64;
                site_cache::record(host, "cef_timeout", false, timing_ms);
                if render == RenderMode::Cef {
                    return Err(anyhow::anyhow!("CEF renderer failed: {}", e));
                }
                tracing::warn!("CEF renderer failed: {}. Falling back to transport.", e);
            }
        }
    }

    let escalated_from = if cef_first { Some("cef") } else { None };
    let resp = client.get(url).await?;
    let status = resp.status;
    let body = resp.text();

    if (status == 403 || status == 503) && escalated_from.is_none() && render != RenderMode::Cronet && cef_installed {
        if let Ok(html) = cef_render_with_retry(cef, url, effective_selector).await {
            let extracted = extract::extract(&html, &parsed, format)?;
            let timing_ms = start.elapsed().as_millis() as u64;
            site_cache::record(host, "cef", false, timing_ms);
            return Ok(FetchResult {
                content: extracted.content,
                title: extracted.title,
                url: url.to_string(),
                status_code: 200,
                timing_ms,
            });
        }
    }

    if status >= 400 {
        let timing_ms = start.elapsed().as_millis() as u64;
        return Ok(FetchResult {
            content: format!("HTTP {}: {}", status, body),
            title: None,
            url: url.to_string(),
            status_code: status,
            timing_ms,
        });
    }

    let js_shell = is_js_required_shell(&body);
    if escalated_from.is_none() && js_shell && render != RenderMode::Cronet && cef_installed {
        if let Ok(html) = cef_render_with_retry(cef, url, effective_selector).await {
            let extracted = extract::extract(&html, &parsed, format)?;
            let timing_ms = start.elapsed().as_millis() as u64;
            site_cache::record(host, "cef", false, timing_ms);
            return Ok(FetchResult {
                content: extracted.content,
                title: extracted.title,
                url: url.to_string(),
                status_code: 200,
                timing_ms,
            });
        }
    }

    let extracted = extract::extract(&body, &parsed, format)?;
    let timing_ms = start.elapsed().as_millis() as u64;
    site_cache::record(host, "cronet", false, timing_ms);
    Ok(FetchResult {
        content: extracted.content,
        title: extracted.title,
        url: resp.final_url,
        status_code: status,
        timing_ms,
    })
}

async fn cef_render_with_retry(
    cef: &CefRenderer,
    url: &str,
    wait_for_selector: Option<&str>,
) -> Result<String> {
    let opts = CefRenderOptions {
        wait_for_selector: wait_for_selector.map(|s| s.to_string()),
    };
    match cef.render(url, &opts).await {
        Ok(html) if is_acceptable_render(&html) => Ok(html),
        _ => cef.render(url, &opts).await,
    }
}

fn is_acceptable_render(html: &str) -> bool {
    html.len() >= 2000 && !looks_like_cloudflare_interstitial(html)
}

fn looks_like_cloudflare_interstitial(html: &str) -> bool {
    let lower = html.to_lowercase();
    lower.contains("just a moment")
        && (lower.contains("performing security verification")
            || lower.contains("checking your browser")
            || lower.contains("challenge-platform"))
}

pub async fn fetch_html(
    client: &TransportClient,
    cef: &CefRenderer,
    url: &str,
    respect_robots: bool,
) -> Result<FetchHtmlResult> {
    let start = Instant::now();
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL: {}", e))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;

    if host == "www.reddit.com" || host == "reddit.com" {
        let old_url = url
            .replace("://www.reddit.com", "://old.reddit.com")
            .replace("://reddit.com", "://old.reddit.com");
        return Box::pin(fetch_html(client, cef, &old_url, respect_robots)).await;
    }

    if respect_robots && !robots::check(client, url).await {
        return Ok(FetchHtmlResult {
            html: String::new(),
            url: url.to_string(),
            status_code: 0,
        });
    }

    let cef_installed = cef.is_available();
    let cached = site_cache::get(host);
    let rule = site_rules::get(host);
    let effective_selector = rule.as_ref().and_then(|r| r.wait_for_selector.as_deref());
    let cef_first = should_use_cef_first(
        rule.as_ref().map(|r| r.render.as_str()),
        cached.as_ref().map(|s| s.strategy.as_str()),
        cef_installed,
    );

    if cef_first {
        match cef_render_with_retry(cef, url, effective_selector).await {
            Ok(html) => {
                let timing_ms = start.elapsed().as_millis() as u64;
                site_cache::record(host, "cef", false, timing_ms);
                return Ok(FetchHtmlResult {
                    html,
                    url: url.to_string(),
                    status_code: 200,
                });
            }
            Err(_) => {
                let timing_ms = start.elapsed().as_millis() as u64;
                site_cache::record(host, "cef_timeout", false, timing_ms);
            }
        }
    }

    let resp = client.get(url).await?;
    let timing_ms = start.elapsed().as_millis() as u64;
    let blocked = matches!(resp.status, 403 | 503);

    if blocked && cef_installed && !cef_first {
        if let Ok(html) = cef_render_with_retry(cef, url, effective_selector).await {
            let timing_ms = start.elapsed().as_millis() as u64;
            site_cache::record(host, "cef", false, timing_ms);
            return Ok(FetchHtmlResult {
                html,
                url: url.to_string(),
                status_code: 200,
            });
        }
    }

    if !blocked && resp.status < 400 {
        site_cache::record(host, "cronet", false, timing_ms);
    }

    Ok(FetchHtmlResult {
        html: resp.text(),
        url: resp.final_url,
        status_code: resp.status,
    })
}

pub async fn fetch_raw(
    client: &TransportClient,
    url: &str,
    respect_robots: bool,
) -> Result<FetchRawResult> {
    let start = Instant::now();
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL: {}", e))?;

    match parsed.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("unsupported scheme {:?} (only http and https)", s),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing host"))?;

    if respect_robots && !robots::check(client, url).await {
        return Ok(FetchRawResult {
            bytes: Vec::new(),
            url: url.to_string(),
            status_code: 0,
            timing_ms: start.elapsed().as_millis() as u64,
        });
    }

    let resp = client.get(url).await?;
    let timing_ms = start.elapsed().as_millis() as u64;
    if resp.status >= 200 && resp.status < 400 {
        site_cache::record(host, "cronet", false, timing_ms);
    }

    Ok(FetchRawResult {
        bytes: resp.body,
        url: resp.final_url,
        status_code: resp.status,
        timing_ms,
    })
}

fn is_js_required_shell(body: &str) -> bool {
    if body.len() > 2_000_000 {
        return false;
    }
    let lower = body.to_lowercase();
    const SIGNALS: &[&str] = &[
        "javascript is not available",
        "we've detected that javascript is disabled",
        "we’ve detected that javascript is disabled",
        "you need to enable javascript to run this app",
        "please enable javascript to continue",
        "enable javascript and cookies to continue",
        "this site requires javascript",
        "enable javascript to see google maps",
        "when you have eliminated the javascript",
    ];
    SIGNALS.iter().any(|s| lower.contains(s))
}

fn should_use_cef_first(
    rule_render: Option<&str>,
    cached_strategy: Option<&str>,
    cef_installed: bool,
) -> bool {
    if !cef_installed {
        return false;
    }
    match rule_render {
        Some("cef") => true,
        Some("cronet") => false,
        _ => matches!(cached_strategy, Some("cef")),
    }
}
