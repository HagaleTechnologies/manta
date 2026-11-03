//! MAN-323 D7: cut a window's 12 kHz audio into FT8 (15 s) or FT4 (7.5 s)
//! slots on UTC boundaries.
//!
//! The slicer knows the UTC instant of its stream's sample 0 (the session
//! epoch the recording and the bus already use) and the demodulator's
//! group delay. Audio sample `m` stands for `epoch + m / 12000 -
//! group_delay`, so the slot starting at boundary `B` begins at audio index
//! `round((B - epoch) * 12000) + round(group_delay * 12000)`. Boundaries are
//! every UTC instant divisible by the slot length, in integer milliseconds
//! from the Unix epoch, and indices are integers, so the cut is
//! reproducible. The slot in progress at startup is reported
//! [`SlotOutcome::Incomplete`] and never decoded; a discontinuity drops the
//! slot it breaks and advances the clock by the gap; missing samples
//! (MAN-305) are zero-filled inside the slot.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{FtxMode, AUDIO_RATE_HZ};

/// Whether a slot reached the decoder whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotOutcome {
    /// Every sample of the slot arrived: hand it to the decoder.
    Complete,
    /// The stream did not deliver the slot whole (startup, a gap, the end).
    /// `audio` is empty.
    Incomplete,
}

/// One slot of audio, sample 0 on its UTC boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// The slot's UTC start, milliseconds since the Unix epoch.
    pub start_ms: i64,
    /// `mode.slot_samples()` samples when complete, empty otherwise.
    pub audio: Vec<f32>,
    pub outcome: SlotOutcome,
}

/// Slices one window's audio stream into UTC-aligned slots.
#[derive(Clone, Debug)]
pub struct CycleSlicer {
    mode: FtxMode,
    epoch_ns: i128,
    delay_samples: i64,
    /// Index of the next audio sample, counting gaps.
    pos: i64,
    /// Start of the slot being filled, ms since the Unix epoch.
    cur_start_ms: i64,
    /// Audio index of that slot's first sample.
    cur_first: i64,
    /// Whether `buf` holds every sample from `cur_first` to `pos`.
    whole: bool,
    buf: Vec<f32>,
    /// Gap time not yet a whole audio sample, in input samples, so
    /// rounding never accumulates across gaps (as `GapScaler` does for
    /// decimated sources).
    gap_carry: u64,
}

impl CycleSlicer {
    /// A slicer for `mode` whose audio sample 0 is input sample 0 at UTC
    /// `epoch`, behind a demodulator with `group_delay_s` of delay.
    pub fn new(mode: FtxMode, epoch: SystemTime, group_delay_s: f64) -> Self {
        let epoch_ns = match epoch.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        };
        let delay_samples = (group_delay_s * f64::from(AUDIO_RATE_HZ)).round() as i64;
        let mut s = CycleSlicer {
            mode,
            epoch_ns,
            delay_samples,
            pos: 0,
            cur_start_ms: 0,
            cur_first: 0,
            whole: true,
            buf: Vec::new(),
            gap_carry: 0,
        };
        // The first boundary whose first sample the stream delivers.
        let slot = mode.slot_ms();
        let epoch_ms = epoch_ns.div_euclid(1_000_000) as i64;
        let mut start = epoch_ms.div_euclid(slot) * slot - slot;
        while s.first_index(start) < 0 {
            start += slot;
        }
        if s.first_index(start) > 0 {
            // Samples 0.. belong to the slot in progress at startup.
            start -= slot;
            s.whole = false;
        }
        s.cur_start_ms = start;
        s.cur_first = s.first_index(start);
        if s.whole {
            s.buf.reserve(mode.slot_samples());
        }
        s
    }

    /// The audio index of the first sample of the slot starting at `start_ms`.
    fn first_index(&self, start_ms: i64) -> i64 {
        let offset_ns = i128::from(start_ms) * 1_000_000 - self.epoch_ns;
        let samples =
            (offset_ns * i128::from(AUDIO_RATE_HZ) + 500_000_000).div_euclid(1_000_000_000);
        samples as i64 + self.delay_samples
    }

    /// The slicer's mode.
    pub fn mode(&self) -> FtxMode {
        self.mode
    }

    /// Index of the next audio sample, counting gaps and zero fill.
    pub fn position(&self) -> i64 {
        self.pos
    }

    fn end(&self) -> i64 {
        self.cur_first + self.mode.slot_samples() as i64
    }

    fn emit(&mut self, on_slot: &mut impl FnMut(Slot)) {
        let complete = self.whole && self.buf.len() == self.mode.slot_samples();
        let slot = Slot {
            start_ms: self.cur_start_ms,
            audio: if complete {
                std::mem::replace(&mut self.buf, Vec::with_capacity(self.mode.slot_samples()))
            } else {
                self.buf.clear();
                Vec::new()
            },
            outcome: if complete {
                SlotOutcome::Complete
            } else {
                SlotOutcome::Incomplete
            },
        };
        self.cur_start_ms += self.mode.slot_ms();
        self.cur_first += self.mode.slot_samples() as i64;
        self.whole = true;
        on_slot(slot);
    }

    /// Append demodulated audio, calling `on_slot` for every slot it ends,
    /// as soon as the slot's last sample arrives.
    pub fn push(&mut self, audio: &[f32], mut on_slot: impl FnMut(Slot)) {
        let mut i = 0;
        while i < audio.len() {
            let take = ((self.end() - self.pos) as usize).min(audio.len() - i);
            if self.whole {
                self.buf.extend_from_slice(&audio[i..i + take]);
            }
            self.pos += take as i64;
            i += take;
            if self.pos == self.end() {
                self.emit(&mut on_slot);
            }
        }
    }

    /// A stream discontinuity of `input_samples` samples at `fs` S/s: the
    /// slot it breaks is reported incomplete (as is every slot the gap
    /// spans), and the clock advances by the gap so later slots stay on
    /// their boundaries.
    pub fn gap(&mut self, input_samples: u64, fs: f64, mut on_slot: impl FnMut(Slot)) {
        // fs is a multiple of 24 000 S/s, so each audio sample is a whole
        // number of input samples; the remainder carries to the next gap.
        let per_audio = ((fs / f64::from(AUDIO_RATE_HZ)).round() as u64).max(1);
        let total = self.gap_carry + input_samples;
        self.gap_carry = total % per_audio;
        let gap = (total / per_audio) as i64;
        self.whole = false;
        self.buf.clear();
        self.pos += gap;
        while self.pos >= self.end() {
            self.emit(&mut on_slot);
        }
        self.whole = self.pos == self.cur_first;
    }

    /// `input_samples` samples the source lost but whose time passed
    /// (MAN-305): zero-filled inside the slot, so it may still decode.
    /// The tap instead runs zero IQ through its demodulator, which keeps the
    /// demodulator's decimation phase in step; this is the audio-side form.
    pub fn missing(&mut self, input_samples: u64, fs: f64, on_slot: impl FnMut(Slot)) {
        let n = (input_samples as f64 * f64::from(AUDIO_RATE_HZ) / fs).round() as usize;
        self.push(&vec![0.0; n], on_slot);
    }

    /// The stream ended: a slot it delivered part of is reported incomplete.
    pub fn finish(&mut self, mut on_slot: impl FnMut(Slot)) {
        if self.pos > self.cur_first {
            self.whole = false;
            self.emit(&mut on_slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2024-11-01T12:00:00Z, on a 15 s (and 7.5 s) boundary.
    const BOUNDARY_S: u64 = 1_730_462_400;

    fn at_ms(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn run(mode: FtxMode, epoch: SystemTime, delay_s: f64, seconds: f64) -> Vec<Slot> {
        let mut s = CycleSlicer::new(mode, epoch, delay_s);
        let n = (seconds * 12_000.0) as usize;
        // Sample value = its index, so a slot's first value names its cut point.
        let audio: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let mut slots = Vec::new();
        for chunk in audio.chunks(4_097) {
            s.push(chunk, |slot| slots.push(slot));
        }
        slots
    }

    fn assert_aligned(mode: FtxMode, slots: &[Slot], first_boundary_ms: i64) {
        let complete: Vec<&Slot> = slots
            .iter()
            .filter(|s| s.outcome == SlotOutcome::Complete)
            .collect();
        assert!(complete.len() >= 2, "{} complete slots", complete.len());
        assert_eq!(complete[0].start_ms, first_boundary_ms);
        for pair in complete.windows(2) {
            assert_eq!(pair[1].start_ms - pair[0].start_ms, mode.slot_ms());
        }
        for slot in &complete {
            assert_eq!(slot.start_ms % mode.slot_ms(), 0, "{}", slot.start_ms);
            assert_eq!(slot.audio.len(), mode.slot_samples());
        }
    }

    #[test]
    fn ft8_slots_start_on_15_second_utc_boundaries() {
        let epoch_ms = BOUNDARY_S * 1000 + 3_700;
        let slots = run(FtxMode::Ft8, at_ms(epoch_ms), 0.0, 50.0);
        assert_aligned(FtxMode::Ft8, &slots, (BOUNDARY_S * 1000 + 15_000) as i64);
        // 11.3 s to the first boundary: slot 1 begins at audio index 135 600.
        let first = slots
            .iter()
            .find(|s| s.outcome == SlotOutcome::Complete)
            .unwrap();
        assert_eq!(first.audio[0], 135_600.0);
    }

    #[test]
    fn ft4_slots_start_on_7_5_second_utc_boundaries() {
        let epoch_ms = BOUNDARY_S * 1000 + 3_700;
        let slots = run(FtxMode::Ft4, at_ms(epoch_ms), 0.0, 50.0);
        assert_aligned(FtxMode::Ft4, &slots, (BOUNDARY_S * 1000 + 7_500) as i64);
        let complete = slots
            .iter()
            .filter(|s| s.outcome == SlotOutcome::Complete)
            .count();
        // 3.8 s to the first boundary, then 46.2 s: six whole FT4 slots.
        assert_eq!(complete, 6);
        assert!(slots
            .iter()
            .any(|s| s.start_ms % 7_500 == 0 && s.start_ms % 15_000 != 0));
    }

    #[test]
    fn group_delay_shifts_the_cut_point() {
        let epoch_ms = BOUNDARY_S * 1000 + 3_700;
        let delay_s = 0.003_74;
        let d = (delay_s * 12_000.0f64).round();
        let slots = run(FtxMode::Ft8, at_ms(epoch_ms), delay_s, 50.0);
        let complete: Vec<&Slot> = slots
            .iter()
            .filter(|s| s.outcome == SlotOutcome::Complete)
            .collect();
        for slot in complete {
            let b_minus_epoch_s = (slot.start_ms - epoch_ms as i64) as f64 / 1000.0;
            let want = (b_minus_epoch_s * 12_000.0).round() + d;
            assert_eq!(f64::from(slot.audio[0]), want, "slot {}", slot.start_ms);
        }
    }

    #[test]
    fn a_discontinuity_drops_the_partial_slot_and_keeps_alignment() {
        let fs = 96_000.0;
        let epoch_ms = BOUNDARY_S * 1000; // on a boundary: slot 0 is whole
        let mut s = CycleSlicer::new(FtxMode::Ft8, at_ms(epoch_ms), 0.0);
        let mut slots = Vec::new();
        s.push(&vec![0.5; 5 * 12_000], |slot| slots.push(slot));
        assert!(slots.is_empty());
        let before = s.position();
        s.gap((4.2 * fs) as u64, fs, |slot| slots.push(slot));
        assert_eq!(
            s.position() - before,
            (4.2f64 * 12_000.0) as i64,
            "clock advanced by the gap"
        );
        // The rest of the broken slot, then a whole one.
        s.push(&vec![0.5; 25 * 12_000], |slot| slots.push(slot));
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].start_ms, (BOUNDARY_S * 1000) as i64);
        assert_eq!(slots[0].outcome, SlotOutcome::Incomplete);
        assert!(slots[0].audio.is_empty());
        assert_eq!(slots[1].start_ms, (BOUNDARY_S * 1000 + 15_000) as i64);
        assert_eq!(slots[1].outcome, SlotOutcome::Complete);
        assert_eq!(slots[1].audio.len(), 180_000);
    }

    #[test]
    fn a_gap_spanning_whole_slots_reports_each_one() {
        let fs = 48_000.0;
        let mut s = CycleSlicer::new(FtxMode::Ft4, at_ms(BOUNDARY_S * 1000), 0.0);
        let mut slots = Vec::new();
        s.push(&vec![0.1; 12_000], |slot| slots.push(slot));
        s.gap((20.0 * fs) as u64, fs, |slot| slots.push(slot));
        let starts: Vec<i64> = slots
            .iter()
            .map(|s| s.start_ms - (BOUNDARY_S * 1000) as i64)
            .collect();
        assert_eq!(starts, vec![0, 7_500]);
        assert!(slots.iter().all(|s| s.outcome == SlotOutcome::Incomplete));
        // 21 s in, the 15 s slot is partial; the 22.5 s slot is whole.
        s.push(&vec![0.1; 9 * 12_000], |slot| slots.push(slot));
        assert_eq!(slots.len(), 4);
        assert_eq!(slots[2].start_ms - (BOUNDARY_S * 1000) as i64, 15_000);
        assert_eq!(slots[2].outcome, SlotOutcome::Incomplete);
        assert_eq!(slots[3].start_ms - (BOUNDARY_S * 1000) as i64, 22_500);
        assert_eq!(slots[3].outcome, SlotOutcome::Complete);
    }

    #[test]
    fn gap_rounding_carries_between_gaps() {
        // At 96 kS/s an audio sample is 8 input samples: two 4-sample gaps
        // make one audio sample, not zero (or two).
        let fs = 96_000.0;
        let mut s = CycleSlicer::new(FtxMode::Ft8, at_ms(BOUNDARY_S * 1000), 0.0);
        s.gap(4, fs, |_| {});
        assert_eq!(s.position(), 0);
        s.gap(4, fs, |_| {});
        assert_eq!(s.position(), 1);
        for _ in 0..1000 {
            s.gap(238, fs, |_| {});
        }
        assert_eq!(s.position(), 1 + 238_000 / 8);
    }

    #[test]
    fn missing_samples_are_zero_filled_inside_the_slot() {
        let fs = 96_000.0;
        let mut s = CycleSlicer::new(FtxMode::Ft8, at_ms(BOUNDARY_S * 1000), 0.0);
        let mut slots = Vec::new();
        s.push(&vec![1.0; 6 * 12_000], |slot| slots.push(slot));
        s.missing(96_000, fs, |slot| slots.push(slot)); // 1 s lost
        s.push(&vec![1.0; 8 * 12_000], |slot| slots.push(slot));
        assert_eq!(slots.len(), 1);
        let slot = &slots[0];
        assert_eq!(slot.outcome, SlotOutcome::Complete);
        assert_eq!(slot.audio.len(), 180_000);
        assert!(slot.audio[72_000..84_000].iter().all(|&x| x == 0.0));
        assert_eq!(slot.audio[71_999], 1.0);
        assert_eq!(slot.audio[84_000], 1.0);
    }

    #[test]
    fn the_slot_in_progress_at_startup_is_skipped_and_counted_incomplete() {
        let epoch_ms = BOUNDARY_S * 1000 + 3_700;
        let mut s = CycleSlicer::new(FtxMode::Ft8, at_ms(epoch_ms), 0.0);
        let mut slots = Vec::new();
        s.push(&vec![0.0; 12 * 12_000], |slot| slots.push(slot));
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].start_ms, (BOUNDARY_S * 1000) as i64);
        assert_eq!(slots[0].outcome, SlotOutcome::Incomplete);
        // The stream ends partway through the next slot: incomplete too.
        s.finish(|slot| slots.push(slot));
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[1].start_ms, (BOUNDARY_S * 1000 + 15_000) as i64);
        assert_eq!(slots[1].outcome, SlotOutcome::Incomplete);
        // Nothing more to report.
        s.finish(|slot| slots.push(slot));
        assert_eq!(slots.len(), 2);
    }
}
