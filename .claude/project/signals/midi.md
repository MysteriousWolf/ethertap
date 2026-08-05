# midi

## What it does

Three components: `midi_clock` runs a dedicated worker thread emitting beat-aligned 0xF8 MIDI clock bursts and bridging a selected physical MIDI device (bidirectional passthrough) or an in-process loopback port; `midi_watcher` detects MIDI output port hot-plug/unplug (native CoreMIDI notifications on macOS, polling on other platforms) and broadcasts device-name lists to the editor and the clock worker; `midi_hw` isolates the midir hardware-port connect/disconnect calls used by the clock worker. The `midi-loopback` workspace crate is a process-global named-port registry (`crossbeam_channel` queues) that lets `phys_out`/`phys_in` connect to an in-process software port instead of a real OS MIDI device, so the worker's bridge logic is exercisable without any OS MIDI driver.

## CLI code

- `src/midi_clock.rs` — `MidiClockWorker` (fields: `enabled`, `auto_connect`, `clock_rx`, `device_change_rx`, `device_watch_rx`, `initial_device`, `bridge_connected`, `bridge_connecting`, `clock_stats`, `midi_ppq`), `ClockMsg` enum (`Tick { on_beat }`, `BpmChanged { new_bpm }`, `TransportStart`, `Stop`), `ClockStats`/`AtomicClockStats`; `run_worker` is the platform-neutral worker loop (unconditional — no `cfg(not(target_os = "windows"))` split); `PhysOutput` enum wraps either a `midir::MidiOutputConnection` (`Hardware`) or a loopback `Sender<Vec<u8>>` (`Loopback`); `try_connect_out`/`try_connect_in` consult `midi_loopback::connect` first, then fall back to `crate::midi_hw::try_hw_out`/`try_hw_in`; `handle_port_scan` unions `midi_loopback::registered_names()` into the midir-enumerated port list before its presence check
- `src/midi_watcher.rs` — `spawn()` returns `MidiWatcherChannels` (`editor_rx`, `worker_rx`, `shutdown`, `last_update_ts`, `has_update`); macOS path (`spawn_macos`) uses `coremidi::Client::new_with_notifications` + a `CFRunLoop` slice loop driven by `BroadcastPlanner` (platform-independent, unit-tested decision logic); non-macOS path (`spawn_polling`) ticks every `POLL_INTERVAL_SECS` (2s) via `midir::MidiOutput` enumeration
- `src/midi_hw.rs` — `try_hw_out`/`try_hw_in`: the only midir hardware-port connect calls for the clock worker's bridge; module doc notes it is excluded from coverage measurement (`scripts/coverage.sh` `IGNORE_REGEX`) because it requires real hardware
- `midi-loopback/src/lib.rs` — `register`/`connect`/`send`/`unregister`/`registered_names`, `LoopbackPort` (RAII: `Drop` calls `unregister`), `LoopbackError` (`NameTaken`, `NotFound`); registry is a `OnceLock<Mutex<HashMap<String, Sender<Vec<u8>>>>>` (process-global)
- `midi-loopback/Cargo.toml` — crate `midi-loopback` v0.1.0, edition 2024, `publish = false`; deps: `crossbeam-channel = "0.5"`, `parking_lot = "0.12"`

## Docs

- `docs/design/cross-platform-midi-clock.md` — problem statement (Windows had no MIDI output at all; `ubuntu-latest` CI lacked `/dev/snd/seq`), three approaches evaluated, recommends Approach C (cross-platform bridge + shared in-process loopback crate)
- `docs/spec/cross-platform-midi-clock.md` — implementation contract for the above; 5 checkpoints (CP1 new `midi-loopback` crate, CP2 `run_unix`→`run_worker` made unconditional + loopback consult, CP3 `mock-suite::loopback_sink::LoopbackClockSink`, CP4 `registered_names()` union fix, CP5 rewire `tests/midi_clock_tests.rs`); change log records a correction where `MidiClockWorker::run()` originally aborted the whole bridge when `midir::MidiOutput::new` failed (no ALSA seq on CI) — now treats that as non-fatal (`Option<midir::MidiOutput>`); Implementation log records all 5 checkpoints shipped, commits `eadfc8d`..`b3e7243`

## Coupling

- **core** (`src/lib.rs`): owns `midi_clock_tx`/`midi_clock_rx` (bounded 256, `ClockMsg`), `device_change_tx`, spawns the MIDI clock worker on a thread named `"ethertap-midi-clk"` and `midi_watcher::spawn()` before any `midir::MidiOutput` is created (comment: macOS CoreMIDI notification client must be first); owns `midi_clock_activity_ts`, `midi_bridge_connected`, `midi_bridge_connecting`, `midi_clock_stats: Arc<midi_clock::AtomicClockStats>`, `midi_device_rx`, `midi_last_update_ts`, `midi_has_update`, `midi_watcher_shutdown`, `midi_clock_pulse_count`, `midi_clock_drop_count`; `process()` derives `BpmChanged` dispatch from `BPM_MIDI_THRESHOLD = 0.5`; `params.midi_clock_enabled_atom`/`midi_auto_connect_atom`/`midi_clock_ppq_atom` are synced from param values in `initialize()` and passed into `MidiClockWorker::new`.
- **build** (`mock-suite` crate, part of the `build`/test-infra domain): `midi-loopback` is also a dependency of `mock-suite`, whose `loopback_sink::LoopbackClockSink` registers a named port in the same registry so `tests/midi_clock_tests.rs` can drive the worker's bridge without an OS MIDI driver on any platform (per `docs/spec/cross-platform-midi-clock.md` CP3/CP5).
- **editor**: reads `midi_clock_activity_ts`, `midi_bridge_connected`/`midi_bridge_connecting`, `midi_clock_stats`, `midi_device_rx`/`midi_last_update_ts`/`midi_has_update` for device-picker and status display (per `src/lib.rs` field docs).

## Conventions worth knowing

- Clock uses a burst approach (no `std::thread::sleep` between pulses) — spacing pulses by sleeping causes OS scheduler overshoot (0.5–2 ms) that skews receiver-perceived BPM, and would let the bounded channel saturate.
- `MIN_RESYNC_GAP_MS = 1_500`, `MAX_RESYNC_GAP_MS = 3_000`, `BEATS_IN_GAP = 1.5` — silence gap inserted on `BpmChanged` (>0.5 BPM delta), floored/capped and phase-aligned to the next beat boundary before resuming.
- `DEBUG_LOG_INTERVAL_TICKS = 96` (4 beats @ 24 PPQ), `STAT_WINDOW = 256` (rolling jitter-stat sample window).
- No MIDI Start/Stop/Continue messages are ever sent — they travel back into the DAW through the virtual port and would inadvertently control playback.
- macOS-only: `set_realtime_priority()` calls `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)` (period 8 ms / computation 0.5 ms / constraint 4 ms / preemptible) to reduce pulse-bunching stutter; no-op on other platforms.
- `virt_conn` (the self-published "EtherTap MIDI Clock" virtual port via `output.create_virtual`) stays under `cfg(unix)` — never attempted on Windows; its absence there does not block the `phys_out`/loopback bridge, which is unconditional.
- `midi_watcher`: macOS debounce window is `SLICE_MS = 300` (a CFRunLoop slice = the debounce window); `SAFETY_POLL_MS = 15_000` re-enumerates even without a CoreMIDI notification, to self-heal a dropped `try_send` broadcast or an untriggered notification class (e.g. MIDIServer restart); non-macOS polling interval is `POLL_INTERVAL_SECS = 2`.
- A registered loopback port is invisible to midir's hardware enumeration, so `handle_port_scan` explicitly unions `midi_loopback::registered_names()` into its scanned port list — otherwise a connected loopback bridge is reported as "disappeared" roughly 1s after connecting.
- Loopback-backed devices have no input passthrough: `try_connect_in` returns `None` for any name registered in `midi_loopback` (loopback ports are output-only sinks from the worker's point of view).
- `PhysOutput::send` treats a loopback `try_send` failure (full channel or dropped receiver) the same as a hardware send failure — both are mapped to a disconnect.
