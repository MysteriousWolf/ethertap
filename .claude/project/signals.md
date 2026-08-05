# Project signals

## Framework & runtime

- Language: Rust (stable), edition 2021
- Plugin format: VST3 via `nice-plug` 0.2.3 (git fork, `github.com/MysteriousWolf/nice-plug` branch `ethertap`, pending upstream PR)
- GUI: `nice-plug-iced` (Elm-style, iced 0.14; 360×280 in VST3 mode, 500×620 with `standalone` feature)
- Async/concurrency: `crossbeam-channel` 0.5 (bounded lock-free queues), `parking_lot` 0.12 (uncontended Mutex only), `std::sync::atomic`
- OSC: `rosc` 0.10 (encoder + decoder)
- MIDI: `midir` 0.9; macOS: `coremidi` 0.9 + `core-foundation` 0.9
- Crate version: 0.2.0; crate type: `cdylib` + `rlib`

## Build / test / lint

| Purpose | Command | Source |
|---------|---------|--------|
| Bundle VST3 | `cargo run -p xtask -- bundle ethertap --release` | `xtask/src/main.rs` |
| Universal macOS | `./scripts/build.sh --universal` | `scripts/build.sh` |
| Run tests | `cargo test --workspace` | `tests/` |
| Benchmarks | `cargo bench` | `benches/core.rs` |
| Standalone GUI test | `./scripts/gui_test.sh` (+ `cargo run -p mock-suite` for the mock) | `scripts/gui_test.sh` |

CI gate: GitHub Actions — `.github/workflows/ci.yml` on every push/PR (3-OS test matrix, clippy both feature sets `-D warnings`, fmt check, bench compile-check, cargo-audit); release on `v*` tag push via `.github/workflows/release.yml`, gated on the full CI workflow.

## Language breakdown

| Language | LOC | Files | % |
|----------|-----|-------|---|
| Rust | 6994 | 14 | 56% |
| Markdown | 3168 | 9 | 25% |
| Python | 1359 | 3 | 11% |
| JSON | 345 | 8 | 2% |
| Shell | 280 | 7 | 2% |
| YAML | 118 | 1 | 1% |
| TOML | 75 | 2 | 0% |

## DevOps & CI

- Release pipeline: tag push to `v*` triggers `.github/workflows/release.yml`; builds artifacts on 3 platforms, uploads as GitHub release assets.
- No deploy step beyond artifact upload (manual install from Releases page).

---

## Domains

| Domain | Repo paths | One-liner | Detail |
|--------|------------|-----------|--------|
| core | `src/lib.rs`, `src/params.rs`, `src/bin/` | Plugin struct, `process()` RT loop, BPM settle, params, sync dispatch, standalone transport atomics | signals/core.md |
| network | `src/network.rs`, `src/osc.rs`, `src/reconnect.rs` | UDP/OSC worker, telemetry, LAN scanner, Backoff | signals/network.md |
| midi | `src/midi_clock.rs`, `src/midi_watcher.rs` | MIDI clock burst worker, bridge passthrough, hot-plug watcher | signals/midi.md |
| editor | `src/editor.rs` | Iced GUI: LEDs, sync controls, FX slot/device pickers, telemetry display, standalone transport panel | signals/editor.md |
| build | `scripts/`, `xtask/`, `.github/`, `Cargo.toml` | VST3 bundle, CI matrix, fork dep story | signals/build.md |

## Cross-cutting

- **Test layout:** `tests/integration_tests.rs` (network worker against `MockMixer`, incl. scan-port discovery), `tests/reconnect_tests.rs` (auto_reconnect: opt-in self-connect, identity verification, rescan retargeting; worker- and harness-level), `tests/osc_tests.rs` (OSC encode/decode), `tests/harness_e2e.rs` + `tests/sync_matrix.rs` + `tests/midi_clock_tests.rs` (vst-runtime harness driving the full plugin against `MockMixer` / virtual MIDI sink; VST3-build-gated), `tests/vst_runtime_integration.rs` (harness sanity + host-param-set coverage guard), `tests/common/mod.rs` (NetworkWorker glue + `harness_util` helpers; `MockMixer`/`SlotState` re-exported from the `mock-suite` crate). Unit tests co-located in `src/reconnect.rs`; `vst-runtime` and `mock-suite` carry their own unit tests (incl. TUI helper math).
- **RT safety contract:** `process()` in `src/lib.rs` must never allocate, block, or lock a contended mutex. Enforced by convention; no static analysis gate.
- **Atomic bit-packing convention:** `f32` values shared across threads stored as `u32` via `f32::to_bits`/`f32::from_bits` (BPM, hardware delay float); `f64` standalone beat position stored as `u64` bits in `standalone_pos_beats`.
- **Fork dep workflow:** `nice-plug`/`nice-plug-iced` pulled via `git =` dep on `github.com/MysteriousWolf/nice-plug` branch `ethertap` (single patch commit: `ProcessContext::set_parameter`). No vendoring, no local patch-apply step; `cargo build` resolves it like any other git dependency. Fork PR pending upstream.
- **Deterministic substrate:** `.claude/project/deterministic-signals.md`
- **Domain partitioning basis:** vertical slices by runtime concern — audio-thread process loop (core), network I/O worker (network), MIDI clock + bridge (midi), GUI render (editor), tooling/CI (build). Things that break together are grouped together.
