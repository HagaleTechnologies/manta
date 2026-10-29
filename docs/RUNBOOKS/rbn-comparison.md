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
rbn_compare: enabled=true feed=telnet.reversebeacon.net:7000 login_callsign=W3XYZ-1 window_secs=180 freq_tol_hz=500 speed_tol=3 WPM/20% spotters=all exclude_spotters=none
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

### The median offset is a calibration signal

`manta_rbn_compare_freq_offset_median_hz` is manta's frequency minus RBN's,
over the spots both heard. RBN's own skimmers are calibrated against each
other, so a steady offset of the same sign on every band points at your
receiver's frequency reference, not at RBN. Correct it with
`input.freq_correction_ppm` (MAN-127). A ppm error grows with frequency:
+40 Hz on 20m is about +2.9 ppm, and would show as about +20 Hz on 40m.
Check `freq_offset_samples` before acting: a median over a handful of
grades is noise.

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
