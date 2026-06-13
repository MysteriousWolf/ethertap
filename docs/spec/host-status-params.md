# Host status params — expanded read-only telemetry

## Goal

Expose EtherTap's internal sync/connection telemetry to the VST host as read-only params, extending the existing `is_connected` / `is_matched` pattern (`src/lib.rs` §3b: audio-thread `context.set_parameter()` guarded by shadow values).

## Non-goals

- No new `rate_synced` param — `is_matched` *is* the rate-sync status (connected && hardware delay float matches host BPM). Documented, not duplicated.
- No verified "phase synced" state — the mixer has no phase read-back; only "hard reset armed" is knowable.
- No editor layout changes beyond the standalone DAW-shell PARAMETERS OUT footer chips.
- No new cross-thread channels or atomics for state that already reaches `process()`.

## Success criteria

- [ ] New host-visible params exist with ids: `sync_status`, `phase_reset_pending`, `hardware_bpm`, `compatible_slot_count`, `midi_bridge_connected`.
- [ ] `sync_status` reports: Offline (not connected) / Connected (idle, not matched) / Syncing (settling, retry pending, or hard reset armed) / Synced (matched).
- [ ] `hardware_bpm` reports `20.0 / hardware_float` when telemetry present (float > 0.0001), else 0.0.
- [ ] `compatible_slot_count` equals popcount of the `compatible_slots` bitmask (0–8).
- [ ] `phase_reset_pending` mirrors the quantised Hard Reset armed state (`hr_pending`).
- [ ] `midi_bridge_connected` mirrors the MIDI worker's open-connection flag.
- [ ] All updates happen in `process()` via `context.set_parameter()`, guarded by shadow values — no redundant per-buffer host notifications (test asserts no set when value unchanged is not directly observable; shadow-guard verified by unit test on transitions).
- [ ] RT safety holds: no allocation, no locking, no blocking added to `process()`.
- [ ] `tests/vst_runtime_integration.rs::param_id_set_is_accounted_for` updated and green.
- [ ] Standalone DAW shell PARAMETERS OUT footer shows chips for the new params.
- [ ] `lib.rs::read_only_params_update_on_process` extended to cover new params' transitions.
- [ ] `cargo test --workspace` green; clippy both feature sets `-D warnings` clean.

## Recommendation

Follow the proven §3b pattern exactly (`src/lib.rs:563-598`): read shared atomics / audio-thread fields once per buffer, compare against a shadow field, `set_parameter` only on change. All required state already reaches `process()`:

| Param | Source | Type sketch |
|-------|--------|-------------|
| `sync_status` | `conn_status`, `in_sync`, `bpm_is_settling`, `on_change_retry_pending`, `hr_pending` | `EnumParam<SyncStatus>` (IntParam 0–3 acceptable if EnumParam misbehaves through the audio-thread set_parameter path) |
| `phase_reset_pending` | `self.hr_pending` (audio-thread field) | `BoolParam` |
| `hardware_bpm` | `hardware_float` atomic (`f32::from_bits`) | `FloatParam`, range 0–999, default 0 (= no telemetry) |
| `compatible_slot_count` | `compatible_slots` atomic, `count_ones()` | `IntParam` 0–8 |
| `midi_bridge_connected` | `midi_bridge_connected` atomic | `BoolParam` |

`hardware_bpm` shadow comparison needs an epsilon (telemetry float jitter); update only on meaningful change (> 0.01 BPM).

Precedence inside `sync_status`: Offline if not connected; else Synced if matched; else Syncing if any of settling/retry/HR-pending; else Connected.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Add 5 read-only params + §3b wiring + shadow guards + unit tests | `src/params.rs`, `src/lib.rs` (struct, Default, process §3b, tests) | atomic-builder | ~2 | extended `read_only_params_update_on_process` + new `sync_status` transition test green |
| 2 | Update param-id coverage guard + DAW shell PARAMETERS OUT chips | `tests/vst_runtime_integration.rs`, `src/editor.rs` (daw_shell, standalone-gated) | atomic-surgeon | 2 | `param_id_set_is_accounted_for` green; standalone build compiles |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| `EnumParam` via audio-thread `set_parameter` behaves differently than `BoolParam` in some hosts | low | Same nih-plug internal path as existing pattern; fallback to IntParam if harness test misbehaves |
| `hardware_bpm` float jitter spams host notifications | med | Epsilon-guarded shadow (>0.01 BPM delta) |
| Host writes to read-only params (they're regular params) | low | Same exposure as existing `is_connected`/`is_matched` — audio thread overwrites next buffer; accepted status quo |

## Change log

<!-- Populated on first amendment after the spec is approved. -->

## Implementation log

### shipped — 2026-06-12

Built across 2 iterations of /subagent-implementation on branch `host-status-and-te-reskin`. Commits (chronological):

- `b081063` — CP-1 `SyncStatus` enum + 5 read-only params + §3b shadow-guarded wiring + unit tests (`read_only_params_update_on_process` extended, `sync_status_precedence_transitions` added)
- `f2a0926` — CP-2 param-id coverage guard + standalone DAW shell PARAMETERS OUT chips (`daw_value_chip`, `sync_status_label` helpers)

**Out-of-scope work performed during this build:**
- none

**Unforeseens — surprises that emerged during implementation:**
- Iteration 1 reviewer caught a `cargo fmt` violation (CI hard gate); fixed by orchestrator-run `cargo fmt --all` rather than a re-dispatch.
- `cargo test --workspace` was intentionally red between CP-1 and CP-2 (param-id coverage guard), as the checkpoint split predicted.

**Deferred items still open:**
- none

**Squashed to 2f92ada — 2026-06-13.** Per-iteration SHAs above are historical (unreachable from any branch).

**Merged into main as 13c2265 — 2026-06-13.**
