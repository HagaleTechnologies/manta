# Aggregator swap validation (MAN-17)

This procedure is for an operator whose Aggregator already feeds RBN. It is not part of CI. The live check remains parked while outreach is closed. Record each authorized run below. See the [MAN-17 decision record](../DECISIONS/2026-11-19-man17-aggregator-swap-runbook.md).

## What this proves

The first MAN-17 scenario passes only when the existing Aggregator accepts manta as a Skimmer source without downstream changes and RBN still publishes the operator's spots. The second requires every rejection or misparse to be recorded and either fixed or marked as a known limitation.

## Before you start

Use an Aggregator already admitted to RBN, a Linux host for manta with a synchronized clock, and your primary's spotter callsign. Keep the current Skimmer available. Arrange a short observation window with the operator. The [RBN node guide](../GUIDES/rbn-node.md) covers installation. A hardware recording, such as K5TR's B2 40 m IQ corpus recording, can also replace the synthetic input after the first bench run.

## Stage A. Bench pre-flight without an SDR

From a checkout with manta installed, use an unused local port if 7300 is occupied. This example binds only localhost. Generate the synthetic file, write the minimal config, then start the server:

```sh
manta gen v1 --out /tmp/v1
cat > /tmp/v1/demo.toml <<'CONFIG'
[server]
station_callsign = "W5AU"
bind_addr = "127.0.0.1"
CONFIG
manta run --source /tmp/v1/v1.wav --source-iq --realtime --loop --config /tmp/v1/demo.toml
```

In another terminal, connect within 20 seconds. Enter your callsign, then the handshake and filter commands. A spot takes roughly 21 seconds to arrive on first replay; `sh/dx` retrieves recent spots if you connected late. Stop manta with Ctrl-C after inspection.

```sh
telnet localhost 7300
```

Captured on 2026-11-19 with the generated V1 file and manta 0.1.0. The container run used a free local port instead of 7300; the exchange and spot bytes are the same. The test suite pins the wire layout.

```text
Welcome to the manta Telnet cluster port!
manta 0.1.0 is operated by W5AU
Please enter your callsign: 
N0CALL
de W5AU-# >
SKIMMER/SETT
SETT: vlNormal 14000.0-14048.0
sh/dx
DX de W5AU-#:   14012.34  W1AW           CW    28 dB  20 WPM  CQ      0509Z
BYE
CU AGN!
```

## Stage B. Live swap at the operator site

1. Add a second Skimmer entry in Aggregator pointing to manta's host and port 7300. Leave the existing primary entry in place during the check.
2. In Aggregator's Skimmer Traffic tab, confirm its `SKIMMER/SETT` probe receives `SETT: vlNormal ...` and spots appear within minutes. Aggregator manual v6.0 §9.2 says an unanswered SETT causes spots from that source to be dropped after about five minutes; the [MAN-86 record](../DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md) documents the rule.
3. Confirm RBN Connection Status stays green and the old entry still works. Check the [RBN public telnet feed](https://www.reversebeacon.net/pages/telnet+39) or [daily CSV](https://www.reversebeacon.net/pages/skimmer-records+105) for spots under the operator's call.
4. Only after the checks pass, repoint or retire the old Skimmer entry. If they fail, restore the old entry and record the mismatch below.

## Stage C. Evidence to record

Record UTC dates, manta and Aggregator versions, source and band, exact SETT reply bytes, sample `DX de` lines as Aggregator received them, RBN telnet or daily CSV sightings under the operator's call, and how long the swap ran. Record the old feed's status and the rollback result. Redact credentials and private addresses.

## Incompatibilities found

| Symptom | Where recorded | Disposition: fixed or known limitation |
| --- | --- | --- |
| (none yet) | | |

For each mismatch, create a linked issue or decision record and put its identifier in “Where recorded.” Mark its disposition only `fixed` or `known limitation`; include the evidence and any workaround there. Never discard a rejected spot without an entry.

## Runs

(no live runs yet)
