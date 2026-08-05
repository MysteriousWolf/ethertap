# build

## What it does

Builds and packages the EtherTap VST3 bundle via a Cargo workspace of 5 members (`.`, `xtask`, `vst-runtime`, `mock-suite`, `midi-loopback`); `xtask` wraps the `nice-plug-xtask` bundler, and `scripts/*.sh` cover local dev workflows (build, install, check, format, test, coverage, clean, GUI smoke test). CI (GitHub Actions) runs a 3-OS test matrix, clippy (both feature sets), fmt, bench compile-check, coverage, and `cargo audit` on every push/PR; releases are cut by a manual "Tag Release" workflow that computes the next `<YY>.<N>` tag, pushes it, and triggers the release build/publish workflow, which itself re-runs full CI before building per-platform artifacts.

## Artifacts

- `Cargo.toml` — workspace root; `members = [".", "xtask", "vst-runtime", "mock-suite", "midi-loopback"]`; `ethertap` package deps on `nice-plug`/`nice-plug-iced` (git, fork `MysteriousWolf/nice-plug`, branch `ethertap`) and local path dep `midi-loopback`; dev-deps `vst-runtime`, `mock-suite`, `criterion`
- `scripts/build.sh` — build + package to `dist/`; `--universal` flag for macOS lipo merge of `aarch64-apple-darwin`/`x86_64-apple-darwin`
- `scripts/install.sh` — build (or reuse existing bundle with `--no-build`) and install the VST3 into a plug-in folder; `--dest`, `--yes`, `--universal` flags
- `scripts/check.sh` — clippy (both feature sets) + `cargo fmt --check`, gum-styled streaming output
- `scripts/format.sh` — `cargo fmt --all` with gum-styled status output
- `scripts/test.sh` — `cargo test --workspace` with real-time gum-styled per-test pass/fail output
- `scripts/coverage.sh` — `cargo llvm-cov` workspace coverage, HTML report + text summary, enforces a minimum threshold (default 95%), `--open`/`--threshold` flags
- `scripts/test_standalone_workflow.sh` — end-to-end standalone GUI integration test: launches the real standalone binary plus headless `mock-suite`, verifies connect + BPM sync OSC traffic; `--port`/`--bpm`/`--timeout` flags; macOS-with-display only
- `scripts/gui_test.sh` — run the standalone binary (`cargo run --bin ethertap-gui --features standalone`) without auto-launching the mock mixer
- `scripts/clean.sh` — remove `target/`
- `rust-toolchain.toml` — pins toolchain channel to `stable`
- `.cargo/audit.toml` — `cargo-audit` config; `[advisories] ignore = []` (no suppressed advisories)
- `.github/workflows/ci.yml` — `test` (3-OS matrix), `lint` (clippy default + `standalone` feature, fmt, bench compile-check), `coverage`, `audit` jobs; runs on every push/PR and is callable (`workflow_call`) from `release.yml`
- `.github/workflows/release.yml` — `workflow_dispatch` (dev build, no GitHub Release) or `workflow_call` (from `tag-release.yml`, with `tag`/`prerelease`/`draft` inputs); runs full `ci.yml` first, then builds macOS (universal)/Windows/Linux artifacts via `scripts/build.sh`, then publishes a GitHub Release with AI-generated highlights (`actions/ai-inference@v1`, `openai/gpt-4o`, best-effort) prepended to auto-generated + raw commit-log notes
- `.github/workflows/tag-release.yml` — manual "Tag Release" button; computes the next `v<YY>.<N>` tag (or accepts `version_override`), pushes it, then calls `release.yml` with that tag — the only path that produces a published GitHub Release (a tag push alone does not trigger a release)
- `benches/core.rs` — Criterion 0.8 benchmark (`html_reports` feature), compile-checked by CI's `lint` job (not executed)

## CLI code

- `xtask/src/main.rs` — 3-line shim; delegates to `nice_plug_xtask::main()`
- `xtask/Cargo.toml` — `xtask` crate, version 0.1.0; depends on `nice-plug-xtask` (git, fork, branch `ethertap`) and `anyhow`
- `vst-runtime/src/lib.rs` — `Harness<P: Plugin>`: headless in-process driver for Rust-native `nice_plug::Plugin` impls; mirrors the construct → `initialize` → `process` → teardown sequence `nice_export_standalone` runs, driving the plugin's trait methods directly with no audio backend, no window, and no IPC/serialization; provides `HarnessInitContext`/`HarnessProcessContext` minimal context impls, a `ScenarioResult`/`ScenarioBuilder` scripted-scenario API, and re-exports `ProcessStatus`/`Transport` from `nice_plug::prelude` so tests don't need a separate `nice_plug` dev-dependency; deliberately never references `Plugin::editor()` or `nice_plug_iced`
- `vst-runtime/Cargo.toml` — `vst-runtime` crate, version 0.1.0; depends on `nice-plug` (git, fork, branch `ethertap`, `vst3` feature); dev-dependency on `ethertap` itself (path `..`)
- `mock-suite/src/mixer.rs` — `MockMixer`/`SlotState`: mock X32/M32 UDP OSC server (`/info` heartbeat, `/fx/N/type`, `/fx/N/par/NN` get/set, `/fxrtn/N/mix/on`), used as a test fixture, behind the TUI, and in headless CLI mode
- `mock-suite/src/clock_sink.rs` — `MidiClockSink`: virtual MIDI input port (`cfg(unix)`, CoreMIDI/ALSA only) counting 0xF8 clock bytes for interval/jitter stats
- `mock-suite/src/loopback_sink.rs` — `LoopbackClockSink`: cross-platform sibling of `clock_sink.rs` backed by the `midi-loopback` in-process registry instead of an OS virtual port; compiles and runs on Windows
- `mock-suite/src/sink_state.rs` — `SinkState`: shared MIDI clock accumulation/stats logic (0xF8 counting, BPM sampling, jitter window) used identically by both clock sinks
- `mock-suite/src/tui.rs` — interactive TUI (ratatui/crossterm): tabbed Overview/MIDI Clock/Mixer/Log layout, parity with the retired Python `mock_ethertap.py`
- `mock-suite/src/main.rs` — `mock-suite` binary entry point; bare run → TUI, `--no-tui` → headless mode with `--jsonl` OSC stream and `--expect` assertions (exit 0 satisfied / 1 timeout)
- `mock-suite/src/lib.rs` — crate root; re-exports `MockMixer`/`SlotState`/etc. as the library face consumed by EtherTap's integration tests (`tests/common/mod.rs`)
- `mock-suite/Cargo.toml` — `mock-suite` crate, version 0.1.0, `publish = false`; depends on `midi-loopback` (path `../midi-loopback`), `clap`, `ratatui`, `crossterm`, `rosc`, `parking_lot`, `midir`, `serde_json`, `crossbeam-channel`

## Docs

- `README.md` — "Building from source" section: prerequisites (Rust stable, Git), `cargo run -p xtask -- bundle ethertap --release` → `target/bundled/ethertap.vst3`, and `./scripts/build.sh --universal` → `dist/ethertap-<version>-macos-universal.zip`
- `docs/design/nice-plug-migration.md` — design rationale for dropping the vendored-nih-plug + `scripts/setup.sh` patch-reapply workflow in favor of a plain `git =` fork dependency
- `docs/spec/nice-plug-migration.md` — implementation contract for the nih-plug → nice-plug migration
- `docs/design/vst-host-runtime.md` / `docs/spec/vst-host-runtime.md` — design/spec for the `vst-runtime` crate: a headless single-plugin driver for directed test automation, explicitly not a general-purpose multi-plugin DAW; spec's checkpoint table is the origin of the `vst-runtime` workspace member and the `Harness<P: Plugin>` API

## Coupling

- `xtask`, `vst-runtime`, and the `ethertap` package's `nice-plug`/`nice-plug-iced` dependencies all pin the same fork branch (`MysteriousWolf/nice-plug`, branch `ethertap`) — bumping or unpinning one is a workspace-wide change touching **core** (`src/lib.rs`, `src/params.rs`) and **editor** (`src/editor.rs`, which depends on `nice-plug-iced`).
- `vst-runtime` dev-depends on `ethertap` (path `..`) and drives `Plugin` trait methods directly — changes to `Plugin`/`Params` impls in **core** can break `vst-runtime`'s harness or the integration tests that use it (`tests/harness_e2e.rs`, `tests/sync_matrix.rs`, `tests/midi_clock_tests.rs`, `tests/vst_runtime_integration.rs`).
- `mock-suite` depends on `midi-loopback` (path dep) for `LoopbackClockSink`; `midi-loopback` is itself a workspace member (`Cargo.toml` `members` list) but its internals belong to the **midi** domain, not this one — only its presence in the build graph is a build-domain fact.
- `mock-suite`'s `MockMixer`/`SlotState` are re-exported through `tests/common/mod.rs` as the shared fixture for **network** domain integration tests (`tests/integration_tests.rs`, `tests/reconnect_tests.rs`).
- CI's Linux system-library install list (ALSA, X11, GL, xcb, jack dev packages) exists because `midir` needs ALSA headers and `nice-plug-iced`/baseview need X11+GL — a change to GUI (**editor**) or MIDI (**midi**) backend deps can require updating that apt-get list in `ci.yml` and `release.yml`.

## Conventions worth knowing

- Release profile (`Cargo.toml`): `lto = "thin"`, `strip = "symbols"`, `opt-level = 3`.
- Versioning scheme for tagged releases: `v<YY>.<N>` (two-digit year, sequence number starting at 1 per year), computed automatically by `tag-release.yml` from existing `v<YY>.*` tags unless `version_override` is given.
- `VERSION` env var overrides the version baked into `dist/` filenames by `scripts/build.sh`; CI's release job sets it from the git tag (`inputs.tag`).
- A tag push alone does **not** publish a release — `tag-release.yml`'s manual `workflow_dispatch` button is the single path; it pushes the tag and then calls `release.yml`, which itself calls `ci.yml` as a required gate before building.
- All gum-based scripts (`check.sh`, `format.sh`, `test.sh`, `coverage.sh`, `clean.sh`, `gui_test.sh`) fall back to plain `echo`-based output helpers when `gum` is not on `PATH`.
- `scripts/coverage.sh` requires `cargo-llvm-cov` and the `llvm-tools-preview` rustup component; default coverage threshold is 95%.
- `rust-toolchain.toml` pins `channel = "stable"` (no pinned patch version); CI installs Rust via `dtolnay/rust-toolchain@stable` separately, so the two are independent but intended to track the same channel.
- `.cargo/audit.toml` currently ignores zero advisories — `cargo audit` in CI fails on any unignored advisory.
- No vendor directory and no patch-reapply setup step exist in the current build (the nice-plug migration removed `scripts/setup.sh` and the vendored `nih-plug`/`baseview` clones described in `docs/design/nice-plug-migration.md`); dependencies resolve as plain `git =` deps like any other Cargo dependency.
