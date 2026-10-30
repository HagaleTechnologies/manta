# MAN-106: the confidence multiplier `q` reads the engine-supplied floor-based SNR, not the keying rail

Date: 2026-10-30. Ticket: MAN-106. Reverses, for the confidence path
only, the pin recorded in
[MAN-102](2026-09-07-man102-snr-reference-and-estimator.md) decision
D102.5.

## Context

`q = clamp(SNR_2500/20, 0.3, 1.0)` scales every decoded character's
confidence (SPEC-decode-core §4.5). Until this change the Legacy engine
read `SNR_2500` from the M0 keying-rail estimate
(`Demod::snr_2500_db`), per MAN-102 D102.5, while the engine already
pushed the MAN-102 floor-based peak-hold estimate into every
`TrackDecoder` on every queued hop (`set_snr_2500_db`,
`manta-engine/src/track.rs`) and `tick_meta` already preferred it for
the *reported* `TrackMeta.snr_2500_db`.

The research phase for this ticket (commit 65ad1bb, before this change)
measured the consequence end to end:

| scene | characters at the 0.3 `q` floor | spot confidences |
|---|---|---|
| AWGN 3 dB | 100% | median pinned at 0.225 = 0.3 × 0.75 |
| AWGN 6 dB | 100% | pinned |
| AWGN 9 dB | 49% | 83 of 100 spots carry exactly 0.225 |
| V8w (fading) | 69% of 9,344 | near-constant |

The rail estimate understates SNR by ~8 dB, so it sits below `q`'s own
6 dB clamp knee across the operating range: `q` is a near-constant and
`Spot.confidence` carries no dynamic range — one of the two root causes
of the ticket's overlapping true/bogus confidence ranges.

## Decision

The Legacy `q` reads `self.snr_2500_db.or_else(|| self.demod.
snr_2500_db())` — the same precedence `tick_meta` already uses for the
reported value (`decoder.rs`). The keying-rail estimate remains the
fallback for a decoder driven standalone (manta-decode's own tests,
manta-testkit roundtrip vectors, which never call `set_snr_2500_db`).
`Hsmm`/`EdgeLegacy` already use their evidence-derived estimates;
unchanged.

MAN-102 D102.5 deliberately decoupled the *reported* SNR (floor-based)
from the *rail* estimate as an instrumentation boundary; it never
measured the confidence path. This record reverses that pin for `q`
only: the reported-value precedence is untouched, the rail estimator is
kept (fallback + `rail_ratio` instrumentation), and MAN-102's SNR
reporting semantics do not move.

## Verification

- Unit: `legacy_q_follows_the_engine_supplied_snr` (a clean character at
  a 12 dB estimate carries softmax × 0.6 exactly) and
  `legacy_q_falls_back_to_the_rail_estimate_when_standalone`
  (`crates/manta-decode/src/decoder.rs` tests).
- Integration: `manta-cli`'s `confidence_desaturation` test — the
  median correct-spot confidence on the deterministic AWGN 25 WPM
  10-station scene is now strictly increasing across 3 → 9 → 15 dB, and
  at 9 dB fewer than 10% of spots carry exactly 0.225 (before: median
  pinned equal, 83 of 100 at 0.225).
- Golden decode gates unchanged: `q` multiplies every beam hypothesis's
  confidence by the same factor, so glyph selection, decode text and
  spot sets do not move (`golden_v8_v8w` green at this commit).

## Alternatives rejected

- Keep the rail pin and calibrate around it: the saturation destroys
  information before any calibration can recover it — 83 of 100 spots
  carry the *identical* raw score with wildly different accuracy.
- Replace the rail stand-in with a true physical SNR measurement
  (gain-corrected key-down power, ENBW-corrected noise density):
  investigation 11's M2/§2.3 front-end work, explicitly out of scope
  for this ticket; the floor-based estimate is the measured non-saturating
  input the decoder already receives.

## First-slice boundary

The previous attempt changed 55 files. This slice keeps phases 1 and 2
of the MAN-106 plan, including zero-evidence abstention and the interim
repetition-based emission state. Phases 3 and 4 move together to one
follow-up: the bench measurement instrument, decision confidence capture,
fit data and calibration, probability-based state threshold, and the
V8w separation gate. The confidence score is not yet a calibrated
probability. The separation acceptance criterion remains outstanding.

`Reason::NoEvidence` maps to `noEvidence` in the live-state stream. The
gate covers ordinary candidates, allowlisted candidates and deferred
beacons at track close. `emissionState` is `confirmed` when the configured
pattern tier's repetition requirement is met, otherwise `tentative`.
Both states publish; the :7301 spot message does not gain this field.
The support ledger remains unchanged.

The decision index is generated after landing under MAN-440; this record
therefore does not rewrite its generated row.

## First-slice measurements

`cargo test -p manta-cli --test confidence_desaturation --locked --
--nocapture` passed on the 30 s, seed 1, 25 WPM, ten-station AWGN
recipe. Correct-spot medians at 3/9/15 dB were respectively
0.22495812 / 0.22498953 / 0.34301296. At 9 dB, 0 of 5 correct spots
were within 1e-6 of 0.225. The low-SNR medians remain near the floor;
this input correction alone does not establish empirical calibration
or the true/bogus separation bar.

`cargo test -p manta-spot --test golden_v52_zero_evidence --locked`
passed all six tests, including the all-zero CQ stream with and without
SCP, allowlisted and deferred-beacon paths, positive and mixed-evidence
controls, repetition-based states and the serialized field labels.
