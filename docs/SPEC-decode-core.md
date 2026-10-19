# SPEC — Decode Core (channelizer → detector → demod → Morse decode)

Status: draft v1. Companion to `ARCHITECTURE.md` §4–§5. This document is the
implementation-level specification: an implementer should be able to code
`manta-dsp` and `manta-decode` from this file without making design
decisions. All constants are normative defaults; every one is exposed in the
TOML config under the names given in [§9](#9-configuration-keys).

Deviations from ARCHITECTURE.md are marked **[DEVIATION]** and summarized in
[§10](#10-deviations-from-architecturemd).

Notation: `fs` = input complex sample rate (S/s), `N` = FFT/channel count,
`Δ` = channel spacing (Hz), `m` = hop index, `k` = channel index `0..N-1`,
`hop` = samples advanced per PFB output frame.

---

## 1. Channelizer (4×-oversampled polyphase filterbank)

### 1.1 Dimensions

`N = fs / 93.75`, which is a power of two for all supported rates:

| Input rate `fs` | `N` | Spacing `Δ = fs/N` | `hop = N/4` | Output rate `fo = fs/hop` | Hop period |
|---|---|---|---|---|---|
| 96 000  | 1024 | 93.75 Hz | 256  | 375 Hz | 2.667 ms |
| 192 000 | 2048 | 93.75 Hz | 512  | 375 Hz | 2.667 ms |
| 384 000 | 4096 | 93.75 Hz | 1024 | 375 Hz | 2.667 ms |
| 768 000 | 8192 | 93.75 Hz | 2048 | 375 Hz | 2.667 ms |

Non-power-of-two input rates (e.g. KiwiSDR 12 kHz IQ) are rational-resampled
in `manta-input` to the nearest table rate before the channelizer;
the channelizer itself only ever sees a table rate. Audio-passband mode
(48 kHz real) is Hilbert-transformed then treated as `fs = 48 000`, `N = 512`
(same 93.75 Hz spacing; the table generalizes as `N = fs/93.75`).

All downstream timing constants are defined in milliseconds and converted to
hops at `fo = 375 Hz` exactly once, at startup, rounding half-up to the
nearest integer hop (the single normative rounding rule for all ms→hop
conversions in this spec). `fo` is invariant across input
rates by construction; nothing below depends on `fs`.

### 1.2 Prototype filter

**[DEVIATION]** `coppa-dsp::filter` provides only `RrcFilter` (root-raised
cosine); there is no general Kaiser/windowed-sinc designer to reuse. The
prototype designer is therefore new code in `manta-dsp::proto` (~40 lines:
Kaiser window + windowed sinc). `coppa-dsp::fft::FftProcessor` is reused as
specified.

Prototype lowpass, length `L·N` with **L = 8 taps per branch**:

- Windowed-sinc: `h[n] = sinc(2·f_c·(n − (LN−1)/2) / fs) · w_kaiser[n]`,
  `n = 0..LN-1`, normalized so `Σ h[n] = 1` (unity DC gain per channel).
- Cutoff `f_c = Δ/2 = 46.875 Hz` (−6 dB point at the channel edge; adjacent
  channels cross at −6 dB, giving the required ≥50 % spectral overlap so a
  signal straddling an edge loses ≤ 3 dB in the better channel after the §1.4
  interpolation).
- Kaiser `β = 7.857` (target stopband attenuation **A = 80 dB**;
  `β = 0.1102·(A − 8.7)`). With `LN ≥ 8192` taps at 96 kS/s the transition
  band is ≈ 60 Hz — stopband is reached by ~107 Hz offset, i.e. alias
  rejection ≥ 80 dB from 1.15 channels away.
- Coefficients are computed in `f64` and stored as `f32`, generated once at
  startup; the same function is unit-tested against fixed reference values
  (first/middle/last 4 taps at N=1024 pinned to 1e-7).

### 1.3 Structure and per-hop processing (WOLA form)

Maintain a sliding input window `x[0..LN)` (newest sample at the end). Every
`hop` new samples:

0. **Replace unusable samples (MAN-234):** as new samples are appended to
   the window, before any hop is computed from them, every sample whose
   `|re|` or `|im|` is not `≤ MAX_SAMPLE_MAGNITUDE = 1e17` (NaN and ±Inf
   included, since NaN compares false) is replaced with `0+0j` and counted
   once (`Channelizer::bad_samples`, monotonic per channelizer). The bound: the prototype filter's absolute tap sum is
   `Σ|h| < 1.34` for every supported N (1.33746–1.33821 for N = 64…4096),
   so with every component within `B = 1e17`, `|X[k]| ≤ √2·Σ|h|·B` and
   `P[k] < 3.6e34`, four decades below `f32::MAX` (3.4e38). No NaN or Inf
   ever reaches channel power, and so none reaches floor or gate state
   (§2). Sources deliver full scale at 1.0, so the bound only catches
   corrupt or mis-scaled input. A usable sample is read and never written,
   so output on clean input is bit-identical to an unguarded channelizer
   (§6). `manta calibrate`, which does not use the channelizer, applies
   the same replacement to its read buffer.
1. **Window & fold:** `u[n] = x[n] · h[LN−1−n]` for `n = 0..LN`, then fold to
   `N` points: `v[j] = Σ_{p=0..L-1} u[j + pN]`, `j = 0..N`.
2. **Phase correction for hop < N:** circularly rotate `v` left by
   `r = (m · hop) mod N` samples: `v'[j] = v[(j + r) mod N]`. (Equivalent to
   multiplying bin `k` by `e^{+j2πk·m·hop/N}`; the rotation keeps every
   channel's passband centered at DC in its own output stream so the envelope
   is phase-continuous across hops.)
3. **FFT:** `X[k, m] = FFT_N(v')[k]` via `FftProcessor::new(N)` (one instance,
   created once; `forward()` allocates — acceptable at 375 calls/s, but the
   engine may pre-allocate via `try_forward` into a scratch buffer if profiling
   demands; not required for M0/M1).
4. **Power:** `P[k, m] = |X[k, m]|²`. Report in dB: `PdB = 10·log10(P + ε)`,
   `ε = 1e-20`.

Channel `k` corresponds to RF frequency
`f(k) = f_center + ((k + N/2) mod N − N/2) · Δ` (standard FFT bin order,
negative frequencies in the upper half). All detector/decoder code works in
channel index; conversion to Hz happens only when a spot is emitted.

### 1.4 Fine frequency estimate (for ±10 Hz spot accuracy)

Per hop, for a track whose selected channel is `k₀`, two estimators run.

**Ownership centroid** (§2.5) and the drift fitter's input (§2.8):

- Quadratic interpolation on **dB** powers of `(k₀−1, k₀, k₀+1)`:
  `δ_m = 0.5·(P₋ − P₊) / (P₋ − 2P₀ + P₊)` where `P• = PdB[k₀•, m]`.
  Clamp `δ_m` to `[−0.5, +0.5]`; if the denominator ≥ 0 (no local max — a
  peak requires `P₋ − 2P₀ + P₊ < 0`), set `δ_m = 0` and mark the hop unusable.
- Centroid `C`: an EMA of `k₀ + δ_m` with α = 0.01 over the usable hops.

**Spot frequency** (MAN-114): a power-weighted lag-1 phase estimator.

- Only **key-down** hops (§3.4) with `SNR ≥ 6 dB` contribute. Each one, with
  the previous hop `m−1` also available, gives
  `z_m = X̃[k₀,m]·conj(X̃[k₀,m−1])·e^{jπ(k₀ mod 4)/2}`, where
  `X̃[k,m] = X[k,m]·s(m,k)` and `s` is the odd-channel sign correction
  (`odd_channel_sign_correction`: −1 when `m` and `k` are both odd). The
  rotation puts every channel's product in one absolute frame: with
  hop = N/4, a tone at channel position `p` gives `arg z_m = 2π·p/4`.
  `z_m` is not normalised, so a hop weighs in by its power.
- Track phase sum: `Z ← Z + α·(z_m − Z)`, α = 0.01, accumulated in `f64`.
- Position `C_f = arg(Z)/2π · 4`, unwrapped to the value nearest the
  centroid `C`. Until the first contributing hop (`Z = 0`), `C_f = C`.
  The reported `C_f` uses `Z` as it stood before the last two unsuppressed
  hops (the two a crash declared on the next hop rolls back, §2.7), so no
  report carries a crash's onset hops.
- Spot frequency: `f_spot = f(0) + C_f·Δ` rounded to **0.01 kHz** for the
  telnet output (MAN-88 — RBN's live feed carries 2 decimals of kHz; the
  previous one-decimal rounding discarded 10 Hz of the estimator's real
  precision), full precision (Hz) in the JSON stream.

Both are live EMAs rather than a lifetime power-weighted mean
(`Σ (k₀ + δ_m)·P₀[m] / Σ P₀[m]`): an undecayed lifetime mean of a drifting
signal converges to the midpoint of its range and fails V9 (see
`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 5).

Absolute accuracy is bounded by the SDR's reference oscillator, which is out
of scope (config `input.freq_correction_ppm` exists). `manta calibrate`
measures it against a known carrier. The spot frequency's measured worst
error across a channel is ≤ 4 Hz on all three tables (48, 96 and 192 kHz) at
+6 and +20 dB; the dB parabola it replaced measured up to 38 Hz, on every
table (`docs/DECISIONS/2026-10-17-man114-phase-difference-spot-frequency.md`).

### 1.5 Decimation (variable-width capture, issue #169)

An optional stage between the `IqSource` and the channelizer: a cascade
of `log2(factor)` Kaiser-windowed halfband FIR decimate-by-2 stages,
`factor` restricted to powers of two. Each stage's ideal cutoff sits at
`CUTOFF_FRACTION * fs_in` (0.235, deliberately below the theoretical
quarter-band point `fs_in/4` -- the output Nyquist after that stage's
decimate-by-2 -- reserving a transition-band margin so full stopband
attenuation is actually reached by the new Nyquist rather than only
somewhere past it), Kaiser-windowed at the same beta/stopband target as
the channelizer prototype (§1.2's `KAISER_BETA`, 80 dB). The decimated
rate must itself satisfy §1.1's `fs/93.75` power-of-two table constraint.
Module: `manta-dsp::decimate`.

A halfband filter has every other tap forced to zero by construction
(except the center tap), which in principle halves the multiply-accumulate
cost per output sample. The current implementation does not exploit this:
`HalfbandStage::process` iterates all taps unconditionally, including the
structurally-zero ones, so the 2x MAC saving is not yet realized. This is a
known, deliberate gap for now (tracked in issue #176), not an oversight,
and there is likewise no criterion bench yet measuring this stage's cost
against the repo's Pi4 CPU budget. Moving the cutoff off exactly `fs_in/4`
(above) also gives up this exact-zero-tap property, so issue #176's skip
opportunity no longer applies to this stage regardless.

**Trusted band** (MAN-196): the `CUTOFF_FRACTION` margin means channels
near the decimated Nyquist edge see real, non-negligible attenuation
(roughly -6 dB to -22 dB in the last ~1.5 kHz below the edge, for a
96k->48k stage). The decimator therefore publishes a **trusted
half-bandwidth** — `TRUSTED_HALF_BAND_FRACTION = 0.4526` of `fs_out`, the
<= 1 dB crossover of the halfband design (21 728.5 Hz measured at
48 kS/s; the constant sits at 21 724.8 Hz, at-or-inside it), exposed as
`Decimator::trusted_half_bandwidth_hz()` and DTFT-self-verified by
bisection in the module's tests. Two rules consume it:

- **Advertisement**: `DecimatingSource::rf_passband_hz` clips to
  `+/- trusted` (intersected with the inner source's passband), so
  `manta check`, SETT segments, live-state `bandState` and the
  static-crash live set all advertise/judge the honest band. The JSON
  `passband_offsets_hz` value narrows; its name and schema do not change.
- **Spawn veto**: `IqSource::trusted_band_hz()` (default `None`) reports
  the band — for `DecimatingSource`, the same intersected band
  `rf_passband_hz` advertises, so the two never disagree; `listen` reads it once per source and every segment's
  `TrackManager::set_trusted_band_hz(lo, hi)` builds a per-channel mask
  (true iff the channel centre `f(k)` lies in `[lo, hi]`; no margin
  channel). A channel whose centre lies outside the band may not START a
  candidate (the §2.4 conjunction gains `!trusted_vetoes_spawn(k)`) —
  spawns only, never closing or de-weighting a running track, per the
  MAN-314 key-up-gap lesson. `None` (every non-decimated source) vets
  nothing: behaviour-identical to before, bit for bit.

**In-band foldover aliases** (round-1 review on PR #300): frequencies at
or above `fs_out - trusted` fold *inside* the trusted band, where the
spawn veto cannot see them — only the halfband's stopband stands between
the alias and a mirrored in-band track. That region is pinned at the
design's >= 80 dB Kaiser stopband target (measured >= ~-86.5 dB, worst at
the region's lower edge 26 275.2 Hz at `fs_out` = 48 kS/s;
`halfband_stopband_covers_frequencies_folding_inside_the_trusted_band`).
A carrier above Nyquist stronger than the stopband margin can still
promote a mirrored track in digital silence (measured boundary: -35 dBFS
at +26 343.75 Hz promotes, -40 dBFS does not; e2e pin
`an_in_band_foldover_residue_promotes_only_above_the_stopband_margin`).
Deepening the stopband is a filter-design change excluded from MAN-196's
scope and raised as a decision ticket (see the decision record).

Channel granularity makes the effective edge the last channel centre
inside the boundary (channel 231, 21 656.25 Hz, at 48 kS/s): channels
232-255 per side (24 of 256) are conceded by design. See
docs/DECISIONS/2026-10-14-man196-decimator-trusted-band.md.

---

## 2. Noise floor & signal-presence detection

### 2.1 Per-channel floor estimator

Order-statistic estimator over a sliding window, computed from a decimated
power stream to bound cost:

- Every 15th hop (25 Hz), push `PdB[k, m]` into a per-channel ring of
  **250 entries (10 s)**.
- Maintain a per-channel histogram of the ring contents: 0.5 dB bins spanning
  −140..0 dBFS (280 bins, `u8` counts; increment on push, decrement on evict —
  O(1) per update, no sorting).
- Floor `F_ch[k]` = the **25th percentile** of the histogram. Median is NOT
  used: CW key-down duty cycle reaches 50–60 % on a busy channel, which
  inflates the median by the full signal power; the lower quartile stays on
  the noise rail unless duty exceeds 75 %.

### 2.2 Neighborhood floor and effective floor

A channel occupied continuously for > 10 s still inflates its own quartile.
Guard with a spectral-neighborhood floor:

- Group channels into blocks of 32. `F_blk[b]` = median of the 32 `F_ch`
  values in block `b`, recomputed at 25 Hz.
- **Effective floor:** `F[k] = min(F_ch[k], F_blk[⌊k/32⌋] + 3 dB)`.

The +3 dB allowance tolerates genuine floor slope across a block (e.g. filter
edges); the `min` guarantees a parked carrier can never raise its own
detection threshold by more than 3 dB relative to its neighbors.

Startup: for the first 10 s the ring is partially filled; the quantile is
taken over whatever is present. Three things keep that young floor honest
(MAN-115, `docs/DECISIONS/2026-10-11-man115-startup-latency.md`; this
replaces SPEC v1's 2 s inhibit):

- **Pre-roll skip.** The `filter_len` zero samples a decode path prepends
  (`pad_hops` hops, all-zero or partial windows) never reach the floor, the
  gate or the warm-up count; the segment's first real hop is its hop 0.
- **Block-median seed.** While a channel's ring holds fewer than 25 entries
  taken since its block's leading silence ended (that block's first 1 s of
  input), its effective floor is raised to at least its block's seed: the
  running
  mean, over the block's first 25 decimated hops, of the median raw `PdB`
  across the block's 32 channels, minus 3.8 dB (`10·log10(ln 2 / ln(4/3))`,
  the median-to-lower-quartile gap of a chi-squared 2-DOF power). Block
  medians below −140 dBFS (digital silence, e.g. a source still opening)
  are left out of that mean; a block's leading silence lasts until its
  first decimated hop with a median at or above −140 dBFS. The seed only
  raises `F[k]`; `F_ch` stays raw.
- **Leading silence.** The histogram bottom-bin entries a channel's ring
  takes during its block's leading silence are left out of the `F_ch[k]`
  and `F_blk` that `F[k]` uses until they leave the ring (10 s); counted,
  they would pin the lower quartile at −140 dBFS. Removing bottom-bin
  entries can only raise a quantile. Bottom-bin entries after the leading
  silence count as usual.
- **Warm-up.** Track creation is inhibited for the first **25 ms**
  (`detector.warmup_ms = 25`, 9 hops) of real hops.

### 2.3 Gate

Per channel, smoothed power `S[k, m]`: EMA of `PdB[k, m]` with time constant
**τ = 40 ms** (`α = 1 − e^{−2.667/40} = 0.0645`).

- **Rise:** `S ≥ F + 6 dB` (`detector.on_snr_db = 6.0`) sustained for
  **19 consecutive hops (≈ 50 ms)** — rejects impulse noise and clicks on
  one channel. It does not reject a band-wide impulse (a static crash): the
  85.33 ms prototype filter (§1.2) and this 40 ms EMA smear a crash of a
  few milliseconds across tens of hops of every channel at once, long
  enough to pass the 19-hop test on hundreds of channels. §2.7 detects
  such a crash across the band and holds the gate through it.
- **Drop:** `S < F + 3 dB` (`detector.off_snr_db = 3.0`, i.e. 3 dB
  hysteresis) continuously for **hang = 5 000 ms** (1875 hops) — survives QSB
  troughs and inter-word gaps at slow speeds.

Reported track SNR (for spots) is converted from the 93.75 Hz channel to the
conventional 2500 Hz reference bandwidth:
`SNR_2500 = (S − F) − 10·log10(2500/93.75) = (S − F) − 14.3 dB`.

`S − F` is **peak-held over each `TrackMeta` reporting interval** (§5's 375
hops) before conversion: `S` is a τ = 40 ms EMA that decays toward the floor
on every key-up, so an instantaneous sample is a function of keying phase,
not of signal strength. The peak over the interval is the settled key-down
level (MAN-102; see
`docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the
measured rejection of an instantaneous sample and of a key-down mean).

This 2500 Hz value is what `TrackMeta`, `Spot.snr_db`, and the JSON stream
carry. The telnet and RBN-uplink wire lines convert it to the 500 Hz
reference bandwidth RBN and CW Skimmer use (`+10·log10(2500/500) ≈ 6.99
dB`), per decision D3
(`docs/DECISIONS/2026-09-06-broad-review-decisions.md`) — a rendering step
at the output boundary only. The internal pipeline, §4.5's confidence `q`,
and §7's vector pass criteria are all unchanged and remain defined in
2500 Hz.

### 2.4 Track lifecycle state machine

States: `IDLE → CANDIDATE → ACTIVE → HANG → CLOSED`.

| Transition | Condition |
|---|---|
| IDLE → CANDIDATE | rise condition first met on channel `k`, `k` is not owned by an existing track (§2.5), `k` is not RTTY-vetoed (§2.6), no static crash is being suppressed this hop (§2.7), `k` is not inside a co-modulation span (§2.8), and `k`'s centre is not outside the source's trusted band when one was supplied (§1.5, MAN-196) |
| CANDIDATE → ACTIVE | rise sustained 19 hops → lease decoder from pool; with `non_cw_filter` on, the lease waits for qualification (below, §2.8) |
| CANDIDATE → IDLE | rise condition lost before 19 hops |
| ACTIVE → HANG | drop condition met (below off threshold) |
| HANG → ACTIVE | `S ≥ F + on_snr_db` again (hang timer reset) |
| HANG → CLOSED | hang timer (5 000 ms) expires → decoder returned, final spots flushed |
| ACTIVE/HANG → CLOSED | **garbage collect:** no character emitted for 30 000 ms (`detector.gc_ms`) — carrier or non-CW signal; the channel is marked *suppressed* for 60 s (re-detection allowed but logged) |
| any → CLOSED | eviction: track cap reached and this is the lowest-SNR track (counted in metrics, per ARCHITECTURE §4) |
| ACTIVE/HANG/CANDIDATE → CLOSED | `round(c)` inside a marked RTTY signal's hard span (§2.6) → closed as bookkeeping (`CloseReason` `rtty`, counted) |
| ACTIVE/HANG → CLOSED | promoted but not qualified as keyed CW within `unkeyed_close_hops` of promotion (§2.8) → closed as bookkeeping (`CloseReason` `not_cw`, counted); no events, because no decoder was leased |
| ACTIVE → CLOSED | qualified track key-down for `unkeyed_close_hops` with no re-rise, no decoded character and no callsign read (§2.8) → closed as bookkeeping (`CloseReason` `unkeyed`, counted), flushed like the garbage-collect close |

All timers are hop-counted (integers), never wall-clock.

**Lazy lease (MAN-313).** With `detector.non_cw_filter = true` (the
default) the lifecycle above is unchanged, but CANDIDATE → ACTIVE does not
lease a decoder. The promoted track queues its demod input from the
promotion hop and emits nothing. When it qualifies as keyed CW (§2.8) it
leases a decoder, the queue is replayed into it, and `TrackPromoted` is
emitted carrying the promotion hop's `sample_ts` and `freq_hz`. Decoded
output is the same as an immediate lease; only the batch that carries it
is later (0.5–1.5 s of stream on the golden vectors). A track that never
qualifies emits nothing, including at end of input. `NotCw`, `Unkeyed` and
the garbage-collect close each start the MAN-171 respawn cooldown on their
channels. `non_cw_filter = false` leases at promotion, as before.
On a hop where a static crash is suppressed (§2.7, `detector.crash_filter`
`"full"` or `"starts-only"`), no IDLE → CANDIDATE transition happens, a
CANDIDATE's lifecycle does not step (its confirm count neither advances
nor resets), and ACTIVE/HANG tracks step on rise/drop recomputed from the
gate's held `S[k]` (its value before the crash) against the current floor.
The crash's energy therefore cannot confirm a candidate or turn a HANG
track back to ACTIVE.

### 2.5 Adjacent-channel ownership (one signal ⇒ one track)

- A track tracks a fractional center `c` (the §1.4 running centroid,
  initialized to its birth channel `k₀`). It **owns** channels
  `{round(c) − 1, round(c), round(c) + 1}`.
- A CANDIDATE in an owned channel is absorbed: no new track; ownership stays
  with the incumbent.
- A rising, unowned channel `k` may start a spawn decision only if its
  smoothed power `S[k]` is ≥ both `S[k−1]` and `S[k+1]` (a local maximum;
  neighbours wrap). (MAN-172, see docs/DECISIONS/2026-10-11-man172-high-snr-track-churn.md)
- A channel whose centre lies outside the source's trusted band, when one
  was supplied, may not start a spawn decision — in both the `k` and
  `k+1` slots of the same-hop pairing rule below, so the arbitration
  cannot route around the veto. Spawns only: a track that already exists
  on an untrusted channel is never closed or de-weighted by it (MAN-196,
  the MAN-314 spawns-only lesson).
- A spawn decision at `k` is additionally rejected while *any* channel of
  the prospective ownership window `{k−1, k, k+1}` (neighbours wrap) is
  still serving a post-`Silent` respawn cooldown (§9's
  `detector.silent_respawn_cooldown_ms`). The same-hop pairing rule below
  applies this to `k+1`'s own window. Gating the birth channel alone let a
  neighbour-born candidate absorb the cooling channel via max-power
  selection. (MAN-197, see docs/DECISIONS/2026-10-14-man197-respawn-cooldown-ownership-window.md)
- Each hop, the track's demod input is taken from the **max-power channel
  among its owned set** (per ARCHITECTURE §4 "max-power selection"); the
  owned set follows `round(c)` as the centroid drifts, which is how drifting
  signals (§7 test 9) are followed without retuning. A live track's
  selection additionally **skips channels still serving a post-`Silent`
  respawn cooldown**, falling back to the plain max-power channel only when
  every owned channel is cooling: a candidate born two bins from a cooling
  channel passes the spawn gate legitimately, yet a stronger neighbour can
  drag `round(c)` onto the cooling channel, whose continuous power would
  otherwise keep the track decoding the silenced birdie for a whole extra
  GC cycle inside the cooldown window. (MAN-197, see
  docs/DECISIONS/2026-10-14-man197-live-window-cooldown-enforcement.md)
- If two channels `k` and `k+1` meet the rise condition on the *same hop* and
  neither is owned, a spawn decision at `k` creates one CANDIDATE at the
  higher instantaneous-power channel. This existing pairing rule can select
  `k+1` even when only `k` is a smoothed-power local maximum.
- Two tracks whose centers converge within 1.0 channel (interference or
  drift-collision) are merged: the lower-SNR track is CLOSED with reason
  `merged` (counted); its decoder state is discarded (text already emitted
  stands). On equal SNR the older track (lower `track_id`) survives.
  (MAN-172, see docs/DECISIONS/2026-10-11-man172-high-snr-track-churn.md)
  With `non_cw_filter` on, a qualified track beats an unqualified one
  first (§2.8 F).

### 2.6 Two-tone FSK (RTTY) recognition

MAN-314, `manta-dsp::fsk::FskDetector`, see
docs/DECISIONS/2026-10-11-man314-rtty-recognition.md. With
`detector.rtty_filter = true` (the default) the track manager feeds the
detector every hop, after the floor and gate update, with the same
`PdB[k]` and `FloorBank`. Channels are judged in frequency order
`j = (k + N/2) mod N`; pairs never wrap across Nyquist.

Constants (compile-time, like the floor constants):

| Constant | Value | Meaning |
|---|---|---|
| `BLOCK_HOPS` | 375 | one detector block, 1.000 s |
| `LAGS` | 1, 2, 3, 4, 5, 9, 10 | pair distances `d` in channels (170–850 Hz at any offset) |
| `HOT_MEAN_DB` | 6.0 | both channels' block-mean SNR must reach this |
| `RHO_MAX` | −0.5 | Pearson ρ of the pair's per-hop SNR (dB) must be ≤ this |
| `FILL_DB`, `FILL_MIN` | 10.0 dB, 0.8 | `max(a, b) ≥ FILL_DB` in ≥ 80 % of hops (constant envelope) |
| `DOM_DB`, `DOM_MIN` | 6.0 dB, 0.1 | each tone leads the other by ≥ 6 dB in ≥ 10 % of hops |
| `PHASE_SNR_DB` | 10.0 | hops at or above this feed the frequency estimator |
| shift windows | [130, 230], [395, 475], [820, 885] Hz | measured shifts that count as RTTY |
| standard shifts | 170, 200, 425, 450, 850 Hz | the nominal shift is the nearest |
| `HOLD_BLOCKS` | 30 | evidence and mark survive 30 blocks without a pass |
| `GUARD_CHANNELS` | 1 | hard span = pair channels ± 1 |
| `SHADOW_WIDTH_CHANNELS` | 10 | shadow = span ± 10 channels (not the hard span) … |
| `SHADOW_DEPTH_DB` | 40.0 | … for candidates more than 40 dB below the key-down level |

Per hop, for every channel: `snr = PdB[k] − F_eff[k]` is stored in the
block buffer; if `snr ≥ PHASE_SNR_DB`, `z[j] += x̃[k]·conj(x̃_prev[k])`
where `x̃ = X[k]·odd_channel_sign_correction(m, k)`.

At each block end, for each pair `(j, j+d)` with both channels hot:

1. ρ > 0 → **contra**: the pair's evidence drops by 2 (saturating at 0).
2. Shape check: ρ ≤ `RHO_MAX`, fill ≥ `FILL_MIN`, both dominance
   fractions ≥ `DOM_MIN`; otherwise **neutral** (no change).
3. Tone positions: for each tone, `j_peak` is the channel among `c − 1 … c + 1`
   with the highest mean SNR over the hops that tone leads; the position
   is `j_peak + arg(z[j_peak]) / 2π · 4` (hop = N/4, so one cycle per hop
   spans four channels). Shift = (high − low) · 93.75 Hz.
4. A shift inside a window is a **pass**: evidence + 1, the pass count and
   the tone positions accumulate, and the level (block mean of
   `max(a, b)`) is stored. Anything else is neutral.

A pair with `HOLD_BLOCKS` consecutive blocks without a pass is dropped. A
pair is **marked** once its evidence reaches
`rtty_dwell_blocks = floor(rtty_dwell_hops / 375)` and stays marked until
dropped. Marked pairs whose channel intervals overlap or touch within one
channel form one signal; its tones are the pass-averaged positions of the
pair with the most passes (ties → lower `j`), its span is the union of the
group's pair channels and tone channels, and it keeps a stable id while
its span overlaps the previous block's signal.

Effects on the track manager (§2.4/§2.5):

- **Hard span** (span ± `GUARD_CHANNELS`): no spawn decision at `k` (nor
  the same-hop `k+1` pairing), and every open track whose `round(c)` lies
  inside it closes as `CloseReason::Rtty` (`ClosureKind::Bookkeeping`, no
  survivor; counted in `CloseCounts::rtty`).
- **Shadow** (span ± `SHADOW_WIDTH_CHANNELS`): a spawn decision at `k` is
  vetoed when `S[k] − F_eff[k] < level − SHADOW_DEPTH_DB`. The shadow never
  closes an existing track: a real CW track's key-up gaps put it below any
  relative level.

Determinism: no RNG and no clock; pairs iterate in frequency order through
a `BTreeMap`; sums run in index order with `f64` accumulators.
`rtty_filter = false` builds no detector and leaves every output as before.

### 2.7 Band-wide static-crash detection (MAN-312)

MAN-312, `manta-dsp::crash::CrashDetector`, see
docs/DECISIONS/2026-10-13-man312-static-crash-filter.md. A static crash
(lightning or another atmospheric impulse) is a few milliseconds of
broadband energy. The prototype filter is `L·N` taps, `8 / 93.75 Hz =
85.33 ms` at every table rate (§1.2), and the gate EMA has τ = 40 ms
(§2.3); together they spread one crash across tens of hops of every
channel. Judged one channel at a time (§2.3), a +40 dB, 5 ms crash on an
empty 96 kS/s band starts 177 tracks, and the same energy reaches every
running track's decoder as extra key-down. A crash lifts nearly every
channel together and CW never does, so the detector judges each hop
across the band. The track manager runs it every hop, before the floor
and gate updates, on the same `PdB[k]` they receive.

**Live channels.** The statistic runs over the channels inside the
source's RF passband, `(lo, hi)` Hz relative to the centre frequency
(`IqSource::rf_passband_hz()`):

- If `lo ≤ −fs/2` and `hi ≥ fs/2` (the default, which file, SoapySDR and
  HPSDR sources report and a `DecimatingSource` clamps to), every channel
  is live.
- Otherwise channel `k` is live when `lo + Δ ≤ o(k)·Δ ≤ hi − Δ`, where
  `o(k) = ((k + N/2) mod N) − N/2` is §1.3's signed channel offset: one
  channel of margin inside each passband edge, where the source's own
  filter roll-off sits. A KiwiSDR's ±5 kHz at 96 kS/s gives 105 live
  channels; 48 kHz rig audio with a 300–3000 Hz passband gives 27.

The paths that open a live source (`manta-engine::listen`, which `run`
uses, and the soak harness) give the track manager the source's
passband; `decode` (file input) uses the full band, which is what every
IQ WAV source reports. With fewer than
`CRASH_MIN_LIVE_CHANNELS` (96) live channels the detector is
**inactive**: it never fires and counts nothing, and every
`crash_filter` value behaves as `"off"`. `run` logs one INFO line when
the filter is not `"off"` but inactive. The passband matters: over all
1024 channels of a KiwiSDR-shaped 96 kS/s stream the out-of-band
channels never lift, so crashes went undetected (22 and 60 tracks
started under `"full"`); over the 105 passband channels none started.

**Reference.** The detector keeps its own per-channel dB reference `R[k]`
for every channel, live or not. On its first hop, `R[k] = PdB[k]`. On
every later hop whose reference is not frozen (below), each channel
updates:

```text
R[k] ← PdB[k]                         if R[k] < HIST_MIN_DB (−140 dBFS, §2.1)
R[k] ← R[k] + α·(PdB[k] − R[k])       otherwise, α = 0.0645 (§2.3's gate α)
```

The first rule is for digital silence (a source still opening, or a gap
of zeros). Input returning from silence lifts every channel, so it is
held as a crash until `CRASH_MAX_HOPS` (below); when the reference then
unfreezes, the rule moves it to the new level in one hop rather than over
the EMA's tail, and the crash releases `CRASH_RELEASE_HOPS` later.

**Lift.** On hop `m`, when the detector is **armed** (the segment's hop
count has reached `detector.warmup_ms`'s hop count, §2.2; earlier hops
only warm the reference) and active, with `n` live channels:

```text
lift[m] = Q25 over live k of (PdB[k, m] − R[k]) + CRASH_ONSET_Q25_OFFSET_DB
```

`Q25` is the element at index `⌊n/4⌋` of the `n` excesses in ascending
`f64::total_cmp` order, a selection with no interpolation. `R` is the
reference before this hop's update. On noise, `PdB − R` is the dB-domain
spread of a chi-squared 2-DOF power about its smoothed level; its lower
quartile measured −2.81 dB (theory −2.90), so the offset puts the lift
of a quiet band near 0 dB. An unarmed or inactive hop has
`lift = −∞`. A lift of 3 dB at the lower quartile means at least three
quarters of the live channels rose about 3 dB together. The median was
measured and rejected: it fired up to 9 times per 30 s on dense bands,
where Q25 never fired.

**State machine.** Per hop, after the lift:

1. `above = lift ≥ CRASH_LIFT_DB`. `above_run` counts consecutive
   `above` hops and resets to 0 on any other hop.
2. If `above` and (`in_crash` or `above_run ≥ CRASH_MIN_HOPS`): set
   `below_hops = 0`. If not already `in_crash`, the crash **begins**:
   `in_crash = true`, `count += 1`, `run_hops = 0`.
3. Otherwise, if `in_crash`: `below_hops += 1`; once
   `below_hops ≥ CRASH_RELEASE_HOPS`, `in_crash = false`.
4. If `in_crash`: `run_hops += 1`.
5. The reference is **frozen** on this hop when
   `(above or in_crash) and not (in_crash and run_hops > CRASH_MAX_HOPS)`;
   otherwise it updates as above.

The hop's outputs are `suppress = in_crash and run_hops ≤ CRASH_MAX_HOPS`
and `hold_decoder = in_crash and run_hops ≤ CRASH_HOLD_MAX_HOPS`. A crash
begins on its third lifted hop, so its first two lifted hops are neither
suppressed nor held; the decoder hold and the phase-sum rollback below
reach back past them.
A lift that outlasts `CRASH_MAX_HOPS` is a level change rather than a
crash (a gain step, or input returning from silence): suppression ends,
the reference unfreezes and tracks the new level, the lift falls back
under the threshold, and `in_crash` releases `CRASH_RELEASE_HOPS` later.
It counts as one crash.

Constants (compile-time, `pub const` in `manta-dsp::crash`; the measured
basis is in the decision record):

| Constant | Value | Basis |
|---|---|---|
| `CRASH_LIFT_DB` | 3.0 dB | the ticket's "≥ 3 dB across most of the stream". Noise's largest 3-hop lift was 0.40–1.03 dB at ≥ 512 live channels and 2.69 dB at 97 |
| `CRASH_ONSET_Q25_OFFSET_DB` | 2.8 dB | −Q25 of `PdB − R` on noise: measured 2.81, theory 2.90 |
| `CRASH_MIN_HOPS` | 3 (8 ms) | dense bands reached single-hop lifts of 3.27–3.38 dB but at most 2.48 dB over 3 hops; a 1 ms +40 dB crash still flags 16 hops |
| `CRASH_RELEASE_HOPS` | 4 (10.7 ms) | one prototype-filter sidelobe period; every measured crash counted exactly once |
| `CRASH_HOLD_MAX_HOPS` | 75 (200 ms) | caps the decoder hold; measured short crashes flag at most 30 hops |
| `CRASH_MAX_HOPS` | 375 (1 s) | past this the lift is a level change: suppression ends and the reference re-tracks |
| `CRASH_MIN_LIVE_CHANNELS` | 96 | 64 live channels gave 7 false crashes per hour of noise; 97, 105 and 128 gave none |

**Mode actions.** `detector.crash_filter` (§9) chooses what the track
manager does on a hop with `suppress` set:

| Action | `"off"` | `"starts-only"` | `"full"` (default) |
|---|---|---|---|
| detect; count each crash once, when it begins | yes | yes | yes |
| no spawn decision (§2.4 IDLE → CANDIDATE, §2.5) | no | yes | yes |
| CANDIDATE lifecycles do not step | no | yes | yes |
| gate EMA `S[k]` held; rise/drop recomputed from the held `S[k]` against the current floor | no | yes | yes |
| track centroids (§1.4, §2.5) not updated | no | yes | yes |
| spot-frequency phase sum `Z` (§1.4) not updated, and on the hop the crash begins reset to its value before the two lifted hops that preceded it | no | yes | yes |
| ACTIVE/HANG lifecycles step on the held rise/drop | no (live rise/drop) | yes | yes |
| decoder input held at its pre-onset value (hops with `hold_decoder`) | no | no | yes |
| spectral reference (`FloorBank`'s 40 ms linear-power EMA, SPEC-decode-core-v2 §2.2) held (hops with `hold_decoder`) | no | no | yes |
| floor rings (§2.1) keep taking their decimated pushes | yes | yes | yes |
| RTTY detector (§2.6, when `rtty_filter` is on) takes the hop's unheld `PdB[k]` and floor | yes | yes | yes |

The floor rings are never frozen: a sustained band-wide rise has to reach
the floor eventually, or a level step would never release. `"off"`
counts crashes and changes nothing else; its event stream equals that of
an inactive detector, which is the behaviour before MAN-312.
`"starts-only"` lets a running track decode the crash as received.

**Decoder hold (`"full"`).** Each decoder input entry (§3's `a[m]` with
its raw power, spectral reference, sample timestamp, current SNR and
centroid frequency)
carries the `hold_decoder` flag of the hop on which it was observed. The
flag is captured at observation, so it stays with its entry through the
refiner's `GROUP_DELAY_HOPS` delay (MAN-194, SPEC-decode-core-v2 §3),
which emits a refined entry several hops after it was observed. Each
track keeps the `(amplitude, raw power)` of its last `CRASH_MIN_HOPS`
clean entries. When an entry is queued for the decoder:

- a clean entry joins that history (the oldest pair leaves past
  `CRASH_MIN_HOPS`) and passes unchanged;
- a flagged entry has its `(amplitude, raw power)` replaced by the
  history's oldest pair, the value from before the lift crossed the
  threshold. With an empty history it passes unchanged. Its timestamp,
  spectral reference and SNR are kept, and it does not join the history.

The held value is the one from before the onset, not from just before the
flag: the crash's first two lifted hops precede the flag and are already
in the history. A hold, rather than zeroed input, leaves the keyer in
the state it was in before the crash: zeros during a key-down would read
as a key-up and split the element. On VC2 (§7.2) the hold added no dits,
but it can substitute a character (`W1AW` → `A1AW` or `WAOAW`, `CQ` →
`CGT` were measured), so the §7.2 vectors assert dits and spots, not
text.

**Counting.** `TrackManager::static_crash_count()` is one decode
segment's count. The daemon sums segments across source reconnects and
exports the total as `manta_static_crashes_total`, which never decreases
(docs/METRICS.md); the soak report carries it as `static_crashes`.

**Determinism.** The detector holds only per-hop state (the reference,
the run counters and the count), allocates nothing per hop, uses no RNG
and no clock, and takes its quartile by `f64::total_cmp` selection over
the live channels in ascending index order. It consumes one channelizer
hop at a time, so its state and every output are independent of how the
input is chunked (§6); `crash_state_is_chunk_invariant` checks 256-,
4096- and 65536-sample chunks.
### 2.8 Non-CW rejection (MAN-313)

MAN-313, `manta-dsp::comod::CoModDetector` plus per-track keying evidence
in `manta-engine::track`, see
docs/DECISIONS/2026-10-13-man313-non-cw-rejection.md. Every rule here
applies only with `detector.non_cw_filter = true` (the default).
Noise-like wideband data and swept carriers start no CW track, speech
(SSB) starts far fewer (a measured residual, not zero: see the decision
record's known limits and MAN-372), and a track that stops keying closes. `non_cw_filter = false` builds no
classifier and runs none of the rules below: decoder and `TrackPromoted`
at promotion, no qualification, veto, timeout or unkeyed close.

`unkeyed_close_hops = ms_to_hops(detector.unkeyed_close_ms)`, default
3750 (10 s). The loader rejects a value below 1000 ms.

Constants (compile-time; `COMOD_*` in `manta-dsp::comod`, the rest in
`manta-engine::track`):

| Constant | Value | Meaning |
|---|---|---|
| `COMOD_WINDOW_HOPS` | 375 | ring length per channel, 1.000 s |
| `COMOD_EVAL_HOPS` | 75 | evaluation cadence, 0.2 s |
| `COMOD_MIN_FILL_HOPS` | 188 | first evaluation once this many hops are stored |
| `COMOD_HOT_DB` | 6.0 | a channel is hot when its window-mean SNR reaches this |
| `COMOD_LAG_MIN`, `COMOD_LAG_MAX` | 3, 16 | pair distances in channels, inclusive |
| `COMOD_RHO` | 0.5 | Pearson ρ of a hot pair's per-hop SNR must be ≥ this |
| `COMOD_COUNT` | 4 | a channel with ≥ 4 co-modulated partners joins a span |
| `COMOD_SPAN_MIN_WIDTH` | 8 | span width, first to last channel inclusive |
| `COMOD_SPAN_STRONG` | 4 | channels in `[first − 2, last + 2]` whose window peak … |
| `COMOD_SPAN_STRONG_DB` | 10.0 | … is within 10 dB of that range's strongest window peak |
| `COMOD_SPAN_GUARD` | 1 | marked span = `[first − 1, last + 1]` |
| `COMOD_HOLD_HOPS` | 1875 | a span stays marked 5 s after the evaluation that found it |
| `MAX_MARK_HOPS` | 1125 | longest ACTIVE run (3 s) that still counts as a keying cycle |
| `QUALIFY_MIN_ACTIVE_HOPS` | 8 | ACTIVE hops needed since the previous evaluation |
| `SLOPE_MIN_SAMPLES` | 20 | key-down hops needed for the slope |
| `MAX_SWEEP_HZ_PER_S` | 75.0 | largest qualifying \|slope\| |
| `KEYDOWN_SPAN_MIN_HOPS` | 150 | key-down hops must span at least 0.4 s |
| `ISO_DB` | 6.0 | flat-neighbourhood threshold |
| `ISO_MIN_FRACTION` | 0.5 | smallest qualifying non-flat fraction |
| `MAX_UNQUALIFIED_BACKLOG` | 7500 | most queued demod inputs an unqualified track keeps (20 s) |
| `SHORT_MIN_MARKS` | 3 | D′: fewest debounced keyed marks in the queued input |
| `SHORT_MARK_DEBOUNCE_HOPS` | 4 | D′: key-down/key-up run debounce for a keyed mark |
| `SHORT_HYST_FRAC` | 0.15 | D′: keyed-marks hysteresis half-band, fraction of the Q10–Q90 gap |
| `SHORT_MIN_KEYING_RATIO` | 2.0 | D′: Q90/Q10 ratio the queued input must show at all |

**A. Lazy lease.** At CANDIDATE → ACTIVE the track records its promotion
hop, `sample_ts` and `freq_hz`, queues the promotion burst, and from then
on queues its demod input (§3) every hop. It leases no decoder and emits
no `TrackPromoted`. The pool drains only tracks that hold a decoder. At
qualification (D) it leases a decoder, sets the decoder frequency to the
current centroid, and emits `TrackPromoted { sample_ts, freq_hz }` with
the recorded promotion values; the queue is replayed into the decoder at
the next drain. Once an unqualified track's queue exceeds
`MAX_UNQUALIFIED_BACKLOG` + `MAX_UNQUALIFIED_BACKLOG`/4 entries, its oldest
entries are dropped down to `MAX_UNQUALIFIED_BACKLOG`; the default
`unkeyed_close_hops` (3750) never reaches that, so only a raised
`unkeyed_close_ms` changes what a late-qualifying track replays. At end of
input an unqualified track is dropped with no events. Until it qualifies, the track's
garbage-collect timer (§2.4) is held at zero, since with no decoder it
cannot decode a character; `NotCw` (D) bounds its life instead.

**B. Keying evidence.** Every hop, for each promoted, unqualified track:

- `cycles += 1` on each HANG → ACTIVE whose preceding ACTIVE run lasted
  ≤ `MAX_MARK_HOPS`.
- `active_hops` += 1 on each ACTIVE hop; reset to 0 at each evaluation.
- `dropped_since_eval` (MAN-271) is set on each ACTIVE → HANG and cleared
  at each evaluation, so D′ can fire only at the first evaluation after a
  drop.
- A **key-down hop** is one with `PdB[k] − F_eff[k] ≥ on_snr_db`, `k` the
  track's selected channel (§2.5). On it, until the track qualifies:
  - **Slope:** accumulate `n, Σt, Σf, Σt², Σtf` in `f64`, with
    `t = hop − first key-down hop` and `f = wrap(k − k₀) + δ` in
    channels, where `k₀` is the birth channel, `wrap` maps to
    `[−N/2, N/2)`, and `δ` is §1.4's interpolated offset from the linear
    powers at `k − 1, k, k + 1` (0 when undefined). `t_last` is the last
    key-down hop's `t`. The slope in Hz/s is the least-squares `df/dt`
    (channels per hop) × channel spacing × hops per second
    (4 × spacing; 375 at 93.75 Hz).
  - **Flatness:** let `side = max(PdB[k±2], PdB[k±3])` (indices mod N).
    The hop is **non-flat** when `|PdB[k] − side| ≥ ISO_DB`: a neighbour
    at least 6 dB louder is a different signal, so only a neighbourhood
    within 6 dB of `PdB[k]` counts as flat. The non-flat fraction is
    non-flat hops ÷ key-down hops.

**C. Co-modulation classifier.** Every hop the track manager pushes each
channel's gate-smoothed SNR `S[k] − F_eff[k]` (§2.2, §2.3), stored in
frequency order `j = (k + N/2) mod N` in an `N × COMOD_WINDOW_HOPS` `f32`
ring. Once `COMOD_MIN_FILL_HOPS` hops are stored, it evaluates every
`COMOD_EVAL_HOPS` hops over the stored hops (at most the last 375):

1. Per channel: window mean and window peak. A channel is hot when its
   mean ≥ `COMOD_HOT_DB`.
2. Counts start at 0. For each hot pair `(j, j + d)`, `d` in
   `COMOD_LAG_MIN..=COMOD_LAG_MAX`, `j + d < N` (no wrap across Nyquist),
   with Pearson ρ ≥ `COMOD_RHO`: both channels' count += 1.
3. A **span** is a run of channels with count ≥ `COMOD_COUNT`, bridging
   single-channel gaps, at least `COMOD_SPAN_MIN_WIDTH` wide, that holds
   at least `COMOD_SPAN_STRONG` channels in `[first − 2, last + 2]` whose
   window peak is within `COMOD_SPAN_STRONG_DB` of the strongest window
   peak in that range. The peak, not the mean, keeps a loud keyed tone's
   click skirt (30–60 dB down) from forming a span.
4. Each span marks `[first − COMOD_SPAN_GUARD, last + COMOD_SPAN_GUARD]`
   until `hop + COMOD_HOLD_HOPS`. `in_span(k)` is true while `k` is
   marked.

Lags start at 3 so a tone's own ±1/±2 key-click splatter never counts.
There is no per-track taint: a high count at a track's channel never
closes it.

**D. Qualification.** At each evaluation, each promoted, unqualified
track (`k = round(c)`) qualifies when all hold:

- `cycles ≥ 1`;
- `active_hops ≥ QUALIFY_MIN_ACTIVE_HOPS`;
- `!in_span(k)`;
- `!comodulated_with_span(k)`: channel `k`'s window does not correlate
  (Pearson ρ ≥ `COMOD_RHO`) with any marked span channel at least
  `COMOD_LAG_MIN` channels away. A voice's weaker harmonics outside the
  span its strong harmonics mark follow the same syllable envelope; an
  independently keyed CW signal does not;
- key-down hops `n ≥ SLOPE_MIN_SAMPLES`;
- `|slope| ≤ MAX_SWEEP_HZ_PER_S`;
- `t_last ≥ KEYDOWN_SPAN_MIN_HOPS`;
- non-flat fraction ≥ `ISO_MIN_FRACTION`.

A track not qualified `unkeyed_close_hops` after its promotion closes
`CloseReason::NotCw` (`ClosureKind::Bookkeeping`, `CloseCounts::not_cw`).
It has no decoder, so it emits nothing. Its channels get the MAN-171
respawn cooldown.

**D′. Short-transmission qualification** (MAN-271,
docs/DECISIONS/2026-10-16-man271-short-transmission-decode.md). A second
way to qualify, checked at the same evaluations as D. A single short
transmission — one 2-character exchange at contest speed — can never show
`cycles ≥ 1` (its whole message is one ACTIVE run bridged by the gate's
40 ms EMA, and it never re-rises) nor span `KEYDOWN_SPAN_MIN_HOPS`. At the
first evaluation after an ACTIVE → HANG drop, a promoted, unqualified
track qualifies when all hold:

- the track's state is HANG and it dropped ACTIVE → HANG since the
  previous evaluation (`dropped_since_eval`);
- `active_run ≤ MAX_MARK_HOPS` (the drop ends one mark, not a carrier);
- `!in_span(k)` and `!comodulated_with_span(k)`, exactly as D;
- key-down hops `n ≥ SLOPE_MIN_SAMPLES`;
- `|slope| ≤ MAX_SWEEP_HZ_PER_S · (KEYDOWN_SPAN_MIN_HOPS / max(t_last,
  1))^1.5` when `t_last < KEYDOWN_SPAN_MIN_HOPS`, else
  `MAX_SWEEP_HZ_PER_S`: a least-squares slope's standard error over
  evenly keyed samples grows as span^−1.5, so the short-span tolerance
  scales to keep the 150-hop reference span's noise margin;
- non-flat fraction ≥ `ISO_MIN_FRACTION`, exactly as D;
- **keyed marks ≥ `SHORT_MIN_MARKS`**, counted from the track's queued
  input (§2.8 A's `pending` amplitudes — never drained before
  qualification, so the count is deterministic and chunk-size invariant):

  1. Rails: nearest-rank Q90 and Q10 over the queued amplitudes (Q10
     floored at 1e-12; `f32::total_cmp` selection). No mark at all is
     counted when `Q90 < SHORT_MIN_KEYING_RATIO · Q10` — flat input is
     unkeyed.
  2. Threshold `mid = (Q90 + Q10)/2` with hysteresis
     `±SHORT_HYST_FRAC · (Q90 − Q10)`: while key-down, go key-up below
     `mid − half`; while key-up, go key-down above `mid + half`.
  3. A key-down (or key-up) run counts once it reaches
     `SHORT_MARK_DEBOUNCE_HOPS` hops; a key-up run shorter than that is a
     dip inside a mark, not a gap between two marks. These are Demod's
     own §3.2/§3.3 shapes, so "a mark" means what the leased decoder will
     see.

  A swept carrier crossing one channel is a single smooth bump and counts
  one mark; a real short CW message counts its dits and dahs. This is the
  measured discriminator that lets D′ waive `cycles` and `t_last` safely.
  The count is computed only at an evaluation where the track is in HANG
  with `dropped_since_eval` set — one quantile selection over the bounded
  queue per drop, nowhere else.

Qualification through D′ leases the decoder, replays the queue and emits
the deferred `TrackPromoted` exactly as D does. A track still unqualified
at end of input is dropped with no events, unchanged.

**E. Spawn veto.** A spawn decision at `k`, and the same-hop `k+1`
pairing (§2.5), is vetoed while `in_span(k)`, next to the RTTY veto
(§2.6).

**F. Merge preference.** When two converging tracks merge (§2.5) and
exactly one is qualified, the qualified one survives. Otherwise §2.5's
rule applies.

**G. Unkeyed close.** A qualified track in ACTIVE closes
`CloseReason::Unkeyed` (`ClosureKind::Bookkeeping`,
`CloseCounts::unkeyed`) when `hop − max(last_rise_hop, last_char_hop) ≥
unkeyed_close_hops` and no callsign was read.

- `last_rise_hop` is the promotion hop, then each HANG → ACTIVE.
- `last_char_hop` is set from each batch's `CharDecoded` events (batch
  granularity, like the garbage-collect timer's reset).
- A callsign is read when the current word passes
  `manta_spot::grammar::is_plausible`. The word is built from
  `CharDecoded` glyph text and ends at a space, a prosign or a
  `WordBoundary`.

The close flushes like the garbage-collect close: refiner drained,
speed-only decoder finish, `TrackClosed` if the track emitted, and the
MAN-171 respawn cooldown. A callsign read leaves the track to the 30 s
garbage-collect rule.

Determinism: no RNG and no clock. Every rule is hop-counted and runs
sequentially in the track step; tracks iterate in id order, channel pairs
in frequency order, and sums in index order with `f64` accumulators.
Memory: the ring is `N × 375` `f32` (3 MB at N = 2048); an unqualified
track's queue is bounded by `unkeyed_close_hops` and by
`MAX_UNQUALIFIED_BACKLOG` (A).

---

## 3. Per-track demodulation

Input: `a[m] = sqrt(max-power-owned-channel P)` — the linear envelope at
375 Hz. All constants below in ms are converted to hops (2.667 ms/hop).

### 3.1 Normalization

**[DEVIATION — narrowed]** `coppa-dsp::agc::AdaptiveAgc` is block-based
(`new(target_level, block_size)`, `process(&[f32])`) and designed for audio
block flows. At 375 Hz a meaningful block is 16 samples ≈ 43 ms of latency
and its adaptation interacts with the keying envelope itself. Since the §3.2
threshold is self-normalizing (it estimates both rails), AGC adds no decision
value. **Normalization is a single fixed scale per track:** divide `a[m]` by
`A_ref` = the 90th percentile of `a` over the first 500 ms after ACTIVE
(re-estimated once if `E_hi` later drifts above `3·A_ref` or below
`A_ref/3`). `AdaptiveAgc` is not used in the decode path.

### 3.2 Dual-EMA adaptive keying threshold

State: `E_hi` (key-down level), `E_lo` (key-up level). **[DEVIATION]**
(MAN-103, `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`): the
keying decision is not a single threshold but an additive band about the
linear-amplitude midpoint of the two rails, `mid = (E_hi + E_lo)/2`,
half-width `half = hyst_frac · (E_hi − E_lo)` (§3.3). The prior geometric
mean `T = sqrt(E_hi · E_lo)` (ARCHITECTURE §5's original text) sits closer
to `E_lo` than to `E_hi` as apparent keying depth grows, so a rising edge
only had to climb a small fraction of the amplitude range while a falling
edge had to decay nearly all the way back down — inflating every measured
mark, worse at higher SNR and near a channel edge (where the recovered
envelope's own rise/fall transient is slower). A band symmetric in *linear
amplitude* about the midpoint measures a symmetric transition's true
50%-crossing duration exactly, for any transition width or keying depth,
while the rails sit at the true mark and space levels (up to one hop of
quantization of the continuous crossing point). The rail-update split
below (MAN-213) lets transition samples pull `E_hi` slightly low, so slow
transitions read a little long. MAN-213 decision B3
(`docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md`)
measured the steady-state mark bias on a synthetic raised-cosine envelope:
none for ramps up to 6 hops (16 ms), 0 to +2 hops for 8–10-hop ramps and
+2 to +3 hops for a 12-hop ramp (a triangular pulse with no flat top),
against at most +1 hop with the pre-MAN-213 rails.
`keying_edge_placement_is_unbiased_across_depth_and_ramp` gates ±1 hop at
ramps of 2 and 6 hops and `0..=3` hops at 12.

Initialization, from the first 375 hops (1 s) after ACTIVE:
`E_hi = Q90(a)`, `E_lo = max(Q10(a), 1e-6)`. If `E_hi / E_lo < 2` (< 6 dB
apparent keying depth) the track stays in a *pre-decode* state and
re-attempts initialization every 1 s; no elements are emitted (prevents
decoding carriers/noise).

Per-hop update — **[DEVIATION]** (MAN-213,
`docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md`, superseding
MAN-103's D5 band-gated rail update). The key-decision band (§3.3) does
**not** gate rail updates: under that rule a mark that fades to about half
its pre-fade amplitude sits inside the band, so neither rail updates, `E_hi`
freezes at its pre-fade level, the key never goes down and the faded marks
are lost. Every sample instead updates exactly one rail, split at the
geometric mean `T_cls = sqrt(E_hi · E_lo)` — the pre-MAN-103 classification.
On top of that split, a **fade re-anchor**: a sustained run of at least
`debounce_hops` (§3.3, 12 ms) consecutive samples between the old key-down
threshold `1.25 · T_cls` and the key-down bound `mid + half` is a faded mark,
and sets `E_hi` to their mean, so the band follows the fade down. This
restores the pre-MAN-103 `1.25·T` fade margin without moving the key
decision itself (§3.3 is unchanged). All of `mid`, `half` and `T_cls` are
computed from the rails before this hop's update:

```
mid = (E_hi + E_lo) / 2 ; half = hyst_frac * (E_hi - E_lo)
T_cls = sqrt(E_hi * max(E_lo, 1e-6))
if a[m] > T_cls: E_hi ← E_hi + α_hi · (a[m] − E_hi)
else:            E_lo ← E_lo + α_lo · (a[m] − E_lo)
# fade re-anchor: n consecutive hops with 1.25·T_cls < a ≤ mid + half,
# n ≥ max(debounce_hops, 1)  →  E_hi ← mean(a over those n hops)
```

The re-anchor fires on every hop while the run stays at or above
`debounce_hops` (the run is not reset when it fires); a `debounce_ms` that
rounds to 0 hops still needs one in-zone sample. The one-shot `A_ref`
re-estimation (§3.1) rescales the run's accumulated sum by the same factor as
the rails.

Time constants: `τ_lo = 500 ms` fixed
(`α_lo = 1 − e^{−2.667/500} = 0.00532`).
`τ_hi` is WPM-adaptive once speed is tracked (§4.1):
`τ_hi = clamp(5 · dit_ms, 100, 400) ms`, initial 200 ms. Rationale: `E_hi`
must ride QSB (fast) but average over several elements (≥ 5 dits) so a single
stretched dah doesn't drag it.

Floors: `E_hi ≥ 2·E_lo` is enforced after every update (if violated, set
`E_hi = 2·E_lo`); prevents rail collapse during long silences. It is also the
lowest value the fade re-anchor can drive `E_hi` to.

### 3.3 Key decision with hysteresis and debounce

- **[DEVIATION]** (MAN-103): key-down when `a[m] > mid + half`; key-up when
  `a[m] < mid - half` (recomputed from the current rails, after §3.2's
  update); between the two bounds the previous state holds. This band is the
  **key decision only** — it does not gate §3.2's rail updates (MAN-213).
  `hyst_frac = 0.15` (band = 35%..65% of the keying depth `E_hi − E_lo`).
  Replaces the prior `1.25·T` / `0.80·T` multiplicative band: that band was
  symmetric in the *log* domain, not the linear one, so it reintroduced the
  very rise/fall asymmetry an unbiased threshold placement removes. For a
  transition that is symmetric in time (true here: the PFB prototype filter
  is linear-phase, and the testkit's raised-cosine keying edges are
  symmetric), the rise-crossing delay equals the fall-crossing delay *iff*
  the up/down thresholds are placed symmetrically about the amplitude
  midpoint — and then the measured mark equals the true 50%-point mark
  exactly, for any transition width and any keying depth, as long as the
  rails sit at the true levels, up to one hop of quantization (§3.2 states
  the slow-ramp residual its rail-update split leaves). `hyst_frac` is
  purely a noise-immunity knob: the underlying timing is provably
  independent of the band width (confirmed by a real-pipeline sweep over
  0.05–0.45 that left on-centre WPM readings unchanged).
- **Debounce:** a run (mark or space) shorter than **12 ms (≈ 4.5 hops)** is
  merged into its neighbors (the two adjacent runs and the short run become
  one run of the neighbors' polarity). 12 ms ≈ half a dit at 50 WPM — nothing
  legitimate is that short.

### 3.4 Element stream

Output of this stage: alternating `Mark(duration_hops)` /
`Space(duration_hops)` events, timestamped by the sample counter of their
leading edge. A space is not emitted until the next mark begins (open-ended
trailing space is flushed as a word boundary by the 7-dit timeout rule,
§4.2).

---

## 4. Element classification & Morse decoding

### 4.1 Online 2-means speed tracking (marks)

State: `μ_dit`, `μ_dah` in ms. Boundary between clusters:
`B = sqrt(μ_dit · μ_dah)` (geometric mean ≈ `1.73·μ_dit` at nominal 3:1).

**Initialization** — after the first **5 marks**: sort durations;
find the largest ratio gap between consecutive sorted values. If
`max/min ≥ 2.0`, split at the largest gap: `μ_dit` = mean below,
`μ_dah` = mean above. Otherwise (all one cluster — e.g. `EEE` or `TTT`):
`μ_dit = mean`, `μ_dah = 3·μ_dit` provisionally, flagged *unconfirmed* until
a mark lands ≥ `2·μ_dit` (then it re-anchors: `μ_dah = that duration`).

Special case: an all-dah opening (plausible for `CQ` at the margin) corrects
itself via the constraint clamp below the first time a real dit arrives —
the dit falls far below `μ_dit`, gets assigned to the dit cluster, drags it
down, and the clamp re-anchors `μ_dah`.

**[DEVIATION] (MAN-271)** — at a **genuine end-of-signal** `finish()`
(stream EOF, or a HangExpired/Silent closure, where a sustained real
on-air gap has been observed), a tracker that saw **2–4 marks**
initializes from them with exactly the split rule above, and the runs
buffered while the quorum was unfilled replay through the ordinary
pipeline; the trailing truncated space teaches the sender timing nothing
(same rule as `finish`'s MAN-315 truncation handling). Without this, a
short single transmission (2-character text at 34–39 WPM, whose leading
element is consumed before the decoder exists) ends with zero decoded
characters. `finish_speed_only` — the bookkeeping close path
(Merged/Evicted) — never bootstraps: fabricating characters there was the
PR #154 defect. One mark carries no ratio to split and bootstraps
nothing; `Hsmm`, which never feeds `SpeedTracker`, is unaffected. See
docs/DECISIONS/2026-10-16-man271-short-transmission-decode.md.

**Update** — per mark of duration `d`: assign to the nearer cluster in log
space (i.e. `dit` iff `d < B`), then EMA the assigned centroid:
`μ ← μ + 0.15·(d − μ)`.

**Constraints** — after every update enforce `2.2 ≤ μ_dah/μ_dit ≤ 6.0`
(weighted keying and Farnsworth stay inside this window); on violation,
re-anchor `μ_dah = r·μ_dit`, where `r` is the sender's learned dash/dot
ratio (§4.1b; both mark classes need ≥ 3 observations, clamped to the same
window) and `3.0` before that. **[DEVIATION]** (MAN-315 D5,
`docs/DECISIONS/2026-10-11-man315-sender-timing.md`): the ceiling was
`4.5` and the target always `3.0`. Heavy-dash senders sit at or above `4.5`,
so a dit cluster seeded by a noise fragment never healed: the re-anchor
kept snapping `μ_dah` back to `3·μ_dit` (measured: dit stuck at 20 ms for
weight 4 and 5). Clamp `μ_dit` to `[20 ms, 150 ms]`
(60 WPM .. 8 WPM); the reported PARIS WPM is `1200 / dit_estimate`
(§4.1a's symmetric correction of `μ_dit_ms`), EMA-smoothed with
`α = 0.1`.

**Drift/regime change** — if 12 consecutive marks assign to a single cluster
*and* their coefficient of variation < 0.35 *and* their mean is off that
centroid by > 40 %, the operator has changed speed (QRQ/QRS): reinitialize
from the last 5 marks. (Plain EMA tracking already follows ≤ ~20 % gradual
drift; this rule catches step changes.) A regime-change reinit also resets
the §4.1a gap centroid below (a stale correction from the old speed regime
must not carry into the new one), and the sender timing model (§4.1b,
MAN-315 D7): its statistics were learned in the old scale's dits.

#### 4.1a WPM reporting: symmetric mark/gap dit-period estimate

**[DEVIATION]** (MAN-103, `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`):
the PARIS WPM report does not use `μ_dit` directly. A keying-edge crossing
only moves the mark/space *boundary* — it neither creates nor destroys time
— so `μ_dit + μ_egap = 2 · true_dit` regardless of where that boundary sits.
This holds even for a *correctly-placed* §3.2/§3.3 threshold, because the
testkit's (and any real transmitter's) raised-cosine keying edge is
"contained inside the element": the true 50%-point mark is one full rise
time shorter than its nominal keyed length, and the following gap is
exactly that much longer. A threshold-placement fix alone (§3.2/§3.3) removes
the *SNR/offset-dependent* error term but leaves this *constant*
transmitter-shaping term untouched — left alone, it reads high by roughly
one rise time's worth of WPM at every speed.

Track a second EMA centroid, `μ_egap` (SPEC §9 `cluster_alpha`), over gaps
classified `InterElement` (§4.2), and **only** those. Gaps the §4.2 flush
safety net resolves directly are never element gaps (the safety net fires at
`7·μ_dit`); folding them in here would pin `dit_estimate` at its
`+DIT_BIAS_CAP_FRAC` cap and read WPM ~26 % low. §4.2's flush note feeds
those gaps to the *Farnsworth long-gap* statistics, which is a different
estimator. The dit period used for reporting is:

```
delta = clamp(0.5 * (mu_dit - mu_egap), -DIT_BIAS_CAP_FRAC * mu_dit, +DIT_BIAS_CAP_FRAC * mu_dit)
dit_estimate = clamp(mu_dit - delta, DIT_CLAMP_MS)
wpm_raw = 1200 / dit_estimate
```

with `DIT_BIAS_CAP_FRAC = 0.35`, a two-sided cap (bounding a runaway if
mark/gap pairing ever breaks down, e.g. element gaps swallowed by the 12 ms
debounce at extreme WPM). `μ_dit` itself (and its boundary `B`) stays
uncorrected: §4.3's beam likelihoods and §3.2's `τ_hi` want a centroid
consistent with the marks actually being classified. Gaps sit on the other
side of the same boundary, so §4.2 measures them in `dit_estimate` too
(**[DEVIATION]**, MAN-315 D3): in `μ_dit` units the rise time made every gap
read long. Before any element gap has been observed, `dit_estimate = μ_dit`
(no correction). Element gaps buffered during tracker warm-up (the first 5
marks) feed `μ_egap` when they are replayed (MAN-315 D3).

The mark that makes the tracker ready seeds the reporting EMA *before* that
replay runs, so the seed itself is still mark-only. **[DEVIATION]** (MAN-265):
immediately after the warm-up replay, the seed is replaced outright with
`1200 / dit_estimate` — an assignment, not an EMA fold, because this is still
the first sample (the replayed marks were already counted; only their gaps
arrived late). A transmission that ends at the boundary — a beacon ID, a
single exchange — reports the gap-corrected speed; every later live mark
keeps the normal `α = 0.1` smoothing steps on top of the corrected seed.

#### 4.1b Sender timing model

**[DEVIATION]** (MAN-315, `docs/DECISIONS/2026-10-11-man315-sender-timing.md`):
each track learns its own sender's timing (`SenderTiming` in
`crates/manta-decode/src/timing.rs`) instead of assuming 1 / 3 / 1 / 3 / 7.
Five classes, each an EW mean and variance **in dits of `dit_estimate`**
(§4.1a) at observation time, with an observation count:
`d = x − mean; mean += α·d; var = (1−α)·(var + α·d²)`, `α = cluster_alpha`;
the first sample sets `mean = x`, `var = 0`.

| Class | Fed by |
|---|---|
| dot / dash | the marks of a character decoded as a plain `Char` whose pattern has as many elements as there were marks, labelled by that pattern; garbles, prosigns and length mismatches feed nothing |
| element gap | gaps classified inter-element (§4.2), live and replayed |
| char gap | closed gaps classified inter-character |
| word gap | closed gaps classified inter-word, and flushed gaps at their closed length (flush length + the space that follows, or the flush length alone when the next mark absorbs a remainder shorter than the demod's debounce window); a gap ≥ `max(15, 4·char-gap mean)` dits is a pause and is not learned |

Uses: §4.1's ratio re-anchor target and §4.2's sender word boundary.
MAN-111 (scored boundaries) and MAN-113 (WPM hypotheses) read the means
and variances; this section uses only the means. A §4.1 drift re-init
resets every class. Legacy and EdgeLegacy only; the HSMM engine (SPEC v2)
keeps its own per-token speed and learns none of this.

### 4.2 Gap classification (spaces)

Nominal thresholds in dits (`u = gap_ms / dit_estimate`, §4.1a; was
`gap_ms / μ_dit` before MAN-315 D3 — the safety-net flush threshold below
uses the same unit):

- `u < 2.0` → **inter-element** (within a character)
- `2.0 ≤ u < 5.0` → **inter-character**
- `u ≥ 5.0` → **inter-word**

The implementation matches this nominal `2.0` boundary (`CHAR_GAP_DITS` in
`crates/manta-decode/src/timing.rs`). It was lowered to `1.6` for a period
(`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`) to compensate for
§3.2's old geometric-mean threshold systematically inflating measured
`μ_dit`; MAN-103 fixed that threshold at its source (§3.2/§3.3), so the
compensation this deviation existed for is gone. The 2026-07-18 sweep
methodology was re-run against the corrected timing before restoring `2.0`
(500 cases x 2 independent seeds, at both the envelope and the full IQ layer;
table in `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`, harness
in `crates/manta-engine/tests/char_gap_sweep.rs`): `1.6` no longer buys
anything at either layer, and `2.0` is the middle of a flat region running
from `1.6` to at least `2.5`.

**Farnsworth decoupling** (ARCHITECTURE §5.3): run the same 2-means machinery
on the "long gaps" — those with `u ≥` the inter-character boundary above,
i.e. exactly the gaps this section does *not* call inter-element. The floor
and the character boundary are one boundary seen from two sides and must
always carry the same value; a floor below the boundary admits ordinary
element gaps (which measure `u ≈ 1.5`–`1.6` near 40 WPM) into the long-gap
clusters and drags the word threshold down onto real character gaps
(MAN-103). Yields `μ_cgap` (character gap)
and `μ_wgap` (word gap) when bimodal. Once ≥ 8 long gaps have been observed
and `μ_wgap / μ_cgap ≥ 1.8`, the word threshold becomes the geometric mean
`sqrt(μ_cgap · μ_wgap)` instead of the fixed `5.0` dits; the character
threshold stays at `2.0` dits (element/character confusion is speed-locked;
character/word spacing is what Farnsworth stretches).

**Sender word boundary** — **[DEVIATION]** (MAN-315 D4). When Farnsworth is
not active, the character/word threshold is the sender's own, from §4.1b:
`sqrt(char_gap · word_gap)` once both are learned, `char_gap · sqrt(7/3)`
with only the char gap, clamped to `[3.0, 12.0]` dits; before any char gap,
the nominal `5.0`. Precedence: an active Farnsworth pair, then the sender
boundary, then `5.0`. The inter-element/inter-character threshold stays
`2.0` (scoring it from the learned variances is MAN-111's). Measured
(envelope level, 25 WPM, 5 ms rise): letter gaps of 4.5 dits and word gaps
of 9 decoded `C Q T E S T W 1 A W` before; `CQ TEST W1AW` after.

A trailing space reaching `7·μ_dit` without a new mark forces character +
word flush immediately (don't wait for the next mark to close a word —
spots must not lag the transmission). **[DEVIATION]** (MAN-103 D8): this
flush resolves its gap *outside* the classification path above, so it must
separately fold that gap into the Farnsworth long-gap statistics
(`long_seen`, `μ_cgap`/`μ_wgap`) or the Farnsworth bootstrap can never
complete once `μ_dit` runs at its corrected (non-inflated) value — with the
old, inflated `μ_dit`, `flush_gap_dits · μ_dit` sat comfortably above real
Farnsworth character gaps and this never mattered; with the corrected value
it can drop below them, so the safety net would otherwise intercept nearly
every character gap before `classify` ever saw it.
The folded value is the flush threshold, not the gap's closed length (the
space is still open), so a pause between calls cannot inflate `μ_wgap`.
Under heavy Farnsworth spacing the character gap itself outruns the flush,
and that censoring hides the spacing: every character is flushed as its own
word. So the closed lengths of the last five gaps flushed after decoded
one-character words re-initialize the long-gap pair. A word of two or more
characters, or a flushed garble, clears that window. The rebuild happens
only on a clean split: the largest-ratio split leaves at least two gaps in
each cluster, max/min is `≥ 2`, and the clusters' nearest members differ by
`≥ 1.8` (MAN-213; MAN-103 D8 resolution). A rebuilt pair is discarded, and
the long-gap statistics restart, when a classified gap of at least `2.0` but
under `5.0` dits arrives before five confirming word gaps. An ordinary
character gap contradicts the rebuild's premise that every character gap
outran the flush. With `flush_gap_dits` below `5.0`, the upper bound is
the larger of `flush_gap_dits` and the rebuilt low cluster over `1.8`: a
character gap above both outran the flush and belongs to the low cluster, so
it is consistent with the rebuild. A confirming word gap, classified or
flushed, ends a decoded word of two or more characters. Gaps after decoded
one-character words built the pair, so they never confirm it. Under a false
rebuild, one-character words at ordinary spacing decode as one merged word,
so a pause after such a run does confirm it. After five confirmations the
rebuild stands, so a dit lost to a fade inside a character cannot discard
it (MAN-264).

### 4.3 Per-element likelihoods

Marks are modeled log-normally about their centroid with fixed log-domain
σ = 0.25:

```
ll(d | dit) = −(ln d − ln μ_dit)² / (2·0.25²)
ll(d | dah) = −(ln d − ln μ_dah)² / (2·0.25²)
```

(σ = 0.25 ⇒ ±28 % duration at 1σ — measured human keying jitter is 10–20 %,
QSB-induced edge erosion adds the rest. **This is the riskiest constant in
the spec**; it is config `decode.timing_sigma` and must be validated against
the golden corpus at M3.)

Spaces inside a character contribute no score (already classified by §4.2);
the character boundary decision itself is hard, not beamed — beaming gap
types explodes state for negligible gain at CW speeds.

### 4.4 Beam search over the Morse tree (width 4)

The code tree: root, dit = left child, dah = right child; nodes carry an
optional glyph. Standard table A–Z, 0–9, `. , ? / = + - ( ) @ : ; ' " _ $ !`
plus prosign terminal nodes: `AR` (`.-.-.`), `SK` (`...-.-`), `BT` (`-...-`,
same node as `=`, emitted as `=`), `KN` (`-.--.`, same node as `(`), `AS`
(`.-...`), `VE/SN` (`...-.`). Prosigns emit as text tokens `<AR>` `<SK>`
`<AS>` `<SN>` in the JSON stream and are dropped from the telnet-facing text.

**[DEVIATION — narrowed]** The beam is **character-local**: it resets to the
tree root at every character boundary, and inter-character continuity is
greedy (winning character is committed). ARCHITECTURE §5.4 could be read as a
transmission-length beam; word-level ambiguity is the validator's job
(cty.dat / SCP context in `manta-spot`), and a character-local beam makes
determinism and confidence bookkeeping trivial. Cross-character correction is
explicitly out of scope for the classical decoder.

Per character:

1. Start with one hypothesis: `(node = root, score = 0)`.
2. For each mark `d` in the character: every hypothesis branches to its
   dit-child (score `+ ll(d|dit)`) and dah-child (score `+ ll(d|dah)`).
   A branch into a nonexistent child (sequence longer than any code, > 7
   elements) is dropped; if *all* branches drop, the character aborts as
   garble (emits nothing, counts as a decode error for confidence).
3. Prune to the best **4** hypotheses by score after each mark.
4. At the character boundary: surviving hypotheses whose node has no glyph
   are dropped; if all four are glyphless, the character emits `?` with
   confidence 0. The winner is the highest-score glyph-bearing hypothesis.

**Error prosign:** a mark run of ≥ 6 dits-classified marks with no dah
(operator sending `........`) emits control token `<ERR>`; the validator
discards the current word buffer back to the previous word boundary.

### 4.5 Per-character confidence

Softmax over the final hypothesis scores `s₁ ≥ s₂ ≥ …` (the ≤ 4 survivors,
plus dropped-at-boundary hypotheses excluded):

```
c_char = exp(s₁) / Σᵢ exp(sᵢ)          (∈ (0, 1], =1 if single survivor)
c_char ← c_char · q,  q = clamp(SNR_2500 / 20 dB, 0.3, 1.0)
```

`q` folds channel quality in so that a clean-timed character in the mud never
reaches full confidence. Emitted per character in the decoder output stream.

`q`'s `SNR_2500` is still the demod's own §3.2 keying-rail estimate
(`Demod::snr_2500_db`), not `TrackMeta.snr_2500_db`'s §2.3 floor-based value
below — deliberately (MAN-102 / decision D3): the two were the same value by
coincidence before MAN-102, and are now intentionally decoupled, not merged.

### 4.6 Per-callsign confidence (consumed by `manta-spot`)

**[DEVIATION]** `r` is no longer strictly "on the track" -- MAN-166,
2026-09-09 (`docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md`,
`crates/manta-spot/src/gate.rs`). A real signal's `track_id` changes every
time its track closes and reopens (e.g. a 5s silence timer), which a
literal per-track `r` would reset on every churn regardless of whether the
same callsign was still genuinely repeating -- confirmed as a real bug
against a genuine 40m contest recording. `r` is counted per
(frequency-bucket, callsign) instead, which survives that churn; a decode
from a *different* track_id within a reasoned minimum gap of the most
recent one is still rejected as a likely concurrent duplicate (two tracks
decoding the same real transmission), while the same track_id always
counts (one decode stream can't decode the same instant twice) --
`RepetitionGate::record`'s own doc has the full mechanism.

For a candidate callsign of `n` characters with confidences `c₁..c_n`,
decoded `r` distinct times within the 90 s window:

```
c_call = (Π cᵢ)^(1/n) · (1 − 0.5^r)
```

Geometric mean (one garbled character tanks it, correctly) times a
repetition factor: `r=1 → 0.5`, `r=2 → 0.75`, `r=3 → 0.875`. The validator's
own adjustments (cty/SCP hits, per ARCHITECTURE §6) multiply on top of this
and are specified in `manta-spot`, not here. The first-spot gate requires
the pattern tier's count (2 for a common
pattern, 3 for a listed pattern, 5 for an unlisted pattern at the default
`normal` level; `minimal` requires 2 for every pattern, MAN-95); a
message already type-tagged `BEACON` by the context parse (ARCHITECTURE §6
step 1), or a callsign the operator has explicitly allowlisted (ARCHITECTURE
§6's Watch List), is exempt from this gate — never needs a second, distinct
decode (MAN-28). `r` still feeds `c_call` above unchanged, so a spot of
either kind still carries the `r=1` confidence penalty at the repetition
count it actually resolved with. An allowlisted callsign also bypasses
ARCHITECTURE §6 steps 1 (context parse -- tagged `SpotType::Unknown` when no
CQ/DE/UP/beacon pattern matched) and 2 (grammar/cty) entirely, and may spot
the instant its pattern completes, as before.

**Beacon emission timing (amended 2026-09-09, see
`docs/DECISIONS/2026-09-09-beacon-emission-deferred-to-track-close.md`):** a
non-allowlisted `BEACON`-tagged candidate no longer spots "on the first
decode" in the sense of immediately as it's parsed. It is captured (grammar/
cty/blocklist/notch checked immediately, as always) but held until the
track's true close (`TrackClosed`), and only then evaluated against the
track's true final reported speed (an implausibly fast final speed --
`manta-spot`'s `MAX_PLAUSIBLE_WPM`, a validator-local heuristic, not a SPEC
value -- permanently discards it; nothing here is retried). This was found
necessary in practice: a live WPM reading taken at the moment of first
decode is not yet the track's true, settled value, and gating emission on
it (in either direction) reopened exactly the noise-artifact false-positive
problem the heuristic exists to close. The repetition-gate exemption itself
is unchanged -- a non-allowlisted beacon still never needs a second, distinct
decode -- only the MOMENT of emission moved from "first decode" to "track
close." An allowlisted callsign is unaffected by this and still spots
immediately.

**"Distinct" (MAN-100).** Two decodes of the same callsign text on a track
count as separate repetitions toward `r` only when at least
`MIN_MESSAGE_WORD_GAP = 3` decoded words on that track separate them, **or**
at least `MIN_MESSAGE_TIME_GAP_SECONDS = 60` seconds of `sample_ts` separate
them (MAN-100 remediation C2). SPEC's own default payload template repeats
the callsign back-to-back within one transmission (`CQ CQ DE <CALL> <CALL>
K`, §7's payload note) — without the word-gap half of this rule, that
single, possibly fading-corrupted message alone could satisfy the tier's requirement
(2 for a common pattern). 3 words sits strictly between the one-word gap inside a
single message and the minimum five-word gap between two separate ones
(`<CALL> K CQ CQ DE <CALL>`). That word-gap reasoning assumes SPEC's own
payload template, though, and does not hold for a real, shorter ID (e.g.
"DE `<CALL>`", 2 words) — the time-gap half exists for exactly that case: 60
s comfortably covers a full "CQ CQ DE `<CALL>` `<CALL>` K" transmission even
at 8 WPM (this section's slowest supported speed, ~40 s for that template)
with margin, while staying well under the 90 s ledger/gate window itself.
The beacon/allowlist exemptions above are unaffected — they never consult
`r`'s distinctness rule at all. A short "DE `<CALL>`" ID repeated only
twice at ordinary (sub-60 s) cadence remains unspotted under this rule — an
accepted, bounded recall cost (MAN-100 remediation C2, quantified; V43),
not tightened further: any time-gap threshold low enough to rescue it would
also treat a single corrupted message's own doubled utterance as two
distinct messages, reopening the hole this rule exists to close.

**Cross-candidate variant arbitration (MAN-100), ARCHITECTURE §6 step 4b.**
Before a candidate spots, it is checked against every other decoded,
spottable-shaped word observed on the same track within the same 90 s
window. It is withheld if a confusable, better-supported rival exists —
"confusable" meaning a shared contiguous substring relationship, or a
shared prefix of at least 3 characters with edit distance ≤ 2 — where
"better-supported" means strictly more message-distinct repetitions (ties
broken by summed per-occurrence confidence), or the candidate being a
strict prefix of a rival that has been observed at all (≥ 1 message-distinct
repetition — shape alone decides a prefix-containment pair once the rival
exists, however little support it has). An earlier attempt (MAN-100
remediation C5) also required the rival to independently clear the same
tier's repetition floor a spottable candidate must, on the reasoning that a
single stray, garbled decode that happens to be a textual prefix-extension
of a well-supported candidate should not be enough on its own to veto it;
reverted in remediation round 3 because it excluded the ticket's own
measured case (a 3-rep truncation losing to a genuine, longer call that
had only a single observation on the track) — the two shapes are
numerically indistinguishable from the ledger alone, and the measured,
real case takes priority over the unmeasured, synthetic one that motivated
C5. Symmetrically, a rival that is itself a strict prefix of the
candidate never wins this comparison on repetition count alone (MAN-100
remediation C1) — shape decides a prefix-containment pair in both
directions, not just when the shorter form is being arbitrated. This
mechanism is purely subtractive: it can only withhold a spot the rest of
this section would otherwise emit, never produce one, and it never fires
against an operator-allowlisted callsign, one present in the bundled SCP
list, or a candidate already type-tagged `BEACON` (MAN-100 remediation C3 —
the same once-per-cycle reasoning as this section's own beacon
repetition-gate exemption above: a beacon's structurally low rep count
would otherwise let a confusable, fading-corrupted rival permanently
outrank it). The per-track ledger this arbitration reads evicts an entry
once its newest observation ages out of the 90 s window (MAN-100
remediation C6), so a long-lived track's key space stays bounded by what's
currently live rather than growing with track history. See
`manta-spot::variant`/`manta-spot::support` for the exact relation and
comparison, and the MAN-100 decision record for the measured rationale
behind the prefix-only asymmetry.

---

## 5. Decoder output

Per track, an ordered event stream:

```
CharDecoded    { track_id, sample_ts: u64, char: char | Token, confidence: f32 }
WordBoundary   { track_id, sample_ts: u64 }
SpeedUpdate    { track_id, wpm: f32 }          (emitted on ≥ 1 WPM change)
TrackMeta      { track_id, sample_ts: u64, snr_2500_db: f32, freq_centroid: f64 }  (1 Hz cadence)
TrackPromoted  { track_id, sample_ts: u64, freq_hz: f64 }  (detector-internal;
                 added post-freeze, 2026-09-09 — the exact hop a track is
                 promoted from CANDIDATE to ACTIVE, independent of whether the
                 decoder subsequently produces anything. See
                 docs/DECISIONS/2026-09-09-doctor-track-promoted-event.md.)
TrackClosed    { track_id, closure: SignalEnded
                                     | Bookkeeping { survivor_track_id: Option<u32> } }
                 (added post-freeze, MAN-19; `closure` PR #154 round 9,
                 MAN-174 — a track has closed and will never emit another
                 event under this track_id. Emitted only for a track that
                 produced at least one other event first, or that a
                 merged-away track named as its `survivor_track_id`. The
                 `closure` kind says what a consumer holding deferred
                 per-track evidence, such as manta-spot's pending Beacon
                 candidates (§4 amendment), does with it:
                   SignalEnded — `HangExpired`, or end of stream
                     (`finish()`): the signal is gone; deferred evidence
                     is judged now.
                   Bookkeeping { survivor_track_id: Some(s) } — `Merged`:
                     the identity may continue on track `s`; deferred
                     evidence moves to `s` and is judged at `s`'s own
                     close.
                   Bookkeeping { survivor_track_id: None } — `Evicted`,
                     `Silent`, `Rtty`, or a source discontinuity (MAN-73),
                     including a stop while the source is down (MAN-258):
                     tracking stopped with no successor; deferred evidence
                     is discarded and counted.
                 Missing in JSON, `closure` defaults to `SignalEnded`.)
TimingUpdate   { track_id, sample_ts: u64, dit_ms: f32, dot, dash, element_gap,
                 char_gap, word_gap }  (added post-freeze, MAN-315 — the
                 track's §4.1b sender timing; each class is
                 { mean_dits, var_dits2, n } or null before its first
                 observation, in dits of dit_ms = dit_estimate. On the
                 TrackMeta cadence when a class gained an observation since
                 the last one, and once at track finish if anything changed.
                 A drift re-init (§4.1b) that wipes timing an update already
                 reported counts as a change: the next update goes out even
                 with no new observation, wiped classes null, so a stale
                 update never stands as the track's last. Legacy/EdgeLegacy
                 only.)
```

`manta decode --json` also reports the primary track's last `TimingUpdate`
(without `track_id`/`sample_ts`) as the top-level `timing`, and
`manta decode --timing` prints it on stderr, after the summary line, as
`timing (dits, mean ± sd): dot … dash … element gap … char gap … word gap …`.

`TrackMeta.snr_2500_db` is §2.3's floor-based `S − F` estimate (peak-held
over the reporting interval, per §2.3), supplied by the detector layer that
owns the gate/floor state -- not the §3.2 keying-rail ratio §4.5's `q` uses
(MAN-102 / decision D3).

`TrackMeta.sample_ts` is the hop that produced it, so §6 rule 6's
`(sample_ts, track_id)` resequencing places it in its true chronological
position -- not a synthetic tie value -- among the same batch's
`CharDecoded`/`WordBoundary` events (MAN-102 review round 2, finding 1:
tying it to a synthetic `0` let a just-reported SNR retroactively attach to
characters decoded earlier in the same batch, with the effect's magnitude
depending on the caller's chunk size).

`TrackMeta.freq_centroid` (`freq_hz` in code) is the track's centroid at the
hop its `sample_ts` names. The detector captures it when it observes that
hop and carries it with the queued hop through the refiner's
`GROUP_DELAY_HOPS` delay (SPEC-decode-core-v2 §3), so, like `snr_2500_db`,
it does not depend on the caller's chunk size (MAN-211).

`sample_ts` is the input-stream sample counter (u64, monotonic from stream
start). Wall-clock time exists only at the spot-emission boundary
(`manta-server`), derived as `stream_start_time + sample_ts / fs` where
`stream_start_time` comes from config/file sidecar — never from `Instant::now()`
inside the decode path. A live source's disconnect/reconnect (MAN-73) reports
the missed span, in samples, via `IqSource::take_discontinuity()`; `listen()`
advances the sample clock by that count and starts a fresh track segment, so
`sample_ts` stays monotonic and wall-clock-true across the gap with no audio
spliced in. File replay never reports a discontinuity. An end of stream
reported with a discontinuity pending (MAN-258: a stop while the source is
down) closes the segment the same way and starts none.

---

## 6. Determinism requirements

A daemon run from an IQ file MUST produce byte-identical decoder output (and
therefore spot logs) across runs and platforms (ARCHITECTURE §9). Normative
rules:

1. **No RNG** anywhere in `manta-dsp` / `manta-decode`. (The testkit's
   jitter models use seeded `rand_chacha`; seeds are part of the test vector.)
2. **No wall clock** in the decode path (§5). All timers are hop/sample
   counters.
3. **Fixed iteration order:** tracks are processed in ascending birth order
   (track_id, a monotonic u32) each hop; channels in ascending index. Any map
   keyed by track/channel in an output-affecting path is a `BTreeMap` or
   sorted `Vec`, never a `HashMap` iterated.
4. **Float discipline:** all per-sample state is `f32` with a fixed operation
   order (no fast-math, no FMA-dependent reductions: the fold in §1.3 and the
   centroid in §1.4 accumulate in `f64` sequentially). `rustfft` is
   deterministic per-platform for power-of-two sizes; cross-platform FFT
   bit-equality is NOT assumed — the byte-identical requirement applies to
   like-for-like builds, and cross-platform equality is asserted at the
   *decoded-text* level (test vectors, §7), not the sample level.
   Replacement (§1.3 step 0) is a deterministic function of the input.
5. **Beam tie-break:** equal scores order by (element-sequence lexical order,
   dit < dah). Softmax computed with the max-subtraction trick, fixed order.
6. **Pool scheduling must not affect output:** decoder workers may run in any
   order, but each track's decoder is a pure function of its own input queue;
   emitted events are sequenced by `(sample_ts, track_id)` before the
   validator sees them. `TrackClosed` events sort after every other event of
   their batch; a merge loser's `TrackClosed` precedes its survivor's own
   `TrackClosed` in the same batch; other closures stay in `track_id` order
   (MAN-174).
7. **Several inputs (MAN-13):** each `[[input]]` runs rules 1–6 on its own
   thread. Timestamps share one clock at the highest input rate F: input
   *i*'s `sample_ts` maps to `offset_i + sample_ts × F / fs_i`, exact
   because every channelizer rate is 93.75 Hz × 2^k. Replay offsets are 0;
   live offsets are each input's open-completion time after the earliest.
   Input *i* of N numbers its tracks `1+i, 1+i+N, …`, so ids stay unique
   and `input = (track_id − 1) mod N`. Replay merges whole batches into the
   one validator in `(batch end time, input index)` order, so the same
   files give byte-identical output; live inputs merge in arrival order,
   so no input waits on another. Dedupe treats another input's report of a
   stored spot (same 300 Hz bucket, or within 93.75 Hz across a bucket edge,
   inside the 600 s window) as a duplicate, with no SNR or spot-type
   override; the stored time never moves backwards. See
   docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md.

CI enforces: same binary + same IQ file, 3 runs → identical SHA-256 of the
JSON spot log; and the §7 vectors on all platforms.

---

## 7. Golden test vectors (M0/M1 acceptance)

All generated by `manta-testkit`: text → keyed envelope (raised-cosine
edges, 5 ms rise/fall, per-element timing jitter σ = 8 % where stated) →
complex tone at the stated offset → impairments via `coppa-channel`
(`awgn(seed)`; Watterson via the streaming `WattersonChannel` API per
SPEC-watterson §6 — not the deprecated one-shot helper). SNR is quoted
**in 2500 Hz**.
Every vector: `fs = 96 000`, 120 s scene unless stated, fixed seeds recorded
in the fixture manifest. Text payload (unless stated):
`CQ CQ DE <CALL> <CALL> K` repeated for the scene duration.

| # | Name | Signal(s) | Impairment | Pass criteria |
|---|---|---|---|---|
| V1 | clean-20 | 20 WPM, +20 dB, offset +12.34 kHz, W1AW | AWGN only, no jitter | char accuracy = 100 %; 1 track; freq error ≤ 10 Hz |
| V2 | fast-35 | 35 WPM, +15 dB, JA1ABC | AWGN, jitter 8 % | char ≥ 99 %; WPM reported 35 ± 2 |
| V3 | slow-weak | 12 WPM, +6 dB, VK9DX | AWGN, jitter 8 % | char ≥ 95 %; callsign validated (≥ 2 reps) |
| V4 | fade-good | 25 WPM, +10 dB, DL1ABC | Watterson CCIR-good | char ≥ 95 % |
| V5 | fade-poor | 22 WPM, +3 dB, ZL2XYZ | Watterson CCIR-poor | char ≥ 80 %; callsign validated within 90 s |
| V6 | qsb-sine | 20 WPM, envelope ×(0.55 + 0.45·sin 2π·0.2t) (≈ +20→0 dB), K5ZZZ | AWGN | char ≥ 90 %; track survives (no CLOSED before end) |
| V7 | adjacent | 24 WPM @ +10.000 kHz and 28 WPM @ +10.150 kHz, both +15 dB, calls N1AA / N2BB | AWGN | exactly 2 tracks; both char ≥ 95 %; both freqs ± 15 Hz |
| V8 | pileup-50 | 50 signals, 10–35 WPM, −2..+25 dB, uniform over ±45 kHz, unique calls from fixture list | AWGN, jitter 8 % | ≥ 45/50 callsigns validated in 120 s; 0 bogus (non-fixture) callsigns spotted |
| V8w | pileup-50-fading | same scene as V8 | Watterson CCIR-poor, jitter 8 % | ≥ 90 % of signals with mean SNR ≥ +6 dB decoded with CER < 10 %; 0 bogus callsigns; 0 cross-channel ghost decodes |
| V9 | drift | 18 WPM, +12 dB, drift +50 Hz/min, EA8AAA | AWGN | 1 track (no split); char ≥ 90 %; final freq tracks within 15 Hz |
| V10 | farnsworth | 15 WPM chars / 25 WPM char-speed (Farnsworth), +15 dB, G4XXX | AWGN | char ≥ 95 %; word boundaries 100 % correct |
| V31 | decimated-clean-20 | Same scene as V1 (20 WPM, +20 dB, offset +12.34 kHz, W1AW), synthesized at 192 kHz then decimated to 48 kHz via `manta_dsp::decimate::Decimator` | AWGN only, no jitter | char ≥ 98%; 1 track; freq error ≤ 10 Hz |
| V51 | drift-refine | Same scene as V9 (18 WPM, +12 dB, drift +50 Hz/min, EA8AAA), decoded with `decode.refine_bw_hz = 30` (the narrowband refiner, MAN-168/MAN-194) via a `--config` TOML | AWGN only, no jitter (as V9) | 1 track (no split); char ≥ 90 %; final freq within 15 Hz; the refine-on report differs from the refine-off control (the config reaches the pipeline) |

V31's freq-error bound was 25 Hz until MAN-114. The dB-parabola spot
frequency measured ~17.4 Hz here, and ~15-17 Hz on native 48 kHz and 192 kHz
control renders of the same scene, which was read as a channel-table-size
effect. MAN-114 found it is not: the error depends on the signal's
fractional position inside its channel plus AWGN on the parabola's weak
neighbour bin, and every table shows it (V1 and V31 share the +12.34 kHz
offset, so the same fractional position). The §1.4 phase estimator measures
1.4 Hz on V31 (`crates/manta-cli/tests/golden_decimated_capture.rs`).

V51 (MAN-210) gates the narrowband refiner's refine-enabled path
(`Track::decoder_input`'s delay-compensated hold-back/drain, MAN-194)
end-to-end: V9's drift crosses a channel boundary mid-scene, so the
reset-drain branch fires through the real channelizer, and the decode ends
in `TrackManager::finish`'s end-of-stream drain. Like V31 it is a standalone
test in `crates/manta-cli/tests/golden_v7_v9_v10.rs`
(`v51_drift_with_refiner_enabled_passes_end_to_end`), not part of the
`manta-testkit::vectors` V1–V10 fixture table. This is the end-to-end
evidence any future change that turns the refiner on by default must keep
green (found by MAN-194's final review: V4/V5/V8w never execute the
refine-enabled branch, so their re-verification could not cover it).

The drain protocols themselves cannot be pinned from the CLI report V51
asserts on (Codex review, PR #303): measured on this scene, the
end-of-stream backlog is trailing silence — the last keyed character
closes ~0.24 s before EOF — and a backlog dropped at a reset or at EOF
costs at most ~6 × `GROUP_DELAY_HOPS` hops of envelope, inside V51's CER
bar. They are therefore asserted one layer down, on the same real scene:
`real_drift_scene_drains_refiner_backlog_at_resets_and_eof`
(`crates/manta-engine/src/track.rs`) drives `render_v9_drift`'s fixture
through the real channelizer and `TrackManager` with the refiner on and
asserts that the drift's channel crossing resets the refiner with a
non-empty backlog, that delivery is exactly-once (decoder `hop_count` +
`pending` + `refiner_backlog` equals the refine-off control's deliveries
at every hop — a backlog dropped or duplicated at any reset diverges
there), and that a non-empty backlog is still held when `finish()` runs
(whose delivery wiring is pinned by
`finish_drains_every_remaining_tracks_refiner_backlog_before_flushing`).

M0 = V1 passing end-to-end from a WAV file. M1 = V1–V6. V7–V10 and V8w gate M2
(multi-track engine). The RBN-parity corpus benchmark remains the M3 gate
(ARCHITECTURE §9) and is not redefined here. V31 gates variable-width capture
(issue #169); unlike V1–V10 it is a standalone test in
`crates/manta-cli/tests/golden_decimated_capture.rs`, not part of the
`manta-testkit::vectors` V1–V10 fixture table.

### 7.1 `manta-spot` validator vectors (M3 sub-project 1)

Unlike V1–V10 (testkit-synthesized IQ), these operate at the
`DecoderEvent`-stream level -- hand-built event sequences feeding
`Validator::ingest` directly, no IQ synthesis involved. V11-V15, V18-V50
are implemented in `crates/manta-spot/tests/golden_v11_v15.rs`; V16-V17
(operator suppression, MAN-31 -- orthogonal to this pipeline, see
ARCHITECTURE §6) in `crates/manta-spot/tests/golden_v16_v17.rs`; V32-V33
(non-gating RST/QRL? spot annotations, MAN-272) in
`crates/manta-spot/tests/golden_v32_v33.rs`.

| # | Name | Scenario | Pass criteria |
|---|---|---|---|
| V11 | context-parse | Each of `CQ <call>`, `CQ TEST <call>`, `CQ <contest> <call>` (e.g. `CQ WPX`; filler set per `manta-spot::context`), `TEST <call>`, `DE <call>`, `<call> UP`, `V V V <call>`, `<call> T` | Correct `SpotType` assigned per pattern family |
| V12 | bogus-prefix | Structurally-valid callsign with a prefix absent from cty.dat | 0 spots, even though grammar passes |
| V13 | scp-boost | Same callsign/confidences with vs. without SCP membership | `c_call` strictly higher when a member; absence never rejects |
| V14 | repetition-gate | 1 decode vs. 2 decodes of a common-pattern callsign within 90 s, non-beacon spot type | 1 rep never spots; the tier's requirement (2 for a common pattern) spots |
| V15 | dedupe | Repeat spot inside the 10 min window, then an SNR jump >= 6 dB | Suppressed inside the window; allowed after the SNR jump |
| V16 | bad-call blocklist | Callsign present vs. absent from the operator's bad-call list | Present → 0 spots; absent → spots normally |
| V17 | notched frequency | Track frequency inside vs. outside a notched range | Inside → 0 spots; outside → spots normally |
| V18 | beacon-repetition-exemption | 1 decode of a `V V V <call>` beacon pattern, track closed at a plausible speed | `BEACON`-tagged spot emits once the track closes -- repetition gate not applied regardless (MAN-28); emission TIMING moved to track-close 2026-09-09, see §4's amendment note -- no spot before `TrackClosed` |
| V19 | allowlist-bypass | A single decode of a callsign with an unallocated cty prefix, explicitly allowlisted | Spots despite failing grammar/cty and despite only 1 decode (MAN-28 Watch List) |
| V20 | allowlist-no-context | An allowlisted callsign decoded with no CQ/DE/UP/beacon framing at all | Spots, tagged `SpotType::Unknown` (MAN-28 Watch List, the primary NCDXF-beacon case) |
| V21 | allowlist-independent-of-context | A stale, already-attempted context match (e.g. `CQ K5ARH`, decoded once, never spotted) sits in the window when a different, freshly-allowlisted word arrives | The allowlisted word still spots -- context-match and allowlist candidates are evaluated independently, not one-or-the-other by priority (MAN-28 Watch List) |
| V22 | exempt-spot-waits-for-metadata | An allowlisted callsign decoded before the track's first `TrackMeta` event | No spot until real telemetry arrives; the pending candidate spots once metadata does arrive, on the next word boundary. A non-allowlisted BEACON candidate decoded before `TrackMeta` is captured once metadata arrives and spots only at `TrackClosed` (V18/V30; `power_step_beacon_retains_every_unattempted_occurrence_across_the_metadata_gate`) |
| V23 | allowlist-reclassification | An allowlisted call spots immediately with no context (type `Unknown`), then a trailing word completes a real context pattern (e.g. `<call> UP` -> `De`) | A second, corrected spot is emitted with the new type -- an already-attempted word is not permanently locked to its first type |
| V24 | reclassification-reps-reuse | "DE K5ARH" decodes once (reps=1, held back by the repetition gate), then a trailing `CQ` token reclassifies the same word from `De` to `Cq` | Still 0 spots -- the reclassification reuses the word's existing rep count rather than recording a second decode |
| V25 | metadata-retries-pending-candidate | A `TrackMeta` event arrives for a track with a pending allowlisted candidate (held back by V22) and no further word ever completes | The candidate is retried and spots as part of the `TrackMeta` ingest itself, not left waiting on a `WordBoundary` that may never come. A pending non-allowlisted BEACON candidate is captured at that `TrackMeta`, not spotted; it spots only at `TrackClosed` (V18/V30) |
| V26 | reclassification-never-downgrades | "DE K5ARH" spots as `De`; 15 more words push "DE" out of the 16-word window while "K5ARH" remains | No spot reverts to `Unknown` -- reclassification only ever promotes a word's type, never downgrades one that already earned a contextual type |
| V27 | reclassification-never-downgrades-between-types | "CQ DE K5ARH" spots as `Cq`; 15 more words push both "CQ" and "DE" out of the window while "K5ARH" remains | No spot reclassifies to `De` -- the same aging-out bug shape as V26, for a pair of two contextual types instead of type-vs-`Unknown` |
| V28 | reclassification-still-accepted | "DE K5ARH" spots as `De`; a `CQ` token then arrives as a genuinely new trailing word (not via aging) | A second spot promotes it to `Cq` -- V26/V27's fix rejects aging-driven changes specifically, not reclassification in general |
| V29 | provenance-bound-to-occurrence | "CQ DE K5ARH K CQ DE K5ARH" repeats DE-K5ARH across two genuinely separate messages (MAN-100 Scenario 2 requires the gap); the newest K5ARH spots as `Cq` after 2 reps, then "CQ" and the first "DE" age out while the second "DE K5ARH" remains | No spot reclassifies to `De` -- provenance is bound to the exact word occurrence `evaluate_candidate` selects, not whichever occurrence the regex matched first |
| V30 | power-step-beacon-exemption | 1 decode of a `<call> T` power-step beacon pattern (MAN-37), track closed at a plausible speed | `BEACON`-tagged spot emits once the track closes, gate not applied regardless -- same exemption V18 proves for `V V V <call>`, extended to the power-step pattern; emission timing per V18's amendment note |
| V32 | rst-on-spot | A track decodes `TU 5NN` before its callsign spots; a later `339` replaces it; a report aged out of the 16-word window is still carried | The spot carries `rst = "599"`, then `"339"`; aged-out still `"599"`; no report → `null` |
| V33 | qrl-query-flag | A track sends `QRL?` before its callsign spots; bare `QRL`; a merge loser's `QRL?`; a closed track's `QRL?` | `qrl_query` true only in the first case; a merge survivor and a new track carry neither annotation (MAN-272 D5) |
| V38 | variant-arbitration | A track decodes both a callsign and a confusable, less-supported variant of it (truncation or shared-prefix near-miss) inside one 90 s window -- variants V38b (per-track scoping) and V38c (a well-supported real call is not suppressed by a 1-rep head-merge artifact) | Only the better-supported candidate spots; arbitration is per track, and never fires against a form that could not itself be spotted |
| V39 | same-message-repetition | One `CQ CQ DE <CALL> <CALL> K` transmission, then a second, genuinely later one | The first message's doubled call alone never satisfies the repetition gate (2 for its common pattern); the second message completes it |
| V40 | truncation-arrives-first | A strict-prefix truncation clears the repetition gate on a track before the genuine, longer call has any support at all, which then appears | The genuine call still spots once observed -- the prefix-containment asymmetry fires regardless of arrival order (MAN-100 remediation C1) |
| V41 | short-id-wide-time-gap | A 2-word ID ("DE `<CALL>`") repeated 80 s apart -- below `MIN_MESSAGE_WORD_GAP` but past `MIN_MESSAGE_TIME_GAP_SECONDS` | Still clears the repetition gate as two distinct messages (MAN-100 remediation C2) |
| V42 | beacon-exempt-from-arbitration | A confusable rival of a `BEACON`-tagged candidate reaches more reps than the genuine, once-per-cycle beacon | The genuine beacon still spots -- `BEACON` candidates are exempt from step 4b arbitration (MAN-100 remediation C3) |
| V43 | short-id-ordinary-cadence-unspotted | A 2-word ID ("DE `<CALL>`") repeated only twice, 20 s apart -- below both `MIN_MESSAGE_WORD_GAP` and `MIN_MESSAGE_TIME_GAP_SECONDS` | Not spotted -- an accepted, bounded recall cost (MAN-100 remediation C2, quantified), not tightened further |
| V44 | 1-rep-rival-still-wins-by-shape | The literal, measured V8w track-90 shape: a 3-rep truncation ("W6JQ") vs. its genuine, longer form ("W6JQA") observed only once on the track | The truncation is withheld -- shape decides a prefix-containment pair once the rival has been observed at all, regardless of how few reps it has (MAN-100 remediation round 3; a rival-side rep floor tried in remediation C5 excluded this exact case and was reverted) |
| V45 | non-callsign-convention | Each listed CW convention (`5NN`, `3NN`, `599`, `TEST`, …) sent as a CQ/DE/beacon candidate, some of which match an allocated cty.dat prefix (`5N` Nigeria) | 0 spots — rejected by grammar regardless of cty.dat (MAN-105) |
| V45b | convention-never-vetoes | A track that repeats `5NN` exchanges, then a real non-SCP call ending in `5NN` (`HA5NN`) | The real call spots — the convention never enters the MAN-100 support ledger (MAN-105) |
| V46 | common-pattern-two-reps | `DE K5ARH K` with `+K5@@@` in the pattern table, on the same track | No spot at 1; spots at 2 message-distinct repetitions |
| V47 | listed-pattern-three-reps | `DE K9ABC K` with `K9@@@` listed without `+`, on the same track | No spot at 2; spots at 3 message-distinct repetitions |
| V48 | unlisted-pattern-five-reps | `DE W4KTNL K` with its pattern absent, on the same track | No spot through 4; spots at 5 message-distinct repetitions |
| V49 | minimal-level-is-flat-two | V48's unlisted pattern at `validation_level = "minimal"` | No spot at 1; spots at 2 |
| V50 | exemptions-ignore-pattern-tier | An unlisted-pattern allowlisted call, then a `V V V <call>` beacon | Allowlisted call spots at its first decode; beacon spots at track close with confirmed plausible speed |

### 7.2 Static-crash vectors (VC1–VC3, MAN-312)

These gate §2.7. Each crash is complex white Gaussian noise `width` long,
`level` dB above the scene's unit AWGN power, added to the rendered IQ by
`manta-testkit::crash` (a separate adder, so no existing scene or golden
vector changes) from ChaCha8 seeds pinned in that module; every MAN-312
number depends on that generator and those seeds. The vectors are not
part of `manta gen`.

| # | Name | Signal(s) | Impairment | Pass criteria |
|---|---|---|---|---|
| VC1 | crash-only-band | none (10 s; 48/96/192 kHz; KiwiSDR-shaped ±5 kHz) | one crash at 5 s, 1–500 ms, +6…+60 dB | 0 tracks under `full`/`starts-only`; each crash ≥ +20 dB counted once |
| VC2 | crash-mid-character | V1's W1AW at +15 dB, 60 s | 24 crashes, 5 ms, +20/+40/+60 dB | 1 track closed only at end of stream; dits ≤ crash-free; W1AW spotted (`full`, legacy and HSMM); `starts-only` decodes the crash |
| VC3 | pileup-50-fading-crashes | the V8w scene | 47 crashes (train seed `0x3333`), 1–10 ms, +20…+60 dB | 0 bogus; ≥ 20/50 validated; ≤ 60 tracks promoted |

VC1 and VC2 run in-crate, in `crates/manta-engine/src/static_crash_tests.rs`,
alongside the dense-band, long-noise, level-step, silence lead-in,
chunk-invariance and `off`-identity checks; VC3 runs end to end through
`manta decode --json` as `vc3_pileup_fading_with_crashes_spots_no_bogus_callsigns`
in `crates/manta-cli/tests/golden_v8_v8w.rs`; the generator and the
vector definitions live in `crates/manta-testkit/src/crash.rs`. VC3's
bogus count depends on the crash draw under every `crash_filter` value
(five train seeds gave 0, 1, 4, 1 and 1 under `full`), so its seed is
pinned; promotions and validated calls are the robust discriminators
(see the decision record).

### 7.3 Sender-weighting vectors VW1–VW5 (MAN-315)

Senders who do not key ideal 1:3 / 3 / 7 timing, decoded on the default
`legacy` engine (`crates/manta-cli/tests/golden_vw.rs`; `manta gen vw1` …
`vw5`). All: 96 kHz, 14 MHz centre, offset +12.34 kHz unless stated, 5 ms
rise, AWGN, looped text, 60 s unless stated. `dit_units`/`element_gap_units`
set the dot and the element gap in dits (1.0 standard). Learned timing is
read from the track's last `TimingUpdate` (§5).

| # | Name | Signal(s) | Impairment | Pass criteria |
|---|---|---|---|---|
| VW1 | heavy-dash | 25 WPM, dash 4.0 dits, gaps 3/7, +10 dB, `CQ TEST K1ABC K1ABC TEST`, noise seed `0x3153` | AWGN, jitter 8 % (seed 3) | first exact `K1ABC` among the first 10 words, every word after it a keyed word; CER ≤ 0.25; learned dash 3.6–4.4 dits |
| VW2 | long-gaps | 25 WPM, 3:1, char gap 4.5, word gap 9, +15 dB, `CQ TEST W1AW` | AWGN, jitter 8 % (seed 3) | CER ≤ 0.06; word boundaries ≥ 95 %; learned char gap 4.0–5.0, word gap 8.0–10.0 dits |
| VW3 | bug-key | 22 WPM, dot 0.85, dash 4.5, element gap 1.0, gaps 3.5/8, +12 dB, `CQ DX DE W9BUG W9BUG K` | AWGN, jitter 10 % (seed 5) | CER ≤ 0.08; learned dash > 4.0 dits |
| VW4 | heavy hand-key | 18 WPM, dot 1.2, element gap 0.8, dash 3.6, gaps 4/9, +15 dB, `CQ CQ DE G4HND G4HND K` | AWGN, jitter 12 % (seed 6) | CER ≤ 0.08 |
| VW5 | keyboard + heavy, adjacent | A: 28 WPM ideal 3:1, 3/7, `CQ TEST N1AA N1AA TEST`; B: 22 WPM dash 4.0, gaps 4.5/9, offset +12.34 kHz + 3 × 93.75 Hz, `CQ TEST N2BB N2BB TEST`; both +20 dB; 90 s | AWGN; B jitter 5 % (seed 7) | exactly 2 decoded tracks, each within half a channel (46.875 Hz) of its signal; CER ≤ 0.06 each; A learned dash 2.6–3.4 and char gap 2.5–3.5, B dash 3.6–4.4 and char gap 4.0–5.0, on different `track_id`s |

---

## 8. Module map (where each section lands)

| Spec section | Crate::module |
|---|---|
| §1 channelizer, prototype | `manta-dsp::pfb`, `manta-dsp::proto` |
| §1.4 interpolation | `manta-dsp::centroid` |
| §2 floor + gate + state machine | `manta-dsp::floor`, `manta-engine::track` |
| §2.6 RTTY recognition | `manta-dsp::fsk` |
| §2.7 static-crash detection | `manta-dsp::crash`, `manta-engine::track` |
| §2.8 non-CW rejection | `manta-dsp::comod`, `manta-engine::track` |
| §3 demod | `manta-decode::envelope` |
| §4.1–4.2 timing | `manta-decode::timing` |
| §4.3–4.5 beam decode | `manta-decode::beam`, `manta-decode::tree` |
| §5 events | `manta-decode::events` |
| §7 vectors | `manta-testkit::vectors` |
| §7.2 static-crash vectors | `manta-testkit::crash` |

## 9. Configuration keys

Every configurable constant above, with its real code default. The block
below is valid TOML that `manta` loads as-is (`manta decode --config` on
it decodes byte-identically to no config at all; `docs_consistency.rs`
checks both that and that each value equals the code default). Keys the
code still hard-codes are listed commented out, marked `not configurable
yet` -- the loader rejects them with that message rather than ignoring
them.

```toml
[detector]
# Bounds: 0 <= off_snr_db <= on_snr_db <= 100; every *_ms in [0, 3600000],
# converted with ms_to_hops (§1.1); confirm_ms/hang_ms/gc_ms must round to
# >= 1 hop; track_cap >= 1.
# Rise threshold, dB SNR. 12.0, not SPEC v1's 6.0: raised by
# docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md item 2 (§10).
on_snr_db = 12.0
off_snr_db = 3.0
confirm_ms = 50                     # 19 hops
hang_ms = 5000                      # 1875 hops
gc_ms = 30000                       # 11250 hops
warmup_ms = 25                      # 9 hops
# Max concurrent tracks (ARCHITECTURE §4); 1200 per
# docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md.
track_cap = 1200
# MAN-171: a channel whose track closed Silent may not spawn a new
# CANDIDATE for this long.
silent_respawn_cooldown_ms = 30000  # 11250 hops
crash_filter = "full"               # "full" | "starts-only" | "off"; §2.7 (MAN-312)
# MAN-314: two-tone FSK (RTTY) recognition, §2.6. rtty_dwell_ms >= 1000.
rtty_filter = true
rtty_dwell_ms = 20000   # 7500 hops; whole 1 s blocks
# MAN-313: non-CW rejection, §2.8. unkeyed_close_ms >= 1000.
non_cw_filter = true
unkeyed_close_ms = 10000.0   # 3750 hops; qualification timeout and unkeyed close
# floor_quantile = 0.25       # not configurable yet: compile-time constant in manta-dsp::floor
# floor_window_ms = 10000     # not configurable yet: compile-time constant in manta-dsp::floor
# block_channels = 32         # not configurable yet: compile-time constant in manta-dsp::floor
# block_allowance_db = 3.0    # not configurable yet: compile-time constant in manta-dsp::floor

[decode]
# SPEC-decode-core-v2.md §7 lists the v2 evidence/noise/HSMM keys.
engine = "legacy"           # "legacy" | "edge-legacy" | "hsmm"; --engine overrides
timing_sigma = 0.25
beam_width = 4
debounce_ms = 12
# Key-decision band half-width as a fraction of keying depth (§3.3,
# MAN-103); replaces v1's hyst_up/hyst_down. Must be > 0.0 and < 0.5.
hyst_frac = 0.15
tau_lo_ms = 500
tau_hi_bounds_ms = [100, 400]
flush_gap_dits = 7.0
# mu_ratio_bounds = [2.2, 6.0]  # not configurable yet: compile-time constant in manta-decode::timing (MAN-315: was 4.5)
# char_gap_dits = 2.0           # not configurable yet: compile-time constant in manta-decode::timing (every engine, MAN-103)
# word_gap_dits = 5.0           # not configurable yet: compile-time constant in manta-decode::timing
# cluster_alpha = 0.15          # not configurable yet: compile-time constant in manta-decode::timing

[input]
# The source. Omit `type` (and every source key) to use the default audio
# device, or when command-line flags pick the source. Each type takes only
# its own source keys (required ones marked *):
#   audio: device
#   file:  path*, iq, realtime, loop
#   kiwi:  host*, freq_hz*, port, password
#   soapy: driver*, freq_hz*, rate_hz*, gain_db   (build with --features soapy)
#   hpsdr: host*, freq_hz*, rate_hz*, port        (included in every build)
#   ka9q:  group*, port, ssrc, interface
# Any source flag on the command line (--device, --source, --kiwi-host,
# --soapy-driver, --hpsdr-host, --ka9q-group) replaces a typed [input]
# table whole, its shared keys below included. Example:
# type = "kiwi"               # audio | file | kiwi | soapy | hpsdr | ka9q
# host = "<your-kiwi-host>"   # kiwi, hpsdr
# port = 8073                 # kiwi (default 8073), hpsdr (default 1024), ka9q (default 5004)
# freq_hz = 7030000.0         # kiwi, soapy, hpsdr: RF center frequency, Hz
# password = ""               # kiwi (default "")
# The other types' keys:
# device = "USB Audio"        # audio: device-name substring; omit for the default device
# path = "capture.wav"        # file: relative to this file's directory
# iq = false                  # file: true for a raw complex-IQ WAV (--source-iq)
# realtime = false            # file: replay at the recording's own pace (run only; --realtime)
# loop = false                # file: replay again from the start each time it ends, until stopped; implies realtime (run only; --loop)
# driver = "driver=rtlsdr"    # soapy: SoapySDR device args
# rate_hz = 192000.0          # soapy, hpsdr: sample rate, Hz
# gain_db = 30.0              # soapy: omit for the device's AGC
# group = "239.101.6.1"       # ka9q: the radiod channel's multicast data group, IPv4 address or host name (--ka9q-group)
# ssrc = 14100000             # ka9q: the channel's SSRC, 0 to 4294967295; omit when the group carries one channel (--ka9q-ssrc)
# interface = "192.168.1.20"  # ka9q: local IPv4 address to join the group on; omit for the kernel's choice (127.0.0.1 is always joined too) (--ka9q-interface)

# Shared keys, valid with any type or none:

# Per-source oscillator drift correction, ppm; range [-1000, 1000]
# (`manta_spot::calibration_factor_from_ppm`). §1.4, MAN-29. `decode`
# applies it too. --freq-correction-ppm and
# MANTA_INPUT_FREQ_CORRECTION_PPM override it.
freq_correction_ppm = 0.0

# Target post-decimation capture rate, Hz (issue #169). Omit to use the
# source's native rate unchanged. Must evenly divide the source's native
# rate by a power of two, and must itself satisfy fs/93.75 being a power
# of two. manta_dsp::decimate::Decimator, manta_input::DecimatingSource.
# --capture-rate-hz and MANTA_INPUT_CAPTURE_RATE_HZ override it.
# capture_rate_hz = 48000

# Operator-supplied RF dial frequency, Hz, for a source that has no RF
# reference of its own (rig-audio passband). §1.3; MAN-34. Omit for
# sources that report their own tuned frequency. Must be finite and > 0;
# without it, audio-sourced frequencies are baseband offsets, and `run`
# with a [server] table refuses such a source. --dial-freq-hz and
# MANTA_INPUT_CENTER_FREQ_HZ override it.
# center_freq_hz = 14030000

# Fixed replay epoch, Unix seconds: the wall-clock instant a replayed
# file's first sample maps to, in place of the file's mtime (`run` on file
# replay only). --replay-epoch and MANTA_INPUT_REPLAY_EPOCH override it.
# replay_epoch = 1700000000

[spot]
# allowlist, blocklist_path and notch_path are re-read on SIGHUP when the config has a [server] table (MAN-78); every other [spot] key needs a restart.
# Operator Watch List (§6, MAN-28): callsigns here bypass grammar/cty
# validation and the repetition gate entirely in manta-spot's validator.
# A non-empty --allowlist replaces this list.
allowlist = []
# Bad-callsign file, one callsign per line, `#` comments allowed (MAN-31).
# A relative path resolves against this config file's directory.
# --blocklist overrides it.
# blocklist_path = "bad-calls.txt"
# Notched frequency ranges, one `low_hz-high_hz` per line (MAN-31). Same
# path rule; --notch overrides it.
# notch_path = "notches.txt"
# AD1C's cty.dat replaces the bundled country-prefix table (MAN-79).
# A prefix missing from this file is rejected unless the call is allowlisted.
# Download: https://www.country-files.com/cty/cty.dat
# A relative path resolves against this config file's directory.
# Unset: bundled. --cty overrides it.
# cty_path = "cty.dat"
# MASTER.SCP replaces the bundled known-callsign list. Membership raises
# confidence; absence alone never rejects a call.
# Download: https://www.supercheckpartial.com/MASTER.SCP
# Same path rule. Unset: bundled. --scp overrides it.
# scp_path = "MASTER.SCP"
# Scheduled list refresh (MAN-316). Opt-in; only `run` from a live
# receiver downloads. cty.dat is checked every 7 days and MASTER.SCP every
# day (If-Modified-Since); a download replaces the table in use only after
# it parses and passes the acceptance checks, and a failure keeps the
# current table and retries in 1 hour. File replay never downloads and
# never reads the cache's latest copy. Skipped for a list that a path or
# a SHA-256 pin below sets.
# refresh_lists = false
# Accepted downloads, content-addressed (cty/<sha256>.dat,
# scp/<sha256>.scp, state.json). Same path rule. Unset: memory only.
# list_cache_dir = "/var/cache/manta"
# http:// only (no TLS); https:// is refused at load.
# cty_url = "http://www.country-files.com/cty/cty.dat"
# scp_url = "http://www.supercheckpartial.com/MASTER.SCP"
# Pin a list by the SHA-256 of its bytes (64 hex digits), for every
# validating command: a set cty_path/scp_path must match it; otherwise it
# is looked up in the bundled copy, then list_cache_dir. No match stops
# before the source is read.
# --cty-sha256 / --scp-sha256 override them.
# cty_sha256 = "e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1"
# scp_sha256 = "00692208fa86b1498dcd6fc0f88e14d2a1a0462ba5d481855da5de69be4e3cf0"
# Pattern tier counts: normal = common 2 / listed 3 / unlisted 5; minimal = 2 for all.
# Allowlisted calls and beacons stay exempt.
validation_level = "normal"
# Independent list derived from MASTER.SCP. One [+]PATTERN per line, e.g. +K5@@@.
# Leading # comments; @ for later letters, # for later digits. Same path rule.
# Unset: bundled. --patterns overrides it.
# pattern_path = "patterns.lst"

# [server] key line_format (MAN-88), set inside your real [server] table:
# Which fixed-column wire layout the INBOUND telnet cluster server renders
# each spot line in (§1.4's 0.01 kHz rounding rule, ARCHITECTURE.md §7,
# MAN-88). Accepted values, exhaustive -- an
# unrecognized one is rejected at startup, never silently defaulted:
#   "rbn"     (default) the RBN relay's AK1A layout: frequency to 0.01 kHz
#             ending at column 24, 15-wide callsign column starting at
#             column 27, 6-wide mode field ("CW") at column 42, time at
#             column 71. Matches a live telnet.reversebeacon.net:7000
#             capture byte-for-byte.
#   "skimmer" the CW-Skimmer-native layout: same geometry with the mode
#             field deleted, so everything after the callsign column
#             shifts left by 4 and time lands at column 67. For operators
#             running manta behind W3OA's Aggregator.
# Scoped to the inbound listener only: the outbound `[[rbn_uplink]]`
# client always emits "rbn" regardless of this key. See
# docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md (Decisions 1-3)
# for the measured column table and that scoping.
# line_format = "rbn"

# [server] keys for station identity (MAN-86), set inside your real
# [server] table: the operator details RBN Aggregator reads out of
# the telnet greeting banner on connect (MAN-86, Aggregator manual v6.0
# §3.1/§9.2; wire format in
# `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md`).
# `station_callsign` is REQUIRED and has no default; the three operator
# keys are optional, and each one is dropped from the banner when absent
# rather than rendered as an empty placeholder. All four are validated at
# deserialize time (a bad value fails daemon start, it is never written to
# the wire): `station_callsign` by `manta_server::config::
# check_operator_callsign` (a base of 3-20 chars of A-Z, 0-9 and `/`, at
# most prefix/base/suffix, one segment a complete callsign -- prefix,
# separating digit, letter suffix -- plus an optional trailing `-N` SSID,
# N = 1-99, sent as `CALL-N-#` (MAN-89); deliberately broader than the
# decoder's `grammar::is_plausible` so real calls such as `JW/LB2PG` and
# `GB3LER/B` start), `operator_name`/`operator_qth` as non-empty free text with no control
# characters (they are interpolated verbatim into every client's banner,
# so an embedded CR/LF would forge cluster lines), `operator_grid` as a 4-
# or 6-character Maidenhead locator.
#
# `operator_grid`, `operator_lat` and `operator_lon` also set every JSON
# spot's `deGrid`/`deLat`/`deLon`, the receiving station's position
# (MAN-138, `docs/DECISIONS/2026-10-11-man138-station-location.md`): a
# grid alone gives its square's centre, a lat/lon pair alone gives the
# 6-character square containing it, and with none set all three are
# `null` -- never a `cty.dat` country centroid. `operator_lat` is a finite
# number from -90 to 90 and `operator_lon` from -180 to 180 (decimal
# degrees, south and west negative), checked at deserialize time. The two
# are set together or not at all, and alongside `operator_grid` the point
# must lie inside that square; both rules fail config loading before any
# I/O.
# station_callsign = "W3XYZ"    # required, no default
# operator_name = "Art"         # optional, default: absent
# operator_qth = "Switzerland"  # optional, default: absent
# operator_grid = "JN46la"      # optional, default: absent
# operator_lat = 46.0208        # optional, default: absent
# operator_lon = 8.9583         # optional, default: absent

# [server] and [[rbn_uplink]] (manta_server::config) configure the
# telnet/JSON/metrics servers and the RBN uplink; README.md's "Run it as a
# node" shows both. `run` starts the servers only when the resolved config
# has a [server] table.
```

The `[server]` block's normative keys here are `line_format` above
(`"rbn"` default | `"skimmer"`, MAN-88) and the station/operator identity
keys (MAN-86); its further transport keys (listen
addresses, per-IP connection and command budgets) are deployment settings
rather than normative constants of this spec, and
`crates/manta-server/src/config.rs` is their reference.

**Precedence and the environment.** `run`, `soak`, `doctor` and
`calibrate` resolve every key as: command-line flag, then
`MANTA_<TABLE>_<KEY>` environment variable (`MANTA_INPUT_FREQ_CORRECTION_PPM` is `input.freq_correction_ppm`;
`<TABLE>` is one of `SERVER`, `RBN_COMPARE`, `INPUT`, `SPOT`, `DETECTOR`,
`DECODE`), then the config file, then the default above. `--config`
names the file; without it they read `MANTA_CONFIG`. An environment value is parsed as a
TOML value (`9300` is an integer, `["W1AW","K1ABC"]` an array), falling
back to a bare string -- except for the string-typed keys, which are
always taken verbatim: `server.station_callsign`, `server.bind_addr`,
`server.metrics_bind_addr`, `server.live_bind_addr`,
`server.operator_name`, `server.operator_qth`, `server.operator_grid`,
`input.type`, `input.device`, `input.path`, `input.host`,
`input.password`, `input.driver`, `spot.blocklist_path`,
`spot.notch_path`, `spot.cty_path`, `spot.scp_path`, `spot.pattern_path`,
`spot.validation_level`, `decode.engine`,
`detector.crash_filter`, `rbn_compare.feed_host` and
`rbn_compare.login_callsign`. Relative
paths from the file resolve against the file's directory; those from a flag or the
environment resolve against the working directory. `[[rbn_uplink]]`
cannot be set from the environment. `[rbn_compare]` (MAN-301) is, like
`[server]`, operational rather than a decode-core constant, so the key
block above omits it; its keys and defaults are in
`docs/RUNBOOKS/rbn-comparison.md`. `calibrate` is the only command that
edits an existing config file, and it writes only `input.freq_correction_ppm`, after
confirmation (MAN-127,
`docs/DECISIONS/2026-10-11-man127-calibrate-command.md`).

Unknown tables, unknown keys in a known table, and unknown `MANTA_*`
variables are errors that name the offender, raised before any source
I/O. `MANTA_GIT_SHA` and `MANTA_FEATURES` are exempt: they are build-time
values of `crates/manta-cli/build.rs`, never read at run time, and
`cargo run`/`cargo test` export both into the processes they start
(MAN-83). `decode` and `oracle`
read only the file `--config` names and never the environment
(`MANTA_CONFIG` included), so their output cannot depend on the ambient
environment; `decode` applies `[detector]`, `[spot]`, `[decode]` and
`input.freq_correction_ppm`, `oracle` applies `[decode]`, and both
validate the whole file.

`SPEC-decode-core-v2.md` §7 adds `[decode]` keys beyond `engine` (`"legacy"`
| `"edge-legacy"` | `"hsmm"`, see that doc's §0): the `EdgeLegacy`/`Hsmm`
evidence/noise/HSMM tunables, additive over this table -- see that
document's §7 for the full v2 key list and defaults. All of them are
parsed by `manta_decode::config_file::DecodeConfigToml`, and every command
that takes `--config` (`run`, `soak`, `doctor`, `decode`, `oracle`) reads
`[decode]` through it. The full decision record is
`docs/DECISIONS/2026-10-06-man261-config-surface.md`.

`[server]`'s remaining keys are operational limits, not decode-core
constants: listener ports and the per-listener connection/rate quotas
(MAN-57/MAN-61) are documented on `manta_server::config::ServerConfig`'s
own fields, with the exposure policy in ARCHITECTURE §7.

`docs/CONFIG.md` is the operator's per-key reference: every key of every
table, `[[rbn_uplink]]` included, with its default and which commands and
engines it affects. This section stays normative for the decode-core
defaults.

## 10. Deviations from ARCHITECTURE.md

1. **Kaiser prototype is new code, not reused** (§1.2): `coppa-dsp::filter`
   only ships `RrcFilter`; the reuse table's "FIR design → coppa-dsp::filter"
   row is wrong for the PFB prototype. `FftProcessor` reuse stands.
2. **`AdaptiveAgc` dropped from the decode path** (§3.1): the dual-EMA
   threshold is self-normalizing; coppa's block AGC adds latency and couples
   with keying. Replaced by a per-track fixed reference scale.
3. **Beam search is character-local** (§4.4): resets at character boundaries;
   greedy across characters. Word-level context belongs to the validator.
4. Stopband target tightened from the implied ~60 dB to **80 dB** (§1.2) —
   free given 8 taps/branch, and pileup scenes (V8) have ≥ 27 dB dynamic
   range between neighbors.
5. **`[detector] on_snr_db` defaults to 12.0, not 6.0** (§2.3, §9): raised
   by `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 2 --
   at 6.0 dB the channelizer's autocorrelated per-hop noise produced 298
   spurious ACTIVE tracks on V1.
6. **Keying threshold is an additive band about the linear-amplitude
   midpoint, not a geometric-mean threshold with multiplicative hysteresis**
   (§3.2/§3.3, MAN-103): ARCHITECTURE §5's "adaptive threshold at their
   geometric mean" text is superseded — see
   `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`.
7. **No ring-and-thread split** (ARCHITECTURE §1, §3, §10): ARCHITECTURE's
   design put an `rtrb` ring between an input thread and a channelizer
   thread, per-track queues in front of a decoder pool, and a crossbeam
   channel into tokio. The implementation reads, channelizes, detects,
   decodes and validates synchronously on one thread; the decoder pool is a
   rayon pass joined every batch, and §6 rule 6 keeps its output independent
   of scheduling. ARCHITECTURE §10 describes this model and keeps the ring
   design as a target with the measurement that would justify building it.
