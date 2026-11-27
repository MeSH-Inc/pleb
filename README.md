# pleb

`pleb`, developed by Memory Sync Hub LLC, is a local, zero-telemetry web fetcher (CLI + stdio MCP server) for Claude Code, Codex, and AI agents that get `403 Forbidden` or empty JavaScript shells from built-in cloud fetchers. It is a zero-telemetry fork of [wickproject/wick](https://github.com/wickproject/wick).

Built-in harness fetchers run from datacenter IP ranges with bot User-Agents and non-browser TLS stacks, which causes sites to block them even when the page is public. `pleb` runs from your local machine using Chrome's network stack (Cronet TLS/HTTP2/QUIC), escalates `403`/`503` blocks and unhydrated JS shells to a warm offscreen Chromium (`CEF v5-meta`) or remote CDP/Kitesurf renderer (`--render cdp`), exposes `pleb mcp` directly to Claude Code and Codex via `pleb setup`, and extracts clean Markdown or plain text in a single DOM pass.

## Install and Harness Setup

Install `pleb` onto `PATH`:

```bash
# From a source checkout on macOS Apple Silicon with Cronet (recommended for Chrome TLS/HTTP2/QUIC)
./scripts/download-cronet-macos.sh
cargo install --path . --features cronet --locked

# From a source checkout on Linux with Cronet (binary links libcronet.so with $ORIGIN rpath)
./scripts/download-cronet-linux.sh
cargo install --path . --features cronet --locked

# From crates.io (pure-Rust reqwest/rustls transport with session cookie jar; no Chrome TLS)
cargo install pleb

# Register `pleb mcp` + bundled skill into Claude Code and Codex in one command
pleb setup
```

Optional components:

```bash
# Install the local offscreen CEF v5-meta renderer for automatic 403/SPA escalation (use install-cef-linux.sh on Linux)
bash scripts/install-cef-mac.sh --force

# Install only the bundled agent skill (or symlink from a source checkout via bash scripts/install-skill.sh)
pleb skill install
```

See [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) for prerequisites, custom install prefixes, and verification.

## Capabilities

- **Harness MCP server & Chrome-grade `403`/SPA unblocking**: Registers `pleb mcp` (`pleb_fetch`, `pleb_search`, `pleb_crawl`, `pleb_map`, `pleb_status`) with Claude Code and Codex via `pleb setup`, runs over Cronet (or cookie-jar `reqwest` fallback) with redirect-time SSRF protection through `daemon.sock`, classifies WAF/CDN response headers into typed `block_reason` diagnostics, recovers from `406` negotiation mismatches and short `429` `Retry-After` limits, supports optional RFC 9421 Web Bot Auth signing (`--no-web-bot-auth`), and escalates blocks/JS shells to local CEF (`v5-meta`) or remote CDP/Kitesurf (`--render cdp`, `--cdp-url`, `--format axtree`). Active bot challenges fail fast without CAPTCHA solving or evasion scripts. See [Transport and Rendering](README-FULL.md#transport-and-rendering).
- **Deterministic HTML cleanup, WebMCP discovery, and source bypass**: Removes navigation, hidden nodes, cookie banners, tracking parameters, and code-gutter tables while rescuing formulas, code blocks, SVG diagram labels, and responsive images. Indexes read-only declarative and imperative WebMCP tools (`--tools manifest`, `FetchOutput.tools`), and automatically uses raw files for GitHub/GitLab/Gist URLs, advertised Markdown mirrors, RSS/Atom/JSON feeds, YouTube captions, and WebVTT/SRT transcripts. See [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation).
- **On-demand asset staging**: Indexes diagrams, transcripts, PDFs, datasets, and schemas during the initial parse without making extra requests, and downloads selected assets into a local cache only when requested. See [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging).
- **Multi-provider search and local credentials**: Routes search queries across Brave, Tavily, Serper, Exa, SearXNG, or DuckDuckGo with cooldown tracking, burst pacing, Reciprocal Rank Fusion, and local credential storage (`pleb auth`) for search, Web Bot Auth, and CDP Access credentials. See [Search and Credentials](README-FULL.md#search-and-credentials).
- **Site crawling and mapping**: Crawls links with bounded concurrency and `Crawl-delay` pacing, and discovers site URLs from sitemaps and `robots.txt`. See [Crawl and Map](README-FULL.md#crawl-and-map).

## Quick Start

```bash
# Preview or run Claude Code and Codex MCP + skill registration
pleb setup --dry-run
pleb setup

# Inspect Cronet TLS, CEF v5-meta, daemon, and search provider readiness
pleb status

# Fetch a page that blocks cloud agent fetchers as clean Markdown
pleb fetch https://example.com

# Scope extraction to the main article, omit images, or focus on a query
pleb fetch --extract article --no-images https://example.com/post
pleb fetch --focus "rate limit retry" https://docs.example.com/api

# Run the stdio MCP server directly (invoked automatically by Claude Code / Codex)
pleb mcp

# Render over a remote CDP / Kitesurf endpoint or inspect the accessibility tree
pleb fetch --render cdp --cdp-url wss://kitesurf.example.com/devtools/browser https://example.com
pleb fetch --format axtree --cdp-url ws://127.0.0.1:8787/devtools/browser https://example.com

# Search and pipe URLs into a concurrent batch fetch
pleb search --urls-only -n 5 "rust async runtime" | pleb fetch - --jsonl --concurrency 4

# Store a search provider key locally (prompts with echo disabled)
pleb auth set brave

# Crawl or map a site (respects robots.txt by default)
pleb crawl https://docs.example.com --depth 2 --max-pages 10
pleb map https://example.com --limit 100
```

See [Command Reference](README-FULL.md#command-reference) for all subcommands, flags, and batch output formats.

## Benchmarks

Measured with `cargo run --release --example corpus_benchmark` across all `45` sanitized fixtures in [tests/corpus_evaluation.rs](tests/corpus_evaluation.rs) (`41` extractable documents across `50` warm iterations, `4` classification gates; token counts estimated via `ceil(bytes / 4)`):

| Category | Fixtures | Raw Bytes | Output Bytes | Byte Reduction | Est. Tokens (`Raw -> Clean`) | Mean Release Latency |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| HTML pages | 28 | 40,221 | 9,652 | 76.0% | 10,056 -> 2,413 | 62.3 us |
| Feeds (RSS/Atom/JSON) | 4 | 3,031 | 1,424 | 53.0% | 758 -> 356 | 22.3 us |
| Transcripts (WebVTT/SRT) | 2 | 520 | 205 | 60.6% | 130 -> 52 | 1.5 us |
| Native Markdown | 5 | 2,108 | 2,142 | -1.6% | 527 -> 536 | 3.0 us |
| Plain/JSON passthrough | 2 | 152 | 152 | 0.0% | 38 -> 38 | 0.1 us |
| **All extractable** | **41** | **46,032** | **13,575** | **70.5%** | **11,508 -> 3,394** | **45.1 us** (`23.72 MiB/s`) |

All `4/4` non-extractable fixtures (`ratchet_challenge_403.html`, `frontier_soft_404_login_wall.html`, `ratchet_js_shell.html`, `frontier_canary_sveltekit_spa.html`) classified accurately for fail-fast blocking or CEF escalation. For opt-in live unblock validation against sites that frequently `403` cloud agent fetchers, run `cargo test --features cronet --test unblock_canary_tests -- --ignored --nocapture`. See [Benchmarks](README-FULL.md#benchmarks) for methodology and native Markdown link-resolution details.

## Comparison

| Tool | Execution & State | HTTP / Browser Stack | Challenge Policy | Built-in Search & Auth |
| :--- | :--- | :--- | :--- | :--- |
| **`pleb`** | Single MIT Rust binary (`~/.pleb`) + `pleb mcp`; zero telemetry | Cronet (`Chromium 154`) or `reqwest` (`cookie_store`) + optional CEF `v5-meta` / remote CDP (`--render cdp`) | Fail fast (`exit 2`, typed `block_reason`), zero CAPTCHA bypass | `pleb search` (6 providers + RRF), `pleb auth`, `pleb setup`, & RFC 9421 Web Bot Auth |
| **Firecrawl** | Cloud API, `firecrawl-local` binary, or AGPL-3.0 Docker stack (Redis, Postgres, browser) | Cloud browsers or Playwright/Fire-engine | Managed cloud unblocking | Cloud/v2 `/search` endpoint |
| **Jina Reader** | Hosted `r.jina.ai` (`20 RPM` no-key / paid tokens) or Apache-2.0 container | Hosted Chromium fleet | Rate-limit tiers (not bypass) | Separate `s.jina.ai` endpoint |
| **Crawl4AI** | Python + Playwright (Apache-2.0) or Crawl4AI Cloud | Async Playwright browser pool | Community hooks/proxies | Crawl4AI Cloud only |
| **`curl` + `html2text`** | Local CLI tools; no state | `libcurl` HTTP, no JS rendering | Raw `403` HTML output | None |

See [Comparison with Alternatives](README-FULL.md#comparison-with-alternatives) for feature trade-offs and when to pick another tool.

## Full Documentation

Machine-readable agent discovery files live at [llms.txt](llms.txt), [llms-full.txt](llms-full.txt), and [skills/pleb/SKILL.md](skills/pleb/SKILL.md). Complete human and integrator documentation lives in [README-FULL.md](README-FULL.md):

- [Overview and Design Boundaries](README-FULL.md#overview-and-design-boundaries) — scope, differences from upstream `wickproject/wick`, and non-bypass boundary
- [Benchmarks](README-FULL.md#benchmarks) — reproducible corpus byte/token reduction and release latency across `tests/corpus/`
- [Comparison with Alternatives](README-FULL.md#comparison-with-alternatives) — trade-offs vs Firecrawl, Jina Reader (`r.jina.ai`), Crawl4AI, and `curl` + `html2text`
- [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) — Cronet, optional CEF `v5-meta` renderer, `pleb setup`, and agent skill installation
- [Distribution and Packaging](README-FULL.md#distribution-and-packaging) — default `reqwest` build (`crates.io`), optional Cronet builds, and Homebrew tap formula template
- [Architecture](README-FULL.md#architecture) — request pipeline and repository directory layout
- [Transport and Rendering](README-FULL.md#transport-and-rendering) — Cronet, CEF escalation, response classification, header diagnostics, and `daemon.sock`
- [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation) — cleanup stages, worked HTML-to-Markdown example, source bypass, and non-HTML formats
- [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging) — zero-fetch resource cues, `--resources`, `--stage`, and local artifact validation
- [Search and Credentials](README-FULL.md#search-and-credentials) — provider routing, federation, cooldowns, and `pleb auth`
- [Crawl and Map](README-FULL.md#crawl-and-map) — bounded wave crawling and sitemap discovery
- [Command Reference](README-FULL.md#command-reference) — global flags, subcommand options (`mcp`, `setup`), and output formats
- [Configuration and State Layout](README-FULL.md#configuration-and-state-layout) — environment variables, state files, and `site-rules.json`
- [Exit Codes and Error Envelopes](README-FULL.md#exit-codes-and-error-envelopes) — exit codes `0` through `5`, `130`, `block_reason`, and JSON error format
- [JSON Output Schemas](README-FULL.md#json-output-schemas) — exact field contracts for `--json` and `--jsonl` outputs
- [Testing and Design Documents](README-FULL.md#testing-and-design-documents) — test commands, corpus evaluation, opt-in live unblock canary, hermetic isolation, and specs in `docs/`

## License

MIT — Copyright (c) 2026 Memory Sync Hub LLC (forked from [wickproject/wick](https://github.com/wickproject/wick)). See [LICENSE](LICENSE).
