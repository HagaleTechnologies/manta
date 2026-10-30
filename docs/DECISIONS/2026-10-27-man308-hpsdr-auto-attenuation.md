# 2026-10-27 — MAN-308: automatic step attenuation on a protocol-2 openHPSDR radio's ADC overload

**Status:** Implemented (branch `MAN-308`) against the fake protocol-2 radio
in the tests. No protocol-2 radio was reachable; four facts stay open until
one is ([D-HW](#d-hw-settled-only-with-a-protocol-2-radio-on-the-lan-man-52)),
owned by MAN-52. This record supersedes one row of
[MAN-305](2026-10-19-man305-hpsdr-protocol-2-input.md) D6 (high-priority
`[1442]`/`[1443]` = 0) for ADC0 when the key is on; that record is not
rewritten. Operator steps:
[docs/CONFIG.md, Automatic attenuation](../CONFIG.md#automatic-attenuation).

## Context

An ANAN-class radio that overloads its ADC kept overloading: manta sent 0 dB
in both step-attenuator bytes of every high-priority packet and dropped,
unread, the status datagram that reports the overload. The ticket had two
scenarios:

1. With automatic attenuation enabled, 2 s of reported ADC overload makes
   manta raise the radio's attenuation, log the change and export it as a
   metric.
2. After automatic attenuation added 12 dB during a static storm, 10 minutes
   with no overload makes manta step it back down.

Reproduced before the change: a fake radio that sent a status datagram with
ADC0's overload bit set every 50 ms for 2.5 s saw 26 high-priority packets
from manta, all with bytes 1442 and 1443 at 0.

## Decisions

**D1 — Scope: protocol 2, ADC0, opt-in.** `[input].auto_attenuation = true`
on a `type = "hpsdr"` input turns it on; it is off by default because it
writes to the radio, and another program driving the same radio's
attenuator would fight it. Config load refuses the key with
`protocol = "1"` and on a radio that several `[[input]]` entries share
(several receivers on one radio speak protocol 1 in manta). Under
`protocol = "auto"`, a radio that answers as protocol 1 opens as before and
gets one warning per open naming the key; manta leaves that radio's
attenuator alone. manta's protocol-2 driver runs DDC0 on ADC0 only, so ADC1
(byte 1442, status bit 1) is not touched. Protocol 1 (Hermes-Lite 2) has no
pinned attenuator or overload field and stays out of scope.

**D2 — Wire facts.** The implementation is written from these facts; no code
is translated from piHPSDR, Thetis or sdroxide (MAN-140 D1).

| Fact | Status | Sources |
|---|---|---|
| High-priority status datagram, radio `port + 1` → host, 60 bytes; byte 5 bit *i* = ADC *i* overload, 1 = overload | VERIFIED | openHPSDR Ethernet Protocol v4.4 p.47; piHPSDR `new_protocol.c` `process_high_priority()`; Thetis `ChannelMaster/network.c` |
| The status datagram arrives every 50 ms while receiving, and at once when an input changes | VERIFIED | Protocol v4.4 p.47; piHPSDR comment "During RX, HighPrio packets arrive every 50 msec" |
| The overload bit covers one report; every client adds its own hold or latch | Consistent across three clients; the spec is silent | piHPSDR (sticky flag), Thetis (latched until read), sdroxide (`OVERLOAD_HOLD = 1500 ms`) |
| High-priority command byte 1443 = ADC0 step attenuator, byte 1442 = ADC1, each 0-31 dB in 1 dB units | VERIFIED | Protocol v4.4 pp.33-34; piHPSDR; Thetis; sdroxide `protocol2.rs` |

With the key on, byte 1443 carries the controller's level on every
high-priority packet, capped at 31 on the wire; byte 1442 stays 0. With it
off, both stay 0 as MAN-305 D6 recorded. Any status datagram not 60 bytes
long is ignored, not counted malformed.

**D3 — Overload semantics.** Overload reports no more than 500 ms apart
(`OVERLOAD_HOLD`) are one overload. A run of 2 s (`TRIP_AFTER`) raises one
step, and the run clock restarts at each raise, so continuing overload
raises again 2 s later. 600 s (`RELEASE_AFTER`) since the later of the last
overload report, the last change and the open lowers one step, and lowering
continues every further quiet 600 s down to 0 dB. The bit covers one 50 ms
report, so without a hold "2 s of overload" would need 40 positive reports
in a row; the hold means at least two reports per second for 2 s.

**D4 — Step 6 dB, maximum 30 dB.** 6 dB halves the ADC's input amplitude, and
two trips (4 s) reach the ticket's 12 dB. Stopping at 30 dB, not the wire's
31, makes every raise retrace exactly when lowered.

**D5 — The CLI owns the state; it reaches the source through
`HpsdrConfig`.** `manta-cli` builds one `Arc<AutoAttenuation>` per input
(atomics plus a notice callback that writes the log line, the
`with_overflow_notice` pattern, since `manta-input` has no logging
dependency) and passes it through `HpsdrConfig.auto_attenuation`. Every
reopen clones the same spec, so the change count is monotonic across
reconnects. Not an `IqSource` trait method: `ReconnectingSource`,
`GroupSource` and `SampleBoundedSource` would each have to forward it, as
MAN-56 needed an attach/retire layer for the health counters.
`Protocol2IqSource` holds the pure controller, reads the status datagram,
puts the level in every high-priority packet, and sends one at once on a
change.

**D6 — A reopen restores the learned level.** A reconnect mid-storm keeps
12 dB rather than starting again at 0 dB and re-climbing through 2 s of
overload per step. The bring-up's run = 1 packets already carry it. The
quiet clock restarts at the reopen, so an outage never counts as quiet
time. A manta restart starts at 0 dB.

**D7 — Config key only.** `[input].auto_attenuation`, bool, default
`false`; `MANTA_INPUT_AUTO_ATTENUATION` sets it. No CLI flag: a source flag
such as `--hpsdr-host` replaces a typed `[input]` whole, so an operator who
selects the radio with flags gets no automatic attenuation. Restart-only,
like every `input.*` key. `manta config check` appends
` auto_attenuation=true` to the `input:` line only when the key is on.

**D8 — Log lines and metrics.** One `note:` line per change, at the moment
of the change, from whichever command has the radio open (`run`, `soak`,
`doctor`, `calibrate`, `check`, `listen`):

```
note: HPSDR radio 192.0.2.10:1024: ADC overload for 2 s; automatic attenuation raised the step attenuator from 0 dB to 6 dB
note: HPSDR radio 192.0.2.10:1024: ADC overload for 2 s; automatic attenuation raised the step attenuator from 24 dB to 30 dB, its maximum
note: HPSDR radio 192.0.2.10:1024: no ADC overload for 10 min; automatic attenuation lowered the step attenuator from 12 dB to 6 dB
```

`manta run` with a `[server]` samples the shared state every second into
`manta_input_attenuation_db` (gauge) and
`manta_input_attenuation_changes_total` (counter), labelled `source` as
every `manta_input_*` family. An input publishes them only once a
protocol-2 open has engaged its handle, so an input that only ever found a
protocol-1 radio has no series.

**D9 — Unchanged.** File replay and decoder output, `/healthz`, `/status`,
the status line, telnet, JSON spots and the RBN uplink. Without the key the
status datagram is still ignored and both attenuator bytes stay 0.

## D-HW: settled only with a protocol-2 radio on the LAN (MAN-52)

Each is assumed true until a radio contradicts it:

1. The firmware reports the overload bit per 50 ms report without its own
   filtering.
2. The status datagram is 60 bytes on every firmware. Any other length is
   ignored, which fails safe: no raise.
3. The radio applies byte 1443 within one high-priority period.
4. The 0-31 dB, 1 dB-per-count scale holds on every ANAN board.

## Known behaviour

A band at the edge of overload can dip and re-trip once per 10 minutes:
after a quiet lower, 2 s of overload raises again. There is no anti-hunting
back-off.

## Follow-ups (to file)

- An ADC-overload metric with the controller off (MAN-305 follow-up 2).
- Manual attenuation (MAN-305 follow-up 4).
- Protocol-1 / Hermes-Lite 2 gain control.
- ADC1, once MAN-307's protocol-2 half lands.

## References

Read for facts only (MAN-140 D1): openHPSDR Ethernet Protocol v4.4
(TAPR/OpenHPSDR-Firmware, `Protocol 2/Documentation/`); piHPSDR
`src/new_protocol.c`; Thetis `ChannelMaster/network.c` and
`ReleaseNotes.txt` (2.10.3.7, 2.10.3.13: automatic attenuation with fast
attack and slow decay); sdroxide `crates/sdroxide-hpsdr/src/{net.rs,protocol2.rs}`.
Code: `crates/manta-input/src/hpsdr/proto2/attenuator.rs` (controller and
handle), `crates/manta-input/src/hpsdr/proto2/mod.rs` (status datagram and
high-priority send), `crates/manta-cli/src/main.rs` (log lines, pollers).
