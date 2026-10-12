# MAN-301: grade every manta spot live against what the RBN heard

Before this change manta published spots and never learned whether anyone
else heard them. The only comparisons with the Reverse Beacon Network were
offline batch scripts: `scripts/score-against-rbn.py` (MAN-20) and
`scripts/shadow-compare.py` (MAN-96).

With `[rbn_compare]` enabled, a `[server]` daemon now reads RBN's public CW
telnet feed, holds each published spot until a window around its time
closes, and grades it:

| Grade | Meaning |
|---|---|
| `match` | RBN skimmers reported the same call on the same band, within the frequency and speed tolerances |
| `disagree` | RBN reported the same call on the same band, but its modal frequency or median speed is outside tolerance. `disagreements` names which: `frequency`, `speed` or both |
| `elsewhere` | RBN reported the call only on other bands |
| `none` | RBN reported the call nowhere, and the feed was up for the whole window |
| `unknown` | No same-band report, and the feed was down for part of the window, so `none`/`elsewhere` cannot be claimed |

Each grade is a `spotGrade` record on the `:7301` JSON stream and a sample
in the `manta_rbn_compare_*` metric families. `manta grade` replays a
node's spot archive against RBN's daily CSV through the same code, and
`scripts/score-against-rbn.py --node-log` wraps `manta grade`. The
operator's guide is [rbn-comparison](../RUNBOOKS/rbn-comparison.md).

## Decisions

### 1. One grading engine, in Rust

The engine is `manta_server::rbn_compare::grade`. It has no I/O and no wall
clock: time comes in as Unix seconds. The live comparator
(`rbn_compare::live`) and `manta grade` are thin drivers around it, and
`score-against-rbn.py --node-log` runs `manta grade` rather than grading in
Python.

The ticket asks for the offline path to replay "through the same grading
code". A Python port kept in step by a comment (the
`_is_plausible_callsign` precedent) can drift silently. Both drivers also
derive their spot input from the same SpotMessage fields (`id`,
`timestamp`, `frequency`, `dxCall`, `wpm`) through shared helpers in
`spot_message.rs`, so live and offline grade the same numbers.

### 2. Correlate on (call, band)

A spot and an RBN report correlate when the callsigns are equal
(uppercase, exact) and both frequencies fall in the same band of
`band::band_for_freq_hz`. The existing scripts used a ±500 Hz or ±1 kHz-bin
window instead. That cannot tell a frequency disagreement from a miss: the
ticket's scenario 3 (manta 1.4 kHz from RBN on the same band) must grade
`disagree`, not `none`, and scenario 4 (the call heard only on 40m) must
grade `elsewhere`.

When manta's spot lies outside every band (`band_for_freq_hz` returns
`unknown`), same-band falls back to |manta − report| ≤ `freq_tol_hz`.

The statistics are over the same-band reports in the window:

- Modal frequency: report frequencies rounded to 100 Hz (RBN's
  resolution). A tie goes to the value nearest manta's frequency, then
  the lower one.
- Median speed: over reports that carry one. An even count averages the
  middle two, as `shadow-compare.py`'s `_median` does.
- `rbnSkimmers` counts distinct spotters; `rbnReports` counts reports.

Carried over from the existing scripts: callsigns compared uppercase and
exactly; the spotter filter applied to RBN reports before anything else;
CW rows only; the `-#` suffix stripped from spotter calls; the node's own
call excluded with and without its SSID. Not carried over: the callsign
grammar filter on RBN rows. It kept unspottable calls out of a recall
denominator. Here it would only turn a real match of an allowlisted call
into `none`.

### 3. Tolerance defaults

- `freq_tol_hz = 500.0`: both existing scripts match within 500 Hz. Here it
  is the disagreement threshold. Exactly 500 Hz is not a disagreement
  (strict `>`, as `score-against-rbn.py` compares).
- Speed: a disagreement needs more than `speed_tol_wpm = 3.0` WPM **and**
  more than `speed_tol_pct = 20.0` percent of RBN's median speed. No tool
  in this repo had gated on speed before. The ratio follows `oracle.rs`'s
  `wpm_ratio`; the 3 WPM floor keeps rounding at slow speeds from flagging.
  30 against 20 WPM is flagged; 22 against 20 is not. A spot or an RBN
  window without a speed never flags speed.

Both speed values are first estimates, not measurements. Recalibrate them
against field data, as MAN-96's D-H/D-I thresholds were marked.

### 4. A 180 s window

A report counts when |report time − spot time| ≤ `window_secs`, default
180 s (the ticket's default), inclusive at both ends. 90 s
(`score-against-rbn.py`) is shorter than one RepetitionGate cycle plus
RBN's relay latency, so real confirmations would fall outside. 600 s
(`shadow-compare.py`) lets a station's activity ten minutes away confirm an
unrelated spot. Operators can set 30 to 3600 s.

`windowClose` in the record is `spotTime + windowS`, not the time the
record was emitted, so a live grade and an offline grade of the same inputs
are byte-identical.

### 5. Live reports use arrival time; the archive uses its `date` column

The live feed's `DX de` line carries only `HHMMZ`, with no date (see the
captured line `LIVE_RBN_CAPTURE` in `rbn.rs`). A live report's time is
therefore the second it arrived. RBN relays within seconds. Offline,
`manta grade` reads the archive's full `date` column.

This is the one accepted input difference between live and offline
grading. It matters only for a report within a few seconds of a window
edge, which is small against a 180 s window.

### 6. The `unknown` grade

A grade can claim `none` or `elsewhere` only when the feed was up for the
whole window. Otherwise a feed outage would read as "RBN never heard it",
the same false-negative trap MAN-96 hit with gaps inside a comparison
interval. A same-band report found anyway still grades `match` or
`disagree`, since an outage cannot invent a report.

Live coverage is the feed's list of up and down intervals, plus the
horizon below which buffered reports were pruned: a spot whose window
starts behind that horizon has lost reports and is treated as not covered.
A feed that goes silent is declared down only by its 60 s idle timeout, at
its last line. Until then the still-open interval cannot vouch for a window
that closed during the silence, so a spot whose window only that interval
covers is held past its close until a report arrives at or after the
close, or for at most `FEED_SILENCE_GRACE_SECS` (70 s). By then a silent
feed has been declared down and the spot grades `unknown`; a feed still up
was delivering lines (non-CW ones, say), and the spot grades on the
interval. Reports a held spot's window needs are not pruned while it waits.
Offline coverage is each CSV's earliest to latest CW report time. Spans that
overlap or lie at most 60 s apart (the feed's idle timeout,
`ARCHIVE_JOIN_SECS`) join, so consecutive daily dumps cover a window across
midnight, while a day left out between two CSVs is not covered.

### 7. The record shape

A grade is a new record that references the spot's `id`. Spot publication
is append-only (`SpotBus::publish` broadcasts once), and spots are not
delayed or amended.

The record deliberately carries **no** top-level `dxCall`, `frequency` or
`timestamp`; it uses `call`, `mantaFreqHz` and `spotTime`. The probe, run
on `5346c33` before the change: today's `field-node.py validate_spot_line`
and `shadow-compare.py load_node_spots` were fed a grade line with and
without those three keys.

- With them, `field-node.py` accepted the grade as a spot and archived it,
  and `shadow-compare.py` then loaded two spots from one. MAN-96's D-I
  "uncorroborated" gate would have been silently skewed.
- Without them, `field-node.py` rejected the line
  (`invalid line skipped (missing dxCall)`) and archived nothing.

So an old copy of either script is safe against a new daemon. The new
copies recognise grades: `field-node.py record-spots` writes them to
`grades-<UTC day of windowClose>.jsonl`, and `shadow-compare.py` skips and
counts them. `grade_records_never_look_like_spots_to_field_node`
(`crates/manta-server/tests/field_node_contract.rs`) pins the missing keys.

Other field rules:

- `rbnModalFreqHz`, `rbnMedianWpm`, `rbnSkimmers`, `rbnReports` and
  `freqOffsetHz` describe the same-band reports only, and are `null`/`0`
  for `elsewhere`, `none` and `unknown`. `freqOffsetHz` is manta minus RBN.
- `rbnBands` lists every band RBN reported the call on in the window, in
  band-table order.
- `spotterFilter` echoes the active `spotters` list (`[]` means every RBN
  skimmer), so a stored grade says what it was graded against.

`disagree` is its own grade rather than a `match` carrying flags. Then
`match` always means agreement, and the five grade counts on a band sum to
the spots graded there, so no dashboard double-counts.

### 8. Grades share the JSON stream, marked by `kind`

Grades go out on `:7301`, raw TCP and WebSocket alike, and only when
`[rbn_compare]` is enabled. A node without the table emits a byte-identical
stream to before. Spot lines have no `kind` key and grade lines always have
`"kind":"spotGrade"`, so a consumer that wants spots only skips lines with
a `kind` key. The ticket asks for the grade "on the JSON stream", and a
second port would be one more listener to expose and firewall.

Telnet clients and the RBN uplink never see grades. `SpotBus` has a second
broadcast channel for the JSON stream only (`subscribe_json`, carrying
every spot plus grade lines); telnet and the uplink keep `subscribe`. The
JSON stream's existing lag, write-failure and shutdown-drain accounting
count grades as it counts spots.

### 9. A login that does not wait for a newline

The feed reader sends its login when the server's text ends a line, when
the unterminated text ends in `:` (optionally followed by spaces), or after
10 s of silence, whichever comes first. It does not reuse `uplink.rs`'s
login read, which waits for one newline-terminated line. MAN-90's plan
found (finding C7) that a prompt without a trailing newline makes that read
wait the 30 s idle timeout and reconnect forever. manta's own telnet server
sends `Please enter your callsign: ` with no newline, and DX-cluster
prompts conventionally do the same.

This is an assumption about RBN's relay, made without connecting to it
(see "Outstanding manual steps").

After login the reader reads bounded lines with a 60 s idle timeout (RBN's
CW feed is never silent for a minute) and reconnects on `backoff.rs`'s
ladder. Only `DX de` lines whose mode is `CW` with a `WPM` unit become
reports; RTTY and other lines are dropped.

### 10. No grading of file replays

`run --source FILE` or `[input] type = "file"` with comparison enabled is
refused before any I/O:

```
Error: RBN comparison grades spots against the live RBN feed, but this run replays a recording (--source). Grade a recorded node's spot log with `manta grade` instead.
```

A replayed recording's spot times are not the times RBN heard anything, so
every grade would be `none` or `unknown`. `manta grade` is the offline
path. `[rbn_compare]` enabled without `[server]` is refused too (the grades
are published on the JSON stream and `/metrics`, which only the daemon
runs), as is `--rbn-spotter` without comparison, and a `spotters` list
naming the station's own callsign. A disabled `[rbn_compare]` without
`[server]` loads with a note. Every key needs a restart; SIGHUP names a
changed key in its existing restart warning.

### 11. Metrics and their caps

Rendered only when comparison is enabled; a node without it shows no
`manta_rbn_compare_*` family. Band labels are `band_for_freq_hz` of manta's
spot frequency (13 bands plus `unknown`), and grade labels are the five
grades.

| Family | Type | Meaning |
|---|---|---|
| `manta_rbn_compare_grades_total{band,grade}` | counter | Spots graded since start |
| `manta_rbn_compare_grades_last_hour{band,grade}` | gauge | Spots graded in the trailing 3600 s. A series stays, at 0, once its key has been seen |
| `manta_rbn_compare_disagreements_last_hour{band,kind}` | gauge | `disagree` grades in the trailing 3600 s naming `kind` (`frequency` or `speed`). One spot with both counts once under each |
| `manta_rbn_compare_freq_offset_median_hz{band}` | gauge | Median `freqOffsetHz` over `match` and `disagree` grades in the trailing 3600 s. Absent when the band has none |
| `manta_rbn_compare_freq_offset_samples{band}` | gauge | How many grades that median is over |
| `manta_rbn_compare_feed_connected` | gauge | 1 while the feed is logged in and delivering lines |
| `manta_rbn_compare_feed_reports_total` | counter | CW reports accepted from the feed |
| `manta_rbn_compare_feed_reconnects_total` | counter | Feed connections lost or failed |
| `manta_rbn_compare_pending` | gauge | Spots waiting for their window to close |
| `manta_rbn_compare_ungraded_total` | counter | Spots never graded: the pending queue was full, or the comparator lagged the spot bus |

The trailing-hour gauges follow `manta_uplink_target_recent_reconnects`:
emission times in a bounded ring per key, pruned at scrape. The bounds:

- `MAX_RECENT_GRADES = 20_000` per (band, grade) key in the last-hour
  rings. A node grades far fewer spots per band per hour; the cap only
  stops a runaway from growing memory without limit.
- `MAX_PENDING_SPOTS = 20_000` spots waiting for their window, the same
  bound as the spot bus's occurrence table. A spot past it is counted in
  `ungraded_total`.
- `MAX_BUFFERED_REPORTS = 200_000` RBN reports, kept for `2 × window + 60 s`.
  At a contest peak of about 100 CW lines/s that retention holds about
  42 000 reports.

The median offset is a receiver-calibration signal: a steady offset of the
same sign on every band points at a ppm error (MAN-127).

### 12. Read-only toward RBN

The feed reader writes exactly one line, `login_callsign` followed by CRLF.
It sends no commands, no filters (`set/skimmer` or similar) and no spots.
`sends_nothing_but_the_login`
(`crates/manta-server/tests/rbn_compare_feed.rs`) streams 50 DX lines
through a fake feed and asserts that the bytes manta wrote over the whole
session equal `W3XYZ-1\r\n`. `manta doctor`'s `rbn compare feed` row opens
a TCP connection and sends nothing at all.

`login_callsign` defaults to `[server].station_callsign`, as
`[[rbn_uplink]].login_callsign` does. Reports from the node's own call,
with and without its SSID, never count. This change does not settle
MAN-96's open R14 (which spotter label RBN gives a secondary's forwarded
spots). The runbook tells operators to add the primary's call to
`exclude_spotters` if their forwarded spots appear under it.

### 13. Proposed dispensa `spotGrade` schema fragment

`spots.v1` is unchanged: spot lines are byte-identical. Grade lines are a
new record kind on the same stream, and dispensa is where the ecosystem's
JSON contract lives. This fragment is ready to lift into dispensa as a
`spotGrade` schema beside `spots.v1.schema.json`. Filing it there is a
follow-up outside this repo, as it was for MAN-102's `snrRefHz` (dispensa
Q-0065) and MAN-83's `decoderVersion`.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "spotGrade.v1.schema.json",
  "title": "spotGrade",
  "description": "A grade of one published spot against the RBN reports heard within windowS seconds of it. Shares the spot stream; spot records have no kind key.",
  "type": "object",
  "additionalProperties": false,
  "required": [
    "kind", "spotId", "call", "band", "spotTime", "windowClose", "windowS",
    "grade", "disagreements", "mantaFreqHz", "mantaWpm", "rbnModalFreqHz",
    "rbnMedianWpm", "rbnSkimmers", "rbnReports", "freqOffsetHz", "rbnBands",
    "spotterFilter"
  ],
  "properties": {
    "kind": { "const": "spotGrade" },
    "spotId": { "type": "string", "description": "The graded spot's id." },
    "call": { "type": "string", "description": "The spot's dxCall." },
    "band": { "type": "string", "description": "Band of mantaFreqHz, or \"unknown\" outside every band." },
    "spotTime": { "type": "integer", "description": "The spot's timestamp, Unix seconds." },
    "windowClose": { "type": "integer", "description": "spotTime + windowS, Unix seconds." },
    "windowS": { "type": "integer", "minimum": 30, "maximum": 3600 },
    "grade": { "enum": ["match", "disagree", "elsewhere", "none", "unknown"] },
    "disagreements": {
      "type": "array",
      "items": { "enum": ["frequency", "speed"] },
      "uniqueItems": true,
      "description": "Empty unless grade is disagree."
    },
    "mantaFreqHz": { "type": "integer", "description": "The spot's frequency, Hz." },
    "mantaWpm": { "type": ["integer", "null"] },
    "rbnModalFreqHz": { "type": ["integer", "null"], "description": "Modal same-band RBN frequency, Hz, at 100 Hz resolution. null unless grade is match or disagree." },
    "rbnMedianWpm": { "type": ["number", "null"], "description": "Median same-band RBN speed. null when no same-band report carried one." },
    "rbnSkimmers": { "type": "integer", "minimum": 0, "description": "Distinct RBN skimmers among the same-band reports." },
    "rbnReports": { "type": "integer", "minimum": 0, "description": "Same-band RBN reports." },
    "freqOffsetHz": { "type": ["integer", "null"], "description": "mantaFreqHz minus rbnModalFreqHz." },
    "rbnBands": { "type": "array", "items": { "type": "string" }, "description": "Every band RBN reported the call on in the window, in band-table order." },
    "spotterFilter": { "type": "array", "items": { "type": "string" }, "description": "The RBN skimmers counted; empty means all." }
  }
}
```

## Outstanding manual steps

These need an operator with internet access, a callsign and, for the last
two, a receiver. None was run in this environment: it has no route to
`telnet.reversebeacon.net`, and logging in sends a callsign to an external
service.

1. **RBN's login prompt bytes.** Point the feed reader at
   `telnet.reversebeacon.net:7000` for 10 minutes. Record here the exact
   bytes RBN sends before the login, confirm the login succeeds, and
   confirm only the login line was sent (`tcpdump`, `ss`, or the reader's
   debug log of written bytes). Until then, decision 9's tolerant login is
   an assumption about RBN's relay.
2. **Report rate.** In the same session, check that the CW reports per
   minute (`manta_rbn_compare_feed_reports_total`) are plausible against
   RBN's web view for the same minutes.
3. **Live/offline agreement.** Run `manta run --config node.toml
   --rbn-compare` for 30 minutes on a busy band while
   `field-node.py record-spots` archives the stream. Then run
   `manta grade --spots <archive> --rbn <that day's RBN CSV>` and compare
   with the live grades. Differences are expected only near feed gaps
   (`unknown`) and from arrival time against skimmer time for reports at a
   window edge (decision 5). Record the agreement rate here.
4. **Co-located spotter.** `manta run --config node.toml --rbn-compare
   --rbn-spotter <a nearby skimmer>`: check that `spotterFilter` echoes it
   and `rbnSkimmers` is at most 1.

## Evidence and tests

- Engine (`crates/manta-server/src/rbn_compare/grade.rs`): one test per
  ticket scenario (`same_call_same_band_within_tolerance_is_a_match`,
  `no_report_anywhere_with_full_coverage_is_none`,
  `frequency_and_speed_gaps_are_disagree_not_none`,
  `same_call_only_on_another_band_is_elsewhere`), plus the tolerance,
  window, spotter-filter and coverage edges.
  `grade_record_serialises_to_the_documented_shape` pins the four records
  in the runbook byte for byte. `incremental_grader_matches_batch` drives
  the live `Grader` and `grade_batch` over the same seeded inputs and
  requires equal output.
- Offline (`crates/manta-cli/tests/grade_cmd.rs`): `manta grade` against the
  fixtures in `crates/manta-cli/tests/fixtures/rbn_grade/`, including the
  K5TR co-located case and byte-identical repeat runs.
- Live (`crates/manta-server/tests/rbn_compare_acceptance.rs`): a fake RBN
  feed, a test clock and the real JSON stream. Each scenario's record
  arrives after its window closes; `live_records_equal_manta_grade_records`
  replays the offline fixture live and requires the offline expected
  records byte for byte.
- Feed (`crates/manta-server/tests/rbn_compare_feed.rs`): login after a
  prompt with and without a newline and after 10 s of silence; read-only;
  reconnect on idle, close and an oversized line.
- `scripts/tests/test_score_against_rbn_replay.py` (CI): the `--node-log`
  wrapper against a stub `manta`.
- `rbn_comparison_runbook_names_every_rbn_compare_metric`
  (`crates/manta-cli/tests/docs_consistency.rs`) renders the families from
  the real `Metrics` and requires each in the runbook.
