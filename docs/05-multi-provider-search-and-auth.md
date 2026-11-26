# Milestone 5 Specification: Tool-Owned Credential Storage (`wick auth`) and Multi-Provider Search Balancing

## 1. Purpose and Scope

`wick search` currently supports only `BRAVE_API_KEY` (Brave Search API) with DuckDuckGo HTML fallback. Relying on a single API key loaded via `~/.zshrc` `security find-generic-password` forks introduces four structural limitations:
1. **Burst and quota fragility**: Brave's free tier enforces `1 req/sec` and a monthly cap; concurrent agent or swarm searches trigger `429` and fall straight back to DuckDuckGo HTML throttling.
2. **Index blind spots**: Brave and DuckDuckGo (Bing-backed) lack Google index recall (strongest for exact compiler errors, stack traces, and niche GitHub issues) and neural/semantic discovery (Exa).
3. **Shell-startup coupling and blast radius**: Exporting every API key into `~/.zshrc` adds shell startup latency, fails in non-interactive GUI/LaunchAgent contexts, and exposes unrelated keys to every subprocess.
4. **Non-portable onboarding**: Manual `security add-generic-password` + `~/.zshrc` editing does not transfer cleanly to headless Linux servers or fresh installs.

This specification defines:
1. **Canonical 3-tier credential storage (`wick auth`)**: lazy `env -> OS keychain -> 0o600 credentials.json` resolution with zero `~/.zshrc` edits and hermetic test isolation (`WICK_NO_KEYCHAIN=1`).
2. **Multi-provider search backends (`brave`, `tavily`, `serper`, `exa`, `searxng`, `duckduckgo`, `federated`)**: uniform `SearchResponse` / `SearchResult` output and uniform `--freshness`, `--site`, and `--extra-snippets` mapping.
3. **Health-aware cooldown, burst pacing, and intent routing (`<state_dir>/search-state.json`)**: `flock`-guarded per-provider rate-limit cooldowns, `< 1100ms` Brave burst spillover, deterministic lexical vs. general vs. semantic routing, and Reciprocal Rank Fusion (`--provider federated`).

---

## 2. Tool-Owned Credential Storage (`src/auth.rs`, `wick auth`)

### 2.1 Credential Identifiers and Precedence

Supported search provider credentials (`CredentialProvider`, `#[derive(ValueEnum)]`):
- `brave` -> env `BRAVE_API_KEY`
- `tavily` -> env `TAVILY_API_KEY`
- `serper` -> env `SERPER_API_KEY`
- `exa` -> env `EXA_API_KEY`
- `searxng` -> env `SEARXNG_URL` (base URL such as `http://127.0.0.1:8888`, never logged with userinfo)

Strict resolution precedence (`resolve_credential(state_dir, provider) -> Option<ResolvedCredential>`):
1. **Environment variable (`CredentialSource::Environment`)**: trimmed non-empty value of the provider's canonical env var. Preserves 100% compatibility with CI, containers, ephemeral overrides, and existing `~/.zshrc` exports.
2. **OS Native Keychain (`CredentialSource::Keychain`)**: skipped when `WICK_NO_KEYCHAIN=1` is set (mandatory in all tests).
   - On macOS: `security find-generic-password -a <provider> -s mesh-wick -w` (stored via `security add-generic-password -U -a <provider> -s mesh-wick -w <secret>`, deleted via `security delete-generic-password -a <provider> -s mesh-wick`). Using a dedicated `mesh-wick` service namespace avoids colliding with arbitrary user Keychain entries and requires zero new native crate dependencies.
   - On Linux: falls back cleanly to Tier 3 (`credentials.json`) when no secret-tool/keyring backend is available.
3. **Private Credentials File (`CredentialSource::File`)**: `<state_dir>/credentials.json` inside `0o700` `state_dir`:
   - Stored as a flat JSON map `{"brave": "...", "tavily": "..."}` written atomically at mode `0o600` under `credentials.lock` (`File::lock()`).
   - On Unix, if `<state_dir>/credentials.json` exists with group or world permissions (`mode & 0o077 != 0`), fail loudly with a permission-remediation message instead of silently reading a world-readable secret file.
   - **`wick session clear` invariant**: `clear_state_dir` clears ephemeral caches (`data/`, `cef-cache/`, `artifacts/`, `site-cache.json`, `search-state.json`) and **never deletes `credentials.json` or Keychain entries**.

### 2.2 CLI Contract (`wick auth`)

```bash
wick auth set <brave|tavily|serper|exa|searxng> [--stdin] [--storage <auto|keychain|file>]
wick auth remove <brave|tavily|serper|exa|searxng> [--storage <auto|keychain|file>]
wick auth status [--json]
```

- `wick auth set <provider>`:
  - Without `--stdin`: prompts interactively on stderr with terminal echo disabled (`termios` / hidden read, never echoing the key to terminal or shell history). Rejects non-TTY stdin unless `--stdin` is passed explicitly.
  - With `--stdin`: reads trimmed single-line secret from stdin (composable with `op read ... | wick auth set tavily --stdin`).
  - `--storage auto` (default): writes to macOS Keychain when available and `WICK_NO_KEYCHAIN != 1`, otherwise `<state_dir>/credentials.json` (`0o600`).
- `wick auth status`: displays each provider's `configured: bool` and `source: "environment" | "keychain" | "file" | null` without ever printing secret values. `wick status --json` includes this `search_providers` summary alongside cooldown/usage state.

---

## 3. Multi-Provider Search Architecture (`src/search/`)

Decompose `src/search.rs` into a cohesive `src/search/` module (`mod.rs`, `providers.rs`, `state.rs`, `fusion.rs`):

### 3.1 `SearchProvider` and `SearchMode` Enums

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

### 3.2 Provider Endpoints and Uniform Flag Mapping

All JSON providers reuse the pooled `RuntimeContext::http_client()` (renamed from `brave_client()`):

| Provider | Endpoint & Auth | `--freshness` Mapping | `--site <domain>` Mapping | `--extra-snippets` Mapping |
| :--- | :--- | :--- | :--- | :--- |
| **Brave** | `GET https://api.search.brave.com/res/v1/web/search` (`X-Subscription-Token`) | `freshness=pd\|pw\|pm\|py` | `site:<domain> <q>` | `extra_snippets=true` |
| **Tavily** | `POST https://api.tavily.com/search` (`Authorization: Bearer <key>`) | `time_range: "day"\|"week"\|"month"\|"year"` | `include_domains: [domain]` | `search_depth: "advanced"` |
| **Serper** | `POST https://google.serper.dev/search` (`X-API-KEY`) | `tbs: "qdr:d"\|"qdr:w"\|"qdr:m"\|"qdr:y"` | `site:<domain> <q>` | Include `sitelinks` summaries in `extra_snippets` |
| **Exa** | `POST https://api.exa.ai/search` (`x-api-key`) | `startPublishedDate: <RFC 3339 UTC>` | `includeDomains: [domain]` | `contents: { highlights: { numSentences: 3 } }` |
| **SearXNG** | `GET <SEARXNG_URL>/search?format=json&q=...` | `time_range=day\|week\|month\|year` | `site:<domain> <q>` | — |
| **DuckDuckGo** | `GET https://html.duckduckgo.com/html/?q=...` (via `TransportClient`) | `df=d\|w\|m\|y` | `site:<domain> <q>` | — |

Every provider normalizes results into the existing `SearchResult { title, url, snippet, extra_snippets }` with HTML tags/entities stripped and whitespace collapsed.

### 3.3 Cooldown, Burst Pacing, and Usage State (`src/search/state.rs`)

Persisted at `<state_dir>/search-state.json` (`0o600`, `flock`-guarded via `search-state.lock`, wiped by `wick session clear`):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderHealthEntry {
    pub last_used_epoch_ms: u64,
    pub cooldown_until_epoch_secs: u64,
    pub month_key: String, // "YYYY-MM" UTC
    pub month_requests: u32,
    pub last_error_status: Option<u16>,
}
```

- **Cooldown durations on failure**:
  - HTTP `429` (rate limit): honor numeric `Retry-After` header clamped to `5..=300` seconds (default `15` seconds).
  - HTTP `402` or `403` (quota exhausted / invalid key): `86_400` seconds (24 hours).
  - HTTP `5xx` or network timeout: `60` seconds.
- **Brave `1 RPS` burst spillover**: when evaluating candidate order in `SearchProvider::Auto`, if `brave.last_used_epoch_ms` is within `1100ms` of `now_ms`, demote `brave` behind healthy `tavily`/`serper` for that request so concurrent swarm calls spill over cleanly with zero `429` errors.
- **Explicit provider overrides**: calling `--provider brave` (or `tavily`, `serper`, `exa`, `searxng`) directly attempts that provider regardless of soft burst pacing and fails loudly if its credential is missing or the request fails.

### 3.4 Deterministic Intent Routing (`SearchProvider::Auto`)

Pure, zero-network classification of `(query, options)`:
1. **`SearchMode::Semantic`**: prefer `Exa -> Tavily -> Brave -> Serper -> DuckDuckGo`.
2. **`SearchMode::Exact`** OR `SearchMode::Auto` when query has **lexical needle signals** (quoted phrase `"..."`, `--site` specified, `::` or file path tokens, or error-code identifiers like `E0382` / `ERR_*` / `0x[0-9a-f]+`):
   - Prefer `Searxng -> Serper -> Brave -> Tavily -> DuckDuckGo` (skipping unconfigured or cooled-down providers).
3. **`SearchMode::Auto` general query**:
   - Prefer `Searxng -> Brave -> Tavily -> Serper -> DuckDuckGo` (demoting `Brave` behind `Tavily`/`Serper` if Brave was used `< 1100ms` ago).

### 3.5 Reciprocal Rank Fusion (`SearchProvider::Federated`)

When `--provider federated` is requested:
- Select up to `3` configured, non-cooled-down orthogonal providers (preferring distinct index families: `Brave` + `Serper` + `Exa`, filling from `Tavily` / `Searxng` / `DuckDuckGo` if fewer are configured).
- Execute them concurrently via `tokio::task::JoinSet`, tolerating partial provider failures (recording cooldowns on failed providers; failing only if all selected providers fail).
- Normalize and deduplicate URLs using `crate::extract::url::normalize_crawl_url`, merge non-duplicate `extra_snippets`, score each unique URL via:
  $$\text{score}(u) = \sum_{p \in \text{providers}} \frac{1}{60 + \text{rank}_p(u)}$$
  and return the top `limit` results with `provider: "federated"` (and `providers_used: Vec<&'static str>` on `SearchResponse`).

---

## 4. Swarm Execution Plan (Single Wave + Subtraction Barrier)

1. **Wave Header Commit (`herald` on `main`)**:
   - Rename `RuntimeContext::brave_client` -> `RuntimeContext::http_client` (keeping a brief compile-clean call site update), add `pub mod auth;` stub in `src/lib.rs` + `src/auth.rs` (`CredentialProvider`, `CredentialSource`, `CredentialStorage`, `resolve_credential`), and expand `SearchProvider` / `SearchMode` / `SearchOptions` in `src/search.rs`. Verify `cargo test --features cronet` passes cleanly on `main`.
2. **Parallel Slices (`workspace: "share"`)**:
   - `auth-store`: owns `src/auth.rs`, `src/context.rs` (verifying `clear_state_dir` preserves `credentials.json` while clearing `search-state.json`), and `tests/auth_tests.rs` (3-tier precedence `env -> keychain -> 0o600 file`, `0o077` permission rejection, atomic `flock` writes, `WICK_NO_KEYCHAIN=1` isolation).
   - `search-backends`: owns `src/search/{mod,providers,state,fusion}.rs` and `tests/search_tests.rs` (`tavily`, `serper`, `exa`, `searxng` parsers + loopback HTTP tests with configurable endpoint overrides, `search-state.json` `429`/`402` cooldowns + `< 1100ms` burst spillover, deterministic lexical/general/semantic routing, and RRF `federated` merging).
   - `cli-docs`: owns `src/main.rs`, `README.md`, `skills/mesh-wick/SKILL.md`, and `tests/cli_tests.rs` (`wick auth set/remove/status`, `wick search --mode`, expanded `wick status --json` `search_providers`, and mechanical parity tests across `README.md`, `SKILL.md`, and `wick skill print`).
   - `verifier`: owns adversarial review commits across `tests/` (testing `Retry-After` clamping, `0o644` credential rejection, `session clear` credential preservation, federated partial-failure tolerance, and zero secret leakage in `--json` / `--verbose` logs).
3. **Wave Barrier Subtraction Pass & Host Sync (`cli-docs` -> `verifier` -> `herald`)**:
   - Run the closing Whole-Repo Subtraction Pass, push linear `main`, install host `wick` (`cargo install --path . --features cronet --locked`), and live-smoke-test `wick status --json`, `wick search --provider tavily`, `wick search --provider serper`, `wick search --provider exa`, and `wick search --provider federated`.
