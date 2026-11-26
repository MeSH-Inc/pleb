# Milestone 1 and 2 Specification: Extraction Pipeline, CEF `v4-meta`, Unified Transport Daemon, and Batch Fetch

## Goals and Scope

This specification defines the next two dependency-ordered milestones for `mesh-wick` (`wick`):

- **Milestone 1 — Core Fidelity, Extraction Pipeline, CEF `v4-meta`, and CLI Contract**: eliminate redundant HTML parses, add `<base href>`-aware URL resolution and tracking-parameter hygiene, guard against binary payloads, upgrade `wick-renderer` to `v4-meta` with real status/redirect metadata and idempotent stealth patches, and lock down a uniform `ValueEnum` + JSON/exit-code CLI contract.
- **Milestone 2 — Unified Warm Transport Daemon, Multi-URL Batch Fetch, and Search Enhancements**: promote the Unix-socket CEF daemon into a concurrent low-level transport daemon (`Ping`, `Shutdown`, `HttpGet`, `CefRender`) with proxy-partitioned storage and Cronet lifecycle hardening, add multi-URL / stdin batch `fetch`, and expand `search` for pipeline composition.

Per repository defaults, `mesh-wick` is greenfield and solo-owned: legacy protocols (`v3-cf-aware`), dead code (CEF one-shot mode, `--cookie`, `g_html_visitor`, `cef/build-linux.sh`, `WICK_PROXY`), and old CLI/env flag spellings are replaced outright rather than dual-supported.

---

## Key Architectural Decisions

### Unified Daemon Boundary: Low-Level Transport RPC (Option A)

The daemon owns **transport resources only** (`HttpGet` via Cronet/reqwest and `CefRender` via `wick-renderer`), exposed behind four RPC operations: `Ping`, `Shutdown`, `HttpGet`, and `CefRender`. Policy routing (`SiteRules`, `SiteCache`, `RobotsCache`), DOM extraction (`ParsedHtml`, `htmd`), `search`, `crawl`, and `map` remain in-process inside `mesh_wick` library calls.

- **Rationale**: `TransportClient` and `CefRenderer` route through the daemon transparently, keeping one execution path across the CLI, `--no-daemon`, and Rust library callers (`tests/fetch_tests.rs`). `crawl` and `map` stay cancellable and non-blocking instead of becoming multi-minute monolithic socket RPCs, and `BRAVE_API_KEY` never traverses `daemon.sock`. Concurrent `site-cache.json` updates across processes are serialized with an advisory `flock` around read-merge-write in `SiteCache::record`.

### CEF Wire Framing and Daemon Framing: Two-Part Frame

Both `wick-renderer` (`v4-meta`) and `daemon.sock` responses use a single two-part frame:

1. One UTF-8 JSON metadata line terminated by `\n`.
2. Immediately followed by the exact number of raw payload bytes declared in `body_bytes` (or `html_bytes` on the CEF pipe), which is `0` when no body is attached.

- **Rationale**: avoids base64 bloat for raw binary downloads and eliminates multi-megabyte HTML JSON escaping/unescaping on both C and Rust sides.

---

## Milestone 1: Extraction Pipeline, CEF `v4-meta`, and CLI Contract

### 1.1 Single-Parse DOM Pipeline (`src/extract/`, `src/policy/`, `src/fetch.rs`, `src/crawl.rs`)

- **Enable `scraper/atomic` (`Cargo.toml`)**:
  - Change `scraper = "0.26.0"` to `scraper = { version = "0.26.0", features = ["atomic"] }` so `scraper::Html` is `Send` and can move across `.await` points and `tokio::spawn` tasks. (`Html` remains `!Sync`, so always pass by ownership across `.await`, never `&ParsedHtml`.)
- **Introduce `ParsedHtml` (`src/extract/dom.rs`)**:
  - Wraps `scraper::Html`, reducing `scraper` parses per HTML fetch from 3 (`dom_text_metrics` + `extract_title` + `prune_html_for_markdown`) to 1. (`htmd 0.5.1` uses `html5ever 0.36` vs `scraper 0.26`'s `html5ever 0.39`, so `htmd` still performs one parse on the pruned HTML fragment.)
  - Static CSS selectors compiled once via `std::sync::OnceLock<Selector>` across `dom.rs`, `crawl.rs`, and `search.rs`.
  - Methods on `ParsedHtml`:
    - `parse(html: &str) -> Self`
    - `title(&self) -> Option<String>`
    - `base_href(&self) -> Option<String>` (extracts first `base[href]` **before** DOM pruning removes `<head>`)
    - `text_metrics(&self) -> DomTextMetrics`
    - `links(&self, base_url: &Url) -> Vec<Url>` (reused by `crawl.rs` and `map`, resolving against `effective_base` and applying tracking-parameter stripping)
    - `into_pruned_html(self, mode: ExtractMode) -> String`
    - `into_plain_text(self, mode: ExtractMode) -> String`
- **Lazy DOM Parsing in Response Classification (`src/policy/mod.rs`, `src/fetch.rs`)**:
  - Do not eagerly parse HTML before `classify_response`.
  - Update `classify_response` / `detect_js_shell` to accept `&mut Option<ParsedHtml>` and populate it lazily **only** when `is_html && status < 400 && body.len() <= 2_000_000` and no fast-path `noscript` string matched.
  - Carry the populated `Option<ParsedHtml>` on `AcquiredPage` straight into `extract_document` (and into `crawl`/`map` alongside actual `content_type`, replacing the hardcoded `Some("text/html")` in `crawl.rs:107`).
- **Base URL Resolution & Tracking-Parameter Hygiene (`src/extract/markdown.rs`, `src/extract/url.rs`)**:
  - Fix `fetch.rs:150-155`: pass parsed `page.final_url` (falling back to `&effective`) instead of pre-redirect `&effective` into `extract_document`.
  - Compute `effective_base = parsed.base_href().and_then(|b| final_url.join(&b).ok()).unwrap_or_else(|| final_url.clone())` before pruning strips `<head>`.
  - Shared URL hygiene helper (`clean_and_resolve_url(raw: &str, base: &Url) -> ResolvedLink`):
    - Strips ASCII control characters and whitespace before scheme inspection (catching `java\tscript:`).
    - Classifies `javascript:`, `vbscript:`, and `data:` as non-navigable (`UnwrapText` for anchors, `Drop` for images).
    - Leaves fragment-only anchors (`#section`) intact to avoid bloating internal TOC links.
    - Resolves relative URLs against `effective_base`.
    - Strips tracking parameters case-insensitively (`utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `mc_cid`, `mc_eid`, `igshid`, `_hsenc`, `_hsmi`, `ref_src`). **Only mutates the query string if at least one tracking parameter matched**, filtering raw `&`-delimited segments so untouched signed URLs (S3/CloudFront) preserve exact percent-encoding, and calling `url.set_query(None)` when all parameters are removed (preventing trailing `?`).
  - Implement dedicated `htmd 0.5.1` element handlers for `"a"` and `"img"` in `src/extract/markdown.rs` (avoiding `htmd`'s `RefCell` `BorrowMutError` and immutable `fallback()` slice trap):
    - `"a"`: walk children via `handlers.walk_children(element.node)` so nested `**bold**`, `` `code` ``, and images survive; drop links whose rendered content is whitespace-only (also cleaning up empty image-wrapper anchors under `--no-images`); unwrap text for `--no-links` or `javascript:`/`vbscript:`/`data:`; otherwise emit cleaned/resolved `[label](url "title")` with proper destination/title escaping.
    - `"img"`: resolve/clean `src` against `effective_base`; drop `data:` URIs and empty sources; emit `![alt](src "title")`.
- **Binary Content-Type Guard & Broader Text Passthrough (`src/fetch.rs`, `src/extract/mod.rs`)**:
  - Ordering in `acquire_page`: HTTP status classification (`>= 400`, `Blocked`, `Challenge`) executes **first**; binary detection executes **second** on raw `&resp.body` **before** `String::from_utf8_lossy`; HTML/text extraction runs third.
  - Detect binary payloads when:
    - `Content-Type` MIME matches `application/pdf`, `application/octet-stream`, `application/zip`, `application/gzip`, `application/x-tar`, `application/wasm`, `audio/*`, `video/*`, `font/*`, or `image/*` excluding `image/svg+xml`; OR
    - `Content-Type` is absent or `application/octet-stream` and leading bytes match binary magic (`%PDF-`, PNG, JPEG, GIF, `PK\x03\x04`, `\x1f\x8b`, `\x00asm`) or contain a `0x00` byte within the first 8 KiB. (Do not reject arbitrary non-UTF8 bytes so legacy ISO-8859-1/Windows-1252 HTML still extracts.)
  - On binary detection in non-raw mode, return `FetchError::UnsupportedContentType { url, content_type: Option<String>, byte_count }` with exit code `5` and guidance to re-run with `--format raw --output <path>`.
  - Broaden `is_passthrough_non_html` to accept all `text/*` except `text/html`, plus `application/json`, `application/xml`, `+json`, `+xml`, `application/javascript`, `application/yaml`, `application/toml`, `application/graphql`, and `image/svg+xml`.
- **Format-Specific Boundary-Aware Truncation (`src/extract/mod.rs`, `src/extract/markdown.rs`)**:
  - Keep `truncate_utf8(text, max_bytes)` format-agnostic (for JSON, XML, HTML, plain text): prefer cutting at the last newline within the final 15% of the allowed prefix before appending `\n\n[... truncated]`.
  - Add `truncate_markdown(md, max_bytes)` used only for `OutputFormat::Markdown`: track exact opening fence `(fence_char, fence_len)` (handling 4+ backtick fences emitted by `htmd`), reserve `1 + fence_len + SUFFIX.len()` bytes before slicing so output strictly satisfies `len <= max_bytes`, close an open fence before appending the truncation suffix, and rewind before the opening fence line if the cut lands on it.

### 1.2 CEF `v4-meta` Protocol and Renderer Hardening (`cef/`, `src/transport/cef.rs`, `src/fetch.rs`, `scripts/`)

- **Deletions (greenfield cleanup)**:
  - Delete one-shot mode, `--cookie`, and dead `g_html_visitor` from `cef/renderer.m` and `cef/renderer_linux.c`.
  - Delete `cef/build-linux.sh` and `WICK_PROXY` fallback in `cef/renderer_linux.c` and `src/main.rs`.
- **Protocol Version & Startup Handshake**:
  - Bump marker to `WICK_RENDERER_PROTOCOL=v4-meta` in `cef/renderer.m`, `cef/renderer_linux.c`, `scripts/install-cef-mac.sh`, and `scripts/install-cef-linux.sh`.
  - `find_renderer_in` / `CefRenderer` scans the installed binary for `b"WICK_RENDERER_PROTOCOL="` without spawning it; if missing or mismatched (e.g., `v3-cf-aware`), fail loudly with the exact rebuild command (`bash scripts/install-cef-mac.sh --force` or `install-cef-linux.sh --force`). `wick status` reports protocol compatibility directly.
  - On startup after `g_browser` creation, `wick-renderer` writes one ready line to stdout:
    - Success: `{"ready":true,"protocol":"v4-meta"}\n`
    - Failure: `{"ready":false,"protocol":"v4-meta","error":"..."}\n`
  - Accept `--cache-dir=<path>` on argv (overriding default `$MESH_WICK_HOME/cef-cache`) so the daemon can isolate cache paths per proxy key.
- **Per-Request Wire Protocol (stdin / stdout)**:
  - **Request (Rust -> `wick-renderer` stdin)**: one JSON line per request, read with bounded `getline` (max 64 KiB; reject oversized lines cleanly):
    ```json
    {"url":"https://x.com/user","wait_for_selector":"[data-testid='tweet']","timeout_ms":30000}
    ```
  - C parses the three fixed fields (`url`, `wait_for_selector`, `timeout_ms`) with a strict flat-JSON parser supporting standard escapes and `\uXXXX` (including UTF-16 surrogate pairs).
  - `wait_for_selector` is dynamically allocated (no 256-byte cap) and properly JS-escaped (escaping `\`, `'`, `"`, control chars, `\u2028`, `\u2029`); the JS poller is built with `asprintf` (no 2400-byte truncation) and wraps `document.querySelector(sel)` in `try/catch` so invalid selectors report an explicit error instead of hanging.
  - `timeout_ms` clamps C's `total_cap` to `min(default_cap, timeout_ms > 2000 ? timeout_ms - 1500 : timeout_ms * 3 / 4)` (leaving headroom for force-dump before Rust's deadline kills the process), and clamps `post_load_cap` and inner JS `cap` accordingly.
  - **Response (`wick-renderer` stdout -> Rust)**: two-part frame:
    ```
    {"status_code":200,"final_url":"https://x.com/user","html_bytes":12345,"error":null}\n
    <exact 12345 raw UTF-8 HTML bytes>
    ```
    `final_url` and `error` are JSON-escaped in C.
- **Metadata Capture, Load Errors, and CEF Refcount Fixes**:
  - `on_load_end`: filter strictly by `frame && frame->is_main(frame)`; store `g_status_code = httpStatusCode` and mark `g_committed_main_frame = 1`. Reset `g_status_code = 0`, `g_load_error = 0`, `g_committed_main_frame = 0` at start of each request and on main-frame `on_load_start` (never in `on_loading_state_change`).
  - `on_load_error`: filter by `frame && frame->is_main(frame)`; ignore `ERR_ABORTED (-3)` (normal during redirects/challenge reloads); for real errors set `g_load_error = errorCode`, `g_source_done = 1`, and emit `html_bytes: 0` + `"error": "load error <code>"` without running the JS poller on stale DOM. Guard force-dump on `total_cap` with `if (g_committed_main_frame)` and `(document.documentElement ? document.documentElement.outerHTML : '')`.
  - Sync `g_click_x = -1; g_clicked = 0` on `g_nav_count` change in both `renderer.m` and `renderer_linux.c`.
  - Capture `final_url` via `frame->get_url(frame)` and free with `cef_string_userfree_free`. Release every owned `frame` and `host` pointer acquired from `get_main_frame` / `get_host` via `->base.release(...)`.
  - In `on_console_message`: change `utf8.length > strlen(g_marker)` to `>=` and return `1` when capturing the marker so multi-MB HTML is not re-logged.
- **Unify `stealth.h` and Fix Double-Execution / Timezone Leak**:
  - Make `cef/helper.m` `#include "stealth.h"` (deleting the stale duplicated block).
  - Fix internal `stealth.h` defects: fold Patch #17 (`PluginArray.prototype`) into Patch #3; resolve Patch #22 vs Patch #11 `configurable: false` collision on `window.outerHeight`; make `STEALTH_JS` idempotent via an in-context guard so a second injection never throws mid-script and leaks `window.__markNative`; avoid re-injecting full `STEALTH_JS` in `renderer.m`'s `on_load_start` (where `wick Helper` always runs `on_context_created`); wrap `POST_LOAD_FIXUP`'s `chrome.runtime` methods cleanly.
  - Remove hardcoded `America/Denver` timezone from `stealth.h`. Only inject a timezone override when `MESH_WICK_TIMEZONE` is set (JS-escaped, preserving both call and construct behavior on `Intl.DateTimeFormat`).
- **Rust `CefRenderResult` & Retry/Classification Fixes (`src/transport/cef.rs`, `src/fetch.rs`)**:
  - `CefRenderer::render` takes per-request `timeout: Duration` and returns `CefRenderResult { html, final_url, status_code }`.
  - Remove the `< 2000` byte blind retry in `render_cef_checked`. Retry at most once on transient renderer crash, `detect_interstitial_challenge`, empty HTML, or `status_code == 0`, **never** retrying after a full timeout and only when remaining timeout budget permits.
  - Route `CefRenderResult` through `classify_response` so real `404`/`500` return `FetchError::HttpError` (exit 3), real `403`/`503` return `Blocked`/`Challenge` (exit 2) with the actual status code, and `SiteCache` records `"cef_timeout"` **only** on actual timeouts.

### 1.3 CLI Contract, `ValueEnum`, and Uniform Machine Output (`src/main.rs`)

- **Strict `clap::ValueEnum` Types**:
  - Derive `clap::ValueEnum` directly on `OutputFormat`, `ExtractMode`, `RenderMode`, `SearchProvider` (canonical `duckduckgo`, alias `ddg`), and `Freshness` (`day`, `week`, `month`, `year`), deleting manual `FromStr` parsers.
- **Ada/RFC Naming Alignment**:
  - `--num` -> `--limit` (`-n`, consistent across `search` and `map`)
  - `--meta-only` -> `--metadata-only`
  - `--no-cef-daemon` / `MESH_WICK_NO_CEF_DAEMON` -> `--no-daemon` / `MESH_WICK_NO_DAEMON`
  - `--cef-idle-timeout` / `MESH_WICK_CEF_IDLE_SECS` / `daemon run --idle-secs` -> `--daemon-idle-timeout` / `MESH_WICK_DAEMON_IDLE_SECONDS` / `daemon run --idle-timeout`
  - Add `content_bytes: usize` directly to `FetchOutput` so `main.rs` uses one serializer path.
- **Clean `Drop` Exit & Uniform JSON Contract**:
  - Change `main()` to return `std::process::ExitCode` after `run(cli).await` finishes and drops `RuntimeContext` (eliminating `std::process::exit`, which previously skipped `Cronet_Engine_Shutdown` and CEF cleanup).
  - Make `--json` a `global = true` flag supported across all subcommands (`fetch`, `search`, `crawl`, `map`, `status`, `daemon`, `session`).
  - Route **all** command errors through a single typed error formatter emitting `{"error": "...", "kind": "...", "exit_code": N}` to stderr when `--json` (or `--jsonl`) is active.
  - Add `-o` / `--output <path>` to `fetch` (writes/creates file only after fetch succeeds; bypasses raw terminal guard; conflicts with batch mode).
  - Exit code table:
    - `0`: success
    - `1`: invalid input, unavailable/mismatched CEF renderer, transport/runtime failure, or heterogeneous all-failed batch
    - `2`: blocked (`403`/`503`), anti-bot challenge, or `robots.txt` disallow
    - `3`: origin HTTP `4xx`/`5xx` error
    - `4`: partial batch failure (some URLs succeeded, some failed)
    - `5`: unsupported binary content type in non-raw mode

---

## Milestone 2: Unified Warm Transport Daemon, Multi-URL Batch Fetch, and Search Enhancements

### 2.1 Cronet Lifecycle Hardening (`src/transport/cronet.rs`, `src/transport/mod.rs`)

Because the daemon keeps `Cronet_Engine` alive across many concurrent requests, fix four latent bugs in `src/transport/cronet.rs` before sharing `Engine`:

1. **Fix 32 KB `Cronet_Buffer` leak per request**: store the allocated `Cronet_BufferPtr` on `RequestState` and call `Cronet_Buffer_Destroy` in `finish_request`.
2. **Fix `CancelGuard::drop` lock-across-FFI hazard**: coordinate cancellation with an atomic/state transition so no `std::sync::Mutex` is held while invoking `Cronet_UrlRequest_Cancel`.
3. **Drain in-flight native requests in `Engine::drop`**: maintain an in-flight counter + `Condvar` on `Engine` (incremented before `Cronet_UrlRequest_Start`, decremented in `finish_request`), wait for count `== 0` before calling `Cronet_Engine_Shutdown` (checking its return code), `Cronet_Engine_Destroy`, and `Cronet_Executor_Destroy`.
4. **Enforce max response body cap & per-request timeout**:
   - Move `timeout` from `Engine::new` / `TransportClient::new` to per-call `get(url, timeout, max_body_bytes)`.
   - Enforce `max_body_bytes` inside `on_read_completed` (default `16 MiB` for document fetches, `128 MiB` for raw fetches, configurable via `--max-response-bytes`), canceling via `Cronet_UrlRequest_Cancel` with `FetchError::ResponseTooLarge` on overflow.
- Keep `CHROME_FULL` in `src/transport/mod.rs` aligned with the linked Chromium/Cronet version.

### 2.2 Unified Warm Transport Daemon (`src/daemon/`, replacing `src/transport/cef_daemon.rs`)

- **Socket, Lock, and Storage Isolation**:
  - Socket/lock paths: `state_dir/daemon.sock` (`0o600` inside `0o700` `state_dir`) and `state_dir/daemon.lock` (exclusive `flock`).
  - Legacy cleanup: on `daemon run` startup, `daemon stop`, and `session clear`, if legacy `cef.sock` exists, send `Shutdown` and unlink `cef.sock` / `cef.lock` so an older CEF daemon cannot hold `cef-cache/SingletonLock`.
  - Proxy-partitioned storage: key each pooled `(TransportClient, CefRenderer)` slot strictly by `Option<String>` proxy (never by timeout):
    - `None` -> key `"direct"` -> `state_dir/data/direct` and `state_dir/cef-cache/direct`
    - `Some(proxy)` -> key `"proxy-<fnv1a_64_hex16>"` -> `state_dir/data/<key>` and `state_dir/cef-cache/<key>`
  - Cap active proxy slots at 4 with LRU eviction of idle slots (only evicting slots with zero in-flight requests).
  - `--no-daemon`: check `daemon.lock`; if a live daemon owns `state_dir`, fail loudly directing the caller to run `wick daemon stop` or specify an isolated `--state-dir`.
  - `session clear`: must fail loudly if `daemon stop` fails or lock release is not observed before removing `data/`, `cef-cache/`, and `site-cache.json`.
  - Inside the daemon process, force `use_daemon = false` to prevent self-recursion.
- **Concurrent Accept Loop & Idle Accounting**:
  - Spawn a Tokio task per accepted connection; cap request header line read at 64 KiB.
  - Track `active_requests: AtomicUsize` (excluding `Ping`) + `Notify`; idle shutdown triggers only when `active_requests == 0` continuously for `idle_timeout`.
  - `Ping` returns immediately with `{ pid, protocol_version, build_id, active_requests, cronet_slots, cef_renderers_alive }` even while renders are in flight.
  - `HttpGet` runs concurrently on the slot's shared `Arc<TransportClient>`.
  - `CefRender` serializes per proxy slot on a Tokio `AsyncMutex`, tracking `timeout_ms` as an end-to-end deadline from request arrival (covering mutex wait + render) and selecting on client socket disconnect while waiting for the mutex so abandoned callers never waste renders.
- **Wire Protocol (`daemon.sock`)**:
  - **Client invariant**: clients keep the Unix socket write half open until the full response is read (never half-close after sending the request line). Server monitors `read_half` for unexpected `EOF`; on client disconnect, cancels the in-flight `HttpGet` via `CancelGuard` or aborts a queued `CefRender`.
  - **Requests** (single JSON line, max 64 KiB):
    - `{"op":"ping"}`
    - `{"op":"shutdown"}`
    - `{"op":"http_get","url":"...","proxy":null|"...","timeout_ms":30000,"max_body_bytes":16777216}`
    - `{"op":"cef_render","url":"...","wait_for_selector":null|"...","proxy":null|"...","timeout_ms":60000}`
  - **Responses** (two-part frame: JSON header line + exact `body_bytes` raw bytes):
    ```json
    {"ok":true,"protocol_version":1,"build_id":"...","status_code":200,"final_url":"...","content_type":"text/html","body_bytes":4096,"error":null}
    ```
    (`Ping` / `Shutdown` / errors emit `body_bytes: 0` plus status fields.)
- **Upgrade Safety, Startup Diagnostics, and Race-Free Restart**:
  - Bake compile-time `WICK_BUILD_ID` in `build.rs` (using `cargo:rerun-if-changed=src`, `Cargo.lock`, `cef`) and return it on every response. If a client observes a `protocol_version` or `build_id` mismatch, it sends `Shutdown`, waits for `daemon.lock` release, spawns the current executable, and retries the request once.
  - New `daemon run` retries `lock_file.try_lock()` for up to 2s on `WouldBlock` before exiting cleanly (eliminating the stop/restart lock race).
  - On shutdown/idle exit, unlink `daemon.sock` **before** dropping the listener so new clients immediately get `NotFound`. Clients retry once on `NotFound | ConnectionRefused | UnexpectedEof | ConnectionReset | BrokenPipe` when zero response bytes were read.
  - Delete `wait_for_socket_ready`'s throwaway connect-and-drop probe; retry connecting + sending the real request with short backoff.
  - Route detached daemon stderr to `state_dir/daemon.log` (not `/dev/null`) for startup diagnostics.
- **`SiteCache` Multi-Process Safety & `SiteRules` Reload (`src/policy/mod.rs`)**:
  - Guard `SiteCache::record` with an advisory `flock` on `site-cache.lock`, reading the latest on-disk map, merging the updated host entry, and atomically renaming into place.
  - Log a warning (via `tracing::warn!`) on malformed local `site-rules.json` overlay instead of silently swallowing JSON parse errors.

### 2.3 Multi-URL Batch Fetch (`src/fetch.rs`, `src/main.rs`)

- **Invocation**:
  - `wick fetch <url>...`: accepts one or more URLs, or `-` to read newline-separated URLs from stdin (trimming whitespace, skipping blank lines and `#` comments).
  - Empty stdin (0 URLs after filtering) fails loudly with exit code `1`.
- **Mode & Output Contracts**:
  - **Single-URL mode** (exact 1 positional URL, not `-`): preserves current stdout/stderr output; `--json` emits a single JSON object `{...}`.
  - **Batch mode** (`2+` positional URLs OR `-`, regardless of line count):
    - `--concurrency <N>` (default `4`, clamped to `1..=16`), with a per-host `Semaphore(2)` cap and zero artificial inter-request sleep.
    - `--jsonl`: streams one JSON object per completed URL as it finishes.
    - `--json`: emits a JSON array `[{...}, ...]` in input order.
    - Default text/markdown mode: emits sections using one shared multi-page Markdown formatter shared with `crawl::format_crawl_output` (`---\n## [i/N] <title>\n**URL:** <final_url>\n\n<content>`), printing failed entries clearly to stderr.
    - Uniform batch item JSON schema (shared by `--json` array and `--jsonl`):
      - Success: `{"url":"...","ok":true,"final_url":"...","status_code":200,"content_type":"...","title":"...","strategy":"...","escalated":false,"truncated":false,"timing_ms":120,"content_bytes":1024,"content":"..."}` (omitting `content` when `--metadata-only` is set).
      - Failure: `{"url":"...","ok":false,"error":"...","kind":"...","exit_code":2}`
    - Exit code in batch mode: `0` if all succeeded; `4` if mixed success/failure; if all failed and every failure shares exit code `c`, return `c`, else `1`.
  - **Enforced `clap` conflicts**:
    - `--format raw` conflicts with batch mode, `--json`, and `--jsonl`.
    - `--jsonl` conflicts with `--json`.
    - `--metadata-only` requires `--json` or `--jsonl`.
    - `--output <path>` conflicts with batch mode.

### 2.4 Search Enhancements (`src/search.rs`, `src/main.rs`)

- **Pooled Brave HTTP Client**:
  - Store a lazy `OnceLock<reqwest::Client>` (configured with `config.proxy`) on `RuntimeContext` for Brave Search API requests, avoiding per-call client construction even when `--features cronet` is enabled.
- **New CLI Flags**:
  - `--urls-only`: prints one bare URL per line with no markdown framing; conflicts with `--json`; prints **nothing** to stdout on empty results so `wick search --urls-only "query" | wick fetch -` never pipes `"No results found."`.
  - `--freshness <day|week|month|year>`: maps to Brave `freshness=pd|pw|pm|py` and DuckDuckGo `df=d|w|m|y`.
  - `--site <domain>`: normalizes away accidental `http://`/`https://` prefixes and trailing slashes, prepending `site:<domain> ` to the query.
  - `--extra-snippets`: passes `extra_snippets=true` to Brave Search API and includes `extra_snippets: Vec<String>` on `SearchResult` (`skip_serializing_if = "Vec::is_empty"`, gracefully empty on DuckDuckGo fallback).
- **DuckDuckGo Parser Cleanup**:
  - Collapse inline whitespace consistently in `parse_ddg_html` (matching `clean_html_text` used for Brave) and filter out internal/ad `//duckduckgo.com/...` links lacking a `uddg=` target.

---

## Verification Plan

### Milestone 1 Verification
- `cargo test` and `cargo test --features cronet` covering:
  - `ParsedHtml` single-parse title, `<base href>`, metrics, and link extraction (`scraper/atomic` `Send` assertion across `.await`).
  - Relative URL resolution against post-redirect `final_url` and `<base href>`, `#fragment` preservation, `javascript:` text unwrapping, empty-link removal, and signed-URL-safe tracking-parameter stripping (no trailing `?`, untouched encoding on non-tracking queries).
  - Binary content-type and magic-byte/NUL rejection (exit code `5`) with `image/svg+xml` and Latin-1 HTML exemptions, plus status-before-binary ordering.
  - `truncate_utf8` vs fence-aware `truncate_markdown` (including 4-backtick fences and strict `len <= max_bytes` invariant).
  - `ValueEnum` CLI parsing, flag conflicts, `--output` no-empty-file-on-error, and uniform JSON error envelopes.
  - CEF `v4-meta` protocol parser/framing tests (mock renderer script emitting `ready` + two-part `v4-meta` frames, testing real `status_code`, `final_url`, `ERR_ABORTED`, `on_load_error`, selector escaping with single quotes, and stale `v3-cf-aware` binary rejection).
- Rebuild macOS CEF renderer (`bash scripts/install-cef-mac.sh --force`) and live-verify `wick status` + a CEF-rendered page returning real `status_code` and `final_url`.

### Milestone 2 Verification
- `cargo test` and `cargo test --features cronet` covering:
  - Cronet `Engine` stress test: concurrent requests + cancellation + drop without buffer leak, deadlock, or shutdown failure, plus `max_body_bytes` overflow cancellation.
  - Unified daemon tests: `Ping` responsiveness during an active blocked request, concurrent `HttpGet`, client disconnect cancellation (both active `HttpGet` and queued `CefRender`), proxy-partitioned storage slots without `-201` collision, `build_id` mismatch auto-restart, `daemon stop` / restart lock retry, accept-during-idle-exit retry, `session clear` safety, and legacy `cef.sock` shutdown.
  - `SiteCache` concurrent multi-writer merge under `flock`.
  - Batch `fetch` tests: multi-URL and stdin `-` (including 1-line stdin -> JSON array, empty stdin -> exit 1, `--jsonl` streaming, mixed success/failure -> exit 4, uniform all-fail -> shared exit code, heterogeneous all-fail -> exit 1).
  - `search` tests: `--urls-only` (including empty results -> empty stdout), `--freshness` query mapping for Brave and DDG, `--site` normalization, `--extra-snippets`, and DDG ad/whitespace cleanup.
