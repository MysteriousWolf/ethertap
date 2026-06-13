# TE dark reskin — flatten the editor, keep amber

## Goal

Restyle the iced editor to a dark-mode teenage-engineering aesthetic: flat surfaces, hairline borders, unified generous corner radius, high-contrast text. Layout, control choices, and the amber brand accent stay exactly as they are.

## Non-goals

- No structural layout changes: same sections, rows, control choices, window dimensions. (Minimal padding/spacing adjustments for breathability ARE in scope — see success criteria.)
- No accent hue change — amber (`THEME.accent`) stays.
- No status-LED semantic color changes (green/red/amber keep their meanings).
- No font swap — JetBrains Mono + Solar icons stay.
- Standalone DAW-shell chrome (Asiimov theme, `daw_chrome_*`) untouched — test harness scaffolding, not shipped UI.

## Success criteria

- [ ] No simulated 3D: `bevel_hi` / `bevel_lo` light/dark edge pairs are gone from rendered styles — every surface is one flat fill with at most a 1px uniform hairline border.
- [ ] One corner-radius language: buttons share a single larger radius (TE rounded-key feel); inputs and section cards share a consistent (possibly smaller) radius. No mixed 0/4px leftovers on shipped-UI widgets.
- [ ] Background steps read as flat layers: window bg darkest, section cards one subtle step up (or hairline-only), buttons one step above cards — no borders doing double duty as fake depth.
- [ ] Text contrast tightened: primary text near off-white on dark, secondary clearly dimmer but readable; placeholder/disabled distinct from secondary.
- [ ] Active/selected, Force, Enabled, Error button states remain visually distinct at a glance.
- [ ] Inputs and dropdowns (TextInput, PickList) share the same flat style language as buttons/cards — same fill-step logic, same hairline border tone, same radius family. No widget class reads as belonging to a different theme.
- [ ] Minimal padding pass: small, consistent breathing room around sections and controls; outer bounds correctly packed — no component flush against the window edge or clipped, all content fits the fixed window dimensions.
- [ ] All changes confined to `src/editor.rs` theme tokens + stylesheet impls + padding/spacing constants (the file's own restyle contract, `src/editor.rs:3-6`).
- [ ] `cargo build` both feature sets compiles; clippy both feature sets `-D warnings` clean; `cargo test --workspace` green (no behavior change expected).
- [ ] Visual pass via `./scripts/gui_test.sh` — user confirms the look.

## Recommendation

The editor was built for this: all colors live in `Theme::dark()` (`src/editor.rs:152-217`) and shape constants (`BORDER_RADIUS`, stylesheet impls) sit directly below. The reskin is a token + stylesheet edit, no widget-tree changes.

Direction sketch (implementer may tune values during the visual pass):

| Token group | Current | TE direction |
|-------------|---------|--------------|
| `bg` | 12,12,16 charcoal | near-black neutral (drop the blue cast, e.g. ~10,10,10) |
| `surface` / `section_bg` | bluish charcoals | neutral grays, flat steps (~22 / ~16) |
| `bevel_hi` / `bevel_lo` | light/dark edge pair | collapse to one hairline border tone (or remove borders where fill contrast suffices) |
| `BORDER_RADIUS` | 4.0 global | buttons up (~7–9, rounded-key feel); inputs/cards consistent |
| `text` / `text_dim` | 205,205,215 / 95,95,108 | off-white warm-neutral (~230) / dimmer but readable (~120) |
| `accent`, `ok`, `err`, `warn` | amber + status set | unchanged |
| selected/danger/enabled/error fills | colored fills | keep hues, flatten (no border, slightly desaturated TE-style blocks) |

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Retoken `Theme::dark()` + flatten stylesheets (button/input/picklist/container) + radius pass | `src/editor.rs` | atomic-surgeon | 1 | builds + clippy clean both feature sets; tests green |
| 2 | Visual pass with user via standalone GUI; tune tokens from feedback | `src/editor.rs` | atomic-surgeon | 1 | user sign-off on `./scripts/gui_test.sh` look |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| Flat surfaces lose affordance (buttons stop reading as pressable) | med | Keep fill-step contrast between card and button; hover/pressed lighten deltas already exist (`lighten()`) |
| Taste mismatch — "TE" means different things | med | Checkpoint 2 is an explicit user visual pass before done |
| Radius change clips small fixed-size widgets | low | Radii tuned during visual pass; layout constants untouched |

## Change log

### 2026-06-12 — CP-2 visual-pass feedback folded in

**What changed:** Added two success criteria from the user's first visual pass: (1) unify TextInput/PickList styling with the rest of the flat language; (2) minimal padding/breathing-room pass with correctly packed outer bounds. Non-goal relaxed accordingly.

**Why:** User visual pass found inputs/dropdowns reading as a different style from other components, and the UI too tightly packed.

**Superseded:** prior non-goal "No layout changes: same sections, rows, sizes, spacing, window dimensions" — spacing/padding tweaks now in scope; sections, rows, control choices, window dimensions still frozen.

### 2026-06-13 — CP-2 round 3: outer-frame padding + input/picklist recess

**What changed:** Two existing success criteria ("Inputs and dropdowns share flat style language" and "Minimal padding pass... outer bounds correctly packed") were not yet fully met after CP-2 round 1. User's second visual pass flagged: (1) `PluginFrame` (the single outer container in both VST3 and standalone modes, `src/editor.rs:2195-2199` and `src/editor.rs:2464-2467`) has zero padding — content sits flush against the frame's hairline border; (2) IP/port `TextInput`s and the MIDI-clock PPQ `PickList` still read as a different widget class from buttons even though they already reference `surface`/`surface_border`/`BORDER_RADIUS` tokens — same fill step as idle buttons gives data-entry fields no distinct identity.

**Why:** Same-fill-as-buttons reads as "no input styling" rather than "unified styling" — a TE-style device gives data readouts/inputs a recessed "screen" treatment, distinct from raised buttons, using the existing `inset_bg`/`inset_border` tokens (`src/editor.rs:122-124`, already defined for "recessed text inputs, sunken panels" but currently unused after CP-2 moved `EtherInputLocked` off them).

**Superseded:** none — both items are concrete instances of criteria already in scope; this entry records the diagnosis and fix direction (recess `EtherInput`/`EtherInputLocked`/`PpqPickStyle` onto `inset_bg`/`inset_border`; add minimal uniform padding to `PluginFrame`'s content in both render paths) rather than changing the criteria.

## Implementation log

### Shipped — 2026-06-13

Built across 3 iterations of `/subagent-implementation`. Commits (chronological):

- `ad28e90` — CP-1: collapsed `bevel_hi`/`bevel_lo` into a single `border` hairline token, retokened `Theme::dark()` (bg/section/surface step ladder, text/dim/muted), split `BORDER_RADIUS_BTN`/`BORDER_RADIUS`, flattened all `BtnKind` variants.
- `d4701ce` — CP-2 round 1: rebased `EtherInput`/`EtherInputLocked` onto `surface`/`surface_border`, closed the redundant `border` token (F-1), evened content-column padding.
- `8b31441` — CP-2 round 2/3: added `PLUGIN_FRAME_PAD` (3px uniform inset on the outer `PluginFrame` in both render paths) and recessed `EtherInput`/`EtherInputLocked`/`PpqPickStyle` onto `inset_bg`/`inset_border` for a distinct "screen" look vs. raised buttons.

**Out-of-scope work performed during this build:**
- none

**Unforeseens — surprises that emerged during implementation:**
- CP-2 round 1's token unification (moving inputs/picklist onto `surface`/`surface_border`, same fill step as idle buttons) read to the user as "still not styled" rather than "unified" — round 2/3 diagnosed this as a missing visual identity for data-entry widgets and resolved it with the pre-existing but unused `inset_bg`/`inset_border` tokens (recessed "screen" treatment).
- MIDI rescan-status parity (raised during CP-2 round 1's visual pass) was scoped out of this reskin and built as a separate inline feature — commits `acf3e09`, `ec65d3a` (no spec file, inline brief).

**Deferred items still open:**
- none
