# MAN-494: calibrated IQ conditioning

A receiver's gain and phase imbalance copies a station into its conjugate
mirror. MAN-494 adds the IQ stage omitted from MAN-203's first slice.

## Fixed calibration

The sample model is `m.re = s.re` and
`m.im = g*(s.im*cos(phi) + s.re*sin(phi))`. A separate controlled-tone
capture measures `rho = c_minus / conj(c_plus)`. The fixed corrector applies
`y = m - rho*conj(m)` after impulse suppression and DC removal. Its inverse
is `(y + rho*conj(y)) / (1 - |rho|²)`.

A capture must contain at least five one-second segments, a median tone SNR
of at least 60 dB in a 1 Hz bin, and segment coefficients within
`10^(-50/20)` of their mean. The tone search covers 300 Hz on either side
of the declared offset. DC, Nyquist and aliased noise-probe locations are
refused. An image rejection of at least 60 dB describes a balanced receiver.
The meter retains at most the first second of samples. Later measurements
use streaming phasors and bounded statistics. SNR uses the lower median
of 0.1 dB bins. The deviation check bounds all coefficients using their
component-extrema rectangle around the mean. These conservative bounds may
reject a marginal capture that an exact median or per-segment maximum would
accept. They cannot admit a capture outside the stated thresholds.

Coefficients freeze when the stream starts. A tone measurement never changes
an active corrector. This keeps results identical for every input chunk size.

## Observation and withholding

The monitor reads each formed channelizer hop once and accumulates five-second
windows. It excludes DC, Nyquist and two channels on either side when choosing
the strongest channel. A suspected image needs the strong channel at least
30 dB over the median floor and its mirror at least 6 dB over the floor,
with the mirror no stronger than the selected channel. Coherence is evidence
only. Two independent mirrored stations never authorize correction.

Apply requires validated gain, phase and a positive calibrated centre that
agrees with the source-reported centre within 1 Hz. Missing or mismatched
calibration leaves samples unchanged, observes mirrors and marks uncertainty.
A centre mismatch warns once per input, including reconnects. Suspected
complete windows without active correction mark newly covered samples
uncertain. Partial windows wait. Logs retain bounded span and measurement
state with exact counters.

## Operator configuration

`iq_correction` defaults to `off`; its other modes are `observe` and `apply`.
The calibration keys `iq_gain_ratio`, `iq_phase_deg` and
`iq_calibrated_center_hz` are all-or-none. Gain must be finite and between
0.5 and 2.0, phase between -45 and 45 degrees, and centre finite and positive.
Apply requires the triple. All four keys are restart-only. Each `[[input]]`
keeps its own settings. `bands` refuses a calibration because generated
streams have different centres. Decode refuses conflicting multi-entry IQ
settings. Environment overlays share the file validators.

Measure on a controlled tone and remeasure after receiver gain changes.
`check` and frequency `calibrate` remain raw. MAN-495 owns doctor's future
measurement command and presentation. MAN-496 owns conditioning JSON and
metrics; this change defines neither an IQ JSON object nor a new metric.

## Measured outcomes

The deterministic six-station fixture runs at 96 kS/s with one 45 dB station
at +12056.25 Hz and five 8 dB stations. Trials 1 and 2 with gain 1.05 and
phase error 2 degrees each produce one false mirror spot on the clean
implementation baseline `4420ec7`. The separate five-second tone capture
misses its declared offset by 13.7 Hz.

`cargo test -p manta-engine --locked --test man203_conditioners iq_` passes
four tests. For both trials the measured calibration removes the false
mirror spot and retains all six clean calls. A balanced ten-station 45 dB
capture retains the same calls in both trials. A mismatched centre retains
the exact bypass spots and marks the samples uncertain.

No live receiver or Raspberry Pi timing was measured. Default decoding stays
unchanged because every stage remains off unless configured.
