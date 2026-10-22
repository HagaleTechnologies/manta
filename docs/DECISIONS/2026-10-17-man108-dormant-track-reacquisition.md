# MAN-108: a fading signal keeps its track through the fade (DORMANT re-acquisition)

Date: 2026-10-17. Ticket: MAN-108, found by the 2026-09-05 broad review
(lens 3, hit-list #8).

## Problem

A track whose signal stays under the rise threshold for `hang_hops` (1875
hops, 5 s) closed `HangExpired`. When a Watterson fade ended, the signal
spawned a brand-new `track_id` and paid the whole restart again: candidate
confirmation, MAN-313 qualification, `Demod` initialisation and the
validator's first-`TrackMeta` wait. The validator also dropped the old
track's words, variant ledger and MAN-272 RST/`QRL?` state at its
`TrackClosed` (`crates/manta-spot/src/validator.rs`, whose own comment
called this a "known, unmeasured risk").

The ticket quoted a +20 dB Watterson-good signal splitting into 4-5 tracks
over 180 s. That figure predates MAN-115…MAN-314: on `35deb42` a +20 dB
signal no longer splits, but a weaker one still does. 240 s scenes,
`CQ CQ DE W1AW W1AW K` at 20 WPM, offset +9.1 kHz, six fade seeds per cell,
distinct promoted `track_id`s within 300 Hz:

| preset \ SNR | 0 dB | +3 dB | +6 dB | +12 dB | +20 dB |
|---|---|---|---|---|---|
| Watterson-Good | 6/6 split (3-4 tracks) | 3/6 split (2) | 1/6 split (2) | 0/6 | 0/6 |
| Watterson-Poor | 0/6 | 0/6 | 0/6 | 0/6 | 0/6 |
| AWGN (0/6/20 dB) | 1 track | — | 1 track | — | 1 track |

At 120 s the Good 0/+3/+6 dB cells split 6 of 18 scenes (26 tracks for 18
signals). In one 0 dB scene the spots carried `track_id` 21 and 24 for the
same W1AW.

## Decision

A **DORMANT** lifecycle state (SPEC §2.4), configured by a new
`[detector].reacquire_ms` (default 10000 ms = 3750 hops):

- A track that has decoded at least one character while qualified, and
  whose hang timer runs out, goes DORMANT instead of closing. Its decoder
  is flushed exactly as a `HangExpired` close flushes it (refiner backlog
  drained, then the forcing `finish_decoder`) and released. The track
  keeps its `track_id`, its centroid (frozen) and its three-channel
  ownership.
- A rise held for `confirm_hops` (19) consecutive hops on its owned
  channels within `reacquire_hops` wakes it: ACTIVE under the same
  `track_id`, with a fresh `TrackDecoder`, no MAN-313 requalification and
  no new `TrackPromoted`.
- Otherwise it closes `HangExpired` → `ClosureKind::SignalEnded` at
  `hang_hops + reacquire_hops` after the drop.

The code is `Lifecycle::on_hop`'s Hang and Dormant arms,
`Track::go_dormant`/`Track::reacquire` and their call in
`TrackManager::step_hop`, and the `set_reacquirable` latch in
`TrackManager::process_hops` (all `crates/manta-engine/src/track.rs`).

### D1. DORMANT with a fresh decoder, not a longer hang

The ticket's second direction was "extend HANG". Measured on `35deb42`
with `--config [detector] hang_ms = H` over the 18 Good 0/+3/+6 dB 240 s
scenes:

| variant | tracks | split scenes | spots | mean CER (no spaces) |
|---|---|---|---|---|
| baseline (5 s) | 36 | 10 | 32 | 0.298 |
| `hang_ms = 10000` | 19 | 1 | — | — |
| `hang_ms = 15000/20000/30000` | 18 | 0 | 30 | 0.372 |

A longer hang keeps one track but decodes worse. Worst case
`good_s6_x5663`: CER 0.092 → 0.547, first spot 33.7 s → 114.1 s. The
decoder that lost lock in the fade kept running and printed
`T TTT TTT TTTTT …` for about 40 s (every element read as a dah, so the
speed tracker's drift re-init, which needs 12 same-cluster marks, never
trips). Today's respawn had a fresh decoder that decoded
`W1AW W1AW K CQ …` cleanly. DORMANT keeps the identity and gives the
decoder the fresh start: on the same 18 scenes it measured 18 tracks, 0
split, 35 spots and mean CER 0.298, every scene's CER equal to baseline.
First spot came earlier in 3 scenes and later in none (`good_s0_x0bad`
123.6 → 101.3 s, `good_s3_x5663` and `good_s6_x5663` 33.7 → 24.3 s).

### D2. Waking takes the same 19-hop confirmation a new candidate takes

A prototype that woke on a single rise hop also gave 18 tracks and 0
splits, but mean CER rose to 0.317 (34 spots) and one scene lost its word
boundaries (CER 0.105 → 0.297). The decoder is sensitive to the hop it
starts on (the MAN-166 note in `DetectorConfig`'s `Default` doc), so the
wake rule matches a fresh candidate's 19-hop confirmation; with it every
scene's text equals today's fragmented decode, under one `track_id`.
Hops without a rise reset the wake count, as they end a candidate.
Review round 1 (PR #344) added the candidate's crash rule too: on a hop
where MAN-312 suppresses a static crash, the gate's EMA is held, so the
`rise` such a hop reports is not evidence the signal returned. The wake
confirmation freezes there (neither advancing nor resetting) while the
`reacquire_ms` window itself keeps aging, exactly as a CANDIDATE's confirm
window freezes while a HANG track's hang timer runs
(`suppressed_crash_cannot_finish_dormant_wake_confirmation` and
`suppressed_crash_still_ages_the_dormant_expiry`). Without the freeze, a
crash that peaked on the dormant track's channel could complete the wake
after the signal had already disappeared and hand the fresh decoder the
crash itself (`go_dormant` clears `clean_hist`, so no pre-crash samples
were left to substitute).

### D3. Default on, at 10 s

| window | tracks | split scenes | spots | mean CER |
|---|---|---|---|---|
| 5 s | 20 | 2 | 35 | 0.298 |
| **10 s** | **18** | **0** | **35** | **0.298** |
| 15 s | 18 | 0 | 35 | 0.298 |

5 s left 2/18 (240 s) and 1/18 (120 s) scenes split; 10 s and 15 s left
none, so 10 s is the shortest window that removed every measured split.
The 45 other 240 s scenes (Good +12/+20 dB, all Poor, AWGN) had identical
summary metrics with and without DORMANT.

### D4. Only a track that has decoded is kept

The `reacquirable` latch is set when a qualified track's `CharDecoded`
arrives in `process_hops`. It is batch-granular, the same precedent as the
GC reset and MAN-313's `last_char_hop` (SPEC §2.8 G); the dormancy
decision comes at least `hang_hops` (5 s) after the drop, far longer than
any batch. A track that never decoded a character (an unmodulated carrier,
a garbled signal, a promoted-but-unqualified MAN-313 track) closes at the
hang exactly as before: keeping it would only hold a slot and its
channels for nothing.

While DORMANT the track runs no decoder (so `decoding_track_count` and the
daemon's `manta_active_tracks` gauge do not count it, and
`decoding_track_freqs_hz` omits it from the MAN-324 live state), the
garbage-collect timer holds, and the centroid does not update: noise or a
MAN-197-style neighbour would otherwise drag it off the frequency the
signal will come back on. The wake counts as a re-rise for MAN-313's
`Unkeyed` close, so a woken track without a callsign read is not closed
on its first hop. `live.rs`'s `LiveTap` reads only `contains_track` (a
DORMANT track is still open, so its live-tap state is kept) and
`decoding_track_freqs_hz`; nothing else outside `track.rs` reads track
lifecycle state.

### D5. No validator or event-schema change

No new `DecoderEvent` variant or field, and no change to `manta-spot`
validation, gating, dedupe or annotations; only the validator comment
that flagged the risk is updated. A woken track keeps its words, variant
ledger, RST and `QRL?` flag because its `track_id` never closed, the same
as a track that rides through today's 5 s hang. Its
`TrackMeta`/`SpeedUpdate`/`TimingUpdate` sequence restarts from the fresh
decoder (WPM re-estimated, `TrackMeta` resumes after about 1 s of
`Demod` init). Accepted trade-offs:

- A different station keying up inside a DORMANT track's owned channels
  (±1 channel, about ±140 Hz at 96 kHz) within the window wakes the old
  track (fresh decoder, old `track_id`) instead of starting a new one. The
  same already happens inside today's 5 s hang. Pinned by
  `second_station_inside_the_window_wakes_the_dormant_track`.
- A track whose signal really ends closes 15 s after the drop instead of
  5 s, so a deferred Beacon candidate, judged at `SignalEnded`, is judged
  up to 10 s later.
- A DORMANT track holds one `tracks` slot, which counts toward
  `track_cap`; its SNR is noise-level, so at cap it is evicted first.

Event ordering: the DORMANT-entry flush is placed ahead of `drain_pool`'s
output in the batch before the stable `(sample_ts, …)` sort, so the old
decoder's synthetic-timestamp events (`SpeedUpdate` sorts at 0) cannot
sort after a same-batch woken decoder's. At `decode`'s 4096-sample
batches (16 hops, under the 19-hop wake) the two cannot share a batch;
larger batches can. Pinned by
`dormancy_and_wake_in_one_batch_keep_old_decoder_events_first`.

### D6. `reacquire_ms = 0` restores the old behaviour byte for byte

With `reacquire_hops == 0` the Hang arm never enters DORMANT, so nothing
else in the change runs. Measured during planning: `cmp` of the
`decode --json` reports was identical on all 21 fixtures checked (18 Good
0/+3/+6 dB and 3 AWGN). Pinned in-crate by `reacquire_zero_is_pre_man108`
and `reacquire_hops_zero_closes_at_hang_hops_even_when_reacquirable`.

## Golden vector V34

SPEC §7 V34 "fade-good-reacquire" (`manta_testkit::vectors::v34`,
`crates/manta-cli/tests/golden_v34_fade_reacquire.rs`): one 20 WPM W1AW at
+3 dB, Watterson-Good fade seed `0x0BAD`, 120 s, noise seed
`0x4D41_4E01_3B95`. V34 was the lowest free §7 number. The test needs one
promoted track within 300 Hz, W1AW spotted only from it and CER ≤ 0.20;
its control decodes the same WAV with `[detector] reacquire_ms = 0` and
needs ≥ 2 tracks, so the scene is shown to still exercise the mechanism.

## Branch measurements

Measured on the MAN-108 branch with `target/debug/manta decode --json`
(this repository's dev profile is optimised):

| Scene | `35deb42` | branch, `reacquire_ms = 0` | branch, default |
|---|---|---|---|
| V34: tracks promoted within 300 Hz | 3 | 3 (ids 28, 29, 32) | **1** (id 28) |
| V34: W1AW spot | 20.9 s | 20.9 s, track 28 | 20.9 s, track 28 |
| V34: CER (Levenshtein / keyed length) | — | 0.1546 (the three tracks' texts joined in id order) | 0.1546 |
| V8w: tracks promoted | 52 | 52 | 51 |
| V8w: validated / bogus | 27/50, 0 | 27/50, 0 | 27/50, 0 |
| V8w: strong (≥ +6 dB) signals with > 1 track within 300 Hz | 0/34 | 0/34 | 0/34 |
| VC3: tracks promoted (ceiling 60) | 53 | 53 | 51 |
| VC3: validated / bogus | — | 25/50, 0 | 25/50, 0 |

V34 run through `reacquire_ms = 0` is the Red step: it reproduces the
`35deb42` split (3 tracks), which is why the golden test keeps that run as
its control. Three back-to-back `decode --json` runs of the V34 WAV gave
the same SHA-256 (`5ceb2615…a2cc`), SPEC §6.

V8w is not a useful gate for this ticket: none of its strong signals
fragments on `35deb42` either, and the ignored V8w CER test's "3/34
fragmented" sentence was out of date (corrected here). Its one fewer
promotion, like VC3's two fewer, is a track that DORMANT kept instead of
respawning.

Existing test constants: none moved. The whole workspace suite passed
unchanged (no pinned value in `golden_v8_v8w.rs`, `man171_repro.rs`,
`non_cw.rs`, the `step_hop_*`/refiner-drain tests or the beacon-deferral
tests needed a new value). `MAX_VC3_PROMOTED = 60` holds with 51.

## Follow-ups

None required. The out-of-date "3/34 fragmented" sentence in the ignored
V8w CER test's doc comment is corrected in this change.
