# pleb

`pleb`, developed by Memory Sync Hub LLC, is a local, zero-telemetry web acquisition and search CLI for AI agents and scripts that need clean, compact page text instead of raw HTML. It is a zero-telemetry fork of [wickproject/wick](https://github.com/wickproject/wick).

By default, `pleb` makes HTTP requests using Chrome's network stack (Cronet) or a `reqwest` fallback, strips page chrome into compact Markdown or plain text in a single DOM pass, and optionally escalates JavaScript-heavy or blocked pages to a local offscreen Chromium (CEF) renderer.

## Capabilities

- **Browser-grade transport and rendering**: Sends requests over Cronet (or `reqwest`) with redirect-time SSRF protection through a shared local Unix-socket daemon, and escalates unhydrated JavaScript pages or `403`/`503` responses to an optional local CEF renderer (native Ozone headless on Linux without Xvfb). Active bot challenges fail fast without CAPTCHA solving or bypass scripts. See [Transport and Rendering](README-FULL.md#transport-and-rendering).
- **Deterministic HTML cleanup and source bypass**: Removes navigation, hidden nodes, cookie banners, tracking parameters, and code-gutter tables while rescuing formulas, code blocks, SVG diagram labels, and responsive images. Automatically uses raw files for GitHub/GitLab/Gist URLs, advertised Markdown mirrors, RSS/Atom/JSON feeds, YouTube captions, and WebVTT/SRT transcripts. See [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation).
- **On-demand asset staging**: Indexes diagrams, transcripts, PDFs, datasets, and schemas during the initial parse without making extra requests, and downloads selected assets into a local cache only when requested. See [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging).
- **Multi-provider search and local credentials**: Routes search queries across Brave, Tavily, Serper, Exa, SearXNG, or DuckDuckGo with cooldown tracking, burst pacing, Reciprocal Rank Fusion, and local credential storage (`pleb auth`). See [Search and Credentials](README-FULL.md#search-and-credentials).
- **Site crawling and mapping**: Crawls links with bounded concurrency and `Crawl-delay` pacing, and discovers site URLs from sitemaps and `robots.txt`. See [Crawl and Map](README-FULL.md#crawl-and-map).

## Benchmarks

Measured with `cargo run --release --example corpus_benchmark` across all `43` sanitized fixtures in [tests/corpus_evaluation.rs](tests/corpus_evaluation.rs) (`39` extractable documents across `50` warm iterations, `4` classification gates; token counts estimated via `ceil(bytes / 4)`):

| Category | Fixtures | Raw Bytes | Output Bytes | Byte Reduction | Est. Tokens (`Raw -> Clean`) | Mean Release Latency |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| HTML pages | 26 | 26,402 | 9,306 | 64.8% | 6,601 -> 2,327 | 51.8 us |
| Feeds (RSS/Atom/JSON) | 4 | 3,031 | 1,424 | 53.0% | 758 -> 356 | 23.2 us |
| Transcripts (WebVTT/SRT) | 2 | 520 | 205 | 60.6% | 130 -> 52 | 1.4 us |
| Native Markdown | 5 | 2,108 | 2,142 | -1.6% | 527 -> 536 | 2.8 us |
| Plain/JSON passthrough | 2 | 152 | 152 | 0.0% | 38 -> 38 | 0.1 us |
| **All extractable** | **39** | **32,213** | **13,229** | **58.9%** | **8,054 -> 3,308** | **37.4 us** (`21.08 MiB/s`) |

All `4/4` non-extractable fixtures (`ratchet_challenge_403.html`, `frontier_soft_404_login_wall.html`, `ratchet_js_shell.html`, `frontier_canary_sveltekit_spa.html`) classified accurately for fail-fast blocking or CEF escalation. See [Benchmarks](README-FULL.md#benchmarks) for methodology and native Markdown link-resolution details.

## Comparison

| Tool | Execution & State | HTTP / Browser Stack | Challenge Policy | Built-in Search & Auth |
| :--- | :--- | :--- | :--- | :--- |
| **`pleb`** | Single MIT Rust binary (`~/.pleb`); zero telemetry | Cronet (`Chromium 154`) or `reqwest` + optional CEF `v5-meta` daemon | Fail fast (`exit 2`), zero CAPTCHA bypass | `pleb search` (6 providers + RRF) & `pleb auth` |
| **Firecrawl** | Cloud API, `firecrawl-local` binary, or AGPL-3.0 Docker stack (Redis, Postgres, browser) | Cloud browsers or Playwright/Fire-engine | Managed cloud unblocking | Cloud/v2 `/search` endpoint |
| **Jina Reader** | Hosted `r.jina.ai` (`20 RPM` no-key / paid tokens) or Apache-2.0 container | Hosted Chromium fleet | Rate-limit tiers (not bypass) | Separate `s.jina.ai` endpoint |
| **Crawl4AI** | Python + Playwright (Apache-2.0) or Crawl4AI Cloud | Async Playwright browser pool | Community hooks/proxies | Crawl4AI Cloud only |
| **`curl` + `html2text`** | Local CLI tools; no state | `libcurl` HTTP, no JS rendering | Raw `403` HTML output | None |

See [Comparison with Alternatives](README-FULL.md#comparison-with-alternatives) for feature trade-offs and when to pick another tool.

## Install

Build and install the `pleb` binary onto `PATH`:

```bash
# macOS Apple Silicon with Cronet (recommended)
./scripts/download-cronet-macos.sh
cargo install --path . --features cronet --locked

# Linux with Cronet
./scripts/download-cronet-linux.sh
cargo install --path . --features cronet --locked

# Without Cronet (reqwest fallback)
cargo build --release
```

Optional components:

```bash
# Install the local offscreen CEF renderer (macOS arm64; use install-cef-linux.sh on Linux x86_64/aarch64)
bash scripts/install-cef-mac.sh --force

# Symlink the bundled agent skill into local agent directories
bash scripts/install-skill.sh
```

See [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) for prerequisites, custom install prefixes, and verification.

## Quick Start

```bash
# Fetch a page as cleaned Markdown
pleb fetch https://example.com

# Scope extraction to the main article and omit images
pleb fetch --extract article --no-images https://example.com/post

# Search and pipe URLs into a concurrent batch fetch
pleb search --urls-only -n 5 "rust async runtime" | pleb fetch - --jsonl --concurrency 4

# Store a search provider key locally (prompts with echo disabled)
pleb auth set brave

# Crawl or map a site (respects robots.txt by default)
pleb crawl https://docs.example.com --depth 2 --max-pages 10
pleb map https://example.com --limit 100

# Inspect Cronet, CEF, daemon, and search provider state
pleb status
```

See [Command Reference](README-FULL.md#command-reference) for all subcommands, flags, and batch output formats.

## Full Documentation

Machine-readable agent discovery files live at [llms.txt](llms.txt), [llms-full.txt](llms-full.txt), and [skills/pleb/SKILL.md](skills/pleb/SKILL.md). Complete human and integrator documentation lives in [README-FULL.md](README-FULL.md):

- [Overview and Design Boundaries](README-FULL.md#overview-and-design-boundaries) — scope, differences from upstream `wickproject/wick`, and non-bypass boundary
- [Benchmarks](README-FULL.md#benchmarks) — reproducible corpus byte/token reduction and release latency across `tests/corpus/`
- [Comparison with Alternatives](README-FULL.md#comparison-with-alternatives) — trade-offs vs Firecrawl, Jina Reader (`r.jina.ai`), Crawl4AI, and `curl` + `html2text`
- [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) — Cronet, optional CEF `v5-meta` renderer, and agent skill installation
- [Distribution and Packaging](README-FULL.md#distribution-and-packaging) — default `reqwest` build (`crates.io`), optional Cronet builds, and Homebrew tap formula template
- [Architecture](README-FULL.md#architecture) — request pipeline and repository directory layout
- [Transport and Rendering](README-FULL.md#transport-and-rendering) — Cronet, CEF escalation, response classification, and the `daemon.sock` transport daemon
- [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation) — cleanup stages, worked HTML-to-Markdown example, source bypass, and non-HTML formats
- [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging) — zero-fetch resource cues, `--resources`, `--stage`, and local artifact validation
- [Search and Credentials](README-FULL.md#search-and-credentials) — provider routing, federation, cooldowns, and `pleb auth`
- [Crawl and Map](README-FULL.md#crawl-and-map) — bounded wave crawling and sitemap discovery
- [Command Reference](README-FULL.md#command-reference) — global flags, subcommand options, and output formats
- [Configuration and State Layout](README-FULL.md#configuration-and-state-layout) — environment variables, state files, and `site-rules.json`
- [Exit Codes and Error Envelopes](README-FULL.md#exit-codes-and-error-envelopes) — exit codes `0` through `5`, `130`, and JSON error format
- [JSON Output Schemas](README-FULL.md#json-output-schemas) — exact field contracts for `--json` and `--jsonl` outputs
- [Testing and Design Documents](README-FULL.md#testing-and-design-documents) — test commands, corpus evaluation, hermetic isolation, and specs in `docs/`

## License

MIT — Copyright (c) 2026 Memory Sync Hub LLC (forked from [wickproject/wick](https://github.com/wickproject/wick)). See [LICENSE](LICENSE).
