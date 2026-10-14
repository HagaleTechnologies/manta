# MAN-95: callsign-pattern rarity sets the repetition requirement

Date: 2026-10-14. Base: `9636fe0494b4bce7973c3e0c5ad854f9ebc16f73`.

The flat two-message gate admitted unusual callsign shapes as readily as common
ones. Default validation now requires 2, 3 or 5 message-distinct repetitions,
using an independent reference list derived from the vendored MASTER.SCP.

## Decisions

1. Use the publicly documented pattern rule: retain the first three characters,
   except two for letter-digit-letter and four for digit-letter-letter. Replace
   subsequent letters with `@` and digits with `#`. Apply this to the base call
   before `/`. Manta's grammar accepts suffix portables only, so prefix-portable
   combination rules do not arise. Examples: `K5ARH` becomes `K5@@@`, `3DA0RU`
   becomes `3DA0@@`, and `VE3ABC` becomes `VE3@@@`.
2. Vendor `crates/manta-spot/data/patterns.lst`, derived solely from the vendored
   `master.scp`. No Skimmer Server pattern entries were copied. An operator's
   `--scp` replacement does not change the pattern table. The consistency test
   compares the generated text byte for byte with the vendored artifact.
3. Keep one Rust rule in `pattern::pattern_of` and `pattern::derive_list`. The
   maintainer example `cargo run -p manta-spot --example gen_pattern_list`
   atomically replaces the list. This is an example, not a hidden CLI command.
4. Two or more distinct, grammar-plausible SCP calls sharing a pattern make it
   common (`+`). One makes it listed without `+`. Absence makes it unlisted.
   The vendored 2026.07.24 release produces 6,036 patterns, 4,018 common.
5. Default `[spot].validation_level = "normal"` requires 2/3/5 repetitions.
   `"minimal"` requires 2/2/2. These follow the documented Normal and Minimal
   pattern columns. **Counting differs from Skimmer Server:** manta counts
   message-distinct observations in its existing 90-second frequency-bucket
   history, while Skimmer uses occurrences in a 256-character buffer. A doubled
   call within one message still counts once. Five is therefore stricter here;
   `minimal` is available when that recall cost is undesirable.
6. Preserve exemptions. Allowlisted calls and beacons do not acquire a new
   repetition gate. Beacon speed checks still apply at track close. SCP
   membership continues to affect confidence and variant arbitration; it does
   not lower a pattern's requirement. Operator blocklist and notches still win.
7. Each recorded `Decision` carries `required_reps` and `pattern_tier`.
   `Validator::decide` fills them consistently for all decision paths, including
   early refusals. Exempt calls report 1 and no tier. Live `candidateDecision`
   exposes `requiredReps` and nullable `patternTier`. Spot wire formats stay the
   same; the admitted calls and their emission times can change.
8. Use `HashMap<String, bool>` only for pure pattern lookup, like SCP membership.
   Its iteration order never determines output order. Generation uses ordered
   collections. The bundled table is parsed from `include_str!`; pipeline
   consumers share supplied tables with `Arc`.
9. Operator files use `[+]PATTERN`, with optional spaces after `+`, blank lines,
   leading `#` comments, BOM and CRLF accepted. Duplicate `+` wins. Malformed
   lines fail with a path and line number; an empty list fails. Silent omission
   would change admission thresholds. `[spot].pattern_path` or `--patterns`
   replaces the list. CLI overrides environment overrides file; file-relative
   paths resolve from the config directory. `decode` ignores environment
   settings. Both new keys require restart, and SIGHUP reports that fact.
10. Add no metric family. Update `manta_candidate_repetitions` HELP to describe
    the tiered requirements. The live decision carries the tier. SETT advertises
    `vlNormal` or `vlMinimal` to match the configured level.
11. Leave Aggressive/Paranoid levels, automatic refresh/staleness warnings and
    the testkit's AA–AL 2x3 calls outside this change. Keep the V8/V8w fixtures,
    their floors and the coppa pin unchanged.

## Measurements on the implementation

The generator and `bundled_table_has_the_measured_shape` reproduce the planned
6,036/4,018 counts. Regeneration is byte-identical. Golden V46–V50 cover common
at two, listed at three, unlisted at five, minimal at two and both exemptions.
`unlisted_decisions_and_reclassification_keep_the_five_rep_requirement` proves
that a reclassified word keeps its old repetition count and the same threshold.

A release-built measurement program used the production
`manta_testkit::vectors::write_fixture_set` and `manta_engine::decode_wav`
with `PipelineConfig::validation_level` set to each level. It used the same
WAV quantization as the CLI golden tests and the unchanged coppa revision
`f8a4d16df7e5776a0756943c05712038774e6c70`.

| Vector | Minimal | Normal | Bogus at either level | Lost at normal |
|---|---|---|---|---|
| V8 | 50/50 | 48/50 | 0 | `AC3AGO`, `AC3GYH` |
| V8w | 28/50 | 27/50 | 0 | `AB2KLK` |

The five AB2/AC3 fixture calls are `AB2KLK`, `AC3AGO`, `AC3ECY`, `AC3GYH`
and `AC3PJI`. These 2x3 shapes are not represented in the vendored SCP's
AA–AL allocation block. The plan identifies them as invalid issued-call
fixtures; changing their generation would move every pileup baseline. Their
rarity cost is retained, rather than changing the fixtures or acceptance floors.

Production `Table::bundled().tier()` over
`crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv`
reproduces the planning sample: 26,802 spots, 957 calls and 177 skimmers.

| Classification | Spots | Share of all spots |
|---|---|---|
| Common | 24,655 | 91.9894% |
| Listed | 1,796 | 6.7010% |
| Unlisted | 225 | 0.8395% |
| Grammar-invalid | 126 | 0.4701% |

Among calls heard by one skimmer, 66/462 are unlisted (14.3%); among calls
heard by at least five, 7/334 are unlisted (2.1%). Those denominators include
invalid calls, as in the plan. Restricting denominators to grammar-plausible
calls gives 66/454 (14.54%) and 7/332 (2.11%). Genuine rare calls such as
`OF9X` and `MG3J` also land in unlisted: requiring more messages has a recall
cost, not just a false-positive benefit. The listed tier discriminates less
strongly, supporting the conservative common threshold of two SCP calls.

## Verification note

The plan's known `ka9q::tests::stash_takes_at_most_one_batch_per_call`
failure reproduced at the unmodified base in this container: 222 packets
retained instead of 256. The fixture sent 276 datagrams before reading,
using a small default socket receive buffer. The test now requests the
same `RCVBUF_BYTES` as production before sending. The original assertions
pass without changing production code or weakening the batch-limit check.

## Reference

The pattern rule and level table are described by the RBN blog's
[How Skimmer Server decides what to spot](https://reversebeacon.blogspot.com/2021/09/how-skimmer-server-decides-what-to-spot.html).
This is admission parity work, not a claim of identical validation semantics.
See [CONFIG.md](../CONFIG.md), [LIVE-STATE.md](../LIVE-STATE.md) and
[the data provenance](../../crates/manta-spot/data/SOURCES.md#patternslst).
