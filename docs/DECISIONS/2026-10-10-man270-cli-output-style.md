# MAN-270: command output style

Human output uses deliberate labels and shared measurement formatters. Rust
Debug formatting is for developer and test diagnostics, not operator results,
application errors or daemon logs. `SpotType` has human Display labels; its
serde spellings remain unchanged.

## Measurements

| Measurement | Human text |
|---|---|
| RF or tone frequency | `14012.3 kHz`, one decimal |
| SNR | `20 dB`, integer, native 2500 Hz reference |
| Speed | `20 WPM`, integer |
| Confidence | `0.79`, two decimals |
| Spot context | `CQ`, `DE`, `BEACON`, `unknown` |
| Missing speed or non-finite measurement | `unknown` |
| Sample rate | `48000 Hz sample rate` or `sample_rate_hz=48000` |

Integer dB and WPM round to nearest, with halves away from zero. Rounded
negative zero becomes `0`. Frequency and confidence use Rust's fixed fractional
precision. Presentation does not clamp or modify measurements.

`manta-server::human` supplies the measurement formatters. CLI report composition
lives in `manta-cli/src/fmt.rs`. For example:

```text
frequency: 14012.3 kHz  speed: 20 WPM  spots: 1
SPOT: W1AW (CQ) 14000.8 kHz 29 dB 20 WPM conf=0.79
```

## Streams

| Command | stdout | stderr |
|---|---|---|
| `decode` | Decoded text or existing JSON report | Text-mode summary and diagnostics |
| `run`, alias `listen` | Human spot lines or existing JSON Lines | Character monitor, startup, logs, warnings, errors |
| `gen` | Empty | Fixture-written confirmation or error |
| `soak` | Human report | Warnings or execution errors |
| `doctor` | Human report or existing JSON | Warnings or execution errors |
| `status` | Existing human or JSON status document | Query/config errors |
| `config check` | Existing exact-value configuration summary | Notes and errors |
| `config init` | TOML only with `--out -` | Written-file confirmation or error |
| `oracle` | Existing JSON summary | Diagnostics and errors |

`manta run --config manta.toml > spots.log 2> monitor.log` now captures spots
in the first file and monitor text plus diagnostics in the second. Scripts
previously collecting spots from stderr must change their redirection.

The monitor ends its partial line at EOF or error. Diagnostics terminate any
partial monitor line before writing a complete record. Monitor state and writes
are coordinated; tracing holds the stderr lock for the entire record. Spot
publishing, server draining, timing and JSON serializers are unchanged.

## Errors and quoting

Application errors occupy one physical stderr line, beginning `error:`. Context
and cause Display text retain their order, separated by `: `. Empty causes do
not introduce empty segments. Control characters become visible escapes, such
as `\n`, `\t` or `\u{1b}`. Leading, interior and trailing spaces, ordinary Unicode
and already escaped text are preserved. A separately printed `hint:` line is
reserved for a concrete suggestion; errors need not include one.

Quoted inspection values use explicit JSON string quoting plus terminal control
escaping, never arbitrary Debug rendering. Single-quoted error values escape
embedded quotes. Credentials remain redacted before rendering. Protocol peer
text retains its existing escaping at ingress and remains control-safe in logs.

Status config errors now use the same single-line convention. This supersedes
the earlier multiline human TOML-snippet expectation: source text, indentation,
caret and line/column information remain present as visible escaped text.
Status query/config errors still exit 2. Healthy/unhealthy status reports still
exit 0/1. Other application failures and failed soak reports still exit 1.
Clap retains its help/usage layout, lowercase `error:` prefix, exit 2 on usage
errors and exit 0 for help/version. Custom parser causes avoid repeating clap's
own flag/value description.

## Machine and inspection contracts

- JSON uses existing serializers, field names, enum spellings and numeric values.
  Display rounding does not change decoded events, spot confidence or reports.
- `SpotMessage` retains its existing whole-Hz frequency and integer SNR/WPM
  conversion. Confidence stays full precision, including values adjacent to a
  consumer's threshold. Its SNR reference is 2500 Hz.
- RBN keeps the measured two-decimal kHz layout from MAN-88 and the 500 Hz SNR
  conversion from MAN-102. No wire formatter or schema changes.
- `config check` echoes original keys and values, including
  `freq_hz=14012349.9` and fractional gain. Those are exact settings, not measured
  display values. `config init` remains valid TOML. Invalid-input errors may
  quote the exact supplied number and unit.
- Sample rates and observation durations retain their existing units. A sample
  rate is not an RF frequency measurement.

## Report meaning

A soak reports events, worst sampled peak-RSS growth relative to the baseline,
and whether it caught a panic. Its result uses the existing `soak_passed` rule:
no caught panic and growth strictly below 200 MiB. An execution error before a
report exists prints only an error. No duration or overrun claim is added.

```text
soak: passed
events: 2551
RSS growth: 0.0 MiB
panicked: no
```

Zero events do not establish a working receiver. RSS sampling begins after the
existing warmup; zero RSS growth on a short soak may mean no post-warmup sample.
A short successful run establishes neither sustained memory stability nor a
hardware milestone or a requested observation duration.

Doctor retains its source-sample observation duration, TrackMeta statistics,
verdict calculation and verdict wording. Its SNR label explicitly identifies the
2500 Hz reference. Missing statistics distinguish no promoted track from a
promoted track that produced no TrackMeta before the run ended.

## Regression coverage

Pure renderer tests pin values and absent states. Subprocess tests capture the
two streams independently and check errors, exit codes, reports, deterministic
spot replay and unchanged JSON. A dev-only `syn` visitor scans production sources
and nested macro token trees, including optional input modules, for Debug output.
Test-only items, assertion/panic diagnostics and the separate internal
`man19-soak` binary are outside this guard. The guard supplements behavior tests;
it cannot prove how every third-party error implements Display.
