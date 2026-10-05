# MAN-166 decode-core diagnostics (2026-09-09)

Throwaway, clean-room diagnostic scripts behind
`docs/superpowers/specs/2026-09-09-decode-core-real-hf-design.md` §1.
They are kept for reproducibility only; the real harness is Task 6 of
`docs/superpowers/plans/2026-09-09-decode-core-v2.md`. Python 3 with
`numpy` + `scipy` (create a venv; nothing here is part of the build).

- `probe2.py <B2.wav> <rbn.csv> <outdir> [max]` — numpy replica of the SPEC
  §1 WOLA channelizer; extracts a 40 s window around every K5TR-spotted
  station, writes the own-channel power stream (`*.own.f32`, 375 Hz) plus
  neighbors, prints envelope statistics, writes `summary.json`.
- `thr_experiment.py <outdir>` — run-length histograms in true-dit units for
  thresholds at the geometric mean of the rails vs −10/−6/−3 dB re the mark
  rail (design §1.3).
- `hsmm_proto.py <outdir> [max] [nproc]` — the untuned HSMM feasibility
  prototype (design §1.5–1.6). Not a reference implementation.
- `crates/manta-decode/examples/envelope_oracle.rs` — feeds `*.own.f32`
  streams straight into `TrackDecoder` (design §1.2).
