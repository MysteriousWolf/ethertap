# Project signals

## Framework & runtime

- Language: Rust (stable), edition 2021
- Plugin format: VST3 via `nih-plug` (git, vendored at `vendor/nih-plug`)
- GUI: `nih-plug-iced` (stateful widget API, 360×280 in VST3 mode, 500×340 with `standalone` feature)
- Async/concurrency: `crossbeam-channel` 0.5 (bounded lock-free queues), `parking_lot` 0.12 (uncontended Mutex only), `std::sync::atomic`
- OSC: `rosc` 0.10 (encoder + decoder)
- MIDI: `midir` 0.9; macOS: `coremidi` 0.9 + `core-foundation` 0.9
- Crate version: 0.2.0; crate type: `cdylib` + `rlib`

## Build / test / lint

| Purpose | Command | Source |
|---------|---------|--------|
| Bundle VST3 | `cargo run -p xtask -- bundle ethertap --release` | `xtask/src/main.rs` |
| Universal macOS | `./scripts/build.sh --universal` | `scripts/build.sh` |
| Setup (vendor + patch) | `./scripts/setup.sh` | `scripts/setup.sh` |
| Run tests | `cargo test` | `tests/` |
| Benchmarks | `cargo bench` | `benches/core.rs` |
| Standalone GUI test | `./scripts/gui_test_with_mock.sh` | `scripts/gui_test_with_mock.sh` |

CI gate: GitHub Actions on `v*` tag push — matrix: macos-latest (universal), windows-latest, ubuntu-latest. Defined in `.github/workflows/release.yml`.

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
| build | `scripts/`, `xtask/`, `.github/`, `patches/`, `Cargo.toml` | VST3 bundle, CI matrix, vendor patch workflow | signals/build.md |

## Cross-cutting

- **Test layout:** `tests/integration_tests.rs` (network worker against `MockMixer`), `tests/osc_tests.rs` (OSC encode/decode), `tests/harness_e2e.rs` + `tests/sync_matrix.rs` + `tests/midi_clock_tests.rs` (vst-runtime harness driving the full plugin against `MockMixer` / virtual MIDI sink; VST3-build-gated), `tests/common/mod.rs` (NetworkWorker glue + `harness_util` helpers; `MockMixer`/`SlotState` re-exported from the `mock-suite` crate). Unit tests co-located in `src/reconnect.rs`; `vst-runtime` and `mock-suite` carry their own unit tests.
- **RT safety contract:** `process()` in `src/lib.rs` must never allocate, block, or lock a contended mutex. Enforced by convention; no static analysis gate.
- **Atomic bit-packing convention:** `f32` values shared across threads stored as `u32` via `f32::to_bits`/`f32::from_bits` (BPM, hardware delay float); `f64` standalone beat position stored as `u64` bits in `standalone_pos_beats`.
- **Vendor patch workflow:** `scripts/setup.sh` clones `vendor/baseview` and `vendor/nih-plug`, applies `patches/`. Must re-run after upstream vendor updates.
- **Deterministic substrate:** `.claude/project/deterministic-signals.md`
- **Domain partitioning basis:** vertical slices by runtime concern — audio-thread process loop (core), network I/O worker (network), MIDI clock + bridge (midi), GUI render (editor), tooling/CI (build). Things that break together are grouped together.
