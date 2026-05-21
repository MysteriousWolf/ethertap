#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "python-rtmidi",
# ]
# ///
"""
uv:deps = python-rtmidi

Mock MIDI sink for EtherTap standalone GUI testing.

This script uses `uv` script metadata so `uv run scripts/mock_midi_sink.py`
will automatically create an isolated virtual environment (in .venv) and
install `python-rtmidi` locally for this script only. No global package
installs are required.

Usage with uv (preferred):
  uv run scripts/mock_midi_sink.py

Fallback usage without uv (if you have python-rtmidi installed globally):
  python3 scripts/mock_midi_sink.py

The script opens a virtual MIDI input port named "EtherTap Mock MIDI Sink"
and prints incoming MIDI messages in hex so you can observe clock bytes
(0xF8) and other messages.
"""

from __future__ import annotations

import sys
import time

try:
    import rtmidi
except Exception:
    print(
        "error: python-rtmidi not available. Run with: uv run scripts/mock_midi_sink.py",
        file=sys.stderr,
    )
    sys.exit(1)


PORT_NAME = "EtherTap Mock MIDI Sink"


def midi_callback(event, data=None):
    message, delta = event
    ts = time.time()
    # Display bytes in hex for clarity.
    hex_bytes = " ".join(f"0x{b:02X}" for b in message)
    print(f"[midi {ts:.3f}] {hex_bytes}")


def main() -> None:
    print(f"Starting mock MIDI sink — virtual input port: {PORT_NAME}")
    print("Press Ctrl+C to stop")
    midi_in = rtmidi.MidiIn()
    try:
        midi_in.open_virtual_port(PORT_NAME)
    except Exception as e:
        print(f"failed to open virtual port: {e}", file=sys.stderr)
        sys.exit(1)
    midi_in.set_callback(midi_callback)

    try:
        while True:
            # Keep the process alive; callbacks run in background threads.
            time.sleep(1.0)
    except KeyboardInterrupt:
        print("\n[mock-midi] stopped")
    finally:
        try:
            midi_in.close_port()
        except Exception:
            pass


if __name__ == "__main__":
    main()
