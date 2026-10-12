# 2026-10-12 — MAN-118: decode-quality signals on `/metrics`

**Status:** Implemented (branch `MAN-118`). Records how the per-track
decode-quality signals the decode loop already computed (garble count, the
keying-rail ratio, the repetition gate's count) reach Prometheus, and why
they are exported as node-level families rather than one series per track.

## Context

MAN-45's `manta_active_tracks` and MAN-64's `manta_source_health` say
whether the daemon is working at all. Nothing said whether the decode
itself looked healthy: three signals were computed per track and never left
the decode loop (found by the 2026-09-05 broad review, lens 3, hit-list
#32: "so live busted-rate can be watched"):

- `TrackDecoder::garble_count`, incremented when every beam branch rejects
  a character's marks (SPEC §4.4). Only an example binary read it.
- `Demod`'s private `E_hi / E_lo` keying-rail ratio. Only the derived
  `snr_2500_db()` was public.
- The count `RepetitionGate::record` returns. The validator compared it
  with 2 and discarded it.

## Decisions

- **D1 — four node-level families, no `track_id` label.**
  `manta_decode_chars_total` and `manta_decode_garbles_total` (counters),
  `manta_track_rail_ratio` and `manta_candidate_repetitions` (histograms,
  one observation per track sample or per gate call). Track ids are never
  reused within a session (MAN-19, `events.rs`), the track cap is 1200 and
  MAN-166 measured 886 concurrent tracks on a contest recording, so a
  per-id series would be an unbounded-label family: every closed id stays
  in the TSDB until retention expires, served from a Pi 4. A 15 s scrape
  would also see a short-lived track once or twice, too few points to mean
  anything. The operator's question is whether the node decodes well, which
  a garble rate, a rail-ratio distribution and the share of unrepeated
  candidates answer with bounded cardinality. A per-track view, if wanted
  later, belongs on `/status` as a bounded array, in its own ticket.
- **D2 — a `manta_decode_chars_total` denominator.** A raw garble count
  rises with traffic; with the character count beside it the garble share
  is one PromQL division.
- **D3 — the rail ratio is sampled once per second per track,** at the
  hop each track crosses its SPEC §5 `META_INTERVAL_HOPS` boundary, the same
  hop `TrackMeta` reports on and `drain_pool` already resets the SNR peak
  on. Buckets 2, 2.5, 3, 4, 6, 10, 16, 25, 40, 63 cover the measured range
  (2.0 to about 65) and put the rail-collapse floor in its own bucket:
  `envelope.rs` keeps `E_hi >= 2·E_lo`, so floor samples read exactly 2.
- **D4 — the rail ratio is `legacy`-engine only.** `edge-legacy` and
  `hsmm` have no `Demod` rails; `TrackDecoder::rail_ratio()` returns `None`
  for them and the histogram stays at zero. `hsmm` also never garbles: its
  commits are always a glyph or a word boundary.
- **D5 — repetitions are counted inside `RepetitionGate::record`,** not at
  a validator call site. The validator calls `record` from two places (the
  word path and the pending-beacon path); the probe below first counted
  only the first, and missed V5's single spot, which came through the
  second. `record` wraps a private `record_inner`, so every early return,
  including the 0 for a non-finite frequency, is counted.
- **D6 — one lock-free observer, bridged by the existing poller.**
  `manta_engine::DecodeQualityObserver` is relaxed atomics only (the rail
  sum is an `f64` held as bits and added with a compare-and-swap loop),
  because `observe_rail_ratio` runs on `drain_pool`'s rayon workers. Garbles
  are published as per-track deltas (`Track::garbles_published`) from
  `drain_pool` and from `finish_decoder*`, so a garble the closing flush
  produces is counted too. The validator's repetition counts are already
  cumulative and overwrite the observer's copy after each batch. The
  daemon copies a snapshot into `Metrics` in MAN-128's 250 ms
  decode-latency poller and in its final post-loop refresh; no new task.
  `set_decode_quality` does not feed the decode watchdog, which keeps
  keying off latency.
- **D7 — output is unchanged.** The observer is write-only from the decode
  path. `listen()`, `soak`, `doctor` and the CPU-budget bench pass no
  observer and pay one `Option` check per drained track per batch;
  `listen_with_observers_publishes_decode_quality_without_changing_spots`
  checks the spot list is identical with and without it.

## What the signals look like on the golden vectors

Measured before implementation with a throwaway probe that drove
`TrackManager` over rendered vectors in 1 s chunks and read every decoder
once per chunk:

| Vector | Track-seconds | Rail ratio min / p50 / p90 | At floor (≤ 2.0) | Chars | Garbles | Gate records | reps = 1 |
|---|---|---|---|---|---|---|---|
| V1 clean | 119 | 19.2 / 22.0 / 25.1 | 0 | 139 | 0 | 18 | 2 |
| V4 | 119 | 4.46 / 11.2 / 15.9 | 0 | 181 | 0 | — | — |
| V5 CCIR-poor fading | 119 | 2.28 / 3.28 / 4.43 | 0 | 166 | 0 | (missed, D5) | — |
| V8w pileup + fading | 6039 | 2.00 / 6.80 / 21.0 | 6 | 9690 | 3 | 274 | 116 |
| VR3 high SNR | 119 | 20.7 / 29.6 / 38.7 | 0 | 278 | 0 | — | — |

- **The rail ratio is the strongest of the three.** V5, whose CER gate
  fails under fading, has a median of 3.3 against V1's 22.
- **Garbles are rare.** The beam usually commits a wrong character rather
  than aborting: V5 had none despite failing CER, V8w had 3 in 9,690
  characters, and 50 of V8w's 52 decoded tracks closed with 0. Expect
  `manta_decode_garbles_total` near 0 on a healthy node; it is exported
  because the ticket names it and a sudden rise is a real fault signal.
  For the same reason there is no per-track garble histogram at close.

## Not done

- No `track_id` or frequency label on any family, and no per-track
  `/status` array (D1).
- No change to `DecoderEvent`, the JSON event stream or spot logs.
- No rail ratio for `edge-legacy`/`hsmm` (D4).

See `docs/METRICS.md` for the four rows and `docs/RUNBOOKS/node-health.md`
("Decode quality") for the queries and how to read them.
