# M2 remaining sub-project 1 (V8/V8w pileup validation + CPU-budget bench) implementation pins

This is the M2 "V8/V8w pileup validation + CPU-budget bench" sub-project's
(`docs/superpowers/plans/2026-07-24-m2-pileup-cpu-budget.md`, design:
`docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md`)
implementation's pinned-decision record. Treat every numbered item below as
decided; SPEC and docs/ still win on anything not listed here.

## Deviations and pinned decisions

1. **Pileup fixture callsigns are synthetic/deterministic, not real operator
   calls.** `crates/manta-testkit/src/callsigns.rs`'s `pileup_calls()`
   generates 50 unique callsigns via ChaCha8-seeded prefix+suffix
   composition, with uniqueness enforced at generation time. Same
   determinism discipline as the rest of `manta-testkit` ("all randomness
   is ChaCha8 seeded per fixture") — none of these calls reference real
   operators.

2. **V8/V8w's "callsign validated"/"bogus callsign"/"cross-channel ghost
   decode" are test-local heuristics, not the real `manta-spot`
   validator** (which doesn't exist yet — M3 scope). Same convention V5/V6
   already established for "callsign validated" in
   `docs/DECISIONS/2026-07-17-m1-implementation-pins.md`, but upgraded here:
   each decoded track is matched to its originating signal via nearest
   `TrackMeta.freq_hz` (SPEC §5's live centroid report), not a bare
   substring search, giving precise per-signal CER instead of a loose
   presence/absence guess.

3. **V8 (pileup-50, AWGN) passes cleanly, no `#[ignore]`, no issue filed.**
   `crates/manta-cli/tests/golden_v8_v8w.rs`'s
   `v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls` measures
   49/50 callsigns validated (threshold ≥ 45/50, SPEC §7) with 0 bogus
   callsigns.

4. **V8w (pileup-50-fading, Watterson CCIR-poor) fails and is `#[ignore]`d,
   filed as issue #28.** Same file,
   `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts` measures
   only 1/34 (2.9 %) of the ≥ +6 dB-SNR signals decoding at CER < 10 %
   (threshold ≥ 90 %, i.e. ≥ 31/34); median CER ≈ 0.276 across the 34 strong
   signals (≈ 2.76x the 0.10 gate). Investigation ruled out a harness/
   matching artifact: the sibling AWGN-only V8 test passes 49/50 using the
   identical scene/matching code, isolating fading as the only changed
   variable, and within V8w itself 31/34 failures are full-length captures
   with scattered character-level corruption (not track fragmentation) —
   only 3/34 show fragmentation, a secondary QSB-driven symptom (same
   family as issue #26). Confirmed as the same classical-decoder
   fading-robustness gap already tracked for V5/V6
   (`docs/DECISIONS/2026-07-17-m1-implementation-pins.md`, issue #25), now
   demonstrated at scale (34 independent fading realizations in one scene).
   Filed as <https://github.com/HagaleTechnologies/manta/issues/28>;
   revisit alongside V5/V6 once `manta-decode` gains real fading
   resilience (M4).

5. **CPU-budget Mac measurement: PASSES the < 0.5x budget.**
   `crates/manta-engine/benches/cpu_budget.rs` (criterion, 192 kS/s, 300
   simultaneous tones, 15 s synthetic scene) measures mean 5.5455 s
   wall-clock per iteration, 95 % CI [5.5341 s, 5.5578 s] (≈ 0.37x
   realtime). `crates/manta-engine/tests/cpu_budget.rs`'s `#[ignore]`d
   `cpu_budget_mac_under_half_core` — the actual accept-criterion gate, same
   scene — confirms it under `cargo test --release`:
   ```
   cpu_budget: 5.39s wall / 15.00s audio = 0.360x realtime (Mac budget: < 0.5x)
   ```
   0.360x clears the < 0.5x (< 50 % of one Mac core) budget comfortably,
   consistent with the criterion bench's 0.37x. **Build-profile pitfall:**
   plain dev-profile `cargo test` (without `--release`) measures a
   misleading ~0.54x, *over* budget — this workspace's root `Cargo.toml`
   sets `opt-level = 1` for first-party crates in the dev profile
   (`opt-level = 2` only for dependencies), so dev-profile runs are ~1.45x
   slower than release and not representative. This is a build-profile
   artifact, not a real regression; the test's own doc comment records it.

6. **Raspberry Pi 4 leg (< 1 core / < 1.0x realtime) is explicitly
   outstanding, not run in this branch.** Deferred pending Tony running
   `cargo test --release -p manta-engine --test cpu_budget -- --ignored
   --nocapture` on real Pi4 hardware — same pattern as M1's still-
   outstanding W1AW live-copy run (see CLAUDE.md Status).

7. **CPU-budget test measures wall-clock realtime ratio, not CPU-time; the
   two are currently equivalent but not guaranteed.** The test
   `cpu_budget_mac_under_half_core` measures wall-clock realtime ratio
   (elapsed / audio_duration), not CPU-time as stated in ROADMAP's < 50% of
   one core criterion. These are currently equivalent: independent
   measurement found whole-process CPU/wall ratio ≈ 1.13 and decode section
   ≈ 0.35 core-seconds/audio-second, both consistent with this pipeline
   being essentially single-core-bound at current track counts (300), so the
   wall-clock PASS (0.360x realtime) comfortably satisfies the CPU-time
   budget too. This equivalence is NOT a guarantee: if the decoder pool's
   parallelism scales up (more simultaneous tracks, heavier per-track
   computation, or different core counts on other platforms), the wall-clock
   gate could report a false PASS while the real per-core CPU time exceeds
   the budget. Worth revisiting with genuine CPU-time measurement (e.g.
   `getrusage`) if/when the pipeline's parallelism profile changes
   substantially.
