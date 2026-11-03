# M2 sub-project 2 — Detector, Track Manager, Decoder Pool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `manta-engine::detect::calibrate_channel` (sub-project 1's one-shot single-channel-argmax placeholder) with SPEC-decode-core.md §2's real order-statistic noise-floor detector and track-lifecycle state machine, and fold in the decoder-pool mechanism from ARCHITECTURE.md §10 (rayon-style, `Send` per-track decoders) — both `decode_samples`/`decode_wav` and `listen()` gain real multi-track detection.

**Architecture:** New `manta-dsp::floor` module computes per-channel order-statistic noise floor + EMA-smoothed power + rise/drop hysteresis booleans (SPEC §2.1–2.3), pure and stateful-per-channel. New `manta-engine::track` module drives the SPEC §2.4 lifecycle state machine and §2.5 adjacent-channel ownership across all channels sequentially, hop by hop; at the end of each hop-batch it dispatches queued per-track samples to `rayon` across all ACTIVE tracks' own `TrackDecoder`s (unmodified from sub-project 1), then resequences the merged event stream by `(sample_ts, track_id)` per SPEC §6 rule 6.

**Tech Stack:** Rust (edition 2021, rust-version 1.85.0), adds `rayon` as a new workspace dependency; reuses `manta-dsp::channelizer` and `manta-decode::decoder::TrackDecoder` unchanged.

**Design doc:** `docs/superpowers/specs/2026-07-19-m2-detector-track-pool-design.md` — read it first; this plan implements it section by section.

## Global Constraints

- **Determinism (SPEC §6):** no RNG, no wall clock anywhere in `manta-dsp`/`manta-engine`. All lifecycle timers are hop counters, never `ms`/wall-clock, at runtime (config still stores `_ms` fields for documentation parity with SPEC §9's table; they are converted to hop counts once, at `DetectorConfig` construction). Tracks live in a `BTreeMap<u32, Track>` (never `HashMap`) keyed by monotonic `track_id`, ascending birth order. `rayon` parallelizes only the per-track *decode* step (§6 rule 6 explicitly permits decoder workers to run in any order); the per-hop state-machine bookkeeping in `TrackManager` stays single-threaded and hop-ordered. Emitted events are resequenced by `(sample_ts, track_id)` before returning to the caller.
- **SPEC §2.1 floor estimator:** 250-entry ring (10 s at 25 Hz decimation — push every 15th hop), 280-bin `u8`-count histogram (0.5 dB bins, −140..0 dBFS), floor = 25th percentile, O(1) amortized per update via cumulative histogram scan, no sorting.
- **SPEC §2.2 neighborhood floor:** 32-channel blocks, `F_blk[b]` = median of the block's `F_ch` values, `F[k] = min(F_ch[k], F_blk[⌊k/32⌋] + 3 dB)`.
- **SPEC §2.3 gate:** EMA-smoothed power, τ = 40 ms → `α = 1 − e^{−2.667/40} ≈ 0.0645` at the channelizer's fixed 375 Hz hop rate (`manta_decode::FO_HZ`). Rise: `S ≥ F + 6 dB` (`on_snr_db`). Drop: `S < F + 3 dB` (`off_snr_db`).
- **SPEC §2.4 lifecycle hop counts** (derived once from SPEC §9's ms defaults at the fixed 375 Hz hop rate, then used as integer hop counts everywhere — never re-derived per-hop): confirm = 19 hops (≈50 ms), hang = 1875 hops (5000 ms), gc = 11250 hops (30000 ms), warmup = 750 hops (2000 ms).
- **SPEC §2.5 ownership:** a track owns `{round(c)−1, round(c), round(c)+1}`; `c` is initialized to the birth channel and is the fractional channel center. CANDIDATE-in-owned-channel is absorbed (no new track). Same-hop simultaneous rise on two unowned channels: one CANDIDATE at the higher-power channel. Two tracks converging within 1.0 channel merge; the lower-current-SNR one is CLOSED with reason `merged`.
- **Track cap:** ARCHITECTURE §4 default 500 (not in SPEC §9's literal `[detector]` table — `DetectorConfig.track_cap` is a documented, deliberate addition beyond the SPEC table; note this as a candidate pin at close-out, same as sub-project 1's pins 4 and 6).
- **No TOML config loader exists anywhere in this repo.** `DetectorConfig` is a plain struct + `Default` impl (same pattern as `manta_decode::decoder::DecodeConfig`) — do not add file loading.
- **CI:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` (full workspace, not per-crate — sub-project 1 hit this mistake twice), `cargo test --workspace` all clean.
- **Multi-agent hygiene (CLAUDE.md):** branch `feat/m2-pfb-channelizer` (already reset onto current `origin/main`), draft PR #20 already open as the claim, `--force-with-lease` only, main moves only by PR merge.
- Rustdoc comments on every public item cite the SPEC/ARCHITECTURE section they implement.

---

### Task 1: Add `rayon` dependency

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/manta-engine/Cargo.toml`

**Interfaces:**
- Produces: `rayon` available to `manta-engine` (and transitively anything it re-exports from) for Task 6's parallel decode dispatch.

- [ ] **Step 1: Add to workspace dependencies**

In `Cargo.toml`, in the `[workspace.dependencies]` block, add alongside the other version-pinned deps (after `tempfile = "3"`):

```toml
rayon = "1"
```

- [ ] **Step 2: Add to manta-engine's dependencies**

In `crates/manta-engine/Cargo.toml`, in `[dependencies]`, add:

```toml
rayon = { workspace = true }
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build --workspace`
Expected: succeeds, `Cargo.lock` gains `rayon` and its transitive deps.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock crates/manta-engine/Cargo.toml
git commit -m "build: add rayon for the decoder-pool parallel dispatch"
```

---

### Task 2: `manta-dsp::floor` — order-statistic floor estimator

**Files:**
- Create: `crates/manta-dsp/src/floor.rs`
- Modify: `crates/manta-dsp/src/lib.rs`

**Interfaces:**
- Consumes: nothing new (plain per-channel dB power pushes).
- Produces: `pub struct FloorBank`, `impl FloorBank { pub fn new(n_channels: usize) -> Self; pub fn update(&mut self, power_db: &[f64]); pub fn effective_floor_db(&self, k: usize) -> f64; pub fn n_channels(&self) -> usize }`. Consumed by Task 3 (gate) and Task 5 (`TrackManager`).

- [ ] **Step 1: Write the failing tests**

Create `crates/manta-dsp/src/floor.rs`:

```rust
//! Per-channel order-statistic noise floor + neighborhood floor. SPEC
//! §2.1-2.2. Pure, stateful-per-channel — no track lifecycle here (that's
//! `manta-engine::track`).

/// SPEC §2.1: 250-entry ring = 10 s at 25 Hz decimation (push every 15th hop
/// at the channelizer's fixed 375 Hz hop rate).
const RING_LEN: usize = 250;
const DECIMATION_HOPS: u64 = 15;
/// SPEC §2.1: 280 bins, 0.5 dB wide, spanning -140..0 dBFS.
const HIST_BINS: usize = 280;
const BIN_WIDTH_DB: f64 = 0.5;
const HIST_MIN_DB: f64 = -140.0;
/// SPEC §2.1: floor = 25th percentile (median would be inflated by CW
/// key-down duty cycle up to 50-60% on a busy channel).
const FLOOR_QUANTILE: f64 = 0.25;
/// SPEC §2.2.
const BLOCK_CHANNELS: usize = 32;
const BLOCK_ALLOWANCE_DB: f64 = 3.0;

fn bin_index(power_db: f64) -> usize {
    (((power_db - HIST_MIN_DB) / BIN_WIDTH_DB).floor() as isize).clamp(0, HIST_BINS as isize - 1)
        as usize
}

/// One channel's order-statistic floor estimator: a 250-entry ring backed
/// by a 280-bin histogram for O(1)-amortized quantile lookup (no sorting).
/// SPEC §2.1.
struct ChannelFloor {
    ring: [u16; RING_LEN],
    hist: [u8; HIST_BINS],
    ring_pos: usize,
    filled: usize,
}

impl ChannelFloor {
    fn new() -> Self {
        ChannelFloor {
            ring: [0; RING_LEN],
            hist: [0; HIST_BINS],
            ring_pos: 0,
            filled: 0,
        }
    }

    fn push(&mut self, power_db: f64) {
        let bin = bin_index(power_db);
        if self.filled == RING_LEN {
            let evict_bin = self.ring[self.ring_pos] as usize;
            self.hist[evict_bin] = self.hist[evict_bin].saturating_sub(1);
        } else {
            self.filled += 1;
        }
        self.ring[self.ring_pos] = bin as u16;
        self.hist[bin] = self.hist[bin].saturating_add(1);
        self.ring_pos = (self.ring_pos + 1) % RING_LEN;
    }

    /// SPEC §2.1: 25th percentile via cumulative histogram scan. Startup
    /// (ring partially filled): quantile is taken over whatever's present.
    fn quantile_db(&self, q: f64) -> f64 {
        if self.filled == 0 {
            return HIST_MIN_DB;
        }
        let target = ((q * self.filled as f64).ceil() as usize).max(1);
        let mut cum = 0usize;
        for (b, &c) in self.hist.iter().enumerate() {
            cum += c as usize;
            if cum >= target {
                return HIST_MIN_DB + (b as f64 + 0.5) * BIN_WIDTH_DB;
            }
        }
        HIST_MIN_DB + (HIST_BINS as f64 - 0.5) * BIN_WIDTH_DB
    }

    fn floor_db(&self) -> f64 {
        self.quantile_db(FLOOR_QUANTILE)
    }
}

fn median(vals: &[f64]) -> f64 {
    let mut v = vals.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n == 0 {
        return HIST_MIN_DB;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Per-channel noise floor across the whole channelizer output. SPEC
/// §2.1-2.2. Call `update` once per hop with that hop's per-channel dB
/// power (`manta_dsp::channelizer::power_db` applied to each
/// `HopOutput.power[k]`); query `effective_floor_db` any time.
pub struct FloorBank {
    channels: Vec<ChannelFloor>,
    floor_db: Vec<f64>,
    block_floor_db: Vec<f64>,
    hop_counter: u64,
}

impl FloorBank {
    pub fn new(n_channels: usize) -> Self {
        let n_blocks = n_channels.div_ceil(BLOCK_CHANNELS);
        FloorBank {
            channels: (0..n_channels).map(|_| ChannelFloor::new()).collect(),
            floor_db: vec![HIST_MIN_DB; n_channels],
            block_floor_db: vec![HIST_MIN_DB; n_blocks],
            hop_counter: 0,
        }
    }

    pub fn n_channels(&self) -> usize {
        self.channels.len()
    }

    /// One hop's per-channel dB power. Internally decimates to 25 Hz (every
    /// 15th hop) per SPEC §2.1; cheap to call every hop.
    pub fn update(&mut self, power_db: &[f64]) {
        debug_assert_eq!(power_db.len(), self.channels.len());
        if self.hop_counter % DECIMATION_HOPS == 0 {
            for (ch, &p) in self.channels.iter_mut().zip(power_db) {
                ch.push(p);
            }
            for (k, ch) in self.channels.iter().enumerate() {
                self.floor_db[k] = ch.floor_db();
            }
            for (b, block_floor) in self.block_floor_db.iter_mut().enumerate() {
                let start = b * BLOCK_CHANNELS;
                let end = (start + BLOCK_CHANNELS).min(self.channels.len());
                *block_floor = median(&self.floor_db[start..end]);
            }
        }
        self.hop_counter += 1;
    }

    /// Effective floor `F[k] = min(F_ch[k], F_blk[k/32] + 3dB)`. SPEC §2.2.
    pub fn effective_floor_db(&self, k: usize) -> f64 {
        let block = k / BLOCK_CHANNELS;
        self.floor_db[k].min(self.block_floor_db[block] + BLOCK_ALLOWANCE_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_of_uniform_noise_lands_near_25th_percentile() {
        // 250 values 0..249 (dB, offset from HIST_MIN_DB); 25th percentile
        // of a uniform 0..249 population is index ceil(0.25*250)-1 = 62,
        // i.e. value 62 dB above HIST_MIN_DB, landing in the 62.0-62.5 bin.
        let mut ch = ChannelFloor::new();
        for i in 0..250 {
            ch.push(HIST_MIN_DB + i as f64);
        }
        let f = ch.floor_db();
        assert!(
            (f - (HIST_MIN_DB + 62.25)).abs() < 0.5,
            "floor {f}, expected near {}",
            HIST_MIN_DB + 62.25
        );
    }

    #[test]
    fn duty_cycle_does_not_inflate_25th_percentile_floor() {
        // 55% "key down" at -20 dB, 45% "key up" (noise) at -100 dB -- a
        // median estimator would land near -20 dB (inflated by keying); the
        // 25th percentile must stay on the noise rail.
        let mut ch = ChannelFloor::new();
        for i in 0..250 {
            ch.push(if i % 20 < 11 { -20.0 } else { -100.0 });
        }
        assert!(
            ch.floor_db() < -50.0,
            "25th percentile floor {} should stay near the noise rail, not the keyed level",
            ch.floor_db()
        );
    }

    #[test]
    fn ring_evicts_oldest_after_full() {
        let mut ch = ChannelFloor::new();
        for _ in 0..250 {
            ch.push(-50.0);
        }
        assert_eq!(ch.floor_db(), -49.75); // all mass in the -50.0 bin
        for _ in 0..250 {
            ch.push(-10.0);
        }
        // fully evicted the -50 dB population; floor should now reflect only -10 dB
        assert_eq!(ch.floor_db(), -9.75);
    }

    #[test]
    fn startup_partial_window_uses_whatever_is_present() {
        let mut ch = ChannelFloor::new();
        ch.push(-30.0);
        ch.push(-30.0);
        ch.push(-30.0);
        ch.push(-30.0);
        // 4 entries, all -30 dB: 25th percentile of 4 identical values is -30 dB.
        assert_eq!(ch.floor_db(), -29.75);
    }

    #[test]
    fn neighborhood_floor_clamps_a_parked_carrier() {
        // Channel 5 (in block 0) has been key-down long enough that its own
        // 25th percentile inflates to -10 dB; its 31 neighbors in the same
        // block sit at a true -90 dB noise floor. Effective floor must not
        // exceed neighborhood + 3 dB.
        let mut bank = FloorBank::new(64); // 2 blocks of 32
        let hops_needed = RING_LEN as u64 * DECIMATION_HOPS;
        for _ in 0..hops_needed {
            let mut power_db = vec![-90.0; 64];
            power_db[5] = -10.0;
            bank.update(&power_db);
        }
        let f5 = bank.effective_floor_db(5);
        assert!(
            f5 <= -90.0 + BLOCK_ALLOWANCE_DB + 0.5, // +0.5 for bin quantization
            "effective floor for the parked carrier channel is {f5}, expected clamped near -87 dB"
        );
    }

    #[test]
    fn effective_floor_uses_own_quantile_when_below_neighborhood_cap() {
        let mut bank = FloorBank::new(64);
        let hops_needed = RING_LEN as u64 * DECIMATION_HOPS;
        for _ in 0..hops_needed {
            bank.update(&vec![-90.0; 64]); // uniform quiet band
        }
        let f = bank.effective_floor_db(10);
        assert!(
            (f - (-90.0)).abs() < 1.0,
            "uniform-noise floor should read near -90 dB, got {f}"
        );
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-dsp/src/lib.rs`, add alongside the existing `pub mod` declarations:

```rust
pub mod floor;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p manta-dsp floor:: -- --nocapture`
Expected: all 6 tests pass.

- [ ] **Step 4: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-dsp/src/floor.rs crates/manta-dsp/src/lib.rs
git commit -m "feat(dsp): order-statistic floor estimator + neighborhood floor (SPEC §2.1-2.2)"
```

---

### Task 3: `manta-dsp::floor` — EMA gate (rise/drop booleans)

**Files:**
- Modify: `crates/manta-dsp/src/floor.rs`

**Interfaces:**
- Consumes: `FloorBank::effective_floor_db` (Task 2).
- Produces: `pub struct Gate`, `impl Gate { pub fn new(n_channels: usize, on_snr_db: f32, off_snr_db: f32) -> Self; pub fn update(&mut self, power_db: &[f64], floor: &FloorBank) -> (Vec<bool>, Vec<bool>); pub fn smoothed_db(&self, k: usize) -> f64 }`. Consumed by Task 5 (`TrackManager`).

- [ ] **Step 1: Write the failing tests**

Append to `crates/manta-dsp/src/floor.rs` (before the existing `#[cfg(test)] mod tests` block's closing brace, as new items in the same test module, plus the new pub struct above the test module):

Add above `#[cfg(test)] mod tests {`:

```rust
/// SPEC §2.3: tau=40ms at the channelizer's fixed 375 Hz hop rate ->
/// alpha = 1 - e^(-2.667/40).
const GATE_EMA_ALPHA: f64 = 0.0645;

/// Per-channel EMA-smoothed power + rise/drop hysteresis booleans. SPEC
/// §2.3. Carries **no** persistence/timing state (confirm-hop-counting,
/// hang-ms-counting) -- that's the track lifecycle's job
/// (`manta-engine::track`). This struct is a pure function of its own
/// per-channel EMA state.
pub struct Gate {
    smoothed_db: Vec<f64>,
    initialized: Vec<bool>,
    on_snr_db: f32,
    off_snr_db: f32,
}

impl Gate {
    pub fn new(n_channels: usize, on_snr_db: f32, off_snr_db: f32) -> Self {
        Gate {
            smoothed_db: vec![0.0; n_channels],
            initialized: vec![false; n_channels],
            on_snr_db,
            off_snr_db,
        }
    }

    /// One hop's rise/drop booleans per channel, against `floor`'s current
    /// effective floor. SPEC §2.3: rise = S >= F + on_snr_db; drop = S < F
    /// + off_snr_db.
    pub fn update(&mut self, power_db: &[f64], floor: &FloorBank) -> (Vec<bool>, Vec<bool>) {
        debug_assert_eq!(power_db.len(), self.smoothed_db.len());
        let mut rise = vec![false; power_db.len()];
        let mut drop = vec![false; power_db.len()];
        for k in 0..power_db.len() {
            self.smoothed_db[k] = if self.initialized[k] {
                self.smoothed_db[k] + GATE_EMA_ALPHA * (power_db[k] - self.smoothed_db[k])
            } else {
                self.initialized[k] = true;
                power_db[k]
            };
            let f = floor.effective_floor_db(k);
            rise[k] = self.smoothed_db[k] >= f + self.on_snr_db as f64;
            drop[k] = self.smoothed_db[k] < f + self.off_snr_db as f64;
        }
        (rise, drop)
    }

    pub fn smoothed_db(&self, k: usize) -> f64 {
        self.smoothed_db[k]
    }
}
```

Add inside `mod tests`:

```rust
    fn warmed_up_bank(n: usize, floor_db: f64) -> FloorBank {
        let mut bank = FloorBank::new(n);
        let hops_needed = RING_LEN as u64 * DECIMATION_HOPS;
        for _ in 0..hops_needed {
            bank.update(&vec![floor_db; n]);
        }
        bank
    }

    #[test]
    fn rise_met_once_ema_settles_above_threshold() {
        let bank = warmed_up_bank(4, -90.0);
        let mut gate = Gate::new(4, 6.0, 3.0);
        let mut rise = vec![false; 4];
        // Feed a steady +10 dB-above-floor signal on channel 0 until the EMA settles.
        for _ in 0..200 {
            let (r, _) = gate.update(&[-80.0, -90.0, -90.0, -90.0], &bank);
            rise = r;
        }
        assert!(rise[0], "channel 0 should meet rise (10 dB above -90 floor, on_snr=6)");
        assert!(!rise[1], "channel 1 stays at the floor, should not rise");
    }

    #[test]
    fn drop_met_when_below_off_threshold() {
        let bank = warmed_up_bank(1, -90.0);
        let mut gate = Gate::new(1, 6.0, 3.0);
        let mut drop = vec![false];
        for _ in 0..200 {
            let (_, d) = gate.update(&[-89.0], &bank); // 1 dB above floor, below off_snr=3
            drop = d;
        }
        assert!(drop[0], "1 dB above floor is below the 3 dB off threshold");
    }

    #[test]
    fn hysteresis_gap_between_on_and_off() {
        // At 4.5 dB above floor (between off=3 and on=6): neither rise nor
        // drop should be true once settled -- this is the hysteresis dead
        // band SPEC §2.3 depends on for QSB survival.
        let bank = warmed_up_bank(1, -90.0);
        let mut gate = Gate::new(1, 6.0, 3.0);
        let mut last = (false, false);
        for _ in 0..200 {
            let (r, d) = gate.update(&[-85.5], &bank);
            last = (r[0], d[0]);
        }
        assert_eq!(last, (false, false), "4.5 dB above floor sits in the hysteresis dead band");
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p manta-dsp floor:: -- --nocapture`
Expected: 9 tests pass (6 from Task 2 + 3 new).

- [ ] **Step 3: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/manta-dsp/src/floor.rs
git commit -m "feat(dsp): EMA-smoothed power + rise/drop hysteresis gate (SPEC §2.3)"
```

---

### Task 4: `manta-engine::track` — single-channel lifecycle state machine

**Files:**
- Create: `crates/manta-engine/src/track.rs`
- Modify: `crates/manta-engine/src/lib.rs`

**Interfaces:**
- Consumes: nothing from `floor` yet — this task tests the FSM in isolation against a synthetic `(rise, drop)` boolean sequence, deferring real channel wiring to Task 5.
- Produces: `pub struct DetectorConfig` (with `Default`), `pub(crate) enum LifecycleState { Candidate, Active, Hang }`, `pub(crate) struct Lifecycle`, `impl Lifecycle { fn new(cfg: &DetectorConfig) -> Self; fn on_hop(&mut self, rise: bool, drop: bool, silent: bool) -> LifecycleEvent }` where `LifecycleEvent` signals `None | Promoted | Closed(CloseReason)`. Consumed by Task 5 (`TrackManager`/`Track`).

- [ ] **Step 1: Write the failing tests**

Create `crates/manta-engine/src/track.rs`:

```rust
//! Track lifecycle state machine (SPEC §2.4) and adjacent-channel ownership
//! (SPEC §2.5), driving the decoder pool. This task covers only the
//! single-channel FSM in isolation; `TrackManager` (Task 5) wires it to
//! real per-channel floor/gate state and multi-channel ownership.

/// SPEC §9 `[detector]` table, plus ARCHITECTURE §4's track cap (not in the
/// literal SPEC table -- see the plan's Global Constraints).
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    pub on_snr_db: f32,
    pub off_snr_db: f32,
    /// SPEC §2.3/§2.4: rise sustained this many hops (~50ms) before CANDIDATE -> ACTIVE.
    pub confirm_hops: u64,
    /// SPEC §2.3/§2.4: drop sustained this many hops (5000ms) before ACTIVE/HANG -> CLOSED.
    pub hang_hops: u64,
    /// SPEC §2.4: no character emitted for this many hops (30000ms) -> CLOSED (garbage collect).
    pub gc_hops: u64,
    /// SPEC §2.1: track creation inhibited for this many hops (2000ms) after start.
    pub warmup_hops: u64,
    /// ARCHITECTURE §4 (not SPEC §9): max concurrent ACTIVE tracks.
    pub track_cap: usize,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            on_snr_db: 6.0,
            off_snr_db: 3.0,
            confirm_hops: 19,
            hang_hops: 1875,
            gc_hops: 11250,
            warmup_hops: 750,
            track_cap: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    Candidate,
    Active,
    Hang,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// Rise never confirmed within `confirm_hops` (SPEC §2.4: CANDIDATE -> IDLE).
    Unconfirmed,
    /// Hang timer expired (SPEC §2.4: HANG -> CLOSED).
    HangExpired,
    /// No character emitted for `gc_hops` (SPEC §2.4: garbage collect).
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    None,
    /// CANDIDATE -> ACTIVE: lease a decoder from the pool.
    Promoted,
    Closed(CloseReason),
}

/// One channel-slot's SPEC §2.4 state machine, driven one hop at a time.
/// `Lifecycle` itself has no notion of "channel" or "ownership" -- the
/// caller (`TrackManager`, Task 5) decides which channel feeds `on_hop`
/// each cycle.
pub(crate) struct Lifecycle {
    state: LifecycleState,
    confirm_count: u64,
    hang_count: u64,
    silent_count: u64,
    confirm_hops: u64,
    hang_hops: u64,
    gc_hops: u64,
}

impl Lifecycle {
    /// A brand-new CANDIDATE, just born on a rise hop.
    pub(crate) fn new(cfg: &DetectorConfig) -> Self {
        Lifecycle {
            state: LifecycleState::Candidate,
            confirm_count: 1, // this hop's rise already counts as the first
            hang_count: 0,
            silent_count: 0,
            confirm_hops: cfg.confirm_hops,
            hang_hops: cfg.hang_hops,
            gc_hops: cfg.gc_hops,
        }
    }

    pub(crate) fn state(&self) -> LifecycleState {
        self.state
    }

    /// Advance one hop. `rise`/`drop` are this hop's gate booleans for the
    /// channel currently feeding this track (its owned max-power channel,
    /// per SPEC §2.5). `char_emitted` marks whether a character was decoded
    /// this hop (drives the §2.4 GC timer; always pass `false` while
    /// CANDIDATE, since nothing is being decoded yet).
    pub(crate) fn on_hop(&mut self, rise: bool, drop: bool, char_emitted: bool) -> LifecycleEvent {
        match self.state {
            LifecycleState::Candidate => {
                if rise {
                    self.confirm_count += 1;
                    if self.confirm_count >= self.confirm_hops {
                        self.state = LifecycleState::Active;
                        return LifecycleEvent::Promoted;
                    }
                } else {
                    return LifecycleEvent::Closed(CloseReason::Unconfirmed);
                }
                LifecycleEvent::None
            }
            LifecycleState::Active => {
                if char_emitted {
                    self.silent_count = 0;
                } else {
                    self.silent_count += 1;
                    if self.silent_count >= self.gc_hops {
                        return LifecycleEvent::Closed(CloseReason::Silent);
                    }
                }
                if drop {
                    self.state = LifecycleState::Hang;
                    self.hang_count = 1;
                }
                LifecycleEvent::None
            }
            LifecycleState::Hang => {
                if char_emitted {
                    self.silent_count = 0;
                } else {
                    self.silent_count += 1;
                    if self.silent_count >= self.gc_hops {
                        return LifecycleEvent::Closed(CloseReason::Silent);
                    }
                }
                if rise {
                    self.state = LifecycleState::Active;
                    self.hang_count = 0;
                } else {
                    self.hang_count += 1;
                    if self.hang_count >= self.hang_hops {
                        return LifecycleEvent::Closed(CloseReason::HangExpired);
                    }
                }
                LifecycleEvent::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DetectorConfig {
        DetectorConfig {
            confirm_hops: 5,
            hang_hops: 10,
            gc_hops: 20,
            ..DetectorConfig::default()
        }
    }

    #[test]
    fn promotes_after_confirm_hops_of_sustained_rise() {
        let mut lc = Lifecycle::new(&cfg()); // hop 1 (birth) already counted
        for _ in 0..3 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::Promoted); // hop 5
        assert_eq!(lc.state(), LifecycleState::Active);
    }

    #[test]
    fn candidate_closes_unconfirmed_on_any_non_rise_hop() {
        let mut lc = Lifecycle::new(&cfg());
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        assert_eq!(
            lc.on_hop(false, true, false),
            LifecycleEvent::Closed(CloseReason::Unconfirmed)
        );
    }

    #[test]
    fn active_to_hang_to_active_resets_hang_timer() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..4 {
            lc.on_hop(true, false, false);
        }
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::Promoted);
        assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // -> HANG
        assert_eq!(lc.state(), LifecycleState::Hang);
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // still within hang_hops=10
        }
        assert_eq!(lc.on_hop(true, false, true), LifecycleEvent::None); // recovers -> ACTIVE
        assert_eq!(lc.state(), LifecycleState::Active);
    }

    #[test]
    fn hang_expires_after_hang_hops() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..4 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted
        lc.on_hop(false, true, true); // -> HANG, hang_count=1
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // hang_count 2..9
        }
        assert_eq!(
            lc.on_hop(false, true, true),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        ); // hang_count=10 >= hang_hops=10
    }

    #[test]
    fn garbage_collected_after_gc_hops_silent() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..4 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted, ACTIVE
        for _ in 0..19 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None); // no char emitted, silent_count 1..19
        }
        assert_eq!(
            lc.on_hop(true, false, false),
            LifecycleEvent::Closed(CloseReason::Silent)
        ); // silent_count=20 >= gc_hops=20
    }

    #[test]
    fn char_emission_resets_silent_counter() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..4 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted
        for _ in 0..19 {
            lc.on_hop(true, false, false);
        }
        assert_eq!(lc.on_hop(true, false, true), LifecycleEvent::None); // char emitted at what would be the GC boundary -- resets
        for _ in 0..19 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/manta-engine/src/lib.rs`, add alongside the existing `mod detect;` declaration:

```rust
mod track;
pub use track::DetectorConfig;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p manta-engine track:: -- --nocapture`
Expected: all 6 tests pass.

- [ ] **Step 4: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-engine/src/track.rs crates/manta-engine/src/lib.rs
git commit -m "feat(engine): track lifecycle state machine, single-channel (SPEC §2.4)"
```

---

### Task 5: `manta-engine::track` — `TrackManager`: multi-channel orchestration + ownership

**Files:**
- Modify: `crates/manta-engine/src/track.rs`

**Interfaces:**
- Consumes: `manta_dsp::floor::{FloorBank, Gate}` (Tasks 2-3), `manta_dsp::channelizer::{HopOutput, power_db, interpolate_offset}`, `Lifecycle`/`DetectorConfig` (Task 4).
- Produces: `pub(crate) struct Track { pub(crate) id: u32, center: f64, pub(crate) current_snr_db: f32, ... }`, `pub struct TrackManager`, `impl TrackManager { pub fn new(n_channels: usize, cfg: DetectorConfig) -> Self; fn step_hop(&mut self, hop: &HopOutput) -> Vec<(u32, usize, f32)> }` (returns `(track_id, selected_channel, magnitude)` per currently-ACTIVE track) — this task stops there (no `TrackDecoder`/pool dispatch yet; Task 6 changes `new`'s signature again to add `fs`/`center_freq_hz`/`decode_cfg` and adds the real public `process_hops` entry point).

- [ ] **Step 1: Write the failing tests**

Insert into `crates/manta-engine/src/track.rs`, after the `Lifecycle` impl block and before `#[cfg(test)] mod tests`:

```rust
use manta_dsp::channelizer::{interpolate_offset, power_db, HopOutput};
use manta_dsp::floor::{FloorBank, Gate};
use std::collections::BTreeMap;

/// One tracked signal. Owns channels `{round(center)-1, round(center),
/// round(center)+1}` per SPEC §2.5; `center` is a live, per-hop EMA of the
/// fine-frequency-interpolated channel position (fast enough to follow
/// realistic drift, e.g. SPEC §7 V9's 50 Hz/min) -- distinct from the
/// slower track-*lifetime* power-weighted average used for the final
/// reported frequency (Task 6).
pub(crate) struct Track {
    pub(crate) id: u32,
    lifecycle: Lifecycle,
    pub(crate) center: f64,
    pub(crate) current_snr_db: f32,
    pub(crate) sum_weighted: f64,
    pub(crate) sum_power: f64,
    pub(crate) birth_channel: usize,
}

/// SPEC §2.5: a live centroid EMA responsive enough to follow realistic
/// drift (V9: 50 Hz/min == ~0.0089 channels/s at 93.75 Hz spacing) with
/// negligible lag relative to the 375 Hz hop rate; tuned empirically
/// against V9 in Task 9 and pinned there.
const CENTER_EMA_ALPHA: f64 = 0.01;

impl Track {
    fn new(id: u32, birth_channel: usize, cfg: &DetectorConfig) -> Self {
        Track {
            id,
            lifecycle: Lifecycle::new(cfg),
            center: birth_channel as f64,
            current_snr_db: 0.0,
            sum_weighted: 0.0,
            sum_power: 0.0,
            birth_channel,
        }
    }

    pub(crate) fn state(&self) -> LifecycleState {
        self.lifecycle.state()
    }

    /// SPEC §2.5: owned channel indices for this hop's ownership checks.
    fn owned(&self, n_channels: usize) -> [usize; 3] {
        let c = self.center.round() as i64;
        let n = n_channels as i64;
        [
            ((c - 1).rem_euclid(n)) as usize,
            (c.rem_euclid(n)) as usize,
            ((c + 1).rem_euclid(n)) as usize,
        ]
    }

    /// Max-power channel among this track's owned set this hop. SPEC §2.5.
    fn select_channel(&self, power: &[f32], n_channels: usize) -> usize {
        let owned = self.owned(n_channels);
        owned
            .into_iter()
            .max_by(|&a, &b| power[a].partial_cmp(&power[b]).unwrap())
            .unwrap()
    }

    /// Update `center` (live EMA) and the lifetime power-weighted
    /// accumulator, from this hop's selected channel's fine-frequency
    /// interpolation. SPEC §1.4/§2.5.
    fn update_centroid(&mut self, k: usize, power: &[f32], n_channels: usize) {
        let k_minus = (k + n_channels - 1) % n_channels;
        let k_plus = (k + 1) % n_channels;
        if let Some(delta) = interpolate_offset(power[k_minus], power[k], power[k_plus]) {
            let raw = k as f64 + delta;
            self.center += CENTER_EMA_ALPHA * (raw - self.center);
            let w = power[k] as f64;
            self.sum_weighted += raw * w;
            self.sum_power += w;
        }
    }
}

/// Orchestrates SPEC §2's real detector across all channels: per-channel
/// floor + gate (`manta-dsp::floor`), per-channel lifecycle state
/// machines (`Lifecycle`), and §2.5 adjacent-channel ownership. This task's
/// `step_hop` returns which ACTIVE tracks selected which channel this hop,
/// without touching `TrackDecoder` -- Task 6 adds the decoder pool.
pub struct TrackManager {
    floor: FloorBank,
    gate: Gate,
    tracks: BTreeMap<u32, Track>,
    owner_of: Vec<Option<u32>>, // channel -> owning track_id, recomputed each hop
    next_id: u32,
    cfg: DetectorConfig,
    hop_counter: u64,
}

impl TrackManager {
    pub fn new(n_channels: usize, cfg: DetectorConfig) -> Self {
        TrackManager {
            floor: FloorBank::new(n_channels),
            gate: Gate::new(n_channels, cfg.on_snr_db, cfg.off_snr_db),
            tracks: BTreeMap::new(),
            owner_of: vec![None; n_channels],
            next_id: 1,
            cfg,
            hop_counter: 0,
        }
    }

    fn n_channels(&self) -> usize {
        self.owner_of.len()
    }

    fn recompute_ownership(&mut self) {
        self.owner_of.iter_mut().for_each(|o| *o = None);
        for (&id, track) in &self.tracks {
            for ch in track.owned(self.n_channels()) {
                self.owner_of[ch] = Some(id);
            }
        }
    }

    /// One hop: update floor/gate, drive every track's lifecycle, spawn new
    /// CANDIDATEs, apply ownership/merge, evict over cap. Returns
    /// `(track_id, selected_channel, magnitude)` for every currently-ACTIVE
    /// track this hop -- `char_emitted` (GC timer input) is always `false`
    /// here since no decoder is wired yet; Task 6 threads the real value
    /// through once `TrackDecoder` is attached.
    fn step_hop(&mut self, hop: &HopOutput) -> Vec<(u32, usize, f32)> {
        let power_db_vals: Vec<f64> = hop.power.iter().map(|&p| power_db(p)).collect();
        self.floor.update(&power_db_vals);
        let (rise, drop) = self.gate.update(&power_db_vals, &self.floor);
        self.recompute_ownership();

        let past_warmup = self.hop_counter >= self.cfg.warmup_hops;
        self.hop_counter += 1;

        // Drive existing tracks; collect closures to apply after the loop
        // (avoids mutating `self.tracks` while iterating it).
        let mut closed: Vec<u32> = Vec::new();
        let mut selections: Vec<(u32, usize, f32)> = Vec::new();
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for id in ids {
            let n = self.n_channels();
            let track = self.tracks.get_mut(&id).unwrap();
            let k = track.select_channel(&hop.power, n);
            track.update_centroid(k, &hop.power, n);
            let f = self.floor.effective_floor_db(k);
            track.current_snr_db = (self.gate.smoothed_db(k) - f) as f32;
            let event = track.lifecycle.on_hop(rise[k], drop[k], false);
            match event {
                LifecycleEvent::Closed(_) => {
                    closed.push(id);
                }
                _ => {
                    if track.state() == LifecycleState::Active {
                        selections.push((id, k, hop.power[k].sqrt()));
                    }
                }
            }
        }
        for id in closed {
            self.tracks.remove(&id);
        }
        self.recompute_ownership();

        // Same-hop simultaneous-rise tie-break (SPEC §2.5): scan channels in
        // ascending order; a rise on an unowned channel spawns a CANDIDATE
        // unless a higher-power unowned neighbor also rose this hop, in
        // which case only the higher-power one spawns.
        if past_warmup {
            let n = self.n_channels();
            let mut k = 0;
            while k < n {
                if rise[k] && self.owner_of[k].is_none() {
                    let mut winner = k;
                    if k + 1 < n && rise[k + 1] && self.owner_of[k + 1].is_none() {
                        if hop.power[k + 1] > hop.power[winner] {
                            winner = k + 1;
                        }
                        // both channels claimed by this decision; skip past k+1
                        // next iteration so we don't re-evaluate it as its own birth.
                        self.spawn(winner);
                        k += 2;
                        continue;
                    }
                    self.spawn(winner);
                }
                k += 1;
            }
        }
        self.recompute_ownership();

        self.merge_converged();
        self.evict_over_cap();

        selections
    }

    fn spawn(&mut self, birth_channel: usize) {
        let id = self.next_id;
        self.next_id += 1;
        self.tracks
            .insert(id, Track::new(id, birth_channel, &self.cfg));
        for ch in self.tracks[&id].owned(self.n_channels()) {
            self.owner_of[ch] = Some(id);
        }
    }

    /// SPEC §2.5: tracks whose centers converge within 1.0 channel merge;
    /// the lower-current-SNR one is closed.
    fn merge_converged(&mut self) {
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        let mut to_close = Vec::new();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (a, b) = (ids[i], ids[j]);
                if to_close.contains(&a) || to_close.contains(&b) {
                    continue;
                }
                let (ca, cb) = (self.tracks[&a].center, self.tracks[&b].center);
                if (ca - cb).abs() < 1.0 {
                    let loser = if self.tracks[&a].current_snr_db <= self.tracks[&b].current_snr_db
                    {
                        a
                    } else {
                        b
                    };
                    to_close.push(loser);
                }
            }
        }
        for id in to_close {
            self.tracks.remove(&id);
        }
        if !ids.is_empty() {
            self.recompute_ownership();
        }
    }

    /// SPEC §2.4/ARCHITECTURE §4: track cap with lowest-current-SNR
    /// eviction.
    fn evict_over_cap(&mut self) {
        while self.tracks.len() > self.cfg.track_cap {
            let loser = *self
                .tracks
                .iter()
                .min_by(|(_, a), (_, b)| a.current_snr_db.partial_cmp(&b.current_snr_db).unwrap())
                .map(|(id, _)| id)
                .unwrap();
            self.tracks.remove(&loser);
        }
        self.recompute_ownership();
    }
}
```

- [ ] **Step 2: Write the tests**

Add to `#[cfg(test)] mod tests` in `crates/manta-engine/src/track.rs`:

```rust
    use manta_dsp::channelizer::HopOutput;

    fn hop(m: u64, power: Vec<f32>) -> HopOutput {
        HopOutput {
            m,
            x: vec![],
            power,
        }
    }

    fn quiet_power(n: usize) -> Vec<f32> {
        vec![1e-9; n] // ~ -90 dBFS
    }

    fn feed_warmup(tm: &mut TrackManager, n: usize) {
        let hops_needed = 250u64 * 15; // floor ring fill, same as manta-dsp::floor tests
        for m in 0..hops_needed {
            tm.step_hop(&hop(m, quiet_power(n)));
        }
    }

    #[test]
    fn spawns_and_promotes_a_track_on_a_strong_channel() {
        let mut tm = TrackManager::new(64, DetectorConfig::default());
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // +20 dB above the ~-90 dBFS floor
        let mut m = 250 * 15;
        let mut promoted = false;
        for _ in 0..25 {
            tm.step_hop(&hop(m, power.clone()));
            m += 1;
            if tm.tracks.values().any(|t| t.state() == LifecycleState::Active) {
                promoted = true;
                break;
            }
        }
        assert!(promoted, "a strong channel should spawn and promote a track");
        assert_eq!(tm.tracks.len(), 1);
    }

    #[test]
    fn adjacent_strong_channel_is_absorbed_not_a_new_track() {
        let mut tm = TrackManager::new(64, DetectorConfig::default());
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        power[11] = 1e-9 * 10f32.powf(15.0 / 10.0); // weaker neighbor, inside the owned window
        let mut m = 250 * 15;
        for _ in 0..25 {
            tm.step_hop(&hop(m, power.clone()));
            m += 1;
        }
        assert_eq!(
            tm.tracks.len(),
            1,
            "channel 11 is inside channel 10's owned window {{9,10,11}} and must be absorbed"
        );
    }

    #[test]
    fn two_well_separated_strong_channels_yield_two_tracks() {
        let mut tm = TrackManager::new(64, DetectorConfig::default());
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        power[40] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        for _ in 0..25 {
            tm.step_hop(&hop(m, power.clone()));
            m += 1;
        }
        assert_eq!(tm.tracks.len(), 2);
    }

    #[test]
    fn track_cap_evicts_lowest_snr() {
        let mut cfg = DetectorConfig::default();
        cfg.track_cap = 1;
        let mut tm = TrackManager::new(64, cfg);
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // strong, spawns first
        let mut m = 250 * 15;
        for _ in 0..25 {
            tm.step_hop(&hop(m, power.clone()));
            m += 1;
        }
        assert_eq!(tm.tracks.len(), 1);
        power[40] = 1e-9 * 10f32.powf(25.0 / 10.0); // stronger second signal, over cap
        for _ in 0..25 {
            tm.step_hop(&hop(m, power.clone()));
            m += 1;
        }
        assert_eq!(tm.tracks.len(), 1, "cap=1 must hold even with a second strong signal");
        assert!(
            tm.tracks.values().next().unwrap().birth_channel == 40,
            "the lower-SNR (weaker) track must be the one evicted"
        );
    }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p manta-engine track:: -- --nocapture`
Expected: all 10 tests pass (6 from Task 4 + 4 new). This is a slow test module (each test feeds ~3750+ warmup hops) — expect several seconds, not instant.

- [ ] **Step 4: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-engine/src/track.rs
git commit -m "feat(engine): TrackManager multi-channel orchestration + ownership (SPEC §2.5)"
```

---

### Task 6: `manta-engine::track` — decoder pool + `process_hops` public entry point

**Files:**
- Modify: `crates/manta-engine/src/track.rs`

**Interfaces:**
- Consumes: `manta_decode::decoder::{DecodeConfig, TrackDecoder}`, `manta_decode::events::DecoderEvent`, `rayon::prelude::*`.
- Produces: `impl TrackManager { pub fn new(n_channels: usize, fs: f64, center_freq_hz: f64, detector_cfg: DetectorConfig, decode_cfg: DecodeConfig) -> Self; pub fn process_hops(&mut self, hops: &[HopOutput], hop_to_sample_ts: impl Fn(u64) -> u64) -> Vec<DecoderEvent>; pub fn finish(&mut self) -> Vec<DecoderEvent> }`. This is the entry point Tasks 7-8 wire into `decode_samples`/`decode_wav`/`listen`.

- [ ] **Step 1: Write the failing tests**

Modify `TrackManager` in `crates/manta-engine/src/track.rs`: add fields and change `new`'s signature, add `pending`/`decoder` to `Track`, add the pool dispatch to `step_hop`, and add `process_hops`/`finish`.

Two separate replacements inside `impl Track`'s existing block: replace the `struct Track { ... }` definition, and separately replace just its `fn new` method. Leave `owned`, `select_channel`, and `update_centroid` (Task 5) untouched — `freq_hz` is a new method added after them, not a replacement. The struct and `fn new` become:

```rust
pub(crate) struct Track {
    pub(crate) id: u32,
    lifecycle: Lifecycle,
    pub(crate) center: f64,
    pub(crate) current_snr_db: f32,
    sum_weighted: f64,
    sum_power: f64,
    pub(crate) birth_channel: usize,
    decoder: Option<TrackDecoder>,
    pending: Vec<(f32, u64)>,
}
```

```rust
    fn new(id: u32, birth_channel: usize, cfg: &DetectorConfig) -> Self {
        Track {
            id,
            lifecycle: Lifecycle::new(cfg),
            center: birth_channel as f64,
            current_snr_db: 0.0,
            sum_weighted: 0.0,
            sum_power: 0.0,
            birth_channel,
            decoder: None,
            pending: Vec::new(),
        }
    }

    /// SPEC §1.1/§1.4: absolute Hz for this track's current lifetime
    /// power-weighted centroid (not the fast ownership-following EMA).
    fn freq_hz(&self, center_freq_hz: f64, channel_spacing_hz: f64, n_channels: usize) -> f64 {
        let centroid = if self.sum_power > 0.0 {
            self.sum_weighted / self.sum_power
        } else {
            self.birth_channel as f64
        };
        let k0_freq = center_freq_hz
            + wrapped_channel_offset(self.birth_channel, n_channels) * channel_spacing_hz;
        k0_freq + (centroid - self.birth_channel as f64) * channel_spacing_hz
    }
```

Add this free function near the top of the file (below the `const`s):

```rust
/// SPEC §1.1 f(k) mapping: signed channel offset from center, FFT bin order.
fn wrapped_channel_offset(k: usize, n_channels: usize) -> f64 {
    let half = n_channels as i64 / 2;
    let signed = ((k as i64 + half).rem_euclid(n_channels as i64)) - half;
    signed as f64
}
```

Replace `TrackManager`'s struct definition and `new`:

```rust
pub struct TrackManager {
    floor: FloorBank,
    gate: Gate,
    tracks: BTreeMap<u32, Track>,
    owner_of: Vec<Option<u32>>,
    next_id: u32,
    cfg: DetectorConfig,
    decode_cfg: DecodeConfig,
    hop_counter: u64,
    center_freq_hz: f64,
    channel_spacing_hz: f64,
}

impl TrackManager {
    pub fn new(
        n_channels: usize,
        fs: f64,
        center_freq_hz: f64,
        detector_cfg: DetectorConfig,
        decode_cfg: DecodeConfig,
    ) -> Self {
        TrackManager {
            floor: FloorBank::new(n_channels),
            gate: Gate::new(n_channels, detector_cfg.on_snr_db, detector_cfg.off_snr_db),
            tracks: BTreeMap::new(),
            owner_of: vec![None; n_channels],
            next_id: 1,
            cfg: detector_cfg,
            decode_cfg,
            hop_counter: 0,
            center_freq_hz,
            channel_spacing_hz: fs / n_channels as f64,
        }
    }
```

Replace `step_hop`'s signature and body (the per-track loop and the promoted-track handling) so newly-ACTIVE tracks lease a decoder, and ACTIVE tracks queue `(mag, sample_ts)` instead of returning selections directly. `step_hop` now takes a plain `sample_ts: u64` (computed by the caller, `process_hops` below, from its own `hop_to_sample_ts` closure) instead of returning `Vec<(u32, usize, f32)>`:

```rust
    fn step_hop(&mut self, hop: &HopOutput, sample_ts: u64) {
        let power_db_vals: Vec<f64> = hop.power.iter().map(|&p| power_db(p)).collect();
        self.floor.update(&power_db_vals);
        let (rise, drop) = self.gate.update(&power_db_vals, &self.floor);
        self.recompute_ownership();

        let past_warmup = self.hop_counter >= self.cfg.warmup_hops;
        self.hop_counter += 1;

        let mut closed: Vec<u32> = Vec::new();
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for id in ids {
            let n = self.n_channels();
            let track = self.tracks.get_mut(&id).unwrap();
            let k = track.select_channel(&hop.power, n);
            track.update_centroid(k, &hop.power, n);
            let f = self.floor.effective_floor_db(k);
            track.current_snr_db = (self.gate.smoothed_db(k) - f) as f32;
            let char_emitted = false; // GC timer input; refined below once a decoder exists.
            let event = track.lifecycle.on_hop(rise[k], drop[k], char_emitted);
            match event {
                LifecycleEvent::Closed(_) => closed.push(id),
                LifecycleEvent::Promoted => {
                    track.decoder = Some(TrackDecoder::new(id, self.decode_cfg.clone()));
                    track
                        .decoder
                        .as_mut()
                        .unwrap()
                        .set_freq_hz(self.center_freq_hz);
                    track.pending.push((hop.power[k].sqrt(), sample_ts));
                }
                LifecycleEvent::None => {
                    if track.state() == LifecycleState::Active {
                        track.pending.push((hop.power[k].sqrt(), sample_ts));
                    }
                }
            }
        }
        for id in closed {
            self.tracks.remove(&id);
        }
        self.recompute_ownership();

        if past_warmup {
            let n = self.n_channels();
            let mut k = 0;
            while k < n {
                if rise[k] && self.owner_of[k].is_none() {
                    let mut winner = k;
                    if k + 1 < n && rise[k + 1] && self.owner_of[k + 1].is_none() {
                        if hop.power[k + 1] > hop.power[winner] {
                            winner = k + 1;
                        }
                        self.spawn(winner);
                        k += 2;
                        continue;
                    }
                    self.spawn(winner);
                }
                k += 1;
            }
        }
        self.recompute_ownership();
        self.merge_converged();
        self.evict_over_cap();
    }
```

Add `process_hops`/`finish` and the rayon dispatch after `evict_over_cap`:

```rust
    /// Process one `Channelizer::process()` slice: sequential per-hop
    /// state-machine bookkeeping (ownership/promotion/eviction), then a
    /// single rayon-parallel decode pass across all currently-queued
    /// ACTIVE tracks (the decoder pool -- ARCHITECTURE §10), then
    /// resequence by `(sample_ts, track_id)` per SPEC §6 rule 6.
    /// `hop_to_sample_ts` mirrors the existing `decode_samples`/`listen`
    /// convention of converting a channelizer hop index to a raw-sample
    /// timestamp.
    pub fn process_hops(
        &mut self,
        hops: &[HopOutput],
        hop_to_sample_ts: impl Fn(u64) -> u64,
    ) -> Vec<DecoderEvent> {
        for h in hops {
            self.step_hop(h, hop_to_sample_ts(h.m));
        }
        self.drain_pool()
    }

    /// Flush every track's decoder (SPEC §5 end-of-stream). Call once,
    /// after the last `process_hops`.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| t.decoder.as_mut())
            .par_bridge()
            .flat_map_iter(|d| d.finish())
            .collect();
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        events
    }

    fn drain_pool(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| {
                if t.pending.is_empty() {
                    None
                } else {
                    let pending = std::mem::take(&mut t.pending);
                    Some((t.decoder.as_mut().unwrap(), pending))
                }
            })
            .par_bridge()
            .flat_map_iter(|(decoder, pending)| {
                pending
                    .into_iter()
                    .flat_map(|(mag, ts)| decoder.push_envelope(mag, ts))
                    .collect::<Vec<_>>()
            })
            .collect();
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        events
    }
```

Add these two free functions near `wrapped_channel_offset`:

```rust
fn event_sample_ts(e: &DecoderEvent) -> u64 {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. } | DecoderEvent::WordBoundary { sample_ts, .. } => {
            *sample_ts
        }
        DecoderEvent::SpeedUpdate { .. } | DecoderEvent::TrackMeta { .. } => 0,
    }
}

fn event_track_id(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. } => *track_id,
    }
}
```

Add `use manta_decode::decoder::{DecodeConfig, TrackDecoder};` and `use manta_decode::events::DecoderEvent;` to the top of `crates/manta-engine/src/track.rs`.

Update the existing Task-5 tests: `TrackManager::new` and `step_hop` calls in `mod tests` now need the new signature. Replace every `TrackManager::new(64, DetectorConfig::default())` (and the cap-test's `TrackManager::new(64, cfg)`) with `TrackManager::new(64, 96_000.0, 14_000_000.0, DetectorConfig::default(), DecodeConfig::default())` (or `cfg` in place of the default), and every `tm.step_hop(&hop(m, power.clone()))` with `tm.step_hop(&hop(m, power.clone()), m)` (identity sample_ts mapping is fine for these unit tests, which don't assert on decoded text/timing). Add `use manta_decode::decoder::DecodeConfig;` to the test module's imports.

Add a new integration-style test proving the pool actually decodes, at the end of `mod tests`:

```rust
    #[test]
    fn active_track_decodes_real_text() {
        use manta_dsp::channelizer::Channelizer;
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
        let hop_samples = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            spec.fs,
            spec.center_freq_hz,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        let mut all_events = Vec::new();
        for chunk in rendered.samples.chunks(4096) {
            let hops = ch.process(chunk);
            all_events.extend(tm.process_hops(&hops, |m| m * hop_samples));
        }
        all_events.extend(tm.finish());
        let track_ids: std::collections::BTreeSet<u32> =
            all_events.iter().map(event_track_id).collect();
        assert_eq!(track_ids.len(), 1, "V1 is single-signal, expected exactly 1 track");
        let text = manta_decode::decoder::events_to_text(&all_events);
        assert_eq!(
            manta_testkit::cer::cer(&rendered.keyed_texts[0], &text),
            0.0,
            "expected {:?} got {:?}",
            rendered.keyed_texts[0],
            text
        );
    }
```

Add `manta-testkit = { workspace = true }` to `crates/manta-engine/Cargo.toml`'s `[dev-dependencies]` if not already present (it already is, per Task 4's read of that file — no change needed).

- [ ] **Step 2: Run the tests**

Run: `cargo test -p manta-engine track:: -- --nocapture`
Expected: all 11 tests pass (10 from Tasks 4-5 + 1 new). The new `active_track_decodes_real_text` test takes several seconds (real V1 decode).

- [ ] **Step 3: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/manta-engine/src/track.rs crates/manta-engine/Cargo.toml
git commit -m "feat(engine): decoder pool (rayon) + TrackManager::process_hops (ARCHITECTURE §10)"
```

---

### Task 7: Wire `decode_samples`/`decode_wav` onto `TrackManager`

**Files:**
- Modify: `crates/manta-engine/src/lib.rs`
- Delete: `crates/manta-engine/src/detect.rs`

**Interfaces:**
- Consumes: `TrackManager::{new, process_hops, finish}` (Task 6).
- Produces: `decode_samples`/`decode_wav` unchanged public signatures; `DecodeReport` unchanged fields, now computed from the lowest-`track_id` track's events (single-track scenes keep behaving exactly as before).

- [ ] **Step 1: Update `decode_samples`**

In `crates/manta-engine/src/lib.rs`, replace the whole body of `decode_samples` (from the `let mut ch = ...` line through `Ok(DecodeReport { ... })`) with:

```rust
    let mut ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))
        .context("channelizer")?;
    let hop = ch.hop() as u64;

    debug_assert!(
        (fs / hop as f64 - manta_decode::FO_HZ).abs() < 0.01,
        "channelizer hop rate {} Hz diverges from manta_decode::FO_HZ {}",
        fs / hop as f64,
        manta_decode::FO_HZ
    );

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut padded_iq = Vec::with_capacity(pad_samples + iq.len());
    padded_iq.resize(pad_samples, Complex32::new(0.0, 0.0));
    padded_iq.extend_from_slice(iq);

    let mut tm = manta_engine_track_manager(fs, ch.n_channels(), center_freq_hz, cfg);
    let hops = ch.process(&padded_iq);
    if hops.is_empty() {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    let mut events = tm.process_hops(&hops, |m| (m.saturating_sub(pad_hops)) * hop);
    events.extend(tm.finish());

    if events.is_empty() {
        bail!("no signal found (input shorter than one filter length or empty)");
    }
    let min_track_id = events.iter().map(track::event_track_id_pub).min().unwrap();
    let this_track: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| track::event_track_id_pub(e) == min_track_id)
        .cloned()
        .collect();
    let freq_hz = this_track
        .iter()
        .rev()
        .find_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .unwrap_or(center_freq_hz);
    let wpm = this_track.iter().rev().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let text = events_to_text(&this_track);
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
    })
```

This drops the SPEC §1.4 centroid loop that used to live directly in `decode_samples` (`k0`, `k_minus`, `k_plus`, `sum_weighted`, `sum_power`) — that logic now lives per-track inside `Track::update_centroid` (Task 5) and is reflected via `TrackMeta.freq_hz`. `freq_hz` here is read from the lowest-track_id track's most recent `TrackMeta` event instead of being computed inline; this only fires once every 375 hops (`META_INTERVAL_HOPS` in `manta_decode::decoder`), so it's the freshest available estimate at report time, not necessarily the very-last-hop value — call out this precision difference in Task 10's tolerance re-measurement.

`TrackManager` needs a `fs`-and-`n_channels`-and-`center_freq_hz`-and-config constructor helper visible to `lib.rs`; add this small free function to `crates/manta-engine/src/track.rs` (it just forwards to `TrackManager::new`, existing purely so `lib.rs` doesn't need to import `DecodeConfig`/`DetectorConfig` construction details — actually simplest: just call `track::TrackManager::new` directly). Replace the `manta_engine_track_manager(...)` call above with:

```rust
    let mut tm = track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector.clone(),
        cfg.decode.clone(),
    );
```

And make `track::event_track_id_pub` a `pub(crate)` re-export of the existing `event_track_id` free function in `track.rs` — simplest: change `fn event_track_id` (added in Task 6) to `pub(crate) fn event_track_id` directly (no separate `_pub` wrapper needed). Use `track::event_track_id(e)` at both call sites above instead of `track::event_track_id_pub(e)`.

Add `mod track;` is already present from Task 4; add `use crate::track;` is unnecessary since `lib.rs` is the crate root — reference it as `track::TrackManager` and `track::event_track_id` directly (both already `pub`/`pub(crate)` in the same crate).

- [ ] **Step 2: Add `detector` to `PipelineConfig`**

In `crates/manta-engine/src/lib.rs`, update `PipelineConfig`:

```rust
/// M0 pipeline tunables. SPEC §5.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    pub decode: DecodeConfig,
    pub detector: track::DetectorConfig,
}
```

(`track::DetectorConfig` already derives/implements `Default` per Task 4, so `#[derive(Default)]` on `PipelineConfig` keeps working unchanged.)

- [ ] **Step 3: Remove the placeholder detector**

```bash
git rm crates/manta-engine/src/detect.rs
```

Remove `mod detect;` from `crates/manta-engine/src/lib.rs`.

- [ ] **Step 4: Remove now-dead imports/debug_assert duplication**

In `crates/manta-engine/src/lib.rs`, confirm `use manta_decode::decoder::{events_to_text, DecodeConfig, TrackDecoder};` — `TrackDecoder` is no longer used directly in `lib.rs` (it's constructed inside `track.rs` now); change the import to `use manta_decode::decoder::{events_to_text, DecodeConfig};` and `use manta_decode::events::DecoderEvent;` stays (used in the new report-building code above).

- [ ] **Step 5: Update `channelizer_chunking_determinism.rs` and `chunking_determinism.rs`**

These tests construct `Channelizer` + `TrackDecoder` directly, bypassing `decode_samples`/`detect.rs` entirely — re-read both files first to confirm neither imports `manta_engine::detect` (they don't; Task 4/6's earlier read confirmed `channelizer_chunking_determinism.rs` only imports `manta_decode::decoder::{DecodeConfig, TrackDecoder}` and `manta_dsp::channelizer::Channelizer`). No changes needed to either file.

- [ ] **Step 6: Run the existing regression suite**

Run: `cargo test -p manta-engine`
Expected: `pipeline.rs`'s `v1_lite_decodes_end_to_end` and `silence_errors_cleanly`, `listen_audio.rs` (unaffected until Task 8), `channelizer_chunking_determinism.rs`, `chunking_determinism.rs`, `roundtrip_iq.rs`, and `regression_char_gap_high_wpm.rs` all pass. If `v1_lite_decodes_end_to_end`'s freq/WPM assertions fail against the current ±25 Hz/±3 WPM tolerances, that's expected to investigate now (don't defer silently) — the whole point of this sub-project is these should tighten, not loosen further; if they now fail even at the *current* widened tolerance, stop and diagnose before proceeding (likely a bug in the ownership/centroid wiring, not a real tolerance regression).

- [ ] **Step 7: Run workspace build + clippy**

Run: `cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. Fix any leftover unused-import/dead-code warnings from removing `detect.rs`.

- [ ] **Step 8: Commit**

```bash
git add crates/manta-engine/src/lib.rs
git rm crates/manta-engine/src/detect.rs
git commit -m "feat(engine): wire TrackManager into decode_samples/decode_wav, remove placeholder detector"
```

---

### Task 8: Wire `listen()` onto `TrackManager`

**Files:**
- Modify: `crates/manta-engine/src/listen.rs`

**Interfaces:**
- Consumes: `TrackManager::{new, process_hops, finish}` (Task 6), same as Task 7.
- Produces: `listen`'s public signature unchanged (`fn listen(src: AudioIqSource, cfg: &PipelineConfig, stop: Arc<AtomicBool>, on_event: impl FnMut(&DecoderEvent)) -> Result<()>`), now emitting the merged multi-track event stream.

- [ ] **Step 1: Rewrite `listen`**

Replace `crates/manta-engine/src/listen.rs`'s body from the `let mut calib_ch = ...` line through the end of the function with:

```rust
    let mut ch =
        manta_dsp::channelizer::Channelizer::new(fs, 0.0).map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        0.0,
        cfg.detector.clone(),
        cfg.decode.clone(),
    );

    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let padding = vec![Complex32::new(0.0, 0.0); pad_samples];
    for ev in tm.process_hops(&ch.process(&padding), |m| {
        m.saturating_sub(pad_hops) * hop
    }) {
        on_event(&ev);
    }
    for ev in tm.process_hops(&ch.process(&calib), |m| {
        m.saturating_sub(pad_hops) * hop
    }) {
        on_event(&ev);
    }

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        for ev in tm.process_hops(&ch.process(&chunk[..n]), |m| {
            m.saturating_sub(pad_hops) * hop
        }) {
            on_event(&ev);
        }
    }
    for ev in tm.finish() {
        on_event(&ev);
    }
    Ok(())
```

Note this drops `listen`'s own startup `calib_ch`/`calibrate_channel`/`k0`/`offset_hz` block entirely — `TrackManager` does its own real detection continuously, so there's no separate one-time calibration pass. The `CALIBRATION_SECONDS`-buffered `calib` read loop stays (it still fills a startup buffer of raw IQ before building the channelizer), but that buffer is now just fed through `tm.process_hops` like any other chunk rather than driving a one-shot channel pick.

Remove the now-unused `use anyhow::Context;` if `.context(...)` is no longer called anywhere in this file after the rewrite (check: the original file's only `.context(...)` call was on the removed `calibrate_channel` line) — replace `use anyhow::{Context, Result};` with `use anyhow::Result;`.

- [ ] **Step 2: Run `listen_audio.rs`**

Run: `cargo test -p manta-engine --test listen_audio`
Expected: `listen_decodes_a_clean_real_audio_signal` passes (decoded text still contains "W1AW"; the test doesn't assert on `track_id`).

- [ ] **Step 3: Run `channelizer_chunking_determinism.rs`-style chunk invariance for `listen`**

There is no existing dedicated "listen chunk-size invariance" test to update (the `channelizer_chunking_determinism.rs`/`chunking_determinism.rs` tests exercise the batch-style manual pipeline, unaffected by this task). Skip — Task 6's determinism guarantee (SPEC §6 rule 6, resequencing before returning) already covers `process_hops` being called repeatedly across chunk boundaries, since the design doc's data-flow section establishes this holds for any hop-slice granularity.

- [ ] **Step 4: Run workspace build + clippy**

Run: `cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-engine/src/listen.rs
git commit -m "feat(engine): wire TrackManager into listen(), drop one-shot startup calibration"
```

---

### Task 9: Golden vectors V7 (adjacent) and V9 (drift)

**Files:**
- Modify: `crates/manta-testkit/src/vectors.rs`
- Create: `crates/manta-cli/tests/golden_v7_v9_v10.rs`

**Interfaces:**
- Consumes: `SignalSpec`/`render_scene` (existing, unchanged — already multi-signal-generic per sub-project 1).
- Produces: `pub fn v7() -> VectorSpec`, `pub fn v9() -> VectorSpec`, `Manifest.expected_freqs_hz: Vec<f64>` (new field, additive).

- [ ] **Step 1: Add V7/V9 VectorSpecs and extend `Manifest`**

In `crates/manta-testkit/src/vectors.rs`, add after `v6()`:

```rust
/// SPEC §7 V7 "adjacent": 24 WPM @ +10.000 kHz and 28 WPM @ +10.150 kHz,
/// both +15 dB, AWGN. Pass: exactly 2 tracks; both char >= 95%; both freqs
/// within ±15 Hz.
pub fn v7() -> VectorSpec {
    VectorSpec {
        name: "v7",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5637, // "SKIMV7"
        signals: vec![
            SignalSpec {
                text: "CQ CQ DE N1AA N1AA K".into(),
                loop_text: true,
                wpm: 24.0,
                offset_hz: 10_000.0,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
            },
            SignalSpec {
                text: "CQ CQ DE N2BB N2BB K".into(),
                loop_text: true,
                wpm: 28.0,
                offset_hz: 10_150.0,
                snr_2500_db: 15.0,
                jitter: None,
                qsb: None,
                watterson: None,
            },
        ],
    }
}

/// SPEC §7 V9 "drift": 18 WPM, +12 dB, drift +50 Hz/min, AWGN. Pass: 1
/// track (no split); char >= 90%; final freq within ±15 Hz of the drifted
/// end frequency.
///
/// `render_scene` has no built-in linear-drift primitive (only `offset_hz`,
/// a fixed NCO frequency) -- this vector approximates drift as a sequence
/// of short fixed-offset segments stepped every 2 s, each `render_scene`d
/// separately and concatenated, giving a staircase that closely
/// approximates linear drift at the channelizer's ~94 Hz channel
/// resolution (each 2 s step moves ~1.67 Hz, far under one channel).
pub fn v9() -> VectorSpec {
    VectorSpec {
        name: "v9",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5639, // "SKIMV9"
        signals: vec![SignalSpec {
            text: "CQ CQ DE EA8AAA EA8AAA K".into(),
            loop_text: true,
            wpm: 18.0,
            offset_hz: 6_000.0, // start frequency; end = 6000 + 100 = 6100 Hz over 120s @ 50Hz/min
            snr_2500_db: 12.0,
            jitter: None,
            qsb: None,
            watterson: None,
        }],
    }
}
```

Add a dedicated render function for V9's staircase drift next to `render` (V9 needs different rendering, not the plain `render_scene` call every other vector uses):

```rust
/// SPEC §7 V9: render a linear +50 Hz/min drift as a staircase of 2 s
/// fixed-offset segments (see `v9`'s doc comment). Returns the same shape
/// as `render`, with `expected_freq_hz` set to the *final* segment's
/// offset (SPEC's "final freq tracks within 15 Hz" pass criterion).
pub fn render_v9_drift(spec: &VectorSpec) -> Result<RenderedVector> {
    const DRIFT_HZ_PER_MIN: f64 = 50.0;
    const STEP_S: f64 = 2.0;
    let sig = &spec.signals[0];
    let n_steps = (spec.duration_s / STEP_S).round() as usize;
    let mut samples = Vec::new();
    let mut keyed_text = String::new();
    for i in 0..n_steps {
        let t_start_s = i as f64 * STEP_S;
        let offset_hz = sig.offset_hz + DRIFT_HZ_PER_MIN * t_start_s / 60.0;
        let step_sig = SignalSpec {
            offset_hz,
            ..sig.clone()
        };
        // Each step keys a fresh loop from t=0 (a small, deliberate
        // approximation: real drift wouldn't reset keying phase every
        // step, but the decode-accuracy pass criterion only cares about
        // character content, and word/char boundaries are far longer than
        // one 2 s step at 18 WPM).
        let (step_samples, step_text) =
            render_scene(std::slice::from_ref(&step_sig), spec.fs, STEP_S, None)?;
        samples.extend(step_samples);
        if i == n_steps / 2 {
            keyed_text = step_text; // representative mid-scene text for CER comparison
        }
    }
    if let Some(seed) = Some(spec.noise_seed) {
        crate::noise::add_unit_awgn(&mut samples, seed);
    }
    for s in &mut samples {
        *s *= crate::scene::MASTER_SCALE;
    }
    let final_offset_hz = sig.offset_hz + DRIFT_HZ_PER_MIN * (spec.duration_s - STEP_S) / 60.0;
    Ok(RenderedVector {
        samples,
        keyed_texts: vec![keyed_text],
        expected_freq_hz: spec.center_freq_hz + final_offset_hz,
    })
}
```

Add `expected_freqs_hz: Vec<f64>` to `Manifest` (additive, keeps `expected_freq_hz` for existing single-signal callers) and thread it through `write_fixture_set`:

```rust
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub name: String,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub expected_freq_hz: f64,
    /// Per-signal expected absolute frequency, in `signals` order. For
    /// single-signal vectors this is `[expected_freq_hz]`.
    pub expected_freqs_hz: Vec<f64>,
    pub keyed_texts: Vec<String>,
    pub generator: String,
}

pub fn write_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render(spec)?;
    write_fixture(
        dir,
        spec.name,
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
    )?;
    let expected_freqs_hz: Vec<f64> = spec
        .signals
        .iter()
        .map(|s| spec.center_freq_hz + s.offset_hz)
        .collect();
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        expected_freqs_hz,
        keyed_texts: rendered.keyed_texts,
        generator: concat!("manta-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
```

Add a dedicated V9 fixture writer (parallel to `write_fixture_set`, since V9 needs `render_v9_drift` instead of `render`):

```rust
/// V9-specific fixture writer: same shape as `write_fixture_set`, using
/// `render_v9_drift` instead of `render`.
pub fn write_v9_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render_v9_drift(spec)?;
    write_fixture(
        dir,
        spec.name,
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
    )?;
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        expected_freqs_hz: vec![rendered.expected_freq_hz],
        keyed_texts: rendered.keyed_texts,
        generator: concat!("manta-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}
```

- [ ] **Step 2: Write the golden tests**

Create `crates/manta-cli/tests/golden_v7_v9_v10.rs`:

```rust
//! SPEC §7 V7/V9/V10 golden gates (M2 sub-project 2: real multi-track
//! detector). V10 is added in Task 10.

use std::collections::BTreeMap;
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

/// Group `report["events"]` by `track_id`, returning each track's decoded
/// text and its last-reported TrackMeta freq_hz.
fn per_track(report: &serde_json::Value) -> BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u64, f64> = BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = ev["track_id"].as_u64().unwrap();
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"]["Char"].as_str() {
                    texts.entry(tid).or_default().push_str(c);
                }
            }
            "WordBoundary" => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            "TrackMeta" => {
                freqs.insert(tid, ev["freq_hz"].as_f64().unwrap());
            }
            _ => {}
        }
    }
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

#[test]
fn v7_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v7();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    assert_eq!(tracks.len(), 2, "V7 must produce exactly 2 tracks, got {}", tracks.len());

    for (i, expected_text) in manifest.keyed_texts.iter().enumerate() {
        let expected_freq = manifest.expected_freqs_hz[i];
        // Match each expected signal to whichever decoded track's freq is closest.
        let (_, (decoded_text, freq)) = tracks
            .iter()
            .min_by(|(_, (_, fa)), (_, (_, fb))| {
                let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap();
        let cer = manta_testkit::cer::cer(expected_text, decoded_text);
        assert!(
            cer <= 0.05,
            "signal {i} ({expected_text:?}) char accuracy must be >= 95%, got CER {cer} (decoded {decoded_text:?})"
        );
        let freq = freq.expect("TrackMeta freq_hz must have fired at least once in a 120s scene");
        assert!(
            (freq - expected_freq).abs() <= 15.0,
            "signal {i} freq {} expected {} (err {})",
            freq,
            expected_freq,
            (freq - expected_freq).abs()
        );
    }
}

#[test]
fn v9_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v9();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_v9_fixture_set(&spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join("v9.wav"))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let tracks = per_track(&report);
    assert_eq!(tracks.len(), 1, "V9 must not split into multiple tracks under drift");
    let (_, (decoded_text, freq)) = tracks.iter().next().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_text);
    assert!(cer <= 0.10, "V9 char accuracy must be >= 90%, got CER {cer}");
    let freq = freq.expect("TrackMeta freq_hz must have fired at least once");
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 15.0,
        "final freq {} expected {} (err {})",
        freq,
        manifest.expected_freq_hz,
        (freq - manifest.expected_freq_hz).abs()
    );
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p manta-testkit -p manta-cli v7_ v9_ -- --nocapture`
Expected: both pass. If `v7`'s exactly-2-tracks or freq-error assertion fails, or `v9` splits into >1 track, treat it as a real bug in ownership/centroid tracking (Task 5/6) to fix — not a tolerance to loosen; only V2/pins-9-10 (Task 11) are pre-authorized to have their tolerances adjusted, and only after measurement.

If `CENTER_EMA_ALPHA` (Task 5) is too slow to keep V9's track from being evicted/re-spawned as drift carries it out of its current owned window before the EMA catches up, tune it here empirically (try values in `[0.005, 0.05]`) and update its doc comment with the measured reasoning, same as `CHAR_GAP_DITS`'s empirical-tuning precedent (`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`).

- [ ] **Step 4: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/manta-testkit/src/vectors.rs crates/manta-cli/tests/golden_v7_v9_v10.rs
git commit -m "test: V7 (adjacent-channel) and V9 (drift) golden vectors (SPEC §7)"
```

---

### Task 10: Farnsworth timing support + golden vector V10

**Files:**
- Modify: `crates/manta-testkit/src/keyer.rs`
- Modify: `crates/manta-testkit/src/scene.rs`
- Modify: `crates/manta-testkit/src/vectors.rs`
- Modify: `crates/manta-cli/tests/golden_v7_v9_v10.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `KeyerSpec.char_wpm: Option<f32>` (new field), `SignalSpec.char_wpm: Option<f32>` (new field), `pub fn v10() -> VectorSpec`.

- [ ] **Step 1: Add Farnsworth timing to `KeyerSpec`**

In `crates/manta-testkit/src/keyer.rs`, update `KeyerSpec`:

```rust
/// Keying parameters: speed, edge shape, optional jitter. SPEC §7.
#[derive(Debug, Clone, Copy)]
pub struct KeyerSpec {
    /// Effective/word speed. With `char_wpm: None`, this is also the
    /// character speed (the pre-Farnsworth behavior). With `char_wpm:
    /// Some(_)`, this is the *slower* overall pace (SPEC §7 V10).
    pub wpm: f32,
    /// Farnsworth character speed (SPEC §7 V10: characters keyed faster
    /// than the overall word-boundary pacing). `None` = no Farnsworth
    /// stretching; `wpm` alone sets both content and spacing timing.
    pub char_wpm: Option<f32>,
    /// Raised-cosine rise/fall, contained inside the element. SPEC §7: 5 ms.
    pub rise_ms: f64,
    pub jitter: Option<Jitter>,
}

impl KeyerSpec {
    /// A clean keyer at `wpm` with 5 ms raised-cosine edges and no jitter. SPEC §7.
    pub fn new(wpm: f32) -> Self {
        KeyerSpec {
            wpm,
            char_wpm: None,
            rise_ms: 5.0,
            jitter: None,
        }
    }
}

/// SPEC §7 V10 Farnsworth timing, standard PARIS-reference derivation:
/// content (dits/dahs + intra-character element gaps) run at `char_wpm`'s
/// unit; inter-character/inter-word gaps are stretched to a slower
/// `gap_unit_ms` so the overall pace matches `word_wpm`. The reference word
/// "PARIS " = 31 content units + 19 spacing units = 50 units total (the
/// standard definition of WPM in Morse), giving:
/// `gap_unit = (50 * 1200/word_wpm - 31 * content_unit) / 19`.
fn units_ms(word_wpm: f32, char_wpm: Option<f32>) -> (f64, f64) {
    let content_wpm = char_wpm.unwrap_or(word_wpm);
    let content_unit = 1200.0 / content_wpm as f64;
    let gap_unit = match char_wpm {
        None => content_unit,
        Some(_) => {
            let target_word_ms = 50.0 * 1200.0 / word_wpm as f64;
            (target_word_ms - 31.0 * content_unit) / 19.0
        }
    };
    (content_unit, gap_unit)
}
```

Update `push_word` to take both units (intra-character element gaps use `unit`/content speed; inter-character gaps use `gap_unit`/spacing speed):

```rust
/// Append one word's segments. `unit` = dit ms (content/char speed).
/// `gap_unit` = inter-character gap ms (spacing speed; equals `unit` unless
/// Farnsworth is active). Returns Err on unknown chars.
fn push_word(b: &mut SegmentBuilder, word: &str, unit: f64, gap_unit: f64) -> Result<()> {
    let chars: Vec<char> = word.chars().collect();
    for (ci, c) in chars.iter().enumerate() {
        let Some(pattern) = pattern_for(*c) else {
            bail!("character {c:?} has no Morse encoding");
        };
        let els: Vec<char> = pattern.chars().collect();
        for (ei, e) in els.iter().enumerate() {
            b.push(true, if *e == '.' { unit } else { 3.0 * unit });
            if ei < els.len() - 1 {
                b.push(false, unit);
            }
        }
        if ci < chars.len() - 1 {
            b.push(false, 3.0 * gap_unit);
        }
    }
    Ok(())
}
```

Update `key_text` and `key_text_loop` to compute and thread `gap_unit`:

```rust
/// Key `text` once. Returns (envelope at fs, normalized keyed text). SPEC §7.
pub fn key_text(text: &str, spec: &KeyerSpec, fs: f64) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let (unit, gap_unit) = units_ms(spec.wpm, spec.char_wpm);
    let mut b = SegmentBuilder::new(spec.jitter);
    let words: Vec<&str> = norm.split(' ').collect();
    for (wi, w) in words.iter().enumerate() {
        push_word(&mut b, w, unit, gap_unit)?;
        if wi < words.len() - 1 {
            b.push(false, 7.0 * gap_unit);
        }
    }
    let env = render(&b.segs, spec.rise_ms, fs, None);
    Ok((env, norm))
}
```

```rust
/// Key `text` repeatedly (7-dit gaps between repetitions) until `duration_s`.
/// Characters are keyed only if they fit entirely (pinned decision 13). SPEC §7.
pub fn key_text_loop(
    text: &str,
    spec: &KeyerSpec,
    fs: f64,
    duration_s: f64,
) -> Result<(Vec<f32>, String)> {
    let norm = normalize(text);
    let (unit, gap_unit) = units_ms(spec.wpm, spec.char_wpm);
    let budget_ms = duration_s * 1000.0;
    let mut b = SegmentBuilder::new(spec.jitter);
    let mut keyed = String::new();
    let mut elapsed = 0.0f64;
    'outer: loop {
        let words: Vec<&str> = norm.split(' ').collect();
        for (wi, w) in words.iter().enumerate() {
            let chars: Vec<char> = w.chars().collect();
            for (ci, c) in chars.iter().enumerate() {
                let mut scratch = SegmentBuilder {
                    segs: Vec::new(),
                    rng: b.rng.take(),
                };
                push_word(&mut scratch, &c.to_string(), unit, gap_unit)?;
                let char_ms: f64 = scratch.segs.iter().map(|s| s.dur_ms).sum();
                b.rng = scratch.rng.take();
                if elapsed + char_ms > budget_ms {
                    break 'outer;
                }
                b.segs.extend(scratch.segs);
                elapsed += char_ms;
                keyed.push(*c);
                if ci < chars.len() - 1 {
                    b.push(false, 3.0 * gap_unit);
                    elapsed += b.segs.last().unwrap().dur_ms;
                }
            }
            if wi < words.len() - 1 {
                b.push(false, 7.0 * gap_unit);
                elapsed += b.segs.last().unwrap().dur_ms;
                keyed.push(' ');
            }
        }
        b.push(false, 7.0 * gap_unit);
        elapsed += b.segs.last().unwrap().dur_ms;
        keyed.push(' ');
        if elapsed >= budget_ms {
            break;
        }
    }
    let n = (duration_s * fs).round() as usize;
    let env = render(&b.segs, spec.rise_ms, fs, Some(n));
    Ok((env, keyed.trim().to_string()))
}
```

Add a test to `keyer.rs`'s `mod tests`:

```rust
    #[test]
    fn farnsworth_stretches_only_gaps_not_marks() {
        // 25 WPM chars / 15 WPM effective: dit/dah durations must match a
        // plain 25 WPM keyer; the inter-character gap must be longer than
        // a plain 25 WPM keyer's 3-unit gap.
        let fast = KeyerSpec::new(25.0);
        let farnsworth = KeyerSpec {
            wpm: 15.0,
            char_wpm: Some(25.0),
            rise_ms: 5.0,
            jitter: None,
        };
        let (env_fast, _) = key_text("E", &fast, FS).unwrap();
        let (env_fw, _) = key_text("E", &farnsworth, FS).unwrap();
        assert_eq!(env_fast.len(), env_fw.len(), "a lone character's mark duration must be identical");

        let (env_fast_pair, _) = key_text("EE", &fast, FS).unwrap();
        let (env_fw_pair, _) = key_text("EE", &farnsworth, FS).unwrap();
        assert!(
            env_fw_pair.len() > env_fast_pair.len(),
            "Farnsworth inter-character gap must be longer than plain 25 WPM"
        );
    }
```

- [ ] **Step 2: Thread `char_wpm` through `SignalSpec`/`render_scene`**

In `crates/manta-testkit/src/scene.rs`, add `pub char_wpm: Option<f32>` to `SignalSpec` (as the new last field, after `watterson`):

```rust
#[derive(Debug, Clone)]
pub struct SignalSpec {
    pub text: String,
    pub loop_text: bool,
    pub wpm: f32,
    pub offset_hz: f64,
    pub snr_2500_db: f32,
    pub jitter: Option<Jitter>,
    pub qsb: Option<QsbSine>,
    pub watterson: Option<WattersonFade>,
    /// SPEC §7 V10 Farnsworth: character speed, if different from `wpm`
    /// (the effective/word speed). `None` for every existing vector.
    pub char_wpm: Option<f32>,
}
```

In `render_scene`, update the `KeyerSpec` construction:

```rust
        let spec = KeyerSpec {
            wpm: sig.wpm,
            char_wpm: sig.char_wpm,
            rise_ms: 5.0,
            jitter: sig.jitter,
        };
```

- [ ] **Step 3: Fix every existing `SignalSpec` literal (compiler-driven)**

Run: `cargo build --workspace 2>&1 | grep -B2 "missing field"`

This enumerates every `SignalSpec { ... }` literal that doesn't yet set `char_wpm` (expected: the 15 sites found across `crates/manta-engine/tests/{pipeline,regression_char_gap_high_wpm,roundtrip_iq}.rs`, `crates/manta-testkit/src/{scene,vectors}.rs`, `crates/manta-testkit/tests/channelizer_multisignal.rs`). For each reported site, add `char_wpm: None,` as the literal's last field. Re-run the build after each file until the grep for `missing field` returns nothing.

Run: `cargo build --workspace`
Expected: clean (no missing-field errors).

- [ ] **Step 4: Add V10 to `vectors.rs`**

In `crates/manta-testkit/src/vectors.rs`, add after `v9()`:

```rust
/// SPEC §7 V10 "farnsworth": 15 WPM effective / 25 WPM character speed,
/// +15 dB, AWGN. Pass: char >= 95%; word boundaries 100% correct.
pub fn v10() -> VectorSpec {
    VectorSpec {
        name: "v10",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5610, // "SKIMV10" truncated to fit
        signals: vec![SignalSpec {
            text: "CQ CQ DE G4XXX G4XXX K".into(),
            loop_text: true,
            wpm: 15.0,
            offset_hz: 8_000.0,
            snr_2500_db: 15.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: Some(25.0),
        }],
    }
}
```

Update the earlier `v1()` through `v9()` (and any other pre-existing `SignalSpec` literals in this file, e.g. inside `#[cfg(test)] mod tests` if any) to include `char_wpm: None,` — same compiler-driven sweep as Step 3, scoped to this file if the workspace-wide sweep didn't already catch it (it should have; this step is a safety re-check specific to this file since `v10` is added in the same commit).

- [ ] **Step 5: Add the V10 golden test**

Append to `crates/manta-cli/tests/golden_v7_v9_v10.rs`:

```rust
#[test]
fn v10_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v10();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V10 char accuracy must be >= 95%, got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // Word boundaries 100% correct: word count (space-separated) must match exactly.
    let expected_words = manifest.keyed_texts[0].split(' ').count();
    let decoded_words = decoded.split(' ').filter(|w| !w.is_empty()).count();
    assert_eq!(
        decoded_words, expected_words,
        "word boundary count mismatch: expected {expected_words} words, decoded {decoded_words}\nexpected: {:?}\ndecoded:  {decoded:?}",
        manifest.keyed_texts[0]
    );
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p manta-testkit -p manta-cli farnsworth v10_ -- --nocapture`
Expected: `farnsworth_stretches_only_gaps_not_marks` and `v10_passes_end_to_end_from_wav` both pass.

- [ ] **Step 7: Run full workspace test + clippy**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean, everything still passes (this is the first full-workspace run since Task 2, catches any missed `SignalSpec` site or regression).

- [ ] **Step 8: Commit**

```bash
git add crates/manta-testkit/src/keyer.rs crates/manta-testkit/src/scene.rs crates/manta-testkit/src/vectors.rs crates/manta-cli/tests/golden_v7_v9_v10.rs
git commit -m "test(testkit): Farnsworth timing support + V10 golden vector (SPEC §7)"
```

---

### Task 11: V2 un-ignore; pins 9/10 tolerance re-measurement; warmup-floor CER fixes

**Files:**
- Modify: `crates/manta-cli/tests/golden_v2_v3.rs`
- Modify: `crates/manta-cli/tests/golden_v1.rs`
- Modify: `crates/manta-engine/tests/pipeline.rs`
- Modify: `crates/manta-engine/tests/roundtrip_iq.rs`
- Modify: `crates/manta-engine/tests/regression_char_gap_high_wpm.rs`
- Modify: `crates/manta-cli/tests/cli.rs`

**Interfaces:** none new — this task only adjusts test assertions/scene durations against the now-real detector.

**Scope addition (discovered during Task 7, not known when this task was originally planned):**
Task 7's wiring exposed that SPEC §2.1's 750-hop (2.0 s) mandatory track-creation warmup —
absent from the old placeholder detector — deterministically loses a leading prefix of any
scene's decoded text. This is real, SPEC-compliant, already-precedented behavior (`track.rs`'s
`active_track_decodes_real_text` asserts `CER < 0.02` for exactly this reason, for a 120 s
scene), not a bug — but it breaks two different categories of pre-existing test, neither of
which this task's original scope anticipated:

1. **Exact-equality CER assertions** (`assert_eq!(cer(...), 0.0, ...)`) on scenes that ARE long
   enough to decode, but lose their opening `"CQ "` or similar to the warmup+confirm window
   (~2.05 s: 750 warmup hops + ~19 confirm hops + EMA settle). Affects
   `pipeline.rs::v1_lite_decodes_end_to_end` (20 s scene), `golden_v1.rs::v1_passes_end_to_end_from_wav`
   (120 s scene), `golden_v2_v3.rs::v2_passes_end_to_end_from_wav` (90 s scene, Step 1 below),
   `cli.rs::gen_then_decode_prints_text`. Each scene's CER floor is duration-dependent (the same
   ~2 s absolute loss is a larger fraction of a shorter scene) — do not copy one number across
   files; measure each independently, same methodology as Steps 2-3 below use for freq/WPM:
   run the test, read the actual measured CER from the failure output, and set a tolerance with
   a small margin above the measured floor (matching `track.rs`'s own `< 0.02` precedent's
   margin-over-floor ratio as a rough guide, not a value to copy verbatim).
2. **Scenes too short to ever produce output** — a scene whose total duration is under the
   ~2.05 s warmup+confirm floor never promotes a track at all (`decode_samples` returns an
   error or empty text, not a CER problem). Affects `regression_char_gap_high_wpm.rs` (its
   "AB"-at-33WPM scene is `keyed_length + 1.5 s ≈ 2.1 s`, too close to the floor) and
   `roundtrip_iq.rs`'s proptest generator (scenes are `keyed_length + 1.5 s`, frequently under
   2.05 s for short generated text — confirmed failing case: `wpm=10, "AO", snr=15,
   offset=0kHz`). Fix by extending scene duration with margin (e.g. add a fixed lead-in/pad
   comfortably over 2.05 s, or raise the proptest generator's minimum duration floor) —
   preserve each test's original intent (char-gap-at-high-WPM regression coverage;
   WPM/SNR/offset round-trip coverage), don't just pad arbitrarily.

Do this warmup-floor work (both categories, all 6 files) as **Step 0**, before Steps 1-4 below
(which retain their original freq/WPM scope). Steps 1 and 2 below will need re-running after
Step 0's CER/duration fixes land, since Step 1's V2 un-ignore and Step 2's freq re-measurement
both currently assume the old warmup-unaware test bodies.

- [ ] **Step 0: Fix the warmup-floor CER/duration issues across all 6 files above**, per the
  two categories described above. For each file: run its current test, capture the actual
  failure (CER value, or "no signal"/error), apply the appropriate fix (tolerance adjustment
  with measured margin, or duration extension with margin), re-run to confirm it now passes,
  and leave a short comment citing this exact situation (SPEC §2.1 warmup floor, not a bug) —
  do not silently loosen without a comment explaining why, matching this project's established
  convention (see `track.rs`'s own `active_track_decodes_real_text` doc comment for the model
  to follow). `roundtrip_iq.rs` is a proptest — after adjusting its duration floor, let it run
  its full default case count, don't reduce it.

- [ ] **Step 1: Un-ignore V2 and measure**

In `crates/manta-cli/tests/golden_v2_v3.rs`, remove the `#[ignore]` attribute and the long doc comment above `v2_passes_end_to_end_from_wav` (both were pinned specifically to the placeholder-detector limitation this sub-project fixes — pin 8 in `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`). Replace with a short comment: `// Un-ignored: SPEC §2's real hysteresis-gated detector (M2 sub-project 2) fixes the near-channel-edge decode degradation pin 7/8 tracked. See docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md.`

Run: `cargo test -p manta-cli v2_passes_end_to_end_from_wav -- --nocapture`

If it passes: leave it un-ignored, done. If it still fails: do not re-add `#[ignore]` silently — this is a real, expected-to-be-fixed regression per the design doc's premise; treat a continued failure as a bug in Tasks 4-8 to diagnose (likely the gate hysteresis isn't actually filtering the near-edge transient the way pin 7's diagnosis predicted), not a new tolerance to widen. Stop and report if it doesn't resolve after investigation.

- [ ] **Step 2: Re-measure V1's freq/WPM tolerances**

In `crates/manta-cli/tests/golden_v1.rs`, temporarily tighten the two M2-sub-project-1 tolerances back to SPEC's original values to measure current behavior:
- Freq error: change `<= 25.0` to `<= 10.0` (SPEC's original).
- WPM: change `< 3.0` to `< 2.0` (the "free" sanity check's original margin, pin 10).

Run: `cargo test -p manta-cli v1_passes_end_to_end_from_wav -- --nocapture` and note the actual measured error/WPM-delta from the assertion failure message (if it fails).

- [ ] **Step 3: Repeat for `pipeline.rs` and `roundtrip_iq.rs`**

In `crates/manta-engine/tests/pipeline.rs`'s `v1_lite_decodes_end_to_end`, tighten `<= 25.0` to `<= 10.0` and `< 3.0` to `< 2.0`, same as Step 2. Run: `cargo test -p manta-engine --test pipeline -- --nocapture`.

In `crates/manta-engine/tests/roundtrip_iq.rs`'s `prop_assert!((report.freq_hz - offset_khz as f64 * 1000.0).abs() <= 25.0)`, tighten to `<= 10.0`. Run: `cargo test -p manta-engine --test roundtrip_iq -- --nocapture` (this is a proptest over 10-40 WPM / offsets — let it run its full default case count, do not reduce it).

- [ ] **Step 4: Record the outcome**

Whatever the three tightened runs show, keep whichever tolerance actually passes reliably (re-run each failing/borderline case 3× to rule out proptest flakiness, same diligence as pin 12's flaky-proptest note) — either fully reverted to SPEC's original 10 Hz/±2 WPM, partially tightened (e.g. 15 Hz), or left at the current 25 Hz/±3 WPM with a measured explanation of why the real detector didn't fully close the gap. This outcome becomes a new pinned decision, written in Task 12's close-out doc — do not leave the three files' comments referencing the now-obsolete "placeholder detector" reasoning; update each tightened/reverted assertion's inline comment to state the new measured value and point at the new pins doc instead.

- [ ] **Step 5: Run workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/manta-cli/tests/golden_v2_v3.rs crates/manta-cli/tests/golden_v1.rs crates/manta-engine/tests/pipeline.rs crates/manta-engine/tests/roundtrip_iq.rs
git commit -m "test: un-ignore V2, re-measure and tighten freq/WPM tolerances under the real detector"
```

---

### Task 12: Full-workspace verification + close-out docs

**Files:**
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Create: `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`
- Modify: `wiki/pages/detector-tracks.md`

**Interfaces:** none — documentation and final verification only.

- [ ] **Step 1: Full workspace verification**

Run, in order:
```bash
cargo fmt --all --check
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all clean. This is the actual CI gate (per this repo's own repeated pin about per-crate clippy runs being insufficient) — do not report done until this full sequence passes with nothing skipped.

- [ ] **Step 2: Write the close-out pins doc**

Create `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`, following the exact structure of `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md` (numbered deviations/decisions list). At minimum, record:
1. `DetectorConfig.track_cap` — not in SPEC §9's literal `[detector]` table; sourced from ARCHITECTURE §4's "default 500", added as a deliberate config field.
2. `CENTER_EMA_ALPHA`'s final tuned value and the reasoning/measurement from Task 9's V9 empirical-tuning step (or "0.01 as designed, no retuning needed" if it worked first try — record that outcome either way).
3. The exact outcome of Task 11's freq/WPM tolerance re-measurement across all three files (fully reverted / partially tightened / unchanged-with-explanation), with the measured numbers.
4. V9's staircase-drift rendering approximation (`render_v9_drift`) as a testkit deviation from a true continuous linear-drift NCO — note it as a candidate for a real drift primitive in `render_scene` if a future vector needs finer drift-rate resolution.
5. Confirm (or correct) the design doc's decision to defer V8/V8w — restate that decision here as the sub-project's actual, executed scope boundary.

- [ ] **Step 3: Update ROADMAP.md**

In `ROADMAP.md`'s M2 section, replace the "Remaining M2 sub-projects: detector/track manager..., decoder pool, SoapySDR input, KiwiSDR input" sentence with something reflecting the merge, e.g.: "M2 sub-project 2 (detector/track manager + decoder pool, `manta-dsp::floor` + `manta-engine::track`) is complete — see `docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md` and `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`. Remaining M2 sub-projects: V8/V8w pileup-scene validation + CPU-budget criterion bench, SoapySDR input, KiwiSDR input. M2 itself is not yet complete."

- [ ] **Step 4: Update CLAUDE.md's Status section**

In `CLAUDE.md`, update the `## Status` paragraph to reflect sub-project 2's completion, following this repo's existing style (one or two sentences, pointing at the new pins doc, not restating detail). Keep the file under the user's global ~100-line CLAUDE.md discipline — check current line count with `wc -l CLAUDE.md` before and after; trim elsewhere in the same edit if the addition would push it over.

- [ ] **Step 5: Update `wiki/pages/detector-tracks.md`**

This page currently describes SPEC §2 in the future/normative tense as if unimplemented. Update its frontmatter `verified.commit`/`verified.date` to this sub-project's final commit/date, and adjust any phrasing that implied the placeholder detector was still current (re-read the file first — per this project's wiki convention, the wiki points at docs/code, it doesn't restate; likely only the frontmatter and maybe one sentence need touching, not a rewrite).

- [ ] **Step 6: Commit**

```bash
git add docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md ROADMAP.md CLAUDE.md wiki/pages/detector-tracks.md
git commit -m "docs: M2 sub-project 2 close-out - detector/track manager/decoder pool implemented, pinned decisions"
```

- [ ] **Step 7: Push and mark the PR ready for review**

```bash
git push --force-with-lease origin feat/m2-pfb-channelizer
gh pr ready 20
```

Per this repo's PR-hygiene convention: mark ready for review once work is actually finished, not left in draft.

---

## Plan Self-Review Notes

- **Spec coverage:** §2.1 (Task 2), §2.2 (Task 2), §2.3 (Task 3), §2.4 (Task 4), §2.5 (Task 5), decoder pool/ARCHITECTURE §10 (Task 6), batch wiring (Task 7), streaming wiring (Task 8), V7/V9 (Task 9), V10 + Farnsworth (Task 10), V2 fix + pins 9/10 (Task 11), close-out (Task 12). V8/V8w and the CPU-budget bench are explicitly out of scope per the design doc — no task covers them, by design.
- **Type consistency:** `TrackManager::new`'s final signature (Task 6: `(n_channels, fs, center_freq_hz, detector_cfg, decode_cfg)`) supersedes Task 5's interim `(n_channels, detector_cfg)` — Task 6 explicitly calls out updating the Task 5 tests to the new signature. `process_hops`/`finish` are only introduced in Task 6 and used consistently from Task 7 onward. `event_track_id`/`event_sample_ts` are defined once in Task 6 and reused (not redefined) in Task 7.
- **No placeholders:** every task's code is complete and compiles against the interfaces defined in earlier tasks; Task 10's Step 3 (compiler-driven `SignalSpec` literal sweep) is a deliberate, precise mechanical procedure rather than a hand-enumerated list of 15 near-duplicate edits, not a vague "fix the rest."
