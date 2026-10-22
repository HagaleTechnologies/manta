# MAN-271: a short single-shot high-WPM transmission decodes to at least one character

Date: 2026-10-16
Ticket: MAN-271 (replaces MAN-3, whose PR #96 was closed by the
2026-10-05 audit triage)
Branch: `MAN-271`, from `main` @ `e89ae9e416114e391dca7cda3a2388baadd1f`
Plan: `thoughts/repos/manta/shared/plans/2026-10-16-MAN-271-the-decoder-should-not-silently-emit-zero-characters-for.md`

## Decision

Two fixes, one per root cause:

1. **Finish-time speed-tracker bootstrap** (`manta-decode`, SPEC §4.1
   **[DEVIATION]**). At a genuine end-of-signal `TrackDecoder::finish()`
   (stream EOF, HangExpired/Silent closure), a tracker that saw 2–4 marks
   initializes from them with the ordinary largest-ratio split and replays
   its buffered runs; the trailing truncated space is not learned (the
   MAN-315 `finish_runs` rule). `finish_speed_only()` is untouched.
2. **Short-transmission qualification D′** (`manta-engine::track`, SPEC
   §2.8 D′). At the first classifier evaluation after an ACTIVE → HANG
   drop, a promoted, unqualified track whose ACTIVE run was at most
   `MAX_MARK_HOPS` qualifies when it passes every spectral check D applies
   (no span, no co-modulation, `n ≥ SLOPE_MIN_SAMPLES`, non-flat fraction)
   with a span-scaled slope tolerance, **plus at least
   `SHORT_MIN_MARKS = 3` keyed marks** counted from its queued input with
   Demod-style rails (nearest-rank Q90/Q10, ≥ 2 keying ratio, midpoint
   threshold ± 15 % hysteresis, 4-hop debounce on both edges). `cycles`
   and `t_last` are not required. Qualification leases the decoder,
   replays the queue and emits the deferred `TrackPromoted` exactly as
   rule D does — no new close path, no promotion, candidate, merge,
   eviction or `soak_metrics.rs` change.

No new config key, metric, `CloseReason`, event or JSON field.

## Root causes (both required; either fix alone leaves failures)

Instrumented in the planning session (research + plan, from `main` @
`e89ae9e`):

1. **Decoder:** `SpeedTracker` needs 5 marks (`ClusterPair::observe`);
   promotion (`confirm_hops = 19`) consumes the leading element(s), so a
   2-character text at 34–39 WPM arrives as 2–4 marks — "DA" as 4
   (32.0/34.7/32.0/104.0 ms), "VE" as 3 (40.0/90.7/29.3 ms) — the tracker
   never becomes ready, `finish()` emits nothing. Reproduces with
   `non_cw_filter = false`.
2. **Detector (MAN-313 SPEC §2.8 D):** a single short burst never
   qualifies. At high SNR the gate's 40 ms EMA bridges every gap, so the
   whole message is one ACTIVE run and `cycles ≥ 1` (a HANG → ACTIVE
   re-rise) never happens (DA, Z5, D5); VE reached HANG once but its
   key-down span (110 hops) was short of `KEYDOWN_SPAN_MIN_HOPS` (150).
   The track then closes or ends unqualified with zero events —
   `decode_samples` returns `Err("no signal found (no keyed CW signal in
   the input)")`.

## Evidence (commands run on this branch unless noted)

Baseline numbers marked *(plan)* were measured in the planning session on
`main` @ `e89ae9e` and are cited from its record; everything else was run
on this branch.

- **Ticket tuples, single-shot (3.0 s, default config), on main *(plan)**:
  all four `Err(no signal found …)`; filter off: DA 0 chars, VE 0,
  Z5 `"D5"`, D5 `"I5"`. **On this branch**:
  `cargo test --release -p manta-engine --test regression_short_high_wpm`
  — `ticket_tuples_single_shot_decode_at_least_one_character` and
  `ticket_tuples_single_shot_decode_across_durations` (1.2/1.5/2.0/6.0/15 s)
  pass: ≥ 1 `CharDecoded` each, last character exact ("IA", "AE", "D5",
  "I5" — the first character is the known out-of-scope corruption).
- **Red confirmed on main** (this session, before the fix):
  `regression_short_high_wpm` — 4 of 6 tests failed; the 24-case domain
  test reported **15 of 24 zero-output**; the looped guard passed.
- **Domain sweep (200 cases, SplitMix64 seed `0x271_2026_1016`)**:
  `cargo test --release -p manta-engine --test regression_short_high_wpm
  two_char_domain_single_shot_sweep_200 -- --ignored --nocapture`:

  ```
  single-shot 200-case sweep: 0 zero-output, 200 not exact, 0 exact
  ```

  0 zero-output (main *(plan)*: 112 of 200; "not exact" = decoded but not
  containing the keyed text verbatim — every single-shot case keeps the
  first-character corruption, hence 0 exact). The 24-case always-run slice
  passes. Looped *(plan)*: 0/200 zero on main and after.
- **Ablation (plan, prototype instrumentation)**: decoder bootstrap alone
  leaves 102/200 at zero output; D′ alone leaves 23/200; both give 0/200.
- **Keyed-marks discriminator (plan, measured)**: CW short evaluations
  that pass the spectral checks read marks {2: 2, 3: 10, 4: 12, 5: 30,
  6: 40, 7: 64, 8: 17}; swept-carrier false positives read 1 mark (one
  track at 2, at 1000 Hz/s +60 dB). `SHORT_MIN_MARKS = 3` rejects every
  measured sweep track. Floor-referenced counting was measured and
  rejected (most CW bursts read 2 — the channelizer smear keeps 1-dit
  gaps above floor + 12 dB).
- **Slope tolerance scaling**: without `(150/t_last)^1.5`, 2 of 200 domain
  cases (LE @ 36.4 WPM, 83.7 Hz/s over 119 hops; AD @ 37.9 WPM, −75.1 Hz/s
  over 139 hops *(plan)*) stay at zero. Pinned by
  `short_rule_slope_tolerance_scales_with_span`.
- **MAN-313 gates unchanged**:
  `cargo test --release -p manta-engine --test non_cw` — 11 passed
  including the new `single_channel_sweep_bumps_do_not_qualify_as_short_transmissions`
  (1000 Hz/s, 5 ms ramp: 0 `TrackPromoted`; a naive D′ without the
  keyed-marks check promoted 13 there *(plan)*).
- **Heavy `non_cw` (this branch, `--release -- --ignored --nocapture`)**:
  `all_golden_vectors_decode_identically_with_the_filter` **pass**;
  `sweeps_across_rates_and_levels_start_no_cw_track` **pass**;
  `ssb_voice_at_14040_starts_no_cw_track` still fails as on main (the
  `#[ignore]`d MAN-372 residual), printing **8** in-band `TrackPromoted`
  at +30 dB where main printed 7 *(plan)* — a +1 residual, no SSB spot
  emitted (`ssb_voice_at_14040_starts_far_fewer_cw_tracks` passes).
- **Suites**: `manta-decode` 210 passed (204 on main + 6 new);
  `manta-engine --lib` 291 passed (282 + 9 new), 1 pre-existing ignored;
  every `manta-engine` integration suite green (chunking/channelizer
  determinism included); `manta-testkit` green; `manta-cli` green
  including the golden V-suites (560 lib tests + all integration targets).

## Rejected alternatives

- **Terminal qualification at close or end of input** — changes
  `closed.retain`/`close_all` sequencing and `has_emitted` semantics, the
  MAN-19/MAN-174 area where PR #96's review findings lived. D′ reuses the
  existing evaluation-time lease instead.
- **Floor-referenced keyed marks** (`P_dB − F_eff ≥ on_snr_db`) — measured
  not to separate CW from sweeps (most CW bursts read 2).
- **Loosening `cycles ≥ 1` for all tracks** — a naive short rule without
  the keyed-marks check broke `swept_carrier_7000_to_7040…` (2
  promotions), the sweeps matrix (5–17 per level at 1000 Hz/s) and
  `non_cw_only_input_reports_no_keyed_cw` *(plan)*.
- **Candidate-history replay / `confirm_hops` changes** — PR #96 finding
  #3 territory; `confirm_hops` is pinned by MAN-166's V10 cliff.
- **Dropping the slope check for D′** — rejected; the span-scaled slope
  keeps a second spectral guard at short spans.
- **PR #96 wholesale** — its target (the pre-MAN-115 warm-up floor) no
  longer exists; today's bug lives in code that did not exist when it was
  written.

PR #96's four still-open findings touch none of the code changed here: no
merge-work change, no candidate-history replay, no eviction/track-ID
change, no `soak_metrics.rs` change.

## Known limits

- **Clips shorter than ~1.1 s after promotion** still return `Err`: the
  track qualifies but the decoder has fewer than Demod's 375-hop init
  window (SPEC §3.2), so `demod_running = false` at finish (measured at
  1.0 s: qualifies, `hop_count = 317` *(plan)*). Pre-existing, general.
- **First-character corruption** ("IA" for "DA", "AE" for "VE", "D5" for
  "Z5", "I5" for "D5"): promotion consumes the leading element. Every
  track's first character, looped or not; does not violate MAN-271's
  Gherkin. This is why the 200-case sweep reads 0 exact.
- **Texts reaching the decoder with fewer than 2 marks** ("EE", "ET")
  still decode to nothing — outside the ticket's proptest domain
  (`total_marks ≥ 5`) and one mark carries no speed evidence.
- **`Hsmm` unaffected**: it never feeds `SpeedTracker`, so the bootstrap
  is a no-op there (and D′ leases whichever decoder the config selects).
- **+1 SSB in-band residual** at +30 dB with this voice model (8 vs main's
  7) — inside MAN-372's already-open residual, none spotted.
- **No Pi4 measurement** was possible in this environment (as MAN-313);
  the per-drop cost is one nearest-rank quantile selection over the
  bounded queue (≤ `MAX_UNQUALIFIED_BACKLOG` + 25 % entries) for
  unqualified tracks only.

## Follow-ups (for a maintainer to file)

- **First-character corruption** — candidate lead-in replay at
  qualification, coordinated with MAN-166's V10 `confirm_hops` cliff.
- **Sub-1.1 s clips** — Demod partial-window init plus end-of-input
  qualification; a general decode-timing change, explicitly out of this
  ticket's scope.
