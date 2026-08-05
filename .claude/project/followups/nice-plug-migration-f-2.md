---
id: nice-plug-migration-f-2
title: 'Manual DAW pass: VST3 embedding + live resize on nice-plug stack'
created: "2026-08-05"
origin: |
    docs/spec/nice-plug-migration.md, iter 5 reviewer (CP-4)
kind: finding
severity: question
review_by: "2026-10-04"
status: open
file: docs/spec/nice-plug-migration.md:40
---

CP4 verified only: standalone launch, ARM64 no-crash, CoreAudio stream. Still unverified empirically: VST3 window embedding in a real DAW and live resize (standalone + embedded). Manual pass: load target/bundled/ethertap.vst3, open/close/resize editor; resize standalone window. Do before tagging a release on the nice-plug stack.
