# nice-plug migration

## Goal

Replace the vendored/patched nih-plug stack (`vendor/nih-plug`, `vendor/baseview`, `vendor/copypasta`, 17 patch files, `scripts/setup.sh` patch dance) with **nice-plug 0.2.3** via a thin git fork (3 patches), GUI rewritten on **nice-plug-iced** (iced 0.14 Elm-style). Zero behavior change to param surface, telemetry, momentary triggers, standalone mode, or sync/telemetry philosophy.

## Non-goals

- GUI redesign — reproduce existing 360×280 layout and standalone chrome as-is.
- New plugin formats (CLAP/AU). VST3 only.
- Changing sync/telemetry semantics ("Status-Aware Proxy" philosophy untouched).
- Migrating shimmers or unifying its stack.
- Full atomics refactor of the 8 telemetry params (rejected approach P-A — see design doc).

## Success criteria

- `cargo test --workspace` green, including VST3-build-gated harness suites (`tests/harness_e2e.rs`, `tests/sync_matrix.rs`, `tests/midi_clock_tests.rs`, `tests/vst_runtime_integration.rs`).
- VST3 bundle step succeeds: `cargo run -p xtask -- bundle ethertap --release` produces a loadable bundle.
- Param surface unchanged — same param IDs/count. Verified by the coverage guard `param_id_set_is_accounted_for` in `tests/vst_runtime_integration.rs:73` (fails if the host-visible param id set drifts).
- Zero vendored crates remain: `vendor/`, `patches/`, `scripts/setup.sh` patch-apply step, copypasta shim all deleted.
- RT-safety contract intact: no new heap allocations, blocking I/O, or contended-mutex locks introduced in `process()` (`src/lib.rs`). Manual review gate (no static-analysis tooling exists) — reviewer re-checks the `process()` diff against `CLAUDE.md` Real-Time Safety section at every checkpoint touching `src/lib.rs`.
- All 14 audio-thread `set_parameter` call sites (8 telemetry + 5 momentary self-resets + 1 audit retrigger — `src/lib.rs:629-704`, `:1021-1072`) preserved with identical behavior.
- Standalone binary (`ethertap-gui`, `src/bin/gui_test.rs`) builds and launches under the `standalone` feature.

## Approaches

See `docs/design/nice-plug-migration.md` — Approaches tables (GUI adapter: G-A/G-B/G-C; patched-API strategy: P-A/P-B/P-C).

## Recommendation

**G-A + P-B**: nice-plug 0.2.3 via thin git fork (3 patches: `ProcessContext::set_parameter`, `Transport` visibility + `set_song_position`, `ParamPtr` pub methods), GUI on nice-plug-iced. Full rationale in design doc's Recommendation section.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Fork creation + patch porting + smoke build | new fork repo (external; hosted on user's GitHub per design assumption — override before starting if Codeberg preferred); local: none yet | atomic-builder | n/a (external repo) | Fork builds standalone; 3 patches (`set_parameter`, `Transport` visibility/`set_song_position`, `ParamPtr` pub) applied as commits; an in-fork example plugin compiles against the patched crates |
| 2 | Cargo rewire + core migration, editor stubbed | `Cargo.toml`, `xtask/Cargo.toml`, `vst-runtime/Cargo.toml`, `src/lib.rs`, `src/params.rs`; `src/editor.rs` behind a temporary stub/gate | atomic-builder | ~6 | `cargo check --workspace` green with editor stubbed; `[patch]`/vendor deps replaced by fork `git =` dep; `network.rs`/`midi_clock.rs`/`midi_watcher.rs` untouched |
| 3 | editor.rs rewrite (iced 0.4 stateful → nice-plug-iced 0.14 Elm-style) | `src/editor.rs`, `src/params.rs` (IcedState sizing only) | atomic-builder | ~2 | Plugin builds and bundles (`cargo run -p xtask -- bundle ethertap --release`); 360×280 layout reproduced; 23 Message variants and all styling carried over behaviorally; nice-plug-iced built-in param widgets (e.g. ParamSlider) used where they fit the existing layout instead of hand-rolled equivalents (user preference, 2026-08-05) |
| 4 | Standalone binary + gui_test parity | `src/bin/gui_test.rs`, `scripts/gui_test.sh`, `Cargo.toml` (`standalone` feature) | atomic-builder | ~3 | `ethertap-gui` builds and launches under `standalone`; baseview 0.2 parity verified empirically — window embedding, live resize, ARM64 (no crash) |
| 5 | vst-runtime harness migration | `vst-runtime/src/lib.rs`, `tests/common/mod.rs`, `tests/harness_e2e.rs`, `tests/sync_matrix.rs`, `tests/midi_clock_tests.rs`, `tests/vst_runtime_integration.rs` | atomic-builder | ~6 | Full `cargo test --workspace` green; coverage guard `param_id_set_is_accounted_for` passes unchanged; `Transport::new()`/`set_song_position()`/`ParamPtr` pub methods resolve against fork |
| 6 | Teardown + docs/signals refresh | delete `vendor/`, `patches/`; `scripts/setup.sh` rewrite; `.github/workflows/ci.yml`; `CLAUDE.md`, `.claude/project/signals.md` | atomic-surgeon | ~6 | CI green on 3-OS matrix; no `vendor/`/`patches/`/copypasta shim references remain; signals/CLAUDE.md reflect nice-plug dep story |

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| baseview 0.2 standalone parity unverified (window embedding, live resize, ARM64 crash guard) | Standalone regressions ship silently if changelog claims don't hold | Empirical verification gated at checkpoint 4; fallback is a 4th fork patch if a upstream gap surfaces |
| nice-plug-iced adapter API gaps vs. `WindowSubs::on_frame` / `WindowQueue` telemetry-poll needs | GUI can't drive the 3 s telemetry refresh or LED pulse timing | Confirm adapter equivalents during checkpoint 3 before committing to full rewrite; escalate to fork patch if no equivalent exists |
| Fork rebase burden until upstream PRs land | Every nice-plug release requires manual rebase of 3 patches | Batch-submit both PRs (`set_parameter`, `test-support` visibility) promptly after migration lands; track fork drift as ongoing maintenance, not one-time cost |
| iced 0.14 layout drift vs. pixel-exact 360×280 current layout | Visual regression not caught by `cargo test` | Manual visual diff against current GUI screenshot at checkpoint 3; no automated layout test exists to gate this |

## Change log

<!-- New entries go above this line. Format: ### YYYY-MM-DD — <title> / **What changed** / **Why** / (if behavior changed) **Superseded:** -->
