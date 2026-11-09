# Runbook: compare manta's spots with the RBN

With `[rbn_compare]` enabled, a manta daemon reads the Reverse Beacon
Network's public CW telnet feed and grades each spot it publishes against
what RBN's skimmers heard within a few minutes of it. Each grade goes out
on the JSON stream (`:7301`) and into `/metrics`. `manta grade` does the
same offline, for a recorded node's spot log and an RBN daily archive.

manta only reads the feed. It logs in with a callsign and sends nothing
else: no commands, no filters, no spots.

Decision record:
[2026-10-11-man301-live-rbn-comparison](../DECISIONS/2026-10-11-man301-live-rbn-comparison.md).

## Enabling it

Comparison needs a `[server]` table: the grades are published on the JSON
stream and `/metrics`, which only the daemon runs. Add to the node's
config:

```toml
[rbn_compare]
enabled = true
# login_callsign = "W3XYZ"   # default: [server].station_callsign
```

Or turn it on for one run without editing the file:

```sh
manta run --config manta.toml --rbn-compare
```

Check the config before starting. `manta config check` prints the settings
comparison will use:

```console
$ manta config check --config manta.toml
...
rbn_compare: enabled=true feed=telnet.reversebeacon.net:7000 login_callsign=W3XYZ-1 window_secs=180 freq_tol_hz=500 speed_tol=3 WPM/20% spotters=all exclude_spotters=none skew_warn_ppm=2
```

Every key, with its default:

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | `--rbn-compare` turns it on for one run |
| `feed_host` | `"telnet.reversebeacon.net"` | RBN's telnet feed |
| `feed_port` | `7000` | RBN's CW and RTTY feed; manta keeps CW only |
| `login_callsign` | `[server].station_callsign` | The one line manta sends |
| `window_secs` | `180` | RBN reports this close to a spot's time count; 30 to 3600 |
| `freq_tol_hz` | `500.0` | Further than this from RBN's modal frequency is a frequency disagreement |
| `speed_tol_wpm` | `3.0` | A speed disagreement needs more than this many WPM |
| `speed_tol_pct` | `20.0` | and more than this percent of RBN's median speed |
| `spotters` | `[]` | Only these RBN skimmers count; empty means all. `--rbn-spotter` replaces it |
| `exclude_spotters` | `[]` | Never count these skimmers. Your own callsign, with and without SSID, is always excluded |
| `skew_warn_ppm` | `2.0` | 0 to 1000; 0 turns the warning off. A band whose skew stays past it for 30 minutes warns on `/healthz` (see [The skew reading](#the-skew-reading-and-a-suggested-correction)) |

`MANTA_RBN_COMPARE_<KEY>` overrides any key. Every key needs a restart: a
SIGHUP reload names a changed `rbn_compare` key in its restart warning and
keeps the old value.

manta refuses to start, before opening anything, when:

- `[rbn_compare]` is enabled without a `[server]` table.
- The run replays a recording (`--source FILE` or `[input] type = "file"`).
  A recording's spot times are not when RBN heard anything; grade its spot
  log with `manta grade` instead.
- `--rbn-spotter` is given without comparison enabled.
- `window_secs` is outside 30 to 3600, or another key is out of range.
- `spotters` names this station's own callsign, whose reports are always
  excluded.

A disabled `[rbn_compare]` table without `[server]` loads with a note.

### The firewall rule

The daemon makes one outbound TCP connection, to `feed_host:feed_port`
(`telnet.reversebeacon.net:7000` by default), and holds it open. Allow it
outbound. Nothing inbound changes: grades go out on the JSON and metrics
listeners you already have.

`manta doctor` checks that the feed accepts a connection. It opens one and
sends nothing:

```
PASS  rbn compare feed telnet.reversebeacon.net:7000: accepted a connection in 41 ms
```

See [setup-checks](setup-checks.md) for the failure lines.

### What the log says

```
INFO manta_server::rbn_compare: rbn_compare: grading spots against telnet.reversebeacon.net:7000 (window 180 s, spotters all); read-only: manta sends only its login callsign W3XYZ-1
INFO manta_server::rbn_compare::feed: rbn_compare: feed connected, logged in as W3XYZ-1, receiving spots
WARN manta_server::rbn_compare::feed: rbn_compare: feed lost (no line for 60 s); spots whose window overlaps the gap grade unknown unless RBN matched them; reconnecting in 2 s
```

## Reading a grade

Spot lines on `:7301` do not change. With comparison on, each spot is
followed, once its window has closed (`window_secs` after the spot, plus up
to 5 s; up to 75 s while the feed is silent, see "Many unknown grades"), by
one grade line on the same stream, raw TCP and WebSocket alike:

```json
{"kind":"spotGrade","seq":2,"spotId":"W3XYZ-1:1791719990000000000:7:960000:W1AW","call":"W1AW","band":"20m","spotTime":1791720000,"windowClose":1791720180,"windowS":180,"grade":"match","disagreements":[],"mantaFreqHz":14025100,"mantaWpm":25,"rbnModalFreqHz":14025000,"rbnMedianWpm":24.0,"rbnSkimmers":3,"rbnReports":3,"freqOffsetHz":100,"rbnBands":["20m"],"spotterFilter":[]}
{"kind":"spotGrade","seq":4,"spotId":"W3XYZ-1:1791719990000000000:9:58560000:K5XYZ","call":"K5XYZ","band":"40m","spotTime":1791720600,"windowClose":1791720780,"windowS":180,"grade":"none","disagreements":[],"mantaFreqHz":7012300,"mantaWpm":22,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":[],"spotterFilter":[]}
{"kind":"spotGrade","seq":6,"spotId":"W3XYZ-1:1791719990000000000:12:173760000:W1AW","call":"W1AW","band":"20m","spotTime":1791721800,"windowClose":1791721980,"windowS":180,"grade":"disagree","disagreements":["frequency","speed"],"mantaFreqHz":14026400,"mantaWpm":30,"rbnModalFreqHz":14025000,"rbnMedianWpm":20.0,"rbnSkimmers":2,"rbnReports":2,"freqOffsetHz":1400,"rbnBands":["20m"],"spotterFilter":[]}
{"kind":"spotGrade","seq":8,"spotId":"W3XYZ-1:1791719990000000000:15:260160000:DL1ABC","call":"DL1ABC","band":"20m","spotTime":1791722700,"windowClose":1791722880,"windowS":180,"grade":"elsewhere","disagreements":[],"mantaFreqHz":14030000,"mantaWpm":28,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":["40m"],"spotterFilter":[]}
```

A spot line has no `kind` key, so a consumer that wants spots only skips
lines that have one. `spotId` is the graded spot's `id`.

| `grade` | What it means | What to do |
|---|---|---|
| `match` | RBN skimmers reported the same call on the same band, within the frequency and speed tolerances | Nothing: RBN confirms the spot |
| `disagree` | RBN reported the same call on the same band, but its modal frequency or median speed is outside tolerance. `disagreements` says which: `frequency`, `speed` or both | A steady frequency disagreement on every spot is a calibration problem (see below). An occasional one is often a different station on the same call, or a busted spot |
| `elsewhere` | RBN reported the call only on other bands (`rbnBands` lists them) | The station was active, but RBN did not hear it on this band. Often a station working two bands, sometimes an image or a busted call |
| `none` | RBN reported the call nowhere, and the feed was up for the whole window | Either a weak signal only you heard, or a busted call. A run of `none` on strong signals is worth a look |
| `unknown` | No same-band report, and the feed was down for part of the window | No verdict. See "Many unknown grades" |

The other fields:

- `rbnModalFreqHz`: the most common RBN frequency for the call on this
  band, rounded to 100 Hz. `rbnMedianWpm`: the median speed those reports
  give. `rbnSkimmers`: how many different skimmers reported it.
  `rbnReports`: how many reports. These are `null` or `0` unless the grade
  is `match` or `disagree`.
- `freqOffsetHz`: manta's frequency minus `rbnModalFreqHz`.
- `windowClose`: `spotTime + windowS`. It is not when the line was sent,
  so a live grade and `manta grade`'s grade of the same inputs are
  identical.
- `spotterFilter`: the `spotters` list the grade was made with; `[]` means
  every RBN skimmer.

A speed disagreement needs both more than `speed_tol_wpm` WPM and more than
`speed_tol_pct` percent of RBN's median: 30 against 20 WPM is flagged, 22
against 20 is not. A spot or an RBN window without a speed never flags
speed. Frequency exactly `freq_tol_hz` away is not a disagreement.

`field-node.py record-spots` archives grade lines to
`grades-<UTC day>.jsonl`, next to `spots-<UTC day>.jsonl`.

## Metrics

None of these families appear on `/metrics` unless comparison is enabled.
`band` is the band of manta's spot frequency (`160m` to `6m`, or
`unknown`); `grade` is one of the five grades.

| Family | Type | Meaning |
|---|---|---|
| `manta_rbn_compare_grades_total{band,grade}` | counter | Spots graded since the daemon started |
| `manta_rbn_compare_grades_last_hour{band,grade}` | gauge | Spots graded in the last 3600 s. A series stays, at 0, once it has been seen |
| `manta_rbn_compare_disagreements_last_hour{band,kind}` | gauge | `disagree` grades in the last 3600 s naming `kind` (`frequency` or `speed`). A spot with both counts once under each |
| `manta_rbn_compare_freq_offset_median_hz{band}` | gauge | Median `freqOffsetHz` over `match` and `disagree` grades in the last 3600 s. Absent for a band with none |
| `manta_rbn_compare_freq_offset_samples{band}` | gauge | How many grades that median is over |
| `manta_rbn_compare_freq_offset_median_ppm{band}` | gauge | Median of each of those grades' offset over RBN's modal frequency, in ppm (the band's skew; positive means manta reads high), rounded to 0.001 |
| `manta_rbn_compare_skew_warning{band}` | gauge | 1 while the band's skew has been past `skew_warn_ppm` for 30 minutes on a reading of at least 20 spots precise to a third of the limit, else 0 (see [The skew reading](#the-skew-reading-and-a-suggested-correction)) |
| `manta_rbn_compare_feed_connected` | gauge | 1 while the feed is logged in and delivering lines, else 0 |
| `manta_rbn_compare_feed_reports_total` | counter | CW reports accepted from the feed |
| `manta_rbn_compare_feed_reconnects_total` | counter | Feed connections lost or failed |
| `manta_rbn_compare_pending` | gauge | Spots waiting for their window to close |
| `manta_rbn_compare_ungraded_total` | counter | Spots never graded: the pending queue was full, or the comparator fell behind the spot bus |

The five grade series on a band sum to the spots graded there: `disagree`
is its own grade, not a flagged `match`.

A sample scrape, 100 spots graded in the last hour:

```
manta_rbn_compare_grades_last_hour{band="20m",grade="disagree"} 6
manta_rbn_compare_grades_last_hour{band="20m",grade="elsewhere"} 2
manta_rbn_compare_grades_last_hour{band="20m",grade="match"} 52
manta_rbn_compare_grades_last_hour{band="20m",grade="none"} 9
manta_rbn_compare_grades_last_hour{band="40m",grade="match"} 23
manta_rbn_compare_grades_last_hour{band="40m",grade="none"} 8
manta_rbn_compare_freq_offset_median_hz{band="20m"} 40
manta_rbn_compare_freq_offset_median_hz{band="40m"} -20
manta_rbn_compare_freq_offset_samples{band="20m"} 58
manta_rbn_compare_freq_offset_samples{band="40m"} 23
```

Example PromQL:

```promql
# Last hour's grades on 20m, by grade
sum by (grade) (manta_rbn_compare_grades_last_hour{band="20m"})

# Share of 20m spots RBN confirmed in the last hour
manta_rbn_compare_grades_last_hour{band="20m",grade="match"}
  / ignoring(grade) sum by (band) (manta_rbn_compare_grades_last_hour{band="20m"})

# Long-range dashboards: grades per hour from the counter
sum by (band, grade) (increase(manta_rbn_compare_grades_total[1h]))

# Feed down
manta_rbn_compare_feed_connected == 0
```

The `_last_hour` gauges reset when the daemon restarts; use
`increase(manta_rbn_compare_grades_total[...])` for anything longer than
an hour or across restarts.

### The skew reading and a suggested correction

`manta_rbn_compare_freq_offset_median_hz` is manta's frequency minus RBN's,
over the spots both heard. RBN's own skimmers are calibrated against each
other, so a steady offset of the same sign on every band points at your
receiver's frequency reference, not at RBN. A ppm error grows with
frequency, so manta reads it in ppm for you (MAN-541): each spot's offset
is divided by RBN's modal frequency, and a band's **skew** is the median
of those, in `manta_rbn_compare_freq_offset_median_ppm`. A positive skew
means manta reads high, as a CW Skimmer `FreqCalibration` above 1 does.

**Precision.** RBN reports frequencies to 100 Hz, so a band's reading is
good to about `50 Hz / √n / f`, its `±` figure. A band needs **20 spots**
in the last hour before it shows a ppm at all. The low bands need many
more for a usable reading: at the default limit, a band warns only when
its error is at most a third of the limit, which takes about 29 spots an
hour on 20m, 114 on 40m and 447 on 80m (20 from 17m up).

**The suggestion replaces the current value.** The offsets are measured
after the `freq_correction_ppm` the daemon already applies, so the value
to save is not just the skew with its sign flipped. `manta status` works
it out: with `-2.00` applied and a skew of `+1.0 ppm`, the receiver's own
error is +3 ppm and the suggestion is `-3.00`, not `-1.00`. Each config
entry gets its own suggestion, pooling its bands by precision (bands with
at least 20 of its spots). Save the suggested value as the entry's
`freq_correction_ppm` and restart manta to apply it.

`manta status` shows both:

```text
RBN skew, last hour (+ means manta reads high; warns past 2 ppm for 30 min)
  BAND    OFFSET  SPOTS  SKEW            STATE
  80m     +14 Hz     41  +3.8 ±2.2 ppm   too few spots to warn
  40m     +21 Hz      8  -               needs 20 spots
  20m     +42 Hz     40  +3.0 ±0.6 ppm   WARN for 35 min
  15m     +63 Hz     24  +3.0 ±0.5 ppm   over 2 ppm for 12 min
Suggested freq_correction_ppm (replaces the current value; restart to apply)
  INPUT                               NOW     SET  ±PPM  NOTE
  [input]                            0.00   -3.02  0.36
```

`GET /status` carries the same numbers in its `rbn_compare` object.

**The warning.** A band is over the limit when it has at least 20 spots,
`skew_warn_ppm` is above 0, its error is at most a third of the limit,
and its skew is beyond the limit. After 30 minutes over without a break,
`/healthz` adds a line such as
`WARN rbn skew 20m: +3.0 ppm for 35 min (median +42 Hz, 40 spots; limit 2 ppm)`,
`manta_rbn_compare_skew_warning{band}` reads 1, and the shipped
[`MantaRbnSkew`](monitoring.md#mantarbnskew) alert fires. The probe itself
stays `ok` and 200: a drifted oscillator is no reason to restart a node.

**Save it.** `manta calibrate --from-rbn --config manta.toml` reads the
running daemon's suggestion from `GET /status` (its metrics listener, or
`--addr`), prints the evidence per band, and asks before saving it as the
entry's `freq_correction_ppm`, as the WWV path does; `--write` saves without
asking, and `--input NAME` picks an `[[input]]` entry. It opens no receiver,
so `manta run` keeps running. It saves only a reading good to ±1.00 ppm or
better, and refuses a daemon whose receiver came from a command-line source
flag. Restart manta to apply the value.

The reading is relative to the RBN skimmers that heard the same stations
over the last hour. SM7IUN's
[rbnskew](https://github.com/bjornekelund/rbnskew) compares against
GPS-locked anchor skimmers over a day, so expect the two to agree to
about a ppm, not exactly. A restart empties the reading.

## Co-located-spotter mode

To grade against one RBN skimmer only, for example one on the same antenna
as your node, name it:

```sh
manta run --config manta.toml --rbn-compare --rbn-spotter K5TR
```

or set `spotters = ["K5TR"]`. `--rbn-spotter` is repeatable and replaces
`spotters`. Reports from every other skimmer are dropped before grading,
so `rbnSkimmers` is at most the number of calls listed, and
`spotterFilter` in each grade records the list. A spot that skimmer did not
hear grades `none` even when other skimmers did: that is the point of the
mode, which is ROADMAP M3's co-located-spotter comparison.

## Offline replay

`manta grade` grades a recorded node's spot log against RBN's daily archive
with the same code the daemon uses, and the same inputs give the same
grades.

```console
$ manta grade --spots node/spots-2026-10-11.jsonl --rbn 20261011.csv
spots: 4 from 1 file (0 grade records skipped, 0 torn lines)
rbn: 9 CW reports from 1 file (1 non-CW row skipped), 2026-10-11 11:00:00Z to 13:00:00Z
spotters: all (excluding W3XYZ-1, W3XYZ)
window: 180 s; frequency disagreement over 500 Hz; speed disagreement over 3 WPM and 20% of RBN's median

band   spots  match  disagree  elsewhere  none  unknown  median offset
20m        3      1         1          1     0        0  +750 Hz (2)
40m        1      0         0          0     1        0  n/a
total      4      1         1          1     1        0
```

- `--spots` takes the `field-node.py record-spots` archive: SpotMessage
  JSON Lines files, or directories (which expand to `spots-*.jsonl`).
  Grade lines in them are skipped, and so is a torn last line. A file named
  twice (directly and through its directory, with `./` or `..`, or through
  a symlink) is read once; a hard link to it is a different file.
- Each spot excludes its own node's `deCall`, with and without SSID, as
  that node's live comparator does. Archives from several nodes can be
  graded together. Their calls are then listed on an `own calls, excluded
  only from that node's spots` line, and one node's RBN reports can confirm
  another node's spot. To stop that, for example for the primary a
  secondary forwards through, pass `--exclude-spotter`.
- `--rbn` takes RBN daily-archive CSVs, or directories of them. Unzip the
  archive first: `manta grade` refuses a `.zip`. A file named twice
  (directly and through its directory, with `./` or `..`, or through a
  symlink) is read once; a hard link to it is a different file.
- `--json` prints the grade records, one per line, in the live stream's
  shape.
- `--spotter`, `--exclude-spotter`, `--window-secs`, `--freq-tol-hz`,
  `--speed-tol-wpm` and `--speed-tol-pct` match the config keys, and are
  checked the same way: a `--spotter` or `--exclude-spotter` value that is
  not a plausible callsign is refused, and a tolerance past its key's
  maximum (10000 Hz, 20 WPM, 100%) is refused.
  `--config FILE` reads that file's `[rbn_compare]` tolerances, and flags
  win, so a node's own config replays with its live settings.
- `manta grade` never reads `MANTA_*` variables. Its output depends only on
  its inputs and flags.
- Each CSV covers only the span from its first to its last CW report.
  Spans join when they overlap or lie at most 60 s apart, as consecutive
  daily dumps do at midnight. A spot whose window reaches outside the
  joined spans, past either end or into a day left out between two CSVs,
  grades `unknown` unless matched, so give it the whole day's archive.

`scripts/score-against-rbn.py --node-log` wraps `manta grade` and unzips
the archive for you:

```sh
scripts/score-against-rbn.py --node-log node/ --rbn 20261011.zip --spotter K5TR
```

It finds the binary from `--manta PATH`, then `$MANTA_BIN`, then `manta` on
`PATH`, then `target/release/manta` and `target/debug/manta` in the repo.
Its output and exit status are `manta grade`'s. Without `--node-log` the
script scores a `manta decode` report, as before.

Live and offline grades can differ in one known way: a live RBN report is
timed by when it arrived, an archived one by RBN's own timestamp. That
matters only for a report within a few seconds of a window edge.

## Paired report against a co-located skimmer (MAN-97)

`manta grade --paired` compares one node's spots with one reference skimmer
on the same antenna, both ways, and prints one report:

- **recall**: the reference's reports that manta also spotted;
- **missed**: the reference's reports manta did not spot;
- **candidate false spots**: manta's spots the reference did not report.

Use it with a CW Skimmer Server (SkimSrv) on the same antenna, through
`--reference` and a saved copy of its telnet output, or with an RBN
skimmer's daily-archive rows, through `--rbn` and `--spotter`. It is the
offline mechanism behind a nightly report: run it from cron over the
night's files. No recorder or scheduler ships with manta.

### The reference transcript

`--reference` reads a skimmer's `DX de` lines saved to files, or
directories of them (their `*.txt` and `*.log` files, sorted). Both layouts
a skimmer's telnet port sends are read, by the same parser the skimmer
relay uses:

```text
DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z
DX de W3XYZ-#:  14030.0  DL1ABC         19 dB  28 WPM  CQ      0110Z
```

- A line may start with its UTC arrival time, then a space:
  `2026-10-31T00:05:00Z DX de ...`. That stamp is the report's time, and the
  line's own `HHMMZ` is ignored, as the live path uses arrival time. A
  stamp that is not a UTC time is an error naming the file and line.
- A line without a stamp takes its UTC day from the file name (the last
  real `YYYY-MM-DD` in it, as in `reference-2026-10-31.txt`) and its minute
  from its `HHMMZ`, at the minute's middle (30 s in), so its time is at most
  30 s off either way. A minute more than 12 hours before the previous
  date-less line's starts the next UTC day (`2358Z` then `0003Z`). Without
  a day in the file name such a line is an error naming the file and line.
  Keep one file per UTC day, or stamp every line.
- RTTY and other non-CW spot lines are counted and skipped, and so is any
  other line: a banner, a login prompt, a torn last line. Blank lines and
  CRLF endings are ignored.
- Each file covers only the span from its first to its last CW report, as
  each RBN CSV does, and spans join when at most 60 s apart. With
  `--paired`, only reports from an admitted spotter count toward the span.
- The node's own calls are not excluded: the transcript holds only that
  skimmer's spots, and a SkimSrv usually runs under the node's base call
  (manta `W3XYZ-1` beside SkimSrv `W3XYZ`). `--spotter W3XYZ` is accepted.
  With `--rbn`, own calls stay excluded as before.

`--reference` also works without `--paired`, giving the one-way table above
against a transcript.

This layout and the capture recipe below have not been checked against a
live SkimSrv's output. SkimSrv's own `spots.txt` file is not read: its
format has not been seen.

### Running it

```console
$ manta grade --paired --spots spots-2026-10-31.jsonl --reference reference-2026-10-31.txt \
    --passband-khz 14000-14070 --start 2026-10-31T00:00:00Z --end 2026-10-31T06:00:00Z
spots: 8 from 1 file (0 grade records skipped, 0 torn lines)
reference: 8 CW reports from 1 file (1 non-CW line, 1 other line skipped), 2026-10-31 00:05:00Z to 06:02:00Z
spotters: all
own calls are not excluded from a --reference transcript
window: 600 s; frequency disagreement over 500 Hz; speed disagreement over 3 WPM and 20% of RBN's median
paired: node W3XYZ-1, 2026-10-31 00:00:00Z to 06:00:00Z, passband 14000-14070 kHz
manta spots: 6 counted (1 out of passband, 1 out of window)
reference reports: 5 counted (0 other spotters, 1 out of passband, 1 out of window, 1 not a plausible callsign)
reference silence: longest 3:50:00 from 2026-10-31 02:00:00Z

recall: 3 / 5 reference reports that manta also spotted (60.0%)
missed: 2 reference reports (1 of them spotted by manta on another band)
confirmed: 3 manta spots the reference also reported (match 2, disagree 1)
candidate false: 2 / 5 manta spots the reference's span covered (40.0%): none 1, elsewhere 1
unknown: 1 manta spot whose window the reference's span did not cover

band   reports  found  missed  recall  spots  confirmed  none  elsewhere  unknown
20m          5      3       2   60.0%      6          3     1          1        1
total        5      3       2   60.0%      6          3     1          1        1

missed by manta (no manta spot of the call on the report's band within 600 s):
time                  call        freq kHz  dB  WPM  manta heard it on
2026-10-31 00:20:00Z  N4RV         14021.0  18   24  -
2026-10-31 01:10:00Z  DL1ABC       14030.0  19   28  40m

candidate false spots (no reference report of the call on the spot's band within 600 s):
time                  call        freq kHz  snr  conf  grade      reference heard it on
2026-10-31 01:25:00Z  K5XYZ        14012.3   15  0.71  elsewhere  40m
2026-10-31 03:00:00Z  W9ZZZ        14018.0    9  0.55  none       -

dB is the reference's own figure (500 Hz); snr is manta's stream snr (2500 Hz).
```

- `--passband-khz LO-HI` is required: count only spots and reports inside
  this range, in kHz. Repeatable. A SkimSrv often covers bands manta does
  not, and its reports there are not misses.
- `--start` and `--end` (UTC, `2026-10-31T00:00:00Z`) count only spots and
  reports at or after `--start` and before `--end`. Without them everything
  loaded counts.
- Filters choose what is counted, never what can match. A report just
  after `--end` can confirm a spot just before it, and manta's 40m spot can
  make a 20m report `elsewhere`.
- A reference report also needs a plausible callsign (`TEST` is not one:
  manta can never spot it, so it is not a miss) and a spotter the spotter
  filter admits.
- With `--rbn`, `--paired` needs `--spotter`, the co-located skimmer:
  against every RBN skimmer, each call heard anywhere would count as a miss.
- `--paired` grades one node: spots from two `deCall`s are refused.
- `--json` prints the report as one JSON object. Exit status is 0 whenever
  the report is printed: it makes no pass or fail judgement.
- `--passband-khz`, `--start` and `--end` need `--paired`.

| Metric | Definition |
|---|---|
| recall | found / counted reference reports. A report is *found* when manta has a spot of the same call on the same band within the window either side of it. |
| missed | counted reference reports that are not found. Of them, `missedElsewhere` were spotted by manta within the window on another band only. |
| confirmed | counted manta spots graded `match` or `disagree`: a same-call, same-band reference report within the window (`disagree` when the frequency or speed is outside the tolerances). |
| candidate false rate | (`none` + `elsewhere`) / (counted manta spots - `unknown`). `none`: the reference reported the call nowhere in the window, and its span covered the window. `elsewhere`: it reported the call only on another band. |
| unknown | counted manta spots with no match whose window reaches outside the reference files' spans. Left out of every rate. |
| reference silence | the longest gap between consecutive CW reports from an admitted spotter, on any band and with any call, inside `--start`/`--end`. |

found + missed always equals the counted reference reports, and match +
disagree + elsewhere + none + unknown the counted manta spots.

### Outages, the window and the silence line

- manta is taken to be up all night, so a manta outage shows as misses: an
  operator must see it. The reference is trusted only between each file's
  first and last report from an admitted spotter, so manta's unmatched
  spots outside that span are `unknown`, not false: a reference outage is
  not manta's fault. Another skimmer's reports in the same file, as in an
  RBN CSV, cover nothing. So two daily RBN CSVs join at midnight only when
  the selected skimmer's last report in the first and first report in the
  second lie at most 60 s apart; otherwise spots whose window reaches into
  that gap are `unknown`.
- Inside a file a reference outage shows only on the `reference silence`
  line. It is a hint to read the night's numbers with care, never used to
  grade.
- The window defaults to 600 s, manta's re-spot suppression interval, so a
  station both skimmers hear all night always has a reference report within
  the window of each manta spot. With the live comparator's 180 s, such a
  station grades `none` whenever the two skimmers' re-spots fall out of
  phase. `--window-secs` changes it; `--config`'s `window_secs` is not used
  (its other tolerances are).

### The paired report's JSON

One object per run, on one line. `recall`, `candidateFalseRate` and
`referenceSpan` are `null` when there is nothing to divide by or no CW
report; `start` and `end` are `null` when not given; `snrDb`, `wpm`, `snr`
and `decodeConfidence` are `null` when the source line lacks them. Times
are Unix seconds. It has all 65 keys below, in wire order (`a.b` is key `b`
of object `a`; `a[].b` is key `b` of each object in array `a`):

- `kind` (always `pairedReport`)
- `node` (the spots' `deCall`, empty when they carry none)
- `referenceSource` (`transcript` for `--reference`, `rbn` for `--rbn`)
- `spotters` (the spotter filter; empty means every spotter)
- `start` (`--start`, or `null`)
- `end` (`--end`, or `null`)
- `passbandsKhz` (each `--passband-khz` as `[lo, hi]` in kHz)
- `windowS` (the window either side of a spot or report, in seconds)
- `mantaSpots` (manta's side)
- `mantaSpots.loaded` (every spot read)
- `mantaSpots.outOfPassband` (spots outside every passband)
- `mantaSpots.outOfWindow` (spots inside a passband but outside `--start`/`--end`)
- `mantaSpots.counted` (spots inside both)
- `mantaSpots.match` (counted spots graded `match`)
- `mantaSpots.disagree` (counted spots graded `disagree`)
- `mantaSpots.elsewhere` (counted spots graded `elsewhere`)
- `mantaSpots.none` (counted spots graded `none`)
- `mantaSpots.unknown` (counted spots graded `unknown`)
- `referenceReports` (the reference's side)
- `referenceReports.loaded` (every CW report read)
- `referenceReports.otherSpotter` (reports from a spotter the filter rejects)
- `referenceReports.outOfPassband` (reports outside every passband)
- `referenceReports.outOfWindow` (reports outside `--start`/`--end`)
- `referenceReports.implausibleCall` (reports of a call manta could never spot)
- `referenceReports.counted` (the recall denominator)
- `referenceReports.found` (counted reports manta also spotted)
- `referenceReports.missed` (counted reports manta did not spot)
- `referenceReports.missedElsewhere` (missed reports manta spotted on another band)
- `recall` (found / counted, or `null`)
- `candidateFalseRate` ((none + elsewhere) / (counted - unknown), or `null`)
- `referenceSpan` (the admitted reports' span, or `null`)
- `referenceSpan.from` (the first admitted report's time)
- `referenceSpan.to` (the last admitted report's time)
- `referenceSpan.longestSilenceS` (the longest gap inside the window, or `null` with fewer than two reports there)
- `referenceSpan.longestSilenceFrom` (when that gap began)
- `bands` (one row per band, highest band first)
- `bands[].band` (the band name, `unknown` outside every band)
- `bands[].referenceReports` (counted reference reports on the band)
- `bands[].found` (of them, found)
- `bands[].missed` (of them, missed)
- `bands[].mantaSpots` (counted manta spots on the band)
- `bands[].match` (of them, `match`)
- `bands[].disagree` (of them, `disagree`)
- `bands[].elsewhere` (of them, `elsewhere`)
- `bands[].none` (of them, `none`)
- `bands[].unknown` (of them, `unknown`)
- `missed` (the missed reports, by time, call and frequency)
- `missed[].time` (the report's time)
- `missed[].call` (the reported call)
- `missed[].band` (the report's band)
- `missed[].freqHz` (the report's frequency, whole Hz)
- `missed[].snrDb` (the reference's own SNR figure, 500 Hz convention)
- `missed[].wpm` (the reference's speed)
- `missed[].spotter` (the reporting skimmer)
- `missed[].mantaBands` (bands manta spotted the call on within the window)
- `candidateFalse` (the candidate false spots, by time and spot id)
- `candidateFalse[].spotId` (the SpotMessage `id`)
- `candidateFalse[].time` (the spot's time)
- `candidateFalse[].call` (the spotted call)
- `candidateFalse[].band` (the spot's band)
- `candidateFalse[].freqHz` (the spot's frequency in Hz)
- `candidateFalse[].snr` (manta's stream SNR, 2500 Hz convention)
- `candidateFalse[].decodeConfidence` (manta's decode confidence)
- `candidateFalse[].grade` (`none` or `elsewhere`)
- `candidateFalse[].referenceBands` (bands the reference reported the call on within the window)

### A nightly run

An example capture and cron line, not run against a live SkimSrv. Save the
skimmer's telnet output with each line stamped by its arrival time, for
example with `nc` and `ts` from moreutils (log in with the node's call when
SkimSrv asks):

```sh
# Example only: stamp each DX de line with its UTC arrival time.
nc skimsrv.local 7310 | TZ=UTC ts '%Y-%m-%dT%H:%M:%SZ' >> /var/lib/skimsrv-capture/reference-$(date -u +%F).txt
```

Then grade the night once it is over:

```sh
# 06:15 UTC daily: the night 00:00-06:00Z (example; not run against a live SkimSrv)
15 6 * * * manta grade --paired --spots /var/lib/manta-field/spots --reference /var/lib/skimsrv-capture --passband-khz 14000-14070 --start "$(date -u +\%Y-\%m-\%dT00:00:00Z)" --end "$(date -u +\%Y-\%m-\%dT06:00:00Z)" > /var/lib/manta-field/paired-$(date -u +\%F).txt
```

The rules and their reasons are in
[docs/DECISIONS/2026-11-01-man97-paired-skimmer-report.md](../DECISIONS/2026-11-01-man97-paired-skimmer-report.md).

## Grading with the language prior enabled (MAN-317)

The prior ships off. Passing synthetic vectors alone does not authorize a
default flip. Collect a complete, paired seven-day live comparison before
proposing that change. No recorded node log for this experiment ships in the
repository, and this ticket does not claim that the live week passed.

Run the same binary on the same recorded IQ or simultaneous identical SDR
streams, once with `[decode] prior = "off"` and once with `prior = "on"`.
Keep every other setting fixed, including the SCP/cty versions, calls-heard
file, validation level, receiver frequencies and RBN tolerances. Save the
configs and list hashes with both sets of node logs. Use separate output
directories and listener ports for parallel nodes. Prior-enabled config:

```toml
[decode]
prior = "on"
prior_weight = 2.0
reading_margin = 3.0
held_readings = 4
```

Grade both sets against the same complete daily RBN archives and the same
reference skimmer and exclusions:

```sh
scripts/score-against-rbn.py --node-log prior-off/ --rbn rbn-week/ --spotter K5TR
scripts/score-against-rbn.py --node-log prior-on/ --rbn rbn-week/ --spotter K5TR
```

The script grades already-produced logs; the prior config belongs to the
node that produced them. It cannot apply a different decoder to a spot log.
Compare `manta grade`'s `band, spots, match, disagree, elsewhere, none,
unknown` summaries. Compute the `none` rate as `none / (spots - unknown)`
on each paired band and over all bands with covered observations. Require
at least one covered spot in each compared population and require the
prior-on rate not to exceed the prior-off rate. Report raw counts, missing
coverage and every band alongside the aggregate; do not count `unknown`
as evidence that the prior is correct.

Attach those summaries and the archived inputs to the separate default-flip
change. It must also show zero bogus calls on prior-on V8, V8w and VC3 with
the existing validation floors intact. A failed precision or live-rate bar
keeps the default off.

## Secondary skimmers and your own spots

Reports from your own callsign, with and without SSID, never count:
otherwise a node forwarding its spots to RBN would confirm itself.

A secondary skimmer that forwards spots through a primary's RBN connection
(see [secondary-skimmer-field-node](secondary-skimmer-field-node.md)) may
see its own spots come back under the primary's call. Which label RBN gives
them is not settled (MAN-96's open R14). If grades for your spots show your
primary in `rbnSkimmers` far more often than chance, add the primary's call
to `exclude_spotters`:

```toml
[rbn_compare]
enabled = true
exclude_spotters = ["W3XYZ"]   # the primary this node forwards through
```

## Troubleshooting

### `manta_rbn_compare_feed_connected` is 0

The feed is not logged in. Read the log for `rbn_compare: feed lost` or a
connection error, then:

- Run `manta doctor --config manta.toml` with the daemon stopped. A `FAIL`
  on `rbn compare feed` names the cause: a DNS failure, a refused
  connection, or no answer (a firewall dropping outbound TCP to
  `feed_port`).
- Check `manta_rbn_compare_feed_reconnects_total`. A climbing count with a
  reachable host means the connection opens and drops. Check that
  `login_callsign` is a callsign RBN accepts.
- While the feed is down, spots keep publishing as before. Their grades
  come out `unknown` unless RBN matched them before the outage.

### Many `unknown` grades

`unknown` means the feed was down for part of a spot's window. Each outage
turns up to `2 × window_secs` of spots into `unknown`. Look at
`manta_rbn_compare_feed_reconnects_total` and the `feed lost` log lines
around those spots. Spots published in the first `window_secs` after the
daemon starts can also grade `unknown`, because the feed was not yet up
for the start of their window.

A feed that goes silent is declared down only after 60 s without a line.
A spot whose window closes during that silence is held, and counted in
`manta_rbn_compare_pending`, for up to 70 s past its close: it grades as
soon as a report arrives, or `unknown` once the feed is declared down.

### Grades never appear

- Check that `manta_rbn_compare_pending` rises as spots publish. If no
  `manta_rbn_compare_*` family is on `/metrics` at all, comparison is not
  enabled: run `manta config check` and look for the `rbn_compare:` line.
- Grades come `window_secs` (default 180 s) after each spot, not at once.
- `manta_rbn_compare_ungraded_total` counts spots dropped without a grade:
  more than 20 000 waiting at once, or the comparator falling behind the
  spot bus.

### Every grade is `none`

Check `spotterFilter`. With `--rbn-spotter` or `spotters` set, only those
skimmers count; a call they did not hear is `none`. Also check that
`manta_rbn_compare_feed_reports_total` is rising: a feed that connects but
delivers no CW reports grades everything `none`.
