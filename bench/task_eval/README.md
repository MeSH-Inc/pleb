# pleb Held-Out Task Evaluation Harness (`bench/task_eval`)

Standalone, external evaluation harness for measuring **task-level** acquisition and context-projection quality outside the `pleb` binary and published crate tarball (`Cargo.toml` excludes `bench/`).

Unlike `tests/corpus_evaluation.rs` (a fast unit-level defect ratchet over ~`1.1 KB` sanitized fixtures) and `examples/corpus_benchmark.rs` (a CPU microbenchmark using `ceil(bytes / 4)`), this harness runs the real compiled `pleb` binary over a hermetic local HTTP origin serving realistic multi-section documents (`13–33 KB`) **plus three frozen wild production documentation snapshots (`42 KB–502 KB`, totaling `615 KB`)** paired with unavailable-evidence cases, compares head-to-head against external static/BM25 extractors on identical frozen bytes, and counts **all model-visible tokens** (URL/outcome/title/warning/resource envelope + body) with a pinned **`tiktoken:o200k_base`** tokenizer.

## Task Families (`tasks.json`, 13 tasks)

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

## Running

```bash
# Build release pleb with Cronet
cargo build --release --features cronet --locked

# Run full head-to-head evaluation (pleb modes + trafilatura + html2text + Crawl4AI BM25 ref) via uv
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000

# Run pleb modes only
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --no-competitors

# Regenerate synthetic structured fixtures after editing generate_fixtures.py
python3 bench/task_eval/generate_fixtures.py
```

## Verified Head-to-Head Baseline (`pleb` release `--features cronet`, `tiktoken:o200k_base`)

- `S(B)`: fraction of tasks with all required evidence (or exact unavailable-evidence refusal/incomplete signal), zero forbidden-substring leaks, and total model-visible tokens `<= B`.
- `Tok / Supported`: total model-visible tokens across all 13 attempted tasks divided by supported completions (\(C_{\text{success}}\) in tokens).
- `pleb_focus_cli`: compiled `./target/release/pleb fetch --focus <query> --max-bytes <N>` (zero-dependency Rust BM25 section projection + bounded collapsed-section map).
- `trafilatura_full`, `html2text_full`, `crawl4ai_bm25_ref`: external Python extractors run on the exact same frozen HTML inputs (note: static HTML extractors lack HTTP status/challenge/login-wall triage, reflected in `False-Usable`; `Answerable` and `Wild Docs` columns compare extraction/budget quality directly).

| Budget (tok) | Strategy | S(B) | Answerable | Wild Docs | Unavailable Honest | False-Usable | Leaks | Mean Tok | Tok / Supported | p95 ms |
| ---: | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1200 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 17448 | 85.3 |
| 1200 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 17437 | 46.5 |
| 1200 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 615 | 1332 | 42.0 |
| 1200 | `pleb_focus_cli` | 11/13 (0.846) | 7/9 | 1/3 | 4/4 | 0 | 0 | 439 | 519 | 65.0 |
| 1200 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 131.8 |
| 1200 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 55.4 |
| 1200 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 430 | 1119 | 97.6 |
| 2000 | `pleb_clean_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 17448 | 43.5 |
| 2000 | `pleb_article_full` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 17437 | 48.5 |
| 2000 | `pleb_prefix_budget` | 6/13 (0.462) | 2/9 | 0/3 | 4/4 | 0 | 0 | 942 | 2040 | 49.2 |
| 2000 | `pleb_focus_cli` | 11/13 (0.846) | 7/9 | 1/3 | 4/4 | 0 | 0 | 735 | 869 | 64.1 |
| 2000 | `trafilatura_full` | 2/13 (0.154) | 2/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 44908 | 162.9 |
| 2000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 59.5 |
| 2000 | `crawl4ai_bm25_ref` | 5/13 (0.385) | 4/9 | 0/3 | 1/4 | 3 | 1 | 676 | 1758 | 105.9 |
| 4000 | `pleb_clean_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 8053 | 11632 | 44.1 |
| 4000 | `pleb_article_full` | 9/13 (0.692) | 5/9 | 0/3 | 4/4 | 0 | 0 | 8048 | 11625 | 60.4 |
| 4000 | `pleb_prefix_budget` | 7/13 (0.538) | 3/9 | 0/3 | 4/4 | 0 | 0 | 1750 | 3249 | 46.2 |
| 4000 | `pleb_focus_cli` | 12/13 (0.923) | 8/9 | 2/3 | 4/4 | 0 | 0 | 1333 | 1444 | 92.4 |
| 4000 | `trafilatura_full` | 4/13 (0.308) | 4/9 | 0/3 | 0/4 | 4 | 0 | 6909 | 22454 | 182.9 |
| 4000 | `html2text_full` | 0/13 (0.000) | 0/9 | 0/3 | 0/4 | 4 | 6 | 7956 | inf | 93.5 |
| 4000 | `crawl4ai_bm25_ref` | 6/13 (0.462) | 5/9 | 1/3 | 1/4 | 3 | 1 | 1099 | 2381 | 108.9 |

Key observations:
- **Concealed-channel hardening closed the `Leaks` gap (`1 -> 0`)** across all `pleb` modes while `html2text` leaked `6` and `crawl4ai_bm25_ref` leaked `1`.
- **Compiled `pleb fetch --focus` beats every baseline across all three token budgets**, lifting `S(1200)` from `0.462` (`pleb_clean_full` / `--max-bytes`) to `0.846` at `519` tokens per supported completion (`33.6x` fewer tokens/supported than full clean), and solving `502 KB` / `72,724`-token RFC 9421 inside `4,000` tokens (`S(4000) = 0.923`, `8/9` answerable vs. `5/9` `crawl4ai_bm25_ref` and `4/9` `trafilatura_full`).
- **Remaining open wild frontier**: `wild_rust_mutex_poison_recovery` reaches `3/4` at `4,000` tokens (69 link-heavy Rustdoc method headings competing for budget), motivating future `--section` drill-down or link-compacted focus profiles.
