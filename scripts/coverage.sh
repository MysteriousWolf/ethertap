#!/usr/bin/env bash
# coverage.sh — measure workspace code coverage and enforce a minimum threshold.
#
# Generates an HTML report and prints a text summary to stdout.
# Exits non-zero if line coverage falls below --threshold (default: 95).
#
# Requirements:
#   cargo install cargo-llvm-cov
#   rustup component add llvm-tools-preview
#   brew install gum          (optional: styled output; plain fallback used otherwise)
#
# Usage:
#   ./scripts/coverage.sh [--open] [--threshold N]
#
#   --open        open HTML report in the default browser when done
#   --threshold N minimum line-coverage % to pass (default: 95)
#
# Zed editor task:
#   Add to .zed/tasks.json:
#     { "label": "Coverage", "command": "bash", "args": ["scripts/coverage.sh"] }
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

# ── Output helpers (gum when available; plain fallback) ───────────────────────
if command -v gum &>/dev/null; then
    step() { gum log --level info "$*"; }
    err()  { gum log --level error "$*"; }
    ok()   { printf '\n'; gum style --foreground 2 --bold "  ✓  $*"; printf '\n'; }
    bail() { printf '\n'; gum style --foreground 1 --bold "  ✗  $*"; printf '\n'; }
else
    step() { echo "  → $*"; }
    err()  { echo "ERROR: $*" >&2; }
    ok()   { echo; echo "✓ $*"; echo; }
    bail() { echo; echo "✗ $*"; }
fi

# ── Verify tooling ────────────────────────────────────────────────────────────

step "Checking tooling"
if ! cargo llvm-cov --version &>/dev/null; then
    err "cargo-llvm-cov not found — install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
    exit 2
fi

# ── Run coverage ──────────────────────────────────────────────────────────────

OPEN_FLAG=()
[[ $OPEN -eq 1 ]] && OPEN_FLAG=(--open)

# Excluded from coverage — these surfaces have no headless test path:
#   src/editor.rs        — nih-plug-iced GUI rendering; no headless path exists
#   mock-suite/src/tui   — ratatui TUI rendering; widget impl blocks are UI-only
#   mock-suite/src/main  — CLI entry point (binary main); not library logic
#   mock-suite/src/clock_sink — Unix-only virtual MIDI port (OS device driver
#                               layer); the equivalent loopback_sink IS tested
IGNORE_REGEX='src/editor\.rs|src/midi_hw\.rs|mock-suite/src/tui\.rs|mock-suite/src/main\.rs|mock-suite/src/clock_sink\.rs'

# Filter the benign "N functions have mismatched data" llvm-cov warning.
# Root cause: tests/common/mod.rs is compiled into multiple test binaries and
# each exercises a different subset, so llvm-profdata sees conflicting hit
# counts on merge.  Tooling artifact — counts and pass/fail are unaffected.
#
# Two variants:
#   filter_llvm_warnings      — stderr only; stdout is real-time. Used for the
#                               test-run step so test progress appears live.
#   filter_report_warnings    — both stdout and stderr buffered. Used for report
#                               steps where the warning embeds in stdout text.
#
# Both use tmpfiles (not process substitution) to avoid a bash race condition
# where the parent exits before background grep finishes draining the pipe.
filter_llvm_warnings() {
    local _tmp _rc
    _tmp=$(mktemp)
    "$@" 2>"$_tmp"; _rc=$?
    grep -v "functions have mismatched data" "$_tmp" >&2 || true
    rm -f "$_tmp"
    return "$_rc"
}

filter_report_warnings() {
    local _out _err _rc
    _out=$(mktemp)
    _err=$(mktemp)
    "$@" >"$_out" 2>"$_err"; _rc=$?
    grep -v "functions have mismatched data" "$_out" || true
    grep -v "functions have mismatched data" "$_err" >&2 || true
    rm -f "$_out" "$_err"
    return "$_rc"
}

# Step 1: pre-count tests with a live spinner, then run with always-spinning progress.
# --list just prints test names (no execution) so it's fast on a warm cache.
# The llvm-cov instrumented build shares the same source, so the count is valid.
_disc_out=$(mktemp)
_disc_done=$(mktemp); rm -f "$_disc_done"
(cargo test --workspace -- --list 2>/dev/null > "$_disc_out"; touch "$_disc_done") &
_disc_pid=$!
_spin_frames=("⠋" "⠙" "⠹" "⠸" "⠼" "⠴" "⠦" "⠧" "⠇" "⠏")
_spin_i=0
while [[ ! -f "$_disc_done" ]]; do
    printf "\r  \033[36m%s\033[0m  Discovering tests…    " "${_spin_frames[$((_spin_i % 10))]}"
    _spin_i=$((_spin_i + 1))
    sleep 0.08
done
wait "$_disc_pid" 2>/dev/null || true
rm -f "$_disc_done"
printf "\r\033[K"
_total=$(grep -cE '^.+: test$' "$_disc_out" 2>/dev/null || echo 0)
rm -f "$_disc_out"
step "Running ${_total} tests"

# A background tick generator writes __TICK__ every ~80 ms into a FIFO that
# awk also reads cargo's output from, keeping the spinner frame advancing
# even between test completions (during compilation or slow suites).
_count_file=$(mktemp)
_tmp_err=$(mktemp)
_exit_file=$(mktemp)
_tick_sentinel=$(mktemp); rm -f "$_tick_sentinel"
_fifo=$(mktemp -u); mkfifo "$_fifo"

set +e

{
    cargo llvm-cov --workspace --exclude xtask --no-report 2>"$_tmp_err"
    echo $? > "$_exit_file"
    touch "$_tick_sentinel"
} > "$_fifo" &
_cargo_bg=$!

{
    while [[ ! -f "$_tick_sentinel" ]]; do
        sleep 0.08
        printf '__TICK__\n'
    done
} > "$_fifo" &
_tick_bg=$!

awk -v count_file="$_count_file" -v total="$_total" '
    function spin(n,    frames, nf) {
        split("⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏", frames, " ")
        nf = 10
        return frames[(n % nf) + 1]
    }
    function draw(    ) {
        if (total > 0) {
            printf "\r  \033[36m%s\033[0m  \033[1m%d\033[0m/%d  \033[2m(%d%%)\033[0m    ",
                spin(tick), passed, total, int(passed * 100 / total)
        } else {
            printf "\r  \033[36m%s\033[0m  \033[1m%d\033[0m tests…    ", spin(tick), passed
        }
        fflush()
        on_spinner = 1
    }
    function newline(    ) {
        if (on_spinner) { printf "\n"; on_spinner = 0 }
    }
    BEGIN { passed=0; failed=0; on_spinner=0; tick=0 }
    /^__TICK__$/ { tick++; draw(); next }
    /^test .* \.\.\. ok$/ {
        passed++; tick++; draw(); next
    }
    /^test .* \.\.\. FAILED$/ {
        newline()
        name = $0; sub(/^test /, "", name); sub(/ \.\.\. FAILED$/, "", name)
        printf "  \033[31m✗\033[0m  %s\n", name
        failed++; next
    }
    /^running [0-9]+ test/ { next }
    /^test result:/         { next }
    /^$/                    { next }
    { newline(); print }
    END { print passed > count_file }
' < "$_fifo"

wait "$_cargo_bg" 2>/dev/null || true
wait "$_tick_bg" 2>/dev/null || true
rm -f "$_fifo" "$_tick_sentinel"
COLLECT_EXIT=$(cat "$_exit_file" 2>/dev/null || echo 1)
rm -f "$_exit_file"
printf "\r\033[K"
set -e
grep -Ev "functions have mismatched data|^[[:space:]]+(Running|Finished|Compiling|Locking|Checking|Generating|Archiving)|^info: cargo-llvm-cov|^[[:space:]]*$" "$_tmp_err" >&2 || true
rm -f "$_tmp_err"

if [[ $COLLECT_EXIT -ne 0 ]]; then
    err "Test run failed (exit ${COLLECT_EXIT})"
    rm -f "$_count_file"
    exit $COLLECT_EXIT
fi

_passed=$(cat "$_count_file" 2>/dev/null || echo 0)
rm -f "$_count_file"
step "${_passed} tests passed"

# Step 2: coverage table + threshold check in one pass (--summary-only gives the
# per-file table without annotated source; --fail-under-lines sets the exit code).
step "Coverage"
set +e
_table_raw=$(filter_report_warnings cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE_REGEX" \
    --summary-only \
    --fail-under-lines "$THRESHOLD")
EXIT=$?
set -e

printf '\n'
echo "$_table_raw" | awk -v thr="$THRESHOLD" '
    /^[-]+$/ { next }
    NR == 1 {
        printf "  \033[1m%-32s  %-9s  %-9s\033[0m\n", "File", "Line%", "Fn%"
        printf "  \033[2m%s\033[0m\n", "──────────────────────────────────────────────────────"
        next
    }
    NF >= 10 {
        lpct = $10 + 0
        fpct = ($7 == "-" ? 100 : $7 + 0)
        lc = (lpct >= thr ? "\033[32m" : lpct >= 80 ? "\033[33m" : "\033[31m")
        fc = (fpct >= thr ? "\033[32m" : fpct >= 80 ? "\033[33m" : "\033[31m")
        if ($1 == "TOTAL") {
            printf "  \033[2m%s\033[0m\n", "──────────────────────────────────────────────────────"
            printf "  \033[1m%-32s\033[0m  %s%-9s\033[0m  %s%-9s\033[0m\n", $1, lc, $10, fc, $7
        } else {
            printf "  %-32s  %s%-9s\033[0m  %s%-9s\033[0m\n", $1, lc, $10, fc, $7
        }
    }
'
printf '\n'

# Step 3: generate HTML report (suppress cargo noise).
cargo llvm-cov report \
    --ignore-filename-regex "$IGNORE_REGEX" \
    --html \
    "${OPEN_FLAG[@]}" >/dev/null 2>&1

if [[ $EXIT -eq 0 ]]; then
    ok "Coverage PASS (≥ ${THRESHOLD}% lines)"
else
    bail "Coverage FAIL (< ${THRESHOLD}% lines)"
fi
echo "  HTML report: target/llvm-cov/html/index.html"

exit $EXIT
