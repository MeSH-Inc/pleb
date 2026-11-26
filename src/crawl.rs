use anyhow::Result;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};
use url::Url;

use crate::context::RuntimeContext;
use crate::extract::{
    self, url::normalize_crawl_url, ExtractMode, ExtractOptions, ExtractedDocument, OutputFormat,
    ParsedHtml,
};
use crate::fetch;

#[derive(Debug, Clone)]
pub struct CrawlOptions {
    pub max_depth: u32,
    pub max_pages: u32,
    pub format: OutputFormat,
    pub extract_mode: ExtractMode,
    pub respect_robots: bool,
    pub path_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrawlPage {
    pub url: String,
    pub title: Option<String>,
    pub content: String,
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrawlResult {
    pub pages: Vec<CrawlPage>,
    pub urls_discovered: usize,
    pub timing_ms: u64,
}

pub async fn crawl(
    ctx: &RuntimeContext,
    start_url: &str,
    options: CrawlOptions,
) -> Result<CrawlResult> {
    let start = Instant::now();
    let parsed = Url::parse(start_url)?;
    let origin = get_origin(&parsed);

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut pages: Vec<CrawlPage> = Vec::new();
    let mut urls_discovered: usize = 0;

    let normalized = normalize_url(start_url);
    visited.insert(normalized);
    queue.push_back((start_url.to_string(), 0));

    while let Some((url, depth)) = queue.pop_front() {
        if pages.len() >= options.max_pages as usize {
            break;
        }

        if !pages.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let html_result = match fetch::fetch_html(ctx, &url, options.respect_robots).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("crawl skip {}: {}", url, e);
                continue;
            }
        };

        if html_result.status_code >= 400 || html_result.html.is_empty() {
            continue;
        }

        let page_url = Url::parse(&html_result.url).unwrap_or_else(|_| parsed.clone());
        let is_html = extract::is_html_content_type(html_result.content_type.as_deref());
        let parsed_doc = if is_html {
            Some(
                html_result
                    .parsed
                    .unwrap_or_else(|| ParsedHtml::parse(&html_result.html)),
            )
        } else {
            None
        };

        if depth < options.max_depth {
            if let Some(ref doc) = parsed_doc {
                for link in doc.links(&page_url) {
                    let link_str = link.to_string();
                    let norm = normalize_crawl_url(&link);

                    if !is_same_origin(&link, &origin) {
                        continue;
                    }
                    if let Some(ref prefix) = options.path_filter {
                        if !link.path().starts_with(prefix.as_str()) {
                            continue;
                        }
                    }
                    if !is_crawlable_extension(&link) || visited.contains(&norm) {
                        continue;
                    }

                    visited.insert(norm);
                    urls_discovered += 1;
                    queue.push_back((link_str, depth + 1));
                }
            }
        }

        let opts = ExtractOptions {
            format: options.format,
            mode: options.extract_mode,
            max_bytes: Some(10_000),
            ..Default::default()
        };
        let extracted = extract::extract_document_parsed(
            &html_result.html,
            parsed_doc,
            &page_url,
            html_result.content_type.as_deref(),
            &opts,
        )
        .unwrap_or(ExtractedDocument {
            content: String::new(),
            title: None,
            truncated: false,
        });

        pages.push(CrawlPage {
            url: html_result.url,
            title: extracted.title,
            content: extracted.content,
            depth,
        });
    }

    Ok(CrawlResult {
        pages,
        urls_discovered,
        timing_ms: start.elapsed().as_millis() as u64,
    })
}

pub fn format_crawl_output(result: &CrawlResult, host: &str) -> String {
    let mut out = format!(
        "# Crawl Results: {}\nCrawled {} pages in {:.1}s\n",
        host,
        result.pages.len(),
        result.timing_ms as f64 / 1000.0
    );

    let mut total_len = out.len();
    for (i, page) in result.pages.iter().enumerate() {
        let title = page.title.as_deref().unwrap_or("Untitled");
        let section = format!(
            "\n---\n## [{}/{}] {}\n**URL:** {}\n\n{}\n",
            i + 1,
            result.pages.len(),
            title,
            page.url,
            page.content,
        );

        if total_len + section.len() > 100_000 {
            out.push_str(&format!(
                "\n---\n*Output truncated. {} more pages not shown.*\n",
                result.pages.len() - i
            ));
            break;
        }
        total_len += section.len();
        out.push_str(&section);
    }

    out
}

#[derive(Debug, Clone)]
pub struct MapOptions {
    pub limit: u32,
    pub use_sitemap: bool,
    pub respect_robots: bool,
    pub path_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MapResult {
    pub urls: Vec<String>,
    pub timing_ms: u64,
    pub from_sitemap: usize,
}

pub async fn map(
    ctx: &RuntimeContext,
    start_url: &str,
    options: MapOptions,
) -> Result<MapResult> {
    let start = Instant::now();
    let parsed = Url::parse(start_url)?;
    let origin = get_origin(&parsed);
    let limit = options.limit as usize;

    let mut discovered: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut from_sitemap = 0;

    if options.use_sitemap {
        for url in fetch_sitemap(ctx, &parsed).await {
            let norm = normalize_url(&url);
            if !visited.contains(&norm) {
                if let Some(ref prefix) = options.path_filter {
                    if let Ok(u) = Url::parse(&url) {
                        if !u.path().starts_with(prefix.as_str()) {
                            continue;
                        }
                    }
                }
                visited.insert(norm);
                discovered.push(url);
                from_sitemap += 1;
                if discovered.len() >= limit {
                    break;
                }
            }
        }
    }

    let mut queue: VecDeque<String> = VecDeque::new();
    let seed_norm = normalize_url(start_url);
    if !visited.contains(&seed_norm) {
        visited.insert(seed_norm);
        discovered.push(start_url.to_string());
    }
    queue.push_back(start_url.to_string());

    let mut pages_fetched = 0;
    let max_fetch = 20.min(limit);

    while let Some(url) = queue.pop_front() {
        if discovered.len() >= limit || pages_fetched >= max_fetch {
            break;
        }

        if pages_fetched > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }

        let html_result = match fetch::fetch_html(ctx, &url, options.respect_robots).await {
            Ok(r) if r.status_code < 400 && !r.html.is_empty() => r,
            _ => continue,
        };
        pages_fetched += 1;

        if !extract::is_html_content_type(html_result.content_type.as_deref()) {
            continue;
        }

        let page_url = Url::parse(&html_result.url).unwrap_or_else(|_| parsed.clone());
        let doc = html_result
            .parsed
            .unwrap_or_else(|| ParsedHtml::parse(&html_result.html));
        for link in doc.links(&page_url) {
            let link_str = link.to_string();
            let norm = normalize_crawl_url(&link);

            if !is_same_origin(&link, &origin) {
                continue;
            }
            if let Some(ref prefix) = options.path_filter {
                if !link.path().starts_with(prefix.as_str()) {
                    continue;
                }
            }
            if !is_crawlable_extension(&link) || visited.contains(&norm) {
                continue;
            }

            visited.insert(norm);
            discovered.push(link_str.clone());
            queue.push_back(link_str);

            if discovered.len() >= limit {
                break;
            }
        }
    }

    discovered.sort();

    Ok(MapResult {
        urls: discovered,
        timing_ms: start.elapsed().as_millis() as u64,
        from_sitemap,
    })
}

pub fn format_map_output(result: &MapResult, host: &str) -> String {
    let mut out = format!("# Site Map: {}\nFound {} URLs", host, result.urls.len());
    if result.from_sitemap > 0 {
        out.push_str(&format!(" ({} from sitemap.xml)", result.from_sitemap));
    }
    out.push_str("\n\n");

    for url in &result.urls {
        out.push_str(&format!("- {}\n", url));
    }
    out
}

async fn fetch_sitemap(ctx: &RuntimeContext, base_url: &Url) -> Vec<String> {
    let origin = base_url.origin().ascii_serialization();
    if origin == "null" {
        return Vec::new();
    }
    let sitemap_url = format!("{}/sitemap.xml", origin);

    let resp = match ctx.client.get(&sitemap_url).await {
        Ok(r) if r.status < 400 => r,
        _ => return Vec::new(),
    };

    parse_sitemap_xml(&resp.text())
}

fn parse_sitemap_xml(xml: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let mut search_from = 0;

    while let Some(start_tag) = lower[search_from..].find("<loc>") {
        let content_start = search_from + start_tag + 5;
        if let Some(end_offset) = lower[content_start..].find("</loc>") {
            let content_end = content_start + end_offset;
            let url = xml[content_start..content_end].trim();
            if url.starts_with("http") && !url.ends_with(".xml") && !url.ends_with(".xml.gz") {
                urls.push(url.to_string());
            }
            search_from = content_end + 6;
        } else {
            break;
        }
    }

    urls
}

fn normalize_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => normalize_crawl_url(&u),
        Err(_) => url.to_string(),
    }
}

fn get_origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn is_same_origin(url: &Url, origin: &str) -> bool {
    url.origin().ascii_serialization() == origin
}

const SKIP_EXTENSIONS: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".ico", ".bmp", ".pdf", ".zip", ".tar",
    ".gz", ".rar", ".css", ".js", ".woff", ".woff2", ".ttf", ".eot", ".mp3", ".mp4", ".avi",
    ".mov", ".wmv", ".flv", ".xml", ".json", ".rss", ".atom",
];

fn is_crawlable_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    !SKIP_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}
