---
id: replay-spots-harness
title: How do you iterate on manta-spot rules without a 6-minute IQ decode?
kind: gotcha
status: current
maintainer: agent
sources:
  - crates/manta-cli/examples/replay_spots.rs
  - docs/DECISIONS/2026-09-07-man100-variant-arbitration.md
verified:
  commit: f34f9d0
  date: 2026-09-07
links:
  - spot-validation
---
`manta-spot::Validator::ingest` is a pure function of the `DecoderEvent` stream. `manta decode --json` already dumps that whole stream (`DecodeReport::events`) alongside its computed spots, so re-feeding a *saved* report through a fresh `Validator` reproduces the same spot list the original decode produced — without re-running the channelizer/decoder over IQ samples at all.

## How to use it

```
cargo build --release -p manta-cli --example replay_spots
cargo run --release -p manta-cli -- decode --json v8w.wav > v8w-report.json   # the slow part, once
./target/release/examples/replay_spots v8w-report.json calls.txt | tail -2
=== ingest 13.6 ms over 21028 events
=== spots=30 distinct=27 validated=22/50 bogus=5 ["AB2TTLK","K6F","W4KTNL","W6DW","W6JQ"]
```

`calls.txt` is a newline-separated list of the scene's genuine callsigns (a fixture manifest's known set) — it only labels each spot genuine/bogus in the output, it never affects what spots. Measured on the V8w 50-signal pileup fixture: the 6-minute end-to-end IQ decode collapses to ~14ms of validator time, identical spot list.

## Why this matters

Every `manta-spot` rule change (MAN-100's variant arbitration and message-aware repetition gate, and its remediation round's C1–C3 fixes) was iterated against this harness, then re-confirmed end to end through the real `manta decode` binary once. Without it, testing a rule-variant hypothesis against a real fading pileup costs a 6-minute IQ decode per iteration — intractable for the kind of measure-a-dozen-variants work `docs/DECISIONS/2026-09-07-man100-variant-arbitration.md`'s rule-variant table required. Any future `manta-spot` change touching a multi-signal scene should use this loop rather than re-running `manta decode` per iteration.

`DecoderEvent` derives both `Serialize` and `Deserialize`, so the harness deserializes the saved report's `events` array directly rather than reconstructing it field-by-field. `sample_ts` values in a saved report are raw-IQ sample indices at the WAV's own sample rate — the harness defaults to 96 kHz (every fixture `manta-testkit::vectors` generates) but takes an optional third `[sample_rate_hz]` argument that **must** be set to the WAV's actual rate when replaying a report produced with `manta decode --capture-rate-hz <n>` (Codex review, PR #133): `DecodeReport` carries no sample-rate field of its own, so the correct rate can't be read out of the report and has to be supplied.

`DecodeReport` likewise carries none of the original run's allowlist/blocklist/notch/freq-correction settings (Codex review, PR #133 round 3) — those affect admission, suppression, frequencies, and dedupe buckets in the real validator just as much as the sample rate does. `replay_spots` accepts `--freq-correction-ppm <n>`, `--allowlist <CALL>` (repeatable), `--blocklist <path>`, and `--notch <path>`, mirroring `manta decode`'s own flags of the same name — pass the SAME values the original `manta decode` run used, or the replayed spot list won't match it.
