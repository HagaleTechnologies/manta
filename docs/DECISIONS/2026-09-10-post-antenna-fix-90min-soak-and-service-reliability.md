# Post-antenna-fix 90-minute unattended soak: real detection confirmed at scale; SDRplay service reliability is a real, recurring operational problem

Follow-up to `docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md`.
That doc confirmed the antenna/feedline fix (reseated connections + common-
mode choke) resolved the mid-band detection gap with a single 5-minute
capture (3 confirmed spots, one verified real callsign). This session ran
a longer, unattended 4-cycle soak (nominally 22 min/cycle, ~90 min total)
on 20m to see whether that result holds up at scale, and surfaced a
second, independent problem along the way.

## Method

Same config as the prior fix-confirmation test (`--soapy-freq 14030000
--soapy-rate 192000 --soapy-gain 15`), run as 4 sequential cycles rather
than one long capture (deliberately, for resilience -- see Result 2).
Each cycle ran `manta run --json` simultaneously with a live RBN telnet
capture over the identical window, checked the SDRplay device was
enumerable before starting, and summarized independently.

## Result 1: real detection confirmed at scale, with strong external validation

22 confirmed spots across all cycles. Classified by the same signature
that distinguished real catches from the known Beacon-exemption residual
gap all session:

- **13 spots (59%) match the real-catch signature**: `Cq`/`De` type,
  confidence 0.22-0.43, plausible WPM, real signal-level SNR. Several
  confirmed repeatedly across independent cycles -- **W3RJ four times**,
  **KC4X four times** -- both at consistent frequencies.
- **9 spots (41%) match the known residual-artifact signature**:
  `Beacon` type, confidence 0.14-0.18 -- the small, accepted gap from PR
  #154 (`docs/DECISIONS/...`, tracked in issue #163/#173). Four of these
  in cycle 1 were near-identical garbled callsigns (AU1UN/VU1UN/EU1US/
  EU1UN) all at the same ~14100.0xx kHz frequency -- almost certainly one
  recurring noise source, not four real DX stations.

**External validation against the simultaneously-captured RBN logs**: of
7 real-looking callsigns checked, 4 matched RBN almost exactly in
frequency (KX2P, KQ4TDQ, WB0RSZ, W3RJ -- all within 6-40 Hz), 1 matched
on callsign with a plausible ~4 kHz discrepancy (KC4X, consistent with a
QSY between sightings), 1 is a real active station RBN saw on a different
band at a different moment (N2XDD, 40m vs. our 20m capture -- confirms
the callsign is real and active that day, not a frequency-exact match),
and 1 had no RBN entry in this window (KE8TBM -- same as `WI9Q` in the
original fix-confirmation test; RBN coverage isn't exhaustive, this
doesn't count against it). `W3RJ` was independently verified as a real,
currently-registered US callsign via QRZ.com and QRZCQ.

**This confirms the single-capture result from the prior doc generalizes**:
the antenna/feedline fix produces real, externally-verifiable detections
repeatably, not as a one-off.

## Result 2 (new): the SDRplay API service crashed twice more during this soak, disrupting half the test

Of 4 planned cycles:
- Cycle 1: completed cleanly, full duration.
- Cycle 2: `sdrplay_api_ServiceNotResponding` crashed the stream partway
  through -- cycle ended early with partial data (113,941 events instead
  of cycle 1's 180,151).
- Cycle 3: failed immediately -- device enumeration itself failed
  (`SoapySDRUtil --find` returned "No devices found!", not just a stream
  activation failure).
- Cycle 4: the device had self-recovered by the time this cycle's
  pre-check ran, captured data for a while, then crashed the same way
  (`sdrplay_api_ServiceNotResponding`) partway through -- shortest cycle
  of all (20,651 events).

This is the third time this exact failure has occurred in one session
(see `docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`'s
"Blocked mid-session" section for the first two occurrences). Unlike
those earlier occurrences, this time the service **partially
self-recovered on its own** between cycle 3's failure and cycle 4's
successful start -- no privileged restart or physical USB replug was
performed between cycles 3 and 4, yet cycle 4 briefly worked before
failing again. This suggests the service's failure mode is genuinely
intermittent/flaky under sustained load, not simply "wedged until a human
intervenes" -- both behaviors have now been observed.

**Running this as multiple shorter cycles (rather than one long capture)
turned out to be the right call for resilience**: a single ~90-minute
capture would likely have been killed entirely by the first crash
(partway through what would have been "cycle 2"), losing everything after
that point. Splitting into cycles meant each crash only cost that one
cycle's remaining data, and the loop's own device health check correctly
detected and skipped cycle 3 rather than hanging or silently producing a
corrupt/empty result.

## Recommendation

**For any future unattended/long-duration live-hardware session**: use
short, independent capture cycles (this session used ~22 min) rather than
one long-running capture, specifically because of this service's
demonstrated unreliability under sustained load. A single long capture
has no resilience against a mid-run crash; cycling does. Consider adding
an automatic retry-on-device-not-found loop with a short backoff to the
capture tooling itself (not done here -- this session's cycling was
external shell scripting, not a manta code change) so an unattended run
doesn't need a human to notice a skipped cycle.

The underlying SDRplay service reliability issue itself (why it crashes
under sustained ~192 kS/s streaming, and why it sometimes self-recovers
and sometimes doesn't) is outside this repo's control -- it's SDRplay's
own API service, not manta code -- but is worth tracking as a known
operational hazard for anyone running unattended live-hardware sessions
with this hardware.
