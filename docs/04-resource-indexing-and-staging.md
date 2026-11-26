# Milestone 4 Specification: Resource Salience Indexing, SVG/Transcript Rescue, and Session-Aware Staging

## 1. Purpose and Scope

AI agents reading technical web pages routinely encounter high-value non-prose artifacts: architecture diagrams inside `<figure>`, inline `<svg>` flowcharts whose `<text>` labels are discarded during DOM pruning, `<video>`/`<audio>` `<track>` caption transcripts (`.vtt`/`.srt`), and linked `.pdf` specifications, `.csv` datasets, or OpenAPI `.yaml`/`.json` schemas.

Eagerly downloading every subresource wastes bandwidth and latency on icons, avatars, and badges, whereas returning text-only Markdown without resource metadata leaves agents blind to asset salience, dimensions, and hotlink-protected staging paths.

This specification adds:
1. **Zero-Network `ResourceDescriptor` Salience Indexing**: extracted during the existing `ParsedHtml` pass with zero extra HTTP requests, surfaced on `FetchOutput.resources` (`--json` / `--jsonl` / `--metadata-only`), a concise `stderr` cue line, and `--resources manifest`.
2. **Zero-Network Inline `<svg>` `<text>` Label Rescue**: preserving readable node/edge labels inside content `<svg>` diagrams (`[Diagram: <label> — Client, Sequencer, WAL]`) while dropping icon sprites.
3. **First-Class WebVTT & SubRip Transcript Extraction (`text/vtt`, `.srt`)**: stripping timing cues and rolling-caption duplicates while preserving speaker changes, both for direct fetches and `--resources transcripts`.
4. **Session-Aware Subresource Staging (`<state_dir>/artifacts/`)**: fetching selected or policy-matched resources through `daemon.sock` with Chrome subresource headers (`Referer` governed by `strict-origin-when-cross-origin` + `Sec-Fetch-Dest` / `Sec-Fetch-Mode` / `Sec-Fetch-Site`), magic-byte validation, SSRF guards, and `256 MiB` LRU eviction so agents can inspect staged PNG/JPEG/WebP/PDF/dataset files on demand via multimodal file tools without burning vision tokens upfront.

---

## 2. Domain Model and CLI Contract

### 2.1 Resource Types (`src/extract/resources.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Diagram,
    Image,
    Transcript,
    Document,
    Dataset,
    Schema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceSalience {
    High,
    Medium,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    pub identifier: String, // "r1", "r2", ... in document order
    pub kind: ResourceKind,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<(u32, u32)>,
    pub salience: ResourceSalience,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}
```

### 2.2 Resource Policy and CLI Flags (`src/policy/mod.rs`, `src/main.rs`)

- `--resources <none|manifest|transcripts|diagrams|all>` (`#[derive(ValueEnum)] pub enum ResourcePolicy`, default: `None`):
  - `none` (default): performs zero subresource network requests; populates `resources` in `--json` / `--jsonl` (including `--metadata-only`) and prints a single concise `stderr` cue line when `!--quiet`, `!json`, and `>= 1` `High`-salience resource exists (`Resources: 2 diagrams (r1, r2), 1 transcript (r3) — pass --resources manifest or --stage-resources <ids>`).
  - `manifest`: appends a `## Linked Resources` Markdown table (`ID | Kind | Salience | Section | Dimensions | Label | URL`) to `stdout` when resources are non-empty.
  - `transcripts`: fetches up to `4` `Transcript` resources, converts `.vtt`/`.srt` into clean prose, and appends `## Media Transcript` sections to `stdout` (honoring `--max-bytes`).
  - `diagrams`: stages up to `8` `High`-salience `Diagram` resources (`<= 16 MiB` each) into `<state_dir>/artifacts/`, populates `staged_path` / `byte_count` / `content_type` on `ResourceDescriptor`, and appends a `## Staged Resources` summary (plus rewriting matching Markdown image targets to local paths when staged).
  - `all`: executes `transcripts` plus stages up to `8` `High`-salience `Diagram`, `Document`, `Dataset`, and `Schema` resources.
- `--stage-resources <IDS>` (comma-separated resource identifiers, e.g. `--stage-resources r1,r3`): selectively stages (or inlines, for `Transcript`) only the named resources from the fetched document.
- `--referer <URL>`: optional parent page URL supplying `strict-origin-when-cross-origin` `Referer` and `Sec-Fetch-*` context on direct asset fetches.
- `--stage`: stages a directly fetched URL (binary image, PDF, dataset, or document) into `<state_dir>/artifacts/<hash16>-<stem>.<ext>` instead of failing with exit code `5` (`UnsupportedContentType`). In `--json` mode, emits `{ url, final_url, status_code, content_type, byte_count, staged_path, timing_ms }`; in plain mode, prints `staged_path` followed by `\n` to `stdout` (with status metadata on `stderr` when `!--quiet`). `--stage` conflicts with batch mode and `--output`.

---

## 3. Subsystem Specifications

### 3.1 Salience Indexing & Inline SVG Rescue (`src/extract/dom.rs`, `src/extract/resources.rs`)

1. **Inline `<svg>` `<text>` Rescue (`src/extract/dom.rs`)**:
   - Skip `<svg>` elements inside interactive controls (`a`, `button`, `nav`, `header`, `footer`, `[role="button"]`), `<svg>`s with `width < 64` or `height < 64`, or icon classes (`icon`, `octicon`, `lucide`, `feather`, `fa-`).
   - Collect deduplicated visible text segments from `<text>` and `<foreignObject>` descendants (ignoring `<style>`, `<script>`, `<defs>`, single-character punctuation, or segments `> 80` chars), capped at `32` segments.
   - If `svg_label` (`aria-label` or direct `<title>`) is present: emit `[Diagram: <svg_label>]` when `0` text segments exist, or `[Diagram: <svg_label> — <seg_1>, <seg_2>, ...]` when `1..=32` segments exist.
   - If `svg_label` is absent and `3..=32` distinct text segments exist (`total_chars >= 12`, e.g., an unlabeled inline Mermaid/Graphviz flowchart): rescue as `[Diagram: <seg_1>, <seg_2>, ...]`.
2. **`ResourceDescriptor` Extraction (`src/extract/resources.rs`)**:
   - Walk the pruned content root (`select_root_id` after `prune_dom` and after `<picture>`/`srcset`/`data-src` image normalization, while preserving `<video>`/`<audio>`/`<track>` tags during `prune_dom` long enough to index `<track>` before detaching media chrome):
   - Track the current `section_heading: Option<String>` from `h1..=h6` elements in document order, and deduplicate emitted `ResourceDescriptor`s by resolved URL.
   - **Images (`<img>`)**:
     - Reject noise: declared `width < 100 || height < 80`, `role="presentation"|"none"`, `aria-hidden="true"`, `data:` URIs, or badge/avatar/tracker patterns (`shields.io`, `badge`, `avatar`, `gravatar`, `emoji`, `1x1`, `pixel`, `.ico`).
     - Classify `Diagram` (`High`) when inside `<figure>` (using `<figcaption>` or `alt` as `label`), OR `width >= 300 && height >= 150`, OR `label`/URL path contains technical visual terms (`diagram`, `arch`, `flow`, `state`, `chart`, `plot`, `graph`, `bench`, `schema`, `topology`, `figure`, `fig`, `pipeline`, `sequence`, `flamegraph`).
     - Classify `Image` (`Medium`) when inside prose with `alt.len() >= 16`; drop uncaptioned decorative images.
   - **Media Transcripts (`<track>`)**:
     - Match `<track src="...">` where `kind` is absent or `"captions"` / `"subtitles"` / `"descriptions"` (or `.vtt`/`.srt` extension): `ResourceKind::Transcript` (`High`).
   - **Primary Linked Artifacts (`<a href="...">`)**:
     - Match resolved HTTP/HTTPS destinations by path/extension:
       - `.pdf` -> `Document` (`High`, `application/pdf`)
       - `.csv`, `.tsv`, `.parquet`, `.jsonl` -> `Dataset` (`High`)
       - `.proto`, `.sql`, `openapi.yaml`/`openapi.yml`/`openapi.json`/`swagger.json`, `.schema.json`, `llms.txt`, `llms-full.txt` -> `Schema` (`High`).

### 3.2 WebVTT & SubRip Transcript Extraction (`src/extract/transcript.rs`, `src/extract/mod.rs`)

- Detect transcripts when MIME is `text/vtt` or `application/x-subrip` (`application/srt`), or when content starts with `WEBVTT` (after optional UTF-8 BOM).
- In `ExtractMode::Full` or `OutputFormat::Html`: preserve verbatim passthrough with `truncate_utf8`.
- In `ExtractMode::Clean` / `ExtractMode::Article` (`Markdown` or `Text`):
  - Skip `WEBVTT` header metadata, `STYLE`, `NOTE`, and `REGION` blocks, numeric cue counters, and `HH:MM:SS.mmm --> ...` / `MM:SS.mmm --> ...` timing lines.
  - Extract `<v Speaker Name>` voice tags into `**[Speaker Name]:** ` (Markdown) or `[Speaker Name]: ` (Text) whenever the active speaker changes; strip remaining inline cue tags (`<c...>`, `<i>`, `<b>`, `<00:...>`), decode basic HTML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, `&nbsp;`), and collapse adjacent duplicate lines caused by rolling HTML5/YouTube captions.

### 3.3 Subresource Headers, Referer Policy, and SSRF Guard (`src/policy/source.rs`, `src/transport/`, `src/daemon/`)

1. **`is_safe_subresource_url(parent: &Url, target: &Url) -> bool` (`src/policy/source.rs`)**:
   - Require `http` or `https` scheme and forbid `https -> http` scheme downgrade when `parent` is `https`.
   - When `parent` host is local/private (loopback tests), require exact same host and port; when `parent` host is public, reject any local, RFC 1918 private, link-local (`169.254.0.0/16`), CGNAT (`100.64.0.0/10`), or IPv4-mapped IPv6 private target while **allowing** public cross-origin CDN hosts (e.g., `cdn.example.com`, `user-images.githubusercontent.com`).
2. **`compute_safe_referer(parent: &Url, target: &Url) -> Option<String>` (`src/policy/source.rs`)**:
   - Implements Chrome's default `strict-origin-when-cross-origin`:
     - `https` parent -> `http` target: `None`.
     - Same origin: `parent` with username, password, and fragment cleared (`https://example.com/post?a=1`).
     - Cross origin: serialized origin with trailing `/` (`https://example.com/`).
3. **Subresource Transport Options (`src/transport/mod.rs`, `src/transport/cronet.rs`, `src/daemon/protocol.rs`)**:
   - Define `SubresourceDest` (`Image`, `Asset`) and `RequestHeaderOptions { pub accept_markdown: bool, pub referer: Option<String>, pub subresource_dest: Option<SubresourceDest> }`.
   - Bump `daemon::PROTOCOL_VERSION` (`2 -> 3`) and replace `accept_markdown: bool` (or add `referer` + `subresource_dest` alongside `accept_markdown`) on `DaemonRequest::HttpGet`.
   - When `subresource_dest` is `Some(SubresourceDest::Image)`: set `Accept: image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8`, `Sec-Fetch-Dest: image`, `Sec-Fetch-Mode: no-cors`, and `Sec-Fetch-Site` (`same-origin` / `same-site` / `cross-site`), omitting `Sec-Fetch-User` and `Upgrade-Insecure-Requests`. When `Some(SubresourceDest::Asset)`: set `Accept: */*`, `Sec-Fetch-Dest: empty`, `Sec-Fetch-Mode: no-cors`, and `Sec-Fetch-Site`. Attach `Referer` when `referer` is `Some`.

### 3.4 Content-Addressed Artifact Store (`src/artifacts.rs`, `src/context.rs`, `src/fetch.rs`)

- Directory: `<state_dir>/artifacts` (`0o700`, files `0o600`), removed on `wick session clear` (`clear_state_dir` in `src/context.rs`).
- `stage_artifact(state_dir: &Path, source_url: &Url, content_type: Option<&str>, bytes: &[u8]) -> Result<StagedArtifact>`:
  - **Disguised-HTML Rejection**: if expected kind/extension is binary or image (`.png`, `.jpg`, `.webp`, `.gif`, `.pdf`) or `--stage` receives `text/html` error/interstitial markup when URL has an image/PDF extension (or `bytes` starts with `<!DOCTYPE html` / `<html` while claiming `image/*` or `application/pdf`), reject with a clear error rather than saving corrupt HTML under `.png`/`.pdf`!
  - Compute 64-bit FNV-1a (or SHA/SipHash) content digest over `(source_url.as_str(), bytes)` -> `<hex16>-<sanitized_stem>.<verified_ext>` (sanitizing `stem` to `[A-Za-z0-9_-]`, capped at `40` chars, writing atomically via `.tmp.<pid>` + `rename`).
  - Enforce `MAX_ARTIFACT_CACHE_BYTES = 256 * 1024 * 1024` (`256 MiB`) by evicting oldest existing files in `<state_dir>/artifacts` when total directory size exceeds the cap.

---

## 4. Swarm Execution Plan (Single Wave + Subtraction Barrier)

1. **Wave Header Commit (`conductor` on `main`)**:
   - Define shared types/signatures (`ResourceKind`, `ResourceSalience`, `ResourceDescriptor` in `src/extract/resources.rs`; `ResourcePolicy` in `src/policy/mod.rs`; `SubresourceDest`, `RequestHeaderOptions`, `TransportClient::get_with_request_options` stub in `src/transport/mod.rs`; `PROTOCOL_VERSION = 3` in `src/daemon/protocol.rs`; `pub mod artifacts;` stub in `src/lib.rs` + `src/artifacts.rs`; `ExtractedDocument.resources`, `FetchOptions` resource/stage/referer fields, `FetchOutput.resources` in `src/fetch.rs`). Verify `cargo test --features cronet` compiles and passes on `main`.
2. **Parallel Slices (`workspace: "share"`)**:
   - `ratchet-judge`: owns `tests/corpus_evaluation.rs` and `tests/corpus/*` (adds Milestone 4 `"frontier"` fixtures for `ResourceDescriptor` salience filtering, inline `<svg>` `<text>` label rescue, and WebVTT/SRT transcript extraction; gates all commits with adversarial SSRF/disguised-HTML/path-sanitization tests and promotes `"frontier"` -> `"ratchet"`).
   - `subresource-net`: owns `src/transport/{mod,cronet}.rs`, `src/daemon/{protocol,client,server}.rs`, `src/policy/{source,mod}.rs`, and `tests/daemon_tests.rs` (`is_safe_subresource_url`, `compute_safe_referer`, `SubresourceDest` + `Referer` + `Sec-Fetch-*` across Cronet/reqwest/`daemon.sock` v3).
   - `dom-indexer`: owns `src/extract/{mod,dom,resources,transcript}.rs` and `tests/extract_tests.rs` (inline `<svg>` `<text>` rescue, `ResourceDescriptor` salience pass, `format_resource_manifest_markdown`, and WebVTT/SRT `transcript.rs`).
   - `artifact-stager`: owns `src/artifacts.rs`, `src/context.rs`, `src/fetch.rs`, `src/main.rs`, `README.md`, `tests/fetch_tests.rs`, and `tests/cli_tests.rs` (`<state_dir>/artifacts/` atomic store + disguised-HTML guard + `256 MiB` LRU + `clear_state_dir`, `--resources`, `--stage-resources`, `--referer`, `--stage`, parallel `Semaphore(2)` staging, and `README.md` sync).
3. **Wave Barrier Subtraction Pass & Host Sync (`artifact-stager` -> `ratchet-judge` -> `conductor`)**:
   - `artifact-stager` runs the closing Whole-Repo Subtraction Pass on `swarm/m4-subtraction`, `ratchet-judge` verifies zero regressions (`ratchet == 1.000`), and `conductor` rebases onto `main`, pushes `origin/main`, runs `cargo install --path . --features cronet --locked`, and live-smoke-tests resource cueing and staging.
