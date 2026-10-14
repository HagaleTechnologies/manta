# manta and similar projects

Several projects decode CW from an SDR. They do different jobs, and pages
elsewhere on the web describe some of them wrongly. This page says what each
one does and does not do, using each project's own documentation as read on
2026-10-14. It compares features, not decode quality: no published
benchmark compares any of them, manta included. manta is a pre-stability
alpha; the README's [Status](../README.md#status) says what has been measured.

| Project | Many CW signals at once | Other modes | Interface | `DX de` telnet server | Route into the RBN | Licence |
|---|---|---|---|---|---|---|
| manta | Yes, the whole SDR passband | None (RTTY recognised, not decoded) | Headless daemon and CLI | Yes, :7300 (plus JSON on :7301) | Through RBN Aggregator reading its telnet server; its direct uplink is experimental | MIT OR Apache-2.0 |
| CW Skimmer Server | Yes, up to 7 bands × 192 kHz | CW only | Headless, Windows system tray | Yes | Through RBN Aggregator | Closed; unlocked by a CW Skimmer key |
| SparkSDR | No: "There is a cw decoder but it is far from finished" | FT8, FT4, WSPR, JT65, JT9, Q65, FST4, FST4W | Desktop app | No | None for CW | Closed, free |
| sdroxide | Yes, a ~192 kHz window that follows the waterfall | RTTY and PSK31 skimmers | Desktop app, or the same UI in a browser (`--server`) | No | None: it reads the RBN feed for a propagation map | GPL-3.0 |
| OpenWebRX+ with csdr-skimmer | Yes, up to 96 ksps (since 1.2.101) | RTTY skimmer, plus OpenWebRX's other decoders | Web SDR server; skimmers can run as background services | No | None: CW spots go to PSK Reporter | AGPL-3.0 / GPL-3.0 |
| sdrainer | Yes; "experimental and incomplete" | None yet (PSK31, RTTY planned) | Command line; spots shown on a TCI radio's spectrum | Yes, a local DX-cluster-style server | None; sending spots to a cluster is planned | MIT |

## CW Skimmer Server
VE3NEA's headless sibling of CW Skimmer, and the engine behind most of the
RBN. Decodes all CW on up to seven 192 kHz bands, serves spots over telnet,
and reaches the RBN through the separate RBN Aggregator program. Windows
only, and needs an x86 CPU with SSE3. Sources: [Skimmer Server](https://www.dxatlas.com/SkimServer/),
[RBN Aggregator](https://www.reversebeacon.net/pages/Aggregator+34).

## SparkSDR
A cross-platform SDR app for Hermes-Lite 2, OpenHPSDR-compatible radios and
SDRplay, on Windows, Linux (x64 and Raspberry Pi) and macOS. It skims
WSJT-X-family digital modes. It has no CW skimmer of its own: its manual
says its CW decoder "is far from finished", and it documents no
many-signal CW decoding. SparkSDR is sometimes listed as an open CW skimmer
for the RBN. It is neither open source nor a CW skimmer. Sources:
[home](https://www.sparksdr.com/), [manual](https://www.sparksdr.com/manual).

## sdroxide
A Rust SDR transceiver with built-in CW, RTTY and PSK31 skimmers. They label
signals on the waterfall inside a ~192 kHz window that follows where you are
looking, re-reading each signal twice a second ("a coarse best-effort copy",
its manual says). It reads DX clusters and the RBN's feed, but it serves no
spots: its RBN connection feeds a propagation heat map, and its decoded-CW
UDP output carries the CW panel's copy, not skimmer spots. Sources:
[README](https://github.com/dividebysandwich/sdroxide),
[manual §4 and §6.5](https://github.com/dividebysandwich/sdroxide/blob/main/docs/USER_MANUAL.md).

## OpenWebRX+ and csdr-skimmer
OpenWebRX+ is a web SDR server. Since 1.2.74 it runs csdr-skimmer's CW
skimmer, and later an RTTY one. Version 1.2.101 widened both to 96 ksps,
added a callsign spotter, let them run as background services, and added CW
spot reporting to PSK Reporter. Older write-ups that give 48 kHz, or call it
a display-only feature with no spot reporting, are out of date. It has no
RBN or telnet spot output. Sources:
[changelog](https://github.com/0xAF/openwebrxplus/blob/master/CHANGELOG.md),
[csdr-skimmer](https://github.com/luarvique/csdr-skimmer).

## sdrainer
A Go CW skimmer for TCI radios and KiwiSDR, by its own description
"experimental and incomplete". It shows spotted calls on the radio's
spectrum and serves them to local telnet clients like a DX cluster. Its
README lists sending spots to an external cluster as future work, so it is
not an RBN feed today. Source: [sdrainer](https://github.com/ftl/sdrainer).

## manta
A headless CW skimmer that serves `DX de` spots on :7300 and JSON on :7301.
The supported route into the RBN is an RBN Aggregator reading that telnet
server. manta's own direct uplink is experimental and has not been checked
against the RBN's real ingest
([MAN-90](DECISIONS/2026-10-11-man90-uplink-protocol-verification.md)).
It has no waterfall and no GUI ([Non-goals](../README.md#non-goals)).
