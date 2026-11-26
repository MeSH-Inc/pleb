---
name: mesh-wick
description: Reference for the local `mesh-wick` CLI (`wick` binary with Cronet TLS, `v5-meta` CEF renderer, unified `daemon.sock` v3, structured source negotiation, feed/transcript extraction, and selective resource staging). Use when fetching pages that resist built-in web fetch, batch-fetching search results, inspecting or staging diagrams/PDFs/transcripts, or debugging `wick` daemon and CEF behavior.
metadata:
  author: fluffyrabbot
  repository: https://github.com/MeSH-Inc/mesh-wick
  scope: user
---

# `mesh-wick` (`wick`)

Local, MIT-licensed, zero-telemetry web acquisition and search CLI built from `~/apps/mesh-wick`.

## Identity and Version Contract

Verify the active `wick` binary is `mesh-wick` (not upstream `wickproject/wick`) before relying on `mesh-wick`-specific flags:

```bash
wick status --json
```

Expected markers: `"cronet_enabled": true`, `"cef_renderer": { "expected_protocol": "v5-meta" }`, and unified daemon (`daemon.sock`, `protocol_version: 3`). All upstream telemetry pings and remote `site-rules.json` sync calls are removed.

---

## Decision Tree 1 — Page Acquisition and Escalation

1. **Default fetch (`--render auto --source auto --extract clean`):**
   ```bash
   wick fetch <url>
   ```
   - Skips `robots.txt` by default on single-URL fetches (pass `--respect-robots` to enforce).
   - **Structured source resolution (`--source auto`):** automatically rewrites GitHub `/blob/`, GitLab `/-/blob/`, and Gist URLs to lossless raw source (`strategy: "raw_source"`, with automatic HTML fallback on `404`/binary), and follows RFC 8288 `Link` / HTML `<link rel="alternate" type="text/markdown">` mirrors (`strategy: "markdown_mirror"`).
   - **First-class non-HTML extraction:** handles native `text/markdown` (YAML frontmatter `title` + relative link resolution + fence-safe truncation), RSS 2.0 / Atom 1.0 / JSON Feed 1.1 digests, and WebVTT (`.vtt`) / SubRip (`.srt`) transcripts.
   - **Automatic CEF escalation:** escalates `403`/`503` blocks and unhydrated JS/SvelteKit/React SPA shells to the warm `v5-meta` offscreen Chromium renderer.
2. **Documentation Markdown negotiation (`--source negotiate`):**
   ```bash
   wick fetch --source negotiate <url>
   ```
   Sends `Accept: text/markdown` on the primary request (`strategy: "markdown_negotiated"`) and probes `<path>.md` if HTML returns without an advertised alternate. Use `--source none` (or `--format html` / `--format raw` / `--render cef`) to disable source resolution.
3. **Forced Chromium rendering (`--render cef`):**
   ```bash
   wick fetch --render cef --wait-for-selector "<css>" <url>
   ```
   Use `--render cronet` to force network-only transport without spawning CEF.
4. **Active bot challenge or login wall (exit code `2`):**
   `mesh-wick` never solves CAPTCHAs or runs anti-bot bypass scripts. Stop retrying via `wick` and switch to an interactive browser session.

---

## Decision Tree 2 — Selective Resource Enrichment

`wick fetch` indexes high-salience page resources (`diagram`, `image`, `transcript`, `document`, `dataset`, `schema`) during its single `O(N)` DOM pass with zero extra network requests, and rescues inline `<svg>` `<text>` labels directly into Markdown (`[Diagram: <label> — ...]`).

1. **Read prose first and check resource cues:**
   - Default non-JSON fetches print a one-line `stderr` cue whenever high-salience resources exist (`Resources: 2 diagrams (r1, r2), 1 transcript (r3) ...`).
   - `--json` / `--jsonl` / `--metadata-only` include the full `resources` array (`identifier`, `kind`, `url`, `section_heading`, `label`, `dimensions`, `salience`).
   - If the prose and rescued `<svg>` labels answer the task, stop—zero extra requests or vision tokens spent.
2. **Pull only the resources that enrich context:**
   - Inspect full resource table on `stdout`: `wick fetch --resources manifest <url>`
   - Pull specific resource IDs from the page: `wick fetch --stage-resources r1,r3 <url> --json`
   - Inline video/audio captions (`.vtt`/`.srt`): `wick fetch --resources transcripts <url>`
   - Stage high-salience diagrams (or all high-salience assets): `wick fetch --resources diagrams <url>` / `--resources all`
   - Stage a direct binary/image/PDF URL with parent `Referer` and Chrome subresource headers (`Sec-Fetch-Dest`, `strict-origin-when-cross-origin`):
     ```bash
     wick fetch --stage --referer <parent_url> <asset_url>
     ```
3. **Inspect staged binaries locally:**
   Staged files live under `<state_dir>/artifacts/<hash>-<name>.<ext>` (`0o600`, `256 MiB` LRU cap, magic-byte verified against disguised HTML soft-404s). Pass `staged_path` directly to a local file/image/PDF reader tool.

---

## Decision Tree 3 — Search, Credentials, Batch Fetch, and Crawl Pipelines

- **Multi-provider search (`auto` balances configured providers with cooldown/burst pacing and intent routing; DuckDuckGo HTML fallback):**
  ```bash
  wick search -n 8 [--provider auto|brave|tavily|serper|exa|searxng|duckduckgo|federated] [--mode auto|exact|semantic] [--freshness day|week|month|year] [--site <domain>] [--extra-snippets] "<query>"
  ```
- **Tool-owned credential storage (`env -> macOS Keychain -> <state_dir>/credentials.json`):**
  ```bash
  wick auth set <brave|tavily|serper|exa|searxng> [--stdin] [--storage auto|keychain|file]
  wick auth remove <brave|tavily|serper|exa|searxng> [--storage auto|keychain|file]
  wick auth status [--json]
  ```
  Environment overrides: `BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL`. Set `WICK_NO_KEYCHAIN=1` to bypass macOS Keychain in hermetic/headless runs. Secrets are never accepted as positional CLI args or printed in status/error output.
- **Search-to-Batch Fetch pipeline:**
  ```bash
  wick search --urls-only -n 5 "<query>" | wick fetch - --jsonl --concurrency 4
  ```
  Batch mode (`2+` URLs or `-` stdin) enforces a per-host `Semaphore(2)` cap; `--json` emits an ordered array, `--jsonl` streams per completion, and `--metadata-only` omits `content` while keeping `resources`.
- **Extraction & output flags:** `--format <markdown|html|text|raw>`, `--extract <clean|article|full>`, `--no-rewrite`, `--no-images`, `--no-links`, `--max-bytes <N>`, `--max-response-bytes <BYTES>`, `-o <path>` (written only on success).
- **Site crawl and map (`robots.txt` enforced by default; pass `--no-robots` to override):**
  ```bash
  wick crawl <url> --depth 2 --max-pages 10
  wick map <url> --limit 100
  ```

---

## Exit Codes

| Code | Meaning |
| :--- | :--- |
| `0` | Success |
| `1` | Invalid input, unavailable/mismatched CEF renderer, response-size overflow, transport failure, or heterogeneous all-failed batch |
| `2` | Origin block (`403`/`503`), anti-bot challenge, or `robots.txt` disallow |
| `3` | Origin HTTP `4xx`/`5xx` error |
| `4` | Partial batch failure (mixed success and failure) |
| `5` | Unsupported binary content type in non-raw mode (re-run with `--stage` or `--format raw --output <path>`) |

---

## Daemon, State, CEF `v5-meta`, and Skill Maintenance

- **State directory (`MESH_WICK_HOME` / `--state-dir`, default `~/.mesh-wick`, `0o700`):** holds `data/`, `cef-cache/`, `artifacts/`, `daemon.sock`, `daemon.log`, `site-cache.json`, `search-state.json`, `credentials.json` (`0o600`), and optional local `site-rules.json`.
- **Daemon controls:** auto-starts `daemon.sock` (idle exit `--daemon-idle-timeout` / `MESH_WICK_DAEMON_IDLE_SECONDS`, default `300`; bypass via `--no-daemon` / `MESH_WICK_NO_DAEMON`). Manage with `wick daemon status`, `wick daemon stop`, and `wick session clear` (stops daemon and clears `data/`, `cef-cache/`, `artifacts/`, `site-cache.json`, and `search-state.json` while preserving `credentials.json` and Keychain items).
- **CEF `v5-meta` installer & overrides:** rebuild via `bash ~/apps/mesh-wick/scripts/install-cef-mac.sh --force` (supports `--prefix <dir>`). Override binaries in tests/scripts with `WICK_CEF_RENDERER`, `WICK_DAEMON_EXECUTABLE`, and `WICK_NO_HOME_CEF=1`.
- **Emit this exact version-locked skill from any machine with `wick` installed:** `wick skill print`.
