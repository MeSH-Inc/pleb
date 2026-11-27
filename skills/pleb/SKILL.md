---
name: pleb
description: Local web acquisition, search, crawl, and browser-rendering CLI (Cronet/reqwest HTTP, v5-meta CEF daemon, CDP/Kitesurf axtree, structured source bypass, YouTube captions, feed/transcript extraction, WebMCP indexing, Web Bot Auth signing, resource staging, and local credential vault). Use when fetching pages that resist built-in web fetch, searching the web via pleb search, storing search/signing keys via pleb auth, crawling or mapping sites, rendering SPAs/accessibility trees, staging diagrams/PDFs/transcripts, or diagnosing daemon and CEF state.
metadata:
  author: Memory Sync Hub LLC
  repository: https://github.com/MeSH-Inc/pleb
  scope: user
---

# pleb

Local, MIT-licensed, zero-telemetry web acquisition and search CLI by Memory Sync Hub LLC.

## Identity and Version Contract

Verify the active `pleb` binary before relying on `pleb`-specific flags:

```bash
pleb status --json
```

Genuine `pleb` installations report `"version"`, `"cef_renderer": { "expected_protocol": "v5-meta" }`, `"cdp_renderer"`, `"agent_identity"`, and `"daemon": { "supported": true }` (the unified `daemon.sock` transport daemon starts lazily on demand and reports `protocol_version: 4` while running; `protocol_version: null` when stopped is normal). `"cronet_enabled"` is `true` for `--features cronet` source builds and `false` for pure-Rust `reqwest` (`rustls`) builds (`cargo install pleb`) — both share the same CLI contract.

---

## Decision Tree 1 — Page Acquisition and Escalation

1. **Default fetch (`--render auto --source auto --extract clean`):**
   ```bash
   pleb fetch <url>
   ```
   - Skips `robots.txt` by default on single-URL fetches (pass `--respect-robots` to enforce).
   - **Web Bot Auth (RFC 9421 Ed25519):** When `web-bot-auth-jwk` and `web-bot-auth-agent` are configured, public `https://` acquisitions attach `Signature`, `Signature-Input`, and `Signature-Agent` headers (pass `--no-web-bot-auth` / `PLEB_NO_WEB_BOT_AUTH=1` to disable; `http://`, local/private targets, `pleb search`, `robots.txt`, `sitemap.xml`, and CDP handshakes stay unsigned).
   - **Structured source resolution (`--source auto`):** automatically rewrites GitHub `/blob/`, GitLab `/-/blob/`, and Gist URLs to lossless raw source (`strategy: "raw_source"`, with automatic HTML fallback on `404`/binary), and follows RFC 8288 `Link` / HTML `<link rel="alternate" type="text/markdown">` mirrors (`strategy: "markdown_mirror"`).
   - **First-class non-HTML & YouTube extraction:** handles native `text/markdown` (YAML frontmatter `title` promotion + relative link resolution + fence-safe truncation), RSS 2.0 / Atom 1.0 / JSON Feed 1.1 digests, WebVTT (`.vtt`) / SubRip (`.srt`) transcripts, and `youtube.com`/`youtu.be` watch metadata, descriptions, chapters, and caption tracks (`ytInitialPlayerResponse` over Cronet without CEF; pull captions via `--resources transcripts` or `--stage-resources`).
   - **Extraction scopes (`--extract`):** `clean` (default: strips nav/footer/aside/banners/scripts across the whole page), `article` (scopes to primary `<article>` / `<main>`), or `full` (keeps full body, YAML frontmatter, and verbatim feed/transcript XML/JSON).
   - **Transport & redirect SSRF guard:** both Cronet (Chromium 154) and `reqwest` block `3xx` redirects to non-HTTP(S) schemes or private/loopback/IMDS hosts from public origins (`redirect_blocked` transport error).
   - **Automatic CEF / CDP escalation:** escalates `403`/`503` blocks and unhydrated JS/SvelteKit/React SPA shells to local CEF (`v5-meta`) or a configured CDP / Kitesurf WebSocket endpoint (`--cdp-url` / `PLEB_CDP_URL`; pass `--prefer-cdp` / `PLEB_PREFER_CDP=1` to prefer CDP over CEF). If a JS shell is detected when neither CEF nor CDP is available (or escalation fails), `pleb` exits `0` with an explicit `Warning:` on `stderr` and `FetchOutput.warning`. Active bot challenges fail fast (exit `2`) without retries.
2. **Documentation Markdown negotiation (`--source negotiate`):**
   ```bash
   pleb fetch --source negotiate <url>
   ```
   Sends `Accept: text/markdown` on the primary request (`strategy: "markdown_negotiated"`) and probes `<path>.md` if HTML returns without an advertised alternate. Use `--source none` (or `--format html` / `--format raw` / `--format axtree` / `--render cef` / `--render cdp`) to disable source resolution.
3. **Forced Chromium or Remote CDP / Kitesurf rendering (`--render cef` / `--render cdp`):**
   ```bash
   pleb fetch --render cef --wait-for-selector "<css>" <url>
   pleb fetch --render cdp --cdp-url wss://<worker>/devtools/browser <url>
   pleb fetch --format axtree --cdp-url wss://<worker>/devtools/browser <url>
   ```
   Use `--render cronet` to force network-only transport without spawning CEF or connecting to CDP. CDP fails closed when `--proxy` is set so WebSockets never bypass `--proxy`; plaintext `ws://` is restricted to loopback hosts (`peer_addr().ip().is_loopback()` verified). Local Docker/`wrangler` Kitesurf runs page JS in `workerd`, which is not a hardened sandbox — target Cloudflare Workers Dynamic Workers (or an OS-sandboxed browser) for untrusted public sites.
4. **Active bot challenge or login wall (exit code `2`):**
   `pleb` never solves CAPTCHAs or runs anti-bot bypass scripts. Stop retrying via `pleb` and switch to an interactive browser session.

---

## Decision Tree 2 — Selective Resource Enrichment

`pleb fetch` indexes high-salience page resources (`diagram`, `image`, `transcript`, `document`, `dataset`, `schema`) during its single `O(N)` DOM pass with zero extra network requests, and rescues inline `<svg>` `<text>` labels directly into Markdown (`[Diagram: <label> — ...]`).

1. **Read prose first and check resource cues:**
   - Default non-JSON fetches print a one-line `stderr` cue whenever high-salience resources exist (`Resources: 2 diagrams (r1, r2), 1 transcript (r3) ...`).
   - `--json` / `--jsonl` / `--metadata-only` include the full `resources` array (`identifier`, `kind`, `url`, `section_heading`, `label`, `dimensions`, `salience`).
   - If the prose and rescued `<svg>` labels answer the task, stop — zero extra requests or vision tokens spent.
2. **Pull only the resources that enrich context:**
   - Inspect full resource table on `stdout`: `pleb fetch --resources manifest <url>`
   - Pull specific resource IDs from the page: `pleb fetch --stage-resources r1,r3 <url> --json`
   - Inline video/audio captions (`.vtt`/`.srt` or YouTube captions): `pleb fetch --resources transcripts <url>`
   - Stage high-salience diagrams (or all high-salience assets): `pleb fetch --resources diagrams <url>` / `--resources all`
   - Stage a direct binary/image/PDF URL with parent `Referer` and Chrome subresource headers (`Sec-Fetch-Dest`, `strict-origin-when-cross-origin`):
     ```bash
     pleb fetch --stage --referer <parent_url> <asset_url>
     ```
3. **Inspect staged binaries locally:**
   Staged files live under `<state_dir>/artifacts/<hash>-<name>.<ext>` (`0o600`, `256 MiB` LRU cap, magic-byte verified against disguised HTML soft-404s). Pass `staged_path` directly to a local file/image/PDF reader tool.
4. **Inspect WebMCP page tools (`--tools`, read-only):**
   - Declarative HTML `<form toolname="..." tooldescription="...">` and imperative CDP `WebMCP.enable` tools are indexed into `tools` (`--json` / `--jsonl` / `--metadata-only`, all marked `untrusted_content: true`) with a `Tools: ...` `stderr` cue by default. Pass `--tools manifest` to append `## Page Tools (WebMCP)` and JSON Schemas.

---

## Decision Tree 3 — Search, Credentials, Batch Fetch, and Crawl Pipelines

- **Multi-provider search (`auto` balances configured providers with cooldown/burst pacing and intent routing; DuckDuckGo HTML fallback):**
  ```bash
  pleb search -n 8 [--provider auto|brave|tavily|serper|exa|searxng|duckduckgo|federated] [--mode auto|exact|semantic] [--freshness day|week|month|year] [--site <domain>] [--extra-snippets] "<query>"
  ```
- **Tool-owned credential storage (`env -> <state_dir>/credentials.enc (AES-256-GCM wrapped by one macOS Keychain vault-key item) -> <state_dir>/credentials.json`):**
  ```bash
  pleb auth set <brave|tavily|serper|exa|searxng|web-bot-auth-jwk|web-bot-auth-agent|cdp-access-client-id|cdp-access-client-secret|cdp-access-host> [--stdin] [--storage auto|keychain|file]
  pleb auth remove <provider> [--storage auto|keychain|file]
  pleb auth status [--json]
  ```
  Value formats: `web-bot-auth-jwk` requires an Ed25519 private OKP JWK JSON object (`kty`, `crv`, `d`, `x`; multi-line JSON allowed via `--stdin`); `web-bot-auth-agent` requires an `https://` URL (no whitespace/query/fragment); `cdp-access-host` requires a bare `host[:port]` (no scheme/path); `searxng` requires an `http://` or `https://` base URL. Environment overrides: `BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL`, `PLEB_WEB_BOT_AUTH_JWK`, `PLEB_WEB_BOT_AUTH_AGENT`, `PLEB_CDP_ACCESS_CLIENT_ID`, `PLEB_CDP_ACCESS_CLIENT_SECRET`, `PLEB_CDP_ACCESS_HOST`. Set `PLEB_NO_KEYCHAIN=1` (or `true`) to bypass macOS Keychain in hermetic/headless runs. Secrets are never accepted as positional CLI args, printed in status/error output, or leaked into daemon/CEF child processes.
- **Search-to-Batch Fetch pipeline:**
  ```bash
  pleb search --urls-only -n 5 "<query>" | pleb fetch - --jsonl --concurrency 4
  ```
  Batch mode (`2+` URLs or `-` stdin) enforces a per-host `Semaphore(2)` cap; `--json` emits an ordered array, `--jsonl` streams per completion, and `--metadata-only` omits `content` while keeping `resources`, `tools`, and optional `warning`.
- **Flags reference:**
  - **Global flags** (accepted before or after subcommands): `--proxy <URL>`, `--timeout <SECS>`, `--cef-timeout <SECS>`, `--cdp-url <URL>`, `--cdp-timeout <SECS>`, `--prefer-cdp`, `--no-web-bot-auth`, `--max-response-bytes <BYTES>`, `--state-dir <DIR>`, `--no-daemon`, `--daemon-idle-timeout <SECS>`, `-q`/`--quiet`, `-v`/`--verbose`, `--json`.
  - **`pleb fetch` flags:** `--format <markdown|html|text|axtree|raw>`, `--extract <clean|article|full>`, `--render <auto|cef|cdp|cronet>`, `--source <auto|negotiate|none>`, `--resources <none|manifest|transcripts|diagrams|all>`, `--tools <none|manifest>`, `--stage-resources <ids>`, `--referer <url>`, `--stage`, `--wait-for-selector <css>`, `--respect-robots`, `--no-rewrite`, `--no-images`, `--no-links`, `--max-bytes <N>`, `--concurrency <1..=16>`, `--jsonl`, `--metadata-only`, `-o <path>` (written only on success).
- **Site crawl and map (`robots.txt` enforced by default; pass `--no-robots` to override):**
  ```bash
  pleb crawl <url> --depth 2 --max-pages 10 [--extract clean|article|full] [--path-filter <prefix>] [--no-robots]
  pleb map <url> --limit 100 [--path-filter <prefix>] [--no-sitemap] [--no-robots]
  ```
  `pleb crawl` caps `--depth` at `5`, `--max-pages` at `50`, per-page extracted content at `10,000` bytes, and plain output at `100,000` bytes (fails non-zero if the seed URL fails; skips broken child links). `pleb map` caps `--limit` at `5000` (sitemaps + up to `20` HTML page crawls).

---

## Exit Codes and JSON Contracts

| Code | Meaning |
| :--- | :--- |
| `0` | Success (check `warning` if JS shell was unrendered) |
| `1` | Invalid flags/input, unavailable/mismatched CEF or CDP renderer, response-size overflow, transport failure, or heterogeneous all-failed batch |
| `2` | Origin block (`403`/`503`), anti-bot challenge, or `robots.txt` disallow |
| `3` | Origin HTTP `4xx`/`5xx` error |
| `4` | Partial batch failure (mixed success and failure) |
| `5` | Unsupported binary content type in non-raw mode (re-run with `--stage` or `--format raw --output <path>`) |
| `130` | Interactive TTY credential prompt interrupted (`SIGINT`/`SIGTERM`) |

Key `--json` / `--jsonl` outputs:
- **`FetchOutput`**: `{ url, final_url, status_code, content_type, title, content, content_bytes, strategy, escalated, truncated, timing_ms, warning?, resources?, tools? }` (`--metadata-only` omits `content`).
- **`BatchItemResult`**: untagged on `ok` (`true` -> `FetchOutput` fields with `content?`; `false` -> `{ url, ok, error, kind, exit_code }`).
- **`StagedAssetOutput`**: `{ url, final_url, status_code, content_type, byte_count, staged_path, timing_ms }`.
- **`SearchResponse`**: `{ provider, providers_used?, results: [{ title, url, snippet, extra_snippets? }] }`.
- **Error envelope on `stderr` (`--json` / `--jsonl`)**: `{"error":"...","kind":"...","exit_code":N}` (`kind`: `invalid_input`, `invalid_url`, `robots_disallowed`, `blocked`, `challenge`, `http_error`, `unsupported_content_type`, `cef_unavailable`, `cdp_unavailable`, `timeout`, `response_too_large`, `transport`, `search_error`, `auth_error`, `crawl_error`, `map_error`, `daemon_error`, `session_error`, `status_error`, `skill_error`, `io_error`).

---

## Daemon, State, CEF `v5-meta`, and Skill Maintenance

- **State directory (`PLEB_HOME` / `--state-dir`, default `~/.pleb`, `0o700`):** holds `data/`, `cef-cache/`, `artifacts/`, `daemon.sock`, `daemon.log`, `site-cache.json`, `search-state.json`, `credentials.enc` (`0o600`), `credentials.json` (`0o600`), and optional local `site-rules.json`.
- **Daemon controls:** auto-starts `daemon.sock` (idle exit `--daemon-idle-timeout` / `PLEB_DAEMON_IDLE_SECONDS`, default `300`; bypass via `--no-daemon` / `PLEB_NO_DAEMON`). Manage with `pleb daemon status`, `pleb daemon stop`, and `pleb session clear` (stops daemon and clears `data/`, `cef-cache/`, `artifacts/`, `site-cache.json`, and `search-state.json` while preserving `credentials.enc`, `credentials.json`, Keychain `vault-key`, `site-rules.json`, and `cef/`).
- **CEF `v5-meta` installer & overrides:** rebuild from the repository root via `bash scripts/install-cef-mac.sh --force` (macOS arm64) or `bash scripts/install-cef-linux.sh --force` (Linux `x86_64` / `aarch64`, runs natively under Ozone headless without Xvfb when `DISPLAY` is unset or empty; both support `--prefix <dir>`). Override binaries in tests/scripts with `PLEB_CEF_RENDERER`, `PLEB_DAEMON_EXECUTABLE`, and `PLEB_NO_HOME_CEF=1`.
- **Version-locked skill maintenance from any machine with `pleb` installed:** run `pleb skill print` to emit this skill, or `pleb skill install [--dir <path>] [--force]` to write it into local agent skill directories without needing a source checkout.
