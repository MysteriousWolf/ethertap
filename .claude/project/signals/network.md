# network

## What it does

`NetworkWorker` runs on a dedicated OS thread and owns all UDP I/O. It receives `NetworkCommand` from the audio/editor threads via a bounded(64) crossbeam channel, sends OSC packets to the X32/M32, maintains a 5-second heartbeat, polls hardware delay state every 3 seconds (telemetry), and reports back via `NetworkStatus`. Includes LAN device scanner (`scan_for_devices`) that probes all local interfaces.

## CLI code

- `src/network.rs` — `NetworkWorker`, `NetworkCommand` enum, `NetworkStatus` enum, `DeviceInfo` struct, `scan_for_devices`, `now_ms` helper
- `src/osc.rs` — OSC packet construction (`build_set_delay`, `build_hard_reset_sequence`, etc.), BPM↔float math, effect-type tables from X32.c / fxparse1.c
- `src/reconnect.rs` — `Backoff` struct: exponential backoff (base_ms, cap_ms, saturating shift); used by network worker for reconnect delay

## Docs

- `README.md` — OSC address table, heartbeat/retry behaviour description
- `CLAUDE.md` — OSC quick reference, inter-thread channel table

## Coupling

- `src/lib.rs` sends `NetworkCommand` via `cmd_tx`; receives `NetworkStatus` via `status_rx` — channel bounded(64).
- `src/editor.rs` sends commands directly on user actions (Connect, Scan, ForceSync buttons).
- `src/osc.rs` is used exclusively by `src/network.rs`; no other callers.
- `hardware_float: Arc<AtomicU32>` written by network worker, read by editor for telemetry display.
- `tx_activity_ts` / `rx_activity_ts: Arc<AtomicU64>` written by network worker (ms since epoch), read by editor for TX/RX LED pulse.

## Conventions worth knowing

- `HEARTBEAT_INTERVAL = 5 s`, `TELEMETRY_INTERVAL = 3 s`, `RECV_TIMEOUT = 250 ms`, `HARD_RESET_DWELL = 75 ms`, `LOOP_SLEEP = 10 ms`.
- Effect type routing for BPM parameter: DLY (type 10) uses `par/02`; 3TAP/4TAP/MODD/D/RV/D/CR/D/FL (types 11/12/26/21/24/25) use `par/01`.
- `fx_type_filter` bitmask: bit 0=DLY, 1=3TAP, 2=4TAP, 3=D/RV, 4=D/CR, 5=D/FL, 6=MODD; default 0x7F (all enabled).
- Worker exits automatically when the audio thread's `Sender` is dropped.
- `scan_for_devices` probes all local interface addresses with `/info`; selects same-subnet IPs as primary.
- `DeviceInfo.display_name()` format: "name (model)", "name", "model", or "ip:port" depending on what the device returns.
