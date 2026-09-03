# manta — Architecture

Headless wideband CW skimmer: SDR IQ in, RBN-compatible spots out.

This document records the core design decisions. It is deliberately opinionated:
where a choice had to be made, it is made here, with the rationale. Sections
marked **(research-dependent)** are the only intentionally open questions.

## 1. System overview

```
                ┌─────────────────────────────────────────────────────────────┐
                │                         manta daemon                         │
                │                                                              │
 RTL-SDR ──┐    │  ┌──────────┐   ┌─────────────┐   ┌───────────────────────┐  │
 Airspy  ──┼─▶  │  │  input   │──▶│ channelizer │──▶│  detector             │  │
 SDRplay ──┘    │  │ (IQ src) │   │ (PFB, 1024  │   │  (noise floor, SNR    │  │
 (SoapySDR)     │  └──────────┘   │  channels)  │   │   gate, track mgmt)   │  │
 KiwiSDR ──────▶│       ▲         └─────────────┘   └──────────┬────────────┘  │
 (network)      │       │                                      │ active tracks │
 IQ/WAV file ──▶│       │ config                    ┌──────────▼────────────┐  │
 rig audio ────▶│       │                           │  decoder pool         │  │
 (cpal)         │       │                           │  (per-signal CW       │  │
                │       │                           │   decoders, N ≤ 500)  │  │
                │       │                           └──────────┬────────────┘  │
                │       │                                      │ decoded text  │
                │  ┌────┴─────┐   ┌─────────────┐   ┌──────────▼────────────┐  │
                │  │ metrics/ │◀──│ spot server │◀──│  validator            │  │
                │  │ tracing  │   │ telnet+JSON │   │  (callsign, CQ/DE,    │  │
                │  └──────────┘   └──────┬──────┘   │   dedupe, confidence) │  │
                │                        │          └───────────────────────┘  │
                └────────────────────────┼─────────────────────────────────────┘
                                         │
                        telnet :7300 (DX cluster protocol, RBN format)
                        tcp/ws :7301 (JSON Lines spot stream → cqdx)
```

Data flows one direction. Every stage is a bounded-queue actor on the tokio
runtime except the channelizer and decoders, which run on dedicated
compute threads fed by lock-free SPSC rings (`rtrb`, same as coppa-audio) —
IQ never touches the async runtime.

## 2. Workspace layout

Multi-crate workspace following coppa's conventions (edition 2021, MIT/Apache-2.0,
workspace-level dependency table, `criterion` benches, `proptest` where invariants
allow).

```
manta/
├── Cargo.toml                 # workspace
├── crates/
│   ├── manta-input          # IQ sources: SoapySDR, KiwiSDR client, file, audio
│   ├── manta-dsp            # PFB channelizer, noise-floor estimation, envelope
│   ├── manta-decode         # CW keying state machine, timing, Morse decode
│   ├── manta-spot           # callsign validation, CQ/DE parse, dedupe, scoring
│   ├── manta-server         # telnet cluster server + JSON/WebSocket stream
│   ├── manta-engine         # orchestration: track lifecycle, decoder pool
│   ├── manta-testkit        # synthetic CW generator, golden-IQ harness
│   └── manta-cli            # `manta` binary: daemon + subcommands
```

Dependency graph (arrows = depends on):

```
manta-cli ──▶ manta-engine ──▶ manta-input ──▶ manta-dsp
                     │        ├──▶ manta-dsp ──────▶ coppa-dsp
                     │        ├──▶ manta-decode
                     │        └──▶ manta-spot ──────▶ manta-decode
                     └──▶ manta-server
manta-testkit ──▶ manta-dsp, manta-decode, coppa-channel
```

M1 added `manta-input → manta-dsp` (the shared Hilbert transformer, used
by both `AudioIqSource` and `manta-testkit`'s Watterson vector rendering)
and `manta-testkit → coppa-channel` (Watterson fading, see the M1
pinned-decisions doc).

### Reused from coppa vs. new

| Capability | Source |
|---|---|
| FFT (`FftProcessor`) | **reuse** `coppa-dsp::fft` |
| FIR design (PFB prototype) | **new** (`manta-dsp::proto`) — `coppa-dsp::filter` ships only `RrcFilter` (SPEC §10.1) |
| Envelope normalization | **new** — per-track fixed reference scale; `coppa-dsp::agc` not used in the decode path (SPEC §10.2) |
| Channel impairments for tests (AWGN, freq offset, fading, **Watterson HF**) | **reuse** `coppa-channel` |
| Audio-device input (single-channel mode) | **reuse** `coppa-audio` (cpal) — no automatic resampling; source must run natively at exactly 48000 Hz (M1 pinned decisions doc) |
| Real-to-analytic Hilbert conversion | **new** (`manta-dsp::hilbert`) — used by both live audio input and offline Watterson vector rendering |
| Polyphase filterbank channelizer | **new** (`manta-dsp`) — coppa has no channelizer |
| Order-statistic noise-floor estimator | **new** (`manta-dsp`) |
| CW keying/timing/Morse decode | **new** (`manta-decode`) — dit's algorithms, ported & headless |
| Callsign/spot validation | **new** (`manta-spot`) |
| DX cluster telnet protocol | **new** (`manta-server`) |

coppa crates are consumed as git dependencies (path deps during co-development in
this workspace-of-workspaces). If coppa publishes to crates.io first, switch to
versioned deps.

## 3. Input layer (`manta-input`)

One trait, four implementations:

```
trait IqSource: sample_rate(), center_freq(), read(&mut [Complex32]) -> …
```

- **SoapySDR** (`soapysdr` crate, feature-gated `soapy`): RTL-SDR (2.4 MS/s max,
  8-bit), Airspy HF+ (768 kS/s, the reference device), SDRplay. Runtime device
  selection by driver string. Feature-gating keeps the core buildable without the
  native SoapySDR library (CI, contributors without hardware).
- **KiwiSDR client**: the kiwisdr websocket IQ protocol (12 kHz IQ per channel) —
  narrow, but gives instant worldwide receiver access for development and lets
  low-budget nodes contribute spots.
- **File playback**: WAV (via `hound`, matching coppa) and raw interleaved
  `f32`/`i16` IQ with a small JSON sidecar for rate/center-freq. Drives the entire
  test strategy; the daemon must run identically from file and live SDR.
- **Audio passband** (via `coppa-audio`): 48 kHz real audio from a rig's RX audio,
  Hilbert-transformed to analytic. Degenerate ~3 kHz "wideband" mode; exists
  because it makes manta useful to people with zero SDR hardware, and it is the
  M1 bring-up path.

**Sample-rate assumptions.** Design center: 96–192 kS/s complex (covers any HF CW
band segment; CW allocations are ≤ 100 kHz wide). Supported ceiling: 768 kS/s
(Airspy HF+ full span). The channelizer parameterizes N to hold channel spacing
near 100 Hz regardless of input rate (§4). Multi-band via multiple daemon
instances, not one instance retuning — simpler, and SDRs are cheap.

All sources normalize to `Complex32` at the native rate into an `rtrb` ring;
input overruns are counted, surfaced as metrics, and never block the SDR thread.

## 4. Channelizer (`manta-dsp`)

**Implemented** as of M2 sub-project 1 (`manta-dsp::channelizer`) -- the
design below is now built, not just decided.

**Decision: 4×-oversampled polyphase filterbank (PFB), ~100 Hz channel spacing,
detection on channel powers, decoders attached only to active channels.**

- N = input_rate / ~93.75 Hz, rounded to a power of two: N=1024 at 96 kS/s,
  N=2048 at 192 kS/s, N=8192 at 768 kS/s. Channel spacing = rate/N ≈ 94 Hz.
- Prototype lowpass: Kaiser-designed FIR (new code, `manta-dsp::proto` — see
  SPEC §1.2), 8 taps/branch,
  passband ~140 Hz — each channel fully contains a CW signal up to ~45 WPM
  (occupied BW ≈ 4·WPM Hz ≈ 180 Hz at 45 WPM spans ≤ 2 channels; the decoder reads
  the peak channel, and the 50%+ spectral overlap between adjacent channels means
  no signal is lost straddling an edge).
- **4× oversampled outputs** (hop = N/4): per-channel output rate ≈ 375 Hz. At
  40 WPM a dit is 30 ms ≈ 11 samples — comfortably enough for envelope timing.
  2× (187 Hz, 5.6 samples/dit) was rejected as too marginal for QSB'd fast CW.
- Implementation: polyphase FIR commutator + one N-point FFT per hop
  (`coppa-dsp::fft::FftProcessor`). Frequency-domain output magnitude² feeds the
  detector directly — the PFB *is* the spectrum analyzer; no separate FFT path.

**Detector / track manager.** Per-channel noise floor by order statistics
(median of channel power over a sliding ~10 s window — median, not mean, so CW
keying doesn't inflate its own floor). A channel goes *active* when smoothed power
exceeds floor + threshold (default 6 dB) with hysteresis (3 dB drop + 5 s hang to
survive QSB and inter-word gaps). Active channel ⇒ a **track** (center channel ±1
neighbor, combined by max-power selection) ⇒ a decoder is leased from the pool.
Track cap (default 500) with lowest-SNR eviction; evictions are counted and
reported (no silent coverage loss).

**CPU budget** (the reason this whole design is viable):

| Stage | Cost at 192 kS/s | Notes |
|---|---|---|
| PFB FIR (8 taps/branch, complex) | ~12 MFLOP/s | 192k samples × 8 CMACs |
| FFT (2048-pt, 375/s) | ~42 MFLOP/s | 5·N·log₂N per FFT |
| Detection (power, medians) | ~5 MFLOP/s | incremental order statistics |
| 300 active decoders @ 375 Hz | ~10 MFLOP/s | envelope + state machine is cheap |
| **Total** | **< 100 MFLOP/s** | **≪ 1 core**; a Pi 4 core does ~5 GFLOP/s |

Even at 768 kS/s the pipeline stays under half a core; the machine's job is I/O,
not math. This budget is enforced by `criterion` benches in CI (M2 acceptance).

## 5. Per-channel decoder (`manta-decode`)

The wideband, headless port of dit's proven single-channel chain. Classical
first; ML is a fusion stage later (M4), exactly as dit evolved.

Per track, operating on the ~375 Hz complex channel stream:

1. **Envelope**: |x| → per-track fixed reference scale (SPEC §3.1; coppa's
   block AGC is not used) → smoothed magnitude.
   (A separate tone-finder stage is unnecessary here — the PFB already did the
   frequency selection.)
2. **Keying detection**: dual-rail noise/signal EMA estimators → adaptive
   threshold at their geometric mean → key-down/key-up decisions with hysteresis
   and minimum-duration debounce (the same keying-decision approach as dit,
   simplified).
3. **Speed tracking**: online 2-means clustering of mark durations into
   {dit, dah}; WPM = 1200/dit_ms, tracked with EMA. Handles 10–40+ WPM and drift;
   Farnsworth spacing tolerated by decoupling inter-element and inter-word gap
   thresholds (dit's speed-detector lesson).
4. **Element→character decode**: marks/spaces classified against the tracked
   timing model with per-element likelihoods, then a **beam search (width 4) over
   the Morse code tree** — small-Viterbi rather than hard thresholding, so a
   marginal dit/dah keeps both hypotheses alive until character boundary. Emits
   characters with confidence.
5. **(M4, research-dependent) ML decoder**: small CTC model on the channel
   envelope, fused with the classical decoder by adaptive confidence weighting —
   a direct port of dit's fusion-engine design (sliding-window accuracy
   tracking, EMA-smoothed weights, weight floor). Training corpus comes from
   `manta-testkit` synthesis + RBN-validated on-air recordings. The classical
   decoder must ship first and defines the accuracy baseline the ML stage has to
   beat under QRM/QSB (measured, not assumed).

Decoder output: timestamped character stream + WPM + SNR + confidence per track.

## 6. Spot validation (`manta-spot`)

Decoded text is noisy; validation is what makes spots trustworthy. Pipeline per
track, over a rolling text window:

No spot is ever emitted before a track's first `TrackMeta` event (SPEC §5, 1 Hz
cadence) — until then `freq_hz`/`snr_db` hold bogus `0.0` defaults, and a spot
carrying them would poison both the emitted record and dedupe's frequency
bucket. The old ≥2-repetition gate hid this by construction (reaching two
repetitions takes long enough that real telemetry always arrived first); the
BEACON/allowlist exemptions below removed that incidental protection, so it is
now an explicit invariant checked before any candidate is evaluated (MAN-28).
A candidate held back by this gate is retried the moment `TrackMeta` arrives
(not left waiting on a `WordBoundary` that a short, already-finished
transmission may never produce again).

1. **CQ/DE context parse**: regex-level scan for `CQ <call>`, `CQ TEST <call>`,
   `DE <call>`, `<call> UP`, beacon patterns (`V V V <call>`, and `<call> T`
   for NCDXF-style power-step beacons the decoder can't resolve past a
   single trailing dash, MAN-37 — suppressed whenever a bare `CQ`/`DE`
   token appears anywhere in the window at all, a deliberately coarse
   guard against mistagging an ordinary, unrecognized CQ/DE call as
   Beacon). Context determines spot type (CQ / DE / BEACON) — RBN spots
   carry this flag.
2. **Callsign plausibility**: structural grammar (prefix-digit-suffix, portable
   designators `/P /QRP /3`), then prefix lookup against **cty.dat** (bundled,
   refreshable) — a call with an unallocated prefix is rejected.
3. **SCP cross-check** (optional, default on if file present): membership in
   `master.scp` (contest super-check-partial list) *raises* confidence; absence
   only lowers it (new/rare calls must still spot, not just well-known ones).
4. **Repetition requirement**: a callsign must decode ≥ 2 times within 90 s on
   the same track before first spot (CW ops repeat their calls; single decodes
   are overwhelmingly garble). Confidence = f(decoder confidence, repetitions,
   SNR, SCP/cty hits). **Exemption**: messages already type-tagged `BEACON` by
   step 1's context parse skip this gate entirely — NCDXF-style beacons ID
   once per power-step cycle and legitimately won't repeat within the window
   (MAN-28).
5. **Dedupe/aggregation**: key = (callsign, freq bucket ±0.3 kHz); suppress
   re-spots for 10 min unless SNR improves ≥ 6 dB or type changes. Emitted spot
   carries freq (from PFB bin + track centroid, ~10 Hz absolute accuracy), SNR,
   WPM, type, confidence.

**Operator allowlist (Watch List)**: a callsign the operator explicitly lists
bypasses step 1's context-parse requirement too — not just steps 2
(grammar/cty) and 4 (repetition) — since a listed callsign with no
recognized CQ/DE/UP/beacon framing (tagged type `Unknown`) is exactly the
primary real-world case: an NCDXF beacon transmits its callsign followed
by power-step dashes, no framing words at all. Evaluated independently of
context parsing, not as a lower-priority fallback: a stale, already-
processed context match elsewhere in the rolling word window never blocks
discovery of a different, freshly-allowlisted word (`Validator::candidates`
gathers both per event). An immediate `Unknown`-typed spot is not final: if
a trailing word later completes a real context pattern for the same word
(e.g. `<call> UP` -> `De`), that reclassification is emitted as a second
spot via dedupe's existing type-changed override (step 5) -- an
already-processed word is not permanently locked to its first type.
Reclassification requires a genuinely younger word: `manta-spot::context`
returns not just a candidate/type but the byte span of every word that
determined it, which the validator maps back to word identities and their
insertion order; a later classification is only accepted when it involves
a word strictly newer than any that produced the previous one. This is
what makes reclassification promotion-only in practice -- a word is never
downgraded (to `Unknown`, or between two real context types) just because
an older framing word ages out of the rolling window, since aging out
never introduces a *newer* word, only removes an old one. Legacy
precedent: CW Skimmer's Watch List (Aggregator manual Appendix A2), which
exists specifically for calls that wouldn't otherwise pass automatic
validation (MAN-28). Dedupe (step 5) still applies.

## 7. Output layer (`manta-server`)

- **Telnet DX cluster server** (default :7300): standard login prompt, emits
  RBN-format spots —
  `DX de W3XYZ-#:  14027.1  JA1ABC   CW  23 dB  28 WPM  CQ  0312Z`.
  Read-mostly protocol; enough command grammar (`sh/dx`, filters) for common
  clients not to choke. This is the RBN/aggregator compatibility surface.
- **JSON Lines stream** (TCP and WebSocket, :7301): full-fidelity spot objects
  (adds confidence, track id, decoder text context). This is the cqdx ingest
  surface; schema published in `dispensa` as a JSON Schema contract alongside the
  existing ecosystem contracts.
- Both servers are thin fan-out consumers of one broadcast channel; slow clients
  are disconnected, never back-pressure the pipeline.
- **Exposure policy (normative, not just observed behavior):** both servers are
  designed to be internet-reachable with no client authentication, matching the
  DX cluster/RBN ecosystem's own long-standing convention (CW Skimmer, SkimSrv,
  and Aggregator have the identical property) — this is a deliberate compatibility
  choice, not an oversight, and manta-specific client auth would make it
  incompatible with the clients it exists to interoperate with. See
  `docs/DECISIONS/2026-09-02-man23-threat-model.md` findings 10/20 for the full
  threat-model rationale. The metrics HTTP endpoint (§8) shares the same
  publicly-bound-by-default posture but is NOT part of this compatibility
  contract — it's operational tooling, not an RBN-facing surface — see that same
  doc's finding 11 and `docs/RUNBOOKS/network-exposure.md` for how to restrict it.

## 8. Configuration & observability

- Single TOML config (coppa convention): device, center freq, band plan
  (CW segment limits — don't decode/spot outside them), thresholds, track cap,
  server ports, cty/scp paths, station callsign (spotter ID).
- **`tracing` throughout with `EnvFilter` is aspirational, not yet
  implemented** — corrected 2026-09-03: neither `tracing` nor `log` is a
  dependency of any crate in this workspace today (confirmed by a
  workspace-wide grep while investigating
  `docs/DECISIONS/2026-09-02-man23-threat-model.md`'s finding 9/MAN-59),
  and there is no structured logging or audit trail anywhere in
  `manta-server`/`manta-input`. `manta --status` hitting a local control
  socket for live stats is similarly not yet implemented. Prometheus text
  endpoint (feature `metrics`): input overruns, active tracks, evictions,
  decode rate, spots/min, per-stage queue depths, spot confidence
  histogram — also aspirational for several of these fields; the
  currently-implemented subset is `manta_spots_total`,
  `manta_spots_dropped_lagged_total`,
  `manta_spots_suppressed_by_filter_total`,
  `manta_spots_dropped_write_failed_total`, per-protocol client-connected
  gauges, `manta_active_tracks`, `manta_source_health`, and the uplink
  counters (`crates/manta-server/src/metrics.rs`) — not input-layer
  overruns or per-stage queue depths, which MAN-56 tracks as a separate
  gap.
- Every dropped/evicted/suppressed item is counted. **No silent loss anywhere in
  the pipeline** — if coverage was bounded, the metrics say so.

## 9. Test strategy (`manta-testkit`)

The decisive advantage of building this in this ecosystem: **synthetic ground
truth with realistic HF impairment already exists.**

- **Synthetic CW generator**: text → keyed envelope (configurable WPM, weighting,
  rise-time/click shaping, human timing jitter model) → complex tone at arbitrary
  offset. Compose *many* generators into one wideband IQ scene ("50 signals,
  10–35 WPM, −5 to +30 dB SNR, 200 Hz–96 kHz spread").
- **Impairments from `coppa-channel`**: AWGN, frequency offset/drift, and the
  **Watterson HF model** (the standard ionospheric fading/multipath model) —
  reused, not rebuilt. manta accuracy is quoted *under Watterson CCIR-poor*,
  not just clean AWGN.
- **Golden IQ corpus**: recorded band segments (contest weekends = dense QRM;
  quiet weekdays = weak-signal) with RBN's own spots for the same time/frequency
  window as reference labels → recall/precision vs. the incumbent, the headline
  benchmark for M3.
- Unit level: proptest round-trips (text → CW → decode == text) across the
  WPM/SNR envelope; criterion benches gate the CPU budget (§4).
- End-to-end: daemon run from an IQ file must produce byte-identical spot logs
  across platforms (determinism requirement; no wall-clock in the decode path).

## 10. Concurrency model

- SDR/input thread → `rtrb` ring → **channelizer thread** (owns PFB, detector) →
  per-track sample queues → **decoder pool** (rayon-style fixed worker pool,
  tracks are work items; decoders are `Send` state machines, no shared state) →
  crossbeam channel → **tokio runtime** (validator, servers, metrics, control).
- Rationale: identical to coppa's proven audio/engine split — real-time DSP on
  dedicated threads with lock-free handoff; everything with a socket lives in
  async-land.
