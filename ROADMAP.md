# skimmer — Roadmap

Milestones are strictly ordered; each has hard acceptance criteria. "Accuracy"
always means character error rate (CER) on `skimmer-testkit` scenes unless
stated otherwise.

## M0 — One clean signal from a file

Workspace scaffolding, `skimmer-testkit` synthetic CW generator, `skimmer-input`
file playback, single hardwired channel (no PFB), classical decoder chain
(envelope → keying → speed tracking → beam-search Morse decode).

**Accept when:**
- `skimmer decode fixture.wav` (WAV input, per SPEC-decode-core §7's M0
  definition) prints the correct text for a synthetic
  20 WPM / +20 dB SNR / AWGN-only single-signal IQ file (SPEC §7 V1).
- Proptest round-trip (text → testkit CW → decoder) passes for 10–40 WPM at
  ≥ +15 dB SNR, CER = 0.
- CI green on Linux + macOS (no SoapySDR dependency in default features).

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
- Pileup vectors V8 (AWGN: ≥ 45/50 callsigns validated, 0 bogus) and V8w
  (same 50-signal scene under Watterson CCIR-poor: ≥ 90 % of signals ≥ +6 dB
  SNR decoded with CER < 10 %, 0 bogus, zero cross-channel ghost decodes)
  both pass per SPEC-decode-core §7.
- Criterion bench: full pipeline at 192 kS/s with 300 active tracks uses < 50 %
  of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4.
- 24 h soak on live 40 m CW segment via SDR: no crash, no overrun, track
  count and evictions visible in metrics.

M2 sub-project 1 (PFB channelizer, `skimmer-dsp::channelizer`) is complete —
see `docs/superpowers/plans/2026-07-18-m2-pfb-channelizer.md` and
`docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`. M2 sub-project 2
(detector/track manager + decoder pool, `skimmer-dsp::floor` +
`skimmer-engine::track`) is complete — see
`docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md` and
`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`. M2's V8/V8w
pileup-scene validation + CPU-budget criterion bench is complete-with-
documented-`#[ignore]`s (same precedent as V2/V5/V6) — V8 passes (49/50
callsigns, 0 bogus), V8w is `#[ignore]`d as issue #28 (classical-decoder
fading-robustness gap, same family as V5/V6), and the CPU-budget Mac leg
passes at 0.360x realtime (< 0.5x budget); the Pi4 leg remains an
outstanding manual step — see
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`. Remaining M2
sub-projects: SoapySDR input, KiwiSDR input. M2 itself is not yet complete.

## M3 — Spots: validation + servers + RBN parity benchmark

`skimmer-spot` (cty.dat, SCP, CQ/DE parse, repetition gate, dedupe),
`skimmer-server` (telnet cluster protocol + JSON Lines/WebSocket), TOML config,
metrics endpoint, spot JSON Schema contributed to `dispensa`.

**Accept when:**
- A stock DX cluster client (e.g. `telnet`, N1MM) connects, logs in, and
  receives well-formed RBN-format spots.
- **Parity benchmark**: on ≥ 2 h of recorded contest-weekend IQ, skimmer achieves
  ≥ 80 % recall of RBN's spots for the same slice with ≤ 5 % false (bogus-call)
  spots. Numbers published in the repo, whatever they are.
- cqdx ingests the JSON stream in a dev environment.
- 7-day unattended soak feeding spots continuously.

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
- Upstream conversation with RBN operators about accepting skimmer nodes.
- Spot quality feedback loop: cqdx-side confirmation (same call spotted by other
  nodes) fed back to tune validation thresholds.
