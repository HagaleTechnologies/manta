# MAN-531: blocklisted calls cannot supply variant support

Date: 2026-11-09. Ticket: MAN-531. Baseline: `58406a2`.

## Decision

Ordinary completed words enter the support ledger only when grammar and country
allocation pass and the operator has not blocklisted the text. Call-changing
refinement applies the same rule to its destination. Refinement into blocked
text still removes the superseded occurrence and records the blocklist
withhold. The permanent spottability guard remains grammar/country based.

Ordinary, refined and fused rival searches filter current blocklist membership
inside the shared scan. This preserves an eligible runner-up when the strongest
rival is blocked. Fusion still excludes its own disagreeing episode copies.
The existing public support-query signatures and behavior remain unchanged.

Both operator-list replacement and the blocklist builder prune blocked ledger
entries across every track before installing the incoming list. Other support,
tracks, repetition counts, dedupe and suppression counts survive installation.
Removing a blocklist entry cannot restore pruned observations. Fresh eligible
observations can build support again.

SCP membership continues to exempt candidates from variant arbitration. It
does not bypass the candidate's repetition requirement or operator blocklist.

## Evidence and limits

Four validator regressions failed against the baseline with blocked ledger
support or a blocked rival veto. The reload regression separately failed with
three repetitions still present after list installation. After the fix,
`cargo test -p manta-spot --locked --lib man531_` passed all nine tests.

The tests cover three blocked `K5ARH` transmissions followed by two genuine
`K5ARG` transmissions, case-insensitive membership, blocked refinement, stale
rivals in all three arbitration paths, pruning across tracks, active-builder
installation, preservation of unrelated support, and fresh support after
unblocking. Support-unit tests cover runner-up selection, combined fusion-copy
and eligibility exclusions, and retention ordering.

Matched minimal SCP fixtures exercise ordinary and refined `K5AR` against
`K5ARH`, and fused `W1AW` against `W1AX`. Each has a non-SCP variant-veto
control and a blocked-SCP control with real competing support. These tests
characterize existing exemption behavior; no SCP production logic changed.

This resolves MAN-100 round-3 Findings 3 and 6. Findings 4 and 5 remain outside
this change. The final implementation run reports the full repository gate
separately. No live SDR, Pi CPU-budget, macOS or Windows measurement is claimed.
