# mesh-wick

Local, zero-telemetry web fetch and search CLI (`wick`) for AI agents, forked from [wickproject/wick](https://github.com/wickproject/wick).

Uses Chrome's network stack (Cronet) for browser-identical TLS/HTTP2/QUIC fingerprinting, an optional embedded Chromium (CEF `v4-meta`) renderer for JS-heavy pages, single-parse DOM-pruned compact markdown extraction, and Brave Search API / DuckDuckGo search.

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

When installed at `~/.mesh-wick/cef/wick-renderer.app` (or `$MESH_WICK_HOME/cef`) with protocol `v4-meta`, `wick` automatically escalates blocked responses (`403`/`503`) and JS-only shells to CEF in `--render auto` mode. The renderer runs offscreen with minimal OSR viewport/screen/language defaults (no synthetic challenge clicking).

To build and install (or upgrade to `v4-meta`) on macOS from this checkout:

```bash
bash scripts/install-cef-mac.sh --force
```

On Linux:

```bash
bash scripts/install-cef-linux.sh --force
```

## Usage

### Fetch a page

```bash
wick fetch https://example.com
wick fetch --extract article --no-images https://example.com/post
wick fetch --render cef --wait-for-selector "article" https://x.com/user
wick fetch --format text https://news.ycombinator.com
wick fetch --format raw --output doc.pdf https://example.com/doc.pdf
wick fetch --json --metadata-only https://example.com
```

Key `fetch` flags:

- `--format <markdown|html|text|raw>` (default: `markdown`; aliases `md`, `txt`)
- `--extract <clean|article|full>` (default: `clean`)
- `--render <auto|cef|cronet>` (default: `auto`)
- `--wait-for-selector <css>`
- `--respect-robots` (disabled by default for interactive single-URL fetches)
- `--no-rewrite` (disable host rewrites such as `reddit.com` -> `old.reddit.com`)
- `--no-images`, `--no-links`, `--max-bytes <N>`
- `--output <path>` (`-o`): write output to a file instead of stdout (created only after fetch succeeds)
- `--json` (global flag), `--metadata-only`

### Search

Uses the Brave Search API when `BRAVE_API_KEY` is set (`--provider auto`), falling back to DuckDuckGo HTML search:

```bash
wick search "rust async runtime" -n 8
wick search --provider duckduckgo --json "cronet tls fingerprint"
```

### Crawl and Map

Both `crawl` and `map` respect `robots.txt` by default (pass `--no-robots` to override):

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

- `MESH_WICK_HOME` / `--state-dir`: private (`0o700`) state directory for cookies, HTTP cache, CEF `cef-cache`, CEF daemon socket (`cef.sock`), `site-cache.json` (`0o600`), and optional local `site-rules.json` overlay over bundled structural rules (`reddit.com` -> `old.reddit.com`, `x.com`/`twitter.com`) (default: `~/.mesh-wick`; `wick session clear` stops any running CEF daemon and clears cookies, HTTP cache, `cef-cache`, and `site-cache.json`)
- `MESH_WICK_PROXY` / `--proxy`: SOCKS5/HTTP proxy URL
- `MESH_WICK_TIMEZONE`: optional IANA timezone override injected into CEF `Intl.DateTimeFormat`
- `BRAVE_API_KEY`: enables Brave Search API in `search`
- `--timeout <SECS>`: network request timeout (default: `30`)
- `--cef-timeout <SECS>`: CEF render timeout (default: `60`)
- `--daemon-idle-timeout <SECS>` / `MESH_WICK_DAEMON_IDLE_SECONDS`: auto-exit timeout for the warm Unix-socket daemon (also honored by `wick daemon run`; default: `300`)
- `--no-daemon` / `MESH_WICK_NO_DAEMON`: bypass the shared background daemon and run in-process

## Exit Codes

- `0`: success
- `1`: invalid input, unavailable/mismatched CEF renderer, or network/transport failure
- `2`: blocked by origin (`403`/`503`), anti-bot challenge, or `robots.txt`
- `3`: origin HTTP `4xx`/`5xx` error
- `5`: unsupported binary content type in non-raw mode (re-run with `--format raw --output <path>`)

## License

MIT — see [LICENSE](LICENSE).
