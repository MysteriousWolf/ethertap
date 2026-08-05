# nice-plug migration

## Goal

Replace the vendored/patched nih-plug stack (`vendor/nih-plug`, `vendor/baseview`, `vendor/copypasta`, 17 patch files, `scripts/setup.sh` patch dance) with **nice-plug 0.2.3** via a thin git fork (1 patch: `ProcessContext::set_parameter`), GUI rewritten on **nice-plug-iced** (iced 0.14 Elm-style). Zero behavior change to param surface, telemetry, momentary triggers, standalone mode, or sync/telemetry philosophy.

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

**G-A + P-B**: nice-plug 0.2.3 via thin git fork carrying a single patch (`ProcessContext::set_parameter` + VST3 host-notify), GUI on nice-plug-iced. The other two planned patches proved unnecessary — nice-plug already ships `Transport::new()` pub with pub position fields, and `ParamPtr` setters pub as `_internal_set_normalized_value` / `_internal_update_smoother` (verified in fork clone, 2026-08-05). Full rationale in design doc's Recommendation section.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Fork creation + patch porting + smoke build | new fork repo (user's GitHub, user-approved 2026-08-05; local clone `~/RustProjects/nice-plug`, branch `ethertap` off `v0.2.3`); local: none yet | orchestrator (subagent dispatch blocked by permission classifier) | n/a (external repo) | Fork builds standalone; 1 patch commit (`ProcessContext::set_parameter` default impl + VST3 wrapper impl + inner host-notify); an in-fork example plugin compiles against the patched crates |
| 2 | Cargo rewire + core migration, editor stubbed | `Cargo.toml`, `xtask/Cargo.toml`, `vst-runtime/Cargo.toml`, `src/lib.rs`, `src/params.rs`; `src/editor.rs` behind a temporary stub/gate | atomic-builder | ~6 | `cargo check -p ethertap` green with editor stubbed (workspace-wide check deferred to checkpoint 5 — vst-runtime still targets old APIs until then); `[patch]`/vendor deps replaced by fork `git =` dep; `network.rs`/`midi_clock.rs`/`midi_watcher.rs` untouched |
| 3 | editor.rs rewrite (iced 0.4 stateful → nice-plug-iced 0.14 Elm-style) | `src/editor.rs`, `src/params.rs` (IcedState sizing only) | atomic-builder | ~2 | Plugin builds and bundles (`cargo run -p xtask -- bundle ethertap --release`); 360×280 layout reproduced; 23 Message variants and all styling carried over behaviorally; nice-plug-iced built-in param widgets (e.g. ParamSlider) used where they fit the existing layout instead of hand-rolled equivalents (user preference, 2026-08-05) |
| 4 | Standalone binary + gui_test parity | `src/bin/gui_test.rs`, `scripts/gui_test.sh`, `Cargo.toml` (`standalone` feature) | atomic-builder | ~3 | `ethertap-gui` builds and launches under `standalone`; baseview 0.2 parity verified empirically — window embedding, live resize, ARM64 (no crash) |
| 5 | vst-runtime harness migration | `vst-runtime/src/lib.rs`, `tests/common/mod.rs`, `tests/harness_e2e.rs`, `tests/sync_matrix.rs`, `tests/midi_clock_tests.rs`, `tests/vst_runtime_integration.rs` | atomic-builder | ~6 | Full `cargo test --workspace` green (vs. known baseline: `identity_mismatch_rescans_to_matching_device` fails env-sensitively on dev LAN — see FOLLOWUPS F-1); coverage guard `param_id_set_is_accounted_for` passes unchanged; harness migrated to upstream APIs: pub `Transport` fields (direct writes replace `set_song_position()`), `ParamPtr::_internal_set_normalized_value`/`_internal_update_smoother` |
| 6 | Teardown + docs/signals refresh | delete `vendor/`, `patches/`; `scripts/setup.sh` rewrite; `.github/workflows/ci.yml`; `CLAUDE.md`, `.claude/project/signals.md` | atomic-surgeon | ~6 | CI green on 3-OS matrix; no `vendor/`/`patches/`/copypasta shim references remain; signals/CLAUDE.md reflect nice-plug dep story |

## Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| baseview 0.2 standalone parity unverified (window embedding, live resize, ARM64 crash guard) | Standalone regressions ship silently if changelog claims don't hold | Empirical verification gated at checkpoint 4; fallback is a 4th fork patch if a upstream gap surfaces |
| nice-plug-iced adapter API gaps vs. `WindowSubs::on_frame` / `WindowQueue` telemetry-poll needs | GUI can't drive the 3 s telemetry refresh or LED pulse timing | Confirm adapter equivalents during checkpoint 3 before committing to full rewrite; escalate to fork patch if no equivalent exists |
| Fork rebase burden until upstream PR lands | Every nice-plug release requires manual rebase of 1 patch | Submit the `set_parameter` PR promptly after migration lands (fills an acknowledged upstream TODO); track fork drift as ongoing maintenance, not one-time cost |
| iced 0.14 layout drift vs. pixel-exact 360×280 current layout | Visual regression not caught by `cargo test` | Manual visual diff against current GUI screenshot at checkpoint 3; no automated layout test exists to gate this |

## Change log

### 2026-08-05 — Fork shrinks to a single patch

**What changed** Checkpoint 1 carries 1 fork commit instead of 3; checkpoint 5 migrates the harness to upstream APIs instead of relying on visibility patches; checkpoint 5's green criterion qualified against a known env-sensitive baseline failure (F-1); checkpoint 2's compile gate narrowed from `cargo check --workspace` to `cargo check -p ethertap` (vst-runtime cannot compile against unpatched APIs until its checkpoint-5 migration, so a workspace-wide gate at checkpoint 2 was unsatisfiable).
**Why** Fork clone inspection (v0.2.3) showed nice-plug already upstreamed what two patches provided: `Transport::new()` is pub with pub position fields (`crates/nice-plug-core/src/context/process.rs:125-156`), and `ParamPtr` setters are pub as `_internal_set_normalized_value`/`_internal_update_smoother` (`crates/nice-plug-core/src/params/internals.rs:79-81`). Baseline run also surfaced `identity_mismatch_rescans_to_matching_device` failing on the dev LAN before any migration change.
**Superseded:** 3-patch fork (`set_parameter`, `Transport` visibility + `set_song_position`, `ParamPtr` pub); unqualified "workspace green" criterion.

<!-- New entries go above this line. Format: ### YYYY-MM-DD — <title> / **What changed** / **Why** / (if behavior changed) **Superseded:** -->

## Implementation log

### complete (pending ship) — 2026-08-05

Built across 7 iterations of /subagent-implementation on branch `nice-plug-migration` (worktree). Commits (chronological):

- `eb4d3621` (external, github.com/MysteriousWolf/nice-plug branch `ethertap`) — CP-1 fork: v0.2.3 + single `ProcessContext::set_parameter` patch
- `f684a3a` — spec amendment: fork shrinks 3→1 patches
- `6eda0c6` — CP-2 dependency swap + core migration, editor stubbed
- `fc89e57` — CP-3 editor rewrite on nice-plug-iced 0.2 / iced 0.14
- `3341716` — CP-3 polish: dedicated timer thread, 30 ms tick
- `95e0375` — CP-4 standalone binary (`nice_export_standalone`), launch smoke green
- `1be8cf7` — CP-5 vst-runtime harness on upstream APIs, full workspace green
- `3fe7b50` — CP-6 teardown: patches/, PATCHES.md, setup.sh, copypasta shim, CI vendor steps deleted; CLAUDE.md/signals rewritten

**Out-of-scope work performed during this build:**
- `release.yml` vendor-step removal (brief named only ci.yml; glob wording covered it — a tag build would have called deleted setup.sh)
- Dead `RUSTSEC-2021-0019` ignore dropped from `.cargo/audit.toml` (xcb chain left dep tree with copypasta)

**Unforeseens — surprises that emerged during implementation:**
- nice-plug v0.2.3 had already upstreamed 2 of the 3 planned patches (pub `Transport` + `_internal_*` setters) — fork delta is 1 commit, not 3
- No `ParamSlider` exists in nice-plug-iced 0.2 — user's slider preference had no applicable slot (layout is discrete controls + one pick_list)
- `WindowSubs::on_frame` has no adapter equivalent; replaced with a dedicated-thread tick subscription (30 ms), reviewer-hardened against aliasing/leaks
- Setup.sh multibyte-ellipsis bash bug under C locale (background-shell only); moot after teardown deleted the script
- Baseline test `identity_mismatch_rescans_to_matching_device` is LAN-sensitive flaky (predates migration) — F-1

**Deferred items still open:**
- F-1: flaky LAN-sensitive reconnect test (pre-existing) — triaged at finalize
- F-2: in-DAW VST3 embedding + live window resize not empirically verified (manual DAW pass needed) — triaged at finalize
- Upstream PR of the `set_parameter` patch to codeberg.org/RustAudio/nice-plug (retires the fork) — user-driven, needs Codeberg identity
- CI 3-OS matrix green verifies on first push (local 1-OS equivalent gates all green 2026-08-05)

**Squashed to 9a1efda — 2026-08-05.** Per-iteration SHAs above are historical (unreachable from any branch).

**Merged into main as a950d1e — 2026-08-05** (fast-forward; squash commit 9a1efda + spec-log and lint/signals follow-ups).
