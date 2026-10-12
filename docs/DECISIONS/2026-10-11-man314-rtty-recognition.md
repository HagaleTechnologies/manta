# MAN-314: RTTY recognition keeps RTTY out of CW tracks

## Decision

manta recognises two-tone FSK (RTTY) with a deterministic classical
detector, `manta-dsp::fsk::FskDetector` (SPEC-decode-core.md §2.6). It
reads the per-hop channelizer output and noise floor the `TrackManager`
already computes. A tone pair is marked as RTTY after it has passed
`rtty_dwell_ms / 1000` one-second blocks (20 s by default). A block passes
when:

- the two tones' per-hop SNR is anti-correlated (ρ ≤ −0.5);
- the envelope is near constant;
- each tone leads in at least 10 % of hops;
- the measured shift falls in a standard window.

From then on the `TrackManager`:

- starts no CW candidate on the marked signal's hard span (its tones ± 1
  channel);
- vetoes splatter-level candidates in its shadow (± 10 channels, more than
  40 dB below the key-down level), **for spawns only**;
- closes open tracks whose centre channel sits on the hard span, as
  bookkeeping (`CloseReason::Rtty`, `CloseCounts::rtty`).

`[detector] rtty_filter` (default `true`) and `rtty_dwell_ms` (default
20000, at least 1000) configure it; `rtty_filter = false` builds no
detector and reproduces the previous behaviour exactly. The marked set is
exposed as:

- the `rtty` array in `decode --json`, and an `rtty:` line in plain `decode`
  when RTTY was marked (a CW-only decode keeps MAN-270's one-line summary);
- `RTTY marked:`/`RTTY cleared:` log lines in `run`;
- the gauge `manta_rtty_signals{band}`, a count of marked signals per
  amateur band (`manta_server::band::band_for_freq_hz` of the mark
  frequency).

The detector is also the front end for the separate RTTY-decoding ticket.
Each signal carries a stable id, mark and space frequencies, and the
nominal and measured shift.

## Evidence

The 2026-10-11 MAN-314 plan prototyped the design end to end at
`5346c333efd8eb6c9704b2218523f539e3c68b1a`. Its measurements:

- **Baseline.** A single 170 Hz, +30 dB RTTY signal produced 71 CW track
  promotions in 40 s, all decoding garbage. Every shift produced at least
  two whole-file garbage tracks.
- **Classifier.** Single blocks pass for all five shifts at 0–40 dB (2500
  Hz reference); the weakest case was 0 dB at 200 Hz, 20/24 blocks. Tone
  ρ is −0.62 to −0.81.
- **Golden CW vectors.** The most negative single-block ρ on V1–V10/VR is
  −0.66 (V8/V8w pileups). Maximum accumulated evidence is 1 of the 20
  needed, so no vector is ever marked.
- **Shift sweep** (phase-difference estimator, 20 mark offsets): at ≥ 10 dB,
  170 Hz measures 147.5–175.6, 200 Hz 186.9–215.2, 425 Hz 409–437, 450 Hz
  429.8–458.2 and 850 Hz 840.5–870.4. This fixes the windows at
  [130, 230], [395, 475] and [820, 885] Hz. 425 and 450 Hz overlap, so
  the nominal shift is informational and the measured shift is reported
  alongside.
- **End to end at 10 dB.** All five shifts mark at 21.0 s of stream, with
  no promotion inside the RTTY region after the mark. The same holds at
  30, 45 and 50 dB.
- **Scenario 3.** RTTY at 14080/14086 kHz plus CW at 14083 kHz: K5ARH is
  spotted, decoded by one track. Tracks started in the RTTY region over
  60 s: 105 with the detector off, 39 with it on (all before the mark).

This branch's tests reproduce the scenarios in
`crates/manta-engine/tests/rtty.rs` and `tests/rtty_detector.rs`.

## Closes use the hard span only

The prototype first applied the splatter shadow to existing tracks too.
That killed real CW near a strong RTTY signal during its key-up gaps. CW
600 Hz above a +45 dB RTTY, with CW at +20 dB, decoded 0 characters after
the mark with shadow-on-close; it decoded 63 with the detector off and 63
with hard-span-only closes. Closes therefore use the hard span only. The
regression test is `strong_rtty_does_not_kill_nearby_cw`.

## Guard 1, not 2

CW keyed only from t = 30 s (after the mark), RTTY at 30 dB and CW at
10 dB: a 2-channel guard lost the spot at +250 and +300 Hz. A 1-channel
guard gives the same spot outcome as the detector off at
+150/+200/+250/+300/−200/−300 Hz, and still allows no post-mark promotion
for all five shifts. The regression test is
`cw_starting_after_mark_near_rtty_still_decodes`.

## Known limits and follow-ups

- **High SNR.** At ≥ +45 dB, 1–2 tracks spawned *before* the mark on far
  splatter (400–1000 Hz out) can keep decoding garbage after the mark
  until they end. No spot came from them, and no new track starts after
  the mark.
- **The first 20 s.** RTTY still makes CW tracks during the dwell, by the
  ticket's design.
- **FT8.** Fifteen synthetic 8-FSK FT8-like signals at 14074.2–14077 kHz
  produced no RTTY mark and identical CW garbage with the detector on or
  off. The field cluster near 14075 kHz noted in the 2026-09-09/09-10
  decision records is very likely FT8, and MAN-314 does not remove it.
  **Follow-up recommended:** an FT8/8-GFSK detector on the same front
  end. This session had no Linear access and its GitHub token was scoped
  to the MAN-314 branch and PR, so the follow-up ticket is left for a
  maintainer to file.

## Cost

- `cargo bench -p manta-dsp --bench fsk -- --quick` (192 kHz, n = 2048,
  60 s of hops, ten RTTY pairs over noise): 0.347–0.362 s per 60 s, about
  0.006× realtime. The prototype measured 0.0063–0.0065× with 300 CW and
  10 RTTY signals, against about 1.0 s for the channelizer and 1.5 s for
  the floor and `power_db` at that rate.
- Memory: one block of per-hop SNR, `n × 375` `f32`, which is 3 MB at
  n = 2048.
- The Pi4/Mac CPU-budget check stays manual. The `#[ignore]`d Mac-budget
  test already failed in the planning container with the detector off.
