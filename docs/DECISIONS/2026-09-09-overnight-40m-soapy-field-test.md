# Overnight 40m SoapySDR field test: fixed a real crash, still no confirmed off-air copy

Follow-up to `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md` (same
RSP1B, hwVer 6, serial 2402041760, `--soapy-gain 40`). That run left
Finding 3 open: no validated `Spot` ever emitted, negative SNR throughout,
not diagnosable from software alone. This run adds ~5.5 hours of
additional real-hardware data via `manta doctor` and `manta listen
--json`, 7000-7125 kHz US 40m CW segment (`--soapy-freq 7025000
--soapy-rate 192000`, 192 kHz passband ≈ 6930-7120 kHz).

## Finding 1 (fixed): a single SoapySDR stream overflow killed the whole session

`SoapySdrIqSource::read` (`crates/manta-input/src/soapy.rs`) retried
`ErrorCode::Timeout` internally but treated any other error, including
`ErrorCode::Overflow`, as immediately fatal -- tearing down the entire
source and losing all accumulated track/SNR stats. Two consecutive
30-minute `manta doctor` runs died this way, several minutes in, not at
startup: the RSP1B's ring buffer overflowed once under sustained 192 kS/s
USB streaming, and that alone was enough to kill an otherwise-healthy
session.

Fixed in #146 (merged to `main`): `Overflow` is now retried like
`Timeout`, but against its own bound (`MAX_OVERFLOW_RETRIES = 100`,
separate counter from `MAX_TIMEOUT_RETRIES`) rather than folded into the
silence budget -- `Overflow` returns immediately without waiting out
`TIMEOUT_US`, unlike `Timeout`, so retrying it unboundedly risked exactly
the busy-loop-that-never-returns hazard `MAX_TIMEOUT_RETRIES`'s own doc
comment already warns about for silence. Confirmed live afterward: one
`listen` window alone absorbed 200+ consecutive overflow events and still
completed and shut down cleanly on Ctrl-C.

This was a real reliability gap for any unattended run longer than a few
minutes, not a hypothetical -- worth keeping in mind for the still-open
M2 Pi4 CPU-budget/24h-soak acceptance legs (CLAUDE.md Status): a
resource-constrained Pi4 is *more* likely to overflow the ring buffer
under load than this Mac Mini was, not less.

## Finding 2 (field-confirmed, not fixed here): MAN-7/103's near-channel-edge bug produces plausible-looking false-positive spots

Across 10 completed 30-minute `listen --json` windows plus one partial
(11 total, ~5.5h), 29 `Spot`s were confirmed -- but every one shares the
same signature:

- confidence pinned to 0.118-0.172 (the low end of the scale)
- `snr_db` pinned to -8.28 to -7.29 dB -- indistinguishable from the
  session's own background noise floor, not a real carrier
- WPM scattered widely and often implausible for real CW traffic/beacons
  (30.8-60.0 wpm)
- callsigns frequently malformed (`3EMEEEE`, `ER1EEAE`, `4AEEEEE` --
  repeated-dit noise artifacts), occasionally coincidentally
  plausible-looking (`N4IIR`, `I8ET`)

More telling: spot frequencies cluster tightly at a handful of near-
identical values across *separate, non-overlapping* windows (6931.2-
6931.9, 6936.0, 6944.0, 6968.0, 7064.0 kHz) rather than being randomly
distributed -- a signature of a fixed decode artifact, not noise
variance. Isolated with a dial-shift test: retuning from 7025 kHz to
7075 kHz (passband ~6979-7171 kHz) moved the artifact to the *new*
passband's edge (7168.2 kHz) instead of leaving it at 6931 kHz. This
matches CLAUDE.md's already-tracked "V2: near-channel-edge WPM bug
(MAN-7/103)" exactly -- first real-world field confirmation of its
practical impact (spurious RBN-shaped spots, not just a WPM-accuracy
metric), not a new bug.

Operational implication for anyone running `manta` against real RF before
MAN-7/103 lands: `spots_confirmed`/emitted `Spot`s alone are not
sufficient evidence of a real decode today. Cross-check `snr_db` against
the session's own noise floor and `confidence` before trusting a spot,
especially near a channelizer boundary.

## Finding 3 (extends 2026-09-08's Finding 3, still open): no confirmed real off-air CW copy on 40m tonight

Zero of the 29 confirmed spots were real signal by the above analysis.
Max instantaneous `TrackMeta.snr_2500_db` touched positive values in
several windows (best full-window case +5.6 dB; one anomalous +18.3 dB
track, `track_id` 5700 at 7076576 Hz, appeared in the final partial
window right as the session was cut short for a band switch to 20m --
zero characters decoded on that track before it was interrupted, so it's
unresolved either way, not confirmed real or confirmed artifact). No
track this session produced a validated callsign backed by above-noise
SNR and non-floor confidence.

Same open question as 2026-09-08: antenna/feedline/band conditions vs.
something else remains undiagnosable from software alone -- manta still
has no built-in RF-level/antenna diagnostic. Given 40m CW is rarely
silent for 5.5 continuous hours in practice, antenna/feedline is now the
more likely suspect than band conditions alone; worth a manual continuity/
SWR check before the next unattended run.

## Takeaway

The overflow fix is a genuine reliability improvement, confirmed in the
field, independent of whether 40m ever produces a real decode. MAN-7/103
now has concrete field evidence beyond the WPM-accuracy angle it was
originally tracked under. Off-air real-signal confirmation is still the
one open item neither this run nor 2026-09-08's closed -- next attempted
on 20m same night, see follow-up session notes.
