# pleb

`pleb`, developed by Memory Sync Hub LLC, is a local, zero-telemetry web fetcher (CLI + stdio MCP server + Claude Code hooks) that unblocks `403 Forbidden` responses and empty JavaScript shells encountered by built-in cloud agent fetchers. Forked from [wickproject/wick](https://github.com/wickproject/wick), it fetches from your host IP over Chrome's network stack (Cronet TLS/HTTP2/QUIC), escalates blocks and unhydrated SPAs to offscreen Chromium (`CEF v5-meta`) or WebSocket CDP/Kitesurf, signs public requests via RFC 9421 Web Bot Auth, and extracts compact Markdown.

## Install and Harness Setup

```bash
# macOS arm64 or Linux with Chromium 154 Cronet (recommended for Chrome TLS/HTTP2/QUIC)
./scripts/download-cronet-macos.sh   # or ./scripts/download-cronet-linux.sh
cargo install --path . --features cronet --locked

# Or install from crates.io (pure-Rust reqwest/rustls with session cookies; no Chrome TLS)
cargo install pleb

# Optional: local offscreen CEF v5-meta renderer for automatic 403/SPA escalation
bash scripts/install-cef-mac.sh --force   # or bash scripts/install-cef-linux.sh --force

# Register `pleb mcp`, bundled SKILL.md, and Claude Code WebFetch hook (PreToolUse redirect by default)
pleb setup
```

See [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) for prerequisites, `--hooks` options (`redirect`, `fallback`, `none`), and verification.

## Quick Start

```bash
# Fall back only when built-in WebFetch errors (--hooks fallback), or inspect readiness
pleb setup --hooks fallback
pleb status

# Fetch as clean Markdown (auto-escalates 403/JS shells to CEF/CDP when available)
pleb fetch https://example.com
pleb fetch --expand tabs --focus "rate limit retry" https://docs.example.com/api

# Extract offline HTML from Playwright or stdin, or passively snapshot an open loopback CDP tab
node dump-page.mjs | pleb extract --url https://docs.example.com/api --expand tabs --focus "rate limits"
pleb fetch --render cdp --cdp-url ws://127.0.0.1:9222/devtools/page/<id> --no-navigate

# Search across providers (or DuckDuckGo) and pipe URLs into concurrent batch fetch
pleb search --urls-only -n 5 "rust async runtime" | pleb fetch - --jsonl --concurrency 4

# Generate an Ed25519 Web Bot Auth keypair and print public directory JSON
pleb auth init-web-bot-auth --agent https://bot.example.com

# Crawl or map a site (respects robots.txt by default)
pleb crawl https://docs.example.com --depth 2 --max-pages 10
```

See [Command Reference](README-FULL.md#command-reference) for global flags, `pleb mcp`, `pleb hook claude-webfetch`, `pleb auth`, and batch formats.

## Benchmarks and Boundaries

Across all `43` extractable fixtures in [examples/corpus_benchmark.rs](examples/corpus_benchmark.rs) (`cargo run --release --example corpus_benchmark`, `49` benchmarked / `52` total contracts in [tests/corpus_evaluation.rs](tests/corpus_evaluation.rs)), `pleb` reduces payload size by `70.2%` (`48,578` -> `14,480` bytes) at `53.6 us` mean release latency (`20.11 MiB/s`), and classifies `6/6` challenge/login/JS-shell gates. Active CAPTCHA/bot challenges fail fast (`exit 2`, typed `block_reason`) with zero evasion scripts; see [Benchmarks](README-FULL.md#benchmarks) and [Comparison with Alternatives](README-FULL.md#comparison-with-alternatives) for methodology and trade-offs.

## Documentation

- **Agent reference**: [skills/pleb/SKILL.md](skills/pleb/SKILL.md) (embedded in `pleb skill print`), [llms.txt](llms.txt), and [llms-full.txt](llms-full.txt)
- **Full human & integrator manual**: [README-FULL.md](README-FULL.md)
  - [Transport and Rendering](README-FULL.md#transport-and-rendering) — Cronet, CEF `v5-meta`, remote CDP/Kitesurf, Web Bot Auth, and `daemon.sock`
  - [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation) — 5-stage cleanup, Markdown mirrors, RSS/Atom, transcripts, and YouTube
  - [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging) — zero-fetch `resources` cues and selective artifact staging
  - [Search and Credentials](README-FULL.md#search-and-credentials) — multi-provider RRF search and `pleb auth` vault
  - [Exit Codes and Error Envelopes](README-FULL.md#exit-codes-and-error-envelopes) and [JSON Output Schemas](README-FULL.md#json-output-schemas)

## License

MIT — Copyright (c) 2026 Memory Sync Hub LLC (forked from [wickproject/wick](https://github.com/wickproject/wick)). See [LICENSE](LICENSE).
