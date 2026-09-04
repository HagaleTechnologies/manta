# MAN-3: short, high-WPM texts silently decoding to zero output

## Background

`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`'s 500-case sweep
surfaced, but did not investigate, a failure family distinct from that fix's
`CHAR_GAP_DITS` issue: some 2-character texts at 34-39 WPM decoded to an
empty string with **zero** `CharDecoded` events -- not garbled output, a
total absence of it. MAN-3 is the root-cause pass that doc deferred. Four
repro tuples were provided (`text, wpm, snr_2500_db, offset_hz, seed`), all
via `manta_testkit::scene::render_scene` + `manta_engine::decode_samples`,
`fs=96_000.0`, `center_freq_hz=0.0`, `jitter: None`:

| text | wpm | snr_2500_db | offset_hz | seed |
|---|---|---|---|---|
| DA | 35.0 | 28.3 |  14000.0 | 63032404875482 |
| VE | 39.1 | 20.9 |   1000.0 | 685126706563701970 |
| Z5 | 37.5 | 29.9 | -32000.0 | 10751217158967957828 |
| D5 | 34.1 | 26.8 |  19000.0 | 10012388600385395947 |

The ticket's own hypothesis was a `SpeedTracker`/`GapClassifier` mark-quorum
problem (`ClusterPair` needs 5 marks to leave `init`) and asked whether the
`total_marks(text) >= 5` proptest guard was actually invalid for these
cases. It is not: `total_marks("DA") = 3 + 2 = 5` (exactly the quorum, not
narrowly missed), `TrackDecoder::on_run`'s retroactive pending-buffer drain
is correct and covered by its own passing tests
(`crates/manta-decode/src/decoder.rs`'s `first_characters_are_not_lost`,
`crates/manta-testkit/tests/roundtrip_envelope.rs`'s detector-free
`clean_envelope_roundtrip` proptest), and no debounce/hysteresis in `Demod`
merges marks at these WPMs (`debounce_hops` is ~4-5 hops; even the fastest
dit here, VE's 30.7 ms, is ~11.5 hops). **The failure is not in
`manta-decode`.** It is two layers up, in `manta-engine`'s real detector
(`crates/manta-engine/src/track.rs`) and in `decode_samples`'s single-track
reporting (`crates/manta-engine/src/lib.rs`).

## Root cause, part 1: `decode_samples` could report a dead track instead of the one that decoded

`decode_samples` derived its single "headline" `.text`/`.wpm`/`.freq_hz`
from the **lowest `track_id`** present in the multi-track event stream
(`lib.rs`'s old `min_track_id` reduction), while returning the full,
unfiltered multi-track stream as `DecodeReport.events`. `TrackManager::
spawn`'s `next_id` is spawn-ordered and unrelated to which track actually
captures a signal. A short, fast signal routinely produces several
short-lived tracks in succession for one physical carrier -- measured live
for "D5" at HEAD: `track_ids: [6, 16]`, both independently decoding the same
looped text, with track 6 (the lower id) reported even though track 16 held
more (or equally complete) output.

**Fix (Phase 2):** replaced the `min_track_id` reduction with
`select_report_track` (`lib.rs`): most `CharDecoded` events wins, ties break
to the lowest `track_id` (preserving the old behavior exactly on the
all-telemetry-only case). Implemented as a pure function of the event
multiset via a `BTreeMap` scan (order-independent by construction --
SPEC §8). Also split `decode_samples`'s two `bail!` sites: the pre-existing
"no signal found (input shorter than one filter length or empty)" now fires
only when the channelizer produced zero hops; a new, distinct message fires
when hops were produced but no track ever promoted-and-emitted, naming the
hop count and pointing at SPEC §2.4 instead of implying an input-length
problem (no test anywhere matched the old message text; verified by grep).

This alone fixes any case where **some** track fully decoded the signal but
an earlier, doomed track happened to hold a lower id. It cannot fix a case
where **no** track ever survives long enough to decode anything, which is
part 2.

## Root cause, part 2: CANDIDATE confirmation was undetectable for most short, fast signals

`Lifecycle::on_hop`'s CANDIDATE arm required **19 *consecutive*** rise hops
(`confirm_hops`, ~50.7 ms) to promote; a single non-rise hop closed the
candidate `Unconfirmed`, invisibly (no event; `has_emitted` stays `false`).
19 consecutive hops is 50.7 ms of sustained *smoothed* key-down, but at
34-40 WPM a dit is only 30-35 ms, and the Gate's τ=40 ms EMA
(`manta-dsp::floor::Gate`) decays back below the on-threshold across the
following inter-element gap -- so only a **dah** could ever hold the rise
condition for the full 19 hops, and only if a CANDIDATE happened to be born
exactly on its leading edge. Everything else died `Unconfirmed` before ever
reaching a decoder (measured: 12 invisible failed candidates before the
first surviving one, for "DA").

**Fix (Phase 3):** `Lifecycle` now accumulates `confirm_hops` rise hops
**cumulatively** within a bounded `confirm_window_hops` window (new
`DetectorConfig` field, default 75 hops = 200 ms) counted from CANDIDATE
birth, instead of requiring them consecutive. A non-rise hop no longer
kills the candidate; only exhausting the window without reaching the count
does. This is a **strict relaxation**: any signal slow enough that 19 rise
hops already arrive consecutively promotes on the identical hop as before
(V1-V10's promotion hops are unchanged; verified by
`consecutive_rise_still_promotes_on_the_nineteenth_hop` and the unchanged
`active_track_decodes_real_text` CER bound). `confirm_window_hops` values
below `confirm_hops` are clamped up to it (`Lifecycle::new`) so promotion
can never become impossible via misconfiguration.

**Why 200 ms.** At 40 WPM (dit = 30 ms = 11.25 hops), no single dit can hold
the τ=40 ms smoothed gate above threshold for 19 consecutive hops, but a
200 ms window spans ~6.7 dit-units -- enough for two or three marks to
accumulate 19 rise hops at CW's ~50% key-down duty even with the EMA's
rise/decay lag, while staying short enough to remain a confirmation window
rather than a second hang timer. At 10 WPM (dit = 120 ms = 45 hops) a single
dit already satisfies the consecutive rule, so the window is never reached.

## Root cause, part 3 (found via Phase 0 instrumentation on part 2's own fix): a merge tie-break systematically evicted the established track

Implementing part 2 alone and re-running the four repro cases individually
(not through the ticket's shared test loop, which short-circuits on the
first failure) showed **two of the four cases got worse, not better**: "DA"
regressed from passing (at HEAD, via part 1 alone) to `Err("no signal
found... no track was ever promoted and emitted")`, and "Z5" -- which part 2
alone left unfixed, as predicted -- was *still* failing the same way even
with part 2's window relaxation in place. This contradicted the "strict
relaxation" reasoning above, which only considered promotion in isolation
and not the effect of longer-lived CANDIDATEs on ownership/merge dynamics.

**Instrumentation** (temporary `eprintln!`s in `TrackManager::{step_hop,
spawn, merge_converged}`, gated on `MAN3_DEBUG`, removed before commit;
per-case throwaway diagnostic harness run via `cargo test`) showed, for
"DA" (offset 14000 Hz / 93.75 Hz-per-channel = channel 149.33 -- a
fractional position roughly a third of the way from channel 149 to 150):

```
hop=751  id=1  SPAWN channel=149
hop=769  id=1  PROMOTED channel=149
hop=929  id=2  SPAWN channel=151
hop=947  id=2  PROMOTED channel=150
id=1 center=149.216 snr=37.84 vs id=2 center=150.211 snr=37.84 -- MERGE, loser=1
hop=1201 id=3  SPAWN channel=148
hop=1219 id=3  PROMOTED channel=149
id=2 center=149.588 snr=22.30 vs id=3 center=148.594 snr=22.30 -- MERGE, loser=2
... (19 promotions total across the 12 s / 4501-hop scene, every one merged
     away within 79-295 hops of its own promotion -- never reaching the
     ~375-hop `Demod` init window, so `has_emitted` never became true for
     any of them)
```

Two things, together, are the mechanism:

1. **The relaxed window (part 2) made a *second*, boundary-adjacent
   CANDIDATE reliably promotable.** A true center whose fractional channel
   offset isn't near 0 or 1 (14000/93.75 = 149.33; -32000/93.75 = -341.33,
   both close to a third/two-thirds split) leaks real signal energy into
   *both* neighboring PFB channels. Under the old, strict consecutive-hop
   rule, the weaker neighbor's gate flickered too often to ever sustain 19
   straight rises, so a second track rarely got the chance to promote at
   all. Under the windowed rule it promotes almost every time (own
   confirmation completing in as few as 18 hops here -- nowhere near
   needing the window's slack).
2. **`merge_converged`'s tie-break always killed the *older* track on an
   exact SNR tie -- and exact ties are the common case here, not an edge
   case.** Two tracks whose ~3-channel owned windows overlap (149's
   `{148,149,150}` and 150's `{149,150,151}`) both run `select_channel`
   over the *same* shared peak channel and so read **bit-identical**
   `current_snr_db` (see the trace above: `37.84` vs `37.84`, `22.30` vs
   `22.30`). The old code, `let loser = if snr_a <= snr_b { a } else { b
   }` with `a` always the lower/older id (`ids` is a BTreeMap-ordered
   ascending scan), broke that tie in favor of `b` -- the *newer* track --
   every single time. So the moment a boundary-adjacent second candidate
   promotes, it evicts the incumbent outright, regardless of which one was
   actually further along toward a real decode. The freed channel then
   spawns yet another candidate, and the cycle repeats indefinitely: 19
   promotions, 0 survivors, for "DA"; 31 promotions, 0 survivors, for "Z5".
   "VE" and "D5" (fractional offsets 10.67 and 202.67, both closer to a
   whole channel) mostly escaped this -- "D5" occasionally got a long
   enough gap between merges (up to ~786 hops, by chance) to decode a
   fragment before its track, too, was evicted, which is exactly the
   `track_ids: [6, 16]`-shaped partial success part 1 targets.

**Fix:** `merge_converged`'s comparison changed from `<=` to strict `<`
(`crates/manta-engine/src/track.rs`, one operator). On an exact tie the
*incumbent* now survives -- there is no evidence a bit-identical reading
means the older track is actually weaker, and the older track is by
definition closer to accumulating the ~375-hop continuity `Demod`'s init
window and `SpeedTracker`'s 5-mark quorum need. A track only loses a merge
now when it reads a **strictly** lower SNR than its competitor, which is
still true and unchanged for genuinely different-strength signals
(interference, pileups) -- V7/V8w's pileup gates and
`merge_closes_the_lower_snr_track_when_centers_converge`'s hand-built
8.0 dB vs 18.0 dB case are decisive, non-tied comparisons and are
unaffected.

This fix is filed under MAN-3 (not a separate ticket) because it was
discovered *as a direct, causal consequence* of part 2's own relaxation --
without it, part 2 is a net regression for exactly the boundary-straddling
signals it was meant to help, which is precisely what live instrumentation
against the ticket's own four cases caught before this shipped.

## Measured results

All four cases, run individually (not short-circuited) via the ticket's own
`render_scene`/`decode_samples` harness, `duration_s = 12.0`
(`max(keyed_len/fs + 1.5, 12.0)`, matching `roundtrip_iq.rs`'s
`iq_roundtrip_with_noise` formula for these keyed lengths):

| case | before (HEAD) | after part 1 only | after parts 1+2, before part 3's merge fix | after all three fixes |
|---|---|---|---|---|
| DA | `"E DA"` (works at 12 s; fails most durations 2.0-9.75 s) | `"E DA"` (unchanged -- part 1 doesn't touch this case) | `Err`, 0 events (**regression**) | `"E DA DA DA DA..."`, 27 chars |
| VE | `"TE VE VE..."` (works from 3.25 s) | unchanged | unchanged | `"TE VE VE..."`, 33 chars |
| Z5 | `Err`, 0 events (fails 39/41 durations 2.0-12.0 s) | `Err`, 0 events (part 1 alone cannot fix a track-less decode) | `Err`, 0 events (**still broken**) | `"L5 Z5 Z5 Z5..."`, 21 chars |
| D5 | `"E5 D5"` (works from 5.0 s), `track_ids: [6, 16]` | unchanged | unchanged | `"E5 D5 D5 D5..."`, 22 chars |

All four now decode with at least one `CharDecoded` event and non-empty
`.text`; DA/VE/D5 contain their keyed text verbatim (the pass criterion
this plan set for them). Z5's decode is deliberately *not* held to the
verbatim bar in the regression test -- see "What this does not fix" below.

**False-track measurement gate (`TrackManager::promoted_count()`, new,
Phase 3):**

| scenario | promoted_count |
|---|---|
| Pure AWGN, 3 seeds, 60 s, 1024 channels, past the 2 s warmup | 0 (unchanged from the `on_snr_db=12.0` retune's original guarantee) |
| V1 (single clean +20 dB signal, 1024 channels, 120 s) | 1 |

Both measured with the full set of changes (parts 1-3) in place; the
noise-only and V1 tests are permanent regression gates
(`track::tests::noise_only_scene_promotes_no_tracks`,
`track::tests::v1_promotes_exactly_one_track`), not one-off measurements.

**Duration sweep (the honest measure of whether the fix is general or
duration-lucky):** the regression test pins `duration_s = 12.0`; the plan
called for sweeping all four cases at 250 ms resolution to check the fix
isn't tuned to that one point. Run via a throwaway `cargo run --release
--example` harness (same four tuples/seeds, `render_scene` +
`decode_samples`, durations `2.0..=19.5 s` in 0.25 s steps; deleted before
commit, not shipped code) with the full set of changes (parts 1-3) in
place:

| case | first duration with verbatim decode | durations that still fail (of 71 swept, 2.0-19.5 s) |
|---|---|---|
| DA | 3.25 s | 2.0, 2.25, 2.5, 2.75, 3.0 s |
| VE | 3.25 s | 2.0, 2.25, 2.5, 2.75, 3.0 s |
| Z5 | 3.75 s | 2.0, 2.25, 2.5, 2.75, 3.0, 3.25, 3.5 s |
| D5 | 3.50 s | 2.0, 2.25, 2.5, 2.75, 3.0, 3.25 s |

All four cases decode their keyed text verbatim at every duration >= 4.0 s,
including "Z5" (which the regression test itself does not hold to the
verbatim bar -- see "What this does not fix" below; the sweep shows it
*can* decode verbatim, just not asserted as a permanent guarantee). The
short failures below ~3.5-3.75 s are structural, not a fix regression: they
sit below the ~2 s AGC/noise-floor warmup plus the confirm-window and
`Demod` init-continuity floor every case needs regardless of WPM, the same
floor the regression test's own 12.0 s pin and `roundtrip_iq.rs`'s
`max(keyed_len/fs + 1.5, 12.0)` formula both clear by a wide margin. The
fix is general across duration, not tuned to the regression test's 12.0 s
point.

**CPU budget:** `cpu_budget_mac_under_half_core` (`tests/cpu_budget.rs`) is
`#[ignore]`d in this environment (no reachable macOS/Pi4 hardware --
ROADMAP.md's M2 acceptance already tracks the Pi4 leg as unmet for reasons
unrelated to this ticket) and was not run. The separate criterion bench the
plan actually asked for, `cargo bench -p manta-engine --bench cpu_budget`
(`benches/cpu_budget.rs`, 300 concurrent tracks at 192 kS/s), **is**
runnable on Linux and was run with the full set of changes (parts 1-3) in
place:

```
cpu_budget/192khz_300tracks
                        time:   [16.519 s 16.777 s 17.073 s]
                        (10 samples, 15.0 s of 192 kS/s audio decoded per iteration)
```

That is this container's core, not Mac-series or Pi4 hardware, so it is not
a pass/fail measurement against ROADMAP.md's M2 acceptance criterion (<50%
of one M-series core, <1 Pi4 core) -- only a relative data point. It is
close to 1x real-time here (16.8 s to decode 15.0 s of audio on one shared
container core), with no crash, panic, or runaway allocation across 10
samples despite the 300 concurrent tracks all confirming under the new
windowed rule. A CANDIDATE can now live up to 200 ms instead of dying on
its first non-rise hop, which raises peak concurrent `Lifecycle` count;
this result is at least evidence that the relaxation does not blow up cost
catastrophically at the pileup scale, though it is not a substitute for the
actual Mac/Pi4 measurement. The Pi4-specific pass/fail threshold in
`cpu_budget_mac_under_half_core` remains unmeasured (no Pi4 hardware here)
and is unrelated to this ticket's change -- flagged for whoever next runs
it, consistent with ROADMAP.md's existing open M2 Pi4 acceptance leg.

**Chunking/determinism:** `chunking_determinism` and
`channelizer_chunking_determinism` both pass unchanged -- the confirm
window and the merge tie-break are both hop-counted integer/float
comparisons with no wall-clock or chunk-boundary dependence, so SPEC §8's
"byte-identical output regardless of chunk boundaries" guarantee is
unaffected by construction.

## What this does not fix

- **Z5's decoded text is not asserted verbatim.** `"Z5"` decoded correctly
  in this investigation's measurements (`"L5 Z5 Z5 Z5..."`, an "L5" leading
  artifact from the necessarily-partial first post-promotion repetition,
  same shape as DA/D5's leading artifacts), but MAN-3's own regression test
  deliberately does not hold it to the byte-verbatim bar: an earlier,
  narrower historical sample from before this fix showed at least one
  Z5-shaped run garbling to `"SZ"` under different timing, a character-merge
  failure distinct from -- and owned by -- MAN-5/MAN-6, not this ticket's
  zero-output scope.
- **`roundtrip_iq.rs::iq_roundtrip_with_noise` stays `#[ignore]`d.** It is
  blocked on three other, separately-filed real-detector bugs (issues
  #12 offset_hz==0 dead-DC-channel, #22 the 10.0-10.15 WPM cliff, #23
  non-converging garble = MAN-6), none of which this ticket touches. Run
  once locally post-fix (`cargo test -p manta-engine --test roundtrip_iq --
  --ignored`) to confirm it still fails only on those three pre-existing,
  unrelated failure modes and not on anything newly introduced here.
- **Ticket's issue reference.** MAN-3's metadata cites
  `HagaleTechnologies/manta#12`, but issue #12 as described in
  `roundtrip_iq.rs`'s own doc comment is specifically the `offset_hz == 0`
  dead-DC-channel case; none of MAN-3's four tuples use offset 0. The
  ticket reference is approximate -- this fix does not touch offset_hz==0,
  which remains open.

## Config/API changes

- `DetectorConfig` gains `confirm_window_hops: u64` (default 75). The
  struct is `Copy` and every in-tree construction site uses
  `..Default::default()`, so no in-tree call site breaks; an out-of-tree
  constructor using a full struct literal would -- acceptable pre-M3
  (crate not published).
- `TrackManager` gains `promoted_count() -> u64`, wired into
  `soak_metrics::{SoakMetricsSample, SoakMetricsReport}` and the
  `manta-soak-harness` JSONL/summary output alongside the existing
  `close_counts`, ahead of the real M3 Prometheus endpoint (same pattern as
  issue #26's `CloseCounts`).
- `decode_samples`'s `.text`/`.wpm`/`.freq_hz` can now differ from HEAD for
  any input where more than one track ever emitted -- always in the
  direction of *more* decoded text, never less. `.events`/`.spots` are
  byte-unchanged by the reporting fix (part 1); they can change from the
  merge-tie-break and confirm-window fixes (parts 2-3) exactly where those
  fixes change which physical track survives to decode -- which is the
  point.
- The `"no signal found"` error message text changed (split into two
  distinct messages). No test matched the old text (verified by grep);
  `manta-cli` surfaces it verbatim to the operator, which is the reason to
  make it truthful.

## SPEC / wiki cross-references

- `docs/SPEC-decode-core.md` §2.4's CANDIDATE transition rows and §9's
  `[detector]` table now document `confirm_window_hops`/`confirm_window_ms`
  as a normative deviation from the literal "19 consecutive hops" wording,
  citing this document.
- `wiki/pages/detector-tracks.md` points at this document for both the
  CANDIDATE-confirmation deviation and the `select_report_track` reporting
  rule.
- `docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`'s "Known
  limitations" item 2 (this ticket's origin) cross-references this
  document as its resolution.
