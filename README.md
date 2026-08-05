# EtherTap

**VST3 OSC bridge for Behringer X32 / Midas M32.**  
Keeps your mixer's delay time locked to the DAW BPM — automatically or on demand.

![EtherTap UI](assets/preview.png)

---

## What it does

EtherTap runs as a zero-audio VST3 plugin and talks to the mixer over UDP/OSC.  
When the host BPM changes, or on a manual trigger, it calculates the correct delay value and sends it to one or more Stereo Delay FX slots on the X32/M32.

**Rate sync** — updates the delay parameter derived from the current BPM.  
**Phase sync** — performs a hard reset: mute → set → unmute, removing residual echoes.

Both can fire on BPM change, on every beat, or only when you press the button.

---

## Install

Download the latest release from the [Releases](../../releases) page and copy the `.vst3`
bundle to your plugin folder:

| Platform | Folder |
|---|---|
| macOS | `~/Library/Audio/Plug-Ins/VST3/` |
| Windows | `C:\Program Files\Common Files\VST3\` |
| Linux | `~/.vst3/` |

### macOS: "EtherTap.vst3 is damaged and can't be opened"

Builds aren't notarized, so Gatekeeper quarantines the downloaded bundle. Remove
the quarantine flag after copying it into place:

```sh
xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/VST3/EtherTap.vst3
```

---

## Connecting

1. Load the plugin in any track in your DAW.
2. Enter the mixer's IP address and port (`10023` is the X32/M32 default).
3. Press **Connect** — the status indicator turns green once the device responds.
4. Select which FX slot holds a Stereo Delay, or press **Query** to auto-detect.

EtherTap sends a heartbeat every 5 seconds. If the mixer stops responding it shows
a disconnected state and retries automatically every 2 seconds until it comes back.
Pressing **Disconnect** stops all retries.

### Auto reconnect

The **Auto** toggle next to the Connect button (also the `auto_reconnect` host
parameter) is off by default: EtherTap never sends network traffic on load
unless you opt in. With Auto on, the plugin reconnects to the last mixer when
the session loads and remembers the console's name and model. If a different
device answers at the saved address, or the connection stays down, EtherTap
rescans the network for the console it knows and follows it to its new IP.

Auto also turns on background discovery. While disconnected, EtherTap scans for
consoles on its own every few seconds, whether or not the plugin window is open,
and backs off to one scan every 30 seconds when it keeps finding nothing. This is
what makes a cold start work: the DAW, the network interface and the console can
come up in any order and EtherTap still converges. If it recognises the console
it connected to before, it reconnects to it wherever the address moved to. If it
has never connected to anything and finds exactly one console, it adopts that
one. Two or more unknown consoles is ambiguous, so it waits for you to choose.

### macOS: nothing is discovered

From macOS 15, applications need the Local Network permission to reach anything
on the LAN. A plugin inherits this from the DAW that loaded it, and a denied
permission is silent: every probe and every reply is discarded with no error, so
an empty device list looks exactly like a mixer that is switched off.

If EtherTap finds nothing, open **System Settings → Privacy & Security → Local
Network** and confirm your DAW is listed and enabled. The scan button turns
amber when several scans in a row go unanswered, which is the usual sign that
something is dropping the traffic; it turns red when the machine has no usable
network interface at all. The plugin log records the interface, probe and reply
counts for every scan.

---

## Sync modes

| Mode | Rate Sync | Phase Sync |
|---|---|---|
| **Manual** | Force button only | Force button only |
| **On Change** | After BPM settles for 500 ms | Hard reset at next beat boundary |
| **Continuous** | Every quarter-note beat | Hard reset every beat |

**Hard reset sequence:** mute → 75 ms → update delay → 75 ms → unmute.  
This re-anchors the hardware delay buffer and eliminates phase drift.

---

## Building from source

**Prerequisites:** Rust (stable), Git

```sh
git clone <repo-url>
cd ethertap
cargo run -p xtask -- bundle ethertap --release
# → target/bundled/ethertap.vst3
```

**macOS universal binary (Intel + Apple Silicon):**

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/build.sh --universal
# → dist/ethertap-<version>-macos-universal.zip
```

---

## BPM ↔ delay float

The X32/M32 stores delay time as a normalised float `[0, 1]` where `1.0 = 3000 ms`.

```
delay_float = 20 / bpm     (e.g. 120 BPM → 0.1667)
bpm         = 20 / delay_float
```

---

## OSC reference

| Address | Dir | Type | Description |
|---|---|---|---|
| `/info` | TX | — | Heartbeat probe |
| `/fx/{n}/type` | TX | — | Query effect type (DLY = 10) |
| `/fx/{n}/par/02` | TX | `float` | Set / query delay time |
| `/fxrtn/{n}/mix/on` | TX | `int` | Mute (0) / unmute (1) FX return |

---

## Architecture

```
┌────────────────────────────────────────────────────────┐
│  Host / DAW                                            │
│  ┌─────────────────┐    ┌──────────────────────────┐  │
│  │  Audio Thread   │    │  GUI Thread (Iced editor) │  │
│  │  process()      │    │  Telemetry + controls     │  │
│  └────────┬────────┘    └──────────────┬────────────┘  │
└───────────┼─────────────────────────── │ ──────────────┘
            │ crossbeam_channel (bounded, lock-free)  │
            └──────────────┬─────────────────────────┘
                           ▼
               ┌───────────────────────┐
               │   NetworkWorker       │
               │   UDP  →  X32 / M32   │
               └───────────────────────┘
```

`process()` never allocates, blocks, or locks a contended mutex.  
All network I/O is delegated via bounded lock-free channels.

---

## License

[MIT](LICENSE)
