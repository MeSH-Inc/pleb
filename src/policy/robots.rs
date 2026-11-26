use robotstxt::DefaultMatcher;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::transport::TransportClient;

const TTL: Duration = Duration::from_secs(3600);
const MAX_HOSTS: usize = 500;
const MAX_CRAWL_DELAY_SECS: f64 = 30.0;

struct CacheEntry {
    body: String,
    fetched_at: Instant,
}

#[derive(Default)]
pub struct RobotsCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    origin_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn is_allowed(&self, client: &TransportClient, url: &str) -> bool {
        let Some(origin) = origin_for_url(url) else {
            return true;
        };

        let Some(robots_body) = self.get_robots(client, &origin).await else {
            return true;
        };

        let mut matcher = DefaultMatcher::default();
        if !matcher.one_agent_allowed_by_robots(&robots_body, "pleb", url) {
            return false;
        }
        matcher.one_agent_allowed_by_robots(&robots_body, "*", url)
    }

    /// Extract `Sitemap:` directive URLs (case-insensitive) from the origin's cached `robots.txt`.
    pub async fn sitemap_urls(&self, client: &TransportClient, url: &str) -> Vec<String> {
        let Some(origin) = origin_for_url(url) else {
            return Vec::new();
        };
        let Some(robots_body) = self.get_robots(client, &origin).await else {
            return Vec::new();
        };
        parse_sitemap_directives(&robots_body)
    }

    /// Extract the applicable `Crawl-delay` directive (in seconds) for our user agent or `*`.
    pub async fn crawl_delay(&self, client: &TransportClient, url: &str) -> Option<Duration> {
        let origin = origin_for_url(url)?;
        let robots_body = self.get_robots(client, &origin).await?;
        parse_crawl_delay(&robots_body)
    }

    async fn get_robots(&self, client: &TransportClient, origin: &str) -> Option<String> {
        if let Some(cached) = self.get_cached(origin) {
            return Some(cached);
        }

        let origin_lock = self.origin_lock(origin)?;
        let _guard = origin_lock.lock().await;

        if let Some(cached) = self.get_cached(origin) {
            return Some(cached);
        }

        let robots_url = format!("{}/robots.txt", origin);
        let resp = client.get(&robots_url).await.ok()?;
        let robots_body = match resp.status {
            200 => resp.text(),
            404 | 410 => String::new(),
            _ => return None,
        };

        let mut cache = self.entries.lock().ok()?;
        if cache.len() >= MAX_HOSTS {
            cache.retain(|_, e| e.fetched_at.elapsed() < TTL);
        }
        cache.insert(
            origin.to_string(),
            CacheEntry {
                body: robots_body.clone(),
                fetched_at: Instant::now(),
            },
        );

        Some(robots_body)
    }

    fn get_cached(&self, origin: &str) -> Option<String> {
        let cache = self.entries.lock().ok()?;
        let entry = cache.get(origin)?;
        if entry.fetched_at.elapsed() < TTL {
            Some(entry.body.clone())
        } else {
            None
        }
    }

    fn origin_lock(&self, origin: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        let mut locks = self.origin_locks.lock().ok()?;
        if locks.len() >= MAX_HOSTS {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        Some(
            locks
                .entry(origin.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone(),
        )
    }
}

fn origin_for_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        None
    } else {
        Some(origin)
    }
}

fn parse_sitemap_directives(robots_body: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();

    for raw_line in robots_body.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("sitemap") {
            continue;
        }
        let candidate = value.trim();
        if let Ok(parsed) = url::Url::parse(candidate) {
            if matches!(parsed.scheme(), "http" | "https") && seen.insert(candidate.to_string()) {
                urls.push(candidate.to_string());
            }
        }
    }

    urls
}

fn parse_crawl_delay(robots_body: &str) -> Option<Duration> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum GroupMatch {
        None,
        Wildcard,
        Specific,
    }

    let mut current = GroupMatch::None;
    let mut saw_rule_in_group = false;
    let mut specific_delay: Option<Duration> = None;
    let mut wildcard_delay: Option<Duration> = None;

    for raw_line in robots_body.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if key.eq_ignore_ascii_case("user-agent") {
            if saw_rule_in_group {
                current = GroupMatch::None;
                saw_rule_in_group = false;
            }
            if value.eq_ignore_ascii_case("pleb") {
                current = GroupMatch::Specific;
            } else if value == "*" && current != GroupMatch::Specific {
                current = GroupMatch::Wildcard;
            }
        } else {
            saw_rule_in_group = true;
            if key.eq_ignore_ascii_case("crawl-delay") && current != GroupMatch::None {
                if let Ok(secs) = value.parse::<f64>() {
                    if secs.is_finite() && secs >= 0.0 {
                        let dur = Duration::from_secs_f64(secs.min(MAX_CRAWL_DELAY_SECS));
                        match current {
                            GroupMatch::Specific => specific_delay = Some(dur),
                            GroupMatch::Wildcard => wildcard_delay = Some(dur),
                            GroupMatch::None => {}
                        }
                    }
                }
            }
        }
    }

    specific_delay.or(wildcard_delay)
}
