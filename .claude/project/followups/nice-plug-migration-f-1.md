---
id: nice-plug-migration-f-1
title: Reconnect rescan test flaky on multi-homed machines (self-IP via WiFi)
created: "2026-08-05"
origin: |
    docs/spec/nice-plug-migration.md, iter 1 orchestrator baseline (CP-5 criterion)
kind: finding
severity: risk
review_by: "2026-10-04"
status: open
file: tests/reconnect_tests.rs:172
---

Flaky on multi-homed dev machines: the rescan probe reaches the MockMixer (bound 0.0.0.0) via the machine's own WiFi interface IP (e.g. 10.0.0.66) and the test asserts the connection target is 127.0.0.1. Not a foreign device — self-IP via second interface (WiFi .66 vs wired .65). Fix: bind the mock/scan to loopback in tests, or assert by mock identity instead of literal IP. Pre-existing before the nice-plug migration.
