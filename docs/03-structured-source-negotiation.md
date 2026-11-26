# Milestone 3 Specification: Structured Source Negotiation, Native Markdown, and Feed Extraction

## 1. Purpose and Scope

Many high-value agent targets—developer documentation platforms (`llms.txt`, Cloudflare Markdown-for-Agents, Mintlify, Nextra, Stripe, Antigravity docs), source forges (GitHub blobs, GitLab blobs, GitHub Gists), and release/blog syndication endpoints (RSS 2.0, Atom 1.0, JSON Feed 1.1)—expose lossless machine-structured or raw Markdown representations alongside heavy HTML chrome.

Currently in `mesh-wick`:
1. Fetching `https://github.com/<owner>/<repo>/blob/<ref>/<path>` downloads hundreds of kilobytes of React HTML chrome instead of the raw Markdown or source file.
2. `TransportClient` discards HTTP `Link` response headers and `ParsedHtml` discards `<head>` without inspecting `<link rel="alternate" type="text/markdown" href="...">`, missing zero-risk Markdown mirrors advertised by documentation servers.
3. Native `text/markdown` responses hit `is_passthrough_non_html` (`src/extract/mod.rs`), returning `title: None`, leaving YAML frontmatter noise unstripped, leaving relative Markdown links unresolved, skipping `compact_markdown`, and slicing via `truncate_utf8` instead of fence-safe `truncate_markdown` (leaving unclosed code fences under `--max-bytes`).
4. `application/rss+xml` and `application/atom+xml` hit `mime.ends_with("+xml")` in `is_passthrough_non_html`, dumping raw XML tags and `<![CDATA[...]]>` blocks into Markdown output instead of a structured Markdown digest.

This specification adds **zero-fingerprint-regression structured source acquisition**, **first-class `text/markdown` extraction**, and **RSS / Atom / JSON Feed Markdown conversion**.

---

## 2. Key Architectural Decisions

### 2.1 Preserve Exact Chrome Navigation Headers by Default (`SourceMode::Auto`)

Bot-scoring WAFs correlate `User-Agent: Chrome/...` and `Sec-Fetch-Mode: navigate` against Chrome's exact `Accept: text/html,application/xhtml+xml,...` header. Mutating `Accept` globally to `Accept: text/markdown, ...` on every primary GET would degrade TLS/HTTP fingerprint parity on strict WAF origins.

- **Decision**: Introduce `SourceMode` (`--source <auto|negotiate|none>`, default: `auto`):
  - `SourceMode::Auto` keeps Chrome's exact primary `Accept` header untouched on generic origins, while (a) rewriting known lossless raw-source forge URLs (GitHub/GitLab/Gist) with automatic `404`/`>= 400` fallback to the original HTML URL, and (b) following server-advertised HTTP `Link` or HTML `<link rel="alternate" type="text/markdown">` mirrors on a second targeted GET (`Accept: text/markdown, text/plain;q=0.9, */*;q=0.1`).
  - `SourceMode::Negotiate` (and hosts with `accept_markdown: true` in `site-rules.json`) explicitly sends `Accept: text/markdown, text/x-markdown;q=0.95, text/html;q=0.9, application/xhtml+xml;q=0.8, */*;q=0.7` on the primary HTTP GET, and probes `<path>.md` on documentation paths when HTML is returned without an advertised alternate.
  - `SourceMode::None` disables all source rewrites, alternate-link follows, and Markdown `Accept` negotiation.
- **Bypass Invariants**: Structured source resolution is automatically bypassed whenever:
  - `OutputFormat` is `Html` or `Raw` (callers requesting HTML or raw bytes receive the exact target representation).
  - `RenderMode` is `Cef` (`--render cef` explicitly requests Chromium DOM rendering).
  - `--no-rewrite` disables Tier A URL rewrites (`github.com` / `gitlab.com` / `gist.github.com` and `site-rules.json` host rewrites); `--source none` disables both URL rewrites and alternate/negotiated Markdown resolution.

### 2.2 Observable Acquisition Strategy Metadata

`FetchOutput.strategy` (`--json` / `--jsonl` and stderr status) records the exact acquisition path so calling agents know what representation was used:
- `"raw_source"`: deterministic forge raw URL rewrite succeeded (`raw.githubusercontent.com`, GitLab `/-/raw/`, `gist.githubusercontent.com`).
- `"markdown_mirror"`: HTTP `Link` or HTML `<link rel="alternate" type="text/markdown">` (or `.md` doc mirror) succeeded.
- `"markdown_negotiated"`: origin returned `Content-Type: text/markdown` (or `text/x-markdown`) directly via `Accept` content negotiation.
- `"cronet"` / `"reqwest"` / `"cef"`: standard transport strategies.

---

## 3. Subsystem Specifications

### 3.1 Transport `Link` Header Capture and `accept_markdown` Option (`src/transport/`, `src/daemon/`)

1. **`HttpResponse` (`src/transport/mod.rs`)**:
   - Add `pub link_header: Option<String>` to `HttpResponse` (joining multiple `Link` response headers with `", "` when more than one is present).
   - Capture `Link` case-insensitively in `src/transport/cronet.rs` (`on_response_started`) and in `reqwest` (`src/transport/mod.rs`).
2. **Per-Request `HttpGetOptions` (`src/transport/mod.rs`, `src/daemon/protocol.rs`)**:
   - Add `pub accept_markdown: bool` to `DaemonRequest::HttpGet` (`#[serde(default)]`) and `pub link_header: Option<String>` to `DaemonResponseHeader` (`#[serde(default)]`). Bump `daemon::PROTOCOL_VERSION` (`1 -> 2`) so any live v1 daemon transparently restarts via existing protocol-version mismatch handling.
   - Add `TransportClient::get_with_options(&self, url: &str, timeout: Duration, max_body_bytes: usize, accept_markdown: bool) -> Result<HttpResponse>` (`get` and `get_with_limits` delegate with `accept_markdown = false`).
   - When `accept_markdown == false`, send exact `chrome_header_pairs`. When `accept_markdown == true`, replace `Accept` with `"text/markdown, text/x-markdown;q=0.95, text/html;q=0.9, application/xhtml+xml;q=0.8, */*;q=0.7"`.

### 3.2 Forge Raw-Source Rewrites and Alternate Link Resolution (`src/policy/source.rs`, `src/policy/rules.rs`, `src/fetch.rs`)

1. **`SiteRule` Extension (`src/policy/rules.rs`, `data/site-rules.json`)**:
   - Add `#[serde(default)] pub accept_markdown: bool` to `SiteRule` so users or bundled rules can mark specific documentation origins safe for primary `Accept: text/markdown` negotiation in `SourceMode::Auto`.
2. **Deterministic Forge Raw-Source Resolver (`src/policy/source.rs`)**:
   - `resolve_raw_source_candidate(url: &Url) -> Option<RawSourceCandidate>` where `RawSourceCandidate { raw_url: Url, filename: String, kind: RawSourceKind }` and `RawSourceKind` is `Markdown` (`.md`, `.markdown`, `.mdx`) or `Code { language: Option<&'static str> }` (mapping `.rs`, `.py`, `.js`, `.ts`, `.tsx`, `.jsx`, `.go`, `.c`, `.h`, `.cpp`, `.m`, `.java`, `.kt`, `.swift`, `.rb`, `.sh`, `.zsh`, `.toml`, `.yaml`, `.yml`, `.json`, `.sql`, `.css`, `.html`, `.xml`, `.proto`, `Dockerfile`, `Makefile`):
     - **GitHub Blob**: `https://github.com/<owner>/<repo>/blob/<ref_and_path>` (at least 5 path segments, excluding binary extensions in `detect_binary_content` / image/archive suffixes) -> `https://raw.githubusercontent.com/<owner>/<repo>/<ref_and_path>`.
     - **GitLab Blob**: `https://gitlab.com/<namespace...>/<project>/-/blob/<ref_and_path>` -> `https://gitlab.com/<namespace...>/<project>/-/raw/<ref_and_path>`.
     - **GitHub Gist**: `https://gist.github.com/<user>/<gist_id>` (exact 2 non-empty segments, second segment hexadecimal `>= 8` chars, no trailing `/revisions`, `/forks`, `/stargazers`) -> `https://gist.githubusercontent.com/<user>/<gist_id>/raw`.
   - **Fallback on Ambiguous Ref / Error**: if fetching `raw_url` returns HTTP `>= 400` (for example, a GitHub branch name containing `/` that breaks naive `/blob/` path splitting) or binary content, `acquire_page` transparently falls back to fetching the original HTML `url`!
   - **Respect `robots.txt`**: when `respect_robots == true`, check `ctx.robots.is_allowed` on `raw_url` before fetching it.
3. **Alternate Markdown Link Discovery (`src/policy/source.rs`, `src/extract/dom.rs`)**:
   - **HTTP `Link` Header Parser**: `parse_markdown_link_header(header: &str, base: &Url) -> Option<Url>` parses RFC 8288 `<target>; param=val` entries where `rel` contains token `alternate` (case-insensitive) and `type` is `text/markdown` or `text/x-markdown`, resolving `target` against `base` (`http`/`https` only, rejecting cross-origin downgrades to `http` or private/loopback hosts when `base` is public).
   - **HTML `<link>` Discovery (`ParsedHtml::markdown_alternate_url(&self, page_url: &Url) -> Option<Url>`)**: inspects `<link rel="..." type="..." href="...">` inside `<head>` *before* DOM pruning, matching `rel` token `alternate` and `type` `text/markdown` or `text/x-markdown`, resolved against `effective_base_url(page_url)` with same-origin (or subdomain-of-same-registrable-domain) validation so untrusted third-party `<link>` tags cannot trigger SSRF.
   - **Mirror Fetch**: when `SourceMode` is `Auto` or `Negotiate` and an HTTP `Link` or HTML `<link>` Markdown alternate URL (distinct from `final_url`) is discovered on a `200 OK` HTML response, `acquire_page` performs one GET with `accept_markdown = true`; if it returns `200 OK` non-HTML text/markdown (non-empty, non-binary), `acquire_page` returns that Markdown payload with `strategy = "markdown_mirror"` and `final_url` set to the canonical page `final_url` (so relative links resolve cleanly against the mirror URL while reporting the document). On any failure/`>= 400`/HTML response, `acquire_page` keeps the already-fetched HTML page with zero error!
   - **`.md` Path Probe in `SourceMode::Negotiate`**: when `SourceMode::Negotiate` is explicitly requested, primary GET sends `accept_markdown = true` (returning `strategy = "markdown_negotiated"` if response `Content-Type` is `text/markdown` or `text/x-markdown`); if `text/html` is returned with no `alternate` link and `final_url` path does not end in `/` or `.html`/`.md`, probe `<path>.md` once (`accept_markdown = true`), accepting only `200 OK` with `Content-Type` `text/markdown` or `text/x-markdown` (never accepting soft-404 HTML).

### 3.3 First-Class Native Markdown Extraction (`src/extract/native_markdown.rs`, `src/extract/mod.rs`)

Remove `text/markdown` and `text/x-markdown` from raw `is_passthrough_non_html` when `options.format` is `OutputFormat::Markdown` or `OutputFormat::Text` (in `OutputFormat::Html`, emit verbatim body):

1. **YAML Frontmatter Extraction and Pruning**:
   - Detect leading `---\n...\n---\n` block at byte `0` (outside code fences): extract `title:` (unquoting single/double quotes) as `ExtractedDocument.title`.
   - Strip the YAML frontmatter block in `ExtractMode::Clean` and `ExtractMode::Article`; preserve it in `ExtractMode::Full`.
   - If frontmatter lacked `title:`, extract `title` from the first ATX `# Heading` outside code fences.
2. **Relative URL Hygiene Outside Code Fences**:
   - Rewrite inline Markdown links `[label](dest)` and images `![alt](dest)` (outside fenced code blocks and inline backtick code spans) through `resolve_and_clean_url(&dest, base_url)`, resolving relative `./foo.md` or `/assets/diagram.png` paths against the Markdown resource's URL and stripping `?utm_*` tracking parameters (honoring `--no-images` and `--no-links` consistently with HTML extraction!).
3. **Compaction, Title Injection, and Fence-Safe Truncation**:
   - Run `compact_markdown`, inject `# <title>` only if `title` is present and the document does not already begin with a matching `# ` heading, and apply `truncate_markdown` (guaranteeing `output.len() <= max_bytes` and zero unclosed fences).
   - For `OutputFormat::Text`, strip Markdown formatting/fences cleanly or render normalized text with `truncate_utf8`.
4. **Raw Source Code Wrapping (`wrap_raw_code_document`)**:
   - When Tier A `RawSourceKind::Code { language }` succeeds and `options.format == OutputFormat::Markdown`, wrap the source file in a fence long enough not to collide with internal backticks (`# <filename>\n\n```<lang>\n<code>\n````), then apply `truncate_markdown`! (In `OutputFormat::Text`, emit raw code with `truncate_utf8`.)

### 3.4 Structured Feed Extraction (`src/extract/feed.rs`, `src/extract/mod.rs`)

Detect syndication feeds when MIME is `application/rss+xml`, `application/atom+xml`, `application/rdf+xml`, or `application/feed+json`, OR when MIME is `application/xml` / `text/xml` and the root element (after optional `<?xml ...?>` / comments) is `<rss`, `<feed`, or `<rdf:RDF`:

- In `ExtractMode::Full` (or `OutputFormat::Html`), preserve verbatim passthrough with `truncate_utf8` so callers wanting raw XML/JSON feed text still have `--extract full` (or `--format raw`).
- In `ExtractMode::Clean` and `ExtractMode::Article` (`OutputFormat::Markdown` or `OutputFormat::Text`):
  - Parse RSS 2.0 / RDF (`<channel><title>`, `<description>`, `<item>` -> `<title>`, `<link>`, `<pubDate>`/`<dc:date>`, `<content:encoded>`/`<description>`) and Atom 1.0 (`<feed><title>`, `<subtitle>`, `<entry>` -> `<title>`, `<link rel="alternate" href="...">`, `<updated>`/`<published>`, `<summary>`/`<content>`) and JSON Feed 1.1 (`title`, `description`, `items[]` -> `title`, `url`/`external_url`, `date_published`, `content_html`/`content_text`/`summary`).
  - Clean HTML fragments inside item descriptions/summaries via `ParsedHtml` -> compact Markdown/text, resolve relative item URLs against `page_url` with tracking-parameter stripping, cap items to a sane bound (`<= 50`) before applying `truncate_markdown` / `truncate_utf8`, and return `# <Feed Title>` + `## [<Item Title>](<url>)` sections!

---

## 4. CLI Contract Updates (`src/main.rs`, `README.md`)

- Add `--source <auto|negotiate|none>` (`#[arg(long, value_enum, default_value_t = SourceMode::Auto, ignore_case = true)]`) to `wick fetch`.
- Document `--source`, Forge raw-source rewrites, `<link rel="alternate" type="text/markdown">` mirror resolution, native Markdown frontmatter/link handling, and RSS/Atom/JSON Feed extraction in `README.md` (covered by the `README.md` contract parity test in `tests/cli_tests.rs`).

---

## 5. Swarm Execution Plan (Single Wave + Subtraction Barrier)

1. **Wave Header Commit (`shepherd` on `main`)**:
   - Define `SourceMode` (`Auto`, `Negotiate`, `None`) in `src/policy/mod.rs`, add `pub source: SourceMode` on `FetchOptions` (`src/fetch.rs`), add `pub link_header: Option<String>` on `HttpResponse` and `get_with_options(..., accept_markdown: bool)` stub on `TransportClient` (`src/transport/mod.rs`), and add `accept_markdown` / `link_header` + `PROTOCOL_VERSION = 2` in `src/daemon/protocol.rs`. Verify `cargo check --all-targets --features cronet` passes cleanly on `main`.
2. **Parallel Slices (`workspace: "share"`)**:
   - `corpus-auditor`: owns `tests/corpus_evaluation.rs` and `tests/corpus/*` (adds `"frontier"` fixtures for native `text/markdown` frontmatter + relative links + `--no-images`/`--no-links` + fence truncation, RSS 2.0, Atom 1.0, JSON Feed 1.1, and HTML `<link rel="alternate" type="text/markdown">`; audits all 3 author commits with adversarial counterexample tests and promotes `"frontier"` -> `"ratchet"`).
   - `transport-link`: owns `src/transport/{mod,cronet}.rs`, `src/daemon/{protocol,client,server}.rs`, and `tests/daemon_tests.rs` (captures `Link` headers in Cronet, reqwest, and `daemon.sock`; wires `accept_markdown` header override; tests `Link` + `accept_markdown` + `PROTOCOL_VERSION` 2 over `daemon.sock`).
   - `markdown-feed`: owns `src/extract/{mod,dom,markdown,native_markdown,feed}.rs` and `tests/extract_tests.rs` (`ParsedHtml::markdown_alternate_url` with SSRF/origin guard, `native_markdown.rs`, `wrap_raw_code_document`, and `feed.rs` for RSS/Atom/JSON Feed).
   - `source-pipeline`: owns `src/policy/{source,rules,mod}.rs`, `data/site-rules.json`, `src/fetch.rs`, `src/main.rs`, `README.md`, `tests/fetch_tests.rs`, and `tests/cli_tests.rs` (GitHub/GitLab/Gist raw source resolver with 404 HTML fallback, HTTP `Link` + HTML `<link>` mirror follow, `--source <auto|negotiate|none>`, loopback end-to-end tests, and `README.md` sync).
3. **Wave Barrier Subtraction Pass & Host Sync (`source-pipeline` -> `corpus-auditor` -> `shepherd`)**:
   - After all slices reach `ratchet == 1.000` on `main`, `source-pipeline` runs the closing Whole-Repo Subtraction Pass, `corpus-auditor` verifies zero regressions, and `shepherd` rebases onto `main`, pushes `origin/main`, runs a live smoke verification against a GitHub `/blob/` URL, a `.md`-mirror doc URL, and an RSS/Atom feed, and runs `cargo install --path . --features cronet --locked`.
