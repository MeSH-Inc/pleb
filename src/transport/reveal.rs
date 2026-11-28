use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use url::Url;

use crate::extract::resources::{clean_optional_text_pub, sanitize_dataset_stem};
use crate::extract::url::{resolve_and_clean_url, ResolvedUrl};
use crate::extract::{
    resource_identifier, ExpandPolicy, ResourceDescriptor, ResourceKind, ResourceSalience,
};
use crate::policy::is_safe_subresource_url;
use crate::policy::source::canonical_url_host;

pub const MAX_REVEAL_ACTIONS: usize = 16;
pub const MAX_REVEAL_PER_GROUP: usize = 8;
pub const MAX_SCROLL_STEPS: usize = 20;
pub const MAX_REVEALED_PANEL_HTML_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_REVEAL_META_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_XHR_DATASETS: usize = 12;
pub const MAX_RAW_CAPTURED_DATASETS: usize = 32;

const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "token",
    "access_token",
    "auth",
    "key",
    "api_key",
    "apikey",
    "secret",
    "sig",
    "signature",
    "jwt",
    "session",
    "password",
    "credential",
];

const NOISE_DATASET_FILENAMES: &[&str] = &[
    "manifest.json",
    "site.webmanifest",
    "asset-manifest.json",
    "robots.json",
    "browserconfig.xml",
    "browserconfig.json",
    "oembed.json",
];

const NOISE_DATASET_SEGMENTS: &[&str] = &[
    ".well-known",
    "telemetry",
    "analytics",
    "metrics",
    "beacon",
    "collect",
    "sentry",
    "datadog",
    "ping",
    "health",
    "oembed",
];

/// Declarative bounded pre-capture DOM disclosure reveal policy for browser renderers.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealPolicy {
    /// Do not click any controls prior to DOM capture.
    #[default]
    None,
    /// Snapshot active/inactive in-landmark `[role="tabpanel"]` tabs requiring interaction.
    Tabs,
    /// Snapshot collapsed in-landmark ARIA disclosures (`[aria-expanded="false"]`) requiring interaction.
    Disclosures,
    /// Reveal both unmounted tabs and collapsed disclosures inside the main landmark.
    All,
}

impl RevealPolicy {
    /// Canonical lowercase serialization token shared with CEF `v6-meta` and CLI/MCP contracts.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tabs => "tabs",
            Self::Disclosures => "disclosures",
            Self::All => "all",
        }
    }

    /// Upgrade static `--expand` so in-DOM hidden panels are rescued in Rust without clicks.
    pub fn implies_expand(self, current: ExpandPolicy) -> ExpandPolicy {
        match self {
            Self::None => current,
            Self::Tabs => match current {
                ExpandPolicy::All => ExpandPolicy::All,
                _ => ExpandPolicy::Tabs,
            },
            Self::Disclosures | Self::All => ExpandPolicy::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealedPanelKind {
    Tab,
    Disclosure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevealedPanelSnapshot {
    pub kind: RevealedPanelKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_heading: Option<String>,
    #[serde(default = "default_heading_level")]
    pub heading_level: u8,
    pub html: String,
}

fn default_heading_level() -> u8 {
    3
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedDataset {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_count: Option<usize>,
}

/// Metadata sidecar returned by CEF `v6-meta` and navigated CDP renders.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrowserCaptureMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revealed_panels: Vec<RevealedPanelSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub datasets: Vec<CapturedDataset>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reveal_capped: bool,
    #[serde(default)]
    pub scroll_steps_performed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_warning: Option<String>,
}

impl BrowserCaptureMetadata {
    /// Enforce per-panel and total byte caps on captured panel snapshots.
    pub fn enforce_bounds(&mut self) {
        let mut kept = Vec::with_capacity(self.revealed_panels.len());
        let mut total_bytes = 0usize;
        for mut snap in self.revealed_panels.drain(..) {
            if snap.html.len() > MAX_REVEALED_PANEL_HTML_BYTES {
                let mut end = MAX_REVEALED_PANEL_HTML_BYTES;
                while end > 0 && !snap.html.is_char_boundary(end) {
                    end -= 1;
                }
                snap.html.truncate(end);
                self.reveal_capped = true;
            }
            let next_total = total_bytes.saturating_add(snap.html.len());
            if next_total > MAX_TOTAL_REVEAL_META_BYTES {
                self.reveal_capped = true;
                break;
            }
            total_bytes = next_total;
            kept.push(snap);
        }
        self.revealed_panels = kept;
    }
}

/// Returns `true` when `mime` represents an indexable JSON data payload (excluding error/manifest stubs).
pub fn is_indexable_json_mime(mime: &str) -> bool {
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base.is_empty()
        || matches!(
            base.as_str(),
            "application/problem+json" | "application/manifest+json"
        )
    {
        return false;
    }
    base == "application/json"
        || base == "text/json"
        || (base.starts_with("application/") && base.ends_with("+json"))
}

fn has_sensitive_query_param(url: &Url) -> bool {
    url.query_pairs().any(|(k, _)| {
        let lower = k.trim().to_ascii_lowercase();
        SENSITIVE_QUERY_KEYS.contains(&lower.as_str())
    })
}

fn is_noise_dataset_url(url: &Url) -> bool {
    let Some(segments) = url.path_segments() else {
        return false;
    };
    let lower_segments: Vec<String> = segments
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if lower_segments
        .last()
        .is_some_and(|last| NOISE_DATASET_FILENAMES.contains(&last.as_str()))
    {
        return true;
    }
    lower_segments
        .iter()
        .any(|seg| NOISE_DATASET_SEGMENTS.contains(&seg.as_str()))
}

/// Filter passively captured XHR/Fetch JSON responses into `ResourceKind::Dataset` entries.
pub fn filter_and_index_xhr_datasets(
    page_url: &Url,
    captured: &[CapturedDataset],
    resources: &mut Vec<ResourceDescriptor>,
) {
    let Some(page_host) = canonical_url_host(page_url) else {
        return;
    };
    let page_origin = (
        page_url.scheme().to_ascii_lowercase(),
        page_host,
        page_url.port_or_known_default(),
    );

    let mut seen_ids: HashSet<String> = resources.iter().map(|r| r.identifier.clone()).collect();
    let mut added = 0usize;

    for item in captured {
        if added >= MAX_XHR_DATASETS {
            break;
        }
        if item.byte_count.is_some_and(|b| b <= 2) {
            continue;
        }
        let mime_clean = item.mime_type.as_deref().and_then(|m| {
            let b = m.split(';').next()?.trim().to_ascii_lowercase();
            is_indexable_json_mime(&b).then_some(b)
        });
        let Some(mime_hint) = mime_clean else {
            continue;
        };

        let Ok(raw_parsed) = Url::parse(item.url.trim()) else {
            continue;
        };
        if has_sensitive_query_param(&raw_parsed) || !is_safe_subresource_url(page_url, &raw_parsed)
        {
            continue;
        }
        let Some(cand_host) = canonical_url_host(&raw_parsed) else {
            continue;
        };
        let cand_origin = (
            raw_parsed.scheme().to_ascii_lowercase(),
            cand_host,
            raw_parsed.port_or_known_default(),
        );
        if cand_origin != page_origin || is_noise_dataset_url(&raw_parsed) {
            continue;
        }

        let ResolvedUrl::Resolved(clean_url) = resolve_and_clean_url(raw_parsed.as_str(), page_url)
        else {
            continue;
        };
        let Ok(clean_parsed) = Url::parse(&clean_url) else {
            continue;
        };
        if has_sensitive_query_param(&clean_parsed) || is_noise_dataset_url(&clean_parsed) {
            continue;
        }

        let identifier = resource_identifier(&clean_url);
        if !seen_ids.insert(identifier.clone()) {
            continue;
        }

        let label_stem = sanitize_dataset_stem(&clean_parsed);
        let label = clean_optional_text_pub(&format!("XHR JSON: {label_stem}"));
        resources.push(ResourceDescriptor {
            identifier,
            kind: ResourceKind::Dataset,
            url: clean_url,
            mime_hint: Some(mime_hint),
            section_heading: Some("Network XHR".to_string()),
            label,
            dimensions: None,
            salience: ResourceSalience::High,
            staged_path: None,
            byte_count: item.byte_count,
            content_type: None,
        });
        added += 1;
    }
}

/// Build an async CDP `Runtime.evaluate` wrapper executing `__plebRevealAndScroll`.
pub fn build_cdp_reveal_eval_js(
    reveal: RevealPolicy,
    scroll_steps: usize,
    budget_ms: u64,
) -> String {
    let clamped_scroll = scroll_steps.min(MAX_SCROLL_STEPS);
    let script = include_str!("../../cef/reveal_script.js");
    format!(
        "(async function(){{\n{script}\nreturn await __plebRevealAndScroll({{\n  mode: {mode:?},\n  scrollSteps: {clamped_scroll},\n  maxActions: {MAX_REVEAL_ACTIONS},\n  maxPerGroup: {MAX_REVEAL_PER_GROUP},\n  maxPanelBytes: {MAX_REVEALED_PANEL_HTML_BYTES},\n  maxTotalBytes: 2097152,\n  budgetMs: {budget}\n}});\n}})()",
        mode = reveal.as_str(),
        budget = budget_ms.max(200),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_policy_implies_expand_matrix() {
        assert_eq!(
            RevealPolicy::None.implies_expand(ExpandPolicy::None),
            ExpandPolicy::None
        );
        assert_eq!(
            RevealPolicy::Tabs.implies_expand(ExpandPolicy::None),
            ExpandPolicy::Tabs
        );
        assert_eq!(
            RevealPolicy::Tabs.implies_expand(ExpandPolicy::All),
            ExpandPolicy::All
        );
        assert_eq!(
            RevealPolicy::Disclosures.implies_expand(ExpandPolicy::Tabs),
            ExpandPolicy::All
        );
        assert_eq!(
            RevealPolicy::All.implies_expand(ExpandPolicy::None),
            ExpandPolicy::All
        );
    }

    #[test]
    fn indexable_json_mime_and_xhr_dataset_filtering() {
        assert!(is_indexable_json_mime("application/json; charset=utf-8"));
        assert!(is_indexable_json_mime("application/vnd.api+json"));
        assert!(is_indexable_json_mime("text/json"));
        assert!(!is_indexable_json_mime("application/problem+json"));
        assert!(!is_indexable_json_mime("application/manifest+json"));
        assert!(!is_indexable_json_mime("text/html"));

        let page_url = Url::parse("https://app.example.com/dashboard").unwrap();
        let captured = vec![
            // 1. Valid same-origin JSON API
            CapturedDataset {
                url: "https://app.example.com/api/v1/metrics.json?page=2&utm_source=ui".into(),
                mime_type: Some("application/json; charset=utf-8".into()),
                byte_count: Some(1240),
            },
            // 2. Duplicate after tracking-param stripping
            CapturedDataset {
                url: "https://app.example.com/api/v1/metrics.json?page=2".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(1240),
            },
            // 3. Cross-origin API rejected
            CapturedDataset {
                url: "https://api.other.com/v1/metrics.json".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(500),
            },
            // 4. Sensitive query param rejected before cleaning
            CapturedDataset {
                url: "https://app.example.com/api/private.json?token=secret123".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(500),
            },
            // 5. Noise telemetry / manifest / empty stub rejected
            CapturedDataset {
                url: "https://app.example.com/analytics/events.json".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(400),
            },
            CapturedDataset {
                url: "https://app.example.com/manifest.json".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(300),
            },
            CapturedDataset {
                url: "https://app.example.com/api/empty.json".into(),
                mime_type: Some("application/json".into()),
                byte_count: Some(2),
            },
            // 6. Problem+json rejected
            CapturedDataset {
                url: "https://app.example.com/api/err.json".into(),
                mime_type: Some("application/problem+json".into()),
                byte_count: Some(100),
            },
            // 7. Valid vnd+json payload indexed
            CapturedDataset {
                url: "https://app.example.com/api/items".into(),
                mime_type: Some("application/vnd.api+json".into()),
                byte_count: Some(8192),
            },
        ];

        let mut resources = Vec::new();
        filter_and_index_xhr_datasets(&page_url, &captured, &mut resources);
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].kind, ResourceKind::Dataset);
        assert_eq!(resources[0].salience, ResourceSalience::High);
        assert_eq!(
            resources[0].url,
            "https://app.example.com/api/v1/metrics.json?page=2"
        );
        assert_eq!(resources[0].section_heading.as_deref(), Some("Network XHR"));
        assert_eq!(resources[1].url, "https://app.example.com/api/items");
    }
}
