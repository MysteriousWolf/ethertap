# midi

## What it does

Two background workers handle MIDI: `midi_clock` sends timed 0xF8 burst packets on a dedicated thread, implementing beat-aligned MIDI clock at configurable PPQ; `midi_watcher` detects hot-plug/unplug of physical MIDI output devices and notifies the editor and clock worker. When a physical device is selected, the clock worker also bridges MIDI input from that device through to the virtual port.

## CLI code

- `src/midi_clock.rs` — `ClockWorker`, `ClockMsg` enum (`Tick`, `BpmChanged`, `DeviceChanged`, `Stop`), `ClockStats` struct; burst-based clock (not inter-pulse sleep); resync gap logic (1500–3000 ms silence on BPM change > 0.5); transport-start beat alignment; MIDI bridge passthrough
- `src/midi_watcher.rs` — `spawn()` returns `MidiWatcherChannels`; macOS: CoreMIDI `Client::new_with_notifications` + CFRunLoop (event-driven); non-macOS: 2-second polling via `midir::MidiOutput`; rate-limited broadcasts (300 ms cooldown)

## Docs

- `CLAUDE.md` — inter-thread channel table (midi_clock_tx, device_change_tx), RT-safety notes

## Coupling

- `src/lib.rs` sends `ClockMsg::Tick` from `process()` on every sample (PPQ-based); sends `ClockMsg::BpmChanged` when tempo changes; sends `Option<String>` via `device_change_tx` when MIDI device param changes.
- `midi_clock_activity_ts: Arc<AtomicU64>` written by clock worker per beat, read by editor for CK LED.
- `midi_bridge_connected / midi_bridge_connecting: Arc<AtomicBool>` written by clock worker, read by editor for bridge status display.
- `midi_clock_drop_count: Arc<AtomicU32>` incremented in `process()` on channel-full; drained by editor each frame.
- `midi_clock_stats: Arc<AtomicClockStats>` (lock-free, RT-safe — avoids mutex on the MIDI worker's RT-priority thread) written by clock worker after each beat; read by editor via `.load()` for stats row.
- `midi_device_rx: Arc<Receiver<Vec<String>>>` (editor channel from watcher) read by editor to update device picker.
- `midi_watcher_shutdown: Arc<AtomicBool>` set in `EtherTap::Drop` to stop non-macOS polling thread.

## Conventions worth knowing

- Clock uses burst approach: 24 0xF8 bytes sent at beat boundary, not spaced by sleep. Prevents scheduler jitter from causing wrong BPM at receiver.
- `MIN_RESYNC_GAP_MS = 1500`, `MAX_RESYNC_GAP_MS = 3000` — inserted after BPM change > 0.5 BPM.
- No MIDI Start/Stop/Continue messages sent — would inadvertently control the DAW via virtual port loopback.
- Default PPQ = 24 (MIDI spec); configurable via `midi_clock_ppq` param (options: 3,4,6,8,12,16,24,32,48,96).
- macOS: CoreMIDI watcher cannot be interrupted; process exit cleans it up. Non-macOS: `shutdown` AtomicBool signals polling thread.
- `BROADCAST_COOLDOWN_MS = 300` rate-limits CoreMIDI notification flurries during USB hub events.
