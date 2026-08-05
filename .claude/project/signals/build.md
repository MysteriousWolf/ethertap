# build

## What it does

Produces a cross-platform VST3 bundle via `nice-plug-xtask` (wrapped by the `xtask` crate). The `scripts/build.sh` script packages platform artifacts into `dist/`. CI uses GitHub Actions to build on macOS (universal), Windows, and Linux on push to `v*` tags or manual dispatch. No vendoring/setup step: `nice-plug`/`nice-plug-iced`/`nice-plug-xtask` are plain `git =` deps on a fork (`github.com/MysteriousWolf/nice-plug`, branch `ethertap`), resolved by `cargo build` like any other dependency.

## Artifacts

- `scripts/build.sh` — build + package to `dist/`; `--universal` flag for macOS lipo
- `scripts/clean.sh` — remove `target/`
- `scripts/gui_test.sh` — run the standalone binary (`cargo run --bin ethertap-gui --features standalone`)
- `mock-suite` crate — mock mixer + MIDI clock sink (library for tests, `cargo run -p mock-suite` for the TUI); replaced the retired Python mock scripts

## CLI code

- `xtask/src/main.rs` — 3-line shim; delegates bundle to `nice_plug_xtask::main()`
- `xtask/Cargo.toml` — `xtask` crate, version 0.1.0; `nice-plug-xtask` fork dep

## Docs

- `README.md` — build prerequisites, `cargo run -p xtask` workflow, universal binary instructions
- `.github/workflows/release.yml` — CI matrix: macos-latest (universal), windows-latest, ubuntu-latest; triggered on `v*` tag push or `workflow_dispatch`

## Coupling

- `Cargo.toml` depends on `nice-plug`/`nice-plug-iced` via `git =` on the `ethertap` fork branch (1 patch commit: `ProcessContext::set_parameter`); `xtask/Cargo.toml` depends on `nice-plug-xtask` from the same fork/branch. No vendor dirs, no local patch-apply step.
- `standalone` feature flag required for `src/bin/gui_test.rs` binary and for default IP to use `127.0.0.1`.

## Conventions worth knowing

- Release target: `lto = "thin"`, `strip = "symbols"`, `opt-level = 3`.
- Version sourced from `Cargo.toml` unless `VERSION` env var is set (CI sets from git tag).
- Output paths: `target/bundled/ethertap.vst3` (xtask), `dist/ethertap-{version}-{platform}.{zip|tar.gz}` (build.sh).
- macOS universal: builds `aarch64-apple-darwin` + `x86_64-apple-darwin` separately, merges with `lipo`.
- `benches/core.rs` uses Criterion 0.5 with `html_reports`; run via `cargo bench`.
