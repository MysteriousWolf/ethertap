#!/usr/bin/env bash
# test.sh — cargo test --workspace with real-time gum-styled test output.
#
# Each test prints as it finishes (✓ green / ✗ red).
# Build noise is suppressed; a final pass/fail count is shown.
#
# Usage:
#   ./scripts/test.sh [-- <cargo-test-args>]

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

# ── Discover tests with a live spinner ───────────────────────────────────────
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

# ── Test run with always-spinning progress ────────────────────────────────────
# A background tick generator writes __TICK__ every ~80 ms into a FIFO that
# awk also reads cargo's output from. This keeps the spinner frame advancing
# even when no tests complete (e.g. during compilation or slow suites).
_count_file=$(mktemp)
_exit_file=$(mktemp)
_tick_sentinel=$(mktemp); rm -f "$_tick_sentinel"
_fifo=$(mktemp -u); mkfifo "$_fifo"

set +e

# cargo in background; on exit: capture exit code and signal tick to stop
{
    cargo test --workspace "$@" 2>&1
    echo $? > "$_exit_file"
    touch "$_tick_sentinel"
} > "$_fifo" &
_cargo_bg=$!

# Tick generator: fires every ~80 ms until cargo signals done
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
        # Truncate section to keep the line from wrapping (pad with spaces to clear old text)
        label = (section != "") ? "  \033[2m" section "\033[0m" : ""
        if (total > 0) {
            printf "\r  \033[36m%s\033[0m  \033[1m%d\033[0m/%d  \033[2m(%d%%)\033[0m%s\033[K",
                spin(tick), passed, total, int(passed * 100 / total), label
        } else {
            printf "\r  \033[36m%s\033[0m  \033[1m%d\033[0m tests…%s\033[K", spin(tick), passed, label
        }
        fflush()
    }
    function fail_line(name    ) {
        printf "\r\033[K  \033[31m✗\033[0m  %s\n", name
    }

    BEGIN { passed=0; failed=0; tick=0; section="" }

    /^__TICK__$/ { tick++; draw(); next }

    # Section headers — update section label in spinner, no newline
    /[[:space:]]Running tests\// {
        s = $0; sub(/.*Running tests\//, "", s); sub(/\.rs[[:space:]].*$/, "", s)
        section = s; draw(); next
    }
    /[[:space:]]Running unittests / {
        s = $0; sub(/.*deps\//, "", s); sub(/-[a-f0-9].*$/, "", s)
        section = s; draw(); next
    }
    /^[[:space:]]*Doc-tests / {
        s = $0; sub(/^[[:space:]]*Doc-tests[[:space:]]/, "", s)
        section = "doc: " s; draw(); next
    }

    /^test .* \.\.\. ok$/ {
        passed++; tick++; draw(); next
    }
    /^test .* \.\.\. FAILED$/ {
        name = $0; sub(/^test /, "", name); sub(/ \.\.\. FAILED$/, "", name)
        fail_line(name); failed++; draw(); next
    }

    # Cargo / libtest noise
    /^[[:space:]]+(Compiling|Finished|Locking|Checking|Fresh|Updating|Downloading|Generating|Archiving|Documenting)/ { next }
    /^all doctests ran/ { next }
    /^running [0-9]+ test/ { next }
    /^test result:/ { next }
    /^$/ { next }

    { printf "\r\033[K"; print }

    END { print passed ":" failed > count_file }
' < "$_fifo"

wait "$_cargo_bg" 2>/dev/null || true
wait "$_tick_bg" 2>/dev/null || true
rm -f "$_fifo" "$_tick_sentinel"
TEST_EXIT=$(cat "$_exit_file" 2>/dev/null || echo 1)
rm -f "$_exit_file"
printf "\r\033[K"
set -e

_counts=$(cat "$_count_file" 2>/dev/null || echo "0:0")
rm -f "$_count_file"
_passed="${_counts%%:*}"
_failed="${_counts##*:}"

if [[ $TEST_EXIT -eq 0 ]]; then
    ok "${_passed} tests passed"
else
    bail "${_passed} passed, ${_failed} failed"
    exit $TEST_EXIT
fi
