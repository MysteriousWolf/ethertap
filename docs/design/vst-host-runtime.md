# Headless VST host runtime (workspace sub-crate)

## Problem

EtherTap's standalone mode is a single bespoke binary
(`nih_export_standalone::<ethertap::EtherTap>()`, `src/bin/gui_test.rs:14`)
wrapping nih-plug's standalone machinery, with hand-built "DAW chrome"
(`transport_row`/footer/frame, `editor.rs`, see `daw-controls-overhaul` spec
CP-1–5) glued onto EtherTap's own editor. Two pains follow from this shape:

1. **No programmatic test harness.** The only way to exercise EtherTap's
   runtime behavior beyond bare `process()` unit tests (`src/lib.rs` `mod
   tests`) is to launch the GUI by hand. There's no way to script "feed it
   this BPM curve, these MIDI events, this transport sequence, then assert on
   the OSC traffic it emits" — the kind of directed, repeatable runtime
   insight a host-side harness gives you for free.
2. **Standalone chrome is entangled with EtherTap's UI.** The transport/
   footer/frame in `editor.rs` exists only because the standalone binary has
   nowhere else to put DAW-side controls — they're bolted onto the plugin's
   own `view()`. A generic runtime would own that chrome itself, and EtherTap
   would just be "a plugin this runtime happens to load" — cleanly decoupled,
   the way a real DAW relates to any plugin it hosts.

## Goals

- A reusable, **programmatically controllable** host runtime: load one
  plugin, drive it via parameters / transport / MIDI / audio buffers, observe
  what it does — for automated testing and runtime insight.
- **Single plugin, explicitly referenced** — no scanning, no discovery.
- Becomes the standalone-binary backend, replacing `nih_export_standalone`
  and the bespoke chrome in `editor.rs` over time.
- Lives as a **workspace sub-crate** (alongside `xtask`,
  `Cargo.toml:2` `members = [".", "xtask"]`) so EtherTap's own test suite can
  depend on it and drive it directly — in-process, no IPC, no serialization
  boundary.

## Non-goals

- Plugin discovery / scanning across OS install paths.
- A general-purpose multi-plugin DAW.
- GUI embedding (rendering a hosted plugin's native view) — see Open
  questions; likely permanently out of scope for the *test-harness* use case,
  conditionally in scope for the *standalone-replacement* use case in a later
  phase.

## What "host" means here — two materially different things

The investigation surfaced a fork that the initial framing ("run any VST3")
glossed over. There are two distinct things a "VST3 runtime" could mean, and
they have wildly different costs:

### A. Driving a Rust-native `nih_plug::Plugin` directly

`nih_export_standalone<P: Plugin>()` (`vendor/nih-plug/src/wrapper/
standalone.rs:50`) is *already* generic over any type implementing
`Plugin: Default + Send + 'static` (`vendor/nih-plug/src/plugin.rs` —
required methods `initialize`/`process`/`reset`/`deactivate`/`params`/
`editor`, full surface mapped during Ground). EtherTap is exactly such a
type. Driving it means calling these trait methods directly — the same
sequence nih-plug's own standalone wrapper already performs internally.
**No VST3 ABI is involved.** This is closer to "expose nih-plug's internal
host-loop as a reusable, scriptable library" than to "build a VST3 host."

This mode satisfies *both* stated goals for EtherTap (and any other
nih-plug-based Rust plugin in this workspace or linked as a crate) at near-
zero new risk: zero new unstable dependencies, a trait surface that's already
fully mapped and stable, and a calling convention nih-plug has exercised in
production for years.

### B. Loading an arbitrary compiled `.vst3` bundle (any vendor, any language)

This is a genuine VST3 **host** in the traditional sense — load a `.vst3`
bundle from disk, pull its `IPluginFactory`, instantiate `IComponent`/
`IAudioProcessor`/`IEditController`, and — critically — implement the
**host-side** callback interfaces the plugin calls *out* to
(`IHostApplication`, `IComponentHandler` — stored at `vendor/nih-plug/src/
wrapper/vst3/inner.rs:46`, `IPlugFrame` — `vendor/nih-plug/src/wrapper/vst3/
view.rs:59`). Nothing in this codebase or its dependency tree implements
the host side of this protocol — nih-plug's `vst3/wrapper.rs:77-704` is
entirely plugin-side.

The raw bindings exist (`vst3-sys`/`vst3-com` 0.1.0, but **git-only**, custom
branch `robbert-vdh/vst3-sys#fix/drop-box-from-raw` — not on crates.io,
currently reachable only as nih-plug's transitive deps, would need a direct
git dependency or `[patch]` entry to use from a new crate). Community host
crates exist (`plugin_host`, `Rack`) but `plugin_host`'s own docs describe
its VST3 bridge as "fully scaffolded... method stubs" — unverified maturity,
possibly not production-usable. Bundle loading is also platform-specific
(macOS `.vst3` bundles vs. Windows DLL-in-folder vs. Linux `.so`-in-folder).

This is a multi-week subsystem in its own right, with real unknowns about
whether any existing crate is usable as-is.

## Approaches

| # | Approach | Pros | Cons |
|---|----------|------|------|
| A | **Native-`Plugin`-trait runtime only** — generic `fn run<P: Plugin>(…)` harness driving any Rust-native nih-plug `Plugin` impl directly via its trait methods (mode A above) | Near-zero new risk — reuses a fully-mapped, stable trait surface and a calling convention nih-plug already runs in production; zero new unstable deps; immediately covers both stated goals for EtherTap | Can only host Rust-native nih-plug plugins, not arbitrary compiled `.vst3` from other vendors — doesn't deliver literal "any VST3" |
| B | **Arbitrary-`.vst3`-bundle host** — full VST3 COM host-side implementation (mode B above), built from `vst3-sys`/`vst3-com` | Delivers the literal "any VST3, any vendor" capability | High cost, high risk: no host-side COM code anywhere in the dependency tree to build from; git-only/custom-branch deps; platform-specific bundle loading; multi-week scope with unresolved unknowns |
| C | **Wrap an existing host crate** (`plugin_host` or `Rack`) for bundle loading, build the automation layer on top | Avoids writing host-side COM from scratch | `plugin_host`'s VST3 bridge is reportedly stub-level (unverified); adds an external dependency of unknown stability to a tool meant to be the *trustworthy* test harness — if the harness itself is shaky, it undermines the "insight into runtime behavior" goal |
| D | **Phased: ship A now, scope B/C as a separate future spec** | De-risks by sequencing — proves the harness shape and delivers value immediately without blocking on B/C's unresolved maturity questions; A's shape (generic driver, scripted scenarios, assertion surface) is largely reusable scaffolding for B/C later regardless of which bundle-loading approach wins | Doesn't deliver "any VST3" in v1 — must be explicit about that scope boundary so it isn't read as a silent walk-back |

## Recommendation

**D — ship Approach A now; defer B/C to a future spec, explicitly scoped as
"arbitrary compiled-bundle hosting," once A's harness shape has proven out.**

Evidence basis: `nih_export_standalone<P: Plugin>()` already *is* a generic
single-plugin host — Approach A is substantially "extract and generalize
existing, production-proven machinery into a reusable, scriptable form,"
not "build a VST3 host from nothing." That's a fundamentally different (and
much smaller) engineering problem than Approach B, which has no precedent
anywhere in this dependency tree and rests on unverified third-party crate
maturity (Approach C) or a from-scratch host-side COM implementation
(Approach B proper).

Approach A alone fully satisfies both motivating goals as stated:
- **Test automation / runtime insight**: drive `EtherTap` (or any
  `nih_plug::Plugin` impl) in-process — script BPM curves, MIDI sequences,
  transport changes; assert on emitted OSC/state — with zero IPC and zero
  serialization boundary, because the plugin is linked directly as a library
  (exactly the "or directly... via cargo as a library" option the user named
  as acceptable).
- **Standalone-runtime replacement**: a generic `run<P: Plugin>(…)` harness
  *is* what the standalone binary needs underneath — it would own the DAW
  chrome itself (decoupled from EtherTap's `editor.rs`) and EtherTap becomes
  "a plugin this runtime loads," resolving the entanglement pain directly.

"Any VST3" in the literal sense (third-party compiled bundles) remains a real
ambition but is a separable, much larger bet that shouldn't gate or dilute
the immediate, low-risk win. Scoping it out explicitly — rather than silently
dropping it — keeps the door open for a dedicated future spec once there's
evidence Approach A's shape generalizes cleanly.

## Open questions

- **GUI for the standalone-replacement use case**: Approach A's harness is
  headless by design (test-automation goal doesn't need a window). But
  "replaces `nih_export_standalone`" implies a window eventually. Does the
  runtime grow its own minimal chrome-rendering (reusing `nih_plug_iced`,
  already a dependency) around the loaded plugin's `editor()`  — which *is*
  expressible for a `Plugin` impl without touching the VST3 ABI
  (`Plugin::editor()`, `vendor/nih-plug/src/plugin.rs:165`, returns
  `Option<Box<dyn Editor>>`) — or does the headless harness and the
  standalone-GUI-replacement stay two separate consumers of a shared driving
  core? Spec should pick one shape; this design doesn't resolve it.
- **RT-fidelity of the driving loop**: should the runtime's own
  buffer-feeding loop honor the same no-alloc/no-block discipline as a real
  DAW's audio thread (so timing-sensitive behavior — e.g. EtherTap's
  quantised-sync logic — is exercised realistically), or is "test/dev tooling,
  relaxed constraints outside the `process()` call boundary" sufficient? Leans
  toward the latter (the call *into* `process()` is what matters; the harness
  driving it is not itself an audio thread), but worth confirming explicitly
  in the spec's success criteria so a future contributor doesn't assume
  RT-safety guarantees the harness doesn't actually provide.
- **Crate name / location**: `vst-host-runtime/`? `runtime/`? Workspace
  convention is flat member dirs at root (`xtask/`, `Cargo.toml:2`) —
  whatever name is picked should read clearly as "test/dev tooling that also
  backs the standalone binary," not as a third plugin-shipping artifact.
