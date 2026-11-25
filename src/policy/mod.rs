pub mod robots;

use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extract::dom;

const BUNDLED_RULES: &str = include_str!("../../data/site-rules.json");
const STRATEGY_TTL_SECS: u64 = 7 * 24 * 3600;
const TIMEOUT_TTL_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    #[default]
    Auto,
    Cef,
    Cronet,
}

impl FromStr for RenderMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "cef" | "browser" | "render" | "js" => Ok(Self::Cef),
            "cronet" | "network" | "fast" | "no-js" | "nojs" => Ok(Self::Cronet),
            other => bail!(
                "invalid render mode {:?} (expected auto, cef, or cronet)",
                other
            ),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SiteRule {
    #[serde(default)]
    pub render: Option<String>,
    #[serde(default)]
    pub wait_for_selector: Option<String>,
    #[serde(default)]
    pub rewrite_host: Option<String>,
    #[serde(default)]
    pub vendor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    #[serde(default)]
    rules: HashMap<String, SiteRule>,
}

#[derive(Debug, Clone)]
pub struct SiteRules {
    rules: HashMap<String, SiteRule>,
}

impl SiteRules {
    pub fn load(state_dir: &Path) -> Self {
        let overlay_path = state_dir.join("site-rules.json");
        let overlay = std::fs::read_to_string(overlay_path).ok();
        Self::from_json_with_overlay(BUNDLED_RULES, overlay.as_deref())
    }

    pub fn from_json_with_overlay(seed: &str, overlay: Option<&str>) -> Self {
        let mut rules = parse_rule_map(seed);
        if let Some(ov) = overlay {
            for (host, rule) in parse_rule_map(ov) {
                rules.insert(host, rule);
            }
        }
        Self { rules }
    }

    pub fn get(&self, host: &str) -> Option<&SiteRule> {
        if let Some(rule) = self.rules.get(host) {
            return Some(rule);
        }
        parent_domain(host).and_then(|parent| self.rules.get(parent))
    }

    pub fn rewrite_url(&self, parsed: &url::Url) -> Option<url::Url> {
        let host = parsed.host_str()?;
        let rule = self.rules.get(host)?;
        let target_host = rule.rewrite_host.as_deref()?;
        if target_host.is_empty() || target_host.eq_ignore_ascii_case(host) {
            return None;
        }
        let mut rewritten = parsed.clone();
        rewritten.set_host(Some(target_host)).ok()?;
        Some(rewritten)
    }
}

fn parse_rule_map(data: &str) -> HashMap<String, SiteRule> {
    serde_json::from_str::<RuleFile>(data)
        .map(|f| f.rules)
        .unwrap_or_default()
}

pub fn parent_domain(host: &str) -> Option<&str> {
    let dot = host.find('.')?;
    let parent = &host[dot + 1..];
    parent.contains('.').then_some(parent)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCacheEntry {
    pub strategy: String,
    pub avg_time_ms: u64,
    pub successes: u32,
    pub updated_at_epoch_secs: u64,
}

pub struct SiteCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, SiteCacheEntry>>,
}

impl SiteCache {
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    pub fn get(&self, host: &str) -> Option<SiteCacheEntry> {
        self.get_at(host, now_epoch_secs())
    }

    pub fn get_at(&self, host: &str, now_secs: u64) -> Option<SiteCacheEntry> {
        let guard = self.entries.lock().ok()?;
        let entry = guard
            .get(host)
            .or_else(|| parent_domain(host).and_then(|p| guard.get(p)))?;
        if is_expired(entry, now_secs) {
            None
        } else {
            Some(entry.clone())
        }
    }

    pub fn record(&self, host: &str, strategy: &str, time_ms: u64) {
        self.record_at(host, strategy, time_ms, now_epoch_secs());
    }

    pub fn record_at(&self, host: &str, strategy: &str, time_ms: u64, now_secs: u64) {
        let Ok(mut guard) = self.entries.lock() else {
            return;
        };

        let entry = guard
            .entry(host.to_string())
            .or_insert_with(|| SiteCacheEntry {
                strategy: strategy.to_string(),
                avg_time_ms: time_ms,
                successes: 0,
                updated_at_epoch_secs: now_secs,
            });

        entry.successes = entry.successes.saturating_add(1);
        entry.avg_time_ms = (entry.avg_time_ms * 3 + time_ms) / 4;
        entry.updated_at_epoch_secs = now_secs;
        entry.strategy = strategy.to_string();

        let _ = save_atomic(&self.path, &guard);
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

fn is_expired(entry: &SiteCacheEntry, now_secs: u64) -> bool {
    let ttl = match entry.strategy.as_str() {
        "cef_timeout" => TIMEOUT_TTL_SECS,
        _ => STRATEGY_TTL_SECS,
    };
    now_secs.saturating_sub(entry.updated_at_epoch_secs) > ttl
}

fn save_atomic(path: &Path, entries: &HashMap<String, SiteCacheEntry>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("site-cache.json");
    let tmp = path.with_file_name(format!("{}.tmp.{}", file_name, std::process::id()));
    std::fs::write(&tmp, data.as_bytes())?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryTransport {
    Cronet,
    Cef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyPlan {
    pub primary: PrimaryTransport,
    pub allow_cef_escalation: bool,
    pub allow_cronet_fallback: bool,
}

pub fn plan_strategy(
    render_mode: RenderMode,
    rule: Option<&SiteRule>,
    cached: Option<&SiteCacheEntry>,
    cef_available: bool,
) -> StrategyPlan {
    match render_mode {
        RenderMode::Cronet => StrategyPlan {
            primary: PrimaryTransport::Cronet,
            allow_cef_escalation: false,
            allow_cronet_fallback: false,
        },
        RenderMode::Cef => StrategyPlan {
            primary: PrimaryTransport::Cef,
            allow_cef_escalation: false,
            allow_cronet_fallback: false,
        },
        RenderMode::Auto => {
            if !cef_available {
                return StrategyPlan {
                    primary: PrimaryTransport::Cronet,
                    allow_cef_escalation: false,
                    allow_cronet_fallback: false,
                };
            }

            let rule_render = rule.and_then(|r| r.render.as_deref());
            match rule_render {
                Some("cef") => StrategyPlan {
                    primary: PrimaryTransport::Cef,
                    allow_cef_escalation: false,
                    allow_cronet_fallback: true,
                },
                Some("cronet") => StrategyPlan {
                    primary: PrimaryTransport::Cronet,
                    allow_cef_escalation: true,
                    allow_cronet_fallback: false,
                },
                _ => {
                    if cached.map(|c| c.strategy.as_str()) == Some("cef") {
                        StrategyPlan {
                            primary: PrimaryTransport::Cef,
                            allow_cef_escalation: false,
                            allow_cronet_fallback: true,
                        }
                    } else {
                        StrategyPlan {
                            primary: PrimaryTransport::Cronet,
                            allow_cef_escalation: true,
                            allow_cronet_fallback: false,
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseClassification {
    Ok,
    Blocked { status: u16 },
    Challenge { status: u16, reason: String },
    JavaScriptShell { reason: String },
    HttpError { status: u16 },
}

pub fn classify_response(status: u16, is_html: bool, body: &str) -> ResponseClassification {
    if status == 403 || status == 503 {
        if let Some(reason) = detect_challenge(body) {
            return ResponseClassification::Challenge { status, reason };
        }
        return ResponseClassification::Blocked { status };
    }

    if status >= 400 {
        return ResponseClassification::HttpError { status };
    }

    if is_html {
        if let Some(reason) = detect_interstitial_challenge(body) {
            return ResponseClassification::Challenge { status, reason };
        }
        if let Some(reason) = detect_js_shell(body) {
            return ResponseClassification::JavaScriptShell { reason };
        }
    }

    ResponseClassification::Ok
}

pub fn detect_challenge(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    let signals = [
        ("challenges.cloudflare.com", "Cloudflare Turnstile/challenge"),
        ("cf-browser-verification", "Cloudflare browser verification"),
        ("just a moment...", "Cloudflare challenge interstitial"),
        ("checking your browser", "Browser verification challenge"),
        ("google.com/recaptcha", "Google reCAPTCHA"),
        ("hcaptcha.com", "hCaptcha challenge"),
        ("captcha-delivery.com", "DataDome challenge"),
        ("px-captcha", "PerimeterX challenge"),
        ("awswaf", "AWS WAF challenge"),
    ];
    for (sig, label) in signals {
        if lower.contains(sig) {
            return Some(label.to_string());
        }
    }
    None
}

pub fn detect_interstitial_challenge(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if lower.contains("just a moment")
        && (lower.contains("performing security verification")
            || lower.contains("checking your browser")
            || lower.contains("challenge-platform"))
    {
        return Some("Cloudflare challenge interstitial".to_string());
    }
    if lower.contains("captcha-delivery.com") && body.len() < 50_000 {
        return Some("DataDome interstitial".to_string());
    }
    None
}

pub fn detect_js_shell(body: &str) -> Option<String> {
    if body.len() > 2_000_000 {
        return None;
    }

    let lower = body.to_ascii_lowercase();
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
    for sig in SIGNALS {
        if lower.contains(sig) {
            return Some(format!("noscript signal ({})", sig));
        }
    }

    let metrics = dom::dom_text_metrics(body);
    if metrics.visible_text_len < 200
        && (metrics.has_empty_spa_root || metrics.script_count >= 3)
    {
        return Some(format!(
            "empty SPA shell (visible_text_len={}, scripts={}, empty_root={})",
            metrics.visible_text_len, metrics.script_count, metrics.has_empty_spa_root
        ));
    }

    None
}
