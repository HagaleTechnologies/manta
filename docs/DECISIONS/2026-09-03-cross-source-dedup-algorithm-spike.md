# 2026-09-03 — Cross-source spot-dedup algorithm spike (MAN-53)

**Status:** accepted (investigation only — no implementation in this doc or its PR).

## Scope and method

MAN-16 (cross-source spot dedup, blocked on this ticket) already proposes a
concrete algorithm in its own technical notes: match on **same DE (reporting
node) + same DX (spotted callsign) + same frequency, within roughly a
10-minute window**. That figure traces to a single forum comment attached to
MAN-15's research thread, not to either third-party tool's own documentation.
MAN-53 exists specifically to close that gap: validate (or correct) the
proposed algorithm against SkimCon's and RBNSpotNormalizer's *actual*
published logic, and stress-test it against the three edge cases MAN-16's own
technical notes name — frequency drift across sources, near-simultaneous
distinct callsigns close in frequency, and multi-mode same-callsign spots.

Method, matching MAN-10's spike precedent (`docs/DECISIONS/2026-09-02-hpsdr-hermes-protocol-spike.md`):
sourced-only research, every concrete claim cited to a URL, with unverifiable
items in their own explicit section rather than guessed at. Sources this
time include primary-author testimony (not just third-party docs) — the
RBN-OPS groups.io archive turned out to have the tool author himself
explaining his own algorithm in his own words, which is stronger evidence
than the marketing page alone.

## Decision

**MAN-16's proposed DE+DX+frequency+~10min algorithm's overall shape and
order of magnitude are confirmed against real, independently-converging
external implementations — but a review round on this doc, checking it
against manta's own already-existing `Dedupe` code
(`crates/manta-spot/src/dedupe.rs`), found several concrete integration
questions that need explicit answers before implementation, not just
before this spike closes.** An earlier draft of this doc understated that —
it validated the algorithm against external tools without also checking it
against manta's own existing single-pipeline implementation, and one of its
two original "corrections" was itself wrong for exactly that reason
(retracted below, not silently edited away).

### 1. SkimCon — confirmed via primary source (tool author, not just docs)

[SkimCon — DF7GB](https://df7gb.de/skimcon.html) (product page) plus, more
authoritatively, the tool's own author explaining his matching logic in his
own words in the RBN-OPS announcement thread
([groups.io/g/RBN-OPS/topic/skimcon_a_new_app_that/97947012](https://groups.io/g/RBN-OPS/topic/skimcon_a_new_app_that/97947012),
message #6620, Günter DF7GB, 3/31/23):

> "What is a dupe spot. Dupe spots contain same DE call sign incl. SSID, same
> DX call sign and same frequency in a time range, let's say, up to 10
> minutes."

This is **exactly** MAN-16's proposed algorithm, confirmed by the actual
tool author, not inferred from a marketing page or a secondhand forum
paraphrase. Parameters:

- **Time window:** up to 10 minutes, configurable per output, and DF7GB
  himself later revises his own recommendation *downward* in the same
  thread (message #6640) after a community member's objection (see
  "Correction 2" below): "The time range should be less than 10 minutes...
  9 or 9 and a half minutes should be used." The product page's stated max
  is "9 minutes 50 seconds," consistent with this.
- **Frequency tolerance:** configurable, "100Hz or 200Hz or whatever you
  want" per a long-time user (Max NG7M, message #6623); the product page's
  stated max is ±300 Hz. Real-world calibration-error context: ~100 Hz
  error at 160m is ~60 ppm (Björn SM7IUN, a separate but relevant thread —
  see "Frequency drift" below); the community's own accuracy target is
  roughly 10 ppm or better per node.
- **DE/reporting-station scope:** cross-instance — SkimCon sits in front of
  multiple Skimmer/SkimSrv *processes run by one operator* (not across
  independent RBN nodes) and dedupes before that operator's single
  consolidated feed reaches Aggregator/RBN.
- **Real observed inter-arrival time of genuine duplicates:** DF7GB posted a
  screenshot (message #6640) of an actual duplicate pair "sent 0.702 seconds
  after the first one." This matters for MAN-16's implementation: two
  receivers hearing the *same live transmission* decode and report it within
  roughly a second of each other in practice, not minutes apart. The 10-minute
  figure is not really about catching this fast-arriving case — a much
  shorter window would already catch it — see "Correction 2" for what the
  long window is actually protecting against.

**Mode scope, confirmed:** SkimCon runs **three independent outputs, one per
mode** (CW/RTTY/PSK) — "the three outputs can be used individually for each
mode" (df7gb.de) — because "the aggregator does not eliminate CW, RTTY and
PSK dupes" on its own. Mode is an implicit hard partition, never a
cross-mode match. MAN-16's own technical notes don't name mode as a match
field at all (reasonable, since manta is CW-only per README's Non-goals),
but this confirms the omission is currently correct by construction, not an
oversight — worth stating explicitly if/when manta ever adds another mode,
so a future implementer doesn't have to rediscover it.

### 2. RBNSpotNormalizer — not found; treat as unconfirmed, not as a data point

Extensive search (GitHub, general web, groups.io, reversebeacon.net,
dxatlas.com, ham radio forums) turned up **no evidence this tool exists**
under this name or any close variant. No repository, no announcement
thread, no changelog, no forum mention. Two caveats on completeness: a
direct GitHub code-search hit a rate limit rather than returning a clean
zero-result page, and groups.io message bodies (as opposed to indexed
thread titles) aren't fully covered by general web search. Neither
plausibly hides a well-known, actively-used tool of this exact name,
though.

**Recommendation:** MAN-16 (and any future reference to this ticket) should
stop citing "RBNSpotNormalizer" as a second independent data point — the
research this session could find supports exactly one third-party tool
(SkimCon), not two. The likeliest explanation is that "RBNSpotNormalizer"
is a misremembering of SkimCon itself, of the older **WintelnetX** tool
(described below), or of the ArCluster **"Spot Close Dupe Filter"**
(`AR-Cluster` command reference, cited by a community member in the
frequency-calibration thread: "Rejects spots with the same call spotted
within the last x minutes within a designated frequency spread" — the
same DE+DX+frequency+time shape again, this time as a downstream
cluster-side filter rather than an upstream combiner).

### 3. WintelnetX — a second real, independently-corroborating data point

Not originally named in MAN-16, but found via research and directly
relevant: [Reverse Beacon: Frequency Calibration and the RBN, N4ZR blog,
Dec 2010](http://reversebeacon.blogspot.com/2010/12/frequency-calibration-and-rbn.html)
describes WintelnetX (an older, separate combiner tool, predating SkimCon)
as "the most popular deduplication software" at the time, matching **"only
... duplicate spots that are on the same frequency (to the nearest 100
Hz)."** This independently corroborates the same-shaped algorithm (exact
callsign + frequency-bucket match) at a tighter, fixed 100 Hz tolerance,
from a different tool with a different author, years before SkimCon
existed. Two independent implementations converging on the same match
shape (callsign + frequency-with-tolerance + short time window) is
stronger validation than either alone.

## Corrections to MAN-16's proposed algorithm

### Correction 1 — frequency drift across *manta's own* sources is a narrower, more tractable problem than the wider RBN's cross-node drift problem

The [Frequency Calibration Duplicate
Spots](https://groups.io/g/RBN-OPS/topic/frequency_calibration/75613144)
thread (2020, 45 messages) is entirely about **independent RBN nodes around
the world**, each with its own uncorrected calibration error, producing
near-frequency near-duplicates of the *same station* as seen by *different
operators' hardware*. This is explicitly **unsolved** at the RBN
Aggregator level as of that thread — Pete N4ZR (RBN's own maintainer)
responds "I see your problem... What I don't know is what to do about it,"
and the community's actual mitigations are all client-side (N1MM+/DXLog/
Win-Test replacing displayed frequency with "last heard" rather than
reconciling multiple reported frequencies).

**This is not MAN-16's problem** in the sense of the *wider RBN's* unsolved
cross-node case. MAN-16 is about dedup *within* one manta daemon, across
*manta's own* configured sources — not across independent RBN nodes manta
doesn't control. Manta already has its own answer to inter-source
calibration drift for sources that share a common RF reference: **MAN-29**
(per-source frequency-calibration correction factor, from the legacy
capability matrix, `docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`).
A reasonable design conclusion: **MAN-16's dedup should assume its inputs
are post-MAN-29-calibration**, and a modest fixed tolerance matching
SkimCon's real-world-validated default (100–300 Hz, not a ppm-scaled
figure — nothing in either source ties the tolerance to band/frequency) is
appropriate, without needing the harder cross-node reconciliation
(voting/median-based frequency estimation) the wider RBN community still
hasn't solved.

**Correction (review round 1, chatgpt-codex-connector): MAN-29 alone is
not sufficient for every source pair.** MAN-29 only *scales/corrects* an
already-RF-referenced frequency value — it has nothing to put a source
into a shared frequency coordinate system with if that source has no RF
reference at all. `AudioIqSource::center_freq_hz()` always returns `0.0`
(confirmed in the capability matrix as **MAN-34**'s gap, not MAN-29's) — an
audio-input source reports baseband tone frequencies, not absolute RF Hz.
Cross-source frequency matching between an audio source and any
RF-referenced source (SDR/HPSDR/Kiwi) is **not possible at all** until
MAN-34 lands, regardless of MAN-29's calibration correction. **MAN-16
should either explicitly depend on MAN-34 for audio-source participation,
or explicitly exclude unreferenced audio sources from cross-source dedup**
— this needs a stated decision, not an assumption that MAN-29 alone
resolves inter-source frequency alignment for every source type.

### Correction 2 — retracted: manta's own `Dedupe` already IS the mechanism the community's ~10-minute figure describes; there is no external jitter to route around

**This correction from an earlier draft of this doc was itself wrong** (caught
in review round 1) — retracting it here rather than silently editing it away,
since the error is instructive. The original text recommended narrowing
MAN-16's proposed window to "~9.5 minutes, under Skimmer's own jittered
respot timer" by analogy to DF7GB's own revised SkimCon guidance. That
guidance is about **SkimCon sitting downstream of a separate, independent
Skimmer Server process** whose own internal ~10-minute self-respot timer has
observed jitter — SkimCon's tolerance exists to avoid double-suppressing
around that *external* jitter.

**Manta has no such external upstream to jitter against.** Reading
`crates/manta-spot/src/dedupe.rs` (not consulted before the earlier draft —
this doc's own research gap): manta already has a single-pipeline `Dedupe`
with `SUPPRESSION_SECONDS = 600` (exactly 10 minutes) — manta's `Dedupe` *is*
the re-spot timer, not a downstream consumer of someone else's. There is no
separate, independently-jittered upstream process for a shorter window to
leave headroom under. Shortening `SUPPRESSION_SECONDS` to 9.5 minutes
would not prevent the 20-minute-gap failure mode DF7GB was addressing (that
failure mode is specific to *two independent* respot timers interacting) —
it would simply make manta itself re-spot ongoing CQ callers slightly more
often, for no corresponding benefit.

**Corrected conclusion: keep `SUPPRESSION_SECONDS = 600` as-is.** The
~10-minute figure is still well-corroborated (SkimCon, WintelnetX, and
manta's own existing independent implementation all converge on the same
order of magnitude for the same underlying purpose — bounding repeat-CQ
spot volume) — there is just no manta-specific reason to shave it down, and
this doc should not have recommended doing so without checking the
code its recommendation was actually about.

### Correction 3 — manta's existing frequency matching is hard bucketing, not the tolerance/distance comparison SkimCon and WintelnetX actually use

Also caught in review round 1, also traced to not having read
`dedupe.rs` before the earlier draft. `Dedupe::bucket()` computes
`(freq_hz / 300.0).round()` and compares *bucket identity*, not distance:
two frequencies a few Hz apart that happen to straddle a bucket edge (e.g.
149.9 Hz and 150.1 Hz relative to a bucket boundary) land in *different*
buckets and are treated as non-duplicates, while two frequencies up to
~300 Hz apart that happen to fall in the *same* bucket are treated as
duplicates. This is a materially different match shape from SkimCon's
(±100–300 Hz, configurable, read as an absolute-distance tolerance around
each observed frequency, no fixed grid) or WintelnetX's (round to the
nearest 100 Hz, which is *also* a bucketing scheme, but a finer one than
manta's 300 Hz default). Calling manta's 300 Hz bucket width "validated
against SkimCon's ±300 Hz tolerance" (as an earlier draft of this doc did)
overstated the equivalence — same rough magnitude, different mechanism,
different boundary behavior.

**This is a decision MAN-16 needs to make explicitly, not a bug this spike
can fix:** keep the existing bucket-based approach (simple, already
implemented and tested, existing boundary-crossing false-negative behavior
already accepted for the single-pipeline case) and extend it as-is to
cross-source matching, or move to true absolute-distance matching (closer
to what the validated external tools do, no boundary artifacts, but a
materially different data structure than the current `BTreeMap<(String,
i64), LastSpot>` — a distance query isn't a simple key lookup). Either is
defensible; leaving the distinction unstated, as the earlier draft did, is
not.

## Integration gaps with manta's existing `Dedupe` (found in review round 1)

Two more findings from reading `crates/manta-spot/src/dedupe.rs` against
MAN-16's scenario, neither addressed by the earlier draft of this doc:

### `sample_ts` has no shared unit or epoch across sources

`Dedupe::new(fs: f64)` takes exactly one sample rate, and `should_emit`
compares raw `sample_ts: u64` values directly — correct for today's
single-pipeline case (one source, one `fs`), but MAN-13 combining sources
at *different* sample rates (e.g. 48 kHz rig audio alongside 192 kHz SDR
IQ) breaks that comparison outright: the same wall-clock instant is a
different raw sample count on each source, with no common unit and no
guarantee of a common epoch (start-of-stream) either. Comparing them
directly would suppress or emit duplicates depending on which source
happens to report a numerically larger `sample_ts`, not on real elapsed
time.

**This needs a normalized shared time basis before cross-source windowed
matching is implementable**, not just a wider `Dedupe`. `SpotBus` already
has to solve an adjacent version of this problem for display purposes
(`unix_ts_for(sample_ts)`, seen in the acceptance tests this session
touched in MAN-22/23's work) — MAN-16 should specify whether cross-source
comparison happens on a similarly-converted absolute timestamp (unix time
or equivalent), and if so, confirm that conversion's own precision is fine
grained enough for a same-instant-duplicate check (SkimCon's real observed
duplicates arrived ~0.7s apart — a conversion that only has, say, 1-second
resolution could still work for the 10-minute suppression window but would
be too coarse to reason about the sub-second same-instant case explicitly).
Process wall-clock arrival time (`Instant::now()` at the point a spot is
produced) is **not** an acceptable substitute — manta's file-replay path
must stay deterministic (`SPEC-decode-core.md` §6's `sample_ts`-based rule,
cited in `dedupe.rs`'s own module doc), and wall-clock arrival time isn't
reproducible from a recorded file.

### Cross-source matches need a stated interaction with the existing SNR/type override

`Dedupe::should_emit` already has override logic for the single-pipeline
case: a suppressed-by-window spot still emits if the new occurrence's SNR
improved by ≥6 dB (`SNR_IMPROVEMENT_DB`) or its `SpotType` changed. MAN-16
doesn't currently say whether a **cross-source** match should honor these
same overrides, and the answer isn't obvious either way:

- Honoring them (a second source's report with meaningfully better SNR
  still emits) is arguably the *more correct* choice, and it directly
  connects to the design question below — a better-SNR report from a
  different receiver/antenna is exactly the kind of signal-diversity
  information N6TV/SM7IUN's objection (below) says shouldn't be silently
  thrown away.
- Suppressing unconditionally on any cross-source match (never emit twice
  for the "same" transmission regardless of SNR) is simpler but reproduces
  exactly the data-loss the community's downstream-not-upstream argument
  warns against — two receivers with different gains would keep
  duplicating today, `should_emit`'s override or not, if cross-source
  matching bypasses it, since manta's own real-world receiver-count-1 case
  is the common one this override was built for in the first place.

MAN-16 needs to pick one (or a third option) explicitly — this spike
surfaces the gap but the choice depends on the same upstream-vs-downstream
question the next section covers, so it shouldn't be decided in isolation
from that one.

## Design question this spike surfaces but does not resolve

### Should manta dedup its own multi-source spots upstream at all?

This is the most consequential finding from this research, and it's a
**design decision for MAN-16 to make explicitly, not something this spike
can settle** — MAN-53's own scope is validating the matching algorithm, not
re-litigating MAN-16's premise.

The SkimCon announcement thread contains a substantive, unresolved
technical objection from experienced RBN community members (Bob Wilson
N6TV, Björn SM7IUN — both recognized subject-matter experts in this exact
space) against upstream consolidation in general:

> "If you have three skimmers online skimming the same bands with three
> different SDRs and/or three different antennas from the same location,
> all spots should be posted by three spotter callsigns like DF7GB-1,
> DF7GB-2, and DF7GB-3... Otherwise, if SkimCon just 'picks one' and
> ignores the duplicate spots from the other skimmers, there is no longer
> any consistency in the signal strength being reported... spots from
> different physical receivers should NEVER be posted under the same
> callsign/SSID." (N6TV, message #6617; SM7IUN concurs, message #6618)
>
> "De-duping and consolidation of downstream spots can be done freely
> since the information loss only affects the cluster node and its
> users. Not the entire community... there is no technical need for
> additional software to consolidate multiple physical receivers under
> one identity." (SM7IUN, message #6619/#6627, paraphrased/combined)

The community's stated best practice is: **dedup downstream (at the
consumer/display layer), never upstream (before spots reach the shared
network)** — because upstream consolidation silently discards
signal-strength diversity across physically distinct receivers/antennas
that has real value to the wider RBN community's propagation statistics,
whereas downstream dedup only affects the local consumer's own convenience.

**Why this may or may not actually apply to manta**, and why it needs a
real decision rather than either blind adoption or blind dismissal:

- MAN-16's own scenario (two overlapping *manta-configured* sources hearing
  the same station) is structurally the exact case N6TV/SM7IUN object to —
  consolidating multiple receivers behind one reported identity before the
  spot reaches manta's own telnet/JSON output (MAN-12), which other tools
  (cqdx, DX cluster clients) then treat as ground truth.
- Countering that: manta is a **single daemon reporting as one logical
  station** (one MAN-12 telnet/JSON identity), architecturally closer to
  "one physical multi-DDC skimmer" (uncontroversial — nobody objects to a
  single 8-DDC Red Pitaya reporting under one callsign) than to "combining
  N independently-operated skimmer processes" (the actually-controversial
  case in the thread, which involves genuinely separate hardware chains
  an operator chose to run as separate processes). Where exactly manta's
  actual deployment shape (one daemon, N sources, possibly N antennas)
  falls on that spectrum is an open question this research can surface but
  not answer — it depends on real deployment patterns MAN-13 hasn't
  shipped yet to observe.
- A middle path exists and has real precedent in the same thread: expose
  per-source identity (SSID-style, e.g. distinguishing which configured
  source produced a spot) rather than either fully consolidating or not
  deduping at all — this is explicitly what N6TV/SM7IUN recommend as the
  *correct* way to combine receivers (each gets its own spotter
  callsign/SSID), and MAN-14 ("cqdx and other spot consumers should be
  able to tell which physical receiver or segment produced a given manta
  spot") already exists in the backlog and may already be the intended
  answer to this — worth cross-referencing explicitly when MAN-16 is
  picked up.

**Recommendation:** MAN-16's implementation should explicitly address this
question (even if the answer is "we consolidate anyway, and here's why
manta's single-daemon-single-identity case differs from the
multi-operator-multi-process case the RBN community objects to") rather
than silently proceeding as if the objection doesn't exist. This doesn't
block MAN-16 on its own — but combined with the integration gaps above
(shared time basis, bucket-vs-tolerance decision, override interaction,
MAN-34 dependency for audio sources), the matching *algorithm's overall
shape* is validated while several concrete implementation questions remain
genuinely open, not just this one. The *decision to consolidate at all*
(versus relying on MAN-14's per-source attribution instead, or offering
both as a config choice) deserves a stated rationale in MAN-16's own spec,
not silence — same as the others.

## Answers to MAN-16's three named edge cases

1. **Frequency drift across sources** — addressed by Correction 1 above:
   not the wider RBN's unsolved cross-node problem; scoped to manta's own
   *RF-referenced* sources (MAN-34 dependency noted above for audio
   sources specifically), which MAN-29's calibration correction targets. A
   fixed tolerance in the 100–300 Hz range (SkimCon/WintelnetX-validated
   order of magnitude) is appropriate for post-calibration inputs — whether
   that's implemented as true distance-tolerance matching or as a
   bucketing scheme (and if the latter, at what bucket width) is the open
   decision in Correction 3 above, not settled by this order-of-magnitude
   validation alone.
2. **Near-simultaneous distinct callsigns close in frequency** — already
   correctly handled by construction. DX (callsign) is an **exact-match**
   field in every source found (SkimCon, WintelnetX, the ArCluster dupe
   filter) — none of them fuzzy-match callsigns, so two genuinely distinct
   callsigns never collide regardless of how close in frequency or time
   they are. The only real risk in this shape is manta's own decoder
   *misreading* one callsign as another under close-frequency interference
   — a decode-accuracy problem (MAN-2 through MAN-9's territory), not a
   dedup-algorithm problem. Worth stating this boundary explicitly in
   MAN-16 so a future reader doesn't conflate the two.
3. **Multi-mode same-callsign spots** — addressed above: mode is an
   implicit hard partition in every source found (SkimCon's three
   independent per-mode outputs). Not a current concern for CW-only manta,
   but worth a one-line note for whenever that non-goal is revisited.

## Needs real-world validation (can't be settled by research alone)

- The exact tolerance/window values manta should ship as defaults (this
  spike validates the *shape* and *rough magnitude* of both parameters
  against two real, independently-converging implementations — it doesn't
  derive manta-specific optimal values, which depend on manta's own
  decoder's timing jitter and MAN-29's actual achieved calibration
  accuracy, neither measurable until MAN-11/MAN-13 exist in a runnable
  form).
- Whether manta's own decode pipeline produces same-instant cross-source
  duplicates on a similar ~1-second timescale to SkimCon's observed 0.702s
  figure, or a different one (depends on manta's own detection/decode
  latency, not researchable from outside).
- Whether a converted shared time basis (see "sample_ts has no shared unit
  or epoch across sources" above) has enough precision to distinguish the
  same-instant case from the repeat-CQ case in practice — not measurable
  until a real conversion path and real multi-source timing exist.

## Non-outcomes

- No implementation work was done from this ticket.
- MAN-16 remains blocked until this ticket reaches Done, per its own
  invariant scenario — this doc is that closure, but MAN-16's own spec
  still needs a real update before implementation: the corrections above
  (frequency drift scoping + MAN-34 dependency, the retracted
  suppression-window change, bucket-vs-tolerance), the two integration
  gaps (shared time basis, override interaction), and an explicit
  answer to the design question) before it's picked up for implementation.
  Filing that spec update is MAN-16's own next step, not a new ticket from
  this one — MAN-53's job was validation, not rewriting MAN-16.
