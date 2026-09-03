# MAN-55 Finding 1: OpenHPSDR Protocol 1 C&C tuning packet layout

Companion to `2026-09-02-hpsdr-hermes-protocol-spike.md` (MAN-10), which
pinned the *inbound* IQ packet framing. This doc pins the *outbound*
C&C ("Command & Control") fields `HpsdrDevice` needs to actually configure
a device's sample rate, receiver count, and per-receiver tuned frequency,
rather than treating `HpsdrConfig` as metadata-only (the pre-Finding-1 gap
this file's own doc comments flagged).

Sourced by an external-research agent from the Hermes-Lite2 protocol wiki,
piHPSDR's `old_protocol.c` (sender) and `hpsdrsim.c` (protocol
simulator/decoder) — three independent codebases. Confidence markers below
match the agent's own: **[CONFIRMED]** read directly from a primary
source, **[CROSS-CONFIRMED]** independently agreeing across ≥2 primary
sources, **[UNCERTAIN]** genuinely unresolved.

## C0 byte: ADDR + MOX

**[CROSS-CONFIRMED]** across all three sources, bit-for-bit:

```
C0 = (ADDR << 1) | MOX
```

`ADDR` is 6 bits (not 2-3 as an earlier internal assumption held), `MOX`
is 1 bit (transmit-active flag; always 0 here — this driver never
transmits).

## ADDR → C1-C4 data field mapping (fields this driver uses)

`C1..C4` form one 32-bit big-endian word (`C1`=bits 31:24 … `C4`=bits 7:0).

| ADDR | Field | Encoding |
|---|---|---|
| `0x00` | General settings | `C1` bits[1:0] = sample rate: `00`=48k `01`=96k `10`=192k `11`=384k; `C4` bits[6:3] = `num_receivers - 1` |
| `0x01` | TX1 NCO frequency, Hz | big-endian u32, no scaling (unused — RX-only driver) |
| `0x02`-`0x08` | RX1-RX7 NCO frequency, Hz | big-endian u32, no scaling |
| `0x09`-`0x11` | TX drive/Alex/CW settings | unused — RX-only driver |
| `0x12`-`0x16` | RX8-RX12 NCO frequency, Hz | big-endian u32, no scaling (note the gap: RX8 resumes at `0x12`, not `0x09`) |

Frequency byte order and units are **[CROSS-CONFIRMED]**: piHPSDR's sender
packs `output_buffer[C1]=freq>>24 … output_buffer[C4]=freq` directly from a
Hz-valued integer, matching the wiki's `DATA[31:24]=C1…DATA[7:0]=C4`
labeling of every RX/TX frequency field as "NCO Frequency in Hz" — no
scale factor.

A commonly-repeated but **wrong** claim (search-engine summaries, not
primary sources): "sample rate is in C0 bits 1-2." Contradicted by three
independent sources placing it in **C1 bits[1:0]**, gated by `ADDR=0x00`.

## Sending cadence

**[CONFIRMED mechanism, SECONDARY exact wire format]**: real clients
(piHPSDR) don't send C&C settings once at connect and stop — they cycle
through ADDR values continuously, embedded in the C0-C4 header of every
outgoing frame, for the life of the connection. This driver approximates
that by piggybacking a rotating pair of C&C USB frames on the existing
`KEEPALIVE_INTERVAL` (1s) re-send, cycling through `[general settings, RX1
freq, RX2 freq, ...]` — sufficient to converge the device's configuration
even across UDP loss, without needing sub-second precision since this
driver has no live retuning (config is static per `HpsdrDevice::open`
call).

## What's still unverified

- The exact outbound "EP2 write" Metis header format (this driver reuses
  the already-pinned 8-byte-header + two-512-byte-USB-frame packet shape
  from the inbound direction, endpoint byte `0x02`) — **not** itself
  confirmed by this research; only the *inbound* framing and the C0-C4
  field semantics carry primary-source confirmation. Same caveat class as
  `build_start_command`'s pre-existing "best-effort, not re-verified"
  status.
- Whether the very first Metis start packet's buffer needs to carry an
  embedded C&C sub-frame simultaneously, or whether the driver's
  keepalive-cadence approach (first C&C traffic ~1s after connect) is
  sufficient — untested against real hardware.
- HL2's practical receiver-count ceiling (protocol allows 12; HL2 hardware
  likely tops out at 2 for a single-ADC design) — not asserted in code;
  callers should read the discovery reply rather than assume.

Per this project's established policy (MAN-10, MAN-11), this remains
hardware-control code pending confirmation against George's (K5TR) real
HPSDR units before shipping as verified.
