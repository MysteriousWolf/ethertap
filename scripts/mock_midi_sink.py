#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "python-rtmidi",
#     "rich",
# ]
# ///
"""
Mock MIDI sink for EtherTap standalone GUI testing.

Opens a virtual MIDI input port and displays a live dashboard: current BPM,
MIDI clock statistics, and jitter percentiles — all updated in-place using Rich.

Usage (preferred — auto-installs deps):
  uv run scripts/mock_midi_sink.py

Fallback (requires python-rtmidi and rich installed globally):
  python3 scripts/mock_midi_sink.py
"""

from __future__ import annotations

import select
import statistics
import sys
import termios
import threading
import time
import tty
from collections import deque

try:
    import rtmidi
except ImportError:
    print("error: python-rtmidi not available. Run with: uv run scripts/mock_midi_sink.py", file=sys.stderr)
    sys.exit(1)

try:
    from rich import box
    from rich.live import Live
    from rich.panel import Panel
    from rich.table import Table
except ImportError:
    print("error: rich not available. Run with: uv run scripts/mock_midi_sink.py", file=sys.stderr)
    sys.exit(1)


PORT_NAME = "EtherTap Mock MIDI Sink"
CLOCK_BYTE = 0xF8
CLOCKS_PER_BEAT = 24
# 241 timestamps → 240 intervals = 10 beats at 24 PPQ
WINDOW = 241

_lock = threading.Lock()
_clock_times: deque[float] = deque(maxlen=WINDOW)
_total_clocks = 0
_other_msgs = 0
_last_msg_hex = ""
_last_msg_ts = 0.0
_quit = threading.Event()


# ---------------------------------------------------------------------------
# MIDI callback (runs on rtmidi background thread)
# ---------------------------------------------------------------------------

def _midi_callback(event, data=None):
    global _total_clocks, _other_msgs, _last_msg_hex, _last_msg_ts
    message, _delta = event
    ts = time.time()
    with _lock:
        _last_msg_hex = " ".join(f"0x{b:02X}" for b in message)
        _last_msg_ts = ts
        if message[0] == CLOCK_BYTE:
            _total_clocks += 1
            _clock_times.append(ts)
        else:
            _other_msgs += 1


# ---------------------------------------------------------------------------
# Keyboard reader (separate thread, setcbreak so Ctrl+C still raises SIGINT)
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
                if ch in ("q", "Q", "\x03", "\x04"):  # q/Q/Ctrl-C/Ctrl-D
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
# Stats computation
# ---------------------------------------------------------------------------

def _percentile(sorted_data: list[float], p: float) -> float:
    """Linear-interpolation percentile over a pre-sorted list."""
    if not sorted_data:
        return 0.0
    idx = (p / 100.0) * (len(sorted_data) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(sorted_data) - 1)
    return sorted_data[lo] + (sorted_data[hi] - sorted_data[lo]) * (idx - lo)


def _compute_stats() -> dict | None:
    with _lock:
        times = list(_clock_times)
        total = _total_clocks
        other = _other_msgs
        last_hex = _last_msg_hex
        last_ts = _last_msg_ts

    if len(times) < 2:
        return None

    intervals = [times[i + 1] - times[i] for i in range(len(times) - 1)]
    mean_iv = statistics.mean(intervals)
    # BPM: there are CLOCKS_PER_BEAT ticks per beat, so one beat = mean_iv * 24 s
    bpm = 60.0 / (mean_iv * CLOCKS_PER_BEAT) if mean_iv > 0 else 0.0

    # Absolute deviation of each interval from the mean, in microseconds.
    # Sorted ascending so percentile indexing is straightforward.
    abs_jitter_us = sorted(abs(iv - mean_iv) * 1_000_000 for iv in intervals)

    return {
        "bpm": bpm,
        "total_clocks": total,
        "other_msgs": other,
        "sample_count": len(intervals),
        "mean_us": mean_iv * 1_000_000,
        "std_us": statistics.stdev(intervals) * 1_000_000 if len(intervals) > 1 else 0.0,
        "p50": _percentile(abs_jitter_us, 50),
        "p75": _percentile(abs_jitter_us, 75),
        "p95": _percentile(abs_jitter_us, 95),
        "p99": _percentile(abs_jitter_us, 99),
        "max_jitter": abs_jitter_us[-1] if abs_jitter_us else 0.0,
        "last_hex": last_hex,
        "last_ts": last_ts,
    }


# ---------------------------------------------------------------------------
# Rich panel builder
# ---------------------------------------------------------------------------

def _build_panel(stats: dict | None) -> Panel:
    HINTS = "[dim]q  quit[/dim]"

    if stats is None:
        return Panel(
            "Waiting for MIDI clock messages (0xF8)…",
            title="[bold]EtherTap Mock MIDI Sink[/bold]",
            subtitle=HINTS,
            border_style="dim",
        )

    t = Table(box=box.SIMPLE, show_header=False, padding=(0, 1))
    t.add_column("Metric", style="bold", min_width=24)
    t.add_column("Value", justify="right", min_width=14)

    ago = time.time() - stats["last_ts"] if stats["last_ts"] else 0.0
    bpm_color = "green" if stats["bpm"] > 0 else "dim"

    t.add_row("BPM", f"[bold {bpm_color}]{stats['bpm']:.2f}[/bold {bpm_color}]")
    t.add_row("PPQ (clocks / beat)", str(CLOCKS_PER_BEAT))
    t.add_row("Total clocks", str(stats["total_clocks"]))
    t.add_row("Measurement window", f"{stats['sample_count']} intervals")
    t.add_row("Other MIDI messages", str(stats["other_msgs"]))
    t.add_row("", "")
    t.add_row("[cyan]Mean interval[/cyan]", f"{stats['mean_us']:.1f} µs")
    t.add_row("[cyan]Std deviation[/cyan]", f"{stats['std_us']:.1f} µs")
    t.add_row("", "")
    t.add_row("[yellow]Jitter p50[/yellow]", f"{stats['p50']:.1f} µs")
    t.add_row("[yellow]Jitter p75[/yellow]", f"{stats['p75']:.1f} µs")
    t.add_row("[yellow]Jitter p95[/yellow]", f"{stats['p95']:.1f} µs")
    t.add_row("[red]Jitter p99[/red]", f"{stats['p99']:.1f} µs")
    t.add_row("[red]Jitter max[/red]", f"{stats['max_jitter']:.1f} µs")
    t.add_row("", "")
    t.add_row("Last message", stats["last_hex"] or "—")
    t.add_row("Received", f"{ago:.1f}s ago")

    return Panel(
        t,
        title="[bold]EtherTap Mock MIDI Sink[/bold]",
        subtitle=HINTS,
        border_style="blue",
    )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main() -> None:
    midi_in = rtmidi.MidiIn()
    try:
        midi_in.open_virtual_port(PORT_NAME)
    except Exception as e:
        print(f"failed to open virtual port: {e}", file=sys.stderr)
        sys.exit(1)
    midi_in.set_callback(_midi_callback)

    kb = threading.Thread(target=_keyboard_reader, daemon=True)
    kb.start()

    try:
        with Live(_build_panel(None), refresh_per_second=4, screen=True) as live:
            while not _quit.is_set():
                live.update(_build_panel(_compute_stats()))
                time.sleep(0.25)
    except KeyboardInterrupt:
        pass
    finally:
        _quit.set()

    print("[mock-midi] stopped")
    try:
        midi_in.close_port()
    except Exception:
        pass


if __name__ == "__main__":
    main()
