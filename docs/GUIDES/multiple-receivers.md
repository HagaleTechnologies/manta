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
  drops to 0 for the dead one, the status line ends in `inputs=2/3`, and the
  shipped `MantaInputDown` alert fires after 2 minutes
  ([monitoring.md](../RUNBOOKS/monitoring.md#mantainputdown)).

## CPU

Each receiver needs about one Raspberry Pi 4 core at 192 kS/s, so a Pi 4 runs
up to four 192 kS/s receivers, or more at 96 kS/s
([raspberry-pi.md](raspberry-pi.md)).

## Commands that examine one receiver

`manta calibrate`, `manta doctor`, `manta soak` and `manta check` look at one
receiver at a time; with several entries, pass `--input NAME`. A source flag
(`--kiwi-host`, `--ka9q-group` and the others) selects one receiver and
ignores the config's entries.

## Not supported yet

- Several receivers on one HPSDR radio: MAN-307. The config loader refuses
  two entries on one radio.
- Several channels of one SoapySDR device in one stream (a dual-tuner
  RSPduo): two Soapy devices, one entry each, work today.
- A per-receiver identity on RBN or a receiver field on spots: MAN-14. The
  combined output has one station identity.
- Adding or removing receivers without a restart.
