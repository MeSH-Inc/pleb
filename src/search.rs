use anyhow::{bail, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::Duration;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
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

pub async fn search(
    ctx: &RuntimeContext,
    query: &str,
    num_results: usize,
    provider: SearchProvider,
) -> Result<SearchResponse> {
    let count = num_results.clamp(1, 20);

    match provider {
        SearchProvider::Brave => {
            let key = load_brave_api_key()
                .ok_or_else(|| anyhow::anyhow!("BRAVE_API_KEY is not set"))?;
            let results = search_brave(
                query,
                count,
                &key,
                ctx.config.proxy.as_deref(),
                ctx.config.timeout,
            )
            .await?;
            Ok(SearchResponse {
                provider: "brave",
                results,
            })
        }
        SearchProvider::DuckDuckGo => {
            let results = search_ddg(ctx, query, count).await?;
            Ok(SearchResponse {
                provider: "duckduckgo",
                results,
            })
        }
        SearchProvider::Auto => {
            if let Some(key) = load_brave_api_key() {
                match search_brave(
                    query,
                    count,
                    &key,
                    ctx.config.proxy.as_deref(),
                    ctx.config.timeout,
                )
                .await
                {
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
            let results = search_ddg(ctx, query, count).await?;
            Ok(SearchResponse {
                provider: "duckduckgo",
                results,
            })
        }
    }
}

async fn search_brave(
    query: &str,
    num_results: usize,
    api_key: &str,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<Vec<SearchResult>> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(proxy_url) = proxy {
        let trimmed = proxy_url.trim();
        if !trimmed.is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(trimmed)?);
        }
    }
    let client = builder.build()?;
    let encoded = urlencoding::encode(query);
    let url = format!("{BRAVE_SEARCH_ENDPOINT}?count={num_results}&q={encoded}");

    let resp = client
        .get(&url)
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
) -> Result<Vec<SearchResult>> {
    let encoded = urlencoding::encode(query);
    let search_url = format!("https://html.duckduckgo.com/html/?q={}", encoded);

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
            Some(SearchResult {
                title,
                url,
                snippet: clean_html_text(&r.description),
            })
        })
        .take(max)
        .collect();

    Ok(results)
}

fn clean_html_text(input: &str) -> String {
    let fragment = Html::parse_fragment(input);
    let raw: String = fragment.root_element().text().collect();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
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

        let title = element
            .select(&DDG_TITLE_SEL)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let url = element
            .select(&DDG_TITLE_SEL)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(clean_ddg_url)
            .unwrap_or_default();

        let snippet = element
            .select(&DDG_SNIPPET_SEL)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
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

pub fn format_results(response: &SearchResponse) -> String {
    if response.results.is_empty() {
        return "No results found.".to_string();
    }

    let mut out = String::new();
    for (i, r) in response.results.iter().enumerate() {
        out.push_str(&format!(
            "{}. [{}]({})\n   {}\n\n",
            i + 1,
            r.title,
            r.url,
            r.snippet
        ));
    }
    out.trim_end().to_string()
}
