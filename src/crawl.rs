use anyhow::Result;
use flate2::read::GzDecoder;
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};
use url::Url;

use crate::context::RuntimeContext;
use crate::extract::{
    self, url::normalize_crawl_url, ExtractMode, ExtractOptions, OutputFormat, ParsedHtml,
};
use crate::fetch::{self, ContentOutcome, FetchError};

const DEFAULT_CRAWL_CONCURRENCY: usize = 4;
const DEFAULT_POLITE_SPACING: Duration = Duration::from_millis(100);
const MAX_SITEMAP_DEPTH: usize = 3;
const MAX_SITEMAPS: usize = 64;
const MAX_SITEMAP_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

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
    pub status_code: u16,
    pub strategy: String,
    pub content_outcome: ContentOutcome,
    pub truncated: bool,
    pub acquired_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrawlFailure {
    pub url: String,
    pub depth: u32,
    pub error: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrawlResult {
    pub pages: Vec<CrawlPage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_pages: Vec<CrawlFailure>,
    pub urls_discovered: usize,
    pub timing_ms: u64,
}

struct FetchedCrawlPage {
    depth: u32,
    discovery_sequence: usize,
    page: CrawlPage,
    links: Vec<Url>,
}

enum CrawlAttempt {
    Page(FetchedCrawlPage),
    Skipped,
    Failed {
        discovery_sequence: usize,
        failure: CrawlFailure,
    },
}

pub async fn crawl(
    ctx: &RuntimeContext,
    start_url: &str,
    options: CrawlOptions,
) -> Result<CrawlResult> {
    let start = Instant::now();
    let parsed = Url::parse(start_url)?;
    let max_pages = options.max_pages as usize;
    if max_pages == 0 {
        return Ok(CrawlResult {
            pages: Vec::new(),
            failed_pages: Vec::new(),
            urls_discovered: 0,
            timing_ms: start.elapsed().as_millis() as u64,
        });
    }

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(normalize_url(start_url));

    let Some(seed_fetched) = fetch_and_extract_page(
        ctx,
        start_url.to_string(),
        0,
        0,
        &parsed,
        &options,
        0 < options.max_depth,
    )
    .await
    .map_err(anyhow::Error::new)?
    else {
        return Ok(CrawlResult {
            pages: Vec::new(),
            failed_pages: Vec::new(),
            urls_discovered: 0,
            timing_ms: start.elapsed().as_millis() as u64,
        });
    };

    // Update origin and visited from the seed page's post-redirect final URL so
    // http -> https or apex -> www redirects do not drop all depth-0 links.
    let seed_final_url = Url::parse(&seed_fetched.page.url).unwrap_or_else(|_| parsed.clone());
    let origin = get_origin(&seed_final_url);
    visited.insert(normalize_crawl_url(&seed_final_url));

    let polite_spacing = if options.respect_robots {
        ctx.robots
            .crawl_delay(&ctx.client, seed_final_url.as_str())
            .await
            .unwrap_or(DEFAULT_POLITE_SPACING)
    } else {
        DEFAULT_POLITE_SPACING
    };

    let mut collected: Vec<(u32, usize, CrawlPage)> = vec![(0, 0, seed_fetched.page)];
    let mut failed: Vec<(u32, usize, CrawlFailure)> = Vec::new();
    let mut urls_discovered: usize = 0;
    let mut next_discovery_seq: usize = 1;

    let mut current_wave: VecDeque<(String, usize)> = VecDeque::new();
    if 0 < options.max_depth {
        enqueue_discovered_links(
            seed_fetched.links,
            &origin,
            options.path_filter.as_deref(),
            &mut visited,
            &mut current_wave,
            &mut urls_discovered,
            &mut next_discovery_seq,
        );
    }

    let mut next_dispatch_at = tokio::time::Instant::now() + polite_spacing;
    let mut depth: u32 = 1;

    while depth <= options.max_depth && collected.len() < max_pages && !current_wave.is_empty() {
        let collect_links = depth < options.max_depth;
        let mut reserved_slots = collected.len();
        let mut wave_results: Vec<FetchedCrawlPage> = Vec::new();
        let mut in_flight: Vec<Pin<Box<dyn Future<Output = CrawlAttempt> + Send + '_>>> =
            Vec::new();

        while !current_wave.is_empty() || !in_flight.is_empty() {
            while in_flight.len() < DEFAULT_CRAWL_CONCURRENCY && reserved_slots < max_pages {
                let Some((url, seq)) = current_wave.pop_front() else {
                    break;
                };
                reserved_slots += 1;

                let now = tokio::time::Instant::now();
                let dispatch_at = next_dispatch_at.max(now);
                next_dispatch_at = dispatch_at + polite_spacing;

                let fallback_url = &seed_final_url;
                let opts_ref = &options;
                let url_for_log = url.clone();
                in_flight.push(Box::pin(async move {
                    if dispatch_at > tokio::time::Instant::now() {
                        tokio::time::sleep_until(dispatch_at).await;
                    }
                    match fetch_and_extract_page(
                        ctx,
                        url,
                        depth,
                        seq,
                        fallback_url,
                        opts_ref,
                        collect_links,
                    )
                    .await
                    {
                        Ok(Some(page)) => CrawlAttempt::Page(page),
                        Ok(None) => CrawlAttempt::Skipped,
                        Err(e) => {
                            tracing::debug!("crawl skip {}: {}", url_for_log, e);
                            CrawlAttempt::Failed {
                                discovery_sequence: seq,
                                failure: CrawlFailure {
                                    url: url_for_log,
                                    depth,
                                    error: e.to_string(),
                                    kind: e.kind().to_string(),
                                },
                            }
                        }
                    }
                }));
            }

            if let Some(outcome) = poll_next_ready(&mut in_flight).await {
                match outcome {
                    CrawlAttempt::Page(fetched) => wave_results.push(fetched),
                    CrawlAttempt::Skipped => reserved_slots = reserved_slots.saturating_sub(1),
                    CrawlAttempt::Failed {
                        discovery_sequence,
                        failure,
                    } => {
                        reserved_slots = reserved_slots.saturating_sub(1);
                        failed.push((failure.depth, discovery_sequence, failure));
                    }
                }
            } else {
                break;
            }
        }

        wave_results.sort_by_key(|item| item.discovery_sequence);

        let mut next_wave: VecDeque<(String, usize)> = VecDeque::new();
        for item in wave_results {
            if collect_links {
                enqueue_discovered_links(
                    item.links,
                    &origin,
                    options.path_filter.as_deref(),
                    &mut visited,
                    &mut next_wave,
                    &mut urls_discovered,
                    &mut next_discovery_seq,
                );
            }
            collected.push((item.depth, item.discovery_sequence, item.page));
        }

        current_wave = next_wave;
        depth += 1;
    }

    collected.sort_by_key(|(page_depth, seq, _)| (*page_depth, *seq));
    failed.sort_by_key(|(page_depth, seq, _)| (*page_depth, *seq));
    let pages = collected.into_iter().map(|(_, _, page)| page).collect();
    let failed_pages = failed.into_iter().map(|(_, _, f)| f).collect();

    Ok(CrawlResult {
        pages,
        failed_pages,
        urls_discovered,
        timing_ms: start.elapsed().as_millis() as u64,
    })
}

async fn fetch_and_extract_page(
    ctx: &RuntimeContext,
    url: String,
    depth: u32,
    discovery_sequence: usize,
    fallback_url: &Url,
    options: &CrawlOptions,
    collect_links: bool,
) -> std::result::Result<Option<FetchedCrawlPage>, FetchError> {
    let html_result = fetch::fetch_html(ctx, &url, options.respect_robots).await?;

    if html_result.status_code >= 400 || html_result.html.is_empty() {
        return Ok(None);
    }

    let page_url = Url::parse(&html_result.url).unwrap_or_else(|_| fallback_url.clone());
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

    let links = if collect_links {
        parsed_doc
            .as_ref()
            .map(|doc| doc.links(&page_url))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

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
    .map_err(FetchError::Transport)?;

    Ok(Some(FetchedCrawlPage {
        depth,
        discovery_sequence,
        page: CrawlPage {
            url: html_result.url,
            title: extracted.title,
            content: extracted.content,
            depth,
            status_code: html_result.status_code,
            strategy: html_result.strategy,
            content_outcome: html_result.content_outcome,
            truncated: extracted.truncated,
            acquired_sha256: html_result.acquired_sha256,
            warning: html_result.warning,
        },
        links,
    }))
}

fn enqueue_discovered_links(
    links: Vec<Url>,
    origin: &str,
    path_filter: Option<&str>,
    visited: &mut HashSet<String>,
    queue: &mut VecDeque<(String, usize)>,
    urls_discovered: &mut usize,
    next_discovery_seq: &mut usize,
) {
    for link in links {
        if !is_same_origin(&link, origin) {
            continue;
        }
        if let Some(prefix) = path_filter {
            if !link.path().starts_with(prefix) {
                continue;
            }
        }
        let norm = normalize_crawl_url(&link);
        if !is_crawlable_extension(&link) || visited.contains(&norm) {
            continue;
        }

        visited.insert(norm);
        *urls_discovered += 1;
        let seq = *next_discovery_seq;
        *next_discovery_seq += 1;
        queue.push_back((link.to_string(), seq));
    }
}

async fn poll_next_ready<T>(
    in_flight: &mut Vec<Pin<Box<dyn Future<Output = T> + Send + '_>>>,
) -> Option<T> {
    if in_flight.is_empty() {
        return None;
    }
    std::future::poll_fn(|cx| {
        for idx in 0..in_flight.len() {
            if let Poll::Ready(val) = in_flight[idx].as_mut().poll(cx) {
                drop(in_flight.swap_remove(idx));
                return Poll::Ready(Some(val));
            }
        }
        Poll::Pending
    })
    .await
}

pub fn format_crawl_output(result: &CrawlResult, host: &str) -> String {
    let mut out = format!(
        "# Crawl Results: {}\nCrawled {} pages in {:.1}s",
        host,
        result.pages.len(),
        result.timing_ms as f64 / 1000.0
    );
    if !result.failed_pages.is_empty() {
        out.push_str(&format!(" ({} failed)", result.failed_pages.len()));
    }
    out.push('\n');

    let mut total_len = out.len();
    for (i, page) in result.pages.iter().enumerate() {
        let mut body = String::new();
        if let Some(warning) = &page.warning {
            body.push_str(&format!("> Warning: {warning}\n\n"));
        }
        body.push_str(&page.content);
        let section = extract::format_page_section(
            i + 1,
            result.pages.len(),
            page.title.as_deref(),
            &page.url,
            &body,
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

pub async fn map(ctx: &RuntimeContext, start_url: &str, options: MapOptions) -> Result<MapResult> {
    let start = Instant::now();
    let parsed = Url::parse(start_url)?;
    let origin = get_origin(&parsed);
    let limit = options.limit as usize;

    let mut discovered: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut from_sitemap = 0;

    if options.use_sitemap {
        let sitemap_urls = fetch_sitemap(ctx, &parsed, &origin, &options, limit).await;
        for url in sitemap_urls {
            let norm = normalize_url(&url);
            if !visited.contains(&norm) {
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
    if discovered.len() < limit && !visited.contains(&seed_norm) {
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

enum ParsedSitemap {
    Index(Vec<String>),
    UrlSet(Vec<String>),
}

async fn fetch_sitemap(
    ctx: &RuntimeContext,
    base_url: &Url,
    origin: &str,
    options: &MapOptions,
    limit: usize,
) -> Vec<String> {
    if origin == "null" || limit == 0 {
        return Vec::new();
    }

    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut visited_sitemaps: HashSet<String> = HashSet::new();
    let mut seen_pages: HashSet<String> = HashSet::new();
    let mut page_urls: Vec<String> = Vec::new();
    let mut sitemaps_fetched: usize = 0;

    for robots_sitemap in ctx
        .robots
        .sitemap_urls(&ctx.client, base_url.as_str())
        .await
    {
        queue.push_back((robots_sitemap, 0));
    }
    queue.push_back((format!("{}/sitemap.xml", origin), 0));

    while let Some((sitemap_url, depth)) = queue.pop_front() {
        if sitemaps_fetched >= MAX_SITEMAPS || page_urls.len() >= limit {
            break;
        }
        let sitemap_norm = normalize_url(&sitemap_url);
        if !visited_sitemaps.insert(sitemap_norm) {
            continue;
        }
        sitemaps_fetched += 1;

        let resp = match ctx.client.get(&sitemap_url).await {
            Ok(r) if r.status < 400 => r,
            _ => continue,
        };

        let Some(xml) = decode_sitemap_body(&resp.body, resp.content_type.as_deref()) else {
            continue;
        };

        match parse_sitemap_xml(&xml) {
            ParsedSitemap::Index(children) => {
                if depth < MAX_SITEMAP_DEPTH {
                    for child in children {
                        queue.push_back((child, depth + 1));
                    }
                }
            }
            ParsedSitemap::UrlSet(urls) => {
                for url in urls {
                    if page_urls.len() >= limit {
                        break;
                    }
                    let Ok(parsed_url) = Url::parse(&url) else {
                        continue;
                    };
                    if !is_same_origin(&parsed_url, origin) {
                        continue;
                    }
                    if let Some(ref prefix) = options.path_filter {
                        if !parsed_url.path().starts_with(prefix.as_str()) {
                            continue;
                        }
                    }
                    let norm = normalize_crawl_url(&parsed_url);
                    if seen_pages.contains(&norm) {
                        continue;
                    }
                    if options.respect_robots && !ctx.robots.is_allowed(&ctx.client, &url).await {
                        continue;
                    }
                    seen_pages.insert(norm);
                    page_urls.push(url);
                }
            }
        }
    }

    page_urls
}

fn decode_sitemap_body(body: &[u8], content_type: Option<&str>) -> Option<String> {
    if body.starts_with(&[0x1f, 0x8b]) {
        let decoder = GzDecoder::new(body);
        let mut limited = decoder.take(MAX_SITEMAP_DECOMPRESSED_BYTES + 1);
        let mut out = Vec::new();
        limited.read_to_end(&mut out).ok()?;
        if out.len() as u64 > MAX_SITEMAP_DECOMPRESSED_BYTES {
            return None;
        }
        Some(extract::decode_text_bytes(&out, None))
    } else {
        Some(extract::decode_text_bytes(body, content_type))
    }
}

fn parse_sitemap_xml(xml: &str) -> ParsedSitemap {
    let bytes = xml.as_bytes();
    let mut is_index = false;
    let mut urls = Vec::new();
    let mut idx = 0;

    while idx < bytes.len() {
        let Some(rel_lt) = xml[idx..].find('<') else {
            break;
        };
        let tag_start = idx + rel_lt + 1;
        if tag_start >= bytes.len() {
            break;
        }
        let first = bytes[tag_start];
        if first == b'?' || first == b'!' || first == b'/' {
            idx = tag_start + 1;
            continue;
        }

        let mut name_end = tag_start;
        while name_end < bytes.len() && is_xml_name_char(bytes[name_end]) {
            name_end += 1;
        }
        let raw_name = &xml[tag_start..name_end];
        let local_name = raw_name.rsplit_once(':').map_or(raw_name, |(_, l)| l);

        if local_name.eq_ignore_ascii_case("sitemapindex") {
            is_index = true;
            idx = name_end.max(tag_start + 1);
            continue;
        }
        if !local_name.eq_ignore_ascii_case("loc") {
            idx = name_end.max(tag_start + 1);
            continue;
        }

        let Some(rel_gt) = xml[name_end..].find('>') else {
            break;
        };
        let open_end = name_end + rel_gt;
        if open_end > name_end && bytes[open_end - 1] == b'/' {
            idx = open_end + 1;
            continue;
        }
        let content_start = open_end + 1;

        let Some((content_end, next_idx)) = find_closing_loc(xml, content_start) else {
            break;
        };

        let raw_value = xml[content_start..content_end].trim();
        let unescaped = decode_xml_text(raw_value);
        let trimmed = unescaped.trim();
        if let Ok(parsed) = Url::parse(trimmed) {
            if matches!(parsed.scheme(), "http" | "https") {
                urls.push(trimmed.to_string());
            }
        }

        idx = next_idx;
    }

    if is_index {
        ParsedSitemap::Index(urls)
    } else {
        ParsedSitemap::UrlSet(urls)
    }
}

fn find_closing_loc(xml: &str, mut search_from: usize) -> Option<(usize, usize)> {
    let bytes = xml.as_bytes();
    while search_from < bytes.len() {
        let rel = xml[search_from..].find("</")?;
        let close_start = search_from + rel;
        let name_start = close_start + 2;
        let mut name_end = name_start;
        while name_end < bytes.len() && is_xml_name_char(bytes[name_end]) {
            name_end += 1;
        }
        let raw_name = &xml[name_start..name_end];
        let local_name = raw_name.rsplit_once(':').map_or(raw_name, |(_, l)| l);
        if local_name.eq_ignore_ascii_case("loc") {
            let rel_gt = xml[name_end..].find('>')?;
            return Some((close_start, name_end + rel_gt + 1));
        }
        search_from = name_end.max(name_start);
    }
    None
}

fn decode_xml_text(raw: &str) -> String {
    if let Some(inner) = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
    {
        return inner.to_string();
    }

    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn is_xml_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
}

fn normalize_url(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => normalize_crawl_url(&u),
        Err(_) => url.to_string(),
    }
}

/// Returns the URL host string for display output, falling back to `raw`.
pub fn url_host_or_raw(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
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
