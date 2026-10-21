# MAN-14: each receiver's hearing on /metrics, the status line and /status

Date: 2026-10-19. Ticket: MAN-14, rescoped on 2026-10-19 after dispensa
Q-0073 (answered by cqdx in dispensa#160). Builds on MAN-13
(`docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md`). Baseline: main
`80cc42d`.

## Problem

With several `[[input]]` entries (MAN-13), an operator could not tell a
deaf, overloaded or miscalibrated receiver from one that hears.
`manta_source_health{source}` and `/healthz` measure sample delivery; the
only spot counter, `manta_spots_total`, is node-wide; and MAN-13 D6's
cross-input dedupe withheld a second receiver's report without counting it
anywhere.

Reproduced on `80cc42d`: in a two-input daemon whose second input read
digital silence, `manta_source_health{source="deaf"}` read 1 and `/healthz`
said `source deaf: healthy` for the whole run. With an identical second
input instead, the opt-in live-state stream carried 11 `candidateDecision`
records withheld for `dedupe`, and no public surface showed any of them.

## Scope

MAN-14 first asked for a per-receiver identity on the wire: on RBN, or as a
receiver field on spots. MAN-13 D12 kept the wire output unchanged and
left per-input identity to this ticket. dispensa Q-0073 asked whether the
ecosystem needs a receiver field on spots; cqdx answered in dispensa#160
that it does not. MAN-14 therefore shows each receiver's hearing on manta's
own operator surfaces and leaves the spot wire alone. Attribution uses
MAN-13 D3's track-id arithmetic, `(track_id − 1) mod N`, never a wire
field.

## Decision

| Surface | What changes (two or more `[[input]]` entries) |
|---|---|
| `/metrics` | Four `{source}` families: counters `manta_input_spots_total`, `manta_input_distinct_calls_total` and `manta_input_cross_input_dupes_total`, gauge `manta_input_silent`. Every input's series is 0 from startup |
| Status line | ` hearing=<name>:<spots>/<distinct calls>/<cross-input dupes>,...` after ` inputs=k/n`, one triple per input in config order |
| `GET /status` | An `inputs` array, one row per input; `schema_version` stays 1. `manta status` prints it as an Inputs table (NAME, SPOTS, CALLS, XDUPES, LAST SPOT, SILENT) |
| stderr | The silent-input warning and its recovery line |
| Config | `[server].silent_input_warning_secs`, default 600 |
| `/healthz`, spot JSON/WebSocket, telnet, RBN uplink, `SPOT:` lines, `docs/SPOT-SCHEMA.md` | Nothing |

With one input, the four families render their `# HELP`/`# TYPE` lines and
no samples, and the status line and `/status` are unchanged.

The warning and recovery lines:

```text
warning: input deaf: no spots decoded for 600 s while input live decoded 7 in that window (counted in manta_input_silent{source="deaf"}; warning repeats at most once per silent_input_warning_secs)
input deaf: decoding again after 1800 s of silence
```

The decisions, each with what it rules out:

- **D1. Four new `{source}` families.** This knowingly reverses MAN-13 D9
  ("No new families") for the follow-up MAN-13 left here. New names only:
  no existing family is renamed, because the MAN-96 field-node ledger reads
  the existing names. Rejected: a `{source}` label on `manta_spots_total`.
  It stays the unlabelled node total, and in multi-input mode the
  per-input spots sum to it, as MAN-128 D1 kept the aggregate beside
  `manta_spots_by_band_total`.
- **D2. The dupes label is the withheld input.** A withheld report shows
  that the input that lost heard the station too. Counted against the
  input that won, it would add nothing that input's spot count does not
  already say. A receiver with dupes but few spots hears and loses the race.
- **D3. Silence is not delivery loss.** `/healthz`, `manta_healthy` and
  MAN-13 D8's `source <name>:` lines are unchanged. With two or more
  inputs `/healthz` passes while any one input is healthy; folding silence
  into that verdict would change D8's meaning without saying so, and a
  `MantaInputDown`-style alert on it would be confused with a dead input.
  The gauge, the warning and `/status` carry silence.
- **D4. The window.** Default 600 s; `0` turns the check off; otherwise
  60 to 86400, validated at load like `history_minutes`, naming the key and range. Ten times the
  house's 60 s operator interval because a quiet band is healthy: a 60 s
  window would flag a receiver whose segment is briefly quiet. The bar is
  at least one spot from another input in the same window, so another
  receiver has shown the band is open. The check runs once per window, so
  the warning repeats at most once per window. An input whose sample
  delivery went unhealthy during the window is not flagged: its `lost` or
  `stopped` line, `manta_source_health` 0 and `MantaInputDown` already
  report it. When every input is quiet, none is flagged and the gauge
  reads 0.
- **D5. No alert rule or dashboard panel in this ticket.**
  `docs/RUNBOOKS/monitoring/` is untouched. Whoever adds one should read
  MAN-129 D4 first: an `increase()` rule false-fires after a scrape gap,
  and a restarted process needs an uptime guard.
- **D6. Daemon mode only.** The check runs only under `manta run` with
  `[server]` and two or more inputs. Without `[server]` there is no
  `/metrics`, `/status` or status line to report silence on, and with one
  input there is no other receiver to compare against.
- **D7. Status-line grammar.** ` hearing=name:s/d/x,...`, gated on two or
  more inputs exactly like ` inputs=k/n`. Rejected: a separate per-input
  log line each interval. One segment keeps one line to grep and leaves a
  one-input line byte-identical. A lost input keeps its last counts;
  ` inputs=k/n` already reports the loss.
- **D8. Distinct calls and label values.** Each input keeps a
  `BTreeSet<u64>` of 64-bit FNV-1a callsign hashes: deterministic, no new
  dependency, ordered like the rest of manta's state. It holds at most
  20,000 calls per input (160 KB of hashes, before set overhead), with a
  one-time warning when an input reaches the cap; past it the family
  undercounts, as its `# HELP` says. A hash collision also undercounts by
  one, which at 20,000 entries in 2^64 does not happen in practice.
  `{source}` values are interpolated unescaped, like the existing
  `{source}` families: MAN-13 D10's name charset
  `[A-Za-z0-9][A-Za-z0-9._-]{0,31}` holds nothing Prometheus escapes.
- **D9. `/status` rows are snake_case.** The plan sketched camelCase
  (`spotsTotal`), but every other `StatusDoc` field is snake_case
  (`schema_version`, `uptime_seconds`, `uplink.connected_targets`), so
  each row is `name`, `spots_total`, `distinct_calls`,
  `cross_input_dupes_total`, `silent` and `last_spot_age_seconds` (`null`
  before the input's first spot). camelCase belongs to the spot wire, which
  this document is not. The array is additive, so `schema_version` stays 1
  (`STATUS_SCHEMA_VERSION`'s rule), and it is absent with one input.
- **D10. The hearing state lives in `Metrics`.** One row per input,
  kept in registration (config) order, so the status-line task and
  the `/status` builder read it with no new plumbing. The multi-input
  daemon publishes all four families at 0 for every input right after the
  spot server starts, beside MAN-234's `manta_input_bad_samples_total`
  series, so a configured receiver that never hears shows from the first
  scrape.

## Evidence

- `dedupe::tests::assess_classifies_each_withhold_reason`,
  `should_emit_agrees_with_assess` (manta-spot): the withhold reason.
- `validator::tests::a_cross_input_duplicate_counts_against_the_withheld_input`,
  `no_cross_input_duplicate_counts_nothing`: D2's count.
- `listen::tests::back_publishes_cross_input_dedupe_counts_per_withheld_input`
  (manta-engine): the back end's observer.
- `metrics::tests::input_hearing_families_are_labelled_eager_and_never_go_backwards`,
  `status::tests::status_line_appends_hearing_after_inputs`,
  `status_doc::tests::status_doc_carries_input_hearing_rows_only_with_two_or_more_inputs`
  (manta-server): the three surfaces.
- `multi_run::tests::hearing_counters_attribute_spots_and_distinct_calls_by_track_id`
  and `silent::tests::*` (manta-cli): attribution and the D4 rule.

## Determinism

The counters and the silence check are side outputs: no spot, dedupe or
validation decision reads them, and the distinct-calls set is ordered and
hashed without a random seed. File input still gives byte-identical spot
logs.

## Follow-ups (for the owner to file)

- **FU-1** An alert rule and dashboard panel for `manta_input_silent` and
  the per-input spot rate (D5).
- **FU-2** MAN-13 D6's parked refinement, honouring another input's much
  stronger report in the cross-input dedupe, can now be sized from
  `manta_input_cross_input_dupes_total`; it stays with MAN-16.
