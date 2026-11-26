# MeSH-wick

MeSH-wick, developed by Memory Sync Hub LLC, provides the `wick` command-line tool for local web fetching, web search, site crawling, and URL mapping. It is a zero-telemetry fork of [wickproject/wick](https://github.com/wickproject/wick) intended for AI agents and scripts that need clean, compact page text instead of raw HTML.

By default, `wick` makes HTTP requests using Chrome's network stack (Cronet) or a `reqwest` fallback, strips page chrome into compact Markdown or plain text in a single DOM pass, and optionally escalates JavaScript-heavy or blocked pages to a local offscreen Chromium (CEF) renderer.

## Capabilities

- **Browser-grade transport and rendering**: Sends requests over Cronet (or `reqwest`) through a shared local Unix-socket daemon, and escalates unhydrated JavaScript pages or `403`/`503` responses to an optional local CEF renderer. Active bot challenges fail fast without CAPTCHA solving or bypass scripts. See [Transport and Rendering](README-FULL.md#transport-and-rendering).
- **Deterministic HTML cleanup and source bypass**: Removes navigation, hidden nodes, cookie banners, tracking parameters, and code-gutter tables while rescuing formulas, code blocks, SVG diagram labels, and responsive images. Automatically uses raw files for GitHub/GitLab/Gist URLs, advertised Markdown mirrors, RSS/Atom/JSON feeds, YouTube captions, and WebVTT/SRT transcripts. See [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation).
- **On-demand asset staging**: Indexes diagrams, transcripts, PDFs, datasets, and schemas during the initial parse without making extra requests, and downloads selected assets into a local cache only when requested. See [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging).
- **Multi-provider search and local credentials**: Routes search queries across Brave, Tavily, Serper, Exa, SearXNG, or DuckDuckGo with cooldown tracking, burst pacing, Reciprocal Rank Fusion, and local credential storage (`wick auth`). See [Search and Credentials](README-FULL.md#search-and-credentials).
- **Site crawling and mapping**: Crawls links with bounded concurrency and `Crawl-delay` pacing, and discovers site URLs from sitemaps and `robots.txt`. See [Crawl and Map](README-FULL.md#crawl-and-map).

## Install

Build and install the `wick` binary onto `PATH`:

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
# Install the local offscreen CEF renderer (macOS arm64; use install-cef-linux.sh on Linux)
bash scripts/install-cef-mac.sh --force

# Symlink the bundled agent skill into local agent directories
bash scripts/install-skill.sh
```

See [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) for prerequisites, custom install prefixes, and verification.

## Quick Start

```bash
# Fetch a page as cleaned Markdown
wick fetch https://example.com

# Scope extraction to the main article and omit images
wick fetch --extract article --no-images https://example.com/post

# Search and pipe URLs into a concurrent batch fetch
wick search --urls-only -n 5 "rust async runtime" | wick fetch - --jsonl --concurrency 4

# Store a search provider key locally (prompts with echo disabled)
wick auth set brave

# Crawl or map a site (respects robots.txt by default)
wick crawl https://docs.example.com --depth 2 --max-pages 10
wick map https://example.com --limit 100

# Inspect Cronet, CEF, daemon, and search provider state
wick status
```

See [Command Reference](README-FULL.md#command-reference) for all subcommands, flags, and batch output formats.

## Full Documentation

Complete documentation lives in [README-FULL.md](README-FULL.md):

- [Overview and Design Boundaries](README-FULL.md#overview-and-design-boundaries) — scope, differences from upstream `wick`, and non-bypass boundary
- [Installation and Runtime Setup](README-FULL.md#installation-and-runtime-setup) — Cronet, optional CEF `v5-meta` renderer, and agent skill installation
- [Architecture](README-FULL.md#architecture) — request pipeline and repository directory layout
- [Transport and Rendering](README-FULL.md#transport-and-rendering) — Cronet, CEF escalation, response classification, and the `daemon.sock` transport daemon
- [Extraction and Source Negotiation](README-FULL.md#extraction-and-source-negotiation) — cleanup stages, worked HTML-to-Markdown example, source bypass, and non-HTML formats
- [Resource Indexing and Staging](README-FULL.md#resource-indexing-and-staging) — zero-fetch resource cues, `--resources`, `--stage`, and local artifact validation
- [Search and Credentials](README-FULL.md#search-and-credentials) — provider routing, federation, cooldowns, and `wick auth`
- [Crawl and Map](README-FULL.md#crawl-and-map) — bounded wave crawling and sitemap discovery
- [Command Reference](README-FULL.md#command-reference) — global flags, subcommand options, and output formats
- [Configuration and State Layout](README-FULL.md#configuration-and-state-layout) — environment variables, state files, and `site-rules.json`
- [Exit Codes and Error Envelopes](README-FULL.md#exit-codes-and-error-envelopes) — exit codes `0` through `5`, `130`, and JSON error format
- [JSON Output Schemas](README-FULL.md#json-output-schemas) — exact field contracts for `--json` and `--jsonl` outputs
- [Testing and Design Documents](README-FULL.md#testing-and-design-documents) — test commands, corpus evaluation, hermetic isolation, and specs in `docs/`

## License

MIT — Copyright (c) 2026 Memory Sync Hub LLC (forked from [wickproject/wick](https://github.com/wickproject/wick)). See [LICENSE](LICENSE).
