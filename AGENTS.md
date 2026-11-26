# Repository Agent Rules (pleb)

pleb (Cargo crate `pleb`, binary `pleb`, by Memory Sync Hub LLC) is a solo-owned, greenfield Rust + C/Objective-C CLI providing local, zero-telemetry web acquisition and search for AI agents.

## Git Flow and Host Binary Sync

- Work directly on linear `main` (`git rebase`, zero merge commits). Temporary worker worktree branches (`swarm/*`) must fast-forward onto `main` and be deleted upon wave completion.
- After pushing code changes on `main`, keep the local `pleb` installation synchronized:
  - Rust changes: `cargo install --path . --features cronet --locked` (compile-time `PLEB_BUILD_ID` automatically restarts any running `daemon.sock` process on the next invocation).
  - `cef/` changes: `pleb daemon stop || true && bash scripts/install-cef-mac.sh --force` (or `install-cef-linux.sh --force` on Linux).

## Hermetic Build and Test Rules

- Never place `target-*` directories inside the repository working tree. Use the default `/target` symlink or external build storage (`/Volumes/rabbitx10/build/...` when present, else `${TMPDIR:-/tmp}/pleb-workers/<role>`).
- Never touch `~/.pleb` in tests. Every test must allocate a short-path temporary state directory `/tmp/pleb-t-<pid>-<seq>` (`0o700`, staying below the 100-byte / 104-byte Unix domain socket `sun_path` ceiling), set `PLEB_NO_HOME_CEF=1`, and populate `RuntimeConfig::daemon_executable` from `env!("CARGO_BIN_EXE_pleb")` when testing daemon auto-spawn.
- Behavioral boundary: never implement CAPTCHA solving, synthetic challenge clicking, or anti-bot evasion scripts. Active bot challenges must classify immediately into `FetchError::Challenge` (exit code `2`) with zero duplicate CEF retries.

## Three-Tier Iteration Protocol

### Tier 1 — Single-Defect Flywheel (Default: 1 Agent on `main`)
1. **Fixture First**: Capture a minimal (`< 15 KB`), sanitized HTML/text reproducer in `tests/corpus/frontier_<slug>.html` with a line-1 `<!-- pleb-fixture: {"tier":"frontier", ...} -->` contract. Verify `cargo test --test corpus_evaluation` preserves `ratchet == 1.000` while recording the new `"frontier"` failure before editing `src/`.
2. **Structural Fix**: Solve the general HTML5 / ARIA / CSS / HTTP pattern in `src/extract/`, `src/policy/`, or `cef/`, paired with an inverse false-positive test. Reserve `data/site-rules.json` strictly for structural host/source rewrites or mandatory `render = "cef"` overrides that cannot be inferred from response bytes.
3. **Promote & Budget**: Promote `"tier"` from `"frontier"` to `"ratchet"`, verify `cargo fmt --check`, `cargo clippy --all-targets --features cronet -- -D warnings`, and `cargo test --features cronet`, and hold net growth `<= 40` lines per defect class (deduplicating adjacent helpers in the same commit if exceeded).

### Tier 2 — Batch Canary Wave (3-Worker `/swarm`)
- **Trigger**: `>= 5` `"frontier"` fixtures accumulate, Chromium/Cronet/CEF SDK versions bump, or an out-of-loop 20–30 URL live domain audit is warranted.
- **Topology**: `corpus-verifier` (`share`, owns `tests/`) + `author` (`share`, climbs `"frontier"` -> `"ratchet"`) + `shepherd` (`inherit`, rebases linearly onto `main`, runs one closing whole-repo Subtraction Pass, pushes `main`, and runs `cargo install`).

### Tier 3 — Cross-Seam Capability Wave (`docs/0N-*.md` -> 4-Worker `/swarm`)
- **Trigger**: A capability alters transport, policy, extraction, and CLI contracts simultaneously.
- **Protocol**: Commit a specification in `docs/0N-*.md`, land one compiling **Wave Header Commit** on `main` defining shared types/signatures, execute parallel `share` worktree slices with strict Generator–Verifier separation (authors never edit `tests/corpus/`), and close behind a Wave Barrier Subtraction Pass per `docs/02-swarm-hillclimb-protocol.md`.

## Standing Non-Regression Gates

- `ratchet == 1.000` across all `tests/corpus/` and binary magic-byte cases in `tests/corpus_evaluation.rs`.
- `untyped_error_matches == 0`: all cross-layer transport/daemon/CEF errors use `TransportError` / `TransportErrorKind` (zero `.contains(` / `.starts_with(` string sniffing).
- `README.md` and `README-FULL.md` CLI and environment contract parity verified by `tests/cli_tests.rs`.
- **5-to-1 Subtraction Ratio**: every 5 efficacy commits on `main` are followed by 1 pure Subtraction Commit (`ratchet == 1.000`, `signal_density` non-decreasing, net negative lines, preserving `///` doc comments, `debug_assert!`, and `tests/*.rs` `pub` items).
