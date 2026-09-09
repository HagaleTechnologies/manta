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
`listen` window alone logged 200+ `ErrorCode::Overflow` occurrences over
its ~30-minute run (SoapySDR's own `O` status character, one per
occurrence) and still completed and shut down cleanly on Ctrl-C -- an
aggregate count across many separate `read()` calls, each recovering via
a normal read afterward, not 200 in a single unbroken run against the
100-count `MAX_OVERFLOW_RETRIES` bound (which would itself abort the
session at the 100th consecutive occurrence within one `read()` call).

This was a real reliability gap for any unattended run longer than a few
minutes, not a hypothetical -- worth keeping in mind for the still-open
M2 Pi4 CPU-budget/24h-soak acceptance legs (CLAUDE.md Status): a
resource-constrained Pi4 is *more* likely to overflow the ring buffer
under load than this Mac Mini was, not less.

## Finding 2 (new, unresolved -- likely distinct from MAN-7/103): a passband-edge artifact produces plausible-looking false-positive spots

Across 10 completed 30-minute `listen --json` windows plus one partial
(11 total, ~5.5h), 29 `Spot`s were confirmed -- but every one shares the
same signature:

- confidence pinned to 0.118-0.172 (the low end of the scale)
- `snr_db` pinned to -8.28 to -7.29 dB. This is emitted spot-side from
  `TrackMeta.snr_2500_db`, which is `Demod::snr_2500_db()`
  (`crates/manta-decode/src/envelope.rs`): `20*log10(e_hi/e_lo) - 14.3 dB`,
  where `e_hi`/`e_lo` are the 90th/10th-percentile envelope amplitude
  *within the track's own decode window* (the decoder's keying-rail
  contrast). It is **not** the detector's `(S-F)` signal-vs-local-floor
  SNR -- an earlier revision of this finding wrongly attributed it to
  that quantity and derived a "+6 dB above the noise floor" claim from
  it that doesn't follow from what's actually computed. What the pinned
  value *does* establish: this track's keying-rail contrast is nearly
  identical across all 29 independent detections rather than varying the
  way real fading/QRM would -- consistent with a deterministic mechanism,
  not proof by itself of anything about signal-vs-floor.
- WPM scattered widely and often implausible for real CW traffic/beacons
  (30.8-60.0 wpm)
- callsigns frequently malformed (`3EMEEEE`, `ER1EEAE`, `4AEEEEE` --
  repeated-dit noise artifacts), occasionally coincidentally
  plausible-looking (`N4IIR`, `I8ET`)

More telling: spot frequencies cluster tightly at a handful of near-
identical values across *separate, non-overlapping* windows (6931.2-
6931.9, 6936.0, 6944.0, 6968.0, 7064.0 kHz) rather than being randomly
distributed -- a signature of a fixed decode artifact, not noise
variance. **But only the 6931.x cluster is dial-shift-confirmed as a
passband-edge effect**: retuning from 7025 kHz to 7075 kHz (passband
~6979-7171 kHz) moved *that* cluster to the *new* passband's lower/upper
edge (7168.2 kHz) instead of leaving it at 6931 kHz. The 6936.0, 6944.0,
and 7064.0 kHz clusters sit well inside the 6930-7120 kHz passband, not
at either edge -- the dial-shift test was never repeated against them,
so grouping them under the same "passband-edge artifact" label is not
established. They could be a different fixed-bin spur (e.g. an
individual 93.75 Hz channelizer boundary landing at a different absolute
frequency each session, which would actually be closer to MAN-7/103's
per-channel mechanism than the confirmed passband-edge cluster is) or a
distinct decoder false-positive path entirely -- unresolved pending a
retune test repeated against each interior cluster specifically.

**Not the same bug as MAN-7/103 without more evidence, despite the
"edge" coincidence.** MAN-7/103 (`crates/manta-cli/tests/
golden_v2_v3.rs:23-55`, `#[ignore]`d pending
<https://github.com/HagaleTechnologies/manta/issues/24>) is a WPM-
*estimation* error (~29 vs. an expected 35 WPM) for a real keyed signal
near one *individual* 93.75 Hz PFB channel edge -- the golden test shows
a clean decode with correct WPM at channel center, and nothing in that
regression shows the near-edge offset spawning tracks or spots from
noise. What this session isolated tracks the edge of the entire 192 kHz
*input passband* (a single boundary, not one of the many per-channel
boundaries scattered through it) and produces spurious *detections*, not
a WPM error on a real one. These could still share a root cause (e.g.
channelizer transient response at a boundary), but that link isn't
established here -- treat this as a separate, currently untracked
front-end/passband-edge false-positive defect until individual-channel
residuals and the detection path (not just the WPM path) are examined
directly.

Operational implication either way, for anyone running `manta` against
real RF right now: `spots_confirmed`/emitted `Spot`s alone are not
sufficient evidence of a real decode. A spot whose `snr_db` sits within
about a dB of this session's own recurring floor value and whose
confidence sits at the low end of the scale is suspect -- especially
near a passband or channelizer boundary -- but confirming it's actually
noise (rather than a real weak/repeating signal) needs more than the
`snr_db` value alone.

**`manta doctor` specifically is fully blind to this.**
`DoctorReport::verdict()` (`crates/manta-engine/src/doctor.rs:143-146`)
returns `Verdict::Decoding` the instant `spots_confirmed > 0`, before
looking at SNR at all -- an affected run driven entirely by this artifact
still reports `"DECODING -- at least one confirmed spot. End to end,
working."` with no hint anything is wrong. This is a real, currently
unresolved consequence of the false-positive defect above, not just a
caution for a human reading raw `listen --json` output: `doctor`'s whole
purpose is to be the quick automated health check, and right now it can
be quick, automated, and wrong at the same time.

## Finding 3 (extends 2026-09-08's Finding 3, still open): no confirmed real off-air CW copy on 40m tonight

None of the 29 confirmed spots are backed by a validated callsign at a
clearly-varying, above-population SNR -- by the signature in Finding 2
(pinned near-identical `snr_db`, low confidence, malformed text,
fixed-frequency clustering) every one is far more consistent with a
deterministic artifact (confirmed passband-edge for the 6931.x cluster,
unconfirmed mechanism for the rest) than with real copy, though this is
"no *evidence* of real copy," not a from-first-principles proof each one
is noise. Max
instantaneous `TrackMeta.snr_2500_db` touched positive values in
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
field, independent of whether 40m ever produces a real decode. This run
also surfaced a second, separate, currently-untracked false-positive
defect at the input passband edge -- plausibly but not confirmedly
related to MAN-7/103 -- plus at least one more fixed-frequency cluster
with an unconfirmed, possibly-distinct mechanism, both worth their own
investigation rather than folding into an existing bug by assumption.
`manta doctor`'s verdict currently can't tell either of these apart from
a real decode. Off-air real-signal confirmation is still the one open
item neither this run nor 2026-09-08's closed -- next attempted on 20m
same night, see follow-up session notes.
