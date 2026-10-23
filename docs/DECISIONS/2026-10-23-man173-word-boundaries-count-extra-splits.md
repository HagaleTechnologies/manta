# MAN-173: a word-boundary gate fails a decode that adds word breaks

Date: 2026-10-23. Ticket: MAN-173, scoped by Tony Hagale's grooming comment
of 2026-10-13. The quoted word-count formula was already gone, replaced by
Levenshtein alignment in `4e71cba`; inserted spaces were still unpenalised.

## Problem

`word_boundary_fraction` scored recall only. Matching every keyed space
scored 1.0 even if the decode also split words. The MAN-173 plan's probe on
2026-10-23 added splits to the real VR7 HSMM transcript, which has 69 keyed
boundaries. Both existing VR7 bars accepted these altered transcripts:

| Extra splits | Decoded boundaries | Old score | New score | CER |
|---|---|---|---|---|
| 0 | 69 | 1.0000 | 1.0000 | 0.0027 |
| 7 | 76 | 1.0000 | 0.8986 | 0.0214 |
| 17 | 86 | 1.0000 | 0.7536 | 0.0481 |
| 18 | 87 | 1.0000 | 0.7391 | 0.0508 |

CER alone failed only from 18 splits. V10's every-character split failure
would also have scored a perfect 1.0. VW2's "long gaps keep word
boundaries" gate carried the same helper and blind spot.

## Decision

Both copies now score `max(0, credited - spurious) / expected_boundaries`.
The character-level Levenshtein alignment matches keyed spaces to decoded
spaces. `credited` counts these matches plus the existing forgiveness for
an entirely dropped first word; `spurious = decoded_spaces - matched`.
This is 1 minus a boundary error rate, as character accuracy is 1 minus CER.
A missed boundary and an extra boundary each cost `1 / expected_boundaries`.
A moved boundary is two errors. With no expected boundary, the score is 1
only if the decode adds none, otherwise 0. Warm-up forgiveness is unchanged
and independent of the spurious-boundary penalty.

Thresholds stay at VR1 ≥ 0.95, VR7 ≥ 0.90 and VW2 ≥ 0.95. The plan's real
decodes measured on 2026-10-23 all still score 1.0: VR7 HSMM matched 69/69
boundaries, VR1 HSMM 109/109, and VW2 legacy 28/28. The VR7 number in the
[2026-09-09 stage-2 gate record](2026-09-09-decode-core-v2-stage2-gate.md)
used the earlier recall-only scoring and remains a historical measurement.

`vr7_fails_a_decode_that_over_segments_words` pins a VR7-shaped transcript
with seven split calls. It passes VR7's CER bar but fails its boundary bar.
The real VR1, VR7 and VW2 decode gates retain their thresholds.

## What this rules out

- F1 fails the altered VR7 transcript only from 16 splits, two short of
  CER's failure at 18. It would add little signal to the boundary gate.
- Word counts cannot detect a split and merge that cancel each other.
  Boundary-position comparisons conflate inserted or deleted letters with
  moved spaces. Rounds 1 and 4 of the helper's history record these cases.
- The round-6 alignment limitation remains. For `CQ TEST W5AU W5AU TEST`
  decoded as `CQT E ST W5AU W5AU TEST`, repeated structure lets alignment
  credit the wrong space. The score is now 0.75, while 0.25 is correct.
  Closing that gap needs word-level alignment with a content-similarity
  cost. The existing known-limitation test documents it.
