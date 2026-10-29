# MAN-302: UberSDR wide IQ, clean-room provenance; the input waits on a hardware-free local receiver

**Status:** accepted. The published UberSDR-IQ specification describes the codec, but the receiver's wide-IQ transport is not documented sufficiently to connect to it. No local receiver can produce IQ in this environment under the ticket's rules, so no UberSDR input code was written.

## Context

MAN-302 permits only the published specification, receiver documentation and wire traffic captured from a receiver run locally from a published image or release binary. It forbids reading UberSDR AGPL/GPL codec or server source, contacting an operator or maintainer, and connecting to another person's receiver. The work stops if the local stack cannot emit IQ without SDR hardware or the allowed material is insufficient. MAN-303's live-receiver leg remains parked; MAN-304's add-on packaging remains blocked.

## Inputs used

| ID | Input | Version, URL or path | Integrity and licence | Used for |
|---|---|---|---|---|
| I1 | UberSDR-IQ `SPEC.md` | [Raw specification](https://raw.githubusercontent.com/madpsy/ubersdr-iq/a26ebc87ac6e83d743d7f223c3ba953c7aba5615/SPEC.md), commit `a26ebc87ac6e83d743d7f223c3ba953c7aba5615` (2026-10-14); 1,785 source lines | SHA-256 `1f2b38ed571c1144f5dac471cca1e5dbd1fe549014bd72b088d5964423ca2aab`; CC BY-SA 4.0 | F1–F17; licence and implementation boundary |
| I2 | `LICENSE-DOCS` | [Raw licence](https://raw.githubusercontent.com/madpsy/ubersdr-iq/a26ebc87ac6e83d743d7f223c3ba953c7aba5615/LICENSE-DOCS), same commit; checked 2026-10-26 | SHA-256 `28a9529c7d0bb4dc51f4bf5c116a3d16ef247a052f7591466768ddf563fd1cf5`; CC BY-SA 4.0 | Specification's document licence |
| I3 | UberSDR website | [Published receiver site](https://ubersdr.org/), checked 2026-10-26 | Website, not source code | RX-888 MkII requirement and advertised 384 kHz IQ |
| I4 | Receiver `README.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/7192bfd615830eab26bda0fa3863fcf26265c8ee/README.md), commit `7192bfd615830eab26bda0fa3863fcf26265c8ee`; checked 2026-10-26 | Published documentation; SHA-256 `f600112886b01f1660b4c49124b422809eb77d4820648212baaee3ff8c143757` | Installation requires an RX-888 MkII; published flow builds a container locally |
| I5 | Receiver `release_notes.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/7192bfd615830eab26bda0fa3863fcf26265c8ee/release_notes.md), same commit; checked 2026-10-26 | Published documentation; SHA-256 `e3874b578c3f25b9f033e257f3cfc1002225a2e769fb18d2528781fcd44e5ac0` | 384 kHz client support and session-limit context |
| I6 | Receiver `static/v2/BRIDGE_API.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/7192bfd615830eab26bda0fa3863fcf26265c8ee/static/v2/BRIDGE_API.md), same commit; checked 2026-10-26 | Published documentation; SHA-256 `ecc82f2fa9804c58587d2f83900b05c625b6f335210c70020cab630417a7d519` | Mentions `/api/description` but gives no complete wide-IQ transport contract |
| I7 | Receiver `clients/python/README.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/7192bfd615830eab26bda0fa3863fcf26265c8ee/clients/python/README.md), same commit; checked 2026-10-26 | Published documentation; SHA-256 `6a5663765c6f4abc5ca79afdaee097718878ab1589a24b35930502ab0eaca453` | Documents an audio client's WebSocket URL example, not the wide-IQ request |
| I8 | GitHub API metadata | `https://api.github.com/repos/madpsy/ubersdr-iq` and `https://api.github.com/repos/madpsy/ka9q_ubersdr/releases/latest`, consulted 2026-10-26 in planning | Repository names, file names and release asset names only | Locate the allowed documents and check for a published server binary; no protocol fact taken from it |
| I9 | Search for the specification | Query `ubersdr-iq SPEC` in the 2026-10-26 planning phase; recorded in `thoughts/shared/plans/2026-10-26-MAN-302-operators-should-be-able-to-feed-manta-wide-iq-from-an.md` | Search-result titles only; no source result read | Locate I1; no protocol fact taken from it |
| I10 | Prior research and plan | `thoughts/shared/research/2026-10-26-MAN-302-operators-should-be-able-to-feed-manta-wide-iq-from-an.md` and `thoughts/shared/plans/2026-10-26-MAN-302-operators-should-be-able-to-feed-manta-wide-iq-from-an.md`, both dated 2026-10-26 | Internal project documents; plan SHA-256 `a60eec3c7515046a4083f025f54af1665920fed1618b579326586355ce52361b` | Prior gate observations, implementation plan and prototype measurements |

Local wire traces: none. The gate stopped before a receiver could run. No AGPL/GPL source was read. A planning-phase tree listing exposed file and directory names only; no repository page, archive, package, issue, gist or code search result containing source was opened. The upstream `testdata/` and `conformance/` vectors were not used.

## Off-limits for MAN-302 work

- The `madpsy/ubersdr-iq` and `madpsy/ka9q_ubersdr` repository pages and all files other than raw `SPEC.md`, `LICENSE-DOCS` and published Markdown documentation: their codec and server source is AGPL/GPL material.
- Those repositories' `testdata/` and `conformance/` directories: they are not locally captured wire traffic and may contain source-derived vectors.
- Dockerfiles, Compose files and `install.sh`: they are installation program source, and building the server from source is forbidden by this ticket.
- `thoughts/shared/research/2026-10-08-ubersdr-competitive-analysis/` and lines 157–158 of `thoughts/shared/research/2026-10-13-MAN-140-ingest-iq-from-a-ka9q-radio-multicast-stream.md`: these predate this clean-room boundary and contain source-derived discussion.

## Gate

The commands below were run on 2026-10-26. All document reads were limited to I1–I7. The scratch copies live outside the checkout; no upstream code was copied into manta.

| ID | Check | Command | Output | Result |
|---|---|---|---|---|
| G1 | Local container runtime | `command -v docker; command -v podman; test -S /var/run/docker.sock; test -d /dev/bus/usb` | Both commands absent; no Docker socket or USB bus | FAIL |
| G2 | Published receiver image | Fetch I3 and I4 at the URLs above; inspect their installation instructions for a published image reference | I4 names `docker compose build` and `docker compose up -d`; neither I3 nor I4 identifies a pullable receiver image. No `docker pull` target exists. | FAIL |
| G3 | Hardware-free IQ | Inspect I3 and I4 for a receiver simulator or file input | I3 requires an RX-888 MkII; I4 says to plug it in before installation. Neither documents a hardware-free IQ producer. The site's recording playback describes playback to clients, not a receiver input. | FAIL |
| G4 | Codec specification | `curl -sS -o SPEC.md https://raw.githubusercontent.com/madpsy/ubersdr-iq/a26ebc87ac6e83d743d7f223c3ba953c7aba5615/SPEC.md && sha256sum SPEC.md`; inspect licence lines 12–19 | SHA-256 `1f2b38ed571c1144f5dac471cca1e5dbd1fe549014bd72b088d5964423ca2aab`; the document says CC BY-SA 4.0 and allows independently written decoders under their own licence | PASS |
| G5 | Wide-IQ transport facts | Inspect I3–I7 for the stream path, width negotiation, bypass-password placement and description schema | I6 mentions `/api/description` and I7 shows an audio WebSocket example; neither specifies the wide-IQ exchange. No permitted local trace exists. | FAIL |

**Verdict: STOP**

GO requires G1, G2, G3 and G4 all to pass; they do not. The receiver was not started, and no third-party receiver was contacted.

## What the SPEC establishes (F-rows)

These are factual constraints paraphrased from I1, not copied implementation text. They do not establish how to request a stream from the receiver.

| ID | Fact | SPEC § |
|---|---|---|
| F1 | Transport messages each hold one packet, with a header and a sample body; integer fields use little-endian and variable-length encodings. | 3, 4.1 |
| F2 | A packet starts with hexadecimal bytes `50 43 4D 34`. | 5.2 |
| F3 | The next byte carries escape, quality, metadata, silence and count flags plus a three-bit profile; escape and silence cannot occur together. | 4.2 |
| F4 | Header fields cover time, sample count, and sometimes rate, channel count and two quality readings; later packets may carry time as a signed delta. | 5.1, 5.3–5.6 |
| F5 | State carries between packets, so a delta before initial full metadata is invalid; zero rate or zero channels are invalid. | 5.5, 5.7, 5.9 |
| F6 | Profile 0 is complex IQ, 1 is real audio and 2 is requantised complex IQ. Values 3–7 must fail; a changed profile resets predictor state. | 6.1, 8.4 |
| F7 | Complex prediction uses 16 taps and a 16-sample stride with fixed-point tap updates, rounding, leak and saturation rules. Both ends must use identical integer arithmetic. | 6.2 |
| F8 | A coded body starts with a bounded Rice parameter, then encodes signed prediction errors as low-bit-first unary/remainder values. | 6.3 |
| F9 | Escape bodies carry raw little-endian 16-bit samples while preserving predictor evolution. | 6.4 |
| F10 | Silence bodies have no payload; their declared zeros still advance predictor state. | 6.5 |
| F11 | A nonsilent profile-2 body adds a shift of 0–15; decoded values are restored with saturating scaling after prediction. | 6.6.1–6.6.2 |
| F12 | Packet loss or reordering invalidates later predictor state; a new stream needs a fresh decoder and its first packet. | 8.1–8.3 |
| F13 | The specification describes `min_margin` as an integer dB request for reduced depth, with absent or zero meaning lossless; its use on the receiver transport remains T7. | 6.6.4 |
| F14 | A `.uiq` file repeats a little-endian 32-bit packet length followed by that packet. | 9 |
| F15 | Other, magic-less frame forms can share a socket and begin with a small profile byte; handling them awaits T8. | Appendix B |
| F16 | IQ sample components are signed 16-bit values in I/Q order; the header's count is across components. | 4.1, 5.4 |
| F17 | The reference's 384 kHz block sizes are usually 720 component samples and otherwise 240; this is context, not a transport guarantee. | 5.4 |

## Transport facts still open (T-rows)

| ID | Fact needed | Status |
|---|---|---|
| T1 | Default port of the receiver's web service | open |
| T2 | Description request and complete response format (`/api/description` is mentioned in I6) | open |
| T3 | Description fields for tuning range, password-dependent IQ widths, callsign and locator | open |
| T4 | Wide-IQ stream endpoint and selection of centre frequency and width | open |
| T5 | Bypass-password placement and wrong-password response | open |
| T6 | Refusal response when a width needs a password | open |
| T7 | Reduced-depth request on this receiver transport | open |
| T8 | Other frames on the IQ socket and any required keepalive | open |
| T9 | Session set-up, concurrent-use and time limits | open |
| T10 | Centre-frequency meaning and I/Q sign, checked with a positive-offset tone | open |
| T11 | Whether timestamps follow the sample clock | open |
| T12 | Edge response across all four IQ widths | open |
| T13 | Profiles actually sent for lossless and reduced-depth IQ | open |

## Evidence the SPEC suffices for the codec

The planning phase built an uncommitted throwaway encoder and decoder solely from I1. With manta's 96 kHz W1AW vector at 20 dB SNR, profile 0 round-tripped the signed samples exactly, carried about 202.9 kB/s and yielded one W1AW spot at 22.85 dB; profile 2 at shift 5 carried about 84.4 kB/s and yielded the same call and text at 22.10 dB. A shift-9 control carried 18.1 kB/s but lost the spot. Decode cost was 1.0–1.3% of one Ryzen 9 5950X core at 96 kHz; a Raspberry Pi 4 was not measured. The prototype was never committed to manta. A self-round-trip can share an error on both ends, so it does not replace an independent receiver capture. Upstream vectors outside I1 are off-limits.

## Stop: what is missing

- M1: no container runtime or USB bus is available on this host (G1).
- M2: the published deployment material identifies no pullable receiver image or server release binary (G2).
- M3: the published receiver material documents no hardware-free IQ source for the local stack (G3).
- M4: no permitted local wire capture exists, leaving T1–T13 open (G5).

## What would unblock it

The gate can be rerun if the published documentation later provides a pullable receiver image and a hardware-free IQ mode on a capable host. A separately authorized owner-run RX-888 MkII would change the local-stack premise, but MAN-303 remains parked. No operator contact is part of this path. The website also advertises KiwiSDR emulation, which manta's existing narrowband Kiwi input might use, but that has not been tested and would not meet this wide-IQ ticket.

## Consequences

There is no new input, flag, configuration key or metric. MAN-304 remains blocked. The GO design is in the 2026-10-26 MAN-302 plan (D6–D21), to be restated in a later GO decision record if the gate can pass. Manta remains MIT OR Apache-2.0 clean.

## References

MAN-302 ticket and 2026-10-26 implementation plan; [MAN-140 network-input decision](2026-10-13-man140-ka9q-radio-input.md), [MAN-319 capture-dependent decision](2026-10-19-man319-uplink-registration-exchange.md), [MAN-73 reconnect decision](2026-10-05-man73-source-reconnect.md), and [source-reconnect wiki](../../wiki/pages/source-reconnect.md).
