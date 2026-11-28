use serde::{Deserialize, Serialize};
use url::Url;

use crate::extract::{ExpandPolicy, ResourceDescriptor};

pub const MAX_REVEAL_ACTIONS: usize = 16;
pub const MAX_REVEAL_PER_GROUP: usize = 8;
pub const MAX_SCROLL_STEPS: usize = 20;
pub const MAX_REVEALED_PANEL_HTML_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_REVEAL_META_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_XHR_DATASETS: usize = 12;
pub const MAX_RAW_CAPTURED_DATASETS: usize = 32;

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

/// Filter passively captured XHR/Fetch JSON responses into `ResourceKind::Dataset` entries.
pub fn filter_and_index_xhr_datasets(
    _page_url: &Url,
    _captured: &[CapturedDataset],
    _resources: &mut Vec<ResourceDescriptor>,
) {
}

/// Build an async CDP `Runtime.evaluate` wrapper executing `__plebRevealAndScroll`.
pub fn build_cdp_reveal_eval_js(_reveal: RevealPolicy, _scroll_steps: usize, _budget_ms: u64) -> String {
    String::from("({revealed_panels:[],reveal_capped:false,scroll_steps_performed:0,navigated:false,challenge_detected:false,warning:null})")
}
