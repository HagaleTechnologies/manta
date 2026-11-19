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

Configure the live SDR and `[server]` using the [RBN node guide](../GUIDES/rbn-node.md#3-write-the-config). For Hermes or Hermes-Lite, use the [OpenHPSDR input example](../GUIDES/raspberry-pi.md#3-configure-your-receiver). Use the operator's spotter callsign and a receiver that can run alongside the original Skimmer. Bind manta's telnet listener to a LAN address reachable from the Aggregator host on port 7300, or use loopback if both run on the same host. Apply the [firewall rules](../GUIDES/rbn-node.md#6-firewall-and-exposure) before starting. Leave direct RBN uplink disabled.

Check the config, then start manta once the check passes.

```sh
manta config check --config manta.toml
manta run --config manta.toml
```

From the Aggregator host, connect with `telnet <manta-host> 7300` and confirm that fresh `DX de` spots arrive from live CW. Close the test connection. Save the original primary connection settings and confirm that the existing feed is publishing fresh spots before proceeding.

1. Add manta on Aggregator's Secondary Skimmers tab. Note its secondary source number and keep the original primary connected. In Skimmer Traffic, use manta's source number to find its `SETT: vlNormal ...` reply and `DX de` rows. An unanswered SETT prevents forwarding from that source. See the [Aggregator v6.0 manual](https://cms.reversebeacon.net/sites/cms.reversebeacon.net/files/2019/12/21/Using%20Aggregator%20-%20v6.0.pdf), §§6 and 9.2.
2. Check that RBN Connection Status stays green. In Skimmer Traffic, `+` marks a row selected for forwarding. Match those rows from manta to spots under the operator's spotter callsign on the [RBN public CW telnet feed](https://reversebeacon.net/pages/Telnet%2Bservers%2B30) (`telnet.reversebeacon.net:7000`) by spotted callsign, forwarded frequency and UTC time at the feed's precision. The [daily RBN CSV](https://reversebeacon.net/raw_data/) can corroborate the match when available. Exclude matches that could also come from the original primary or another source. If you cannot verify a manta match within the agreed window, remove the temporary secondary and record what happened.
3. After a confirmed match, remove the temporary manta secondary. On the Connections tab, disconnect the Primary Skimmer Connection, set its address and port to manta's, and reconnect. Keep the original Skimmer running for rollback. The manual's §9.1 explains why manta must move to the primary slot before the old Skimmer can be retired.
4. Record the UTC cutover time. Confirm the primary reconnects, manta answers SETT and RBN Connection Status is green. Repeat step 2's match with fresh `+` rows from source `0` received after cutover. Retire the old Skimmer only after those rows match new public spots.
5. If the post-cutover check fails or the observation window expires without a confirmed match, restore the saved primary settings and reconnect to the original Skimmer. Verify fresh spots from that Skimmer reach the public feed again and record the rollback result and any mismatch below.

## Stage C. Evidence to record

Record UTC dates and the cutover time, manta and Aggregator versions, SDR and band, and exact SETT reply bytes. Keep paired samples from before and after cutover, including the Skimmer Traffic source number and `+` marker, the raw `DX de` line, and the matching public RBN row. Record how long the swap ran, the old feed's status and the rollback result if applicable. Redact credentials and private addresses.

## Incompatibilities found

| Symptom | Where recorded | Disposition: fixed or known limitation |
| --- | --- | --- |
| (none yet) | | |

For each mismatch, create a linked issue or decision record and put its identifier in “Where recorded.” Mark its disposition only `fixed` or `known limitation`; include the evidence and any workaround there. Never discard a rejected spot without an entry.

## Runs

(no live runs yet)
