# mesh-wick

Local, zero-telemetry web fetch and search CLI for AI agents, forked from [wickproject/wick](https://github.com/wickproject/wick).

Uses Chrome's network stack (Cronet) for browser-identical TLS/HTTP2/QUIC fingerprinting, an optional embedded Chromium (CEF) renderer for JS-heavy pages, DOM-pruned compact markdown extraction, and Brave Search API / DuckDuckGo search.

## Build

Default (`reqwest` TLS fallback):

```bash
cargo build --release
```

With Cronet on macOS Apple Silicon (recommended):

```bash
./scripts/download-cronet-macos.sh
cargo build --release --features cronet
```

With Cronet on Linux:

```bash
./scripts/download-cronet-linux.sh
cargo build --release --features cronet
```

## CEF Renderer (Optional)

When installed at `~/.mesh-wick/cef/wick-renderer.app` (or `$MESH_WICK_HOME/cef`), `mesh-wick` automatically escalates blocked responses (`403`/`503`) and JS-only shells to CEF in `--render auto` mode.

To build and install the CEF renderer on macOS from this checkout:

```bash
bash scripts/install-cef-mac.sh
```

On Linux:

```bash
bash scripts/install-cef-linux.sh
```

## Usage

### Fetch a page

```bash
mesh-wick fetch https://example.com
mesh-wick fetch --extract article --no-images https://example.com/post
mesh-wick fetch --render cef --wait-for-selector "article" https://x.com/user
mesh-wick fetch --format text https://news.ycombinator.com
mesh-wick fetch --format raw https://example.com/doc.pdf > doc.pdf
mesh-wick fetch --json --meta-only https://example.com
```

Key `fetch` flags:

- `--format <markdown|html|text|raw>` (default: `markdown`)
- `--extract <clean|article|full>` (default: `clean`)
- `--render <auto|cef|cronet>` (default: `auto`)
- `--wait-for-selector <css>`
- `--respect-robots` (disabled by default for interactive single-URL fetches)
- `--no-rewrite` (disable host rewrites such as `reddit.com` -> `old.reddit.com`)
- `--no-images`, `--no-links`, `--max-bytes <N>`
- `--json`, `--meta-only`

### Search

Uses the Brave Search API when `BRAVE_API_KEY` is set (`--provider auto`), falling back to DuckDuckGo HTML search:

```bash
mesh-wick search "rust async runtime" -n 8
mesh-wick search --provider ddg --json "cronet tls fingerprint"
```

### Crawl and Map

Both `crawl` and `map` respect `robots.txt` by default (pass `--no-robots` to override):

```bash
mesh-wick crawl https://docs.example.com --depth 2 --max-pages 10
mesh-wick map https://example.com --limit 100
```

### Session and Status

```bash
mesh-wick status
mesh-wick session clear
```

## Configuration

- `MESH_WICK_HOME` / `--state-dir`: state directory for cookies, HTTP cache, CEF `cef-cache`, `site-cache.json`, and optional local `site-rules.json` overlay (default: `~/.mesh-wick`; `mesh-wick session clear` clears cookies, HTTP cache, `cef-cache`, and `site-cache.json`)
- `MESH_WICK_PROXY` / `WICK_PROXY` / `--proxy`: SOCKS5/HTTP proxy URL
- `BRAVE_API_KEY`: enables Brave Search API in `search`
- `--timeout <SECS>`: network request timeout (default: `30`)
- `--cef-timeout <SECS>`: CEF render timeout (default: `60`)

## Exit Codes

- `0`: success
- `1`: invalid input, unavailable forced CEF renderer, or network/transport failure
- `2`: blocked by origin (`403`/`503`), anti-bot challenge, or `robots.txt`
- `3`: origin HTTP `4xx`/`5xx` error

## License

MIT — see [LICENSE](LICENSE).
