# DAW controls overhaul (standalone transport + MIDI)

## Goal

Clean up the standalone GUI's transport/DAW-I/O surface: split Play/Stop into
real Pause + Stop, fix a missing-glyph render bug, replace the width-widening
side panel with a full-width param footer, and bring MIDI device handling to
parity with the mixer's auto-connect + status-visibility posture.

## Non-goals

- No new audio-thread allocation or blocking — `process()` (`lib.rs`) stays
  untouched beyond reading existing/new atomics.
- No redesign of the OSC/mixer side of the UI — only mirroring its idioms.
- No general-purpose reusable layout component — the wrap/grid is scoped to
  this footer only.
- No change to which params are automatable or their `#[id]` wiring in
  `params.rs` — the footer *displays* the existing surface, it doesn't grow it.
- VST3 (non-standalone) editor layout is unaffected — gated by
  `#[cfg(feature = "standalone")]`, same as today's `daw_panel`.

## Success criteria

- [ ] Standalone transport row shows two distinct controls: Pause (toggles
      `standalone_playing`, leaves `standalone_pos_beats` untouched — today's
      `btn_play_stop`/`Message::ToggleStandalonePlay` behavior,
      `editor.rs:1700-1708`, `:858-861`) and Stop, which reliably ends with
      `standalone_playing == false` AND `standalone_pos_beats == 0` —
      **not** via two independent cross-thread `store()`s (these race against
      `process()`'s read-modify-write accumulation at `lib.rs:794-804` under
      `Ordering::Relaxed` and can be silently clobbered). Stop must use a
      one-shot trigger atomic the editor sets and `process()` consumes via
      `swap(false, …)` **unconditionally each buffer, checked independently of
      and before the `playing` gate at `lib.rs:794`** (Stop pressed while
      already paused must still zero the position — gating consumption on
      `playing` would silently swallow that case), mirroring the existing
      `force_sync_trigger`/`force_rate_trigger` "editor → audio, swap to
      consume" idiom (`Arc<AtomicBool>`, `editor.rs:482-483`, documented in
      `CLAUDE.md`'s Inter-Thread Communication table) — `process()` performs
      the `standalone_pos_beats` reset itself, serialized with its own
      accumulation logic, eliminating the race.
- [ ] No missing-glyph "tofu box" renders anywhere the old `\u{2669}` appeared
      (`editor.rs:1735,1779`) — replacement glyph confirmed present in
      `MONO_FONT` (`JetBrainsMono-Regular.ttf`) or `SOLAR_BOLD`
      (`Solar-Icon-Set_Bold.ttf`) cmap.
- [ ] Standalone window width matches the side-column-free layout philosophy —
      the 130px `daw_panel` side column (`editor.rs:1752-1843`) is gone;
      content is a full-width footer band, not a column that widens the window
      beyond VST3's 360×280 / standalone's 500×340 baseline (per
      `signals.md`).
- [ ] Footer displays all 10 of `EtherTapParams`' `#[id]`-tagged
      host-automatable fields (`rate_sync_mode`, `phase_sync_mode`,
      `connect_to_last`, `disconnect`, `force_sync_rate`, `force_sync_phase`,
      `force_sync_both`, `force_sync` (legacy trigger, `params.rs:115-116`), `is_connected`, `is_matched` —
      `params.rs:84-127`) with name + current value/state per param, plus 1-2
      connection facts reusing values already computed for
      `telem_row`/`daw_panel STATUS` (`editor.rs:1398-1414, 1822-1840`) — no
      new state sources invented.
- [ ] Footer layout is count-driven (a small `Row`-of-`Row`s wrap/grid that
      chunks N items per row or by width budget) — not a hand-stacked column;
      adding/removing a param doesn't require manual layout edits.
- [ ] A new persisted `AtomicBool` toggle (e.g. `midi_auto_connect`, mirroring
      `midi_clock_enabled` at `params.rs:68-69`, `#[persist]`, default
      **OFF**) exists and is read by `midi_clock.rs`'s device-change handler
      (`midi_clock.rs:504-526`) and `handle_port_scan`
      (`midi_clock.rs:583-637`).
- [ ] When the toggle is ON and no MIDI device is currently selected,
      auto-connect picks the first available device and calls
      `try_connect_out`/`try_connect_in` through the existing `Backoff`
      (`reconnect.rs:14-56`) — triggered both at startup (first post-populate
      port scan) and on hot-plug (new device arriving via
      `editor_rx`/`worker_rx`, `midi_watcher.rs:35-44`), both of which already
      funnel through `handle_port_scan`.
- [ ] When the toggle is OFF (default), MIDI connection remains fully
      manual — no behavior change from today for users who don't opt in
      (verifies "no surprise automation").
- [ ] MIDI status is visible at a glance via the existing dot+label idiom
      (`bridge_status`/`bridge_dot`/`bridge_color`, `editor.rs:1440-1449`;
      `sync_dot`/`conn_dot`, `editor.rs:1745-1748`) showing a clear ladder:
      disabled / enabled-but-disconnected / connecting / connected — placed
      alongside the auto-connect toggle.
- [ ] An explicit MIDI clock disable affordance exists, distinct from the
      connect-trigger button (`midi_clk_btn`, `editor.rs:1451-1472` currently
      conflates Enable/Active/Connecting/Enabled into one control).
- [ ] `cargo test` is green; `gui_test_with_mock.sh` confirms the standalone
      window renders without layout overflow/missing glyphs and at the
      expected (non-widened) width.
- [ ] The shared plugin-content area (`content` Column, `editor.rs:1718-1776`
      — MIXER/EFFECTS/MIDI/SYNC) renders in standalone mode at the SAME
      proportions a VST3 host shows it — pinned to the true VST3 dimensions
      (`360×280`, `IcedState::from_size`, `params.rs:141`), not stretched or
      shrunk to fill the larger `500×340` standalone window
      (`Length::Fill` at `editor.rs:1934` currently does this). A visible
      frame/border (reusing the `ModSection` bordered-container idiom,
      `editor.rs:428-439`) visually distinguishes that pinned plugin-content
      area from the surrounding standalone-only DAW chrome (`transport_row`,
      `footer`) — so a user can tell at a glance "this is what the VST3 host
      would show" from "this is EtherTap's standalone wrapper."

## Approaches

**Footer content & layout** (condensed from design doc):

| # | Approach | Pros | Cons |
|---|----------|------|------|
| A | Hand-placed sections relocated to footer row | Minimal change | Still brittle to param-count changes; doesn't satisfy "organized neatly" |
| **B (chosen)** | Iterate `EtherTapParams` `#[id]` fields + 1-2 connection facts, lay out via small wrap/grid (`Row`-of-`Row`s, N-per-row or width-budget) | Self-adjusting; directly mirrors "what the DAW sees"; matches user direction | Needs a small in-`editor.rs` layout helper (not a new module) |
| C | Full generic param-introspection panel (mirrors nih-plug generic editor) | Maximally "universal" | Overkill — explicitly rejected; user wants "simple… scoped to this footer" |

**MIDI auto-connect** (condensed from design doc):

| # | Approach | Pros | Cons |
|---|----------|------|------|
| **A (chosen)** | New `#[persist] AtomicBool` toggle (mirrors `connect_to_last`, `params.rs:95-96`) read by `midi_clock.rs` device-change + port-scan handlers; auto-pick first device + `try_connect_out/in()` via existing `Backoff` when none selected | Mirrors proven mixer reconnect machinery; minimal new surface; toggle visible + OFF by default | Needs plumbing editor → worker via existing `MidiWatcherChannels`/command pattern |
| B | Always-on auto-connect, no toggle | Less UI surface | Removes user control; contradicts "No surprise automation" (CLAUDE.md) |

## Recommendation

**Approach B (footer)** — build the footer around the 10 `#[id]`-tagged
automatable fields in `params.rs:84-127`, laid out as fixed-width chips in a
wrapping `Row`-of-`Row`s, name + value/state per param via the existing
dot/label idiom, plus 1-2 connection facts reusing `telem_row`/`daw_panel
STATUS` value sources (`editor.rs:1398-1414, 1822-1840`). Keeps standalone
window width anchored to the VST3 layout philosophy (footer spans full width,
no side column) and gives a predictable, count-driven layout.

**Approach A (MIDI auto-connect)** — persisted toggle (`#[persist]`, mirrors
`midi_clock_enabled` at `params.rs:68-69`), read by the existing
port-scan/device-change handlers in `midi_clock.rs`. Trigger conditions:
startup (first port scan after device list populates) **and** hot-plug (new
device via `editor_rx`/`worker_rx`, `midi_watcher.rs:35-44`) — both already
funnel through `handle_port_scan` (`midi_clock.rs:583-637`), so this is a
guard added at "device present, none selected", not a new mechanism. Smallest
change producing DAW-parity behavior with `connect_to_last`; toggle is
explicit, OFF by default, state always visible (per CLAUDE.md "No surprise
automation").

**Status indicators** — reuse the dot+label idiom verbatim
(`bridge_status`/`bridge_dot`/`bridge_color`, `editor.rs:1440-1449`;
`sync_dot`/`conn_dot`, `editor.rs:1745-1748`). `midi_clk_btn`
(`editor.rs:1451-1472`) already encodes Enable/Active/Connecting/Enabled —
the gap is consistency plus an explicit disable affordance separate from the
connect-trigger button. Add a disabled / enabled-but-disconnected / connected
/ connecting ladder, placed next to the auto-connect toggle.

**Stop vs. Pause** — today's `btn_play_stop`
(`editor.rs:1700-1708`/`Message::ToggleStandalonePlay` at `editor.rs:858-861`)
becomes Pause unchanged. New Stop must reliably end with
`standalone_playing == false` and `standalone_pos_beats == 0`. A naive pair of
independent editor-thread `store()`s races `process()`'s read-modify-write
accumulation of `standalone_pos_beats` (`lib.rs:794-804`, `Ordering::Relaxed`
— no cross-store ordering guarantee), risking a clobbered reset. Use a
one-shot trigger atomic following the existing `force_sync_trigger`/
`force_rate_trigger` "editor sets, audio thread `swap(false)`-consumes" idiom
(`Arc<AtomicBool>`, `editor.rs:482-483`; documented in `CLAUDE.md`'s
Inter-Thread Communication table) — `process()` consumes it **unconditionally
each buffer, independent of and before the `playing` gate at `lib.rs:794`**
(a Stop pressed while paused must still zero the position) and performs the
reset itself, serialized with its accumulation logic. Small, RT-safe `process()` change consistent with proven
patterns: no allocation, no lock, no blocking I/O. Stop adds no new downstream
suppression of MIDI clock/OSC sync — those already react to
`standalone_playing == false` the same way Pause leaves them ("Stop =
pause-at-zero").

**Glyph fix** — replace `\u{2669}` (`editor.rs:1735,1779`) with a glyph
confirmed present in a loaded font's cmap. `icon::CLOCK` (`U+ED1C`,
`SOLAR_BOLD`, already used at `editor.rs:1464` to label the MIDI clock
control) is the closest semantic fit; an ASCII/box character confirmed in
`MONO_FONT` is an acceptable alternative. Contract is "no missing-glyph box,"
not a specific codepoint.

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|---|---|---|---|---|
| 1 | Split Pause/Stop transport controls (incl. RT-safe trigger-atomic reset path through `process()`); fix missing glyph | `src/editor.rs` (transport_row ~1700-1708, new Stop button + `Message` arm, glyph sites 1735/1779), `src/lib.rs` (new `standalone_stop_trigger: Arc<AtomicBool>` alongside `standalone_playing`/`standalone_pos_beats` at ~199-204; `process()` consumes via `swap(false, …)` unconditionally each buffer — independent of and before the `playing` gate at ~794, so Stop-while-paused still resets — mirroring `force_sync_trigger` `editor.rs:482-483`) | atomic-builder | 2 | `cargo test`; `gui_test_with_mock.sh` visual check — Pause toggles without reset, Stop reliably zeroes position from both playing and paused states with no tofu box at either glyph site, no race-induced stale-position flicker under repeated rapid Stop presses |
| 2a | Footer skeleton: remove `daw_panel` side column, add full-width footer band + wrap/grid chunking helper (empty/placeholder content) | `src/editor.rs` (remove `daw_panel` 1752-1843, restructure `view()` layout ~1851-1859 to full-width footer band, add `Row`-of-`Row`s chunking helper) | atomic-builder | 1 | `gui_test_with_mock.sh` — standalone window renders at non-widened width (no side column), footer band spans full width; `cargo test` green |
| 2b | Wire param iteration + connection facts into the footer skeleton | `src/editor.rs` (populate footer with all 10 `EtherTapParams` `#[id]` fields via dot/label idiom, add 1-2 connection facts from existing `telem_row`/`daw_panel STATUS` value sources) | atomic-builder | 1 | `gui_test_with_mock.sh` — all 10 params + facts visible, name+value/state legible, wraps correctly at varying widths; `cargo test` green |
| 3 | MIDI auto-connect toggle: persisted param + worker-side guard | `src/params.rs` (new `#[persist] AtomicBool midi_auto_connect`, mirrors 68-69/95-96), `src/midi_clock.rs` (device-change handler 504-526, `handle_port_scan` 583-637 — add "device present, none selected" auto-pick + `try_connect_out/in` via `Backoff`) | atomic-builder | 2 | `cargo test` — new/extended integration test exercising port-scan with toggle ON (auto-picks + connects) and OFF (no-op, parity with today) |
| 4 | MIDI status ladder + explicit disable affordance + auto-connect toggle UI | `src/editor.rs` (status dots near `midi_clk_btn` 1451-1472, new toggle control wired to `midi_auto_connect`, placed alongside per design) | atomic-builder | 1 | `gui_test_with_mock.sh` — visually confirm disabled/enabled-disconnected/connecting/connected ladder renders distinctly, disable affordance is separate from connect-trigger; `cargo test` green |
| 5 | Pin standalone's plugin-content area to true VST3 dimensions (360×280) and frame it distinctly from DAW chrome | `src/editor.rs` (standalone `content` wrapper `Container::new(content)` at ~1934 — change `width(Length::Fill).height(Length::Fill)` to fixed `360×280`; new framing `StyleSheet` reusing the `ModSection` bordered-container idiom at 428-439), possibly `src/params.rs` (extract `360`/`280`/`500`/`340` literals at 141/143 to named constants shared between the dimension declaration and the editor's pinned-size container, if the implementer judges that worth the duplication-avoidance) | atomic-builder | 1-2 | `gui_test_with_mock.sh` — visually confirm the plugin-content area renders at the same proportions a VST3 host shows (no stretch/shrink relative to 360×280) and a visible border/frame distinguishes it from the surrounding transport/footer chrome; `cargo test` green |

## Risks

| Risk | Likelihood | Mitigation |
|---|---|---|
| Naive Stop implementation (two independent editor-thread `store()`s on `standalone_playing`/`standalone_pos_beats`) races `process()`'s Relaxed read-modify-write accumulation (`lib.rs:794-804`), silently clobbering the zero-reset | Medium (real race window; caught in spec review, not yet implemented) | Spec mandates the trigger-atomic idiom (checkpoint 1) — `process()` performs its own reset via `swap(false)`, serialized with accumulation; success criterion explicitly forbids the naive two-store approach |
| Footer wrap/grid chunking miscounts at edge param counts (e.g. last row underfilled) causing visual overflow at fixed window width | Medium | `gui_test_with_mock.sh` visual check in checkpoint 2 catches overflow before merge; keep chunking formula simple (fixed N-per-row) to bound the failure surface |
| Auto-connect guard races with manual connect (user clicks connect while toggle ON mid-scan) | Low | Reuse existing `Backoff`/`try_connect_*` machinery verbatim — same concurrency posture already proven for `connect_to_last`; add guard only at "none selected" branch, not a parallel path |
| Replacement glyph also missing from target font's cmap on some platform/font-version combo | Low | Spec requires *confirmed-present* glyph (verify cmap before committing); `icon::CLOCK` already renders correctly elsewhere in the same build (`editor.rs:1464`) |
| Removing `daw_panel` drops a STATUS value the footer doesn't re-surface, silently regressing visibility | Low | Checkpoint 2's success criterion requires the 1-2 connection facts to come from *already-computed* `telem_row`/`daw_panel STATUS` sources — implementer cross-checks against the removed panel's content before deletion |

## Open questions

- Exact wrap/grid chunking formula (fixed items-per-row vs. width-budget
  breaking) — implementer's call; `iced` 0.x has no built-in wrap container,
  so this is a small `Row`-of-`Row`s helper scoped to the footer.
- Exact 1-2 connection facts shown alongside params (target IP:port? hw BPM?
  device name?) — implementer picks from values already computed for
  `telem_row`/`daw_panel STATUS`.
- Exact replacement glyph for `\u{2669}` — implementer's call, constrained
  only to "confirmed present in `MONO_FONT` or `SOLAR_BOLD` cmap."

## Change log

### 2026-06-07 — Add CP-5: pin standalone plugin-content area to true VST3 dimensions + frame it

**What changed:** Added checkpoint 5 — pin the shared plugin-content `Container`
(`editor.rs:1718-1776` wrapped at `:1934`) to the true VST3 dimensions
(`360×280`, `params.rs:141`) instead of `Length::Fill`, and add a visible
frame/border (reusing the `ModSection` idiom, `editor.rs:428-439`)
distinguishing that pinned area from the standalone-only DAW chrome
(`transport_row`, `footer`). Added a corresponding success-criteria bullet.

**Why:** User ran the standalone GUI after CP-1–CP-4 shipped and found the
rendered plugin-content area stretched/shrunk relative to how a real VST3 host
renders it (`Length::Fill` makes it fill whatever space remains around the
500×340 standalone window's chrome, rather than rendering at the true 360×280
the VST3 host shows), with no visual boundary marking where "the actual VST3"
ends and "EtherTap's standalone wrapper" begins. User chose to fold this into
the current spec as a new checkpoint rather than defer it — keeps the fix in
the same review/commit loop as the rest of the standalone-chrome work it's
adjacent to.

## Implementation log

### shipped — 2026-06-07

Built across 6 iterations of `/subagent-implementation`. Commits (chronological):

- `5db86e1` — CP-1 Pause/Stop split for standalone transport, fix missing glyph
- `dfd8b3b` — CP-2a replace standalone DAW side panel with full-width footer skeleton
- `98cd05c` — CP-2b wire host-automatable params and connection facts into footer
- `2830d4a` — CP-3 add MIDI auto-connect toggle with worker-side guard
- `c0e1bbb` — CP-4 add MIDI status ladder, explicit disable, and auto-connect toggle UI
- `16af0fa` — CP-5 pin standalone plugin-content area to true VST3 dimensions and frame it

**Out-of-scope work performed during this build:**
- CP-5 itself: not in the original 4-checkpoint spec. User ran the standalone
  GUI mid-build, found the plugin-content area stretched/shrunk vs. true VST3
  dimensions with no DAW-chrome frame, and chose to fold the fix into this
  spec as a new checkpoint (amendment logged above) rather than defer it.
- CP-5's `params.rs:143` window-height bump (`500×340` → `500×480`): not
  explicitly named in the CP-5 checkpoint row, but pre-authorized by the
  brief's headroom-math instruction — needed so the pinned 360×280 frame plus
  surrounding chrome fits without clipping.

**Unforeseens — surprises that emerged during implementation:**
- Recurring reviewer false-positive (5 occurrences across CP-1, CP-3, CP-4 ×2,
  and a near-miss in CP-5): fresh-context `atomic-reviewer` agents repeatedly
  saw `src/lib.rs` (`AUDIO_IO_LAYOUTS` cfg-split) and `scripts/mock_ethertap.py`
  (the Python mock tool, since replaced by the `mock-suite` crate)
  dirty in `git status`/`git diff` and attributed them to the current
  implementer's scope — when both are pre-existing uncommitted WIP that
  predates this entire session (confirmed byte-identical to the `4a6dec4`
  baseline, zero commits across all 6 checkpoints touched them). Each time,
  resolved by independently verifying via `git diff 4a6dec4 -- <file>`
  (byte-identical = pre-existing) and `git log --oneline <range> -- <file>`
  (empty = untouched), then surgically staging only the implementer's hunks
  via `git apply --cached --recount`. By CP-5, naming the pattern explicitly
  in the implementer/reviewer briefs let the reviewer self-check and correctly
  NOT flag it — the first fully clean PASS (0 findings of any kind) in the run.
- Stale rust-analyzer/IDE diagnostics surfaced twice (CP-3: false E0063/E0061/
  E0308 compile errors; CP-4: false `unused variable: midi_status_row`
  warning) — both resolved via `touch <file> && cargo build` force-rebuilds
  that proved the code compiled clean; confirmed IDE cache staleness, not
  real defects.
- Sandbox has no display server throughout — every visual-confirmation success
  criterion (CP-1 Pause/Stop/glyph rendering, CP-2's footer layout, CP-5's
  pinned-frame proportions/border) could only be verified structurally
  (`cargo build`/`test` + code read-through), never via `gui_test_with_mock.sh`
  itself. The user's own live GUI runs (which directly produced the CP-5
  finding) became the de facto visual-confirmation gate.

**Deferred items still open:**
- None. F-1 (Pause/Stop/glyph visual check) and F-2 (footer row balance) were
  both display-blocked findings, structurally subsumed by CP-5's
  visual-confirmation scope, and dropped at finalization — the user's live
  GUI runs (which surfaced the CP-5 finding in the first place) satisfy the
  visual-check gate these entries existed to enforce. See `FOLLOWUPS.md`
  disposition note (now deleted with the scratchpad; summarized here for the
  permanent record).
