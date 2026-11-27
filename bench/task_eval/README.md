# pleb Held-Out Task Evaluation Harness (`bench/task_eval`)

Standalone, external evaluation harness for measuring **task-level** acquisition and context-projection quality outside the `pleb` binary and published crate tarball (`Cargo.toml` excludes `bench/`).

Unlike `tests/corpus_evaluation.rs` (a fast unit-level defect ratchet over ~`1.1 KB` sanitized fixtures) and `examples/corpus_benchmark.rs` (a CPU microbenchmark using `ceil(bytes / 4)`), this harness runs the real compiled `pleb` binary across four complementary evaluation tracks and counts **all model-visible tokens** (URL/outcome/title/warning/resource envelope + body) with a pinned **`tiktoken:o200k_base`** tokenizer:

1. **Offline single-page suite (`tasks`, 13 tasks)** over a hermetic local HTTP origin serving realistic multi-section documents (`13–33 KB`) **plus three frozen wild production documentation snapshots (`42 KB–502 KB`, totaling `615 KB`)** paired with unavailable-evidence cases and head-to-head external Python extractors (`trafilatura`, `html2text`, Crawl4AI BM25 pattern).
2. **Offline multi-URL stdin batch track (`batch_tasks`, 2 workflows)** exercising `pleb fetch - --jsonl` across mixed relevant, distractor, unavailable-evidence, and cloaked-injection URLs under a single total batch token budget (`4,000` tok).
3. **Opt-in CEF SPA hydration track (`--cef-track`, `spa_tasks`)** verifying static `--render cronet` honest `incomplete_shell` triage against local `v5-meta` CEF `--render cef` full and `--focus` JS-hydrated extraction.
4. **Opt-in paired live-URL track (`--live-track` / `--live-only`, `live_tasks`, 4 live URLs)** comparing `pleb_live_clean` and `pleb_live_focus` head-to-head against direct `Accept: text/markdown`, `r.jina.ai`, and Firecrawl `/v1/scrape`.

## Task Families (`tasks.json`)

### Offline Single-Page Suite (13 tasks)

1. **`version_sensitive_api_patch`** (1 answerable, 2 unavailable):
   - Versioned SDK reference where v1 quick-start appears at the top while v2.4 breaking migration flags live in Section 6, paired with an unhydrated JS shell (`incomplete_shell`) and a 302 login redirect (`exit 2`).
2. **`cross_section_exception_lookup`** (2 answerable, 1 unavailable):
   - CLI reference and RFC-91 transfer spec whose governing shell-quoting caveat and `Idempotency-Key` 86400s/409 Conflict rules appear in late sections, paired with a 403 Turnstile challenge (`exit 2`).
3. **`table_formula_figure_citation`** (2 answerable, 1 unavailable):
   - Multi-column benchmark table with units/footnotes, KaTeX formula with `N >= 64` boundary condition, inline `<svg>` pipeline diagram, `citation_pdf_url` metadata, and a multi-version changelog where only `v3.8.2` changed `max_frame_bytes`, paired with an HTTP 200 soft-404 (`exit 3`).
4. **`adversarial_concealed_channel`** (1 answerable):
   - Technical policy page containing inline `opacity: 0` and `font-size: 0px` hidden instruction spans verifying concealed-channel suppression (`Leaks == 0`).
5. **`wild_production_docs`** (3 answerable, `42 KB–502 KB` real frozen snapshots):
   - `wild_sqlite_wal.html` (`42 KB`, SQLite WAL documentation — Section 5 read-only database caveats).
   - `wild_rust_mutex.html` (`70 KB`, Rustdoc `std::sync::Mutex` — `clear_poison` / `into_inner` poison recovery across 69 linked headings).
   - `wild_rfc9421.html` (`502 KB`, IETF RFC 9421 HTTP Message Signatures — Section 2.5 ABNF and duplicate/`req` error rules across 126 headings).

### Multi-URL Batch Workflows (2 tasks, offline)

1. **`batch_api_migration_mixed_4urls`**: 4 URLs piped to `pleb fetch - --jsonl` combining the v2.4 SDK target, a `42 KB` SQLite distractor, an unhydrated JS shell (`incomplete_shell`), and a 403 Turnstile challenge (`exit_code == 2`).
2. **`batch_wild_rust_and_cloaked_3urls`**: 3 URLs combining `70 KB` Rustdoc `Mutex`, a `33 KB` RFC distractor, and the adversarial cloaked-injection fixture (`0` leaks).

### Opt-In CEF SPA Hydration Task (1 local JS-hydrating SPA)

1. **`spa_pulsequeue_rate_limiter_hydration`** (`fixtures/spa_hydrated_sdk_docs.html`): initial empty `<div id="root"></div>` that hydrates a multi-section SDK reference + concealed `opacity:0` injection span via `setTimeout` after `40 ms`.

### Opt-In Live-Pair Panel (4 live public URLs)

1. **`live_cloudflare_markdown_agents`** (`https://developers.cloudflare.com/fundamentals/reference/markdown-for-agents/`): native `Accept: text/markdown` content negotiation (`markdown_mirror`).
2. **`live_sqlite_wal`** (`https://sqlite.org/wal.html`): dense static technical specification (`cronet`).
3. **`live_rust_mutex`** (`https://doc.rust-lang.org/std/sync/struct.Mutex.html`): link-dense Rustdoc API reference (`cronet`).
4. **`live_github_rust_readme`** (`https://github.com/rust-lang/rust/blob/master/README.md`): GitHub blob structural raw-source rewrite (`raw_source`).

## Running

```bash
# Build release pleb with Cronet
cargo build --release --features cronet --locked

# Run hermetic offline suite (13 single-page tasks + external competitors + multi-URL batch track)
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000

# Run pleb modes only
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --no-competitors

# Add opt-in CEF SPA hydration track (requires local v5-meta CEF renderer)
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000 --cef-track

# Run opt-in paired live-URL track (set JINA_API_KEY / FIRECRAWL_API_KEY to enable authenticated cloud readers)
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --live-only --live-budget 4000

# Regenerate synthetic structured fixtures after editing generate_fixtures.py
python3 bench/task_eval/generate_fixtures.py
```

## Verified Offline Single-Page Baseline (`pleb` release `--features cronet`, `tiktoken:o200k_base`)

| Budget (tok) | Strategy | S(B) | Answerable | Wild Docs | Unavailable Honest | False-Usable | Leaks | Mean Tok | Tok / Supported | p95 ms |
| ---: | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1200 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 7215 | 15632 | 112.0 |
| 1200 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 7210 | 15622 | 45.2 |
| 1200 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 614 | 1330 | 55.6 |
| 1200 | `pleb_focus_cli` | 12/13 (0.923) | 8/9 | 2/3 | 4/4 | 0 | 0 | 437 | 473 | 45.5 |
| 1200 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 110.7 |
| 1200 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 48.8 |
| 1200 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 430 | 1119 | 84.0 |
| 2000 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 7215 | 15632 | 33.0 |
| 2000 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 7210 | 15622 | 32.2 |
| 2000 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 932 | 2019 | 30.9 |
| 2000 | `pleb_focus_cli` | 12/13 (0.923) | 8/9 | 2/3 | 4/4 | 0 | 0 | 732 | 793 | 47.2 |
| 2000 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 112.1 |
| 2000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 50.8 |
| 2000 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 676 | 1758 | 84.5 |
| 4000 | `pleb_clean_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 7215 | 10421 | 32.1 |
| 4000 | `pleb_article_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 7210 | 10414 | 34.3 |
| 4000 | `pleb_prefix_budget` | 7/13 (0.538) | 3/9 | 0/3 | 4/4 | 0 | 0 | 1732 | 3217 | 31.8 |
| 4000 | `pleb_focus_cli` | 13/13 (1.000) | 9/9 | 3/3 | 4/4 | 0 | 0 | 1304 | 1304 | 48.3 |
| 4000 | `trafilatura_full` | 4/13 (0.308) | 4/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 22454 | 113.3 |
| 4000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 50.8 |
| 4000 | `crawl4ai_bm25_ref` | 6/13 (0.462) | 5/9 | 1/3 | 1/4 | 3 | 1 | 1099 | 2381 | 86.7 |

## Verified Multi-URL Batch Baseline (`4,000` tok total across all URLs per task)

| Strategy | Supported | False-Usable | Leaks | Mean Batch Tok | Tok / Supported |
| :--- | ---: | ---: | ---: | ---: | ---: |
| `pleb_batch_clean` | 0/2 | 0 | 0 | 12154 | inf |
| `pleb_batch_focus` | 2/2 | 0 | 0 | 1640 | 1640 |

## Verified Opt-In CEF SPA Hydration Baseline (`1,500` tok budget, local `v5-meta` CEF)

| Task | Strategy | Status | Outcome | Evidence | Leaks | Tok | Budget OK | Supported | ms |
| :--- | :--- | :--- | :--- | ---: | ---: | ---: | :---: | :---: | ---: |
| `spa_pulsequeue_rate_limiter_hydration` | `pleb_cronet_only` | ok | `incomplete_shell` | 0/3 | 0 | 92 | yes | yes | 16.2 |
| `spa_pulsequeue_rate_limiter_hydration` | `pleb_cef_full` | ok | `complete` | 3/3 | 0 | 1640 | no | no | 1698.1 |
| `spa_pulsequeue_rate_limiter_hydration` | `pleb_cef_focus` | ok | `complete` | 3/3 | 0 | 671 | yes | yes | 1367.4 |

## Verified Opt-In Live-Pair Baseline (`4,000` tok budget, `tiktoken:o200k_base`)

| Strategy | Supported | Evidence Full | Mean Tok | Tok / Supported | p95 ms | Notes |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `pleb_live_clean` | 1/4 | 4/4 | 5440 | 21759 | 490.2 | ok |
| `pleb_live_focus` | 4/4 | 4/4 | 2275 | 2275 | 399.6 | ok |
| `accept_markdown_direct` | 0/4 | 2/4 | 9510 | inf | 449.0 | error:HTTP 503 |
| `jina_reader` | 0/4 | 0/4 | 0 | inf | 220.7 | auth_required(set JINA_API_KEY) |
| `firecrawl_api` | 0/0 | 0/0 | 0 | inf | 0.0 | skipped_no_api_key |

Key observations:
- **Structural pilcrow stripping, anchor-tooltip compaction, and `<summary>` heading-first metadata ordering cut `pleb_clean_full` mean tokens by `-10.4%` (`8,053 -> 7,215` tok)** and lifted tight-budget **`S(1200)` and `S(2000)` to `12/13 (0.923)`** (`473` and `793` tok/supported, `45.5–47.2 ms` p95) while preserving **`S(4000) = 13/13 (1.000)`** (`1,304` tok/supported, `48.3 ms` p95).
- **Multi-URL stdin batch (`pleb_batch_focus`) fits 3–4 mixed pages inside `1,379–1,901` tokens total (`7.4x` lower than `pleb_batch_clean`)** in `17.1–21.1 ms`, with `0` false-usable items and `0` leaks.
- **CEF SPA hydration (`pleb_cef_focus`) recovers `3/3` JS-rendered evidence tuples in `671` tokens (`0` leaks)** while static `pleb_cronet_only` honestly emits `incomplete_shell` in `16.2 ms` (`92` tok).
