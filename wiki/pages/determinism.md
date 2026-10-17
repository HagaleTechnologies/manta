---
id: determinism
title: What will bite you about the byte-identical decode requirement?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#6-determinism-requirements
  - ARCHITECTURE.md
  - docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md
verified:
  commit: 6596e3a
  date: 2026-10-15
links:
  - decode-chain
---
Running the daemon from an IQ file must produce **byte-identical** spot logs for the same build and input (across platforms, equality is asserted on the decoded text — SPEC §6 rule 4) — this is a hard invariant, not a nice-to-have, and it constrains code far from where you would expect. The most common way to break it is to let wall-clock time, unordered iteration, or platform-varying float reduction into the decode path. The exact determinism rules are normative in SPEC §6 — cite them, do not restate.

## Symptom

The same fixture WAV yields different spot logs run-to-run, and SPEC §6's three-run identical-SHA-256 check (`json_output_is_valid_and_deterministic_across_three_runs` in `crates/manta-cli/tests/cli.rs`) fails; or the decoded text differs between Linux and macOS while staying stable on each, which that check cannot see (it compares three runs of one binary on one platform) — there the golden-vector runs on every CI platform (SPEC §7, `crates/manta-cli/tests/golden_*.rs`) are the gate that fails. Timestamps in spots that follow wall-clock rather than sample count are the classic tell.

## Cause and workaround

No wall-clock anywhere in the decode path — time is derived from sample count. Watch for: HashMap iteration order (use ordered structures where output depends on it), non-deterministic thread scheduling affecting result ordering, and float summation order in the FFT/FIR reductions. Decoder-pool workers may run in any order: each track's decoder is a pure function of its own input, and events are sequenced by `(sample_ts, track_id)` before the validator sees them (SPEC §6 rule 6). When in doubt, the rule and the test vectors that enforce it are in SPEC §6–7.

Multi-input sessions (MAN-13, several `[[input]]` entries) add one more rule (SPEC §6 rule 7): replay merges whole per-input batches in `(batch end time on the common clock, input index)` order, blocking only on inputs with nothing queued, so the merged stream is a pure function of the files even though the inputs run on separate threads; every replayed input's clock offset is 0. Live inputs merge in arrival order instead — holding one live input back for another would lose its samples, and live output is not deterministic anyway. Per-input events are still sorted per batch before the shared validator sees them. See [the decision record](../../docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md) for the common-clock mapping (highest input rate, exact power-of-two ratios).
