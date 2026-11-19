# MAN-17: Aggregator swap validation runbook

Status: accepted (interim). Live check parked (outreach closed, 2026-11-13).

## Decision trail

Tony Hagale, 2026-10-13T22:04:33Z: "Mostly done: MAN-86 (Aggregator SKIMMER/SETT handshake) and MAN-90 (verified against real Aggregator traffic). What's left is the live check with K5TR. Rescope to that, or close." This narrows MAN-17 to George's live check.

Tony Hagale, 2026-11-13T23:58:25Z: "RBN outreach stays closed for now (verify-only, ka9q input first). Keep parked." This keeps that live check parked. Tony Hagale, 2026-09-06T16:06:45Z: "Four concrete blocking gaps found and filed as their own tickets." MAN-86 through MAN-89 have landed. Catalyst Cloud, 2026-10-08T04:10:33Z: MAN-52 is a soft prerequisite only for the end-to-end path decoding from the operator's own Hermes hardware; "the telnet-only path doesn't" need it.

## Evidence and remaining claim

The composed `aggregator_session_survives_banner_to_bye_on_one_connection` acceptance test checks the documented inbound session. [MAN-86](2026-09-07-man86-aggregator-sett-handshake.md) records the SETT behavior. [MAN-90](2026-10-11-man90-uplink-protocol-verification.md) addresses the separate Aggregator-to-RBN uplink. No real Aggregator binary has yet connected to a running manta, so an unmodified swap remains unverified. MAN-96 concerns a separate secondary-node soak.

When outreach opens, the operator follows the [Aggregator swap runbook](../RUNBOOKS/aggregator-swap.md), records the SETT and spot bytes, and verifies RBN output. Any rejection is recorded as a fixed issue or known limitation there. Until then, neither the runbook nor the test proves delivery to the live RBN network.
