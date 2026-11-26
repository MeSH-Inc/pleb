# Milestone 5 Specification: Tool-Owned Credential Storage (`wick auth`) and Multi-Provider Search Balancing

## 1. Purpose and Scope

`wick search` currently supports only `BRAVE_API_KEY` (Brave Search API) with DuckDuckGo HTML fallback. Relying on a single API key loaded via `~/.zshrc` `security find-generic-password` forks introduces four structural limitations:
1. **Burst and quota fragility**: Brave's free tier enforces `1 req/sec` and a monthly cap; concurrent agent or swarm searches trigger `429` and fall straight back to DuckDuckGo HTML throttling.
2. **Index blind spots**: Brave and DuckDuckGo (Bing-backed) lack Google index recall (strongest for exact compiler errors, stack traces, and niche GitHub issues) and neural/semantic discovery (Exa).
3. **Shell-startup coupling and blast radius**: Exporting every API key into `~/.zshrc` adds shell startup latency, fails in non-interactive GUI/LaunchAgent contexts, and exposes unrelated keys to every subprocess.
4. **Non-portable onboarding**: Manual `security add-generic-password` + `~/.zshrc` editing does not transfer cleanly to headless Linux servers or fresh installs.

This specification defines:
1. **Canonical 3-tier credential storage (`wick auth`)**: lazy `env -> macOS Keychain -> 0o600 credentials.json` resolution with zero `~/.zshrc` edits, zero `argv` secret exposure, and hermetic test isolation (`WICK_NO_KEYCHAIN=1`).
2. **Multi-provider search backends (`brave`, `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`, `federated`)**: uniform `SearchResponse` / `SearchResult` output, uniform `--freshness`, `--site`, and `--extra-snippets` mapping, and zero new date/HTTP dependencies.
3. **Health-aware cooldown, atomic burst pacing, and intent routing (`<state_dir>/search-state.json`)**: `spawn_blocking` + `flock`-guarded two-phase dispatch/outcome transitions, `< 1100ms` Brave burst spillover, strict lexical vs. general vs. semantic routing, and deterministic Reciprocal Rank Fusion (`--provider federated`).

---

## 2. Tool-Owned Credential Storage (`src/auth.rs`, `wick auth`)

### 2.1 Credential Identifiers, Validation, and Precedence

Supported search provider credentials (`CredentialProvider`, `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]`):
- `brave` -> env `BRAVE_API_KEY`
- `tavily` -> env `TAVILY_API_KEY`
- `serper` -> env `SERPER_API_KEY`
- `exa` -> env `EXA_API_KEY`
- `searxng` -> env `SEARXNG_URL` (base URL such as `http://127.0.0.1:8888`)

**Secret & URL validation**:
- Bound `--stdin` and TTY prompt reads to `4096` bytes maximum. Trim leading/trailing ASCII whitespace (`\r`, `\n`, spaces), reject empty strings, and reject any byte where `!b.is_ascii() || b.is_ascii_control()` (printable ASCII only — prevents HTTP header injection and ensures macOS `security find-generic-password -w` always prints raw text rather than hex).
- For `CredentialProvider::Searxng`, validate at both `wick auth set searxng` and `resolve_credential`: parse with `url::Url::parse`, require `scheme == "http" || scheme == "https"` and `host_str().is_some()`, reject embedded userinfo (`!url.username().is_empty() || url.password().is_some()`), and reject `url.query().is_some() || url.fragment().is_some()`. Error messages never echo the rejected secret or URL.
- Wrap all resolved secrets in a `SecretString` newtype (manual `Debug` printing `"[REDACTED]"`, no `Display`, zeroizing its backing bytes on `Drop`) and implement manual `Debug` on `ResolvedCredential`.

**Strict resolution precedence (`resolve_credential(state_dir, provider) -> Result<Option<ResolvedCredential>>`)**:
1. **Environment variable (`CredentialSource::Environment`)**: validated non-empty value of the provider's canonical env var. Preserves 100% compatibility with CI, containers, ephemeral overrides, and existing shell exports.
2. **macOS Native Keychain (`CredentialSource::Keychain`, `#[cfg(target_os = "macos")]` only)**: skipped when `WICK_NO_KEYCHAIN=1` is set (mandatory in all tests).
   - **Write (zero `argv` exposure)**: invoke `/usr/bin/security -q -i` with `Stdio::piped()` stdin and write `add-generic-password -U -a <provider> -s mesh-wick -X <lowercase_hex>\n`, flush, drop stdin (EOF), check non-zero exit status, and zeroize the stdin buffer. Hex encoding prevents both `argv` leakage (`ps`/`KERN_PROCARGS2`) and `security -i` tokenizer injection while keeping Apple-signed `/usr/bin/security` as the trusted app (zero CDHash GUI prompt churn across `cargo install`).
   - **Read / Delete**: `/usr/bin/security find-generic-password -a <provider> -s mesh-wick -w` and `/usr/bin/security delete-generic-password -a <provider> -s mesh-wick`. Treat only macOS `errSecItemNotFound` (exit code `44`) as absent; surface locked-keychain or permission errors loudly.
   - On non-macOS targets, `Auto` uses Tier 3 (`credentials.json`) directly and explicit `--storage keychain` fails loudly.
3. **Private Credentials File (`CredentialSource::File`)**: `<state_dir>/credentials.json` inside `0o700` `state_dir`:
   - Add `[target.'cfg(unix)'.dependencies] libc = "0.2"` (already in `Cargo.lock` — zero new transitive crates).
   - Harden `ensure_private_dir` in `src/context.rs`: check `symlink_metadata`, reject if `is_symlink()`, `!is_dir()`, or `meta.uid() != unsafe { libc::geteuid() }`, and enforce `0o700`.
   - Share one `acquire_private_lock(lock_path: &Path) -> std::io::Result<std::fs::File>` helper in `src/context.rs` (`O_NOFOLLOW`, `0o600`, `File::lock()`) across `clear_state_dir`, `auth.rs`, `search/state.rs`, and `policy/cache.rs`.
   - Open `credentials.json` with `custom_flags(libc::O_NOFOLLOW)` under `credentials.lock`; inspect via `file.metadata()` (`fstat` on the open fd, zero TOCTOU) verifying `is_file()`, `uid == geteuid()`, and `mode & 0o077 == 0`. Fail loudly on `mode & 0o077 != 0` or JSON parse errors (never silently overwrite corrupted credential files).
   - Write flat JSON map `{"brave": "...", "tavily": "..."}` via atomic temp file with `create_new(true)` (`O_EXCL`), `O_NOFOLLOW`, `mode(0o600)`, explicit `set_permissions(0o600)` (immune to `umask`), `file.sync_all()`, atomic `rename`, and zeroize the serialized JSON buffer after writing.

**`wick session clear` allowlist invariant**:
- `clear_state_dir` deletes strictly an explicit allowlist: `data/`, `cef-cache/`, `artifacts/`, `site-cache.json` (under `site-cache.lock`), and `search-state.json` (via `search::clear_search_state(state_dir)` under `search-state.lock`).
- `clear_state_dir` **never deletes** `credentials.json`, `credentials.lock`, `site-rules.json`, or Keychain items.

### 2.2 Secret Hygiene and CLI Contract (`wick auth`)

```bash
wick auth set <brave|tavily|serper|exa|searxng> [--stdin] [--storage <auto|keychain|file>]
wick auth remove <brave|tavily|serper|exa|searxng> [--storage <auto|keychain|file>]
wick auth status
```

- Note: `--json` is already `global = true` on `Cli` (`src/main.rs`) and is inherited automatically by `wick auth status` (do not redeclare `--json` on `AuthAction::Status`). Secrets are never accepted as positional CLI arguments.
- **`wick auth set <provider>`**:
  - Without `--stdin`: requires `std::io::stdin().is_terminal()`. Opens `/dev/tty` (`O_RDWR | O_NOCTTY`), saves `termios` via `tcgetattr`, disables `ECHO | ECHONL` (preserving `ICANON | ISIG`) in an RAII `EchoGuard`, and temporarily installs `sigaction` for `SIGINT` / `SIGTERM` **without `SA_RESTART`** setting an `AtomicBool`. On `Ctrl-C`, `read()` returns `EINTR`, `EchoGuard::drop` restores terminal echo (`TCSAFLUSH`) + previous signal handlers and prints `\n` to stderr, zeroizes the partial buffer, and exits with code `130`.
  - With `--stdin`: reads bounded (`<= 4096` bytes) single-line secret from stdin (composable with `op read ... | wick auth set tavily --stdin`).
  - `--storage auto` (default): writes to macOS Keychain when on macOS and `WICK_NO_KEYCHAIN != 1`, otherwise `<state_dir>/credentials.json` (`0o600`). Explicit `--storage keychain` fails loudly when `WICK_NO_KEYCHAIN=1` or on non-macOS.
  - On success, calls `search::clear_provider_cooldown(state_dir, provider)` so rotating a key immediately clears any stale 24h cooldown.
- **`wick auth remove <provider>`**:
  - `--storage auto` (default): removes from all writable stores (Keychain when enabled + `credentials.json`) idempotently, warns on stderr if the provider's env var is still set, and calls `search::clear_provider_cooldown(state_dir, provider)`.
- **`wick auth status` & `wick status --json`**:
  - Share a single `SearchProviderStatus` struct (`configured: bool`, `source: Option<CredentialSource>`, `cooldown_remaining_secs: u64`, `month_requests: u32`, `last_error_status: Option<u16>`) across `wick auth status --json` and `wick status --json` (`search_providers` map), deleting the legacy env-only `brave_api_key_configured` boolean and `load_brave_api_key()`. Never print secret values, URLs, prefixes, or lengths.
- **Zero-leakage error & logging rules**:
  - Map every `reqwest::Error` with `err.without_url()`. For HTTP `>= 400`, construct sanitized errors from `provider` + `status.as_u16()` without calling `.error_for_status()?` or embedding raw response bodies or URLs.
  - Scope `init_tracing(cli.verbose)` in `src/main.rs` to `mesh_wick=debug` with `reqwest=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn` defaults.
  - Never transmit API credentials over `daemon.sock` or `cef-renderer` IPC.

---

## 3. Multi-Provider Search Architecture (`src/search/`)

Decompose `src/search.rs` into a 3-file module (`src/search/{mod,providers,state}.rs`), keeping RRF fusion directly in `src/search/mod.rs`.

### 3.1 `SearchProvider`, `SearchMode`, and `ProviderEndpoints`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SearchProvider {
    #[default]
    Auto,
    Brave,
    Tavily,
    Serper,
    Exa,
    Searxng,
    #[value(name = "duckduckgo", alias = "ddg")]
    DuckDuckGo,
    Federated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SearchMode {
    #[default]
    Auto,
    Exact,
    Semantic,
}
```

Expose `pub struct ProviderEndpoints` (defaulting to production base URLs) and `pub async fn search_with_endpoints(ctx, query, options, &endpoints)` in `src/search/mod.rs` so `tests/search_tests.rs` can test every provider against `127.0.0.1:0` loopback servers without modifying `RuntimeConfig` or `SearchOptions`.

### 3.2 Provider Endpoints, Request/Response Schemas, and Uniform Flag Mapping

All JSON providers reuse `RuntimeContext::http_client()` (renamed from `brave_client()`, configured with a default `User-Agent` and `ctx.config.timeout`) through one shared `execute_provider_request` helper in `src/search/providers.rs`:

| Provider | Endpoint & Auth | Request Payload / Query Params | Response Mapping & `--extra-snippets` |
| :--- | :--- | :--- | :--- |
| **Brave** | `GET https://api.search.brave.com/res/v1/web/search` (`X-Subscription-Token`) | `q=effective_query`, `count=limit`, `freshness=pd\|pw\|pm\|py`, `extra_snippets=true` | `web.results[].{ title, url, description -> snippet, extra_snippets }` |
| **Tavily** | `POST https://api.tavily.com/search` (`Authorization: Bearer <key>`) | `{"query", "max_results": limit, "search_depth": "advanced"\|"basic", "chunks_per_source"?: 3, "time_range"?: "day"\|"week"\|"month"\|"year", "include_domains"?: [host_only]}` (if `--site` has a `/path`, pass host-only in `include_domains` and retain `site:<domain/path>` in `query`) | `results[].{ title, url, content }`; if `extra_snippets` is true and `content` contains `" [...] "` chunk separators, first chunk -> `snippet`, remaining -> `extra_snippets` |
| **Serper** | `POST https://google.serper.dev/search` (`X-API-KEY`) | `{"q": effective_query, "num": limit, "tbs"?: "qdr:d"\|"qdr:w"\|"qdr:m"\|"qdr:y"}` | `organic[].{ title, link -> url, #[serde(default)] snippet, #[serde(default)] sitelinks }`; when `extra_snippets` is true, format each sitelink as `"{title}: {snippet}"` (or `"{title} ({link})"` when `snippet` is absent) |
| **Exa** | `POST https://api.exa.ai/search` (`x-api-key`) | `{"query", "numResults": limit, "contents": {"highlights": {"maxCharacters": 400 \| 1200}}, "startPublishedDate"?: <RFC 3339 UTC>, "includeDomains"?: [host_only]}` (always send `contents.highlights`, never deprecated `numSentences`; retain `site:<domain/path>` in `query` when path present) | `results[].{ #[serde(default)] title, url, #[serde(default)] highlights, #[serde(default)] text }`; `highlights.first()` (fallback `text`) -> `snippet`; `highlights[1..]` -> `extra_snippets` only when `--extra-snippets` is true |
| **SearXNG** | `GET <SEARXNG_URL>/search?format=json&q=...&pageno=1` (trim trailing `/` and avoid `/search/search`) | `q=effective_query`, `format=json`, `pageno=1`, optional `time_range=day\|week\|month\|year` | `results[].{ title, url, #[serde(default)] content -> snippet }`, sliced `.take(limit)` client-side |
| **DuckDuckGo** | `GET https://html.duckduckgo.com/html/?q=...` (via `TransportClient`) | `q=effective_query`, optional `df=d\|w\|m\|y` | Existing HTML parser; treat HTTP `202` or challenge/anomaly markers (`anomaly-modal`, `challenge-form`, `bots use DuckDuckGo` with empty results) as a rate-limit error (`15s` cooldown) rather than `Ok(vec![])` |

- **Pure-`std` UTC date/time math**: implement Howard Hinnant's `civil_from_days` and `days_from_civil` (~25 lines over `UNIX_EPOCH` seconds, zero new crates) for `month_key` (`YYYY-MM`), Exa `startPublishedDate` (`Day=86_400`, `Week=7*86_400`, `Month=30*86_400`, `Year=365*86_400`), and RFC 9110 IMF-fixdate `Retry-After` parsing.
- Every provider normalizes results into `SearchResult { title, url, snippet, extra_snippets }` with HTML tags/entities stripped and whitespace collapsed.

### 3.3 Atomic Cooldown, Burst Pacing, and Usage State (`src/search/state.rs`)

Persisted at `<state_dir>/search-state.json` (`0o600`, `flock`-guarded via `acquire_private_lock(&state_dir.join("search-state.lock"))`, executed inside `tokio::task::spawn_blocking`, wiped by `wick session clear`):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderHealthEntry {
    pub last_used_epoch_ms: u64,
    pub cooldown_until_epoch_secs: u64,
    pub month_key: String, // "YYYY-MM" UTC
    pub month_requests: u32,
    pub last_error_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>, // truncated non-reversible hash of resolved secret, never logged
}
```

- **Two-phase atomic transitions** (disk `search-state.json` is the sole source of truth, eliminating cross-process TOCTOU races):
  1. `reserve_dispatch(state_dir, ordered_candidates, fingerprints, now_ms)` **before** HTTP dispatch: atomically filters providers with active cooldowns (ignoring a cooldown whenever the current `key_fingerprint` differs from the stored `key_fingerprint`), guards clock rollback (`last_used_epoch_ms > now_ms`), applies `< 1100ms` Brave burst demotion uniformly across all `Auto` modes, and immediately writes `last_used_epoch_ms = slot_ms` + `YYYY-MM` rollover + `month_requests += 1`. If Brave is the only healthy configured provider within `< 1100ms`, reserves `slot_ms = max(now_ms, prev + 1100)` (capped at `min(timeout, 2500ms)`) and returns `wait_ms` for the caller to sleep before HTTP dispatch.
  2. `record_outcome(state_dir, provider, fingerprint, outcome, now_secs)` **after** HTTP response: reloads under lock and mutates **only** that provider's entry (`2xx` clears `cooldown_until_epoch_secs = 0` and `last_error_status = None`; failure writes cooldown + `last_error_status` + `key_fingerprint`).
- **Cooldown durations on failure** (inspected via `resp.status()` and `RETRY_AFTER` before reading body):
  - HTTP `429` (or DuckDuckGo `202`/challenge): parse `Retry-After` as integer seconds or IMF-fixdate, clamp to `5..=300`s (default `15`s).
  - HTTP `401 | 402 | 403 | 432 | 433` on keyed JSON providers with structured JSON error response: `86_400`s (24 hours). Downgrade non-JSON/HTML WAF `403` and keyless SearXNG `401`/`403` (noting `format=json` disabled) to `300`s (5 minutes).
  - HTTP `500..=599` or network/timeout error: `60`s.
  - HTTP `400 | 404 | 422`: fail request without poisoning provider cooldown.
- **Explicit provider overrides**: calling `--provider brave` (or `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`) bypasses cooldowns and burst pacing, clears any stale cooldown on `2xx`, and fails loudly if its credential is missing or the request fails.

### 3.4 Deterministic Intent Routing (`SearchProvider::Auto`)

Pure, zero-network classification of raw `query` (never `effective_query` with `site:`) and `options`:
- **Lexical needle detector** (strict boundaries — `--site` alone does **not** force `Exact`):
  - Non-empty quoted phrase `"[^"\s][^"]*"`
  - Path/namespace tokens excluding dates (`2026/11`), ratios (`50/50`), and common prose pairs (`pros/cons`, `and/or`, `ci/cd`, `true/false`, `yes/no`, `read/write`, `input/output`)
  - Strict ASCII word boundaries for `[A-Za-z0-9_]::[A-Za-z0-9_]`, `\bE[0-9]{4}\b`, `\bERR_[A-Z0-9_]{2,}\b`, or `\b0x[0-9a-fA-F]{2,}\b` (rejecting `20x20`).
- **Candidate preference order** (applying `< 1100ms` Brave burst demotion uniformly across all `Auto` modes):
  1. `SearchMode::Semantic`: `Exa -> Tavily -> Brave -> Serper -> Searxng -> DuckDuckGo`.
  2. `SearchMode::Exact` OR (`SearchMode::Auto` with lexical needle): `Searxng -> Serper -> Brave -> Tavily -> Exa -> DuckDuckGo`.
  3. `SearchMode::Auto` with `--extra-snippets`: `Brave -> Tavily -> Exa -> Serper -> Searxng -> DuckDuckGo`.
  4. `SearchMode::Auto` general query: `Searxng -> Brave -> Tavily -> Serper -> Exa -> DuckDuckGo`.
- Track `Vec<ProviderAttempt>` across the `Auto` chain; if every candidate fails or is skipped due to cooldown, fail loudly enumerating each configured/attempted provider's cooldown remaining or failure status.

### 3.5 Reciprocal Rank Fusion (`SearchProvider::Federated`)

When `--provider federated` is requested:
- Select up to `3` configured, non-cooled-down orthogonal providers (preferring distinct index families: `Brave` + `Serper` + `Exa`, preferring `Tavily` ahead of `Brave` if Brave is within its `< 1100ms` burst window, filling from `Tavily` / `Searxng` / `DuckDuckGo`; if 0 API/SearXNG credentials are configured, degrade cleanly to `[DuckDuckGo]` unless DuckDuckGo is in cooldown).
- Request `limit.max(10).min(20)` from each selected provider concurrently via `tokio::task::JoinSet`, tolerating partial provider failures (recording cooldowns on failed providers; failing only if all selected providers fail).
- Deduplicate normalized URLs **within each provider first** using `Url::parse(u).ok().map(|u| normalize_crawl_url(&u)).unwrap_or_else(|| u.to_string())` (keeping best 1-indexed rank `rank = idx + 1`), then score each unique URL across providers via:
  $$\text{score}(u) = \sum_{p \in \text{providers}} \frac{1}{60 + \text{rank}_p(u)}$$
- Break ties deterministically by `(score desc, provider_hit_count desc, min_rank asc, first_provider_slot asc, canonical_url asc)`.
- Merge duplicate hits by keeping the best non-empty `title` and richest non-empty `snippet`, appending distinct secondary extras (and distinct secondary snippets when `--extra-snippets` is enabled) into `extra_snippets` capped at `MAX_EXTRA_SNIPPETS = 5`.
- Reorder `providers_used` back to selected-provider slot order (excluding failed providers) and return top `limit` results with `provider: "federated"` and `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub providers_used: Vec<&'static str>` on `SearchResponse`.

---

## 4. Swarm Execution Plan (Single Wave + Subtraction Barrier)

1. **Wave Header Commit (`herald` on `main`)**:
   - Add `[target.'cfg(unix)'.dependencies] libc = "0.2"` to `Cargo.toml`.
   - In `src/context.rs`: rename `brave_client` -> `http_client` (with default `User-Agent` and `ctx.config.timeout`) and add `acquire_private_lock`.
   - In `src/lib.rs` + `src/auth.rs`: stub `SecretString`, `CredentialProvider`, `CredentialSource` (`#[serde(rename_all = "snake_case")]`), `CredentialStorage`, `CredentialStatus`, `ResolvedCredential`, `resolve_credential`, `set_credential`, `remove_credential`, and `all_credential_statuses`.
   - Run `git mv src/search.rs src/search/mod.rs` + add stub `src/search/state.rs` (`ProviderHealthEntry`, `provider_health_summary`, `clear_search_state`, `clear_provider_cooldown`), expand `SearchProvider` / `SearchMode` / `SearchOptions` (`pub mode: SearchMode`) / `SearchResponse` (`pub providers_used: Vec<&'static str>`), add `SearchProviderStatus`, `ProviderEndpoints`, and `search_with_endpoints(...)`.
   - Delete `search::brave_api_key_configured()` / `load_brave_api_key()`, replace `StatusJson.brave_api_key_configured` with `search_providers` in `src/main.rs`, update struct literals in `src/main.rs` and `tests/search_tests.rs`, and add `.env("WICK_NO_KEYCHAIN", "1")` to existing search/status subprocess tests in `tests/cli_tests.rs`.
   - Verify `cargo fmt --check`, `cargo clippy --all-targets --features cronet -- -D warnings`, and `cargo test --features cronet` pass cleanly on `main`.
2. **Parallel Author Slices (`workspace: "share"`, 100% file-disjoint)**:
   - `auth-store`: owns `src/auth.rs`, `src/context.rs`, and `tests/auth_tests.rs` (3-tier precedence, `/usr/bin/security -q -i` hex write, `O_NOFOLLOW`/`fstat`/`0o077`/symlink hardening, `SEARXNG_URL` validation, `Ctrl-C` `EchoGuard` + `SecretString`, `clear_state_dir` allowlist preservation of `credentials.json` + `credentials.lock`).
   - `search-backends`: owns `src/search/{mod,providers,state}.rs` and `tests/search_tests.rs` (`tavily`, `serper`, `exa`, `searxng`, DDG challenge detection, pure-`std` UTC + `Retry-After` IMF-fixdate, two-phase `spawn_blocking` `reserve_dispatch`/`record_outcome` + `key_fingerprint` cooldown invalidation + `< 1100ms` burst pacing, strict lexical routing, and deterministic RRF `federated` merging).
   - `cli-docs`: owns `src/main.rs`, `README.md`, `skills/mesh-wick/SKILL.md`, and `tests/cli_tests.rs` (`wick auth set/remove/status`, `wick search --mode`, `wick status --json` `search_providers`, scoped `init_tracing`, and mechanical parity tests across `README.md`, `SKILL.md`, and `wick skill print`).
3. **Sequential Adversarial Verification (`verifier` via `git checkout --detach <head_sha>`)**:
   - Reviews each author handoff sequentially and commits adversarial regression tests strictly in `tests/m5_adversarial_tests.rs` (zero file collision with author test files), covering `Retry-After` integer + HTTP-date clamping, `0o644`/symlink credential rejection, `session clear` credential + lock preservation, concurrent Brave burst spillover, federated partial-failure + deterministic tie-breaking, and zero secret leakage in `--json` / `--verbose` logs.
4. **Wave Barrier Subtraction Pass & Host Sync (`search-backends` -> `verifier` -> `herald`)**:
   - Once all author slices land on `main`, `search-backends` runs one closing Whole-Repo Subtraction Pass (`ratchet == 1.000`, `untyped_error_matches == 0`, net negative lines), `verifier` validates the reduced tree, and `herald` fast-forwards `main`, pushes `origin/main`, installs host `wick` (`cargo install --path . --features cronet --locked`), and live-smoke-tests `wick status --json`, `wick search --provider tavily`, `wick search --provider serper`, `wick search --provider exa`, and `wick search --provider federated`.
