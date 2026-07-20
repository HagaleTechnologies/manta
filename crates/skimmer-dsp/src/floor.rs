//! Per-channel order-statistic noise floor + neighborhood floor. SPEC
//! §2.1-2.2. Pure, stateful-per-channel — no track lifecycle here (that's
//! `skimmer-engine::track`).

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
/// power (`skimmer_dsp::channelizer::power_db` applied to each
/// `HopOutput.power[k]`); query `effective_floor_db` any time.
pub struct FloorBank {
    channels: Vec<ChannelFloor>,
    floor_db: Vec<f64>,
    block_floor_db: Vec<f64>,
    hop_counter: u64,
}

impl FloorBank {
    /// A fresh floor bank for `n_channels` channels, all floors starting at the histogram's minimum (-140 dBFS) until the ring warms up. SPEC §2.1.
    pub fn new(n_channels: usize) -> Self {
        let n_blocks = n_channels.div_ceil(BLOCK_CHANNELS);
        FloorBank {
            channels: (0..n_channels).map(|_| ChannelFloor::new()).collect(),
            floor_db: vec![HIST_MIN_DB; n_channels],
            block_floor_db: vec![HIST_MIN_DB; n_blocks],
            hop_counter: 0,
        }
    }

    /// Number of channels this bank was constructed for.
    pub fn n_channels(&self) -> usize {
        self.channels.len()
    }

    /// One hop's per-channel dB power. Internally decimates to 25 Hz (every
    /// 15th hop) per SPEC §2.1; cheap to call every hop.
    pub fn update(&mut self, power_db: &[f64]) {
        assert_eq!(power_db.len(), self.channels.len(), "FloorBank::update: power_db length {} does not match n_channels {}", power_db.len(), self.channels.len());
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

/// SPEC §2.3: tau=40ms at the channelizer's fixed 375 Hz hop rate ->
/// alpha = 1 - e^(-2.667/40).
const GATE_EMA_ALPHA: f64 = 0.0645;

/// Per-channel EMA-smoothed power + rise/drop hysteresis booleans. SPEC
/// §2.3. Carries **no** persistence/timing state (confirm-hop-counting,
/// hang-ms-counting) -- that's the track lifecycle's job
/// (`skimmer-engine::track`). This struct is a pure function of its own
/// per-channel EMA state.
pub struct Gate {
    smoothed_db: Vec<f64>,
    initialized: Vec<bool>,
    on_snr_db: f32,
    off_snr_db: f32,
}

impl Gate {
    /// A fresh gate for `n_channels` channels, configured with the SPEC §9
    /// `detector.on_snr_db`/`detector.off_snr_db` thresholds. All channels
    /// start uninitialized (the first `update` call seeds each channel's
    /// smoothed power directly from that hop's raw power, per SPEC §2.3).
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
        assert_eq!(power_db.len(), self.smoothed_db.len(), "Gate::update: power_db length {} does not match n_channels {}", power_db.len(), self.smoothed_db.len());
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

    /// The current EMA-smoothed power `S[k,m]` for channel `k`. SPEC §2.3.
    pub fn smoothed_db(&self, k: usize) -> f64 {
        self.smoothed_db[k]
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

    #[test]
    fn gate_ema_follows_the_configured_time_constant() {
        // Verifies S[k,m] genuinely follows the closed-form EMA recurrence
        // S_n = x - (x - S_0)*(1-alpha)^n, not an instant jump to the input.
        let bank = warmed_up_bank(1, -90.0);
        let mut gate = Gate::new(1, 6.0, 3.0);
        gate.update(&[-90.0], &bank); // hop 1: initializes S_0 = -90.0 exactly
        assert_eq!(gate.smoothed_db(0), -90.0);

        gate.update(&[-70.0], &bank); // hop 2: one EMA step toward -70.0
        let expected_s1 = -90.0 + GATE_EMA_ALPHA * (-70.0 - -90.0); // = -90 + alpha*20
        assert!(
            (gate.smoothed_db(0) - expected_s1).abs() < 1e-9,
            "after one EMA step, S={} expected {expected_s1} (must NOT have jumped straight to -70.0)",
            gate.smoothed_db(0)
        );
        assert!(
            gate.smoothed_db(0) < -85.0,
            "one EMA step at alpha={GATE_EMA_ALPHA} must still be far from the -70.0 target, got {}",
            gate.smoothed_db(0)
        );

        // Many more hops at the same target: should now have converged close to it.
        for _ in 0..500 {
            gate.update(&[-70.0], &bank);
        }
        assert!(
            (gate.smoothed_db(0) - -70.0).abs() < 0.01,
            "after 500 more hops the EMA should have converged near -70.0, got {}",
            gate.smoothed_db(0)
        );
    }
}
