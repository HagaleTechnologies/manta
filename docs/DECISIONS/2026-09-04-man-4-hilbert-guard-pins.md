# MAN-4 (live-audio spurious duplicate tracks) implementation pins

This is MAN-4's pinned-decision record: "Live audio input should not spawn
spurious duplicate tracks on a single clean signal"
(`crates/manta-engine/tests/listen_audio.rs::listen_decodes_a_clean_real_audio_signal`,
formerly `#[ignore]`d, GitHub issue #21). Treat every numbered item below as
decided; SPEC and docs/ still win on anything not listed here.

## Root cause (restated from the MAN-4 research document)

`manta-input::AudioIqSource` converts real audio to analytic IQ via
`manta-dsp::hilbert::HilbertTransformer`, a windowed-sinc FIR whose own (now
rewritten) doc comment admitted only being "well-behaved" "from a few
hundred Hz to several kHz" -- i.e. weak negative-frequency image rejection
near DC and near Nyquist. Under the M1-era placeholder detector this never
mattered (only the single loudest channel was ever examined); M2's real
per-channel `TrackManager` (SPEC §2) watches every channel, so the leaked
image at a real tone's negative-frequency mirror -- which, because the
channelizer's channel index is a circular FFT-bin ordering (SPEC §1.1),
lands at a *high* channel index for a *near-DC* tone -- became eligible for
its own spurious track. `Track::owned`'s fixed ±1-channel window and
`Lifecycle`'s zero-hysteresis CANDIDATE state (both already correctly
tuned for real signals, `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`
items 2/4) then turned that one leaked image into "dozens of spurious
tracks... churning track IDs": neither of those mechanics is itself a bug,
both simply treat the leakage exactly like a real, borderline signal.

## Deviations and pinned decisions

1. **`HILBERT_TAPS` 129 -> 511.** `crates/manta-dsp/src/hilbert.rs`'s
   `image_rejection_meets_the_guaranteed_band_contract` test measures real
   image rejection (not a prose claim) across
   `[HILBERT_GUARD_HZ, fs/2 - HILBERT_GUARD_HZ]` at `fs = 48 kHz` and
   requires >= `HILBERT_MIN_IMAGE_REJECTION_DB` (70 dB) everywhere in that
   band; at the old 129 taps this test fails (measures roughly 12-13 dB at
   the 300 Hz band edge, and ~43 dB at the ticket's 750 Hz tone --
   `the_m1_legacy_design_is_why_man_4_happened` pins that regression
   witness). 511 taps is the smallest odd tap count that reaches the
   Kaiser beta=7.857 design floor (~80 dB) by 300 Hz, given the FIR
   transition width scales as ~1/taps.
   - **Alternative tried and REJECTED: 257 taps.** Clears 80 dB only from
     ~500 Hz outward, not 300 Hz -- would have required
     `HILBERT_GUARD_HZ = 500.0` instead (still below every practical CW
     receive-filter passband, per item 8, so this remained a viable
     fallback if the CPU-budget bench below hadn't cleared -- **remediate
     round measured it clears, see item 2's updated figure** -- so 511
     taps was kept).
   - **Alternative tried and REJECTED: 1023 taps.** Buys only a 150 Hz
     band edge instead of 300 Hz, for double the per-sample cost, over a
     region no CW receiver passes anyway (item 8).
   - **Alternative tried and REJECTED: FFT overlap-save analytic
     construction.** Cheaper asymptotically, but introduces block latency,
     a new determinism surface, and a `coppa-dsp` FFT dependency inside a
     streaming source -- 511 sparse taps already fits the CPU budget
     (item 2), so this added complexity bought nothing here.

2. **CPU: exploit the ideal kernel's structurally-zero even taps.**
   `HilbertTransformer` now precomputes `nz`, the indices of the ~half of
   `taps` that are exactly `0.0` (already asserted by
   `fir_is_odd_length_and_zero_at_even_offsets`), and `process()`'s inner
   loop iterates only `nz` -- halving the per-sample cost. Bit-identical to
   the dense loop (`sparse_tap_evaluation_is_bit_identical_to_dense`):
   skipping an exact-zero product changes a running f64 accumulator only
   via `(-0.0) + 0.0`, which IEEE 754 addition always resolves to `+0.0`
   regardless of term order once any nonzero term has already landed, and
   the relative order of the surviving nonzero terms is unchanged (SPEC
   §6.4's sequential-accumulation determinism convention is preserved).
   `crates/manta-dsp/benches/hilbert.rs` (criterion, not CI-wired, same
   rationale as `crates/manta-engine/benches/cpu_budget.rs`) measures the
   widened, sparse-evaluated 48 kHz path. Estimated cost: 256 f64
   MACs/sample x 48 kS/s ~= 12.3 M MAC/s, alongside the audio
   channelizer's own ~8.4 Mops/s -- combined, well under the ~48 Mops/s
   the Pi4-budget-binding 192 kS/s / N=2048 SDR passband already carries
   with *no* Hilbert stage at all.
   **Remediate round update:** the implement session's Constraints section
   (below) is correct that this was never measured there, but its own
   item 1 nonetheless asserted the bench "did" clear -- a contradiction
   this round resolves by actually running it: `cargo bench -p manta-dsp
   --bench hilbert` measures **18.7 ms** (mean of 100 samples, range
   18.3-19.2 ms) to process one second of 48 kHz audio in *this*
   container -- i.e. ~1.9% of one core in real-time terms here, well
   inside the estimate above. This is **still not a Pi4 measurement**
   (this container's CPU is unknown/unrelated hardware) -- the Pi4-budget
   leg of M2 acceptance remains open per `CLAUDE.md`'s Status section,
   unmet for lack of physical hardware reachable from any of these
   sessions. If a future Pi4 run finds the estimate wrong, the documented
   fallback is item 1's rejected 257-tap/500 Hz-guard alternative, not a
   redesign.

3. **The guard band is declared by the *source*, not hardcoded in the
   detector.** `IqSource` gains a defaulted `analytic_guard_hz() -> f64 {
   0.0 }` (`crates/manta-input/src/lib.rs`, same defaulted-trait-method
   precedent as `confirmed_live_handle`). `AudioIqSource` and
   `manta-soak-harness::LoopingAudioIqSource` (the only two production/
   tooling call sites of `HilbertTransformer` on a live-decode path)
   override it to `manta_dsp::hilbert::HILBERT_GUARD_HZ`; every complex-IQ
   source (`WavIqSource`, SoapySDR, KiwiSDR, HPSDR) keeps the `0.0`
   default. A detector-level DC guard applied unconditionally would
   suppress genuine signals near an SDR's RF center frequency, where
   channel 0 is a real operating frequency with no Hilbert transform in
   the path at all -- only the source knows whether its own front end
   needs this.

4. **Operator config raises the guard, never lowers it.**
   `crates/manta-engine/src/listen.rs`'s `effective_guard_hz(configured_hz,
   source_hz) = configured_hz.max(source_hz)`, applied to
   `DetectorConfig.guard_hz` before constructing `TrackManager`. An
   operator may widen the guard (a real, common need: LO leakage/DC spur
   on a direct-conversion SDR) but cannot configure the pipeline below
   what the front end physically requires. `max` avoids an
   `Option`/sentinel in `DetectorConfig`, keeping it `Copy`.
   `decode_samples`/`decode_wav` (no `IqSource`, raw `&[Complex32]` in)
   pass `cfg.detector` straight through unchanged -- with the `0.0`
   default, every golden vector is untouched by construction.
   **Remediate round update:** the implement session applied this only in
   `listen()`; `soak_metrics::soak_with_metrics` -- the *other* production
   `IqSource` pipeline, and what `manta-soak-harness` actually drives with
   `LoopingAudioIqSource` for the MAN-19 24h soak -- built its
   `TrackManager` from `cfg.detector` verbatim, leaving
   `LoopingAudioIqSource::analytic_guard_hz`'s override dead and the soak
   unguarded. `effective_guard_hz` is now `pub(crate)` and applied there
   too, identically.

5. **The guard gates spawning only.** `TrackManager::is_guarded` is
   checked only in `step_hop`'s same-hop spawn-eligibility scan (both the
   primary `rise[k]` test and the `k+1` tie-break, so a guarded channel
   can neither spawn nor win a tie-break); it does not close an existing
   track that later drifts into the band, and floor/gate estimation
   (`FloorBank`/`Gate`) still runs on all channels unconditionally, so the
   32-channel neighborhood-median clamp (`floor.rs`'s
   `effective_floor_db`) keeps working undisturbed. Narrowest change that
   fixes the defect.

6. **Suppressing < 300 Hz (and the symmetric region near +/-Nyquist) costs
   nothing real.** At the audio path's N=512 channels, a 300 Hz guard
   excludes 14 of 512 channels (2.7% of the band): `k in {0,1,2,3,
   509,510,511}` near DC and `k in {253..259}` near +/-Nyquist.
   Practical CW receive filters are 300-800 Hz centered with <= 500 Hz
   width, and RBN/skimmer practice sits at 400-1000 Hz; a sub-300 Hz audio
   CW tone is outside every real operating convention *and* is a
   frequency where the front end cannot deliver a clean analytic signal
   at any finite tap count. Nothing decodable is lost.

7. **The guard alone does NOT fix MAN-4 -- the tap widening is load-
   bearing, not redundant.** The ticket's 750 Hz tone's negative-frequency
   image lands at channel 504 (`= 512 - 8`), whose `|offset|` is 750 Hz --
   well *outside* a 300 Hz guard. Only the widened filter (item 1) kills
   that specific leakage; the guard (items 3-6) closes the narrower,
   structurally-unfixable residual band no finite-tap Hilbert design can
   ever clean up, and is what keeps a future "let's shorten the FIR back
   down" edit from silently reopening MAN-4's headline symptom outside the
   guarded band. **A later reader must not delete the tap change as
   redundant with the guard, or vice versa -- both are required, and they
   fix different, non-overlapping frequency regions.**

8. **`spawns_by_channel`/`total_spawns` are real public accessors, not
   test-only.** `TrackManager::close_counts` already exists on the same
   rationale ("exposed for the future M3 metrics endpoint"). A `Vec<u32>`
   of length `n_channels` is bounded, allocation-free after construction,
   incremented once per spawn (spawns are rare by design); `total_spawns`
   is that vector's sum rather than a derivative of the pre-existing
   `next_id` counter, since `next_id` starts at 1 and is post-incremented
   in `spawn()` -- reading it directly as "total spawns" is off by one
   against the count actually wanted here.
   **Remediate round update:** the implement session left these wired into
   nothing outside `#[cfg(test)]`, which is genuinely dead code in the
   non-test lib build (`close_counts` avoids this only because
   `soak_metrics::soak_with_metrics` already reads it) -- a real
   `cargo clippy --workspace --all-targets -- -D warnings` run makes this
   a CI-red `dead_code` error, not a hypothetical one. Fixed by actually
   wiring both into `soak_metrics::soak_with_metrics`'s report/sample
   types (`SoakMetricsReport::total_spawns`/`final_spawns_by_channel`),
   the same "visible in metrics" precedent `close_counts` already
   established, and surfaced in `manta-soak-harness`'s `report.json`
   alongside `final_close_counts`.

9. **`manta-testkit::scene`'s Watterson fixture-rendering path is frozen
   at the legacy 129-tap design** (`HilbertTransformer::with_taps(
   manta_dsp::hilbert::HILBERT_TAPS_M1_LEGACY)`, `scene.rs`), not migrated
   to the new 511-tap default. That path is a *synthesis* use (rendering a
   known faded real tone back into complex baseband for golden-vector
   fixtures), not an image-rejection-critical *analysis* use, and every
   golden vector's offset is >= 5.6 kHz, where even 129 taps already
   delivers >= 94 dB rejection -- changing the live-decode transformer's
   default must not silently move the V1-V10 byte baseline in the same
   diff as this fix. Migrating testkit to the new default (if ever
   desired) is a legitimate, independently-scoped vector-refresh change.

10. **`crates/manta-engine/tests/listen_audio.rs::listen_decodes_a_clean_real_audio_signal`
    is un-`#[ignore]`d** and now additionally asserts exactly one distinct
    `track_id` appears across the decoded event stream (not just that
    "W1AW" appears in the text), matching the ticket's Gherkin. A second,
    lower-level regression test,
    `manta_engine::listen::a_clean_audio_tone_spawns_one_track_and_no_churn`,
    drives `Channelizer` + `TrackManager` directly over
    `AudioIqSource`-produced IQ (the level `listen()`'s callback API can't
    reach) and asserts the per-channel spawn census: every spawn lands in
    `{7, 8, 9}` (channel 8 +/- the ownership window) and
    `close_counts().unconfirmed == 0` (no CANDIDATE churn).

11. **`manta-cli`'s `FixedCenterFreqSource` wrapper forwards
    `analytic_guard_hz` to its inner source -- found during implementation,
    not named in the original research/plan.** This wrapper
    (`crates/manta-cli/src/main.rs`) exists solely to override
    `center_freq_hz()` for the CLI's `--dial-freq-hz` flag, and commonly
    wraps `AudioIqSource` (the real `manta listen --device ...
    --dial-freq-hz ...` invocation is the actual real-world W1AW-copy
    scenario this ticket blocks). It already forwards
    `confirmed_live_handle` to `self.inner` rather than accepting the
    trait's default -- the same transparent-wrapper reasoning requires
    forwarding `analytic_guard_hz` too; without it, this wrapper would
    silently report the trait's default `0.0` regardless of what
    `AudioIqSource` declares, defeating items 3-4 above on exactly the
    CLI path they exist to protect. No new test was added for this
    specific wrapper (it has no existing test module in
    `crates/manta-cli/src/main.rs` to extend); its correctness follows
    directly from the one-line forwarding call, mirroring
    `confirmed_live_handle`'s already-established pattern in the same
    `impl` block.

## Remediate round (2026-09-06): additional pinned decisions

The implement session above could not compile or run a single line of what
it wrote (see "Constraints" below) -- every one of this round's decisions
was reached by actually running the code, not by further hand-derivation.

12. **`hilbert.rs`'s own nonzero-tap count assertion was off by one.**
    `default_design_is_511_taps_with_half_of_them_structurally_zero`
    asserted 255 nonzero taps; the widened 511-tap design has **256**. For
    an odd-length FIR the center index is `(len-1)/2`; the ideal Hilbert
    kernel is zero at every *even* offset from center, so the nonzero
    count is `len/2` rounded up when the center index is itself odd
    (511's center is 255, odd) versus rounded down when it's even (129's
    legacy center is 64, even -- which is where the "half of them"
    shorthand came from, and remains true only for that case). The 511-tap
    filter itself was always correct; only this assertion, the `nz` field
    doc, and pin item 2's MAC count (now fixed above) repeated the error.
    Fixed at `crates/manta-dsp/src/hilbert.rs`.

13. **`HILBERT_GUARD_HZ` 300.0 -> 600.0.** The plan's own contingency
    ladder anticipated needing exactly one of three branches; this round's
    actual measurement needed two, layered. Adding a realistic AWGN floor
    to `listen_audio.rs`/`a_clean_audio_tone_spawns_one_track_and_no_churn`
    (item 14) eliminates the flood of spurious mid-passband spawns the
    implement session's zero-noise fixture produced, but leaves a residue:
    at 300 Hz, channels at -562.5 Hz and -23,437.5 Hz (the negative-side
    counterparts, near DC and near -Nyquist respectively) still spawn
    dozens of times each -- a genuine Kaiser-window sidelobe ripple that
    is *not* monotonic with distance from DC/Nyquist, so "300 Hz was
    already the measured floor" (item 6, at the old value) undersold it
    once real noise was in the fixture. Measured directly against this
    branch's 511-tap filter: raising the guard to 600 Hz (channels with
    `|offset| < 600` or `> nyquist - 600`) eliminates that entire
    secondary flood, leaving only the single structural residual item 15
    describes. 600 Hz remains below every practical CW receive-filter
    passband (item 8's reasoning is unaffected by the specific number).
    **Correction (finding 9 of the remediate-round code review): this item
    originally claimed the soak scene's lowest offset (420 Hz) was
    unaffected because `manta-soak-harness` "doesn't use the audio-guarded
    path" -- that is the opposite of what item 4's own update says, and
    `soak_metrics.rs:213-215` proves item 4 right: `LoopingAudioIqSource`
    IS driven through the guarded path, at the same 600 Hz floor. The soak
    scene's own offsets needed, and got, a real fix -- see item 16.** At
    N=512 channels a 600 Hz guard excludes 26 of 512 channels (5.1%, up
    from item 6's 2.7% at 300 Hz): `k in {0..=6, 506..=511}` near DC and
    `k in {250..=262}` near +/-Nyquist -- still outside every real CW
    operating convention. (Corrected from an earlier, self-contradicting
    `{0..=5, 507..=511}` DC-side enumeration -- 11 channels, not 13 -- that
    did not match its own stated total of 26; `is_guarded` at 93.75 Hz
    spacing guards `|signed offset in channels| <= 6`, i.e. 7 + 6 = 13
    channels on the DC side.)

14. **Realistic AWGN added to both MAN-4 regression fixtures (plan's
    pre-decided contingency branch 2).**
    `crates/manta-engine/tests/listen_audio.rs` and
    `a_clean_audio_tone_spawns_one_track_and_no_churn`
    (`crates/manta-engine/src/listen.rs`) built a strictly noiseless real
    audio signal; a zero-noise input gives the 25th-percentile floor
    estimator nothing to find, so it reports the f32 numerical floor,
    against which ordinary filter sidelobes ~100 dB down still clear the
    +12 dB spawn gate -- flooding the census with spurious spawns
    unrelated to the Hilbert-image mechanism this ticket is about. No
    real receiver produces zero noise. `manta_testkit::noise` gains
    `add_real_awgn`/`real_awgn_sigma_for_snr_2500`, a real-valued analogue
    of the existing complex-domain `add_unit_awgn`/`amplitude_for_snr_2500`
    pair: a real cosine tone's average power is half a complex phasor's
    (`A^2/2` vs `A^2`), and its noise occupies a one-sided real band of
    width `fs/2` (not the complex convention's full `fs`) -- both net out
    to `sigma^2 * (2500/(fs/2)) == 0.5/10^(snr_db/10)`, verified by a new
    `real_awgn_achieves_the_requested_snr_in_2500_hz` test (within 0.5 dB).
    Both fixtures use +20 dB SNR-in-2500 (the plan's stated target) with a
    fixed seed chosen so the real 750 Hz tone's own steady-state SNR
    dominates the fixture cleanly (item 15 covers the one remaining,
    seed-independent residual).

15. **New: `TrackManager::is_image_of_owned`/`merge_mirror_images` --
    an active-track-aware companion to the frequency-fixed guard.** Item 7
    already named the residual the guard cannot reach: the 750 Hz tone's
    own negative-frequency image sits at the *same* `|750 Hz|` offset from
    DC as the tone itself, so raising `HILBERT_GUARD_HZ` above 750 Hz
    would suppress the real signal too. What item 7 did not anticipate
    (the plan's contingency ladder, written without build access, assumed
    this residual was merely "a spurious spawn" in the census sense): once
    a real per-channel `TrackManager` actually runs this fixture, that
    residual image is a *coherent* copy of the real signal's own keyed
    envelope, not incoherent leakage -- consistently promoting and
    decoding **duplicate, character-interleaved text** through `listen()`
    (measured: `track_ids = {1, 2}`, decoded text with every character
    doubled), independent of noise seed or SNR (tested at 10/15/20/25/30
    dB SNR-in-2500 and two different noise seeds: the residual's single
    spawn at the mirror channel occurred identically in every case). This
    is squarely the ticket's own headline symptom ("locking onto the same
    real signal in parallel"), not a benign statistical artifact, so
    relaxing the acceptance test to tolerate it (the plan's contingency
    branch 3, meant for the *different*, out-of-scope ~2.5-channel
    pileup-separation floor) would have hidden a real defect rather than
    fixed one.

    Fix: `TrackManager` now tracks, at spawn-eligibility time, whether a
    rising channel's circular mirror (`n_channels - k`, the same
    reflection `wrapped_channel_offset` uses) is already owned, and if so,
    compares **raw per-hop `hop.power`** (not the EMA-smoothed
    `gate.smoothed_db`/`current_snr_db` used elsewhere in this file) at
    the two channels: a candidate that is not yet the stronger side is
    blocked from spawning (`is_image_of_owned`); a candidate that spawns
    anyway (a keying-edge broadband click can let the weak image win the
    race to spawn *before* the real signal's own next mark, measured
    happening at exactly the first post-warmup hop in the regression test)
    leaves its now-weaker mirror to be closed by a new per-hop merge step,
    `merge_mirror_images`, mirroring `merge_converged`'s existing SPEC
    §2.5 "close the lower-SNR side of a pair" rule but keyed on the
    circular mirror relationship instead of raw distance.

    **Raw power, not smoothed SNR, and why this needed two iterations to
    get right:** an earlier version of this fix compared
    `current_snr_db`/`gate.smoothed_db`, which ramps up over several hops
    at the *start* of every new mark -- since the real and image channels'
    marks begin at the same instant (same keyed envelope), their smoothed
    statistics briefly cross paths at every mark onset, and unconditional
    blocking flip-flopped hundreds of times across the fixture's keyed
    message (measured: 400+ spurious spawn/merge cycles, far worse than
    the original bug). Raw `hop.power` has no ramp-up and reflects the
    ~90 dB steady-state gap between the real signal and its image
    immediately, which resolved the oscillation in testing. A second,
    simpler-looking fix (block unconditionally, no merge, whichever side
    owns its channel first wins forever) was also tried and rejected: it
    is stable, but has no way to *correct* a bad first spawn order, so a
    keying-edge click winning the initial race would permanently suppress
    the real signal for the rest of the run -- verified by disabling the
    merge step and confirming the real signal's own subsequent marks were
    silently blocked.

    Always a no-op when `guard_hz <= 0.0` -- i.e. every complex-IQ path,
    which never runs a real-to-analytic front end and so has no self-image
    to speak of -- so no golden vector is affected (confirmed: full
    `cargo test --workspace` green, including all V1-V10 golden vectors
    and the chunking-determinism tests, with this fix in place).
    **Superseded by item 17's `mirror_image_guard` split below** -- the
    no-op condition described here as `guard_hz <= 0.0` is now
    `!mirror_image_guard`, for the reason item 17 explains.

## Remediate round 2 (2026-09-06): code-review response

The prior remediate round's own validation gate ran a real `/code-review`
at high effort (this container can build -- see the first remediate
round's "How this was validated"), which surfaced four correctness
findings, one efficiency finding, one test-quality finding, and three
documentation-accuracy findings, all in the mirror-image machinery items
12-15 added (the part of the change the plan never constrained). Full
finding text lives in the validation report; this section pins the fix for
each so a later reader does not have to reconstruct it from the diff.

16. **Finding 1 (correctness, CONFIRMED): `is_image_of_owned`/
    `merge_mirror_images` were gated on `cfg.guard_hz <= 0.0`, conflating
    "this front end synthesizes an analytic signal from real input" with
    "the operator widened the DC guard" -- two different, orthogonal
    facts. Item 4 already documents widening `guard_hz` as a legitimate
    operator action on a genuine complex-IQ source (LO leakage/DC spur);
    under the old gating, doing so on a SoapySDR/KiwiSDR source would have
    silently turned on mirror merging, and two real stations symmetric
    about the dial frequency would suppress each other. Fixed by a new
    `DetectorConfig::mirror_image_guard: bool` (default `false`),
    independent of `guard_hz`, set by `listen()`/`soak_with_metrics` from
    the source's own `analytic_guard_hz() > 0.0` -- never from the merged
    `guard_hz`. See that field's doc comment (`track.rs`) for the full
    rationale, and
    `TrackManager::widened_guard_hz_alone_does_not_enable_mirror_suppression`
    for the regression test.

17. **Findings 2 and 3 (correctness, CONFIRMED): the mirror match required
    bit-exact `select_channel` reflection, and the power comparison had no
    margin.** `select_channel` is argmax over each track's own +/-1 owned
    window, so a genuine mirror pair's two argmaxes need not be exact
    reflections under noise (finding 2) -- fixed by a `circular_distance`
    tolerance of 1 channel instead of `==`. Raw single-hop power with no
    margin is a coin flip when both sides of a pair sit close together,
    e.g. both near the noise floor during a key-up gap (finding 3) --
    fixed by a `MIRROR_POWER_MARGIN_DB = 3.0` requirement before either
    side of a comparison is treated as a clear winner; an ambiguous
    comparison now defers instead of firing on noise. 3 dB is comfortably
    below the ~90 dB steady-state real/image gap this mechanism exists to
    resolve (item 15), so real decisions still fire promptly. See
    `TrackManager::merge_mirror_images_tolerates_a_one_channel_argmax_drift`
    and `TrackManager::mirror_merge_defers_on_an_ambiguous_power_tie_instead_of_closing_a_track`
    for the regression tests (`track.rs`).

18. **Finding 4 (correctness, PLAUSIBLE): `total_spawns()` summed a
    `Vec<u32>` census into a `u32`.** A churn regression over the 24h soak
    (32.4M hops, up to `n/2` spawns per hop in the pathological case)
    could overflow. Widened to `u64`, matching `CloseCounts`' existing
    convention; `SoakMetricsReport::total_spawns` widened to match.

19. **Finding 5 (efficiency): `merge_mirror_images` recomputed `ka` via
    `select_channel` on every inner-loop iteration `j`, though it depends
    only on `i`/`a`.** Fixed by precomputing each open track's selected
    channel once per hop into a `channel -> usize` map before the O(T^2)
    pairwise scan, rather than inside it. The O(T^2) skeleton itself
    (shared with the pre-existing `merge_converged`) is unchanged --
    out of scope for this fix.

20. **Finding 6 (test quality, CONFIRMED): `hilbert.rs`'s
    `image_rejection_meets_the_guaranteed_band_contract` probed `[f, fs -
    f]`, but `f` only ranges over `[HILBERT_GUARD_HZ, fs/2 -
    HILBERT_GUARD_HZ]`, so `fs - f` always landed >= `fs/2` and was
    unconditionally skipped -- dead code claiming "both sidebands"
    coverage it never had.** Removed rather than fixed: for a real-valued
    cosine input, `fs - f` is bit-for-bit the same signal as `f`
    (aliasing), so probing `f` alone already exercises the full declared
    band.

21. **Finding 9 (documentation accuracy): item 13 above asserted
    `manta-soak-harness` "doesn't use the audio-guarded path" -- directly
    contradicting item 4's own update, and wrong: `soak_metrics.rs:213-215`
    (item 4) applies `effective_guard_hz` to `LoopingAudioIqSource` exactly
    like `listen()` does.** Item 13's text is corrected in place above. The
    real, and previously undocumented, consequence: `manta-soak-harness`'s
    `pileup_signals()` (`main.rs`) shifts every MAN-19 soak-scene offset up
    by a `GUARD_SAFE_SHIFT_HZ = 250.0` constant so the lowest (was 420 Hz)
    clears the 600 Hz guard with margin, without touching the pileup's
    relative spacing/design. This shift previously existed only as an
    inline code comment with no pin backing it; it is now also recorded
    here, per the same "decisions get a pin, not just a comment"
    convention this whole document exists to enforce.

22. **Finding 7 (documentation accuracy, CONFIRMED): `ARCHITECTURE.md` and
    `docs/RUNBOOKS/m1-w1aw-live-copy.md` still said "~300 Hz" after item 13
    doubled `HILBERT_GUARD_HZ` to 600.0.** Both updated to 600 Hz; the
    runbook one is operator-facing and was actively misleading (an
    operator told "no tracks below ~300 Hz is expected" would misread a
    missing 450 Hz signal as normal).

## Remediate round 3 (2026-09-06): validate-gate code-review response

23. **Finding 2 (correctness, PLAUSIBLE): `is_guarded` is sign-blind --
    decided: keep it symmetric, and correct the cost claim rather than
    attempt an unverified asymmetric redesign.** The reviewer measured
    that image rejection at 511 taps already clears the 70 dB floor by
    250-300 Hz, i.e. the 300 -> 600 Hz raise in item 13 was driven purely
    by the Kaiser sidelobe residue at -562.5 Hz/-23,437.5 Hz -- both on
    the *negative* (image) side -- so a guard that suppressed only the
    negative-offset half near DC (and the corresponding half near
    Nyquist) would kill that residue while leaving the real positive
    300-600 Hz audio slice spawnable.

    That asymmetric design was considered and **rejected for this round**:
    `is_guarded` runs on a circular channel index (`wrapped_channel_offset`,
    same convention `channel_offset_hz`/`Track::freq_hz` use), so "the
    negative-image side" near DC and "the negative-image side" near
    Nyquist are opposite signs of the *same* signed-offset test, and
    getting that circular sign convention wrong is exactly the class of
    bug this ticket exists to fix (item 7's own warning about the tap
    widening and the guard being non-redundant, non-overlapping fixes
    applies equally to getting either one subtly wrong). Item 15 also
    means a real signal placed inside the surviving positive 300-600 Hz
    slice would still need its own negative-frequency coherent image
    (at the *same* `|offset|`) suppressed by `mirror_image_guard`, not by
    `is_guarded` -- so the asymmetric-guard win is smaller than it first
    looks, and this round has no hardware or measurement campaign budgeted
    to re-verify a frequency-domain sign change with the same rigor items
    1/13/15 got. Deferred, not abandoned: a future ticket with room for a
    proper measurement pass (repeat `image_rejection_meets_the_guaranteed_
    band_contract`-style verification split by sign) is the right place to
    revisit this, not a validate-gate remediate round.

    **What this round actually fixes: the stale cost claim.** Pin item 6
    (and the plan's own D8) argued "nothing decodable is lost" for a
    **300 Hz** guard, citing 300-800 Hz-centered CW receive filters and
    RBN/skimmer practice at 400-1000 Hz. That argument does not cover the
    **shipped 600 Hz** value: a 400-550 Hz CW sidetone is a real point
    inside both cited ranges and is symmetrically excluded by `is_guarded`
    today. `docs/RUNBOOKS/m1-w1aw-live-copy.md` step 4 is corrected below
    to say so plainly instead of repeating item 6's 300 Hz-era reassurance
    at the 600 Hz value, so an operator who tunes a 400-550 Hz sidetone
    and sees no tracks reads it as this known, owned limitation rather
    than a mystery bug.

### Deferred to a follow-up ticket (P2-and-lower, per PR review convergence policy)

Per `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`, this round
is past round one, so genuinely new P2-and-lower findings are captured
verbatim here (not fixed inline) for whoever files the follow-up ticket --
this remediate session has no Linear credential to file it directly.

- **Findings 3 (efficiency) and 5 (test-robustness) from the round-3
  `/code-review`**, verbatim:

  > Finding 3 -- `merge_mirror_images` adds an O(T²) all-pairs scan plus a
  > per-hop `BTreeMap` allocation on the Pi 4-budgeted path -- efficiency.
  > `crates/manta-engine/src/track.rs` (`merge_mirror_images`). At
  > `track_cap = 500` and 375 hops/s that is ≈125 k pair iterations per
  > hop, doubling `merge_converged`'s existing cost -- when
  > `owner_of[(n - ka) % n]` yields the mirror partner directly in O(T).
  > It also calls `recompute_ownership()` every hop even when nothing
  > closed, immediately before `evict_over_cap` recomputes again. (The
  > O(T²) *skeleton* is copied verbatim from the pre-existing
  > `merge_converged`, so this is a new instance of an old pattern, not a
  > new pattern; the per-hop `BTreeMap` allocation is genuinely new.)
  > Does not fail the gate.

  > Finding 5 -- the newly un-`#[ignore]`d test's `track_ids.len() == 1`
  > may be cross-platform-fragile -- test-robustness, PLAUSIBLE.
  > `crates/manta-engine/tests/listen_audio.rs` (the new assertion block).
  > The branch's own `listen.rs` census test documents that the mirror
  > image can win the spawn race (`census[image_channel] <= 1`) and that
  > its SNR against its own local floor is comparable to the real track's
  > -- so it can in principle promote and emit before
  > `merge_mirror_images` closes it. The fixture is built with
  > `phi.cos()`, whose f64 result differs between glibc and macOS libm, so
  > spawn timing is not bit-identical across the two required CI legs
  > (`test (ubuntu-latest)` and `test (macos-latest)`). It passes here on
  > Linux; the macOS leg is not observable from this container. Recording
  > as a watch item for whoever babysits the PR's CI, not as a confirmed
  > defect.

## Constraints encountered during this implementation session

This session ran in a network-isolated container: the pinned `coppa` git
dependency (`Cargo.toml`, `f8a4d16df7e5776a0756943c05712038774e6c70`) could
not be fetched (`cargo check` fails with a `class=Net` error before
reaching any compilation step), so **no part of this implementation could
be compiled, run, or test-verified in this session** -- not the new/
modified unit tests, not the un-ignored integration test, not the
criterion bench. Every numeric claim in this document not attributed to a
specific in-repo Rust test (the image-rejection table in the prior
research/plan documents) is a hand-derivation cross-checked against the
Kaiser beta=7.857 stopband floor and the existing `129`-tap-era doc
comment's own "a few hundred Hz to several kHz" claim, not a fresh
in-session measurement. The very next `cargo test --workspace` /
`cargo bench -p manta-dsp --bench hilbert` run against this branch (once
network access to the `coppa` dependency is available) is the actual
verification step for every "Automated" success criterion in this
ticket's implementation plan; until then this pin doc's numbers should be
read as "derived and internally consistent," not "measured in CI."

### coppa dependency pin

Unchanged from M1's pin: `coppa-dsp`/`coppa-audio`/`coppa-channel` remain
pinned in the workspace `Cargo.toml` to git rev
`f8a4d16df7e5776a0756943c05712038774e6c70` of
`https://github.com/HagaleTechnologies/coppa.git`. This change made no
coppa API changes and needs no bump.
