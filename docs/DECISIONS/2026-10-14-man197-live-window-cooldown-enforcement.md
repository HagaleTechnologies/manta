# MAN-197: a live track's input selection also skips cooling-down channels

## Decision

`TrackManager::step_hop`'s live-track loop selects each track's input
channel through the new `Track::select_live_channel`, which is identical
to `select_channel` except that it skips channels still serving a
post-`Silent` respawn cooldown, falling back to the plain max-power
channel when every owned channel is cooling. This is the live-track
counterpart of the spawn gate's `spawn_window_cooling_down` and closes
the last known way a silenced birdie can be decoded inside its cooldown
window.

This supersedes one claim in
`2026-10-14-man197-respawn-cooldown-ownership-window.md` (same ticket,
earlier today): that record rejected "close or deselect a live track
whose `select_channel` lands on a cooling channel" because it "gives no
protection the spawn gate does not". The review of PR #301 showed a path
the spawn gate alone cannot cover; deselecting (this record) is the fix,
closing was still rejected.

## Context

Codex's round-1 review of PR #301 (the MAN-197 branch) re-checked the
respawn cooldown as the ownership *window moves*, not only at birth. A
candidate born two bins from the cooling channel — channel 8 next to a
birdie on channel 10 — has a prospective window `{7, 8, 9}` that
legitimately does not intersect the cooldown, so the spawn gate (rightly)
lets it through. A stronger signal on the intervening bin 9 is absorbed
(the track already owns channel 9) and drags the centroid EMA across the
rounding boundary via `update_centroid`: `round(c)` becomes 9, the owned
window expands to `{8, 9, 10}`, and once the real signals end, max-power
selection reads the birdie's continuous +20 dB — feeding the decoder from
the cooling channel for a whole extra GC cycle, exactly the absorption
MAN-197's spawn gate was meant to stop. The interval the cooldown
configures is broken again, one illegitimate cycle at a time.

## Measured reproduction

`drifting_track_cannot_take_input_from_a_cooling_down_channel` in
`crates/manta-engine/src/track.rs` drives the full sequence: birdie
promote → `Silent` close → cooldown armed on 10; a +30 dB signal on 8
spawns and promotes (legitimately); a +40 dB signal on 9 drags the window
onto `{8, 9, 10}` (asserted); both real signals end, birdie alone
remains. Pre-fix the track was still alive 2,500 hops later — `drop` on
the cooling channel never fires, so nothing hang-closes it — while a hang
close was due within `hang_hops` (1,875). Post-fix it closes and channel
10's cooldown is not re-armed out of the illegitimate cycle.

## Alternatives rejected

- **Spawn-gate-only** (the earlier record's position): leaves the
  drift path open, as measured above.
- **Close a track whose selection lands on a cooling channel:** a close
  with a new reason touches the lifecycle FSM and close accounting; a
  `Silent` close would re-arm the cooldown from *now*, changing the
  rate-limit interval semantics. Deselecting needs neither: the track
  simply reads its next-best owned channel, which after the real signals
  end is noise, so the ordinary `HangExpired`/`Silent` path closes it.
- **Freeze `center` near a cooling channel:** the centroid must keep
  following genuine drift (SPEC §7 test 9); freezing it would break
  drift tracking for every track that merely passes a cooling neighbour.

## Accepted trade-off

A live track whose only strong owned channel is cooling reads noise
instead, until the birdie's own `rise` drops (round-4 clear) or the
cooldown expires. That window is at most `silent_respawn_cooldown_ms`
(30 s by default), the same bound the spawn gate already imposes on new
births, and it only affects a track that already absorbed or drifted
onto a channel that just proved 30 s of undecodable carrier. Selection
is byte-identical to `select_channel` whenever the max-power owned
channel is not cooling — the common case — so no tested decode scenario
changes.

## Evidence

- `drifting_track_cannot_take_input_from_a_cooling_down_channel`:
  red pre-fix (track alive at +2,500 hops past the signals' end, hang
  close overdue), green post-fix, and the cooldown's `until` is
  unchanged by the close.
- `neighbour_spawn_cannot_absorb_a_cooling_down_channel`,
  `silent_close_starts_a_respawn_cooldown_on_that_channel` and
  `respawn_cooldown_clears_once_the_channel_actually_goes_quiet` pass
  unchanged.
- Full `cargo test -p manta-engine --lib`: 223 passed, 0 failed.
