---
id: spot-validation
title: How does manta decide a decoded call is trustworthy enough to spot?
kind: subsystem
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - docs/SPEC-decode-core.md#46-per-callsign-confidence-consumed-by-manta-spot
  - docs/SPEC-decode-core.md#9-configuration-keys
verified:
  commit: fefb469186bfa83fd546cfe8843d88e1978ad91c
  date: 2026-09-09
links:
  - decode-chain
  - spot-output-contract
  - live-hardware-field-testing
---
Decoded CW text is noisy, so validation — not decoding — is what makes a spot trustworthy. Per track, over a rolling text window, `manta-spot` parses CQ/DE context, checks callsign plausibility against a bundled cty.dat prefix list, optionally cross-checks the SCP super-check-partial list, requires a call to repeat before first spot, and dedupes/aggregates re-spots. Two MAN-28 exemptions cut across that and are easy to miss: a BEACON-tagged message is exempt from the repetition requirement, and an operator-allowlisted callsign is exempt from the *requirement to obtain* a context match (the parse itself always still runs, and a real match still sets the type), from the grammar/cty.dat check *and* from the repetition requirement. The full pipeline and its parameters are described in ARCHITECTURE §6 — this page is the map, not the spec.

## How it works

- CQ/DE/beacon context parse sets spot type (carried in the RBN flag): ARCHITECTURE §6.1.
- cty.dat prefix lookup rejects unallocated prefixes — unless the callsign is operator-allowlisted, which is the *only* exemption from this check (a BEACON tag is **not** one: beacons still have to pass grammar/cty.dat); SCP membership only *raises* confidence, never gates (rare/new calls must still spot, not just well-known ones): §6.2–6.3.
- Repetition requirement (a call must decode more than once within a window before first spot) is the main garble filter, and has two exemptions: BEACON-tagged messages (NCDXF-style beacons ID once per power-step cycle and legitimately won't repeat in the window) and operator-allowlisted callsigns both spot on their first decode once the track has real `TrackMeta` telemetry — the gate is lifted, the `r=1` confidence penalty is not. "First decode" is not "immediately": `Validator::try_spot` returns without evaluating any candidate while the track has no metadata yet, so an exempt candidate that completes before the track's first `TrackMeta` event stays pending and emits only once metadata arrives (golden vector V22): §6.4, SPEC §4.6.
- Operator allowlist (Watch List) is the broader of the two exemptions: a listed call bypasses step 1's context-match *requirement* plus steps 2 and 4 — it spots with no CQ/DE/UP/beacon framing at all, tagged `Unknown`, and is reclassified promotion-only if framing arrives later. Because the parse is never skipped, a listed call that *does* arrive framed still takes its real type (`DE K5ARH` spots as `De`, not `Unknown`). It does **not** bypass the operator blocklist/notch overrides (evaluated first) or dedupe: ARCHITECTURE §6's "Operator allowlist (Watch List)" paragraph.
- Populate the Watch List with the repeatable `--allowlist <CALL>` flag on `manta decode`/`listen`/`soak`/`doctor` — every subcommand that runs the decode pipeline takes it (`crates/manta-cli/src/main.rs`, all four `allowlist: Vec<String>` args forwarded through `build_pipeline_config`), and that flag is the only path wired up today. SPEC §9's `[spot] allowlist` TOML key is **spec-only**: the daemon config file loader deserializes just `[server]` and `[[rbn_uplink]]` (`crates/manta-server/src/config.rs`, `DaemonConfigFile`), so an `allowlist` under `[spot]` in a `--server-config` file is read by nothing and enables no exemption.
- Dedupe key = (callsign, freq bucket) with a re-spot suppression window unless SNR improves or type changes: §6.5.
- `Validator::tracks`/`RepetitionGate::seen` are per-track_id state that must be freed on `DecoderEvent::TrackClosed` — the normative teardown contract (a real, measured leak this bug produced) lives in `docs/DECISIONS/2026-09-02-man19-track-closed-teardown-invariant.md`, not here.
- **MAN-28's Watch List allowlist bypasses this gate deliberately**: an operator-specified callsign in `Validator::allowlist` skips `cty.is_allocated()`/the repetition gate entirely, so an allowlisted call can reach the emit path even when `cty.lookup` itself can't resolve its geography. The output-side consequence (what `SpotMessage::from_spot` emits for that case) is [[spot-output-contract]]'s concern, not this page's — see `docs/DECISIONS/2026-09-04-man45-unresolved-geography-sentinels.md`.
- The decoder-output prefilter (`manta_spot::grammar::is_plausible`) is scoped to THIS pipeline only — it is not, and must not be reused as, operator-identity validation (e.g. a configured station callsign). See `crates/manta-spot/src/grammar.rs`'s module doc and `manta_server::config::check_operator_callsign`.

## Why it is shaped this way

The asymmetry is deliberate: false spots (bogus callsigns) are the failure mode that discredits the whole network, so the repetition gate and cty.dat rejection are tuned to make bogus spots rare — a V8/V8w pass criterion is *0 bogus callsigns*. The two exemptions are kept narrow for the same reason: BEACON lifts only the repetition gate (cty.dat rejection still applies), and the allowlist is opt-in per callsign by the operator who owns the consequences. Validated spots flow to [[spot-output-contract]].

The repetition gate assumes a bogus decode is random noise that won't repeat identically. A deterministic front-end artifact breaks that assumption — it produces the *same* garbled decode at a fixed frequency every time, so it repeats and passes the gate. Field-confirmed against real hardware: [[live-hardware-field-testing]].

**The BEACON exemption (step 4) doesn't need that assumption to break at all — it needs zero repeats.** A candidate the CQ/DE context parse tags `BEACON` (ARCHITECTURE §6.1/MAN-37: any plausible 3-15 char word followed by a lone trailing `T`, the shape an NCDXF/IARU beacon's unmodulated power-step dashes decode to) skips the repetition gate entirely by design — real beacons only ID once per cycle. That same pattern also matches noise-decoded garble that happens to end in a solo "T" word, and once tagged BEACON, one glimpse of that garble is a "confirmed" public spot with no second decode ever required. A 2026-09-09 overnight 40m capture ([[live-hardware-field-testing]]) produced 29 confirmed spots that were entirely this: implausibly fast WPM (avg ~51.5, several pinned at the tracker's own 60 WPM ceiling) and malformed text (`4AEEEEE`, `ER1EEAE`, ...), all as one-shot BEACON-exempt spots — not evidence of a deterministic front-end artifact needing to repeat identically, as an earlier reading of that data assumed. (A fixed-bin channelizer artifact could still explain why the *same* garbled decode recurs at the *same* frequency across separate sessions — but each occurrence only needed to survive one BEACON classification, not the repetition gate, to become a spot.) PR #154 (`crates/manta-spot/src/validator.rs`, `grammar.rs` — open as of this writing, not yet merged) adds a WPM-implausibility check scoped specifically to `SpotType::Beacon` candidates to close this; a residual few (structurally plausible, not implausibly fast) aren't caught by it and remain a known gap.
