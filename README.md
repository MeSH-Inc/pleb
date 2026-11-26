# mesh-wick

Local, zero-telemetry web fetch and search CLI (`wick`) for AI agents, forked from [wickproject/wick](https://github.com/wickproject/wick).

Uses Chrome's network stack (Cronet) for browser-identical TLS/HTTP2/QUIC fingerprinting, an optional embedded Chromium (CEF `v5-meta`) renderer for JS-heavy pages, structured source negotiation (GitHub/GitLab/Gist raw source rewrites, advertised Markdown mirrors, native `text/markdown` extraction, and RSS 2.0 / Atom 1.0 / JSON Feed 1.1 conversion), single-parse `O(N)` DOM-pruned compact markdown extraction with charset-aware decoding, and multi-provider search balancing (`brave`, `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`, `federated`) with tool-owned credential storage (`wick auth`).

## Context Cleanup for Agents

Raw HTML spends most of its bytes on navigation menus, hidden popovers, cookie banners, tracking query parameters, code-gutter tables, and duplicate visual spans that waste context tokens. In its default mode (`--extract clean --source auto --resources none`), `wick fetch` runs a deterministic, single-parse `O(N)` cleanup pipeline before emitting Markdown or plain text.

| Stage | Removed or Bypassed | Preserved or Rescued |
| :--- | :--- | :--- |
| **1. Source bypass (`--source`)** | Forge chrome on GitHub `/blob/`, GitLab `/-/blob/`, and Gist pages; HTML wrappers when an RFC 8288 `Link` or `<link rel="alternate" type="text/markdown">` mirror (or `--source negotiate` `Accept: text/markdown` / `<path>.md`) is available; leading YAML frontmatter (`<= 4 KiB`) in `clean`/`article` modes | Raw source files wrapped in language-tagged code fences (`raw_source`, with HTML fallback on `404`/binary); native Markdown with YAML `title` promoted to `# Heading` and relative links/reference definitions resolved outside code fences |
| **2. Single-pass DOM pruning** | `<script>`, `<style>`, `<noscript>`, `<template>`, `[hidden]`, `[aria-hidden="true"]`, `.sr-only`, inline `display: none` / `visibility: hidden` (except TeX-annotated math), `<= 1px` tracking pixels; `<nav>`, `<footer>`, `<aside>`, `<header>` without `<h1>`, ARIA `navigation` / `search` / `banner` / `contentinfo` / `complementary` / `menu` / `menubar` / `tooltip`; cookie consent banners (`onetrust`, `cookie-banner`, `cmp-container`), standalone `Skip to ...` blocks, unchecked checkbox popups (`aria-haspopup="true"`), mdBook/MediaWiki chrome (`[edit]`, hidden categories, CS1 maintenance tags, print footers), heading permalink glyphs (`#`, `§`, `¶`, `🔗`), `<pre>` copy buttons and `.line-numbers-rows`; non-landmark link clusters (`> 65%` link text, average link `<= 32` bytes) | `<main>`, `<article>`, `[role="main"]`, `<table>`, headings, prose, and code blocks shielded from link-density pruning; `<h1>` document headers and real interactive checkboxes inside lists |
| **3. Technical signal rescue** | Line-number pipe artifacts (`\| 1 \|`) in syntax-highlighted code tables; duplicate `aria-hidden` KaTeX/MathJax spans; UI icon `<svg>` sprites (`icon`, `octicon`, `lucide`, `feather`, `fa-*`, `< 64px`, or inside controls); `data:` placeholder image URIs; legacy charset mojibake | Two-column gutter tables (`hljs-ln`, `blob-num`, `linenos`, `rouge-table`, `highlighttable`, or sequential `1..N` code rows) unwrapped to language-tagged fenced code while leaving `<thead>`/`<th>` data tables intact; KaTeX/MathJax TeX annotations as `$...$` / `$$...$$`; content `<svg>` `<title>`/`aria-label`/`<text>`/`<foreignObject>` labels as `[Diagram: ...]`; `data-src` / `<picture>` / descriptor-aware `srcset` URLs (preserving commas in image-transform paths); UTF-8/UTF-16 BOM and WHATWG Windows-1252 (`0x80..=0x9F`) decoding |
| **4. URL hygiene & Markdown compaction** | Tracking parameters (`utm_*`, `fbclid`, `gclid`, `dclid`, `msclkid`, `mc_cid`, `mc_eid`, `igshid`, `_hsenc`, `_hsmi`, `ref_src`); `javascript:` / `vbscript:` / `data:` link wrappers (unwrapped to plain text); horizontal pipe-table padding and separator dashes (`------` -> `---`), trailing whitespace, excess blank lines, duplicate `# Title` headings | Clean and presigned query strings (such as S3 `X-Amz-Signature`) left un-reencoded; relative links resolved against `<base href>` / final URL; unbalanced URL parentheses escaped; table alignment colons and fenced code blocks untouched; fence-safe `--max-bytes` truncation that closes open code fences (or rewinds before them) before `[... truncated]` |
| **5. Non-HTML & deferred assets** | Verbose RSS/Atom/JSON Feed XML/JSON boilerplate; YouTube player/sidebar chrome; WebVTT/SRT timing cues, `STYLE`/`NOTE`/`REGION` blocks, inline cue tags, and rolling-caption duplicates; decorative badges (`shields.io`), avatars, emojis, and `< 100x80` icons; accidental binary dumps in text mode (fail fast with exit `5`) | Compact Markdown digests for RSS 2.0, Atom 1.0, and JSON Feed 1.1; YouTube title, channel, duration, description, and prioritized caption tracks from `ytInitialPlayerResponse`; `**[Speaker]:**` transcript turns; zero-fetch indexing of diagrams, transcripts, PDFs (`citation_pdf_url` + `.pdf`), datasets (`.csv`, `.tsv`, `.parquet`, `.jsonl`), and schemas (`openapi.yaml`, `llms.txt`, `.proto`, `.sql`) surfaced via `stderr` and `--json` for selective staging |

### Example: Raw HTML to Agent-Ready Markdown

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
    <pre><button type="button">Copy</button><code class="language-bash">wick fetch --max-bytes 4096 https://docs.example.com/replication</code></pre>
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

`wick fetch` reduces this to `590` bytes (`76.9%` smaller) on `stdout`, plus a zero-request resource cue on `stderr`:

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
wick fetch --max-bytes 4096 https://docs.example.com/replication
```

[Diagram: Write Path — Leader, Quorum, Commit Log]

![Replication topology](https://docs.example.com/cdn-cgi/image/width=800,quality=80/arch.webp)
````

```text
Resources: 1 diagram (r1) — pass --resources manifest or --stage-resources <ids>
```

### Extraction Modes & Context Controls

| Control | Behavior |
| :--- | :--- |
| `--extract clean` *(default)* | Prunes page chrome, hidden nodes, and non-landmark link clusters across `<body>` while preserving full page structure (docs, forums, indexes, feeds, and transcripts). |
| `--extract article` | Applies all `clean` pruning, then scopes output to the primary `<article>` (single or `>= 65%` dominant), `<main>` / `[role="main"]`, or highest non-link-text container. |
| `--extract full` | Keeps the full document body (including YAML frontmatter and verbatim feed/transcript payloads), stripping only executable `<script>`/`<style>`/`<noscript>`/`<template>` and `<pre>` UI noise. |
| `--max-bytes <N>` | Hard UTF-8 byte cap with line-boundary and balanced code-fence closing before `[... truncated]`. |
| `--no-images` / `--no-links` | Drops image tags or unwraps anchors to plain text when context budget is tight. |
| `--resources <mode>` / `--stage-resources <ids>` | Defaults to `none` (index only, zero asset fetches); pulls transcripts or stages selected diagrams/documents locally into `<state_dir>/artifacts/` only on demand. |

Extraction and cleanup regressions are enforced against HTML, Markdown, feed, transcript, and binary fixtures in [`tests/corpus/`](tests/corpus/) via `cargo test --test corpus_evaluation`.

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
- `--source <auto|negotiate|none>` (default: `auto`):
  - `auto`: preserves Chrome's exact primary `Accept` header while rewriting GitHub/GitLab/Gist blob/gist URLs to lossless raw sources (`strategy: "raw_source"`, with automatic `404` HTML fallback) and following advertised HTTP `Link` or HTML `<link rel="alternate" type="text/markdown">` mirrors (`strategy: "markdown_mirror"`)
  - `negotiate`: sends `Accept: text/markdown` on the primary HTTP GET (`strategy: "markdown_negotiated"`) and probes `<path>.md` on documentation paths when HTML lacks an alternate link
  - `none`: disables raw-source rewrites, Markdown mirror discovery, and `Accept: text/markdown` negotiation (also bypassed automatically for `--format html`, `--format raw`, and `--render cef`)
- `--wait-for-selector <css>`
- `--resources <none|manifest|transcripts|diagrams|all>` (default: `none`):
  - `none`: zero subresource network requests; populates `resources` in `--json`/`--jsonl`/`--metadata-only` and prints a concise `stderr` cue line when high-salience diagrams, transcripts, documents, datasets, or schemas are detected
  - `manifest`: appends a `## Linked Resources` Markdown table
  - `transcripts`: fetches up to 4 `.vtt`/`.srt` media transcripts with safe `Referer` headers and appends clean speaker prose
  - `diagrams`: stages up to 8 high-salience diagrams (`<= 16 MiB` each) into `<state_dir>/artifacts/` and rewrites Markdown image paths
  - `all`: runs `transcripts` plus stages high-salience diagrams, documents, datasets, and schemas
- `--stage-resources <IDS>` (comma-separated resource IDs, e.g. `r1,r3`): selectively stage or inline named resources from the fetched document
- `--referer <URL>`: optional parent page URL supplying `strict-origin-when-cross-origin` `Referer` and `Sec-Fetch-*` headers on direct asset fetches
- `--stage`: directly fetch and stage a single binary/document asset into `<state_dir>/artifacts/` (validating magic bytes and rejecting disguised HTML), printing its local path (or JSON metadata with `--json`); conflicts with `--output`, `--metadata-only`, `--jsonl`, and batch mode
- `--respect-robots` (disabled by default for interactive single-URL fetches)
- `--no-rewrite` (disable host rewrites such as `reddit.com` -> `old.reddit.com` and forge raw-source rewrites)
- `--no-images`, `--no-links`, `--max-bytes <N>`
- `--concurrency <N>` (batch mode max concurrent fetches, `1..=16`, default `4`, capped at 2 per host)
- `--output <path>` (`-o`): write output to a file instead of stdout (single-URL only; created only after fetch succeeds)
- `--json` (global flag), `--jsonl` (streaming per-URL JSON lines), `--metadata-only`

Native `text/markdown` responses extract/strip YAML frontmatter in `clean`/`article` modes, resolve relative links outside code fences, and apply fence-safe `--max-bytes` truncation. Syndication feeds (RSS 2.0, Atom 1.0, and JSON Feed 1.1) are automatically converted into compact Markdown digests in `clean`/`article` modes (pass `--extract full` or `--format raw` for verbatim XML/JSON). Inline content `<svg>` diagrams rescue readable `<text>` labels (`[Diagram: ...]`) while filtering icon sprites, and WebVTT (`.vtt`) / SubRip (`.srt`) transcripts strip timing cues and rolling-caption duplicates while preserving speaker turns.

### Search

Balances queries across configured providers (`brave`, `tavily`, `serper`, `exa`, `searxng`) with health-aware cooldowns, burst pacing, intent routing (`--mode`), Reciprocal Rank Fusion (`--provider federated`), and DuckDuckGo HTML fallback:

```bash
wick search "rust async runtime" -n 8
wick search --mode exact "E0382 borrow checker"
wick search --mode semantic "local-first crdt sync architectures"
wick search --provider federated --json "cronet tls fingerprint"
wick search --provider duckduckgo --json "cronet tls fingerprint"
wick search --site docs.rs --freshness month --urls-only "tokio"
wick search --extra-snippets "cronet http2"
```

Key `search` flags:

- `-n` / `--limit <N>` (`1..=20`, default `8`)
- `--provider <auto|brave|tavily|serper|exa|searxng|duckduckgo|federated>` (alias `ddg`)
- `--mode <auto|exact|semantic>` (default: `auto`; steers intent routing in `--provider auto`)
- `--freshness <day|week|month|year>`
- `--site <domain>` (prepends `site:<domain>` after stripping scheme/trailing slash)
- `--extra-snippets` (additional snippet excerpts from supported providers)
- `--urls-only` (one URL per line, empty stdout on zero results; conflicts with `--json`)

### Search Credentials (`wick auth`)

Store search provider credentials locally without exporting keys in shell profiles or passing secrets in command-line arguments:

```bash
wick auth set brave
op read "op://Vault/Tavily/credential" | wick auth set tavily --stdin
wick auth set searxng --stdin --storage file <<< "http://127.0.0.1:8888"
wick auth status
wick auth status --json
wick auth remove tavily
```

Subcommands:

- `wick auth set <brave|tavily|serper|exa|searxng> [--stdin] [--storage <auto|keychain|file>]` (prompts with echo disabled on `/dev/tty`, or reads a single-line `<= 4096`-byte secret from `--stdin`; clears any stale provider cooldown)
- `wick auth remove <brave|tavily|serper|exa|searxng> [--storage <auto|keychain|file>]` (removes stored credentials, clears cooldown, and warns on `stderr` if the provider's environment variable is still set)
- `wick auth status` (inherits global `--json`; reports provider configuration source, monthly request count, cooldown remaining, and last HTTP error status without printing secret values, URLs, prefixes, or lengths)

Resolution precedence: environment variable (`BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL`) -> macOS Keychain (skipped when `WICK_NO_KEYCHAIN=1`) -> `<state_dir>/credentials.json` (`0o600`).

### Crawl and Map

Both `crawl` (bounded concurrent wave frontier with polite per-origin spacing / `Crawl-delay` and deterministic ordering) and `map` (recursive `<sitemapindex>` + `.xml.gz` + `robots.txt` `Sitemap:` discovery) respect `robots.txt` by default (pass `--no-robots` to override):

```bash
wick crawl https://docs.example.com --depth 2 --max-pages 10
wick map https://example.com --limit 100
```

### Session, Daemon, Status, and Agent Skill

```bash
wick status
wick status --json
wick daemon status
wick daemon stop
wick session clear
wick skill print
```

To symlink the canonical `mesh-wick` skill (`skills/mesh-wick/SKILL.md`) into local agent harnesses (`~/.agents`, `~/.gemini`, `~/.claude`, `~/.codex`, `~/.config/opencode`):

```bash
bash scripts/install-skill.sh
```

## Configuration

- `MESH_WICK_HOME` / `--state-dir`: private (`0o700`) state directory for proxy-partitioned HTTP cache/cookies (`data/`), CEF `cef-cache/`, content-addressed staged artifacts (`artifacts/`, `256 MiB` LRU cap), unified transport daemon socket (`daemon.sock`), daemon logs (`daemon.log`), `site-cache.json` (`0o600`), `search-state.json` (`0o600`), `credentials.json` (`0o600`), and optional local `site-rules.json` overlay over bundled structural rules (`reddit.com` -> `old.reddit.com`, `x.com`/`twitter.com`) (default: `~/.mesh-wick`; `wick session clear` stops any running daemon and clears `data/`, `cef-cache/`, `artifacts/`, `site-cache.json`, and `search-state.json` while preserving `credentials.json`, `site-rules.json`, and Keychain entries)
- `MESH_WICK_PROXY` / `--proxy`: SOCKS5/HTTP proxy URL
- `MESH_WICK_TIMEZONE`: optional IANA timezone override injected into CEF `Intl.DateTimeFormat`
- `BRAVE_API_KEY`, `TAVILY_API_KEY`, `SERPER_API_KEY`, `EXA_API_KEY`, `SEARXNG_URL`: environment overrides for search provider credentials (take precedence over `wick auth` Keychain and file storage)
- `WICK_NO_KEYCHAIN=1`: disables macOS Keychain lookups/writes (forces `<state_dir>/credentials.json` for `--storage auto`; required for hermetic tests)
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
