# pleb Held-Out Task Evaluation Harness (`bench/task_eval`)

Standalone, external evaluation harness for measuring **task-level** acquisition and context-projection quality outside the `pleb` binary and published crate tarball (`Cargo.toml` excludes `bench/`).

Unlike `tests/corpus_evaluation.rs` (a fast unit-level defect ratchet over ~`1.1 KB` sanitized fixtures) and `examples/corpus_benchmark.rs` (a CPU microbenchmark using `ceil(bytes / 4)`), this harness runs the real compiled `pleb` binary over a hermetic local HTTP origin serving realistic multi-section documents (`13–33 KB`) paired with unavailable-evidence cases, and counts **all model-visible tokens** (URL/outcome/title/warning/resource envelope + body) with a pinned **`tiktoken:o200k_base`** tokenizer.

## Task Families (`tasks.json`, 10 tasks)

1. **`version_sensitive_api_patch`** (1 answerable, 2 unavailable):
   - Versioned SDK reference where v1 quick-start appears at the top while v2.4 breaking migration flags live in Section 6, paired with an unhydrated JS shell (`incomplete_shell`) and a 302 login redirect (`exit 2`).
2. **`cross_section_exception_lookup`** (2 answerable, 1 unavailable):
   - CLI reference and RFC-91 transfer spec whose governing shell-quoting caveat and `Idempotency-Key` 86400s/409 Conflict rules appear in late sections (defeating naive byte-prefix truncation), paired with a 403 Turnstile challenge (`exit 2`).
3. **`table_formula_figure_citation`** (2 answerable, 1 unavailable):
   - Multi-column benchmark table with units/footnotes, KaTeX formula with `N >= 64` boundary condition, inline `<svg>` pipeline diagram, `citation_pdf_url` metadata, and a multi-version changelog where only `v3.8.2` changed `max_frame_bytes`, paired with an HTTP 200 soft-404 (`exit 3`).
4. **`adversarial_concealed_channel`** (1 answerable frontier):
   - Technical policy page containing inline `opacity: 0` and `font-size: 0px` hidden instruction spans to keep concealed-channel suppression un-saturated and measurable.

## Running

```bash
# Build release pleb with Cronet
cargo build --release --features cronet --locked

# Run evaluation with pinned tiktoken o200k_base via uv
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --budgets 1200,2000,4000

# Optionally write full per-task JSON output
uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb --json-out /tmp/pleb-task-eval.json

# Regenerate frozen HTML fixtures after editing generate_fixtures.py
python3 bench/task_eval/generate_fixtures.py
```

## Verified Baseline (`pleb` release `--features cronet`, `tiktoken:o200k_base`)

- `S(B)`: fraction of tasks with all required evidence (or exact unavailable-evidence refusal/incomplete signal), zero forbidden-substring leaks, and total model-visible tokens `<= B`.
- `Tok / Supported`: total model-visible tokens spent across all attempted tasks divided by supported completions (\(C_{\text{success}}\) in tokens).
- `pleb_focus_proto`: external prototype of heading-aware BM25 + enclosing-heading projection with collapsed TOC map counted inside budget `B`, evaluated against full clean output and `--max-bytes` prefix truncation before promoting any projection flag into `src/`.

| Budget (tok) | Strategy | S(B) | Answerable | Unavailable Honest | False-Usable | Usable-Rejected | Leaks | Mean Tok | Tok / Supported | p95 ms |
| ---: | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1200 | `pleb_clean_full` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 1424 | 2849 | 29.1 |
| 1200 | `pleb_article_full` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 1424 | 2849 | 34.2 |
| 1200 | `pleb_prefix_budget` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 456 | 911 | 72.1 |
| 1200 | `pleb_focus_proto` | 9/10 (0.900) | 5/6 | 4/4 | 0 | 0 | 1 | 510 | 567 | 28.1 |
| 2000 | `pleb_clean_full` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 1424 | 2849 | 35.2 |
| 2000 | `pleb_article_full` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 1424 | 2849 | 51.2 |
| 2000 | `pleb_prefix_budget` | 5/10 (0.500) | 1/6 | 4/4 | 0 | 0 | 1 | 652 | 1304 | 23.3 |
| 2000 | `pleb_focus_proto` | 9/10 (0.900) | 5/6 | 4/4 | 0 | 0 | 1 | 776 | 862 | 28.2 |
| 4000 | `pleb_clean_full` | 8/10 (0.800) | 4/6 | 4/4 | 0 | 0 | 1 | 1424 | 1781 | 31.8 |
| 4000 | `pleb_article_full` | 8/10 (0.800) | 4/6 | 4/4 | 0 | 0 | 1 | 1424 | 1781 | 26.3 |
| 4000 | `pleb_prefix_budget` | 6/10 (0.600) | 2/6 | 4/4 | 0 | 0 | 1 | 1124 | 1874 | 25.6 |
| 4000 | `pleb_focus_proto` | 9/10 (0.900) | 5/6 | 4/4 | 0 | 0 | 1 | 1370 | 1522 | 22.2 |

Key observations from the baseline:
- **Outcome calibration holds (`0` false-usable, `0` usable-rejected, `4/4` honest unavailable).**
- **Byte-prefix truncation (`--max-bytes`) degrades task completion at 4,000 tokens** (`6/10` vs. `8/10` full clean) because late migration/exception sections are cut off.
- **Heading-aware focused projection lifts `S(1200)` from `0.500` to `0.900`** while cutting tokens per supported completion from `2,849` to `567` (`5.0x`).
- **Open frontier (`Leaks = 1`)**: `cloaked_injection_frontier_answerable` passes `opacity: 0` and `font-size: 0px` spans through `is_inline_hidden_style`, confirming the next extraction hardening target.
