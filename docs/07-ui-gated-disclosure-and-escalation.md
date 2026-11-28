# Milestone 7 Specification: UI-Gated Disclosure Rescue, Affordance Indexing, and Composable Browser Escalation

> [!NOTE]
> Milestone 7 design specification (audit-hardened against `main` at `207c90c`). Authoritative runtime contracts live in [README-FULL.md](../README-FULL.md), [llms-full.txt](../llms-full.txt), [skills/pleb/SKILL.md](../skills/pleb/SKILL.md), and `src/` once implemented.

## 1. Purpose and Scope

`pleb` acquires public web documents through a deterministic, read-only escalation funnel (`source bypass -> Cronet/reqwest -> optional CEF v5-meta or CDP/Kitesurf -> single-parse O(N) extraction`). When technical documentation or web applications gate content behind UI controls—language-switcher tablists, collapsed in-article disclosures, React/Radix unmounted tabs, pagination buttons, `<canvas>` visualizations, or interactive browser state—`pleb` currently has three structural blind spots:

1. **Static DOM disclosures are pruned unconditionally**: In `ExtractMode::Clean` and `ExtractMode::Article`, `prune_dom` (`src/extract/dom.rs`) unconditionally detaches `[hidden]`, inline `style="display: none"`, and `.hidden`/`.d-none`/`.is-hidden` nodes without distinguishing paired in-landmark ARIA `[role="tabpanel"]` or `[aria-expanded="false"][aria-controls]` disclosures from hidden boilerplate or concealed stealth channels.
2. **Unmounted interactive gates fail silently**: When a page mounts only the active tab (`JavaScript`) and omits `Rust`/`Go`/`curl` from the DOM until clicked—or gates content behind a "Load more" button, modal overlay, or `<canvas>`—`pleb` reports `content_outcome: "complete"` with no indication that gated content exists.
3. **No composable handoff back from Playwright / interactive browsers**: When an agent escalates to Playwright or a browser subagent for login, multi-step filters, or dynamic clicks, there is no way to run the post-interaction DOM through `pleb`'s `O(N)` cleaner, concealed-channel sanitizer, and `--focus`/`--section` budgeter without re-navigating a URL.

Milestone 7 closes all three gaps **without turning `pleb` into a stateful browser automation engine**:

1. **Unified Visibility & In-Landmark ARIA Disclosure Rescue (`--expand <none|tabs|all>`)**: Establishes one shared visibility/exclusion model across DOM pruning, disclosure rescue, `ui_gates` indexing, and declarative WebMCP discovery; rescues uniquely paired, visible-control-backed `[role="tabpanel"]` and in-landmark disclosures already present in the DOM under plain synthetic ATX headings (`Tab: Rust`, `Disclosure: ...`) that compose directly with `--focus` and `--section`.
2. **Zero-Network UI-Gate Affordance Indexing (`ui_gates`)**: Mirrors `FetchOutput.resources` and `FetchOutput.tools` by indexing gated in-landmark controls (`tablist`, `disclosure`, `pagination`, `modal_overlay`, `visual_canvas`) derived from the exact same eligibility analysis as `--expand`, with bounded visible-text label extraction, canonical `g_<8hex>` IDs, `untrusted_content: true`, and sanitized action cues separated from page-authored labels.
3. **Offline Extraction Subcommand (`pleb extract`)**: Exposes `pleb`'s exact local extraction, `--expand`, `ui_gates`, manifest formatting, `--focus`/`--section`/`--offset` projection, binary guard, and concealed-channel sanitization over `stdin` or a local file with zero `RuntimeContext` initialization, zero network I/O, zero daemon startup, and bounded input reads.
4. **Document-Bound Passive CDP Page Attach (`--no-navigate`)**: Lets `pleb fetch --render cdp --cdp-url ws://127.0.0.1:<port>/devtools/page/<id> --no-navigate` snapshot an already-positioned local Playwright or Chromium main document via a strictly passive CDP session bound to `(frame_id, loader_id, security_origin, url)`, without navigating, mutating layout mode, intercepting requests, closing the tab, or reading/writing `site-cache.json`.

### Phasing and Dependency Order

Milestone 7 splits into two independently shippable sub-milestones sharing one Wave Header Commit (`ExpandPolicy`, `UiGateDescriptor`, `FetchTarget`, and `finalize_extracted_document` signatures):

- **Phase 7A (Pure Rust, zero transport / daemon / CEF changes)**:
  - Unified element visibility classification (`src/extract/dom.rs`, also hardening `src/extract/webmcp.rs` against class-hidden and `inert` forms), `ExpandPolicy` (`--expand <none|tabs|all>`), and baseline-stable `ExtractMode::Article` root selection.
  - Linear-time `UiGateDescriptor` indexing and cue formatting (`src/extract/ui_gates.rs`), wired into `ExtractedDocument`, wire structs (`FetchOutput`, `FetchMetadataOutput`, `BatchItemSuccess`, `ExtractOutput`), CLI `stderr`, and `src/mcp.rs`.
  - Shared `finalize_extracted_document` helper and offline `pleb extract` subcommand (`src/main.rs`).
- **Phase 7B (Passive CDP Attach Bridge & Harness Escalation Contract)**:
  - `FetchTarget::{Navigate, AttachCdp}` acquisition split and document-bound passive CDP capture (`src/transport/cdp.rs`, `src/fetch.rs`, `src/main.rs`, `src/mcp.rs`).
  - Deterministic Playwright escalation decision matrix in `skills/pleb/SKILL.md`, `README.md`, `README-FULL.md`, `llms.txt`, and `llms-full.txt`, verified by `tests/cli_tests.rs` and `tests/mcp_and_setup_tests.rs`.

### Behavioral and Security Boundaries

- **Strictly read-only**: Neither Phase 7A nor Phase 7B clicks elements, mutates DOM state, submits forms, evaluates arbitrary page JavaScript, or exposes stateful multi-turn browser session APIs.
- **Zero CAPTCHA or anti-bot evasion**: Active bot challenges, login walls, and cookie walls fail fast with exit code `2` and typed `block_reason` across both navigated fetches and passive `--no-navigate` snapshots. Challenge resolution is reserved exclusively for a human in a visible browser—never scripted automation.
- **Structural concealed-channel non-regression (`Leaks == 0`)**: Hard exclusion signals—`opacity: 0`, `font-size: 0`, zero-box `overflow: hidden` + zero dimension, offscreen `position`/`text-indent`, `clip: rect(0,0,0,0)`, `clip-path: inset(50%)/inset(100%)/circle(0)`, `.sr-only`, `.visually-hidden`, `.screen-reader-text`, HTML `inert`, `<template>`/`<noscript>`/`<script>`/`<style>`, closed `<dialog:not([open])>`, and unpaired `[hidden]`/`display: none`—remain unconditionally stripped across document content, rescued panel subtrees, `ui_gates`, and `tools`. (Static HTML carries no general computed-stylesheet guarantee; `Leaks == 0` is defined and enforced over all supported HTML/ARIA/inline-CSS/class structural signals.)
- **Loopback-only passive attach**: `--no-navigate` requires a loopback `ws://` `/devtools/page/<nonempty-id>` endpoint (verified post-TCP via `peer_addr().ip().is_loopback()` plus self-SSRF checks). Remote `wss://` attach is explicitly rejected until a remote backend supplies verified resolved-destination IP evidence (URL-host inspection alone cannot prove what IP a remote browser connected to before attach).
- **Out of scope for Milestone 7 (seam-reserved only)**: Declarative pre-capture clicking/scrolling (`--reveal`, `--scroll`), XHR JSON dataset indexing, and domain-pinned `storageState` cookie handoff are deferred to future milestones (Section 8).

---

## 2. Unified Visibility Model & Phase 7A.1 — In-Landmark ARIA Disclosure Rescue (`--expand`)

### 2.1 Shared Visibility & Exclusion Classification (`src/extract/dom.rs`)

Replace ad-hoc visibility predicates (`is_element_hidden_or_inert`, `is_inline_hidden_style`, `has_hidden_or_sr_class`) with one cohesive classification shared by DOM pruning, `--expand`, `ui_gates`, and declarative WebMCP indexing (`src/extract/webmcp.rs`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementVisibility {
    /// No structural hiding or hard exclusion signal on this element.
    Visible,
    /// Hidden solely by a recoverable disclosure mechanism:
    /// `[hidden]`, `[aria-hidden="true"]`, inline `display: none` / `visibility: hidden` (only),
    /// or non-responsive hiding class (`hidden`, `d-none`, `is-hidden`).
    StructuralDisclosureHidden,
    /// Disqualified from rescue, control eligibility, visible text, and WebMCP discovery:
    /// inert tags (`script`/`style`/`noscript`/`template`), closed `<dialog:not([open])>`,
    /// HTML `inert` attribute, stealth CSS (`opacity: 0`, `font-size: 0`, `clip`, `clip-path`,
    /// zero-box `overflow: hidden`, offscreen positioning/text-indent), screen-reader classes
    /// (`.sr-only`, `.visually-hidden`, `.screen-reader-text`), or boilerplate id/class patterns.
    HardExcluded,
}
```

**Mandatory implementation invariants (fixing existing subtle defects):**
1. **Full-style scan (never early-return on `display: none`)**: `classify_inline_style(style: &str)` MUST inspect **every** semicolon-separated declaration before returning. If *any* declaration matches a hard exclusion (`opacity: 0`, `font-size: 0`, `clip: rect(0...)`, `clip-path`, zero-box `overflow: hidden`, offscreen `left`/`top`/`text-indent`), return `HardExcluded`—even if `display: none` appeared earlier in the same `style` attribute! Only when zero hard exclusions are present and `display: none` or `visibility: hidden`/`collapse` is present does it return `StructuralDisclosureHidden`.
2. **UTF-8-safe offscreen suffix check**: Fix `is_offscreen_px` (`src/extract/dom.rs:756`) to check `t.len() > 2 && t.is_char_boundary(t.len() - 2)` (or use `strip_suffix` case-insensitively on ASCII bytes) so trailing multi-byte UTF-8 characters in `style` values can never panic on pre-prune inputs.
3. **Separate boilerplate from hiding classes**: Split `is_boilerplate_attr` (`src/extract/dom.rs:966`) into:
   - `has_boilerplate_pattern_attr(id, class)` (cookie banners, sidebars, nav chrome -> `HardExcluded` for rescue),
   - `has_sr_only_class(class)` (`.sr-only`, `.visually-hidden`, `.screen-reader-text` -> `HardExcluded`),
   - `has_disclosure_hidden_class(class)` (`hidden`, `d-none`, `is-hidden` respecting existing responsive breakpoints `sm:`, `d-md-`, `is-block-tablet`, etc. -> `StructuralDisclosureHidden`).
4. **TeX exemption scoped to math nodes, not surrounding subtrees**: Retain TeX `<annotation encoding="application/x-tex">` rescue strictly at math containers (`.katex`, `.katex-mathml`, `mjx-container`, `<math>`) rather than letting an embedded `<annotation>` exempt an arbitrary enclosing hidden `<div>` carrying injected prose.
5. **HTML `inert` and hiding-class parity in WebMCP (`src/extract/webmcp.rs`)**: Update `is_element_hidden_or_inert` (or replace callers in `src/extract/webmcp.rs`) to reject any element or ancestor whose classification is not `ElementVisibility::Visible`—closing the gap where class-hidden (`class="hidden"`) or `inert` `<form toolname="...">`, inputs, or `<label>` nodes could enter `FetchOutput.tools`. Keep WebMCP discovery running on the original DOM visibility classification so `--expand` **never** promotes hidden forms into `tools`.

### 2.2 Bounded Visible-Text Walker (Eliminating Concealed-Label Leakage)

Do **not** use `collapsed_node_text` (`src/extract/dom.rs:830`, which calls `collect_raw_text` and skips only inert tags) for control labels, headings, or `ui_gates`. Add one shared helper:

```rust
pub(crate) fn extract_bounded_visible_text(
    node: NodeRef<'_, Node>,
    max_bytes: usize,
) -> Option<String>
```

Behavior:
- Traverses descendants of `node`, skipping any descendant element whose `classify_element_visibility(el)` is not `ElementVisibility::Visible` (stopping recursion into hidden/excluded subtrees immediately—zero allocation for hidden text).
- Strips ASCII control characters, bidi overrides, and `is_concealed_unicode` characters on the fly, collapses inline whitespace, and stops traversal as soon as output reaches `max_bytes` (no unbounded full-subtree allocation before truncation).
- Returns `None` if no visible text remains.
- **Accessible-name policy**: `aria-label` is **never** used as a silent fallback for synthetic Markdown headings or CLI/MCP action cues (an `aria-label` without visible text is uncorroborated page-authored metadata). For `VisualCanvas` and `ModalOverlay` (inherently non-text/structural widgets), sanitized `aria-label` may populate `UiGateDescriptor.labels` (marked `untrusted_content: true`), while action cues print generic counts only (Section 3.3).

### 2.3 Policy Enum, Modes, and Content vs. Metadata Matrix (`src/extract/mod.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ExpandPolicy {
    /// Preserve existing pruning of all hidden/collapsed panels (default).
    #[default]
    None,
    /// Rescue uniquely paired in-landmark ARIA `[role="tabpanel"]` elements under plain `Tab: <label>` headings.
    Tabs,
    /// Rescue paired tabpanels plus uniquely paired in-landmark `[aria-expanded="false"][aria-controls]` disclosures.
    All,
}
```

Add `pub expand: ExpandPolicy` to `ExtractOptions` (default `ExpandPolicy::None`).

**Exact Content vs. Metadata Matrix across formats and modes:**

| Format | ExtractMode | `--expand` allowed? | Extracted `content` behavior | `ui_gates` metadata & cue behavior |
| :--- | :--- | :--- | :--- | :--- |
| `markdown` / `text` | `clean` | Yes (`none`/`tabs`/`all`) | Rescues eligible panels when `tabs`/`all`; byte-identical content when `none`. | Indexed on document; `in_dom` cues emitted when `expand` did not already rescue them. |
| `markdown` / `text` | `article` | Yes (`none`/`tabs`/`all`) | Article root selected from **baseline visible metrics** first; expansion scoped strictly inside selected root. | Scoped strictly to selected article root; `in_dom` cues emitted for unexpanded eligible panels inside that root. |
| `markdown` / `text` | `full` | Accepted (no-op on panels) | Strips only `ALWAYS_STRIP_SEL` and `PRE_NOISE_SEL` (existing `dom.rs:522-523`), retains other DOM nodes without synthetic headings. | `ui_gates` empty (no `--expand` advice on `full`). |
| `html` | `clean` / `article` / `full` | Accepted (no-op on HTML body) | Returns decoded HTML input string (subject to `--max-bytes` / `--offset` slicing). | Indexed in `clean`/`article` for JSON/metadata parity, but `stderr`/MCP cues suppress `--expand` advice when `--format html` (since `--expand` has no effect on raw HTML output). |
| `axtree` | any | Rejected (`!= none` -> exit `1`) | CDP `Accessibility.getFullAXTree` formatted text; metadata pass runs internally with `expand = None`. | Indexed from underlying HTML for JSON/metadata parity; suppresses `--expand` advice on `axtree`. |
| `raw` / `--stage` | any | Rejected (`!= none` -> exit `1`) | Raw byte passthrough / artifact staging. | None. |
| Non-HTML early returns (native Markdown, RSS/Atom/JSON feeds, VTT/SRT, YouTube, plain/JSON passthrough) | any | Accepted (no-op) | Existing format-specific pipeline unchanged. | `ui_gates` empty. |

Note on compatibility: Under `--expand none` (default), extracted `content` stays byte-identical on all existing pages (`ratchet == 1.000`), while `--json` and non-quiet `stderr` gain `ui_gates` / `UI-Gated:` cues solely on HTML pages that actually contain qualifying in-landmark gates.

### 2.4 Linear-Time Single-Pass Eligibility and Strict Pairing Rules (`src/extract/dom.rs`)

Both `--expand` rescue and `ui_gates` indexing run over one shared `O(N)` pre-analysis (`analyze_disclosures`), guaranteeing zero quadratic rescans and exact consistency between what `ui_gates` advertises as `in_dom` and what `--expand` actually rescues:

**Pre-pass (`O(N)` single DFS over the document tree):**
1. Build `id_counts: HashMap<&str, usize>` and `unique_id_to_node: HashMap<&str, NodeId>` (recording an ID as **ambiguous** if `count > 1`).
2. Propagate inherited ancestor state downward in O(1) per node:
   - `enclosing_landmark: Option<NodeId>` (innermost `<main>`, `<article>`, or `[role="main"]`),
   - `in_boilerplate_ancestor: bool` (`<nav>`, `<header>` without `<h1>`, `<footer>`, `<aside>`, `[role="navigation"|"banner"|"contentinfo"|"complementary"|"menu"|"menubar"|"dialog"|"tooltip"]`, or `has_boilerplate_pattern_attr`),
   - `nearest_tablist: Option<NodeId>` (innermost `[role="tablist"]`),
   - `ancestor_hard_excluded: bool` (true if any ancestor has `ElementVisibility::HardExcluded`),
   - `ancestor_hidden_chain: SmallVec<[NodeId; 4]>` (ancestors with `StructuralDisclosureHidden`),
   - `current_heading_level: u8` and `current_section_heading: Option<String>` (using `extract_bounded_visible_text(heading, 120)`),
   - Baseline visible `NodeMetrics` (computing `select_root_id` for `ExtractMode::Article` **before** any panel rescue so expanding hidden tabs can never flip which `<article>` wins the 100-byte or 65% dominance rule!).
3. Collect candidate index lists in document order: visible `[role="tab"]` controls, `[role="tablist"]` containers, visible disclosure buttons (`aria-expanded="false"` without `aria-haspopup`, not `disabled`), candidate `[role="tabpanel"]` elements, pagination buttons, open modal dialogs, and qualifying `<canvas>` elements, together with each subtree's baseline non-whitespace visible-text length (excluding `HardExcluded` descendants).

**Strict Pairing & Eligibility Contract (resolving indexed candidates in `O(candidates)`):**

Every control `C` and panel `P` pair must satisfy **all** of the following:
1. **Shared Content Scope**:
   - Let `root_id` be `select_root_id(...)` in `ExtractMode::Article` (computed from baseline visible metrics) or `body_or_root_id` in `ExtractMode::Clean`.
   - Both `C` and `P` must lie inside `root_id`.
   - If `doc_has_landmark` is `true`, both `C` and `P` must share the **same** enclosing content landmark (`C.enclosing_landmark == P.enclosing_landmark` and `is_some()`) and have `!in_boilerplate_ancestor`.
   - If `doc_has_landmark` is `false`, neither `C` nor `P` may have `in_boilerplate_ancestor`.
2. **Visible, Active Control `C`**:
   - `classify_element_visibility(C) == Visible` and `!C.ancestor_hard_excluded` and `C.ancestor_hidden_chain.is_empty()` and `C` does not have `disabled` / `aria-disabled="true"`.
   - `extract_bounded_visible_text(C, 64)` must be `Some(label)` (1..=64 bytes, visible text only—no `aria-label` fallback for rescue).
3. **Unique Bidirectional / IDREF-Tokenized Binding**:
   - Tokenize `aria-controls` and `aria-labelledby` on ASCII whitespace (matching `dom.rs:559` and WAI-ARIA IDREFS).
   - Reject any IDREF token whose `id_counts[token] != 1` (duplicate IDs are **never** eligible for rescue).
   - **Tabpanel rescue (`ExpandPolicy::Tabs` and `ExpandPolicy::All`)**:
     - `C` has `role="tab"` and belongs to a `nearest_tablist` (or shares a direct parent with `>= 1` sibling visible `[role="tab"]`).
     - `P` **must** carry `role="tabpanel"` (never rescue an arbitrary generic `<div>` via a tab button).
     - `P` must match `C` via unique `aria-controls` -> `P.id` or unique `P.aria-labelledby` -> `C.id` (if both are present, they must agree—inconsistent forward/reverse references are rejected!).
     - Ownership check: `P` must not belong to a different nested `[role="tablist"]`, and `P` must not already be claimed by another control `C'`.
     - **Positional-only tablists (no `aria-controls` / `aria-labelledby`)**: Never auto-rescued via `--expand` (prevents mis-pairing across nested or irregular markup); instead indexed in `ui_gates` as `presence: "unmounted"` / informational only unless explicitly ID-linked.
   - **Disclosure rescue (`ExpandPolicy::All` only)**:
     - `C` is `<button>` or `[role="button"]` with `aria-expanded="false"`, **no** `aria-haspopup`, and `aria-controls` resolving to exactly one unique target ID `P` (multi-target `aria-controls` tokens are rescued only if every token resolves uniquely to an eligible panel; if any token is ambiguous/disqualified, reject).
     - `P` must not be `html`/`body`/landmark/boilerplate tag (`nav`, `header`, `footer`, `aside`, `dialog`, `form`), must not carry a menu/listbox/tooltip/dialog/navigation ARIA role, and must not have `has_boilerplate_pattern_attr`.
     - Native `<details>` elements continue to be preserved by default (`dom.rs:1070`, `2124`) without requiring `--expand`.
4. **Whole-Ancestor-Chain Exemption Rule & `HardExcluded` Supremacy**:
   - `P` itself must have `classify_element_visibility(P) == StructuralDisclosureHidden` (for inactive panels) or `Visible` (for active sibling tabs), with `!P.ancestor_hard_excluded`.
   - Every element in `P.ancestor_hidden_chain` must itself be an eligible rescued outer panel (evaluated outer-to-inner; if any hidden ancestor is NOT itself rescued, `P` is rejected).
   - **Recursive non-exemption invariant**: Exemption during `prune_dom` applies strictly to `P`'s own `NodeId` (and rescued outer panels). Every non-rescued descendant inside `P` still passes through full `HardExcluded` and `StructuralDisclosureHidden` pruning!
5. **Cardinality Cap & Deterministic Sibling Heading Synthesis**:
   - Cap rescued hidden panels at `MAX_EXPANDED_PANELS = 32` in document order. Panels beyond the cap remain unexpanded and are marked `capped = true` in the gate analysis so cues never suggest re-running `--expand` when the cap is already saturated.
   - **Shared sibling heading level**: Assign **one** heading level per tablist group (`(tablist_preceding_heading_level + 1).clamp(2, 6)`, default `3` when no heading precedes the tablist) so sibling tabs (`JavaScript`, `Rust`, `Go`) always sit at the same ATX heading level and never nest under one another (even if a panel contains internal subheadings).
   - **Plain synthetic heading text (no brackets — `htmd 0.5.1` escapes `[` / `]`)**:
     - Markdown path (`serialize_pruned_html`): emit `<h{level}>Tab: {escaped_label}</h{level}>` (or `Disclosure: {escaped_label}`), which `htmd` converts cleanly to `### Tab: Rust` (never `### \[Tab: Rust\]`).
     - Plain-text path (`collect_plain_text`): emit `\n\nTab: {label}\n\n` (or `Disclosure: {label}`) directly rather than injecting raw HTML tags.
     - Active sibling labeling: when a tablist rescues `>= 1` hidden panel, also label its active visible sibling tabpanel(s) with `Tab: {active_label}` unless the panel's first element child is already an `<h1>`–`<h6>` heading whose normalized visible text contains `active_label`.
   - **Post-prune survival check**: A panel counts as expanded (and is cleared from `ui_gates`) only if its `NodeId` survives final pruning (including link-density pruning) and is emitted inside `root_id`.
   - **Section/Focus behavior note**: Because `--section "Tab: Rust"` uses `extract_section` (`src/extract/focus.rs:25-60`, exact -> prefix -> substring match across all matching headings + subsections), identical tab labels repeated across multiple tablists on the same page match every `Tab: Rust` section in document order; if a page has both `Tab: Rust` and `Tab: Rust SDK`, exact match selects only `Tab: Rust` while substring `"Rust"` matches both.

---

## 3. Phase 7A.2 — Zero-Network UI-Gate Affordance Indexing (`src/extract/ui_gates.rs`)

### 3.1 Domain Types (`src/extract/ui_gates.rs`)

```rust
pub const MAX_UI_GATES: usize = 16;
pub const MAX_UI_GATE_LABELS: usize = 16;
const MAX_UI_GATE_LABEL_BYTES: usize = 64;
const MAX_UI_GATE_HEADING_BYTES: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiGateKind {
    Tablist,
    Disclosure,
    Pagination,
    ModalOverlay,
    VisualCanvas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiGatePresence {
    /// All gated items in this descriptor are eligible in-DOM disclosures recoverable via `--expand`.
    InDom,
    /// One or more gated controls have missing, ambiguous, positional-only, or empty panels (requires browser interaction).
    Unmounted,
    /// Descriptor contains both recoverable in-DOM panels and unmounted controls (see `in_dom_labels` vs `unmounted_labels`).
    Mixed,
    /// Visual/opaque surface (`<canvas>`) or modal obstruction.
    Opaque,
}

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
    /// Visible (or accessible, for Opaque) labels of unmounted/opaque controls (capped at `MAX_UI_GATE_LABELS`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmounted_labels: Vec<String>,
    /// Total count of currently gated items represented (includes items beyond the label preview cap).
    pub gated_count: usize,
    /// True when `MAX_EXPANDED_PANELS` capped further in-DOM expansion (re-running `--expand` will not recover more).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_label: Option<String>,
    /// Always `true` (page-authored labels and headings are untrusted data).
    pub untrusted_content: bool,
}
```

**Canonical ID & deduplication contract**:
- Compute `g_<8hex>` from `ring::digest::SHA256` over a length-delimited canonical encoding of `(kind, presence, section_heading, active_label, in_dom_labels, unmounted_labels, capped, occurrence_index)` (writing `<len>:<bytes>` per field so label boundaries can never collide).
- When identical widgets repeat in the same section (e.g., two identical `JavaScript / Rust / Go` tablists under `## Examples`), keep distinct widgets (incrementing `occurrence_index` so each gets a distinct ID and total `gated_count` is preserved) rather than silently collapsing them; stop collecting after `MAX_UI_GATES = 16` in document order.

### 3.2 Detection Rules (Derived Directly from Shared Eligibility)

All rules operate strictly within `root_id` (and shared content-landmark scope when `doc_has_landmark` is `true`):

1. **`UiGateKind::Tablist`**:
   - For each eligible `[role="tablist"]` (or visible sibling `[role="tab"]` group) with `>= 1` inactive visible tab:
     - Partition inactive tabs that were NOT rescued and rendered by `--expand` into:
       - `in_dom_labels`: tabs whose paired `[role="tabpanel"]` satisfies the **exact same rescue eligibility predicate** as Section 2.4 and has `>= 1` non-whitespace visible text byte (no arbitrary 16-byte floor that drops short code snippets like `go test`!),
       - `unmounted_labels`: inactive tabs with no eligible panel, empty panel (`0` visible text bytes), ambiguous/duplicate ID, or positional-only unlinked panel.
     - If `in_dom_labels` is non-empty and `unmounted_labels` is empty -> `InDom`; if only `unmounted_labels` -> `Unmounted`; if both -> `Mixed`. Set `capped = true` if any `in_dom_labels` tab was eligible but skipped due to `MAX_EXPANDED_PANELS`.
2. **`UiGateKind::Disclosure`**:
   - Group consecutive unexpanded eligible in-landmark `[aria-expanded="false"]` buttons (no `aria-haspopup`, not `disabled`, not `type="submit"`) under the same `section_heading`:
     - Partition into `in_dom_labels` (exact Section 2.4 eligibility + `>= 1` visible text byte) and `unmounted_labels` (missing/ambiguous/empty target with a descriptive visible label of `>= 3` letters).
     - Classify `InDom`, `Unmounted`, or `Mixed` (plus `capped` when hitting `MAX_EXPANDED_PANELS`).
3. **`UiGateKind::Pagination`**:
   - Match visible in-landmark `<button>` or `[role="button"]` (explicitly excluding `<a href>`, `disabled`, `aria-disabled="true"`, and `type="submit"` inside `<form>`) whose normalized visible label matches `"load more"`, `"show more"`, `"view more"`, `"more results"`, `"show all"`, `"next page"`, `"load previous"`, or `"view all"` -> `Unmounted`.
4. **`UiGateKind::ModalOverlay`** *(descriptive)*:
   - Match visible `dialog[open]` or `[role="dialog"][aria-modal="true"]` when total non-dialog visible prose in `root_id` is `< 400` bytes -> `Opaque` (descriptive cue only; note `pleb fetch` challenge/login/cookie classification always runs first and exits `2` before extraction!).
5. **`UiGateKind::VisualCanvas`** *(descriptive)*:
   - Match visible in-landmark `<canvas>` elements declaring `width >= 200 && height >= 120` (via `parse_html_dimension`) or `role="img"` / non-empty sanitized `aria-label` when enclosing section/landmark prose is `< 600` bytes (or `>= 2` qualifying canvases appear in `root_id`) -> `Opaque`.

### 3.3 Wire Types, `ContentOutcome`, and Untrusted-Label-Safe Cues

- **`ContentOutcome` stability**: Keep `ContentOutcome` two-valued (`{ Complete, IncompleteShell }` at `src/fetch.rs:168`).
- **Internal vs. wire structs**:
  - `ExtractedDocument` (`src/extract/mod.rs:80`, non-Serde internal struct): add `pub ui_gates: Vec<UiGateDescriptor>` (initialize `Vec::new()` in `wrap_raw_code_document`, native Markdown, feeds, transcripts, YouTube, and passthrough paths).
  - Serde wire structs (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`):
    - `FetchOutput.ui_gates` (`src/fetch.rs:187` — including raw-source `try_fetch_raw_source` constructor at `src/fetch.rs:1106`),
    - `BatchItemSuccess.ui_gates` (`src/fetch.rs:2301`, converted in `From` / batch mapper at `src/fetch.rs:2354`),
    - `FetchMetadataOutput.ui_gates` (`src/main.rs:416`, converted at `src/main.rs:441`),
    - `ExtractOutput.ui_gates` (`src/main.rs`).
- **Separated Action Cue vs. Untrusted Labels (`format_ui_gates_cue`)**:
  - Never interpolate raw page-authored labels directly into instruction prose! Format as two explicitly separated clauses—an action/count summary first, followed by a delimited untrusted-labels suffix ONLY for visible-text labels (omitting `aria-label`-only opaque names from the action cue; inspect `--json` for full `untrusted_content: true` descriptors):
    - Expandable in-DOM tabs (not capped, format is `markdown`/`text`, mode is `clean`/`article`):
      `UI-Gated: 3 tabs in DOM (pass --expand tabs) | untrusted labels: "Rust", "Go", "curl"`
    - Expandable disclosures + tabs (not capped):
      `UI-Gated: 2 tabs, 4 disclosures in DOM (pass --expand all) | untrusted labels: "Rust", "Go", "Advanced"`
    - Capped in-DOM panels:
      `UI-Gated: 4 additional in-DOM panels omitted by 32-panel cap`
    - Unmounted / mixed / opaque (or formats where `--expand` has no effect):
      `UI-Gated: 3 unmounted tabs, 1 visual canvas (escalate to browser or attach via --no-navigate) | untrusted labels: "Rust", "Go", "curl"`
  - Cap cue to 1 line, `<= 220` bytes, max `6` quoted visible labels (escaping `"` and control chars).
  - Emitted on non-JSON `stderr` when `!quiet && !ui_gates.is_empty()` and as a single `UI-Gated: ...` metadata header line in MCP `pleb_fetch` (including `--metadata-only` / `metadata_only: true`, keeping the `| untrusted labels:` boundary intact).

---

## 4. Phase 7A.3 — Shared Finalization & Offline Extraction (`pleb extract`)

### 4.1 Shared Pure Finalization (`src/extract/mod.rs`)

Today `fetch_document` (`src/fetch.rs:344-430`) defers `--focus`/`--section`/`--offset`/`--max-bytes` until after appending `--resources manifest` / `--tools manifest`, whereas calling `extract_document_bytes` alone applies projection immediately. To give `pleb fetch` and `pleb extract` identical manifest and pagination semantics with zero `RuntimeContext` coupling:

- Add a pure helper `pub fn finalize_extracted_document(doc: &mut ExtractedDocument, options: &ExtractOptions, append_resources_manifest: bool, append_tools_manifest: bool)` in `src/extract/mod.rs`:
  1. Appends `format_resource_manifest_markdown(&doc.resources)` when `append_resources_manifest && !doc.resources.is_empty()`.
  2. Appends `format_webmcp_manifest_markdown(&doc.tools)` when `append_tools_manifest && !doc.tools.is_empty()`.
  3. Runs `apply_projection_or_slice(&doc.content, options)` once, updating `doc.content`, `doc.truncated`, `doc.next_offset`, and merging `doc.warning`.
- Reuse `finalize_extracted_document` from both `fetch_document` (for offline manifest cases + final slice) and `pleb extract`, hashing the **original raw input bytes** before charset decoding for `acquired_sha256`.
- Expose `parse_http_url` (`src/fetch.rs:2280`) as `pub fn parse_http_url` (or in `src/policy/source.rs`) so `pleb extract` shares exact URL validation without instantiating `RuntimeContext`.

### 4.2 Offline Binary Guard Precedence (`src/extract/mod.rs`)

Reuse and extend `detect_binary_content` (`src/extract/mod.rs:354`) with an explicit `sniff_magic_always: bool` mode (used by `pleb extract` and raw-source fetches so a PDF or PNG piped without `--content-type` is caught even when the default MIME is `text/html; charset=utf-8`):

1. **Declared binary MIME** (`application/pdf`, `image/png`, `audio/*`, `video/*`, `font/*`, `application/octet-stream` with binary bytes, etc.) -> exit `5` (`kind: "unsupported_content_type"`).
2. **Recognized binary magic header** (`%PDF-`, PNG, JPEG, GIF87a/89a, ZIP `PK\x03\x04`, GZIP `\x1F\x8B`, WASM `\x00asm`) -> exit `5` regardless of default `--content-type`.
3. **Supported UTF-16 BOM or declared UTF-16 charset** (`\xFE\xFF`, `\xFF\xFE`, or `charset=utf-16*` in `--content-type`) -> allow text decoding via `decode_text_bytes` (`src/extract/charset.rs:53`); do **not** run generic NUL-byte rejection on valid UTF-16!
4. **Generic NUL-byte sniff** (within first `8192` bytes when no UTF-16 BOM/charset applies) -> exit `5` (`kind: "unsupported_content_type"`).

On exit `5`, emit nothing to `stdout`, emit the JSON/plain error envelope to `stderr`, and never create or overwrite `-o <path>`.

### 4.3 `pleb extract` CLI Contract (`src/main.rs`)

Dispatch `Command::Extract` directly in `src/main.rs` **before** constructing `RuntimeContext` (zero `~/.pleb` creation, zero `daemon.sock`, zero Keychain, works even when `PLEB_HOME` points to an unwritable path):

```bash
node dump-page.mjs | pleb extract --url https://docs.example.com/api --expand tabs --focus "rate limits"
pleb extract ./snapshot.html --url https://docs.example.com/api --json
```

| Flag / Arg | Type / Default | Validation & Semantics |
| :--- | :--- | :--- |
| `[input]` | `Option<PathBuf>`, default `"-"` | File path or `"-"` (`stdin`). If reading `stdin` and `std::io::stdin().is_terminal()`, fail immediately with exit `1` (`kind: "invalid_input"`). |
| `--url <URL>` | required `String` | `http://` or `https://` with host (`parse_http_url`). Never fetched. |
| `--content-type <MIME>` | `Option<String>`, default `"text/html; charset=utf-8"` | Selects parser via `extract_document_bytes`. |
| `--format <FMT>` | `markdown` *(default)*, `text`, `html` | Rejects `raw` and `axtree` (exit `1`, `invalid_input`). |
| `--extract <MODE>` | `clean` *(default)*, `article`, `full` | Same extraction scope semantics as `pleb fetch`. |
| `--expand <POLICY>` | `none` *(default)*, `tabs`, `all` | In-landmark ARIA disclosure rescue (Section 2). |
| `--resources <POLICY>` | `none` *(default)*, `manifest` | Rejects `transcripts`, `diagrams`, `all` (exit `1`, `invalid_input`). |
| `--tools <POLICY>` | `none` *(default)*, `manifest` | Read-only declarative WebMCP indexing. |
| `--no-images`, `--no-links` | flags | Markdown compaction controls. |
| `--max-bytes <N>` | `Option<usize>` | Fence-safe UTF-8 byte cap. |
| `--focus <QUERY>` | `Option<String>` | BM25 section projection (conflicts with `--section`, `--offset > 0`, `--format html`). |
| `--section <SELECTOR>` | `Option<String>` | Markdown heading extractor (requires `--format markdown`; conflicts with `--focus`; composes with `--offset` and `--max-bytes`). |
| `--offset <BYTES>` | `usize`, default `0` | Continuation offset (conflicts with `--focus`). |
| `--max-input-bytes <N>` | `usize`, default `16 MiB` (`DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES`) | Reject explicit global `--max-response-bytes` on `extract` with exit `1` (`"use --max-input-bytes with 'pleb extract'"`). Read bounded with `limit.checked_add(1)` (rejecting `limit == 0` or overflow as `invalid_input`); exact `limit` bytes succeed, `limit + 1` fails with exit `1` (`kind: "response_too_large"`). |
| `--metadata-only` | flag | Requires global `--json` (exit `1` otherwise); conflicts with `-o`. Omits `content`. |
| `-o`, `--output <PATH>` | `Option<PathBuf>` | When combined with `--json`, writes JSON to `-o` with empty `stdout` (matching `pleb fetch`). Uses same-directory temp file + `rename` only after extraction succeeds so an existing file is never clobbered on error. |

**Canonical `ExtractOutput` JSON Examples (`pleb extract --json`):**

Complete page with no gates/resources/tools (omits all empty vectors and absent optionals):
```json
{
  "url": "https://docs.example.com/api",
  "content_type": "text/html; charset=utf-8",
  "title": "API Reference",
  "content": "# API Reference\n\nUse bearer authentication.",
  "content_bytes": 43,
  "acquired_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "truncated": false,
  "timing_ms": 2
}
```

Truncated page with continuation offset and detected in-DOM tablist:
```json
{
  "url": "https://docs.example.com/api",
  "content_type": "text/html; charset=utf-8",
  "title": "API Reference",
  "content": "# API Reference\n\n...\n\n[... truncated]",
  "content_bytes": 4096,
  "acquired_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "truncated": true,
  "next_offset": 4078,
  "timing_ms": 3,
  "ui_gates": [
    {
      "identifier": "g_4a9f1c2e",
      "kind": "tablist",
      "presence": "in_dom",
      "section_heading": "Quick Start",
      "in_dom_labels": ["Rust", "Go", "curl"],
      "gated_count": 3,
      "active_label": "JavaScript",
      "untrusted_content": true
    }
  ]
}
```

**No `pleb_extract` MCP tool:** Passing large raw HTML strings through LLM tool arguments burns more context tokens than it saves; `pleb extract` is a CLI + Rust library pipe primitive while MCP uses `pleb_fetch` (`expand` + `no_navigate`).

---

## 5. Phase 7B — Document-Bound Passive CDP Page Attach (`--no-navigate`)

### 5.1 Unified Acquisition Target Model (`src/fetch.rs`, `src/main.rs`, `src/mcp.rs`)

Model navigation vs. passive attach once at the acquisition boundary instead of combining contradictory booleans and duplicate entrypoints:

```rust
#[derive(Debug, Clone)]
pub enum FetchTarget {
    /// Standard network/rendered fetch of an `http://` or `https://` URL.
    Navigate(String),
    /// Passive read-only snapshot of an already-loaded loopback `/devtools/page/<id>` tab.
    AttachCdp {
        /// Optional origin assertion (`http://` or `https://`); fails if attached tab origin differs.
        expected_origin: Option<String>,
    },
}
```

**CLI contract (`Command::Fetch` in `src/main.rs`):**
- Change `urls: Vec<String>` to optional at clap level (`urls: Vec<String>`), and validate **before** stdin reading, `target_urls[0]` indexing, or `RuntimeContext` creation:
  - When `!no_navigate`: require `!urls.is_empty()` (exact existing behavior).
  - When `no_navigate`:
    - Allow `urls.len() <= 1`; reject `urls.len() > 1` or `urls == ["-"]` (exit `1`, `invalid_input`).
    - If `urls.len() == 1`, validate it with `parse_http_url` immediately (reject blank or non-HTTP URLs—never silently discard an invalid origin assertion!).
    - Require `render == RenderMode::Cdp` (never attach implicitly in `--render auto`).
    - Require `--cdp-url` / `PLEB_CDP_URL` to be a **loopback `ws://` endpoint** with exact path shape `/devtools/page/<nonempty-single-segment-id>` (reject `/devtools/browser`, `/devtools/page/`, trailing segments, and remote `wss://`).
    - Reject `--proxy`, `--stage`, `--format raw`, `--source negotiate`, `--respect-robots`, `--resources transcripts|diagrams|all`, and non-empty `--stage-resources` (only `--resources none|manifest` and `--tools none|manifest` are allowed offline on the attached snapshot so attach never fires unauthenticated Cronet subresource fetches).
    - Single-item `--json`, `--jsonl`, and `--metadata-only` work uniformly with `--no-navigate`.
- **MCP contract (`src/mcp.rs` `pleb_fetch`)**:
  - Make `"url"` optional in `inputSchema.required` and add `"expand"` (`"none" | "tabs" | "all"`) and `"no_navigate"` (`boolean`).
  - Validate explicitly in `call_fetch`:
    - If `"url"` key is present in `arguments` and is not a non-empty valid `http`/`https` string (including `""`), return JSON-RPC `-32602` (`invalid_params`)—never silently coerce blank strings to `None`!
    - When `no_navigate == false` (default): require `"url"`.
    - When `no_navigate == true`: apply the exact same validation rules as CLI (`render == "cdp"`, loopback `/devtools/page/<id>`, offline-only `resources`, etc.), returning `-32602` on violations.

### 5.2 Document-Bound Passive CDP Protocol (`src/transport/cdp.rs`)

Extend `FrameTreeFrame` (`src/transport/cdp.rs:736`) to capture document identity and failure metadata:

```rust
#[derive(Deserialize)]
struct FrameTreeFrame {
    id: String,
    #[serde(rename = "loaderId", default)]
    loader_id: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "urlFragment", default)]
    url_fragment: Option<String>,
    #[serde(rename = "securityOrigin", default)]
    security_origin: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "unreachableUrl", default)]
    unreachable_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachedDocumentBinding {
    frame_id: String,
    loader_id: String,
    url: String,                 // reconstructed url + optional urlFragment
    origin: (String, String, u16),
    security_origin: Option<String>,
}
```

**Dedicated passive connection & RPC allowlist (fixing active `handle_event` & TOCTOU gaps):**
Share WebSocket TCP connect, post-connect `peer_addr().ip().is_loopback()`, deadline enforcement, and `teardown()` with `CdpRenderer::render` via an internal `connect_cdp_ws` helper, but execute passive attach inside a dedicated `PassiveAttachSession` that does **not** reuse `CdpSession::handle_event` and has a hard-coded outbound RPC allowlist:

- **Permitted outbound CDP methods ONLY**:
  - `Page.enable` (passive frame-navigation & invalidation events),
  - `Page.getFrameTree` (document identity binding + verification),
  - `DOM.enable` (for `DOM.documentUpdated` invalidation events),
  - `DOM.getDocument` (`{ "depth": 0 }`),
  - `DOM.querySelector` (only when `--wait-for-selector` is set),
  - `DOM.getOuterHTML`,
  - `Accessibility.getFullAXTree` (`{ "frameId": binding.frame_id }`, only when `--format axtree`),
  - `WebMCP.enable` (best-effort; ignores `-32601`).
- **Strictly forbidden outbound methods** (verified by unit tests inspecting the complete outbound RPC log):
  - `Fetch.enable`, `Fetch.continueRequest`, `Fetch.failRequest`, `Network.enable`, `Page.navigate`, `Kitesurf.setLayoutMode`, `Target.createTarget`, `Target.attachToTarget`, `Target.closeTarget`, `Browser.close`.
  - Unsolicited `Fetch.requestPaused` or unknown events are **ignored** (never answered with `Fetch.*` commands!).

**Exact Document-Bound Capture Sequence (`execute_attached`):**

1. **Bind main document identity first**:
   - Call `Page.enable` (required so navigation events fire if the tab moves mid-capture) and `DOM.enable` (best-effort for `DOM.documentUpdated` invalidation).
   - Call `Page.getFrameTree` and parse main `frame` (main document only; child iframes are out of scope).
   - Require non-empty `frame.id` AND non-empty `frame.loader_id` (if a backend omits `loaderId`, fail closed with `TransportErrorKind::ProtocolMismatch` because document identity cannot be bound across reads!).
   - Reject non-empty `frame.unreachable_url` (failed/error page -> `TransportErrorKind::LoadError`).
   - If `frame.mime_type` is present and indicates an unsupported binary MIME -> fail fast cleanly (`LoadError` / binary check).
   - Reconstruct `live_url = format!("{}{}", frame.url, frame.url_fragment.as_deref().unwrap_or(""))`, parse as `Url`, require `http` or `https` (rejecting `about:blank`, `chrome://`, `devtools://`, `file://`, `data:`, `javascript:`), enforce `is_url_self_ssrf` and `is_ip_self_ssrf(&live_url, peer_ip, port)`.
   - If `expected_origin` is `Some(expected)`: verify exact origin equality (`scheme`, `canonical_url_host`, `port_or_known_default`) AND verify `frame.security_origin` (when present and not `"null"`) matches that exact origin; mismatch fails immediately with `TransportErrorKind::RedirectBlocked`.
   - Record `AttachedDocumentBinding` and set `main_frame_id` **before** calling `WebMCP.enable` so initial tool-replay events for `main_frame_id` are never dropped.
2. **Continuous invalidation watching**:
   - While pumping any CDP message during `WebMCP.enable`, selector polling, `DOM.getOuterHTML`, or `Accessibility.getFullAXTree`, if `Page.frameNavigated` (for `main_frame_id`), `Page.navigatedWithinDocument` (changing origin/path), `Page.frameStartedLoading` (for `main_frame_id`), or `DOM.documentUpdated` arrives with a changed `loaderId` or URL, set `document_invalidated = true`, **immediately abort**, discard all intermediate HTML, AXTree, and WebMCP tools, and return `TransportErrorKind::LoadError` (`"attached CDP page navigated during capture"`).
3. **Initial HTML capture & pre-wait challenge classification**:
   - Call `WebMCP.enable` best-effort (`call_best_effort`); if supported, pump up to `100ms` bounded under the shared deadline (note: best-effort replay is not a readiness guarantee on all backends).
   - Perform an initial main-document `read_outer_html` (enforcing `max_response_bytes` immediately before returning) and **verify `Page.getFrameTree` binding has not changed**.
   - Run `detect_interstitial_challenge` and HTML classification on this initial snapshot **before** entering any `--wait-for-selector` polling loop so an active Cloudflare/bot challenge fails fast (`exit 2`, zero wasted selector timeout)!
4. **Optional `--wait-for-selector` (typed errors, no `.contains` string sniffing)**:
   - If `options.wait_for_selector` is set, pre-validate syntax with `scraper::Selector::parse` (already enforced at `src/transport/cdp.rs:146`, returning `TransportErrorKind::InvalidSelector` without sniffing CDP error strings via `.contains(...)`).
   - Poll `DOM.querySelector` under the shared deadline, checking `document_invalidated` on every tick and re-verifying `Page.getFrameTree` (`frame_id`, `loader_id`, `url`, `security_origin`) after the selector matches.
   - Re-capture `outer_html` after selector match (enforcing `max_response_bytes`) and re-classify if content changed.
5. **Scoped AXTree capture (with successful-payload size enforcement)**:
   - When `--format axtree` is set, call `Accessibility.getFullAXTree` with `{ "frameId": binding.frame_id }` (explicitly scoping to the bound main frame).
   - Check the **raw serialized AXTree JSON byte length** against `max_bytes` BEFORE calling `format_axtree_from_cdp_json` (fixing the existing gap in `src/transport/cdp.rs:1842-1859` where `max_bytes` was only used on RPC error!), returning `ResponseTooLargeError` on overflow.
6. **Final post-capture identity verification**:
   - Call `Page.getFrameTree` one final time after all DOM/AX reads and confirm `(frame_id, loader_id, url, security_origin)` matches `AttachedDocumentBinding` identically (closing the TOCTOU window—even if the tab navigated away and back to the same URL, `loader_id` differs!). Discard all intermediates and fail on mismatch.
7. **Non-destructive teardown**:
   - `owns_target` is unconditionally `false`; close only the WebSocket connection cleanly on every exit path (success, timeout, navigation abort, error).

### 5.3 Early Dispatch, Classification, and `site-cache.json` Isolation (`src/fetch.rs`)

- **Early dispatch (zero side effects from ordinary fetch)**: Inside `fetch_document` (taking `FetchTarget`), branch on `FetchTarget::AttachCdp { expected_origin }` **before** `resolve_and_check_robots`, `site_rules`, `site_cache`, `try_fetch_raw_source`, and `acquire_page`.
  - Never consults `site-rules.json` (never inherits a host's `wait_for_selector` or `rewrite_host`).
  - Never checks CEF availability.
  - Never reads or writes `ctx.site_cache` on success, timeout, challenge, or origin mismatch.
- **Attached classification (`classify_cdp_attach_result`)**:
  - Keep the `status_code == 0` rejection intact for all **navigated** CEF and CDP renders (`src/fetch.rs:1971`).
  - For attached captures (`strategy = "cdp_attach"`, `escalated = false`):
    1. Run `detect_interstitial_challenge(&res.html)` **first** (matching navigated CDP at `src/fetch.rs:1950`), returning `FetchError::Challenge { url: res.final_url, status: 0, reason }` (keeping HTTP status honestly `0` rather than fabricating `403`!).
    2. Run `classify_response_with_headers(200, is_html, expected_origin.as_deref(), Some(&res.final_url), &res.html, &DiagnosticHeaders::default(), &mut parsed_html)` so HTTP-200 challenges, `login_wall` (password form / login path), `cookie_wall`, DOM-inferred soft-404s, and `incomplete_shell` classify identically to live pages while keeping `FetchOutput.status_code = 0` (`"0 indicates HTTP status was not observed because pleb attached to an already-loaded tab"`).
    3. Set `FetchOutput.url` to `expected_origin.unwrap_or_else(|| res.final_url.clone())` (always populated with a valid URL, even for URL-less attach).
    4. Pass into the shared post-acquisition extraction + `finalize_extracted_document` tail (running `--expand`, `ui_gates`, offline `--resources manifest`, `--tools manifest`, `--focus`, `--section`, `--offset`, and `--max-bytes`).

---

## 6. Harness Escalation Contract (`skills/pleb/SKILL.md`, `README*`, `llms*.txt`)

Document one deterministic escalation matrix across `skills/pleb/SKILL.md`, `README.md`, `README-FULL.md`, `llms.txt`, and `llms-full.txt`:

| Signal from `pleb fetch` | Next Step |
| :--- | :--- |
| `exit 2` with `block_reason` in `["cloudflare_challenge", "bot_challenge"]` | Stop retrying in `pleb` (zero evasion scripts). Hand off to a **human** in a visible browser pane to complete the challenge. |
| `exit 2` with `block_reason` in `["login_wall", "cookie_wall"]` | Stop retrying in `pleb`; escalate to an interactive browser session (Playwright / browser subagent, headed for SSO/MFA). |
| `UI-Gated: ... in DOM (pass --expand tabs\|all)` | Stay in `pleb`: re-run with `--expand tabs` (or `--expand all`) combined with `--focus "<query>"` or `--section "Tab: <label>"`. |
| `UI-Gated: ... unmounted` or `visual canvas` / `modal` | Escalate to Playwright / interactive browser subagent to click the interactive control, scroll, or inspect visual content. |
| Returning from Playwright with a dense post-interaction DOM | Pipe `page.content()` into `pleb extract --url <url> --expand tabs --focus "<query>"`, or snapshot the local loopback tab via `pleb fetch --render cdp --cdp-url ws://127.0.0.1:<port>/devtools/page/<id> --no-navigate --focus "<query>"`. |

---

## 7. Verification, Corpus Fixtures, and Tier 3 Acceptance Gates

### 7.1 Corpus Harness Extensions (`tests/corpus_evaluation.rs`)

Before adding fixtures, extend `VariantSpec` (`tests/corpus_evaluation.rs:34`) with `#[serde(deny_unknown_fields)]` (preventing silently ignored fixture keys) and explicit typed assertions:
- `expand: Option<String>` (`"none"` / `"tabs"` / `"all"`)
- `section: Option<String>` and `focus: Option<String>`
- `expected_ui_gates: Vec<ExpectedUiGate>` (`kind`, `presence`, `in_dom_labels`, `unmounted_labels`, `capped`)
- `expected_ui_gate_count: Option<usize>`
- `forbidden_all_outputs_substrings: Vec<String>` — asserted simultaneously across extracted `content`, `title`, `resources`, `tools`, `ui_gates` (JSON-serialized), rendered manifests, and formatted `stderr` cues!

Follow Tier 1/Tier 3 Generator–Verifier protocol (`AGENTS.md`): add fixtures as `"tier": "frontier"`, confirm each fails for the intended reason while `ratchet == 1.000`, implement the fix, then promote to `"tier": "ratchet"`.

### 7.2 Corpus & Security Fixtures (`tests/corpus/`)

1. **`frontier_aria_tabpanels_hidden.html`**: In-landmark `[role="tablist"]` (`JavaScript` active; `Rust` `[hidden]`, `Go` `style="display: none"`, `curl` `class="hidden"`).
   - `--expand none`: content keeps only `JavaScript`; `ui_gates` reports `tablist` / `in_dom` / `in_dom_labels: ["Rust", "Go", "curl"]` / `active_label: "JavaScript"` / `gated_count: 3`.
   - `--expand tabs`: emits exact ATX headings `### Tab: JavaScript`, `### Tab: Rust`, `### Tab: Go`, `### Tab: curl` (verifying no `\[` escaping from `htmd`), `ui_gates` empty; `--section "Tab: Rust"` returns the Rust block; `--focus "cargo"` under a restrictive `--max-bytes` budget keeps `Tab: Rust` and collapses sibling tabs.
2. **`frontier_adversarial_disclosure_injection.html` (Inverse Security Fixture — `Leaks == 0`)**:
   - Hidden control `<button role="tab" style="opacity:0" aria-controls="e1">Fake</button>` + `<div id="e1" role="tabpanel" hidden>LEAK_HIDDEN_CTRL</div>`.
   - Concealed span inside a visible tab button `<button role="tab" aria-controls="ok">Rust<span style="opacity:0">LEAK_LABEL_OPACITY</span></button>` and inside a section `<h2>Guide<span style="font-size:0">LEAK_HEADING</span></h2>`.
   - Mixed style on panel: `style="display: none; opacity: 0"` (`LEAK_MIXED_STYLE`) and `style="display: none"` enclosing `<span style="opacity:0">LEAK_NESTED_SPAN</span>` and `<math><annotation encoding="application/x-tex">x^2</annotation></math>` alongside `<div style="opacity:0">LEAK_TEX_ADJACENT</div>`.
   - Hidden WebMCP forms: `<form class="hidden" toolname="leak_cls" tooldescription="bad">` and `<form inert toolname="leak_inert" tooldescription="bad">`, plus a hidden form inside a rescued tabpanel (`leak_inside_panel`).
   - Ambiguous duplicate `id="dup"` across two panels (`LEAK_DUP_ID`) and UTF-8 multi-byte trailing text in `style="left: -1000é"`.
   - **Assertions**: Positive control (`cargo build`) present under `--expand tabs`; `forbidden_all_outputs_substrings` verifies zero `LEAK_*` strings across `content`, `tools`, `resources`, `ui_gates`, manifests, and `stderr` cues under `--expand none`, `--expand tabs`, and `--expand all`.
3. **`frontier_article_two_competitors_expand.html` (Baseline Root Stability)**:
   - Two `<article>` elements where Article A wins baseline visible prose (`>= 65%`), while Article B holds large hidden tabpanels that would overtake Article A if measured post-expansion.
   - **Assertions**: Under `--extract article --expand tabs`, Article A stays selected and Article B's tabpanels neither appear in `content` nor leak into `ui_gates`.
4. **`frontier_unmounted_and_mixed_tabs.html`**:
   - Mixed tablist (`JavaScript` active, `Rust` in-DOM with short 7-byte `go test` snippet, `Go` unmounted) -> `presence: "mixed"`, `in_dom_labels: ["Rust"]`, `unmounted_labels: ["Go"]`, plus pure unmounted React tablist -> `presence: "unmounted"` remaining unmounted after `--expand tabs`.
5. **`frontier_nav_accordion_non_landmark.html` (Inverse False-Positive)**:
   - `<nav>` dropdown with `[aria-expanded="false"][aria-controls]` + disabled/submit "Load more" buttons -> remains pruned under `--expand all` with empty `ui_gates`.
6. **`frontier_canvas_and_pagination.html`**:
   - Sparse `<main>` with `<canvas width="640" height="360" aria-label="Latency percentiles">` and `<button type="button">Load more</button>` -> `visual_canvas` (`opaque`, generic cue count only) and `pagination` (`unmounted`).

### 7.3 Integration Test Suites (Hermetic `/tmp/pleb-t-<pid>-<seq>`, `PLEB_NO_HOME_CEF=1`, `PLEB_NO_KEYCHAIN=1`)

- **`tests/extract_cli_tests.rs`**:
  - `stdin` and file path extraction; `--expand tabs` + `--section` + `--offset` (valid combination) vs `--focus` + `--section` / `--offset` (exit `1` conflicts); `--resources manifest` / `--tools manifest` crossing a `--max-bytes` boundary (verifying continuation `next_offset` matches finalized output); `--json -o <path>` (writes JSON to file, empty stdout, atomic temp+rename preserving prior file on failure); `--metadata-only` requiring `--json` and rejecting `-o`; TTY stdin fail-fast; rejecting `--max-response-bytes` on `extract`; `--max-input-bytes` exact-limit success vs `limit + 1` overflow (`response_too_large`); binary guard exit `5` on PDF with omitted MIME, mislabeled PNG, generic NUL input, and **success** on valid UTF-16 BOM HTML; verifying zero `daemon.sock` or state-dir creation even with an unwritable `PLEB_HOME`.
- **`tests/cdp_tests.rs`** (mock loopback CDP WebSocket server):
  - **Outbound RPC audit**: asserts complete outbound method log on `--no-navigate` includes `Page.enable`, `Page.getFrameTree`, `DOM.getDocument`, `DOM.getOuterHTML` and strictly excludes `Fetch.enable`, `Fetch.continueRequest`, `Fetch.failRequest`, `Network.enable`, `Page.navigate`, `Kitesurf.setLayoutMode`, `Target.createTarget`, `Target.closeTarget`, `Browser.close`—including when the mock server injects unsolicited `Fetch.requestPaused` events, errors, or sudden close!
  - **TOCTOU document-swap tests**: (a) redirect/navigation during `--wait-for-selector`, (b) document change between `DOM.getOuterHTML` and `Accessibility.getFullAXTree`, and (c) tab navigates away and back to the same URL (new `loaderId`) -> all abort with zero leaked HTML/AX/tools.
  - **Scope, size, and endpoint guards**: rejects `/devtools/browser`, `/devtools/page/`, remote `wss://`, `chrome://`, `about:blank`, missing `loaderId`, `unreachableUrl`, expected-origin mismatch (`RedirectBlocked`), oversized successful `Accessibility.getFullAXTree` reply (`ResponseTooLarge`), and child-frame WebMCP tool injection.
  - **Classification & cache isolation**: challenge HTML fails fast (`exit 2`, `cloudflare_challenge`, `status_code == 0` / zero fallback, BEFORE selector wait expires); login/cookie walls exit `2`; prepopulated `site-cache.json` stays untouched on success, timeout, challenge, and origin mismatch; `--format axtree` passes `frameId`; single-item `--json`, `--jsonl`, and `--metadata-only` golden outputs verified.
- **`tests/cli_tests.rs` & `tests/mcp_and_setup_tests.rs`**:
  - CLI/doc parity across `README.md`, `README-FULL.md`, `skills/pleb/SKILL.md`, `llms.txt`, and `llms-full.txt`.
  - MCP `tools/list` schema parity (`expand`, `no_navigate`, optional `url`), rejecting explicitly blank `url: ""`, verifying `UI-Gated:` header (including `metadata_only: true` with concealed-text absence check), and verifying zero regressions in existing navigated-CDP and CEF challenge suites (`tests/fetch_tests.rs`).

### 7.4 Tier 3 Standing Acceptance Gates (`AGENTS.md`)

1. Land one compiling **Wave Header Commit** on `main` introducing shared types/signatures (`ExpandPolicy`, `UiGateDescriptor`, `FetchTarget`, `finalize_extracted_document`).
2. Maintain Generator–Verifier separation (`corpus-verifier` owns `tests/corpus/` and `tests/corpus_evaluation.rs`; authors never modify corpus assertions).
3. Enforce `untyped_error_matches == 0` across all transport/CDP code (replace `.contains("DOM Error while querying")` at `src/transport/cdp.rs:1715` with typed pre-validation / RPC error handling).
4. Close the wave with a pure **Subtraction Pass** (`ratchet == 1.000`, `signal_density` non-decreasing, net negative lines) before `cargo install --path . --features cronet --locked`:

```bash
cargo fmt --check
cargo clippy --all-targets --features cronet -- -D warnings
cargo test --features cronet
cargo test --test corpus_evaluation
```

---

## 8. Forward Compatibility Seam Reservations (Deferred Milestones)

Milestone 7 stops strictly at read-only rescue, affordance indexing, offline extraction, and passive loopback CDP attach. Two follow-on capabilities layer cleanly onto these exact types without breaking wire compatibility:

1. **Declarative Pre-Capture Reveal (`--reveal <tabs|disclosures|all>`, `--scroll <N>`, and XHR JSON Dataset Indexing)**: Reuses `UiGateKind` semantics to click bounded in-landmark `Unmounted` controls inside `CdpSession` (and optionally a `v6` CEF protocol), capturing per-tab DOM snapshots before switch-unmount while indexing same-origin `application/json` XHR URLs into `FetchOutput.resources` (`ResourceKind::Dataset`).
2. **Domain-Pinned Authenticated Session Handoff**: Accepts an ephemeral Playwright `storageState.json` pinned to an explicit `--cookie-domains` allowlist (stripped on cross-origin redirects, never persisted to `<state_dir>/data/` or shared across `daemon.sock`), enabling high-concurrency `pleb fetch` / `pleb crawl` over private read-only portals after a one-time Playwright login.
