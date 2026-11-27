# pleb Held-Out Task Evaluation Harness (`bench/task_eval`)

Standalone, external evaluation harness for measuring **task-level** acquisition and context-projection quality outside the `pleb` binary and published crate tarball (`Cargo.toml` excludes `bench/`).

Unlike `tests/corpus_evaluation.rs` (a fast unit-level defect ratchet over ~`1.1 KB` sanitized fixtures) and `examples/corpus_benchmark.rs` (a CPU microbenchmark using `ceil(bytes / 4)`), this harness runs the real compiled `pleb` binary over a hermetic local HTTP origin serving realistic multi-section documents (`13–33 KB`) **plus three frozen wild production documentation snapshots (`42 KB–502 KB`, totaling `615 KB`)** paired with unavailable-evidence cases, compares head-to-head against external static/BM25 extractors on identical frozen bytes, supports an **opt-in paired live-URL track** (`--live-track` / `--live-only`), and counts **all model-visible tokens** (URL/outcome/title/warning/resource envelope + body) with a pinned **`tiktoken:o200k_base`** tokenizer.

## Task Families (`tasks.json`)

### Offline Frozen Suite (13 tasks)

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
   - `wild_rfc9421.html` (`502 KB`, IETF RFC 9421 HTTP Message Signatures — Section 2.5 ABNF and duplicate/`req` error rules across 126 headings, ~`72,724` full Markdown tokens).

### Opt-In Live-Pair Panel (4 live public URLs)

1. **`live_cloudflare_markdown_agents`** (`https://developers.cloudflare.com/fundamentals/reference/markdown-for-agents/`): native `Accept: text/markdown` content-negotiation server (`markdown_mirror`).
2. **`live_sqlite_wal`** (`https://sqlite.org/wal.html`): dense static technical specification (`cronet`).
3. **`live_rust_mutex`** (`https://doc.rust-lang.org/std/sync/struct.Mutex.html`): link-dense Rustdoc API reference (`cronet`).
4. **`live_github_rust_readme`** (`https://github.com/rust-lang/rust/blob/master/README.md`): GitHub blob structural raw-source rewrite (`raw_source`).

## Running

```bash
# Build release pleb with Cronet
cargo build --release --features cronet --locked

# Run hermetic offline head-to-head evaluation (pleb modes + trafilatura + html2text + Crawl4AI BM25 ref) via uv
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000

# Run pleb modes only
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --no-competitors

# Run opt-in paired live-URL track (pleb vs Accept:text/markdown, r.jina.ai, and Firecrawl)
# Set JINA_API_KEY and/or FIRECRAWL_API_KEY in env to benchmark authenticated cloud readers
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --live-only --live-budget 4000

# Run offline + live-pair track together
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000 --live-track

# Regenerate synthetic structured fixtures after editing generate_fixtures.py
python3 bench/task_eval/generate_fixtures.py
```

## Verified Offline Head-to-Head Baseline (`pleb` release `--features cronet`, `tiktoken:o200k_base`)

- `S(B)`: fraction of tasks with all required evidence (or exact unavailable-evidence refusal/incomplete signal), zero forbidden-substring leaks, and total model-visible tokens `<= B`.
- `Tok / Supported`: total model-visible tokens across all 13 attempted tasks divided by supported completions (\(C_{\text{success}}\) in tokens).
- `pleb_focus_cli`: compiled `./target/release/pleb fetch --focus <query> --max-bytes <N>` (zero-dependency Rust BM25 section projection + bounded collapsed-section map).
- `trafilatura_full`, `html2text_full`, `crawl4ai_bm25_ref`: external Python extractors run on the exact same frozen HTML inputs (note: static HTML extractors lack HTTP status/challenge/login-wall triage, reflected in `False-Usable`; `Answerable` and `Wild Docs` columns compare extraction/budget quality directly).

| Budget (tok) | Strategy | S(B) | Answerable | Wild Docs | Unavailable Honest | False-Usable | Leaks | Mean Tok | Tok / Supported | p95 ms |
| ---: | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1200 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 17448 | 90.2 |
| 1200 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 17437 | 284.6 |
| 1200 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 615 | 1332 | 108.3 |
| 1200 | `pleb_focus_cli` | 11/13 (0.846) | 7/9 | 1/3 | 4/4 | 0 | 0 | 439 | 519 | 162.4 |
| 1200 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 383.8 |
| 1200 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 174.0 |
| 1200 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 430 | 1119 | 326.4 |
| 2000 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 17448 | 82.6 |
| 2000 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 17437 | 93.2 |
| 2000 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 942 | 2040 | 93.2 |
| 2000 | `pleb_focus_cli` | 11/13 (0.846) | 7/9 | 1/3 | 4/4 | 0 | 0 | 735 | 869 | 152.1 |
| 2000 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 484.8 |
| 2000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 185.1 |
| 2000 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 676 | 1758 | 314.8 |
| 4000 | `pleb_clean_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 11632 | 95.5 |
| 4000 | `pleb_article_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 11625 | 109.5 |
| 4000 | `pleb_prefix_budget` | 7/13 (0.538) | 3/9 | 0/3 | 4/4 | 0 | 0 | 1750 | 3249 | 108.4 |
| 4000 | `pleb_focus_cli` | 13/13 (1.000) | 9/9 | 3/3 | 4/4 | 0 | 0 | 1333 | 1333 | 303.4 |
| 4000 | `trafilatura_full` | 4/13 (0.308) | 4/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 22454 | 641.6 |
| 4000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 256.3 |
| 4000 | `crawl4ai_bm25_ref` | 6/13 (0.462) | 5/9 | 1/3 | 1/4 | 3 | 1 | 1099 | 2381 | 501.7 |

## Verified Opt-In Live-Pair Baseline (`4,000` tok budget, `tiktoken:o200k_base`)

| Strategy | Supported | Evidence Full | Mean Tok | Tok / Supported | p95 ms | Notes |
| :--- | ---: | ---: | ---: | ---: | ---: | :--- |
| `pleb_live_clean` | 1/4 | 4/4 | 5638 | 22552 | 638.0 | ok |
| `pleb_live_focus` | 4/4 | 4/4 | 2271 | 2271 | 492.9 | ok |
| `accept_markdown_direct` | 0/4 | 3/4 | 27772 | inf | 831.4 | ok |
| `jina_reader` | 0/4 | 0/4 | 0 | inf | 198.9 | auth_required(set JINA_API_KEY) |
| `firecrawl_api` | 0/0 | 0/0 | 0 | inf | 0.0 | skipped_no_api_key |

Key observations:
- **Concealed-channel hardening closed the `Leaks` gap (`1 -> 0`)** across all `pleb` modes while `html2text` leaked `6` and `crawl4ai_bm25_ref` leaked `1`.
- **Compiled `pleb fetch --focus` reaches `S(4000) = 1.000` (`13/13`, `9/9` answerable, `3/3` wild docs) at `1,333` tokens/supported**, and lifts `S(1200)` from `0.462` (`pleb_clean_full` / `--max-bytes`) to `0.846` at `519` tokens/supported (`33.6x` lower than full clean).
- **Live-pair acquisition (`pleb_live_focus`) reaches `4/4` supported at `2,271` tokens/supported and `492.9 ms` p95** with zero API keys or cloud proxy: `markdown_mirror` (`2,622` tok) on Cloudflare, `cronet` (`2,633` tok SQLite WAL; `2,961` tok Rustdoc `Mutex`), and `raw_source` (`869` tok on GitHub Rust `README.md` vs. `73,046` tok for raw GitHub HTML).
