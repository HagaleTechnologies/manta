# Manta: twenty investigations toward exceptional CW reception

Manta's largest opportunity is to preserve and combine evidence that the current pipeline commits or discards too early. Keep complex signal information through frequency tracking; retain alternative timing and word boundaries; accumulate evidence across genuine repetitions; distinguish the transmitting station from the station being called. Couple those changes to a benchmark that measures what was actually audible at this receiver. Improving the character classifier alone will leave substantial losses elsewhere.

This is an implementation research handoff, reviewed against Manta on 2026-09-09. The twenty investigations below are proposals, with experiments that can reject them. They do not claim demonstrated superiority over CW Skimmer, measured recall improvements, or new accepted architectural decisions. Several proposals deliberately revisit choices in SPEC-decode-core; promote successful experiments into explicit decision/spec amendments before making them production defaults.

## Baseline, scope, and evidence

The reproducible code baseline is Manta **`45cf11444979e9c1d48aa6f90164aea7b7c3b695`**, freshly fetched `origin/main`. The comparison snapshot of `dit` is **`2e771fa60ad16dd384a4e4a66e4005a67b70d02a`**. Manta's coppa dependency is **`f8a4d16df7e5776a0756943c05712038774e6c70`**. The dispensa contract snapshot inspected is **`0ec48ad9cd7c44428c1de00e85f3d1ef6fe167c0`**. Pin all four when reproducing these observations. References at the end link to exact source revisions where possible.

Evidence labels used throughout:

- **Measured:** executed against the pinned Manta source during this investigation.
- **Observed:** directly established by reading source or a specified data artifact.
- **Reported:** another investigation's result; not reproduced here.
- **Proposed:** an original design, experimental value, or acceptance target requiring validation.

### What the approximately 30% number means

PR #144 reports **31.0% precision, 4.2% recall**, with 197 output spots and 1,451 reference call/kHz bins. It does not report 30% recall. These are historical results from its scorer, not a new measurement of current `main`. [M1], [P144]

The scorer at `7dee010b946798b434f4c5605943139e7bef3d2e` has four consequential limitations:

1. **Reference population:** the committed CSV contains **26,802 parsed rows and 177 distinct spotting labels**. It aggregates receivers with different propagation and antennas. Their union is not the set of signals audible in this one IQ recording.
2. **No time matching:** output times are calculated, but `match()` uses only callsign and frequency. A match anywhere in the entire window counts.
3. **Inconsistent counting units:** the recall numerator is matching output occurrences; the denominator is distinct reference call/kHz bins. A measured probe with one reference and three duplicate outputs returns **300% recall**, although reference coverage is 100%.
4. **Unmatched is not necessarily false:** a valid local decode absent from RBN is counted as a false positive. Conversely, a wrong decode can coincidentally match a distant station in the broad frequency/time window. Default frequency tolerance is 500 Hz, and kHz binning can split one drifting signal or conflate nearby activity.

Consequently, neither historical percentage establishes local decoder recall or precision. Fixing measurement will not improve the receiver, but it will prevent optimizing toward the wrong objective. The poor golden-vector results and concrete source defects independently establish substantial engineering work.

The local recording derivatives already present were inspected by WAV header: B2 is stereo **PCM16, 192 kS/s, 899.2427 seconds**; WPX is stereo **PCM16, 96 kS/s, 691 seconds**; the pileup audio derivative is mono **PCM16, 48 kS/s, 323.4482 seconds**. Existing documentation describes the original 24-bit recordings. Do not convert these derivatives again merely because those descriptions say PCM24 is unsupported. Their ownership/redistribution restrictions remain unchanged. No recording or real transcript accompanies this handoff.

### Confirmed engineering observations

| Observation | Evidence | Implication |
|---|---|---|
| PFB prototype attenuation is **−6.0219 dB at 46.875 Hz offset**, with **82.2317 Hz ENBW**, **85.3333 ms support**, and **42.6615 ms group delay**, for N=1024 at 96 kS/s. | Measured direct DTFT and tap-energy calculation using the production prototype. [M2] | A signal halfway between channels is attenuated in both. Frequency interpolation estimates location; the selected-channel decoder does not combine their energy. |
| Tracking selects the largest instantaneous power of three owned channels; only its square-root magnitude reaches `TrackDecoder`. | Observed `Track::select_channel`, `step_hop`, `drain_pool`. [M3] | Phase, alternate channels, and their uncertainty are lost; selection can follow a neighbor or a noise excursion. |
| Detection defaults are 12 dB onset, 19 confirmation hops, 750 warmup hops, 500 total track entries. | Observed actual defaults, which differ from older prose. [M3] | Thresholds tuned against isolated synthetic signals need dense-band validation. The cap implementation counts candidates as well as decoded tracks. |
| The demodulator initializes from 375 samples, replays the successful initialization window, and waits for five marks to initialize speed. | Observed `Demod::push` and `TrackDecoder::on_run`. [M4], [M15] | The successful one-second buffer is already replayed; missing work is pre-promotion history, failed initialization windows, short EOF, and robust bootstrap. |
| A 0.8-second alternating envelope yields zero events even after `finish()`. | Measured direct `TrackDecoder` probe, 300 samples at 375 Hz. | Short streams remain undecoded while initialization is incomplete. This is a structural probe, not a callsign-recall measurement. |
| Gaps are classified before a character-local beam; all invalid paths can produce no glyph at all. | Observed decoder and beam. [M15], [M5] | A downstream beam cannot recover an earlier merged boundary; silently deleted garble can concatenate surrounding callsign characters. |
| `CQ DX W1AW` returns no context candidate; `DE W1AW DE K1ABC` returns only W1AW. | Measured direct parser calls. [M6] | Correct character copy can still fail to become an identity observation. |
| A synthetic `CQ W1AW W1AW` event sequence with every character confidence zero emits a spot with confidence approximately **1.03×10⁻⁷**. | Measured through the bundled validator; nonzero frequency/SNR metadata supplied, no allowlist. [M7] | Confidence is descriptive today; automatic emission has no minimum acoustic-confidence gate. |
| `F/W1AW` is rejected while `W1AW/P` is accepted. | Measured grammar probe. [M8] | Prefix-style portable identifiers are excluded structurally, independently of acoustic quality. |
| `IqSource::read` returns a sample count with no discontinuity or sample-origin metadata. | Observed input trait. [M9] | Downstream code cannot distinguish missing samples from uninterrupted acquisition through this interface. |
| Some chunking tests still exercise deprecated `SingleChannelExtractor`, while PFB tests cover channelizer output separately. | Observed existing tests. [M10] | These tests do not establish chunk-invariant track lifecycle, spot validation, or streaming/file equivalence for the full current pipeline. |

The normally ignored acceptance tests were explicitly run on the pinned revision:

| Test | Required CER | Measured CER | Result |
|---|---:|---:|---|
| V2, fast near-edge CW | ≤0.01 | **0.0325203** | FAIL |
| V5, Watterson-poor CW | ≤0.20 | **1.4166667** | FAIL |
| V6, sinusoidal QSB | ≤0.10 | **0.1428571** | FAIL |

CER is edit distance divided by reference length; insertions permit values above 1.0. These runs fail at the CER assertions, so they do not independently measure later assertions such as V2 WPM. V8w's 1/34 passing strong signals and median CER 0.2755 are **reported** in PR #106, not rerun measurements here. The source's ordinary green test selection excludes these failing gates. [M11], [P106]

### Decisions and parallel work to preserve

The September 6 decisions require classical fading fixes before M4 and pause Pi4 optimization until MAN-100–113 land. This handoff follows that ordering; it does not inherit another session's purported CPU-budget waiver. Memory and operation counts still matter, and Pi4 remains an eventual measured target. The existing `README` non-goal of a full QSO logger remains compatible with retaining receive observations and uncertain exchange roles. [M12]

| Existing work | Relationship to this handoff |
|---|---|
| [#144: MAN-166 benchmark](https://github.com/HagaleTechnologies/manta/pull/144), head `7dee010b…` | Coordinate scorer corrections and reference labeling with its owner; rank 1 extends this work. |
| [#149: MAN-166 repetition identity and cap](https://github.com/HagaleTechnologies/manta/pull/149), head `c194a358…` when inspected | Inspect before ranks 4, 8, and 9. A larger cap is not proof that all resulting tracks represent distinct stations. |
| [#145: doctor](https://github.com/HagaleTechnologies/manta/pull/145), [#146: overflow retries](https://github.com/HagaleTechnologies/manta/pull/146), merged [#143: manual gain](https://github.com/HagaleTechnologies/manta/pull/143) | Starting points for ranks 12 and 13; avoid duplicate input-health implementations. |
| [#96: short text](https://github.com/HagaleTechnologies/manta/pull/96), [#102: word spaces](https://github.com/HagaleTechnologies/manta/pull/102), [#105: persistent garble](https://github.com/HagaleTechnologies/manta/pull/105) | Acquisition and timing overlap with ranks 3, 5, and 7. |
| [#97](https://github.com/HagaleTechnologies/manta/pull/97), [#137](https://github.com/HagaleTechnologies/manta/pull/137): WPM | Coordinate measured edge compensation with ranks 2 and 7. |
| [#103: Hilbert leakage](https://github.com/HagaleTechnologies/manta/pull/103), [#104: V6 fading](https://github.com/HagaleTechnologies/manta/pull/104), [#106: V8w baseline](https://github.com/HagaleTechnologies/manta/pull/106) | Reuse diagnostics; do not treat unsuccessful parameter sweeps as a bound on all classical methods. |
| [#132: bogus-spot regression](https://github.com/HagaleTechnologies/manta/pull/132), [#133: truncated calls](https://github.com/HagaleTechnologies/manta/pull/133), [#134: SNR](https://github.com/HagaleTechnologies/manta/pull/134), [#90: framing](https://github.com/HagaleTechnologies/manta/pull/90) | Precision, provenance, and calibration dependencies for ranks 3, 10, and 11. |
| MAN-107–113, MAN-33, MAN-117 | Existing classical DSP, exchange extraction, and deferred allocation work. Reconcile current tickets before opening implementation claims. |

These are review-time snapshots, not promises that the PRs remain open or unchanged. Re-fetch before implementing. The transient MAN-166 notes contained differing intermediate percentages; this document does not promote any of them to a new validated baseline. A high proportion of Beacon-tagged outputs could also indicate false beacon classification, not simply scarce callsign repetitions.

## Success criteria and evaluation contract

The research objective is **local, correctly attributed identity recall at a controlled false-report rate**, with raw text and uncertainty retained. Track three different tasks: transcription, transmitter identification, and public spotting. A station's callsign can be copied perfectly as the addressee of a transmission and still be wrong to report as its transmitter.

Use these proposed progression gates, without weakening existing golden tests:

1. **Repair baseline:** reproducible stage-by-stage measurements, zero evaluator accounting defects, all presently passing goldens preserved, failing goldens reported explicitly.
2. **Classical acceptance:** clear V2/V5/V6 and the original V8w requirements; run the existing ≥2-hour M3 comparison at its published ≥80% recall/≤5% bogus criteria using a receiver-appropriate reference. If redefining the reference changes a normative metric, record that explicitly.
3. **Excellence target:** ≥95% recall on independently labeled, locally decodable station-identification opportunities at ≥99% precision; report weak-signal and close-pair strata separately. Also target ≤0.1 false public identities per 100-kHz observation-hour, median first-confirmed-identity delay ≤one additional genuine repetition, and 95th-percentile delay ≤10 seconds after sufficient local evidence exists. These are proposed targets, not measured capability.
4. **Competitor comparison:** identical recorded IQ, equal passband and time window, documented software versions/settings/dictionaries, compatible spot policies, matched latency and computing conditions. Report both an operational default comparison and recall/precision curves. State explicitly whether comparison is CW Skimmer or Skimmer Server. A global RBN union is not a substitute for this paired experiment.

Label uncertain cases `unresolved`, with separate counts and precision bounds; never silently classify them as either correct or false. Report signal-presence recall, track coverage, CER including spaces, callsign exact-match recall, emitter-attribution precision, frequency error, latency, duplicate rate, candidate/track churn, and actual CPU/RSS. Include eligible-but-undetected signals in end-to-end denominators. Use recording/day/site-held-out splits and paired uncertainty intervals clustered by recording or transmission sequence, not per correlated character.

Two hours cannot establish a rare false-report rate of 0.1/hour with much confidence. With zero errors and a Poisson approximation, the one-sided 95% upper rate is approximately `3 / exposure_hours`; about 30 appropriately normalized observation-hours are needed to reach 0.1/hour. Use additional stratification when errors cluster. Publish numerators, denominators, exposure, and uncertainty, not only percentages.

## Stack rank

Ranking is expected engineering value under current evidence, combining likely recall impact, precision risk, and cost. It is not a forecast of percentage-point gains. A dependency may start earlier than its rank: minimal corpus construction from rank 17 starts with rank 1, and rank 11's emission gate must accompany any more permissive production detector.

Effort is a rough engineering estimate: **S** 2–5 working days, **M** 1–2 weeks, **L** 3–6 weeks, **XL** a research program. Estimates exclude waiting for hardware, labeling, or rights clearance.

| Rank | Investigation | Main expected benefit | Evidence confidence | Effort |
|---:|---|---|---|---|
| 1 | Receiver-local truth and a measured loss funnel | Find the actual limiting stages | High | M |
| 2 | Recentered complex subchannels and timing-aware filtering | Recover channel-edge and weak-signal evidence | High | L |
| 3 | Joint soft timing, segmentation, and Morse search | Recover boundaries and fade-damaged elements | High | L |
| 4 | Keying-aware acquisition and evidence-based admission | Hear short/weak CW without filling the pool with artifacts | High | M–L |
| 5 | Pre-roll, short-burst initialization, and complete finalization | Recover first and last callsigns | High | M |
| 6 | A fading-aware observation model | Stop QSB turning into extra dots and missing marks | High | L |
| 7 | Multiple speed hypotheses and separate fist parameters | Recover fast, weighted, and Farnsworth sending | High | M–L |
| 8 | Physical signal identity and close-track arbitration | Avoid merging neighbors and duplicating one signal | High | L |
| 9 | Align and combine repeated callsign evidence | Recover identities no single copy gets entirely right | High | L |
| 10 | Exchange grammar and transmitter/addressee attribution | Understand who actually transmitted the call | High | L |
| 11 | Calibrated confidence, physical SNR, and selective emission | Permit higher recall at a trustworthy false-spot rate | High | M–L |
| 12 | Loss-aware input, sample clocks, and one replay/live core | Prevent acquisition and delivery corruption | High | M–L |
| 13 | RF health, image suppression, and robust interference conditioning | Make real input resemble the assumptions of the decoder | Medium–high | M–L |
| 14 | Joint nearby-signal fitting and cautious cancellation | Recover weak stations beside strong ones | Medium | XL |
| 15 | Bounded delayed re-decoding from preserved observations | Use later evidence to rescue earlier copy | Medium–high | M–L |
| 16 | Diverse classical hypotheses with evidence-level fusion | Exploit complementary decoder errors | Medium | M |
| 17 | A realism-driven corpus and independent synthesis | Generalize beyond repeated golden phrases | High | L, ongoing |
| 18 | Durable observation records and reporting provenance | Keep useful data even when public spotting abstains | High | M |
| 19 | Receiver/antenna diversity with local provenance | Escape fading and collisions one receiver cannot resolve | Medium | XL |
| 20 | Optional learned observations after the classical baseline | Recover residual nonlinear/complex impairments | Medium | XL |

## 1. Establish receiver-local truth and a measured loss funnel

**Why first.** The current 4.2% figure conflates acoustic detection, decoding, context recognition, repetition, and reference coverage. The scorer defect is measured, and the 177-spotter reference cannot establish one receiver's attainable recall. PR #144 is valuable infrastructure to correct and extend. [M1]

**Design.** Introduce an offline `ObservationTruth` model with recording ID, sample interval, RF interval and uncertainty, locally audible signal ID, optional transcript, identity role, and label certainty. Maintain three reference layers: manually verified local truth, paired same-input comparator output, and wider RBN discovery hints. Only the first establishes correctness; the second measures parity; the third helps locate candidate windows for review. Have labelers inspect intervals without Manta's predicted call first, then adjudicate disagreements.

Build a loss funnel that follows each eligible identity opportunity through input coverage → detectable energy → admitted track → usable timing → text/lattice contains call → self-identification recognized → sufficient evidence → emitted → delivered. A rejection can have several contributing flags, but assign exactly one earliest failed stage for the waterfall totals. Candidate births and active-track births must have distinct counters.

**Implementation plan.** Extend `scripts/score-against-rbn.py` after coordination with #144. Read reference timestamps and receiver identifiers. Replace kHz-bin counting with deterministic one-to-one interval/frequency matching under a declared policy. Keep event-level and unique station/frequency-episode recall separate; duplicates receive no extra recall credit. Add a local-only `TraceSink` to engine/decoder/validator with bounded summary counters and opt-in detailed traces. Preserve exact recording/config/code/data hashes in the report.

Add counterfactual experiments: known frequency tracks bypass detection; known key transitions bypass demodulation; known timing bypasses speed estimation; known transcript bypasses acoustics; known transmitter roles bypass parsing. Each oracle diagnoses an upper bound for that intervention on that dataset. Oracle-assisted results must never enter the production scoreboard.

**First experiment and gate.** Verify a tiny scorer fixture: three duplicates of one truth produce 100% coverage, one unmatched call stays unresolved until adjudicated, different times do not match, and two outputs cannot claim the same truth opportunity. Then label 100 missed and 100 unmatched B2 windows stratified by SNR, density, and output type. A useful first deliverable is an accounted loss distribution, not a tuning patch. Grow to ≥2 hours across multiple sessions before performance claims. **Dependencies:** #144, a minimal slice of rank 17. **Stop rule:** do not rank a downstream algorithm by an uncorrected global-union metric.

## 2. Preserve complex observations and recenter each signal before decoding

**Why high.** The actual PFB has a narrow low-pass response centered on each 93.75-Hz bin. Its midpoint attenuation is about 6 dB, and the decoder takes only the strongest magnitude. The spec's interpolation language does not describe energy recovery that exists in code. Its 85-ms impulse-response support also spans several fast-CW elements; support is not the same as resolution, but edge distortion must be measured. [M2], [M3]

**Design.** Separate wideband discovery from narrowband decoding. Use the existing PFB initially for discovery. For admitted signals, retain a complex observation stream, estimate residual carrier frequency, and supply a frequency-centered channel with known amplitude response, noise bandwidth, and delay. Evaluate two independent architectures:

- A shared coarser channelizer feeding per-track NCOs and short low-pass filters. Choose coarse bandwidth/output rate sufficient to preserve the widest supported keyed waveform before decimation.
- A reconstruction from adjacent oversampled PFB channels, using measured channel responses and noise covariance. Treat this as a synthesis problem; do not sum magnitudes or assume arbitrary analysis filters are perfectly reconstructing.

The decoder should choose among a small filter bank according to speed uncertainty and interference. Initial experiments can span 50–250 Hz useful bandwidth and 375/750/1500 samples/s track streams. Every combination must satisfy its anti-aliasing constraints. A filter matched to a known pulse shape is an observation-likelihood operation; it is not synonymous with making the receive filter as narrow as possible. Fldigi documents speed-related matched-filter operation, which is useful behavioral precedent, not a source implementation. [E3]

**Implementation plan.** Add internal `ComplexObservation` and `TrackFrontend` seams in `manta-dsp`/`manta-engine`; keep existing `push_envelope` as a baseline adapter. Carry gain, group delay, validity, and noise estimates with each stream. Estimate residual frequency from phase differences only during trusted key-down spans, with unwrap bounds and uncertainty; coast during silence/fades. Crossfade any filter retune while preserving sample identity. If a single already-narrow PFB bin is retuned, explicitly acknowledge that it cannot reconstruct sidebands that were removed upstream.

**First experiment and gate.** Sweep carrier offsets uniformly across a bin, speeds 8–60 WPM, rise/fall shapes, and neighboring signals. Compare raw edge timing, callsign recall, and false reports for the current path, fine DDC, and multichannel reconstruction. Proposed promotion bar: ≥50% reduction in the current edge-versus-center recall deficit, no significant center regression, and stable performance with a stronger neighbor. Record real CPU/RSS and response curves. **Dependencies:** rank 1; coordinates with #97/#103/#137 and MAN-107. **Reject** a path that merely raises reported SNR or trades edge loss for inseparable neighbors.

## 3. Decode key state, durations, boundaries, and Morse jointly

**Why high.** The current search considers alternate dits/dahs only after the gap classifier has fixed character boundaries. It cannot undo a dropped mark, a merged pair of characters, or a false split. `decode_char(None)` contributes only to `garble_count`, so the validator can receive adjacent characters with missing evidence silently erased. [M15], [M5]

**Design.** Build a bounded explicit-duration state search over key-down/key-up state, Morse-tree node, speed hypothesis, spacing hypothesis, and recent output prefix. States distinguish within-character gap, character boundary, word boundary, and idle/no-signal. Add an erasure/unknown observation that preserves time and uncertainty; it must never make the surrounding text look contiguous and fully supported.

Score complete paths with sample-observation likelihood, duration likelihood, and modest structural priors. An initial semi-Markov recurrence is conceptually `best[t,state] = max(previous_state,duration){best[t-duration,previous_state] + transition + duration_score + observation_score}`. Prefix sums or bounded sufficient statistics make segment scoring cheap. Use proper normalized duration probabilities when comparing different segmentations; constants that cancel for fixed-length dit/dah alternatives may not cancel across different numbers of elements or characters. Explicit-duration models are established sequence-modeling tools, including published Morse examples; this design uses a small deterministic model, not the research paper's nonparametric sampler. [E4]

**Implementation plan.** Add `manta-decode::observations`, `duration_model`, and `trellis` as experimental modules. Begin with current envelope-derived soft evidence and known speed; then enable uncertain boundaries, then speed alternatives. Use a fixed beam/state budget, stable lexical tie-breaks, bounded lookahead, and a committed-prefix watermark. Keep a compact lattice of alternatives until word/identity resolution. Treat operator error prosigns, punctuation, and unknown glyphs explicitly. At the validator seam, preserve exact spans for every token and prohibit automatic calls from spanning unresolved erasures without independent evidence.

**First experiment and gate.** On a frozen trace set, compare four ablations: current path, soft marks only, soft gaps only, joint model. Include `EE/I`, `TT/M`, merged callsign suffixes, shortened dashes, lost dots, and corrupt CQ/DE tokens. Proposed success: ≥25% relative CER reduction on the failed fading subset, fewer truncated/merged false calls, and no regression on clean/random-text controls. Also run exact exhaustive search on very short traces to validate pruning and boundary scoring. **Dependencies:** rank 1; ranks 2/6/7 improve observations later. **Spec change:** explicitly replace the §4.3/§4.4 prohibition on cross-character ambiguity, supported by bounded-cost evidence.

## 4. Acquire keyed signals using accumulated evidence and separate admission budgets

**Why.** A 19-hop continuous rise lasts 50.67 ms. An ideal dit is shorter above approximately **23.7 WPM**, although PFB ringing, EMA memory, and signal strength mean this is not a hard impossibility result for the detector. The current threshold is intentionally high to control synthetic noise excursions; weakening one constant can shift bootstrap behavior as well as false detections. [M3]

**Design.** Separate cheap spectral candidates from leased decoders. Accumulate evidence over several plausible marks and spaces rather than requiring a single uninterrupted threshold excursion. Combine robust local SNR, repeated key-like modulation, carrier concentration, and consistency over time. Use a sequential log-evidence accumulator with bounded forgetting and distinct promote/reject thresholds. A continuous carrier may be real RF but should not consume an indefinite CW decoder lease.

Calibrate false alarms empirically on channelized noise with its actual temporal and cross-channel correlation. A lower-quartile statistic is not mean noise power: for ideal exponential power, the 25th percentile is about 0.288 times its mean, a −5.41-dB offset. The current smoothed dB power minus a quantile floor mixes different statistics. Receiver filters and dense occupancy complicate both further. Estimate a signal-free noise distribution and a locally calibrated detection statistic; do not copy an IID textbook threshold into correlated PFB output.

**Implementation plan.** Refactor `DetectorConfig` and `TrackManager` into bounded candidate, active, and recovery pools. Track spectral peak proposals before assigning full track IDs. Budget admissions using acoustic/keying evidence, age, and reserved exploration capacity; reserve existing promising tracks during a fade. Distinguish `candidate_rejected`, `active_evicted`, `non_cw_expired`, and `merged_same_signal`. Never rank capacity solely by whether the current transcript resembles a known call, which would suppress difficult/unfamiliar stations.

**First experiment and gate.** Sweep 8–60 WPM, all-dit/all-dah openings, sparse one-shot calls, impulsive noise, carriers, shaped receiver noise, and dense contest IQ. Measure promotion probability versus false decoder-seconds, not just track count. Proposed success: ≥20% relative reduction in detector-attributed misses at equal false decoder-seconds and no V10 bootstrap regression. A 1200-slot cap may be an interim setting from #149, but validate whether the extra slots are distinct signals or duplicate/noise demand. **Dependencies:** rank 1, rank 5 for history/bootstrap; rank 11 before more permissive public emission. **Stop rule:** a flat recall result after reducing churn means proceed to other stages rather than endlessly raising the cap.

## 5. Recover acquisition history, short bursts, and trailing evidence

**Why.** Global detection warmup, confirmation, per-track rail initialization, and speed initialization create interacting cold starts. The demodulator already replays its successful initialization window; duplicating that mechanism is wasted work. It has no pre-promotion samples, and short EOF does not initialize from a partial window. [M3], [M4], [M15]

**Design.** Maintain a circular history indexed by original sample time, sufficient to replay the onset once a candidate is confirmed. Preserve failed bootstrap windows within a bounded horizon, and allow hypothesis-based partial initialization on a finite stream. Distinguish what is not yet known from what was not recorded. Warmup can delay emission while analysis/history collection proceeds.

Use separate acquisition status for noise calibration, carrier estimate, mark timing, spacing, and metadata. A short call may contain enough acoustic structure to evaluate several timing hypotheses without meeting a fixed five-mark or one-second milestone. A one-element recording is fundamentally ambiguous; retain alternatives rather than fabricating an identity. Delay public output until the requisite evidence exists, while retaining tentative local observations.

**Implementation plan.** Add history at the engine frontend, storing complex/coarse-channel samples with provenance. As a scale estimate, four seconds of 192-kS/s complex f32 IQ is **6.144 MB**; four seconds of all 2048 PFB outputs at 375 Hz is **24.576 MB**, before metadata. Choose the representation deliberately and avoid a whole-wideband copy per track. Initialize a new decoder at a sample-aligned replay cursor, catch it up once, and deduplicate by evidence span. Unify finite EOF, hang expiry, merge, eviction, and shutdown: consume pending data, resolve/erase open tokens, emit final metadata if meaningful, then close. Current mid-batch removal can discard pending observations; verify exact cases before changing lifecycle behavior.

**First experiment and gate.** A table of prefix/suffix truncations at every hop phase, 0.3–10-second bursts, different chunk boundaries, and streams ending before demod initialization. Include a two-call transmission that currently loses the first call. Proposed success: ≥50% reduction in acquisition-attributed misses on locally decodable short bursts, no duplicated repetitions from replay, and exactly one closure after all prior evidence. **Dependencies:** rank 1; coordinate #96 and #149. **Reject** adding arbitrary padding that is counted as real received silence or extending every transmission until a golden happens to pass.

## 6. Model fading as observation uncertainty rather than keying

**Why.** V5 and V6 fail when ordinary clean vectors pass. The rail tracker updates based on its own current threshold, so a fade can both change the apparent key state and contaminate the next threshold. Speed estimation then learns the resulting false durations. The observed V5 insertion-heavy output is consistent with this failure family, but a causal trace is required to allocate blame. [M4], [M11]

**Design.** Start from a generative local model: a keyed carrier with slowly/rapidly varying complex gain plus additive noise, optionally extended to delayed paths. Estimate a noise distribution independently of the key-down level. Produce a soft likelihood for key-up/key-down conditioned on recent signal amplitude and uncertainty. A deep fade should reduce discriminability, rather than assert a clean key-up transition.

Evaluate two classical observation engines: a noncoherent amplitude model with Rayleigh/Rician-like statistics and a complex-gain tracker used only where coherence is supported. Robust gain updates should be weighted by key-down posterior, protected against clipped samples and impulses, and regularized across plausible fade timescales. Retain multiple amplitude hypotheses when a fade and a genuine gap are indistinguishable locally. The duration search in rank 3 supplies additional evidence without forcing every trough into a dash.

**Implementation plan.** Add a `FadingState` and `NoiseState` behind the rank-3 observation interface. Implement an interpretable baseline first, then optional two-timescale or small gain-state mixtures. Log predicted gain, observation residual, uncertainty, and transition attribution. Use ITU-R F.1487 as an impairment reference; validate the pinned generator's Doppler convention, ensemble normalization, and power budget before attributing every pathology to the receiver. [E5]

**First experiment and gate.** V6 sinusoidal fading separates envelope adaptation from multipath. Progress to held-out Watterson seeds, then actual fading bursts. Compare transition insertion/deletion counts before speed adaptation and downstream callsign accuracy. Require improvements at matched false-identity rate and no degradation when a real key-up occurs during a fade. Do not normalize away fades in test data. **Dependencies:** ranks 1/3; coordinates MAN-108–109 and #104/#106. **Reject** a gain tracker that simply smooths short dots away. No one-receiver method can recover arbitrary characters completely buried in an unobserved fade without additional evidence.

## 7. Keep multiple speed hypotheses and model the sender's timing separately

**Why.** One online two-cluster estimate conflates actual keying speed, edge stretch from the receive filter, dah weighting, human fist, and fade-induced segmentation errors. An all-dah opening can be ambiguous with a slower all-dit interpretation. Character and word spacing can vary independently, including operator/keyer speed changes mid-exchange. [M15], [M13]

**Design.** Maintain a bounded mixture of dit-period hypotheses rather than one centroid pair. Begin with a broad log-spaced 8–60 WPM bank, prune after evidence, and refine locally. Each hypothesis has separate parameters for dit duration, dah/dit ratio, intra-element spacing, character spacing, word spacing, and edge bias. Include straight-key/bug-style asymmetric variability and Farnsworth as explicit spacing possibilities, with regularization toward standard timing. International Morse timing is the reference, not a claim that every human sends exactly those durations. [E6]

Estimate receive-path edge bias from paired mark/space behavior and the known filter response; a fixed subtraction from every measured mark is an experiment, not a universal correction. Use posterior responsibilities to update timing, so a likely fade fragment does not immediately move the global speed estimate. Keep reporting WPM separate from the instantaneous timing hypotheses used to decode. Detect genuine speed changes by sustained improvement of another hypothesis, preserving the previous state long enough to reconsider a false switch.

**Implementation plan.** Introduce `TimingHypothesis` and a bounded `TimingBank` in `manta-decode::timing`. Initially score existing runs under several hypotheses, then connect to rank 3. Use sample-based age and deterministic tie ordering. Treat Farnsworth bootstrap and forced word flushing as one consistent model: a timeout must not prevent long-gap evidence from ever reaching the model that could lengthen that timeout.

**First experiment and gate.** Openings `EEE`, `TTT`, digits, short calls, speed steps, drifting speed, weighted dashes, uneven gaps, and per-hop onset sweeps near channel edges. Score identification recall before speed convergence, boundary errors, and time to recover after a step. Retain V10 and add the #149 confirmation-hop cliff to the regression matrix. Proposed success: halve bootstrap/timing-attributed failures, meet existing WPM tolerances, and avoid speed jumps caused by isolated fades. **Dependencies:** ranks 1/3; coordinate #97/#102/#105/#137 and MAN-110–112.

## 8. Track physical emissions, not just nearby channel peaks

**Why.** A track selects maximum power from three channels and merges with any center within one channel. Nearby independent stations, images, multipath spread, and the same station crossing a bin demand different treatment. The centroid updates without key-down gating, and its raw channel-index EMA does not use circular differences at the FFT wrap. These are specific implementation risks to test, not evidence that every observed merge is wrong. [M3]

**Design.** Represent `SignalTrack` separately from `TransmissionEpisode` and `StationHypothesis`. A physical track can contain multiple turns by different stations on nearly the same frequency; a station hypothesis may be supported by several reacquired track fragments. Association should consider frequency uncertainty, drift, temporal overlap, keying-envelope correlation, phase consistency where available, and collision evidence. Frequency proximity alone cannot establish identity.

Use a bounded assignment problem within small overlapping frequency neighborhoods. Penalize unexplained jumps and let an `ambiguous_pair` survive temporarily when two emitters cannot yet be separated. Require duplicate-signal evidence before merging. Prefer a soft observation allocation over making three-bin ownership exclude a weaker neighbor entirely. Carry source identity in all association keys.

**Implementation plan.** Refactor `owner_of` into a candidate-association structure; keep deterministic ordering by source, sample, birth ID. Unwrap carrier coordinates for local tracking and wrap only at channel indexing. Freeze or predict centroid during key-up/fade. Replace unconditional pairwise proximity merging with a tested same-signal score and explicit loser-evidence transfer/finalization. Count ID switches, duplicate decoder-seconds, mistaken merges, and lost true emissions independently. Keep ambiguous episodes separate at the repetition layer until association is justified.

**First experiment and gate.** Independent keyed pairs at 20/40/60/94/150/300 Hz separation, power ratios through 30 dB, crossings, mirror images, intermittent transmitters, and carriers straddling channel zero. Establish an achievable separation region rather than demanding two decoded identities when the observation is physically unidentifiable. Proposed success: zero wrong merges in controlled separable fixtures and ≥50% reduction in duplicate/misassociated track time on audited real windows. **Dependencies:** ranks 1/2, #103/#106/#149. **Reject** widening merge/ownership radii as an isolated fix; #106 already reports how that can repeatedly destroy two real tracks.

## 9. Combine genuine repetitions before requiring a perfect callsign

**Why.** Current repetition counts apply to exact accepted strings on ephemeral track IDs. #149 changes that identity, but it does not recover a call when every copy has a different damaged character. The probability that all characters survive a single copy can fall quickly even with modest per-character error; correlated fading makes a simple independent-character calculation optimistic. [M7], [M14], [P149]

**Design.** Keep a bounded callsign lattice for each plausible self-identification episode. Align independent repetitions in element/time space or token-lattice space, allowing insertions, deletions, and uncertain boundaries. Accumulate acoustic evidence for competing complete calls and an unknown/out-of-lexicon hypothesis. Confirm a winner only when supported spans and its margin over alternatives meet calibrated criteria.

Repeated trials of the same samples are one observation. Adjacent PFB channels, different decoder settings, delayed replay, and an image of a signal must not count as additional repetitions. Require nonoverlapping source sample intervals and credible emitter association; cap or temper correlated evidence. An externally known call can be a bounded prior, never a replacement for missing RF evidence. A second sender saying the first sender's call is a new utterance with a different role, not automatically a repetition by the first sender.

**Implementation plan.** Add a local `IdentityEvidenceStore` keyed by source and signal/episode association, with exact span IDs and expiry. Bound memory by time, hypotheses per episode, and active episodes. Use a confusion network or dynamic programming alignment; preserve alternates through `manta-spot` rather than converting immediately to one string. Score SCP membership separately from acoustics; exact unknown calls must still be discoverable. Handle portable prefixes/suffixes through the rank-10 grammar.

**First experiment and gate.** Generate two or three repetitions with complementary corruptions and compare exact-string gating against evidence fusion. Hold out actual callsigns and include a wrong repeated alternative, a nearby station with a similar call, and replayed duplicate samples. Proposed success: ≥20% relative reduction in repetition-attributed misses with no increase in wrong-call confirmations, and no confidence increase from processing the same evidence twice. **Dependencies:** ranks 3/8/10/11, #149. **Stop rule:** if the candidate lattice never contains the correct identity, improve observations/segmentation before expanding dictionary search.

## 10. Parse exchanges and distinguish transmitter, addressee, and mention

**Why.** Current regex context recognition fails valid CQ variants, returns at most one named-family match, and classifies calls without an explicit conversation model. It also treats `<call> T` as a beacon pattern that bypasses repetition. A contest run dominated by Beacon outputs is therefore a diagnostic warning. A high raw callsign recall with incorrect transmitter attribution would pollute RBN. [M6], [M7]

**Design.** Add a streaming finite-state exchange grammar with uncertainty. Recognize CQ families, TEST/contest abbreviations, DE, TU, RST/cut numbers, serials/zones, QRL?, repeats, corrections, prosigns, UP/split hints, and short contest exchanges. Assign every callsign occurrence a role: `self_id`, `addressee`, `relayed_mention`, or `uncertain`. Preserve original text such as `5NN`; normalized exchange fields are derived values with provenance.

The grammar consumes token lattices and sample spans, with bounded local context and expiry. It can recognize a callsign transmitted without immediately adjacent CQ/DE if repeated self-identification and turn structure provide sufficient evidence. A `CQ` far back in a 16-word window must not indefinitely label later turns. Include an open-ended text path so ordinary QSOs and uncommon prefixes are not forced into contest templates. RBN's published keyword/repetition behavior is a comparator policy reference; it does not define all valid CW traffic. [E2]

**Implementation plan.** Replace growing regex exceptions with an explicit parser in `manta-spot`, retaining a compatibility adapter for current `SpotType`. Add full callsign tokenization for prefix-style portable forms and longer special-event identifiers, followed by allocated-prefix validation and acoustic scoring. Bind context to the exact occurrence that supplied it. Require measured power-step structure, appropriate contextual evidence, or explicit operator configuration before automatic beacon exemption; a trailing decoded T alone is insufficient evidence. Represent frequency-coincident turns separately before linking them into a tentative exchange.

**First experiment and gate.** An event-level corpus plus IQ-backed fixtures for `CQ DX`, `CQ TEST`, `TU <self>`, `<other> DE <self>`, `<other> 5NN ...`, split pileups, partial replies, corrections, and ordinary words that look call-like. Label transmitter/addressee independently. Require zero role inversions in the controlled corpus, lower misses on unrecognized self-ID forms, and no increased beacon false positives. **Dependencies:** ranks 1/3/8/9/11; MAN-33 and #90/#133. This remains a receive interpreter; any completed-QSO/logbook export is a separate product/contract proposal.

## 11. Calibrate confidence, measure physical SNR, and explicitly abstain

**Why.** The current geometric confidence combines a character-local beam score, an envelope-derived SNR factor, repetition, and an SCP boost. It is not a measured probability that the complete callsign is correct or that its role is self-identification. Even all-zero character confidence can pass emission. A softmax over surviving Morse paths excludes invalid segmentation and non-CW alternatives, so it can be confident for the wrong reason. [M5], [M7], [M16]

**Design.** Separate four quantities: physical signal/noise estimates; acoustic sequence evidence; probability of the full identity and transmitter role being correct; and the policy decision to publish. Calibrate the last two using held-out locally labeled events, including rejected candidates and difficult negatives. Reliability diagrams, Brier/log loss, precision–recall curves, and risk versus coverage reveal whether an apparent confidence is usable. Temperature scaling or a small monotonic calibrator are candidates, not automatic guarantees; calibration is a separate validated layer. [E9]

Keep a null/no-supported-identity hypothesis in the decision. Inputs can include acoustic margin against alternate calls and null, boundary uncertainty, independent repetitions, source quality, and attribution evidence. Do not let a callsign database overwhelm contradictory sound. Allow `tentative`, `confirmed`, and `abstained` internal states; report the reasons. Retain tentative evidence locally while protecting public output. Calibration and thresholds must be versioned and frozen before held-out evaluation, with drift checks across bands, modes of sending, and receivers.

**Implementation plan.** First add a regression that refuses public emission from the measured zero-evidence case without setting an arbitrary high cutoff on today's uncalibrated score. Then introduce separate internal score fields in `manta-spot::confidence` and a policy seam in `Validator`. Derive signal power from gain-corrected trusted key-down observations and noise power from a robust noise-density estimate with explicit filter ENBW. The present `20 log10(e_hi/e_lo) - 14.3` rail ratio is a stand-in still used in M2, not this physical measurement. Keep missing or unreliable estimates unknown internally. Wire conversion follows the September 6 decision: for the same signal/noise density, `SNR_500 = SNR_2500 + 10 log10(2500/500)`, approximately **+6.9897 dB**. RBN's published explanation uses a 500-Hz reference, not the width of its extraction filter. [M4], [M12], [E11]

**First experiment and gate.** Measure injected known signal/noise powers across channel offsets, receiver gains, occupancy, and fades; proposed calibration target is median absolute SNR error ≤1 dB and 95th percentile ≤3 dB where identifiable. At the fixed public false-identity budget, demonstrate a recall gain or stronger precision bounds, not merely higher mean confidence. **Dependencies:** ranks 1/9/10; accompany permissive rank-4 rollout. **Reject** “more spots” as success if attribution or false-report exposure worsens.

## 12. Carry sample provenance and discontinuities through one replay/live core

**Why.** The source trait exposes neither sample origin nor missing-data events. File replay and live listening have separate orchestration paths. Hardware overflows, UDP loss, resampler startup, filter padding, and delivery backpressure can therefore alter the perceived Morse timing or event order. A stream that loses a dit is not equivalent to a real key-up interval. [M9], [M19], [M20]

**Design.** Introduce an internal source-block contract: receiver/source ID, epoch, first sample index, rate, tuned frequency, valid passband, samples, optional hardware time, and discontinuity status. Distinguish known-length gaps, unknown-length interruptions, retunes, rate changes, temporary no-data, and EOF. Unknown gaps advance an epoch and invalidate continuity-dependent state; do not invent an exact count of silent samples. Keep the monotonic sample clock authoritative for DSP and associate UTC through an explicit mapping with uncertainty. SigMF's sample-indexed capture metadata is useful vocabulary for this contract, not a requirement to replace existing recording formats. [E10]

Build one `EngineSession` state machine accepting source blocks and finalization events. File, audio, Soapy, Kiwi, and HPSDR are adapters. Track frontend delay and resampling phase so an output span maps back to original samples. A stream's actual valid RF region must remain explicit: interpolating Kiwi's narrow input to 96 kS/s does not create a 96-kHz receiver. Audio-derived IQ requires an audio-frequency/RF mapping before external spotting; a default center frequency of zero is not a measured RF location.

**Implementation plan.** Coordinate the input envelope with coppa/other consumers in dispensa if shared; keep an initial Manta-only adapter behind the existing interface. Consume Soapy status/hardware timing when available and validated protocol sequence numbers for HPSDR. Its outer sequence field exists in the received format but current continuity monitoring uses arrival timing; sequence order and wrap behavior need explicit tests. Route gap/retune notifications to PFB, history, track association, timing, and repetition stores. Drain pending observations before track closure. Order events by source/epoch/sample/type/track with declared same-sample ordering, not unsampled metadata given an artificial timestamp of zero or closures sorted to the end of every arbitrary read chunk.

**First experiment and gate.** Run identical IQ in irregular chunk schedules through replay and simulated-live adapters; require identical semantic events and canonical output bytes. Inject dropped, duplicated, reordered, and late blocks, empty polls, rate/frequency changes, and shutdown during a mark. Require explicit loss metrics, no identity spanning an unknown discontinuity, and no accidental lifetime repetition reset from harmless chunking. Exercise slow/disconnected output clients separately from the acquisition clock. **Dependencies:** rank 1; #143/#145/#146 and rank 18. Avoid extending the deprecated single-channel test alone. As of the final remote refresh, #146 is merged; its retry fix is useful but does not add missing-sample provenance.

## 13. Diagnose receiver impairments and condition interference conservatively

**Why.** A decoder cannot undo ADC clipping, an absent RF signal, wrong IQ orientation, or missing samples. Lesser impairments—DC, image leakage, gain pumping, impulses, clock error, and passband coloration—can create false tracks or damage weak keying. The recent manual-gain and doctor work are evidence that input health deserves first-class treatment, not proof of a particular fault in Tony's current receiver. [M9]

**Design.** Add a passive RF-health summary before optional correction: sample validity, repeated/full-scale values, crest factor, DC, conjugate-image correlation, noise PSD versus frequency, gain changes, dropout counts, and valid passband. Distinguish reliable diagnoses from heuristics: mirror energy alone does not prove IQ imbalance because real stations can occupy symmetric frequencies. A correction must be disabled or uncertainty-marked when its assumptions fail.

Evaluate separate reversible conditioners: DC removal with a declared notch width; calibrated IQ imbalance correction from a controlled tone; robust short-impulse suppression with a validity mask; and slow noise whitening/equalization based on signal-free regions. Keep the untouched samples in the bounded history for A/B replay. Do not blank long intervals, auto-notch keyed stations, or let band-wide AGC follow a strong neighbor and modulate every weak station. Clipping requires operator gain/attenuation changes, not a digital claim of recovered information.

**Implementation plan.** Extend `doctor` diagnostics and add optional `InputConditioner` stages in `manta-dsp`. Use stable sample-indexed configuration changes, record correction coefficients, and reset/retrain them across retunes. Annotate every repaired or suppressed interval. For audio, verify the real-to-analytic converter's response, conjugation convention, and group delay using known positive/negative tones; its fixed 48-kHz requirement must be checked at the adapter. For device input, expose requested versus actual gain/rate/frequency and preserve hardware provenance without logging secrets.

**First experiment and gate.** Sweep clipping, DC offset, gain/phase imbalance, slow sample-clock error, impulses, one strong adjacent carrier, and AGC pumping against a clean control. Follow with authorized local receiver diagnostics. Measure recovered identity recall, false images, erased genuine dits, and SNR bias. Require each conditioner to beat bypass on its target impairment without material clean/weak-signal regression. **Dependencies:** ranks 1/12, #103/#143/#145. **Stop rule:** if clipping or analog overload dominates, recommend a receiver configuration experiment; ask before buying equipment or provisioning anything.

## 14. Fit nearby keyed carriers jointly; cancel only when the evidence supports it

**Why.** A weak signal beside a strong one can remain corrupted even with correct centering. Envelope beating can resemble keying and fading. Dit explicitly treats beat-related interference as a distinct case; that is useful diagnostic inspiration, not a general separation solution or performance guarantee. [D1], [D2]

**Design.** Start with a two-emitter local complex model over a small frequency neighborhood. Fit separate carrier/drift, complex gain, pulse shape, and key-state histories plus noise. Compare the evidence for one signal, two signals, and a non-CW interferer. A strong carrier's beat structure may help estimate separation, but similar beat frequencies can arise from unrelated effects; require coherent spectral and temporal support.

Try a two-stage experiment before a full joint decoder: decode a strong signal, reconstruct its *estimated received waveform* including actual edges and gain variation, and subtract it from a separate working buffer. Re-decode the residual only if held-out sample residuals and independent weak-carrier evidence improve. Preserve the original branch. A wrong strong decode or phase estimate can inject Morse-shaped artifacts, so canceling ideal synthetic dits based only on a callsign string is unacceptable. Limit local joint models to two or three emitters initially; combinatorial whole-band separation is not the first implementation.

**Implementation plan.** Add an experimental `CollisionResolver` consuming rank-2 complex observations and rank-8 ambiguity neighborhoods. Reuse the duration search as a conditional key-state estimator; alternate continuous carrier/gain estimation and bounded discrete state search with a fixed iteration budget. Carry reconstruction uncertainty into residual likelihoods. Gate cancellation by posterior predictive checks and stop if total unexplained structured energy increases. No public identity may be supported solely by an artifact appearing after an uncertain subtraction.

**First experiment and gate.** Independent text pairs across separation, relative power, speed, phase, and Watterson seeds; include one always-on interferer, intermittent overlap, and two callers answering the same DX. Compare adaptive filtering, no cancellation, guarded cancellation, and the bounded joint model at equal false-report rate. Plot the separation/power region each solves. Proposed success: ≥20% relative improvement in weaker-emitter identity recall in a predeclared separable stratum, without invented emitters in one-signal controls. **Dependencies:** ranks 2/3/6/8/11. **Reject** deploying broad cancellation if its gain is restricted to exactly generated training waveforms or requires unacceptable unbounded work.

## 15. Spend bounded extra computation on ambiguous episodes and re-decode history

**Why.** Later evidence can reveal carrier frequency, WPM, fist, and repeated identity that were unknown at acquisition. The current streaming commitment cannot generally revisit earlier character boundaries. Uniformly making every decoder more expensive is unlikely to be the best use of the CPU budget.

**Design.** Keep a fast causal baseline plus a deterministic refinement scheduler. Trigger refinement for promising uncertain episodes: a new reliable speed estimate, unresolved callsign alternatives, acquisition failure followed by strong keying, or a collision that has just become separable. Re-run preserved observations under a bounded alternate frontend/timing configuration and attach the result to the same evidence spans. Distinguish refinement of one observation from additional observations; it adds no repetition count by itself.

Expose latency classes: immediate tentative text, delayed confirmed identity, and optional offline exhaustive research. The online path needs an explicit maximum lookback and decision deadline. Start experimentally with a 4–10-second retained history and at most two refinements per episode, then measure. A later repetition may justify a delayed spot, but timestamp it with observed RF time and processing latency; do not pretend the identity was available earlier in a causal comparator run.

**Implementation plan.** Extend rank-5 history with leases and rank-18 revision IDs. Add a scheduler in `manta-engine` whose admission and work budgets are based on deterministic sample/operation counts, not wall-clock races. Faster hardware may finish earlier but must not silently explore a different hypothesis set for a byte-identical replay mode. Maintain a baseline branch for ablations. Process windows in stable source/episode order, reuse coarse observations, and cap retained bytes, search states, and refinement backlog independently.

**First experiment and gate.** Replay difficult short calls with later clean repetitions, carrier drift, and late speed convergence. Report rescued true identities per extra CPU-second, deadline misses, memory, and delayed false reports. Proposed success: ≥15% relative reduction of the remaining locally decodable misses at a measured ≤25% average compute increase on the selected workload; this is an experiment target, not a Pi4 waiver. **Dependencies:** ranks 3/5/7/11/12. **Reject** a win obtained only by allowing unbounded future context or comparing delayed output against a low-latency competitor without disclosure.

## 16. Fuse genuinely different classical observations, not correlated votes

**Why.** Different failure modes can benefit from different observations: noncoherent amplitude survives phase changes; a coherent carrier model can exploit phase; a matched-pulse bank can preserve fast edges. Dit demonstrates explicit routing/fusion infrastructure, but its source does not establish that fusion improves Manta or that normalized confidences are calibrated. [D3]

**Design.** Run a small diverse ensemble only on ambiguous tracks. A useful first comparison is current envelope timing, rank-3 soft duration decoding, and a pulse-matched observation branch. Share raw sample spans, not irreversible binary runs, where diversity requires different frontends. Measure the *oracle union* first: how often is the correct identity present in one branch but absent in the others? If that gap is negligible, fusion has little opportunity.

Align hypotheses by sample spans and edit operations, retaining erasures and boundaries. Fuse acoustic likelihoods or calibrated branch evidence with an explicit dependence penalty; do not majority-vote three configurations of the same mistaken threshold. Recognizer-output alignment and rescoring has primary literature precedent in ROVER, but speech-system gains are not CW evidence. [E7]

**Implementation plan.** Define an internal `DecoderHypothesis` carrying sequence alternatives, spans, score semantics, and frontend/model version. Implement alignment as a bounded library in `manta-decode`, with a simple best-calibrated-branch selector as the baseline. Cross-fit any selector/calibrator on held-out recordings. Add invariants that duplicating an identical branch changes neither the selected call nor evidence strength, and that alignment cannot erase an unknown span to construct a valid-looking call. Never use the branch's claimed confidence as its correctness label.

**First experiment and gate.** Produce branch-error overlap matrices and an oracle ceiling on the fixed corpus. Require meaningful complementary correct identities before implementing fusion; proposed promotion bar is recovering ≥25% of that oracle gap without exceeding the false-identity budget or the declared compute envelope. **Dependencies:** ranks 1/3/11, optionally 2/6. **Reject** permanently running extra branches that agree on nearly everything or using percentile-normalized confidence as a probability of correctness.

## 17. Build a corpus that can falsify the decoder's assumptions

**Why.** Deterministic goldens are essential but cannot represent the deployment distribution. Repeated clean CQ phrases, comfortable signal separation, shared waveform-generation paths, and hand-picked seeds can conceal failure families. The current strong-signal V8 minimum separation is 300 Hz; passing it does not establish close-signal separation. The testkit's real-passband/Hilbert route and receive-side analytic conversion also deserve independent cross-checks rather than a shared assumption validating itself. [M17], [M21]

**Design.** Maintain three layers: tiny analytically checkable primitives; a large deterministic impairment matrix; and licensed/local-only real recordings with locally adjudicated labels. Vary content independently from channel conditions. Include ordinary text, random valid Morse, unseen calls, prefix/suffix portable calls, contest exchanges, prosigns, corrections, truncated bursts, long silence, and non-CW negatives. Separate character acquisition, self-identification opportunities, and emitter-attribution labels.

Cover keying speed/weight/fist, edge shape, drift/chirp, bin phase, multipath delay and Doppler, SNR, colored noise, receiver AGC, clipping, images, missing blocks, occupancy, and power imbalance. Use designed pairwise combinations plus targeted higher-order interactions; do not grow a Cartesian matrix with no diagnostic purpose. Freeze blind seeds and hold out entire recording/day/site/call families. All synthetic truth must identify the actual transmitter, not just every call appearing in its message.

**Implementation plan.** Extend `manta-testkit` with a direct complex-baseband analytical keyer independent of the production Hilbert path and an independently implemented reference for a small subset. Cross-check spectra, envelope durations, total power, reference-bandwidth SNR, and fading autocorrelation before comparing decoded text. Preserve coppa's fixed Doppler/ensemble conventions; the historical upstream Watterson bugs are already fixed, not current explanations. Record generator revision and parameters with every fixture. Add a failure-minimizer that shortens text and removes interferers while preserving a failure, then commits only rights-cleared minimal synthetic reproductions.

Use compact deterministic cases in required CI; publish a clearly labeled scheduled/full acceptance matrix, including currently ignored failing tests, without making the normal green suite imply they pass. Keep real IQ and identity annotations local unless Tony explicitly clears public redistribution. A benchmark manifest can reference hashes without uploading its recordings. Do not use RBN or dictionary identities to generate held-out ground truth automatically.

**First experiment and gate.** Seed this work immediately with rank 1: label a small stratified receiver-local set, then grow to several independent operating sessions. Demonstrate that the benchmark detects known injected parser, timing, image, and duplicate-count defects. A new algorithm must show a paired improvement on both unseen synthetic conditions and real held-out intervals. **Dependencies:** none for corpus foundation; all algorithm ranks depend on its ongoing quality. **Reject** a test expansion whose extra cases only duplicate the existing repeated-CQ distribution.

## 18. Preserve observations separately from public spot delivery

**Why.** A spot-only view loses almost everything needed to understand why a station was missed: uncertain copy, competing calls, role ambiguity, rejected context, and delivery failure. Conversely, permanently retaining every raw sample is unnecessary and potentially sensitive. The proposed dispensa spot contract is a cross-repository boundary, not permission to add arbitrary fields to public JSON. [C1]

**Design.** Separate an internal append-only observation stream from policy-filtered publication. Record source/epoch, stable observation ID, sample/RF intervals and uncertainty, track and episode association, text alternatives/erasures, identity role, acoustic/calibrated scores, rejection reasons, and evidence links. A refinement creates a revision or supersession record rather than silently rewriting history. A public spot references a confirmed identity decision internally; delivery state references that same ID.

Use bounded local retention: summary counters by default, configurable text/trace retention, short IQ history only when explicitly enabled, and deterministic expiry. Raw receive transcripts can contain personal traffic; storage/export needs deliberate policy. Avoid an automatic full QSO logbook or identity enrichment service. Receiver location, operator contact details, and real transcripts do not belong in a public debug bundle by default.

**Implementation plan.** Define private Rust domain types first, leaving the existing telnet/JSON contract unchanged. Add a local opt-in diagnostic sink with versioned schema and stable ordering; design bounded buffering and backpressure separately from the DSP hot path. Distinguish `decoded`, `policy_confirmed`, `queued`, `delivered`, `dropped`, and `client_disconnected`; expose counters so delivery loss cannot masquerade as decoder failure. Define whether restart resumes publication, replays diagnostics only, or intentionally starts a new source epoch. Do not promise exactly-once delivery over telnet; transport acknowledgements and consumer deduplication capabilities differ.

If cqdx needs richer observations, first propose an explicitly versioned companion contract in dispensa `questions/` and `contracts/`, including privacy, retention, uncertainty, dedupe, revisions, and clock semantics. Its current proposed spot schema already includes skimmer-related optional confidence/version/resolution fields; inspect those before inventing replacements. Implementation of a cross-repo interface waits for that agreement.

**First experiment and gate.** Reconstruct every emitted spot and every rejected eligible identity in an audited short recording from local records. Inject queue saturation, client disconnects, restart, and delayed refinement; require bounded memory, explicit loss accounting, no acquisition stall, and idempotent internal revision/evidence identity. **Dependencies:** ranks 1/10/11/12; coordinate dispensa and cqdx only when proposing the interface. **Reject** schema expansion that merely serializes every mutable decoder field or sends raw audio/text externally without authorization.

## 19. Explore receiver or antenna diversity with honest source attribution

**Why.** A deep fade or overlapping same-frequency transmission may remove information that no single-stream decoder can recover. Independently faded receivers or antenna channels can supply genuinely new evidence. This is lower priority because it adds hardware, synchronization, network, privacy, and reporting-policy complexity, and must not hide the single-receiver baseline's weaknesses.

**Design.** Begin with noncoherent evidence fusion across two recorded receivers or antennas: associate plausible same-emitter episodes by frequency/drift, timing, decoded alternatives, and calibrated clock uncertainty. Keep each receiver's evidence and local visibility separate. A station heard only at receiver B cannot count as receiver A's recovered local recall. Distinguish same-location antenna diversity from geographically distributed reception; they have different propagation and attribution implications.

For genuinely synchronized IQ channels, experimentally compare selection combining, gain/noise-weighted combining, and bounded joint likelihoods. Coherent combining requires measured relative delay, phase, clock drift, and channel coherence; never assume NTP-aligned streams are phase coherent. Geographically distant delay/path differences usually make episode-level evidence fusion the more defensible first experiment. Cap correlated contributions from common frontends or shared interference.

**Implementation plan.** Extend rank-12 provenance to multiple sources and rank-9 evidence IDs to independent receiver spans. Keep each source's track and noise state independent, then add an optional association/fusion layer above them. Simulate independent and correlated fading first; use authorized existing recordings for a physical trial. Output an internal `diversity_supported` identity with its exact source support. Any public reporting of a composite receiver or forwarding remote observations requires an explicit ecosystem policy/contract, not impersonating a local measurement.

**First experiment and gate.** At fixed total exposure and false-identity rate, report single-source A, single-source B, their oracle union, and actual fusion. Separate added RF coverage from decoding gain on jointly audible events. Proposed success: recover a meaningful fraction of complementary fades without false cross-station joins. **Dependencies:** ranks 8/9/11/12/18, single-receiver classical acceptance first. **Stop rule:** no hardware purchases, hosted relays, or new public services without Tony's approval; no new receiver is required to complete the first simulated experiment.

## 20. Add learned observations only after proving the classical residual gap

**Why last.** The project explicitly requires classical fading work before M4. The failures above include deterministic data loss, incorrect evaluation, hard boundary commitment, and parsing omissions; a neural decoder would inherit many of them. Dit is useful for identifying train/runtime integration requirements, not for establishing that its model is a drop-in cure. [M12], [D4], [D5]

**Design.** After the classical gates, compare a small causal learned key-state/edge likelihood model against the best classical observation model, keeping the same duration search, identity grammar, calibration, and public policy. This provides an interpretable ablation: did better observations help, or did a language prior merely guess plausible calls? A compact temporal convolutional model is one candidate. Tony explicitly authorizes reuse of HagaleTechnologies code, so Dit's training model is a legitimate experimental starting point; choose receptive field, features, causality, and parameter count from Manta's needs, and validate any reused weights separately. An initial exploratory budget of 10–100k parameters is proposed, not a measured optimum.

Only if observation learning leaves a documented sequence-level gap, evaluate an end-to-end CTC alternative with explicit blank/repeated-character handling, unknowns, and bounded prefix search. CTC provides a sequence-alignment objective; it does not itself establish acoustic calibration or a valid callsign role. Keep an out-of-dictionary path and a pure classical mode. Do not send live RF/transcripts to a hosted model or use a general-purpose language model to “repair” weak calls without source evidence. [E8]

**Implementation plan.** Create independently specified training features and exact runtime parity fixtures covering resampling, frame alignment, normalization, startup padding, missing samples, and quantization. Pin dataset/generator/split hashes and inference versions. Train on diverse independently generated impairments plus authorized labels, with whole recording/callsign holdouts; retain noise-only and adversarial call-like negatives. Choose a portable deterministic inference path and verify cross-platform outputs before adopting it. Model download, paid training, public release, and data export require the appropriate explicit approvals.

**First experiment and gate.** On a frozen blind corpus, compare classical observations, learned observations with the same search, and any end-to-end branch. Require a meaningful paired identity-recall gain at the fixed false-identity budget, no regression on unfamiliar calls and ordinary text, and measured latency/RSS/CPU within the then-approved deployment envelope. Measure calibration after quantization. **Dependencies:** classical D8 acceptance, ranks 1/3/11/12/17, rank 16 only if ensemble complementarity is established. **Reject** aggregate CER gains that increase confidently hallucinated identities, a result that depends on train/test phrase leakage, or a model used to defer known classical defects.

## The common architecture: preserve evidence at every irreversible boundary

The ideas above are not twenty independent rewrites. Implement a few stable internal seams, then compare alternative algorithms behind them. Proposed names below describe responsibilities, not an approved public Rust API.

```text
Source blocks + sample/clock/gap provenance
  → optional reversible conditioning + shared bounded history
  → shared channel analysis → candidate evidence → physical signal tracks
  → recentered complex observations + noise/gain/delay uncertainty
  → key-state and duration hypotheses → text/element lattice + erasures
  → transmission episodes → repeated identity evidence + exchange roles
  → calibrated identity decision
       ├─ bounded local observations / revisions / diagnostics
       └─ public spot policy → existing output adapters + delivery accounting

Bounded refinement reads preserved observations and revises existing evidence;
it never manufactures another independent repetition.
```

| Internal seam | Owning area | Required invariants | Introduced by |
|---|---|---|---|
| `SourceBlock`, `SourceEpoch`, `SampleSpan` | `manta-input`, `manta-engine` | Original time/rate/validity preserved; gaps explicit; UTC not the DSP clock | 12 |
| `ObservationHistory`, `TrackFrontend` | `manta-engine`, `manta-dsp` | Shared bounded storage; known frequency response/delay; no silent phase loss | 2, 5 |
| `SignalTrack`, `TransmissionEpisode` | `manta-engine` | Physical association separate from station identity; closure drains evidence | 8 |
| `KeyEvidence`, `TimingHypothesis`, `TextLattice` | `manta-decode` | Bounded states; stable ties; erasures and alternates retain sample spans | 3, 6, 7 |
| `IdentityEvidence`, `ExchangeRole` | `manta-spot` | No duplicate sample evidence; unknown identity allowed; self/addressee distinguished | 9, 10 |
| `CalibratedDecision`, `PublicationPolicy` | `manta-spot` | Score semantics versioned; acoustic null; explicit abstention | 11 |
| `ObservationRecord`, `DeliveryRecord` | engine/output boundary | Traceable revisions; bounded queues; external schema unchanged until agreed | 18 |

Use raw input sample indices or an exact rational time mapping for resampled spans; rounding independently at each stage can move boundaries. Floating-point algorithms need deterministic reduction order and explicit tie behavior. Stable source/epoch/track IDs and canonical serialization are part of correctness, not deferred cleanup. Cross-platform byte identity is the repository requirement; a fast nondeterministic research mode must be labeled and cannot silently replace it.

Avoid globally changing `HOP_MS` as a way to enable higher track rates. The present supported PFB configurations intentionally yield 375-Hz streams; a new frontend must convey its own rate through timing/metadata/tests. Likewise, a “better frequency estimate” does not by itself change the filter that produced the envelope, a “better WPM report” does not prove better character segmentation, and a “higher confidence” does not prove more accurate calls.

### Suggested implementation sequence and first claims

This is a dependency-aware sequence, not a demand that one session implement all twenty. Each row should produce a small claimable PR or a measured experiment with a rejection result.

| Phase | Work package | Deliverable and exit evidence |
|---|---|---|
| A: establish truth | Rank 1 + minimal 17 | Correct scorer; audited local mini-corpus; stage funnel; pinned baseline with passing and failing tests shown separately |
| A: stop irreversible losses | Rank 5 + first part of 12 | Lifecycle/prefix/EOF/chunk regressions, evidence-span identity, pending observations drained before closure |
| A: repair semantic precision | Rank 10 regressions + minimal 11 | CQ DX/multiple-context/portable tests; beacon and zero-evidence protections; tentative local diagnostics |
| B: frontend experiment | Rank 2, then 4/8 | Measured edge-response and two-signal curves; candidate/active accounting; current path retained for A/B |
| B: sequence experiment | Rank 3 with known speed, then 6/7 | Short-trace exhaustive oracle; V6 → V5 → V8w progression; explicit unknown spans |
| C: identity recovery | Ranks 9/10/11 with 18 | Complementary-repeat recovery, role accuracy, calibrated public policy, no reused evidence |
| C: complete streaming semantics | Ranks 12/13 | Loss injection, source health, file/live equivalence, actual receiver validation when available |
| D: spend complexity where justified | Ranks 15/16, then 14 | Measured rescue per extra compute; branch-error complementarity; separable collision region |
| E: generalize and exceed | Expanded 17, paired CW Skimmer comparison, eventual CPU/soak gates | Independent held-out performance and operational evidence; no claim based on a global RBN union |
| F: optional new evidence | Rank 19; M4-gated 20 | Source-aware diversity or a verified residual learned-observation gain, with required approvals |

The first five suggested implementation claims are: **(1)** scorer accounting/time matching and local reference model; **(2)** pending-track finalization plus short-stream regressions; **(3)** exact context/role/erasure regressions; **(4)** complex-frontend channel-offset experiment; **(5)** bounded soft-duration search with a tiny exhaustive oracle. Coordinate #144/#149/#96 and their successors before claiming overlapping files. A source-semantics or schema proposal goes to dispensa first if shared.

For each experiment, attach: hypothesis; exact code/data/config revisions; one primary metric and false-report constraint; stratified results; paired uncertainty; CPU/RSS/latency; newly failing cases; and a promote/revise/reject decision. Do not tune and evaluate on the same held-out file. Preserve failed experiments as short decision records and regression fixtures, not undocumented constants or paragraphs appended to CLAUDE.md.

## What Dit contributes—and what must not be inferred from it

This review inspected Dit source at the pinned revision, not only its wiki. No Dit runtime benchmark or model evaluation was executed. Its single-audio-signal application and platform-specific ML stack are not a wideband Manta architecture.

| Observed Dit behavior | Independent Manta lesson | Limitation / do not transfer |
|---|---|---|
| `DitDecoder` has BPF/AGC/Goertzel processing, optional beat-aware keying, interference subtraction, and learned keying paths. [D1] | Separate frontend impairment diagnoses and route observations explicitly; preserve bypass paths. | No evidence here that the combined system is more accurate than Manta or suitable for hundreds of signals. |
| `BeatFrequencyDetector` analyzes envelope beat behavior; subtraction is conditioned on inferred interferer-only intervals. [D2], [D6] | Test a specific two-carrier explanation for beat-damaged keying; validate reconstructed residuals. | Beat structure is ambiguous under fading; copying thresholds or subtracting magnitude spectra is not a general solution. |
| Fusion keeps temporal result buffers and normalizes decoder confidence by trailing percentiles before weighted selection. [D3] | Compare complementary errors; align outputs and evaluate a calibrated selector. | Percentile ranks are not correctness probabilities. Wall-clock alignment is inappropriate for Manta's deterministic sample-time replay. |
| The inspected CTC wrapper expects `[1,125,80]` features and 39 output classes; older prose describes different shapes. [D4] | Treat the executable feature/model contract as authoritative and pin it. | Do not port a stale model shape, assume an installed asset matches a wrapper, or treat design scorecards as benchmark output. |
| Training/runtime feature parity has explicit tests for normal and resampled fixtures. [D5] | Verify each normalization/resampling/frame convention end to end. | Passing feature parity proves matching transforms, not accurate decoding or a suitable Manta model. |

No comparative performance number from Dit is used to rank Manta's expected gains. The review informs failure hypotheses and integration discipline. Manta's own controlled experiments decide whether any analogous behavior is useful.

### Authorized Dit reuse: concrete candidates and required adaptation

Tony clarified during this investigation that **code from HagaleTechnologies repositories, including Dit, may be copied and adapted**. That supersedes the initial no-copy restriction for those owned components. Reuse is optional; it does not lower Manta's correctness, cross-platform, or M4 gates. No code was ported in this documentation-only session.

| Candidate at the pinned Dit revision | Reuse plan | Mandatory checks before adoption |
|---|---|---|
| `Dit/SignalProcessing/BeatFrequencyDetector.swift` [D2] | Port the envelope-modulation estimator and candidate-stability logic into a rank-14 experimental Rust observation branch. Use existing FFT facilities; make sample rate explicit and reuse scratch allocation. | Retune physical window duration and frequency bounds for Manta's stream rate. The Swift implementation assumes the configured audio rate; it is not directly applicable to a 375-Hz envelope. Test keying-harmonic/QSB false detections and close-pair controls. |
| `Dit/SignalProcessing/SpectralSubtractor.swift` [D6] | Reuse the key-up-conditioned interference-estimate concept and, if useful, port its numerical baseline for comparison. | Do not transplant its streaming wrapper unchanged: it passes through input before a full frame, then returns only the chunk-sized suffix of the processed frame for small chunks. Build a sample-conserving queued/overlap-add path with explicit latency and arbitrary-chunk tests. Mean window-gain compensation alone is not perfect reconstruction. |
| `Dit/MLTraining/train_keying_detector.py` plus `Dit/CWProcessing/MLKeyingDetector.swift` [D7], [D8] | Reuse the Python loader/training scaffold and model as an M4 baseline; replace CoreML-only integration with the chosen portable runtime and Manta-generated observations. | Fix the observed evaluation hazards first: overlapping windows are randomly split after extraction; training normalizes over the whole recording while inference uses cumulative prefix statistics; convolutions use symmetric padding while runtime consumes the last output. Split by recording before windowing, match causal normalization exactly, and validate the deployed output position/context. No trained performance conclusion was measured here. |
| `DitTests/FeatureParityTests.swift` and `Dit/MLTraining/generate_parity_vectors.py` [D5], [D9] | Adapt the fixture-driven Python/runtime parity workflow; reuse owned synthetic cases where their assumptions match. | Derive tolerances for Manta instead of inheriting them. Dit currently permits 0.025 maximum feature difference for normal fixtures, 0.08 at 48 kHz, and 0.45 at 44.1 kHz; these are not a universal parity standard. Test shape, each dimension, time alignment, and warmup, not just a zip over matching prefixes. |
| `Dit/MLDecoder/DecoderFusionEngine.swift` [D3] | Reuse useful buffering/alignment scaffolding in rank 16 if it saves work. | Replace wall-clock timestamps with sample spans, percentile confidence with validated evidence semantics, and implicit repeated-observation credit with deduplicated evidence IDs. Keep a measured single-branch selector baseline. |
| `Dit/CWProcessing/GoertzelEnvelopeExtractor.swift` [D10] | Use its training-through-real-frontend pattern; the extractor can serve as an owned comparison frontend during experiments. | Its default 512-sample frames at 48 kHz yield 93.75 frames/s. Do not relabel these as Manta's 375-Hz samples or reuse corresponding labels without delay/rate conversion. Existing AGC/BPF assumptions must be measured on wideband-derived tracks. |

These are candidate reuse boundaries, not an instruction to make Manta depend on Swift, Accelerate, CoreML, or the Dit application. Track upstream revision and local adaptations. Shared reusable primitives should live in the appropriate common crate only after cross-repo agreement; avoid two subtly divergent copies with no provenance. Owned source permission does not automatically clear third-party dependencies, downloaded recordings, or externally sourced training assets.

## Clean-room boundaries for external material and provenance for owned reuse

This document is a **source-informed narrative handoff, with explicitly authorized reuse of HagaleTechnologies code**. For third-party implementations the boundary remains behavioral/mathematical description and independently written Manta code. It is not a claim of a formally isolated two-team clean room: the researcher inspected the named repositories, and those exposures are disclosed. No proprietary CW Skimmer implementation, binary internals, or decompiled code was examined. Its public product description and RBN's published operating/SNR explanations were used as behavioral references. CW Skimmer publicly describes Bayesian decoding; that says nothing about its undisclosed architecture or whether a particular Manta proposal will outperform it. [E1], [E2], [E11]

The handoff contains no copied foreign implementation, weights, source patches, or transferred training set. Manta source was inspected and exercised as the subject of the review; Dit was read for observable design patterns, not translated. Mathematical recurrences and conceptual type names above are explanatory designs, not imported implementations.

Implementation sessions should:

1. Work from the requirements, equations, tests, and invariants here. Copy/adapt owned HagaleTechnologies components when useful, recording provenance and required changes; derive third-party-inspired code independently rather than translating an external source file line by line.
2. Keep a provenance note for any external algorithm or behavior considered. If stronger formal clean-room isolation is required for third-party work, keep implementers unexposed to that source and have a separate reviewer vet the narrative and tests; this document alone does not establish that process.
3. Generate new tests with independent signals and content. Owned synthetic fixtures can be reused; do not assume downloaded model weights, recordings, or corpus annotations are owned merely because they are stored beside owned source.
4. Keep Manta's MIT OR Apache-2.0 licensing intact and preserve applicable notices. Record the source revision for owned reuse and review third-party dependency/asset provenance before transfer.
5. Preserve data boundaries. Local real IQ and third-party repository data were not uploaded as research artifacts; the committed document uses synthetic examples and aggregate measurements. Ask before public data, new services, paid training, equipment, or deployment.

### Tempting changes to avoid

- **One magic threshold:** lowering onset, raising the track cap, extending hang time, or adding SCP entries can inflate spots without recovering correct local identities.
- **Sigma-only “beam improvement”:** with fixed marks and a common sigma, the scale factor cancels in exact dit/dah ranking. Actual f32/pruning behavior is not invariant: a measured grid changed **112 of 3,125** decoded glyphs between sigma 0.15 and 0.50. This is not measured accuracy improvement; investigate tie/rounding behavior and normalized scoring before interpreting it as robustness. See verification below.
- **AGC that only rescales both rails:** a common normalization may barely change threshold decisions. Measure transition evidence, not normalized envelope appearance.
- **Always narrower filtering:** improved noise rejection can erase timing sidebands and worsen short dots. Evaluate matched speed/filter response and neighbor rejection together.
- **Dictionary autocorrection:** plausible popular calls are not necessarily on the air. Preserve unknown calls and an acoustic null hypothesis.
- **Blindly increasing repetitions:** exact-string repetition can reject every imperfect copy and disproportionately lose short callers; support complementary evidence instead.
- **A word-level language model first:** it can make ordinary text prettier while inventing identities or confusing addressees with transmitters.
- **Treating reported negative experiments as impossibility proofs:** #106's unsuccessful scalar changes do not rule out joint timing/gain inference. In particular, a cited 0.32-second fade coherence scale is longer, not shorter, than a 54-ms dit; that comparison cannot support a “faster than every dit” argument.
- **Claiming victory from green default CI:** the ignored V2/V5/V6 failures are real and V8w remains an explicit acceptance gap. Also retain the outstanding hardware CPU and live-soak requirements.

## Verification record and reproduction recipes

No decoder, input, test, or contract implementation was changed for this research. Only this handoff and a wiki pointer are intended repository changes. The source probes were isolated scratch programs using public Manta APIs; neither probe nor the real recordings are required additions to the repository.

### Executed checks

Environment: arm64 macOS, Rust **1.96.1 (31fca3adb, 2026-06-26)**; Manta source baseline `45cf1144…`, workspace lockfile preserved. Build output stayed on local disk. The first sandboxed test attempt failed on three local socket binds with permission errors; the same workspace suite rerun with local networking permission completed successfully.

```sh
cargo test --workspace --locked
# Exit 0: 466 passed, 0 failed, 9 ignored, across 39 result groups.
# Includes the ordinary 120-second CI soak test, not a 24-hour live-SDR soak.

cargo test --locked -p manta-cli --test golden_v2_v3 -- --ignored
# Exit 101: 0 passed, 3 failed (V2, V5, V6), 2 filtered out.

cargo fmt --all --check
git diff --check
# Both passed.
```

These commands were run using the installed cargo binary and an explicit local `CARGO_TARGET_DIR`; the abbreviated commands above are portable reproductions. The full workspace result is not “all acceptance green”: nine tests are ignored by its normal selection. No new ignored tests were introduced. Hardware Soapy/HPSDR feature tests, a live receiver soak, Raspberry Pi CPU measurements, Dit runtime/model tests, and paired CW Skimmer measurements were **not** executed in this investigation.

### Small measured probes

Reimplement these probes as focused regression tests during the relevant investigation; their configurations below make the observations independently checkable without relying on a temporary local script.

| Probe | Exact setup / method | Observed result |
|---|---|---|
| Scorer counting | Load `scripts/score-against-rbn.py` from PR #144 revision `7dee010b…`; one truth W1AW at 7,020,000 Hz, three identical matching output dictionaries, tolerance 500 Hz; invoke its `dedup_truth` and `match`. | Three TP output occurrences over one truth key: **300% reported recall**, **100% unique coverage**. |
| Reference population | Parse the committed `crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv` at that same PR revision using CSV headers, excluding the header from row count; count distinct `callsign` source labels. | **26,802 rows; 177 spotter labels**. |
| Prototype response | Call production `design_prototype(1024, 8)`; compute `H(f) = Σ h[n] exp(−j 2π f n / 96000)` in f64 from returned f32 taps; evaluate `10 log10(|H|²)`. | Offsets 0, 23.4375, 46.875, 70.3125, 93.75 Hz: **−0.0000, −0.0620, −6.0219, −43.1217, −82.0667 dB**. |
| Prototype width/delay | `ENBW = 96000 Σ h[n]² / (Σ h[n])²`; support `L/96000`; linear-phase delay `(L−1)/(2×96000)`. | **82.2317 Hz**, **85.3333 ms**, **42.6615 ms**. |
| Beam scale sensitivity | Enumerate all five-mark sequences from durations {28,45,73,119,194} ms; call `decode_char` with dit=60 ms, dah=180 ms, q=1, width=4; compare glyph for sigma 0.15 versus 0.50. | **112 / 3,125 glyphs differ**. First enumeration difference [194,194,45,194,28] chooses AR prosign versus 7. This has no ground-truth correctness label. Floating-point ties/pruning are a hypothesis for the mathematically unexpected sensitivity, not a proven cause. |
| Context recognizer | Call `manta_spot::context::parse` directly. | `CQ DX W1AW` → none; `CQ TEST W1AW` → W1AW/CQ; `DE W1AW DE K1ABC` → only W1AW; `K1ABC W1AW UP` → W1AW/DE. |
| Grammar | Call `grammar::is_plausible`. | W1AW=true; F/W1AW=false; W1AW/P=true; 3DA0AA=true; LZ2026TEST=false. |
| Zero confidence | `Validator::bundled(96000)`; track 1 metadata SNR2500=10 dB, RF=7,020,000 Hz; ingest letters of CQ, W1AW, W1AW, all confidence=0, each followed by a WordBoundary; increment timestamps 10,000 source samples per event. | One W1AW spot, confidence **1.02818056×10⁻⁷**, WPM=0. Synthetic evidence, not a real on-air observation. |
| Short initialization | Default `TrackDecoder`, 300 envelope samples at 375 Hz, alternating 20-sample runs of 1.0 and 0.001, source timestamps `i×256`; then `finish()`. | **Zero emitted events**; finite stream never completed initialization. |

The beam/prototype probes use actual Manta modules, not a reimplementation of the production decision. The scratch probe dependency resolution was separate from the workspace lockfile; the reported workspace/golden test commands used the checked-in lockfile. Preserve compiler/platform details when exploring numerical tie sensitivity.

The final remote refresh during writing found `origin/main` at **`18b767c`**, whose only change since the measured source baseline is the merged Soapy overflow retry fix (#146). The document deliberately keeps its measured baseline pinned rather than presenting these observations as a fresh benchmark of every concurrent branch. Re-fetch and inspect the new main and active claims before implementation.

## Source inventory

Repository links are commit-pinned except explicitly identified PR status/history. External primary sources were consulted on 2026-09-09; their statements supply established methods or documented behavior, not evidence of Manta's proposed performance. Link labels in the body resolve directly to the supporting file/page.

- [M1] — Historical MAN-166 scorer, including deduplication and matching.
- [P144] — Reported benchmark baseline and recording/RBN context; numbers attributed, not rerun.
- [M2] — Production PFB prototype design.
- [M3] — Detector defaults, channel selection, association, pool lifecycle, event draining.
- [M4] — Rail initialization/adaptation and stand-in SNR.
- [M5] — Character-local beam, scores, and invalid alternatives.
- [M6] — Context recognizers, priority, and beacon fallback.
- [M7] — Candidate validation, repetition, confidence, and emission.
- [M8] — Structural callsign restrictions.
- [M9] — Source trait and WAV adapter; audio/kiwi/soapy/hpsdr adapters are adjacent files in the same directory.
- [M10] — Deprecated-path determinism coverage; compare adjacent channelizer_chunking_determinism.rs.
- [M11] — V2/V3/V4/V5/V6 definitions and assertions.
- [M12] — Normative D3 SNR, D6 CPU sequencing, and D8 classical fading decisions.
- [M13] — Timing and spacing estimation.
- [M14] — Baseline exact repetition gate.
- [M15] — Initialization replay, mark bootstrap, gap commitment, erasures, finish.
- [M16] — Geometric confidence, epsilon floor, repetition factor, SCP boost.
- [M17] — Synthetic multisignal and real-passband/Hilbert generation.
- [M19] — File replay orchestration.
- [M20] — Live orchestration and calibration.
- [M21] — Pinned golden-vector scenes and parameters.
- [P106] — Reported V8w baseline and failed classical parameter experiments.
- [P149] — Concurrent repetition identity/cap work; inspect current head before implementation.
- [D1] — Dit frontend routing and interference/keying paths.
- [D2] — Beat estimator and candidate-stability logic.
- [D3] — Fusion buffers, time association, confidence normalization, and selection.
- [D4] — CTC wrapper feature/output contract.
- [D5] — Actual parity fixtures and thresholds, including resampling.
- [D6] — Interference estimation and streaming reconstruction behavior.
- [D7] — Training model, full-recording normalization, windowing, and split.
- [D8] — Runtime keying model, cumulative normalization, and fallback.
- [D9] — Feature-parity fixture generator.
- [D10] — Training frontend and envelope rate.
- [C1] — Proposed shared spot schema; read questions/0028-skimmer-spot-stream-contract.md at the same revision.
- [E1] — CW Skimmer official product description; behavioral claims only.
- [E2] — RBN: How to get spotted; published recognition/spotting behavior.
- [E3] — Fldigi author documentation: CW filter configuration and matched filtering.
- [E4] — Johnson and Willsky, The Hierarchical Dirichlet Process Hidden Semi-Markov Model, UAI 2010 / arXiv 2012. Explicit durations and a Morse example; not an implementation template.
- [E5] — ITU-R F.1487, HF modem testing/channel-simulation reference.
- [E6] — ITU-R M.1677-1, International Morse code.
- [E7] — Fiscus, 1997, ROVER recognizer-output alignment/combination paper, NIST publication record.
- [E8] — Graves et al., 2006, Connectionist Temporal Classification, original paper.
- [E9] — Guo et al., 2017, On Calibration of Modern Neural Networks. Calibration methodology reference; proposed application to Manta must be validated.
- [E10] — Official SigMF specification: sample-indexed capture metadata; vocabulary reference, not a mandated Manta output format.
- [E11] — RBN, 2014, Understanding Signal-to-Noise Ratio; authorized account of CW Skimmer/SkimServ measurement from its author.

[M1]: https://github.com/HagaleTechnologies/manta/blob/7dee010b946798b434f4c5605943139e7bef3d2e/scripts/score-against-rbn.py
[P144]: https://github.com/HagaleTechnologies/manta/pull/144
[M2]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-dsp/src/proto.rs
[M3]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-engine/src/track.rs
[M4]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-decode/src/envelope.rs
[M5]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-decode/src/beam.rs
[M6]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-spot/src/context.rs
[M7]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-spot/src/validator.rs
[M8]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-spot/src/grammar.rs
[M9]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-input/src/lib.rs
[M10]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-engine/tests/chunking_determinism.rs
[M11]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-cli/tests/golden_v2_v3.rs
[M12]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/docs/DECISIONS/2026-09-06-broad-review-decisions.md
[M13]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-decode/src/timing.rs
[M14]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-spot/src/gate.rs
[M15]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-decode/src/decoder.rs
[M16]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-spot/src/confidence.rs
[M17]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-testkit/src/scene.rs
[M19]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-engine/src/lib.rs
[M20]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-engine/src/listen.rs
[M21]: https://github.com/HagaleTechnologies/manta/blob/45cf11444979e9c1d48aa6f90164aea7b7c3b695/crates/manta-testkit/src/vectors.rs
[P106]: https://github.com/HagaleTechnologies/manta/pull/106
[P149]: https://github.com/HagaleTechnologies/manta/pull/149
[D1]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/CWProcessing/DitDecoder.swift
[D2]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/SignalProcessing/BeatFrequencyDetector.swift
[D3]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/MLDecoder/DecoderFusionEngine.swift
[D4]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/MLDecoder/MLDecoderModel.swift
[D5]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/DitTests/FeatureParityTests.swift
[D6]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/SignalProcessing/SpectralSubtractor.swift
[D7]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/MLTraining/train_keying_detector.py
[D8]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/CWProcessing/MLKeyingDetector.swift
[D9]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/MLTraining/generate_parity_vectors.py
[D10]: https://github.com/HagaleTechnologies/dit/blob/2e771fa60ad16dd384a4e4a66e4005a67b70d02a/Dit/CWProcessing/GoertzelEnvelopeExtractor.swift
[C1]: https://github.com/HagaleTechnologies/dispensa/blob/0ec48ad9cd7c44428c1de00e85f3d1ef6fe167c0/contracts/spots/spots.v1.schema.json
[E1]: https://www.dxatlas.com/CwSkimmer/
[E2]: https://www.reversebeacon.net/pages/How+to+get+spotted+by+the+RBN+44
[E3]: https://www.w1hkj.org/FldigiHelp/cw_configuration_page.html
[E4]: https://arxiv.org/abs/1203.3485
[E5]: https://www.itu.int/rec/R-REC-F.1487/en
[E6]: https://www.itu.int/rec/R-REC-M.1677-1-200910-I
[E7]: https://www.nist.gov/publications/post-processing-system-yield-reduced-word-error-rates-recognizer-output-voting-error
[E8]: https://www.cs.toronto.edu/~graves/icml_2006.pdf
[E9]: https://proceedings.mlr.press/v70/guo17a.html
[E10]: https://sigmf.org/
[E11]: https://reversebeacon.blogspot.com/2014/03/understanding-signal-to-noise-ratio-snr.html
