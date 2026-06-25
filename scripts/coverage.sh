#!/usr/bin/env bash
# coverage.sh — measure workspace code coverage and enforce a minimum threshold.
#
# Generates an HTML report and prints a text summary to stdout.
# Exits non-zero if line coverage falls below --threshold (default: 90).
#
# Requirements:
#   cargo install cargo-llvm-cov
#   rustup component add llvm-tools-preview
#
# Usage:
#   ./scripts/coverage.sh [--open] [--threshold N]
#
#   --open        open HTML report in the default browser when done
#   --threshold N minimum line-coverage % to pass (default: 90)
#
# Zed editor task:
#   Add to .zed/tasks.json:
#     { "label": "coverage", "command": "bash", "args": ["scripts/coverage.sh"] }
#
# Exit codes:
#   0  — coverage at or above threshold
#   1  — coverage below threshold
#   2  — tooling not installed / build error

set -euo pipefail

OPEN=0
THRESHOLD=95

while [[ $# -gt 0 ]]; do
    case $1 in
        --open)       OPEN=1;           shift ;;
        --threshold)  THRESHOLD="$2";   shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# ── Verify tooling ────────────────────────────────────────────────────────────

if ! cargo llvm-cov --version &>/dev/null; then
    echo "cargo-llvm-cov not found. Install with:" >&2
    echo "  cargo install cargo-llvm-cov" >&2
    echo "  rustup component add llvm-tools-preview" >&2
    exit 2
fi

# ── Run coverage ──────────────────────────────────────────────────────────────

echo "Running coverage (threshold: ${THRESHOLD}%)…"
echo ""

OPEN_FLAG=()
[[ $OPEN -eq 1 ]] && OPEN_FLAG=(--open)

# Excluded from coverage — these surfaces have no headless test path:
#   src/editor.rs        — nih-plug-iced GUI rendering; no headless path exists
#   mock-suite/src/tui   — ratatui TUI rendering; widget impl blocks are UI-only
#   mock-suite/src/main  — CLI entry point (binary main); not library logic
#   mock-suite/src/clock_sink — Unix-only virtual MIDI port (OS device driver
#                               layer); the equivalent loopback_sink IS tested
IGNORE_REGEX='src/editor\.rs|src/midi_hw\.rs|mock-suite/src/tui\.rs|mock-suite/src/main\.rs|mock-suite/src/clock_sink\.rs|vendor/'

# Step 1: run tests and collect coverage data (no report output yet).
set +e
cargo llvm-cov \
    --workspace \
    --exclude xtask \
    --no-report
COLLECT_EXIT=$?
set -e

if [[ $COLLECT_EXIT -ne 0 ]]; then
    echo "Test run failed (exit ${COLLECT_EXIT})" >&2
    exit $COLLECT_EXIT
fi

# Step 2: print text summary to stdout (CI logs / terminal).
cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE_REGEX" \
    --text --summary-only

# Step 3: generate HTML report.
cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE_REGEX" \
    --html \
    "${OPEN_FLAG[@]}"

# Step 4: enforce threshold (exits non-zero if below).
set +e
cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE_REGEX" \
    --fail-under-lines "$THRESHOLD"
EXIT=$?
set -e

echo ""
if [[ $EXIT -eq 0 ]]; then
    echo "Coverage PASS (≥ ${THRESHOLD}% lines)"
else
    echo "Coverage FAIL (< ${THRESHOLD}% lines)" >&2
fi
echo "HTML report: target/llvm-cov/html/index.html"

exit $EXIT
