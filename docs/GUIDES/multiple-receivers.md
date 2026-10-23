# Several receivers in one daemon (MAN-13)

One `manta run` can read up to 16 receivers and publish one spot stream: one
telnet server, one JSON stream, one RBN uplink and one `/metrics`. Each
receiver decodes on its own thread, and every receiver's reports go through
one shared repetition gate and one 10-minute dedupe, so a station heard by two
receivers whose segments overlap is spotted once.

The design record is
[2026-10-15-man13-multi-input-daemon.md](../DECISIONS/2026-10-15-man13-multi-input-daemon.md).
Every key is in [CONFIG.md](../CONFIG.md#several-receivers-input).

## Write one `[[input]]` per receiver

Write the `[input]` table as `[[input]]`, once per receiver. Each entry takes
the same keys as `[input]`, plus `name`: the receiver's name in log lines,
`/healthz`, the `{source}` label of `manta_source_health` and the other
per-input metrics, and `--input`. With two or more entries every entry needs
a `name` and a `type`.

### Several channels of one SDR

ka9q-radio (radiod) serves many channels of one SDR front end on one
multicast group; each channel has its own SSRC, so each one is an entry:

```toml
[server]
station_callsign = "<your-callsign>"

[[input]]
name = "40m"
type = "ka9q"
group = "hf-cw.local"
ssrc = 7030000

[[input]]
name = "20m"
type = "ka9q"
group = "hf-cw.local"
ssrc = 14030000

[[input]]
name = "15m"
type = "ka9q"
group = "hf-cw.local"
ssrc = 21030000
freq_correction_ppm = 0.8   # per input; radiod channels of one SDR normally share one value
```

Several channels of one KiwiSDR work the same way: one `type = "kiwi"` entry
per channel, each with its own `freq_hz`. Mind the KiwiSDR owner's channel
limits ([kiwisdr.md](kiwisdr.md)).

### Several SDRs

Independent receivers mix freely, live ones only:

```toml
[[input]]
name = "rsp"
type = "soapy"
driver = "driver=sdrplay"
freq_hz = 14030000.0
rate_hz = 192000.0
freq_correction_ppm = 0.6     # `manta calibrate --input rsp` measures it

[[input]]
name = "kiwi"
type = "kiwi"
host = "<your-receiver-host>"
freq_hz = 14034000.0
freq_correction_ppm = -1.1
```

`manta config check --config manta.toml` prints one `input <name>:` line per
entry. File replay works too (`type = "file"` entries, for testing without
hardware), but one daemon runs either recordings or live receivers, not both.

## Calibrate each receiver

Each receiver has its own oscillator error, so each entry has its own
`freq_correction_ppm`. Measure it one receiver at a time:

```sh
manta calibrate --config manta.toml --input 20m --duration 180
```

`calibrate` saves the value into that `[[input]]` entry after you confirm.
Calibration matters more with several receivers: the dedupe treats another
receiver's report as the same spot only when it is in the same 300 Hz bucket
or within one channel (93.75 Hz) of it. That is about 6.7 ppm at 14 MHz, so
two receivers that disagree by more than that after correction can spot one
station twice.

## What happens when a receiver fails

- Startup opens every receiver at once and stops with
  `error: input "20m" failed to open: <cause>` if any of them fails.
- A live receiver that drops out reconnects on its own thread. The log says
  `input 20m: lost; retrying, 2 of 3 inputs decoding` and
  `input 20m: back; 3 of 3 inputs decoding`; the other receivers keep
  spotting.
- A receiver that ends for good (for example, it reopens at another sample
  rate) logs `input 20m: stopped (<cause>); 2 of 3 inputs decoding`. The
  daemon keeps running while any receiver runs, and exits 1 once all have
  ended if any ended in error.
- `/healthz` stays `ok` while at least one receiver delivers samples; each
  receiver has its own `source <name>:` line. `manta_source_health{source}`
  drops to 0 for the dead one (and stays 0 for a receiver that has ended
  while the others run), the status line ends in `inputs=2/3`, and the
  shipped `MantaInputDown` alert fires after 2 minutes
  ([monitoring.md](../RUNBOOKS/monitoring.md#mantainputdown)).

## Which receiver is deaf?

`manta_source_health` and `/healthz` say whether a receiver delivers
samples, not whether it hears anything. A receiver with a broken antenna
or on the wrong frequency still delivers samples, so it reads 1 and
`/healthz` stays `ok`. With two or more entries, `/metrics` has four more
families per receiver
([MAN-14](../DECISIONS/2026-10-19-man14-per-receiver-hearing-visibility.md)):

- `manta_input_spots_total{source}`: spots it decoded and published. Across
  receivers they sum to `manta_spots_total`.
- `manta_input_distinct_calls_total{source}`: distinct callsigns it spotted
  since startup.
- `manta_input_cross_input_dupes_total{source}`: its reports withheld
  because another receiver had already spotted the station. The count is on the
  receiver that lost, so a receiver with dupes but few spots hears.
- `manta_input_silent{source}`: 1 when it decoded no spots for a whole
  window while another receiver decoded at least one.

Every receiver's series start at 0. Two receivers, `live` hearing and
`deaf` hearing nothing, ten minutes after startup (lines taken from one
scrape):

```text
manta_source_health{source="deaf"} 1
manta_input_spots_total{source="live"} 1
manta_input_spots_total{source="deaf"} 0
manta_input_distinct_calls_total{source="live"} 1
manta_input_distinct_calls_total{source="deaf"} 0
manta_input_cross_input_dupes_total{source="live"} 0
manta_input_cross_input_dupes_total{source="deaf"} 0
manta_input_silent{source="live"} 0
manta_input_silent{source="deaf"} 1
```

The status line ends in `hearing=`, one `name:spots/calls/dupes` per
receiver in config order (here 90 s after startup):

```text
manta status: uptime_s=90 pipeline=decoding tracks=2 spots_per_min=0.7 spots_total=1 clients=0 (telnet=0 json=0 ws=0) uplink=none inputs=2/2 hearing=live:1/1/0,deaf:0/0/0
```

`/status` has the same counts, `silent` and `last_spot_age_seconds` in an
`inputs` array, and `manta status` prints them as an Inputs table
([METRICS.md](../METRICS.md#status)).

The silence check runs once per `[server].silent_input_warning_secs`
window (default 600 s; `0` turns it off, otherwise 60 to 86400). A receiver
that decoded no spots in the window while another decoded at least one is
flagged, unless its sample delivery failed during the window (the
`lost`/`stopped` lines and `MantaInputDown` cover that):

```text
warning: input deaf: no spots decoded for 600 s while input live decoded 7 in that window (counted in manta_input_silent{source="deaf"}; warning repeats at most once per silent_input_warning_secs)
```

When it decodes again the log says
`input deaf: decoding again after 1800 s of silence` and the gauge returns
to 0. When every receiver is quiet, as on a closed band, none is flagged.
No alert rule ships for `manta_input_silent`.

## Several bands on one HPSDR radio

Give each band an entry with the same `host` and `port`. Entries are assigned
receivers 0, 1, 2 and 3 in file order, even if other radios occur between them.
Host comparison ignores letter case; an omitted port means 1024. All entries
on a radio must use the same native `rate_hz` and the same `protocol`
(default `auto`); a radio shared by several entries speaks protocol 1, so
`protocol = "2"` is refused there — a protocol-2 radio opens one receiver
per radio and gets one entry. `capture_rate_hz`,
`center_freq_hz` and `freq_correction_ppm` remain per-entry settings.

<!-- MAN-307 four-band example -->
```toml
[server]
station_callsign = "W1AW"
bind_addr = "127.0.0.1"
metrics_bind_addr = "127.0.0.1"

[[input]]
name = "80m"
type = "hpsdr"
host = "192.168.1.40"
port = 1024
freq_hz = 3525000.0
rate_hz = 192000.0

[[input]]
name = "40m"
type = "hpsdr"
host = "192.168.1.40"
port = 1024
freq_hz = 7025000.0
rate_hz = 192000.0

[[input]]
name = "20m"
type = "hpsdr"
host = "192.168.1.40"
port = 1024
freq_hz = 14025000.0
rate_hz = 192000.0

[[input]]
name = "15m"
type = "hpsdr"
host = "192.168.1.40"
port = 1024
freq_hz = 21025000.0
rate_hz = 192000.0
```

Use your station callsign in place of W1AW. Save this as `four-bands.toml`,
then run `manta config check --config four-bands.toml` and
`manta run --config four-bands.toml`.

At startup, manta queries the configured endpoint for the Hermes-Lite 2
receiver-count extension. This is a directed capability query, not automatic
radio selection. With six bands and a radio reporting four receivers,
startup exits 1, writes no stdout, and reports:

```text
error: input "80m" failed to open: HPSDR radio at 192.168.1.40:1024 has 4 receivers; 6 bands are configured
```

That refusal happens before tuning or starting the radio and before starting
listeners. An unknown or silent capacity reply needs an explicit declaration:
put `receivers = 4` on the first entry for that radio. This declares the
capacity supported by its gateware; it is not a hardware measurement and does
not set the number of active receivers. Later entries must omit it. Manta
cannot verify a declaration on hardware that does not report a count. When
both counts exist, the smaller limit applies. A declaration above eight is
allowed, but manta still starts at most eight receivers and enforces its
existing Protocol 1 link budget. Change the allocation or capacity by
restarting the daemon.

`config check` stays offline, including DNS. It appends `receivers=4` to the
first input's summary when declared, and prints this note for each shared
radio:

```text
note: HPSDR receiver capacity at 192.168.1.40:1024 is checked when manta run starts
```

Six bands with `receivers = 4` fail the offline check with exit 1 and no stdout:

```text
error: six-bands.toml: [[input]] "80m": HPSDR radio at 192.168.1.40:1024 declares 4 receivers; 6 bands are configured
```

The config error prefix names the file and offending input. The runtime
prefix is `error: input "80m" failed to open:` for this group. Other message
bodies are:

| Condition | Message body |
|---|---|
| Invalid declaration | `receivers must be an integer from 1 to 255` |
| Declaration on a later entry | `receivers belongs on the first input for this HPSDR radio ("80m")` |
| Mismatched native rate | `rate_hz must match the HPSDR radio's other receivers (192000 Hz, set by input "80m")` |
| Mismatched protocol | `protocol must match the HPSDR radio's other receivers (set by input "80m")` |
| Protocol 2 on a shared radio | `HPSDR protocol 2 opens one receiver per radio in manta; give each band its own receiver with protocol "1"` |
| No recognized capacity and no declaration | `cannot determine receiver capacity for HPSDR radio at 192.168.1.40:1024; set receivers on input "80m" to the count supported by its gateware` |
| Hostname/IP aliases for one endpoint | `HPSDR inputs "80m" and "40m" resolve to the same radio; use the same host and port for all its bands` |

One radio has one control session and one reconnect coordinator. A radio
outage takes all of its inputs down; other radios continue. Reopening UDP
does not mark an input healthy. Valid IQ must arrive first. Every replacement
restores the complete frequency allocation and rebuilds per-input filters.

Each receiver retains at most two seconds of pending native IQ. A slow
consumer loses its oldest queued samples without restarting siblings. Manta
resets that receiver's decode segment at the next retained sample and counts
the exact eviction in `manta_input_queue_dropped_samples_total{source="80m"}`.
This is a cumulative complex-sample count before decimation, retained across
reconnects and reset on process restart. A growing counter means the consumer
cannot keep up; reduce load or the configured rate. Radio-wide transport
counters appear under every member's name, so do not sum those across members.
See [the metric reference](../METRICS.md).

Single-input configurations and standalone flags need no query or declaration.
A selected `check --input 20m` or `calibrate --input 20m` tunes only that band's
frequency on receiver 0. Stop the daemon before another diagnostic command or
application takes control of its radio.

Loopback tests verify control frames, four-band decoding and shared output.
They do not establish physical RF reception or a live-radio soak. See the
[MAN-307 decision](../DECISIONS/2026-10-19-man307-hpsdr-multiple-receivers.md)
for implementation evidence and optional hardware checks.

## CPU

One 192 kS/s passband in one Raspberry Pi 4 core is a target, not a measured
four-band CPU guarantee. Each receiver runs a separate decode front end.
Measure CPU and queue-loss counters on the intended machine before choosing
its rate and band count ([raspberry-pi.md](raspberry-pi.md)).

## One receiver, several bands: `bands`

An entry's `bands` (`"all"`, band names, or centres in kHz) expands to one
input per stream of the standard CW segments, named `<entry>-<band>-<n>`.
Streams of one plan that overlap split the overlap at the midpoint of their
centres. See [Band plans](../CONFIG.md#band-plans-bands) for the table, the
rules and what each receiver type can open.

## Commands that examine one receiver

`manta calibrate`, `manta doctor`, `manta soak` and `manta check` look at one
receiver at a time; with several entries, pass `--input NAME`. A source flag
(`--kiwi-host`, `--ka9q-group` and the others) selects one receiver and
ignores the config's entries.

## Not supported yet

- `bands` that needs several streams from one HPSDR radio (several receivers
  on one radio are one `[[input]]` each — see above) or one SoapySDR device,
  and radiod channels created from `bands`.
- Several channels of one SoapySDR device in one stream (a dual-tuner
  RSPduo): two Soapy devices, one entry each, work today.
- A per-receiver identity on RBN or a receiver field on spots. The combined
  output has one station identity; per-receiver hearing is on `/metrics`,
  the status line and `/status` instead (MAN-14).
- Adding or removing receivers without a restart.
