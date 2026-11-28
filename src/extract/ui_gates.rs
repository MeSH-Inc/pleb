use std::collections::{HashMap, HashSet};

use ego_tree::{NodeId, NodeRef};
use scraper::{Html, Node};
use serde::{Deserialize, Serialize};

use super::dom::{
    classify_element_visibility, contains_heading_or_landmark, doc_has_content_landmark,
    extract_bounded_visible_text, has_boilerplate_pattern_attr, has_h1_descendant, is_heading_tag,
    is_inert_tag, is_landmark_element, is_protected_container, parse_html_dimension,
    select_root_id, ElementVisibility, NodeMetrics, RescuedPanelKind, RescuedPanelMeta,
};
use super::{floor_char_boundary, ExpandPolicy, ExtractMode, OutputFormat};

pub const MAX_UI_GATES: usize = 16;
pub const MAX_UI_GATE_LABELS: usize = 16;
pub const MAX_EXPANDED_PANELS: usize = 32;
pub(crate) const MAX_UI_GATE_LABEL_BYTES: usize = 64;
pub(crate) const MAX_UI_GATE_HEADING_BYTES: usize = 120;

const MAX_CUE_LABELS: usize = 6;
const MAX_CUE_BYTES: usize = 220;

/// Kind of interactive UI gate detected inside document content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        if super::webmcp::is_stripped_control(ch) {
            if matches!(ch, '\t' | '\n' | '\r') && !out.is_empty() {
                pending_space = true;
            }
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
                match gate.presence {
                    UiGatePresence::InDom => in_dom_tabs += gate.gated_count,
                    UiGatePresence::Unmounted | UiGatePresence::Opaque => {
                        unmounted_tabs += gate.gated_count;
                    }
                    UiGatePresence::Mixed => {
                        in_dom_tabs += gate.in_dom_labels.len();
                        unmounted_tabs += gate.gated_count.saturating_sub(gate.in_dom_labels.len());
                    }
                }
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
                match gate.presence {
                    UiGatePresence::InDom => in_dom_disclosures += gate.gated_count,
                    UiGatePresence::Unmounted | UiGatePresence::Opaque => {
                        unmounted_disclosures += gate.gated_count;
                    }
                    UiGatePresence::Mixed => {
                        in_dom_disclosures += gate.in_dom_labels.len();
                        unmounted_disclosures +=
                            gate.gated_count.saturating_sub(gate.in_dom_labels.len());
                    }
                }
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
            "UI-Gated: {} additional in-DOM panels omitted by {MAX_EXPANDED_PANELS}-panel cap",
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

// ── Single-pass O(N) disclosure & UI-gate analyzer ─────────────

pub(crate) struct DisclosurePlan {
    pub(crate) baseline_root_id: Option<NodeId>,
    pub(crate) rescued_panels: HashMap<NodeId, RescuedPanelMeta>,
    pub(crate) pending_gates: Vec<PendingUiGate>,
}

pub(crate) struct PendingUiGate {
    kind: UiGateKind,
    section_heading: Option<String>,
    active_label: Option<String>,
    capped: bool,
    opaque_only: bool,
    items: Vec<PendingGateItem>,
}

enum PendingGateItem {
    Expanded {
        panel_ids: Vec<NodeId>,
        label: String,
    },
    InDom {
        label: String,
    },
    Unmounted {
        label: String,
    },
    Opaque {
        label: Option<String>,
    },
}

struct NodeAnalysis {
    enter: u32,
    exit: u32,
    visibility: ElementVisibility,
    has_boilerplate_attr: bool,
    in_boilerplate_ancestor: bool,
    enclosing_landmark: Option<NodeId>,
    nearest_tablist: Option<NodeId>,
    ancestor_hard_excluded: bool,
    ancestor_hidden_chain: Vec<NodeId>,
    visible_text_bytes: usize,
    structural_text_bytes: usize,
}

#[derive(Clone)]
struct TabCandidate {
    node_id: NodeId,
    group_key: TabGroupKey,
    heading_level: u8,
    section_heading: Option<String>,
    aria_selected: Option<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TabGroupKey {
    Tablist(NodeId),
    Parent(NodeId),
}

struct DisclosureCandidate {
    node_id: NodeId,
    heading_level: u8,
    section_heading: Option<String>,
}

struct PaginationCandidate {
    node_id: NodeId,
    section_heading: Option<String>,
}

struct ModalCandidate {
    node_id: NodeId,
    section_heading: Option<String>,
}

struct CanvasCandidate {
    node_id: NodeId,
    section_heading: Option<String>,
    enclosing_section: Option<NodeId>,
}

struct AnalyzerState<'a> {
    id_counts: HashMap<&'a str, usize>,
    unique_id_to_node: HashMap<&'a str, NodeId>,
    aria_labelledby_panels: HashMap<&'a str, Vec<NodeId>>,
    nodes: HashMap<NodeId, NodeAnalysis>,
    baseline_metrics: HashMap<NodeId, NodeMetrics>,
    tabs: Vec<TabCandidate>,
    tabpanels: HashSet<NodeId>,
    disclosure_buttons: Vec<DisclosureCandidate>,
    pagination_buttons: Vec<PaginationCandidate>,
    open_modals: Vec<ModalCandidate>,
    canvases: Vec<CanvasCandidate>,
    seq: u32,
    current_heading_level: u8,
    current_section_heading: Option<String>,
    hidden_chain_stack: Vec<NodeId>,
    parent_tab_counts: HashMap<NodeId, usize>,
}

#[derive(Clone, Copy)]
struct WalkContext {
    enclosing_landmark: Option<NodeId>,
    enclosing_section: Option<NodeId>,
    nearest_tablist: Option<NodeId>,
    in_boilerplate: bool,
    in_form: bool,
    ancestor_hard_excluded: bool,
    in_link: bool,
    in_code: bool,
    in_visible_subtree: bool,
    in_structural_subtree: bool,
}

pub(crate) fn plan_disclosures_and_gates(
    doc: &Html,
    mode: ExtractMode,
    expand: ExpandPolicy,
    format: OutputFormat,
) -> DisclosurePlan {
    if !matches!(mode, ExtractMode::Clean | ExtractMode::Article) {
        return DisclosurePlan {
            baseline_root_id: None,
            rescued_panels: HashMap::new(),
            pending_gates: Vec::new(),
        };
    }

    let effective_expand = match format {
        OutputFormat::Markdown | OutputFormat::Text => expand,
        OutputFormat::Html | OutputFormat::Axtree | OutputFormat::Raw => ExpandPolicy::None,
    };

    let mut state = AnalyzerState {
        id_counts: HashMap::new(),
        unique_id_to_node: HashMap::new(),
        aria_labelledby_panels: HashMap::new(),
        nodes: HashMap::new(),
        baseline_metrics: HashMap::new(),
        tabs: Vec::new(),
        tabpanels: HashSet::new(),
        disclosure_buttons: Vec::new(),
        pagination_buttons: Vec::new(),
        open_modals: Vec::new(),
        canvases: Vec::new(),
        seq: 0,
        current_heading_level: 2,
        current_section_heading: None,
        hidden_chain_stack: Vec::new(),
        parent_tab_counts: HashMap::new(),
    };

    let root_ctx = WalkContext {
        enclosing_landmark: None,
        enclosing_section: None,
        nearest_tablist: None,
        in_boilerplate: false,
        in_form: false,
        ancestor_hard_excluded: false,
        in_link: false,
        in_code: false,
        in_visible_subtree: true,
        in_structural_subtree: true,
    };

    if let Some(root) = doc.tree.get(doc.root_element().id()) {
        analyze_tree(root, root_ctx, &mut state);
    }

    let baseline_root_id = select_root_id(doc, mode, &state.baseline_metrics);
    let doc_has_landmark = doc_has_content_landmark(doc);

    let mut rescued_panels: HashMap<NodeId, RescuedPanelMeta> = HashMap::new();
    let mut claimed_panels: HashSet<NodeId> = HashSet::new();
    let mut rescued_hidden_count = 0usize;
    let mut pending_gates: Vec<PendingUiGate> = Vec::new();

    // 1. Resolve tablists in document order.
    let mut tab_groups: Vec<TabGroupKey> = Vec::new();
    let mut grouped_tabs: HashMap<TabGroupKey, Vec<TabCandidate>> = HashMap::new();
    for tab in &state.tabs {
        if let TabGroupKey::Parent(pid) = tab.group_key {
            if state.parent_tab_counts.get(&pid).copied().unwrap_or(0) < 2 {
                continue;
            }
        }
        if !grouped_tabs.contains_key(&tab.group_key) {
            tab_groups.push(tab.group_key);
        }
        grouped_tabs
            .entry(tab.group_key)
            .or_default()
            .push(tab.clone());
    }

    for group_key in tab_groups {
        let Some(group) = grouped_tabs.get(&group_key) else {
            continue;
        };
        let mut eligible_tabs: Vec<(&TabCandidate, String)> = Vec::new();
        for tab in group {
            let Some(t_meta) = state.nodes.get(&tab.node_id) else {
                continue;
            };
            if !node_inside(t_meta, baseline_root_id, &state.nodes)
                || t_meta.in_boilerplate_ancestor
                || t_meta.has_boilerplate_attr
                || t_meta.ancestor_hard_excluded
                || !t_meta.ancestor_hidden_chain.is_empty()
                || t_meta.visibility != ElementVisibility::Visible
                || (doc_has_landmark && t_meta.enclosing_landmark.is_none())
            {
                continue;
            }
            let Some(node) = doc.tree.get(tab.node_id) else {
                continue;
            };
            if let Some(label) = extract_bounded_visible_text(node, MAX_UI_GATE_LABEL_BYTES) {
                eligible_tabs.push((tab, label));
            }
        }
        if eligible_tabs.is_empty() {
            continue;
        }

        let any_selected = eligible_tabs
            .iter()
            .any(|(t, _)| t.aria_selected == Some(true));
        let active_label = eligible_tabs
            .iter()
            .find(|(t, _)| t.aria_selected == Some(true))
            .map(|(_, l)| l.clone());
        let group_heading_level = (eligible_tabs[0].0.heading_level + 1).clamp(2, 6);
        let group_section = eligible_tabs[0].0.section_heading.clone();

        let mut active_panel_candidate: Option<(NodeId, String)> = None;
        let mut expanded_any_hidden_in_group = false;
        let mut items: Vec<PendingGateItem> = Vec::new();
        let mut group_capped = false;

        for (tab, label) in &eligible_tabs {
            let is_active = tab.aria_selected == Some(true) || !any_selected;
            let resolved_panel = resolve_tab_panel(
                doc,
                tab.node_id,
                baseline_root_id,
                doc_has_landmark,
                &state,
                &claimed_panels,
            );

            if is_active && tab.aria_selected == Some(true) {
                if let Some(p_id) = resolved_panel {
                    if let Some(p_meta) = state.nodes.get(&p_id) {
                        if p_meta.visibility == ElementVisibility::Visible
                            && p_meta.ancestor_hidden_chain.is_empty()
                        {
                            active_panel_candidate = Some((p_id, label.clone()));
                        }
                    }
                }
                continue;
            }
            if !any_selected {
                // Without `aria-selected="true"`, treat visible paired panels as active and hidden ones as gated.
                if let Some(p_id) = resolved_panel {
                    if let Some(p_meta) = state.nodes.get(&p_id) {
                        if p_meta.visibility == ElementVisibility::Visible
                            && p_meta.ancestor_hidden_chain.is_empty()
                        {
                            if active_panel_candidate.is_none() {
                                active_panel_candidate = Some((p_id, label.clone()));
                            }
                            continue;
                        }
                    }
                } else {
                    continue;
                }
            }

            let Some(p_id) = resolved_panel else {
                items.push(PendingGateItem::Unmounted {
                    label: label.clone(),
                });
                continue;
            };
            let Some(p_meta) = state.nodes.get(&p_id) else {
                items.push(PendingGateItem::Unmounted {
                    label: label.clone(),
                });
                continue;
            };

            if p_meta.visibility == ElementVisibility::Visible
                && p_meta.ancestor_hidden_chain.is_empty()
            {
                continue;
            }

            let chain_ok = p_meta
                .ancestor_hidden_chain
                .iter()
                .all(|anc| rescued_panels.contains_key(anc));
            let panel_eligible = !p_meta.ancestor_hard_excluded
                && p_meta.visibility != ElementVisibility::HardExcluded
                && chain_ok
                && p_meta.structural_text_bytes >= 1;

            if !panel_eligible {
                items.push(PendingGateItem::Unmounted {
                    label: label.clone(),
                });
                continue;
            }

            if matches!(effective_expand, ExpandPolicy::Tabs | ExpandPolicy::All)
                && rescued_hidden_count < MAX_EXPANDED_PANELS
            {
                rescued_hidden_count += 1;
                expanded_any_hidden_in_group = true;
                claimed_panels.insert(p_id);
                rescued_panels.insert(
                    p_id,
                    RescuedPanelMeta {
                        kind: RescuedPanelKind::Tab,
                        label: label.clone(),
                        was_hidden: true,
                        heading_level: group_heading_level,
                    },
                );
                items.push(PendingGateItem::Expanded {
                    panel_ids: vec![p_id],
                    label: label.clone(),
                });
            } else {
                if matches!(effective_expand, ExpandPolicy::Tabs | ExpandPolicy::All) {
                    group_capped = true;
                }
                items.push(PendingGateItem::InDom {
                    label: label.clone(),
                });
            }
        }

        if expanded_any_hidden_in_group {
            if let Some((act_id, act_label)) = active_panel_candidate {
                rescued_panels.entry(act_id).or_insert_with(|| {
                    claimed_panels.insert(act_id);
                    RescuedPanelMeta {
                        kind: RescuedPanelKind::Tab,
                        label: act_label,
                        was_hidden: false,
                        heading_level: group_heading_level,
                    }
                });
            }
        }

        if !items.is_empty() {
            pending_gates.push(PendingUiGate {
                kind: UiGateKind::Tablist,
                section_heading: group_section,
                active_label,
                capped: group_capped,
                opaque_only: false,
                items,
            });
        }
    }

    // 2. Resolve disclosure buttons (`aria-expanded="false"`, no `aria-haspopup`), grouped by section_heading.
    let mut pending_disclosures: Option<PendingUiGate> = None;
    for disc in &state.disclosure_buttons {
        let Some(c_meta) = state.nodes.get(&disc.node_id) else {
            continue;
        };
        if !node_inside(c_meta, baseline_root_id, &state.nodes)
            || c_meta.in_boilerplate_ancestor
            || c_meta.has_boilerplate_attr
            || c_meta.ancestor_hard_excluded
            || !c_meta.ancestor_hidden_chain.is_empty()
            || c_meta.visibility != ElementVisibility::Visible
            || (doc_has_landmark && c_meta.enclosing_landmark.is_none())
        {
            continue;
        }
        let Some(c_node) = doc.tree.get(disc.node_id) else {
            continue;
        };
        let Some(label) = extract_bounded_visible_text(c_node, MAX_UI_GATE_LABEL_BYTES) else {
            continue;
        };
        let targets = resolve_disclosure_targets(
            c_node,
            c_meta,
            baseline_root_id,
            doc_has_landmark,
            &state,
            &claimed_panels,
        );

        let mut flush_group = false;
        if let Some(ref current) = pending_disclosures {
            if current.section_heading != disc.section_heading {
                flush_group = true;
            }
        }
        if flush_group {
            if let Some(g) = pending_disclosures.take() {
                if !g.items.is_empty() {
                    pending_gates.push(g);
                }
            }
        }
        let group = pending_disclosures.get_or_insert_with(|| PendingUiGate {
            kind: UiGateKind::Disclosure,
            section_heading: disc.section_heading.clone(),
            active_label: None,
            capped: false,
            opaque_only: false,
            items: Vec::new(),
        });

        match targets {
            Some(panel_ids) if !panel_ids.is_empty() => {
                let all_eligible = panel_ids.iter().all(|pid| {
                    let Some(pm) = state.nodes.get(pid) else {
                        return false;
                    };
                    let chain_ok = pm
                        .ancestor_hidden_chain
                        .iter()
                        .all(|anc| rescued_panels.contains_key(anc));
                    let hidden_or_collapsed = pm.visibility
                        == ElementVisibility::StructuralDisclosureHidden
                        || !pm.ancestor_hidden_chain.is_empty()
                        || (!doc_has_landmark
                            && doc
                                .tree
                                .get(*pid)
                                .is_some_and(|n| !contains_heading_or_landmark(n)));
                    !pm.ancestor_hard_excluded
                        && pm.visibility != ElementVisibility::HardExcluded
                        && chain_ok
                        && pm.structural_text_bytes >= 1
                        && hidden_or_collapsed
                });

                if all_eligible {
                    if effective_expand == ExpandPolicy::All
                        && rescued_hidden_count + panel_ids.len() <= MAX_EXPANDED_PANELS
                    {
                        let lvl = (disc.heading_level + 1).clamp(2, 6);
                        for (idx, &pid) in panel_ids.iter().enumerate() {
                            rescued_hidden_count += 1;
                            claimed_panels.insert(pid);
                            let panel_label = if idx == 0 {
                                label.clone()
                            } else {
                                format!("{label} ({})", idx + 1)
                            };
                            rescued_panels.insert(
                                pid,
                                RescuedPanelMeta {
                                    kind: RescuedPanelKind::Disclosure,
                                    label: panel_label,
                                    was_hidden: true,
                                    heading_level: lvl,
                                },
                            );
                        }
                        group
                            .items
                            .push(PendingGateItem::Expanded { panel_ids, label });
                    } else {
                        if effective_expand == ExpandPolicy::All {
                            group.capped = true;
                        }
                        group.items.push(PendingGateItem::InDom { label });
                    }
                } else if label.chars().filter(|c| c.is_alphabetic()).count() >= 3 {
                    group.items.push(PendingGateItem::Unmounted { label });
                }
            }
            _ => {
                if label.chars().filter(|c| c.is_alphabetic()).count() >= 3 {
                    group.items.push(PendingGateItem::Unmounted { label });
                }
            }
        }
    }
    if let Some(g) = pending_disclosures.take() {
        if !g.items.is_empty() {
            pending_gates.push(g);
        }
    }

    // 3. Pagination controls.
    for pag in &state.pagination_buttons {
        let Some(meta) = state.nodes.get(&pag.node_id) else {
            continue;
        };
        if !node_inside(meta, baseline_root_id, &state.nodes)
            || meta.in_boilerplate_ancestor
            || meta.has_boilerplate_attr
            || meta.ancestor_hard_excluded
            || !meta.ancestor_hidden_chain.is_empty()
            || meta.visibility != ElementVisibility::Visible
            || (doc_has_landmark && meta.enclosing_landmark.is_none())
        {
            continue;
        }
        let Some(node) = doc.tree.get(pag.node_id) else {
            continue;
        };
        let Some(label) = extract_bounded_visible_text(node, MAX_UI_GATE_LABEL_BYTES) else {
            continue;
        };
        if is_pagination_label(&label) {
            pending_gates.push(PendingUiGate {
                kind: UiGateKind::Pagination,
                section_heading: pag.section_heading.clone(),
                active_label: None,
                capped: false,
                opaque_only: false,
                items: vec![PendingGateItem::Unmounted { label }],
            });
        }
    }

    // 4. Open modal overlays blocking sparse prose.
    let root_total_visible = state
        .nodes
        .get(&baseline_root_id)
        .map(|m| m.visible_text_bytes)
        .unwrap_or(0);
    for modal in &state.open_modals {
        let Some(m_meta) = state.nodes.get(&modal.node_id) else {
            continue;
        };
        if !node_inside(m_meta, baseline_root_id, &state.nodes)
            || m_meta.has_boilerplate_attr
            || m_meta.ancestor_hard_excluded
            || !m_meta.ancestor_hidden_chain.is_empty()
            || m_meta.visibility != ElementVisibility::Visible
        {
            continue;
        }
        let modal_visible = m_meta.visible_text_bytes;
        let non_dialog_prose = root_total_visible.saturating_sub(modal_visible);
        if non_dialog_prose < 400 {
            let label = doc.tree.get(modal.node_id).and_then(|n| {
                extract_heading_inside(n, MAX_UI_GATE_LABEL_BYTES).or_else(|| {
                    n.value()
                        .as_element()
                        .and_then(|el| el.attr("aria-label"))
                        .and_then(sanitize_cue_label)
                })
            });
            pending_gates.push(PendingUiGate {
                kind: UiGateKind::ModalOverlay,
                section_heading: modal.section_heading.clone(),
                active_label: None,
                capped: false,
                opaque_only: true,
                items: vec![PendingGateItem::Opaque { label }],
            });
        }
    }

    // 5. Visual canvases in sparse sections/landmarks.
    let mut qualifying_canvases: Vec<(&CanvasCandidate, Option<String>)> = Vec::new();
    for canvas in &state.canvases {
        let Some(c_meta) = state.nodes.get(&canvas.node_id) else {
            continue;
        };
        if !node_inside(c_meta, baseline_root_id, &state.nodes)
            || c_meta.in_boilerplate_ancestor
            || c_meta.has_boilerplate_attr
            || c_meta.ancestor_hard_excluded
            || !c_meta.ancestor_hidden_chain.is_empty()
            || c_meta.visibility != ElementVisibility::Visible
            || (doc_has_landmark && c_meta.enclosing_landmark.is_none())
        {
            continue;
        }
        let Some(node) = doc.tree.get(canvas.node_id) else {
            continue;
        };
        let Some(el) = node.value().as_element() else {
            continue;
        };
        let large_dims = el
            .attr("width")
            .and_then(parse_html_dimension)
            .is_some_and(|w| w >= 200)
            && el
                .attr("height")
                .and_then(parse_html_dimension)
                .is_some_and(|h| h >= 120);
        let role_img = el
            .attr("role")
            .is_some_and(|r| r.trim().eq_ignore_ascii_case("img"));
        let aria_label = el.attr("aria-label").and_then(sanitize_cue_label);
        if large_dims || role_img || aria_label.is_some() {
            qualifying_canvases.push((canvas, aria_label));
        }
    }

    let canvas_count = qualifying_canvases.len();
    for (canvas, label) in qualifying_canvases {
        let section_prose = canvas
            .enclosing_section
            .and_then(|sid| state.nodes.get(&sid))
            .map(|m| m.visible_text_bytes)
            .unwrap_or(root_total_visible);
        if section_prose < 600 || canvas_count >= 2 {
            pending_gates.push(PendingUiGate {
                kind: UiGateKind::VisualCanvas,
                section_heading: canvas.section_heading.clone(),
                active_label: None,
                capped: false,
                opaque_only: true,
                items: vec![PendingGateItem::Opaque { label }],
            });
        }
    }

    DisclosurePlan {
        baseline_root_id: Some(baseline_root_id),
        rescued_panels,
        pending_gates,
    }
}

pub(crate) fn finalize_ui_gates(
    pending_gates: &[PendingUiGate],
    surviving_rescued: &HashSet<NodeId>,
) -> Vec<UiGateDescriptor> {
    let mut out = Vec::new();
    let mut occurrences: HashMap<(UiGateKind, Option<&str>), usize> = HashMap::new();

    for gate in pending_gates {
        if out.len() >= MAX_UI_GATES {
            break;
        }
        let mut in_dom_labels: Vec<String> = Vec::new();
        let mut unmounted_labels: Vec<String> = Vec::new();
        let mut in_dom_count = 0usize;
        let mut unmounted_count = 0usize;

        for item in &gate.items {
            match item {
                PendingGateItem::Expanded { panel_ids, label } => {
                    let all_survived = panel_ids.iter().all(|id| surviving_rescued.contains(id));
                    if !all_survived {
                        unmounted_count += 1;
                        if unmounted_labels.len() < MAX_UI_GATE_LABELS {
                            unmounted_labels.push(label.clone());
                        }
                    }
                }
                PendingGateItem::InDom { label } => {
                    in_dom_count += 1;
                    if in_dom_labels.len() < MAX_UI_GATE_LABELS {
                        in_dom_labels.push(label.clone());
                    }
                }
                PendingGateItem::Unmounted { label } => {
                    unmounted_count += 1;
                    if unmounted_labels.len() < MAX_UI_GATE_LABELS {
                        unmounted_labels.push(label.clone());
                    }
                }
                PendingGateItem::Opaque { label } => {
                    unmounted_count += 1;
                    if let Some(l) = label {
                        if unmounted_labels.len() < MAX_UI_GATE_LABELS {
                            unmounted_labels.push(l.clone());
                        }
                    }
                }
            }
        }

        let gated_count = in_dom_count + unmounted_count;
        if gated_count == 0 {
            continue;
        }

        let presence = if gate.opaque_only {
            UiGatePresence::Opaque
        } else if in_dom_count > 0 && unmounted_count > 0 {
            UiGatePresence::Mixed
        } else if in_dom_count > 0 {
            UiGatePresence::InDom
        } else {
            UiGatePresence::Unmounted
        };

        let occ_entry = occurrences
            .entry((gate.kind, gate.section_heading.as_deref()))
            .or_insert(0);
        let occ_idx = *occ_entry;
        *occ_entry += 1;

        let identifier = ui_gate_identifier(
            gate.kind,
            presence,
            gate.section_heading.as_deref(),
            gate.active_label.as_deref(),
            &in_dom_labels,
            &unmounted_labels,
            gate.capped,
            occ_idx,
        );

        out.push(UiGateDescriptor {
            identifier,
            kind: gate.kind,
            presence,
            section_heading: gate.section_heading.clone(),
            in_dom_labels,
            unmounted_labels,
            gated_count,
            capped: gate.capped,
            active_label: gate.active_label.clone(),
            untrusted_content: true,
        });
    }

    out
}

fn analyze_tree<'a>(
    node: NodeRef<'a, Node>,
    ctx: WalkContext,
    state: &mut AnalyzerState<'a>,
) -> (NodeMetrics, usize, usize) {
    match node.value() {
        Node::Text(text) => {
            let bytes: usize = text.split_whitespace().map(str::len).sum();
            let mut m = NodeMetrics::default();
            if ctx.in_visible_subtree {
                if ctx.in_code {
                    m.code_bytes = bytes;
                } else {
                    m.prose_bytes = bytes;
                    if ctx.in_link {
                        m.link_text_bytes = bytes;
                    }
                }
            }
            let vis_bytes = if ctx.in_visible_subtree { bytes } else { 0 };
            let struct_bytes = if ctx.in_structural_subtree { bytes } else { 0 };
            (m, vis_bytes, struct_bytes)
        }
        Node::Element(el) => {
            let tag = el.name();
            if is_inert_tag(tag) || tag == "svg" {
                return (NodeMetrics::default(), 0, 0);
            }

            let enter = state.seq;
            state.seq += 1;

            if let Some(id) = el.attr("id").filter(|id| !id.is_empty()) {
                let cnt = state.id_counts.entry(id).or_insert(0);
                *cnt += 1;
                if *cnt == 1 {
                    state.unique_id_to_node.insert(id, node.id());
                } else {
                    state.unique_id_to_node.remove(id);
                }
            }

            let visibility = classify_element_visibility(el);
            let has_bp_attr = has_boilerplate_pattern_attr(el.attr("id"), el.attr("class"));
            let role = el.attr("role").map(str::trim);

            let is_open_modal = (tag == "dialog" && el.attr("open").is_some())
                || (role.is_some_and(|r| r.eq_ignore_ascii_case("dialog"))
                    && el
                        .attr("aria-modal")
                        .is_some_and(|m| m.trim().eq_ignore_ascii_case("true")));
            if is_open_modal {
                state.open_modals.push(ModalCandidate {
                    node_id: node.id(),
                    section_heading: state.current_section_heading.clone(),
                });
            }

            let is_bp_el = matches!(tag, "nav" | "footer" | "aside")
                || (tag == "header" && !has_h1_descendant(node))
                || role.is_some_and(|r| {
                    r.eq_ignore_ascii_case("navigation")
                        || r.eq_ignore_ascii_case("search")
                        || r.eq_ignore_ascii_case("banner")
                        || r.eq_ignore_ascii_case("contentinfo")
                        || r.eq_ignore_ascii_case("complementary")
                        || r.eq_ignore_ascii_case("menu")
                        || r.eq_ignore_ascii_case("menubar")
                        || r.eq_ignore_ascii_case("dialog")
                        || r.eq_ignore_ascii_case("tooltip")
                })
                || has_bp_attr;

            let is_landmark = is_landmark_element(el);
            let saved_heading_state = if is_landmark {
                let prev = (
                    state.current_heading_level,
                    state.current_section_heading.clone(),
                );
                state.current_heading_level = 2;
                state.current_section_heading = None;
                Some(prev)
            } else {
                None
            };
            let enclosing_landmark = if is_landmark {
                Some(node.id())
            } else {
                ctx.enclosing_landmark
            };
            let enclosing_section = if is_landmark || tag == "section" {
                Some(node.id())
            } else {
                ctx.enclosing_section
            };

            let is_tablist = role.is_some_and(|r| r.eq_ignore_ascii_case("tablist"));
            let nearest_tablist = if is_tablist {
                Some(node.id())
            } else {
                ctx.nearest_tablist
            };

            let ancestor_hard_excluded = ctx.ancestor_hard_excluded;
            let ancestor_hidden_chain = state.hidden_chain_stack.clone();
            let in_boilerplate_ancestor = ctx.in_boilerplate;

            let child_in_visible =
                ctx.in_visible_subtree && visibility == ElementVisibility::Visible;
            let child_in_structural =
                ctx.in_structural_subtree && visibility != ElementVisibility::HardExcluded;

            if child_in_visible && !in_boilerplate_ancestor && !is_bp_el && is_heading_tag(tag) {
                if let Some(h_lvl) = tag.as_bytes().get(1).and_then(|b| b.checked_sub(b'0')) {
                    state.current_heading_level = h_lvl;
                }
                if let Some(h_txt) = extract_bounded_visible_text(node, MAX_UI_GATE_HEADING_BYTES) {
                    state.current_section_heading = Some(h_txt);
                }
            }

            if role.is_some_and(|r| r.eq_ignore_ascii_case("tabpanel")) {
                state.tabpanels.insert(node.id());
                if let Some(labelled) = el.attr("aria-labelledby") {
                    for tok in labelled.split_whitespace() {
                        state
                            .aria_labelledby_panels
                            .entry(tok)
                            .or_default()
                            .push(node.id());
                    }
                }
            }

            let is_disabled = el.attr("disabled").is_some()
                || el
                    .attr("aria-disabled")
                    .is_some_and(|d| d.trim().eq_ignore_ascii_case("true"));

            if role.is_some_and(|r| r.eq_ignore_ascii_case("tab")) && !is_disabled {
                let aria_selected = el.attr("aria-selected").and_then(|v| {
                    let t = v.trim();
                    if t.eq_ignore_ascii_case("true") {
                        Some(true)
                    } else if t.eq_ignore_ascii_case("false") {
                        Some(false)
                    } else {
                        None
                    }
                });
                let group_key = if let Some(tl) = ctx.nearest_tablist {
                    TabGroupKey::Tablist(tl)
                } else {
                    let pid = node.parent().map(|p| p.id()).unwrap_or(node.id());
                    *state.parent_tab_counts.entry(pid).or_insert(0) += 1;
                    TabGroupKey::Parent(pid)
                };
                state.tabs.push(TabCandidate {
                    node_id: node.id(),
                    group_key,
                    heading_level: state.current_heading_level,
                    section_heading: state.current_section_heading.clone(),
                    aria_selected,
                });
            }

            let is_button_ctrl =
                tag == "button" || role.is_some_and(|r| r.eq_ignore_ascii_case("button"));
            if is_button_ctrl && !is_disabled {
                if el
                    .attr("aria-expanded")
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("false"))
                    && el.attr("aria-haspopup").is_none()
                {
                    state.disclosure_buttons.push(DisclosureCandidate {
                        node_id: node.id(),
                        heading_level: state.current_heading_level,
                        section_heading: state.current_section_heading.clone(),
                    });
                }
                let is_submit = ctx.in_form
                    && tag == "button"
                    && el
                        .attr("type")
                        .is_none_or(|t| t.trim().eq_ignore_ascii_case("submit"));
                if !is_submit {
                    state.pagination_buttons.push(PaginationCandidate {
                        node_id: node.id(),
                        section_heading: state.current_section_heading.clone(),
                    });
                }
            }

            if tag == "canvas" {
                state.canvases.push(CanvasCandidate {
                    node_id: node.id(),
                    section_heading: state.current_section_heading.clone(),
                    enclosing_section: ctx.enclosing_section,
                });
            }

            let is_nav_control = matches!(tag, "a" | "button");
            let entering_link = child_in_visible && is_nav_control && !ctx.in_link;
            let child_ctx = WalkContext {
                enclosing_landmark,
                enclosing_section,
                nearest_tablist,
                in_boilerplate: ctx.in_boilerplate || is_bp_el,
                in_form: ctx.in_form || tag == "form",
                ancestor_hard_excluded: ctx.ancestor_hard_excluded
                    || visibility == ElementVisibility::HardExcluded,
                in_link: ctx.in_link || is_nav_control,
                in_code: ctx.in_code || matches!(tag, "pre" | "code"),
                in_visible_subtree: child_in_visible,
                in_structural_subtree: child_in_structural,
            };

            let pushed_hidden = visibility == ElementVisibility::StructuralDisclosureHidden;
            if pushed_hidden {
                state.hidden_chain_stack.push(node.id());
            }

            let mut acc = if child_in_visible {
                NodeMetrics {
                    link_count: usize::from(entering_link),
                    heading_count: usize::from(is_heading_tag(tag)),
                    form_count: usize::from(tag == "form"),
                    has_landmark: is_protected_container(el),
                    ..Default::default()
                }
            } else {
                NodeMetrics::default()
            };
            let mut vis_bytes = 0usize;
            let mut struct_bytes = 0usize;

            for child in node.children() {
                let (cm, cv, cs) = analyze_tree(child, child_ctx, state);
                if child_in_visible {
                    acc.accumulate(cm);
                }
                vis_bytes += cv;
                struct_bytes += cs;
            }

            if pushed_hidden {
                state.hidden_chain_stack.pop();
            }
            if let Some((prev_lvl, prev_heading)) = saved_heading_state {
                state.current_heading_level = prev_lvl;
                state.current_section_heading = prev_heading;
            }

            let exit = state.seq;
            state.seq += 1;

            if child_in_visible {
                state.baseline_metrics.insert(node.id(), acc);
            }

            state.nodes.insert(
                node.id(),
                NodeAnalysis {
                    enter,
                    exit,
                    visibility,
                    has_boilerplate_attr: has_bp_attr,
                    in_boilerplate_ancestor,
                    enclosing_landmark,
                    nearest_tablist,
                    ancestor_hard_excluded,
                    ancestor_hidden_chain,
                    visible_text_bytes: vis_bytes,
                    structural_text_bytes: struct_bytes,
                },
            );

            (acc, vis_bytes, struct_bytes)
        }
        _ => {
            let mut acc = NodeMetrics::default();
            let mut vis_bytes = 0usize;
            let mut struct_bytes = 0usize;
            for child in node.children() {
                let (cm, cv, cs) = analyze_tree(child, ctx, state);
                acc.accumulate(cm);
                vis_bytes += cv;
                struct_bytes += cs;
            }
            (acc, vis_bytes, struct_bytes)
        }
    }
}

fn node_inside(
    node_meta: &NodeAnalysis,
    root_id: NodeId,
    nodes: &HashMap<NodeId, NodeAnalysis>,
) -> bool {
    let Some(root_meta) = nodes.get(&root_id) else {
        return false;
    };
    node_meta.enter >= root_meta.enter && node_meta.exit <= root_meta.exit
}

fn resolve_tab_panel(
    doc: &Html,
    tab_id: NodeId,
    baseline_root_id: NodeId,
    doc_has_landmark: bool,
    state: &AnalyzerState<'_>,
    claimed_panels: &HashSet<NodeId>,
) -> Option<NodeId> {
    let tab_node = doc.tree.get(tab_id)?;
    let tab_el = tab_node.value().as_element()?;
    let tab_meta = state.nodes.get(&tab_id)?;

    let mut forward_targets: Vec<NodeId> = Vec::new();
    let has_aria_controls = tab_el
        .attr("aria-controls")
        .is_some_and(|s| !s.trim().is_empty());
    if let Some(raw_ctrls) = tab_el.attr("aria-controls") {
        for tok in raw_ctrls.split_whitespace() {
            if state.id_counts.get(tok) != Some(&1) {
                return None;
            }
            let pid = *state.unique_id_to_node.get(tok)?;
            forward_targets.push(pid);
        }
        if forward_targets.len() != 1 {
            return None;
        }
    }

    let mut reverse_targets: Vec<NodeId> = Vec::new();
    if let Some(cid) = tab_el.attr("id").filter(|s| !s.is_empty()) {
        if state.id_counts.get(cid) == Some(&1) {
            if let Some(list) = state.aria_labelledby_panels.get(cid) {
                reverse_targets.extend(list.iter().copied());
            }
        }
    }

    let candidate_id = if !forward_targets.is_empty() {
        let pid = forward_targets[0];
        if let Some(p_el) = doc.tree.get(pid).and_then(|n| n.value().as_element()) {
            if let Some(labelled) = p_el
                .attr("aria-labelledby")
                .filter(|s| !s.trim().is_empty())
            {
                let cid = tab_el.attr("id").filter(|s| !s.is_empty())?;
                if !labelled.split_whitespace().any(|t| t == cid) {
                    return None;
                }
            }
        }
        pid
    } else if reverse_targets.len() == 1 && !has_aria_controls {
        reverse_targets[0]
    } else {
        return None;
    };

    if claimed_panels.contains(&candidate_id) || !state.tabpanels.contains(&candidate_id) {
        return None;
    }

    let p_meta = state.nodes.get(&candidate_id)?;
    if !node_inside(p_meta, baseline_root_id, &state.nodes)
        || p_meta.in_boilerplate_ancestor
        || p_meta.has_boilerplate_attr
    {
        return None;
    }
    if doc_has_landmark && p_meta.enclosing_landmark != tab_meta.enclosing_landmark {
        return None;
    }
    if p_meta.nearest_tablist.is_some() && p_meta.nearest_tablist != tab_meta.nearest_tablist {
        return None;
    }
    Some(candidate_id)
}

fn resolve_disclosure_targets(
    ctrl_node: NodeRef<'_, Node>,
    ctrl_meta: &NodeAnalysis,
    baseline_root_id: NodeId,
    doc_has_landmark: bool,
    state: &AnalyzerState<'_>,
    claimed_panels: &HashSet<NodeId>,
) -> Option<Vec<NodeId>> {
    let ctrl_el = ctrl_node.value().as_element()?;
    let raw_ctrls = ctrl_el.attr("aria-controls")?.trim();
    if raw_ctrls.is_empty() {
        return None;
    }

    let mut targets = Vec::new();
    for tok in raw_ctrls.split_whitespace() {
        if state.id_counts.get(tok) != Some(&1) {
            return None;
        }
        let pid = *state.unique_id_to_node.get(tok)?;
        if claimed_panels.contains(&pid) || targets.contains(&pid) {
            return None;
        }
        let p_node = ctrl_node.tree().get(pid)?;
        let p_el = p_node.value().as_element()?;
        if matches!(
            p_el.name(),
            "html" | "body" | "nav" | "header" | "footer" | "aside" | "dialog" | "form"
        ) || is_landmark_element(p_el)
        {
            return None;
        }
        if p_el.attr("role").is_some_and(|r| {
            let t = r.trim();
            t.eq_ignore_ascii_case("menu")
                || t.eq_ignore_ascii_case("menubar")
                || t.eq_ignore_ascii_case("listbox")
                || t.eq_ignore_ascii_case("tooltip")
                || t.eq_ignore_ascii_case("dialog")
                || t.eq_ignore_ascii_case("navigation")
        }) {
            return None;
        }
        let p_meta = state.nodes.get(&pid)?;
        if !node_inside(p_meta, baseline_root_id, &state.nodes)
            || p_meta.in_boilerplate_ancestor
            || p_meta.has_boilerplate_attr
        {
            return None;
        }
        if doc_has_landmark && p_meta.enclosing_landmark != ctrl_meta.enclosing_landmark {
            return None;
        }
        targets.push(pid);
    }

    Some(targets)
}

fn extract_heading_inside(node: NodeRef<'_, Node>, max_bytes: usize) -> Option<String> {
    for desc in node.descendants() {
        if let Some(el) = desc.value().as_element() {
            if is_heading_tag(el.name()) {
                if let Some(txt) = extract_bounded_visible_text(desc, max_bytes) {
                    return Some(txt);
                }
            }
        }
    }
    None
}

fn is_pagination_label(label: &str) -> bool {
    let lower = label.trim().to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "load more",
        "show more",
        "view more",
        "more results",
        "show all",
        "next page",
        "load previous",
        "view all",
    ];
    PATTERNS.iter().any(|pat| {
        if let Some(rest) = lower.strip_prefix(pat) {
            rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == '(' || c == ':')
        } else {
            false
        }
    })
}
