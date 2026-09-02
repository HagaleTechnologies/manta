# 2026-09-02 — OpenHPSDR/Hermes protocol wire-behavior spike

**Status:** accepted (investigation only — no implementation in this doc or its PR).

## Scope and method — no live hardware in this session

**This spike was done entirely from primary-source documentation and
reference-implementation source code, not from real hardware.** MAN-10's own
acceptance criterion calls for exercising discovery framing, IQ framing,
sample rates, and DDC support "against real hardware," and explicitly says
George (K5TR) has an idle Hermes-Lite 2 and two Red-Pitaya-family units
available. Neither this session nor its research agent has network access to
that hardware. Per MAN-10's own instruction to scope down rather than guess
when hardware access isn't available, this doc:

- Answers everything answerable from official docs and reference-client
  source code (piHPSDR, and the `HermesIntf.dll` release history), citing a
  URL for every concrete claim.
- Explicitly separates that from a **"Needs real hardware"** section
  (below) listing exactly what remains unverified and why spec-reading
  can't substitute for it.

MAN-11 should not be unblocked to start coding assuming the unverified items
are fine — the recommendation section below says what to do about that.

## Decision

**Target OpenHPSDR Protocol 1 (Metis) as the sole wire dialect for MAN-11's
initial driver.** All three in-scope devices are Protocol 1 today; Protocol 2
support exists only on a *different* Red Pitaya firmware (the transceiver
build, not the receiver build George would actually run) and only as of a
2026-08 release. Protocol 2's per-DDC framing is architecturally nicer for a
demux+loss/reorder handler (see below), so it's worth tracking as a future
migration, but nothing in scope for MAN-11 speaks it today.

## Devices in scope

Per MAN-10's comment thread, three Hermes-compatible devices, not two:

### 1. Hermes-Lite 2 (HL2), stock firmware

- **Protocol 1/Metis.** Board ID `0x06`. [Protocol wiki](https://github.com/softerhardware/Hermes-Lite2/wiki/Protocol)
- **Receiver count:** the stock/shipped gateware exposes **4 DDCs**. A
  separate **skimmer-oriented gateware variant**
  (`stable/20201212_72p8/variants/hl2b5up_cicrx/hl2b5up_cicrx.rbf`, RX-only,
  no TX) exposes **9–10 DDC slices** and is the variant this repo's use case
  actually needs, not the default 4-DDC gateware.
  [Software wiki](https://github.com/softerhardware/Hermes-Lite2/wiki/Software),
  [G0ORX/M1GEO write-up on CW Skimmer + HL2](https://www.george-smart.co.uk/2020/12/using-cw-skimmer-with-hermes-lite2-sdr/)
- **Sample rates:** 48/96/192/384 kHz on all DDCs — 192 kHz confirmed.
- **Discovery:** standard Metis broadcast, UDP port 1024.
- **Known integration trap, not a protocol detail:** the skimmer gateware's
  FPGA watchdog times out unless the host client sends keepalives the
  reference `HermesIntf.dll` only does after a patch to disable/extend the
  WDT. `manta-input`'s HPSDR driver needs its own keepalive logic regardless
  of client library choice — this is a real MAN-11 implementation
  requirement, not an optional nicety.

### 2. Red Pitaya, Pavel Demin firmware — two separate projects

Pavel Demin ships **two different HPSDR-family Red Pitaya images**; George's
skimming use case maps to the second one, not the first:

- `sdr_transceiver_hpsdr` (full TX+RX): **5 DDC + 1 DUC**, but only **3 DDCs
  are independent receive channels** — the other 2 are PA-linearization
  feedback, not general-purpose receivers. 48/96/192/384 kHz, 0–61.44 MHz
  tunable.
  [Project page](http://pavel-demin.github.io/red-pitaya-notes/sdr-transceiver-hpsdr/)
- `sdr_receiver_hpsdr` (RX-only "Alpine" image): **8 identical DDCs**,
  48/96/192 kHz (**no 384 kHz** on this image), 0–490 MHz. Its own docs have
  a "Running CW Skimmer Server and RBN Aggregator" section — **this is the
  image George's Red Pitaya skimming setup is actually running.**
  [Project page](http://pavel-demin.github.io/red-pitaya-notes/sdr-receiver/)

**Correction to MAN-10/MAN-15's prior research note:** "two independent
receiver instances, 8 bands each, from one Red Pitaya" is **not** what
`sdr_receiver_hpsdr` does. It's **one instance of 8 DDCs per physical
board/network link**, from one project. Getting a second independent
8-DDC instance means a second physical Red Pitaya on a second 100 Mbps
link, not one board exposing two instances. This changes the "8 or 16"
framing in MAN-10's technical notes — see Bandwidth below.

- **Protocol/discovery:** Protocol 1/Metis, but discovery differs from
  native HPSDR hardware — piHPSDR discovers a Red Pitaya via a **dedicated
  avahi/mDNS path** (`stemlab_discovery.c`, MAC prefix `00:26:32`), and the
  Red Pitaya's HPSDR application has to already be running — not the plain
  UDP-1024 broadcast discovery HL2 answers directly. `manta-input`'s driver
  needs a Red-Pitaya-specific discovery path, not one shared blindly with
  HL2. [piHPSDR stemlab_discovery.c](https://github.com/g0orx/pihpsdr/blob/master/stemlab_discovery.c)

### 3. Pavel Demin's separately-designed board (not Red-Pitaya-based)

Identified: a commercial **QMTech Zynq-7020 core board** paired with Pavel
Demin's own open-source **ADC expansion board** (`qmtech-adc`, 2024), shown
at his SDRA'25/Hamradio 2025 talk on JLCPCB-assembled open SDR hardware.

- [`qmtech-adc`](https://github.com/pavel-demin/qmtech-adc)
- [Talk: "Building open-source SDR hardware with the JLCPCB assembly service"](https://talks.darc.de/hamradio-2025/talk/3UYW8A/)
- Runs a ported `sdr_receiver_hpsdr_77_76` firmware — same project family as
  device #2's receiver image, differing only in ADC sample clock (77/76 MHz
  vs. Red Pitaya's 122.88/125 MHz), per the naming convention and the
  project's own repo structure.
  [qmtech-xc7z020-notes/projects](https://github.com/pavel-demin/qmtech-xc7z020-notes/tree/master/projects)
- **Working assumption, not a confirmed fact:** same Protocol 1/Metis
  dialect and DDC/sample-rate behavior as device #2's receiver image, on the
  strength of shared source lineage. No primary source confirms its actual
  discovery-reply board ID or achieved throughput under load — flagged
  below.

## Protocol 1 vs Protocol 2 — framing that matters for the demux/loss handler

Verified against piHPSDR's protocol-handler source, the actual client-side
implementation, not just the spec prose:
[old_protocol.c/.h](https://github.com/g0orx/pihpsdr/blob/master/old_protocol.c),
[new_protocol.c/.h](https://github.com/g0orx/pihpsdr/blob/master/new_protocol.c)

**Protocol 1 (Metis) — what all three in-scope devices speak today:**

- One UDP port (**1024**) carries *everything* — control and every
  receiver's IQ, time-multiplexed in a single stream. There is no
  per-receiver port to demux on.
- Packet = 1032 bytes: 8-byte Metis header + two 512-byte "USB frames."
  Each USB frame = 8-byte sub-header (3 sync bytes + C0–C4 control bytes) +
  up to 504 bytes of payload.
- IQ sample encoding: **24-bit I + 24-bit Q = 6 bytes per complex sample**,
  normalized by `/8388607.0` (2²³−1) in reference client code — this is a
  protocol convention, **not self-describing on the wire**. A P1 driver
  must hard-code 24-bit decoding; it cannot detect bit depth from the
  packet.
- Samples per USB frame = `floor(504 / (num_receivers × 6 + 2))` — the
  fixed `+2` is a mic/aux slot present regardless of receiver count. This
  means **the demux stride depends on how many receivers are configured**,
  known from the C&C setup exchange, not derivable per-packet in isolation.
- **No per-DDC sequence number.** Loss/reorder detection in P1 has to be
  done by tracking the shared Metis-level packet sequence (2 LSBs mark
  4-packet wideband-scan block boundaries — not a general-purpose loss
  counter) — i.e. **gap detection is inferred from the fixed-cadence packet
  stream itself (missing/out-of-order 1032-byte Metis packets), not read
  off an explicit field.** This is the single biggest implementation
  consequence for MAN-11: the loss/reorder handler needs to track expected
  Metis packet cadence and flag gaps in *that*, then propagate uncertainty
  into all DDC streams demuxed from the affected packet — there's no way to
  tell which specific DDC's samples were lost without also losing the
  others in a shared packet.

**Protocol 2 — not spoken by any in-scope device today, but worth
recording for a future migration:**

- **Separate UDP port per DDC**: `RX_IQ_TO_HOST_PORT_0..7` = ports
  **1035–1042** (up to 8 independent per-DDC flows), plus dedicated ports
  for command/response (1024), high-priority (1025), mic/line (1026), and
  wideband (1027).
- Per-DDC packet header is **self-describing**: bytes 0–3 = 32-bit sequence
  number (piHPSDR uses this directly: `if(ddc_sequence[ddc] != sequence)
  sequence_errors++`), bytes 4–11 = 64-bit timestamp (present, unused by
  piHPSDR), bytes 12–13 = bits-per-sample, bytes 14–15 = samples-per-frame.
- This gives demux and per-DDC loss detection "for free" via port number
  and an explicit sequence field — a materially simpler target than P1's
  shared-stream inference. **If/when a Protocol-2-speaking device enters
  scope, MAN-11's demux+loss handler should branch to a much simpler
  per-port path rather than reusing the P1 stride-inference logic.**
- Receiver-count ceiling is firmware-dependent either way — P2 doesn't
  itself grant more DDCs than the gateware implements
  (`HermesIntf.dll`'s 2021 P2 release notes: "most HPSDR P2 supports only 4
  RX except Hermes & ANAN10 only 2 RX" at that time).

**Correction to MAN-10's prior research note on `HermesIntf.dll`:**
Protocol 2 support did **not** first appear in the August 2026 release —
it's existed since **v21.7.18 (2021-07-18)**. What the Aug 2026 release
(`v26.8.9.1`) actually added was **Protocol-2 support specifically for the
Red Pitaya transceiver firmware** — device #2's *transceiver* image, not
the *receiver* image George runs.
[Releases](https://github.com/k3it/HermesIntf/releases)

## Bandwidth math — MAN-10's "8 or 16 192 kHz segments" confirmed and resolved

MAN-10's existing note (192 kHz × 6 bytes/sample ≈ 9.2 Mbps/DDC) is
**confirmed correct**, source-verified against the 24-bit/24-bit P1 encoding
above:

- Raw payload rate per 192 kHz DDC: 192,000 × 6 × 8 = **9.216 Mbps**.
- Realistic wire overhead (Metis sub-headers + UDP/IP/Ethernet framing) adds
  roughly 5–6% at typical packet sizes → **≈ 9.7–9.8 Mbps per DDC actually
  on the wire.**
- A 100 Mbps link saturates at ~10 DDCs; leaving real margin (never run a
  shared link at 95–100% utilization, especially one also carrying C&C
  traffic) puts the **practical ceiling at ~8 simultaneous 192 kHz DDCs per
  100 Mbps link.**
- **This resolves the ticket's "8 or 16" ambiguity: 8 is a per-link/per-unit
  ceiling, not a choice.** "16" is only reachable across **two physical
  units on two separate 100 Mbps links** (or one unit on a faster link) —
  nothing in P1 or P2 changes the raw IQ bit rate (P2 reframes, it doesn't
  compress), so this is a link-layer constraint independent of protocol
  dialect.
- **This is not a coincidence with device #2's design**: Pavel Demin's
  `sdr_receiver_hpsdr` image ships exactly **8 DDCs** as its
  receiver-optimized configuration — it's designed against this exact
  100 Mbps ceiling.
- **HL2's skimmer gateware (9–10 DDC) does not fit this ceiling at
  192 kHz**: 9 × 9.8 ≈ 88 Mbps (borderline, no margin left for anything
  else on the link), 10 × 9.8 ≈ 98 Mbps (effectively saturating a 100 Mbps
  link on IQ alone). **MAN-11 needs to either cap configured DDC count
  below 10 when running the HL2 skimmer gateware at 192 kHz, or drop
  per-DDC rate, or require Gigabit Ethernet on that leg** — this is a real
  constraint the driver's config validation should enforce, not just
  document.

## What MAN-11 needs to special-case per device

| Device | DDC count | Sample rates | Discovery | Notes |
|---|---|---|---|---|
| HL2, stock gateware | 4 | 48/96/192/384 kHz | Metis UDP-1024 broadcast | — |
| HL2, skimmer gateware | 9–10 | 48/96/192/384 kHz | Metis UDP-1024 broadcast | Needs WDT keepalive; exceeds 100 Mbps at 192 kHz above ~8 DDCs — cap in config |
| Red Pitaya, `sdr_transceiver_hpsdr` | 3 independent RX (of 5 DDC) | 48/96/192/384 kHz | avahi/mDNS, app must be running | Not the image George runs for skimming — lower priority |
| Red Pitaya, `sdr_receiver_hpsdr` (Alpine) | 8 | 48/96/192 kHz (no 384k) | avahi/mDNS, app must be running | This is George's actual skimming setup; 8 DDCs matches the bandwidth ceiling exactly |
| QMTech + `qmtech-adc` (Pavel's own board) | Assumed 8 (unconfirmed) | Assumed same as above (unconfirmed) | Assumed avahi/mDNS (unconfirmed) | Same firmware lineage as the Red Pitaya receiver image; no primary-source confirmation of actual behavior |

All three device families speak Protocol 1/Metis today — a single P1
demux/loss-handler implementation covers all of them; discovery needs an
HL2 path (native broadcast) and a separate Red-Pitaya-family path
(avahi/mDNS), and DDC-count/rate limits must come from a per-device config
table like the one above, not be assumed uniform.

## Needs real hardware — do not treat these as settled

Spec-reading and reference-client source code cannot substitute for these;
they need George's actual units in the loop before MAN-11 ships, not just
before it starts:

- Whether HL2 stock and skimmer-gateware firmware actually report/negotiate
  192 kHz correctly in a live discovery reply — wiki docs in this ecosystem
  have lagged shipped gateware before (the WDT keepalive bug wasn't
  documented until a third-party blog post found it).
- Whether the QMTech + `qmtech-adc` board (#3) actually matches the Red
  Pitaya receiver image's DDC count, sample rates, and discovery-reply
  board ID — only circumstantial (repo-lineage) evidence exists for this
  device.
- Real observed packet loss/reorder rates on actual LAN hardware at ~8×
  192 kHz sustained — exactly what the loss/reorder handler needs tuned
  against, and no spec substitutes for it.
- Whether Red Pitaya's *receiver* image (`sdr_receiver_hpsdr`) has gained
  Protocol 2 support the way the *transceiver* image apparently has
  (per the Aug 2026 `HermesIntf.dll` changelog) — the receiver image's
  current protocol version wasn't independently confirmed for this doc.
- Host-side CPU/network-stack overhead running manta's demux at
  ~80–98 Mbps sustained UDP ingest — a software bottleneck distinct from
  the wire-bandwidth math above.

**Recommendation:** MAN-11 can start against Protocol 1/Metis with the
per-device table above as its config-validation baseline, but its
acceptance criteria should require confirming the five items above against
George's actual hardware before merge, not just before starting — this
spike does not substitute for hardware-in-the-loop validation, it only
removes the need to *start* from spec-reading alone.

## Sources

- [Hermes-Lite 2 protocol wiki](https://github.com/softerhardware/Hermes-Lite2/wiki/Protocol)
- [Hermes-Lite 2 software wiki](https://github.com/softerhardware/Hermes-Lite2/wiki/Software)
- [G0ORX/M1GEO — CW Skimmer with Hermes-Lite2](https://www.george-smart.co.uk/2020/12/using-cw-skimmer-with-hermes-lite2-sdr/)
- [Pavel Demin — sdr-transceiver-hpsdr](http://pavel-demin.github.io/red-pitaya-notes/sdr-transceiver-hpsdr/)
- [Pavel Demin — sdr-receiver (HPSDR Alpine image)](http://pavel-demin.github.io/red-pitaya-notes/sdr-receiver/)
- [Pavel Demin — qmtech-adc](https://github.com/pavel-demin/qmtech-adc)
- [Pavel Demin — qmtech-xc7z020-notes projects](https://github.com/pavel-demin/qmtech-xc7z020-notes/tree/master/projects)
- [SDRA'25/Hamradio 2025 talk — open-source SDR hardware via JLCPCB assembly](https://talks.darc.de/hamradio-2025/talk/3UYW8A/)
- [piHPSDR — old_protocol.c](https://github.com/g0orx/pihpsdr/blob/master/old_protocol.c) / [old_protocol.h](https://github.com/g0orx/pihpsdr/blob/master/old_protocol.h)
- [piHPSDR — new_protocol.c](https://github.com/g0orx/pihpsdr/blob/master/new_protocol.c) / [new_protocol.h](https://github.com/g0orx/pihpsdr/blob/master/new_protocol.h)
- [piHPSDR — stemlab_discovery.c](https://github.com/g0orx/pihpsdr/blob/master/stemlab_discovery.c)
- [k3it/HermesIntf releases](https://github.com/k3it/HermesIntf/releases)
- MAN-10, MAN-15 comment threads (prior RBN-OPS groups.io forum research, 2026-09-01/02)

## Non-outcomes

- No implementation work was done from this ticket.
- MAN-11 remains blocked until this doc lands; it should not start from the
  unresolved hardware-verification items above without George's hardware in
  the loop first.
