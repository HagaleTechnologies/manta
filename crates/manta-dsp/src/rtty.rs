//! Streaming 45.45 baud ITA2 decoder for two channelizer tone streams.
//!
//! Both polarities acquire independently. Three consecutive framed characters
//! are required before releasing text. State and acquisition buffers are bounded.

use num_complex::Complex32;

const SAMPLES_PER_BIT: f64 = 375.0 / 45.45;
const LETTERS: [char; 32] = [
    '\0', 'E', '\n', 'A', ' ', 'S', 'I', 'U', '\r', 'D', 'R', 'J', 'N', 'F', 'C', 'K', 'T', 'Z',
    'L', 'W', 'H', 'Y', 'P', 'Q', 'O', 'B', 'G', '\0', 'M', 'X', 'V', '\0',
];
const FIGURES: [char; 32] = [
    '\0', '3', '\n', '-', ' ', '\'', '8', '7', '\r', '$', '4', '\x07', ',', '!', ':', '(', '5',
    '"', ')', '2', '#', '6', '0', '1', '9', '?', '&', '\0', '.', '/', ';', '\0',
];

#[derive(Default)]
struct Framer {
    mark_run: usize,
    age: Option<usize>,
    stage: usize,
    code: u8,
    figures: bool,
    good: usize,
    pending: String,
}

impl Framer {
    fn push(&mut self, mark: bool) -> Option<char> {
        let idle = self.mark_run;
        self.mark_run = if mark { idle.saturating_add(1) } else { 0 };
        let Some(age) = self.age else {
            if !mark && idle >= SAMPLES_PER_BIT.floor() as usize {
                self.age = Some(0);
                self.stage = 0;
                self.code = 0;
            }
            return None;
        };
        let age = age + 1;
        self.age = Some(age);
        if (age as f64) < (self.stage as f64 + 0.5) * SAMPLES_PER_BIT {
            return None;
        }
        let valid = match self.stage {
            0 => !mark,
            1..=5 => {
                self.code |= u8::from(mark) << (self.stage - 1);
                true
            }
            _ => mark,
        };
        if !valid {
            self.age = None;
            self.good = 0;
            self.pending.clear();
            return None;
        }
        self.stage += 1;
        if self.stage != 7 {
            return None;
        }
        self.age = None;
        self.good = self.good.saturating_add(1);
        match self.code {
            27 => self.figures = true,
            31 => self.figures = false,
            code => {
                let c = if self.figures { FIGURES } else { LETTERS }[code as usize];
                if c != '\0' {
                    return Some(c);
                }
            }
        }
        None
    }
}

/// Decoder fed lower and upper tone channel samples at 375 samples/s.
/// Output is diagnostic text; spot validation belongs to the engine adapter.
#[derive(Default)]
pub struct RttyDemod {
    framers: [Framer; 2],
    polarity: Option<usize>,
}

impl RttyDemod {
    /// Feed one pair of channelized complex samples. At most the three
    /// acquisition characters plus the current character are returned.
    pub fn push(&mut self, low: Complex32, high: Complex32) -> String {
        let upper_mark = high.norm_sqr() > low.norm_sqr();
        let mut out = String::new();
        for (i, framer) in self.framers.iter_mut().enumerate() {
            if let Some(c) = framer.push(upper_mark ^ (i == 1)) {
                if self.polarity == Some(i) {
                    out.push(c);
                } else if self.polarity.is_none() {
                    // Acquisition is bounded even for streams of shift codes.
                    if framer.pending.len() < 128 {
                        framer.pending.push(c);
                    }
                }
            }
        }
        if self.polarity.is_none() {
            if let Some(i) = self.framers.iter().position(|f| f.good >= 3) {
                self.polarity = Some(i);
                out = std::mem::take(&mut self.framers[i].pending);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-checked ITA2 codes, independent of the testkit's encoder.
    const CODES: [u8; 17] = [14, 23, 4, 16, 1, 5, 16, 4, 9, 1, 4, 19, 27, 23, 31, 3, 19];

    fn decode(reversed: bool, noise: bool) -> String {
        let mut bits = vec![true; 3];
        for code in CODES {
            bits.extend([false, false]);
            for b in 0..5 {
                bits.extend([code & (1 << b) != 0; 2]);
            }
            bits.extend([true; 3]);
        }
        let mut demod = RttyDemod::default();
        let mut text = String::new();
        for i in 0..(bits.len() as f64 * SAMPLES_PER_BIT / 2.0).ceil() as usize {
            let mark =
                bits[((i as f64 * 2.0 / SAMPLES_PER_BIT) as usize).min(bits.len() - 1)] ^ reversed;
            let fade = if noise {
                0.6 + 0.3 * (i as f32 * 0.01).sin()
            } else {
                1.0
            };
            let n = if noise {
                0.15 * (i as f32 * 2.13).sin()
            } else {
                0.0
            };
            let low = Complex32::new(if mark { n } else { fade + n }, 0.0);
            let high = Complex32::new(if mark { fade - n } else { -n }, 0.0);
            text.push_str(&demod.push(low, high));
        }
        text
    }

    #[test]
    fn clean_signal_decodes() {
        assert_eq!(decode(false, false), "CQ TEST DE W1AW");
    }

    #[test]
    fn faded_and_noisy_signal_decodes_at_moderate_snr() {
        assert_eq!(decode(false, true), "CQ TEST DE W1AW");
    }

    #[test]
    fn reversed_polarity_detected_and_decoded() {
        assert_eq!(decode(true, false), "CQ TEST DE W1AW");
    }

    #[test]
    fn garbage_stream_never_emits() {
        let mut d = RttyDemod::default();
        for i in 0..10000 {
            let x = Complex32::new((i as f32 * 1.71).sin(), 0.0);
            let y = Complex32::new((i as f32 * 2.13).sin(), 0.0);
            assert!(d.push(x, y).is_empty());
        }
    }

    #[test]
    fn deterministic_across_runs() {
        assert_eq!(decode(false, true), decode(false, true));
    }
}
