#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "python-rtmidi",
#     "rich",
# ]
# ///
"""
Combined EtherTap mock suite — MIDI clock sink + X32/M32 mixer in one TUI.

Both services start automatically.  Toggle each independently at runtime:

  k      toggle MIDI clock sink   (virtual port "EtherTap Mock MIDI Sink")
  m      toggle mock mixer        (UDP OSC server, port 10023)
  ↑ / ↓  scroll mixer message log
  r      jump to latest (unpin scroll)
  q      quit

Usage (preferred — auto-installs deps):
  uv run scripts/mock_ethertap.py
"""

from __future__ import annotations

import copy
import re
import select
import socket
import statistics
import struct
import sys
import termios
import threading
import time
import tty
from collections import deque

try:
    import rtmidi
    _HAS_RTMIDI = True
except ImportError:
    _HAS_RTMIDI = False

try:
    from rich import box
    from rich.console import Group
    from rich.layout import Layout
    from rich.live import Live
    from rich.panel import Panel
    from rich.rule import Rule
    from rich.table import Table
    from rich.text import Text
except ImportError:
    print("error: rich not available. Run with: uv run scripts/mock_ethertap.py", file=sys.stderr)
    sys.exit(1)


# ── Tunable constants ─────────────────────────────────────────────────────────

_MIDI_PORT_NAME     = "EtherTap Mock MIDI Sink"
_MIDI_CPB           = 24          # clocks per beat (MIDI standard)
_CLOCK_BYTE         = 0xF8
_MIDI_WINDOW        = 241         # timestamps → 240 intervals = 10 beats
_MIDI_BPM_HIST      = 60          # beats of BPM history for sparkline
_CLOCK_GAP_WARN     = 2.0         # seconds without a clock → yellow
_CLOCK_GAP_DEAD     = 8.0         # seconds without a clock → red

_MIXER_LOG_CAP      = 200         # total messages kept in memory
_MIXER_LOG_VISIBLE  = 15          # rows shown at once
_MIXER_STALE_WARN   = 10.0        # seconds without any OSC → yellow
_MIXER_STALE_DEAD   = 30.0        # seconds without any OSC → red

_SPARK              = "▁▂▃▄▅▆▇█"


# ═══════════════════════════════════════════════════════════════════════════════
# MIDI Clock Sink — state + service
# ═══════════════════════════════════════════════════════════════════════════════

_midi_lock          = threading.Lock()
_midi_clock_times:  deque[float] = deque(maxlen=_MIDI_WINDOW)
_midi_bpm_history:  deque[float] = deque(maxlen=_MIDI_BPM_HIST)
_midi_total_clocks  = 0
_midi_other_msgs    = 0
_midi_last_hex      = ""
_midi_last_ts       = 0.0   # any message
_midi_last_clock_ts = 0.0   # clock-only, for gap detection

_midi_svc_lock      = threading.Lock()
_midi_in_obj        = None  # rtmidi.MidiIn when running
_midi_running       = False
_midi_error         = ""
_midi_svc_start:    float | None = None
_midi_ports_available: list = []  # live list from rtmidi for debugging


def _midi_error_callback(err_type: int, message: str) -> None:
    """Called by rtmidi on errors (buffer overruns, etc.)."""
    print(f"[midi-error] type={err_type} {message}", file=sys.stderr)


def _midi_callback(event, data=None):
    global _midi_total_clocks, _midi_other_msgs
    global _midi_last_hex, _midi_last_ts, _midi_last_clock_ts
    message, _delta = event
    ts = time.time()
    with _midi_lock:
        _midi_last_hex = " ".join(f"0x{b:02X}" for b in message)
        _midi_last_ts  = ts
        # Guard against empty message (can happen on buffer overruns)
        if not message:
            return
        first = message[0]
        if first == _CLOCK_BYTE:
            _midi_total_clocks  += 1
            _midi_last_clock_ts  = ts
            _midi_clock_times.append(ts)
            # Sample BPM once per beat from the last CPB+1 timestamps.
            if _midi_total_clocks % _MIDI_CPB == 0 and len(_midi_clock_times) > _MIDI_CPB:
                recent  = list(_midi_clock_times)[-(  _MIDI_CPB + 1):]
                mean_iv = (recent[-1] - recent[0]) / _MIDI_CPB
                if mean_iv > 0:
                    _midi_bpm_history.append(60.0 / (mean_iv * _MIDI_CPB))
        else:
            _midi_other_msgs += 1


def _midi_start() -> None:
    global _midi_in_obj, _midi_running, _midi_error, _midi_svc_start, _midi_ports_available
    if not _HAS_RTMIDI:
        _midi_error = "python-rtmidi not installed"
        return
    try:
        mi = rtmidi.MidiIn()
        mi.set_error_callback(_midi_error_callback)
        mi.ignore_types(sysex=False, timing=False, active_sense=True)
        mi.open_virtual_port(_MIDI_PORT_NAME)
        mi.set_callback(_midi_callback)
        _midi_in_obj  = mi
        _midi_running = True
        _midi_error   = ""
        _midi_svc_start = time.time()
        # Log available output ports so we can see what CoreMIDI knows about
        try:
            mo = rtmidi.MidiOut()
            _midi_ports_available = mo.get_ports()
            print(f"[midi] virtual port created, available outputs: {_midi_ports_available}", file=sys.stderr)
        except Exception as e:
            print(f"[midi] could not enumerate ports: {e}", file=sys.stderr)
    except Exception as e:
        _midi_error   = str(e)
        _midi_running = False


def _midi_stop() -> None:
    global _midi_in_obj, _midi_running, _midi_svc_start
    try:
        if _midi_in_obj:
            try:
                _midi_in_obj.cancel_callback()
            except AttributeError:
                pass
            _midi_in_obj.close_port()
    except Exception:
        pass
    _midi_in_obj    = None
    _midi_running   = False
    _midi_svc_start = None
    with _midi_lock:
        _midi_clock_times.clear()


def _midi_toggle() -> None:
    with _midi_svc_lock:
        if _midi_running:
            _midi_stop()
        else:
            _midi_start()


def _midi_compute_stats() -> dict | None:
    with _midi_lock:
        times     = list(_midi_clock_times)
        bpm_hist  = list(_midi_bpm_history)
        total     = _midi_total_clocks
        other     = _midi_other_msgs
        last_hex  = _midi_last_hex
        last_ts   = _midi_last_ts
        clock_ts  = _midi_last_clock_ts

    if len(times) < 2:
        return None

    intervals   = [times[i + 1] - times[i] for i in range(len(times) - 1)]
    mean_iv     = statistics.mean(intervals)
    bpm         = 60.0 / (mean_iv * _MIDI_CPB) if mean_iv > 0 else 0.0
    abs_jitter  = sorted(abs(iv - mean_iv) * 1_000_000 for iv in intervals)

    def pct(p: float) -> float:
        if not abs_jitter:
            return 0.0
        idx = (p / 100.0) * (len(abs_jitter) - 1)
        lo  = int(idx)
        hi  = min(lo + 1, len(abs_jitter) - 1)
        return abs_jitter[lo] + (abs_jitter[hi] - abs_jitter[lo]) * (idx - lo)

    return {
        "bpm": bpm, "bpm_hist": bpm_hist,
        "total_clocks": total, "other_msgs": other,
        "sample_count": len(intervals),
        "mean_us": mean_iv * 1_000_000,
        "std_us":  statistics.stdev(intervals) * 1_000_000 if len(intervals) > 1 else 0.0,
        "p50": pct(50), "p75": pct(75), "p95": pct(95), "p99": pct(99),
        "max_jitter": abs_jitter[-1] if abs_jitter else 0.0,
        "last_hex": last_hex, "last_ts": last_ts, "clock_ts": clock_ts,
    }


# ═══════════════════════════════════════════════════════════════════════════════
# Mock Mixer — state + service
# ═══════════════════════════════════════════════════════════════════════════════

_DLY = 10

_SLOTS_INITIAL: dict[int, dict] = {
    1: {"type": _DLY, "init_bpm": 120.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    2: {"type": 1,    "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "REV"},
    3: {"type": _DLY, "init_bpm":  90.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    4: {"type": 3,    "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "CHO"},
    5: {"type": 0,    "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "---"},
    6: {"type": _DLY, "init_bpm": 140.0, "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "DLY"},
    7: {"type": 0,    "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "---"},
    8: {"type": 2,    "init_bpm": None,  "rx_bpm": None, "rx_count": 0, "rx_ts": None, "name": "PIT"},
}

_mixer_lock         = threading.Lock()
_mixer_slots:       dict[int, dict] = copy.deepcopy(_SLOTS_INITIAL)
_mixer_msg_log:     deque[tuple]    = deque(maxlen=_MIXER_LOG_CAP)
_mixer_total_msgs   = 0
_mixer_last_msg_ts  = 0.0

_mixer_svc_lock     = threading.Lock()
_mixer_stop_evt     = threading.Event()
_mixer_sock:        socket.socket | None = None
_mixer_running      = False
_mixer_error        = ""
_mixer_svc_start:   float | None = None


# ── OSC codec ─────────────────────────────────────────────────────────────────

def _osc_str_enc(s: str) -> bytes:
    b = s.encode("ascii") + b"\x00"
    return b + b"\x00" * (-len(b) % 4)


def _osc_str_dec(data: bytes, pos: int) -> tuple[str, int]:
    end = data.index(b"\x00", pos)
    s   = data[pos:end].decode("ascii", errors="replace")
    raw = end - pos + 1
    return s, pos + raw + (-raw % 4)


def _osc_encode(addr: str, *args) -> bytes:
    tags, body = ",", b""
    for a in args:
        if isinstance(a, str):   tags += "s"; body += _osc_str_enc(a)
        elif isinstance(a, int): tags += "i"; body += struct.pack(">i", a)
        elif isinstance(a, float): tags += "f"; body += struct.pack(">f", a)
    return _osc_str_enc(addr) + _osc_str_enc(tags) + body


def _osc_decode(data: bytes) -> tuple[str | None, list]:
    try:
        addr, pos = _osc_str_dec(data, 0)
        if pos >= len(data):
            return addr, []
        tags, pos = _osc_str_dec(data, pos)
        if not tags.startswith(","):
            return addr, []
        args: list = []
        for t in tags[1:]:
            if t == "i":
                args.append(struct.unpack(">i", data[pos:pos + 4])[0]); pos += 4
            elif t == "f":
                args.append(struct.unpack(">f", data[pos:pos + 4])[0]); pos += 4
            elif t == "s":
                s, pos = _osc_str_dec(data, pos); args.append(s)
        return addr, args
    except Exception:
        return None, []


def _mixer_handle(data: bytes, sock: socket.socket, peer: tuple[str, int]) -> None:
    global _mixer_total_msgs, _mixer_last_msg_ts
    osc_addr, args = _osc_decode(data)
    if osc_addr is None:
        return

    ts = time.time()
    with _mixer_lock:
        _mixer_total_msgs  += 1
        _mixer_last_msg_ts  = ts
        _mixer_msg_log.append((ts, f"{peer[0]}:{peer[1]}", osc_addr, args))

    if osc_addr == "/info":
        sock.sendto(_osc_encode("/info", "V2.12", "Mock Console", "X32", "4.06"), peer)
        return

    m = re.fullmatch(r"/fx/(\d+)/type", osc_addr)
    if m:
        slot = int(m.group(1))
        with _mixer_lock:
            type_id = _mixer_slots.get(slot, {}).get("type", 0)
        sock.sendto(_osc_encode(osc_addr, type_id), peer)
        return

    m = re.fullmatch(r"/fx/(\d+)/par/02", osc_addr)
    if m:
        slot = int(m.group(1))
        with _mixer_lock:
            cfg = _mixer_slots.get(slot)
            if not args:
                bpm = cfg["rx_bpm"] or cfg["init_bpm"] if cfg else None
                sock.sendto(_osc_encode(osc_addr, float(20.0 / bpm) if bpm else 0.0), peer)
            elif cfg and isinstance(args[0], float) and args[0] > 0:
                cfg["rx_bpm"]    = 20.0 / args[0]
                cfg["rx_count"] += 1
                cfg["rx_ts"]     = time.time()
        return

    # /fxrtn/{slot}/mix/on — no response needed


def _mixer_recv_loop(sock: socket.socket) -> None:
    while not _mixer_stop_evt.is_set():
        try:
            data, peer = sock.recvfrom(4096)
            _mixer_handle(data, sock, peer)
        except socket.timeout:
            pass
        except OSError:
            break
        except Exception:
            pass


def _mixer_start() -> None:
    global _mixer_sock, _mixer_running, _mixer_error, _mixer_svc_start
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.settimeout(0.2)
        sock.bind(("0.0.0.0", 10023))
        _mixer_stop_evt.clear()
        _mixer_sock     = sock
        _mixer_running  = True
        _mixer_error    = ""
        _mixer_svc_start = time.time()
        threading.Thread(target=_mixer_recv_loop, args=(sock,), daemon=True).start()
    except Exception as e:
        _mixer_error    = str(e)
        _mixer_running  = False


def _mixer_stop() -> None:
    global _mixer_sock, _mixer_running, _mixer_svc_start
    _mixer_stop_evt.set()
    try:
        if _mixer_sock:
            _mixer_sock.close()
            _mixer_sock = None
    except Exception:
        pass
    _mixer_running   = False
    _mixer_svc_start = None


def _mixer_toggle() -> None:
    with _mixer_svc_lock:
        if _mixer_running:
            _mixer_stop()
        else:
            _mixer_start()


# ═══════════════════════════════════════════════════════════════════════════════
# Keyboard reader — handles plain keys + CSI escape sequences (arrow keys)
# ═══════════════════════════════════════════════════════════════════════════════

_quit             = threading.Event()
_mixer_log_scroll = 0  # 0 = follow latest; written only from keyboard thread


def _next_char(timeout: float) -> str:
    return sys.stdin.read(1) if select.select([sys.stdin], [], [], timeout)[0] else ""


def _read_key() -> str | None:
    """Return a logical key name, or None if nothing arrived within 0.1 s."""
    ch = _next_char(0.1)
    if not ch:
        return None
    if ch != "\x1b":
        return ch
    # Possibly a CSI sequence — grab up to two more bytes quickly.
    ch2 = _next_char(0.05)
    if ch2 != "[":
        return "esc"
    ch3 = _next_char(0.05)
    return {"A": "up", "B": "down", "C": "right", "D": "left"}.get(ch3, "esc")


def _keyboard_reader() -> None:
    global _mixer_log_scroll
    if not sys.stdin.isatty():
        return
    fd  = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    try:
        tty.setcbreak(fd)
        while not _quit.is_set():
            key = _read_key()
            if key is None:
                continue
            if key in ("q", "Q", "\x03", "\x04"):
                _quit.set()
            elif key in ("k", "K"):
                _midi_toggle()
            elif key in ("m", "M"):
                _mixer_toggle()
            elif key == "up":
                _mixer_log_scroll += 1
            elif key == "down":
                _mixer_log_scroll = max(0, _mixer_log_scroll - 1)
            elif key in ("r", "R"):
                _mixer_log_scroll = 0
    except Exception:
        pass
    finally:
        try:
            termios.tcsetattr(fd, termios.TCSADRAIN, old)
        except Exception:
            pass


# ═══════════════════════════════════════════════════════════════════════════════
# Rich display helpers
# ═══════════════════════════════════════════════════════════════════════════════

def _uptime(start: float | None) -> str:
    if start is None:
        return "—"
    h, r = divmod(int(time.time() - start), 3600)
    m, s = divmod(r, 60)
    return f"{h:02d}:{m:02d}:{s:02d}"


def _sparkline(values: list[float], width: int = 38) -> str:
    if len(values) < 2:
        return ""
    lo, hi  = min(values), max(values)
    spread  = hi - lo
    chars   = (
        _SPARK[4] * min(len(values), width)
        if spread < 0.5
        else "".join(
            _SPARK[min(7, int((v - lo) / spread * 8))]
            for v in values[-width:]
        )
    )
    return f"[dim]{chars}[/dim]"


def _midi_clock_state(stats: dict | None) -> tuple[str, str]:
    """Return (border_colour, status_markup) based on clock gap."""
    if stats is None:
        return "yellow", "[yellow]waiting for clocks…[/yellow]"
    gap = time.time() - stats["clock_ts"]
    if gap > _CLOCK_GAP_DEAD:
        return "red", f"[red]✗ clock dead  ({gap:.1f}s gap)[/red]"
    if gap > _CLOCK_GAP_WARN:
        return "yellow", f"[yellow]⚠ clock gap  {gap:.1f}s[/yellow]"
    return "green", "[green]● flowing[/green]"


def _mixer_activity_state() -> tuple[str, str]:
    """Return (border_colour, status_markup) based on last OSC message age."""
    with _mixer_lock:
        last = _mixer_last_msg_ts
        total = _mixer_total_msgs
    if total == 0:
        return "blue", "[dim]no traffic yet[/dim]"
    gap = time.time() - last
    if gap > _MIXER_STALE_DEAD:
        return "red", f"[red]✗ no traffic  ({gap:.0f}s)[/red]"
    if gap > _MIXER_STALE_WARN:
        return "yellow", f"[yellow]⚠ stale  {gap:.0f}s ago[/yellow]"
    return "blue", f"[green]● active[/green]  [dim]{gap:.1f}s ago[/dim]"


def _svc_badge(running: bool, error: str, label: str, key: str) -> str:
    if error:
        return f"[red bold]✗ {label}: {error[:50]}[/red bold]"
    dot   = "[green]●[/green]" if running else "[dim]○[/dim]"
    state = "[green]on[/green]" if running else "[dim]off[/dim]"
    return f"{dot} [bold]{label}[/bold] {state} [dim]{key}[/dim]"


# ── Panel builders ────────────────────────────────────────────────────────────

def _build_header() -> Panel:
    midi_b  = _svc_badge(_midi_running,  _midi_error,  "MIDI sink",    "k")
    mixer_b = _svc_badge(_mixer_running, _mixer_error, "Mixer :10023", "m")
    hints   = "[dim]↑↓[/dim] scroll  [dim]r[/dim] latest  [dim]q[/dim] quit"
    return Panel(
        f"  {midi_b}      {mixer_b}      {hints}",
        box=box.HORIZONTALS,
        padding=(0, 0),
    )


def _build_midi_panel() -> Panel:
    stats               = _midi_compute_stats()
    clock_border, clock_status = _midi_clock_state(stats)
    border  = clock_border if _midi_running else "dim"
    title   = f"[bold]MIDI Clock Sink[/bold]  [dim]{_uptime(_midi_svc_start)}[/dim]"

    if not _midi_running:
        body = f"[red]{_midi_error}[/red]" if _midi_error else "[dim]stopped — k to start[/dim]"
        return Panel(body, title=title, border_style=border)

    if stats is None:
        port_info = ""
        if _midi_ports_available:
            port_info = f"\nAvailable outputs: {_midi_ports_available}"
        return Panel(
            f"Waiting for 0xF8 clock bytes…\n{clock_status}{port_info}",
            title=title, border_style=border,
        )

    bpm_color = {"green": "green", "yellow": "yellow", "red": "red"}.get(clock_border, "white")

    t = Table(box=box.SIMPLE, show_header=False, padding=(0, 1))
    t.add_column("Metric", style="bold", min_width=20)
    t.add_column("Value",  justify="right", min_width=13)

    ago = time.time() - stats["last_ts"] if stats["last_ts"] else 0.0

    t.add_row("BPM",               f"[bold {bpm_color}]{stats['bpm']:.2f}[/bold {bpm_color}]")
    t.add_row("Clock",             clock_status)
    t.add_row("PPQ (clocks/beat)", str(_MIDI_CPB))
    t.add_row("Total clocks",      str(stats["total_clocks"]))
    t.add_row("Window",            f"{stats['sample_count']} intervals")
    t.add_row("Other MIDI msgs",   str(stats["other_msgs"]))
    t.add_row("", "")
    t.add_row("[cyan]Mean interval[/cyan]", f"{stats['mean_us']:.1f} µs")
    t.add_row("[cyan]Std deviation[/cyan]", f"{stats['std_us']:.1f} µs")
    t.add_row("", "")
    t.add_row("[yellow]Jitter p50[/yellow]",  f"{stats['p50']:.1f} µs")
    t.add_row("[yellow]Jitter p75[/yellow]",  f"{stats['p75']:.1f} µs")
    t.add_row("[yellow]Jitter p95[/yellow]",  f"{stats['p95']:.1f} µs")
    t.add_row("[red]Jitter p99[/red]",        f"{stats['p99']:.1f} µs")
    t.add_row("[red]Jitter max[/red]",        f"{stats['max_jitter']:.1f} µs")
    t.add_row("", "")
    t.add_row("Last message", stats["last_hex"] or "—")
    t.add_row("Received",     f"{ago:.1f}s ago")

    spark = _sparkline(stats["bpm_hist"])
    hist_row = (
        Group(t, Text(f"  BPM history  {spark}", no_wrap=True))
        if spark else t
    )
    return Panel(hist_row, title=title, border_style=border)


def _build_mixer_panel() -> Panel:
    act_border, act_status = _mixer_activity_state()
    border = act_border if _mixer_running else "dim"
    title  = f"[bold]Mock Mixer  UDP :10023[/bold]  [dim]{_uptime(_mixer_svc_start)}[/dim]"

    with _mixer_lock:
        slots_snap  = copy.deepcopy(_mixer_slots)
        log_full    = list(_mixer_msg_log)
        total       = _mixer_total_msgs

    if not _mixer_running and total == 0:
        body = f"[red]{_mixer_error}[/red]" if _mixer_error else "[dim]stopped — m to start[/dim]"
        return Panel(body, title=title, border_style=border)

    # ── Slot table ────────────────────────────────────────────────────────
    st = Table(box=box.SIMPLE, show_header=True, header_style="bold cyan", padding=(0, 1))
    st.add_column("Slot",     justify="right", min_width=4)
    st.add_column("Name",     min_width=5)
    st.add_column("Init BPM", justify="right", min_width=9)
    st.add_column("Rx BPM",   justify="right", min_width=14)
    st.add_column("Syncs",    justify="right", min_width=5)
    st.add_column("Last sync",min_width=10)
    st.add_column("Compat",   justify="center", min_width=6)

    for sn, cfg in slots_snap.items():
        is_dly  = cfg["type"] == _DLY
        ns      = "bold green" if is_dly else ("dim" if cfg["type"] == 0 else "")
        name_m  = f"[{ns}]{cfg['name']}[/{ns}]" if ns else cfg["name"]
        init_m  = f"[cyan]{cfg['init_bpm']:.1f}[/cyan]" if cfg["init_bpm"] else "[dim]—[/dim]"

        rx = cfg["rx_bpm"]
        if rx is not None:
            diff  = rx - cfg["init_bpm"] if cfg["init_bpm"] else None
            dsuf  = f" ({'+' if diff and diff >= 0 else ''}{diff:.1f})" if diff is not None else ""
            rx_m  = f"[bold green]{rx:.1f}[/bold green][dim]{dsuf}[/dim]"
        else:
            rx_m = "[dim]—[/dim]"

        ago_m = f"[dim]{time.time() - cfg['rx_ts']:.0f}s ago[/dim]" if cfg["rx_ts"] else "[dim]—[/dim]"
        st.add_row(
            str(sn), name_m, init_m, rx_m,
            str(cfg["rx_count"]) if cfg["rx_count"] else "[dim]0[/dim]",
            ago_m,
            "[green]✓[/green]" if is_dly else "[dim]—[/dim]",
        )

    # ── Message log with scroll ───────────────────────────────────────────
    n      = len(log_full)
    scroll = min(_mixer_log_scroll, max(0, n - _MIXER_LOG_VISIBLE))

    if scroll == 0:
        visible    = log_full[-_MIXER_LOG_VISIBLE:]
        above      = max(0, n - _MIXER_LOG_VISIBLE)
        below      = 0
    else:
        end_idx    = n - scroll
        start_idx  = max(0, end_idx - _MIXER_LOG_VISIBLE)
        visible    = log_full[start_idx:end_idx]
        above      = start_idx
        below      = scroll  # = n - end_idx

    lt = Table(box=box.SIMPLE, show_header=True, header_style="bold", padding=(0, 1))
    lt.add_column("Time",        min_width=10, style="dim")
    lt.add_column("Peer",        min_width=18)
    lt.add_column("OSC address", min_width=22)
    lt.add_column("Args")

    for ts_v, peer_s, addr, args in visible:
        t_str = time.strftime("%H:%M:%S", time.localtime(ts_v))
        is_par02_set = (
            bool(re.fullmatch(r"/fx/\d+/par/02", addr))
            and args and isinstance(args[0], float) and args[0] > 0
        )
        if is_par02_set:
            args_m = f"[cyan]{args[0]:.5f}[/cyan]  [bold green]→ {20.0 / args[0]:.2f} BPM[/bold green]"
        elif args:
            args_m = "  ".join(
                f"[cyan]{a:.4f}[/cyan]" if isinstance(a, float) else
                f"[yellow]{a}[/yellow]" if isinstance(a, int) else f'"{a}"'
                for a in args
            )
        else:
            args_m = "[dim]—[/dim]"
        lt.add_row(t_str, peer_s, addr, args_m)

    parts: list = [st, Rule(style="dim")]
    if above > 0:
        parts.append(Text(f"  ↑ {above} older", style="dim italic"))
    parts.append(lt)
    if below > 0:
        parts.append(Text(f"  ↓ {below} newer  (↓ or r to follow)", style="yellow"))
    else:
        parts.append(Text(f"  {act_status}   Messages: {total}", no_wrap=True))

    return Panel(Group(*parts), title=title, border_style=border)


def _build_layout() -> Layout:
    layout = Layout()
    layout.split_column(
        Layout(name="header", size=3),
        Layout(name="body"),
    )
    layout["body"].split_row(
        Layout(name="midi",  ratio=2),
        Layout(name="mixer", ratio=3),
    )
    layout["header"].update(_build_header())
    layout["midi"].update(_build_midi_panel())
    layout["mixer"].update(_build_mixer_panel())
    return layout


# ═══════════════════════════════════════════════════════════════════════════════
# Entry point
# ═══════════════════════════════════════════════════════════════════════════════

def main() -> None:
    with _midi_svc_lock:
        _midi_start()
    with _mixer_svc_lock:
        _mixer_start()

    kb = threading.Thread(target=_keyboard_reader, daemon=True)
    kb.start()

    try:
        with Live(_build_layout(), refresh_per_second=4, screen=True) as live:
            while not _quit.is_set():
                live.update(_build_layout())
                time.sleep(0.25)
    except KeyboardInterrupt:
        pass
    finally:
        _quit.set()
        with _midi_svc_lock:
            _midi_stop()
        with _mixer_svc_lock:
            _mixer_stop()

    print("[mock-ethertap] stopped")


if __name__ == "__main__":
    main()
