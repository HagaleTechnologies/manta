# `manta doctor`: a real `TrackPromoted` event replaces decode-timing proxies

## Context

`manta doctor`'s `Verdict::NoSignal` check (docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md
gave the original PR its motivation) went through four straight rounds of
Codex review each finding a new bug in the same logic:

1. Verdict keyed off the median SNR, not the max -- hid a real carrier
   alongside several noise-floor tracks.
2. `NoSignal` ignored decoder evidence that arrives without a `TrackMeta`
   (a short track can decode a character, or even confirm a spot, before
   its first periodic `TrackMeta` lands).
3. `tracks_closed` (proof *some* event happened) wasn't checked either --
   missed the case of a track closing having emitted only
   `WordBoundary`/`SpeedUpdate`.
4. Fixing #2/#3 exposed a THIRD proxy gap: `snr_db_max == None` (real
   decoder activity, but no SNR ever measured) was silently folding into
   `NoisyNoDecode`, which explicitly claims "every track measured at or
   below the noise floor" -- false when nothing was measured at all.
5. And still: even with `tracks_promoted`/`chars_decoded`/`tracks_closed`
   all covered, a signal rising right at the ~2s detector warmup boundary
   could reach `finish()` at `--duration 3` having only been *promoted* --
   `TrackMeta` needs ~1s more decoder init on top of the promotion
   latency, `CharDecoded` needs a full decode, and `TrackClosed` is
   filtered by `has_emitted` (MAN-19), which a promoted-then-silent track
   never sets.

Four consecutive rounds finding a new bug shape in the same code region is
this repo's own review-convergence policy's explicit "stop and reconsider"
trigger (docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md, "Two
check-in triggers") -- checked in with Tony rather than pushing a fifth
point patch. Decision: add a real engine-level event instead of continuing
to proxy "did the detector find anything" through decoder-timing side
effects.

## Decision

`manta_decode::events::DecoderEvent` gets a new variant:

```rust
TrackPromoted { track_id: u32, sample_ts: u64, freq_hz: f64 }
```

Emitted in `TrackManager::step_hop` (`crates/manta-engine/src/track.rs`) at
the exact hop `LifecycleEvent::Promoted` fires -- the moment the detector
itself decided a candidate looks like a real signal, independent of
whether the decoder subsequently produces anything at all.

`doctor()`'s `NoSignal` check now keys off `tracks_promoted == 0`
directly, replacing the entire `track_meta_count`/`chars_decoded`/
`tracks_closed` proxy cascade. `verdict()` collapses to:

```rust
if self.spots_confirmed > 0 { return Verdict::Decoding; }
if self.tracks_promoted == 0 { return Verdict::NoSignal; }
match self.snr_db_max {
    Some(max) if max > NOISE_FLOOR_SNR_DB => Verdict::WeakNoDecode,
    Some(_) => Verdict::NoisyNoDecode,
    None => Verdict::ActivityNoSnr,
}
```

## Why this actually closes the gap (not just moves it)

Promotion latency is bounded and fast: `DetectorConfig::default()`'s
`warmup_hops: 750` + `confirm_hops: 19`, at `HOP_MS = 8/3` ms/hop
(`FO_HZ = 375` Hz), gives a worst-case ~2.05s from stream start to
promotion (signal rising right at the warmup boundary). `MIN_DURATION`
(3s) already covers that with ~1s of margin -- no further bump needed.
Contrast `TrackMeta`, which additionally needs `TrackDecoder`'s own ~1s
demodulator init on top of promotion latency (~3.05s minimum) -- exactly
the gap round 5's finding identified.

## Consequences / what else had to change

- `TrackManager::step_hop`'s signature changed from `-> Vec<u32>` to
  `-> (Vec<u32>, Vec<DecoderEvent>)` (closed ids, promoted events).
  `process_hops` merges the promoted events in **after** the
  `has_emitted`-marking loop, not before -- a bare promotion (no other
  event before a same-batch merge/evict) must NOT retroactively earn that
  track a `TrackClosed` under MAN-19's existing exclusion. `TrackPromoted`
  is real, permanent signal for a *different* consumer; it isn't itself
  subject to that filter.
- `manta_spot::Validator::ingest` needed a new match arm (Rust's
  exhaustiveness check is the safety net here) -- a deliberate no-op that
  never touches `self.tracks`, so a promoted-then-merged-away track (the
  exact case this event exists to surface) creates no per-track_id state
  to leak.
- `event_sample_ts`/`event_track_id` (`track.rs`) and the two ad hoc
  `DecoderEvent` match helpers in `crates/manta-engine/tests/cpu_budget.rs`
  needed a new arm each; nothing else in the workspace matches
  `DecoderEvent` exhaustively (manta-cli, manta-decode's
  `events_to_text`, and `manta-engine::calibrate_track_meta` all already
  wildcard unknown variants).
- `tracks_closed`'s own doc comment already said "not every detector
  candidate opened during the run" -- that's now literally
  `tracks_promoted`'s job; `tracks_closed`/`chars_decoded`/
  `track_meta_count` remain in `DoctorReport` as informational stats, just
  no longer part of the verdict decision.
