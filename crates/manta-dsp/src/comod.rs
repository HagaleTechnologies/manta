//! Band co-modulation classifier, SPEC §2.8 (MAN-313).
//!
//! Speech (SSB voice) raises many adjacent channels' power together: every
//! harmonic of the voice follows the same syllable envelope. Independent CW
//! signals, even close together, key independently. Fed one hop at a time
//! with every channel's gate-smoothed SNR, the classifier keeps a
//! `COMOD_WINDOW_HOPS` ring per channel and every `COMOD_EVAL_HOPS` hops
//! counts, per hot channel, how many hot channels `COMOD_LAG_MIN..=
//! COMOD_LAG_MAX` away rise and fall with it (Pearson rho >=
//! `COMOD_RHO`). A wide run of high-count channels that holds several
//! comparably strong channels is a co-modulated span; the track manager
//! starts and qualifies no CW track inside it for `COMOD_HOLD_HOPS`.
//!
//! Lags start at 3 so a keyed tone's own key-click splatter (its +/-1 and
//! +/-2 channels share its edges) never counts. The span's "strong" test
//! uses each channel's window *peak*, not its mean, so a loud keyed tone
//! whose clicks co-modulate a skirt 30-60 dB below it is never a span: the
//! skirt channels are nowhere near the carrier's peak.
//!
//! Deterministic: no RNG, no clock; pairs iterate in frequency order and
//! every sum runs in index order with `f64` accumulators.

/// Ring length per channel: 1.000 s at 375 hops/s.
pub const COMOD_WINDOW_HOPS: usize = 375;
/// Evaluation cadence once the ring holds `COMOD_MIN_FILL_HOPS` hops.
pub const COMOD_EVAL_HOPS: u64 = 75;
/// First evaluation after this many hops (about 0.5 s).
pub const COMOD_MIN_FILL_HOPS: usize = 188;
/// A channel is hot when its window-mean SNR reaches this.
pub const COMOD_HOT_DB: f64 = 6.0;
/// Channel distances judged for co-modulation, inclusive.
pub const COMOD_LAG_MIN: usize = 3;
pub const COMOD_LAG_MAX: usize = 16;
/// Pearson rho of two hot channels' per-hop SNR at or above this counts.
pub const COMOD_RHO: f64 = 0.5;
/// A channel with at least this many co-modulated partners joins a span.
pub const COMOD_COUNT: u16 = 4;
/// A span is at least this many channels wide (first to last, inclusive).
pub const COMOD_SPAN_MIN_WIDTH: usize = 8;
/// A span needs at least this many channels in `[first - 2, last + 2]`
/// whose window peak is within `COMOD_SPAN_STRONG_DB` of that range's
/// strongest peak.
pub const COMOD_SPAN_STRONG: usize = 4;
pub const COMOD_SPAN_STRONG_DB: f64 = 10.0;
/// Context channels either side of a span for the strong test.
const COMOD_SPAN_CONTEXT: usize = 2;
/// A marked span covers `[first - COMOD_SPAN_GUARD, last + COMOD_SPAN_GUARD]`.
pub const COMOD_SPAN_GUARD: usize = 1;
/// A marked span stays marked this many hops (5 s) after its evaluation.
pub const COMOD_HOLD_HOPS: u64 = 1875;

/// The classifier. Channel positions are in frequency order internally
/// (`j = (k + n/2) mod n`, `j = n/2` is the centre); the public API takes
/// the channelizer's FFT-order `k`.
#[derive(Debug, Clone)]
pub struct CoModDetector {
    n: usize,
    /// `n * COMOD_WINDOW_HOPS`, channel-major: channel `j`'s ring is
    /// `ring[j * W .. (j + 1) * W]`, written at `pos`.
    ring: Vec<f32>,
    pos: usize,
    filled: usize,
    /// Hops pushed so far.
    hop: u64,
    /// `hop` value at which the next evaluation runs.
    next_eval: u64,
    mean: Vec<f64>,
    peak: Vec<f64>,
    count: Vec<u16>,
    /// Per frequency-order channel: marked while `hop < span_until`.
    span_until: Vec<u64>,
}

impl CoModDetector {
    /// A classifier over `n` channels.
    pub fn new(n: usize) -> Self {
        CoModDetector {
            n,
            ring: vec![0.0; n * COMOD_WINDOW_HOPS],
            pos: 0,
            filled: 0,
            hop: 0,
            next_eval: COMOD_MIN_FILL_HOPS as u64,
            mean: vec![0.0; n],
            peak: vec![0.0; n],
            count: vec![0; n],
            span_until: vec![0; n],
        }
    }

    /// Push one hop of per-channel SNR (dB, FFT order `k`). Returns whether
    /// an evaluation ran on this hop.
    pub fn push_hop(&mut self, snr_db_fft_order: &[f64]) -> bool {
        let n = self.n;
        debug_assert_eq!(snr_db_fft_order.len(), n);
        for (k, &s) in snr_db_fft_order.iter().enumerate().take(n) {
            let j = (k + n / 2) % n;
            self.ring[j * COMOD_WINDOW_HOPS + self.pos] = s as f32;
        }
        self.pos = (self.pos + 1) % COMOD_WINDOW_HOPS;
        self.filled = (self.filled + 1).min(COMOD_WINDOW_HOPS);
        self.hop += 1;
        if self.hop < self.next_eval {
            return false;
        }
        self.next_eval = self.hop + COMOD_EVAL_HOPS;
        self.evaluate();
        true
    }

    /// Co-modulated partner count of FFT-order channel `k` at the latest
    /// evaluation.
    pub fn count_at(&self, k: usize) -> u16 {
        self.count[self.j(k)]
    }

    /// Whether FFT-order channel `k` lies in a marked co-modulated span.
    pub fn in_span(&self, k: usize) -> bool {
        self.span_until[self.j(k)] > self.hop
    }

    /// Whether FFT-order channel `k`'s window co-modulates (Pearson rho >=
    /// `COMOD_RHO`) with any channel of a marked span at least
    /// `COMOD_LAG_MIN` channels away. A voice's weaker harmonics can sit
    /// outside the span its strong harmonics mark yet still follow the
    /// same syllable envelope; an independently keyed CW signal does not.
    pub fn comodulated_with_span(&self, k: usize) -> bool {
        let j = self.j(k);
        if self.filled < 2 {
            return false;
        }
        let a = self.row(j);
        let len = a.len() as f64;
        let ma = a.iter().map(|&v| v as f64).sum::<f64>() / len;
        let na = a
            .iter()
            .map(|&v| (v as f64 - ma).powi(2))
            .sum::<f64>()
            .sqrt();
        if na <= 0.0 {
            return false;
        }
        (0..self.n)
            .filter(|&s| self.span_until[s] > self.hop && s.abs_diff(j) >= COMOD_LAG_MIN)
            .any(|s| {
                let b = self.row(s);
                let mb = b.iter().map(|&v| v as f64).sum::<f64>() / len;
                let mut dot = 0.0f64;
                let mut ss = 0.0f64;
                for (&x, &y) in a.iter().zip(b) {
                    let cb = y as f64 - mb;
                    dot += (x as f64 - ma) * cb;
                    ss += cb * cb;
                }
                ss > 0.0 && dot / (na * ss.sqrt()) >= COMOD_RHO
            })
    }

    /// Currently marked spans as inclusive frequency-order channel ranges
    /// `(first_j, last_j)`, lowest first.
    pub fn spans(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut run: Option<(usize, usize)> = None;
        for j in 0..self.n {
            if self.span_until[j] > self.hop {
                run = Some(match run {
                    Some((a, _)) => (a, j),
                    None => (j, j),
                });
            } else if let Some(r) = run.take() {
                out.push(r);
            }
        }
        if let Some(r) = run {
            out.push(r);
        }
        out
    }

    fn j(&self, k: usize) -> usize {
        (k + self.n / 2) % self.n
    }

    /// The filled part of channel `j`'s ring (time order does not matter:
    /// every channel shares the same slot order).
    fn row(&self, j: usize) -> &[f32] {
        &self.ring[j * COMOD_WINDOW_HOPS..j * COMOD_WINDOW_HOPS + self.filled]
    }

    fn evaluate(&mut self) {
        let n = self.n;
        let len = self.filled as f64;
        for j in 0..n {
            let row = &self.ring[j * COMOD_WINDOW_HOPS..j * COMOD_WINDOW_HOPS + self.filled];
            let mut sum = 0.0f64;
            let mut peak = f64::NEG_INFINITY;
            for &v in row {
                sum += v as f64;
                peak = peak.max(v as f64);
            }
            self.mean[j] = sum / len;
            self.peak[j] = peak;
        }
        // Centred rows and norms for hot channels only.
        let hot: Vec<usize> = (0..n).filter(|&j| self.mean[j] >= COMOD_HOT_DB).collect();
        let mut slot = vec![usize::MAX; n];
        let mut centred: Vec<f64> = Vec::with_capacity(hot.len() * self.filled);
        let mut norm: Vec<f64> = Vec::with_capacity(hot.len());
        for (i, &j) in hot.iter().enumerate() {
            slot[j] = i;
            let m = self.mean[j];
            let mut ss = 0.0f64;
            for &v in self.row(j) {
                let c = v as f64 - m;
                centred.push(c);
                ss += c * c;
            }
            norm.push(ss.sqrt());
        }
        self.count.iter_mut().for_each(|c| *c = 0);
        let w = self.filled;
        for &ja in &hot {
            let ia = slot[ja];
            if norm[ia] <= 0.0 {
                continue;
            }
            for d in COMOD_LAG_MIN..=COMOD_LAG_MAX {
                let jb = ja + d;
                if jb >= n || slot[jb] == usize::MAX {
                    continue;
                }
                let ib = slot[jb];
                if norm[ib] <= 0.0 {
                    continue;
                }
                let a = &centred[ia * w..(ia + 1) * w];
                let b = &centred[ib * w..(ib + 1) * w];
                let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                if dot / (norm[ia] * norm[ib]) >= COMOD_RHO {
                    self.count[ja] = self.count[ja].saturating_add(1);
                    self.count[jb] = self.count[jb].saturating_add(1);
                }
            }
        }
        // Spans: runs of high-count channels, tolerating 1-channel gaps.
        let mut run: Option<(usize, usize)> = None;
        let mut runs = Vec::new();
        for j in 0..n {
            if self.count[j] < COMOD_COUNT {
                continue;
            }
            run = Some(match run {
                Some((a, last)) if j - last <= 2 => (a, j),
                Some(r) => {
                    runs.push(r);
                    (j, j)
                }
                None => (j, j),
            });
        }
        if let Some(r) = run {
            runs.push(r);
        }
        for (first, last) in runs {
            if last - first + 1 < COMOD_SPAN_MIN_WIDTH {
                continue;
            }
            let lo = first.saturating_sub(COMOD_SPAN_CONTEXT);
            let hi = (last + COMOD_SPAN_CONTEXT).min(n - 1);
            let top = (lo..=hi)
                .map(|j| self.peak[j])
                .fold(f64::NEG_INFINITY, f64::max);
            let strong = (lo..=hi)
                .filter(|&j| self.peak[j] >= top - COMOD_SPAN_STRONG_DB)
                .count();
            if strong < COMOD_SPAN_STRONG {
                continue;
            }
            let until = self.hop + COMOD_HOLD_HOPS;
            let a = first.saturating_sub(COMOD_SPAN_GUARD);
            let b = (last + COMOD_SPAN_GUARD).min(n - 1);
            for s in &mut self.span_until[a..=b] {
                *s = (*s).max(until);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 256;

    /// xorshift64*: a tiny deterministic RNG for synthetic rows.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        /// Uniform in [0, 1).
        fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Uniform in [-a, a).
        fn noise(&mut self, a: f64) -> f64 {
            (2.0 * self.unit() - 1.0) * a
        }
        /// Uniform integer in [lo, hi].
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + (self.next_u64() % u64::from(hi - lo + 1)) as u32
        }
    }

    #[derive(Clone, Copy)]
    enum KeyKind {
        /// Speech syllables: on and off runs of 31..=62 hops (3-6 Hz).
        Syllable,
        /// Morse at `dit` hops: marks of 1 or 3 dits, spaces of 1, 3 or 7.
        Morse { dit: u32 },
    }

    /// An on/off key with random run lengths.
    struct Key {
        kind: KeyKind,
        on: bool,
        left: u32,
        rng: Rng,
    }

    impl Key {
        fn new(kind: KeyKind, seed: u64) -> Self {
            Key {
                kind,
                on: false,
                left: 0,
                rng: Rng::new(seed),
            }
        }
        /// Whether the key is down on this hop.
        fn step(&mut self) -> bool {
            if self.left == 0 {
                self.on = !self.on;
                self.left = match self.kind {
                    KeyKind::Syllable => self.rng.range(31, 62),
                    KeyKind::Morse { dit } => {
                        let units = if self.on {
                            [1, 3][self.rng.range(0, 1) as usize]
                        } else {
                            [1, 1, 3, 7][self.rng.range(0, 3) as usize]
                        };
                        units * dit
                    }
                };
            }
            self.left -= 1;
            self.on
        }
    }

    /// `width` adjacent channels from `first` (frequency order) sharing one
    /// syllable envelope, each at its own on level (15-25 dB).
    struct Block {
        first: usize,
        levels: Vec<f64>,
        key: Key,
    }

    impl Block {
        fn new(first: usize, width: usize, seed: u64) -> Self {
            let mut rng = Rng::new(seed ^ 0xb10c);
            Block {
                first,
                levels: (0..width).map(|_| 15.0 + 10.0 * rng.unit()).collect(),
                key: Key::new(KeyKind::Syllable, seed),
            }
        }
        fn add(&mut self, row: &mut [f64]) {
            if self.key.step() {
                for (i, &l) in self.levels.iter().enumerate() {
                    row[self.first + i] += l;
                }
            }
        }
    }

    /// A single-channel Morse tone at `j` (frequency order).
    struct Tone {
        j: usize,
        level: f64,
        key: Key,
    }

    impl Tone {
        fn add(&mut self, row: &mut [f64]) {
            if self.key.step() {
                row[self.j] += self.level;
            }
        }
    }

    /// FFT-order channel of frequency-order channel `j`.
    fn k_of(j: usize) -> usize {
        (j + N - N / 2) % N
    }

    /// Push `hops` rows. `fill` builds each row in frequency order on top of
    /// +/-1.5 dB noise around 0 dB; `on_eval` runs after every evaluation.
    fn drive(
        det: &mut CoModDetector,
        noise: &mut Rng,
        hops: usize,
        mut fill: impl FnMut(&mut [f64]),
        mut on_eval: impl FnMut(&CoModDetector),
    ) {
        let mut row_j = vec![0.0; N];
        let mut row_k = vec![0.0; N];
        for _ in 0..hops {
            for v in row_j.iter_mut() {
                *v = noise.noise(1.5);
            }
            fill(&mut row_j);
            for (j, &v) in row_j.iter().enumerate() {
                row_k[k_of(j)] = v;
            }
            if det.push_hop(&row_k) {
                on_eval(det);
            }
        }
    }

    #[test]
    fn common_envelope_across_thirty_channels_marks_a_span() {
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(1);
        let mut block = Block::new(100, 30, 7);
        drive(&mut det, &mut noise, 375, |row| block.add(row), |_| {});
        for j in 99..=130 {
            assert!(det.in_span(k_of(j)), "j = {j} not in span");
        }
        assert!(!det.in_span(k_of(98)));
        assert!(!det.in_span(k_of(131)));
        assert!(det.count_at(k_of(115)) >= COMOD_COUNT);
        assert_eq!(det.spans(), vec![(99, 130)]);
    }

    /// Twelve Morse tones, three to six channels apart, each keyed
    /// independently at its own speed.
    fn independent_tones(seed: u64) -> Vec<Tone> {
        let mut rng = Rng::new(seed);
        let mut j = 60;
        (0..12)
            .map(|i| {
                let t = Tone {
                    j,
                    level: 20.0 + 10.0 * rng.unit(),
                    key: Key::new(
                        KeyKind::Morse {
                            dit: rng.range(14, 24),
                        },
                        seed * 100 + i,
                    ),
                };
                j += rng.range(3, 6) as usize;
                t
            })
            .collect()
    }

    #[test]
    fn independent_keyed_tones_never_reach_the_count() {
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(2);
        let mut tones = independent_tones(3);
        let tone_js: Vec<usize> = tones.iter().map(|t| t.j).collect();
        let mut evals = 0;
        let mut hot_tones = 0;
        drive(
            &mut det,
            &mut noise,
            375 * 10,
            |row| tones.iter_mut().for_each(|t| t.add(row)),
            |d| {
                evals += 1;
                hot_tones += tone_js
                    .iter()
                    .filter(|&&j| d.mean[j] >= COMOD_HOT_DB)
                    .count();
                for k in 0..N {
                    assert!(d.count_at(k) < COMOD_COUNT, "k = {k}");
                    assert!(!d.in_span(k), "k = {k}");
                }
                assert!(d.spans().is_empty());
            },
        );
        assert!(evals > 40);
        // The tones are hot, so their pairs were actually correlated.
        assert!(hot_tones > evals * 10, "{hot_tones} hot over {evals} evals");
    }

    #[test]
    fn key_click_splatter_within_two_channels_is_ignored() {
        // A tone between channels: its +/-1 neighbours carry the whole mark
        // 3 dB down (hot); its +/-2 neighbours carry 3-hop clicks on every
        // key edge.
        let j0 = 128;
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(4);
        let mut key = Key::new(KeyKind::Morse { dit: 20 }, 5);
        let mut prev = false;
        let mut since_edge = u32::MAX;
        let (mut evals, mut hot_evals) = (0, 0);
        drive(
            &mut det,
            &mut noise,
            375 * 6,
            |row| {
                let on = key.step();
                if on != prev {
                    since_edge = 0;
                }
                prev = on;
                if on {
                    row[j0] += 25.0;
                    row[j0 - 1] += 22.0;
                    row[j0 + 1] += 22.0;
                }
                if since_edge < 3 {
                    row[j0 - 2] += 12.0;
                    row[j0 + 2] += 12.0;
                }
                since_edge = since_edge.saturating_add(1);
            },
            |d| {
                evals += 1;
                if d.mean[j0 - 1] >= COMOD_HOT_DB && d.mean[j0 + 1] >= COMOD_HOT_DB {
                    hot_evals += 1;
                }
                for k in 0..N {
                    assert_eq!(d.count_at(k), 0, "k = {k}");
                }
                assert!(d.spans().is_empty());
            },
        );
        // The +/-1 neighbours are usually hot: lags below 3 are what keep
        // them out of the count, not the hot rule.
        assert!(hot_evals * 2 > evals, "{hot_evals} hot of {evals}");
    }

    #[test]
    fn loud_tone_with_click_skirt_is_not_a_span() {
        // +60 dB keyed tone; +/-1 at 53 dB, +/-2 at 38 dB, and +/-3..+/-10
        // carrying its keying 30 dB down (+/-3) falling to 60 dB down
        // (+/-10). The skirt co-modulates, but only the carrier and its
        // +/-1 neighbours peak within 10 dB of the top.
        let j0 = 128;
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(6);
        let mut key = Key::new(KeyKind::Morse { dit: 18 }, 8);
        let mut wide_counts = 0;
        drive(
            &mut det,
            &mut noise,
            375 * 6,
            |row| {
                if key.step() {
                    row[j0] += 60.0;
                    for s in [-1i64, 1] {
                        row[(j0 as i64 + s) as usize] += 53.0;
                        row[(j0 as i64 + 2 * s) as usize] += 38.0;
                        for d in 3..=10i64 {
                            let lvl = 30.0 - (d - 3) as f64 * 30.0 / 7.0;
                            row[(j0 as i64 + s * d) as usize] += lvl;
                        }
                    }
                }
            },
            |d| {
                let high = (0..N).filter(|&k| d.count_at(k) >= COMOD_COUNT).count();
                if high >= COMOD_SPAN_MIN_WIDTH {
                    wide_counts += 1;
                }
                for k in 0..N {
                    assert!(!d.in_span(k), "k = {k}");
                }
                assert!(d.spans().is_empty());
            },
        );
        // The count rule alone would have marked it; the strong rule did not.
        assert!(wide_counts > 0);
    }

    #[test]
    fn span_is_held_for_hold_hops_then_released() {
        let centre = k_of(115);
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(9);
        let mut block = Block::new(100, 30, 10);
        drive(&mut det, &mut noise, 375, |row| block.add(row), |_| {});
        assert!(det.in_span(centre));
        // The block stops; the window still co-modulates for a while, so
        // the span is refreshed at every evaluation whose centre count holds.
        let mut last_mark = 0u64;
        let total = 375 + 4 * COMOD_HOLD_HOPS as usize;
        let mut hop = 375u64;
        let mut row_j = vec![0.0; N];
        let mut row_k = vec![0.0; N];
        let mut released_at = None;
        while (hop as usize) < total {
            for v in row_j.iter_mut() {
                *v = noise.noise(1.5);
            }
            for (j, &v) in row_j.iter().enumerate() {
                row_k[k_of(j)] = v;
            }
            let evaluated = det.push_hop(&row_k);
            hop += 1;
            if evaluated && det.count_at(centre) >= COMOD_COUNT {
                last_mark = hop;
            }
            if det.in_span(centre) {
                assert!(released_at.is_none(), "re-marked at {hop}");
                assert!(hop < last_mark + COMOD_HOLD_HOPS, "held past hold at {hop}");
            } else if released_at.is_none() {
                released_at = Some(hop);
            }
        }
        assert!(last_mark >= 375);
        assert_eq!(released_at, Some(last_mark + COMOD_HOLD_HOPS));
        assert!(det.spans().is_empty());
    }

    #[test]
    fn first_evaluation_waits_for_min_fill() {
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(12);
        let mut evals = Vec::new();
        let mut row = vec![0.0; N];
        for hop in 1..=1000u64 {
            for v in row.iter_mut() {
                *v = noise.noise(1.5);
            }
            if det.push_hop(&row) {
                evals.push(hop);
            }
        }
        let want: Vec<u64> = (COMOD_MIN_FILL_HOPS as u64..=1000)
            .step_by(COMOD_EVAL_HOPS as usize)
            .collect();
        assert_eq!(evals, want);
        assert_eq!(evals[0], 188);
        assert_eq!(evals[1], 263);
    }

    #[test]
    fn classifier_is_deterministic() {
        let run = || {
            let mut det = CoModDetector::new(N);
            let mut noise = Rng::new(13);
            let mut block = Block::new(20, 30, 14);
            let mut tones = independent_tones(15);
            let mut log = Vec::new();
            drive(
                &mut det,
                &mut noise,
                375 * 4,
                |row| {
                    block.add(row);
                    tones.iter_mut().for_each(|t| t.add(row));
                },
                |d| log.push((d.count.clone(), d.spans())),
            );
            log
        };
        let a = run();
        assert!(!a.is_empty());
        assert!(a.iter().any(|(_, s)| !s.is_empty()));
        assert_eq!(a, run());
    }
}
