# MAN-211: TrackMeta's frequency is captured per queued hop, like its SNR

## Decision

`TrackManager::step_hop` captures the track's centroid frequency into each
hop's decoder input entry, next to the per-entry SNR MAN-102 added. The
entry carries it through the refiner backlog (`DecoderInputEntry`) and
`pending`, and each of the three drain sites (`TrackManager::drain_pool`,
`Track::finish_decoder`, `Track::finish_decoder_speed_only`) calls
`TrackDecoder::set_freq_hz` with that entry's value immediately before its
`push_hop`. A `TrackMeta` therefore reports the centroid of the hop its
`sample_ts` names. SPEC §5 states the rule.

`step_hop` no longer writes the centroid to the decoder live.

## Context

`TrackMeta.sample_ts` is per entry: `push_hop` sets `last_ts` from its own
argument (`crates/manta-decode/src/decoder.rs`), and `tick_meta` reports
it. `TrackMeta.freq_hz` was whatever `step_hop` last wrote through
`set_freq_hz`, once per hop and before the hop was queued. Entries wait in
`pending` until the end of the `process_hops` batch, and up to
`GROUP_DELAY_HOPS` more hops in the refiner backlog, so by the time an
older entry crossed a reporting boundary the decoder held the centroid of
the batch's last hop. The drain sites never set the frequency. `manta-spot`
stores `TrackMeta.freq_hz` for the track (`validator.rs`) and every spot
carries it, so the mismatch reached users.

Found by Codex review on MAN-194's PR #183, where the same per-entry
capture was applied to `snr_db`.

## Measured

`track_meta_freq_hz_is_invariant_to_process_hops_chunk_size` in
`crates/manta-engine/src/track.rs` feeds a vector through `process_hops`
in 4096-sample chunks (`decode_samples`) and 2048-sample chunks
(`listen`/`soak_metrics`) and compares every `(track_id, sample_ts,
freq_hz)` triple. The `sample_ts` sequences were already identical; the
frequencies were not:

| Vector | Before: TrackMetas differing | Before: max difference | After |
|---|---|---|---|
| V1 | 60 of 119 | 8.101 Hz | 0 |
| V4 (Watterson fading) | 60 of 119 | 7.896 Hz | 0 |
| V9 (drift), `refine_bw_hz = 30` | 59 of 119 | 4.991 Hz | 0 |

This is the mechanism behind the audit's `decode` vs `listen` frequency gap
on V1 and V4. `every_pending_drain_sets_track_meta_freq_from_its_own_entry`
pins each drain site: before the fix each reported `freq_hz: 0.0` for a
pending queue drained on its own.

Reported frequencies move by up to the centroid's hop-to-hop jitter. V4's
first report is 14,009,093.5 Hz after the fix, against 14,009,083.2 Hz
(4096-sample chunks) and 14,009,088.3 Hz (2048) before. No golden vector
pins a `TrackMeta` frequency; the workspace suite is unchanged.

## Why the live writes were removed

The decoder's frequency field is read only inside `push_hop`, and every
`push_hop` in `manta-engine` is now preceded by a per-entry
`set_freq_hz`. A second, live writer could only hold a wrong value. It
also matches the SNR pattern: `set_snr_2500_db` has no live caller. If a
future drain site forgets the call, the report reads `0.0` or a stale
value, which tests catch, instead of a plausible wrong centroid.
`TrackDecoder::set_freq_hz` stays public; `manta-decode`'s own tests drive
it directly.

## Alternatives rejected

- **Keep the live writes alongside the per-entry ones:** harmless while
  every drain site sets the value, but it hides a missed site behind a
  plausible frequency.
- **Recompute the frequency at drain time from `Track::center`:** by then
  the centre has moved on, which is the same bug one layer down. MAN-194
  recorded the same mistake, and its fix, for `spectral_ref_power` and
  `snr_db` (see `Track::decoder_input`'s doc comment).
