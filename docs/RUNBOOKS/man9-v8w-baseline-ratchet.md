# Pre-release check: V8w classical-baseline ratchet

`v8w_classical_baseline_does_not_regress`
(`crates/manta-cli/tests/golden_v8_v8w.rs`) is `#[ignore]`d, not run in CI.
Per `docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md`'s CI-cost rule:
one full V8w render+decode measures ~367 s in the environment that pin doc
was measured in, over the 180 s cutoff for a second full-50-signal-scene run
in CI (`golden_v8_v8w.rs` already runs one such scene, the V8 AWGN test).

Run this before cutting a release, and any time a PR touches
`manta-decode::envelope`, `::beam`, `::timing`, or `manta-engine::track`
(the files MAN-9's ladder rungs live in):

```
cargo test -p manta-cli --test golden_v8_v8w -- --ignored --nocapture \
    v8w_classical_baseline_does_not_regress
```

**What it checks:** the classical-only decoder's V8w strong-signal
(`snr_2500_db >= 6.0`) pass count and median CER must not get WORSE than the
pinned floor (`MEASURED_PASSES`/`MEASURED_MEDIAN_CER` constants in the test,
sourced from the pin doc). It is deliberately **not** a relaxed copy of
`v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts` — that gate's
90 %/CER < 0.10 thresholds stay untouched and it stays `#[ignore]`d
separately, pending real fading-robustness work (M4). This ratchet exists so
the floor M4's "beats classical-only CER by a measured, documented margin"
acceptance criterion (`ROADMAP.md`) has something executable to diff
against, and so a future change cannot silently regress the classical
baseline between now and M4 without CI ever noticing (CI itself never runs
either `#[ignore]`d test).

**If it fails:** something regressed classical V8w decode accuracy. Do not
"fix" it by lowering `MEASURED_PASSES`/`MEASURED_MEDIAN_CER` without also
re-running the Phase 1 diagnostic
(`v8w_per_signal_cer_report`/`v8w_fragmentation_is_sequential_or_concurrent`)
and updating the pin doc's measurement record — the constants exist to be
raised when the decoder improves, not silently loosened to match a
regression.

## Runs

(append entries here)

- 2026-09-06 — commit `826bdd8` (MAN-9 remediation round): `1/34` passes,
  median CER `0.2755`, matching the pin doc's baseline. See
  `docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md`.
