# MAN-97: `manta grade --paired` grades a node against a co-located skimmer both ways

Date: 2026-11-01. Ticket: MAN-97 (broad review 2026-09-05, lens 4 #27,
action A-12), narrowed by the 2026-10-31 backlog sweep to "extend grade to
paired spot logs with fixtures; running on a real feed is out of scope".

## Context

The ticket asks for a nightly report comparing manta's spots with a
reference CW Skimmer Server (SkimSrv) on the same antenna: recall (the
reference's spots manta also found), the spots manta missed, and manta's
spots the reference did not report (candidate false spots). It is the
mechanism behind MAN-20's recall and false-spot numbers without a live
outbound connection to the RBN.

`manta grade` (MAN-301) graded each manta spot against RBN's archive, one
way only. Three runs at `d144e36`, with scratch files outside the repo,
showed why that was not enough:

1. **No recall.** A SkimSrv-shaped CSV (spotter `W5AU`, CW reports for
   W1AW, N4RV, W1AW and DL1ABC), graded with `--spotter W5AU` against the
   `rbn_grade` fixture's four spots, printed `match 1, disagree 1, none 1,
   unknown 1`. The reference's N4RV report appeared nowhere: there was no
   recall and no list of misses.
2. **The node's own base call.** The same CSV with the spotter renamed
   `W3XYZ` (manta `W3XYZ-1` beside its primary `W3XYZ`, the MAN-96 layout):
   `--spotter W3XYZ` was refused as the node's own call, and without it
   every W3XYZ report was dropped silently, giving `none 2, unknown 2`. A
   SkimSrv under the node's base call could not serve as the reference.
3. **Phase-offset re-spots.** manta spots W1AW at 12:00:00 and 12:10:30;
   the reference reports it at 11:50, 12:05, 12:15 and 12:40.
   `--window-secs 180` graded both spots `none`; `--window-secs 600`
   graded both `match`.

## Decisions

### D1: the reference is a skimmer's `DX de` transcript

`--reference PATH` reads a skimmer's telnet output saved to files, or
directories of them (`*.txt`, `*.log`), through the relay's own line parser
(`manta_server::relay::skimmer_line::parse`), which reads both the RBN
layout (with a mode column) and the skimmer layout. A line may start with a
UTC stamp, which is then the report's time; the line's own `HHMMZ` is
ignored, as the live path uses arrival time. A line without a stamp takes
the last real `YYYY-MM-DD` in its file name as its day, and its `HHMMZ` at
the minute's middle (30 s in), so it is at most 30 s off. A minute more
than 12 hours before the previous date-less line's starts the next UTC day;
a smaller step back stays on the same day. A date-less line with no day in
its file name, or a stamp that is not a UTC time, is an error naming the
file and line. Non-CW spot lines and other lines (banners, prompts, a torn
last line) are counted, not errors. Each file is one archive, covering its
first to last CW report, exactly as each RBN CSV is.

`--rbn` and `--reference` are one required, mutually exclusive choice.
With `--rbn` every existing output is byte-identical.

### D2: the reverse pass uses MAN-301's (call, band, window) rule

`grade_references` grades each reference report against manta's spots
under the rule `grade_batch` uses forward: a same-call spot on the same
band within the window either side is `found`; a same-call spot within the
window on another band only is `elsewhere` (a miss, listed with the bands
manta heard it on); no same-call spot is `missed`. Frequency and speed gaps
do not make a miss: a 1.4 kHz same-band pair is `disagree` forward and
`found` backward, never a miss plus a false spot. Outside every band,
"same band" falls back to the frequency tolerance, one helper (`same_band`)
shared with `grade_with`, whose grades are unchanged.

The buckets: recall = found / counted reference reports; candidate false
rate = (`none` + `elsewhere`) / (counted manta spots - `unknown`);
confirmed = `match` + `disagree`. The report makes no pass or fail
judgement and exits 0, like `shadow-compare.py`.

### D3: the paired window is 600 s, and `--config`'s window is not used

`--paired` defaults to a 600 s window, manta's re-spot suppression
interval (`manta_spot::dedupe`'s `SUPPRESSION_SECONDS`, MAN-96 D-H), so a
station both skimmers hear all night always has a report within the window
of each manta spot (run 3). The live comparator's 180 s suits RBN's many
skimmers, not one. `--window-secs` wins; `--config`'s `window_secs` is
ignored under `--paired`, while its other tolerances apply.

### D4: filters choose what is counted, never what can match

A spot or report counts when it is inside a `--passband-khz` range and
inside `[--start, --end)`. A reference report also needs a spotter the
spotter filter admits and a callsign `manta_spot::grammar::is_plausible`
accepts. Every loaded spot and report stays match material, so a report
just after `--end` can confirm a spot just before it, and manta's 40m spot
can make a 20m report `elsewhere`. This is `shadow-compare.py`'s edge
margin applied to both the window and the passband. A passband is
required: a SkimSrv often covers bands manta does not.

### D5: coverage is asymmetric

manta is taken to be up all night, so a manta outage shows as misses: it
is a manta failure an operator must see. The reference is trusted only
between each file's first and last report, so manta's unmatched spots
outside that span grade `unknown` and leave every rate. Inside a file a
reference outage shows only on the `reference silence` line (the longest
gap between consecutive admitted reports inside the window), a hint never
used to grade.

### D6: own calls are not excluded from a transcript

With `--reference`, neither the own-call exclusion nor the own-call
`--spotter` refusal applies: a transcript holds only its own skimmer's
decodes, and a SkimSrv usually runs under the node's base call (run 2).
With `--rbn` both stay: a node's forwarded spots must not confirm
themselves.

### D7: the grammar filter applies to reference reports only

MAN-301 §2 dropped the callsign grammar filter on RBN rows: it had kept
unspottable calls out of a recall denominator, and the one-way grade has
none, so it would only have turned a real match into `none`. The paired
report has a recall denominator again: a
reference report of a call manta can never spot (`TEST`) is not a miss, so
it is counted as `implausibleCall` and left out. manta's own spots already
passed manta's validator.

### D8: one node only

`--paired` refuses spots from more than one `deCall`: one node's report
against one reference. Grade each node's archive apart.

## Scope

Not built: a recorder for a SkimSrv's telnet output, a systemd timer or
shipped cron unit, a `--markdown` form, a pass or fail threshold, parsing
SkimSrv's own `spots.txt` (its format has not been seen), and corroborating
candidate false spots with other RBN skimmers. No real feed was run and no
numbers are published (the scope note). The runbook section
(`docs/RUNBOOKS/rbn-comparison.md`, "Paired report against a co-located
skimmer") shows an example capture and cron line, marked as not run.

## Open external step

The transcript layout and the capture recipe have not been checked against
a live SkimSrv's output. Check the reader against a real SkimSrv capture,
for example from the `aggregator-rbn-capture.md` volunteer. This is not an
acceptance criterion of this ticket.
