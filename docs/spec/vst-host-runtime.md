# Headless VST host runtime (workspace sub-crate)

## Goal

A new workspace sub-crate, `vst-runtime`, that loads a single Rust-native
`nih_plug::Plugin` impl (e.g. `EtherTap`) directly as a linked library and
drives it programmatically — params, transport, MIDI, audio buffers — so
EtherTap's test suite can script directed runtime scenarios and assert on
resulting state/emitted OSC, in-process, with zero IPC and zero serialization.

## Non-goals

- **No plugin discovery / scanning.** Single plugin, explicitly referenced as
  a generic type parameter (`P: Plugin`) — never an OS-path search.
- **No arbitrary-`.vst3`-bundle loading.** This crate drives Rust-native
  `nih_plug::Plugin` impls linked directly as library code — *not* a VST3 COM
  host that loads compiled bundles from disk (no `IPluginFactory`,
  `IComponent`, `IHostApplication`, etc.). That capability — "any VST3, any
  vendor, any language" — is deliberately deferred to a **separate future
  spec**, scoped explicitly as "arbitrary compiled-bundle hosting" (design
  doc `docs/design/vst-host-runtime.md` Approaches B/C). Nothing in this spec
  builds toward it; it is not "maybe later in this crate."
- **No GUI / editor rendering in v1.** The runtime is headless — it never
  calls `Plugin::editor()` or instantiates `nih_plug_iced`. The
  standalone-binary-replacement use case (which *would* need a window) is a
  named **follow-on** enabled by this crate's driver core, not built here.
- **Not a general-purpose multi-plugin DAW.** One plugin per runtime
  instance, no plugin chains, no project files, no session management.
- **Not RT-constrained.** The harness's own driving loop (the code that calls
  `process()` in a scripted sequence) is test/dev tooling — it may allocate,
  block, and use whatever control flow is convenient. Only the *call into*
  `process()` needs to mirror a real host's calling convention closely enough
  that EtherTap's RT-sensitive logic (e.g. quantised-sync, BPM settle) is
  exercised realistically. The harness itself carries no RT-safety guarantee
  and must not be assumed to provide one.

## Success criteria

- [ ] `cargo new --lib vst-runtime` (or equivalent) exists as a workspace
      member alongside `xtask` (`Cargo.toml:2` `members = [".", "xtask"]` →
      `members = [".", "xtask", "vst-runtime"]`), builds with `cargo build -p
      vst-runtime`.
- [ ] Crate exposes a generic driver, e.g. `fn run<P: Plugin>(...)` /
      `struct Harness<P: Plugin>`, that: constructs `P::default()`, calls
      `initialize`, drives a `process()` loop over caller-supplied buffers,
      and calls `deactivate`/`reset` per the same sequence
      `nih_export_standalone<P: Plugin>()` already performs
      (`vendor/nih-plug/src/wrapper/standalone.rs:50`).
- [ ] Exposes a scripted-scenario API: caller can sequence parameter changes,
      transport state (playing/tempo/position), MIDI events, and audio-buffer
      shape across multiple `process()` calls without hand-rolling the
      buffer/transport plumbing each time.
- [ ] Exposes an assertion surface: caller can observe plugin-emitted state
      after a scripted run (e.g. read back params, captured output buffers,
      or — for EtherTap specifically — OSC messages emitted on `cmd_tx`)
      and assert on it in a `#[test]`.
- [ ] At least one EtherTap-specific integration test (in `ethertap`'s own
      `tests/`, depending on `vst-runtime` as a dev-dependency) drives
      `EtherTap` through a scripted parameter+MIDI+transport sequence and
      asserts on resulting state or emitted OSC — proving the "directed test
      automation" goal end-to-end, not just that the harness compiles.
- [ ] `cargo test -p vst-runtime` and the new EtherTap integration test both
      pass green.
- [ ] No code path in `vst-runtime` calls `Plugin::editor()`,
      `nih_plug_iced`, or opens a window — headless is enforced by absence,
      not by a feature flag that could silently default on.

## Approaches

| # | Approach | Pros | Cons |
|---|----------|------|------|
| A | **Native-`Plugin`-trait runtime only** — generic `fn run<P: Plugin>(…)` harness driving any Rust-native nih-plug `Plugin` impl directly via its trait methods (mode A above) | Near-zero new risk — reuses a fully-mapped, stable trait surface and a calling convention nih-plug already runs in production; zero new unstable deps; immediately covers both stated goals for EtherTap | Can only host Rust-native nih-plug plugins, not arbitrary compiled `.vst3` from other vendors — doesn't deliver literal "any VST3" |
| B | **Arbitrary-`.vst3`-bundle host** — full VST3 COM host-side implementation (mode B above), built from `vst3-sys`/`vst3-com` | Delivers the literal "any VST3, any vendor" capability | High cost, high risk: no host-side COM code anywhere in the dependency tree to build from; git-only/custom-branch deps; platform-specific bundle loading; multi-week scope with unresolved unknowns |
| C | **Wrap an existing host crate** (`plugin_host` or `Rack`) for bundle loading, build the automation layer on top | Avoids writing host-side COM from scratch | `plugin_host`'s VST3 bridge is reportedly stub-level (unverified); adds an external dependency of unknown stability to a tool meant to be the *trustworthy* test harness — if the harness itself is shaky, it undermines the "insight into runtime behavior" goal |
| D | **Phased: ship A now, scope B/C as a separate future spec** | De-risks by sequencing — proves the harness shape and delivers value immediately without blocking on B/C's unresolved maturity questions; A's shape (generic driver, scripted scenarios, assertion surface) is largely reusable scaffolding for B/C later regardless of which bundle-loading approach wins | Doesn't deliver "any VST3" in v1 — must be explicit about that scope boundary so it isn't read as a silent walk-back |

## Recommendation

**D — ship Approach A now; defer B/C to a future spec**, explicitly scoped as
"arbitrary compiled-bundle hosting," once A's harness shape has proven out.

`nih_export_standalone<P: Plugin>()` (`vendor/nih-plug/src/wrapper/
standalone.rs:50`) is *already* a generic single-plugin host driving the same
`Plugin` trait surface this spec targets (`initialize`/`process`/`reset`/
`deactivate`/`params`/`editor`/`task_executor`/`filter_state`, bound
`Default + Send + 'static` — `vendor/nih-plug/src/plugin.rs`). Approach A is
substantially "extract and generalize existing, production-proven machinery
into a reusable, scriptable form," not "build a VST3 host from nothing." That
is a fundamentally smaller, lower-risk problem than B (no host-side COM
precedent anywhere in this dependency tree, git-only/custom-branch
`vst3-sys`/`vst3-com` deps, platform-specific bundle loading) or C (rests on
`plugin_host`'s reportedly stub-level VST3 bridge — unacceptable foundation
for a *trustworthy* test harness).

A alone fully satisfies both motivating goals: in-process scripted test
automation (zero IPC, zero serialization — the plugin is linked as a crate),
and a reusable driver core that a later standalone-replacement effort can
wrap with its own DAW chrome, decoupling that chrome from EtherTap's
`editor.rs` (see `daw-controls-overhaul` spec CP-1–5 for the current
entanglement). "Any VST3" remains a real ambition but is separable — scoping
it out explicitly keeps the door open for a dedicated future spec.

## Scope decisions (resolving the design doc's open questions)

- **GUI shape — headless-only for v1.** The runtime never touches
  `Plugin::editor()` or `nih_plug_iced`. This satisfies the test-automation
  goal directly. The standalone-binary-replacement angle (which needs a
  window via `Plugin::editor()` → `vendor/nih-plug/src/plugin.rs:165`,
  `Option<Box<dyn Editor>>`) is a **named follow-on**: a future spec can wrap
  this crate's driver core with its own chrome-rendering layer. Not built
  here, not designed here — only *not precluded* here (the driver core's API
  must not assume headlessness in a way that would block that follow-on, but
  no GUI scaffolding is written in this checkpoint set).
- **RT-fidelity — driving loop is test/dev tooling, not RT-constrained.**
  Stated as a non-goal and a success-criterion boundary above. Only the call
  *into* `process()` needs to mirror a real host's convention; the harness
  loop around it may allocate and block freely.
- **Crate name — `vst-runtime`.** Flat workspace member dir at root, matching
  the `xtask/` precedent (`Cargo.toml:2`). Reads as "test/dev tooling that
  also backs the eventual standalone binary," not as a third
  plugin-shipping artifact (rejected alternatives: `runtime/` — too vague
  inside an audio-plugin workspace; `vst-host-runtime/` — redundant with the
  design-doc filename, and "host" overclaims given the Non-goals above).

## Checkpoints

| # | Checkpoint | Files/areas | Agent | Est. files | Verifies |
|---|------------|-------------|-------|------------|----------|
| 1 | Crate scaffold + generic driver core: `vst-runtime` workspace member, `Harness<P: Plugin>` (or equivalent) wrapping construct→`initialize`→`process` loop→`deactivate`/`reset`, mirroring `nih_export_standalone`'s sequence | `Cargo.toml`, `vst-runtime/Cargo.toml`, `vst-runtime/src/lib.rs` | atomic-builder | ~3 | Crate builds (`cargo build -p vst-runtime`); a minimal smoke test drives a trivial/mock `Plugin` impl through one `process()` call and observes output |
| 2 | Scripted-scenario API + assertion surface: param/transport/MIDI sequencing builder, buffer-shape helpers, observation/assertion accessors over post-run plugin state | `vst-runtime/src/lib.rs` (or split `scenario.rs`/`assert.rs`), `vst-runtime/tests/` | atomic-builder | ~3 | `cargo test -p vst-runtime` green; scenario API drives a multi-step sequence (param change → transport change → MIDI event → process) and assertion surface reads back resulting state |
| 3 | EtherTap proof-of-concept integration test: dev-dependency on `vst-runtime`, scripted parameter+MIDI+transport sequence against `EtherTap`, asserts on resulting state / emitted OSC (`cmd_tx`) | `Cargo.toml` (dev-deps), `tests/` (new integration test file) | atomic-builder | ~2 | New EtherTap integration test passes green and demonstrably exercises the "directed test automation" goal end-to-end (not a stub) |

## Risks

| Risk | Likelihood | Mitigation |
|------|-----------|-----------|
| `Plugin` trait's `Default + Send + 'static` bound or its construction sequence assumes a real host context (e.g. `ProcessContext`, GUI context) that's awkward to stub headlessly | Medium | `nih_export_standalone` already constructs and drives `P` headlessly-enough for a window-optional standalone run — mirror its construction path (`vendor/nih-plug/src/wrapper/standalone.rs:50`) rather than inventing a new one |
| Scenario API scope creep — "just one more scriptable axis" turns CP-2 into an open-ended DSL | Medium | Bound CP-2 to the four axes named in the goal (param/transport/MIDI/audio buffers) and the assertions CP-3 actually needs; no speculative scenario primitives |
| EtherTap-specific assertions (CP-3) require reaching into `cmd_tx`/internal state in ways that aren't currently exposed for testing | Low–Medium | `tests/integration_tests.rs` already exercises EtherTap's network worker via `MockMixer`/`spawn_worker` (`tests/common/mod.rs`) — reuse that observation pattern rather than adding new test-only public surface to `lib.rs` |
| Headless boundary erodes over time (someone adds an `editor()` call "just for debugging") | Low | Success criteria explicitly assert headlessness by absence (no `Plugin::editor()` / `nih_plug_iced` references); reviewer checks this on every amendment touching `vst-runtime` |

## Implementation log

### Shipped — 2026-06-09

Built across 3 iterations of /subagent-implementation. Commits (chronological):

- `7b6c3b8` — CP-1: `vst-runtime` crate, `Harness<P: Plugin>`, smoke test
- `d82ddf5` — CP-2: `ScenarioBuilder` API, `ScenarioResult`, `Harness::plugin()`/`plugin_mut()`/`scenario()`
- `20127ce` — CP-3: `EtherTapParams` re-export, `ethertap_params()` accessor, `tests/vst_runtime_integration.rs`
- `11033dd` — polish: `debug_assert!` diagnostics for dropped `BackgroundTask` (F-1/F-2)

**Out-of-scope work performed during this build:** none.

**Unforeseens:**
- Transport position fields (`pos_samples`, `pos_beats`) are `pub(crate)` in nih-plug — `ScenarioBuilder::step()` cannot auto-advance position. Documented; workaround (supply fresh `Transport` per step) noted in API doc comment.

**Deferred items still open:**
- `vst-runtime-transport-pos` — Transport position auto-advance: add a nih-plug patch exposing `pos_samples`/`pos_beats` as `pub`, then update `.step()` to auto-advance. Tracked in `.claude/project/followups/vst-runtime-transport-pos.md`.

## Change log

<!-- Populated on first amendment after the spec is approved. Do not log drafting/refinement turns. -->
