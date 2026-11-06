# MAN-30: CW sub-segments and UTC schedules

Operators can restrict each input to a union of calibrated RF ranges, with daily
UTC switches and dated overrides. `docs/CONFIG.md` is the normative format.
Absent coverage preserves the old path; empty coverage admits no CW tracks.
The Pi 4 gate is still open, so MAN-18 does not justify closing this request.

The engine owns validated, normalized unions. CLI parsing supplies input-aware
errors and checks UTC calendar values. Daily entries wrap across midnight;
nonoverlapping dated overrides win until their exclusive end. Configuration
remains immutable after startup. Edits remain restart-only under MAN-78.
Local timezones, solar calculations and recurring weekday rules are deferred.

Live sources sample UTC before a processing batch. Replays use an explicit epoch
or a native recording's epoch plus corrected source samples. Realtime playback
and loops do not consult wall time. Schedule boundaries split decoder batches.
Every segment, including startup and discontinuity rebuilds, installs the policy.
Multi-input front ends each install their own policy and calibration.

Birth checks use the existing fine-position estimate when available, otherwise
the channel position, with wrapped RF conversion. Updated track estimates must
remain admitted before input is queued. Boundaries are inclusive; existing
frequency uncertainty still applies. Other safety masks retain their own vetoes.

Restriction closes an excluded identity as bookkeeping without a survivor.
It drops decoder and refiner queues without finishing a mark or word, releases
ownership and adds no respawn cooldown. The validator discards pending words and
beacons for bookkeeping without a survivor, retaining conservative activity
history. Other closure behavior stays intact. Retained identities keep state.
Already emitted events remain valid.

Restriction saves track and decoder work, not channelization or acquisition.
Deterministic tests compare empty coverage with a decoding control, and paired
`cpu_budget` benchmark cases use the same seeded scene. No meaningful wall-time
comparison or Pi 4/Mac CPU verdict was measured in this cloud container. The
hardware budget remains MAN-18 work. This adds no dynamic RBN registration and
makes no claim about its blocked exchange.
