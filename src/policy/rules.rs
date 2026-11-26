use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

const BUNDLED_RULES: &str = include_str!("../../data/site-rules.json");

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SiteRule {
    #[serde(default)]
    pub render: Option<String>,
    #[serde(default)]
    pub wait_for_selector: Option<String>,
    #[serde(default)]
    pub rewrite_host: Option<String>,
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
        let overlay = match std::fs::read_to_string(&overlay_path) {
            Ok(data) => Some(data),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                tracing::warn!(
                    "failed to read site-rules overlay {}: {}",
                    overlay_path.display(),
                    err
                );
                None
            }
        };
        Self::from_json_with_overlay(BUNDLED_RULES, overlay.as_deref())
    }

    pub fn from_json_with_overlay(seed: &str, overlay: Option<&str>) -> Self {
        let mut rules = parse_rule_map(seed, "bundled");
        if let Some(ov) = overlay {
            for (host, rule) in parse_rule_map(ov, "overlay") {
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

fn parse_rule_map(data: &str, source: &str) -> HashMap<String, SiteRule> {
    match serde_json::from_str::<RuleFile>(data) {
        Ok(file) => file.rules,
        Err(err) => {
            tracing::warn!("invalid {} site-rules.json ignored: {}", source, err);
            HashMap::new()
        }
    }
}

pub fn parent_domain(host: &str) -> Option<&str> {
    let dot = host.find('.')?;
    let parent = &host[dot + 1..];
    parent.contains('.').then_some(parent)
}
