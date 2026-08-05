# core

## What it does

EtherTap is a zero-audio VST3 plugin (`nice-plug` 0.2.3, Rust) that bridges DAW BPM to Behringer X32 / Midas M32 delay times via UDP/OSC. The `process()` audio callback dispatches sync commands to the network worker via lock-free `crossbeam_channel`s; it never allocates, blocks, or locks a contended mutex. Sync can fire on BPM settle (500 ms debounce), on every beat, or only on manual trigger. In standalone mode (no DAW host / `standalone` feature), `process()` falls back to three shared atomics (`standalone_bpm`, `standalone_playing`, `standalone_pos_beats`) driven by the editor transport panel instead of `context.transport()`.

## Artifacts

- `src/lib.rs` — plugin struct (`EtherTap`), `Plugin` impl (`initialize`, `process`, `editor`), `Vst3Plugin` impl + `nice_export_vst3!`, BPM settle state machine, quantised Hard Reset scheduler (PS1–PS4 bar/time-sig/loop detection), reconnect-sync logic, standalone transport handling, `dispatch()` slot-fanout helper, `adopt_restored_state()` (session-restore reconciliation), `TestHandles` (test-only shared-Arc accessor)
- `src/params.rs` — `EtherTapParams` (`#[derive(Params)]` block), `SyncMode`, `SyncStatus`, `Ppq` enums, persisted config fields; window size conditional: 360×280 in VST3 mode, 500×620 with `standalone` feature

## CLI code

- `src/bin/gui_test.rs` — standalone binary entry point (requires `standalone` feature); calls `nice_plug::nice_export_standalone::<ethertap::EtherTap>()`
- `xtask/src/main.rs` — thin xtask shim; calls `nice_plug_xtask::main()`

## Docs

- `README.md` — install, connect, sync-mode table, BPM math, OSC reference, architecture diagram
- `CLAUDE.md` — RT-safety contract, inter-thread channel table, nice-plug-iced API notes, OSC quick reference
- `docs/spec/host-status-params.md` — contract for the 5 read-only telemetry params (`sync_status`, `phase_reset_pending`, `hardware_bpm`, `compatible_slot_count`, `midi_bridge_connected`), shipped and merged (`13c2265`)
- `docs/design/daw-controls-overhaul.md` — standalone transport Stop/Pause split design; documents the `standalone_pos_beats` read-modify-write race in `process()` and the `standalone_stop_trigger` one-shot-atomic fix

## Coupling

- Changes to `SyncMode`, `SyncStatus`, `Ppq`, or param IDs in `src/params.rs` affect automation lanes in existing DAW sessions; also referenced by `src/editor.rs` (domain: editor) for UI rendering and `src/lib.rs` for sync dispatch.
- Changes to `EtherTap` struct fields shared as `Arc<Atomic*>` require matching updates in `src/editor.rs` (domain: editor, reads) and `src/network.rs` / `src/midi_clock.rs` (domain: network / midi, writes).
- `standalone_bpm`, `standalone_playing`, `standalone_pos_beats`, `standalone_stop_trigger` atomics are written by the editor transport panel (domain: editor) and read/consumed by `process()`; adding new standalone transport state requires updating both.
- `NetworkCommand`/`NetworkStatus` variants dispatched from `process()` (via `cmd_tx`/`status_rx`) are defined in `src/network.rs` (domain: network); adding a new command or status variant requires a matching arm in `process()`.
- `midi_clock::ClockMsg` sent via `midi_clock_tx` and `midi_clock::AtomicClockStats` read via `midi_clock_stats` are defined in `src/midi_clock.rs` (domain: midi); MIDI clock output timing in `process()` depends on that worker's message shape.
- `process()` RT contract: any new code path in `process()` must not allocate or block; violation breaks RT safety across the entire plugin.

## Conventions worth knowing

- `f32` BPM and delay float are stored in atomics as `u32` bit patterns via `f32::to_bits`/`f32::from_bits`; `standalone_pos_beats` is `f64` stored as `u64` bits.
- BPM↔delay formula: `bpm_to_float(bpm) = 20.0/bpm` (`osc::bpm_to_float`); `float_to_bpm(f) = 20.0/f` (`osc::float_to_bpm`).
- `SETTLE_MS = 500` — BPM must be stable for 500 ms before OnChange fires. `BPM_SETTLE_THRESHOLD = 0.01` — minimum delta to restart the settle timer. `BPM_MIDI_THRESHOLD = 0.5` — minimum delta to send `midi_clock::ClockMsg::BpmChanged`.
- `sync_status` precedence (highest first): Offline (not connected) > Synced (matched) > Syncing (settling / retry pending / Hard Reset armed) > Connected (idle).
- Read-only host params (`is_connected`, `is_matched`, `sync_status`, `phase_reset_pending`, `hardware_bpm`, `compatible_slot_count`, `midi_bridge_connected`) are written via `context.set_parameter()` guarded by shadow fields (`last_*`) to avoid redundant host notifications; `force_status_publish` (set by `initialize()`/`adopt_restored_state()`) forces one full republish on the first buffer after load/reactivation, since the host restores these params from session state and that restored value must not be trusted.
- Momentary trigger params (`connect_to_last`, `disconnect`, `force_sync_rate`, `force_sync_phase`, `audit_slots`) self-reset: `process()` consumes the rising edge (compared against a `prev_*` field) then writes the param back to `false` via `context.set_parameter()`.
- `standalone_stop_trigger` follows a different pattern than the other triggers: it is an `Arc<AtomicBool>` set by the editor and consumed unconditionally every buffer via `swap(false, Ordering::AcqRel)` in `process()`, independent of and before the `playing` gate — because `standalone_pos_beats` is read-modify-written by `process()` every buffer while playing, a plain editor-thread `store()` could otherwise race and be silently overwritten.
- `dispatch(bpm, hard_reset)` builds a `[None::<u8>; 8]` fixed-size stack array (no heap allocation) to fan a sync command out to all compatible slots (when `all_slots_atom` is set) or the single selected `fx_slot`; unaudited slots (`i32::MIN`) and slots whose type isn't in `fx_type_to_bit`'s known set are included by default.
- Standalone mode sentinel inside the MIDI clock section: `transport.pos_beats()` forced to `None` under `#[cfg(feature = "standalone")]` so the dummy CPAL backend's synthetic transport position never drives MIDI clock timing; DAW builds use the real `transport.pos_beats()`.
- Default params: target IP `192.168.1.100` (standalone: `127.0.0.1`), port `10023`, FX slot 1, rate sync = OnChange, phase sync = Manual, `all_slots` = true, `midi_clock_enabled` = true, `midi_clock_ppq` = 24.
- All VST3-automatable fields use `#[id]`; persisted-but-not-automatable fields (window state, target IP/port, last device, last slot types, MIDI output device) use `#[persist]`.
- `TestHandles` (`EtherTap::test_handles()`) exposes live shared `Arc`s (conn_status, hardware_float, compatible_slots, occupied_slots, slot_types, connected_device, midi_clock_stats, midi_clock_drop_count, device_change_tx, midi_bridge_connected) for harness-driven integration tests without reaching into private fields.
