# 2026-10-13 — MAN-140: a ka9q-radio (radiod) multicast IQ input

**Status:** Implemented (branch `MAN-140`). The wire facts are pinned against
ka9q-radio's documentation and headers at `main` commit
`f44b3a3ee8dd9d96be195c5de15aa04d94ffebfb` (2026-10-12), not against a live
radiod. Six facts stay open until one is reachable
([D-HW](#d-hw-what-still-needs-a-live-radiod-open)). Operator steps:
[docs/GUIDES/ka9q-radio.md](../GUIDES/ka9q-radio.md).

## Context

The ticket: manta should ingest IQ from a ka9q-radio multicast stream. Given
ka9q-radio is streaming IQ, when manta is configured to join that group, then
it decodes and spots as it does from any other receiver. It came from the
2026-09-05 broad review (C-07) and the UberSDR competitive analysis, which
found a closed-source skimmer already shipping a ka9q-radio input.

ka9q-radio's `radiod` turns one SDR into many separately tuned channels and
sends each as RTP over UDP multicast. A channel in `mode = iq` is complex
baseband, the same kind of data every other manta input delivers. Before this
change nothing in the repository referred to ka9q-radio, radiod, RTP or
multicast. Every source implements `IqSource`; only `manta-input` and
`manta-cli` name concrete sources.

No live radiod was reachable. The transport was prototyped in the container
(a scratch test, reverted) and the protocol facts read from upstream:

| What | Result |
|---|---|
| Today's answer to a `type = "ka9q"` config | `config check` exit 1, ``unknown field `group` `` |
| Loopback multicast, two receivers with `SO_REUSEADDR` + `SO_REUSEPORT` | both got every datagram, bound to the group or to `0.0.0.0` |
| Receiver joined on one interface, sender on the other | joined only on 127.0.0.1 with the sender on the default interface: timed out; the reverse: timed out; joined on both: received from either |
| A socket bound to `0.0.0.0:5004` that joined only `239.7.7.2` | also received `239.7.7.1`'s datagram: Linux delivers every joined group on that port. A group-bound socket got only its own |
| Prototype: fake radiod sends one status packet, then 30 s of V1 as RTP (f32le, 180 samples per 1452-byte packet) to a loopback group | status parsed to ssrc 14000000, 96000 Hz, 2 channels, encoding 4, PT 111, 14000000.0 Hz; 2 880 000 samples received bit-identical to the WAV, 0 sequence gaps, at 4x and 1x real time; a second receiver on the same group and port also got all 2 880 000 |
| Receive buffer, 4 MiB requested | Linux granted 425 984 bytes (default `net.core.rmem_max`, doubled) |
| A second receiver left undrained | kept 33 300 samples (0.35 s at 96 kS/s, 0.17 s at 192 kS/s) and lost the rest |
| `socket2` | 0.6.5 already in `Cargo.lock` through tokio; no new download |
| Tag stability, upstream | the 18 tags checked, every one this source uses among them, have the same value at all 25 commits touching `src/status.h` since 2025-09-02 |
| Encoding stability, upstream | `enum encoding` in `src/rtp.h` stable since `cff32fd6f5` (2026-01-10); from `d0d8f2910e` (2026-01-07) to 2026-01-10, values 4 and 6 meant F32BE and F16BE |

Two claims in the ticket's research were wrong. The RTP payload type is not
the encoding ordinal: `src/rtp.h` maps (sample rate, channels, encoding) to a
payload type through a table, so manta takes the expected payload type from the
status packet's `RTP_PT` tag and never derives it. And SPEC §9's key lines are
checked by `spec_documents_every_input_and_spot_key`; only its per-type
summary comment is not.

## Decisions

**D1 — Scope and provenance.** manta is a passive receiver of one existing
radiod IQ channel, as the ticket's "configured to join that group" reads.
ka9q-radio is GPL-3.0 and manta is MIT OR Apache-2.0, so the source is written
from the protocol facts recorded here: documented behaviour, enum values, wire
types and port numbers. No ka9q-radio code is copied or translated. This is the
clean-room rule set for this input on 2026-10-11 (UberSDR competitive analysis,
§10 decision 3: "clean-room from the protocol description").

**D2 — No Cargo feature.** The source is compiled into every build, like
KiwiSDR. It needs only `socket2`, pure Rust and already in `Cargo.lock`. A
feature would double every source flag's `conflicts_with_all` `cfg_attr`
matrix from four variants (hpsdr × soapy) to eight. Ungated, it ships in the
release archives and the Docker image with no packaging change. HPSDR's gate
was kept "for symmetry"; that reason does not outweigh the matrix cost.

**D3 — Format comes from the channel's status packet.** There are no operator
keys for rate, encoding or frequency. radiod sends each channel's status every
25 frames (about 2 Hz, `update = 25`, its documented default) to the channel's
own data group on port 5006 (`docs/ka9q-radio-3.md`; `src/radio.c` sets the
status destination to the data destination, then the port to
`DEFAULT_STAT_PORT`). A receiver that knows the data group needs nothing from
radiod's control group. If per-channel status is off (`update = 0`), open
fails with the no-status message.

**D4 — Pinned wire table.** Tag and encoding values are those of ka9q-radio
`main` at `f44b3a3ee8dd` ([below](#wire-format)). Integers are decoded by
length (0 to 8 bytes) and floats by length (0, 4 or 8 bytes), so a float/double
change like upstream's 2026-10-02 "change most doubles back to floats"
(`3346dde5e3`) still parses. radiod builds from 2026-01-07 to 2026-01-10 sent
4 and 6 for big-endian floats; manta reads those values as F32LE and F16LE, so
the guide tells operators with a radiod from that window to update it.

**D5 — Encodings.** S16LE, S16BE, F16LE, F16BE, F32LE and F32BE are read.
16-bit integers are divided by 32768.0, as `WavIqSource` does. Half floats use
a small bit-level `f16` to `f32` conversion with its own tests; `f16` is not
stable in std. A packet with any non-finite sample is malformed and discarded
whole, so one spoofed NaN cannot poison the channelizer and the floor history.
Opus, Opus VoIP, AX.25, µ-law and A-law are refused with a message naming the
encoding.

**D6 — Open-time validation**, in this order, each with its own message: two
output channels; envelope not on; independent sideband not on; encoding
readable; rate above 0 and at most `MAX_IQ_WAV_RATE_HZ` (10 MS/s, the cap MAN-269
put on IQ WAVs because a shape-valid absurd rate made `FloorBank::new` ask for
3.36 GB; a rate from the network is peer-controlled in the same way); rate
equal to 93.75 × 2^k with k ≥ 2, the channelizer's rule, checked early so the
message names radiod's `samprate`; finite frequency above 0; the status's data
port equal to the configured port.

**D7 — Choosing the channel.** With `ssrc` set, only that SSRC's status
counts. Without it, the source waits 1.5 s (three status periods) after the
first status packet, then uses the one SSRC it saw, or fails listing every SSRC
with its frequency, rate and encoding. Many channels sharing one group is
normal (wsprdaemon groups). At most 64 channels are remembered while choosing,
so status packets with ever-new SSRCs cannot grow memory during the wait; past
that the message says "64 or more".

**D8 — Open timeout: 10 s**, the same as the 40 × 250 ms stall bound HPSDR and
KiwiSDR use.

**D9 — Sockets** (`socket2`, workspace dependency
`socket2 = { version = "0.6", features = ["all"] }`). For each of the data and
status sockets: `SO_REUSEADDR`, plus `SO_REUSEPORT` on Unix, so manta coexists
with radiod's own tools and with other manta processes. Bind the group address
and port on Unix; bind `0.0.0.0` and the port elsewhere (Windows cannot bind a
multicast address) and rely on the SSRC filter. Join on the configured
interface, or on `0.0.0.0` (the kernel's choice) when none is set, and also on
127.0.0.1 unless the configured interface is 127.0.0.1. radiod's default is
`ttl = 0`, which "keeps multicast traffic from even leaving the system", and
with TTL 0 "the internal loopback interface is used automatically"
(`docs/ka9q-radio.md`); the interface runs above show a receiver joined only
on the default interface misses loopback traffic. With no interface
configured, a failed default join is tolerated if the loopback join succeeded;
a configured interface that cannot be joined is an error naming it, and so is
every join failing. Request a 4 MiB `SO_RCVBUF` and accept a smaller grant; the
guide tells operators to raise `net.core.rmem_max`, since Linux grants 416 KiB
by default. One reused 64 KiB receive buffer, for HPSDR's `RECV_BUF_LEN`
reason. While `open` waits for status it also drains the data socket —
non-blocking, to empty, between status receives that wait at most 10 ms
(`WAIT_POLL`) — into an app-side stash that keeps the newest 4 MiB (each
packet charged its length plus 64 bytes, so a flood of empty datagrams is
bounded too), dropping its own oldest packets past that, and replays the stash through `accept_rtp`
once the channel is chosen. The drain never stops, so no `validate`-accepted
rate can overflow the kernel during the wait (Linux's default grant holds
~70 ms of 768 kS/s `f32le`), and keeping the newest packets means the replay
runs straight into the socket's next packet with no gap; only how much
startup history is kept is bounded (PR review round 4 replaced a stash that
stopped draining at 4 MiB, which a 768 kS/s channel overflows in the 1.5 s
settle window). (Added in PR review round 3: without the drain, a grant smaller than
the no-SSRC settle window overflows while nothing reads the socket — Linux's
default holds ~0.17 s at 192 kS/s `f32` against a 1.5 s window — and the
first `read` reports the loss as a startup gap). The sockets still join
before the wait, so RTP queues in the kernel during it exactly as the plan's
open order requires; the stash extends that queueing past the kernel grant.

**D10 — Read path.** Pull-based, no thread. Each `read()` first drains the
non-blocking status socket, then blocks on the data socket with a 250 ms
timeout until its channel's queue has samples, then returns what is queued
(HPSDR's shape). While the queue still holds samples — above all the startup
stash replay, up to 1.5 s of samples — each `read()` also accepts every packet
already waiting in the data socket without blocking, up to the largest
possible replay (4 MiB of 4-byte samples) plus 3 s of samples (`QUEUE_CAP_S`), so the kernel's grant (~0.27 s at 192 kS/s `f32`)
cannot overflow while the replay is consumed; past the cap the kernel's own
overflow rule applies, so a reader slower than real time cannot grow memory. Every non-blocking drain pass — status in `read()`, RTP while samples are
queued, and the startup stash — takes at most 256 datagrams (`DRAIN_BATCH`)
before returning, so a flood that keeps a socket readable cannot starve the
stall deadline or the open timeout. A wall-clock stall deadline, 10 s since the channel's last
accepted packet, turns silence, or a flood of other channels' or malformed
traffic, into an error. A count bound alone would never fire under a
continuous foreign flood.

**D11 — Counters and liveness.** `confirmed_live_handle()` is `Some`, false
until the first accepted RTP packet, so `run` starts the source unhealthy, as
for HPSDR. `health_counters()` is `Some`. RTP sequence numbers are compared as
a signed 16-bit difference `d`:

- `d == 1`: in order.
- `2 ≤ d ≤ max_jump`: one gap and `d − 1` dropped packets. `max_jump` is the
  number of packets in 10 s at the channel's rate and first packet size,
  capped at 32767.
- `d > max_jump`, or `d < −100`: the stream restarted (a radiod restart);
  re-baseline, accept the packet, count nothing.
- `−100 ≤ d ≤ 0`: duplicate or late; discard it, count nothing.

Malformed, counted and discarded: an RTP datagram shorter than its header, a
version other than 2, a bad padding or extension length, a payload that is not
a whole number of IQ pairs, the wrong payload type for its channel, or a
non-finite sample; and a status datagram whose TLV list is truncated or
over-long. Another SSRC's packets are ignored and not counted.

**D12 — Lost packets are spliced** (no zero-fill), as for HPSDR and KiwiSDR.
Zero-filling a gap inside a mark splits the element; splicing only shortens it,
and the 25th-percentile floor gotcha (`wiki/pages/source-reconnect.md`) rules
out long zero runs anyway. A stall longer than 10 s becomes a reconnect, and
`ReconnectingSource` reports that outage through `take_discontinuity()`. The
source does not override `take_discontinuity()`.

**D13 — A retune or format change ends `read()`.** If the channel's status
reports a different frequency (by more than 1 mHz), rate, encoding, channel
count, payload type, envelope setting, independent-sideband setting or filter
edges (by more than 1 mHz, compared only when both statuses carry usable
edges, so D14's passband never goes stale) than at
open (an absent envelope/ISB tag counts as off, as at open, because a switch
to those modes keeps the channel count and payload type while the second
component stops being Q), `read()` returns an error naming the old and
new values. `ReconnectingSource` reopens; if the frequency, rate or
passband differs, its check ends the run with exit 1, and a supervisor
(systemd `Restart=always`, Compose `restart: unless-stopped`) starts it again
at the new frequency. Following a retune live would mean changing the centre
frequency under the running engine, SpotBus and SETT, which nothing supports.
An encoding-only change reopens cleanly. With `--dial-freq-hz` or
`input.center_freq_hz` set, the override is moved by radiod's own change since
the first open, so the reopen check still sees the retune and ends the run;
otherwise the override would hide it and spots would carry the old frequency
(`ka9q_retune_ends_run_even_with_a_dial_override`).

**D14 — Passband from the filter edges.** `rf_passband_hz()` returns
`(max(LOW_EDGE, −fs/2), min(HIGH_EDGE, fs/2))` when both edges are present
and `LOW_EDGE < HIGH_EDGE`, else the trait default ±fs/2. ka9q-radio's `iq`
preset is ±5 kHz at 12 kS/s (`config/presets.conf`), narrower than Nyquist, so
SETT coverage and `manta check` must follow the spectrum actually delivered.

**D15 — Names.** Metrics and log label `ka9q` (`LiveSourceSpec::name()`),
`type = "ka9q"`, `InputKind::Ka9q`, flags `--ka9q-group`, `--ka9q-port`,
`--ka9q-ssrc` and `--ka9q-interface` under the help heading `ka9q-radio
source`, `ReceiverKind::Ka9q` labelled `ka9q-radio`, module
`manta_input::ka9q`. The group gets its own key and flag, not a reused
`host`, because it is a multicast group many receivers join, not a host manta
connects to.

**D16 — Config keys.** `group` (new, required, string), `port` (shared with
kiwi and hpsdr; ka9q default 5004), `ssrc` (new, optional, 0 to 4294967295),
`interface` (new, optional, IPv4 literal). `group` is checked at load: an IPv4
multicast literal, or a syntactically valid host name (letters, digits, `-`
and `.`, labels of 1 to 63 characters), so a guide placeholder like
`"<your-radiod-data-group>"` is refused. An IPv6 literal is refused with its
own message. A name is resolved at open, by the system resolver only. `group`
and `interface` join `STRING_TYPED_ENV_KEYS`.

**D17 — calibrate.** `calibrate --tune-hz` refuses a ka9q source with a
message saying to retune the channel in radiod. `calibrate` without
`--tune-hz` works unchanged.

**D18 — AGC warning.** ka9q-radio's `iq` preset turns AGC on. With AGC on, one
strong signal changes the gain for every signal in the passband. manta still
starts, since some operators run AGC on purpose, but `LiveSourceSpec::open`
prints a `warning:` line when the status packet has `AGC_ENABLE = 1`, on each
open, as the audio digital-silence notice does.

**D19 — Tests that open sockets are gated to Linux and macOS**, with
`#[cfg(any(target_os = "linux", target_os = "macos"))]`. CI also runs
`cargo test --workspace` on `windows-latest`, where loopback multicast is
unverified. The wire parsers and the config are tested on every OS. If the
macOS CI leg fails only on loopback multicast, that one gate narrows to
`linux` and this record says why; any other macOS failure is a bug to fix.

**D20 — Library knobs for tests.** `Ka9qConfig` has public `status_port`
(default 5006), `open_timeout`, `settle` and `stall_timeout` fields, so
`manta-input`'s tests use free ports and short timeouts. The CLI and TOML
expose none of them: radiod always sends channel status to port 5006, so
CLI-level tests use 5006 on a group unique to the test, which the reuse
options make safe.

## Wire format

Pinned at ka9q-radio `f44b3a3ee8dd`. Every tag used here kept its value across
all 25 commits that touched `src/status.h` since 2025-09-02.

| Tag | Value | Wire type (radiod) | Use |
|---|---|---|---|
| `EOL` | 0 | — | ends the TLV list |
| `OUTPUT_DATA_DEST_SOCKET` | 17 | socket (6 bytes IPv4) | its port must equal the configured data port |
| `OUTPUT_SSRC` | 18 | int (≤ 4 bytes) | channel id |
| `OUTPUT_SAMPRATE` | 20 | int | `sample_rate()` |
| `RADIO_FREQUENCY` | 33 | double | `center_freq_hz()` |
| `LOW_EDGE` / `HIGH_EDGE` | 39 / 40 | float | `rf_passband_hz()`, clamped to ±fs/2 |
| `OUTPUT_CHANNELS` | 49 | int | must be 2 |
| `INDEPENDENT_SIDEBAND` | 50 | bool | must not be 1 |
| `ENVELOPE` | 56 | bool | must not be 1 |
| `AGC_ENABLE` | 62 | bool | 1 gives the warning |
| `RTP_PT` | 105 | int (1 byte) | expected RTP payload type |
| `OUTPUT_ENCODING` | 107 | int | sample decoder |

**Ports.** RTP data 5004 (`DEFAULT_RTP_PORT`); channel status 5006
(`DEFAULT_STAT_PORT`), sent to the data group.

**Status packet.** Byte 0 is `0` (STATUS) or `1` (CMD); CMD packets are
ignored. Then TLVs: a tag byte, then a length. A length of 0 to 127 is the
byte count; with the high bit set, the low 7 bits count the big-endian length
bytes that follow (`82 01 02` is 258, the documentation's own example).
Integers are MSB-first with leading zeros stripped, and length 0 means 0.
Floats are IEEE-754 big-endian. Sockets are 6 bytes (IPv4 address and port),
18 (IPv6), or 0 for a null socket. Source: `docs/ka9q-api.md` v0.1,
2026-01-09.

**Encodings** (`enum encoding`, `src/rtp.h`): 0 none, 1 S16LE, 2 S16BE,
3 Opus, 4 F32LE, 5 AX.25, 6 F16LE, 7 Opus VoIP, 8 F32BE, 9 F16BE, 10 µ-law,
11 A-law. manta reads 1, 2, 4, 6, 8 and 9.

**RTP.** RFC 3550, version 2: a 12-byte header, then 4 × CC CSRC bytes, then a
header extension if X is set (4 bytes plus 4 × its length word), and padding
if P is set (the last byte gives its length). Payload: interleaved I, Q per
sample. I comes first: radiod's linear demodulator keeps each IQ sample as a C
`float complex` and sends the buffer cast to `float *` (`src/linear.c`), and C
stores the real part first. That is source evidence, not an observation (D-HW
1).

## D-HW: what still needs a live radiod (open)

Spec reading and a fake radiod do not settle these. Each is **OPEN** until it
is checked against a real radiod, as the
[HPSDR spike's "Needs real hardware"](2026-09-02-hpsdr-hermes-protocol-spike.md)
list was for that input.

1. **OPEN** — I comes before Q on the wire. Strong source evidence, not
   observed.
2. **OPEN** — The radiod channel section in the guide (`low`/`high` at
   ±90 kHz, `agc = off`, `encoding = f32le`) behaves as described.
3. **OPEN** — How soon a freshly created channel sends status, against the
   1.5 s settle window (D7).
4. **OPEN** — The sustained packet-loss rate of a 192 kS/s f32 channel on a
   Raspberry Pi 4, with the default and a raised `net.core.rmem_max`.
5. **OPEN** — Behaviour through IGMP-snooping switches, and across hosts with
   `ttl = 1`.
6. **OPEN** — A channel at `encoding = s16be`, radiod's default, decodes as
   well as one at `f32le`.

The `#[ignore]` test `reads_a_live_radiod_channel` in `manta-input` reads
`MANTA_TEST_KA9Q_GROUP` (and optionally `MANTA_TEST_KA9Q_SSRC`) and is the
starting point for items 1 to 3.

## Consequences

- An operator with radiod can feed manta from any SDR radiod drives, with no
  SDR driver in manta. Every build has the input: release archives and the
  Docker image pick it up with no packaging change.
- One channel per manta process, as for every other input. Several manta
  processes can read one group.
- A radiod retune ends the run with exit 1; the packaging kit's supervisor
  restarts it at the new frequency.
- A channel with AGC on runs, with a warning on each open.
- No new metric family. The existing per-source series gain the label value
  `source="ka9q"`: `manta_source_health` (0 until the first RTP packet of the
  channel, then 1) and `manta_input_dropped_packets_total`,
  `manta_input_gaps_detected_total` and `manta_input_malformed_packets_total`,
  counted from RTP sequence numbers and discarded datagrams.
- A `.local` group name needs mDNS on the host (Avahi with nss-mdns on Linux).
  The Docker image has none, so there the group is given as an address, and
  the container needs `network_mode: host`.
- With Linux's default `net.core.rmem_max`, a stalled reader has well under a
  second of slack at 192 kS/s; the guide says to raise it and to watch the
  dropped-packet counter.
- The file path is unchanged, so decoder output is unchanged: no
  `### Decoder output` CHANGELOG entry.

## Follow-ups

- Creating and retuning radiod channels through its control group (`CMD`
  packets, `_ka9q-ctl._udp` discovery), as UberSDR's skimmer does.
- A fallback for hosts without nss-mdns (the Docker image): ka9q-radio's own
  name-to-address hash (`make_maddr`, FNV-1 into 239.0.0.0/8), so a `.local`
  name works without a resolver.
- IPv6 groups.

The D-HW checks above are owed to hardware, as for HPSDR; they do not block
this change.

## References

- Upstream (GPL-3.0, read for facts only):
  [ka9q-radio at `f44b3a3ee8dd9d96be195c5de15aa04d94ffebfb`](https://github.com/ka9q/ka9q-radio/tree/f44b3a3ee8dd9d96be195c5de15aa04d94ffebfb):
  `docs/ka9q-api.md` (status and TLV format), `docs/ka9q-radio.md` (`ttl`,
  `iface`, DNS), `docs/ka9q-radio-3.md` (`update`, `encoding`, `agc`, `gain`),
  `config/presets.conf` (`[iq]`), `src/status.h` (`enum status_type`,
  `enum pkt_type`), `src/rtp.h` (`enum encoding`, ports, payload-type table),
  `src/radio.c` (status destination), `src/radio_status.c` (each tag's wire
  type), `src/linear.c` (I/Q order).
- Code: `crates/manta-input/src/ka9q/` (`wire.rs`: status, RTP, IQ payloads,
  sequence tracking; `mod.rs`: `Ka9qConfig`, `Ka9qIqSource`, `check_group`);
  `crates/manta-cli/src/main.rs` (`LiveSourceSpec::Ka9q`, the `--ka9q-*`
  flags); `crates/manta-cli/src/config.rs` (`InputKind::Ka9q`).
- Tests: `crates/manta-input/src/ka9q/` unit and fake-radiod tests;
  `crates/manta-cli/tests/ka9q_input.rs` (a fake radiod streams V1 and a real
  `manta run` spots W1AW at the file replay's frequency).
- Operator guide: [docs/GUIDES/ka9q-radio.md](../GUIDES/ka9q-radio.md).
- Precedents: [2026-09-02 HPSDR protocol spike](2026-09-02-hpsdr-hermes-protocol-spike.md),
  [MAN-56 input health metrics](2026-09-04-man56-input-health-metrics.md),
  [MAN-73 source reconnect](2026-10-05-man73-source-reconnect.md),
  [MAN-269 paced replay and the 10 MS/s IQ ceiling](2026-10-10-man269-paced-looping-replay.md).
