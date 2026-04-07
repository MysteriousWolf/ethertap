#!/usr/bin/env python3
"""
Mock X32/M32 mixer for EtherTap standalone GUI testing.

Simulates a console on UDP port 10023 with a realistic slot layout so the
UI can exercise audit, telemetry, and scan features in a single session:

  Slot 1  DLY  (Stereo Delay, type 10)  – 120 BPM   ← compatible
  Slot 2  REV  (Reverb, type 1)         – occupied, not a delay
  Slot 3  DLY  (Stereo Delay, type 10)  – 90 BPM    ← compatible
  Slot 4  CHO  (Chorus, type 3)         – occupied, not a delay
  Slot 5  ----  (empty, type 0)
  Slot 6  DLY  (Stereo Delay, type 10)  – 140 BPM   ← compatible
  Slot 7  ----  (empty, type 0)
  Slot 8  PIT  (Pitch shift, type 2)    – occupied, not a delay

Responds to the full set of OSC messages EtherTap sends:

  /info                    → /info "mock-x32-ethertap"  (heartbeat + scan)
  /fx/{slot}/type          → /fx/{slot}/type <i32>       (audit query)
  /fx/{slot}/par/02        → /fx/{slot}/par/02 <float>   (telemetry query)
  /fx/{slot}/par/02 <f>    → (no reply; stores the updated float)
  /fxrtn/{slot}/mix/on <n> → (no reply; mute/unmute acknowledged)

Note: the Rust built-in mock (src/mock.rs) also tries to bind port 10023 in
standalone mode and will print a "port already in use" warning — this is
harmless; this Python server handles all requests instead.

Usage:
  python3 scripts/mock_mixer.py
"""

import re
import socket
import struct
import sys

# ─── Slot configuration ───────────────────────────────────────────────────────

DLY = 10  # X32 Stereo Delay type ID (from X32Tap.c: #define DLY 10)

# Mutable dict — SET commands update bpm so telemetry reflects synced values.
SLOTS: dict[int, dict] = {
    1: {"type": DLY, "bpm": 120.0, "name": "DLY"},
    2: {"type": 1,   "bpm": None,  "name": "REV"},
    3: {"type": DLY, "bpm":  90.0, "name": "DLY"},
    4: {"type": 3,   "bpm": None,  "name": "CHO"},
    5: {"type": 0,   "bpm": None,  "name": "---"},
    6: {"type": DLY, "bpm": 140.0, "name": "DLY"},
    7: {"type": 0,   "bpm": None,  "name": "---"},
    8: {"type": 2,   "bpm": None,  "name": "PIT"},
}


def bpm_to_float(bpm: float) -> float:
    """X32 delay normalisation: float = 20.0 / bpm  (see osc.rs)."""
    return 20.0 / bpm


# ─── Minimal OSC codec (no external dependencies) ────────────────────────────

def _osc_str_encode(s: str) -> bytes:
    """Null-terminate and pad to the next multiple of 4."""
    b = s.encode("ascii") + b"\x00"
    r = len(b) % 4
    if r:
        b += b"\x00" * (4 - r)
    return b


def _osc_str_decode(data: bytes, pos: int) -> tuple[str, int]:
    """Read a null-terminated, 4-byte-aligned OSC string from *data* at *pos*."""
    end = data.index(b"\x00", pos)
    s = data[pos:end].decode("ascii", errors="replace")
    raw = end - pos + 1           # bytes consumed including null
    pos += raw + (-raw % 4)       # advance past padding
    return s, pos


def encode_osc(addr: str, *args) -> bytes:
    """Encode a simple OSC message (str / int / float arguments)."""
    tags = ","
    arg_bytes = b""
    for a in args:
        if isinstance(a, str):
            tags += "s"
            arg_bytes += _osc_str_encode(a)
        elif isinstance(a, int):
            tags += "i"
            arg_bytes += struct.pack(">i", a)
        elif isinstance(a, float):
            tags += "f"
            arg_bytes += struct.pack(">f", a)
    return _osc_str_encode(addr) + _osc_str_encode(tags) + arg_bytes


def decode_osc(data: bytes) -> tuple[str | None, list]:
    """Decode an OSC message, returning (address, args).  Returns (None, []) on error."""
    try:
        addr, pos = _osc_str_decode(data, 0)
        if pos >= len(data):
            return addr, []
        tags, pos = _osc_str_decode(data, pos)
        if not tags.startswith(","):
            return addr, []
        args: list = []
        for t in tags[1:]:
            if t == "i":
                args.append(struct.unpack(">i", data[pos : pos + 4])[0])
                pos += 4
            elif t == "f":
                args.append(struct.unpack(">f", data[pos : pos + 4])[0])
                pos += 4
            elif t == "s":
                s, pos = _osc_str_decode(data, pos)
                args.append(s)
        return addr, args
    except Exception:
        return None, []


# ─── Request handler ──────────────────────────────────────────────────────────

def handle(data: bytes, sock: socket.socket, peer: tuple[str, int]) -> None:
    osc_addr, args = decode_osc(data)
    if osc_addr is None:
        return

    ip, port = peer
    print(f"  ← {ip}:{port:<5}  {osc_addr}  {args}")

    # /info — heartbeat and scan-broadcast probe
    # Mimic X32 /info response: version, user-name, model, firmware
    if osc_addr == "/info":
        reply = encode_osc("/info", "V2.12", "Mock Console", "X32", "4.06")
        sock.sendto(reply, peer)
        return

    # /fx/{slot}/type — effect-type audit query
    m = re.fullmatch(r"/fx/(\d+)/type", osc_addr)
    if m:
        slot = int(m.group(1))
        type_id = SLOTS.get(slot, {}).get("type", 0)
        sock.sendto(encode_osc(osc_addr, type_id), peer)
        return

    # /fx/{slot}/par/02 — delay-time query (no args) or SET (with args)
    m = re.fullmatch(r"/fx/(\d+)/par/02", osc_addr)
    if m:
        slot = int(m.group(1))
        cfg = SLOTS.get(slot)
        if not args:
            # Query — return normalised float for this slot's BPM.
            bpm = cfg["bpm"] if cfg and cfg["bpm"] else None
            f = bpm_to_float(bpm) if bpm else 0.0
            sock.sendto(encode_osc(osc_addr, float(f)), peer)
        else:
            # SET — update in-memory BPM so future telemetry reflects the sync.
            if cfg and isinstance(args[0], float) and args[0] > 0:
                cfg["bpm"] = 20.0 / args[0]
        return

    # /fxrtn/{slot}/mix/on — mute/unmute; no response required
    if re.fullmatch(r"/fxrtn/\d+/mix/on", osc_addr):
        return


# ─── Entry point ─────────────────────────────────────────────────────────────

def _render_table(headers: list[str], rows: list[list[str]]) -> str:
    """Render a Unicode box-drawing table from headers and rows."""
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    def bar(left: str, mid: str, right: str, fill: str) -> str:
        return left + mid.join(fill * (w + 2) for w in widths) + right

    lines = []
    lines.append(bar("┌", "┬", "┐", "─"))
    lines.append("│" + "│".join(f" {h:<{widths[i]}} " for i, h in enumerate(headers)) + "│")
    lines.append(bar("├", "┼", "┤", "─"))
    for row in rows:
        lines.append("│" + "│".join(f" {str(cell):<{widths[i]}} " for i, cell in enumerate(row)) + "│")
    lines.append(bar("└", "┴", "┘", "─"))
    return "\n".join(lines)


def _banner() -> None:
    print("EtherTap mock mixer  ·  0.0.0.0:10023 (UDP)")
    print()
    headers = ["Slot", "Type", "BPM", "Compat"]
    rows = []
    for slot, cfg in SLOTS.items():
        bpm_str = f"{cfg['bpm']:.0f}" if cfg["bpm"] else "—"
        compat  = "✓" if cfg["type"] == DLY else ""
        rows.append([str(slot), cfg["name"], bpm_str, compat])
    print(_render_table(headers, rows))
    print()


def main() -> None:
    _banner()

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.settimeout(0.2)

    try:
        sock.bind(("0.0.0.0", 10023))
    except OSError as e:
        print(f"[mock] bind failed: {e}  (port 10023 already in use?)", file=sys.stderr)
        sys.exit(1)

    print("[mock] ready — Ctrl+C to stop\n")

    try:
        while True:
            try:
                data, peer = sock.recvfrom(4096)
                handle(data, sock, peer)
            except socket.timeout:
                pass
    except KeyboardInterrupt:
        print("\n[mock] stopped")
    finally:
        sock.close()


if __name__ == "__main__":
    main()
