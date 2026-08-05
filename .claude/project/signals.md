# Project signals

## Framework & runtime

- Language: Rust (stable), edition 2024
- Plugin format: VST3 via `nice-plug` 0.2.3 (git fork, `github.com/MysteriousWolf/nice-plug` branch `ethertap`, pending upstream PR)
- GUI: `nice-plug-iced` (Elm-style, iced 0.14; 360×280 in VST3 mode, 500×620 with `standalone` feature)
- Async/concurrency: `crossbeam-channel` 0.5 (bounded lock-free queues), `parking_lot` 0.12 (uncontended Mutex only), `std::sync::atomic`
- OSC: `rosc` 0.11 (encoder + decoder)
- MIDI: `midir` 0.11; software loopback via workspace crate `midi-loopback` (path dep); macOS: `coremidi` 0.9 + `core-foundation` 0.10
- Networking: `if-addrs` 0.15 (LAN interface enumeration for device scan)
- Crate version: 0.2.0; crate type: `cdylib` + `rlib`; workspace has 5 members (`.`, `xtask`, `vst-runtime`, `mock-suite`, `midi-loopback`)

## Build / test / lint

| Purpose | Command | Source |
|---------|---------|--------|
| Bundle VST3 | `cargo run -p xtask -- bundle ethertap --release` | `xtask/src/main.rs` |
| Universal macOS build | `./scripts/build.sh --universal` | `scripts/build.sh` |
| Install VST3 locally | `./scripts/install.sh [--universal] [--no-build]` | `scripts/install.sh` |
| Clippy (both feature sets) + fmt check | `./scripts/check.sh` | `scripts/check.sh` |
| Format | `./scripts/format.sh` | `scripts/format.sh` |
| Run tests | `./scripts/test.sh` or `cargo test --workspace` | `scripts/test.sh`, `tests/` |
| Coverage (threshold-gated, default 95%) | `./scripts/coverage.sh [--open] [--threshold N]` | `scripts/coverage.sh` |
| Standalone GUI smoke test | `./scripts/gui_test.sh` (+ `cargo run -p mock-suite` for the mock) | `scripts/gui_test.sh` |
| Standalone end-to-end workflow test | `./scripts/test_standalone_workflow.sh` | `scripts/test_standalone_workflow.sh` |
| Clean build artifacts | `./scripts/clean.sh` | `scripts/clean.sh` |
| Benchmarks | `cargo bench` | `benches/core.rs` |

CI gate: GitHub Actions — `.github/workflows/ci.yml` on every push/PR (3-OS test matrix, clippy both feature sets `-D warnings`, fmt check, bench compile-check, coverage, `cargo audit`); it is also `workflow_call`-able and is re-run as a required gate by `.github/workflows/release.yml`. Releases are not triggered by a tag push alone — `.github/workflows/tag-release.yml` is a manual `workflow_dispatch` that computes/pushes the next `v<YY>.<N>` tag and then calls `release.yml`, which builds macOS (universal)/Windows/Linux artifacts and publishes the GitHub Release.

## Language breakdown

| Language | LOC | Files | % |
|----------|-----|-------|---|
| Rust | 17090 | 33 | 50% |
| Markdown | 1764 | 12 | 18% |
| Shell | 1053 | 9 | 13% |
| YAML | 497 | 4 | 6% |
| TOML | 113 | 7 | 10% |
| JSON | 42 | 1 | 1% |

## DevOps & CI

- Release pipeline: `tag-release.yml` (manual `workflow_dispatch`) computes/pushes a `v<YY>.<N>` tag and calls `release.yml`, which re-runs `ci.yml` then builds artifacts on 3 platforms and publishes a GitHub Release (release notes include a best-effort AI-generated summary via `actions/ai-inference@v1`).
- No deploy step beyond artifact upload (manual install from Releases page, or `scripts/install.sh` locally).

---

## Domains

| Domain | Repo paths | One-liner | Detail |
|--------|------------|-----------|--------|
| core | `src/lib.rs`, `src/params.rs`, `src/bin/` | Plugin struct, `process()` RT loop, BPM settle, params, sync dispatch, standalone transport atomics | signals/core.md |
| network | `src/network.rs`, `src/osc.rs`, `src/reconnect.rs` | UDP/OSC worker, telemetry, LAN scanner, Backoff | signals/network.md |
| midi | `src/midi_clock.rs`, `src/midi_watcher.rs`, `src/midi_hw.rs`, `midi-loopback/` | MIDI clock burst worker, bridge passthrough, hot-plug watcher, in-process software loopback registry | signals/midi.md |
| editor | `src/editor.rs` | Iced GUI (Elm-style, nice-plug-iced): LEDs, sync controls, FX slot/device pickers, telemetry display, standalone DAW-shell transport panel | signals/editor.md |
| build | `scripts/`, `xtask/`, `vst-runtime/`, `mock-suite/`, `.github/`, `Cargo.toml`, `rust-toolchain.toml`, `.cargo/audit.toml` | VST3 bundle, CI matrix, fork dep story, headless VST test harness, mock mixer/MIDI-clock test fixture | signals/build.md |

## Cross-cutting

- **Test layout:** `tests/integration_tests.rs` (network worker against `MockMixer`, incl. scan-port discovery), `tests/reconnect_tests.rs` (auto_reconnect: opt-in self-connect, identity verification, rescan retargeting; worker- and harness-level), `tests/osc_tests.rs` (OSC encode/decode), `tests/harness_e2e.rs` + `tests/sync_matrix.rs` + `tests/midi_clock_tests.rs` + `tests/functional_workflows.rs` + `tests/functional_edge_cases.rs` (vst-runtime `Harness` driving the full plugin against `MockMixer` / `LoopbackClockSink`; gated `#![cfg(not(feature = "standalone"))]`, serialised on `E2E_LOCK`), `tests/vst_runtime_integration.rs` (harness sanity + host-param-set coverage guard), `tests/common/mod.rs` (NetworkWorker glue + `harness_util` helpers; `MockMixer`/`SlotState` re-exported from the `mock-suite` crate). Unit tests co-located in `src/reconnect.rs`; `vst-runtime` and `mock-suite` carry their own unit tests (incl. TUI helper math). `src/midi_hw.rs` (real-hardware MIDI connect calls) is excluded from `scripts/coverage.sh`'s coverage measurement.
- **RT safety contract:** `process()` in `src/lib.rs` must never allocate, block, or lock a contended mutex. Enforced by convention; no static analysis gate.
- **Atomic bit-packing convention:** `f32` values shared across threads stored as `u32` via `f32::to_bits`/`f32::from_bits` (BPM, hardware delay float); `f64` standalone beat position stored as `u64` bits in `standalone_pos_beats`.
- **Fork dep workflow:** `nice-plug`/`nice-plug-iced`/`nice-plug-xtask` pulled via `git =` deps on `github.com/MysteriousWolf/nice-plug` branch `ethertap` (single patch: `ProcessContext::set_parameter`, used only inside `process()` for momentary-trigger self-reset and telemetry write-back). No vendoring, no local patch-apply step; `cargo build` resolves it like any other git dependency. Fork PR pending upstream.
- **Deterministic substrate:** `.claude/project/deterministic-signals.md`
- **Domain partitioning basis:** vertical slices by runtime concern — audio-thread process loop (core), network I/O worker (network), MIDI clock + bridge + software loopback (midi), GUI render (editor), tooling/CI/test-infra crates (build). Things that break together are grouped together.
