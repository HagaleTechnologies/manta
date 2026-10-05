# MAN-100: cross-candidate variant arbitration and a message-aware repetition gate

## The problem (measured)

Under CCIR-poor fading, `manta-spot::Validator` spotted fragments of real
callsigns as if they were separate stations: on a 50-signal pileup fixture
(`manta-testkit::vectors::v8w`), 5 of 27 distinct spotted calls were bogus
(`AB2TTLK` for `AB2KLK`, `K6F` for `K6FXJ`, `W4KTNL` for `W4KCL`, `W6DW`
for `W6DPG`, `W6JQ` for `W6JQA`) — an 18% busted rate, all classified
`Cq`. `confidence::c_call` could not distinguish them: bogus confidence
ranged 0.239–0.339, genuine 0.225–0.614 (median 0.392) — the bogus range
is a strict subset of the genuine range, so no threshold on `c_call` alone
separates them.

Root cause: `Validator::evaluate_candidate` evaluates every context-parse
match in complete isolation (`validator.rs`'s own doc comment on
`candidates()` states this explicitly). Nothing anywhere in the pipeline
ever compares two candidate strings on the same track against each other.
Separately, `RepetitionGate::record` keyed purely on `(track_id, exact
text)` plus a time window, with no concept of a message/transmission
boundary — SPEC's own default payload template repeats a callsign
back-to-back within one transmission (`CQ CQ DE <CALL> <CALL> K`), so a
single corrupted message's two adjacent utterances could satisfy the ≥
2-repetition gate on their own.

## Only 2 of 5 measured failures are literal substrings

| Bogus | Real call, same track | Relation |
|---|---|---|
| `K6F` | `K6FXJ` | strict prefix — containment |
| `W6JQ` | `W6JQA` | strict prefix — containment |
| `AB2TTLK` | `AB2KLK` | shares 3-char prefix `AB2`, Levenshtein 2 — not containment |
| `W4KTNL` | `W4KCL` | shares 3-char prefix `W4K`, Levenshtein 2 — not containment |
| `W6DW` | `W6DPG` | shares 3-char prefix `W6D`, Levenshtein 2 — not containment |

A detector scoped literally to "strict sub- or superstring" (as the
ticket's own Gherkin puts it) catches 2 of 5. `manta-spot::variant`
therefore has two arms: `Containment` (one string is a contiguous
substring of the other) and `NearMiss` (shared prefix ≥ 3 chars, edit
distance ≤ 2). Both thresholds are exact fits to the measured failure
population; they are not tuned margins.

## Truncation vs. merge: why the containment asymmetry is prefix-only

A naive rule ("the longer form of a containment pair always wins") is
wrong in one direction. Classifying every plausible-shaped decoded word in
two 50-signal pileup scenes (V8, V8w) against its track's true callsign:

```
V8w:  183  no containment (near-miss family)
       22  candidate is a strict PREFIX of the true call    (truncation)
        8  candidate is a strict SUFFIX of the true call    (head loss)
        2  candidate is inside the true call
        0  true call is a strict PREFIX of the candidate    (tail merge)
        0  true call is a strict SUFFIX of the candidate    (head merge)

V8:     3  candidate is a strict PREFIX of the true call    (truncation)
        1  true call is a strict SUFFIX of the candidate    (head merge) — "DE" + N3NXI -> DEN3NXI
```

Truncation-by-prefix outnumbers tail-merge 25:0 across both scenes. The
one real merge that occurred was a *head* merge — a framing word ("DE")
glued onto the front of a call, making the real call a **suffix** of the
merged artifact, not a prefix. A blanket "longer wins" rule suppresses the
genuine call in exactly this shape (measured: it held `N3NXI` back for 69s
in V8 behind a 1-rep `DEN3NXI` artifact). Restricting the asymmetry to
"the candidate is a strict *prefix* of the rival" fixes this: a head-merge
candidate is a *suffix*, so the asymmetry never fires for it, and the
ordinary support comparison decides (the genuine call's far larger
repetition count wins on its own merits).

This is why `support::SupportLedger::better_supported_rival`'s containment
check is `text.starts_with(candidate)` (candidate is rival's prefix), not
a bidirectional `contains`.

## Rule-variant measurements

| Rule | V8w bogus | V8w validated | V8 result |
|---|---|---|---|
| baseline (`e398d46`) | 5 | 22/50 | 76 spots, 49/50, 0 bogus |
| support comparison only, no containment asymmetry | 1 (`W6JQ` survives — 3 reps beats its rival's 1, so support alone can't catch a case where the truncated form is itself better-repeated) | 22/50 | byte-identical to baseline |
| support + containment asymmetry, either direction | 0 | 22/50 | 49/50, 0 bogus, but `N3NXI` delayed 69s behind a merge artifact |
| **support + containment asymmetry, prefix-only (shipped)** | **0** | **22/50** | **byte-identical to baseline** |
| + message-aware repetition gate (shipped) | **0** | 20/50 | 51 spots, 49/50, 0 bogus |

Under the shipped rule, the *only* spots removed from V8w are the 5 bogus
ones — verified by diffing the full spot lists end-to-end (`manta decode
--json` on the real V8w fixture), not by comparing counts:

| | spots | distinct calls | validated / 50 | bogus |
|---|---|---|---|---|
| V8w baseline | 30 | 27 | 22 | 5 |
| V8w, this fix | 21 | 20 | 20 | 0 |
| V8 baseline | 76 | 49 | 49 | 0 |
| V8, this fix | 51 | 49 | 49 | 0 |

## Confidence tie-break must be scoped to message-distinct occurrences, not raw decodes

`Support::strictly_better_than` breaks a reps tie on summed per-occurrence
confidence (`conf_sum`). An early implementation summed the confidence of
*every* raw observation in the ledger's 90s window, regardless of the
message-gap collapsing applied to `reps`. This inverted the tie-break for
exactly the measured `W4KTNL`/`W4KCL` pair: `W4KTNL`'s three raw decodes
(two of them from one corrupted message, collapsing to `reps=2`) summed to
a higher raw confidence than `W4KCL`'s two decodes at that point in the
stream, letting `W4KTNL` win a tie it should have lost. `conf_sum` must
sum confidence only for the occurrences that are actually counted toward
`reps` (the first decode of each message), or a candidate whose fading
corruption happens to repeat verbatim several times *within* one message
can out-accumulate a genuinely better-supported rival. Fixed by folding
`reps` and `conf_sum` in one pass using the same greedy message-gap rule
(`support::SupportLedger::support_in_window`). Caught only by an
end-to-end run against the real V8w fixture — the golden vectors (hand-
built `DecoderEvent` sequences) never exercised a candidate with more than
one raw decode per counted message, so they passed both before and after
this fix.

## Message-gap threshold: why 3

SPEC's own default payload template puts one message's two utterances a
single word apart (`CQ CQ DE <CALL> <CALL> K`). The closest two *separate*
messages can put the same callsign is five words apart
(`<CALL> K CQ CQ DE <CALL>`). 3 sits strictly between the two, with
margin on both sides. Measured: gap thresholds of 2 and 3 give identical
outcomes on the V8/V8w reference fixtures; 3 is chosen as the stricter
reading of the ticket's "a second, later message must independently
support the same candidate."

## SCP exemption is not fitted to the test vectors

None of the V8w fixture's 50 callsigns appear in the bundled
`crates/manta-spot/data/master.scp` (checked directly, 0/50 matches). The
SCP exemption in `evaluate_candidate` — a callsign present in the bundled
SCP list is never arbitrated — is therefore provably inert on every
golden vector and the V8/V8w measurements above; it protects real-world
calls without having been tuned to make any measured test pass.

## What this does not fix

- `manta-decode`'s fading-robustness gap (the reason V8w's own CER
  golden test fails and stays `#[ignore]`d) — tracked as MAN-107 through
  MAN-113, unrelated to this ticket's decoder-side root cause per
  `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8. This change
  touches no `manta-decode` code.
- `golden_v8_v8w.rs`'s CER-then-bogus assertion ordering (why the ignored
  V8w test never reaches its own bogus-spot check) — a separate ticket's
  scope; this change does not edit that file.
- `confidence::c_call` calibration — a separate finding from the same
  broad review.

## Implementation

- `manta-spot::variant` (new): `relation(a, b) -> Option<Relation>` —
  `Containment` or `NearMiss`, with `common_prefix_len`/`levenshtein`
  helpers. Symmetric; `None` for a string compared with itself.
- `manta-spot::support` (new): `SupportLedger` — per-track ledger of every
  observed, grammar+cty-plausible decoded word (a form that could never
  itself be spotted must not be able to veto one that could);
  `better_supported_rival` is the arbitration query. Purely additive
  state, purely subtractive effect: it can only withhold a spot the rest
  of the pipeline would have emitted.
- `manta-spot::gate`: `RepetitionGate::record` now takes the resolved
  word's own `word_seq` and counts a greedy chain of occurrences at least
  `MIN_MESSAGE_WORD_GAP = 3` seqs apart, via the shared
  `count_message_distinct` helper.
- `manta-spot::confidence::geo_mean`: extracted from `c_call` so the
  ledger can observe the same per-word quantity `c_call` uses, before its
  repetition factor.
- `manta-spot::validator::Validator`: observes plausible words into the
  ledger on `WordBoundary`, forgets a track's ledger entries on
  `TrackClosed` (mirrors `RepetitionGate::forget_track`, MAN-19), and
  arbitrates between the repetition gate and dedupe in
  `evaluate_candidate`. New `SuppressionCounts::variant` counter
  (ARCHITECTURE §8: every suppression is counted).
- Golden vectors V38 (variant arbitration, plus V38b per-track scoping and
  V38c the head-merge negative control) and V39 (same-message repetition)
  in `crates/manta-spot/tests/golden_v11_v15.rs`. Two pre-existing tests
  whose event sequences relied on the old same-message-counts-as-two
  semantics (`v29_provenance_bound_to_exact_word_occurrence_across_repetitions`,
  `cq_call_with_trailing_t_spots_once_as_cq_not_beacon`) were updated to
  source their second repetition from a genuinely separate message —
  their original subject (provenance binding; the CQ/DE power-step guard)
  is unchanged. V40-V42 (arrival-order, short-ID time gap, beacon exemption)
  added in the first remediation round; V43 (short-ID ordinary-cadence
  acceptance) added in the second.

## Risks and how each is bounded

- **False suppression of a genuinely distinct co-channel station** whose
  call happens to be confusable with another station's on the same track
  within 90s. Bounded by the SCP and allowlist exemptions, and by the
  ticket's own stated priority (false spots de-list a node; low recall
  does not). Measured cost on the only two multi-signal scenes available:
  zero on both V8 and V8w.
- **The message-gap rule delaying or dropping a single-message-only
  spot.** Measured on V8w: two calls (`W7ICA`, `W8SHR`) that were
  previously spotted on one message's adjacent double utterance alone,
  with no second message supporting them in the scene, are no longer
  spotted at all — accepted, since that is exactly the evidence the
  ticket says must no longer be sufficient. Three other calls are merely
  delayed 9–14s and re-spot at higher confidence once a second message
  arrives.
- **Ledger memory.** Same shape and same `TrackClosed` teardown as
  `RepetitionGate::seen` (MAN-19); bounded per track, freed on close.

## Remediation round (validate-plan code-review, 2026-09-07)

The first validate-plan pass reproduced the headline V8w/V8 numbers above
end to end, but code review found three confirmed correctness gaps the
shipped rule and its golden vectors didn't cover. All three are fixed in
this same PR, re-verified against V8/V8w with the replay harness (both
scenes byte-for-byte unchanged: V8w 21 spots/20 distinct/0 bogus, V8 51
spots/49 distinct/0 bogus — identical to the numbers measured above), and
pinned with golden vectors V40–V42 (`docs/SPEC-decode-core.md` §7.1).

- **C1 — the prefix asymmetry was one-directional.** `longer_containment`
  only fired when the *rival* was the longer form; nothing stopped a
  shorter truncation from winning `better_supported_rival` on raw support
  when arbitrating the *longer, genuine* candidate (a truncation that
  simply arrives first and reaches 2 reps before the real call has any
  support could suppress the real call forever after — measured: `CQ DE
  W6JQ K` ×3 then `CQ DE W6JQA K` ×2 on one track spotted only the
  truncation). Fixed by also skipping a rival that is a strict prefix of
  the candidate (`support.rs`'s `shorter_prefix_of_candidate` arm),
  symmetric with the existing `longer_containment` arm and still
  prefix-only for the same reason: a head-merge rival (`DEN3NXI` vs
  `N3NXI`) is a suffix relationship, so this arm never touches it (V38c
  stays green). This does not (and architecturally cannot) retroactively
  revoke a spot the truncation already emitted before the genuine call
  was ever observed — V40 documents that scope boundary explicitly.
- **C2 — `count_message_distinct` never consulted `sample_ts`.** A
  two-word ID (e.g. `DE <CALL>`) puts the callsign only 2 word_seqs apart
  across genuinely *separate* transmissions, below
  `MIN_MESSAGE_WORD_GAP = 3`, with no time-based escape hatch (measured:
  `DE K5ARH` ×10 at 80s spacing over 13 minutes never spotted at all).
  Fixed by adding `MIN_MESSAGE_TIME_GAP_SECONDS = 60` as an OR condition —
  two occurrences now count as separate messages when either the
  word_seq gap clears `MIN_MESSAGE_WORD_GAP` or the `sample_ts` gap
  clears 60s (60s comfortably covers a full "CQ CQ DE `<CALL>` `<CALL>`
  K" transmission even at 8 WPM, SPEC's slowest supported speed, ~40s for
  that template, while staying under the 90s ledger/gate window). The
  greedy chain logic is now a single shared helper
  (`gate::message_distinct_indices`) that both `RepetitionGate::record`
  and `support::SupportLedger::support_in_window` call, closing a related
  low-severity finding (C4) that the two had drifted into independent
  reimplementations of the same rule.
- **C3 — step 4b had no beacon exemption.** `SpotType::Beacon` is exempt
  from the repetition gate (ARCHITECTURE §6.4, MAN-28) because an
  NCDXF-style beacon legitimately IDs once per cycle — but arbitration
  didn't carry the same exemption, so a beacon's structurally low rep
  count let any confusable, fading-corrupted rival with more reps
  permanently outrank it (measured: `V V V W6DPH K` ×2 then `V V V
  W6DPG K` ×1 spotted only the corrupted `W6DPH`, silently displacing the
  genuine once-per-cycle beacon). Decided explicitly (this was previously
  an oversight, not a decision — neither the plan, this decision record,
  nor any test considered beacons under arbitration): `SpotType::Beacon`
  is now exempt from step 4b, alongside the allowlist/SCP exemptions,
  accepting the same bounded tradeoff already accepted for those two — a
  confusable variant of an exempt call may occasionally spot
  unarbitrated, in exchange for never letting this rep-count heuristic
  (tuned for calls that must clear ≥ 2 reps to spot at all) systematically
  displace a true beacon spot with a false one.

## Second remediation round (validate-plan attempt 3, 2026-09-07)

The second validate-plan pass found the first remediation round's own golden
vector for Scenario 1 (V38) was non-discriminating (it passed unchanged on
`e398d46`, because `context::parse`'s single-match-per-window `DE_RE` never
let "K5AR" surface as its own candidate in that event sequence), plus three
further code-review gaps in the shipped mechanism.

- **F1/F2 — V38 was vacuous.** Fixed by applying the same 16-word
  filler-aging technique V40/V42 already use: age the well-supported
  genuine call's earlier context match fully out of the window before the
  truncation is ever decoded, so the truncation reaches its own 2 reps
  (clearing the bare repetition gate on its own -- confirmed red on
  `e398d46`, green on this branch) and is only then arbitrated against the
  already-observed genuine call.
- **C5 — `longer_containment` had no support floor.** The prefix-shape
  override let a rival win `better_supported_rival` on shape alone with as
  little as 1 observation, however badly supported. Measured (a hand-built
  scenario, not the real V8w fixture): a single garbled "K5ARHT" (a stray
  trailing "T" glued onto the real call) vetoed a 3-rep "K5ARH" outright,
  with nothing spotted in its place -- the mirror image of the head-merge
  case this same file already protects against in the other direction.
  Fixed by requiring the rival to also clear
  `MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE = 2` (`support.rs`) -- the same floor
  every other spottable candidate must itself clear.

  **This claim was wrong and the fix was reverted in round 3 below.** This
  paragraph originally asserted "all five of this ticket's
  originally-measured truncation cases had multi-rep rivals, so this floor
  does not move them" -- contradicted by this very document's own §4
  measurement table, which records `cand=W6JQ (reps=3) rival=W6JQA
  (reps=1)` for track 90. The claim was never checked against that table
  before shipping, and the round that added this floor also skipped
  re-running the real V8w fixture end to end (see that round's own closing
  note below) -- the combination let a floor that reopens the ticket's own
  headline case land unnoticed. See "Third remediation round" below.
- **C6 — `SupportLedger::seen` keys never expired.** `observe` only pruned
  the one entry it had just touched; a text observed once and never again
  kept its key (and its now-stale observations) forever, freed only by
  `forget_track` on `TrackClosed` -- which a long-lived track, exactly the
  24 h-soak shape MAN-19 exists for, never triggers mid-run. Fixed by
  sweeping every `(track_id, *)` entry whose newest observation has aged
  out of the window on every `observe` call, bounding both the per-track
  key count and the O(keys) cost `better_supported_rival` pays per
  candidate evaluation to what's currently live, not to track history.
- **C2, quantified — accepted, not tightened.** A short "DE `<CALL>`" ID
  repeated at ordinary (sub-60 s) cadence regresses under the message-gap
  rule: a station that IDs exactly twice, 20 s apart, is not spotted at
  all (a station that keeps IDing is merely delayed one transmission, since
  the greedy chain eventually clears the 3-word gap across occurrences 1
  and 3). Not tightened: any `MIN_MESSAGE_TIME_GAP_SECONDS` low enough to
  rescue a 20 s cadence would also treat a single corrupted "CQ CQ DE
  `<CALL>` `<CALL>` K" message's own doubled utterance -- typically only a
  few seconds apart -- as two distinct messages, reopening the exact hole
  this rule exists to close. Pinned as accepted, current behaviour by V43
  rather than left unquantified.

Not re-run against the real V8w/V8 fixtures this round (same constraint the
first validate-plan pass recorded: the fixture-dump step is uncommitted
scratch code and the container lacks the disk/time for a fresh release
decode) -- the "does not move the measured cases" claims above are
analytical, not re-measured: C5's floor (>= 2 rival reps) is already
cleared by all five originally-measured truncation cases per the first
round's own analysis, and C6 only removes ledger entries that have already
aged past the 90 s window `better_supported_rival` itself reads, so it
cannot change any live comparison's outcome. Confirmed instead by the unit
and golden-vector suites in `support.rs` and `golden_v11_v15.rs`, including
new vectors for both fixes.

## Third remediation round (validate-plan attempt 5, 2026-09-07)

The third validate-plan pass re-measured V8w end to end with a base-commit
control and found C5's floor (`MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE = 2`,
added in round 2 above) regressed the ticket's own headline case:
`spots=22 distinct=21 validated=20/50 bogus=1 ["W6JQ"]`, against the base
commit's control `bogus=5` and this branch's claimed `bogus=0`. 4 of 5
bogus spots were genuinely fixed; `W6JQ` was not, because its rival
`W6JQA` has only 1 message-distinct repetition on the track at the time
`W6JQ` is arbitrated (`W6JQA`'s own second exact-text occurrence on track
90 arrives 51 words later, after `W6JQ` has already reached 3 reps and
been evaluated -- see the research document's word-position analysis).
`MIN_RIVAL_REPS_FOR_SHAPE_OVERRIDE = 2` excludes a 1-rep rival by
construction, so the shape override never fired for this pair and the
plain support comparison (1 rep vs. 3) could not save it either.

**The floor is reverted.** `better_supported_rival`'s `longer_containment`
arm now fires for any rival that has been observed at all (the pre-existing
`s.reps == 0` guard still excludes a rival with zero observations) --
exactly the pre-round-2 rule. This is not a partial fix: the C5 scenario
("K5ARHT", a lone glued-tail artifact overriding a well-supported "K5ARH")
and the ticket's own "W6JQ"/"W6JQA" case are the **same shape** --
a well-supported short candidate, a textually-longer prefix-extension
rival with exactly 1 observed repetition, containment relation -- and nothing
in the ledger (reps, summed confidence, or observation timing) tells them
apart: both are "candidate reps > 1, rival reps == 1, prefix-containment."
No numeric floor on the rival's own rep count can admit one and exclude the
other. Between an unmeasured, hand-built scenario (`K5ARHT` appears in no
real fixture, no golden vector, and no measured pileup scene) and the
ticket's own real, measured, Gherkin-cited regression, the real one wins:
per this ticket's own priority ("false spots are what gets a node
de-listed from RBN, not low recall"), and because the round-2 fix was
never re-verified against real data before landing (see its own closing
note: "not re-run against the real V8w/V8 fixtures this round").

Re-measured end to end against the real V8w fixture after the revert:

| | spots | distinct | validated/50 | bogus |
|---|---|---|---|---|
| V8w baseline (`e398d46`, control) | 30 | 27 | 22 | 5 |
| V8w, this branch after the round-3 revert | 21 | 20 | 20 | **0** |

Matches the ticket's own acceptance criterion and this document's original
headline table exactly. `crates/manta-spot/tests/golden_v11_v15.rs`'s
`v44_the_measured_w6jq_w6jqa_shape_a_1_rep_rival_still_wins_by_shape` pins
this literal shape as a golden vector (`docs/SPEC-decode-core.md` §7.1,
V44) so it cannot silently regress again; `support.rs`'s
`a_strict_prefix_loses_to_a_longer_form_even_with_more_reps` unit test is
restored to the plan's original 1-rep-rival spec, and a new
`a_lone_single_observation_rival_still_overrides_by_shape` unit test
documents the accepted trade-off explicitly (a lone, single-observation
confusable rival now *does* override by shape) rather than leaving it as
an implicit side effect of the revert.

**Accepted, documented trade-off going forward:** a genuinely spurious,
never-repeated decode that happens to be a textual prefix-extension of a
well-supported real call can suppress that call. This has zero measured
occurrences across both available multi-signal fixtures (V8, V8w). If it
is ever measured in practice, the fix is not another numeric floor on
`s.reps` (proven insufficient by this round) -- it needs a signal the
ledger doesn't currently carry (e.g. the rival's own per-character
confidence, or corroboration from a second, independent detection
mechanism), which is out of scope for this ticket.

Findings 3, 4, 5, and 6 from the same validate-plan report (ledger not
gated on the operator blocklist; the 90 s ledger window vs. the 16-word
context window at 8 WPM; the prefix-only asymmetry's untested tail-merge
exposure; the untested SCP exemption) are all rated PLAUSIBLE, not
CONFIRMED, and none is reproduced against the real fixtures or a committed
test. Per `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`
("from round 2 onward, P2-and-lower findings are not fixed inline -- they
are captured verbatim into a follow-up ticket instead"), this is round 3:
each is recorded here verbatim for a follow-up ticket rather than
addressed inline in this PR.

- Finding 3: `Validator`'s `WordBoundary` handler
  (`crates/manta-spot/src/validator.rs:333`) observes any grammar+cty
  plausible word into the ledger without checking `self.blocklist` --
  `Blocklist` is consulted only in `evaluate_candidate`, not on the
  observe path. A blocklisted text can still accumulate ledger support and
  win `better_supported_rival` against a genuine candidate, silently
  costing the operator their real spot on a track where the blocklisted
  variant out-reps it.
- Finding 4: `support_in_window` (`support.rs`) and the 16-word
  `WORD_WINDOW` context match live on different clocks -- a word can stay
  eligible for `context::parse` far longer than the ledger's 90 s window
  at slow speeds (8 WPM), so a late-resolved candidate can be evaluated
  against a ledger where its own earlier support has already aged out.
- Finding 5: the prefix-only `longer_containment` asymmetry (by design)
  never fires for a tail-merge shape (true call is a strict prefix of the
  rival, e.g. a merged trailing "K" glued onto a real call). Measured data
  shows 0 tail-merge occurrences across both fixtures against 25
  truncations, so this is a bounded, reasoned risk, not a demonstrated
  defect -- but it is unmonitored.
- Finding 6: `scp_exempt` (`validator.rs:770`) is not exercised by any
  committed test; every golden vector constructs `Validator::new(..., None)`.
  `master.scp` contains short real calls that are also plausible
  truncations of longer real calls, so this is a live, untested production
  path.

## References

- Ticket: MAN-100.
- Origin: 2026-09-05 broad review, lens 3, hit-list item #1.
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D6 (Pi4 pause
  pending MAN-100 through MAN-113), D8 (classical-DSP fixes before M4).
- Code: `crates/manta-spot/src/variant.rs`, `crates/manta-spot/src/support.rs`,
  `crates/manta-spot/src/gate.rs`, `crates/manta-spot/src/validator.rs`.
- Vectors: `docs/SPEC-decode-core.md` §4.6, §7.1 (V29, V38, V39, V40–V43);
  `crates/manta-spot/tests/golden_v11_v15.rs`.
- Replay harness: `crates/manta-cli/examples/replay_spots.rs`,
  `wiki/pages/replay-spots-harness.md`.
