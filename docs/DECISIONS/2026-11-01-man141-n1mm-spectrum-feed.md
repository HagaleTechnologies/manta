# MAN-141: manta sends its channelizer power spectrum to N1MM+'s Spectrum Display over UDP, as CW Skimmer does

**Status:** accepted. `[input] spectrum_udp` is implemented and tested against a fake N1MM+ listener written from N1MM+'s published format; a real N1MM+ render is an external step (below).

## Context

MAN-141's Gherkin scenario: N1MM+ is configured to receive Spectrum Display
UDP data on its documented port, manta is decoding the band, manta sends its
already-computed power spectrum over that protocol, and N1MM+'s Spectrum
Display renders it as it would from CW Skimmer. The 2026-10-31 scope note
narrows acceptance to a fake UDP listener built from the published protocol;
a real N1MM+ render is an external step for the owner.

- **Scope.** Broad-review D9
  ([2026-09-06-broad-review-decisions.md](2026-09-06-broad-review-decisions.md))
  put the contester persona in scope and called this "a panadapter/spectrum
  feed, not band-map support". It superseded the
  [legacy capability matrix](2026-09-01-legacy-capability-matrix.md)'s
  "Spectrum via UDP" Non-goal row, and only that row. This change updates the
  row to Covered; the Waterfall display/Band Map UI row stays a Non-goal, and
  README's "Not an interactive receiver or panadapter" bullet stays, with a
  note that manta can feed N1MM+.
- **The format.** N1MM+'s "External UDP Broadcasts" appendix
  (<https://n1mmwp.hamdocs.com/appendices/external-udp-broadcasts/>)
  publishes "Sending Spectrum Data to N1MM+": an XML `<Spectrum>` datagram to
  UDP 13064 with `app`, `Name`, `LowScopeFrequency`, `HighScopeFrequency`,
  `ScalingFactor`, `DataCount` and `SpectrumData`. CW Skimmer 2.x's "Send
  Spectrum via UDP" option feeds the same window.
- **The data.** manta's PFB channelizer already computes
  `power[k] = |X[k]|^2` for every channel on every hop (375 hops/s, 93.75 Hz
  apart) and discarded it after detection. No new DSP is needed.

## Decisions

| # | Decision | Reason |
|---|---|---|
| D1 | Scope is the outbound XML spectrum datagram only. No decoded-call band-map data, no receiving on 13064, no `<radio_setfrequency>` QSY handling. | D9 separates the panadapter feed from band-map support. N1MM+'s band map comes from its telnet connection, which `:7300` already serves. |
| D2 | Config is two per-input keys, `spectrum_udp` (`IP:port`; its presence turns the feed on) and `spectrum_name` (optional), not a new table. | The spectrum belongs to one receiver's channelizer, like `record_iq`. Avoids new top-level tables and environment tables. |
| D3 | `spectrum_udp` takes an IP address and port only, no host name, and refuses port 0, the unspecified address, multicast and the IPv4 broadcast address. | N1MM+ listens on a unicast address. Name resolution would add a runtime failure mode for no documented use. `[[wsjtx_relay]].listen` is also a socket address. |
| D4 | The default `Name` is `manta <input name>`; `spectrum_name` overrides it (1 to 64 characters, no control characters). A `bands` entry planning several streams refuses `spectrum_name`; without it each stream sends as `manta <stream name>`. Two inputs with the same destination and the same effective name are refused. | N1MM+ tells sources apart by `Name` only, and its drop-down mixes sources from every program on the PC, so a bare `hpsdr` would not say which program sends it. |
| D5 | The feed needs a real RF centre, refused before source I/O (a single input: beside MAN-34's dial guard; several inputs: the existing `check_rf_references`). The engine also builds no accumulator when the centre is not positive, and the CLI then warns once. | A zero centre would make N1MM+ draw audio offsets. Matches MAN-34's refusal rather than a silent no-op. |
| D6 | Bins are the channels whose offset from the centre lies inside `rf_passband_hz()` (both ends inclusive) and whose calibrated RF frequency is above 0 Hz, sorted by ascending frequency. `Low`/`High` are the first and last bin centres, times the input's calibration factor (`freq_correction_ppm`). | `rf_passband_hz()` already applies a source's trusted band. Bin-centre edges are off by at most one 93.75 Hz bin if N1MM+ means span edges (external step 1). |
| D7 | One frame is the mean linear power over 125 consecutive hops with `m >= pad_hops` (one third of a second of stream time), converted with `manta_dsp::channelizer::power_db`. Pre-roll hops never count. A new segment starts a new, empty accumulator, so a frame never spans a discontinuity. A partial frame at a segment end or stream end is dropped. | N1MM+'s page calls 3 per second adequate and 5 to 10 the ceiling. Stream time keeps the engine deterministic. |
| D8 | Levels are `round((dB + 140) / 0.5)` clamped to 0..=65535 (NaN reads 0), `ScalingFactor` 0.5; `app` is `WaterfallBandmap`; frequencies are kHz with at most 3 decimals and trailing zeros trimmed; CRLF line ends and one tab of indent, as the published example, with no CRLF after `</Spectrum>`. | The page's example and third-party senders. A -140 dB floor at 0.5 dB per level keeps datagrams short (levels 0 to 280 cover the useful range), and N1MM+ has its own floor and reference controls. |
| D9 | The engine tap is a new `SpectrumSink` trait (`manta-engine::spectrum`), separate from `LiveRecord` and independent of `[server].live_port`. Every channelizer call goes through one `Segment::channelize`, so the tap sees every hop the track manager sees. | A fifth `LiveRecord` variant would tie the feed to the live stream. One entry point means no call site can bypass the tap. |
| D10 | One sender thread per feed (`manta-spectrum-<input>`), fed through `std::sync::mpsc::sync_channel(4)`. The decode thread only `try_send`s; a full queue drops the frame (`queue_full`). The thread formats, enforces a 100 ms wall-clock floor (`rate_limit`), and calls unconnected `send_to` on a socket `manta-cli` bound with `bind_udp_for(dest.ip())` and a 1 s write timeout. It ends when every sender handle is dropped; `manta run` joins it after decoding ends. | No new dependency, works without `[server]` (no tokio runtime needed), never blocks decoding, and survives N1MM+ starting late. |
| D11 | Metrics `manta_spectrum_datagrams_total{input}` and `manta_spectrum_dropped_total{input,reason}` (`queue_full`, `rate_limit`, `send_error`), registered only with `[server]`, every series at 0 from startup. The first send error per feed logs one warning. `/status`, `/healthz`, the status line and `manta doctor` are unchanged. | Loss is counted on every exit path, as for relays. |
| D12 | The fake N1MM+ listener lives in `crates/manta-cli/tests/n1mm_spectrum.rs` and is written from the published field list, never from manta's formatter. That file holds the in-process sender tests and the `manta run` acceptance tests. | Avoids a `manta-server` dev-dependency (a `Cargo.toml` edit). A checker written apart from the formatter can catch a mistake the two would otherwise share. |
| D13 | Docs in the same change: this record, the matrix row, README's Non-goals bullet, `docs/CONFIG.md`, `docs/METRICS.md`, [GUIDES/n1mm-spectrum.md](../GUIDES/n1mm-spectrum.md) (checked by `operator_guides.rs`), `ARCHITECTURE.md`, `CHANGELOG.md` under Added, and one `wiki/INDEX.md` row. | D9 asks for the matrix update, and the doc-consistency tests require the CONFIG and METRICS rows. |

Spots are unchanged with and without the feed: the tap is write-only, like
MAN-324's live-state tap, and the pipeline tests compare spots with and
without a recording sink on V1 and V7 and on a two-input session. Because
spots do not change, the CHANGELOG entry is under Added, not Decoder output.

## The datagram

A 192 kS/s HPSDR centred on 14.050 MHz, input `hpsdr`, `SpectrumData`
abbreviated:

```text
<?xml version="1.0" encoding="utf-8"?>
<Spectrum>
	<app>WaterfallBandmap</app>
	<Name>manta hpsdr</Name>
	<LowScopeFrequency>13954</LowScopeFrequency>
	<HighScopeFrequency>14145.906</HighScopeFrequency>
	<ScalingFactor>0.5</ScalingFactor>
	<DataCount>2048</DataCount>
	<SpectrumData>150,152,147,148,152,...,240,...,151</SpectrumData>
</Spectrum>
```

`Name` is XML-escaped (`&`, `<`, `>`, `"`, `'`). dB is manta's relative
scale, where a unit-amplitude complex tone reads 0 dB in its channel
(`docs/LIVE-STATE.md`), so level 0 is -140 dB and level 280 is 0 dB.

## Evidence

Measured in the plan phase at `d144e36` with a probe outside the repository
that used the real `manta-dsp` channelizer: `filter_len` zeros of padding,
then 2 s of a 0.1-amplitude tone 1500 Hz above a 14.050 MHz centre with a
little noise, in 2048-sample chunks. It averaged the first 125 hops with
`m >= pad_hops`, clipped the bins to each passband, sorted them by
frequency, built the XML above and sent it over loopback UDP to a listener
that compared the bytes:

| Source | Sample rate | Bins sent | Low (kHz) | High (kHz) | Peak level | Datagram |
|---|---|---|---|---|---|---|
| Rig audio (passband 300 to 3000 Hz) | 48 kS/s | 29 | 14050.375 | 14053 | 240 | 424 B |
| IQ | 48 kS/s | 512 | 14026 | 14073.906 | 240 | 2356 B |
| IQ (HPSDR) | 192 kS/s | 2048 | 13954 | 14145.906 | 240 | 8502 B |
| IQ, decimated trusted band (`capture_rate_hz`) | 192 kS/s | 927 | 14006.594 | 14093.406 | 240 | 4029 B |

At every rate the padding produced exactly one hop (`m = 0`), hops `m < 32`
were pre-roll, and the first usable hop was `m = 32`; 2 s of stream gave 5
frames. The tone landed in the right bin at level 240 (-20 dB, the
0.1-amplitude tone). Rust's `{:.3}` prints `14000.1875` as `14000.188` and
`14000.0` as `14000.000`, so the trailing-zero trim is needed. The research
phase measured the accumulation at 4.0% of channelizer time at 192 kS/s and
5.1% at 48 kS/s in a release build (a cloud container, not a Raspberry
Pi 4).

**N1MM+ starting late.** Three unconnected `send_to` calls of 8502 bytes to
a closed `127.0.0.1` port all succeeded, so the sender never `connect`s and
start order does not matter. This was verified on Linux only. BSD sockets
also report no ICMP error on an unconnected UDP socket, so macOS should
behave the same; `sending_before_n1mm_listens_is_not_an_error` in
`crates/manta-cli/tests/n1mm_spectrum.rs` checks it on every platform CI
runs.

## External steps

These need N1MM+, CW Skimmer or a second PC. They are not acceptance
criteria (scope note, 2026-10-31), and none blocks the fake-listener tests.

1. Run real N1MM+ with the Spectrum Display window on the `manta <name>`
   source and confirm it draws. This is the ticket's Gherkin scenario and the
   only check of D6's edges, D8's `app` value and the kHz units.
2. Capture CW Skimmer 2.x traffic to `127.0.0.1:13064` and diff it against
   manta's datagram: declaration, line ends, number formats, bins per
   datagram, scale and span.
3. In the same session, check how N1MM+ handles a span wider than 48 kHz, a
   `DataCount` mismatch, and 2048 or 4096 levels.
4. With N1MM+ on another PC, allow inbound UDP 13064 and check that
   fragmented 4 to 17 KB datagrams arrive.
5. Check whether DXLog.net accepts this feed at all.

## Related

- [GUIDES/n1mm-spectrum.md](../GUIDES/n1mm-spectrum.md): the operator guide.
- [MAN-324 live-state stream](2026-10-12-man324-live-state-stream.md): the
  write-only engine tap this one copies.
- [MAN-320 relays](2026-10-27-man320-relay-local-skimmers-and-wsjtx.md): a
  UDP socket bound in `manta-cli`, and a Non-goal reversed with the matrix.
- [MAN-309 IQ recording](2026-10-19-man309-iq-recording-and-replay.md): the
  per-input output key precedent.
