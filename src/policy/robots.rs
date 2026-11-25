use robotstxt::DefaultMatcher;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::transport::TransportClient;

const TTL: Duration = Duration::from_secs(3600);
const MAX_HOSTS: usize = 500;

struct CacheEntry {
    body: String,
    fetched_at: Instant,
}

#[derive(Default)]
pub struct RobotsCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

impl RobotsCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn is_allowed(&self, client: &TransportClient, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return true;
        };
        let origin = parsed.origin().ascii_serialization();
        if origin == "null" {
            return true;
        }

        let Some(robots_body) = self.get_robots(client, &origin).await else {
            return true;
        };

        let mut matcher = DefaultMatcher::default();
        if !matcher.one_agent_allowed_by_robots(&robots_body, "mesh-wick", url) {
            return false;
        }
        if !matcher.one_agent_allowed_by_robots(&robots_body, "Wick", url) {
            return false;
        }
        matcher.one_agent_allowed_by_robots(&robots_body, "*", url)
    }

    async fn get_robots(&self, client: &TransportClient, origin: &str) -> Option<String> {
        {
            let cache = self.entries.lock().ok()?;
            if let Some(entry) = cache.get(origin) {
                if entry.fetched_at.elapsed() < TTL {
                    return Some(entry.body.clone());
                }
            }
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
}
