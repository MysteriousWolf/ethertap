# build

## What it does

Produces a cross-platform VST3 bundle via `cargo nih-plug bundle` (wrapped by xtask). The `scripts/build.sh` script packages platform artifacts into `dist/`. CI uses GitHub Actions to build on macOS (universal), Windows, and Linux on push to `v*` tags or manual dispatch. Setup requires vendoring baseview and nih-plug and applying patches before first build.

## Artifacts

- `scripts/setup.sh` — clone vendor deps (baseview, nih-plug), apply patches in `patches/`
- `scripts/build.sh` — build + package to `dist/`; `--universal` flag for macOS lipo
- `scripts/clean.sh` — remove build artifacts
- `scripts/gui_test.sh`, `scripts/gui_test_no_mixer.sh`, `scripts/gui_test_with_mock.sh` — run standalone binary with/without mock mixer
- `scripts/mock_ethertap.py`, `scripts/mock_mixer.py`, `scripts/mock_midi_sink.py` — Python mock servers for integration testing
- `scripts/run_mock_midi_sink.sh` — launches mock MIDI sink

## CLI code

- `xtask/src/main.rs` — 3-line shim; delegates bundle to `cargo nih-plug bundle`
- `xtask/Cargo.toml` — `xtask` crate, version 0.1.0

## Docs

- `README.md` — build prerequisites, `setup.sh` → `cargo run -p xtask` workflow, universal binary instructions
- `PATCHES.md` — lists vendored patches: baseview (ARM64 macOS crash fix), nih-plug (ProcessContext::set_parameter not implemented)
- `.github/workflows/release.yml` — CI matrix: macos-latest (universal), windows-latest, ubuntu-latest; triggered on `v*` tag push or `workflow_dispatch`

## Coupling

- `Cargo.toml` patches both `baseview` and `nih-plug` git sources to `vendor/` paths; vendor dirs populated by `scripts/setup.sh`.
- `patches/baseview/` and `patches/nih-plug/` must be re-applied after any upstream vendor update.
- `standalone` feature flag required for `src/bin/gui_test.rs` binary and for default IP to use `127.0.0.1`.

## Conventions worth knowing

- Release target: `lto = "thin"`, `strip = "symbols"`, `opt-level = 3`.
- Version sourced from `Cargo.toml` unless `VERSION` env var is set (CI sets from git tag).
- Output paths: `target/bundled/ethertap.vst3` (xtask), `dist/ethertap-{version}-{platform}.{zip|tar.gz}` (build.sh).
- macOS universal: builds `aarch64-apple-darwin` + `x86_64-apple-darwin` separately, merges with `lipo`.
- `benches/core.rs` uses Criterion 0.5 with `html_reports`; run via `cargo bench`.
