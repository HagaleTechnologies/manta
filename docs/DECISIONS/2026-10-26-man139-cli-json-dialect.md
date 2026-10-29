# MAN-139: decode, run and doctor `--json` use the JSON stream's camelCase names

## Context

manta printed two JSON dialects. The `:7301` spot stream and the
live-state stream use camelCase without unit suffixes for shared concepts:
`frequency`, `snr`, `trackId`, `spotType`, `closure`/`survivorTrackId`.
`manta decode --json`, `manta run --json` (alias `listen`) and
`manta doctor --json` printed the engine's Rust types through plain serde
derives. Their keys were snake_case with unit suffixes (`freq_hz`,
`snr_2500_db`). One SNR had three names (`snr_2500_db`, `snr_db`,
`snr_db_min`). Some values were Rust enums (`{"Char":"F"}`,
`{"Bookkeeping":{…}}`), doctor's duration was a raw `Duration`
(`{"nanos":0,"secs":10}`), and its verdict read `WeakNoDecode` in JSON but
`WEAK_NO_DECODE` in the text report.

The 2026-09-05 broad review raised this (lens 5 #15). Tony's 2026-10-24
decision (option A) scoped MAN-139 to `decode`, `run`/`listen` and
`doctor`, and kept the wire's `source: "skimmer"` and every server output
as they are.

## Decision

1. Rename on the serialized types, not in a new CLI layer. Serde
   attributes go on `DecoderEvent`, `ClosureKind`, `Glyph`,
   `TimingEstimates` and `ClassEstimate` (manta-decode), `Spot` and
   `SpotType` (manta-spot), and `DecodeReport`, `RttySignalReport`,
   `DoctorReport` and `Verdict` (manta-engine). `Glyph` and `ClosureKind`
   get hand-written serde impls. This overrides the research's
   recommendation of a CLI-side layer, for four reasons:
   - `manta-cli` is bin-only, so a CLI-side struct could not serve
     `examples/replay_spots.rs` or the integration tests.
   - `replay_spots` needs a `Deserialize` that matches the printed shape,
     which the attributes give for free.
   - `output_style.rs` already pinned "CLI JSON is the types' serde form";
     a parallel layer would leave a second, unused dialect on the types.
   - No server code serializes these types. manta-server builds its own
     wire structs (`SpotMessage`, the live-state records).
2. Naming. Every key is camelCase. A concept the spot or live-state stream
   also carries uses the stream's name: `frequency`, `snr`, `wpm`,
   `dxCall`, `decodeConfidence`, `trackId`, `spotType`, `closure`,
   `survivorTrackId`, `rst`, `qrlQuery`. Any other quantity keeps a short
   unit suffix, as the streams' `loHz` and `windowS` do: `centerFreqHz`,
   `sampleRateHz`, `markHz`, `ditMs`, `durationS`.
3. Values. An enumerated value keeps the spelling it has elsewhere. A glyph
   is its character or SPEC §4.4's prosign token (`"<AR>"`). `spotType`
   (`cq`, `de`, `beacon`, `unknown`) and `closure` (`signalEnded`,
   `bookkeeping`) use the live-state labels, and `TrackClosed` carries
   `closure` beside `survivorTrackId`. The doctor verdict and check status
   use the text report's words (`WEAK_NO_DECODE`, `PASS`). Event names stay
   SPEC §5's. doctor's `durationS` is a number of seconds.
4. The `{"spot": …}` line and the doctor object are serialized from small
   structs, not `json!` or a `Map`. Their keys come out in declaration
   order and f32 values print short, as everywhere else in the CLI JSON.
   `DoctorReport`'s SNR fields are reordered to min, median, max, the order
   of the text line.
5. The CHANGELOG entry goes under `### Changed`, marked breaking for
   scripts that parse `--json`, and not under `### Decoder output`. The
   decoder-output rule covers the spot set's content: which calls, when,
   and at what frequency, SNR, speed, type and confidence. A rename changes
   none of it, the reasoning MAN-83 applied to `decoderVersion`. The next
   release is MINOR anyway, because `[Unreleased]` already has
   `### Decoder output` entries. This overrides the research's
   recommendation 5. `[Unreleased]` bullets that named keys added since
   0.1.0 now use the new names, since those keys never shipped.
6. [docs/CLI-JSON.md](../CLI-JSON.md) lists every field and the names
   before MAN-139. A docs test compares the page with the serialized keys
   in both directions.
7. Every consumer moves in the same change: `scripts/summarize-listen-jsonl.sh`,
   `scripts/score-transcript.py`, `scripts/score-against-rbn.py`, their
   Python fixtures and `examples/replay_spots.rs`. A cargo test runs the
   three scripts on real `manta gen v1` output. Without `bash`, `jq` or
   `python3` it skips locally and fails on GitHub Actions, so CI cannot
   skip it silently.
8. No alias for the old names. A `decode --json` report saved before this
   change no longer loads in `replay_spots`, which names the fix: run
   `manta decode --json` again.

Two details settled during implementation:

- `no_summary_names_a_json_key` checks every verdict sentence against the
  report's own JSON keys, in camelCase and snake_case, plus
  `checksStatus`/`checks_status` and the old `snr_db_*` names. The plan
  asked it to reject any `_`-joined lowercase word, but `NOT_CW`'s sentence
  names the TOML key `non_cw_filter`, which an operator sets, so that rule
  would reject correct text.
- The workspace's serde_json has no `preserve_order` (MAN-285 kept it
  off), so a `serde_json::Value` object lists its keys sorted. The
  key-order unit tests in manta-engine read keys in document order through
  a test-only `json_keys_in_order` helper, a serde visitor.

## Evidence

- manta-decode `events.rs`: `every_event_is_camel_case_and_round_trips`,
  `json_without_additive_fields_still_deserializes`,
  `old_snake_case_event_json_is_rejected`, `timing_update_json_shape`,
  `constructors_fill_defaults`. `tree.rs`:
  `glyph_json_is_its_character_or_prosign_token`.
- manta-engine `lib.rs`: `decode_report_json_is_camel_case`. `doctor.rs`:
  `verdict_json_is_the_summary_label`,
  `doctor_report_json_is_camel_case_with_seconds`,
  `no_summary_names_a_json_key`.
- manta-cli unit tests: `fmt.rs` `spot_specimen_and_enum_serialization`
  and `spot_json_line_is_the_spot_in_declaration_order`; `doctor_checks.rs`
  `check_serializes_with_the_text_status_word` and
  `status_json_is_the_render_word`; `main.rs`
  `doctor_help_names_no_json_key`.
- manta-cli integration tests: `output_style.rs` keeps "CLI JSON is the
  library's serde form" and checks key order and short f32 text;
  `doctor_checks.rs` checks doctor's key set and the object printed when
  the receiver does not open; the golden-vector and input tests read the
  new names.
- `cli_json_consumers.rs`: `summarize_listen_jsonl_reads_real_run_json`,
  `score_transcript_reads_real_run_json`,
  `score_against_rbn_reads_real_decode_json` and
  `transcript_fixture_uses_only_keys_run_json_emits`.
- `docs_consistency.rs`: `cli_json_doc_lists_every_field_both_ways`.

## Not done

- The wire's `source: "skimmer"`, `SpotMessage`, `hello`, the live-state
  records, `spotGrade`, telnet, the RBN uplink, `/status`, `/metrics` and
  dispensa are unchanged. No manta-server file changed.
- Other commands' JSON is unchanged: `bench`, `check`, `calibrate`,
  `devices` and `status --json`, `oracle`, `grade` and `soak`.
  `calibrate --json`'s `bad_samples` stays snake_case. Follow-up ticket to
  file: "`manta bench`/`check`/`calibrate`/`devices --json` should use the
  CLI JSON convention from MAN-139 (`docs/CLI-JSON.md`)".
- Event names (`CharDecoded` and the rest) and the `{"spot": …}` wrapper
  are unchanged.
- Values are not rounded; the CLI keeps full precision (SPEC §1.4).
- No `snrRefHz` on CLI records. docs/CLI-JSON.md states the 2500 Hz
  reference once. `alternatives` stays `[[glyph, probability], …]`.
- Rust identifiers (`freq_hz`, `snr_2500_db` and the rest) and SPEC §5's
  Rust-notation block keep their names.
- `CLAUDE.md` is not edited; this record and its index row hold the
  outcome.
