---
id: detector-tracks
title: How does the detector and track manager decide what to decode?
kind: subsystem
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#2-noise-floor--signal-presence-detection
  - ARCHITECTURE.md
verified:
  commit: d286147
  date: 2026-10-13
links:
  - pfb-channelizer
  - decode-chain
---
The detector estimates a per-channel noise floor by order statistics (a quantile over a sliding window — a median-like estimator, so CW keying does not inflate its own floor), gates channels active when smoothed power exceeds the floor by a threshold with hysteresis, and promotes each active channel to a **track** that leases a decoder from a bounded pool. This is what turns a spectrum into a set of things worth decoding. Exact quantile, window, on/off thresholds, hang/gc timers, and track cap are normative config keys in SPEC §2 and §9 — cite, do not restate.

## How it works

- Floor estimator + neighborhood/effective floor + gate: SPEC §2.1–2.3 (`manta-dsp::floor`).
- Track lifecycle state machine (IDLE → CANDIDATE → ACTIVE → HANG → CLOSED): SPEC §2.4 (`manta-engine::track`, now implemented — see `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`).
- Adjacent-channel ownership so one signal yields exactly one track (no cross-channel ghost decodes): SPEC §2.5. This invariant is a V7/V8w pass criterion — see [[golden-vector-freeze]].
- Track cap with lowest-SNR eviction; merges (SPEC §2.5) and evictions (ARCHITECTURE §4) are counted in-process via `TrackManager::close_counts` (issue #26) — **no silent coverage loss**, though external exposition as Prometheus metrics is explicit M3 scope (ARCHITECTURE §8), not yet wired.
- Every close that ever emitted a real `DecoderEvent` also produces a `DecoderEvent::TrackClosed { track_id, closure }` (`closure` is `SignalEnded`, or `Bookkeeping` for a merge, eviction, silent-track GC, RTTY suppression or source discontinuity, none of which means the signal ended; see SPEC §5 and `ClosureKind` in `manta-decode::events`; `process_hops`/`finish()`, gated on `Track::has_emitted`) — the teardown signal [[spot-validation]]'s per-track_id state depends on. Normative contract: `docs/DECISIONS/2026-09-02-man19-track-closed-teardown-invariant.md`.

## Why it is shaped this way

Order statistics rather than a mean floor is the load-bearing choice: CW is bursty, and a mean would let a strong keyed signal raise the threshold above weaker neighbors. The hysteresis + hang timing exists to survive QSB and inter-word gaps without dropping a track mid-QSO. Active tracks feed the [[decode-chain]].

## Gotchas

- High-SNR PFB skirt leakage can cross the detector threshold outside an
  incumbent's owned channels. MAN-172 adds local-peak spawn eligibility
  and incumbent-wins merge ties; see the
  [decision record](../../docs/DECISIONS/2026-10-11-man172-high-snr-track-churn.md).
- A splatter rule must never close existing tracks: key-up gaps put real
  CW below any relative-level threshold. MAN-314's RTTY shadow therefore
  vetoes spawns only, and closes use the RTTY hard span alone; see the
  [decision record](../../docs/DECISIONS/2026-10-11-man314-rtty-recognition.md)
  and SPEC-decode-core.md §2.6. The spawns-only rule's second instance:
  MAN-196's trusted-band veto (decimated captures) blocks spawns on
  channels whose centre lies beyond the decimator's <= 1 dB boundary and
  touches nothing already running; see the
  [decision record](../../docs/DECISIONS/2026-10-14-man196-decimator-trusted-band.md).
- Start-up: a padded decode path's zero pre-roll hops must stay out of the
  floor (`TrackManager::skip_preroll_hops`); a young ring's lower quartile
  is its minimum, so they would pin the floor near -140 dB. The 25 ms
  default warm-up is safe only together with `FloorBank`'s 1 s
  block-median seed (SPEC §2.2 "Startup"). Never extend the seed on a
  ring's bottom-bin count: one later entry revives a stale seed for 10 s;
  leading silence is left out of the effective floor's quartile instead.
  V8w's 0-bogus and V1's 10 Hz
  frequency gates move with the promotion hop, so re-run them after any
  warm-up change; see the MAN-115
  [decision record](../../docs/DECISIONS/2026-10-11-man115-startup-latency.md).
- Static crashes (MAN-312, SPEC §2.7): judge any band-wide event over the
  source's passband, not every channel. A KiwiSDR's out-of-band channels
  never lift, so an all-channel lower quartile missed its crashes; wherever
  a live source exists, pass `rf_passband_hz()` to the `TrackManager`.
- The crash decoder hold uses the value from before the onset: a crash is
  flagged on its third lifted hop, so the entry just before the flag is
  already lifted. The flag is captured on each decoder input entry when it
  is observed, so it survives the MAN-194 refiner delay.
- Floor rings keep taking their pushes during a crash. Freezing them would
  leave a sustained band-wide rise (a gain step) unable to release.
- Synthetic dense scenes start synchronized (every signal keys its first
  element at t = 0), which a band-wide statistic reads as a crash. Render a
  lead-in and drop it, as `manta-testkit::crash::dense_scene` does, before
  testing one.
- VC3's bogus count depends on the crash draw (five train seeds gave 0, 1,
  4, 1 and 1 under `full`). After touching any crash constant, re-run the
  five-seed sweep, not only the pinned `0x3333`.
- Two worktrees that share one `CARGO_TARGET_DIR` can silently reuse each
  other's path-dependency rlibs; use separate target directories when
  comparing branches (for example `cpu_budget`). See the MAN-312
  [decision record](../../docs/DECISIONS/2026-10-13-man312-static-crash-filter.md).
- MAN-313 lazy lease: with `[detector] non_cw_filter` on, a promoted
  track has no decoder and emits no `TrackPromoted` until it qualifies as
  keyed CW; the event then carries the promotion `sample_ts`/`freq_hz`.
  Tests that expect a decoder or `TrackPromoted` at the promotion hop
  need the filter off. `drain_pool` must skip queued tracks that hold no
  decoder, or it panics on the unwrap.
- MAN-313 flatness: the per-track neighbourhood test must tolerate a
  louder neighbour. Requiring `PdB[k]` to beat its ±2/±3 channels kept
  weak CW 300 Hz from a loud RTTY mark from ever qualifying; only a
  neighbourhood within 6 dB counts as flat.
- MAN-313 co-modulation: lags start at 3 so a tone's own key clicks never
  count, and the span's strong test uses window peaks, not means, or a
  loud keyed tone's click skirt forms a span over the tone itself. See
  SPEC-decode-core.md §2.8 and the
  [decision record](../../docs/DECISIONS/2026-10-13-man313-non-cw-rejection.md).
