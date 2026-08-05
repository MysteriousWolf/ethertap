---
id: standalone-clock-buffer-jitter
title: 'Standalone MIDI clock: inherent buffer-boundary jitter (~10ms periodic)'
created: "2026-08-05"
origin: |
    nice-plug migration aftercare, S1 instrumentation 2026-08-05
kind: finding
severity: question
review_by: "2026-10-04"
status: open
file: src/lib.rs:1034
---

Standalone MIDI clock ticks are emitted at audio-buffer boundaries: at 48kHz/120BPM/24PPQ the tick interval (~1000 samples) vs 512-sample buffers quantizes emission to a 2-1-2 buffer cadence, producing a persistent periodic ~10ms jitter spike (~50% of interval) measured at the sink. Average BPM stays accurate. Eliminating it needs a separately-paced emitter (design change; file header src/midi_clock.rs:9-16 documents why sleep-paced workers were rejected). Decide: accept as characteristic, or design a pacing change.
