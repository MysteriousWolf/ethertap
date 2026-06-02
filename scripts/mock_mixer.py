#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "rich",
# ]
# ///
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

Usage (preferred — auto-installs deps):
  uv run scripts/mock_mixer.py

Fallback (no extra deps needed beyond rich):
  python3 scripts/mock_mixer.py
"""

from __future__ import annotations

import copy
import re
import select
import socket
import struct
import sys
import termios
import threading
import time
import tty
from collections import deque

try:
    from rich import box
    from rich.console import Group
    from rich.live import Live
    from rich.panel import Panel
    from rich.rule import Rule
    from rich.table import Table
    from rich.text import Text
except ImportError:
    print("error: rich not available. Run with: uv run scripts/mock_mixer.py", file=sys.stderr)
    sys.exit(1)


# ---------------------------------------------------------------------------
# Slot configuration
# ---------------------------------------------------------------------------

DLY = 10  # X32 Stereo Delay type ID (from X32Tap.c: #define DLY 10)

_SLOTS_INITIAL: dict[int, dict] = {
    1: {"type": DLY, "init_bpm": 120.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    2: {"type": 1,   "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "REV"},
    3: {"type": DLY, "init_bpm":  90.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    4: {"type": 3,   "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "CHO"},
    5: {"type": 0,   "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "---"},
    6: {"type": DLY, "init_bpm": 140.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    7: {"type": 0,   "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "---"},
    8: {"type": 2,   "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "PIT"},
}

# ---------------------------------------------------------------------------
# Shared state (all writes under _lock)
# ---------------------------------------------------------------------------

_lock = threading.Lock()
_slots: dict[int, dict] = copy.deepcopy(_SLOTS_INITIAL)
_msg_log: deque[tuple[float, str, str, list]] = deque(maxlen=20)  # (ts, peer, addr, args)
_total_msgs = 0
_start_time = time.time()
_quit = threading.Event()


# ---------------------------------------------------------------------------
# Minimal OSC codec (no external dependencies)
# ---------------------------------------------------------------------------

def _osc_str_encode(s: str) -> bytes:
    b = s.encode("ascii") + b"\x00"
    r = len(b) % 4
    if r:
        b += b"\x00" * (4 - r)
    return b


def _osc_str_decode(data: bytes, pos: int) -> tuple[str, int]:
    end = data.index(b"\x00", pos)
    s = data[pos:end].decode("ascii", errors="replace")
    raw = end - pos + 1
    pos += raw + (-raw % 4)
    return s, pos


def encode_osc(addr: str, *args) -> bytes:
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
                args.append(struct.unpack(">i", data[pos: pos + 4])[0])
                pos += 4
            elif t == "f":
                args.append(struct.unpack(">f", data[pos: pos + 4])[0])
                pos += 4
            elif t == "s":
                s, pos = _osc_str_decode(data, pos)
                args.append(s)
        return addr, args
    except Exception:
        return None, []


# ---------------------------------------------------------------------------
# Request handler — called from the recv thread, updates shared state
# ---------------------------------------------------------------------------

def _handle(data: bytes, sock: socket.socket, peer: tuple[str, int]) -> None:
    global _total_msgs
    osc_addr, args = decode_osc(data)
    if osc_addr is None:
        return

    ts = time.time()
    peer_str = f"{peer[0]}:{peer[1]}"

    with _lock:
        _total_msgs += 1
        _msg_log.append((ts, peer_str, osc_addr, args))

    # /info — heartbeat / scan probe
    if osc_addr == "/info":
        sock.sendto(encode_osc("/info", "V2.12", "Mock Console", "X32", "4.06"), peer)
        return

    # /fx/{slot}/type — effect-type audit query
    m = re.fullmatch(r"/fx/(\d+)/type", osc_addr)
    if m:
        slot = int(m.group(1))
        with _lock:
            type_id = _slots.get(slot, {}).get("type", 0)
        sock.sendto(encode_osc(osc_addr, type_id), peer)
        return

    # /fx/{slot}/par/02 — delay-time query (no args) or SET (with float arg)
    m = re.fullmatch(r"/fx/(\d+)/par/02", osc_addr)
    if m:
        slot = int(m.group(1))
        with _lock:
            cfg = _slots.get(slot)
            if not args:
                # Query — respond with last received BPM if synced, else initial
                bpm = cfg["rx_bpm"] or cfg["init_bpm"] if cfg else None
                f = 20.0 / bpm if bpm else 0.0
                sock.sendto(encode_osc(osc_addr, float(f)), peer)
            else:
                if cfg and isinstance(args[0], float) and args[0] > 0:
                    cfg["rx_bpm"] = 20.0 / args[0]
                    cfg["rx_count"] += 1
                    cfg["rx_ts"] = time.time()
        return

    # /fxrtn/{slot}/mix/on — mute/unmute; no response required
    if re.fullmatch(r"/fxrtn/\d+/mix/on", osc_addr):
        return


# ---------------------------------------------------------------------------
# Socket receive loop (background thread)
# ---------------------------------------------------------------------------

def _recv_loop(sock: socket.socket) -> None:
    while not _quit.is_set():
        try:
            data, peer = sock.recvfrom(4096)
            _handle(data, sock, peer)
        except socket.timeout:
            pass
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Keyboard reader (background thread, setcbreak so Ctrl+C still raises SIGINT)
# ---------------------------------------------------------------------------

def _keyboard_reader() -> None:
    if not sys.stdin.isatty():
        return
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    try:
        tty.setcbreak(fd)
        while not _quit.is_set():
            if select.select([sys.stdin], [], [], 0.1)[0]:
                ch = sys.stdin.read(1)
                if ch in ("q", "Q", "\x03", "\x04"):
                    _quit.set()
                    break
    except Exception:
        pass
    finally:
        try:
            termios.tcsetattr(fd, termios.TCSADRAIN, old)
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Rich display builder
# ---------------------------------------------------------------------------

def _build_display() -> Panel:
    HINTS = "[dim]q  quit[/dim]"

    with _lock:
        slots_snap = copy.deepcopy(_slots)
        log_snap = list(_msg_log)
        total = _total_msgs

    uptime = time.time() - _start_time

    # ── Slot table ────────────────────────────────────────────────────────
    slot_table = Table(box=box.SIMPLE, show_header=True, header_style="bold cyan", padding=(0, 1))
    slot_table.add_column("Slot", justify="right", min_width=4)
    slot_table.add_column("Name", min_width=5)
    slot_table.add_column("Type ID", justify="right", min_width=7)
    slot_table.add_column("Init BPM", justify="right", min_width=9)
    slot_table.add_column("Rx BPM", justify="right", min_width=9)
    slot_table.add_column("Syncs", justify="right", min_width=6)
    slot_table.add_column("Last sync", min_width=10)
    slot_table.add_column("Compat", justify="center", min_width=6)

    for slot_num, cfg in slots_snap.items():
        is_dly = cfg["type"] == DLY
        init_str = f"{cfg['init_bpm']:.1f}" if cfg["init_bpm"] else "[dim]—[/dim]"
        name_style = "bold green" if is_dly else ("dim" if cfg["type"] == 0 else "")

        rx = cfg["rx_bpm"]
        init = cfg["init_bpm"]
        if rx is not None:
            diff = rx - init if init else None
            sign = "+" if diff and diff >= 0 else ""
            diff_str = f" ({sign}{diff:.1f})" if diff is not None else ""
            rx_str = f"[bold green]{rx:.1f}[/bold green][dim]{diff_str}[/dim]"
        else:
            rx_str = "[dim]—[/dim]"

        if cfg["rx_ts"]:
            ago = time.time() - cfg["rx_ts"]
            sync_ts = f"[dim]{ago:.0f}s ago[/dim]"
        else:
            sync_ts = "[dim]—[/dim]"

        slot_table.add_row(
            str(slot_num),
            f"[{name_style}]{cfg['name']}[/{name_style}]" if name_style else cfg["name"],
            str(cfg["type"]) if cfg["type"] else "[dim]0[/dim]",
            f"[cyan]{init_str}[/cyan]" if is_dly and init else init_str,
            rx_str,
            str(cfg["rx_count"]) if cfg["rx_count"] else "[dim]0[/dim]",
            sync_ts,
            "[green]✓[/green]" if is_dly else "[dim]—[/dim]",
        )

    # ── Message log ───────────────────────────────────────────────────────
    log_table = Table(box=box.SIMPLE, show_header=True, header_style="bold", padding=(0, 1))
    log_table.add_column("Time", min_width=12, style="dim")
    log_table.add_column("Peer", min_width=20)
    log_table.add_column("OSC address", min_width=24)
    log_table.add_column("Args")

    for ts, peer_str, addr, args in log_snap[-15:]:
        t_str = time.strftime("%H:%M:%S", time.localtime(ts))
        # For par/02 SET messages decode the normalised float back to BPM.
        is_par02_set = (
            bool(re.fullmatch(r"/fx/\d+/par/02", addr))
            and args
            and isinstance(args[0], float)
            and args[0] > 0
        )
        if is_par02_set:
            bpm_rx = 20.0 / args[0]
            args_str = f"[cyan]{args[0]:.5f}[/cyan]  [bold green]→ {bpm_rx:.2f} BPM[/bold green]"
        elif args:
            args_str = "  ".join(
                f"[cyan]{a:.4f}[/cyan]" if isinstance(a, float) else
                f"[yellow]{a}[/yellow]" if isinstance(a, int) else
                f'"{a}"'
                for a in args
            )
        else:
            args_str = "[dim]—[/dim]"
        log_table.add_row(t_str, peer_str, addr, args_str)

    # ── Stats footer ──────────────────────────────────────────────────────
    h, rem = divmod(int(uptime), 3600)
    m, s = divmod(rem, 60)
    uptime_str = f"{h:02d}:{m:02d}:{s:02d}"
    stats_text = Text(f"  Total messages: {total}    Uptime: {uptime_str}    Port: UDP 10023", style="dim")

    return Panel(
        Group(
            slot_table,
            Rule(style="dim"),
            log_table,
            stats_text,
        ),
        title="[bold]EtherTap Mock Mixer[/bold]",
        subtitle=HINTS,
        border_style="blue",
    )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.settimeout(0.2)

    try:
        sock.bind(("0.0.0.0", 10023))
    except OSError as e:
        print(f"[mock] bind failed: {e}  (port 10023 already in use?)", file=sys.stderr)
        sys.exit(1)

    recv_thread = threading.Thread(target=_recv_loop, args=(sock,), daemon=True)
    recv_thread.start()

    kb_thread = threading.Thread(target=_keyboard_reader, daemon=True)
    kb_thread.start()

    try:
        with Live(_build_display(), refresh_per_second=4, screen=True) as live:
            while not _quit.is_set():
                live.update(_build_display())
                time.sleep(0.25)
    except KeyboardInterrupt:
        pass
    finally:
        _quit.set()

    print("[mock-mixer] stopped")
    try:
        sock.close()
    except Exception:
        pass


if __name__ == "__main__":
    main()
