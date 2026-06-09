---
id: vst-runtime-transport-pos
title: Transport position auto-advance blocked by pub(crate) fields in nih-plug
created: "2026-06-09"
origin: |
    docs/spec/vst-host-runtime.md, iter 2 reviewer (CP-2)
kind: plan
severity: risk
review_by: "2026-08-08"
status: open
file: vst-runtime/src/lib.rs:122-125
---

Transport position fields (pos_samples, pos_beats, pos_seconds) are pub(crate) inside nih-plug and cannot be mutated from vst-runtime. ScenarioBuilder::step() cannot auto-advance position across consecutive steps; callers must supply a fresh Transport via .transport() before each step if position matters. Tests that need to verify position-sensitive behavior (quantised-sync, BPM-settle timing) require a nih-plug patch to make these fields pub. Add a patch to patches/nih-plug/ exposing pos_samples and pos_beats as pub, then update ScenarioBuilder::step() to auto-advance by buffer_size samples after each call.
