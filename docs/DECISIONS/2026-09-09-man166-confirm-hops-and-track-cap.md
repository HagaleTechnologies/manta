# MAN-166: track_cap raised; confirm_hops investigated and deliberately left alone

## Background

MAN-166 built manta's first real-world (non-synthetic) recall/precision
benchmark: a genuine 192kHz I/Q recording of CQ WW CW 2025 on 40m (15
minutes, dense contest QRM, `crates/manta-testkit/audio-corpus/
B2_20251129_000000_7080kHz.wav`), scored against the Reverse Beacon
Network's own real skimmer spots for that exact time/frequency window as
ground truth (`crates/manta-testkit/audio-corpus/ground-truth/`,
`scripts/score-against-rbn.py`). Baseline: 29.9% precision, 4.1% recall
(corrected post-hoc by a Codex review on PR #144 that caught two real
scorer bugs -- unbounded-time matching and unreachable-callsign-shape
truth rows counted as misses; originally reported as 31.0%/4.2%. Every
percentage figure elsewhere in this doc predates that fix and uses the
old, slightly-inflated methodology -- directionally correct, not exact).

Diagnosis (via `TrackManager::close_counts()`, `crates/manta-engine/
examples/close_counts.rs`) found `unconfirmed` (Candidate never sustains
`confirm_hops`) at 68.8% of all track churn (286,196 closes) and
`evicted`+`merged` (`track_cap=500` pinned at its ceiling for the entire
recording) at 29.7% (123,479 closes). This doc covers both.

## track_cap: raised 500 -> 1200

500 (ARCHITECTURE §4, not a SPEC value) was never stress-tested against
real contest-band signal density. Removing the cap entirely on the B2
recording measured real organic peak demand at 886 concurrent active
tracks. 1200 keeps meaningful headroom above that without picking an
arbitrarily large number. Verified independent of `confirm_hops`: V10
(`crates/manta-cli/tests/golden_v7_v9_v10.rs`) passes with `track_cap:
1200` regardless of `confirm_hops`'s value. No relationship claimed to
manta's Pi4 CPU-budget gate (MAN-18/MAN-49) -- explicitly out of scope
here; the user waived that constraint for this investigation.

Effect on the B2 benchmark (combined with the repetition-gate fix below
and this `track_cap` change, `confirm_hops` still at 19): recall 4.2% ->
4.7%, precision 31.0% -> ~30% (statistically flat -- see "why this barely
moved recall" below).

## confirm_hops: investigated, a real problem found, the obvious fix rejected

**The problem is real.** `confirm_hops=19` (SPEC §2.3/§2.4's literal
value) requires 50.7ms of *unbroken* rise from a CANDIDATE's very first
hop to promote. A CW dit alone is shorter than that at any real contest
speed above ~20 WPM (30ms at 40 WPM, 34ms at 35 WPM, 40ms at 30 WPM). So
on real contest-speed CW, any word/character starting with a dit-leading
letter (E, I, S, H, 5, ...) structurally cannot promote on its own opening
element -- it spawns a fresh CANDIDATE every dit-onset that immediately
dies `Unconfirmed`, repeatedly, until a dah-leading element promotes it or
the transmission ends. This is the majority contributor to the 68.8%
`unconfirmed` figure above. It was never tested in this direction before:
the existing `on_snr_db` decision doc (`docs/DECISIONS/
2026-07-18-*`) only tried *raising* `confirm_hops` (40..150, rejected for
worse false-track counts and worse decode accuracy against synthetic
fixtures) -- lowering it was untested.

**The obvious fix (lower `confirm_hops`) breaks a real golden vector.**
V10 (15 WPM overall, Farnsworth `char_wpm: 25.0`) starts failing hard the
moment `confirm_hops` drops below 18:

| confirm_hops | V10 result | CER |
|---|---|---|
| 19 (current) | pass | -- |
| 18 | pass | -- |
| 17 | **FAIL** | 0.2368 |
| 14 | **FAIL** | 0.2368 (identical) |
| 8 | **FAIL** | 0.2368 (identical) |

This is a hard cliff, not a gradual tradeoff -- 17, 14, and 8 all produce
the *exact same* corrupted decode (letter-by-letter word-boundary
insertion on the opening transmission: `"CQ CQ DE G4XXX..."` decodes as
`" C Q D E G 4 X X X..."` before self-correcting partway through). This
points at `manta_decode::timing::FARNS_MIN_COUNT`'s Farnsworth
gap-classifier bootstrap (`crates/manta-decode/src/timing.rs`) being
sensitive to *which hop* a track promotes on -- plausibly because
promoting earlier/later shifts which of the signal's early inter-element
gaps the classifier's 5-sample bootstrap window happens to observe, and at
some point that shift crosses into observing a different (worse) sample
set. Not root-caused further; this needs tracing inside
`manta-decode`'s gap-classification code, not `manta-engine`'s detector.

**Also: 18 vs 19 isn't a useful fix even though it's safe.** 18 hops =
48.0ms, barely under the original 50.7ms -- nowhere near enough headroom
to help a 30-40 WPM contest dit (30-40ms). The only value that doesn't
break V10 provides no meaningful benefit against the actual problem.

**Decision: `confirm_hops` stays at 19.** The dit-timing mismatch is a
confirmed real bug, but fixing it safely requires decoupling the
Farnsworth gap-classifier's bootstrap from detector promotion timing
inside `manta-decode` -- a real DSP/decode-core change, not a one-line
constant tweak, and out of scope for this pass. Folded into the wider
real-world decode-accuracy investigation MAN-166 opened (most of manta's
recall/precision gap against real signal is now believed to live in
decode-core accuracy under genuine HF conditions, not detector/tracker
bookkeeping -- see MAN-166's ticket history).

## Why the track_cap fix barely moved recall

Recall only moved 4.2% -> 4.7% despite `unconfirmed` and `evicted` both
dropping sharply in an uncapped experiment run (`track_cap` at 100,000,
`confirm_hops` at 8, purely to measure organic demand -- not shipped).
Spots stayed ~79% `Beacon`-type (the repetition-gate-exempt spot type)
even after the repetition-gate identity fix landed, meaning most real
signals in this recording still only ever produce *one* clean decode
total, with or without cross-fragment repetition credit. That, plus
`merged` staying flat in absolute terms (52,330 -> 56,739, now the single
largest remaining closure category once `unconfirmed`/`evicted` shrank),
points at two further open problems, neither fixed here:

1. **`merge_converged`'s "within 1.0 channel" merge threshold** (SPEC
   §2.5, `crates/manta-engine/src/track.rs`) assumes one real signal never
   sits within 1 channel (~47Hz) of another. In a dense real contest
   pileup that assumption is frequently false -- two genuinely distinct
   real signals packed within ~50-100Hz apart is a normal, common
   real-conditions pattern. SPEC §2.5 explicitly frames any such
   convergence as "interference or drift-collision" of what should be
   treated as the same signal; that's a spectral-resolution/signal-density
   modeling assumption, not an implementation bug, so changing it needs
   the same rigor as an `on_snr_db`-class deviation (or a channelizer/
   ownership-arbitration redesign), not a quick threshold tweak.
2. **Raw per-character decode accuracy under genuine HF noise/QRM** may be
   the dominant remaining gap: most surviving real spots reflect only one
   clean decode ever captured, suggesting the decode-core itself struggles
   with real receiver noise floor, real multipath/QSB, and real
   adjacent-channel QRM more than the synthetic AWGN+Watterson golden
   vectors predict.

Both are being handed to a dedicated deep-dive brainstorming pass (see
MAN-166) rather than rushed as parameter tweaks here.
