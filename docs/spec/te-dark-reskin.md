# TE dark reskin — flatten the editor, keep amber

## Goal

Restyle the iced editor to a dark-mode teenage-engineering aesthetic: flat surfaces, hairline borders, unified generous corner radius, high-contrast text. Layout, control choices, and the amber brand accent stay exactly as they are.

## Non-goals

- No layout changes: same sections, rows, sizes, spacing, window dimensions.
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
- [ ] All changes confined to `src/editor.rs` theme tokens + stylesheet impls (the file's own restyle contract, `src/editor.rs:3-6`).
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

<!-- Populated on first amendment after the spec is approved. -->
