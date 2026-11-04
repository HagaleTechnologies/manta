# MAN-321 first slice: diagnostic ITA2 text

This slice adds a deterministic 45.45 baud ITA2 decoder and feeds it from
already recognised FSK tone channels. `decode --json` reports the latest
256 ASCII characters in each signal's `decodedText`. These are diagnostic
characters, without callsign validation. No RTTY spots are emitted yet.

The channelizer already filters each tone. The demodulator compares their
instantaneous powers, synchronises on a mark-to-space transition after an
idle bit, samples the start bit and five data bits, and checks the stop bit.
Both polarities acquire independently. Three consecutive valid frames select
the polarity and release the acquisition text. ITA2 letters and figures shifts
are local to each polarity. The tests use hand-checked codes independently of
the testkit encoder. The default testkit polarity remains unchanged; reversed
fixtures swap the two physical tones. Coppa remains pinned at `f8a4d16d`.

At most 100 diagnostic demodulators run per input. Existing decoders survive
until recognition clears; free slots admit marked signals in frequency order.
Text and framing state are bounded. Recognition frequencies retain their
existing upper-tone `markHz` convention, including reversed transmissions.
A signal's physical mark frequency will be resolved by the engine adapter.

## Follow-up scope

The full implementation exceeded the dispatcher's 40-file limit in the prior
attempt at 63 files. This slice keeps the DSP and fixture work, with a small
production connection so the decoder can be exercised on band streams.

One follow-up must complete plan phases 2 through 4: candidate-channel preroll,
configurable admission and dropped-episode accounting, shared track IDs,
DecoderEvents and lifecycle, unchanged spot validation with mode metadata,
all spot outputs and filters, metrics, Watterson engine vectors, performance
measurements, and the README/ROADMAP/SPEC scope revision. Until then RTTY
spotting and recovery of the transmission's opening are not available. The
roadmap and README keep their existing scope claims for that reason.
