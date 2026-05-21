#!/usr/bin/env bash
# Wrapper to run the mock MIDI sink via uv so zed tasks invoke a simple shell
# command. This avoids potential quoting/arg-passing issues in some task runners.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

# Ensure uv knows the script's dependency; this creates or updates uv's
# internal manifest so `uv run` will install python-rtmidi into .venv.
uv add --script scripts/mock_midi_sink.py python-rtmidi || true

exec uv run scripts/mock_midi_sink.py "$@"
