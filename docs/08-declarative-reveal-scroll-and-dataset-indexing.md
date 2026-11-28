# Milestone 8 Specification: Declarative Pre-Capture Reveal, Progressive Scroll, and XHR Dataset Indexing

## 1. Context and Scope

Milestone 7 added zero-network in-DOM ARIA expansion (`--expand`), UI-gate affordance indexing (`ui_gates`), offline `pleb extract`, and passive loopback CDP attach (`--no-navigate`). However, when a site renders interactive content **only after click or scroll**—such as React tabs that unmount inactive panels (`UiGatePresence::Unmounted`), single-open accordions, infinite-scroll / virtualized feeds, or JSON API endpoints loaded dynamically via `fetch()`/`XHR`—`pleb` previously had to hand off to an interactive browser session.

Milestone 8 closes that gap natively inside rendered browser transports (`--render cef` and `--render cdp`, plus `--render auto` routing when `--reveal` or `--scroll` is set) using strictly bounded, declarative, read-only actions:

1. **Declarative Pre-Capture Reveal (`--reveal <none|tabs|disclosures|all>`)**: Automatically expands and snapshots in-landmark `Unmounted` tabs and `[aria-expanded="false"]` disclosures that cannot be recovered from static HTML, before restoring the initially active tab or capturing subsequent exclusive accordions.
2. **Bounded Progressive Scroll (`--scroll <0..=20>`)**: Scrolls downward in bounded viewport steps with DOM/XHR settle waits and automatic stagnation early-exit to materialize lazy-loaded sections and trigger XHR data fetches.
3. **Passive Same-Origin XHR JSON Dataset Indexing (`ResourceKind::Dataset`)**: Passively records same-origin `GET` `2xx` `XHR`/`Fetch` responses carrying JSON MIME types during browser rendering and indexes them into `FetchOutput.resources` (`section_heading: "Network XHR"`)—making underlying API payloads inspectable via `--resources manifest` and downloadable via `--stage-resources <id>`.

### Strict Security and Scope Boundaries

- **Zero arbitrary script evaluation from CLI/MCP**: Neither `pleb` CLI nor MCP exposes a free-form `--eval` or `click(selector)` flag. All DOM interaction executes from one compiled, audited script (`cef/reveal_script.js` / `cef/reveal_script.h`).
- **Pre-flight challenge gate**: If the settled page matches an active Cloudflare/anti-bot interstitial challenge, `--reveal` and `--scroll` abort immediately (`0` clicks, `0` scrolls) and let `pleb` classify into `FetchError::Challenge` (exit `2`).
- **In-landmark non-destructive controls only**: Navigation headers/footers/sidebars/dialogs/menus, `<form>` submit buttons, `<a href>` links (unless empty/`#`), `aria-haspopup` popovers, disabled controls, and controls whose visible label matches auth/destructive/challenge verbs (`sign in`, `log in`, `register`, `buy`, `checkout`, `delete`, `remove`, `submit`, `cookie`, `verify`, `captcha`, etc.) are never clicked.
- **Navigation-abort fail-safe**: Origin + pathname are recorded before scroll/reveal; if any scroll or click mutates `location.origin + location.pathname`, capture aborts immediately, post-navigation HTML and panels are discarded, and `pleb` falls back cleanly with an advisory `warning`.
- **Passive `--no-navigate` remains strictly passive**: Combining `--no-navigate` with `--reveal` or `--scroll > 0` fails loud (`exit 1` / MCP `-32602`).
- **Milestone 9 deferred**: Domain-pinned authenticated `storageState.json` cookie handoff remains deferred.

---

## 2. Shared Bounds and Constants (`src/transport/reveal.rs`)

| Constant | Value | Purpose |
| :--- | :--- | :--- |
| `MAX_REVEAL_ACTIONS` | `16` | Maximum total clicks per page across all tablists and disclosures |
| `MAX_REVEAL_PER_GROUP` | `8` | Maximum tabs clicked within a single `[role="tablist"]` |
| `MAX_SCROLL_STEPS` | `20` | Hard upper bound on `--scroll <N>` |
| `MAX_REVEALED_PANEL_HTML_BYTES` | `262144` (`256 KiB`) | Per-panel raw `outerHTML` ceiling |
| `MAX_TOTAL_REVEAL_META_BYTES` | `4194304` (`4 MiB`) | Total serialized `BrowserCaptureMetadata` ceiling (CEF/daemon/CDP) |
| `MAX_XHR_DATASETS` | `12` | Maximum indexed XHR JSON `ResourceDescriptor` items per page |
| `MAX_RAW_CAPTURED_DATASETS` | `32` | Maximum raw captured `XHR`/`Fetch` entries buffered prior to filtering |

---

## 3. Domain Types and Extraction Composition

### 3.1 Domain Types (`src/transport/reveal.rs`)

```rust
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevealPolicy {
    #[default]
    None,
    Tabs,
    Disclosures,
    All,
}
```

- `RevealPolicy::implies_expand(self, current: ExpandPolicy) -> ExpandPolicy`:
  - `RevealPolicy::None => current`
  - `RevealPolicy::Tabs => if matches!(current, ExpandPolicy::All) { ExpandPolicy::All } else { ExpandPolicy::Tabs }`
  - `RevealPolicy::Disclosures | RevealPolicy::All => ExpandPolicy::All`
- Setting `--reveal` automatically enables the corresponding static in-DOM `--expand` rescue so hidden panels already present in the initial/post-scroll DOM are never waisted on redundant clicks.
- `BrowserCaptureMetadata` carries `revealed_panels: Vec<RevealedPanelSnapshot>`, `datasets: Vec<CapturedDataset>`, `reveal_capped: bool`, `scroll_steps_performed: usize`, and `reveal_warning: Option<String>`.

### 3.2 Canonical Reveal Script (`cef/reveal_script.js` & `cef/reveal_script.h`)

A single self-contained async JS routine `__plebRevealAndScroll(cfg)` runs identically in CDP (`Runtime.evaluate` with `awaitPromise: true, returnByValue: true`) and CEF (`cef/protocol.h` poller):

1. **Challenge check**: Scans visible body `< 4000` chars and challenge DOM markers; returns `{ challenge_detected: true }` immediately with zero interactions.
2. **Phase A — Progressive Scroll (`scrollSteps > 0`)**: Scrolls `scrollingElement` (or a tall scrollable `<main>` container) in `max(floor(0.85 * vh), 600)` steps, awaiting `180ms` `MutationObserver` quiet + `__plebInflight === 0` (per-step cap `1200ms`), terminating early after 2 consecutive stagnant steps at bottom.
3. **Phase B — In-Landmark Reveal (`mode !== "none"`)**:
   - **Tablists (`tabs` | `all`)**: Snapshots the initially active panel before switching tabs so mount/unmount frameworks keep all tabs; skips inactive tabs whose panel is already structurally hidden in-DOM with non-empty text (already rescued by implied `--expand`); clicks up to `8` eligible inactive tabs, captures each panel's `outerHTML`, restores the initial active tab.
   - **Disclosures (`disclosures` | `all`)**: Clicks visible in-landmark `[aria-expanded="false"]` controls (skipping structurally hidden in-DOM panels already rescued by `--expand`) and snapshots newly expanded containers/targets before exclusive accordions can collapse them.

### 3.3 Rust-Side HTML Merging & UI-Gate Reconciliation

Before `enrich_document_resources` and `finalize_extracted_document`:

1. **`merge_revealed_panels`** passes each `RevealedPanelSnapshot.html` through `extract_document_parsed` (`ExtractMode::Clean`, `ExpandPolicy::None`), guaranteeing identical `ElementVisibility` stripping (`opacity:0`, `font-size:0`, `aria-hidden`, `inert`, hidden WebMCP forms) on revealed HTML as on the main page.
2. Empty or whitespace-normalized duplicates of already extracted content are dropped.
3. Unique revealed panels append clean `Tab: <label>` / `Disclosure: <label>` sections (`##..######` in Markdown, `[Tab: <label>]` in Text, `<section><h3>...</h3></section>` in HTML) and merge newly discovered `resources` into `doc.resources`.
4. `doc.ui_gates` removes revealed labels from corresponding `Tablist`/`Disclosure` descriptors, recomputes `gated_count` and `presence` (`InDom` / `Unmounted` / `Mixed`), removes resolved gates, and sets `capped = true` if `reveal_capped`.

### 3.4 Passive XHR Dataset Filtering (`filter_and_index_xhr_datasets`)

Passively captured `GET` `2xx` JSON `XHR`/`Fetch` responses undergo strict fail-closed filtering before entering `doc.resources`:
- Allowed MIME types: `application/json`, `text/json`, or `application/*+json` (excluding `application/problem+json` and `application/manifest+json`).
- **Exact same-origin** check against `final_url` (`scheme`, `canonical_url_host`, `port_or_known_default`) plus `is_safe_subresource_url`.
- **Sensitive query stripping/rejection**: any URL carrying query parameter names matching `token`, `access_token`, `auth`, `key`, `api_key`, `apikey`, `secret`, `sig`, `signature`, `jwt`, `session`, `password`, or `credential` is dropped before cleaning.
- **Noise filtering**: drops telemetry/analytics/health/well-known/manifest/oembed paths and `byte_count <= 2` empty `{}`/`[]` responses.
- Remaining unique URLs (up to `MAX_XHR_DATASETS = 12`) are indexed as `ResourceKind::Dataset`, `ResourceSalience::High`, `section_heading: "Network XHR"`, `label: "XHR JSON: <stem>"`.
- `src/artifacts.rs` maps `*+json` to `.json` and rejects `text/html` / HTML-sniffed payloads when `expected_kind == Some(ResourceKind::Dataset)`.

---

## 4. Protocol Upgrades: CEF `v6-meta` and Daemon `PROTOCOL_VERSION = 5`

Per `AGENTS.md` zero backwards-compatibility ceremony:

- **CEF Protocol (`v6-meta`)** (`cef/protocol.h`, `cef/renderer_core.h`, `src/transport/cef.rs`):
  - `EXPECTED_PROTOCOL = "v6-meta"`, `PLEB_RENDERER_PROTOCOL_VERSION = "v6-meta"`.
  - `PlebRenderRequest` adds `"reveal": "none"|"tabs"|"disclosures"|"all"` and `"scroll": 0..=20`.
  - Response JSON header adds `"meta_bytes": <usize>`; raw response payload transmits `html_bytes` followed immediately by `meta_bytes` JSON (`BrowserCaptureMetadata`).
  - `on_resource_load_complete` passively captures main-frame `RT_XHR` `UR_SUCCESS` `GET` `2xx` JSON responses into bounded `g_datasets[32]` alongside the JS poller's `g_reveal_meta`.
- **Transport Daemon (`PROTOCOL_VERSION = 5`)** (`src/daemon/protocol.rs`, `client.rs`, `server.rs`):
  - `PROTOCOL_VERSION = 5`.
  - `DaemonRequest::CefRender` adds `reveal` and `scroll_steps`.
  - `DaemonResponseHeader` adds `meta_bytes`; frame body is `html_bytes || meta_bytes` so client framing remains a single length-bounded transfer.

---

## 5. CDP Transport Integration (`src/transport/cdp.rs`)

- **Navigated `CdpSession`**:
  - Tracks `GET` `XHR`/`Fetch` requests in `xhr_get_requests` and passively records `2xx` indexable JSON responses on `Network.responseReceived`.
  - Keeps Kitesurf `layout=full` when `capture_axtree || reveal != None || scroll_steps > 0`.
  - After settling and reading `initial_html`, runs pre-flight status/challenge/block checks; only invokes `Runtime.evaluate` (`build_cdp_reveal_eval_js(...)`) on clean non-challenge pages.
  - Fails loud (`ProtocolMismatch`) if `Runtime.evaluate` returns `-32601`.
  - Aborts and restores `initial_html` with an advisory `reveal_warning` if navigation occurs mid-reveal.
- **Passive Attach (`PassiveAttachSession`, `--no-navigate`)**:
  - Remains strictly read-only (`Network.enable` and `Runtime.evaluate` stay forbidden).

---

## 6. CLI, MCP, and Escalation Contract

- **`pleb fetch`**: adds `--reveal <none|tabs|disclosures|all>` (default `none`) and `--scroll <0..=20>` (default `0`).
- **Validation**: rejects `--scroll > 20` and rejects non-zero `--scroll` or non-`none` `--reveal` combined with `--no-navigate`, `--render cronet`, `--stage`, or `--format raw` (CLI exit `1` / MCP `-32602`). With `--render auto`, automatically selects CDP (if `--prefer-cdp` or CDP configured without CEF) or CEF, failing loud if neither browser renderer is available.
- **MCP `pleb_fetch`**: exposes optional `reveal` (`"none"|"tabs"|"disclosures"|"all"`) and `scroll` (`integer`, `0..=20`), mirroring exact CLI validation.
