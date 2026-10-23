# MAN-120: dedupe edge hysteresis, a beacon fading regression, the σ
invariance pin, and shared frequency-joined CER

Date: 2026-10-23. Ticket: MAN-120 — four small decode/validation findings
from the 2026-09-05 broad review (lens 3, rows #30/#28/#31/#22), bundled
because each is too small for its own PR. Everything here lands in one PR
against `main` @ `5b7fabd18ef47dc9f4ec46b740bc5b2344310d4d`.

## Problem

Four independent gaps, re-grounded against `main` by the research phase
and re-verified by the plan phase (both at the same HEAD):

1. **#30** `Dedupe::bucket` is a bare `(freq/300).round()`: a station
   whose frequency estimate sits on a 300 Hz bucket edge and wobbles 2 Hz
   across it is double-spotted (reproduced: `bucket(14_012_549) ==
   46_708`, `bucket(14_012_551) == 46_709`, the second report emitted).
2. **#28** the NCDXF `<call> T` power-step Beacon heuristic — the one
   spot type exempt from the repetition gate, deferred to `TrackClosed` —
   had zero IQ-level fading coverage.
3. **#31** SPEC §4.3 called σ "the riskiest constant in the spec" and
   demanded validation "against the golden corpus at M3"; it is provably
   decode-invariant (MAN-9 plan decision 5) and measured so twice — the
   mandated sweep would be flat by construction.
4. **#22** `golden_v8_v8w.rs` scored CER against the nearest single
   track while `manta bench sensitivity` (MAN-116 D6) already scored the
   frequency-joined copy — fragmentation could inflate the golden
   numbers (MAN-116's recorded FU-3).

## D1 — same-input dedupe hysteresis (#30)

`Dedupe::assess` now joins a same-input report to the neighbouring
bucket's stored entry when it is in-window and within one channel
(`CROSS_INPUT_MAX_HZ` = 93.75 Hz, one PFB channel) of the stored spot's
frequency, withholding it as `WithheldRepeat` unless the same-input
overrides (+6 dB SNR jump, spot-type change) hold against that entry.

- **Hz-bounded, never bare adjacency**: the join copies the repetition
  gate's MAN-175 anchor rule (`gate.rs:299-314`), which records why bare
  ±1-bucket adjacency is wrong — it joined reports up to ~200 Hz apart
  (13,999,950.1 and 14,000,149.9 round to adjacent buckets). Pinned by
  `same_input_across_edge_more_than_one_channel_away_emits`.
- **No new outcome**: the withheld report is `WithheldRepeat` (MAN-14's
  classification intact), not a new variant; cross-input rules are
  byte-for-byte unchanged (the `across_edge` check still runs first, so a
  neighbour entry from another input is unreachable in the new branch
  in-window within one channel).
- **No stored-state changes**: a withheld report stores nothing, so
  entries move only where today's Emit path already stored and
  `last_sample_ts` (the deferred-beacon peek) is untouched by
  construction. No key movement, no anchor migration — a withheld
  cross-edge report does not rewrite state.
- **Supersession**: MAN-13's D6
  (docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md) deliberately
  kept same-input behaviour unchanged; this record supersedes exactly
  that sliver. The wider tolerance-versus-bucket question for all spots
  stays with MAN-16
  (docs/DECISIONS/2026-09-03-cross-source-dedup-algorithm-spike.md),
  whose cross-source spike is unaffected: cross-input semantics here are
  unchanged.
- One pre-existing pinned assertion flipped with the behaviour:
  `another_inputs_report_across_a_bucket_edge_is_a_duplicate_over_an_expired_entry`'s
  one-input epilogue now asserts the drift-back is withheld (its
  neighbour-bucket entry is in-window), which is the same outcome class
  dedupe already produced for same-bucket repeats; the comment was
  updated to say exactly that.

Six new unit tests pin the scenario, both overrides, the Hz bound, the
expired neighbour, and the wobble-back. The whole `manta-spot` suite
(276 lib + integration) and the full `manta-cli` golden suite pass:
every golden fixture's spot output is byte-identical to `main` (no
fixture's spot reports straddle an edge within 93.75 Hz).

## D2 — NCDXF beacon fading regression (#28)

New `crates/manta-cli/tests/man120_beacon_fading.rs`: a 40 s, 96 kS/s
scene keying "K5ARH T" in a loop at 20 WPM, +10 dB (2500 Hz-ref), offset
+9.1 kHz, under Watterson CCIR-poor (coppa `f8a4d16d`, fixed seed), with
an identical-seed AWGN control, both decoded through `manta decode
--json` (the V34 harness pattern). Three assertions, fixed:

- the fading scene spots K5ARH exactly once as `Beacon` (measured at
  7.46 s; the control at 11.42 s) with **zero** other callsigns of any
  type;
- the control spots the beacon exactly once (detectability floor, so a
  fading failure is the heuristic, not a dead scene);
- the fading is real: joined-track CER 0.3038 (fading) > 0.0127 (AWGN).

**Scene calibration (deviation from the plan, recorded)**: the plan
specified 120 s. At 120 s the continuously-looped ID gives ~50 `<call> T`
windows and, at every measured SNR (10/13/16/19 dB), at least two
deep-fade repeats garble the call into a *distinct but valid* callsign
(NE5ARH at 94.6 s, V5ARH at 110.4 s — element split/merge timing
corruption, SNR-independent) that dedupe's callsign key cannot join and
no repetition gate stops. 30 s fails differently (the WPM-bootstrap
first-cycle garble U5ARH resolves at EOF in both scenes). 40 s is the
measured realistic-duty scene where the heuristic holds. **Open claim**
(needs a follow-up ticket): a continuously-keyed ID under CCIR-poor
fading eventually emits a garbled-ID `Beacon` spot at any SNR — the
exact un-gated risk the ticket named, now demonstrated. The heuristic
itself (MAN-48's guard) is untouched.

Coordination: MAN-94 (RBN `NCDXF B` label/roster, in flight) changes
labelling/identity; this test pins classification stability under
fading. Files overlap only in `validator.rs`'s beacon paths.

## D3 — σ decode-invariance: SPEC corrected, property pinned (#31)

The sweep is refused as a deliverable: it is flat by proof (MAN-9 plan
decision 5 — every hypothesis surviving a beam step has consumed the same
marks, so σ enters each candidate's score as a common positive factor)
and by two measurements (the research phase's CLI matrix: byte-identical
decode text on V1 and V5 across σ 0.10→1.50 with confidence 0.55→0.20;
the plan phase's in-process run at the same HEAD). SPEC §4.3's stale
"must be validated against the golden corpus at M3" mandate is replaced
with the invariance finding, and the property is pinned by the new
`crates/manta-engine/tests/beam_constant_invariance.rs` (2 tests, ~18 s):
σ ∈ {0.10, 1.00} and width ∈ {2, 8} leave the assembled text and the
glyph stream identical to the {4, 0.25} baseline on both an AWGN and a
Watterson-poor 60 s scene. The tests compare text and glyphs, never
full-event equality — confidence fields legitimately move with σ; the
header comment says so to stop a later "fix" from weakening the pin.
Width 1 (measured changing V5's decode) is deliberately not asserted;
the pinned floor is "width ≥ 2 ≡ width 4". SPEC-decode-core-v2.md never
mentions the keys and is untouched. Confidence calibration — σ's one
real effect — stays with lens-3 #21 / MAN-116 follow-ups, out of scope.

## D4 — shared frequency-joined CER (#22, MAN-116 FU-3)

`manta-testkit::cer::joined_cer(expected, tracks, freq_hz, match_hz)`
extracts MAN-116 D6's convention: the texts of every track within
`match_hz` of the station, joined in ascending track_id, empty texts
skipped, raw CER returned (caller keeps its `.min(1.0)`/ceilings). Four
unit tests pin the join order, the empty-skip, the window bound, and the
empty-window CER of 1.

- `bench.rs` `score_point` computes through the helper; its output is
  unchanged — verified against the research phase's recorded table
  (`--conditions awgn,poor --wpm 25 --snr-db 12,20 --duration-s 20
  --seed 7`: CER 0.03 / 0.03 / 0.44 / 0.23, same recall/bogus/spot-SNR
  columns), same HEAD, deterministic seeds.
- `golden_v8_v8w.rs`'s two CER sites (the `#[ignore]`d legacy V8w
  strong-signal gate and its hsmm copy) score through the same helper at
  `CER_JOIN_HZ = 150.0`; `match_tracks_by_freq` is deleted. Number-
  neutral by measurement: post-MAN-108 every measured scene has exactly
  one track within 300 Hz of each strong signal (0/34 with more than
  one), so the join selects the same single track the nearest-pick did;
  the recorded CER list in the caveat comment carries over unchanged,
  and the comment now also records that the scoring is structurally
  immune to fragmentation since this change.
- The optional other half of FU-3 (rewiring the golden tests onto
  in-process decode) is declined: it would churn every recorded number
  for no invariant gain. The invariance the finding wanted —
  fragmentation cannot inflate CER — is now structural.
