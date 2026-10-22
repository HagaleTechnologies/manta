# 2026-10-19 — MAN-305: protocol-2 openHPSDR radios are inputs; the protocol is discovered, and lost datagrams reach the decoder as missing input

**Status:** Implemented (branch `MAN-305`) against a fake protocol-2 radio in
the tests. No protocol-2 radio was reachable; seven facts stay open until one
is ([D-HW](#d-hw-settled-only-with-a-protocol-2-radio-on-the-lan-man-52)),
owned by MAN-52. This record supersedes the protocol-2 notes in
[2026-09-02-hpsdr-hermes-protocol-spike.md](2026-09-02-hpsdr-hermes-protocol-spike.md)
(its lines 146-159), which are not rewritten. Operator steps:
[docs/CONFIG.md](../CONFIG.md).

## Context

Before this change `--hpsdr-host` spoke only openHPSDR protocol 1 ("Metis":
Hermes-Lite 2 and older boards, `crates/manta-input/src/hpsdr.rs`). ANAN-class
radios (Hermes, Angelia, Orion, Orion II, Saturn/G2) speak protocol 2, which
shares no framing with protocol 1. The ticket had three scenarios:

1. A protocol-2 radio streams IQ at 192 kHz and manta spots CW from it.
2. With no protocol given, manta finds out which protocol the radio speaks.
3. 50 lost datagrams are counted exactly and reach the decoder as missing
   input, not as silence or as a closed track.

Two findings shaped the work. Protocol 2 numbers every datagram, so loss is
exact, but the engine had no way to hear about a short loss without closing
every track. And two wrappers that every live run goes through
(`ReconnectingSource`, `DecimatingSource`) swallowed the inner source's
`take_discontinuity()`, so no live source could report a gap at all
(reproduced, see "What I ran").

## Decisions

**D1 — One source type, one label; the protocol is an attribute.**
`type = "hpsdr"`, `--hpsdr-*` and `source="hpsdr"` cover both protocols.
`crates/manta-input/src/hpsdr.rs` keeps protocol 1 in place and gains
`hpsdr/discovery.rs`, `hpsdr/proto2/mod.rs` (the source) and
`hpsdr/proto2/wire.rs` (pure builders and parsers, as `ka9q/wire.rs` is).
Dashboards, alerts and configs keep working, and the protocol-1 diff stays
readable.

**D2 — Protocol selection: `HpsdrProtocol { Auto, One, Two }`, default
`Auto`.** `hpsdr::open_receiver(cfg, protocol)` returns `OpenedHpsdr { source,
protocol, discovery }`, with `Discovery { Answered(reply), NoAnswer, Skipped }`.
The CLI prints the startup lines from it; logging stays in the CLI.
`HpsdrDevice::open` keeps its signature for protocol 1. There is a
user-visible override (`--hpsdr-protocol`, `[input] protocol`,
`MANTA_INPUT_PROTOCOL`) because a radio whose discovery answer never reaches
manta needs a way in, taking over a busy radio needs an explicit act, and a
protocol-1 operator who wants no extra datagrams and no extra startup time
keeps today's behaviour exactly with `1`.

**D3 — Discovery is a directed probe of the configured host only**, run
inside `open_receiver` under `Auto` on every open and reopen.

- From a fresh socket on `0.0.0.0:0`, closed before streaming starts, send to
  `host:port` both the protocol-2 request (60 bytes, all zero except byte 4 =
  `0x02`) and the protocol-1 request (63 bytes: `EF FE 02` then 60 zeros), at
  t = 0, 250, 500 and 750 ms; wait until t = 1 s.
- Accept replies only from `host`'s address and source port `port`. Bytes 0–1
  = `EF FE` with byte 2 ∈ {2, 3} is protocol 1 (status byte 2, MAC 3–8,
  firmware 9, board 10). Otherwise, length ≥ 21 with byte 4 ∈ {2, 3} is
  protocol 2 (status byte 4, MAC 5–10, board 11, protocol version 12,
  firmware 13, DDC count 20). Anything else is ignored.
- The first protocol-2 reply ends discovery. A protocol-1 reply opens a
  100 ms window in which a protocol-2 reply still wins, because dual-firmware
  radios may answer both; the first protocol-1 reply also resends the
  protocol-2 request once, since the window closes before the next try and a
  single lost protocol-2 datagram would otherwise settle on protocol 1. No
  reply by 1 s: protocol 1, with a warning.
- Protocol-2 status `0x03` (already streaming) is refused under `auto`.
  Protocol-1 status is not acted on.
- `1` and `2` send no discovery datagram.
- MAN-239's invariant holds: discovery runs only when an hpsdr source is
  being opened, and never broadcasts.

Four tries cover the loss of single datagrams; 1 s bounds the added startup
time, and on a LAN the answer arrives in about a millisecond. Rerunning on
each reconnect finds a radio powered on later in the right protocol.

**D4 — The protocol-2 port map is relative to `port` (default 1024).** Host
→ radio: general and discovery `port`, rx-specific `port+1`, tx-specific
`port+2`, high-priority `port+3`. Radio → host source ports: command response
`port`, high-priority status `port+1`, mic `port+2`, wideband `port+3` to
`port+10`, DDC n IQ `port+11+n`. With 1024 this is the published map
(1024–1027 out, 1035 for DDC0). A fake radio in tests binds any free base, so
no test uses a fixed port.

**D5 — One unconnected streaming socket**, built with `socket2`: bind
`0.0.0.0:0`, request `SO_RCVBUF` = 4 MiB (a smaller grant is not an error),
250 ms read timeout, 65,536-byte receive buffer. `recv_from` demultiplexes by
source address and port per D4. Another address is malformed; a radio port
inside the D4 map other than `port+11` is read and ignored; a radio port
outside the map is malformed. `ErrorKind::ConnectionReset` from `recv_from` is
skipped and the read retried: on Windows an unconnected UDP socket reports an
earlier ICMP port-unreachable that way. A `connect`ed socket would drop the
IQ, because the radio sends it from a port the host never sent to.

**D6 — Bring-up datagrams and order.** Each packet starts with its own 32-bit
big-endian per-port sequence number at bytes 0–3, counting from 0. Bytes not
listed are 0.

| Packet → port | Length | Bytes set |
|---|---|---|
| general → `port` | 60 | `[4]=0x00`; `[37]=0x08` (frequencies are phase words); `[38]=0x01` (radio watchdog on); `[58]=0x00` (PA off); `[59]` = filter-board enable per D7 |
| rx-specific → `port+1` | 1444 | `[4]` = ADC count per D7; `[5]=0x00` dither off; `[6]=0x00` random off; `[7]=0x01` (DDC0 only); `[17]=0` (DDC0 ← ADC0); `[18..20]` = rate in kHz, BE u16 (192 kHz → `00 C0`); `[22]=24` bits per sample |
| tx-specific → `port+2` | 60 | `[4]=1` (one DAC), as piHPSDR sends it to receive-only setups |
| high-priority → `port+3` | 1444 | `[4]=0x01` (run; PTT bit 1 never set); `[9..13]` = DDC0 phase word BE u32 = ⌊f·2³²/122 880 000⌋; `[329..333]` = the same word (DUC, so firmware band logic sees the receive band); `[345]=0` drive; `[1428..1432]` = alex1 BE u32; `[1432..1436]` = alex0 BE u32; `[1442]=[1443]=0` (0 dB step attenuators) |

General, rx-specific and tx-specific are each sent twice, then high-priority
with run = 1 twice, with waits of 100 ms after general and 50 ms after each of
rx-specific and tx-specific. Each open costs 200 ms. Pinned phase words:
7,030,000 Hz → `0E A5 55 55`; 14,000,000 Hz → `1D 2A AA AA`. piHPSDR's general
packet leaves byte 4 at 0 (an external summary said otherwise; the source
wins), and Thetis writes 0 too.

**D7 — Board table, ADC count and filter words**, keyed by discovery byte 11.
Filter bits: bypass HPF / ANAN-7000 RX bypass BPF `0x1000` (the same bit);
HPFs `0x40` 1.5 MHz, `0x20` 6.5 MHz, `0x10` 9.5 MHz, `0x02` 13 MHz, `0x04`
20 MHz, `0x08` 6 m preamp; LPFs `0x0080_0000` 160 m, `0x0040_0000` 80 m,
`0x0020_0000` 60/40 m, `0x0010_0000` 30/20 m, `0x8000_0000` 17/15 m,
`0x4000_0000` 12/10 m, `0x2000_0000` 6 m/bypass; BPFs `0x40` 160 m, `0x20`
80/60 m, `0x10` 40/30 m, `0x02` 20/15 m, `0x04` 12/10 m, `0x08` 6 m.

| Byte 11 | Name | ADC count | `[59]` | alex0 (f = DDC0 frequency) | alex1 |
|---|---|---|---|---|---|
| 0 | Atlas | 1 | 0 | 0 | 0 |
| 1 | Hermes | 1 | 0x01 | HPF(f) \| LPF(f) | LPF(f) |
| 2 | Hermes II | 1 | 0x01 | HPF(f) \| LPF(f) | LPF(f) |
| 3 | Angelia | 2 | 0x01 | HPF(f) \| LPF(f) | LPF(f) |
| 4 | Orion | 2 | 0x01 | HPF(f) \| LPF(f) | LPF(f) |
| 5 | Orion II | 2 | 0x03 | BPF(f) \| LPF(f) | `0x1000` \| LPF(f) |
| 6 | Hermes Lite | 1 | 0 | 0 | 0 |
| 10, 11 | Saturn | 2 | 0x03 | BPF(f) \| LPF(f) | `0x1000` \| LPF(f) |
| 20 | G2E | 1 | 0x01 | BPF(f) \| LPF(f) | LPF(f) |
| other / skipped | unknown board | 1 | 0x01 | `0x2000_1000` (bypass) | `0x2000_0000` |

Ladders (Hz):

- **HPF(f):** < 1.8 M bypass; < 6.5 M 1.5 MHz; < 9.5 M 6.5 MHz; < 13 M
  9.5 MHz; < 20 M 13 MHz; < 50 M 20 MHz; else 6 m preamp.
- **LPF(f):** > 35.6 M 6 m/bypass; > 24 M 12/10; > 16.5 M 17/15; > 8 M
  30/20; > 5 M 60/40; > 2.5 M 80; else 160.
- **BPF(f):** < 1.5 M bypass; < 2.1 M 160; < 5.5 M 80/60; < 11 M 40/30;
  < 22 M 20/15; < 35 M 12/10; else 6 m.

Pinned vectors: Orion at 7.03 MHz → alex0 `0x0020_0020`, alex1
`0x0020_0000`; Orion at 14.0123 MHz → alex0 `0x0010_0002`; Orion II at
7.03 MHz → alex0 `0x0020_0010`, alex1 `0x0020_1000`.

Filters are set because on pre-Orion-II boards the receive path from ANT1–3
runs through the TX low-pass filter, and a zero word risks selecting no path.
An unknown board gets bypass because both bypass bits are valid on both
filter schemes and bypass never removes the band being received. Board
numbers 3/4/5 rest on piHPSDR and sdroxide-hpsdr agreeing (D-HW 1).

**D8 — Keepalive and stop.** `read()` drives the resends: high-priority every
100 ms, rx-specific and tx-specific every 200 ms, general every 800 ms
(piHPSDR's cadence, the most frequent of the clients examined, because the
radio's watchdog timeout is unpublished). Dropping a `Protocol2IqSource` sends
high-priority with run = 0 twice, so the radio stops sending ~9.6 Mbit/s to a
closed port and a reconnect does not race the old stream. Send errors are
ignored. Protocol 1 still sends no stop.

**D9 — The IQ datagram** is exactly 1444 bytes from `port+11`: `[0..4]` BE u32
sequence; `[4..12]` timestamp (parsed and ignored; semantics unverified);
`[12..14]` BE u16 bits per sample, must be 24; `[14..16]` BE u16 samples per
frame, must be 238; `[16..1444]` 238 pairs of 24-bit BE two's-complement I
then Q. Any mismatch is malformed. Samples are scaled by 1/2²³, the factor
every reference client uses.

**D10 — Sequence accounting** (`SeqTracker` in `proto2/wire.rs`). The first
datagram is the baseline; `expected = last + 1` (wrapping);
`delta = seq.wrapping_sub(expected) as i32`.

- `delta == 0`: in order, including the `0xFFFF_FFFF → 0` wrap.
- `seq == 0 && expected != 0`: restart (the radio began a new stream).
- `delta > 0`: a gap of `delta` datagrams (`record_dropped(delta)`,
  `record_gap()`).
- `-64 ≤ delta < 0`: stale (late or duplicate, already counted as lost),
  discarded and not counted again. Eight stale in a row is a restart.
- `delta < -64`: restart.
- A restart records one gap, no dropped count (the true count is unknown),
  and takes this datagram as the new baseline.

The naive `seq.wrapping_sub(last) as i64` turns a radio restart into ~4.29 ×
10⁹ lost datagrams; `restart_is_not_four_billion_losses` pins the fix.
A forward jump larger than the wall clock since the last valid datagram
allows (that time's datagrams plus 1%, plus 8,192 for one full socket
buffer) is also a restart, so one corrupt or spoofed sequence number cannot
move the spot clock by days (self-review finding;
`an_impossible_forward_jump_is_a_restart_not_a_loss`).

**D11 — How gaps reach the decoder.** With `missing = delta × 238`:

- `missing < sample_rate / 2` (0.5 s; under 404 datagrams at 192 kHz) is
  added to a pending `take_missing_samples()` value;
- otherwise it is added to a pending `take_discontinuity()` value;
- a restart reports `take_discontinuity()` with the wall-clock estimate
  `max(0, round(elapsed_since_last_valid_datagram × fs) − 238)`, so the clock
  stays true across a watchdog stop and restart. "Valid" here (and in D10's
  plausible-jump bound) means accepted: stale datagrams move only the D13
  stall clock, so a restart after seven stale ones still measures the whole
  outage (PR #357 review; `a_restart_after_stale_datagrams_measures_the_whole_outage`);
- `acquisition_start_offset()` (MAN-259) is the time from entering
  `open_receiver` (the `auto` probe included) to immediately before the
  first run = 1, so a reconnect counts the probe and the 200 ms bring-up as
  outage rather than shifting later spots early (PR #357 review). Protocol 1
  under `auto` adds the probe time to its own offset the same way;
- when a gap is found after `read()` has filled n > 0 samples in the current
  call, `read()` returns n at once and holds the new datagram for the next
  call, so a report always describes the samples that call returns.

Every measured clock-advance case up to a 0.5 s hole kept the spot. Beyond
that a splice joins unrelated transmissions, and a reconnect-style segment
reset is the established answer (MAN-73).

**D12 — The engine hook and the wrappers.** `IqSource` gains a defaulted
method after `take_discontinuity`:

```rust
fn take_missing_samples(&mut self) -> Option<u64> { None }
```

It reports samples lost immediately before the most recent `read()`'s
samples, which the source spliced over. `manta_engine::listen` reads it ahead
of `take_discontinuity` at all four read sites (single-input calibration and
main loop, multi-input calibration and main loop) and adds it to `seg_base`.
In the main loops the add lands between reads, so it re-times nothing already
buffered. In the calibration fills (PR #357 review) the loss is recorded at
the fill offset where it happened and the fill is processed in pieces split
at each recorded loss, `seg_base` advancing between pieces exactly as the
main loop advances it between reads -- so events originating in the pre-gap
part of a fill keep their pre-loss timestamps instead of all being mapped
from the post-gap base. A discontinuity that discards a partial fill puts
that part's recorded losses onto the clock with the discarded samples.
The engine maps hop index *m* to `seg_base + m·hop` when it emits events, so
advancing `seg_base` re-times every later event, including those of tracks
begun before the gap, and closes nothing. `check` and `calibrate` ignore it:
a spliced loss is not a discontinuity, the estimators splice, and the loss
counters already show it.

Every wrapper between a live source and `listen` forwards both hooks, with
tests: `FixedCenterFreqSource` (`main.rs`), `PacedSource` and `LoopingSource`
(`replay.rs`), doctor's `SampleBoundedSource`, `DecimatingSource` (scaled to
the output rate as `(carry + k·fs_out) / fs_in` with the remainder carried,
and the filter history reset on a discontinuity through a new
`Decimator::reset` in `manta-dsp`), and `ReconnectingSource`, which after each
successful inner read adds the inner `take_discontinuity()` to its own pending
outage and forwards `take_missing_samples()`. The last two fix the reproduced
swallowing.

A second hook rather than a threshold inside `listen`, because only the
source knows whether samples were lost (numbered datagrams) or never existed
(an outage). Keeping `take_discontinuity`'s "close tracks" meaning leaves
every existing caller as it was.

**D13 — Liveness and stalls.** `confirmed_live` flips on the first valid IQ
datagram, never on a discovery reply or a status datagram (MAN-55). A stall is
10 s since the last valid IQ datagram, the same budget as protocol 1's
40 × 250 ms but measured as elapsed time, so periodic status datagrams cannot
hide a dead IQ stream. The clock runs only while a read waits (from the
later of the last valid IQ datagram and entering the read), so a caller that
pauses between reads finds its queued IQ instead of a stall. The stall error
names the firewall. The 10,000
consecutive-malformed bound applies as in protocol 1. Both bounds are
`pub(crate)` fields with production defaults, so tests use short values.

**D14 — Protocol-2 validation.** One DDC (`ddc_count == 1`, MAN-307); the rate
exactly 48, 96, 192 or 384 kHz (it is encoded in kHz); the frequency a whole
number of Hz with 0 < f < 61,440,000 (half the 122.88 MHz clock, the phase
word's range). No link-budget check: one DDC at 192 kHz is 9.6 Mbit/s, and
the Gigabit Ethernet note is in the docs.

**D15 — Clean room** (the MAN-140 D1 rule). piHPSDR is GPL. The tables here
are facts (offsets, bit values, thresholds, cadences); the code is written
from these tables, not from any C source, and the test vectors are computed
from them. The sources consulted are listed under References.

**D16 — No Cargo feature.** Protocol 2 is in every build, as protocol 1 is
(MAN-239); the `hpsdr` features stay no-ops.

**D17 — `manta devices`, `doctor` and the receiver description are
unchanged.** `doctor` and `check` open through the same `open_receiver`, so
they inherit `auto`. `ReceiverKind::Hpsdr` keeps the label "HPSDR".

**D18 — Changelog and output stability.** The CHANGELOG entry is under
`### Added`, not `### Decoder output`: file replay is byte-identical because
every new hook defaults to `None`, and the `ReconnectingSource` fix changes
timestamps only for a live source that reports gaps, which none did before.
The wrapper fixes also get a `### Fixed` line.

### Defaults decided without a reviewer answer

The plan put three questions to review and built a default for each; nobody
answered, so the defaults stand:

1. **Short gaps keep tracks open.** A gap under 0.5 s goes through
   `take_missing_samples()` (clock advance, tracks kept); 0.5 s or more goes
   through `take_discontinuity()` (tracks closed). Reporting every gap through
   `take_discontinuity()` lost the spot at every loss rate tried.
2. **A busy radio is refused under `auto`.** A protocol-2 radio whose
   discovery status says it is already streaming makes `auto` fail;
   `--hpsdr-protocol 2` takes it over.
3. **An unknown board gets bypassed filters.** With `--hpsdr-protocol 2`, or a
   board number not in the D7 table, manta sends the bypass words: no
   high-pass, the 6 m/bypass low-pass, ANT1. That costs out-of-band rejection
   on boards with filters but never filters out the band being received.

## D-HW: settled only with a protocol-2 radio on the LAN (MAN-52)

1. The board numbers in discovery byte 11 for Angelia, Orion and Orion II
   (3/4/5 per piHPSDR and sdroxide-hpsdr).
2. The radio's watchdog timeout, and that a 100 ms high-priority keepalive
   keeps it streaming over a 24 h run.
3. That the D7 filter words give normal sensitivity on 40 m and 20 m on a
   filter-equipped ANAN-100/200 and on an ANAN-7000/8000, and that bypass
   works on both.
4. That an ANAN answers directed (unicast) discovery, and whether it answers
   both formats.
5. That general byte 4 = 0 and the tx-specific packet are what a real radio
   needs to stream receive-only (piHPSDR and Thetis agree on 0).
6. Stream continuity at 192 kHz over 100 Mbit/s and over Gigabit Ethernet.
7. The `#[ignore]`d `streams_iq_from_a_real_protocol2_radio` test passes, and
   a live run copies W1AW.

## What I ran

At `0137153` during planning, with scratch code under `/tmp/p2scratch/` that
was never added to the repository, then during implementation on `MAN-305`.

| Command / experiment | Result |
|---|---|
| `cargo test -p manta-input hpsdr --locked` (before) | 38 passed, 1 ignored (the real-hardware test) |
| `cargo test -p manta-input decimate --locked` (before) | 13 passed |
| `cargo test -p manta-cli --test hpsdr_metrics_acceptance --locked` (before) | 1 passed |
| `cargo test -p manta-cli --bin manta reconnect --locked` (before) | 28 passed |
| `manta run --help`, `devices`, `config check`, `run … --hpsdr-protocol 2`, `[input] protocol = "2"` (before) | The "before" outputs quoted under "What the operator sees" |
| Debug-build decode speed, `manta run --source v1.wav --source-iq --json` (V1, 120 s, 96 kHz) | 8.0 s wall time on 32 cores, 15× real time, one W1AW spot at `sample_ts` 2,006,272. 192 kHz was not timed |
| Loss experiment, 96 kHz V1 120 s, a scratch lossy source in 238-sample datagrams; baseline one W1AW spot at 20.899 s | One datagram lost every 0.1 s: splice unchanged; a discontinuity per loss gave 0 spots, here and at every rate tried. Bursts of 9/2 s, 9/5 s, 20/5 s, 40/5 s: discontinuity 0 spots, splice 1 spot. 40/10 s and 100/10 s: discontinuity's first spot only at 72.3 s |
| Same at 192 kHz (baseline `sample_ts` 4,012,544), 50 datagrams lost at 3.0 s | Splice 4,000,768 (−11,776); discontinuity 4,012,628 (+84). A discontinuity at 5 s or 10 s (mid-transmission): 0 spots in a 30 s fixture; in the 120 s fixture, first spot at 33.7 s / 72.26 s |
| Zero-fill instead of splice | Isolated gaps exact (even 0.5 s), but 17 datagrams lost every 2 s gave 0 spots, and 40/5 s gave a first spot at 62.8 s |
| Prototype hook (`seg_base += k` at the two single-input read sites, 6 lines, reverted) | Decode identical to splice; `sample_ts` 4,012,852 (1 per 0.1 s, where splice gave −43,008), 4,012,606 (17/2 s), 4,012,736 (40/5 s), 4,012,668 (50 at 3 s and 50 at 10 s), 4,012,512 (0.5 s at 10 s): every case within 308 samples of 4,012,544 |
| Scratch wrapper probes | `DecimatingSource` over an inner source with a pending `take_discontinuity()` returned `None`; `ReconnectingSource` over an inner `Some(11900)` returned `None`. Both swallowed the report (reproduced) |
| Engine tests in `crates/manta-engine/src/listen.rs`, V1 at 192 kHz for 30 s from `manta-testkit`, 50 datagrams (11,900 samples) removed and reported | Spot `sample_ts` 4,012,668 against the loss-free 4,012,544 (+124) for the loss at 1.0 s (during calibration), at 3.0 s, at 10.0 s (inside the transmission) and on the multi-input loop; one W1AW spot each time |
| The control test, the same loss spliced with no report | `sample_ts` 4,000,768 (−11,776), which shows the ±512 assertion can fail |
| `cargo test -p manta-input hpsdr --locked` (after) | 97 passed, 2 ignored (the two real-hardware tests), including the eight `open_receiver` selection tests in `hpsdr.rs`'s `open_receiver_tests` |
| `cargo test -p manta-cli --test hpsdr_protocol2_acceptance --locked`: the real binary against a loopback fake protocol-2 radio (board 5), V1 at 192 kHz for 30 s streamed at 2× real time once manta prints its ready line; file replay `W1AW` at 14,012,356.7 Hz, `sample_ts` 4,012,544 | 3 passed. Each live run spotted `W1AW` once with `sample_ts` within +358/−14 of the file replay, but the container's kernel itself dropped one burst of 10–213 datagrams after startup (seen at 1× and at 2×), so `manta_input_dropped_packets_total{source="hpsdr"}` was 10–213 with no loss scripted, and the spliced track's frequency moved 1.6–13 Hz. The test therefore checks frequency within 25 Hz and the dropped metric's presence, not `≤ 1 Hz` and `0`; the exact `+50` stays pinned by the library test |
| Reading piHPSDR `src/new_protocol.c` and `src/alex.h` (read for facts only) | Register byte offsets, the bring-up order and delays, the keepalive cadence (100/200/200/800 ms), filter bit values and frequency ladders, as tabulated in D6–D8 |

Not run: anything against a real protocol-2 radio (none is reachable;
MAN-52); 192 kHz decode timing; the Windows build. The Windows
`ConnectionReset` behaviour in D5 comes from documented Winsock semantics, not
from a run.

## What the operator sees

The strings below are exact; the implementation builds and tests them
verbatim.

### 1. The flag (`manta run --help`; identical in `listen`, `soak`, `calibrate` and `check`)

```text
HPSDR source:
      --hpsdr-host <HPSDR_HOST>
          openHPSDR radio hostname or IP address (protocol 1: Hermes-Lite 2, Hermes; protocol 2: ANAN).
          
          Requires --hpsdr-freq-hz and --hpsdr-rate-hz. Included in every build.

      --hpsdr-port <HPSDR_PORT>
          openHPSDR control port.
          
          Protocol 2 also sends to the three ports above it and receives IQ from this port + 11 (1035 by default).
          
          [default: 1024]

      --hpsdr-protocol <HPSDR_PROTOCOL>
          openHPSDR protocol the radio speaks.
          
          auto asks the radio and uses protocol 2 when it answers as protocol 2, protocol 1 otherwise; 1 and 2 skip the question.
          
          [default: auto]
          [possible values: auto, 1, 2]

      --hpsdr-freq-hz <HPSDR_FREQ_HZ>
          Receiver centre frequency, in Hz. Required with --hpsdr-host

      --hpsdr-rate-hz <HPSDR_RATE_HZ>
          Sample rate, in Hz. Required with --hpsdr-host
```

Before this change `--hpsdr-protocol 2` failed with `error: unexpected
argument '--hpsdr-protocol' found`. Now `--hpsdr-protocol 3` fails with clap's
`error: invalid value '3' for '--hpsdr-protocol <HPSDR_PROTOCOL>'` and
`[possible values: auto, 1, 2]`.

### 2. The config file

```toml
[input]
type = "hpsdr"
host = "192.0.2.10"
port = 1024          # optional, default 1024
protocol = "auto"    # optional: "auto" (default), "1" or "2"; the integers 1 and 2 are accepted too
freq_hz = 7030000.0
rate_hz = 192000.0
```

`MANTA_INPUT_PROTOCOL=2` sets it, read verbatim as a string like
`MANTA_INPUT_HOST`. The key is accepted for `type = "hpsdr"` only; any other
type refuses it through the existing per-type key check. Before this change
`protocol = "2"` failed with ``unknown field `protocol` ``. `manta config
init` and `manta.example.toml` gain:

```toml
# protocol = "auto"           # hpsdr: "auto" asks the radio; "1" (Hermes-Lite 2) or "2" (ANAN) skips asking
```

### 3. `manta config check`

```text
input: type=hpsdr host=192.0.2.10 port=1024 protocol=auto freq_hz=7030000 rate_hz=192000
```

(Before: the same line without `protocol=auto`.)

### 4. On every open and reopen (stderr)

```text
note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 2 (board 5, Orion II; protocol version 39; firmware 21); connecting with protocol 2
note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 1 (board 6; firmware 73); connecting with protocol 1
warning: HPSDR radio 192.0.2.10:1024 did not answer discovery within 1 s; connecting with protocol 1 -- pass --hpsdr-protocol 2 (protocol = "2" under [input]) for a protocol-2 radio
note: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 2 (board 42, unknown board; protocol version 41; firmware 12); connecting with protocol 2, front-end filters bypassed
note: HPSDR radio 192.0.2.10:1024: protocol 2 as configured; discovery skipped, board unknown, front-end filters bypassed
```

In order: `auto` with a protocol-2 answer, `auto` with a protocol-1 answer
(a Hermes-Lite 2), `auto` with no answer within 1 s, `auto` with a protocol-2
answer from a board not in the D7 table, and `--hpsdr-protocol 2`.
`--hpsdr-protocol 1` prints nothing new and sends exactly what manta sent
before.

### 5. Errors

```text
error: HPSDR radio 192.0.2.10:1024 answered discovery as openHPSDR protocol 2 but is already streaming to another program; stop that program, or pass --hpsdr-protocol 2 to take the radio over
error: HPSDR protocol 2: centre frequency 70000000 Hz is not below 61440000 Hz, the radio's 122.88 MHz ADC Nyquist limit
error: HPSDR protocol 2 only encodes exactly 48000/96000/192000/384000 Hz sample rates, got 250000
error: HPSDR protocol 2 radio 192.0.2.10 stalled: no IQ datagram for 10 s -- if the radio is on, allow inbound UDP from 192.0.2.10 in the host firewall (protocol 2 sends IQ from port 1035, not a port manta sent to)
error: HPSDR protocol 2 radio 192.0.2.10 stalled: 10000 consecutive malformed datagrams with no valid IQ datagram -- corrupt hardware or a spoofed stream
```

A stall or malformed-flood error mid-run makes the reconnect loop reopen the
radio, rerunning discovery, with backoff from 1 s to 60 s. An error on the
first open ends the command, as protocol 1's `Connection refused (os error
111)` does.

### 6. Metrics

Names, labels and the 1 Hz poller are unchanged. For a protocol-2 radio the
counters mean:

| Metric | Definition for a protocol-2 radio | Source |
|---|---|---|
| `manta_input_dropped_packets_total{source="hpsdr"}` | IQ datagrams proven lost by DDC0's 32-bit sequence number: a jump from *n* to *n + k + 1* adds *k*. Each datagram is 238 samples (1.24 ms at 192 kHz). Radio restarts add nothing, because the count is unknown. | `Protocol2IqSource` sequence tracker → `InputHealthCounters::record_dropped` |
| `manta_input_gaps_detected_total{source="hpsdr"}` | Loss events: one per sequence jump, plus one per radio restart (sequence back to 0). | `record_gap` |
| `manta_input_malformed_packets_total{source="hpsdr"}` | Datagrams discarded unread: a wrong length or header on the IQ port, an unknown source port from the radio, or any datagram from another address. | `record_malformed` |
| `manta_source_health{source="hpsdr"}` | Unchanged: 1 once a valid IQ datagram has arrived since the last open, else 0. | `confirmed_live_handle` |

For protocol 1 the counters keep their meaning: losses estimated from packet
timing, because that protocol carries no sequence number. On the decoder
side, a gap shorter than 0.5 s is spliced and the spot clock jumps by the lost
samples; a gap of 0.5 s or more, or a restart, also closes every track in
progress, as a reconnect does.

### 7. Unchanged

`manta devices` still prints `HPSDR: automatic discovery is not supported;
use --hpsdr-host HOST (included in every build).` and its JSON `status` is
still `"not_supported"`. Discovery asks one configured host and never
broadcasts. The metric labels, `type = "hpsdr"`, telnet, the RBN uplink and
JSON spots are unchanged.

## Consequences

- An operator with an ANAN-class radio points `--hpsdr-host` at it and gets
  spots with no other setting; existing protocol-1 setups keep working, with
  up to 1 s more startup when the radio does not answer discovery
  (`protocol = "1"` removes it).
- A protocol-2 radio sends IQ from a port the host never sent to, so a
  stateful host firewall does not treat it as reply traffic. The host needs
  a rule allowing inbound UDP from the radio; the stall error, `docs/CONFIG.md`
  and this record say so.
- Protocol-2 loss counters are exact. A short loss no longer moves spot
  timestamps early, and no longer closes tracks.
- Any live source can now report a gap through `ReconnectingSource` and
  `DecimatingSource`. Out-of-tree `IqSource` implementors compile unchanged;
  wrapper authors must forward `take_missing_samples` (documented on the
  trait).
- File replay is unchanged, so decoder output is unchanged (D18).
- With a small socket buffer a 192 kHz protocol-2 stream can lose one burst
  of datagrams while manta starts decoding (the acceptance run above lost
  10–213 in a container). The burst is counted and spliced like any other
  loss, and the spot clock stays true; raising `net.core.rmem_max`
  (`docs/CONFIG.md`) lets the requested 4 MiB buffer absorb it.

## Follow-ups (to file)

1. "Operators should see openHPSDR radios on the LAN in `manta devices`":
   broadcast discovery of both protocols.
2. "Operators should see ADC overload from a protocol-2 radio": parse byte 5
   of the high-priority status datagram into a metric.
3. "Operators with a protocol-2 radio should be able to decode several DDCs
   at once": the protocol-2 half of MAN-307.
4. "Operators should be able to choose the antenna and attenuation on an
   openHPSDR radio".

## References

- External sources, read for facts only and not copied (D15): piHPSDR
  (dl1ycf) `src/new_protocol.c`, `src/alex.h`, `src/new_discovery.c`,
  `src/discovered.h`; Thetis `ChannelMaster/network.c`; sdroxide
  `crates/sdroxide-hpsdr`; TAPR Angelia release notes (Gigabit Ethernet for
  protocol 2).
- Code: `crates/manta-input/src/hpsdr.rs`, `crates/manta-input/src/hpsdr/`
  (`discovery.rs`, `proto2/mod.rs`, `proto2/wire.rs`),
  `crates/manta-input/src/lib.rs` (`take_missing_samples`),
  `crates/manta-input/src/{decimate.rs,replay.rs}`,
  `crates/manta-dsp/src/decimate.rs` (`Decimator::reset`),
  `crates/manta-engine/src/{listen.rs,check.rs,calibrate.rs,doctor.rs}`,
  `crates/manta-cli/src/{main.rs,reconnect.rs,config.rs,config_cmd.rs,source_check.rs}`.
- Related tickets: MAN-52 (hardware), MAN-307 (multi-DDC), MAN-239 (no socket
  unless selected), MAN-73 (outages as discontinuities), MAN-55 (liveness),
  MAN-22 (malformed datagrams), MAN-56/MAN-228 (health counters).
- Precedents: [2026-09-02 HPSDR protocol spike](2026-09-02-hpsdr-hermes-protocol-spike.md),
  [MAN-55 C&C tuning](2026-09-03-man55-hpsdr-cc-tuning-protocol.md),
  [MAN-73 source reconnect](2026-10-05-man73-source-reconnect.md),
  [MAN-140 ka9q-radio input](2026-10-13-man140-ka9q-radio-input.md).
