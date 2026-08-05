# network

## What it does

`NetworkWorker` (`src/network.rs`) runs on a dedicated OS thread and owns all UDP I/O: sends OSC to the X32/M32, maintains a 5 s `/info` heartbeat, polls hardware delay state every 3 s (telemetry), runs LAN device discovery/scanning, and drives auto-reconnect/retarget. It receives `NetworkCommand` from the audio/editor threads and reports back via `NetworkStatus`, both over bounded(64) `crossbeam_channel` queues.

## CLI code

- `src/network.rs` — `NetworkWorker` (main loop, heartbeat, telemetry poll, slot audit, discovery/retarget, scan collection), `NetworkCommand` enum, `NetworkStatus` enum, `WorkerShared` (Arc/Mutex fields shared with audio thread and editor), `DeviceInfo`/`ScanHealth`/`ScanStats`/`ScanPublisher`, `now_ms` helper, OSC response parsers (`parse_fx_type`, `extract_info_strings`, `parse_fx_delay_response`)
- `src/osc.rs` — OSC packet construction (`set_fx_delay`, `set_fxrtn_mute`, `query_fx_type`, `query_fx_delay`, `heartbeat`), BPM↔float math (`bpm_to_float`, `float_to_bpm`), effect-type tables (`fx_bus`, `fx_insert`) sourced from X32.c/fxparse1.c, `delay_par`, `is_bpm_compatible`, `fx_type_short`/`fx_type_long`
- `src/reconnect.rs` — `Backoff` struct: exponential backoff (base_ms doubling per failure, capped at cap_ms, saturating shift to avoid overflow past 64 attempts); used by `NetworkWorker` to pace heartbeat/reconnect attempts

## Docs

- `README.md` — OSC address table (lines ~138-141: `/info`, `/fx/{n}/type`, `/fx/{n}/par/02`, `/fxrtn/{n}/mix/on`), heartbeat/retry description (5 s heartbeat, 2 s retry), Auto reconnect + background discovery behaviour section
- `CLAUDE.md` — OSC quick reference (`bpm_to_float`/`float_to_bpm` formulas, OSC address table with effect-type routing), inter-thread communication table (`cmd_tx`/`cmd_rx`, `status_tx`/`status_rx`, `scan_health`, `hardware_float`, `last_device`, `last_slot_types`)

## Coupling

- **core**: `src/lib.rs` owns `cmd_tx`/`cmd_rx` and `status_tx`/`status_rx` (bounded(64) crossbeam channels) and drains `NetworkStatus` on the audio thread in `process()`; it also owns the `WorkerShared` Arc fields (`hardware_float_out`, `compatible_slots`, `occupied_slots`, `slot_types`, `scan_targets`, `connected_device`, `scan_generation`, `auto_reconnect`, `last_device`, `scan_health`, `last_slot_types`) that the worker writes into directly. Changing any `NetworkCommand`/`NetworkStatus` variant or `WorkerShared` field forces a matching change in core.
- **editor**: `src/editor.rs` sends `NetworkCommand` directly only for `ScanTargets` and `UpdateTarget` (user-initiated scan/target-select actions); Connect/ForceSync/AuditSlots reach the worker indirectly — the editor pulses a momentary `BoolParam`, and `process()` in `src/lib.rs` (core domain) translates the rising edge into the corresponding `NetworkCommand`. `editor.rs` also reads the shared Arc fields (`scan_targets`, `scan_health`, `hardware_float_out`, `connected_device`) for display. Adding a new status/telemetry field requires an editor-side reader.
- `src/osc.rs` is used exclusively by `src/network.rs`; no other module calls it directly.
- `src/reconnect.rs`'s `Backoff` is used only by `NetworkWorker`.

## Conventions worth knowing

- Timing constants (`src/network.rs`): `HEARTBEAT_INTERVAL` = 5 s, `TELEMETRY_INTERVAL` = 3 s, `RECV_TIMEOUT` = 250 ms, `HARD_RESET_DWELL` = 75 ms, `LOOP_SLEEP` = 10 ms, `SCAN_WINDOW` = 1000 ms, `SCAN_PROBE_BURST` = 3, `SCAN_PROBE_SPACING` = 200 ms, `AUTO_RESCAN_FAILURES` = 3, `SCAN_HEALTH_FAILURES` = 3, `DISCOVERY_INTERVAL_MIN` = 5 s, `DISCOVERY_INTERVAL_MAX` = 30 s, `RETARGET_FAST_PASSES` = 4.
- Effect-type routing for BPM delay parameter (`src/osc.rs::delay_par`): DLY (type 10) uses `par/02` (mix occupies `par/01`); 3TAP/4TAP/MODD/D/RV/D/CR/D/FL (types 11/12/26/21/24/25) use `par/01` (time is their first parameter). Only bus slots 1-4 are BPM-compatible (`is_bpm_compatible`); insert slots 5-8 never are.
- `NetworkWorker` exits automatically when the audio thread's command `Sender` is dropped.
- Auto-reconnect/discovery only run when `auto_reconnect` (mirrored `Arc<AtomicBool>`) is on — with it off, the worker emits no unrequested network traffic.
- Discovery cadence ramps by doubling from `DISCOVERY_INTERVAL_MIN` to `DISCOVERY_INTERVAL_MAX` on fruitless scans, and resets to the floor on an interface change or an active retarget search (budgeted by `RETARGET_FAST_PASSES`).
- Device identity for auto-reconnect/discovery adoption is `(name, model)` from the `/info` response, persisted in `last_device`; an auto-resumed target that answers with a different identity is rejected and a retarget/rescan is requested instead.
- `DeviceInfo.display_name()` format: `"name (model)"`, `"name"`, `"model"`, or `"ip:port"` depending on what fields the console returned.
- Scan probes go out per-IPv4-interface (plus a loopback socket for local mock mixers) to three destinations each: directed subnet broadcast, limited broadcast (255.255.255.255), and a unicast hint (current/last target) — any one of the three can be the only path that works.
- `ScanHealth` (`Unknown`/`Ok`/`NoReplies`/`NoInterfaces`) is derived per scan and published for the editor to tint the Scan control; it never gates worker behaviour.
- `/info` response format assumed: `,ssss version name model [firmware]` — `extract_info_strings` skips the version arg and returns `(name, model)`, empty when fewer than 2 string args are present.
