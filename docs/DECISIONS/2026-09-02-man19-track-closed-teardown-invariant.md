# MAN-19: `DecoderEvent::TrackClosed` teardown invariant

## The invariant (normative)

Every track that is promoted (CANDIDATE → ACTIVE, SPEC §2.4) and emits at
least one real `DecoderEvent` (`CharDecoded`/`WordBoundary`/`SpeedUpdate`/
`TrackMeta`) **must** eventually produce exactly one
`DecoderEvent::TrackClosed { track_id }`, regardless of why or when it
closes — any `CloseReason` (`Unconfirmed`/`HangExpired`/`Silent`/`Merged`/
`Evicted`, `manta-engine::track`) via `TrackManager::process_hops`, or
still-open at end-of-stream via `TrackManager::finish()`.

Any consumer that keeps per-`track_id` state — today, `manta-spot`'s
`Validator::tracks` and `RepetitionGate::seen` — **must** free that state
on `TrackClosed`. `TrackManager::next_id` never reuses a `track_id`
(`manta-engine::track::TrackManager::spawn`), so a per-`track_id` map with
no eviction signal grows without bound for the life of the process under
any workload with sustained track churn.

A track that closes *without* ever emitting a real event (a CANDIDATE that
never gets promoted, e.g. `CloseReason::Unconfirmed`) is explicitly
exempt — it never appeared in the event stream to begin with, and adding a
`TrackClosed` for it would introduce a track_id that callers like
`decode_samples` (which picks the *lowest* track_id present as its
single-track report, `manta-engine::lib`) never used to see, silently
changing which track gets reported.

## Why this exists

Discovered as a real, measured bug via MAN-19's 24h soak harness
(`crates/manta-soak-harness`): before `DecoderEvent::TrackClosed` existed,
`Validator.tracks` and `RepetitionGate.seen` had no signal that a
`track_id` would never be seen again, so both grew one entry per
historical `track_id` forever. RSS grew from ~95 MiB to 261.7 MiB and was
still climbing after 1h of soaking a synthetic, deliberately track-churn-
heavy 40m CW pileup scene — confirmed both empirically and in source
(zero `.remove(` calls in either file, pre-fix).

## Implementation

- `manta-decode::events::DecoderEvent::TrackClosed { track_id: u32 }` —
  the signal itself.
- `manta-engine::track::TrackManager`: `Track::has_emitted` (set the first
  time `drain_pool` actually yields an event for that `track_id` — NOT
  merely whether it was ever promoted; a track promoted and closed within
  the *same* `process_hops` batch never gets a `drain_pool` pass before
  removal and so can have an allocated decoder yet have produced nothing).
  `step_hop`/`merge_converged`/`evict_over_cap` report only tracks where
  `has_emitted` is true; `process_hops` turns those into `TrackClosed`
  events. `finish()` does the same for whatever is still open when the
  stream ends, then clears `self.tracks`.
- `manta-spot::Validator::ingest`'s `TrackClosed` arm: `self.tracks.remove(track_id)`
  + `self.gate.forget_track(*track_id)`.
- `manta-spot::gate::RepetitionGate::forget_track`: removes every
  `(track_id, *)` key via a `BTreeMap::range` scan (O(log n + k), not a
  full-map `retain` per close — matters at the churn volume that exposed
  this bug in the first place).

## Future producers/consumers must preserve this

Any future code that emits `DecoderEvent` (a new input path, a decoder
variant) or that keeps per-`track_id` state (a future M3 metrics endpoint,
a new validator stage) inherits this contract: emit `TrackClosed` for
every track you promote, and free your own per-`track_id` state when you
see one. `wiki/pages/detector-tracks.md` and `wiki/pages/spot-validation.md`
point here; this document is the source, per AGENTS.md's Knowledge Wiki
convention (the wiki is descriptive and always loses conflicts with code
and docs/).
