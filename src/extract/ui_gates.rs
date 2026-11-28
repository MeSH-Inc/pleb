use serde::{Deserialize, Serialize};

use super::floor_char_boundary;

pub const MAX_UI_GATES: usize = 16;
pub const MAX_UI_GATE_LABELS: usize = 16;
pub(crate) const MAX_UI_GATE_LABEL_BYTES: usize = 64;

const MAX_CUE_LABELS: usize = 6;
const MAX_CUE_BYTES: usize = 220;

/// Kind of interactive UI gate detected inside document content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiGateKind {
    Tablist,
    Disclosure,
    Pagination,
    ModalOverlay,
    VisualCanvas,
}

impl UiGateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tablist => "tablist",
            Self::Disclosure => "disclosure",
            Self::Pagination => "pagination",
            Self::ModalOverlay => "modal_overlay",
            Self::VisualCanvas => "visual_canvas",
        }
    }
}

/// Availability of gated content behind a detected UI gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiGatePresence {
    /// All gated items in this descriptor are eligible in-DOM disclosures recoverable via `--expand`.
    InDom,
    /// One or more gated controls have missing, ambiguous, positional-only, or empty panels.
    Unmounted,
    /// Descriptor contains both recoverable in-DOM panels and unmounted controls.
    Mixed,
    /// Visual/opaque surface (`<canvas>`) or modal obstruction.
    Opaque,
}

impl UiGatePresence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InDom => "in_dom",
            Self::Unmounted => "unmounted",
            Self::Mixed => "mixed",
            Self::Opaque => "opaque",
        }
    }
}

/// Read-only descriptor for a gated interactive control or opaque surface inside content landmarks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiGateDescriptor {
    /// Deterministic identifier (`g_<8hex>`) computed from length-delimited canonical fields.
    pub identifier: String,
    pub kind: UiGateKind,
    pub presence: UiGatePresence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_heading: Option<String>,
    /// Visible labels of in-DOM recoverable controls (capped at `MAX_UI_GATE_LABELS`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_dom_labels: Vec<String>,
    /// Visible (or accessible, for `Opaque`) labels of unmounted/opaque controls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmounted_labels: Vec<String>,
    /// Total count of currently gated items represented (including items beyond the label cap).
    pub gated_count: usize,
    /// True when `MAX_EXPANDED_PANELS` capped further in-DOM expansion.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_label: Option<String>,
    /// Always `true` (page-authored labels and headings are untrusted data).
    pub untrusted_content: bool,
}

fn push_len_prefixed(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn push_opt_len_prefixed(buf: &mut Vec<u8>, opt: Option<&str>) {
    match opt {
        Some(s) => {
            buf.push(1);
            push_len_prefixed(buf, s.as_bytes());
        }
        None => buf.push(0),
    }
}

fn push_slice_len_prefixed(buf: &mut Vec<u8>, items: &[String]) {
    buf.extend_from_slice(&(items.len() as u64).to_le_bytes());
    for item in items {
        push_len_prefixed(buf, item.as_bytes());
    }
}

/// Compute a deterministic length-delimited SHA-256 identifier (`g_<8hex>`) for a UI gate.
#[allow(clippy::too_many_arguments)]
pub fn ui_gate_identifier(
    kind: UiGateKind,
    presence: UiGatePresence,
    section_heading: Option<&str>,
    active_label: Option<&str>,
    in_dom_labels: &[String],
    unmounted_labels: &[String],
    capped: bool,
    occurrence_index: usize,
) -> String {
    let mut buf = Vec::with_capacity(128);
    push_len_prefixed(&mut buf, kind.as_str().as_bytes());
    push_len_prefixed(&mut buf, presence.as_str().as_bytes());
    push_opt_len_prefixed(&mut buf, section_heading);
    push_opt_len_prefixed(&mut buf, active_label);
    push_slice_len_prefixed(&mut buf, in_dom_labels);
    push_slice_len_prefixed(&mut buf, unmounted_labels);
    buf.push(u8::from(capped));
    buf.extend_from_slice(&(occurrence_index as u64).to_le_bytes());
    let digest = ring::digest::digest(&ring::digest::SHA256, &buf);
    let b = digest.as_ref();
    format!("g_{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

fn plural(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

fn sanitize_cue_label(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_UI_GATE_LABEL_BYTES));
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch < '\x20'
            || ch == '\x7f'
            || super::is_concealed_unicode(ch)
            || matches!(ch, '\u{061C}' | '\u{200E}'..='\u{200F}')
        {
            continue;
        }
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if ch == '"' {
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_UI_GATE_LABEL_BYTES {
        return Some(trimmed.to_string());
    }
    let end = floor_char_boundary(trimmed, MAX_UI_GATE_LABEL_BYTES);
    Some(trimmed[..end].trim_end().to_string())
}

/// Format a concise single-line `UI-Gated:` cue separating the action summary from untrusted labels.
pub fn format_ui_gates_cue_with_hint(
    gates: &[UiGateDescriptor],
    allow_expand_hint: bool,
    expand_flag_hint: Option<&str>,
    escalate_hint: &str,
) -> Option<String> {
    if gates.is_empty() {
        return None;
    }

    let mut in_dom_tabs = 0usize;
    let mut in_dom_disclosures = 0usize;
    let mut unmounted_tabs = 0usize;
    let mut unmounted_disclosures = 0usize;
    let mut pagination_count = 0usize;
    let mut modal_count = 0usize;
    let mut canvas_count = 0usize;
    let mut any_capped = false;
    let mut has_in_dom_disclosure = false;
    let mut visible_labels: Vec<String> = Vec::new();

    for gate in gates {
        any_capped |= gate.capped;
        match gate.kind {
            UiGateKind::Tablist => {
                in_dom_tabs += gate.in_dom_labels.len();
                let rem = gate.gated_count.saturating_sub(gate.in_dom_labels.len());
                unmounted_tabs += rem;
                for label in gate
                    .in_dom_labels
                    .iter()
                    .chain(gate.unmounted_labels.iter())
                {
                    if visible_labels.len() < MAX_CUE_LABELS {
                        if let Some(clean) = sanitize_cue_label(label) {
                            visible_labels.push(format!("\"{clean}\""));
                        }
                    }
                }
            }
            UiGateKind::Disclosure => {
                if !gate.in_dom_labels.is_empty() {
                    has_in_dom_disclosure = true;
                }
                in_dom_disclosures += gate.in_dom_labels.len();
                let rem = gate.gated_count.saturating_sub(gate.in_dom_labels.len());
                unmounted_disclosures += rem;
                for label in gate
                    .in_dom_labels
                    .iter()
                    .chain(gate.unmounted_labels.iter())
                {
                    if visible_labels.len() < MAX_CUE_LABELS {
                        if let Some(clean) = sanitize_cue_label(label) {
                            visible_labels.push(format!("\"{clean}\""));
                        }
                    }
                }
            }
            UiGateKind::Pagination => {
                pagination_count += gate.gated_count;
                for label in &gate.unmounted_labels {
                    if visible_labels.len() < MAX_CUE_LABELS {
                        if let Some(clean) = sanitize_cue_label(label) {
                            visible_labels.push(format!("\"{clean}\""));
                        }
                    }
                }
            }
            // Omit `aria-label`-only opaque widget names from the instruction cue.
            UiGateKind::ModalOverlay => modal_count += gate.gated_count,
            UiGateKind::VisualCanvas => canvas_count += gate.gated_count,
        }
    }

    let has_non_in_dom =
        unmounted_tabs + unmounted_disclosures + pagination_count + modal_count + canvas_count > 0;
    let in_dom_total = in_dom_tabs + in_dom_disclosures;

    let mut summary_parts = Vec::new();
    if in_dom_tabs > 0 {
        summary_parts.push(format!("{} in DOM", plural(in_dom_tabs, "tab", "tabs")));
    }
    if in_dom_disclosures > 0 {
        summary_parts.push(format!(
            "{} in DOM",
            plural(in_dom_disclosures, "disclosure", "disclosures")
        ));
    }
    if unmounted_tabs > 0 {
        summary_parts.push(plural(unmounted_tabs, "unmounted tab", "unmounted tabs"));
    }
    if unmounted_disclosures > 0 {
        summary_parts.push(plural(
            unmounted_disclosures,
            "unmounted disclosure",
            "unmounted disclosures",
        ));
    }
    if pagination_count > 0 {
        summary_parts.push(plural(
            pagination_count,
            "load-more control",
            "load-more controls",
        ));
    }
    if modal_count > 0 {
        summary_parts.push(plural(modal_count, "modal overlay", "modal overlays"));
    }
    if canvas_count > 0 {
        summary_parts.push(plural(canvas_count, "visual canvas", "visual canvases"));
    }
    if summary_parts.is_empty() {
        return None;
    }

    let action = if any_capped && !has_non_in_dom {
        return Some(format!(
            "UI-Gated: {} additional in-DOM panels omitted by 32-panel cap",
            in_dom_total
        ));
    } else if !has_non_in_dom && allow_expand_hint {
        let mode_flag = expand_flag_hint.unwrap_or(if has_in_dom_disclosure {
            "--expand all"
        } else {
            "--expand tabs"
        });
        format!("pass {mode_flag}")
    } else if has_non_in_dom {
        escalate_hint.to_string()
    } else {
        "pass --json for gate details".to_string()
    };

    let head = format!("UI-Gated: {} ({action})", summary_parts.join(", "));
    let mut full = if visible_labels.is_empty() {
        head
    } else {
        format!("{head} | untrusted labels: {}", visible_labels.join(", "))
    };
    if full.len() > MAX_CUE_BYTES {
        let cut = floor_char_boundary(&full, MAX_CUE_BYTES - 3);
        full.truncate(cut);
        full.push_str("...");
    }
    Some(full)
}

/// Format the default CLI `UI-Gated:` cue line.
pub fn format_ui_gates_cue(gates: &[UiGateDescriptor], allow_expand_hint: bool) -> Option<String> {
    format_ui_gates_cue_with_hint(
        gates,
        allow_expand_hint,
        None,
        "escalate to browser or attach via --no-navigate",
    )
}
