# pleb — Full Documentation

`pleb`, maintained by Memory Sync Hub LLC, is a local, zero-telemetry web acquisition and search CLI (`pleb`) and Rust library (`pleb`, Cargo package `pleb`) for AI agents, forked from [wickproject/wick](https://github.com/wickproject/wick). For a short overview and quick start, see [README.md](README.md).

## Contents

- [Overview and Design Boundaries](#overview-and-design-boundaries)
- [Benchmarks](#benchmarks)
- [Comparison with Alternatives](#comparison-with-alternatives)
- [Installation and Runtime Setup](#installation-and-runtime-setup)
- [Distribution and Packaging](#distribution-and-packaging)
- [Architecture](#architecture)
- [Transport and Rendering](#transport-and-rendering)
- [Extraction and Source Negotiation](#extraction-and-source-negotiation)
- [Resource Indexing and Staging](#resource-indexing-and-staging)
- [Search and Credentials](#search-and-credentials)
- [Crawl and Map](#crawl-and-map)
- [Command Reference](#command-reference)
- [Configuration and State Layout](#configuration-and-state-layout)
- [Exit Codes and Error Envelopes](#exit-codes-and-error-envelopes)
- [JSON Output Schemas](#json-output-schemas)
- [Testing and Design Documents](#testing-and-design-documents)
- [License](#license)

---

## Overview and Design Boundaries

`pleb` acquires public web documents over browser-grade network and offscreen-rendering transports and emits compact, structurally faithful Markdown, plain text, HTML, accessibility trees (`axtree`), or raw bytes with deterministic metadata and exit codes.

Key capabilities beyond upstream `wickproject/wick`:

- **Zero telemetry and local-only configuration**: All upstream telemetry pings and remote `site-rules.json` sync calls are removed. Per-site routing rules are bundled statically and optionally overlaid from `<state_dir>/site-rules.json`.
- **Unified warm transport daemon (`daemon.sock`, protocol `4`)**: Shares persistent Cronet engines and warm CEF renderer processes across invocations, partitioned by proxy configuration, with automatic restart on binary rebuild (`PLEB_BUILD_ID`).
- **CEF `v5-meta` and external CDP/Kitesurf renderers**: Local CEF reports real HTTP status codes, final redirect URLs, and typed transport error kinds over a framed stdio protocol; `--cdp-url` / `PLEB_CDP_URL` connects over WebSocket (`ws://` loopback or `wss://` with optional Cloudflare Access service tokens) to Chrome DevTools Protocol / Kitesurf for DOM, `--format axtree`, and imperative WebMCP discovery (`--tools manifest`).
- **HTTP Message Signatures (`web-bot-auth`)**: Optional RFC 9421 Ed25519 request signing on outbound HTTPS requests (`pleb auth set web-bot-auth-jwk`, disabled per invocation with `--no-web-bot-auth`).
- **Structured source negotiation and first-class non-HTML parsing**: Rewrites GitHub, GitLab, and Gist pages to raw source, follows advertised Markdown mirrors, negotiates `text/markdown`, converts RSS/Atom/JSON feeds to Markdown digests, extracts YouTube metadata and captions, and cleans WebVTT/SRT transcripts.
- **Single-parse `O(N)` DOM extraction and zero-fetch resource indexing**: Prunes chrome, rescues code tables, math, and SVG labels, indexes high-salience linked assets without extra requests, and stages binary artifacts on demand with magic-byte verification.
- **Multi-provider search and local credential storage (`pleb auth`)**: Balances queries across `brave`, `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`, and `federated` modes with cooldown tracking, burst pacing, intent routing, and tool-owned credentials.

### Behavioral Boundaries

`pleb` does not solve CAPTCHAs, click challenge widgets, or inject anti-bot evasion scripts. When an origin returns an active bot challenge, `pleb` classifies it immediately into a challenge error (exit code `2`) without issuing duplicate CEF or CDP retries. When connecting to Cloudflare Workers / `workerd` Kitesurf endpoints, note that Cloudflare's Worker runtime enforces outbound network isolation — do not expose unauthenticated local CDP endpoints without Cloudflare Access (`cdp-access-*`) or loopback binding.

---

## Benchmarks

Reproducible offline benchmark across all `45` sanitized fixtures in [tests/corpus_evaluation.rs](tests/corpus_evaluation.rs) (`41` extractable documents in `ExtractMode::Clean` -> `OutputFormat::Markdown`, `50` warm release passes per fixture on Apple Silicon, plus `4` non-extractable gate fixtures):

```bash
cargo run --release --example corpus_benchmark
cargo run --release --example corpus_benchmark -- --json
```

| Category | Fixtures | Raw Bytes | Output Bytes | Byte Reduction | Est. Tokens (`Raw -> Clean`) | Mean Release Latency |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| HTML pages | 28 | 40,221 | 9,652 | 76.0% | 10,056 -> 2,413 | 62.3 us |
| Syndication feeds (RSS/Atom/JSON) | 4 | 3,031 | 1,424 | 53.0% | 758 -> 356 | 22.3 us |
| Transcripts (WebVTT/SRT) | 2 | 520 | 205 | 60.6% | 130 -> 52 | 1.5 us |
| Native Markdown | 5 | 2,108 | 2,142 | -1.6% | 527 -> 536 | 3.0 us |
| Plain/JSON passthrough | 2 | 152 | 152 | 0.0% | 38 -> 38 | 0.1 us |
| **All extractable** | **41** | **46,032** | **13,575** | **70.5%** | **11,508 -> 3,394** | **45.1 us** (`23.72 MiB/s`) |

Methodology notes:
- **Token estimation**: Uses the deterministic `ceil(bytes / 4)` heuristic (exact BPE counts vary by tokenizer; raw UTF-8 bytes are exact).
- **Native Markdown expansion (`-1.6%`)**: Relative paths inside Markdown fixtures (such as `./CONTRIBUTING.md`) are resolved against the base URL into full URLs so agents receive valid links.
- **Classification gates (`4/4` matched)**: `ratchet_challenge_403.html` (`challenge`), `frontier_soft_404_login_wall.html` (`blocked`), `ratchet_js_shell.html` (`javascript_shell`), and `frontier_canary_sveltekit_spa.html` (`javascript_shell`) all classify accurately before extraction.
- **Hardware caveat**: Latency (`us`) and combined throughput (`23.72 MiB/s`, `1.851 ms` per full pass) depend on host CPU and reflect release builds (`target/release/examples/corpus_benchmark`).

---

## Comparison with Alternatives

| Dimension | `pleb` | Firecrawl | Jina Reader (`r.jina.ai`) | Crawl4AI | `curl` + `html2text` |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **License & telemetry** | MIT; zero telemetry, local-only | AGPL-3.0 OSS / Cloud API | Apache-2.0 OSS / Cloud API | Apache-2.0 OSS / Cloud API | Permissive; local CLI |
| **Runtime & dependencies** | Single Rust binary (plus optional CEF bundle) | Cloud API, `firecrawl-local` binary, or Docker Compose (Redis, PostgreSQL, browser) | Hosted `r.jina.ai`/`s.jina.ai` (`20 RPM` no-key / paid tokens) or stateless/S3 Docker image | Python + async Playwright Chromium or Crawl4AI Cloud | POSIX CLI tools |
| **Network & JS stack** | Cronet (`Chromium 154`) or `reqwest` (`rustls`) + optional warm CEF `v5-meta` daemon | Hosted unblocking/actions or Playwright/Fire-engine | Hosted headless browser fleet | Playwright Chromium browser pool | `libcurl` HTTP, no JS execution |
| **Challenge policy** | Fail fast (exit `2`), no CAPTCHA bypass | Cloud tier provides managed unblocking | Tiered rate limits (paid keys do not bypass access controls) | Community stealth / proxy hooks | Raw `403` HTML response |
| **Structured source bypass** | GitHub/GitLab/Gist raw, RFC 8288/`<link>` Markdown mirrors, RSS/Atom/JSON, VTT/SRT, YouTube captions | HTML/PDF scraping and LLM `/extract` schemas | HTML, PDF, MS Office, and VLM image captioning | `PruningContentFilterLXML`, BM25/LLM filters | Generic HTML stripping only |
| **Search & credential store** | Built-in `pleb search` (6 providers + RRF) & `pleb auth` (`env` / Keychain / `0o600` file) | Separate `/search` endpoint in Cloud/v2 API | Separate `s.jina.ai` search (`10,000` fixed tokens per API search) | Search in Crawl4AI Cloud | None |

### When to Choose Another Tool

- Choose **Firecrawl** when you need managed cloud anti-bot unblocking, interactive browser actions (`click`, `scroll`), hosted MCP integrations, or LLM-generated structured JSON schemas (`/extract`).
- Choose **Jina Reader** when zero-install HTTP prefixing (`https://r.jina.ai/<url>`) fits your workload or you need hosted PDF parsing and image captioning built into the reader endpoint.
- Choose **Crawl4AI** when you are building async Python + Playwright pipelines with built-in BM25 or LLM content-filtering stages.
- Choose **`pleb`** when you want a fast, single-binary MIT CLI with zero external cloud dependencies, zero telemetry, browser-grade Cronet TLS, deterministic single-pass DOM extraction, structured source bypass, and unified local search + credential storage.

---

## Installation and Runtime Setup

### Prerequisites

- Rust toolchain `1.89` or newer (`Cargo.toml` sets `rust-version = "1.89"`).
- C/Objective-C compiler (`clang` on macOS, `gcc` on Linux) when building with Cronet or the optional CEF renderer.

### Building and Installing `pleb`

From `crates.io` (pure-Rust `reqwest` with `rustls` TLS fallback):

```bash
cargo install pleb
```

From a repository checkout with Cronet on macOS Apple Silicon (recommended for Chrome TLS/HTTP2/QUIC):

```bash
./scripts/download-cronet-macos.sh
cargo install --path . --features cronet --locked
```

With Cronet on Linux (`x86_64` or `aarch64`):

```bash
./scripts/download-cronet-linux.sh
cargo install --path . --features cronet --locked
```

Linux `--features cronet` binaries link dynamically against `libcronet.so` with `-Wl,-rpath,$ORIGIN` and the checkout `lib/linux_<arch>` directory. If you relocate an installed binary and delete the source checkout, copy `lib/linux_<arch>/libcronet.so` next to the binary or set `LD_LIBRARY_PATH`.

Without Cronet (`reqwest` with `rustls` TLS fallback):

```bash
cargo build --release
```

`cargo install --path . --features cronet --locked` builds the `pleb` crate and installs the `pleb` binary onto `PATH`. Because `daemon.sock` checks the compile-time `PLEB_BUILD_ID` on each connection, an older running daemon restarts automatically after reinstall.

### Optional CEF Renderer (`v5-meta`)

To render JavaScript-heavy pages and automatically escalate blocked (`403`/`503`) or unhydrated SPA responses in `--render auto` mode, build and install the offscreen CEF `v5-meta` renderer:

```bash
# macOS Apple Silicon (installs ~/.pleb/cef/pleb-renderer.app)
bash scripts/install-cef-mac.sh --force

# Linux x86_64 or aarch64 (installs ~/.pleb/cef/pleb-renderer and pleb-helper)
bash scripts/install-cef-linux.sh --force
```

Both installers accept `--prefix <dir>` to install into an isolated directory rather than `${PLEB_HOME:-$HOME/.pleb}/cef`. Before upgrading an existing CEF install while a daemon is running, stop the daemon first (`pleb daemon stop || true`).

### Agent Skill Installation

The repository ships a version-locked agent reference at [skills/pleb/SKILL.md](skills/pleb/SKILL.md), embedded directly into the `pleb` binary (`pleb skill print`). Install it into local agent harness directories (`~/.agents`, `~/.gemini`, `~/.claude`, `~/.codex`, `~/.config/opencode`) from any installed binary, or symlink it from a source checkout:

```bash
pleb skill install [--dir <PATH>] [--force]   # works anywhere (binary-only or source)
bash scripts/install-skill.sh                 # source-checkout symlink helper
```

### Verifying the Installation

```bash
pleb status
pleb status --json
```

Healthy local installations report `"version"`, `"cef_renderer": { "status": "ready", "expected_protocol": "v5-meta" }` (when CEF is installed), daemon status (`protocol_version: 4` while running, `null` when stopped), active state directory, and configured search providers. `"cronet_enabled"` is `true` for `--features cronet` builds and `false` for pure-Rust `reqwest` (`rustls`) builds.

---

## Distribution and Packaging

`Cargo.toml` sets pure-Rust `reqwest` (`rustls`) as the default feature set so `cargo install pleb` and offline package-manager sandboxes (such as Homebrew) build cleanly without external prebuilt binary downloads:

- **Cargo / `crates.io` (`pleb`)**: `Cargo.toml` excludes `research/` and `.gitignore` / `Cargo.toml` keep prebuilt `lib/*/libcronet.*` static/shared libraries out of published `.crate` tarballs while packaging all sources (`src/`, `cef/`, `data/`, `scripts/`, `skills/pleb/SKILL.md`, `packaging/`, and `README*`). Build with Cronet TLS from a repository checkout using `./scripts/download-cronet-macos.sh` or `./scripts/download-cronet-linux.sh` followed by `cargo install --path . --features cronet --locked`.
- **Homebrew tap template**: [packaging/homebrew/pleb.rb](packaging/homebrew/pleb.rb) installs the pure-Rust `pleb` binary alongside helper setup scripts (`pkgshare/scripts/`), CEF renderer sources (`pkgshare/cef/`), and [skills/pleb/SKILL.md](skills/pleb/SKILL.md) (`pkgshare/skills/pleb`), printing `pleb skill install`, CEF, and Cronet source-build instructions in `caveats`.
- **Verification**: Check crate packaging contents before publishing with `cargo package --list`.

---

## Architecture

### Request Pipeline

```text
CLI / Library Call
  │
  ├─► Policy & Routing (src/policy/)
  │     • Apply bundled + local site-rules.json (unless --no-rewrite)
  │     • Optional robots.txt check (opt-in on fetch, default on crawl/map)
  │     • Structured source candidate check (GitHub/GitLab/Gist raw_source)
  │     • Select initial transport (Cronet vs. CEF via rules + site-cache.json)
  │
  ├─► Transport Layer (src/transport/ & src/daemon/)
  │     • Unix-socket daemon (daemon.sock, protocol 4) or --no-daemon in-process
  │     • Primary HTTP: Cronet (Chromium 154) or reqwest fallback
  │     • Classify response -> auto-escalate blocks / JS shells to CEF v5-meta
  │     • Follow advertised Markdown mirrors / optional <path>.md negotiation
  │
  ├─► Extraction & Indexing (src/extract/)
  │     • Charset decode (UTF-8/UTF-16 BOM, WHATWG Windows-1252)
  │     • Dispatch native Markdown, RSS/Atom/JSON Feed, YouTube, VTT/SRT, or HTML
  │     • Single-parse O(N) DOM pruning, signal rescue, and salience indexing
  │     • URL hygiene, Markdown compaction, fence-safe --max-bytes truncation
  │
  └─► Selective Resource Staging & Output (src/artifacts.rs, src/fetch.rs)
        • Zero-fetch stderr resource cue & JSON resources array by default
        • Optional transcript inline / content-addressed artifact staging (0o600)
        • Emit Markdown, text, HTML, raw bytes, JSON, or streaming JSONL
```

### Repository Layout

| Path | Responsibility |
| :--- | :--- |
| `src/main.rs` | `clap` CLI definition, subcommand dispatch, TTY/stdin secret reading, JSON/text output and error envelopes |
| `src/lib.rs` | Public Rust library surface (`pleb`) exposing all subsystems |
| `src/context.rs` | `RuntimeConfig`, `RuntimeContext`, private directory/lock helpers (`0o700`/`0o600`), atomic writes, and `session clear` |
| `src/fetch.rs` | Single-URL, raw, direct-stage, and concurrent batch fetch orchestration, resource enrichment, and `FetchError` exit mapping |
| `src/crawl.rs` | Bounded concurrent wave crawler (`pleb crawl`) and sitemap/`robots.txt` URL mapper (`pleb map`) |
| `src/transport/` | `TransportClient`, Chrome 154 headers, RFC 9421 Web Bot Auth (`web_bot_auth.rs`), Cronet FFI (`cronet.rs`, `cronet_ffi.rs`), CEF manager (`cef.rs`), async WebSocket CDP / Kitesurf renderer (`cdp.rs`), typed `TransportError` |
| `src/daemon/` | Persistent Unix-socket transport daemon (`protocol.rs`, `server.rs`, `client.rs`) handling `Ping`, `Shutdown`, `HttpGet`, and `CefRender` |
| `src/policy/` | Response classifier (`classify.rs`), strategy planner (`mod.rs`), host/mirror/raw-source resolver (`source.rs`), `site-rules.json` loader (`rules.rs`), per-site strategy cache (`cache.rs`), and `robots.txt` cache (`robots.rs`) |
| `src/extract/` | Single-parse HTML pruning (`dom.rs`), Markdown conversion and compaction (`markdown.rs`), WebMCP tool indexing (`webmcp.rs`), CDP accessibility tree formatter (`axtree.rs`), native Markdown (`native_markdown.rs`), URL hygiene (`url.rs`), charset decoding (`charset.rs`), feeds (`feed.rs`), transcripts (`transcript.rs`), YouTube watch extraction (`youtube.rs`), and resource salience indexing (`resources.rs`) |
| `src/artifacts.rs` | Content-addressed `<state_dir>/artifacts/` store, magic-byte validation, disguised-HTML rejection, and `256 MiB` LRU eviction |
| `src/search/` | Multi-provider search dispatch, intent routing, Reciprocal Rank Fusion (`mod.rs`), provider HTTP adapters (`providers.rs`), and locked health/cooldown/pacing state (`state.rs`) |
| `src/auth.rs` | 3-tier credential resolution (`env` -> AES-256-GCM `credentials.enc` vault wrapped by macOS Keychain `vault-key` -> `credentials.json`), secret zeroization, and status reporting |
| `cef/` | C/Objective-C sources for the `v5-meta` offscreen CEF renderer and helper (`protocol.h`, `browser_environment.h`, `renderer_core.h`, `helper_core.h`, macOS `.m` and Linux `.c` entrypoints) |
| `data/site-rules.json` | Bundled structural host rewrite and mandatory CEF overrides |
| `examples/corpus_benchmark.rs` | Offline corpus byte/token reduction and warm release extraction benchmark |
| `llms.txt`, `llms-full.txt` | Machine-readable LLM discovery index and single-file technical reference |
| `packaging/homebrew/pleb.rb` | Homebrew tap formula template (`reqwest` default build + setup caveats) |
| `scripts/` | Cronet downloaders, CEF build/install scripts, and agent skill symlink installer |
| `skills/pleb/SKILL.md` | Canonical version-locked skill embedded in `pleb skill print` |
| `docs/` | Milestone architecture specifications (`01` through `06`) |
| `tests/` | Integration suites, CLI/parity checks, and corpus fixtures (`tests/corpus/`, `tests/corpus_evaluation.rs`) |

---

## Transport and Rendering

### Primary HTTP Transport (Cronet and `reqwest`)

When compiled with `--features cronet`, `pleb` links a prebuilt static/shared Chromium `154.0.8037.49` Cronet library (`darwin_arm64`, `linux_amd64`, or `linux_arm64`) to produce browser-identical TLS, HTTP/2, and QUIC fingerprints, paired with matching Chrome `154` `User-Agent`, greased `Sec-Ch-Ua`, `Accept-Encoding` (`gzip, deflate, br, zstd`), and navigation/subresource `Sec-Fetch-*` headers. Without `--features cronet`, `pleb` falls back to `reqwest` (`rustls`).

Both Cronet (`on_redirect_received`) and `reqwest` (`redirect::Policy`) enforce redirect-time SSRF protection: `3xx` hops must remain on `http`/`https`, public origins cannot pivot to loopback, RFC 1918, link-local/IMDS, or `.local`/`.internal` targets, and local origins are restricted to same-host hops. Disallowed redirects cancel immediately with a typed `redirect_blocked` transport error.

### Offscreen CEF Renderer (`v5-meta`)

When installed at `<state_dir>/cef/pleb-renderer.app` (macOS arm64) or `<state_dir>/cef/pleb-renderer` (Linux `x86_64` or `aarch64`), or pointed to by `PLEB_CEF_RENDERER`, `pleb` verifies the embedded `PLEB_RENDERER_PROTOCOL=v5-meta` marker before spawning the renderer. The renderer runs offscreen with:

- Native headless execution without Xvfb on Linux when `DISPLAY` is unset or empty (`--ozone-platform=headless` and `--password-store=basic`; `--use-mock-keychain` on macOS) to avoid X11 and keyring deadlocks.
- Audio muted (`--mute-audio`) and autoplay suppressed.
- Subresource completion plus `MutationObserver` quiet-window readiness (and optional `--wait-for-selector <css>`), dumping immediately when a short challenge interstitial is detected.
- Minimal OSR viewport, screen, language, and optional `PLEB_TIMEZONE` (`Intl.DateTimeFormat`) defaults.
- Framed responses carrying real HTTP status codes, post-redirect final URLs, and typed error kinds (`timeout`, `load_error`, `invalid_selector`, `protocol_mismatch`).

### Response Classification and Strategy Escalation

In `--render auto` mode (default):

1. `pleb` checks bundled and local `site-rules.json` (by default `reddit.com`/`www.reddit.com` rewrite to `old.reddit.com` over Cronet, and `x.com`/`twitter.com` route to CEF waiting for `article`), then consults `<state_dir>/site-cache.json`.
2. Unless a rule or cached strategy selects CEF first, `pleb` fetches via Cronet/`reqwest` and classifies the response:
   - `403`/`503` without an active bot challenge -> `Blocked` (escalates to CEF if available).
   - `403`/`503` or `200` interstitial matching an active bot challenge -> `Challenge` (fails fast with exit code `2`; no duplicate CEF retry).
   - `200` HTML with low prose volume matching an unhydrated SPA/JS shell -> `JavaScriptShell` (escalates to CEF if available).
   - `200` HTML with low prose volume redirected to an authentication path, containing a password form, or presenting a cookie wall -> `Blocked` (exit code `2`).
   - `200` HTML with `< 300` prose bytes and a soft-404 title -> `HttpError` (`404`, exit code `3`).
   - Other `4xx`/`5xx` statuses -> `HttpError` (exit code `3`).
3. Successful escalations are recorded in `site-cache.json` so subsequent requests to the same host avoid repeating failed probes.

Use `--render cef` (aliases: `browser`, `render`, `js`) to force local Chromium rendering, `--render cdp` (aliases: `kitesurf`, `remote`) to force remote/local WebSocket CDP rendering, or `--render cronet` (aliases: `network`, `fast`, `no-js`, `nojs`) to force network-only transport. Pass `--prefer-cdp` (`PLEB_PREFER_CDP`) to escalate to CDP ahead of local CEF in `--render auto`.

### Pluggable CDP / Kitesurf Renderer (`--render cdp`, `--format axtree`)

When `--cdp-url` (`PLEB_CDP_URL`) is configured (and `--proxy` is unset so WebSockets never bypass `--proxy`), `pleb` connects directly from the CLI process over `tokio-tungstenite` (`rustls` `ring` provider + native root CAs):

- **Endpoint security**: Plaintext `ws://` is restricted to loopback hosts (`localhost`, `127.0.0.1`, `[::1]`) and verified via post-TCP-connect `peer_addr().ip().is_loopback()` before WebSocket upgrade. Remote `wss://` endpoints reject local/private target URLs, sanitize non-`101` HTTP errors to status code only, redact query parameters (`?token=...`) in logs/`status`, and attach host-scoped `CF-Access-Client-Id` / `CF-Access-Client-Secret` headers only when the `wss://` host matches `cdp-access-host`.
- **Kitesurf fast mode & isolation**: Automatically appends `?layout=off` on `/devtools/browser` (or sends `Kitesurf.setLayoutMode({ mode: "off" })` on `/devtools/page/<id>`) unless `--format axtree` needs CSS visibility layout. Intercepts sub-navigations via `Fetch.enable` (`Fetch.requestPaused`) and navigation events against SSRF and CDP self-SSRF, captures `WebMCP.enable` imperative tools, polls `Kitesurf.getPageState` (`125ms` ticks, max `1s`, or `250ms` fallback), and closes owned targets cleanly (`Target.closeTarget`, `500ms` grace) without invoking `Browser.close`.
- **`--format axtree` (alias `ax`)**: Requests `Accessibility.getFullAXTree`, skips ignored/generic wrapper nodes while promoting children, escapes control bytes, and truncates safely via `--max-bytes`. Requires CDP (rejects `--stage`, `--render cronet`, `--render cef`, `--proxy`, and unset `--cdp-url`).

### Web Bot Auth (RFC 9421 Ed25519)

When `web-bot-auth-jwk` (Ed25519 private JWK) and `web-bot-auth-agent` (`https://` signature-agent URL) are configured via `pleb auth set`, `pleb fetch` signs public `https://` acquisition requests with RFC 9421 `Signature`, `Signature-Input` (`@authority` and `signature-agent`, `tag="web-bot-auth"`, RFC 7638 `keyid`), and `Signature-Agent` headers. Plaintext `http://`, local/private targets, `pleb search`, `robots.txt`, `sitemap.xml`, and CDP handshakes stay unsigned, and cross-authority / downgrade redirects strip or re-sign headers hop-by-hop. Pass `--no-web-bot-auth` (`PLEB_NO_WEB_BOT_AUTH`) to disable signing.

### Unified Transport Daemon (`daemon.sock`)

On Unix systems, `pleb` automatically starts a background transport daemon bound to `<state_dir>/daemon.sock` (protocol version `4`, locked via `daemon.lock`, logging to `daemon.log`). The daemon:

- Reuses Cronet engines and warm CEF renderer processes across invocations.
- Partitions persistent storage (`data/` and `cef-cache/`) by proxy configuration (`direct` or `proxy-<hash>`).
- Serves `Ping`, `Shutdown`, `HttpGet`, and `CefRender` framed operations with up to `256 MiB` response framing, pre-dispatch protocol/build-ID checks, and immediate cancellation of in-flight CEF renders if the client disconnects.
- Restarts automatically if the client binary's compile-time `PLEB_BUILD_ID` differs from the running daemon.
- Exits automatically after `--daemon-idle-timeout` seconds of inactivity (default `300`).

Pass `--no-daemon` (or set `PLEB_NO_DAEMON=1`) to run transport in-process. If a daemon already owns `<state_dir>`, `--no-daemon` refuses to share the same state directory until you stop the daemon (`pleb daemon stop`) or select an isolated `--state-dir`.

---

## Extraction and Source Negotiation

Raw HTML spends most of its bytes on navigation menus, hidden popovers, cookie banners, tracking query parameters, code-gutter tables, and duplicate visual spans that waste context tokens. In its default mode (`--extract clean --source auto --resources none`), `pleb fetch` runs a deterministic, single-parse `O(N)` pipeline before emitting Markdown or plain text.

### Structured Source Negotiation (`--source`)

| Mode | Behavior |
| :--- | :--- |
| `--source auto` *(default)* | Preserves Chrome's primary `Accept` header while rewriting GitHub `/blob/`, GitLab `/-/blob/`, and Gist URLs to lossless raw sources (`strategy: "raw_source"`, with automatic HTML fallback on `404` or binary responses) and following advertised RFC 8288 `Link` or HTML `<link rel="alternate" type="text/markdown">` mirrors (`strategy: "markdown_mirror"`). |
| `--source negotiate` | Sends `Accept: text/markdown` on the primary HTTP GET (`strategy: "markdown_negotiated"`) and probes `<path>.md` on documentation paths when HTML lacks an alternate link. |
| `--source none` | Disables raw-source rewrites, Markdown mirror discovery, and `Accept: text/markdown` negotiation. Automatically applied for `--format html`, `--format raw`, `--format axtree`, `--render cef`, and `--render cdp`. |

### Five-Stage Cleanup Pipeline

| Stage | Removed or Bypassed | Preserved or Rescued |
| :--- | :--- | :--- |
| **1. Source bypass (`--source`)** | Forge chrome on GitHub `/blob/`, GitLab `/-/blob/`, and Gist pages; HTML wrappers when an RFC 8288 `Link` or `<link rel="alternate" type="text/markdown">` mirror (or `--source negotiate` `Accept: text/markdown` / `<path>.md`) is available; leading YAML frontmatter (`<= 4 KiB`) and standalone `Skip to ...` fragment-link lines in `clean`/`article` modes | Raw source files wrapped in language-tagged code fences (`raw_source`, with HTML fallback on `404`/binary); native Markdown with YAML `title` promoted to `# Heading` (without duplicating a matching first H1) and relative links/reference definitions resolved outside code fences |
| **2. Single-pass DOM pruning** | `<script>`, `<style>`, `<noscript>`, `<template>`, `[hidden]`, `[aria-hidden="true"]`, `.sr-only`, inline `display: none` / `visibility: hidden` (except TeX-annotated math), `<= 1px` tracking pixels; `<nav>`, `<footer>`, `<aside>`, `<header>` without `<h1>`, ARIA `navigation` / `search` / `banner` / `contentinfo` / `complementary` / `menu` / `menubar` / `tooltip`; cookie consent banners (`onetrust`, `cookie-banner`, `cmp-container`), standalone `Skip to ...` links and non-landmark `<button>` skip controls, unchecked checkbox popups (`aria-haspopup="true"`), mdBook/MediaWiki chrome (`[edit]`, hidden categories, CS1 maintenance tags, print footers), heading permalink glyphs (`#`, `§`, `¶`, `🔗`), `<pre>` copy buttons and `.line-numbers-rows`; non-landmark `<a>`/`<button>` navigation clusters (`> 65%` interactive text, average item `<= 32` bytes) | `<main>`, `<article>`, `[role="main"]`, `<table>`, headings, prose, and code blocks shielded from link-density pruning; `<h1>` document headers and real interactive controls inside landmarks |
| **3. Technical signal rescue** | Line-number pipe artifacts (`\| 1 \|`) in syntax-highlighted code tables; duplicate `aria-hidden` KaTeX/MathJax spans; UI icon `<svg>` sprites (`icon`, `octicon`, `lucide`, `feather`, `fa-*`, `< 64px`, or inside controls); `data:` placeholder image URIs; legacy charset mojibake | Two-column gutter tables (`hljs-ln`, `blob-num`, `linenos`, `rouge-table`, `highlighttable`, or sequential `1..N` code rows) unwrapped to language-tagged fenced code while leaving `<thead>`/`<th>` data tables intact; KaTeX/MathJax TeX annotations as `$...$` / `$$...$$`; content `<svg>` `<title>`/`aria-label`/`<text>`/`<foreignObject>` labels as `[Diagram: ...]`; `data-src` / `<picture>` / descriptor-aware `srcset` URLs (preserving commas in image-transform paths); UTF-8/UTF-16 BOM and WHATWG Windows-1252 (`0x80..=0x9F`) decoding |
| **4. URL hygiene & Markdown compaction** | Tracking parameters (`utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `mc_cid`, `mc_eid`, `igshid`, `_hsenc`, `_hsmi`, `ref_src`); `javascript:` / `vbscript:` / `data:` link wrappers (unwrapped to plain text); horizontal pipe-table padding and separator dashes (`------` -> `---`), trailing whitespace, excess blank lines, duplicate `# Title` headings | Clean and presigned query strings (such as S3 `X-Amz-Signature`) left un-reencoded; relative links resolved against `<base href>` / final URL; unbalanced URL parentheses escaped; table alignment colons and fenced code blocks untouched; fence-safe `--max-bytes` truncation that closes open code fences (or rewinds before them) before `[... truncated]` |
| **5. Non-HTML & deferred assets** | Verbose RSS/Atom/JSON Feed XML/JSON boilerplate; YouTube player/sidebar chrome; WebVTT/SRT timing cues, `STYLE`/`NOTE`/`REGION` blocks, inline cue tags, and rolling-caption duplicates; decorative badges (`shields.io`), avatars, emojis, and `< 100x80` icons; accidental binary dumps in text mode (fail fast with exit `5`) | Compact Markdown digests for RSS 2.0, Atom 1.0, and JSON Feed 1.1; YouTube title, channel, duration, description, and prioritized caption tracks from `ytInitialPlayerResponse`; `**[Speaker]:**` transcript turns; zero-fetch indexing of diagrams, transcripts, PDFs (`citation_pdf_url` + `.pdf`), datasets (`.csv`, `.tsv`, `.parquet`, `.jsonl`), and schemas (`openapi.yaml`, `llms.txt`, `.proto`, `.sql`) surfaced via `stderr` and `--json` for selective staging |

### Worked Example: Raw HTML to Agent-Ready Markdown

Given a `2,551`-byte HTML page containing skip navigation, a cookie banner, `<nav>`, an ARIA menu popover, a heading permalink, KaTeX math with an `aria-hidden` duplicate, tracked and presigned links, a hidden telemetry block, a `1x1` tracking pixel, a line-numbered Rust code table, a `<pre>` copy button, an icon `<svg>`, a content `<svg>` diagram, a `<picture>` with a base64 placeholder and comma-bearing `srcset`, and a `<footer>`:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <base href="https://docs.example.com/">
  <title>Replication Notes | Example Docs</title>
</head>
<body>
  <a class="skip-link" href="#main">Skip to main content</a>
  <div class="cookie-banner">We use cookies to personalize ads. <button>Accept</button></div>
  <nav><a href="/">Home</a> <a href="/pricing">Pricing</a> <a href="/login">Sign in</a></nav>
  <div role="menu"><span>Quick switch menu</span></div>
  <main id="main">
    <h1>Replication Notes</h1>
    <h2 id="checksums">Checksums <a class="headerlink" href="#checksums">¶</a></h2>
    <p>Each segment verifies integrity using
      <span class="katex">
        <span class="katex-mathml"><math><semantics><annotation encoding="application/x-tex">H(m) = \sum_{i=0}^{n-1} b_i</annotation></semantics></math></span>
        <span class="katex-html" aria-hidden="true">H(m)=Σbi</span>
      </span>
      before commit. See the <a href="/spec?utm_source=nav&amp;fbclid=xyz">wire spec</a> or
      <a href="https://s3.example.com/seg.bin?X-Amz-Signature=a%2Bb%3Dc">signed snapshot</a>.
    </p>
    <div style="display: none">Hidden telemetry banner</div>
    <img width="1" height="1" src="https://track.example.net/pixel.gif" alt="pixel">
    <div class="highlight highlight-source-rust">
      <table class="hljs-ln"><tbody>
        <tr><td class="hljs-ln-numbers" data-line-number="1">1</td><td><code>pub fn hash(data: &amp;[u8]) -&gt; u64 {</code></td></tr>
        <tr><td class="hljs-ln-numbers" data-line-number="2">2</td><td><code>    data.iter().fold(0u64, |acc, &amp;b| acc.wrapping_add(b as u64))</code></td></tr>
        <tr><td class="hljs-ln-numbers" data-line-number="3">3</td><td><code>}</code></td></tr>
      </tbody></table>
    </div>
    <pre><button type="button">Copy</button><code class="language-bash">pleb fetch --max-bytes 4096 https://docs.example.com/replication</code></pre>
    <button aria-label="Close"><svg class="octicon" width="16" height="16"><text>x</text></svg></button>
    <p><svg role="img" aria-label="Write Path" width="520" height="160">
      <text>Leader</text><text>Quorum</text><text>Commit Log</text>
    </svg></p>
    <figure>
      <picture>
        <source srcset="/cdn-cgi/image/width=400,quality=75/arch.webp 400w, /cdn-cgi/image/width=800,quality=80/arch.webp 800w">
        <img src="data:image/gif;base64,R0lGODlhAQABAAAAACw=" alt="Replication topology">
      </picture>
    </figure>
  </main>
  <footer>Example Corp · Privacy · Terms</footer>
</body>
</html>
```

`pleb fetch` reduces this to `590` bytes (`76.9%` smaller) on `stdout`, plus a zero-request resource cue on `stderr`:

````markdown
# Replication Notes

## Checksums

Each segment verifies integrity using $H(m) = \sum_{i=0}^{n-1} b_i$ before commit. See the [wire spec](https://docs.example.com/spec) or [signed snapshot](https://s3.example.com/seg.bin?X-Amz-Signature=a%2Bb%3Dc).

```rust
pub fn hash(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &b| acc.wrapping_add(b as u64))
}
```

```bash
pleb fetch --max-bytes 4096 https://docs.example.com/replication
```

[Diagram: Write Path — Leader, Quorum, Commit Log]

![Replication topology](https://docs.example.com/cdn-cgi/image/width=800,quality=80/arch.webp)
````

```text
Resources: 1 diagram (r1) — pass --resources manifest or --stage-resources <ids>
```

### Non-HTML First-Class Formats

- **Native Markdown (`text/markdown`)**: Strips leading YAML frontmatter in `clean`/`article` modes (promoting `title` to `# Heading`), resolves relative links and reference definitions outside fenced code blocks, strips inline HTML/MDX tags when emitting `--format text`, compacts whitespace, and applies fence-safe `--max-bytes` truncation.
- **Syndication feeds (RSS 2.0, Atom 1.0, JSON Feed 1.1)**: Automatically converted into compact Markdown digests in `clean`/`article` modes (pass `--extract full` or `--format raw` for verbatim XML/JSON).
- **YouTube watch pages**: Extracts title, channel, duration, description, and prioritized caption tracks directly from `ytInitialPlayerResponse` over Cronet without launching CEF.
- **Transcripts (WebVTT `.vtt` and SubRip `.srt`)**: Strips timing cues, metadata blocks, inline cue tags, and rolling-caption duplicates while preserving `**[Speaker]:**` turns.
- **Binary payloads**: In non-raw document modes (`markdown`, `text`, `html`), unsupported binary content types fail fast with exit code `5` rather than dumping binary bytes to `stdout`.

### Extraction Modes and Context Controls

| Control | Behavior |
| :--- | :--- |
| `--extract clean` *(default)* | Prunes page chrome, hidden nodes, and non-landmark link clusters across `<body>` while preserving full page structure (docs, forums, indexes, feeds, and transcripts). |
| `--extract article` | Applies all `clean` pruning, then scopes output to the primary `<article>` (single or `>= 65%` dominant), `<main>` / `[role="main"]`, or highest non-link-text container. |
| `--extract full` | Keeps the full document body (including YAML frontmatter and verbatim feed/transcript payloads), stripping only executable `<script>`/`<style>`/`<noscript>`/`<template>` and `<pre>` UI noise. |
| `--max-bytes <N>` | Hard UTF-8 byte cap with line-boundary and balanced code-fence closing before `[... truncated]`. |
| `--no-images` / `--no-links` | Drops image tags or unwraps anchors to plain text when context budget is tight. |

---

## Resource Indexing and Staging

During its single `O(N)` DOM pass, `pleb fetch` indexes linked non-prose resources (`diagram`, `image`, `transcript`, `document`, `dataset`, `schema`) and assigns deterministic IDs (`r1`, `r2`, ...) with `section_heading`, `label`, `dimensions`, and `salience` metadata while making zero subresource network requests. Inline content `<svg>` diagrams also rescue readable `<text>` labels directly into Markdown (`[Diagram: ...]`).

### Resource Policies (`--resources`)

| Mode | Behavior |
| :--- | :--- |
| `none` *(default)* | Zero subresource network requests. Populates `resources` in `--json`/`--jsonl`/`--metadata-only` and prints a one-line `stderr` cue whenever high-salience resources are detected. |
| `manifest` | Appends a `## Linked Resources` Markdown table to the document output without downloading assets. |
| `transcripts` | Fetches up to `4` `.vtt`/`.srt` media transcripts with safe `Referer` headers and appends clean speaker prose. |
| `diagrams` | Stages up to `8` high-salience diagrams (`<= 16 MiB` each) into `<state_dir>/artifacts/` and rewrites Markdown image paths to local files. |
| `all` | Runs `transcripts` and stages high-salience diagrams, documents, datasets, and schemas. |

### Selective and Direct Staging

- `--stage-resources <IDS>`: Comma-separated resource IDs (for example `r1,r3`) from a page's indexed resources to selectively stage or inline.
- `--stage`: Directly downloads and stages a single binary/document URL into `<state_dir>/artifacts/`, printing its local path (or JSON metadata with `--json`). Conflicts with `--output`, `--metadata-only`, `--jsonl`, `--format raw`, and batch mode.
- `--referer <URL>`: Supplies a parent page URL to compute `strict-origin-when-cross-origin` `Referer` and `Sec-Fetch-Site` (`same-origin`, `same-site`, `cross-site`) headers on direct asset fetches.

### Artifact Store Contract

Staged files are written atomically to `<state_dir>/artifacts/<hash>-<name>.<ext>` (`0o600`, `256 MiB` LRU eviction budget). Before staging a binary asset, `pleb` checks magic bytes and rejects disguised HTML soft-404 responses.

---

## Search and Credentials

### Search Providers and Routing

`pleb search` supports six concrete providers (`brave`, `tavily`, `serper`, `exa`, `searxng`, and zero-key `duckduckgo` / `ddg`) plus two orchestration modes (`auto` and `federated`):

- **`--provider auto` *(default)***: Routes queries to the highest-priority configured and healthy provider according to `--mode`, `--extra-snippets`, and query shape, falling back to `duckduckgo` if no API provider is configured or all configured providers fail:
  - `--mode semantic`: `exa -> tavily -> brave -> serper -> searxng -> duckduckgo`
  - `--mode exact` (or lexical needle detected in `--mode auto`: quoted phrases, `::` scope operators, `E0382`-style error codes, `ERR_*`, `0x*` hex tokens, or path/namespace tokens): `searxng -> serper -> brave -> tavily -> exa -> duckduckgo`
  - `--mode auto` with `--extra-snippets`: `brave -> tavily -> exa -> serper -> searxng -> duckduckgo`
  - `--mode auto` general query: `searxng -> brave -> tavily -> serper -> exa -> duckduckgo`
- **`--provider federated`**: Queries configured healthy providers concurrently and merges rankings using deterministic Reciprocal Rank Fusion (`RRF`, `k = 60`).

Provider health, monthly UTC request counts, error cooldowns, and Brave burst pacing (`1100ms` window, spilling to the next healthy provider in `auto` mode) are tracked in `<state_dir>/search-state.json` under an exclusive `search-state.lock`.

### WebMCP Tool Indexing (`--tools`)

Pages declaring HTML `<form toolname="..." tooldescription="...">` controls or registering imperative tools via `navigator.modelContext.registerTool(...)` (captured over CDP `WebMCP.enable`) are indexed read-only into `tools` (`WebMcpToolDescriptor[]`, capped at `32` tools per page with `untrusted_content: true` enforced):

- `--tools none` *(default)*: Populates `tools` in `--json`/`--jsonl`/`--metadata-only` and prints a concise one-line `stderr` cue (`Tools: ...`) on non-JSON fetches.
- `--tools manifest`: Appends a `## Page Tools (WebMCP)` Markdown section and tool input JSON Schemas (conflicts with `--stage` and `--format raw`).

### Credential Storage (`pleb auth`)

Store search provider keys, Web Bot Auth credentials, or Cloudflare Access CDP service tokens locally without exporting secrets in shell profiles or passing them on the command line:

```bash
pleb auth set brave
op read "op://Vault/Tavily/credential" | pleb auth set tavily --stdin
pleb auth set searxng --stdin --storage file <<< "http://127.0.0.1:8888"
cat agent.jwk | pleb auth set web-bot-auth-jwk --stdin
pleb auth set web-bot-auth-agent --stdin <<< "https://agent.example.com"
pleb auth set cdp-access-host --stdin <<< "kitesurf.example.com"
pleb auth status
pleb auth status --json
pleb auth remove tavily
```

- **Supported providers**: `brave`, `tavily`, `serper`, `exa`, `searxng`, `web-bot-auth-jwk`, `web-bot-auth-agent`, `cdp-access-client-id`, `cdp-access-client-secret`, `cdp-access-host`.
- **Resolution precedence**: Environment variable (`BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL`, `PLEB_WEB_BOT_AUTH_JWK`, `PLEB_WEB_BOT_AUTH_AGENT`, `PLEB_CDP_ACCESS_CLIENT_ID`, `PLEB_CDP_ACCESS_CLIENT_SECRET`, `PLEB_CDP_ACCESS_HOST`) -> `<state_dir>/credentials.enc` (`0o600` AES-256-GCM vault wrapped by one `vault-key` item in macOS Keychain under service `pleb`; skipped with zero Keychain calls when absent or when `PLEB_NO_KEYCHAIN=1` / `true`) -> `<state_dir>/credentials.json` (`0o600`).
- **Input hygiene**: `pleb auth set` never accepts secrets as positional arguments. Interactive mode reads from `/dev/tty` with echo disabled; `--stdin` accepts printable ASCII up to `4096` bytes (multi-line JSON whitespace is allowed solely for `web-bot-auth-jwk` and canonicalized on store). Child `daemon.sock` and CEF processes explicitly scrub all credential env vars and `PLEB_CDP_URL`. In-memory secret buffers are zeroized on drop, and `pleb auth status` never prints secret values, private keys, or Access tokens.
- **Cooldown reset**: `pleb auth set` and `pleb auth remove` clear any stale cooldown entry for that provider. `pleb auth remove` warns on `stderr` if the provider's environment variable is still set.

---

## Crawl and Map

Both `pleb crawl` and `pleb map` respect `robots.txt` by default (pass `--no-robots` to override):

- **`pleb crawl`**: Performs breadth-first crawling over same-origin HTML links using bounded concurrency (`4` workers), polite per-origin spacing (`100ms` default, or `robots.txt` `Crawl-delay` when present), post-redirect origin tracking, and deterministic output ordering. Caps `--depth` at `5` (default `2`), `--max-pages` at `50` (default `10`), per-page extracted content at `10,000` bytes, and plain output at `100,000` bytes. Seed URL errors propagate loudly (non-zero exit), while broken child links are skipped.
- **`pleb map`**: Discovers site URLs from `robots.txt` `Sitemap:` directives, recursive `<sitemapindex>` trees (up to depth `3`, `64` sitemaps, `32 MiB` decompressed `.xml.gz` cap), and up to `20` HTML page crawls unless `--no-sitemap` is set. Caps `--limit` at `5000` (default `100`).

```bash
pleb crawl https://docs.example.com --depth 2 --max-pages 10 --path-filter /guide
pleb map https://example.com --limit 100
```

---

## Command Reference

### Global Flags

| Flag | Environment Variable | Default | Description |
| :--- | :--- | :--- | :--- |
| `--proxy <URL>` | `PLEB_PROXY` | none | SOCKS5 or HTTP proxy URL |
| `--timeout <SECS>` | — | `30` | Network request timeout in seconds |
| `--max-response-bytes <BYTES>` | — | `16 MiB` doc / `128 MiB` raw | Maximum HTTP response body size |
| `--cef-timeout <SECS>` | — | `60` | CEF render timeout in seconds |
| `--no-web-bot-auth` | `PLEB_NO_WEB_BOT_AUTH` | `false` | Disable RFC 9421 Web Bot Auth request signing |
| `--cdp-url <URL>` | `PLEB_CDP_URL` | none | WebSocket CDP / Kitesurf endpoint (`ws://` loopback or `wss://`) |
| `--cdp-timeout <SECS>` | `PLEB_CDP_TIMEOUT` | `60` | CDP render timeout in seconds |
| `--prefer-cdp` | `PLEB_PREFER_CDP` | `false` | Prefer CDP ahead of local CEF in `--render auto` |
| `--state-dir <DIR>` | `PLEB_HOME` | `~/.pleb` | Private (`0o700`) local cache and state directory |
| `--no-daemon` | `PLEB_NO_DAEMON` | `false` | Bypass `daemon.sock` and run transport in-process |
| `--daemon-idle-timeout <SECS>` | `PLEB_DAEMON_IDLE_SECONDS` | `300` | Idle exit timeout for the background transport daemon |
| `-v`, `--verbose` | — | `false` | Enable debug tracing (`pleb=debug`) to `stderr` |
| `-q`, `--quiet` | — | `false` | Suppress non-error status lines on `stderr` |
| `--json` | — | `false` | Emit structured JSON on `stdout` and JSON error envelopes on `stderr` |

### `pleb fetch`

Fetch one or more `http`/`https` URLs, or pass `-` to read newline-separated URLs from `stdin` (blank lines and `#` comments are ignored).

```bash
pleb fetch https://example.com
pleb fetch --extract article --no-images https://example.com/post
pleb fetch --render cef --wait-for-selector "article" https://x.com/user
pleb fetch --format text https://news.ycombinator.com
pleb fetch --format raw --output doc.pdf https://example.com/doc.pdf
pleb fetch --stage --referer https://example.com/guide https://example.com/arch.png
pleb fetch --json --metadata-only https://example.com
pleb fetch --jsonl --concurrency 4 https://example.com https://example.org
pleb search --urls-only "rust async runtime" | pleb fetch -
```

| Flag | Values / Default | Notes |
| :--- | :--- | :--- |
| `--format <FMT>` | `markdown` *(default)*, `html`, `text`, `axtree`, `raw` | Aliases: `md`, `txt`, `ax`. `axtree` requires CDP (`--cdp-url`); `raw` rejects `--json`, `--jsonl`, `--metadata-only`, `--stage`, batch mode, and writing to a TTY without `-o`. |
| `--extract <MODE>` | `clean` *(default)*, `article`, `full` | Extraction scope for HTML, Markdown, feeds, and transcripts. |
| `--render <MODE>` | `auto` *(default)*, `cef`, `cdp`, `cronet` | Aliases: `browser`/`render`/`js` (`cef`), `kitesurf`/`remote` (`cdp`), `network`/`fast`/`no-js`/`nojs` (`cronet`). |
| `--source <MODE>` | `auto` *(default)*, `negotiate`, `none` | Structured source and Markdown mirror policy. |
| `--resources <POLICY>` | `none` *(default)*, `manifest`, `transcripts`, `diagrams`, `all` | Linked resource enrichment policy. |
| `--tools <POLICY>` | `none` *(default)*, `manifest` | WebMCP tool indexing policy (`manifest` appends `## Page Tools (WebMCP)`). |
| `--stage-resources <IDS>` | comma-separated IDs (e.g., `r1,r3`) | Selectively stage or inline indexed resources from the page. |
| `--referer <URL>` | URL | Parent page URL used to compute `Referer` and `Sec-Fetch-*` headers. |
| `--stage` | flag | Stage a single binary/document URL into `<state_dir>/artifacts/` and print its path (or JSON). |
| `--wait-for-selector <CSS>` | CSS selector | Selector to wait for during CEF or CDP rendering. |
| `--respect-robots` | flag | Enforce `robots.txt` on `fetch` (skipped by default). |
| `--no-rewrite` | flag | Disable host rewrites (`reddit.com` -> `old.reddit.com`) and forge raw-source rewrites. |
| `--no-images` / `--no-links` | flags | Omit images or unwrap links in Markdown output. |
| `--max-bytes <N>` | integer | UTF-8 byte cap with fence-safe Markdown truncation. |
| `--concurrency <N>` | `1..=16` *(default `4`)* | Batch mode concurrency (hard cap of `2` concurrent fetches per host). |
| `--jsonl` | flag | Stream one JSON result object per completed URL (conflicts with `--json`). |
| `--metadata-only` | flag | Omit `content` while retaining metadata, `warning`, `resources`, and `tools` (requires `--json` or `--jsonl`; conflicts with `-o`). |
| `-o`, `--output <PATH>` | file path | Write output to a file on single-URL fetch (created only after success). |

In batch mode (`2+` URLs or `-` on `stdin`), default Markdown output emits numbered `## [i/N]` sections, `--json` emits an input-ordered JSON array, and `--jsonl` streams completion-ordered JSON lines.

### `pleb search`

```bash
pleb search "rust async runtime" -n 8
pleb search --mode exact "E0382 borrow checker"
pleb search --mode semantic "local-first crdt sync architectures"
pleb search --provider federated --json "cronet tls fingerprint"
pleb search --provider duckduckgo --json "cronet tls fingerprint"
pleb search --site docs.rs --freshness month --urls-only "tokio"
pleb search --extra-snippets "cronet http2"
```

| Flag | Values / Default | Notes |
| :--- | :--- | :--- |
| `-n`, `--limit <N>` | `1..=20` *(default `8`)* | Maximum results to return. |
| `--provider <NAME>` | `auto` *(default)*, `brave`, `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`, `federated` | Alias: `ddg` for `duckduckgo`. |
| `--mode <MODE>` | `auto` *(default)*, `exact`, `semantic` | Steers provider routing order in `--provider auto`. |
| `--freshness <WINDOW>` | `day`, `week`, `month`, `year` | Filters results by recency where supported. |
| `--site <DOMAIN>` | domain string | Strips scheme/slashes and prepends `site:<domain>`. |
| `--extra-snippets` | flag | Requests additional snippet excerpts from supported providers. |
| `--urls-only` | flag | Prints one bare URL per line (empty `stdout` on zero results; conflicts with `--json`). |

### `pleb auth`

| Subcommand | Description |
| :--- | :--- |
| `pleb auth set <provider> [--stdin] [--storage <auto\|keychain\|file>]` | Stores a credential (`brave`, `tavily`, `serper`, `exa`, `searxng`, `web-bot-auth-jwk`, `web-bot-auth-agent`, `cdp-access-client-id`, `cdp-access-client-secret`, `cdp-access-host`) via hidden TTY prompt or `--stdin` and clears stale search cooldowns. |
| `pleb auth remove <provider> [--storage <auto\|keychain\|file>]` | Deletes stored credentials, clears cooldowns, and warns if an env var still overrides the provider. |
| `pleb auth status` | Reports search provider statuses, `agent_identity` (`keyid`), and `cdp_access` (`access_host`) configuration (supports `--json`). |

### `pleb crawl` and `pleb map`

| Command / Flag | Default / Limit | Description |
| :--- | :--- | :--- |
| `pleb crawl <URL>` | — | Crawl same-origin pages into Markdown or JSON (`--json`). |
| `-d`, `--depth <N>` | `2` *(max `5`)* | Maximum link depth from seed URL (`crawl`). |
| `-n`, `--max-pages <N>` | `10` *(max `50`)* | Maximum pages to fetch (`crawl`). |
| `--extract <MODE>` | `clean` | Extraction scope (`clean`, `article`, `full`) (`crawl`). |
| `pleb map <URL>` | — | List discovered URLs via sitemap and link discovery (`--json` supported). |
| `-n`, `--limit <N>` | `100` *(max `5000`)* | Maximum URLs to discover (`map`). |
| `--no-sitemap` | `false` | Skip `sitemap.xml` / `robots.txt` sitemap discovery (`map`). |
| `--path-filter <PREFIX>` | none | Restrict to URLs whose path starts with `<PREFIX>` (`crawl`, `map`). |
| `--no-robots` | `false` | Ignore `robots.txt` restrictions (`crawl`, `map`). |

### Daemon, Session, Status, and Skill Commands

```bash
pleb status                 # Binary, Cronet, CEF, daemon, state dir, search providers (--json supported)
pleb daemon status          # Transport daemon status (--json supported)
pleb daemon stop            # Stop the running transport daemon (--json supported)
pleb daemon run             # Run daemon in foreground (auto-started on demand; honors --daemon-idle-timeout)
pleb session clear          # Stop daemon and clear caches/artifacts while preserving credentials and rules
pleb skill print            # Emit bundled skills/pleb/SKILL.md (--json supported)
pleb skill install          # Write bundled SKILL.md into local agent skill directories (or --dir <PATH>)
```

---

## Configuration and State Layout

### Environment Variables

| Variable | Effect |
| :--- | :--- |
| `PLEB_HOME` | Default path for `--state-dir` (fallback: `~/.pleb`). |
| `PLEB_PROXY` | Default value for `--proxy` (SOCKS5/HTTP proxy URL). |
| `PLEB_NO_DAEMON` | Default for `--no-daemon` (bypass `daemon.sock`). |
| `PLEB_DAEMON_IDLE_SECONDS` | Default for `--daemon-idle-timeout` (seconds, default `300`). |
| `PLEB_TIMEZONE` | Optional IANA timezone override injected into CEF `Intl.DateTimeFormat`. |
| `BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL` | Search provider credentials; take precedence over macOS Keychain and `<state_dir>/credentials.json`. |
| `PLEB_WEB_BOT_AUTH_JWK`, `PLEB_WEB_BOT_AUTH_AGENT` | Ed25519 private JWK and `https://` signature-agent URL for RFC 9421 Web Bot Auth signing. |
| `PLEB_CDP_ACCESS_CLIENT_ID`, `PLEB_CDP_ACCESS_CLIENT_SECRET`, `PLEB_CDP_ACCESS_HOST` | Host-scoped Cloudflare Access service-token credentials attached only when `wss://` CDP host matches `PLEB_CDP_ACCESS_HOST`. |
| `PLEB_NO_WEB_BOT_AUTH` | Disables RFC 9421 Web Bot Auth signing (`--no-web-bot-auth`). |
| `PLEB_CDP_URL`, `PLEB_CDP_TIMEOUT`, `PLEB_PREFER_CDP` | Default WebSocket CDP endpoint, render timeout (seconds), and `--render auto` CDP priority. |
| `PLEB_NO_KEYCHAIN=1` | Disables macOS Keychain reads/writes (`1` or case-insensitive `true`; forces `credentials.json` under `--storage auto`; required for hermetic tests). |
| `PLEB_CEF_RENDERER` | Explicit path to a `v5-meta` `pleb-renderer` binary (overrides `<state_dir>/cef` discovery). |
| `PLEB_NO_HOME_CEF=1` | Disables fallback discovery of `~/.pleb/cef` (used in hermetic tests). |
| `PLEB_DAEMON_EXECUTABLE` | Explicit path to the `pleb` executable spawned when auto-starting the background daemon. |
| `RUST_LOG` | Overrides default `tracing` filter on `stderr`. |

### State Directory Layout (`<state_dir>`, mode `0o700`)

| Path | Mode | Cleared by `pleb session clear` | Purpose |
| :--- | :--- | :--- | :--- |
| `data/` | `0o700` | Yes | Proxy-partitioned Cronet HTTP cache and cookies |
| `cef-cache/` | `0o700` | Yes | Proxy-partitioned CEF browser profile/cache |
| `artifacts/` | `0o700` / `0o600` | Yes | Content-addressed staged binaries (`256 MiB` LRU cap) |
| `daemon.sock`, `daemon.lock`, `daemon.log` | private | Daemon stopped | Unified Unix-socket transport daemon IPC, lock, and logs |
| `site-cache.json` | `0o600` | Yes | Learned per-host transport strategy cache |
| `search-state.json` | `0o600` | Yes | Search provider cooldowns, burst timestamps, and monthly request counts |
| `credentials.enc`, `credentials.json` | `0o600` | **Preserved** | Encrypted Keychain-backed vault (`credentials.enc`) and file-backed credentials (`credentials.json`); `vault-key` Keychain item is also preserved |
| `site-rules.json` | user-managed | **Preserved** | Optional local host-routing overlay merged over bundled `data/site-rules.json` |
| `cef/` | `0o700` | **Preserved** | Default install location for `pleb-renderer.app` / `pleb-renderer` |

---

## Exit Codes and Error Envelopes

| Exit Code | Meaning |
| :--- | :--- |
| `0` | Success |
| `1` | Invalid or conflicting CLI flags/input, unavailable/mismatched CEF or CDP renderer, response-size overflow, network/transport failure, or heterogeneous all-failed batch |
| `2` | Blocked by origin (`403`/`503`, login/cookie wall), active anti-bot challenge, or `robots.txt` disallow |
| `3` | Origin HTTP `4xx`/`5xx` error (including low-prose soft-`404` HTML pages) |
| `4` | Partial batch failure (some URLs succeeded and some failed) |
| `5` | Unsupported binary content type in non-raw mode (re-run with `--stage` or `--format raw --output <path>`) |
| `130` | Interactive TTY credential prompt interrupted (`SIGINT`/`SIGTERM`) |

When `--json` or `--jsonl` is passed, CLI errors are emitted on `stderr` as a single JSON object:

```json
{"error":"HTTP 404 error fetching https://example.com/missing","kind":"http_error","exit_code":3}
```

---

## JSON Output Schemas

All `--json` and `--jsonl` payloads serialize directly from `pleb`'s Rust output structs (`src/fetch.rs`, `src/extract/resources.rs`, `src/search/mod.rs`, `src/crawl.rs`, and `src/main.rs`).

### `FetchOutput` and `FetchMetadataOutput` (`pleb fetch --json`)

| Field | Type | Notes |
| :--- | :--- | :--- |
| `url` | `string` | Requested URL |
| `final_url` | `string` | Post-rewrite / post-redirect final URL |
| `representation_url` | `string` | Exact representation URL when distinct from `final_url` (e.g., an advertised Markdown mirror; **omitted** when identical) |
| `status_code` | `number` | Observed HTTP status code (`u16`) |
| `content_type` | `string \| null` | Response `Content-Type` header if present |
| `title` | `string \| null` | Extracted document title if present |
| `content` | `string` | Extracted Markdown, text, HTML, or accessibility tree (`axtree`) (**omitted** with `--metadata-only`) |
| `content_bytes` | `number` | Byte length of `content` (still populated with `--metadata-only`) |
| `strategy` | `string` | Acquisition strategy (`cronet`, `reqwest`, `cef`, `cdp`, `raw_source`, `markdown_mirror`, `markdown_negotiated`) |
| `content_outcome` | `string` | Machine-readable completeness classification (`complete` or `incomplete_shell`) |
| `acquired_sha256` | `string` | Lowercase SHA-256 hex digest of the acquired source body prior to extraction/truncation |
| `escalated` | `boolean` | `true` when transport escalated from primary HTTP to CEF or CDP |
| `truncated` | `boolean` | `true` when `--max-bytes` truncated output |
| `timing_ms` | `number` | End-to-end fetch duration in milliseconds |
| `warning` | `string` | Advisory warning when an unrendered or post-render `JavaScriptShell` returns partial HTML, or when a requested `--stage-resources` selector is absent (**omitted** when absent) |
| `resources` | `ResourceDescriptor[]` | Indexed linked resources (**omitted** when empty) |
| `tools` | `WebMcpToolDescriptor[]` | Indexed WebMCP page tools (**omitted** when empty; `untrusted_content: true`) |

### `BatchItemResult` (`pleb fetch --jsonl` and multi-URL `pleb fetch --json`)

Multi-URL and `stdin` (`-`) fetches emit an array (`--json`) or newline-delimited stream (`--jsonl`) of untagged `BatchItemResult` objects discriminated by `ok`:

- **Success (`ok: true`)**: `url`, `ok`, `final_url`, `representation_url` (omitted when identical to `final_url`), `status_code`, `content_type`, `title`, `strategy`, `content_outcome`, `acquired_sha256`, `escalated`, `truncated`, `timing_ms`, `content_bytes`, plus `warning` (omitted when absent), `content` (omitted with `--metadata-only`), `resources` (omitted when empty), and `tools` (omitted when empty).
- **Failure (`ok: false`)**: `url`, `ok`, `error`, `kind`, `exit_code`.

### `StagedAssetOutput` (`pleb fetch --stage --json`)

| Field | Type | Description |
| :--- | :--- | :--- |
| `url`, `final_url` | `string` | Requested and final asset URLs |
| `status_code` | `number` | HTTP status code |
| `content_type` | `string \| null` | Resolved MIME type (magic-byte verified) |
| `byte_count` | `number` | Staged file size in bytes |
| `staged_path` | `string` | Absolute path under `<state_dir>/artifacts/` |
| `timing_ms` | `number` | Fetch and stage duration in milliseconds |

### `ResourceDescriptor` (entries in `resources`)

| Field | Type | Notes |
| :--- | :--- | :--- |
| `identifier` | `string` | Stable per-page resource ID (e.g., `r1`, `r2`) |
| `kind` | `string` | `diagram`, `image`, `transcript`, `document`, `dataset`, or `schema` |
| `url` | `string` | Resolved resource URL |
| `salience` | `string` | `high` or `medium` |
| `mime_hint`, `section_heading`, `label` | `string` | Optional context metadata (omitted when absent) |
| `dimensions` | `[number, number]` | Optional `[width, height]` pixels (omitted when absent) |
| `staged_path`, `byte_count`, `content_type` | `string` / `number` | Populated after staging (omitted when not staged) |

### `SearchResponse` and `SearchResult` (`pleb search --json`)

- **`SearchResponse`**: `provider` (`string`), `providers_used` (`string[]`, omitted when empty), `results` (`SearchResult[]`).
- **`SearchResult`**: `title` (`string`), `url` (`string`), `snippet` (`string`), `extra_snippets` (`string[]`, omitted when empty).

### `StatusJson`, `DaemonStatusJson`, and `SearchProviderStatus`

- **`pleb status --json`** (`StatusJson`): `version` (`string`), `cronet_enabled` (`boolean`), `cef_renderer` (`{ status, path, found_protocol, expected_protocol }`), `cdp_renderer` (`{ configured, url, prefer_cdp, proxy_blocked, access, status_error }`), `daemon` (`DaemonStatusJson`), `state_dir` (`string`), `search_providers` (map of provider name to `SearchProviderStatus`), `agent_identity` (`AgentIdentityStatus`).
- **`pleb daemon status --json`** (`DaemonStatusJson`): `supported` (`boolean`), `running` (`boolean`), `pid` (`number | null`), `protocol_version` (`number | null`), `build_id` (`string | null`), `active_requests` (`number`), `cronet_slots` (`number`), `cef_renderers_alive` (`number`).
- **`pleb auth status --json`** (`AuthStatusJson`): flattened top-level search provider keys (`brave`, `tavily`, `serper`, `exa`, `searxng` -> `SearchProviderStatus`), plus `agent_identity` (`web_bot_auth_enabled`, `jwk_configured`, `jwk_source`, `agent_configured`, `agent_source`, `keyid`, `status_error`) and `cdp_access` (`configured`, `client_id_configured`, `client_id_source`, `client_secret_configured`, `client_secret_source`, `host_configured`, `host_source`, `access_host`, `status_error`).

### `CrawlResult`, `MapResult`, and Control Commands

| Command (`--json`) | Schema Fields |
| :--- | :--- |
| `pleb crawl` (`CrawlResult`) | `pages` (`[{ url, title, content, depth, status_code, strategy, content_outcome, truncated, acquired_sha256, warning? }]`), `failed_pages` (`[{ url, depth, error, kind }]`, omitted when empty), `urls_discovered`, `timing_ms` |
| `pleb map` (`MapResult`) | `urls` (`string[]`), `timing_ms`, `from_sitemap` |
| `pleb auth set` | `provider`, `stored`, `source` |
| `pleb auth remove` | `provider`, `removed`, `removed_from`, `env_var_still_set` |
| `pleb daemon stop` | `stopped` |
| `pleb session clear` | `cleared`, `state_dir` |
| `pleb skill print` | `name`, `version`, `content` |
| `pleb skill install` | `name`, `version`, `installed` (`[{ path, status }]`) |

---

## Testing and Design Documents

### Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --features cronet -- -D warnings
cargo test --features cronet
cargo test --test corpus_evaluation
cargo test --test cef_protocol_tests -- --ignored  # opt-in live v5-meta CEF smoke test
```

### Corpus Evaluation and Hermetic Test Isolation

- **Extraction corpus**: `tests/corpus/` holds sanitized HTML, Markdown, feed, and transcript fixtures tagged as `"ratchet"` (must pass at `1.000`) or `"frontier"` (tracked open targets), verified together with binary magic-byte guards by `tests/corpus_evaluation.rs`.
- **Hermetic test boundaries**: Tests never touch `~/.pleb` or macOS Keychain. Each test allocates a short temporary state directory under `/tmp/pleb-t-<pid>-<seq>` (`0o700`, staying under the Unix `sun_path` socket length ceiling) and sets `PLEB_NO_HOME_CEF=1` and `PLEB_NO_KEYCHAIN=1`.

### Historical Design Specifications and Contribution Rules

The milestone specifications in `docs/` record point-in-time design history (earlier milestones include pre-rename names and older Keychain/protocol layouts). Always rely on this document, [llms-full.txt](llms-full.txt), [skills/pleb/SKILL.md](skills/pleb/SKILL.md), and `src/` for current runtime and CLI behavior:

- [AGENTS.md](AGENTS.md) — Repository engineering rules, host binary sync, iteration protocol, and non-regression gates.
- [docs/01-transport-daemon-and-batch-fetch.md](docs/01-transport-daemon-and-batch-fetch.md) — Historical Milestone 1–2 design specification.
- [docs/02-swarm-hillclimb-protocol.md](docs/02-swarm-hillclimb-protocol.md) — Corpus evaluation and swarm convergence protocol.
- [docs/03-structured-source-negotiation.md](docs/03-structured-source-negotiation.md) — Historical Milestone 3 design specification.
- [docs/04-resource-indexing-and-staging.md](docs/04-resource-indexing-and-staging.md) — Historical Milestone 4 design specification.
- [docs/05-multi-provider-search-and-auth.md](docs/05-multi-provider-search-and-auth.md) — Historical Milestone 5 design specification.
- [docs/06-agent-identity-webmcp-and-cdp-renderer.md](docs/06-agent-identity-webmcp-and-cdp-renderer.md) — Historical Milestone 6 design specification.

---

## License

MIT — Copyright (c) 2026 Memory Sync Hub LLC (forked from [wickproject/wick](https://github.com/wickproject/wick)). See [LICENSE](LICENSE).
