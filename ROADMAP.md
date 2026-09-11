# manta — Roadmap

Milestones are strictly ordered; each has hard acceptance criteria. "Accuracy"
always means character error rate (CER) on `manta-testkit` scenes unless
stated otherwise.

## M0 — One clean signal from a file

Workspace scaffolding, `manta-testkit` synthetic CW generator, `manta-input`
file playback, single hardwired channel (no PFB), classical decoder chain
(envelope → keying → speed tracking → beam-search Morse decode).

**Accept when:**
- `manta decode fixture.wav` (WAV input, per SPEC-decode-core §7's M0
  definition) prints the correct text for a synthetic
  20 WPM / +20 dB SNR / AWGN-only single-signal IQ file (SPEC §7 V1).
- Proptest round-trip (text → testkit CW → decoder) passes for 10–40 WPM at
  ≥ +15 dB SNR, CER = 0.
- CI green on Linux, macOS, and Windows (no SoapySDR dependency in default
  features), since MAN-212. `--features soapy` remains Linux/macOS-only in
  CI — a real Windows SoapySDR build path exists (vcpkg + SoapyRTLSDR from
  source) but wasn't made reliable enough for CI in that ticket; validated
  manually instead on real hardware (see the MAN-212 follow-up ticket).

## M1 — Live audio, one signal, real hardware

Audio-passband input via `coppa-audio` (rig RX audio), analytic conversion,
decoding a live off-air CW signal. This is the first on-air moment and needs no
SDR.

**Accept when:**
- Copies a live W1AW code-practice transmission (or equivalent scheduled CW) end
  to end with recognizable text.
- Runs ≥ 1 hour without panic, unbounded memory, or input overrun.
- Decoder handles QSB: testkit scene with Watterson CCIR-good fading at +10 dB,
  CER < 5 % (= spec vector V4; M1 gate is V1–V6 per SPEC-decode-core §7).

V1–V4 and V6 pass; V5 (CCIR-poor at +3 dB) is a tracked known limitation, not
yet met — see `docs/DECISIONS/2026-07-17-m1-implementation-pins.md`. The
live W1AW copy run (first bullet above) is still outstanding — see
`docs/RUNBOOKS/m1-w1aw-live-copy.md`.

## M2 — Wideband: PFB + detector + decoder pool

The core of the project. PFB channelizer (96/192 kS/s), order-statistic noise
floor, track manager, decoder pool, SoapySDR input (Airspy HF+ / RTL-SDR),
KiwiSDR input.

**Accept when:**
- Pileup vector V8 (AWGN: ≥ 45/50 callsigns validated, 0 bogus) passes per
  SPEC-decode-core §7. V8w (same scene under Watterson CCIR-poor, ≥ 90 % of
  signals ≥ +6 dB SNR at CER < 10 %) is **not** a blocking M2 gate — measured
  at 1/34 (2.9 %), tracked as issue #28, and reclassified as a known
  classical-decoder fading-robustness limitation in the same family as
  V2/V5/V6/issue #25. Per this repo's own design ("classical decoder first;
  ML fusion only at M4, gated on beating the classical baseline under
  simulated fading"), closing that gap is M4's job, not M2's.
- **Real-conditions vectors VR1–VR8** (`docs/SPEC-decode-core-v2.md` §8.2:
  contest speed 40/45 WPM, ≥ 45 dB in-channel keying depth, hard transmitter
  edges/clicks, co-channel occupancy, non-3:1 weighting, tight Farnsworth-free
  spacing) pass per that spec's §8.4 gate. Added 2026-09-09 (MAN-166,
  decision recorded in
  `docs/superpowers/specs/2026-09-09-decode-core-real-hf-design.md` §8):
  V1–V10 alone did not catch the real-signal keying-threshold defect that
  the B2/K5TR benchmark exposed (a threshold decision 17–30 dB below the
  mark on real 25–60 dB-deep signals), because none of V1–V10 exercises
  contest speed above 35 WPM with deep keying and hard edges together. VR
  vectors are a standing acceptance bar for every future decode-core
  change, not a one-time gate for the v2 redesign. **Measured against
  `engine = "hsmm"` 2026-09-09** (SPEC v2 §8.4 Task 12,
  `docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md`): 3/9 pass
  (VR6a, VR6b, VR7), 6/9 fail (VR1–VR5, VR8) — this gate is **not yet
  cleared**.
- Criterion bench: full pipeline at 192 kS/s with 300 active tracks uses < 50 %
  of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4. **Neither
  leg is currently a resolved pass** — see
  `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md` and the run
  procedure in `docs/RUNBOOKS/m2-pi4-cpu-budget.md`.
  - **Mac leg: unresolved, not passing.** A 0.360x realtime figure was
    recorded as a pass on 2026-07-24 (per M3's engine-wiring sub-project,
    including `manta-spot::Validator` cost — see
    `crates/manta-engine/benches/cpu_budget.rs`), and this session
    initially reconfirmed ~0.457x-0.472x. Both numbers turned out to omit
    the detector's 2s warmup from the timing, diluting the ratio by
    ~13%; corrected, this session measured ≈0.53x-0.58x against the <
    0.5x budget — i.e. likely failing, not the comfortable pass on
    record. Both the original 0.360x and this session's corrected numbers
    are superseded, historical measurements now, not a settled result —
    a clean rerun of the now-fixed `crates/manta-engine/tests/cpu_budget.rs`
    on a quiet, dedicated machine is needed to resolve this leg.
  - **Pi4 leg: outstanding** — needs real Raspberry Pi 4 hardware, tracked
    as MAN-18. A cross-architecture estimate (not a measurement — no Pi4
    was reachable to measure against) puts it at ~5.3x realtime CPU-time,
    well over the < 1.0x budget.
- 24 h soak on live 40 m CW segment via SDR: no crash, no overrun, track
  count and evictions visible in metrics. **Outstanding** — needs a real SDR
  and 24 unattended hours.

M2 sub-project 1 (PFB channelizer, `manta-dsp::channelizer`) is complete —
see `docs/superpowers/plans/2026-07-18-m2-pfb-channelizer.md` and
`docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`. M2 sub-project 2
(detector/track manager + decoder pool, `manta-dsp::floor` +
`manta-engine::track`) is complete — see
`docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md` and
`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`. All remaining M2
sub-projects (V8/V8w pileup-scene validation + CPU-budget criterion bench,
SoapySDR input, KiwiSDR input) are implemented, each with an open/merging
PR — see docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md,
docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md, and
docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md. **M2 itself is still not
complete**: the Pi4 CPU-budget leg and the 24 h live-SDR soak are real,
unmet acceptance gates, not sub-project work.

## M3 — Spots: validation + servers + RBN parity benchmark

`manta-spot` (cty.dat, SCP, CQ/DE parse, repetition gate, dedupe),
`manta-server` (telnet cluster protocol + JSON Lines/WebSocket), TOML config,
metrics endpoint, spot JSON Schema contributed to `dispensa`.

**Accept when:**
- A stock DX cluster client (e.g. `telnet`, N1MM) connects, logs in, and
  receives well-formed RBN-format spots.
- **Parity benchmark**: on ≥ 2 h of recorded contest-weekend IQ, manta achieves
  ≥ 80 % recall of RBN's spots for the same slice with ≤ 5 % false (bogus-call)
  spots. **Primary metric (added 2026-09-09, MAN-166): score against a single
  co-located spotter's own spots** (`scripts/score-against-rbn.py --spotter
  <call>`, e.g. K5TR for the B2 recording — same antenna/receiver as the
  capture) rather than all worldwide RBN spotters; recall against the full
  worldwide set is secondary/informational, since most of those bins are
  heard only by distant skimmers manta's own antenna cannot reach (56 % of
  B2's 1,441 worldwide bins were heard by exactly one skimmer). Numbers
  published in the repo, whatever they are.
- cqdx ingests the JSON stream in a dev environment.
- 7-day unattended soak feeding spots continuously.

`manta-spot` (callsign/CQ-DE validation, cty.dat/SCP cross-check,
repetition gate, dedupe) is complete as a standalone crate -- see
`docs/superpowers/specs/2026-07-25-m3-manta-spot-design.md` and SPEC
-decode-core.md §7.1 (V11-V15). It is now wired into `manta-engine`'s
batch (`decode_samples`/`decode_wav`) and streaming (`listen`) pipelines,
both emitting real `Spot`s -- see
`docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md`. Remaining
M3 sub-projects: `manta-server` (telnet + JSON/WebSocket output, TOML
config, metrics), and the RBN parity benchmark (needs ≥ 2 h of recorded
contest-weekend IQ with RBN reference spots -- a data dependency not yet
resolved).

## M4 — ML decoder stage (research-dependent)

CTC model on channel envelopes, trained on testkit synthesis + on-air recordings
labeled by RBN consensus; fused with the classical decoder via dit-style adaptive
confidence weighting. ONNX/candle inference, feature-gated.

**Accept when:**
- On the M2 50-signal Watterson-poor scene, fusion beats classical-only CER by a
  measured, documented margin at ≤ +6 dB SNR (target: ≥ 25 % relative CER
  reduction below +6 dB); no regression above +10 dB.
- CPU budget still holds with ML enabled on desktop-class hardware (Pi exempt;
  ML is optional).

## Post-1.0 candidates (explicitly deferred)

- RTTY/FT4-adjacent modes on the same channelizer.
- Multi-SDR single-daemon orchestration.
- Upstream conversation with RBN operators about accepting manta nodes.
- Spot quality feedback loop: cqdx-side confirmation (same call spotted by other
  nodes) fed back to tune validation thresholds.
