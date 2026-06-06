# EtherTap — Persistent AI Coding Guidelines

<atomic-signals>

## Project signals (auto-loaded)

@.claude/project/signals.md

</atomic-signals>

## Real-Time Safety (Non-Negotiable)

`process()` in `src/lib.rs` is called on the **audio thread**.  It must never:

- Allocate heap memory (`Vec::new`, `String::from`, `Box::new`, etc.)
- Block on a mutex (`parking_lot::Mutex::lock()` in the hot path is acceptable
  only because parking_lot is wait-free when uncontended, but prefer atomics)
- Call any blocking I/O or system call
- `unwrap()` on anything that could realistically panic

**Allowed patterns in `process()`:**
- `crossbeam_channel::try_recv()` / `try_send()` — non-blocking, lock-free
- `Arc<AtomicBool/U32/U64>::load/store/swap()` — always safe
- `parking_lot::Mutex::lock()` on `params.fx_slot` — uncontended in practice;
  future work should migrate to `AtomicU8`

## EtherTap Design Philosophy

EtherTap is a **Status-Aware Proxy** with a "Human-in-the-loop" approach:

1. **Quantised Sync** — timing-sensitive operations (Hard Reset) are deferred to
   integer beat boundaries so any audio glitch is rhythmically masked.
2. **Telemetry-only read-back** — the hardware is polled for its current state
   every 3 s, but EtherTap never automatically retries or corrects.  The user
   sees the drift and decides to act.
3. **No surprise automation** — Sync Continuous fires plain syncs; Hard Reset
   only fires when the user explicitly enables it (Manual Only / Auto+Manual).

## Inter-Thread Communication

| Channel | Direction | Type | Notes |
|---|---|---|---|
| `cmd_tx` / `cmd_rx` | audio → network | `crossbeam_channel` | bounded(64), non-blocking |
| `status_tx` / `status_rx` | network → audio | `crossbeam_channel` | bounded(64), non-blocking |
| `force_sync_trigger` | editor → audio | `Arc<AtomicBool>` | `swap(false)` to consume |
| `hardware_float` | network → editor | `Arc<AtomicU32>` | f32 bits via `f32::to/from_bits` |
| `host_bpm` | audio → editor | `Arc<AtomicU32>` | f32 bits |
| `tx/rx_activity_ts` | audio → editor | `Arc<AtomicU64>` | ms since epoch for LED pulse |

**Never** add a `std::sync::Mutex` or `tokio::sync::Mutex` to the hot path.

## nih-plug-iced API Notes (this version uses old stateful-widget API)

- `Button::new(&mut self.state, content)` — needs a `button::State` field per button
- `TextInput::new(&mut self.state, placeholder, value, closure)` — same pattern
- `view(&mut self)` — takes `&mut self`, not `&self`
- `context()` returns `&dyn GuiContext`, not `Arc<dyn GuiContext>`
- `IcedState::from_size(w, h)` already returns `Arc<IcedState>` — do not wrap
- `Space::with_height(n.into())` or `Length::Units(n)` — not plain integers
- `ProcessContext::set_parameter()` is **not implemented** in this nih-plug version;
  use `Arc<AtomicBool>` for UI→audio momentary triggers instead
- Button style: create a single enum-based `StyleSheet` struct to avoid
  type mismatches in `if/else` expressions passed to `.style()`

## Commit Standards

All commits must use **Capitalized Imperative Verbs**:  
`Add`, `Fix`, `Improve`, `Implement`, `Update`

Examples:
- `Implement hardware telemetry poll loop`
- `Add BPM settle detection for Sync on Change`
- `Fix Hard Reset quantisation at beat boundary`

## OSC Quick Reference

```
bpm_to_float(bpm) = (60_000 / bpm / 3_000).clamp(0, 1)   // = 20/bpm
float_to_bpm(f)   = 20.0 / f

/fx/{slot}/par/02  float  — delay time for DLY (type 10) — confirmed by X32Tap.c
                            (mix=par/01, time=par/02)
/fx/{slot}/par/01  float  — delay time for 3TAP/4TAP/MODD/D/RV/D/CR/D/FL (types 11/12/26/21/24/25)
                            (time is the first effect param; confirmed fxparse1.c)
/fx/{slot}/type    —      — query effect type (DLY=10, 3TAP=11, 4TAP=12, MODD=26,
                            D/RV=21, D/CR=24, D/FL=25)
/fxrtn/{slot}/mix/on  int — 0=mute, 1=unmute
/info              —      — heartbeat / connectivity probe
```
