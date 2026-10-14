# MAN-196: the decimator's trusted band — advertised and spawn-vetoed

## Decision

A decimated capture (`--capture-rate-hz` / `[input] capture_rate_hz`,
issue #169) now stops treating the decimated Nyquist as usable
bandwidth. One boundary — the decimator's **trusted band**, the <= 1 dB
crossover of the halfband design — serves both halves of the ticket:

- **Advertise** (the ticket's option b): `Decimator` exposes
  `TRUSTED_HALF_BAND_FRACTION = 0.4526` of `fs_out` and
  `trusted_half_bandwidth_hz()`; `DecimatingSource::rf_passband_hz`
  clips to it, so `manta check` (JSON `passband_offsets_hz` narrows from
  `[-24000, 24000]` to `[-21724.8, 21724.8]` at 48 kS/s), SETT/Aggregator
  segments, live-state `bandState` rows and the MAN-312 static-crash
  live set all advertise/judge the honest band in one edit. `run` logs
  one startup info line stating the usable passband in the
  crash-filter-notice position; `soak` prints the same line on stderr
  before it starts. The field names, schemas and metrics
  families do not change.
- **Veto** (option a): `IqSource::trusted_band_hz()` (default `None`)
  reports the band; `listen` reads it once per source and threads it
  into every segment's `TrackManager` (the discontinuity rebuild
  included) via `set_trusted_band_hz`, which builds a per-channel mask —
  true iff the channel centre lies in `[lo, hi]`, no margin channel. The
  §2.5 spawn conjunction gains `!trusted_vetoes_spawn(k)` in both the
  `k` and `k+1` slots. **Spawns only**: never a close path, never a
  decode hold — the MAN-314 measured lesson that applying a veto to
  running tracks kills real CW in key-up gaps. A track that already
  exists on a rim channel keeps decoding.

No new config key, metric family or wire field: the boundary is filter
physics (`CUTOFF_FRACTION`, `HALFBAND_TAPS`), pinned as a constant and
DTFT-self-verified by bisection
(`trusted_half_band_matches_measured_1db_crossover`), the same
documentation workflow `CUTOFF_FRACTION` itself uses. Non-decimated
sources are behaviour-identical to before, bit for bit (the trait
default is `None` = no veto; `unset_trusted_band_is_bit_identical_to_today`
pins it).

## Measured base

Every number was measured by the research phase (MAN-196 research,
2026-10-14) at `957a49e` against the real `design_halfband(127)` taps
(exact DTFT) and the real decimate→channelize→`manta decode` pipeline;
each cell deterministic and run twice:

- Response at the 48 kS/s channel grid: −0.05 dB @ 21 000 Hz, −0.60 dB
  @ 21 562.5, −2.16 dB @ 22 031.25, −5.44 dB @ 22 500, −12.70 dB @
  23 062.5, −22.76 dB @ 23 531.25, −39.05 dB @ Nyquist. Crossovers:
  −1 dB @ 21 728.5 Hz, −3 dB @ 22 183.3, −6 dB @ 22 557.9, −10 dB @
  22 889.5, −20 dB @ 23 422.4. The full 192k→48k cascade matches the
  final stage to within 0.05 dB, so one `fs_out`-normalized constant
  serves every factor.
- The ticket's "loses enough SNR to go undetected" does **not**
  reproduce in the deterministic pipeline: an LTI filter attenuates
  co-channel noise with the signal, so CER is 0.0103 at every offset
  out to 23.5 kHz at both SNR 20 and 10. The risk is real only against
  floors that are not co-attenuated (ADC quantization, AGC, front-end
  noise) — unmeasurable from a file render, so the concession is
  advertised rather than fixed.
- What does reproduce and what the veto kills: frequency error 30.0 Hz
  (SNR 20) / 35.8 Hz (SNR 10) at 23.0 kHz vs 9.2/18.1 Hz native
  controls (breaching the 25 Hz golden bound class); a duplicate second
  track at 23.5 kHz/SNR 10 (decimated only); a −20 dBFS carrier at
  +25.0 kHz folding to a −100.6 dBFS residue that still promotes a
  track in digital silence.

## Why the ≤ 1 dB crossover

Every measured artifact lies beyond it, and the golden rim-inside cell
(+21 kHz, −0.05 dB) sits comfortably inside. A tighter boundary (−3/−6
dB) would concede usable channels; a looser one admits the measured
25 Hz-class frequency corruption.

## Residual, stated exactly as the tests measure it

- Rim channels 232–255 per side of a 512-channel 48 kS/s capture (24 of
  256, the outer ~9.4% of each side) are conceded on decimated captures:
  not advertised, not tracked (`boundary_granularity_is_the_channel_centre`).
  The last trusted channel centre is 231 = 21 656.25 Hz.
- Rim signals **without** a decimator in the path are unaffected
  (native-rate edges are unchanged).
- A rim signal **with** a decimator spawns nothing
  (`beyond_the_trusted_band_a_decimated_signal_spawns_no_tracks`,
  control ≥ 1 promotion); foldover from carriers above the new Nyquist
  promotes nothing (`a_carrier_above_the_decimated_nyquist_promotes_nothing`);
  a signal just inside the band (+21 kHz, −0.05 dB) decodes at the
  golden standard (CER < 0.02, freq error ≤ 25 Hz, one track — measured
  1.5 Hz / 0.0103) (`rim_inside_the_trusted_band_still_decodes_at_the_golden_standard`).
- **In-band foldover aliases** (round-1 review on PR #300): a carrier at
  or above `fs_out − trusted` (26 275.2 Hz at 48 kS/s) folds *inside*
  the trusted band, where the spawn veto cannot see it — only the
  halfband's stopband does. That region is held at ≥ 80 dB (measured
  worst −86.5 dB at the region's lower edge;
  `halfband_stopband_covers_frequencies_folding_inside_the_trusted_band`),
  so the folded residue of a carrier at/under −40 dBFS stays below the
  detector's digital-silence floor (e2e: −40 dBFS at +26 343.75 Hz,
  folding to trusted channel 231's centre, promotes nothing), while a
  carrier at −20 dBFS (residue ≈ −107 dBFS) still promotes one mirrored
  track — measured boundary between −35 and −40 dBFS, pinned by
  `an_in_band_foldover_residue_promotes_only_above_the_stopband_margin`.
- Wrappers forward the band (`FixedCenterFreqSource`, `PacedSource`,
  `LoopingSource`, `ReconnectingSource` capture it beside
  `rf_passband_hz`); a factor-1 pass-through wrap reports `None` (no
  filter, no transition band). The CLI never builds the latter
  (`maybe_decimate` skips an equal-rate target). `doctor`'s
  `SampleBoundedSource` forwards it too (round-1 review on PR #300),
  so `doctor --capture-rate-hz` runs the same detector path as
  `run`/`soak`.

## Not done here, and why

- **No closing/de-weighting/decode-holding for running tracks** — the
  MAN-314 key-up-gap regression; a spawns-only veto cannot strand
  anything mid-decode.
- **No exclusion on non-decimated sources.** The channelizer's own
  native-rate edge artifact (promotion spikes ~3 kHz inside each edge,
  35%/28% of promotions in the 2026-09-09 dial-shift runs — see
  `2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`) is a different
  mechanism: the PFB prototype has cutoff at Δ/2 per channel, no
  transition band at the band edge, and its root cause is still open
  (that doc's follow-up #1). The exclusion mechanism built here is
  frequency-bounded and source-produced, so a future boundary can
  populate it; the policy is not extended. The root-cause follow-up
  ticket is outside this container's reach (no Linear credential); this
  record names it.
- **No stopband work.** The 39–50 dB near-Nyquist alias suppression
  margin belongs to the `CUTOFF_FRACTION`/`HALFBAND_TAPS` history; the
  veto removes its observable consequence (foldover promotions) without
  redesigning the filter. **Correction from the round-1 review on PR
  #300:** that rationale holds only for aliases folding *outside* the
  trusted band; aliases folding *inside* it (input frequencies ≥
  `fs_out − trusted`) are suppressed by the stopband alone, which holds
  ≥ 80 dB there but leaves a ≥ ~−35 dBFS carrier able to promote a
  mirrored track in digital silence (see the residual above). Deepening
  the stopband (~120 dB would close the −20 dBFS window) means a higher
  Kaiser β and roughly double the taps against the missing Pi4
  decimator CPU bench, and shifts the cutoff toward the 0.232–0.2325
  golden cliff — a filter-design tradeoff this ticket's plan explicitly
  excluded, so it is raised as a decision ticket rather than taken here.
- **No `decode`-path change**: `decode` never decimates.

## What would relax this

If real-hardware measurement shows rim SNR intact (quantization floor
far below band noise at operator gain settings), the veto half can be
relaxed to advertisement-only with one constant change; the boundary
stays computed either way. Operators needing the rim back now have the
documented recourse: choose a higher `--capture-rate-hz` (the boundary
scales with `fs_out`; ±21 725 Hz at 48 kS/s becomes ±43 450 Hz at
96 kS/s).

Normative text: [SPEC-decode-core §1.5](../SPEC-decode-core.md) (the
trusted-band specification) and §2.5 (the spawn conjunction),
[docs/CONFIG.md](../CONFIG.md) (`[input] capture_rate_hz`),
[docs/LIVE-STATE.md](../LIVE-STATE.md) (`bandState`), the
`config_init.toml` scaffold comment.
