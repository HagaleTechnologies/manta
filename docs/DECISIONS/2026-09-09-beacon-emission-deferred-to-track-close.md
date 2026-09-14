# Non-allowlisted Beacon spots: emission deferred from first-decode to track-close

## Context

A live overnight RSP1B/40m monitoring session produced hours of noise
decoded as "confirmed" spots at ~100% false-positive rate. Root cause:
`SpotType::Beacon` is exempt from the repetition gate (ARCHITECTURE §6.4,
SPEC-decode-core.md §4) so real NCDXF/IARU beacons -- which ID once per
cycle -- can still spot on a single decode. `context.rs`'s
`POWER_STEP_BEACON_RE` (MAN-37), a deliberately coarse pattern matching any
plausible-looking word followed by a lone "T", tags noise-decoded garbage
as `Beacon` just as readily as a real beacon, and that garbage then bypassed
the repetition gate entirely.

PR #154 (`fix/validator-noise-false-positives`) fixed this over eight
review rounds, converging through several designs before landing on the one
this document records:

1. **Round 1**: reject a Beacon candidate outright if its track's WPM
   exceeds 45 at evaluation time. Reverted -- rejected legitimate fast
   (45+ WPM) contest/computer-keyed CW reaching a spot through the ordinary
   two-repetition-confirmed path (which the WPM check never should have
   touched at all), and a `grammar.rs` companion check rejected real,
   allocated `master.scp` callsigns that legitimately repeat a character
   (`AA6AA`, `BG8GGG`, ...).
2. **Round 2**: scope the WPM check to `SpotType::Beacon` only. Found a new
   bug: rejecting after marking a word `attempted` permanently lost a real
   beacon whose early speed estimate was transiently inflated past the
   ceiling, since nothing ever retried it.
3. **Rounds 3-4**: fixed the retry mechanism's interaction with operator
   suppression counting and the decoder's own WPM-reporting deadband.
4. **Round 5**: found the retry-on-every-live-`SpeedUpdate` design was
   fundamentally asymmetric -- a noise track's speed oscillating across the
   cutoff (46 -> 44 -> 47 WPM) could spot on the transient dip, and that
   spot could never be retracted once the estimate rose again. Redesigned
   to decide once, at the track's true close (`TrackClosed`), using
   `manta-engine`'s guarantee that a final, unthrottled speed reading is
   flushed before that event fires on every closure path.
5. **Rounds 6-7**: found the round-5 redesign was still incomplete --
   `TrackClosed`'s retry-bypass guard could be triggered by an unrelated
   `WordBoundary`, not just the intended retry call; merge/eviction
   closures could fabricate a synthetic character (turning an in-progress,
   unconfirmed mark into a decoded "T", forming exactly the vulnerable
   `<call> T` pattern) when forcing finalization; and a track closing via
   `Silent` (no character decoded for 30s) doesn't actually mean the RF
   signal is gone, unlike `HangExpired`.
6. **Round 8** (this decision): confirmed the round 5-7 redesign was sound
   in direction but still had gaps -- `Silent` closures still forced
   finalization; `pending_beacons` (below) could grow unboundedly on a
   long-lived active track; a deferred candidate resolving after a newer
   spot for the same identity could roll `Dedupe`'s watermark backward; a
   confirmed-but-buffered demodulator run (`Demod::held`) was never
   surfaced to the speed-only finalization path; and this whole redesign
   silently diverged from `docs/SPEC-decode-core.md`'s own normative V18/
   V30 acceptance criteria ("emits on the first decode") without amending
   the spec -- which this document, and the amendment in §4 of that spec,
   now correct.

## Decision

A non-allowlisted `SpotType::Beacon` candidate is never evaluated against
WPM or emitted opportunistically. `Validator::evaluate_candidate` dispatches
it instead to `capture_pending_beacon`, which runs every permanent,
timing-independent check (blocklist/notch/grammar/cty) and, if all pass,
captures `(candidate, sample_ts, freq_hz, snr_db, char_confidences)` into a
new `PendingBeacon` record on the track -- deliberately independent of
`TrackState::words`'s bounded, mutable 16-word window, so a long
transmission can't evict its own identifying word before close, and a later
unrelated word can't corrupt its captured timestamp.

`TrackClosed`'s handler calls a new `resolve_pending_beacons`, the ONE point
where WPM is ever checked for these candidates, using the track's true final
speed. `SpeedUpdate` is back to a pure no-op recording of the value; there
is nothing left to retry, since nothing is ever evaluated before the close
in the first place. This eliminates the oscillation asymmetry structurally:
no interim reading, in either direction, can ever matter.

Supporting changes, each closing a specific round-8 (or earlier) gap:

- **`manta-decode`**: `TrackDecoder::finish()` (forces an in-progress mark
  to resolve -- legitimate only where a genuine, sustained on-air gap has
  actually been observed) is now paired with `finish_speed_only()` (reports
  the confirmed speed estimate only, forcing nothing). `finish_speed_only`
  also surfaces `Demod::held` (a run that already passed its own debounce
  confirmation, just delayed one further polarity flip before reaching live
  processing) via a new `Demod::take_held`, safe to treat as final exactly
  because nothing more will ever arrive for a closing track.
- **`manta-engine`**: routes each closure reason to the matching variant.
  `HangExpired` (a sustained observed power drop) uses the forcing
  `finish()`. `Silent` (no character decoded for `gc_hops`, which can fire
  on a still-ACTIVE track with a real continuous signal) and Merged/Evicted
  (pure bookkeeping) both use `finish_speed_only()`.
- **`manta-spot`**: `PendingBeacon`s per track are capped at
  `MAX_PENDING_BEACONS` (8), oldest dropped first, so a track that never
  closes can't accumulate state forever. `resolve_pending_beacons` peeks
  `Dedupe`'s already-recorded timestamp for the same `(callsign, freq)`
  identity before calling `should_emit`; a deferred candidate whose own
  `sample_ts` is not strictly newer is discarded rather than risking a
  rewind of `Dedupe`'s watermark.

## Spec status

`docs/SPEC-decode-core.md` §4 is amended in place (not merely reworded in
this doc) to describe beacon emission timing as "captured on completion,
judged at track close" rather than "spots on the first decode." The
repetition-gate exemption itself -- a beacon never needs a second, distinct
decode -- is unchanged; only the moment of emission moved. V18/V30's pass
criteria are updated to require a `TrackClosed` event before asserting a
spot. Allowlisted Beacon-shaped candidates are entirely unaffected by any of
this and still spot the instant their pattern completes, exactly as before.
