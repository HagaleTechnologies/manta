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
  source) but wasn't made reliable enough for CI in that ticket; not yet
  validated on real hardware, tracked as a follow-up (GitHub issue #187 /
  Linear MAN-215).

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
  signals ≥ +6 dB SNR at CER < 10 %) is **not** a blocking M2 gate: it is a
  known classical-decoder fading-robustness limitation in the same family as
  V2/V5/V6/issue #25, and closing it is classical-DSP work that lands before
  M4's ML fusion (broad-review decision D8,
  `docs/DECISIONS/2026-09-06-broad-review-decisions.md`). V8w's
  spot-validated recall was restored by MAN-213 (28/50, 0 bogus); its
  per-signal CER gate is still open (issue #28). History:
  `docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md` and
  `docs/DECISIONS/2026-10-23-man158-roadmap-security-scope-refresh.md`.
- **Real-conditions vectors VR1–VR8** (`docs/SPEC-decode-core-v2.md` §8.2)
  pass per that spec's §8.4 gate. VR vectors are a standing acceptance bar
  for every future decode-core change, not a one-time gate. **Not yet
  cleared**: 3/9 pass (VR6a, VR6b, VR7), 6/9 fail, measured against
  `engine = "hsmm"` on 2026-09-09 —
  `docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md`.
- Criterion bench: full pipeline at 192 kS/s with 300 active tracks uses
  < 50 % of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4.
  **Neither leg is a resolved pass**; both need a clean rerun on quiet,
  dedicated hardware — `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md`
  (including the superseded Mac measurements), procedure in
  `docs/RUNBOOKS/m2-pi4-cpu-budget.md`.
- 24 h soak on live 40 m CW segment via SDR: no crash, no overrun, track
  count and evictions visible in metrics. **Outstanding** — procedure:
  `docs/RUNBOOKS/secondary-skimmer-field-node.md` (MAN-96).

**Status: M2 is not complete.** Open gates: the VR1–VR8 gate, both CPU-budget
legs (Mac and Pi4), and the 24 h live-SDR soak. All five M2 sub-projects are
complete; their pins records are linked from `docs/DECISIONS/README.md`.

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
  Procedure: `docs/RUNBOOKS/secondary-skimmer-field-node.md` (MAN-96); still
  outstanding until a run's evidence is recorded.

`manta-spot` (callsign/CQ-DE validation, cty.dat/SCP cross-check,
repetition gate, dedupe) is complete as a standalone crate -- see
`docs/superpowers/specs/2026-07-25-m3-manta-spot-design.md` and SPEC
-decode-core.md §7.1 (V11-V15). It is now wired into `manta-engine`'s
batch (`decode_samples`/`decode_wav`) and streaming (`listen`) pipelines,
both emitting real `Spot`s -- see
`docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md`.
`manta-server` has also landed: the telnet cluster server, the
JSON/WebSocket stream, TOML config, the metrics endpoint, and the
outbound RBN uplink are all shipped and tested. The uplink is tested only
against mock collectors and stays experimental until MAN-90's capture shows
what RBN's ingest accepts. Remaining M3 sub-projects:
just the RBN parity benchmark (needs ≥ 2 h of recorded contest-weekend IQ
with RBN reference spots -- a data dependency not yet resolved).

**Shipped sub-projects are not acceptance, and M3's acceptance is wholly
open**: all four accept-when gates above are still unmet -- the parity
benchmark itself, the 7-day unattended soak, cqdx ingesting the JSON
stream, and the stock DX-cluster client (`telnet`, N1MM) session against a
running node. That last one is *not* discharged by the servers' automated
acceptance suites: those drive scripted TCP clients, which exercise the
wire format but say nothing about how a real contest logger behaves
against it. Since MAN-269 anyone can run that session without an SDR, by
replaying `manta gen v1` output with `manta run --realtime` (see the
README's [60-second demo](README.md#60-second-demo) section, whose replay
keeps the server up for 120 seconds).

**RBN admission** is pre-1.0 work, not deferred: the near-term path is a
Skimmer-Server-compatible handshake so existing aggregators can accept a
manta node, with direct-uplink admission negotiated from a position of
having live nodes.

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
- ~~Multi-SDR single-daemon orchestration.~~ Pulled forward and delivered by
  MAN-13 (`[[input]]`); several receivers on one HPSDR radio remain MAN-307.
- Spot quality feedback loop: cqdx-side confirmation (same call spotted by other
  nodes) fed back to tune validation thresholds.
