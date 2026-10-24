# MAN-195: `spectral_beta = 0.0` skips the spectral-reference scan

Date: 2026-10-23. Ticket: MAN-195, a follow-up filed from Codex review of
PR #178 (MAN-168) under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md` (round 6+ tier:
a new P2 finding becomes a ticket, not another round on the same PR).

## Context

PR #178 round 4 gated `FloorBank::spectral_reference_db` (a six-neighbor
scan plus a `log10`/`powf` dB-to-linear round trip) in
`TrackManager::step_hop` on `engine != Legacy`, because `Legacy` never
reads `spectral_ref_power` — the in-tree comment prices the waste at
~225k–900k transcendental evaluations/sec at 300–1200 tracks. But the
predicate ignored `spectral_beta`, and `spectral_beta = 0.0` is the
documented, validated, supported way to disable the spectral discount
(`docs/CONFIG.md` "0 or more"; config validation keeps "0.0 … legal", and
`load_decode_config_file_accepts_zero_spectral_beta` proves it loads). With
`β = 0`, `NoiseTracker::push`'s term
(`crates/manta-decode/src/noise.rs:96`) is
`n_temp.max(0.0 * b_spec * r) == n_temp` for the always-non-negative
`n_temp` — bit-for-bit the `None` branch — so for `EdgeLegacy`/`Hsmm` the
whole computation was discarded via a multiply-by-zero on every hop: the
same waste class the engine gate closed, back on CPU-marginal
high-track-count 192 kS/s runs.

## Decision

`step_hop`'s predicate
(`crates/manta-engine/src/track.rs`, `compute_spectral_ref_power`) gains
the second dimension of the same gate:

```rust
let compute_spectral_ref_power = !matches!(self.decode_cfg.engine, Engine::Legacy)
    && self.decode_cfg.noise.spectral_beta > 0.0;
```

Read once per hop-batch (two float loads and a compare), consulted at the
two existing gated push sites. With `spectral_beta = 0.0` the scan is
skipped entirely and the queued `spectral_ref_power` stays `None`, which
`NoiseTracker::push` already handles identically. At any other
`spectral_beta` nothing changes: the computed value was already consumed
unchanged.

## Evidence

- Red: `step_hop_skips_spectral_ref_power_when_spectral_beta_is_zero`
  (`crates/manta-engine/src/track.rs`, modeled line-for-line on the
  sibling `Legacy` test) failed on unmodified `83262d0` with its own
  assertion ("spectral_beta = 0.0 must skip computing spectral_ref_power
  (MAN-195)"); 3 passed, 1 failed.
- Green with the fix: the same filter runs 4/4; the three pre-existing
  `spectral_ref_power` tests (default `spectral_beta = 0.5` populate path,
  centroid-centering, `Legacy` skip) pass unchanged;
  `cargo test -p manta-engine --lib` is 361 passed, 0 failed, 1
  pre-existing ignored; `cargo fmt --all --check` and
  `cargo clippy -p manta-engine --all-targets --locked -- -D warnings`
  are clean.

## Not done

- No `manta-decode` change: `NoiseTracker::push` already reduces to the
  `None` branch at `β = 0` and handles `None` correctly.
- No config-validation or `docs/CONFIG.md` change: `0.0` is already legal,
  documented and tested.
- No golden-vector regeneration: the pre-existing V5 (2 failed) and
  V10-hsmm (1 failed) suites fail identically with and without this fix
  (research verified by stashing the fix and rerunning; byte-identical
  failures — tracked elsewhere, not this ticket).
- No `cpu_budget` benchmark change: it runs `PipelineConfig::default()`
  (`Legacy`), so it exercises neither the bug nor the fix.
- No engine-default change; `hsmm` stays unpromoted.
