//! Band co-modulation classifier, SPEC §2.8 (MAN-313, MAN-372).
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
//! MAN-372 adds the speech-band marking: a hot region wide enough to be
//! a band whose aggregate envelope breathes at speech rate (1-5 Hz), not
//! keying rate (8-30 Hz), is marked the same way — the band-level
//! evidence that closes the co-modulation span's coverage gaps (a voice
//! harmonic sitting in the band's internal formant gap correlates with
//! nothing the span rules see).
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
/// The upper lag bound spans a whole voice band: the testkit voice's
/// 2.6 kHz harmonic comb is 28 channels wide at 96 kHz, and only
/// cross-band partners (beyond 16) mark its span from the first
/// evaluation (MAN-372). Still far above a keyed tone's ±2-channel
/// click skirt, which the `COMOD_LAG_MIN` floor already excludes.
pub const COMOD_LAG_MAX: usize = 30;
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
/// Hops per second: the channelizer's hop rate (4 x its channel spacing,
/// 93.75 Hz). Every `COMOD_*` constant above already assumes it.
pub const COMOD_HOPS_PER_S: f64 = 375.0;
/// MAN-372 measurement: hot runs this many cold channels apart are one
/// speech-band region (the span-run tolerance `evaluate` already uses).
pub const COMOD_SPEECH_GAP: usize = 2;
/// MAN-372 (SPEC §2.8 E): a speech-band region holds at least this many
/// hot channels. Measured against the testkit voice (seed 1) at +30 dB:
/// the tightest kill region -- the surviving track at k 445, qualifying
/// through D' at hop 1763 -- sits in a 3-hot region (k 445..447, ratio
/// 2.53), while every real-CW keep's region never exceeds 2 hot
/// channels (the coexistence CW at k 459 with its skirt; the CW-only
/// track; every V8w pileup signal). 3 is the only integer between.
pub const COMOD_SPEECH_MIN_HOT: usize = 3;
/// MAN-372 (SPEC §2.8 E): a speech band's aggregate envelope must carry
/// at least this much more 1-5 Hz (speech: phrase pauses, syllables)
/// than 8-30 Hz (CW keying: 8 elements/s at 20 WPM to ~30 Hz at 35 WPM)
/// DFT magnitude. Measured (testkit voice, seed 1, +30 dB): the kill
/// regions' ratios are 2.53 (hop 1763, 3 hot) and 3.59 (hop 713, 11
/// hot); the binding keep -- a +45 dB CW's own click-skirt region --
/// measures 1.92 (20 WPM) though a harder one, VR3 at 30 WPM, reaches
/// 2.92 on its own: the ratio alone does not separate, which is why the
/// `COMOD_SPEECH_MIN_WIDTH`/`COMOD_SPEECH_MAX_GAP` structure clauses
/// below exist. 2.2 sits near the measured interval's midpoint.
pub const COMOD_SPEECH_RATIO: f64 = 2.2;
/// MAN-372 (SPEC §2.8 E): a speech-shaped region this wide (first to
/// last, inclusive) is a band on its own. Measured: the voice's
/// sub-band regions at the kill evaluations are 10-11 channels wide;
/// every control's speech-shaped region (V8w pileup coincidences, the
/// VR3 carrier's skirt) stays at most 7 wide. 9 maximizes the measured
/// margin.
pub const COMOD_SPEECH_MIN_WIDTH: usize = 9;
/// MAN-372 (SPEC §2.8 E): a narrow speech-shaped region still marks
/// when another speech-shaped region lies this close (edge to edge) --
/// a voice band's sub-bands (its strong lower harmonics and its upper
/// harmonic run, split by the internal formant gap) sit 3-15 channels
/// apart. Measured: the closest two speech-shaped regions ever come in
/// a control scene is 128 channels (V8w). 30 is the module's existing
/// whole-voice-band scale (`COMOD_LAG_MAX`).
pub const COMOD_SPEECH_MAX_GAP: usize = 30;

/// MAN-372 diagnostics: one hot region's band-level speech features
/// (see `CoModDetector::diag_speech_regions`).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiagSpeechRegion {
    /// Inclusive FFT-order channel range of the region.
    pub first_k: usize,
    pub last_k: usize,
    /// Hot channels (window mean >= `COMOD_HOT_DB`) inside the range.
    pub hot: usize,
    /// Aggregate-envelope DFT magnitude summed over 1-5 Hz (speech:
    /// phrase pauses and syllables).
    pub low: f64,
    /// Aggregate-envelope DFT magnitude summed over 8-30 Hz (CW keying:
    /// 8 elements/s at 20 WPM up to ~30 Hz at 35 WPM).
    pub high: f64,
}

impl DiagSpeechRegion {
    /// Speech-rate dominance over keying-rate energy: `low / high`.
    #[doc(hidden)]
    pub fn ratio(&self) -> f64 {
        self.low / self.high.max(1e-12)
    }
}

/// MAN-372: one hot region's band-level features, in frequency order
/// (internal; the `#[doc(hidden)]` diagnostics expose FFT order).
struct SpeechRegion {
    first: usize,
    last: usize,
    hot: usize,
    low: f64,
    high: f64,
}

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
    /// MAN-372: per frequency-order channel: marked while
    /// `hop < speech_until` (a speech band covers it).
    speech_until: Vec<u64>,
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
            speech_until: vec![0; n],
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

    /// MAN-372 (SPEC §2.8 E): whether FFT-order channel `k` lies inside
    /// a marked speech band -- a hot region wide enough to be a band
    /// whose aggregate envelope breathes at speech rate, not keying
    /// rate. Like `in_span`, this is re-judged at every evaluation from
    /// the current window (never a sticky per-track taint) and held
    /// `COMOD_HOLD_HOPS`.
    pub fn in_speech_band(&self, k: usize) -> bool {
        self.speech_until[self.j(k)] > self.hop
    }

    /// MAN-372 diagnostics: window mean, peak and partner count of
    /// FFT-order channel `k` at the latest evaluation.
    #[doc(hidden)]
    pub fn diag_channel(&self, k: usize) -> (f64, f64, u16) {
        let j = self.j(k);
        (self.mean[j], self.peak[j], self.count[j])
    }

    /// MAN-372 diagnostics: runs of consecutive hot channels (window mean
    /// >= `COMOD_HOT_DB`), inclusive FFT-order channel ranges, at the
    /// latest evaluation.
    #[doc(hidden)]
    pub fn diag_hot_runs(&self) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut hot = (0..self.n)
            .filter(|&j| self.mean[j] >= COMOD_HOT_DB)
            .map(|j| self.un_j(j));
        if let Some(first) = hot.next() {
            let mut run = (first, first);
            for k in hot {
                if k == run.1 + 1 {
                    run.1 = k;
                } else {
                    runs.push(run);
                    run = (k, k);
                }
            }
            runs.push(run);
        }
        runs
    }

    fn un_j(&self, j: usize) -> usize {
        (j + self.n / 2) % self.n
    }

    /// MAN-372 diagnostics: the current evaluation's hot regions with
    /// their band-level speech features -- maximal runs of hot channels
    /// bridged across gaps of at most `COMOD_SPEECH_GAP` cold channels,
    /// each with its hot-channel count and the modulation spectrum of its
    /// aggregate envelope (the sum of its hot channels' ring rows,
    /// explicit index-order `f64` DFT, no dependency). Computed from the
    /// same `mean`/`ring` state `evaluate` holds, so what this reports is
    /// what `evaluate`'s speech-band marking judged.
    #[doc(hidden)]
    pub fn diag_speech_regions(&self) -> Vec<DiagSpeechRegion> {
        self.speech_regions()
            .into_iter()
            .map(|r| DiagSpeechRegion {
                first_k: self.un_j(r.first),
                last_k: self.un_j(r.last),
                hot: r.hot,
                low: r.low,
                high: r.high,
            })
            .collect()
    }

    /// MAN-372 diagnostics: currently marked speech bands as inclusive
    /// frequency-order channel ranges, lowest first (like `spans`).
    #[doc(hidden)]
    pub fn diag_speech_bands(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut run: Option<(usize, usize)> = None;
        for j in 0..self.n {
            if self.speech_until[j] > self.hop {
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

    /// MAN-372: the current evaluation's hot regions (frequency order),
    /// maximal runs of hot channels bridged across gaps of at most
    /// `COMOD_SPEECH_GAP` cold channels, each with its hot-channel count
    /// and the modulation spectrum of its aggregate envelope (the sum of
    /// its hot channels' ring rows, explicit index-order `f64` DFT over
    /// 1-5 Hz and 8-30 Hz at `COMOD_HOPS_PER_S`).
    fn speech_regions(&self) -> Vec<SpeechRegion> {
        let n = self.n;
        let hot: Vec<bool> = (0..n).map(|j| self.mean[j] >= COMOD_HOT_DB).collect();
        // Maximal hot runs, bridged across <= COMOD_SPEECH_GAP cold
        // channels (the tolerance `evaluate`'s span runs already use).
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut run: Option<(usize, usize)> = None;
        for (j, &h) in hot.iter().enumerate() {
            if !h {
                continue;
            }
            run = Some(match run {
                Some((a, last)) if j - last <= COMOD_SPEECH_GAP + 1 => (a, j),
                Some(r) => {
                    ranges.push(r);
                    (j, j)
                }
                None => (j, j),
            });
        }
        if let Some(r) = run {
            ranges.push(r);
        }
        let w = self.filled;
        ranges
            .into_iter()
            .filter(|&(a, b)| hot[a..=b].iter().filter(|&&h| h).count() >= 2)
            .map(|(a, b)| {
                let mut count = 0;
                let mut env = vec![0.0f64; w];
                for (j, &h) in hot.iter().enumerate().take(b + 1).skip(a) {
                    if !h {
                        continue;
                    }
                    count += 1;
                    for (t, &v) in self.row(j).iter().enumerate() {
                        env[t] += v as f64;
                    }
                }
                let mut low = 0.0f64;
                let mut high = 0.0f64;
                for f in 1u32..=30 {
                    let mut re = 0.0f64;
                    let mut im = 0.0f64;
                    for (t, &e) in env.iter().enumerate() {
                        let th = std::f64::consts::TAU * f64::from(f) * t as f64 / COMOD_HOPS_PER_S;
                        re += e * th.cos();
                        im -= e * th.sin();
                    }
                    let mag = (re * re + im * im).sqrt();
                    if f <= 5 {
                        low += mag;
                    } else if f >= 8 {
                        high += mag;
                    }
                }
                SpeechRegion {
                    first: a,
                    last: b,
                    hot: count,
                    low,
                    high,
                }
            })
            .collect()
    }

    /// MAN-372 diagnostics: the largest Pearson rho between FFT-order
    /// channel `k`'s window and any hot channel's window at least
    /// `COMOD_LAG_MIN` channels away, at the latest evaluation.
    #[doc(hidden)]
    pub fn diag_best_rho(&self, k: usize) -> f64 {
        let j = self.j(k);
        if self.filled < 2 {
            return 0.0;
        }
        let a = self.row(j);
        let len = a.len() as f64;
        let ma = a.iter().map(|&v| v as f64).sum::<f64>() / len;
        let na: f64 = a
            .iter()
            .map(|&v| (v as f64 - ma).powi(2))
            .sum::<f64>()
            .sqrt();
        if na <= 0.0 {
            return 0.0;
        }
        let mut best = 0.0f64;
        for s in 0..self.n {
            if s.abs_diff(j) < COMOD_LAG_MIN || self.mean[s] < COMOD_HOT_DB {
                continue;
            }
            let b = self.row(s);
            let mb = b.iter().map(|&v| v as f64).sum::<f64>() / len;
            let nb: f64 = b
                .iter()
                .map(|&v| (v as f64 - mb).powi(2))
                .sum::<f64>()
                .sqrt();
            if nb <= 0.0 {
                continue;
            }
            let dot: f64 = a
                .iter()
                .zip(b)
                .map(|(x, y)| (*x as f64 - ma) * (*y as f64 - mb))
                .sum();
            best = best.max(dot / (na * nb));
        }
        best
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
        // MAN-372 (SPEC §2.8 E): speech-band marking. A speech-shaped
        // hot region -- wide enough to be a band
        // (`COMOD_SPEECH_MIN_HOT` channels whose aggregate envelope
        // carries `COMOD_SPEECH_RATIO` times more 1-5 Hz speech than
        // 8-30 Hz keying magnitude) -- is a speech band when the region
        // is itself wide (`COMOD_SPEECH_MIN_WIDTH`) or a sibling
        // speech-shaped region sits within `COMOD_SPEECH_MAX_GAP`: a
        // voice band is a wide structure (its sub-bands split by the
        // internal formant gap), while a real CW's own carrier+skirt
        // region can look speech-shaped only in isolation. It marks
        // `[first - COMOD_SPAN_GUARD, last + COMOD_SPAN_GUARD]` until
        // `hop + COMOD_HOLD_HOPS`, exactly as a span does. Like span
        // marking this is per-evaluation evidence over the current
        // window, never a per-track taint.
        let regions = self.speech_regions();
        let shaped: Vec<bool> = regions
            .iter()
            .map(|r| {
                r.hot >= COMOD_SPEECH_MIN_HOT && r.low >= COMOD_SPEECH_RATIO * r.high.max(1e-12)
            })
            .collect();
        for (i, r) in regions.iter().enumerate() {
            if !shaped[i] {
                continue;
            }
            let wide = r.last - r.first + 1 >= COMOD_SPEECH_MIN_WIDTH;
            let sibling = regions.iter().enumerate().any(|(s, o)| {
                s != i
                    && shaped[s]
                    && (o.first > r.last && o.first - r.last - 1 <= COMOD_SPEECH_MAX_GAP
                        || r.first > o.last && r.first - o.last - 1 <= COMOD_SPEECH_MAX_GAP)
            });
            if !wide && !sibling {
                continue;
            }
            let until = self.hop + COMOD_HOLD_HOPS;
            let a = r.first.saturating_sub(COMOD_SPAN_GUARD);
            let b = (r.last + COMOD_SPAN_GUARD).min(n - 1);
            for s in &mut self.speech_until[a..=b] {
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

    /// MAN-372: 30 adjacent channels sharing one smooth speech-rate
    /// envelope (a raised cosine at 3 Hz) -- the idealized voice -- are a
    /// speech band from the first evaluation. (A binary syllable key
    /// cannot model this: a square wave's odd harmonics land in the
    /// 8-30 Hz keying band and a half-filled first window holds a single
    /// step; both measure below `COMOD_SPEECH_RATIO`. The real voice's
    /// envelope is smooth.)
    #[test]
    fn speech_block_marks_a_speech_band() {
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(21);
        let mut rng = Rng::new(7 ^ 0xb10c);
        let levels: Vec<f64> = (0..30).map(|_| 30.0 + 10.0 * rng.unit()).collect();
        // A smooth pink-weighted random process over 0.5-4.5 Hz at
        // random phases -- the voice envelope's shape: energy across the
        // whole speech-rate band, dominant at the slowest rates.
        let lines: Vec<(f64, f64)> = (1..=9)
            .map(|i| {
                let f = i as f64 * 0.5;
                (f, std::f64::consts::TAU * rng.unit())
            })
            .collect();
        let norm: f64 = lines.iter().map(|(f, _)| 1.0 / f).sum();
        let mut t = 0u64;
        let mut evals = 0;
        drive(
            &mut det,
            &mut noise,
            375 * 3,
            |row| {
                let mut env = 0.55;
                for &(f, ph) in &lines {
                    env += 0.45
                        * (1.0 / f)
                        * (std::f64::consts::TAU * f * t as f64 / 375.0 + ph).cos()
                        / norm;
                }
                for (i, &l) in levels.iter().enumerate() {
                    row[100 + i] += l * env;
                }
                t += 1;
            },
            |d| {
                evals += 1;
                // A half-filled window truncates the envelope and dips
                // its ratio below the threshold (the real +30 dB voice
                // measures 2.28 at the first evaluation but 1.4-1.6 over
                // the next three); from the first full window (filled ==
                // COMOD_WINDOW_HOPS, the 4th evaluation of this drive)
                // the band is marked at every evaluation.
                if evals < 4 {
                    return;
                }
                for j in 100..=129 {
                    assert!(d.in_speech_band(k_of(j)), "eval {evals}: j = {j}");
                }
                // The marking carries the span guard, so 99 and 130 are
                // marked too; one channel beyond each edge is not.
                assert!(d.in_speech_band(k_of(99)));
                assert!(d.in_speech_band(k_of(130)));
                assert!(!d.in_speech_band(k_of(98)));
                assert!(!d.in_speech_band(k_of(131)));
            },
        );
        assert!(evals >= 3);
        assert_eq!(det.diag_speech_bands(), vec![(99, 130)]);
    }

    /// MAN-372: twelve independently keyed Morse tones -- the pileup
    /// shape (V8w's 51 signals all stay decoded with the filter on) --
    /// never form a speech band, at any evaluation, on any channel.
    #[test]
    fn independent_morse_tones_form_no_speech_band() {
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(22);
        let mut tones = independent_tones(3);
        let mut evals = 0;
        drive(
            &mut det,
            &mut noise,
            375 * 10,
            |row| tones.iter_mut().for_each(|t| t.add(row)),
            |d| {
                evals += 1;
                for k in 0..N {
                    assert!(!d.in_speech_band(k), "eval {evals}: k = {k}");
                }
                assert!(d.diag_speech_bands().is_empty());
            },
        );
        assert!(evals > 40);
    }

    /// MAN-372: a loud tone with its click skirt (the +45 dB loud-CW
    /// gate's shape) forms no speech band: its hot region reaches the
    /// occupancy floor, so the envelope's modulation spectrum -- keying
    /// rate, not speech rate -- is what keeps it out.
    #[test]
    fn loud_tone_with_click_skirt_forms_no_speech_band() {
        let j0 = 128;
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(23);
        let mut key = Key::new(KeyKind::Morse { dit: 18 }, 8);
        let mut wide_regions = 0;
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
                for k in 0..N {
                    assert!(!d.in_speech_band(k), "k = {k}");
                }
                assert!(d.diag_speech_bands().is_empty());
                if d.diag_speech_regions()
                    .iter()
                    .any(|r| r.hot >= COMOD_SPEECH_MIN_HOT)
                {
                    wide_regions += 1;
                }
            },
        );
        // The occupancy floor alone would have fired; the ratio did not.
        assert!(wide_regions > 0);
    }

    /// MAN-372: a 9-wide hot run under one Morse key -- the weaker-
    /// neighbour gate's shape, a weak CW swallowed by a louder keyed
    /// signal's hot run -- forms no speech band.
    #[test]
    fn nine_wide_morse_run_forms_no_speech_band() {
        let first = 120;
        let mut det = CoModDetector::new(N);
        let mut noise = Rng::new(24);
        let mut key = Key::new(KeyKind::Morse { dit: 20 }, 25);
        let mut wide_regions = 0;
        drive(
            &mut det,
            &mut noise,
            375 * 6,
            |row| {
                if key.step() {
                    for slot in row.iter_mut().skip(first).take(9) {
                        *slot += 18.0;
                    }
                }
            },
            |d| {
                for k in 0..N {
                    assert!(!d.in_speech_band(k), "k = {k}");
                }
                assert!(d.diag_speech_bands().is_empty());
                if d.diag_speech_regions()
                    .iter()
                    .any(|r| r.hot >= COMOD_SPEECH_MIN_HOT)
                {
                    wide_regions += 1;
                }
            },
        );
        assert!(wide_regions > 0);
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
                |d| {
                    log.push((
                        d.count.clone(),
                        d.spans(),
                        d.diag_speech_regions(),
                        d.diag_speech_bands(),
                    ))
                },
            );
            log
        };
        let a = run();
        assert!(!a.is_empty());
        assert!(a.iter().any(|(_, s, _, _)| !s.is_empty()));
        // MAN-372: the block's hot regions are logged with their
        // features, and the whole log -- speech regions and bands
        // included -- is identical across runs.
        assert!(a.iter().any(|(_, _, r, _)| !r.is_empty()));
        assert_eq!(a, run());
    }
}
