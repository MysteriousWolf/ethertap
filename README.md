# EtherTap

A high-performance **VST3 OSC control bridge** built in Rust.  
EtherTap synchronises hardware FX engines (Behringer X32 / Midas M32) with a DAW's musical timeline via OSC over UDP, and provides real-time telemetry so you always know whether the hardware is in sync.

---

## Quick Start

```bash
# Build & bundle the VST3 (macOS or Windows)
cargo xtask bundle ethertap --release

# Run unit tests (BPM math + OSC encoding)
cargo test --lib

# Lint
cargo clippy --all-targets -- -D warnings
```

The `.vst3` bundle is written to `target/bundled/`.

---

## BPM ↔ Float Conversion

The X32/M32 stores delay time as a **normalised float** in the range `[0.0, 1.0]`,  
where `1.0` corresponds to the maximum delay ceiling of **3 000 ms** (from `X32Tap.c`).

### Host BPM → X32 Float

$$\text{float} = \frac{60\,000}{BPM \times 3\,000} = \frac{20}{BPM}$$

| BPM  | Beat (ms) | Float  |
|------|-----------|--------|
|  20  | 3 000     | 1.0000 |
|  60  | 1 000     | 0.3333 |
| 120  |   500     | 0.1667 |
| 180  |   333     | 0.1111 |

Values outside `[0, 1]` are clamped.  Source: [`src/osc.rs`](src/osc.rs) `bpm_to_float()`.

### X32 Float → BPM (Telemetry Read-Back)

$$BPM = \frac{20}{\text{float}}$$

Source: [`src/osc.rs`](src/osc.rs) `float_to_bpm()`.

---

## OSC Address Map

| Address | Direction | Type | Description |
|---|---|---|---|
| `/info` | TX (query) | — | Heartbeat probe; console replies with metadata |
| `/fx/{n}/type` | TX (query) | — | Request effect type for slot `n` (1–8) |
| `/fx/{n}/par/02` | TX (set) | `float` | Write normalised delay time |
| `/fx/{n}/par/02` | TX (query) | — | Request current delay time (no args) |
| `/fxrtn/{n}/mix/on` | TX (set) | `int` | Mute (`0`) / unmute (`1`) FX return |

**Effect type IDs** (response to `/fx/{n}/type`):

| ID | Effect |
|---|---|
| 10 | Stereo Delay (`DLY`) — the only type EtherTap syncs |

---

## Sync Modes

| Mode | Behaviour |
|---|---|
| **Sync on Change** | Fires OSC once the host BPM has been stable for ≥ 500 ms after a change |
| **Sync Continuous** | Fires a plain sync on every quarter-note beat crossing while the transport is rolling |

### Hard Reset Modes

The Hard Reset sequence performs: **Mute FX return → 75 ms dwell → update delay time → 75 ms dwell → Unmute**.  
This eliminates rhythmic phase drift by forcing the hardware delay buffer to re-anchor.

| Mode | Hard Reset triggers |
|---|---|
| **Manual Only** | Only via the ⚡ FORCE SYNC button |
| **Auto + Manual** | Via FORCE SYNC **and** every settled BPM change (quantised to the next beat boundary) |

### Force Sync

The **⚡ FORCE SYNC** button is an **immediate**, non-quantised trigger that always executes a Hard Reset regardless of mode.  It is also exposed as a VST3 automation parameter (`force_sync`) for DAW control surfaces.

---

## Hardware Telemetry

Every **3 seconds** the background worker queries `/fx/{slot}/par/02` from the console and reports back:

- **Host float** — calculated from the current DAW BPM
- **Mixer float** — the actual value stored in the hardware
- **Sync LED** — green (`● MATCH`) if `|host − mixer| < 0.001`, red (`● DRIFT`) otherwise

This is a **read-only, observer-only** path.  EtherTap never automatically retries a failed sync.

---

## Architecture

```
┌──────────────────────��──────────────────────────────────┐
│  Host / DAW                                             │
│  ┌────────────────┐     ┌───────────────────────────┐  │
│  │  Audio Thread  │     │  GUI Thread (Iced editor)  │  │
│  │  process()     │     │  Telemetry + controls      │  │
│  └───────┬────────┘     └──────────────┬────────────┘  │
└──────────┼──────────────────────────   │  ─────────────┘
           │ crossbeam_channel (bounded, lock-free) │
           └──────────────┬─────────────────────────┘
                          ▼
              ┌───────────────────────┐
              │   NetworkWorker       │
              │   UDP  →  X32 / M32   │
              └───────────────────────┘
```

**Real-time safety contract:** `process()` never allocates, blocks, or locks a contended mutex.  
All network I/O is delegated via bounded `crossbeam-channel` queues.

---

## Project Structure

| Path | Purpose |
|---|---|
| `src/osc.rs` | BPM math, OSC packet builders, `bpm_to_float` / `float_to_bpm` |
| `src/network.rs` | `NetworkWorker` — UDP I/O, heartbeat, telemetry poll |
| `src/params.rs` | VST3 parameters and persistence |
| `src/lib.rs` | `EtherTap` plugin — sync state machine, transport sampling |
| `src/editor.rs` | Iced dark-theme editor (nih-plug-iced stateful-widget API) |
| `xtask/` | `cargo xtask bundle` — generates platform-correct `.vst3` bundles |
| `.zed/tasks.json` | Zed editor task shortcuts (Check, Test, Export) |
