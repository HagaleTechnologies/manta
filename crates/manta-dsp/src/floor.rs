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
/// SPEC §2.2 "Startup", MAN-115: for a channel's first 25 ring pushes (1 s
/// at 25 Hz decimation) after its block's leading silence, its effective
/// floor is never below its block's seed. A 1-4 entry ring's lower
/// quartile is just its minimum, which sits far below the true floor on
/// chi-squared noise and would let noise pass `rise` almost everywhere;
/// the seed is what makes a sub-second warm-up safe
/// (docs/DECISIONS/2026-10-11-man115-startup-latency.md).
const SEED_PUSHES: usize = 25;
/// SPEC §2.2 "Startup", MAN-115: the seed is the running mean of the
/// block median of raw power minus 10*log10(ln 2 / ln(4/3)) ~ 3.8 dB, the
/// median-to-lower-quartile gap of a chi-squared 2-DOF power, so it
/// estimates the same quantity the ring converges to.
const SEED_OFFSET_DB: f64 = 3.8;

/// SPEC §2.3: tau=40ms at the channelizer's fixed 375 Hz hop rate ->
/// alpha = 1 - e^(-2.667/40).
const GATE_EMA_ALPHA: f64 = 0.0645;

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
    /// MAN-115: bottom-bin entries pushed before the block's first
    /// non-silent push (a source opening with digital silence) still in
    /// the ring. The ring is FIFO, so they are always its oldest entries.
    lead_silent: u16,
}

impl ChannelFloor {
    fn new() -> Self {
        ChannelFloor {
            ring: [0; RING_LEN],
            hist: [0; HIST_BINS],
            ring_pos: 0,
            filled: 0,
            lead_silent: 0,
        }
    }

    fn push(&mut self, power_db: f64) {
        let bin = bin_index(power_db);
        if self.filled == RING_LEN {
            let evict_bin = self.ring[self.ring_pos] as usize;
            self.hist[evict_bin] = self.hist[evict_bin].saturating_sub(1);
            // While any leading silence is left it is the oldest entry.
            if evict_bin == 0 {
                self.lead_silent = self.lead_silent.saturating_sub(1);
            }
        } else {
            self.filled += 1;
        }
        self.ring[self.ring_pos] = bin as u16;
        self.hist[bin] = self.hist[bin].saturating_add(1);
        self.ring_pos = (self.ring_pos + 1) % RING_LEN;
    }

    /// SPEC §2.1: 25th percentile via cumulative histogram scan. Startup
    /// (ring partially filled): quantile is taken over whatever's present,
    /// less `skip` bottom-bin entries.
    fn quantile_db(&self, q: f64, skip: usize) -> f64 {
        let n = self.filled - skip;
        if n == 0 {
            return HIST_MIN_DB;
        }
        let target = ((q * n as f64).ceil() as usize).max(1);
        let mut cum = 0usize;
        for (b, &c) in self.hist.iter().enumerate() {
            cum += if b == 0 {
                c as usize - skip
            } else {
                c as usize
            };
            if cum >= target {
                return HIST_MIN_DB + (b as f64 + 0.5) * BIN_WIDTH_DB;
            }
        }
        HIST_MIN_DB + (HIST_BINS as f64 - 0.5) * BIN_WIDTH_DB
    }

    fn floor_db(&self) -> f64 {
        self.quantile_db(FLOOR_QUANTILE, 0)
    }

    /// MAN-115: the lower quartile without the leading silence, or the raw
    /// one when the ring holds nothing else. Never below `floor_db`.
    fn usable_floor_db(&self) -> f64 {
        let skip = self.lead_silent as usize;
        if skip == self.filled {
            self.floor_db()
        } else {
            self.quantile_db(FLOOR_QUANTILE, skip)
        }
    }

    /// Ring entries pushed since the block's leading silence ended.
    fn usable_len(&self) -> usize {
        self.filled - self.lead_silent as usize
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
    /// MAN-115: `floor_db` without each ring's leading silence; equal to
    /// it once that silence has left the ring. `block_floor_db` is the
    /// block median of these.
    usable_floor_db: Vec<f64>,
    block_floor_db: Vec<f64>,
    hop_counter: u64,
    /// Per-hop EMA (tau = 40 ms, same alpha as Gate) of each channel's
    /// LINEAR power, kept here so the decoder's spectral noise reference
    /// (SPEC v2 §2.2) needs no extra per-channel state.
    ///
    /// Codex review, PR #161 round 12: SPEC v2 §2.1-2.2 defines `P_s` as
    /// an EMA of LINEAR channel power, not dB. Averaging dB values
    /// directly understates exactly the transient this reference exists
    /// to catch: a one-hop rise from -90 to -30 dB averages to about
    /// -86 dB in dB-space (`GATE_EMA_ALPHA` is small, so the dB-space
    /// mean barely moves) but about -42 dB in linear-power space -- the
    /// correct answer, since a linear-power EMA is dominated by the
    /// large new value the way real leakage power actually behaves.
    /// Stored in linear units; `spectral_reference_db` converts the
    /// selected minimum back to dB when read.
    smoothed_lin: Vec<f64>,
    smoothed_init: bool,
    /// MAN-115: per-block sum of the block median of raw power over the
    /// first `SEED_PUSHES` decimated hops, and how many were summed.
    seed_sum_db: Vec<f64>,
    seed_pushes: Vec<u32>,
}

/// `10^(db/10)`, the inverse of `manta_dsp::channelizer::power_db`'s
/// `10*log10(power)` (minus that function's epsilon, negligible outside
/// the extreme floor).
fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 10.0)
}

impl FloorBank {
    /// A fresh floor bank for `n_channels` channels, all floors starting at
    /// the histogram's minimum (-140 dBFS) until the first `update`. For
    /// the first second after each block's leading silence,
    /// `effective_floor_db` is held at or above the block-median seed
    /// while the rings fill. SPEC §2.1-2.2.
    pub fn new(n_channels: usize) -> Self {
        let n_blocks = n_channels.div_ceil(BLOCK_CHANNELS);
        FloorBank {
            channels: (0..n_channels).map(|_| ChannelFloor::new()).collect(),
            floor_db: vec![HIST_MIN_DB; n_channels],
            usable_floor_db: vec![HIST_MIN_DB; n_channels],
            block_floor_db: vec![HIST_MIN_DB; n_blocks],
            hop_counter: 0,
            smoothed_lin: vec![db_to_linear(HIST_MIN_DB); n_channels],
            smoothed_init: false,
            seed_sum_db: vec![0.0; n_blocks],
            seed_pushes: vec![0; n_blocks],
        }
    }

    /// Number of channels this bank was constructed for.
    pub fn n_channels(&self) -> usize {
        self.channels.len()
    }

    /// One hop's per-channel dB power. Internally decimates to 25 Hz (every
    /// 15th hop) per SPEC §2.1; cheap to call every hop.
    pub fn update(&mut self, power_db: &[f64]) {
        assert_eq!(
            power_db.len(),
            self.channels.len(),
            "FloorBank::update: power_db length {} does not match n_channels {}",
            power_db.len(),
            self.channels.len()
        );
        if self.smoothed_init {
            for (s, &p) in self.smoothed_lin.iter_mut().zip(power_db) {
                let p_lin = db_to_linear(p);
                *s += GATE_EMA_ALPHA * (p_lin - *s);
            }
        } else {
            for (s, &p) in self.smoothed_lin.iter_mut().zip(power_db) {
                *s = db_to_linear(p);
            }
            self.smoothed_init = true;
        }
        if self.hop_counter.is_multiple_of(DECIMATION_HOPS) {
            for b in 0..self.block_floor_db.len() {
                let start = b * BLOCK_CHANNELS;
                let end = (start + BLOCK_CHANNELS).min(self.channels.len());
                // A block median below the histogram's range is digital
                // silence (a source still opening), not noise: averaged in
                // dB, one -200 dB push would drag the seed ~50 dB low.
                let block_median = median(&power_db[start..end]);
                if (self.seed_pushes[b] as usize) < SEED_PUSHES && block_median >= HIST_MIN_DB {
                    self.seed_sum_db[b] += block_median;
                    self.seed_pushes[b] += 1;
                }
            }
            for (k, (ch, &p)) in self.channels.iter_mut().zip(power_db).enumerate() {
                ch.push(p);
                // A block with no seed push yet is still in leading silence.
                if bin_index(p) == 0 && self.seed_pushes[k / BLOCK_CHANNELS] == 0 {
                    ch.lead_silent += 1;
                }
            }
            for (k, ch) in self.channels.iter().enumerate() {
                self.floor_db[k] = ch.floor_db();
                self.usable_floor_db[k] = if ch.lead_silent == 0 {
                    self.floor_db[k]
                } else {
                    ch.usable_floor_db()
                };
            }
            for (b, block_floor) in self.block_floor_db.iter_mut().enumerate() {
                let start = b * BLOCK_CHANNELS;
                let end = (start + BLOCK_CHANNELS).min(self.channels.len());
                *block_floor = median(&self.usable_floor_db[start..end]);
            }
        }
        self.hop_counter += 1;
    }

    /// Raw per-channel lower quartile, before the neighborhood clamp. SPEC §2.1.
    /// Until the first update this is initialized state, not a measurement.
    pub fn channel_floor_db(&self, k: usize) -> f64 {
        self.floor_db[k]
    }

    /// Effective floor `F[k] = min(F_ch[k], F_blk[k/32] + 3dB)`. SPEC §2.2.
    /// MAN-115: while channel `k`'s ring holds fewer than `SEED_PUSHES`
    /// entries taken since its block's leading silence ended, raised to at
    /// least its block's seed (the mean block median of raw power so far,
    /// minus `SEED_OFFSET_DB`). The seed only ever raises the floor, so a
    /// wrong seed costs sensitivity for at most the first second after the
    /// leading silence and never adds tracks. `channel_floor_db` stays raw.
    ///
    /// Bottom-bin entries a ring took during that leading silence (a
    /// stream that opens with exact zeros) are left out of `F_ch` and
    /// `F_blk` here until they leave the ring; counted, they would pin the
    /// lower quartile at -140 dB. Later bottom-bin entries count as usual.
    pub fn effective_floor_db(&self, k: usize) -> f64 {
        let block = k / BLOCK_CHANNELS;
        let f = self.usable_floor_db[k].min(self.block_floor_db[block] + BLOCK_ALLOWANCE_DB);
        let pushes = self.seed_pushes[block];
        if self.channels[k].usable_len() < SEED_PUSHES && pushes > 0 {
            f.max(self.seed_sum_db[block] / pushes as f64 - SEED_OFFSET_DB)
        } else {
            f
        }
    }

    /// SPEC v2 §2.2: minimum of the 40 ms-smoothed power over the six
    /// guard-banded neighbor channels c±2..c±4 (wrapping), in dB,
    /// uncorrected for the min-of-six bias (the decoder applies it).
    pub fn spectral_reference_db(&self, c: usize) -> f64 {
        let n = self.smoothed_lin.len() as isize;
        let mut m = f64::INFINITY;
        for d in [-4isize, -3, -2, 2, 3, 4] {
            let k = (c as isize + d).rem_euclid(n) as usize;
            m = m.min(self.smoothed_lin[k]);
        }
        10.0 * m.log10()
    }
}

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
        assert_eq!(
            power_db.len(),
            self.smoothed_db.len(),
            "Gate::update: power_db length {} does not match n_channels {}",
            power_db.len(),
            self.smoothed_db.len()
        );
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
    fn raw_channel_floor_preserves_quartile_when_neighbors_lower_effective_floor() {
        let mut bank = FloorBank::new(64);
        let mut powers = vec![-100.0; 64];
        powers[31] = -40.0;
        bank.update(&powers);
        assert_eq!(bank.channel_floor_db(31), -39.75);
        assert_eq!(bank.effective_floor_db(31), -96.75);
    }

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

    /// MAN-115: one decimated push of a block where every channel but one
    /// sits at -90 dB and channel 3 reads a chi-squared-style low draw at
    /// -110 dB. A 1-entry ring's lower quartile is that draw itself, so
    /// without the seed channel 3's effective floor would be -110 dB.
    fn young_bank() -> FloorBank {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[3] = -110.0;
        bank.update(&p);
        bank
    }

    #[test]
    fn young_ring_effective_floor_is_at_least_the_block_seed() {
        let bank = young_bank();
        let seed = -90.0 - SEED_OFFSET_DB;
        let f = bank.effective_floor_db(3);
        assert!(
            f >= seed - 1e-9,
            "young-ring effective floor {f} must not sit below the block seed {seed}"
        );
        assert!(
            (f - seed).abs() < 1e-9,
            "the seed should be the binding term here, got {f} vs seed {seed}"
        );
    }

    #[test]
    fn seed_does_not_change_the_raw_channel_floor() {
        let bank = young_bank();
        // -110.0 falls in the [-110.0, -109.5) bin; its centre is -109.75.
        assert_eq!(bank.channel_floor_db(3), -109.75);
    }

    #[test]
    fn seed_stops_applying_after_seed_pushes() {
        // The block median of raw power is -90 dB on every push, so the
        // seed is -93.8 dB; channel 3 always reads -110 dB, so once the
        // seed lapses its effective floor is its own -109.75 dB quartile.
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[3] = -110.0;
        let seed = -90.0 - SEED_OFFSET_DB;
        for push in 1..=SEED_PUSHES + 1 {
            for _ in 0..DECIMATION_HOPS {
                bank.update(&p);
            }
            let f = bank.effective_floor_db(3);
            if push < SEED_PUSHES {
                assert!(
                    (f - seed).abs() < 1e-9,
                    "push {push}: expected the seed {seed}, got {f}"
                );
            } else {
                assert_eq!(f, -109.75, "push {push}: the seed must have lapsed");
            }
        }
    }

    /// MAN-115 review: a stream that opens with digital silence (exact
    /// zeros read as -200 dB) must neither drag the seed down nor end it
    /// before the channel has a second of non-silent input. Channel 3's
    /// low -110 dB draws make the seed the binding term.
    #[test]
    fn leading_digital_silence_neither_lowers_nor_ends_the_seed() {
        let mut bank = FloorBank::new(64);
        let seed = -90.0 - SEED_OFFSET_DB;
        let mut p = vec![-90.0; 64];
        p[3] = -110.0;
        // Eight silent decimated pushes (320 ms), then noise at -90 dB.
        for _ in 0..8 * DECIMATION_HOPS {
            bank.update(&[-200.0; 64]);
        }
        bank.update(&p);
        let f = bank.effective_floor_db(3);
        assert!(
            (f - seed).abs() < 1e-9,
            "the first real push sets the seed to {seed}, got {f}"
        );
        // 29 ring entries, past SEED_PUSHES but only 21 after the silence;
        // the raw quartile rank ceil(29/4) = 8 is still a silent entry.
        for _ in 0..20 * DECIMATION_HOPS {
            bank.update(&p);
        }
        assert_eq!(bank.channel_floor_db(3), -139.75);
        let f = bank.effective_floor_db(3);
        assert!(
            (f - seed).abs() < 1e-9,
            "silent ring entries keep the seed in force at {seed}, got {f}"
        );
    }

    /// MAN-115 validate R1: a bottom-bin entry in a mature ring, whether a
    /// nonzero -139.6 dB draw or exact digital silence, must not bring the
    /// startup seed (here -93.8 dB, from the first -90 dB second) back.
    #[test]
    fn later_bottom_bin_entry_does_not_revive_the_seed() {
        for low in [-139.6, -200.0] {
            let mut bank = FloorBank::new(64);
            for _ in 0..RING_LEN as u64 * DECIMATION_HOPS {
                bank.update(&[-90.0; 64]);
            }
            for _ in 0..RING_LEN as u64 * DECIMATION_HOPS {
                bank.update(&[-110.0; 64]);
            }
            let mut p = vec![-110.0; 64];
            p[3] = low;
            for _ in 0..DECIMATION_HOPS {
                bank.update(&p);
            }
            assert_eq!(
                bank.effective_floor_db(3),
                -109.75,
                "a {low} dB entry revived the startup seed"
            );
        }
    }

    /// MAN-115 validate R1: a channel that reads digital silence while the
    /// rest of its block carries noise is not in leading silence, so its
    /// seed lapses after `SEED_PUSHES` like any other channel's.
    #[test]
    fn quiet_channel_in_a_live_block_loses_the_seed_after_seed_pushes() {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[3] = -200.0;
        for _ in 0..SEED_PUSHES as u64 * DECIMATION_HOPS {
            bank.update(&p);
        }
        assert_eq!(bank.effective_floor_db(3), -139.75);
    }

    /// MAN-115 validate R1: leading silence long enough to pin the raw
    /// quartile at -140 dB leaves the effective floor on the real input
    /// once the seed lapses, and it follows that input down at once rather
    /// than holding the seed until the silence leaves the ring (10 s).
    #[test]
    fn effective_floor_follows_real_input_past_leading_silence() {
        let mut bank = FloorBank::new(64);
        for _ in 0..20 * DECIMATION_HOPS {
            bank.update(&[-200.0; 64]);
        }
        for _ in 0..SEED_PUSHES as u64 * DECIMATION_HOPS {
            bank.update(&[-90.0; 64]);
        }
        // 45 entries, quartile rank 12: the raw floor is a silent entry.
        assert_eq!(bank.channel_floor_db(3), -139.75);
        assert_eq!(bank.effective_floor_db(3), -89.75);
        for _ in 0..SEED_PUSHES as u64 * DECIMATION_HOPS {
            bank.update(&[-110.0; 64]);
        }
        assert_eq!(bank.channel_floor_db(3), -139.75);
        assert_eq!(bank.effective_floor_db(3), -109.75);
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
        assert!(
            rise[0],
            "channel 0 should meet rise (10 dB above -90 floor, on_snr=6)"
        );
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
        assert_eq!(
            last,
            (false, false),
            "4.5 dB above floor sits in the hysteresis dead band"
        );
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

    #[test]
    fn spectral_reference_is_min_over_guard_banded_neighbors() {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[10] = -30.0; // the track itself
        p[11] = -40.0; // guard cell, must be ignored
        p[12] = -70.0;
        p[13] = -85.0;
        p[14] = -80.0; // c+2..c+4
        p[8] = -60.0;
        p[7] = -95.0;
        p[6] = -75.0; // c-2..c-4
        for _ in 0..400 {
            bank.update(&p);
        } // let the EMA settle
        let r = bank.spectral_reference_db(10);
        assert!(
            (r - -95.0).abs() < 0.2,
            "expected min over {{6,7,8,12,13,14}} = -95 dB, got {r}"
        );
    }

    #[test]
    fn spectral_reference_wraps_at_band_edges() {
        let mut bank = FloorBank::new(64);
        let mut p = vec![-90.0; 64];
        p[62] = -99.0; // c = 0 -> c-2 wraps to 62
        for _ in 0..400 {
            bank.update(&p);
        }
        assert!((bank.spectral_reference_db(0) - -99.0).abs() < 0.2);
    }

    #[test]
    fn spectral_reference_ema_averages_linear_power_not_db() {
        // Codex review, PR #161 round 12: SPEC v2 §2.1-2.2 defines P_s as
        // an EMA of LINEAR channel power. A dB-space EMA badly understates
        // a transient rise: after settling at -90 dB, one hop at -30 dB
        // averages to about -86 dB in dB-space but about -42 dB in
        // linear-power space -- the correct answer, since real leakage
        // power should actually raise the reference. Bump every guard-band
        // neighbor of channel 10 identically so the min-of-six selection
        // reflects the transient directly.
        let mut bank = FloorBank::new(64);
        let steady = vec![-90.0; 64];
        for _ in 0..400 {
            bank.update(&steady);
        }
        let mut bumped = steady.clone();
        for &k in &[6usize, 7, 8, 12, 13, 14] {
            bumped[k] = -30.0;
        }
        bank.update(&bumped);
        let r = bank.spectral_reference_db(10);
        assert!(
            (r - -42.0).abs() < 1.0,
            "expected the linear-power EMA answer (~-42 dB) after a one-hop transient across \
             every guard-band neighbor, got {r}"
        );
    }
}
