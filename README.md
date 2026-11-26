# mesh-wick

Local, zero-telemetry web fetch and search CLI (`wick`) for AI agents, forked from [wickproject/wick](https://github.com/wickproject/wick).

Uses Chrome's network stack (Cronet) for browser-identical TLS/HTTP2/QUIC fingerprinting, an optional embedded Chromium (CEF `v5-meta`) renderer for JS-heavy pages, single-parse `O(N)` DOM-pruned compact markdown extraction with charset-aware decoding, and Brave Search API / DuckDuckGo search.

## Build & Install

Default (`reqwest` TLS fallback):

```bash
cargo build --release
```

With Cronet on macOS Apple Silicon (recommended):

```bash
./scripts/download-cronet-macos.sh
cargo install --path . --features cronet --locked
```

With Cronet on Linux:

```bash
./scripts/download-cronet-linux.sh
cargo install --path . --features cronet --locked
```

This builds the `mesh-wick` crate and installs the canonical `wick` binary onto `PATH`.

## CEF Renderer (Optional)

When installed at `~/.mesh-wick/cef/wick-renderer.app` (or `$MESH_WICK_HOME/cef` / `$WICK_CEF_RENDERER`) with protocol `v5-meta`, `wick` automatically escalates blocked responses (`403`/`503`) and JS-only shells to CEF in `--render auto` mode. The renderer runs offscreen with subresource + `MutationObserver` quiet-window readiness and minimal OSR viewport/screen/language defaults (no synthetic challenge clicking).

To build and install (or upgrade to `v5-meta`) on macOS from this checkout (pass `--prefix <dir>` to install into an isolated directory):

```bash
bash scripts/install-cef-mac.sh --force
```

On Linux:

```bash
bash scripts/install-cef-linux.sh --force
```

## Usage

### Fetch pages (single or batch)

```bash
wick fetch https://example.com
wick fetch --extract article --no-images https://example.com/post
wick fetch --render cef --wait-for-selector "article" https://x.com/user
wick fetch --format text https://news.ycombinator.com
wick fetch --format raw --output doc.pdf https://example.com/doc.pdf
wick fetch --json --metadata-only https://example.com
wick fetch --jsonl --concurrency 4 https://example.com https://example.org
wick search --urls-only "rust async runtime" | wick fetch -
```

Key `fetch` flags:

- `--format <markdown|html|text|raw>` (default: `markdown`; aliases `md`, `txt`)
- `--extract <clean|article|full>` (default: `clean`)
- `--render <auto|cef|cronet>` (default: `auto`)
- `--wait-for-selector <css>`
- `--respect-robots` (disabled by default for interactive single-URL fetches)
- `--no-rewrite` (disable host rewrites such as `reddit.com` -> `old.reddit.com`)
- `--no-images`, `--no-links`, `--max-bytes <N>`
- `--concurrency <N>` (batch mode max concurrent fetches, `1..=16`, default `4`, capped at 2 per host)
- `--output <path>` (`-o`): write output to a file instead of stdout (single-URL only; created only after fetch succeeds)
- `--json` (global flag), `--jsonl` (streaming per-URL JSON lines), `--metadata-only`

### Search

Uses the Brave Search API when `BRAVE_API_KEY` is set (`--provider auto`), falling back to DuckDuckGo HTML search:

```bash
wick search "rust async runtime" -n 8
wick search --provider duckduckgo --json "cronet tls fingerprint"
wick search --site docs.rs --freshness month --urls-only "tokio"
wick search --extra-snippets "cronet http2"
```

Key `search` flags:

- `-n` / `--limit <N>` (`1..=20`, default `8`)
- `--provider <auto|brave|duckduckgo>` (alias `ddg`)
- `--freshness <day|week|month|year>`
- `--site <domain>` (prepends `site:<domain>` after stripping scheme/trailing slash)
- `--extra-snippets` (Brave Search extra excerpts)
- `--urls-only` (one URL per line, empty stdout on zero results; conflicts with `--json`)

### Crawl and Map

Both `crawl` (bounded concurrent wave frontier with polite per-origin spacing / `Crawl-delay` and deterministic ordering) and `map` (recursive `<sitemapindex>` + `.xml.gz` + `robots.txt` `Sitemap:` discovery) respect `robots.txt` by default (pass `--no-robots` to override):

```bash
wick crawl https://docs.example.com --depth 2 --max-pages 10
wick map https://example.com --limit 100
```

### Session, Daemon, and Status

```bash
wick status
wick status --json
wick daemon status
wick daemon stop
wick session clear
```

## Configuration

- `MESH_WICK_HOME` / `--state-dir`: private (`0o700`) state directory for proxy-partitioned HTTP cache/cookies (`data/`), CEF `cef-cache/`, unified transport daemon socket (`daemon.sock`), daemon logs (`daemon.log`), `site-cache.json` (`0o600`), and optional local `site-rules.json` overlay over bundled structural rules (`reddit.com` -> `old.reddit.com`, `x.com`/`twitter.com`) (default: `~/.mesh-wick`; `wick session clear` stops any running daemon and clears `data/`, `cef-cache/`, and `site-cache.json`)
- `MESH_WICK_PROXY` / `--proxy`: SOCKS5/HTTP proxy URL
- `MESH_WICK_TIMEZONE`: optional IANA timezone override injected into CEF `Intl.DateTimeFormat`
- `BRAVE_API_KEY`: enables Brave Search API in `search`
- `--timeout <SECS>`: network request timeout (default: `30`)
- `--max-response-bytes <BYTES>`: maximum HTTP response body size (defaults: `16 MiB` document, `128 MiB` raw)
- `--cef-timeout <SECS>`: CEF render timeout (default: `60`)
- `--daemon-idle-timeout <SECS>` / `MESH_WICK_DAEMON_IDLE_SECONDS`: auto-exit timeout for the warm Unix-socket daemon (also honored by `wick daemon run`; default: `300`)
- `--no-daemon` / `MESH_WICK_NO_DAEMON`: bypass the shared background daemon and run in-process
- `WICK_CEF_RENDERER`: explicit path to a `v5-meta` `wick-renderer` binary (overrides `state_dir/cef` discovery; set `WICK_NO_HOME_CEF=1` in tests to disable `~/.mesh-wick/cef` fallback)
- `WICK_DAEMON_EXECUTABLE`: explicit path to the `wick` executable spawned for background daemon mode

## Exit Codes

- `0`: success
- `1`: invalid input, unavailable/mismatched CEF renderer, response-size overflow, network/transport failure, or heterogeneous all-failed batch
- `2`: blocked by origin (`403`/`503`), anti-bot challenge, or `robots.txt`
- `3`: origin HTTP `4xx`/`5xx` error
- `4`: partial batch failure (some URLs succeeded, some failed)
- `5`: unsupported binary content type in non-raw mode (re-run with `--format raw --output <path>`)

## License

MIT — see [LICENSE](LICENSE).
