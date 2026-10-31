# manta command-line JSON

This page lists every field of the JSON that `manta decode --json`,
`manta run --json` (alias `listen`) and `manta doctor --json` print on
stdout. It is a debugging and scripting format:

- It is not the `:7301` spot stream ([SPOT-SCHEMA.md](SPOT-SCHEMA.md)) and
  not a dispensa contract. A spot here has no `id`, `timestamp`, station
  identity or geography.
- A field change is listed in [CHANGELOG.md](../CHANGELOG.md).
- Every `snr` on this page is in dB in a 2500 Hz reference bandwidth, like
  the stream's `snr` with `snrRefHz: 2500`. Telnet and the RBN uplink quote
  500 Hz, about 7 dB higher.
- Values are not rounded. `frequency` is in Hz with a fraction, and `snr`
  and `wpm` keep their decimals; the stream rounds all three to integers.
- Key order follows the tables below, but a script should look keys up by
  name.

Other commands' `--json` (`bench`, `check`, `calibrate`, `devices`,
`status`) are not described here.

## Naming

- Every key is camelCase.
- A concept that the JSON spot stream or the live-state stream
  ([LIVE-STATE.md](LIVE-STATE.md)) also carries uses the stream's name:
  `frequency`, `snr`, `wpm`, `dxCall`, `decodeConfidence`, `trackId`,
  `spotType`, `closure`, `survivorTrackId`, `rst` and `qrlQuery`.
- Any other quantity ends in a short unit suffix, as the streams' `loHz`
  and `windowS` do: `centerFreqHz`, `sampleRateHz`, `markHz`, `ditMs`,
  `durationS`.
- An enumerated value has the spelling it already has elsewhere. A glyph is
  its character or [SPEC §4.4](SPEC-decode-core.md#44-beam-search-over-the-morse-tree-width-4)'s
  prosign token. `spotType` and `closure` use the live-state labels. The
  doctor verdict and check status use the text report's words
  (`WEAK_NO_DECODE`, `PASS`). Event names are SPEC §5's.

For example, one `TrackMeta` event:

```json
{"event":"TrackMeta","trackId":1,"sampleTs":102656,"snr":22.639294,"frequency":14012341.459738266}
```

`sampleTs` is the input-stream sample counter (SPEC §5): divide it by the
input's sample rate for seconds from the start of the input.

## `decode --json`

One JSON object, printed when the decode ends. The primary track is the
lowest-numbered track that sent any event besides `TrackPromoted` and
`TrackClosed`.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `frequency` | number | never | The primary track's last `TrackMeta` frequency, in Hz, after `--freq-correction-ppm`. The input's centre frequency when that track sent no `TrackMeta`. |
| `wpm` | number or null | when unmeasured | The primary track's last `SpeedUpdate` speed. `null` when it sent none. |
| `text` | string | never | The primary track's decoded text, words separated by single spaces. Prosigns are left out. |
| `events` | array | never | Every track's events in emission order. See [Events](#events). |
| `spots` | array | never | Every validated spot, in emission order; empty when there is none. See [Spots](#spots). |
| `timing` | object or null | until learned | The primary track's last `TimingUpdate`, without `event`, `trackId` and `sampleTs`. See [`timing`](#timing). `null` when the track sent none. |
| `rtty` | array | never | Every RTTY signal the detector marked (SPEC §2.6); empty when there is none. See [`rtty` entries](#rtty-entries). |
| `badSamples` | integer | never | Input samples replaced with silence because they were NaN, infinite or too large (SPEC §1.3 step 0). Omitted when 0. |

## `run --json` and `listen --json`

One JSON object per line, as JSON Lines:

- An event line is one [event](#events) object, with `event` first.
- A spot line is `{"spot": {…}}`, where the inner object is one
  [spot](#spots), with the same keys, order and digits as the same spot in
  `decode --json`.

With several `[[input]]` entries the records are the same.

```json
{"event":"TrackClosed","trackId":1,"closure":"signalEnded","survivorTrackId":null}
{"spot":{"dxCall":"W1AW","frequency":14012341.017764632,"snr":20.995895,"wpm":19.565216,"spotType":"cq","decodeConfidence":0.55182433,"trackId":1,"sampleTs":2006272,"rst":null,"qrlQuery":false}}
```

## Events

[SPEC §5](SPEC-decode-core.md#5-decoder-output) says when each event is
emitted. Every event has `event` and `trackId`.

### `CharDecoded`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"CharDecoded"`. |
| `trackId` | integer | never | The track that decoded the character. |
| `sampleTs` | integer | never | Input sample counter at the character. |
| `glyph` | string | never | The decoded character (`"A"`), or a prosign token: `"<AR>"`, `"<SK>"`, `"<AS>"`, `"<SN>"` or `"<ERR>"`. |
| `confidence` | number | never | Per-character confidence from 0 to 1 (SPEC §4.5). |
| `alternatives` | array | never | Up to two competing glyphs at this position, as `[glyph, probability]` pairs, for example `[["D",0.41]]`. Empty with the default `legacy` engine. |

### `WordBoundary`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"WordBoundary"`. |
| `trackId` | integer | never | The track. |
| `sampleTs` | integer | never | Input sample counter at the gap. |
| `confidence` | number | never | Probability that this gap is a word gap. Always `1.0` with the `legacy` engine. |

### `SpeedUpdate`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"SpeedUpdate"`. |
| `trackId` | integer | never | The track. |
| `wpm` | number | never | The track's keying speed in words per minute. Sent when it changes by 1 WPM or more. |

### `TrackMeta`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"TrackMeta"`. |
| `trackId` | integer | never | The track. |
| `sampleTs` | integer | never | Input sample counter at the hop this measurement is from. |
| `snr` | number | never | The track's SNR in dB, 2500 Hz reference (SPEC §2.3). |
| `frequency` | number | never | The track's frequency at that hop, in Hz. Absolute only when the source has an RF reference. |

### `ReadingsHeld`

Emitted only with `[decode] prior = "on"`, while at least two word readings remain acoustically viable. A character emits a snapshot, and word settlement emits a final snapshot when still ambiguous. Readings contain only glyphs supported by the timing beam.

| Key | Type | Meaning |
| --- | --- | --- |
| `event` | string | `"ReadingsHeld"`. |
| `trackId` | integer | Decoder track identity. |
| `sampleTs` | integer | Input sample timestamp. |
| `readings` | array | `[reading, gap]` pairs ranked by acoustic score plus bounded language bonus; gap is in nats relative to the leader, whose gap is zero. |
| `settled` | boolean | True at word settlement; the leading reading is committed. |

Example: `{"event":"ReadingsHeld","trackId":17,"sampleTs":7123400,"readings":[["W1AW",0.0],["R1AW",1.24]],"settled":false}`.

### `TimingUpdate`

The track's learned sender timing. The [`timing`](#timing) keys are part of
the event object itself.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"TimingUpdate"`. |
| `trackId` | integer | never | The track. |
| `sampleTs` | integer | never | Input sample counter at the update. |
| `ditMs` | number | never | As in [`timing`](#timing). |
| `dot` | object or null | until observed | As in [`timing`](#timing). |
| `dash` | object or null | until observed | As in [`timing`](#timing). |
| `elementGap` | object or null | until observed | As in [`timing`](#timing). |
| `charGap` | object or null | until observed | As in [`timing`](#timing). |
| `wordGap` | object or null | until observed | As in [`timing`](#timing). |

### `TrackPromoted`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"TrackPromoted"`. |
| `trackId` | integer | never | The new track. |
| `sampleTs` | integer | never | Input sample counter at the hop the detector promoted it. |
| `frequency` | number | never | The track's frequency at promotion, in Hz. |

### `TrackClosed`

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `event` | string | never | `"TrackClosed"`. |
| `trackId` | integer | never | The track. It sends no further event. |
| `closure` | string | never | `signalEnded` or `bookkeeping`, the labels of LIVE-STATE's `trackClosed`. `signalEnded`: the signal ended, or the input did. `bookkeeping`: tracking stopped with no evidence that the signal ended, for example a merge or an eviction. SPEC §5 lists which closes are which. |
| `survivorTrackId` | integer or null | unless merged | For a `bookkeeping` closure that merged into another track, that track's `trackId`. `null` otherwise. |

## Spots

A validated spot: an element of `decode --json`'s `spots`, or the inner
object of a `run --json` spot line.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `dxCall` | string | never | The decoded callsign, as validated. |
| `frequency` | number | never | Spot frequency in Hz. Absolute only when the source has an RF reference. |
| `snr` | number | never | SNR in dB, 2500 Hz reference. |
| `wpm` | number | never | Keying speed in words per minute. |
| `spotType` | string | never | `cq`, `de`, `beacon` or `unknown`: the context the call was decoded in, with the same labels as LIVE-STATE's `candidateDecision`. |
| `decodeConfidence` | number | never | Callsign confidence from 0 to 1 (SPEC §4.6), as the stream's `decodeConfidence`. |
| `trackId` | integer | never | The track that decoded the call. |
| `sampleTs` | integer | never | Input sample counter at the decode that produced the spot. |
| `rst` | string or null | when none decoded | The track's latest RST report as three digits (`5NN` reads as `"599"`). `null` when the track decoded none. |
| `qrlQuery` | boolean | never | `true` when the track decoded `QRL?` before the spot. |

## `timing`

A track's learned sender timing (SPEC §4.1b), as `decode --json`'s `timing`
and inside each `TimingUpdate` event.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `ditMs` | number | never | The dit length in milliseconds that the classes below are measured in. |
| `dot` | object or null | until observed | The dot class. See [Timing classes](#timing-classes). |
| `dash` | object or null | until observed | The dash class. |
| `elementGap` | object or null | until observed | The gap between elements of one character. |
| `charGap` | object or null | until observed | The gap between characters. |
| `wordGap` | object or null | until observed | The gap between words. |

## Timing classes

Each class in `timing` that has at least one observation.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `meanDits` | number | never | Mean duration, in dits of `ditMs`. |
| `varDits2` | number | never | Variance, in dits squared. |
| `n` | integer | never | Number of observations. |

## `rtty` entries

One RTTY signal the detector marked in `decode --json`'s `rtty`.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `markHz` | number | never | Upper tone frequency in Hz, on the same scale as `frequency`. |
| `spaceHz` | number | never | Lower tone frequency in Hz. |
| `shiftHz` | number | never | The nearest standard shift: 170, 200, 425, 450 or 850 Hz. |
| `measuredShiftHz` | number | never | The measured `markHz - spaceHz`, without frequency correction. |
| `markedSampleTs` | integer | never | Input sample counter when the detector marked the signal. |
| `clearedSampleTs` | integer or null | while marked | Input sample counter when the mark cleared. `null` when the signal was still marked at the end of the input. |

## `doctor --json`

One JSON object, printed after the checks. Its keys hold the same numbers as
the text report.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `sampleRateHz` | number | never | The receiver's sample rate in Hz, as negotiated. |
| `centerFreqHz` | number | never | The receiver's centre frequency in Hz. For an audio source without `--dial-freq-hz` it is 0, and every frequency is an audio offset. |
| `durationS` | number | never | Seconds of input analysed, the text report's `observed for 10.0s`. |
| `tracksPromoted` | integer | never | `TrackPromoted` events: tracks the detector confirmed as keyed CW. |
| `detectorConfirmations` | integer | never | Every signal the detector confirmed, keyed CW or not. Nonzero with `tracksPromoted` 0 gives `NOT_CW`. |
| `trackMetaCount` | integer | never | `TrackMeta` events seen. |
| `tracksClosed` | integer | never | `TrackClosed` events seen. |
| `snrMin` | number or null | when no `TrackMeta` | The lowest `TrackMeta` `snr`, in dB, 2500 Hz reference. |
| `snrMedian` | number or null | when no `TrackMeta` | The median `TrackMeta` `snr`. |
| `snrMax` | number or null | when no `TrackMeta` | The highest `TrackMeta` `snr`. |
| `charsDecoded` | integer | never | `CharDecoded` events seen. |
| `distinctChars` | integer | never | Distinct decoded characters, prosigns left out. |
| `spotsConfirmed` | integer | never | Validated spots. |
| `badSamples` | integer | never | Input samples replaced with silence. Omitted when 0. |
| `verdict` | string | never | `NO_SIGNAL`, `NOT_CW`, `ACTIVITY_NO_SNR`, `NOISY_NO_DECODE`, `WEAK_NO_DECODE` or `DECODING`, as the text report's `verdict:` line. |
| `checks` | array | never | One object per check line, in the text report's order. See [`checks` entries](#checks-entries). |
| `checksStatus` | string | never | The worst check status: `FAIL` if any check failed, else `WARN` if any warned, else `PASS`. |

When the receiver does not open, the object holds only `checks` and
`checksStatus`.

## `checks` entries

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `name` | string | never | The check's name, as on its text line (`config`, `clock`, `receiver`, …). |
| `status` | string | never | `PASS`, `WARN`, `FAIL` or `SKIP`, the word the text line starts with. |
| `detail` | string | never | What the check found. |
| `fix` | string or null | for `PASS` and `SKIP` | What to do about a `WARN` or `FAIL`. |

[RUNBOOKS/setup-checks.md](RUNBOOKS/setup-checks.md) describes every check.

## Names before MAN-139

Before MAN-139 these outputs printed the Rust field names. A
`decode --json` report saved before then does not load in
`examples/replay_spots.rs`; run `manta decode --json` again to regenerate it.

| Before | Now |
|---|---|
| `freq_hz` (report, `TrackMeta`, `TrackPromoted`, spot) | `frequency` |
| `bad_samples` | `badSamples` |
| `track_id`, `sample_ts` | `trackId`, `sampleTs` |
| `"glyph":{"Char":"F"}`, `{"Prosign":"Ar"}` | `"glyph":"F"`, `"<AR>"` |
| `snr_2500_db` (`TrackMeta`), `snr_db` (spot) | `snr` |
| `"closure":"SignalEnded"` | `"closure":"signalEnded","survivorTrackId":null` |
| `"closure":{"Bookkeeping":{"survivor_track_id":N}}` | `"closure":"bookkeeping","survivorTrackId":N` |
| `dit_ms`, `element_gap`, `char_gap`, `word_gap` | `ditMs`, `elementGap`, `charGap`, `wordGap` |
| `mean_dits`, `var_dits2` | `meanDits`, `varDits2` |
| `callsign` (spot) | `dxCall` |
| `confidence` (spot) | `decodeConfidence` |
| `spot_type` (`"Cq"`) | `spotType` (`"cq"`) |
| `qrl_query` | `qrlQuery` |
| `mark_hz`, `space_hz`, `shift_hz`, `measured_shift_hz`, `marked_sample_ts`, `cleared_sample_ts` | `markHz`, `spaceHz`, `shiftHz`, `measuredShiftHz`, `markedSampleTs`, `clearedSampleTs` |
| `sample_rate_hz`, `center_freq_hz` | `sampleRateHz`, `centerFreqHz` |
| `duration` (`{"secs":10,"nanos":0}`) | `durationS` (`10.0`) |
| `tracks_promoted`, `detector_confirmations`, `track_meta_count`, `tracks_closed` | `tracksPromoted`, `detectorConfirmations`, `trackMetaCount`, `tracksClosed` |
| `snr_db_min`, `snr_db_median`, `snr_db_max` | `snrMin`, `snrMedian`, `snrMax` |
| `chars_decoded`, `distinct_chars`, `spots_confirmed` | `charsDecoded`, `distinctChars`, `spotsConfirmed` |
| `verdict` (`"WeakNoDecode"`) | `verdict` (`"WEAK_NO_DECODE"`) |
| `checks[].status` (`"pass"`) | `checks[].status` (`"PASS"`) |
| `checks_status` (`"pass"`) | `checksStatus` (`"PASS"`) |

`event` names, `wpm`, `text`, `events`, `spots`, `timing`, `rtty`, `dot`,
`dash`, `n`, `confidence` on events, `alternatives`, `rst`, `checks`, and a
check's `name`, `detail` and `fix` kept their names.
