# MAN-206: an independent synthetic generator checks the testkit, and failing scenes shrink to synthetic repros

Date: 2026-11-03. Ticket: MAN-206 (decoder recall research, investigation
17: `docs/DECISIONS/2026-09-09-decoder-recall-research.md`).

## Problem

`manta-testkit` generates every golden vector, and the decoder is tested
against what it generates. Two pieces of shared code meant a bug in both
would pass every vector: the keyer encodes text with the decoder's own Morse
table (`manta_decode::tree::pattern_for`), and the faded route reuses
`manta_dsp::hilbert::HilbertTransformer`. Nothing checked either route
against an outside reference. Separately, there was no tooling to shrink a
failing scene before debugging it.

## What landed

- `manta_testkit::baseband`: an independent complex-baseband generator. Its
  own ITU-R M.1677-1 Morse table, an f64-seconds timeline, `sin²` edges, an
  index-based carrier, a sum-of-sinusoids two-path fader and Marsaglia-polar
  noise. A source-text lint test (`independent_modules_import_no_production_code`)
  rejects any mention of `crate::keyer`, `crate::scene`, `crate::noise`,
  `crate::vectors`, `crate::u01`, `crate::gaussian_pair`, `manta_decode`,
  `manta_dsp`, `coppa_channel` or `super::` before the module's tests, in
  `baseband.rs` and `measure.rs`.
- `manta_testkit::measure`: independent estimators (radix-2 FFT, Welch PSD,
  band power, image, on-runs, cross-correlation group delay, 2500 Hz SNR,
  overlap-normalized fading autocorrelation, ensemble power), each checked
  against a closed form.
- `manta_testkit::crosscheck`: the gate. `verified()` runs the compact gate
  once per process and returns `Ok(&Verified)` or `Err(&CrossCheckReport)`;
  the report prints dimension, result, measured value, tolerance and case.
  `independent_scene` and `independent_tone` take `&Verified`, so code
  cannot decode an independent scene before the gate passed.
- `crates/manta-engine/tests/independent_generator.rs`: decodes five
  vectors (V1, V2, VR4, VW1, V7; 30 s each) from both generators after the
  gate passes. Measured: identical keyed text, identical spot sets (`W1AW`,
  `JA1ABC`, `G3ABC`, `K1ABC`, `N1AA`+`N2BB`), CER 0.0213, 0.0125, 0.0114,
  0.0000, 0.0161 for both.
- `crates/manta-engine/tests/receive_side_hilbert.rs`: `AudioIqSource`
  (511 taps, 48 kHz) against `z` of an independent tone delayed 255 samples
  at 750, 1500, 3000, 12000 and 20000 Hz. Measured: image −98.6 to −114.0
  dBc (bound −70), power within 0.0001 dB (bound 0.01), equal on-runs.
- `scene::render_signal`, `render_signal_with_route` (`Route::{Direct,
  RealPassband}`) and `mix`: `render_scene` is now "each signal's unscaled
  contribution, then mix". Byte-safe: six direct-route digests and two
  faded-route digests (x86_64 Linux) computed before the split are pinned in
  `scene.rs` tests, and `existing_vectors_render_unchanged` keeps its value.
- `manta_testkit::scenespec::SceneSpec`: a synthetic-only scene description
  (`origin` has one variant, `synthetic`; no constructor from samples or
  files; unknown fields rejected). Floats are strings, so all 26 vectors
  round-trip bit for bit.
- `manta_testkit::minimize::minimize` and `manta_testkit::repro`: the
  minimizer, the committed-repro format, its digest and its lint.
- `crates/manta-engine/tests/repros/v8w-near-dc-mirror-ghost.json`
  (1117 bytes) with `README.md`, replayed by `synthetic_repros.rs`.

## Decisions

| # | Decision | Reason |
|---|---|---|
| D1 | "Production Hilbert path" means both the generation-side route (129 taps, `scene.rs`) and the receive side (`AudioIqSource`, 511 taps). The gate covers the first; `receive_side_hilbert.rs` covers the second. | Investigation 17 names both, and the two designs differ by 20 dB or more below 2 kHz. |
| D2 | Independence means no code shared with the production generator or the decoder. External crates are allowed. The jitter recipe (ChaCha8, two `next_u64` per segment, Box–Muller cosine value, clamp ±3σ) is reimplemented from its written description. | Two implementations agreeing confirms the recipe; supplying timings from `keyer.rs` would share the code the ticket worries about. |
| D3 | The gate's ordering is enforced by a type (`Verified`), not by test order. | MAN-101: an earlier failing assert silently disables later checks; a token cannot be skipped. |
| D4 | Tolerances are named constants in `crosscheck.rs`; a tolerance changes only with a line here. The target was 2× the measured disagreement. Most rows hold it; noise whiteness (1.5×), ensemble mean (1.6×) and ensemble standard error (1.4×) do not, nor do the full sweep's worst fading and ensemble rows (1.5× and 1.6×). Averaging went up once, when the Good ensemble standard error broke its ceiling at 32 seeds (now 48), and was not raised further to restore 2×. | Keeps the gate discriminating. Five negative controls prove it fails on real defects. More seeds would lengthen the compact gate, which three test processes run. |
| D5 | The 129-tap route's verified domain is \|offset\| ≥ 2 kHz at 96 kS/s. `faded_signals_inside_the_verified_domain` pins the shipped exceptions (26 vectors plus the sensitivity series) to exactly `{v8w s09}`. | One shipped exception; a new near-DC faded signal fails loudly. |
| D6 | The independent fader is a sum of 256 complex sinusoids per path (Doppler from N(0, (spread/2)²), uniform phases), evaluated every 1 ms by rotating phasors re-anchored exactly every second, linearly interpolated; two equal-power paths with typed ITU-R F.1487 delays. Compared with coppa on statistics only. | A different construction is what makes agreement evidence. 64 sinusoids (the research prototype) left Poor τ = 0.5 s at 0.260 against theory 0.291 with these seeds (error 0.032 > 0.03); the estimator's standard error at M = 64 × 32 seeds is 0.014, so this was variance, and M went to 256. |
| D7 | The minimizer runs on `SceneSpec` only. | "Synthetic only" is true by construction. |
| D8 | Scene floats serialize as Rust shortest round-trip strings. | A plain JSON number changed 44 of 381 shipped values' bits (research R8). |
| D9 | A step is accepted only when the probe returns exactly the original signature; decode errors map to `decode-error`. | One failure per repro; shrinking can turn one failure into another. |
| D10 | Pass order per round: duration (halve, floor 2 s), signals (ddmin, keep ≥ 1), text (ddmin over words, then characters), impairments (drop jitter, QSB, Watterson, Farnsworth, then standard timing). Rounds repeat until one accepts nothing or the budget (default 200 probes) is spent. | Decode cost scales with duration, so shortening first makes later probes cheaper. |
| D11 | Contributions are cached by (signal JSON, duration) and mixed with `scene::mix`, which `render_scene` uses. | `probe_sees_the_bytes_scenespec_render_produces` asserts the probe sees `SceneSpec::render`'s exact bytes. |
| D12 | A repro is JSON (schema, origin, name, `found_in`, generator revision, `expect`, probe, signature, per-target digests, scene) under `crates/manta-engine/tests/repros/`, replayed in required CI. `expect: "fails"` passes while the failure is present and fails once fixed. | A fix cannot slip by without updating the repro. |
| D13 | Digests are FNV-1a over f32 bits keyed `"{ARCH}-{OS}"`; replay checks the running target's entry when present. | coppa's FFT is SIMD-dispatched. |
| D14 | The lint rejects files other than `*.json` and `README.md`, JSON over 8 KiB, unknown fields, and `name`/`found_in` containing `.wav`, `.cf32`, `.ci16`, `audio-corpus`, or a file name or sha256 from `audio-corpus/manifest.json` (embedded with `include_str!`). | "Never real recorded IQ" is a CI check. |
| D15 | Required CI minimizes a 120 s scene of V8w `s09` plus V8 `s00`..`s03`; the 50-signal V8w minimization is `#[ignore]`d. | Required CI runs on three OSes without `--ignored`. |
| D16 | The repro is committed with `expect: "fails"` and signature `["mirror:s09"]`. | It documents a real generator defect and keeps it visible until fixed. |

## Declared tolerances and what this run measured

Domain: fs 96 kS/s for keying and spectra (8 kS/s for fading); \|offset\| ≥
2 kHz for the 129-tap route; scenes of 3 to 60 s. Measured on x86_64 Linux,
`cargo test` profile; the compact gate takes about 15 s.

| Dimension | Cases | Tolerance | Measured (worst) |
|---|---|---|---|
| Morse table | 48 ITU characters (`+` excepted: manta keys `.-.-.` as AR) | 0 mismatches | 0 |
| Envelope | V1 loop 10 s; 35 WPM 8 % jitter loop; Farnsworth 15/25; weight 2.6, gaps 2.5/5, dit 0.8, element gap 1.2, 0.5 ms, jitter; `key_text` once | on-run count and keyed text equal; start/duration ≤ 1 sample; magnitude ≤ 1e-6 | equal; equal; 0 samples; 1.5e-7 |
| Total power | direct +12340, −8200, V6 QSB at 9100; 129-tap +12340, +5600, +2000, −8200 Hz; one direct +12340 Hz scene through both scene renderers (`HEADROOM` vs `MASTER_SCALE`) | ≤ 0.01 dB | 8.6e-4 dB (129-tap, 2 kHz); scene scale 3.7e-8 dB |
| Spectra | same pairs, Welch 2^14 | main band ≤ 0.01 dB; per bin within 60 dB of peak ≤ 0.1 dB | 8.6e-4 dB; 0.011 dB (129-tap, 2 kHz) |
| Image | same | ≤ −75 dBc | −80.1 dBc (129-tap, 2 kHz) |
| Group delay | 129-tap route | exactly 64 samples | 64 |
| SNR (2500 Hz) | 20 and 0 dB requests, 3 s, each generator's own noise | ≤ 0.3 dB of request; ≤ 0.2 dB between | 0.039 dB; 0.029 dB |
| Noise whiteness | both generators, 96 bands of 1 kHz | ≤ 0.5 dB of the mean | 0.32 dB |
| Fading autocorrelation | Poor 0.1/0.2/0.5 s, Good 0.5/1/2 s; 48 seeds × 60 s at 8 kS/s, 2 kHz | ≤ 0.03 vs theory; ≤ 0.03 between | 0.0041; 0.0074 |
| Ensemble power | same | \|mean − 1\| ≤ 4 SE; SE ≤ 0.06 | 2.4 SE (Poor independent, mean 0.971, SE 0.012); SE 0.044 (Good) |

Envelope start/duration is a whole-sample count, so 1 sample is the
smallest non-zero tolerance; a re-run during remediation measured 0 in
every case (an earlier draft of this table said 1, Farnsworth). The scene
scale row is the only one that compares `HEADROOM` with `MASTER_SCALE`: the
per-signal rows compare unscaled contributions and the SNR rows cancel the
scale. It is noise-free, so it ties the signal path at 0.01 dB; the two
noise floors agree only through the SNR rows (0.2 dB).

Two estimator choices were measured, not assumed. On-runs use 0.3 of the
plateau (`ON_RUN_FRACTION`), because 0.5 falls exactly on a sample of a
5 ms raised-cosine ramp at 96 kS/s and f32 rounding flipped it (2-sample
disagreements in two cases). Group delay is the peak of the
cross-correlation magnitude, because the first sample above half the peak
flipped between 64 and 65 for the same reason. The compact gate averages 48
fading realizations, not 32: at 32 the Good preset's independent ensemble
standard error was 0.0601 against the 0.06 ceiling.

Negative controls, each asserting its row fails:
`gate_catches_a_weight_mismatch` (weight 3.3 vs 3.0, envelope),
`gate_catches_the_near_dc_hilbert_image` (129-tap route at 600 Hz, image
above −20 dBc), `gate_catches_doppler_sigma_equal_to_spread` (the historical
coppa defect, fading at τ = 0.2 s), `gate_catches_a_one_db_amplitude_error`
(SNR), `gate_ties_headroom_to_master_scale` (independent scene
scaled by +0.1 dB, power).

The full sweep (`generator_cross_check_full_sweep`, `#[ignore]`: real-route
offsets 2 to 45 kHz in 8 steps of both signs with 120 s keying, Moderate
added, 64 seeds) passed in 75.5 s; its worst fading row used 0.65 of its
tolerance (0.019) and its worst ensemble row 0.61. It does not cover the
511-tap route at 96 kS/s; `receive_side_hilbert.rs` covers 511 taps at
48 kHz.

## The V8w `s09` defect

V8w's `s09` sits at −324.6 Hz with Watterson Poor, outside the 129-tap
route's verified domain. The route leaves a mirror image, and the decoder
promotes a track near +323 Hz where no signal exists (research R2: 120 s,
30 s and 10 s all show it; R3: the same signal on the direct route shows
none). `ghost_probe` (`crates/manta-engine/tests/support/ghost.rs`) reports
it as `mirror:s09`.

`minimizer_shrinks_a_120s_multi_signal_scene` (required CI) starts from
120 s, five signals (`s09` plus V8 `s00`..`s03`) and ends, in 12 probes and
33 s, at 30 s with `s09` alone, text `K`, jitter dropped and the Watterson
fade kept (dropping it removes the ghost). The acceptance run on the whole
V8w scene (`minimizer_shrinks_the_v8w_pileup`, `#[ignore]`, 50 signals,
120 s) took 364 s and 18 probes and reached the same scene, with the same
x86_64-linux digest; its output is the committed repro. Halving to 15 s
removed the ghost in both runs.

Review fixes made in this change: `render_scene` adds and drops each
signal's contribution in turn (holding all 50 V8w contributions at 120 s
would need about 4.6 GB); the minimizer keeps only the current duration's
contributions, capped at 1 GiB; `render_signal_with_route` errors on QSB
with a fade (the real route never applied it, so the generators would
silently disagree); `SceneSpec::validate` bounds `fs` and `duration_s`; the
lint covers `probe` and every signal label and text; the independent
timeline errors on a loop that adds no time and on jitter sigma ≥ 1/3.

Not fixed here: changing the faded route for `s09` moves V8w bytes.

## Follow-ups

- Fix the V8w near-DC route (moves golden bytes; needs its own record).
- A scheduled full-matrix workflow running the `#[ignore]`d sweep and V8w
  acceptance (`.github/`, a CODEOWNERS path).
- Record generator revision and parameters in every fixture manifest, not
  only in committed repros.
- Cross-check the RTTY (`fsk.rs`), FT8 (`ftx.rs`) and non-CW (`noncw.rs`)
  generators, and represent V9's drift staircase in `SceneSpec`.
- Add the 511-tap route at 96 kS/s to the full sweep.
