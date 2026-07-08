---
id: determinism
title: What will bite you about the byte-identical decode requirement?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#6-determinism-requirements
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - decode-chain
---
Running the daemon from an IQ file must produce **byte-identical** spot logs across platforms — this is a hard invariant, not a nice-to-have, and it constrains code far from where you would expect. The most common way to break it is to let wall-clock time, unordered iteration, or platform-varying float reduction into the decode path. The exact determinism rules are normative in SPEC §6 — cite them, do not restate.

## Symptom

The same fixture WAV yields different spot logs on Linux vs macOS, or run-to-run, and CI's byte-identical check (ROADMAP M0/M2) fails. Timestamps in spots that follow wall-clock rather than sample count are the classic tell.

## Cause and workaround

No wall-clock anywhere in the decode path — time is derived from sample count. Watch for: HashMap iteration order (use ordered structures where output depends on it), non-deterministic thread scheduling affecting result ordering, and float summation order in the FFT/FIR reductions. The decode path is deliberately on dedicated threads with lock-free handoff (ARCHITECTURE §10) partly to keep ordering pinned. When in doubt, the rule and the test vectors that enforce it are in SPEC §6–7.
