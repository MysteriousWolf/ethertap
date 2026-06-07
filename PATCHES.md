# Vendored Dependency Patches

The `vendor/` directory is **not committed** to this repo. Run `./scripts/setup.sh`
after cloning to populate it.

---

## baseview

**Upstream:** https://github.com/RustAudio/baseview  
**Pinned rev:** `579130e`  
**Patches:** `patches/baseview/`

### Why we patch

nih-plug pulls two incompatible revisions of baseview (standalone wrapper uses
`579130e`, iced_baseview uses `1d9806d5`). Cargo's `[patch]` can only redirect
all instances to a single source, so we pin both to `579130e` and apply the
following forward-ports and fixes.

### Patches

| File | What it does |
|------|-------------|
| `Cargo_toml.patch` | Adds `rwh04` alias (`raw-window-handle = "0.4"`) so the crate exposes both rwh 0.4 and 0.5 traits |
| `src_window_rs.patch` | Implements `rwh04::HasRawWindowHandle` for `WindowHandle` and `Window`, changes `open_parented` bound to rwh04, adds `open_as_if_parented` dispatch, adds `rwh05_to_rwh04` converter |
| `src_macos_window_rs.patch` | Changes `open_parented` parent bound to `rwh04::HasRawWindowHandle`, adds `open_as_if_parented` (creates NSView without NSWindow — required by iced_baseview) |
| `src_macos_view_rs.patch` | Guards `become_first_responder` against `nil` from `[view window]` on ARM64 macOS before the view is attached — fixes a crash during `setContentView_` |

### Updating baseview

1. Run `./scripts/setup.sh --check` to confirm current patches still apply cleanly.
2. If upgrading the pin:
   a. Update `BASEVIEW_REV` in `scripts/setup.sh`.
   b. Run `./scripts/setup.sh` to repopulate `vendor/baseview`.
   c. Check that `cargo check --lib` and `cargo check --bin ethertap-gui --features standalone` still pass.
   d. If patches no longer apply, resolve conflicts, update the `.patch` files:
      ```bash
      # from repo root, after manually fixing vendor/baseview:
      diff -u /tmp/baseview-upstream/<file> vendor/baseview/<file> \
        --label a/<file> --label b/<file> > patches/baseview/<name>.patch
      ```
   e. Verify regenerated patches with `./scripts/setup.sh --check`.
   f. Update the pinned rev in this file.

---

## nih-plug

**Upstream:** https://github.com/robbert-vdh/nih-plug  
**Pinned rev:** `28b149ec4d62757d0b448809148a0c3ca6e09a95`  
**Patches:** `patches/nih-plug/`

### Why we patch

nih-plug's `ProcessContext` trait has no `set_parameter` method (it is
commented-out as a TODO in `src/context/process.rs`). Without it,
`is_connected` and `is_matched` can only be updated by the GUI thread — so
when the DAW GUI is closed the params freeze and host automation readback
stops working.

### Patches

| File | What it does |
|------|-------------|
| `context_process_rs.patch` | Adds `ProcessContext::set_parameter<Pa: Param>` with a default impl that updates the param's internal atomic via `param.as_ptr().set_normalized_value()` |
| `vst3_context_rs.patch` | Overrides `set_parameter` in `WrapperProcessContext` to additionally call `inner.set_normalized_value_by_hash()`, which schedules a `ParameterValueChanged` event on the GUI event loop so hosts pick up the new value via `getParamNormalized` |
| `vst3_inner_rs.patch` | Extends `Task::ParameterValueChanged` handler to call `IComponentHandler::begin_edit` / `perform_edit` / `end_edit` on the DAW host before notifying the editor widget; without this the DAW never sees plugin-driven param changes |
| `context_process_transport_visibility.patch` | Widens `Transport::new` from `pub(crate)` to `pub` — our unit tests (`src/lib.rs`) construct `Transport` directly to drive `process()` in isolation, which needs a public constructor outside the crate-internal call sites upstream restricts it to |

### Updating nih-plug

1. Update `NIH_PLUG_REV` in `scripts/setup.sh`.
2. Run `./scripts/setup.sh` to repopulate `vendor/nih-plug`.
3. If patches no longer apply, resolve conflicts and regenerate:
   ```bash
   # from repo root, after manually fixing vendor/nih-plug:
   cp vendor/nih-plug/src/context/process.rs /tmp/orig.rs
   # apply your manual edit, then:
   diff -u --label a/src/context/process.rs --label b/src/context/process.rs \
     /tmp/orig.rs vendor/nih-plug/src/context/process.rs \
     > patches/nih-plug/context_process_rs.patch
   ```
4. Verify with `./scripts/setup.sh --check` and `cargo check --lib`.
