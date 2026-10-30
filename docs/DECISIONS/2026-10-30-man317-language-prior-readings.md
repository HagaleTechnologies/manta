# MAN-317: bounded language priors for ambiguous word readings

Date: 2026-10-30. Ticket: MAN-317. Normative text: SPEC-decode-core.md
§4.7 and §5. Configuration: CONFIG.md `[decode]` and `[spot]`.

## Decision

Ship an optional word-readings beam behind `[decode] prior = "off"`.
The legacy and edge-legacy decoders retain acoustically viable complete
readings across characters and settle the word using acoustic likelihood
plus bounded list evidence. The HSMM engine is unchanged.

The per-character beam uses each sender's dot/dash variance after enough
observations. Sparse classes retain the configured sigma. The word beam
prunes on acoustic score alone before consulting the prior, retains at
most `held_readings` readings, and bounds each word to 24 glyphs. The
prior cannot invent characters or rescue an acoustically pruned reading.
Unknown calls remain eligible under the existing validator rules.

The prior combines exact SCP membership, logarithmic hearing counts,
allocated country prefixes and common CW words. Its fixed component
shares total at most one, multiplied by `prior_weight`; the decoder
independently caps the total bonus. The calls-heard file is optional,
local and absent by default. It has one call and an optional hearing count
per line. An absent file supplies no evidence. SCP and cty generation
changes remain visible through the shared reference-list cells.

Characters are delayed until word settlement, preserving their original
sample timestamps. A glyph matching the legacy acoustic winner keeps its
legacy confidence, so enabling the prior cannot perturb an unchanged spot
merely by recalculating its confidence. A changed glyph keeps its
per-sender acoustic confidence; list membership does not boost confidence. `ReadingsHeld` reports the
retained words and score gaps after each ambiguous character and again
at settlement. Its leader has gap zero, and the settlement leader is the
committed word. Prior-off emits no new events. Spot schemas and validator
thresholds are unchanged.

## Acceptance evidence and limits

The controlled ambiguity fixtures key `DE DE W1AW K` with testkit, then
suppress the tail of the leading W's third mark. The envelope-input test
retains 102 ms of that mark. The full CLI IQ test retains 98 ms before the
channelizer and adds 20 dB AWGN with seed 317. Both read R1AW with the prior
off and W1AW with it on. The CLI loads an actual calls-heard file through
`spot.calls_heard_path`, containing `W1AW 1000`. These are controlled
attenuation checks, not Watterson or live-SDR claims. The calls-heard
evidence uses the production `LanguagePrior` implementation.

The clean unknown-call test renders repeated the unlisted common-pattern call K1ZQX at 20 dB SNR through
the full CLI pipeline and compares the prior-off and prior-on spots.
The held-readings test verifies the competing words and finite score gaps,
along with deterministic events from identical samples.
`cargo test -p manta-cli --locked --test decode_prior_readings -- --nocapture`
passed all four tests in 6.72 s.

The prior-on golden test explores weights 0, 1 and 2 nats at a 3-nat
acoustic margin and four held readings on V8, V8w and pinned VC3. It
requires zero bogus calls and preserves the existing floors of 45/50 for
V8 and 20/50 for V8w/VC3, plus the VC3 promotion ceiling. Results are
recorded after the run below. These checks do not close the separate,
known fading CER gap in the ignored V8w accuracy tests. Watterson fixtures
continue to use coppa commit `f8a4d16df7e5776a0756943c05712038774e6c70`.

## Default-flip gate

The default remains off. A separate change must supply passing prior-on
V8/V8w/VC3 precision gates and a paired seven-day live RBN comparison with
no rise in the `none` grade rate. RUNBOOKS/rbn-comparison.md defines the
paired inputs, coverage denominator, raw counts and per-band comparison.
No recorded node logs for this experiment exist in this checkout. No live
week was run or simulated in this ticket.

## Measured weight matrix

The direct golden-test run completed all nine combinations in 602.28 s.
Each cell gives genuine calls validated out of 50, followed by bogus calls:

| Scene | Weight 0 | Weight 1 | Weight 2 | Existing floor |
| --- | --- | --- | --- | --- |
| V8 | 48 / 0 | 48 / 0 | 48 / 0 | 45 |
| V8w | 25 / 0 | 25 / 0 | 25 / 0 | 20 |
| VC3 | 21 / 0 | 21 / 0 | 21 / 0 | 20 |

The VC3 promotion ceiling also passed at every weight. Two nats is the
largest tested cap, so the proposed default cap remains 2.0, with a 3.0-nat
acoustic margin and four held readings. This finite matrix does not prove
a globally largest safe weight, nor does it authorize enabling the prior
by default. The repository gate repeats the matrix on the final tree.
