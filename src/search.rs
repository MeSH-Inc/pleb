use anyhow::{bail, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::context::RuntimeContext;

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SearchProvider {
    #[default]
    Auto,
    Brave,
    #[value(name = "duckduckgo", alias = "ddg")]
    DuckDuckGo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    pub fn brave_param(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    pub fn ddg_param(self) -> &'static str {
        match self {
            Self::Day => "d",
            Self::Week => "w",
            Self::Month => "m",
            Self::Year => "y",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: usize,
    pub provider: SearchProvider,
    pub freshness: Option<Freshness>,
    pub site: Option<String>,
    pub extra_snippets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_snippets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub provider: &'static str,
    pub results: Vec<SearchResult>,
}

pub fn brave_api_key_configured() -> bool {
    load_brave_api_key().is_some()
}

fn load_brave_api_key() -> Option<String> {
    std::env::var("BRAVE_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn normalize_site(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") {
        s = &s[8..];
    } else if lower.starts_with("http://") {
        s = &s[7..];
    }
    let cleaned = s.trim_matches('/').trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

pub fn build_effective_query(query: &str, site: Option<&str>) -> String {
    match site.and_then(normalize_site) {
        Some(domain) => format!("site:{domain} {}", query.trim()),
        None => query.to_string(),
    }
}

pub fn build_brave_search_url(
    query: &str,
    num_results: usize,
    freshness: Option<Freshness>,
    extra_snippets: bool,
) -> String {
    let encoded = urlencoding::encode(query);
    let mut url = format!("{BRAVE_SEARCH_ENDPOINT}?count={num_results}&q={encoded}");
    if let Some(f) = freshness {
        url.push_str("&freshness=");
        url.push_str(f.brave_param());
    }
    if extra_snippets {
        url.push_str("&extra_snippets=true");
    }
    url
}

pub fn build_ddg_search_url(query: &str, freshness: Option<Freshness>) -> String {
    let encoded = urlencoding::encode(query);
    let mut url = format!("https://html.duckduckgo.com/html/?q={encoded}");
    if let Some(f) = freshness {
        url.push_str("&df=");
        url.push_str(f.ddg_param());
    }
    url
}

pub async fn search(
    ctx: &RuntimeContext,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse> {
    let count = options.limit.clamp(1, 20);
    let effective_query = build_effective_query(query, options.site.as_deref());

    match options.provider {
        SearchProvider::Brave => {
            let key =
                load_brave_api_key().ok_or_else(|| anyhow::anyhow!("BRAVE_API_KEY is not set"))?;
            let results = search_brave(ctx, &effective_query, count, &key, options).await?;
            Ok(SearchResponse {
                provider: "brave",
                results,
            })
        }
        SearchProvider::DuckDuckGo => {
            let results = search_ddg(ctx, &effective_query, count, options.freshness).await?;
            Ok(SearchResponse {
                provider: "duckduckgo",
                results,
            })
        }
        SearchProvider::Auto => {
            if let Some(key) = load_brave_api_key() {
                match search_brave(ctx, &effective_query, count, &key, options).await {
                    Ok(results) => {
                        return Ok(SearchResponse {
                            provider: "brave",
                            results,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Brave Search API request failed ({}); falling back to DuckDuckGo",
                            err
                        );
                    }
                }
            }
            let results = search_ddg(ctx, &effective_query, count, options.freshness).await?;
            Ok(SearchResponse {
                provider: "duckduckgo",
                results,
            })
        }
    }
}

async fn search_brave(
    ctx: &RuntimeContext,
    query: &str,
    num_results: usize,
    api_key: &str,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    let client = ctx.brave_client()?;
    let url = build_brave_search_url(
        query,
        num_results,
        options.freshness,
        options.extra_snippets,
    );

    let resp = client
        .get(&url)
        .timeout(ctx.config.timeout)
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?;

    let body = resp.text().await?;
    parse_brave_json(&body, num_results)
}

async fn search_ddg(
    ctx: &RuntimeContext,
    query: &str,
    num_results: usize,
    freshness: Option<Freshness>,
) -> Result<Vec<SearchResult>> {
    let search_url = build_ddg_search_url(query, freshness);
    let resp = ctx.client.get(&search_url).await?;
    if resp.status >= 400 {
        bail!("DuckDuckGo search failed: HTTP {}", resp.status);
    }

    parse_ddg_html(&resp.text(), num_results)
}

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

fn clean_html_text(input: &str) -> String {
    let fragment = Html::parse_fragment(input);
    let raw: String = fragment.root_element().text().collect();
    collapse_whitespace(&raw)
}

fn collapse_whitespace(input: &str) -> String {
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

pub fn format_urls_only(response: &SearchResponse) -> String {
    response
        .results
        .iter()
        .map(|r| r.url.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_results(response: &SearchResponse) -> String {
    if response.results.is_empty() {
        return "No results found.".to_string();
    }

    let mut out = String::new();
    for (i, r) in response.results.iter().enumerate() {
        out.push_str(&format!("{}. [{}]({})\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        for extra in &r.extra_snippets {
            out.push_str(&format!("   > {}\n", extra));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}
