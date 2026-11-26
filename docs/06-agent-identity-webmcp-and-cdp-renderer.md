# Milestone 6 Specification: Cryptographic Agent Identity (Web Bot Auth), WebMCP Discovery, and Pluggable CDP / Kitesurf Renderer Transport

## 1. Purpose and Scope

`pleb` acquires public web documents through a low-cost escalation funnel (source bypass -> Cronet HTTP + single-parse `O(N)` DOM extraction -> optional local CEF `v5-meta`). Studying Cloudflare's experimental Rust+Wasm browser engine (`cloudflare/kitesurf`, cloned locally under `research/kitesurf/`) exposes three orthogonal, high-leverage capabilities that close `pleb`'s structural gaps without compromising its single-binary ergonomics, zero-telemetry guarantee, or strict anti-evasion boundary:

1. **Cryptographic Agent Identity via Web Bot Auth (RFC 9421 Ed25519)**: Signing outbound acquisition requests with locally stored Ed25519 credentials (`Signature`, `Signature-Input`, `Signature-Agent`) unlocks verifiable, opt-in agent access to Web Bot Auth–aware origins and CDNs using crates (`ring`, `base64`) already locked in `Cargo.lock`.
2. **Zero-Network Declarative & Rendered Imperative WebMCP Discovery**: Pages declare agent-callable structured tools via declarative HTML forms (`<form toolname="..." tooldescription="...">`) and imperative `navigator.modelContext.registerTool(...)`. Indexing WebMCP tool descriptors and JSON Schemas into `FetchOutput.tools` (plus `BatchItemSuccess` and `FetchMetadataOutput`) lets AI agents discover page capabilities in the same read-only fetch pass without scraping forms or executing untrusted side effects.
3. **Pluggable CDP / Kitesurf Remote Renderer Transport (`--render cdp`)**: Local CEF requires a ~300–500 MiB Chromium bundle and runs untrusted page JavaScript on the host workstation. Adding an async WebSocket CDP transport lets `pleb` target a Cloudflare Workers Kitesurf deployment (ephemeral per-navigation Dynamic Worker isolation), a local Docker Kitesurf container, or any standard CDP-speaking browser endpoint — with Kitesurf-aware `layout=off` fast mode, `WebMCP.enable` imperative tool capture, optional `--format axtree` accessibility tree output, and zero local CEF footprint.

### Phasing and Dependency Order

Milestone 6 is structured into two cleanly decoupled, independently shippable sub-milestones sharing one CLI, credential, and JSON contract:

- **Phase 6A (Pure Rust, zero external runtime)**: RFC 9421 Web Bot Auth signing (`src/transport/web_bot_auth.rs`, `src/auth.rs`, `daemon.sock` protocol `4`) + zero-network declarative WebMCP indexing (`src/extract/webmcp.rs`, `FetchOutput.tools`, `--tools <none|manifest>`).
- **Phase 6B (WebSocket CDP Transport)**: Pluggable CDP / Kitesurf renderer (`src/transport/cdp.rs`, `--render cdp`, `--cdp-url`, host-scoped Cloudflare Access service-token auth in `src/auth.rs`), imperative `WebMCP` discovery over CDP, and `--format axtree` (via `Accessibility.getFullAXTree`).

### Behavioral & Security Boundaries

- **Strictly read-only**: Neither `pleb fetch` nor Phase 6A/6B automatically invokes or submits any discovered WebMCP tool. All web-sourced tool metadata is unconditionally marked `untrusted_content: true`.
- **No evasion or CAPTCHA bypass**: Active bot challenges returned over signed Cronet requests or CDP/Kitesurf sessions still classify immediately into `FetchError::Challenge` (exit code `2`) with zero duplicate retries.
- **Zero secret material in child processes or IPC**: Private Ed25519 JWKs and Cloudflare Access service tokens are resolved lazily in the CLI process and never cross `daemon.sock` or `pleb-renderer` IPC. Both `spawn_detached_daemon` (`src/daemon/client.rs`) and `CefRenderer::spawn_and_await_ready` (`src/transport/cef.rs`) MUST explicitly call `.env_remove(...)` for every `CredentialProvider::ALL` environment variable plus `PLEB_CDP_URL` so secrets provided via environment variables never leak into long-lived daemon processes or untrusted Chromium/CEF renderer processes.
- **Local `workerd` isolation warning**: Mirroring `research/kitesurf/README.md`, local Docker/wrangler Kitesurf runs page JS in `workerd`, which is **not** a hardened sandbox; document prominently that untrusted public sites should target Cloudflare Workers Dynamic Workers (or an OS-sandboxed browser), not an unhardened local `workerd` instance.

---

## 2. Phase 6A — Cryptographic Agent Identity (Web Bot Auth, RFC 9421)

### 2.1 Credential Providers, Casing, and Validation (`src/auth.rs`)

Change `CredentialProvider`'s serde attribute from `#[serde(rename_all = "snake_case")]` to `#[serde(rename_all = "kebab-case")]` (wire-identical for the 5 existing single-word search providers, and matching clap `ValueEnum`, `as_str()`, `credentials.json`, and Keychain account names uniformly across all new hyphenated providers).

Add `CredentialProvider::SEARCH_PROVIDERS: &'static [Self]` (the 5 search providers) and `pub const fn is_search_provider(self) -> bool`, plus `pub fn resolve_credentials(state_dir: &Path, providers: &[CredentialProvider]) -> Result<BTreeMap<CredentialProvider, ResolvedCredential>>`. Switch `src/search/mod.rs` (`search_provider_statuses` and `resolve_all_credentials`) to `CredentialProvider::SEARCH_PROVIDERS`, and guard `search::clear_provider_cooldown` in `src/main.rs` with `if provider.is_search_provider()`, so non-search credentials never leak into `search_providers`, never pollute `search-state.json`, and never break `pleb search` if malformed.

New Web Bot Auth providers:

| Variant | CLI / JSON / Store name | Env var | Kind & Validation |
| :--- | :--- | :--- | :--- |
| `WebBotAuthJwk` | `web-bot-auth-jwk` | `PLEB_WEB_BOT_AUTH_JWK` | Ed25519 private JWK (`{"kty":"OKP","crv":"Ed25519","x":"...","d":"..."}`) |
| `WebBotAuthAgent` | `web-bot-auth-agent` | `PLEB_WEB_BOT_AUTH_AGENT` | HTTPS URL identifying the signature-agent directory (e.g., `https://agent.example.com`) |

Validation rules enforced at both `pleb auth set` and `resolve_credential`:
1. **`WebBotAuthJwk`**:
   - Pass `provider: CredentialProvider` into `validate_secret_input` (`src/main.rs`). Allow JSON whitespace (`\n`, `\r`, `\t`, `' '`) **solely** for `WebBotAuthJwk` (so pretty-printed `cat key.jwk | pleb auth set web-bot-auth-jwk --stdin` works while preserving single-line strictness for all other providers); reject NUL and all other ASCII control bytes or non-ASCII bytes.
   - Deserialize into a dedicated zeroizing struct (never generic `serde_json::Value` for the secret `d` field) that zeroizes `d` on `Drop`; decode `x` and `d` via `URL_SAFE_NO_PAD` into `32`-byte buffers (wrapping `d_bytes` in `ZeroizingBytes`).
   - Require `kty == "OKP"` and `crv == "Ed25519"`, and verify keypair consistency via `ring::signature::Ed25519KeyPair::from_seed_and_public_key(&d_bytes, &x_bytes)`.
   - Canonicalize the validated JWK to compact single-line JSON `{"crv":"Ed25519","d":"<d>","kty":"OKP","x":"<x>"}` (~135 bytes, keeping Keychain `/usr/bin/security -i` hex lines well under macOS's 4096-byte interactive buffer) before storing in Keychain or `credentials.json`.
   - Emit static, sanitized error strings on all JSON/base64/`ring` validation failures (never embedding raw `serde_json::Error`, `base64::DecodeError`, `d`, `x`, or raw JWK text).
2. **`WebBotAuthAgent`**:
   - Reject ASCII whitespace (`b.is_ascii_whitespace()`), `"`, `\`, non-ASCII, and control bytes.
   - Parse with `url::Url::parse`; require `scheme() == "https"`, `host_str().is_some()`, empty `username()`, `password().is_none()`, `query().is_none()`, and `fragment().is_none()`.
3. **Status reporting (`pleb auth status`, `pleb status --json`)**:
   - Keep `pleb auth status --json` backward-compatible for top-level search keys by serializing `AuthStatusJson` with `#[serde(flatten)] pub search_providers: BTreeMap<String, SearchProviderStatus>`, alongside dedicated `agent_identity` and `cdp_access` fields (and expose the same `agent_identity` and `cdp_renderer` blocks on `pleb status --json`):
     ```json
     "agent_identity": {
       "web_bot_auth_enabled": true,
       "jwk_configured": true,
       "jwk_source": "keychain",
       "agent_configured": true,
       "agent_source": "file",
       "keyid": "<rfc7638-jwk-thumbprint>",
       "status_error": null
     }
     ```
   - `keyid` is the public RFC 7638 SHA-256 thumbprint of `{"crv":"Ed25519","kty":"OKP","x":"<x>"}` (safe to display; never display `d` or raw JWK).
   - Both `pleb status` and `pleb auth status` surface partial or invalid WBA / CDP Access config via `status_error` and still exit `0` so operators can diagnose misconfigurations cleanly.
   - `pleb session clear` continues to preserve `credentials.json` and Keychain entries untouched.

### 2.2 RFC 9421 Wire Format and Signing Engine (`src/transport/web_bot_auth.rs`)

Promote `ring = "0.17"`, `base64 = "0.22"`, and `futures-util = { version = "0.3", default-features = false, features = ["sink"] }` (all three already locked in `Cargo.lock`: `ring 0.17.14`, `base64 0.22.1`, `futures-util 0.3.32`) to direct dependencies in `Cargo.toml`.

1. **RFC 7638 JWK Thumbprint (`keyid`)**:
   - Compute `ring::digest::digest(&ring::digest::SHA256, ...)` over `{"crv":"Ed25519","kty":"OKP","x":"<reencoded_x>"}` where `<reencoded_x>` is `URL_SAFE_NO_PAD.encode(x_bytes)` (ignoring any `kid` property in the input JWK).
   - Encode the 32-byte digest with `URL_SAFE_NO_PAD`.
2. **Covered Components and Parameters** (verified against `research/kitesurf/packages/engine/test/web-bot-auth.test.ts`):
   - `authority`: format via `match url.host()` (`Host::Domain(d) => d.to_ascii_lowercase()`, `Host::Ipv4(ip) => ip.to_string()`, `Host::Ipv6(ip) => format!("[{ip}]")`), appending `:{port}` when `url.port()` is `Some(port)` (`url::Url::port()` automatically omits default ports `443` for `https` and `80` for `http`, and matching `Host::Ipv6` avoids double-bracketing `url.host_str()`).
   - `signature_agent_sf`: RFC 8941 String `"<agent_url>"`.
   - `created`: current UNIX epoch seconds (`u64`); `expires`: `created + 300` (`SIGNATURE_TTL_SECS = 300`).
   - `nonce`: 32 random bytes from `ring::rand::SystemRandom` encoded with `URL_SAFE_NO_PAD`.
   - `sig_params`:
     ```text
     ("@authority" "signature-agent");created=<created>;expires=<expires>;keyid="<thumbprint>";nonce="<nonce>";alg="ed25519";tag="web-bot-auth"
     ```
   - RFC 9421 §2.5 signature base (LF-separated, **no** trailing newline):
     ```text
     "@authority": <authority>
     "signature-agent": "<agent_url>"
     "@signature-params": <sig_params>
     ```
   - Sign with `Ed25519KeyPair::sign` -> 64-byte signature `sig_bytes`.
   - Emitted typed struct `WebBotAuthHeaders { pub signature: String, pub signature_input: String, pub signature_agent: String }`:
     - `Signature`: `sig1=:<STANDARD.encode(sig_bytes)>:` (RFC 8941 Byte Sequence).
     - `Signature-Input`: `sig1=<sig_params>`.
     - `Signature-Agent`: `"<agent_url>"`.
3. **Authority Signature Cache**:
   - In-memory `Mutex<HashMap<String, CachedSignature>>` on `WebBotAuthSigner`, bounded at `MAX_CACHED_AUTHORITIES = 256` entries. Each `CachedSignature` stores `created: u64`, `expires: u64`, `seq: u64`, and `headers: WebBotAuthHeaders`; evict the minimum `seq` entry on overflow.
   - Reuse cached headers only when `now >= cached.created && now < cached.expires.saturating_sub(REFRESH_SKEW_SECS)` (`REFRESH_SKEW_SECS = 30`), protecting against backward system clock steps.
4. **Lazy Resolution & Strict Attachment Guardrails**:
   - **Lazy initialization**: `RuntimeContext::new` does NOT touch Keychain or `credentials.json`. `RuntimeContext::web_bot_auth_headers_for(&url)` resolves and caches `WebBotAuthSigner` lazily inside `OnceCell`/`Mutex<Option<...>>` **only** when `!config.no_web_bot_auth && url.scheme() == "https" && !is_local_or_private_host(host)` (with a test-only `allow_loopback_for_test` flag on `WebBotAuthSigner` for hermetic loopback tests). Consequently, `pleb search`, loopback tests without `PLEB_NO_KEYCHAIN=1`, `--no-web-bot-auth`, and plaintext `http://` fetches pay zero Keychain spawns.
   - **Partial configuration**: When neither `WebBotAuthJwk` nor `WebBotAuthAgent` is configured, signing is disabled (`Ok(None)`). When exactly one of the two is configured (or either is malformed), fail loudly with an actionable error naming the missing `pleb auth set` command rather than silently sending unsigned requests.
   - **Explicit acquisition-callsite signing only**: Attach `RequestHeaderOptions.web_bot_auth` explicitly at web-acquisition callsites in `src/fetch.rs` (`acquire_page`, `try_fetch_raw_source`, `fetch_validated_mirror`, `enrich_document_resources`, `fetch_staged_asset`, `fetch_raw`, and meta-refresh hops). Never attach automatically inside `TransportClient`, keeping DuckDuckGo search (`src/search/providers.rs`), `robots.txt` (`src/policy/robots.rs`), `sitemap.xml` (`src/crawl.rs`), and CDP handshakes unsigned.
5. **Hop-by-Hop Redirect Handling for Signed Requests**:
   - Neither Cronet's `FollowRedirect` nor `reqwest::redirect::Policy::custom` can strip or re-sign custom headers on redirects — automatic following would leak `Signature*` on `https -> http` downgrades and send mismatched `@authority` signatures on cross-authority redirects (`example.com` -> `www.example.com`).
   - Therefore, whenever `opts.web_bot_auth.is_some()`:
     - **Same-origin `https://` redirects**: may follow automatically (same `@authority`).
     - **Cross-authority or `https -> http` redirects**: stop and return a typed redirect hop (`HttpResponse` with `3xx` + `location_header: Option<String>`) to the caller (`TransportClient::get_with_request_options` / `fetch.rs` helper) for up to `10` hops. On each hop, re-run `is_safe_redirect_url`, drop `web_bot_auth` if downgraded to `http://` or private host, or re-sign `web_bot_auth` for the new public `https://` authority before issuing the next hop.
     - In `reqwest` builds (`#[cfg(not(feature = "cronet"))]`), use a dedicated `Policy::none()` client for signed requests to implement the exact same bounded hop loop.

### 2.3 Transport & Daemon IPC Integration (`src/transport/`, `src/daemon/`)

- Add `#[serde(default, skip_serializing_if = "Option::is_none")] pub web_bot_auth: Option<WebBotAuthHeaders>` to `RequestHeaderOptions` (`src/transport/mod.rs`) and `DaemonRequest::HttpGet` (`src/daemon/protocol.rs`), plus `#[serde(default, skip_serializing_if = "Option::is_none")] pub location_header: Option<String>` on `HttpResponse` and `DaemonResponseHeader` for hop-by-hop signed redirect handoff.
- Bump `daemon::PROTOCOL_VERSION` from `3` to `4` (and update hardcoded v3 assertions in `tests/daemon_tests.rs`, `tests/cli_tests.rs`, `skills/pleb/SKILL.md`, and `README-FULL.md`).
- **Defense-in-depth header validation**: Before passing `web_bot_auth` values to Cronet C FFI (`src/transport/cronet.rs`) or `reqwest`, reject any value containing `\r`, `\n`, or `\0` (`TransportErrorKind::Network`).
- Add global CLI flag `--no-web-bot-auth` (env `PLEB_NO_WEB_BOT_AUTH`) on `Cli` / `RuntimeConfig`.

---

## 3. Phase 6A — WebMCP Tool Discovery (`src/extract/webmcp.rs`)

### 3.1 Domain Model

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebMcpToolSource {
    Declarative,
    Imperative,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebMcpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autosubmit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted_content: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebMcpToolDescriptor {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub source: WebMcpToolSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<WebMcpToolAnnotations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}
```

- Add `pub tools: Vec<WebMcpToolDescriptor>` to `ExtractedDocument` (`src/extract/mod.rs`), and `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub tools: Vec<WebMcpToolDescriptor>` to **`FetchOutput`** (`src/fetch.rs`), **`BatchItemSuccess`** (`src/fetch.rs`), and **`FetchMetadataOutput`** (`src/main.rs`) so `--json`, `--jsonl`, and `--metadata-only` all expose `tools` consistently.

### 3.2 Declarative HTML Extraction & Shared Sanitizer (`src/extract/webmcp.rs`)

Aligned with W3C WebMCP and `research/kitesurf/packages/dom-shim/src/model-context.ts`, with explicit defensive hardening documented below:

1. **Parse-order hook and pruning**:
   - Run `ParsedHtml::extract_webmcp_tools(&self, page_url: &Url)` **before** `prune_dom` (since declarative forms often sit inside `<nav>`, `<header>`, `<aside>`, or `[role="search"]`).
   - In `prune_dom` (`src/extract/dom.rs`), detach matched `form[toolname][tooldescription]` nodes during `Clean`/`Article` extraction (after indexing them) so tool form labels/options/buttons do not leak as prose noise into Markdown while leaving ordinary non-WebMCP forms untouched (`ratchet == 1.000`).
2. **Visibility & matching rules** (intentional defensive hardening over Kitesurf's `querySelectorAll`):
   - Match `form[toolname][tooldescription]`, skipping any `<form>` that **itself** or any **ancestor** is `<template>`, `<noscript>`, has `[hidden]`, `[aria-hidden="true"]`, or inline hidden style (`dom::is_inline_hidden_style`, checking `display:none` / `visibility:hidden`).
   - `name`: raw `toolname` attribute (NO whitespace trimming, matching WebMCP / `model-context.ts`); require `1..=128` bytes matching `^[A-Za-z0-9_.\-]+$`.
   - `description`: whitespace-collapsed, ASCII-control-stripped `tooldescription`; require non-empty, safely truncated to `<= 1024` UTF-8 bytes.
   - `title`: optional collapsed, control-stripped `tooltitle` (`<= 256` bytes).
   - `action_url`: per HTML5, when `form[action]` is absent or empty, default to `page_url` (never `<base href>`, preventing `<base>` hijacking); when non-empty, resolve against `page_url` (or `effective_base`), validate via `is_safe_redirect_url(page_url, &resolved)`, reject `https -> http` downgrade, and clear `username`/`password`/`fragment`. Disallowed actions set `action_url = None`.
   - `method`: ASCII-uppercase `form[method]` normalized to `"GET"` (default) or `"POST"`.
   - `annotations`: unconditionally set `untrusted_content: Some(true)`; set `autosubmit: Some(true)` when `toolautosubmit` attribute is present.
3. **JSON Schema synthesis from descendant form controls**:
   - Walk descendant `input`, `select`, and `textarea` elements in document order, skipping elements inside nested `<template>` or with `disabled` (on the control or any ancestor `<fieldset disabled>`).
   - Skip unnamed controls (`name` empty or `> 128` bytes) and non-schema types (ASCII-lowercased `type`: `button`, `submit`, `reset`, `image`, `hidden`, `file`, `password`).
   - Control mappings:
     - `checkbox` -> `{"type": "boolean"}`
     - `number` | `range` -> `{"type": "number"}` plus finite numeric `minimum` / `maximum` when `min` / `max` parse cleanly
     - `radio` -> merge all non-disabled radios sharing `name` into `{"type": "string", "enum": [...]}` (defaulting value to `"on"` **only** when the `value` attribute is absent, not when `value=""`; deduplicate values in document order; preserve the **first** non-empty description across the radio group)
     - `select` -> collect `<option>` descendants skipping any with `option[disabled]` or ancestor `optgroup[disabled]` (`value` attr, fallback collapsed text), deduplicated in document order -> `{"type": "string", "enum": [...]}` (omit `enum` when empty); wrap in `{"type": "array", "items": ...}` when `multiple` is present
     - `email` -> `{"type": "string", "format": "email"}`
     - `textarea` / other `input` types -> `{"type": "string"}`
   - Control description: `toolparamdescription` (collapsed, control-stripped, `<= 512` bytes), falling back to a visible `<label>` whose `for` attribute equals the control's `id` via direct attribute equality (`el.value().attr("for") == Some(id)`, never `Selector::parse` interpolation; `1..=256` bytes).
   - Empty form produces `{"type": "object", "properties": {}}` with `"required"` omitted.
4. **Shared Normalizer & Anti-Flooding Caps (`sanitize_webmcp_tool`)**:
   - Applied identically to **both** HTML-extracted declarative tools and CDP-received imperative tools:
     - `MAX_WEBMCP_TOOLS = 32` per document (first valid entry per `name` wins within the same source).
     - `MAX_SCHEMA_PROPERTIES = 64`, `MAX_ENUM_VALUES = 64` (`<= 256` bytes each), `MAX_SCHEMA_BYTES = 16 * 1024`, `MAX_SCHEMA_DEPTH = 8` (oversized/deep schemas are dropped to `input_schema: None` rather than failing the fetch).
     - Strip all ASCII control characters (`< 0x20`, `0x7f`) from `title` and `description` (preventing ANSI escape injection into terminal stderr cues).
     - Unconditionally force `annotations.untrusted_content = Some(true)` (never trusting a page's `untrustedContentHint: false`).

### 3.3 CLI Policy & Output Surface (`src/policy/mod.rs`, `src/fetch.rs`, `src/main.rs`)

- Add `--tools <none|manifest>` (`#[derive(ValueEnum)] pub enum ToolPolicy`, default `None`) on `pleb fetch` (rejecting `--tools manifest` when combined with `--stage` or `--format raw`).
- `ToolPolicy::None` (default): populates `tools` in `--json` / `--jsonl` / `--metadata-only` and prints one concise stderr cue when `!quiet && !json && !tools.is_empty()`:
  `Tools: 2 WebMCP tools (search_docs, filter_issues) — pass --tools manifest or --json`
- `ToolPolicy::Manifest`: include `options.tools == ToolPolicy::Manifest` in `needs_enrichment` (`src/fetch.rs`) so initial extraction skips premature `--max-bytes` truncation, appends `## Page Tools (WebMCP)` (escaping table cells via `escape_table_cell` and using dynamic backtick fences `longest_backtick_run + 1`, min 3, for JSON schemas to prevent table/fence breakout), and applies `apply_format_limit` once at the end.

---

## 4. Phase 6B — Pluggable CDP / Kitesurf Remote Renderer Transport

### 4.1 Configuration, Host-Scoped Access Auth, and URL Security (`src/auth.rs`, `src/context.rs`, `src/main.rs`)

Add three optional credentials to `CredentialProvider` for Cloudflare Access–protected Kitesurf Workers:

| Variant | CLI / JSON / Store name | Env var | Purpose |
| :--- | :--- | :--- | :--- |
| `CdpAccessClientId` | `cdp-access-client-id` | `PLEB_CDP_ACCESS_CLIENT_ID` | `CF-Access-Client-Id` WebSocket upgrade header |
| `CdpAccessClientSecret` | `cdp-access-client-secret` | `PLEB_CDP_ACCESS_CLIENT_SECRET` | `CF-Access-Client-Secret` WebSocket upgrade header |
| `CdpAccessHost` | `cdp-access-host` | `PLEB_CDP_ACCESS_HOST` | Explicit hostname (or `host:port`) authorized to receive Access tokens |

Global CLI flags / env vars on `Cli` and `RuntimeConfig`:
- `--cdp-url <URL>` (env `PLEB_CDP_URL`): WebSocket CDP endpoint (`ws://127.0.0.1:8787/devtools/browser` or `wss://<worker>/devtools/browser`). Implement manual `Debug` on `RuntimeConfig` so `cdp_url` query strings (`?token=...`) are redacted as `scheme://host[:port]/path`, and sanitize `tungstenite`/`url` errors identically.
- `--cdp-timeout <SECS>` (default `60`): single wall-clock deadline spanning connect, handshake, navigate, wait, settle, and extraction.
- `--prefer-cdp` (env `PLEB_PREFER_CDP`): in `--render auto`, escalate to CDP ahead of local CEF when both are available.

**Endpoint & Access Auth Security Rules**:
1. **`validate_cdp_url`**: Require scheme `ws` or `wss` and non-empty host; reject embedded userinfo and fragments. For plaintext `ws://`, allow loopback hostnames/IPs only (`localhost`, `127.0.0.1`, `[::1]`, `::1`), AND after opening the `TcpStream` assert `stream.peer_addr()?.ip().is_loopback()` before sending the WebSocket upgrade (defeating `/etc/hosts` or DNS spoofing).
2. **Host-scoped Cloudflare Access tokens**: Resolve `CdpAccess*` lazily only when connecting to a `wss://` endpoint. Silently omit Access headers on loopback `ws://`. On `wss://`: if none of the three `CdpAccess*` creds are set, connect without Access headers; if any is set, require all three (`ClientId`, `ClientSecret`, `Host`) to be valid, and attach `CF-Access-Client-Id`/`CF-Access-Client-Secret` **only** when the CDP URL's lowercase `host` (or `host:port`) matches `CdpAccessHost` (otherwise omit them — preventing accidental token exfiltration when overriding `--cdp-url` to another `wss://` server).
3. **Non-101 upgrade errors**: Map `tungstenite::Error::Http(resp)` (e.g., Access 302 login redirect, 401, 403) to a typed `TransportError` reporting only HTTP status code — never logging response headers, `Set-Cookie`, or HTML bodies.
4. **Proxy non-bypass (`--proxy` / `PLEB_PROXY`)**: `CdpRenderer` connects over direct WebSocket and cannot honor local HTTP/SOCKS proxies. When `config.proxy.is_some()`: fail loudly on explicit `--render cdp`, and force `cdp_available = false` in `--render auto` so traffic never silently bypasses `--proxy`.

### 4.2 Routing, Escalation, and Cache Policy (`src/policy/mod.rs`, `src/policy/cache.rs`, `src/fetch.rs`)

Extend `RenderMode`, `PrimaryTransport`, and `EscalationBackend`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum RenderMode {
    #[default]
    Auto,
    #[value(aliases = ["browser", "render", "js"])]
    Cef,
    #[value(aliases = ["kitesurf", "remote"])]
    Cdp,
    #[value(aliases = ["network", "fast", "no-js", "nojs"])]
    Cronet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimaryTransport { Cronet, Cef, Cdp }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationBackend { None, Cef, Cdp }
```

Update `StrategyPlan` (`escalation: EscalationBackend`) and `plan_strategy(render_mode, rule, cached, cef_available, cdp_available, prefer_cdp)`:
- Compute `default_escalation`: if `prefer_cdp && cdp_available` -> `Cdp`; else if `cef_available` -> `Cef`; else if `cdp_available` -> `Cdp`; else `None`.
- `RenderMode::Cdp`: `primary = Cdp`, `escalation = None`, `allow_cronet_fallback = false`. Fails loudly with `FetchError::CdpUnavailable` (exit code `1`, kind `"cdp_unavailable"`) when `cdp_url` is unset or `--proxy` is active.
- `RenderMode::Auto`:
  - `site-rules.json` `"render": "cdp"` -> if `cdp_available` then `primary = Cdp, allow_cronet_fallback = true`, else `primary = Cronet, escalation = default_escalation`.
  - `site-rules.json` `"render": "cef"` -> if `prefer_cdp && cdp_available` -> `primary = Cdp, allow_cronet_fallback = true`; else if `cef_available` -> `primary = Cef, allow_cronet_fallback = true`; else `primary = Cronet, escalation = default_escalation` (so bundled `x.com`/`twitter.com` rules escalate cleanly to CDP on CEF-less hosts!).
  - `site-cache.json` `"cdp"` (when `cdp_available`) -> `primary = Cdp, allow_cronet_fallback = true`; if `!cdp_available`, fall back to `primary = Cronet, escalation = default_escalation`.
- **Source bypass & error mapping (`src/fetch.rs`)**:
  - Update `effective_source_mode` to return `SourceMode::None` when `render == RenderMode::Cdp` OR `format == OutputFormat::Axtree`.
  - Add `"cdp_timeout"` alongside `"cef_timeout"` in `SiteCache::is_expired` (`24h` TTL).
  - Add dedicated `map_cdp_error` so CDP `ProtocolMismatch` maps to `FetchError::CdpUnavailable` (never `CefUnavailable`), and update blocked-without-escalation diagnostics so they don't blame missing CEF when CDP was configured/used.

### 4.3 Async CDP Session State Machine (`src/transport/cdp.rs`)

Add `tokio-tungstenite = { version = "0.26", default-features = false, features = ["rustls-tls-native-roots", "connect"] }` to `Cargo.toml`. Construct `tokio_tungstenite::Connector::Rustls` explicitly with `rustls::crypto::ring::default_provider()` (avoiding `rustls 0.23` dual-CryptoProvider runtime panics).

**Direct execution**: `CdpRenderer` connects directly from the CLI process (no `daemon.sock` hop), keeping Cloudflare Access secrets out of IPC and allowing concurrent batch fetches (`--concurrency N --render cdp`) to open independent ephemeral Kitesurf Worker isolates in parallel.

**Harden `src/policy/source.rs` (`pub fn is_local_or_private_host`, `pub fn is_loopback_host`)**: block `0.0.0.0/8`, `>= 224.0.0.0` (multicast + reserved), IPv4-compatible `v6.to_ipv4()`, NAT64 `64:ff9b::/96`, IPv6 multicast `ff00::/8`, and deprecated site-local `fec0::/10`.

**Per-render lifecycle (`CdpRenderer::render(url, &CdpRenderOptions, timeout) -> Result<CdpRenderResult>`)**:
All steps share one wall-clock `Instant` deadline (`timeout`), plus a bounded `500ms` teardown grace window.

1. **Pre-flight URL & self-SSRF checks**:
   - Reject non-`http`/`https` target URLs.
   - Pre-validate `options.wait_for_selector` syntax locally via `scraper::Selector::parse` -> `TransportErrorKind::InvalidSelector`.
   - Unconditionally reject any target URL whose `host:port` matches the configured CDP server's `host:port` (prevents self-SSRF against unauthenticated local CDP servers).
   - When targeting a remote `wss://` CDP endpoint, reject private/loopback target URLs via `is_local_or_private_host` (note: DNS-rebinding hostnames are mitigated by Cloudflare Workers egress policy on remote Kitesurf, not by client-side string checks on vanilla Chrome CDP).
2. **Connect & Kitesurf fast mode**:
   - Configure `WebSocketConfig` with **both** `max_message_size = Some(MAX_HTML_BYTES)` and `max_frame_size = Some(MAX_HTML_BYTES)` (`64 MiB`).
   - Detect endpoint mode from path (`is_page_endpoint = url.path().starts_with("/devtools/page/")`).
   - When `!options.capture_axtree` and `!is_page_endpoint` and no `layout` query param exists, append `layout=off` to the WebSocket query string (Kitesurf skips Taffy layout + `vello_cpu` paint; standard Chrome `/devtools/browser` ignores unknown query params). Never append `?layout=off` to `/devtools/page/<id>` (Chrome returns HTTP 500); instead send best-effort `Kitesurf.setLayoutMode({ "mode": "off" })` after connect. When `capture_axtree == true`, keep `layout=full` so Kitesurf computes CSS visibility (`display:none` / `visibility:hidden`) accurately in `Accessibility.getFullAXTree`.
3. **Target acquisition & continuous session event reducer**:
   - Start a single multiplexed RPC + event reader loop that processes every incoming WebSocket frame (capped at `MAX_UNSOLICITED_EVENTS = 16_384` / `MAX_EVENT_BYTES = 32 MiB` per render to block event-flood DoS), routing events continuously even while awaiting any JSON-RPC reply ID.
   - If `!is_page_endpoint`: await `Target.createTarget({ "url": "about:blank" })` -> `created_target_id`, then await `Target.attachToTarget({ "targetId": created_target_id, "flatten": true })` -> `session_id`. Record `owns_target = true`.
   - If `is_page_endpoint`: operate directly with `session_id = None`, `owns_target = false`, and send best-effort `Kitesurf.setLayoutMode({ "mode": "off" })` when `!options.capture_axtree` (ignoring `-32601`).
4. **Domain initialization (strictly awaited before `Page.navigate`)**:
   - Because Kitesurf dispatches incoming WebSocket messages concurrently (`connection.ts#L653-703`), sequentially await replies before navigating:
     - `Page.enable`
     - `Network.enable`
     - `Page.getFrameTree` -> capture `main_frame_id` (`result.frameTree.frame.id`) **before** `Page.navigate` (critical because both Kitesurf and Chrome emit `Network.responseReceived` and `Page.frameNavigated` *before* `Page.navigate` replies!).
     - Best-effort `Fetch.enable({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] })` (ignore `-32601` if unsupported): on `Fetch.requestPaused`, validate `request.url` against `is_safe_redirect_url` and CDP self-SSRF; if disallowed, send `Fetch.failRequest({ requestId, errorReason: "BlockedByClient" })` and abort main-frame requests with `TransportErrorKind::RedirectBlocked`; otherwise `Fetch.continueRequest({ requestId })`.
     - Best-effort `WebMCP.enable` (ignore `-32601` on vanilla Chrome).
   - Intentionally omit `DOM.enable` (not required for `DOM.getDocument`/`DOM.querySelector`/`DOM.getOuterHTML`, and avoids Kitesurf's 200ms background DOM mutation streaming).
5. **Navigation and event reduction**:
   - Send `Page.navigate({ "url": target_url })` while the reducer actively filters events by `sessionId` (when attached):
     - `Network.requestWillBeSent` (`frameId == main_frame_id && type == "Document"`): record `main_request_id = requestId` and validate `request.url` (and `redirectResponse.url` if present) against `is_safe_redirect_url` + self-SSRF.
     - `Network.responseReceived` (`frameId == main_frame_id && type == "Document"`): capture `status_code`, ASCII case-insensitive `content-type` (fallback `response.mimeType`), ASCII case-insensitive `link` header (splitting on `"\n"` or `", "`), and validate `response.url`.
     - `Page.frameNavigated` (`frame.id == main_frame_id`): reconstruct `final_url = format!("{}{}", frame.url, frame.url_fragment.unwrap_or(""))` and validate against `is_safe_redirect_url` + self-SSRF.
     - `Page.navigatedWithinDocument` (`frameId == main_frame_id`): update `final_url` for SPA `pushState`/`replaceState`.
     - `Page.loadEventFired`: mark `load_fired = true`.
     - `Network.loadingFailed` (`requestId == main_request_id`): mark main-document load failure -> `TransportErrorKind::LoadError`.
     - `WebMCP.toolsAdded` (`params.tools[]` where `tool.frameId == main_frame_id`): ingest through `sanitize_webmcp_tool`, classifying `backendNodeId.is_some_and(|id| id > 0)` as `WebMcpToolSource::Declarative` and `backendNodeId.is_none()` as `WebMcpToolSource::Imperative`.
     - `WebMCP.toolsRemoved` (`params.tools[]` with `{ name, frameId }` where `frameId == main_frame_id`): remove matching `name` from the tracked CDP tool map.
   - Upon `Page.navigate` reply: if `result.errorText` is non-empty, fail immediately with `TransportErrorKind::LoadError` (or `RedirectBlocked` if private-network blocked) instead of waiting for `Page.loadEventFired`; record `main_request_id.get_or_insert(result.loaderId)`.
6. **Readiness, selector polling, and bounded settle** (continuing to ingest `WebMCP.*` and navigation events):
   - Await `load_fired` (if not already true from Step 5).
   - If `wait_for_selector` is set: poll `DOM.getDocument({ "depth": 0 })` -> `DOM.querySelector({ "nodeId": root_id, "selector": sel })` every `100ms`. Check **both** Kitesurf's in-band `result.error` (non-empty -> `InvalidSelector`) and Chrome's `-32000` `"DOM Error while querying"` (-> `InvalidSelector`), retrying `DOM.getDocument` on transient stale-node errors.
   - Bounded settle: attempt `Kitesurf.getPageState` (passing `{ "targetId": created_target_id }` when `owns_target`, or `{}` in page mode); if supported (numeric `result.seq` and no `result.error`), poll every `125ms` until `seq` is unchanged across two consecutive ticks (capped at `1000ms` total); otherwise wait a single `250ms` settle window.
7. **Typed HTML & optional AXTree extraction**:
   - Call `DOM.getDocument({ "depth": 0 })` -> `DOM.getOuterHTML({ "nodeId": root.nodeId })`, retrying once if `root.nodeId == 0` or a stale-node error occurs. Deserialize into a minimal typed struct (`struct OuterHtmlReply { outer_html: String }`, never generic `serde_json::Value`).
   - Enforce `max_response_bytes` (`ctx.config.max_response_bytes.unwrap_or(DEFAULT_DOCUMENT_MAX_RESPONSE_BYTES)`) on `outer_html.len()` -> `TransportErrorKind::ResponseTooLarge`.
   - If `options.capture_axtree` is true, call `Accessibility.getFullAXTree`, deserialize directly into minimal typed `AxNode` structs, map Kitesurf `-32000` reply-budget errors to `TransportErrorKind::ResponseTooLarge`, and format via `src/extract/axtree.rs`.
8. **Deterministic RAII teardown**:
   - On success, error, or timeout: if `owns_target`, await `Target.closeTarget({ "targetId": created_target_id })` (bounded to `500ms`), then send a WebSocket `Close` frame so Kitesurf immediately retires the per-page isolate and triggers V8 GC. Never call `Browser.close` and never close pre-existing `/devtools/page/<id>` tabs.
9. **WebMCP merge precedence**:
   - Run `pleb`'s HTML extraction + declarative WebMCP scan on `CdpRenderResult.html` (preserving `action_url`, `method`, `title`, `<label for>` descriptions, and hidden-form filtering).
   - Merge CDP-observed tools:
     - **Ignore** CDP entries with `backendNodeId > 0` (`Declarative`) so Kitesurf's unfiltered CDP declarative list neither overwrites richer HTML-extracted descriptors nor re-injects hidden forms that `pleb`'s HTML scanner intentionally excluded.
     - **Insert/override** by `name` for CDP entries with `backendNodeId == None` (`Imperative`), enforcing `MAX_WEBMCP_TOOLS = 32` and `annotations.untrusted_content = Some(true)` after merge.

### 4.4 Accessibility Tree Output (`--format axtree`, `src/extract/axtree.rs`)

- Add `OutputFormat::Axtree` (`#[value(alias = "ax")]`) to `src/extract/mod.rs` (handling exhaustive matches in `src/extract/{mod,native_markdown,feed}.rs`, `src/fetch.rs`, and `src/main.rs`).
- **Validation & routing (`src/main.rs`, `src/fetch.rs`)**:
  - Reject `--format axtree` combined with `--stage`, `--render cronet`, `--render cef`, or unset `--cdp-url` before issuing any network request.
  - When `format == OutputFormat::Axtree` and `render == RenderMode::Auto`, promote to `RenderMode::Cdp` (`allow_cronet_fallback = false`) and extract `title`, `resources`, and declarative+imperative `tools` from `CdpRenderResult.html` while emitting the formatted AXTree as `content` (skipping `htmd`).
- **Compact formatting**:
  - Skip `node.ignored == true`, `"none"`, `"Ignored"`, `"InlineTextBox"`, `"generic"`, and `"GenericContainer"` (promoting children of skipped containers).
  - Deserialize `AXValue.value` as `serde_json::Value` to format tristate strings (`checked`/`pressed`: `"true"`/`"false"`/`"mixed"`), booleans (`disabled`/`required`/`expanded`/`selected`/`focused`), and integers (`level`) uniformly.
  - Escape `"`, `\`, `\r`, `\n` and strip ASCII control bytes (`< 0x20`, `0x7f`) in `role`, `name`, and property values to prevent structure or ANSI injection.
  - Enforce `--max-bytes` via `truncate_utf8`.

---

## 5. Error Mapping and Non-Regression Gates

- **Zero untyped error matching (`untyped_error_matches == 0`)**: All CDP and Web Bot Auth transport failures map through `TransportError` / `TransportErrorKind` and `FetchError::CdpUnavailable` (exit code `1`, kind `"cdp_unavailable"`).
- **Corpus evaluation ratchet (`tests/corpus_evaluation.rs`, `tests/corpus/`)**:
  - Extend `VariantSpec` in `tests/corpus_evaluation.rs` with `expected_tool_names`, `forbidden_tool_names`, and `expected_tool_count`.
  - Add `"frontier"` -> `"ratchet"` fixtures:
    - `frontier_m6_webmcp_declarative_forms.html`: verifies text/email/checkbox/number(min,max)/radio(enum, default `"on"`, first description preserved)/select(multiple, disabled option & optgroup)/`toolparamdescription`/`<label for>`/`required`/`toolautosubmit`, empty-form schema, and clean detachment of `form[toolname][tooldescription]` from Markdown output.
    - `frontier_m6_webmcp_adversarial_forms.html`: verifies invalid/whitespace `toolname`, hidden form & hidden ancestor skipping, `password`/`file`/`hidden` exclusion, `javascript:` and private-IP `action_url` stripping, `<base href>` non-hijack on empty `action`, description/enum/schema caps, ANSI control stripping, duplicate name precedence, and `32`-tool cap.
- **Hermetic loopback tests (`PLEB_NO_KEYCHAIN=1`, `PLEB_NO_HOME_CEF=1`, `/tmp/pleb-t-<pid>-<seq>`)**:
  - `tests/web_bot_auth_tests.rs`: RFC 9421 verification roundtrip with `ring::signature::UnparsedPublicKey(&ED25519, ...)`, RFC 7638 thumbprint ignoring JWK `kid`, pretty-printed multi-line JWK `--stdin` canonicalization, clock-backward cache invalidation, `http://` and loopback refusal, hop-by-hop cross-authority re-signing and `https -> http` header stripping, daemon v4 pass-through + child-process env scrubbing assertion (`spawn_detached_daemon` and `CefRenderer`).
  - `tests/cdp_tests.rs`: local `tokio-tungstenite` mock CDP server verifying browser-mode target creation/teardown vs `/devtools/page/<id>` preservation, `Page.getFrameTree` + pre-reply `Network.responseReceived`/`Page.frameNavigated` reduction, `Page.navigate` `result.errorText` fast-fail, `layout=off` query on `/devtools/browser` vs `Kitesurf.setLayoutMode` on `/devtools/page/<id>`, `Kitesurf.getPageState` with `{ targetId }`, `Fetch.requestPaused` pre-flight SSRF + self-SSRF blocking, `WebMCP.enable` supported (declarative `backendNodeId` non-clobbering + imperative override + `toolsRemoved` `params.tools[*].name`) and unsupported (`-32601`), `--wait-for-selector` Kitesurf in-band `result.error` + Chrome `-32000` + stale-node retry, `Accessibility.getFullAXTree` formatting + escaping + budget error, `max_frame_size`/`max_response_bytes` enforcement, `--proxy` rejection, and host-scoped `CF-Access-*` `wss://` vs loopback `ws://` rules.
- **CLI & Docs Parity**: Update `README.md`, `README-FULL.md`, and `skills/pleb/SKILL.md`, keeping `tests/cli_tests.rs` green.

---

## 6. Swarm Execution Plan (Tier 3 Cross-Seam Wave)

### Wave Header Commit (`conductor` on `main`)
Land one compiling, passing commit on `main` defining every shared type, stub, and cross-slice boundary so parallel `share` worktrees have zero file collisions:
- `Cargo.toml` & `Cargo.lock`: promote `ring = "0.17"`, `base64 = "0.22"`, `futures-util = { version = "0.3", default-features = false, features = ["sink"] }`; add `tokio-tungstenite = { version = "0.26", default-features = false, features = ["rustls-tls-native-roots", "connect"] }`.
- `src/auth.rs` & `src/search/mod.rs`: switch `CredentialProvider` serde to `kebab-case`, add `WebBotAuthJwk`, `WebBotAuthAgent`, `CdpAccessClientId`, `CdpAccessClientSecret`, `CdpAccessHost`, `SEARCH_PROVIDERS`, `is_search_provider`, and `resolve_credentials`; update `src/search/mod.rs` to iterate `SEARCH_PROVIDERS`.
- `src/policy/source.rs` & `src/policy/mod.rs`: export `pub fn is_local_or_private_host` (hardened) and `pub fn is_loopback_host`; add `ToolPolicy`, `RenderMode::Cdp`, `PrimaryTransport::Cdp`, `EscalationBackend`, updated `StrategyPlan` + `plan_strategy` signature; update `tests/policy_tests.rs` callsites.
- `src/transport/mod.rs` & `src/daemon/protocol.rs`: define `WebBotAuthHeaders`, add `web_bot_auth` to `RequestHeaderOptions` and `DaemonRequest::HttpGet`, add `location_header` to `HttpResponse` and `DaemonResponseHeader`, bump `PROTOCOL_VERSION = 4`, add `pub mod web_bot_auth;` and `pub mod cdp;` stubs with frozen public signatures (`WebBotAuthSigner`, `AgentIdentityStatus`, `CdpRenderer`, `CdpRenderOptions`, `CdpRenderResult`); update existing `RequestHeaderOptions` literals and `PROTOCOL_VERSION` assertions (`tests/daemon_tests.rs`, `tests/cli_tests.rs`, `skills/pleb/SKILL.md`, `README-FULL.md`).
- `src/extract/{mod,dom,webmcp,axtree,native_markdown,feed,transcript,youtube}.rs` & `src/crawl.rs`: define `WebMcpToolDescriptor`, `WebMcpToolSource`, `WebMcpToolAnnotations`, `OutputFormat::Axtree`, `ExtractedDocument.tools` (updating all 17 struct literals and `OutputFormat` matches), promote `dom::is_inline_hidden_style` & `dom::collapsed_node_text` to `pub(crate)`, and freeze `ParsedHtml::extract_webmcp_tools`, `webmcp::{sanitize_webmcp_tool, merge_webmcp_tools, format_webmcp_manifest_markdown, format_webmcp_cue}`, and `axtree::format_axtree_from_cdp_json`.
- `src/fetch.rs` & `src/main.rs`: add `FetchError::CdpUnavailable`, `FetchOptions.tools`, `tools` on `FetchOutput`, `BatchItemSuccess`, and `FetchMetadataOutput`.
- Verify `cargo test --features cronet` compiles and passes on `main`.

### Parallel Slices (`workspace: "share"` — strictly disjoint file ownership)
1. **`corpus-verifier`**: owns `tests/corpus/*`, `tests/corpus_evaluation.rs`.
   - Extends `VariantSpec` for `doc.tools`, adds both declarative WebMCP `"frontier"` fixtures, verifies all security/ratchet invariants (`ratchet == 1.000`, `untyped_error_matches == 0`), and promotes `"frontier"` -> `"ratchet"`.
2. **`auth-and-wba`**: owns `src/auth.rs`, `src/transport/web_bot_auth.rs`, `src/transport/{mod,cronet,cef}.rs`, `src/daemon/{protocol,client,server}.rs`, `tests/web_bot_auth_tests.rs`, `tests/auth_tests.rs`, `tests/daemon_tests.rs`.
   - Implements JWK/agent/CDP-Access validation & canonicalization, RFC 9421 Ed25519 signer + authority cache, hop-by-hop signed redirect handling across Cronet/reqwest/daemon v4, child-process env scrubbing (`spawn_detached_daemon` + `CefRenderer`), and full test suites.
3. **`webmcp-and-axtree`**: owns `src/extract/{mod,dom,webmcp,axtree}.rs`, `tests/extract_tests.rs`.
   - Implements single-pass declarative WebMCP scanner, shared `sanitize_webmcp_tool` normalizer, `prune_dom` detachment of `form[toolname][tooldescription]`, Markdown tool manifest + cue formatters, and compact hardened `AXTree` formatter.
4. **`cdp-transport-and-cli`**: owns `src/transport/cdp.rs`, `src/policy/{mod,cache}.rs`, `src/context.rs`, `src/fetch.rs`, `src/main.rs`, `README.md`, `README-FULL.md`, `skills/pleb/SKILL.md`, `tests/cdp_tests.rs`, `tests/fetch_tests.rs`, `tests/cli_tests.rs`, `tests/policy_tests.rs`.
   - Implements async WebSocket CDP / Kitesurf renderer (with `Fetch.enable` SSRF guard, `Page.getFrameTree` pre-capture, `layout=off` / `Kitesurf.setLayoutMode`, `Kitesurf.getPageState`, `WebMCP.enable` merge, `rustls` `ring` provider), `--no-web-bot-auth`, `--tools`, `--render cdp`, `--cdp-url`, `--cdp-timeout`, `--prefer-cdp`, `--format axtree`, `AuthStatusJson` / `pleb status` reporting, and docs parity.

### Wave Barrier Subtraction Pass & Host Sync
- Run one closing whole-repo Subtraction Pass (`ratchet == 1.000`, net negative lines), including replacing the 73-line hand-rolled `sha256()` in `src/search/state.rs` with `ring::digest::digest(&ring::digest::SHA256, ...)` and deduplicating shared validation/header helpers.
- Rebase linearly onto `main`, verify `cargo fmt --check`, `cargo clippy --all-targets --features cronet -- -D warnings`, and `cargo test --features cronet`, push `main`, and run `cargo install --path . --features cronet --locked`.
