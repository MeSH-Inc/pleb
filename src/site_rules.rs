use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

const SEED: &str = include_str!("../data/site-rules.json");

#[derive(Debug, Clone, Deserialize)]
pub struct SiteRule {
    #[serde(default)]
    pub render: String,
    #[serde(default)]
    pub needs_residential: bool,
    #[serde(default)]
    pub wait_for_selector: Option<String>,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    #[serde(default)]
    rules: HashMap<String, SiteRule>,
}

static RULES: std::sync::LazyLock<HashMap<String, SiteRule>> =
    std::sync::LazyLock::new(load_rules);

fn load_rules() -> HashMap<String, SiteRule> {
    let mut map = parse(SEED);
    if let Some(overlay) = read_overlay() {
        for (host, rule) in parse(&overlay) {
            map.insert(host, rule);
        }
    }
    map
}

fn parse(data: &str) -> HashMap<String, SiteRule> {
    serde_json::from_str::<RuleFile>(data)
        .map(|f| f.rules)
        .unwrap_or_default()
}

fn overlay_path() -> PathBuf {
    crate::session::state_dir().join("site-rules.json")
}

fn read_overlay() -> Option<String> {
    std::fs::read_to_string(overlay_path()).ok()
}

pub fn get(host: &str) -> Option<SiteRule> {
    if let Some(r) = RULES.get(host) {
        return Some(r.clone());
    }
    crate::site_cache::parent_domain(host).and_then(|p| RULES.get(p).cloned())
}
