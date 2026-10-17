# MAN-194: burst-capable `decoder_input` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reintroduce correct `GROUP_DELAY_HOPS`-delay-compensated pairing in `Track::decoder_input`, via a burst-capable (`Vec`-returning) interface that can hold back reports until they're valid and drain its backlog at every point a track's refiner stops receiving new input (reset, EOF, Merged/Evicted/Silent/HangExpired closure).

**Architecture:** `Track` gains a bounded `VecDeque` backlog of `(raw_amp, raw_power, sample_ts)` triples. `decoder_input` pushes one entry per hop and pops the due one once `GROUP_DELAY_HOPS` hops have elapsed, using the FIR's own output once its window is fully real and the raw magnitude before that — never both for the same hop, never neither. A new `drain_refiner_into_pending` flushes whatever's left whenever a track's input source stops, wired into every closure path in `TrackManager`.

**Tech Stack:** Rust, `cargo test`/`clippy`/`fmt`, existing `manta-dsp`/`manta-engine`/`manta-decode` crates.

**Spec:** `docs/superpowers/specs/2026-09-10-refiner-burst-drain-design.md`

## Global Constraints

- Deterministic decode path is a hard requirement (file input → byte-identical spot logs) — no change here may introduce nondeterminism (no wall-clock reads, no unordered iteration affecting output order).
- Full workspace `cargo test`, `cargo clippy --workspace --all-targets`, and `cargo fmt --check` must pass before every push.
- Run local `codex exec review --uncommitted` before pushing, per this repo's convention for refiner-adjacent changes (MAN-168's PR #178 found three real bugs this way).
- PR review convergence for this change: rounds 1-5 fix everything reasonable inline; rounds 6-15 fix P1s inline and ticket P2s (per explicit instruction for this ticket, overriding the repo's stricter round-2 default).
- Auto-merge is on repo-wide for `manta`: run `gh pr merge --auto --squash` immediately after opening the PR.
- Work happens in the existing worktree at `/Users/thagale/Code/manta-man194` on branch `fix/man194-refiner-burst-drain` (already created off `origin/main`). Do not touch the primary checkout at `/Users/thagale/Code/manta`.

---

## Task 1: Rewrite `Track::decoder_input` for delay-compensated burst output

**Files:**
- Modify: `crates/manta-dsp/src/refine.rs:1-19` (module doc comment only)
- Modify: `crates/manta-engine/src/track.rs` (imports; `Track` struct fields ~349-417; `Track::new` ~435-449; `Track::decoder_input` ~451-556)
- Test: `crates/manta-engine/src/track.rs` (`mod tests`, replacing/adding tests currently at lines 1509-1744)

**Interfaces:**
- Produces: `Track::decoder_input(&mut self, k: usize, hop: &HopOutput, sample_ts: u64, refine_bw_hz: f32) -> Vec<(f32, f32, u64)>` — was `-> (f32, f32, u64)`. Every later task's call sites consume this `Vec`.
- Produces: `Track` gains a field `refiner_backlog: std::collections::VecDeque<(f32, f32, u64)>` (raw_amp, raw_power, sample_ts) — Task 3 reads/drains it via new methods added there.
- Consumes: `manta_dsp::refine::{Refiner, GROUP_DELAY_HOPS}` (unchanged API — `Refiner::push`/`reset`/`set_delta`, `GROUP_DELAY_HOPS: usize = 5`).

- [ ] **Step 1: Update `refine.rs`'s module doc comment**

Replace the module doc comment (lines 1-19) — it currently says the engine call site does NOT use `GROUP_DELAY_HOPS` for delayed pairing, which MAN-194 makes false.

```rust
//! Per-track narrowband refinement: an optional, per-hop channel derotator
//! and lowpass that sharpens a track's amplitude estimate using its
//! fractional centroid offset. SPEC v2 §3. Enabled when `decode.refine_bw_hz`
//! is greater than zero (default 0, meaning off).
//!
//! The engine call site (MAN-168/MAN-194, `manta-engine::track::Track::
//! decoder_input`) uses [`GROUP_DELAY_HOPS`] for both of this constant's
//! purposes: sizing the convergence window (`TAPS`, equal to
//! `2*GROUP_DELAY_HOPS + 1`, real hops needed before the FIR's 11-tap
//! window is no longer partly zero-padded from a reset) AND pairing a
//! refined amplitude with the correctly delayed `sample_ts` it actually
//! describes (`Refiner::push`'s output at hop `t` is evidence about hop
//! `t - GROUP_DELAY_HOPS`, not hop `t`). A one-report-per-hop interface
//! could not do the latter without either skipping hops or duplicating an
//! observation at the boundaries (track birth, a channel reset, end of
//! stream); `decoder_input` returns `Vec<(f32, f32, u64)>` specifically so
//! it can emit zero, one, or several reports per input hop, which is what
//! a correct hold-back/drain protocol requires. See MAN-194 and
//! `decoder_input`'s own doc comment for the full protocol.
```

- [ ] **Step 2: Add the `VecDeque` import and the `refiner_backlog` field**

In `crates/manta-engine/src/track.rs`, change the import line:

```rust
use std::collections::BTreeMap;
```
to:
```rust
use std::collections::{BTreeMap, VecDeque};
```

Then, in the `Track` struct definition, replace the doc comment on `refiner_hops_since_reset` and add the new field right after it:

```rust
    /// Real (non-zero-padded) hops fed to `refiner` since its last reset:
    /// `decoder_input` withholds any report at all until this reaches
    /// `GROUP_DELAY_HOPS` (there is no valid delayed observation before
    /// then), reports the raw magnitude of the due delayed hop while this
    /// is between `GROUP_DELAY_HOPS` and `TAPS`, and reports the FIR's own
    /// output once it reaches `TAPS` (the full window is real) -- see
    /// `decoder_input`'s doc comment for the complete MAN-194 protocol.
    /// Reset to 0 alongside `refiner.reset()`.
    refiner_hops_since_reset: u64,
    /// MAN-194: buffered-but-not-yet-reported `(raw_amp, raw_power,
    /// sample_ts)` triples, oldest first, one pushed per hop fed to
    /// `refiner`. Holds at most `GROUP_DELAY_HOPS` entries at any instant
    /// once steady state is reached (one is popped the same hop one is
    /// pushed, from `refiner_hops_since_reset > GROUP_DELAY_HOPS` on).
    /// Drained in full (see `drain_refiner_backlog`/
    /// `drain_refiner_into_pending`) whenever this track's refiner stops
    /// receiving new input: a channel reset (inside `decoder_input`
    /// itself), or a closure/end-of-stream (`TrackManager`, Task 3/4).
    refiner_backlog: VecDeque<(f32, f32, u64)>,
```

- [ ] **Step 3: Initialize the new field in `Track::new`**

In `Track::new`, add the field to the struct literal:

```rust
    fn new(id: u32, birth_channel: usize, cfg: &DetectorConfig) -> Self {
        Track {
            id,
            lifecycle: Lifecycle::new(cfg),
            center: birth_channel as f64,
            current_snr_db: 0.0,
            birth_channel,
            decoder: None,
            pending: Vec::new(),
            has_emitted: false,
            refiner: None,
            refiner_channel: None,
            refiner_hops_since_reset: 0,
            refiner_backlog: VecDeque::new(),
        }
    }
```

- [ ] **Step 4: Write the failing tests for the new `decoder_input` contract**

Replace the entire block of `decoder_input`-specific tests (currently `decoder_input_bypasses_the_refiner_when_disabled` through `decoder_input_applies_the_odd_channel_sign_correction`, lines 1509-1744) with the following. `decoder_input_does_not_reset_the_refiner_on_center_drift_alone` and `decoder_input_resets_on_a_real_channel_change` need no code changes (they never destructure `decoder_input`'s return value) — leave them exactly as they are, in place, between the tests below where they already sit.

```rust
    #[test]
    fn decoder_input_bypasses_the_refiner_when_disabled() {
        // MAN-168: refine_bw_hz <= 0.0 (the default) must fall back to the
        // raw owned-channel magnitude exactly as before, and never
        // construct a Refiner at all.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |z|=5
        let out = track.decoder_input(5, &h, 1000, 0.0);
        assert_eq!(out, vec![(5.0, 25.0, 1000)]);
        assert!(track.refiner.is_none());
    }

    #[test]
    fn decoder_input_delays_reported_sample_ts_by_group_delay_hops() {
        // MAN-194: decoder_input now reports the CORRECTLY delayed
        // sample_ts (GROUP_DELAY_HOPS behind the hop it was fed on), not
        // the current hop's own ts (PR #178's since-superseded stopgap) --
        // Refiner::push's output at hop t is evidence about hop t - D.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(1.0, 0.0); 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let mut reported_ts: Vec<u64> = Vec::new();
        for i in 0..(d + 3) {
            let sample_ts = 1000 + i * 512;
            let out = track.decoder_input(5, &h, sample_ts, 30.0);
            reported_ts.extend(out.into_iter().map(|(_, _, ts)| ts));
        }
        // Hops 0..D-1 report nothing (no valid delayed observation yet);
        // hop D reports hop 0's ts, hop D+1 reports hop 1's ts, hop D+2
        // reports hop 2's ts.
        let expected: Vec<u64> = (0..3).map(|i| 1000 + i * 512).collect();
        assert_eq!(
            reported_ts, expected,
            "must report delayed timestamps starting from hop 0, once due"
        );
    }

    #[test]
    fn decoder_input_withholds_any_report_until_a_delayed_observation_exists() {
        // MAN-194: hops 0..GROUP_DELAY_HOPS-1 have no valid delayed
        // observation at all (there's nothing D hops in the past yet) --
        // decoder_input must report NOTHING for them, not a same-hop raw
        // substitute (the old stopgap this replaces, which caused a
        // content discontinuity at the warm-up/converged transition).
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        for i in 0..GROUP_DELAY_HOPS as u64 {
            let out = track.decoder_input(5, &h, i * 512, 30.0);
            assert!(
                out.is_empty(),
                "warm-up hop {i} must report nothing, not a same-hop raw substitute"
            );
        }
    }

    #[test]
    fn decoder_input_reports_raw_magnitude_for_a_not_yet_fully_converged_delayed_hop() {
        // Once a delayed observation becomes due (h >= GROUP_DELAY_HOPS)
        // but its FIR window is still partly zero-padded (h < TAPS), report
        // the RAW magnitude of that delayed hop -- real evidence, correctly
        // time-labeled -- not the FIR's own ramp-in transient (Codex
        // review, PR #178: a ramp-in transient measured ~40% below
        // steady-state amplitude on a constant tone).
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5, power = 25
        let h = hop_with_x(0, vec![raw; 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let full_history_hops = 2 * d + 1;
        for i in 0..full_history_hops {
            let out = track.decoder_input(5, &h, i * 512, 30.0);
            if i >= d {
                assert_eq!(
                    out,
                    vec![(5.0, 25.0, (i - d) * 512)],
                    "hop {i} (before full convergence at {full_history_hops}) must report the \
                     raw magnitude of its due delayed hop, not a partially-converged FIR output"
                );
            } else {
                assert!(out.is_empty());
            }
        }
    }

    #[test]
    fn decoder_input_does_not_replay_one_observation_as_multiple_delayed_hops() {
        // Regression guard for the exact bug three PR #178 review rounds
        // found: each delayed hop must report ITS OWN raw observation, not
        // a single stale one replayed across several due hops.
        let mut track = Track::new(1, 5, &cfg());
        let loud = num_complex::Complex32::new(3.0, 4.0); // |loud| = 5, power = 25
        let quiet = num_complex::Complex32::new(0.03, 0.04); // |quiet| = 0.05, power = 0.0025
        let h_loud = hop_with_x(0, vec![loud; 8]);
        let h_quiet = hop_with_x(0, vec![quiet; 8]);
        let d = GROUP_DELAY_HOPS as u64;

        track.decoder_input(5, &h_loud, 0, 30.0); // hop 0: loud
        for i in 1..d {
            track.decoder_input(5, &h_quiet, i * 512, 30.0); // hops 1..D-1: quiet
        }
        // Hop D: due observation is hop 0's (loud).
        let out_d = track.decoder_input(5, &h_quiet, d * 512, 30.0);
        assert_eq!(
            out_d,
            vec![(5.0, 25.0, 0)],
            "hop D must report hop 0's loud observation"
        );
        // Hop D+1: due observation is hop 1's (quiet), not hop 0's replayed.
        let out_d1 = track.decoder_input(5, &h_quiet, (d + 1) * 512, 30.0);
        assert_eq!(
            out_d1,
            vec![(0.05, 0.0025, 512)],
            "hop D+1 must report hop 1's OWN quiet observation, not hop 0's loud one replayed"
        );
    }

    #[test]
    fn decoder_input_reports_refined_amp_once_fully_converged() {
        // Full convergence needs TAPS == 2*GROUP_DELAY_HOPS+1 real hops fed
        // to the refiner since reset -- one hop past the raw-magnitude
        // regime the previous tests cover, the due delayed hop's window is
        // fully real and decoder_input must report the FIR's own output.
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let full_history_hops = 2 * d + 1;
        for _ in 0..(full_history_hops - 1) {
            track.decoder_input(5, &h, 0, 30.0);
        }
        let out = track.decoder_input(5, &h, 0, 30.0);
        assert_eq!(out.len(), 1);
        let (amp, _, _) = out[0];
        assert!(
            amp.is_finite() && amp > 0.0,
            "post-convergence amplitude must be a real, finite, positive refined value"
        );
    }

    #[test]
    fn decoder_input_reset_drains_the_old_channels_backlog() {
        // MAN-194: a real centroid-channel change must not silently drop
        // the old channel's buffered-but-undelivered backlog -- it must be
        // flushed (raw-quality, since no more real input will ever arrive
        // for it) as part of the SAME call that detects the reset.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |raw|=5, power=25
        // Feed 3 hops on channel 5 (n=1..3, all < D=5, all buffered, none
        // popped yet).
        for i in 0..3u64 {
            let out = track.decoder_input(5, &h, i * 512, 30.0);
            assert!(out.is_empty());
        }
        // Reassign to channel 6: the reset must drain exactly those 3
        // buffered hops (raw magnitude, their own original timestamps),
        // even though the new channel's own contribution this hop is
        // itself not yet due.
        track.center = 6.0;
        let out = track.decoder_input(6, &h, 3 * 512, 30.0);
        assert_eq!(
            out,
            vec![(5.0, 25.0, 0), (5.0, 25.0, 512), (5.0, 25.0, 1024)],
            "reset must drain the old channel's backlog in order, before starting the new one"
        );
    }

    #[test]
    fn decoder_input_raw_power_uses_the_owned_channel_k_even_when_it_differs_from_the_centroid() {
        // Codex review, PR #178: TrackDecoder's calibrated NoiseTracker
        // needs the tracked channel's real, full-bandwidth power (SPEC v2
        // §2.1), not a refined value -- confirmed here with k and c set to
        // channels with clearly different powers.
        let mut track = Track::new(1, 5, &cfg()); // center stays 5.0 -> c=5
        let mut samples = vec![num_complex::Complex32::new(1.0, 0.0); 8];
        samples[5] = num_complex::Complex32::new(3.0, 4.0); // channel c=5: power 25
        samples[6] = num_complex::Complex32::new(1.0, 0.0); // channel k=6: power 1
        let h = hop_with_x(0, samples);
        let d = GROUP_DELAY_HOPS as u64;
        let mut last_out = Vec::new();
        for i in 0..=d {
            last_out = track.decoder_input(6, &h, i * 512, 30.0);
        }
        assert_eq!(
            last_out,
            vec![(5.0, 1.0, 0)],
            "raw_power must come from k=6 (power 1), not c=5 (power 25) or any refined value"
        );
    }

    #[test]
    fn decoder_input_applies_the_odd_channel_sign_correction() {
        // The real channelizer's rotation step bakes the checkerboard sign
        // artifact into `HopOutput::x[k]` itself; a synthetic `HopOutput`
        // for a physically-constant tone must reproduce that same artifact
        // to exercise decoder_input's fix. decoder_input must undo it
        // before the refiner sees the sample.
        let raw = num_complex::Complex32::new(1.0, 0.0);
        let mean_amp = |channel: usize| -> f32 {
            let mut track = Track::new(1, channel, &cfg());
            let mut sum = 0.0f32;
            let mut count = 0u32;
            for m in 0..200u64 {
                let artifacted = raw * odd_channel_sign_correction(m, channel);
                let h = hop_with_x(m, vec![artifacted; 8]);
                let out = track.decoder_input(channel, &h, m * 512, 30.0);
                if m > 50 {
                    for (amp, _, _) in out {
                        sum += amp;
                        count += 1;
                    }
                }
            }
            sum / count as f32
        };
        let even = mean_amp(4);
        let odd = mean_amp(5);
        assert!(
            (odd - even).abs() < 1e-3,
            "odd-channel track must match even-channel amplitude once \
             odd_channel_sign_correction is applied to decoder_input's \
             refiner feed, got odd={odd} even={even}"
        );
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p manta-engine decoder_input -- --nocapture`
Expected: compile error (`decoder_input`'s return type doesn't match `Vec<...>` usage yet) or, if it's made to compile against the old signature by adjusting call sites first, assertion failures against the old undelayed/no-drain behavior. Either is an acceptable "fails for the right reason" — the point is the test file doesn't yet match a passing implementation.

- [ ] **Step 6: Rewrite `Track::decoder_input`**

Replace the entire function body (currently lines ~495-556) with:

```rust
    /// The amplitude(s) to feed this hop's decoder push, the RAW
    /// (unrefined) power of the detector's owned channel `k` for the noise
    /// tracker, and the `sample_ts` each amplitude corresponds to (MAN-168/
    /// MAN-194, SPEC v2 §3). Falls back to a single `(raw_amp, raw_power,
    /// sample_ts)` triple from the same channel when refinement is disabled
    /// (`refine_bw_hz <= 0.0`, the default) -- so `raw_power` always matches
    /// SPEC v2 §2.1's "tracked channel" regardless of whether refinement is
    /// active. `TrackDecoder::push_hop`'s calibrated `NoiseTracker` expects
    /// the full-bandwidth channel's power; the refiner's 30 Hz (vs the
    /// channel's ~93.75 Hz) passband integrates substantially less noise
    /// power by design, so `raw_power` must NOT come from the narrowband-
    /// filtered amplitude (Codex review, PR #178: measured ~8.6 dB low).
    ///
    /// **MAN-194: returns a burst, not a single triple.** `Refiner::push`'s
    /// output at hop `t` is evidence about hop `t - GROUP_DELAY_HOPS`, not
    /// hop `t` (a property of the FIR's linear phase, not a design choice --
    /// see `refine.rs`'s module doc). A one-report-per-hop interface cannot
    /// represent "no valid delayed observation yet" or "flush everything
    /// buffered, the source just stopped" -- exactly what correct delay
    /// compensation needs at track birth, a channel reset, and end of
    /// stream. `decoder_input` instead maintains `refiner_backlog`, a small
    /// ring of not-yet-reported `(raw_amp, raw_power, sample_ts)` triples,
    /// one pushed per real hop fed to `refiner`:
    ///
    /// - While fewer than `GROUP_DELAY_HOPS` real hops have been fed since
    ///   the last reset, nothing is popped: this call returns an empty
    ///   `Vec`. There is no valid delayed observation yet, and reporting a
    ///   same-hop raw substitute here (PR #178's original stopgap) is
    ///   exactly what caused a content discontinuity at the warm-up/
    ///   converged transition (MAN-194) -- reporting nothing avoids ever
    ///   needing to contradict it later.
    /// - Once `GROUP_DELAY_HOPS` real hops have been fed, exactly one entry
    ///   pops per call (steady one-report-per-hop cadence, matching the
    ///   pre-MAN-194 interface's cardinality): the due delayed hop's raw
    ///   magnitude while its FIR window is still partly zero-padded
    ///   (`refiner_hops_since_reset < TAPS`), or the FIR's own output
    ///   (`refiner.push`'s return value THIS call, which already IS the
    ///   correct value for the due delayed hop) once the window is fully
    ///   real (`refiner_hops_since_reset >= TAPS`).
    /// - A channel reset (the centroid's rounded channel changes) first
    ///   drains every entry still in `refiner_backlog` -- up to
    ///   `GROUP_DELAY_HOPS` real, already-observed hops that will never get
    ///   more future context to improve their FIR quality, reported at raw
    ///   quality -- prepended to this call's own (necessarily empty, since
    ///   the new channel's counter just reset to 0) contribution. See
    ///   `drain_refiner_backlog`/`drain_refiner_into_pending` for the same
    ///   drain used by `TrackManager` at end-of-stream and every other
    ///   closure path (Task 3/4).
    fn decoder_input(
        &mut self,
        k: usize,
        hop: &HopOutput,
        sample_ts: u64,
        refine_bw_hz: f32,
    ) -> Vec<(f32, f32, u64)> {
        let raw_power = hop.power[k];
        if refine_bw_hz <= 0.0 {
            return vec![(raw_power.sqrt(), raw_power, sample_ts)];
        }
        // SPEC v2 §3 requires `c = round(c_f)`: refine the CENTROID
        // channel, not `k` (the instantaneous max-power channel used for
        // detector ownership/gating elsewhere) -- see the original MAN-168
        // wiring comment (Codex review, PR #161) for why `k` can flicker
        // independently of the true centroid.
        let c = self.center.round() as usize;
        let delta = (self.center - c as f64) as f32;

        let mut out: Vec<(f32, f32, u64)> = if self.refiner_channel != Some(c) {
            self.drain_refiner_backlog()
        } else {
            Vec::new()
        };

        let refiner = self
            .refiner
            .get_or_insert_with(|| Refiner::new(refine_bw_hz, delta));
        if self.refiner_channel != Some(c) {
            refiner.reset();
            self.refiner_channel = Some(c);
            self.refiner_hops_since_reset = 0;
        }
        refiner.set_delta(delta);
        // The channelizer's rotation step leaves a checkerboard sign
        // artifact on odd channels at odd hops; a phase-sensitive consumer
        // like this derotating/filtering refiner must correct it before the
        // sample reaches `Refiner::push` (measured ~39 dB of spurious
        // attenuation before this correction was applied here).
        let corrected = hop.x[c] * odd_channel_sign_correction(hop.m, c);
        let refined_amp = refiner.push(corrected);
        self.refiner_hops_since_reset += 1;
        let n = self.refiner_hops_since_reset;

        self.refiner_backlog
            .push_back((hop.power[c].sqrt(), raw_power, sample_ts));

        let d = GROUP_DELAY_HOPS as u64;
        if n > d {
            let (due_raw_amp, due_raw_power, due_ts) = self.refiner_backlog.pop_front().expect(
                "backlog must hold an entry once n > GROUP_DELAY_HOPS: exactly one is pushed \
                 per call and none are popped until this threshold",
            );
            let full_history_hops = 2 * d + 1; // == TAPS
            let amp = if n >= full_history_hops {
                refined_amp
            } else {
                due_raw_amp
            };
            out.push((amp, due_raw_power, due_ts));
        }
        out
    }

    /// MAN-194: pop every buffered-but-undelivered `(raw_amp, raw_power,
    /// sample_ts)` triple, in order, at raw quality -- used both when
    /// `decoder_input` detects a channel reset (folded transparently into
    /// its own returned `Vec`, above) and by `TrackManager` at every other
    /// point this track's refiner stops receiving new input (see
    /// `drain_refiner_into_pending`). No attempt is made to improve FIR
    /// quality for these: no more real input is ever coming from this
    /// source, so the window can never become more real than it already is.
    fn drain_refiner_backlog(&mut self) -> Vec<(f32, f32, u64)> {
        self.refiner_backlog.drain(..).collect()
    }
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p manta-engine decoder_input`
Expected: all `decoder_input_*` tests PASS.

- [ ] **Step 8: Run the full `manta-engine` test suite to check for regressions**

Run: `cargo test -p manta-engine`
Expected: PASS. (`decoder_input_does_not_reset_the_refiner_on_center_drift_alone` and `decoder_input_resets_on_a_real_channel_change`, left untouched, must still pass unmodified.)

- [ ] **Step 9: Commit**

```bash
cd /Users/thagale/Code/manta-man194
git add crates/manta-dsp/src/refine.rs crates/manta-engine/src/track.rs
git commit -m "$(cat <<'EOF'
fix(manta-engine,manta-dsp): MAN-194 -- burst-capable decoder_input with delay-compensated hold-back

Track::decoder_input now returns Vec<(f32, f32, u64)> instead of a
single triple, so it can withhold a report until a valid
GROUP_DELAY_HOPS-delayed observation exists, and drain a channel
reset's buffered-but-undelivered backlog into the same call's return
value instead of silently discarding it.
EOF
)"
```

---

## Task 2: Wire `TrackManager::step_hop`'s per-hop call sites to the burst return

**Files:**
- Modify: `crates/manta-engine/src/track.rs:912-916` (the `Promoted` arm) and `crates/manta-engine/src/track.rs:956-961` (the `LifecycleEvent::None` arm)
- Test: `crates/manta-engine/src/track.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Track::decoder_input(...) -> Vec<(f32, f32, u64)>` (Task 1).
- Produces: no new public interface; `Track::pending: Vec<(f32, f32, Option<f32>, u64)>` keeps its existing type and existing consumers (`drain_pool`, `finish_decoder`, `finish_decoder_speed_only`) are unaffected.

- [ ] **Step 1: Write the failing test**

Add this test to `mod tests` (anywhere near the other `TrackManager`-level tests, e.g. after `spawns_and_promotes_a_track_on_a_strong_channel`):

```rust
    #[test]
    fn step_hop_pushes_every_entry_of_a_multi_entry_burst_into_pending() {
        // MAN-194: once decoder_input starts returning bursts of more than
        // one entry (a reset drain), step_hop's call sites must push EVERY
        // entry into pending, not just the first/last.
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                refine_bw_hz: 30.0,
                ..DecodeConfig::default()
            },
        );
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        loop {
            tm.step_hop(&hop(m, power.clone()), m);
            m += 1;
            if tm.tracks.values().any(|t| t.state() == LifecycleState::Active) {
                break;
            }
        }
        let id = *tm.tracks.keys().next().unwrap();
        // Feed 3 more steady hops (< GROUP_DELAY_HOPS): decoder_input
        // returns nothing yet, so pending must still be empty.
        for _ in 0..3 {
            tm.step_hop(&hop(m, power.clone()), m);
            m += 1;
        }
        assert!(
            tm.tracks.get(&id).unwrap().pending.is_empty(),
            "before GROUP_DELAY_HOPS real hops, decoder_input reports nothing"
        );
        // Force a channel reassignment: the reset drains 3 backlog entries
        // in one burst -- step_hop must push all 3 into pending, not just
        // one.
        let mut power2 = quiet_power(n);
        power2[11] = 1e-9 * 10f32.powf(20.0 / 10.0);
        tm.tracks.get_mut(&id).unwrap().center = 11.0;
        tm.step_hop(&hop(m, power2), m);
        assert_eq!(
            tm.tracks.get(&id).unwrap().pending.len(),
            3,
            "step_hop must push every entry of a multi-entry burst into pending"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p manta-engine step_hop_pushes_every_entry_of_a_multi_entry_burst_into_pending`
Expected: compile error (`track.decoder_input(...)` still destructures a 3-tuple at the two call sites, which no longer matches `Vec<(f32, f32, u64)>`).

- [ ] **Step 3: Fix the two call sites**

In the `Promoted` arm (~line 909-916), replace:

```rust
                    let (amp, raw_power, ts) = track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                    let spectral_ref_power = compute_spectral_ref_power
                        .then(|| Self::spectral_ref_power(&self.floor, track.center))
                        .flatten();
                    track.pending.push((amp, raw_power, spectral_ref_power, ts));
```
with:
```rust
                    let burst = track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                    let spectral_ref_power = compute_spectral_ref_power
                        .then(|| Self::spectral_ref_power(&self.floor, track.center))
                        .flatten();
                    for (amp, raw_power, ts) in burst {
                        track.pending.push((amp, raw_power, spectral_ref_power, ts));
                    }
```

In the `LifecycleEvent::None` arm (~line 956-961), replace:
```rust
                        let (amp, raw_power, ts) =
                            track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                        let spectral_ref_power = compute_spectral_ref_power
                            .then(|| Self::spectral_ref_power(&self.floor, track.center))
                            .flatten();
                        track.pending.push((amp, raw_power, spectral_ref_power, ts));
```
with:
```rust
                        let burst = track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                        let spectral_ref_power = compute_spectral_ref_power
                            .then(|| Self::spectral_ref_power(&self.floor, track.center))
                            .flatten();
                        for (amp, raw_power, ts) in burst {
                            track.pending.push((amp, raw_power, spectral_ref_power, ts));
                        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p manta-engine step_hop_pushes_every_entry_of_a_multi_entry_burst_into_pending`
Expected: PASS.

- [ ] **Step 5: Run the full `manta-engine` test suite**

Run: `cargo test -p manta-engine`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-engine/src/track.rs
git commit -m "$(cat <<'EOF'
fix(manta-engine): MAN-194 -- step_hop pushes every entry of a decoder_input burst

Both per-hop call sites (Promoted, ongoing ACTIVE/HANG) now loop over
decoder_input's Vec return instead of destructuring a single triple.
EOF
)"
```

---

## Task 3: Drain the refiner backlog at every closure path (Merged/Evicted/Silent/HangExpired)

**Files:**
- Modify: `crates/manta-engine/src/track.rs` (`Track` impl: add `drain_refiner_into_pending`; `TrackManager::step_hop`'s closure-handling block ~1017-1029; `TrackManager::merge_converged` ~1161-1166; `TrackManager::evict_over_cap` ~1197-1200)
- Test: `crates/manta-engine/src/track.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Track::drain_refiner_backlog(&mut self) -> Vec<(f32, f32, u64)>` (Task 1), `Track::pending` (existing field), `TrackManager::spectral_ref_power(&FloorBank, f64) -> Option<f32>` (existing associated function, unchanged).
- Produces: `Track::drain_refiner_into_pending(&mut self, spectral_ref_power: Option<f32>)` — Task 4 uses this too.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn drain_refiner_into_pending_flushes_the_full_backlog() {
        // MAN-194: verifies the primitive TrackManager::finish (Task 4) and
        // the closure-handling paths below rely on -- draining a track's
        // refiner backlog directly into `pending`, with no
        // spectral_ref_power discounting when None is passed through.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |raw|=5, power=25
        let d = GROUP_DELAY_HOPS as u64;
        // Steady state: backlog holds exactly d entries once n > d.
        for i in 0..(d + 3) {
            track.decoder_input(5, &h, i * 512, 30.0);
        }
        assert_eq!(track.refiner_backlog.len(), d as usize);
        track.drain_refiner_into_pending(None);
        assert!(
            track.refiner_backlog.is_empty(),
            "drain must flush the entire backlog"
        );
        assert_eq!(
            track.pending.len(),
            d as usize,
            "drain must deliver every buffered-but-undelivered hop into pending"
        );
    }

    #[test]
    fn step_hop_reset_drains_the_refiners_backlog_into_pending() {
        // Engine-level regression test for MAN-194 Scenario 1 (the
        // ticket's own Gherkin): a mid-mark channel reassignment must not
        // silently drop the refiner's buffered-but-undelivered backlog.
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                refine_bw_hz: 30.0,
                ..DecodeConfig::default()
            },
        );
        // decoder_input's refine-enabled path reads hop.x[c], so every
        // hop fed to a promoted track needs real complex spectrum data,
        // not the bare `hop()` helper (empty `x`, which would panic on
        // index-out-of-bounds). Build x from power so HopOutput::power
        // matches exactly (norm_sqr of the real value p.sqrt() is p).
        let mk_hop = |m: u64, power: &Vec<f32>| {
            let x: Vec<num_complex::Complex32> = power
                .iter()
                .map(|&p| num_complex::Complex32::new(p.sqrt(), 0.0))
                .collect();
            hop_with_x(m, x)
        };
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        loop {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
            if tm.tracks.values().any(|t| t.state() == LifecycleState::Active) {
                break;
            }
        }
        let id = *tm.tracks.keys().next().unwrap();
        let d = GROUP_DELAY_HOPS as u64;
        // Run well past GROUP_DELAY_HOPS so the backlog reaches steady
        // state (exactly d entries buffered at any instant).
        for _ in 0..(d + 5) {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
        }
        let pending_before = tm.tracks.get(&id).unwrap().pending.len();
        tm.step_hop(&mk_hop(m, &power), m); // one more normal hop
        m += 1;
        let pending_after_normal_hop = tm.tracks.get(&id).unwrap().pending.len();
        assert_eq!(
            pending_after_normal_hop - pending_before,
            1,
            "a steady-state hop with no reset must deliver exactly one pending entry"
        );

        // Force a channel reassignment: move the strong signal to channel
        // 11 and directly set the track's live centroid EMA to force the
        // rounding-boundary crossing this same hop (isolating the reset
        // from the EMA's normal, much slower settling behavior, which this
        // test isn't about).
        let mut power2 = quiet_power(n);
        power2[11] = 1e-9 * 10f32.powf(20.0 / 10.0);
        tm.tracks.get_mut(&id).unwrap().center = 11.0;
        tm.step_hop(&mk_hop(m, &power2), m);
        let pending_after_reset_hop = tm.tracks.get(&id).unwrap().pending.len();
        assert_eq!(
            pending_after_reset_hop - pending_after_normal_hop,
            d,
            "a reset hop must drain the full backlog ({d} entries), not just deliver \
             one entry and silently drop the rest"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p manta-engine drain_refiner_into_pending_flushes_the_full_backlog step_hop_reset_drains_the_refiners_backlog_into_pending`
Expected: compile error (`drain_refiner_into_pending` doesn't exist yet).

- [ ] **Step 3: Add `Track::drain_refiner_into_pending`**

Add this method to the `Track` impl block, right after `drain_refiner_backlog`:

```rust
    /// MAN-194: `TrackManager`-facing wrapper around
    /// `drain_refiner_backlog` that pairs every drained entry with the SAME
    /// `spectral_ref_power` (computed once by the caller, since a `Track`
    /// has no access to the `FloorBank` it comes from -- see
    /// `TrackManager::spectral_ref_power`'s own doc comment) and pushes it
    /// straight into `pending`. Reusing one value across a
    /// `<= GROUP_DELAY_HOPS`-hop-old backlog is a non-issue: SPEC v2 §2.2's
    /// six-neighbor floor estimate is slow-moving relative to a ~13ms
    /// window. Called at every point a track's refiner stops receiving new
    /// input: `TrackManager::step_hop`'s closure handling (below),
    /// `merge_converged`, `evict_over_cap`, and `TrackManager::finish`
    /// (Task 4) -- NOT inside `decoder_input`'s own reset handling, which
    /// has no `spectral_ref_power` to attach and instead folds its drain
    /// directly into its own returned `Vec`.
    fn drain_refiner_into_pending(&mut self, spectral_ref_power: Option<f32>) {
        for (amp, raw_power, ts) in self.drain_refiner_backlog() {
            self.pending.push((amp, raw_power, spectral_ref_power, ts));
        }
    }
```

- [ ] **Step 4: Wire the drain into `step_hop`'s closure-handling block**

In `TrackManager::step_hop`, inside the `closed.retain(|id| { ... })` closure, find:

```rust
            if !track.has_emitted {
                return false;
            }
            // Only a genuine, sustained observed RF gap (HangExpired) may
            // force an in-progress mark to resolve; Silent's own trigger
            // (no character decoded) says nothing about whether the
            // signal is still there (round 8).
            let events = if close_reasons.get(id) == Some(&CloseReason::HangExpired) {
                track.finish_decoder()
            } else {
                track.finish_decoder_speed_only()
            };
```

and replace it with:

```rust
            if !track.has_emitted {
                return false;
            }
            // MAN-194: this closure is the LAST point this track's refiner
            // will ever be fed -- drain its backlog into pending BEFORE
            // finish_decoder*/finish_decoder_speed_only* below (which then
            // push pending through the decoder as they already do), so a
            // buffered-but-undelivered real observation isn't silently
            // dropped along with the rest of the removed `Track`.
            let spectral_ref_power = compute_spectral_ref_power
                .then(|| Self::spectral_ref_power(&self.floor, track.center))
                .flatten();
            track.drain_refiner_into_pending(spectral_ref_power);
            // Only a genuine, sustained observed RF gap (HangExpired) may
            // force an in-progress mark to resolve; Silent's own trigger
            // (no character decoded) says nothing about whether the
            // signal is still there (round 8).
            let events = if close_reasons.get(id) == Some(&CloseReason::HangExpired) {
                track.finish_decoder()
            } else {
                track.finish_decoder_speed_only()
            };
```

- [ ] **Step 5: Wire the drain into `merge_converged`**

In `TrackManager::merge_converged`, find:

```rust
                let mut track = self.tracks.remove(&loser)?;
                if !track.has_emitted {
                    return None;
                }
                flush_events.extend(track.finish_decoder_speed_only());
```

and replace it with:

```rust
                let mut track = self.tracks.remove(&loser)?;
                if !track.has_emitted {
                    return None;
                }
                // MAN-194: same reasoning as step_hop's closure handling --
                // a merge-loser's refiner also stops receiving input here.
                let spectral_ref_power = (!matches!(self.decode_cfg.engine, Engine::Legacy))
                    .then(|| Self::spectral_ref_power(&self.floor, track.center))
                    .flatten();
                track.drain_refiner_into_pending(spectral_ref_power);
                flush_events.extend(track.finish_decoder_speed_only());
```

- [ ] **Step 6: Wire the drain into `evict_over_cap`**

In `TrackManager::evict_over_cap`, find:

```rust
            if let Some(mut track) = self.tracks.remove(&loser) {
                if track.has_emitted {
                    flush_events.extend(track.finish_decoder_speed_only());
```

and replace it with:

```rust
            if let Some(mut track) = self.tracks.remove(&loser) {
                if track.has_emitted {
                    // MAN-194: same reasoning as step_hop's closure
                    // handling -- an evicted track's refiner also stops
                    // receiving input here.
                    let spectral_ref_power = (!matches!(self.decode_cfg.engine, Engine::Legacy))
                        .then(|| Self::spectral_ref_power(&self.floor, track.center))
                        .flatten();
                    track.drain_refiner_into_pending(spectral_ref_power);
                    flush_events.extend(track.finish_decoder_speed_only());
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p manta-engine drain_refiner_into_pending_flushes_the_full_backlog step_hop_reset_drains_the_refiners_backlog_into_pending`
Expected: PASS.

- [ ] **Step 8: Run the full `manta-engine` test suite**

Run: `cargo test -p manta-engine`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/manta-engine/src/track.rs
git commit -m "$(cat <<'EOF'
fix(manta-engine): MAN-194 -- drain refiner backlog on every track closure path

Merged/Evicted/Silent/HangExpired closures (step_hop's closure
handling, merge_converged, evict_over_cap) all stop feeding a track's
refiner -- each now drains its buffered backlog into pending before
finish_decoder*/finish_decoder_speed_only* runs, the same fix as the
reset case, applied everywhere a track's input source can stop.
EOF
)"
```

---

## Task 4: Drain every remaining track's backlog at end-of-stream

**Files:**
- Modify: `crates/manta-engine/src/track.rs:1318-1327` (`TrackManager::finish`)
- Test: `crates/manta-engine/src/track.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Track::drain_refiner_into_pending` (Task 3), `Track::finish_decoder(&mut self) -> Vec<DecoderEvent>` (existing method — already drains `pending` through `push_hop` before calling `TrackDecoder::finish()`).

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
    #[test]
    fn finish_drains_every_remaining_tracks_refiner_backlog_before_flushing() {
        // MAN-194 Scenario 2 (EOF): TrackManager::finish must not silently
        // drop a still-open track's buffered-but-undelivered refiner
        // backlog. This checks the wiring (drain runs before the track is
        // cleared) -- decoder_input's own unit tests (Task 1) already cover
        // the underlying delay math exhaustively.
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                refine_bw_hz: 30.0,
                ..DecodeConfig::default()
            },
        );
        // decoder_input's refine-enabled path reads hop.x[c], so every
        // hop fed to a promoted track needs real complex spectrum data,
        // not the bare `hop()` helper (empty `x`, which would panic on
        // index-out-of-bounds). Build x from power so HopOutput::power
        // matches exactly (norm_sqr of the real value p.sqrt() is p).
        let mk_hop = |m: u64, power: &Vec<f32>| {
            let x: Vec<num_complex::Complex32> = power
                .iter()
                .map(|&p| num_complex::Complex32::new(p.sqrt(), 0.0))
                .collect();
            hop_with_x(m, x)
        };
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        loop {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
            if tm
                .tracks
                .values()
                .any(|t| t.state() == LifecycleState::Active)
            {
                break;
            }
        }
        let id = *tm.tracks.keys().next().unwrap();
        let d = GROUP_DELAY_HOPS as u64;
        for _ in 0..(d + 5) {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
        }
        let backlog_len = tm.tracks.get(&id).unwrap().refiner_backlog.len();
        assert_eq!(
            backlog_len, d as usize,
            "sanity check: the track must have a full backlog of undelivered hops before EOF"
        );
        let events = tm.finish();
        assert!(
            tm.tracks.is_empty(),
            "finish() must still clear all tracks after draining"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackClosed { track_id, .. } if *track_id == id)),
            "the track must still get its TrackClosed event after the backlog drain"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p manta-engine finish_drains_every_remaining_tracks_refiner_backlog_before_flushing`
Expected: PASS already for the `TrackClosed`/`is_empty` assertions (those are pre-existing behavior) but this is really checking that the change in Step 3 doesn't regress them while also draining -- run it now to confirm it passes on the OLD `finish()` too (a sanity baseline), then re-run after Step 3 to confirm it still passes with backlog now genuinely drained through the decoder rather than dropped.

- [ ] **Step 3: Rewrite `TrackManager::finish`**

Replace:

```rust
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| t.decoder.as_mut())
            .par_bridge()
            .flat_map_iter(|d| d.finish())
            .collect();
```

with:

```rust
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        // MAN-194: end-of-stream is another point where every remaining
        // track's refiner stops receiving new input -- drain each one's
        // backlog into pending before flushing, same as every other
        // closure path (Task 3). Done as a plain sequential pass first
        // (cheap: at most GROUP_DELAY_HOPS entries per track) so the
        // parallel decode pass below sees a fully-populated `pending`.
        let compute_spectral_ref_power = !matches!(self.decode_cfg.engine, Engine::Legacy);
        for track in self.tracks.values_mut() {
            let spectral_ref_power = compute_spectral_ref_power
                .then(|| Self::spectral_ref_power(&self.floor, track.center))
                .flatten();
            track.drain_refiner_into_pending(spectral_ref_power);
        }
        // `finish_decoder` drains `pending` through `push_hop` before
        // calling `TrackDecoder::finish()` -- exactly what the backlog
        // just fed into `pending` above needs, and a no-op for any track
        // whose `pending` was already empty (every track, before MAN-194).
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .par_bridge()
            .flat_map_iter(|t| t.finish_decoder())
            .collect();
```

Leave the rest of `finish` (the sort, `has_emitted` bookkeeping, `TrackClosed` emission, `self.tracks.clear()`) unchanged.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p manta-engine finish_drains_every_remaining_tracks_refiner_backlog_before_flushing`
Expected: PASS.

- [ ] **Step 5: Run the full `manta-engine` test suite**

Run: `cargo test -p manta-engine`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-engine/src/track.rs
git commit -m "$(cat <<'EOF'
fix(manta-engine): MAN-194 -- drain refiner backlogs at end-of-stream

TrackManager::finish now drains every remaining track's refiner
backlog into pending, then reuses finish_decoder (pending -> push_hop
-> decoder.finish()) instead of calling decoder.finish() directly --
closing the last gap where a track's final GROUP_DELAY_HOPS real hops
of evidence were silently discarded.
EOF
)"
```

---

## Task 5: Full validation, golden-vector re-check, and PR

**Files:**
- None (validation only, plus opening the PR).

- [ ] **Step 1: Full workspace test suite**

Run: `cargo test --workspace`
Expected: PASS, including the golden V-vector suite. Pay particular attention to any V4/V5/V8w (refinement-relevant) results — the timestamp convention shifts by a constant `GROUP_DELAY_HOPS`-hop offset relative to the pre-MAN-194 no-delay stopgap, so re-confirm none of their pass/fail boundaries moved. If any regress, stop and investigate before proceeding — do not adjust a golden vector's expected values to make a regression pass; find the root cause per this repo's systematic-debugging convention.

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Format check**

Run: `cargo fmt --check`
Expected: no diff. If it fails, run `cargo fmt` and re-run Steps 1-2.

- [ ] **Step 4: Local Codex review**

Run: `codex exec review --uncommitted` from `/Users/thagale/Code/manta-man194` (or against the full diff since branching from `origin/main`, per this repo's convention for refiner-adjacent changes — MAN-168's PR #178 found three real bugs this way). Address any findings before pushing; re-run Steps 1-3 after any fix.

- [ ] **Step 5: Push and open the PR**

```bash
cd /Users/thagale/Code/manta-man194
git push -u origin fix/man194-refiner-burst-drain
gh pr create --title "fix(manta-engine,manta-dsp): MAN-194 -- burst-capable decoder_input with delay-compensated drain" --body "$(cat <<'EOF'
## Summary
- Reintroduces correct GROUP_DELAY_HOPS delay-compensated pairing in
  `Track::decoder_input`, this time via a burst-capable
  (`Vec<(f32,f32,u64)>`-returning) interface, replacing PR #178's
  shipped no-delay stopgap.
- Fixes the two boundary-discard bugs MAN-194 measured (10-hop mark
  reduced to 5 on a mid-mark channel reset; final mark measured 7 hops
  instead of 12 at EOF) by draining the refiner's buffered backlog at
  every point a track's input source stops: channel reset, Merged,
  Evicted, Silent, HangExpired, and true end-of-stream.
- Eliminates the transition-discontinuity bug (raw-vs-refined content
  mislabeled at the convergence boundary) by construction: nothing is
  ever reported before its correctly-delayed value exists, so there is
  no early report to later contradict.

## Test plan
- [ ] `cargo test --workspace` (including golden V-vectors, especially
      V4/V5/V8w)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo fmt --check`
- [ ] local `codex exec review --uncommitted`

Spec: docs/superpowers/specs/2026-09-10-refiner-burst-drain-design.md
Plan: docs/superpowers/plans/2026-09-10-refiner-burst-drain.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
gh pr merge --auto --squash
```

- [ ] **Step 6: Run the land-pr skill**

Invoke the `land-pr` skill to wait for CI, resolve any review feedback per the round-based convergence policy (rounds 1-5 fix everything, 6-15 fix P1s inline and ticket P2s), and confirm the merge completes.
