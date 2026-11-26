use anyhow::Result;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

use crate::context::RuntimeContext;

use super::state::{exa_start_published_date, now_epoch_secs, parse_retry_after};
use super::{
    build_effective_query, normalize_site, ConcreteProvider, Freshness, ProviderEndpoints,
    SearchOptions, SearchResult,
};

#[derive(Debug, Clone)]
pub(crate) struct ProviderFailure {
    pub status: Option<u16>,
    pub cooldown_secs: u64,
    pub message: String,
}

impl ProviderFailure {
    fn new(status: Option<u16>, cooldown_secs: u64, message: String) -> Self {
        Self {
            status,
            cooldown_secs,
            message,
        }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderFailure {}

pub(crate) async fn execute_provider_request(
    provider: ConcreteProvider,
    request: reqwest::RequestBuilder,
    now_secs: u64,
) -> Result<String, ProviderFailure> {
    let resp = request.send().await.map_err(|err| {
        ProviderFailure::new(
            None,
            60,
            format!(
                "{} search request failed: {}",
                provider.as_str(),
                err.without_url()
            ),
        )
    })?;

    let status = resp.status();
    let status_code = status.as_u16();
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());

    if status.is_success() {
        return resp.text().await.map_err(|err| {
            ProviderFailure::new(
                Some(status_code),
                60,
                format!(
                    "{} search body read failed: {}",
                    provider.as_str(),
                    err.without_url()
                ),
            )
        });
    }

    Err(classify_http_failure(
        provider,
        status_code,
        retry_after.as_deref(),
        content_type.as_deref(),
        now_secs,
    ))
}

fn classify_http_failure(
    provider: ConcreteProvider,
    status_code: u16,
    retry_after: Option<&str>,
    content_type: Option<&str>,
    now_secs: u64,
) -> ProviderFailure {
    match status_code {
        429 => {
            let cooldown_secs = retry_after
                .and_then(|raw| parse_retry_after(raw, now_secs))
                .unwrap_or(15);
            ProviderFailure::new(
                Some(429),
                cooldown_secs,
                format!("{} search rate-limited (HTTP 429)", provider.as_str()),
            )
        }
        401 | 402 | 403 | 432 | 433 => {
            if provider == ConcreteProvider::Searxng {
                ProviderFailure::new(
                    Some(status_code),
                    300,
                    format!(
                        "searxng search rejected (HTTP {status_code}; verify format=json is enabled)"
                    ),
                )
            } else if status_code == 403 && content_type.is_some_and(|ct| !ct.contains("json")) {
                ProviderFailure::new(
                    Some(403),
                    300,
                    format!("{} search blocked by WAF (HTTP 403)", provider.as_str()),
                )
            } else {
                ProviderFailure::new(
                    Some(status_code),
                    86_400,
                    format!(
                        "{} search authentication/quota failed (HTTP {status_code})",
                        provider.as_str()
                    ),
                )
            }
        }
        500..=599 | 408 => ProviderFailure::new(
            Some(status_code),
            60,
            format!("{} search failed (HTTP {status_code})", provider.as_str()),
        ),
        400 | 404 | 422 => ProviderFailure::new(
            Some(status_code),
            0,
            format!(
                "{} search rejected query (HTTP {status_code})",
                provider.as_str()
            ),
        ),
        _ => ProviderFailure::new(
            Some(status_code),
            0,
            format!("{} search failed (HTTP {status_code})", provider.as_str()),
        ),
    }
}

/// Splits normalized `--site` into `(host_only, has_path)`.
fn split_site_host(site: Option<&str>) -> Option<(String, bool)> {
    let normalized = normalize_site(site?)?;
    match normalized.split_once('/') {
        Some((host, _path)) if !host.is_empty() => Some((host.to_string(), true)),
        _ => Some((normalized, false)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_json_provider(
    client: &reqwest::Client,
    timeout: Duration,
    provider: ConcreteProvider,
    secret: &str,
    raw_query: &str,
    limit: usize,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let now_secs = now_epoch_secs();
    let effective_query = build_effective_query(raw_query, options.site.as_deref());

    match provider {
        ConcreteProvider::Brave => {
            search_brave(
                client,
                timeout,
                &endpoints.brave_url,
                &effective_query,
                limit,
                secret,
                options,
                now_secs,
            )
            .await
        }
        ConcreteProvider::Tavily => {
            search_tavily(
                client,
                timeout,
                &endpoints.tavily_url,
                raw_query,
                &effective_query,
                limit,
                secret,
                options,
                now_secs,
            )
            .await
        }
        ConcreteProvider::Serper => {
            search_serper(
                client,
                timeout,
                &endpoints.serper_url,
                &effective_query,
                limit,
                secret,
                options,
                now_secs,
            )
            .await
        }
        ConcreteProvider::Exa => {
            search_exa(
                client,
                timeout,
                &endpoints.exa_url,
                raw_query,
                &effective_query,
                limit,
                secret,
                options,
                now_secs,
            )
            .await
        }
        ConcreteProvider::Searxng => {
            search_searxng(
                client,
                timeout,
                secret,
                &effective_query,
                limit,
                options.freshness,
                now_secs,
            )
            .await
        }
        ConcreteProvider::DuckDuckGo => Err(ProviderFailure::new(
            None,
            0,
            "duckduckgo must be dispatched via TransportClient".to_string(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn search_brave(
    client: &reqwest::Client,
    timeout: Duration,
    endpoint: &str,
    effective_query: &str,
    limit: usize,
    api_key: &str,
    options: &SearchOptions,
    now_secs: u64,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let encoded = urlencoding::encode(effective_query);
    let mut url = format!("{endpoint}?count={limit}&q={encoded}");
    if let Some(f) = options.freshness {
        url.push_str("&freshness=");
        url.push_str(f.brave_param());
    }
    if options.extra_snippets {
        url.push_str("&extra_snippets=true");
    }

    let req = client
        .get(&url)
        .timeout(timeout)
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json");
    let body = execute_provider_request(ConcreteProvider::Brave, req, now_secs).await?;
    parse_brave_json(&body, limit).map_err(|err| {
        ProviderFailure::new(
            Some(200),
            60,
            format!("brave search response parse failed: {err}"),
        )
    })
}

#[derive(Serialize)]
struct TavilyRequest<'a> {
    query: &'a str,
    max_results: usize,
    search_depth: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks_per_source: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_range: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_domains: Option<Vec<String>>,
}

#[allow(clippy::too_many_arguments)]
async fn search_tavily(
    client: &reqwest::Client,
    timeout: Duration,
    endpoint: &str,
    raw_query: &str,
    effective_query: &str,
    limit: usize,
    api_key: &str,
    options: &SearchOptions,
    now_secs: u64,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let site_split = split_site_host(options.site.as_deref());
    let (query_text, include_domains) = match site_split {
        Some((host, true)) => (effective_query, Some(vec![host])),
        Some((host, false)) => (raw_query.trim(), Some(vec![host])),
        None => (raw_query.trim(), None),
    };

    let payload = TavilyRequest {
        query: query_text,
        max_results: limit,
        search_depth: if options.extra_snippets {
            "advanced"
        } else {
            "basic"
        },
        chunks_per_source: options.extra_snippets.then_some(3),
        time_range: options.freshness.map(Freshness::time_range_param),
        include_domains,
    };

    let req = client
        .post(endpoint)
        .timeout(timeout)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json")
        .json(&payload);
    let body = execute_provider_request(ConcreteProvider::Tavily, req, now_secs).await?;
    parse_tavily_json(&body, limit, options.extra_snippets).map_err(|err| {
        ProviderFailure::new(
            Some(200),
            60,
            format!("tavily search response parse failed: {err}"),
        )
    })
}

#[derive(Serialize)]
struct SerperRequest<'a> {
    q: &'a str,
    num: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    tbs: Option<&'static str>,
}

#[allow(clippy::too_many_arguments)]
async fn search_serper(
    client: &reqwest::Client,
    timeout: Duration,
    endpoint: &str,
    effective_query: &str,
    limit: usize,
    api_key: &str,
    options: &SearchOptions,
    now_secs: u64,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let payload = SerperRequest {
        q: effective_query,
        num: limit,
        tbs: options.freshness.map(Freshness::serper_tbs),
    };

    let req = client
        .post(endpoint)
        .timeout(timeout)
        .header("X-API-KEY", api_key)
        .header("Accept", "application/json")
        .json(&payload);
    let body = execute_provider_request(ConcreteProvider::Serper, req, now_secs).await?;
    parse_serper_json(&body, limit, options.extra_snippets).map_err(|err| {
        ProviderFailure::new(
            Some(200),
            60,
            format!("serper search response parse failed: {err}"),
        )
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaHighlightsConfig {
    max_characters: usize,
}

#[derive(Serialize)]
struct ExaContentsConfig {
    highlights: ExaHighlightsConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExaRequest<'a> {
    query: &'a str,
    num_results: usize,
    contents: ExaContentsConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_domains: Option<Vec<String>>,
}

#[allow(clippy::too_many_arguments)]
async fn search_exa(
    client: &reqwest::Client,
    timeout: Duration,
    endpoint: &str,
    raw_query: &str,
    effective_query: &str,
    limit: usize,
    api_key: &str,
    options: &SearchOptions,
    now_secs: u64,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let site_split = split_site_host(options.site.as_deref());
    let (query_text, include_domains) = match site_split {
        Some((host, true)) => (effective_query, Some(vec![host])),
        Some((host, false)) => (raw_query.trim(), Some(vec![host])),
        None => (raw_query.trim(), None),
    };

    let payload = ExaRequest {
        query: query_text,
        num_results: limit,
        contents: ExaContentsConfig {
            highlights: ExaHighlightsConfig {
                max_characters: if options.extra_snippets { 1200 } else { 400 },
            },
        },
        start_published_date: options
            .freshness
            .map(|f| exa_start_published_date(f, now_secs)),
        include_domains,
    };

    let req = client
        .post(endpoint)
        .timeout(timeout)
        .header("x-api-key", api_key)
        .header("Accept", "application/json")
        .json(&payload);
    let body = execute_provider_request(ConcreteProvider::Exa, req, now_secs).await?;
    parse_exa_json(&body, limit, options.extra_snippets).map_err(|err| {
        ProviderFailure::new(
            Some(200),
            60,
            format!("exa search response parse failed: {err}"),
        )
    })
}

pub(crate) fn build_searxng_search_url(
    base_url: &str,
    query: &str,
    freshness: Option<Freshness>,
) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let root = trimmed.strip_suffix("/search").unwrap_or(trimmed);
    let encoded = urlencoding::encode(query);
    let mut url = format!("{root}/search?format=json&q={encoded}&pageno=1");
    if let Some(f) = freshness {
        url.push_str("&time_range=");
        url.push_str(f.time_range_param());
    }
    url
}

async fn search_searxng(
    client: &reqwest::Client,
    timeout: Duration,
    base_url: &str,
    effective_query: &str,
    limit: usize,
    freshness: Option<Freshness>,
    now_secs: u64,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let url = build_searxng_search_url(base_url, effective_query, freshness);
    let req = client
        .get(&url)
        .timeout(timeout)
        .header("Accept", "application/json");
    let body = execute_provider_request(ConcreteProvider::Searxng, req, now_secs).await?;
    parse_searxng_json(&body, limit).map_err(|err| {
        ProviderFailure::new(
            Some(200),
            60,
            format!("searxng search response parse failed: {err}"),
        )
    })
}

pub(crate) async fn search_ddg(
    ctx: &RuntimeContext,
    endpoint: &str,
    effective_query: &str,
    limit: usize,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    let encoded = urlencoding::encode(effective_query);
    let mut search_url = format!("{endpoint}?q={encoded}");
    if let Some(f) = freshness {
        search_url.push_str("&df=");
        search_url.push_str(f.ddg_param());
    }
    let resp = ctx.client.get(&search_url).await.map_err(|err| {
        ProviderFailure::new(None, 60, format!("duckduckgo search request failed: {err}"))
    })?;

    if resp.status == 202 || resp.status == 429 {
        return Err(ProviderFailure::new(
            Some(resp.status),
            15,
            format!("duckduckgo search rate-limited (HTTP {})", resp.status),
        ));
    }
    if resp.status >= 400 {
        let cooldown_secs = if matches!(resp.status, 400 | 404 | 422) {
            0
        } else {
            60
        };
        return Err(ProviderFailure::new(
            Some(resp.status),
            cooldown_secs,
            format!("duckduckgo search failed (HTTP {})", resp.status),
        ));
    }

    let body = resp.text();
    let results = parse_ddg_html(&body, limit).map_err(|err| {
        ProviderFailure::new(
            Some(resp.status),
            60,
            format!("duckduckgo search parse failed: {err}"),
        )
    })?;

    if results.is_empty() && is_ddg_challenge_body(&body) {
        return Err(ProviderFailure::new(
            Some(resp.status),
            15,
            "duckduckgo search blocked by anomaly challenge".to_string(),
        ));
    }

    Ok(results)
}

fn is_ddg_challenge_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("anomaly-modal")
        || lower.contains("challenge-form")
        || lower.contains("bots use duckduckgo")
}

// --- Response parsers ---

#[derive(Deserialize)]
struct BraveApiPayload {
    web: Option<BraveWebBlock>,
}

#[derive(Deserialize)]
struct BraveWebBlock {
    #[serde(default)]
    results: Vec<BraveWebResult>,
}

#[derive(Deserialize)]
struct BraveWebResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    extra_snippets: Vec<String>,
}

pub fn parse_brave_json(json_str: &str, max: usize) -> Result<Vec<SearchResult>> {
    let payload: BraveApiPayload = serde_json::from_str(json_str)?;
    let raw_results = payload.web.map(|w| w.results).unwrap_or_default();

    let results = raw_results
        .into_iter()
        .filter_map(|r| {
            let title = clean_html_text(&r.title);
            let url = r.url.trim().to_string();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            let extra_snippets = r
                .extra_snippets
                .into_iter()
                .map(|s| clean_html_text(&s))
                .filter(|s| !s.is_empty())
                .collect();
            Some(SearchResult {
                title,
                url,
                snippet: clean_html_text(&r.description),
                extra_snippets,
            })
        })
        .take(max)
        .collect();

    Ok(results)
}

#[derive(Deserialize)]
struct TavilyPayload {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

pub(crate) fn parse_tavily_json(
    json_str: &str,
    max: usize,
    extra_snippets: bool,
) -> Result<Vec<SearchResult>> {
    let payload: TavilyPayload = serde_json::from_str(json_str)?;
    let results = payload
        .results
        .into_iter()
        .filter_map(|r| {
            let title = clean_html_text(&r.title);
            let url = r.url.trim().to_string();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            let (snippet, extras) = if extra_snippets && r.content.contains(" [...] ") {
                let mut chunks: Vec<String> = r
                    .content
                    .split(" [...] ")
                    .map(clean_html_text)
                    .filter(|s| !s.is_empty())
                    .collect();
                if chunks.is_empty() {
                    (String::new(), Vec::new())
                } else {
                    let first = chunks.remove(0);
                    (first, chunks)
                }
            } else {
                (clean_html_text(&r.content), Vec::new())
            };
            Some(SearchResult {
                title,
                url,
                snippet,
                extra_snippets: extras,
            })
        })
        .take(max)
        .collect();
    Ok(results)
}

#[derive(Deserialize)]
struct SerperPayload {
    #[serde(default)]
    organic: Vec<SerperOrganicResult>,
}

#[derive(Deserialize)]
struct SerperOrganicResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    sitelinks: Vec<SerperSitelink>,
}

#[derive(Deserialize)]
struct SerperSitelink {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    snippet: String,
}

pub(crate) fn parse_serper_json(
    json_str: &str,
    max: usize,
    extra_snippets: bool,
) -> Result<Vec<SearchResult>> {
    let payload: SerperPayload = serde_json::from_str(json_str)?;
    let results = payload
        .organic
        .into_iter()
        .filter_map(|r| {
            let title = clean_html_text(&r.title);
            let url = r.link.trim().to_string();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            let extras = if extra_snippets {
                r.sitelinks
                    .into_iter()
                    .filter_map(|sl| {
                        let sl_title = clean_html_text(&sl.title);
                        if sl_title.is_empty() {
                            return None;
                        }
                        let sl_snippet = clean_html_text(&sl.snippet);
                        let sl_link = sl.link.trim();
                        if !sl_snippet.is_empty() {
                            Some(format!("{sl_title}: {sl_snippet}"))
                        } else if !sl_link.is_empty() {
                            Some(format!("{sl_title} ({sl_link})"))
                        } else {
                            Some(sl_title)
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Some(SearchResult {
                title,
                url,
                snippet: clean_html_text(&r.snippet),
                extra_snippets: extras,
            })
        })
        .take(max)
        .collect();
    Ok(results)
}

#[derive(Deserialize)]
struct ExaPayload {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    highlights: Vec<String>,
    #[serde(default)]
    text: String,
}

pub(crate) fn parse_exa_json(
    json_str: &str,
    max: usize,
    extra_snippets: bool,
) -> Result<Vec<SearchResult>> {
    let payload: ExaPayload = serde_json::from_str(json_str)?;
    let results = payload
        .results
        .into_iter()
        .filter_map(|r| {
            let url = r.url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let cleaned_title = clean_html_text(&r.title);
            let title = if cleaned_title.is_empty() {
                url.clone()
            } else {
                cleaned_title
            };
            let cleaned_highlights: Vec<String> = r
                .highlights
                .into_iter()
                .map(|h| clean_html_text(&h))
                .filter(|h| !h.is_empty())
                .collect();
            let snippet = cleaned_highlights
                .first()
                .cloned()
                .unwrap_or_else(|| clean_html_text(&r.text));
            let extras = if extra_snippets && cleaned_highlights.len() > 1 {
                cleaned_highlights[1..].to_vec()
            } else {
                Vec::new()
            };
            Some(SearchResult {
                title,
                url,
                snippet,
                extra_snippets: extras,
            })
        })
        .take(max)
        .collect();
    Ok(results)
}

#[derive(Deserialize)]
struct SearxngPayload {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

pub(crate) fn parse_searxng_json(json_str: &str, max: usize) -> Result<Vec<SearchResult>> {
    let payload: SearxngPayload = serde_json::from_str(json_str)?;
    let results = payload
        .results
        .into_iter()
        .filter_map(|r| {
            let title = clean_html_text(&r.title);
            let url = r.url.trim().to_string();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            Some(SearchResult {
                title,
                url,
                snippet: clean_html_text(&r.content),
                extra_snippets: Vec::new(),
            })
        })
        .take(max)
        .collect();
    Ok(results)
}

pub(crate) fn clean_html_text(input: &str) -> String {
    let fragment = Html::parse_fragment(input);
    let raw: String = fragment.root_element().text().collect();
    collapse_whitespace(&raw)
}

pub(crate) fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

static DDG_RESULT_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".result__body, .web-result__body").unwrap());
static DDG_TITLE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".result__a, .result__title a").unwrap());
static DDG_SNIPPET_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".result__snippet").unwrap());

pub fn parse_ddg_html(html: &str, max: usize) -> Result<Vec<SearchResult>> {
    let doc = Html::parse_document(html);

    let mut results = Vec::new();

    for element in doc.select(&DDG_RESULT_SEL) {
        if results.len() >= max {
            break;
        }

        let Some(title_el) = element.select(&DDG_TITLE_SEL).next() else {
            continue;
        };
        let title = collapse_whitespace(&title_el.text().collect::<String>());
        let raw_href = title_el.value().attr("href").unwrap_or("").trim();
        let url = clean_ddg_url(raw_href);

        if title.is_empty() || url.is_empty() || is_internal_ddg_url(&url) {
            continue;
        }

        let snippet = element
            .select(&DDG_SNIPPET_SEL)
            .next()
            .map(|el| collapse_whitespace(&el.text().collect::<String>()))
            .unwrap_or_default();

        results.push(SearchResult {
            title,
            url,
            snippet,
            extra_snippets: Vec::new(),
        });
    }

    Ok(results)
}

pub fn clean_ddg_url(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let encoded = &href[pos + 5..];
        let end = encoded.find('&').unwrap_or(encoded.len());
        if let Ok(decoded) = urlencoding::decode(&encoded[..end]) {
            return decoded.into_owned();
        }
    }
    href.to_string()
}

fn is_internal_ddg_url(url: &str) -> bool {
    if url.starts_with('/') {
        return true;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return true;
    };
    let Some(host) = parsed.host_str() else {
        return true;
    };
    host.eq_ignore_ascii_case("duckduckgo.com") || host.ends_with(".duckduckgo.com")
}
