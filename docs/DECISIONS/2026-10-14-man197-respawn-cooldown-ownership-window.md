# MAN-197: the respawn cooldown gates a spawn candidate's whole ownership window

## Decision

`TrackManager::step_hop` rejects a spawn candidate at channel `k` while any
channel of its prospective ownership window `{k−1, k, k+1}` (wrapping, the
same window `Track::owned` returns) is still serving a post-`Silent`
respawn cooldown (`DetectorConfig::silent_respawn_cooldown_hops`, set by
`detector.silent_respawn_cooldown_ms`). Before this change the gate asked
only about `k`. The new private helper `spawn_window_cooling_down(k)`
composes the existing single-channel `channel_cooling_down` over that
window. It replaces the single-channel check at both spawn-gate sites: the
scan index `k` and the same-hop pairing channel `k+1`, whose window
includes `k+2`.

Arming on a `Silent` close, the round-4 per-channel clear on a dropped
`rise`, `Track::owned`, `select_channel`, `update_centroid` and the
lifecycle state machine are unchanged. No configuration key, metric, wire
field or CLI output changes. SPEC §2.5 states the rule.

## Context

PR #174 (MAN-171) added the cooldown so a stationary birdie whose `rise`
never drops cannot spawn, promote, run `gc_hops` with no decoded
character, close `Silent` and respawn immediately, forever. See
`2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md`. Codex's
round-5 review of that PR raised this bypass as a P2. It was filed as
MAN-197 under `2026-08-07-pr-review-convergence-policy.md` rather than
fixed inline.

## Mechanism

Arming covers all three channels the closing track owns. The round-4 clear
releases each channel independently once its own `rise` is false. The
spawn gate checked only the birth channel. After a birdie's track on
channel 10 closed `Silent`, channels 9 and 11 cleared on the next hop while
channel 10 stayed armed. A candidate that was born on 9 owned `{8, 9, 10}`
straight away. Its max-power `select_channel` then read channel 10.
MAN-172's local-peak rule narrows this case but does not close it: the
neighbour has to reach the birdie's smoothed power, and a +30 dB signal
next to a +20 dB birdie does.

## Measured severity

`neighbour_spawn_cannot_absorb_a_cooling_down_channel` in
`crates/manta-engine/src/track.rs` reproduces the case. Against
`7b9b36da35b1ca0893a130cf8278c792124bda04` it fails with a spawn on
channel 9 at hop 15050 while channel 10's cooldown was armed until hop
26282.

MAN-197's research ran the absorbed track on. It did not track the birdie
forever, as the ticket says. Its centre converged onto channel 10 within
2,000 hops. The ordinary `gc_hops` timer then closed it `Silent`, which
re-armed the same window. So the bypass costs one extra promote/decode
cycle inside the cooldown window. The configured interval is broken, but
the rate limit still applies after that cycle. Whether sustained adjacent
traffic can chain several such cycles was not measured.

## Alternatives rejected

- **Clear a close's armed channels together**, only once every one of them
  has a dropped `rise`. The spawn gate gives the same result, but this
  needs per-close grouping state. Overlapping windows from different
  closes become ill-defined. It also changes the round-4 semantics that
  `respawn_cooldown_clears_once_the_channel_actually_goes_quiet` pins.
- **Close or deselect a live track whose `select_channel` lands on a
  cooling channel.** This spreads the rule into the per-track loop and
  lifecycle code, and gives no protection the spawn gate does not.

## Accepted trade-off

A new station exactly one channel (93.75 Hz) from a cooling birdie channel
cannot spawn until the cooldown expires, at most
`silent_respawn_cooldown_ms` (30 s by default). That is the rate limit the
birdie's own channel already has. It ends early when the birdie's own
`rise` drops, because the round-4 clear then releases the channel.
`two_signals_survive_a_busy_band` in `man171_repro.rs`, with signals about
180 Hz apart, still passes.

## Evidence

- `neighbour_spawn_cannot_absorb_a_cooling_down_channel`: no track exists
  on any hop before the cooldown expires, and the channel-9 signal
  promotes once it does (a rate limit, not a denylist).
- `silent_close_starts_a_respawn_cooldown_on_that_channel` and
  `respawn_cooldown_clears_once_the_channel_actually_goes_quiet` pass
  unchanged.
- `two_signals_near_dc_both_promote` in
  `crates/manta-engine/tests/man171_repro.rs` now asserts a promotion
  within 200 Hz of each target, like its three siblings, instead of a
  non-empty list. This was MAN-197's second finding. Promotions measured
  at 14,031,126.4 Hz and 14,037,781.3 Hz, 23.6 Hz and 18.7 Hz from the
  targets.
