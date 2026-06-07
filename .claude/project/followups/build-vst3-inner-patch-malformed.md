---
id: build-vst3-inner-patch-malformed
title: patches/nih-plug/vst3_inner_rs.patch has wrong hunk header — fails on fresh setup.sh checkouts
created: "2026-06-07"
origin: |
    discovered during /worktree-start vst-host-runtime baseline setup, 2026-06-07
kind: finding
severity: risk
review_by: "2026-08-06"
status: open
file: patches/nih-plug/vst3_inner_rs.patch:3
---

`@@ -620,11 +620,24 @@` undercounts: the hunk actually spans 13 old-file
lines / 26 new-file lines (including context), not 11/24. BSD `patch`
(macOS) rejects it as "malformed patch at line 35" on a fresh
`scripts/setup.sh` checkout — the patch silently fails to apply, leaving
`vendor/nih-plug/src/wrapper/vst3/inner.rs` without the
`begin_edit`/`perform_edit`/`end_edit` host-notification code PATCHES.md
says it adds.

Does not block `cargo build` or `cargo test` (the missing code only
affects VST3-host param-automation readback at runtime), which is why
it went unnoticed — but any fresh clone + `setup.sh` run produces a
vendor tree silently missing this behavior, and `set -euo pipefail`
makes `setup.sh` exit 2 without telling the user why.

Fix: regenerate the patch from a pristine clone at the pinned rev with
the manual edit reapplied, per `PATCHES.md`'s nih-plug update recipe
(`diff -u --label a/... --label b/... /tmp/orig.rs vendor/.../inner.rs`).
