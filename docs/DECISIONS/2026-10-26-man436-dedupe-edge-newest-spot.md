# MAN-436: same-input dedupe decides against the station's newest spot across a bucket edge

**Status:** accepted. The fix is in `Dedupe::assess`
(`crates/manta-spot/src/dedupe.rs`), pinned by one regression test.

## Context

Deferred from PR #387 (MAN-120) review, Codex P2 thread
`discussion_r4361034174`, per [the PR review convergence
policy](2026-08-07-pr-review-convergence-policy.md).

MAN-120 D1 joined a same-input report to this input's in-window spot one
bucket over, within one channel, and withheld it unless the SNR/type
overrides held "against that entry". `assess` checked the own-bucket entry
first and then could downgrade an `Emit` against the neighbour entry. So a
report had to override **both** stored spots to emit, and when a station
wobbles across the 300 Hz edge one of those spots is stale. Reproduced at
`b284c398`, input 0, `W1AW`, L = 14_012_549 Hz (bucket 46_708), R =
14_012_551 Hz (bucket 46_709), one second apart:

| Sequence | HEAD | One frequency |
|---|---|---|
| `Cq(L) De(R) Cq(L)` (the ticket) | Emit, Emit, **WithheldRepeat** | Emit, Emit, Emit |
| `De(R) Cq(L) De(R)` (mirror) | Emit, Emit, **WithheldRepeat** | Emit, Emit, Emit |
| `Cq(R) De(L) Cq(L)` | Emit, Emit, **WithheldRepeat** | Emit, Emit, Emit |

The first two are the ticket's form: the stale own-bucket `Cq` withholds,
and the `outcome == Emit` guard never consults the newer `De` across the
edge. The third is a second form: the report lands in the same bucket as
the newer `De` (so the own-bucket check emits), and the neighbour
downgrade then withholds it against the stale `Cq` on the other side.

## Decision

Decide a same-input report against the **newer** of the own-bucket spot
and the joined same-input spot across the edge (larger `sample_ts`; a tie
keeps the own-bucket spot, the key an emit overwrites). That is how one
frequency already behaves, and it matches `last_sample_ts`, which already
returns the newer of the two. The two stages are folded into one decision,
so neither form above can recur.

- **Supersedes only MAN-120 D1's "overrides hold against that entry"
  clause.** The Hz-bounded join itself, its `WithheldRepeat`
  classification and its one-channel bound are unchanged.
- **The superseded bucket is not retired.** Deleting the older entry on
  emit would add the key movement MAN-120 D1 avoided, and could rewind
  `last_sample_ts` for a late-merged report. Choosing which entry to read
  writes nothing new: an `Emit` still writes only its own key, with
  `stored_ts = max(prev, sample_ts)`.
- **Loosen-only.** The old rule emitted iff a report overrode both
  entries; the new rule emits iff it overrides the newer one. Nothing that
  emitted before is withheld now.
- **NaN SNR still withholds.** The override still reads
  `snr_db - prev.snr_db >= SNR_IMPROVEMENT_DB`.
- **Unchanged:** cross-input rules (MAN-13 D6's `WithheldCrossInput` in
  the same bucket or across the edge, and MAN-14's counters),
  `last_sample_ts`, every wire format and every metric.

## What shipped

The fix and `an_edge_wobble_decides_against_the_newest_spot_as_one_frequency_does`
(`dedupe.rs`), which runs the three sequences above, a four-step wobble
and two negative controls (`Cq(L) De(R) De(L)` and a 6 dB-louder `Cq(R)`
then `Cq(L)` stay withheld) both across the edge and at one frequency, and
asserts the same outcomes for both. It fails on the ticket's sequence
without the fix. All pre-existing dedupe tests and both
`resolved_beacon_*` validator tests pass unchanged, and the golden suites
need no fixture change.

## Related

- [MAN-120](2026-10-23-man120-dedupe-beacon-sigma-cer-cleanup.md): D1,
  the same-input edge join this refines.
- [MAN-13 multi-input daemon](2026-10-15-man13-multi-input-daemon.md): D6,
  the cross-input rules this leaves unchanged.
