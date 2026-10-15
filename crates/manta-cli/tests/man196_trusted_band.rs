//! MAN-196 end-to-end regression gates: the research phase's probe cells
//! as permanent, deterministic tests. Each cell renders a scene at
//! 96 kS/s, pushes it through the real `manta_dsp::decimate::Decimator`
//! (96k -> 48k, one halfband stage), and runs the real decode pipeline
//! in-process via `manta_engine::listen` -- the same
//! channelizer + `TrackManager` path `manta run` uses, with the source
//! reporting the trusted band exactly as `DecimatingSource` now does.
//! The assertions restate numbers the research phase measured on this
//! codebase (MAN-313's lesson: gate on measured counts, never on "no
//! longer happens").

use manta_decode::events::DecoderEvent;
use manta_dsp::decimate::Decimator;
use manta_engine::{listen, PipelineConfig};
use manta_testkit::vectors::{render, v1};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// An in-memory `IqSource` at 48 kS/s whose optional trusted band is what
/// `DecimatingSource` would report for a 96k -> 48k wrap:
/// `Some((+-TRUSTED_HALF_BAND_FRACTION * 48_000))`, or `None` for the
/// control runs (every non-decimated source).
struct RenderedSource {
    samples: Vec<Complex32>,
    cursor: usize,
    trusted_band_hz: Option<(f64, f64)>,
}

impl manta_input::IqSource for RenderedSource {
    fn sample_rate(&self) -> f64 {
        48_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        0.0
    }
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.trusted_band_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

/// Render `spec` at 96 kS/s, decimate through the real 96k -> 48k stage,
/// and count `TrackPromoted` events over the whole decode, with the
/// source's trusted band set per `trusted`.
fn promotions_after_decimation(spec: &manta_testkit::vectors::VectorSpec, trusted: bool) -> usize {
    let rendered = render(spec).unwrap();
    assert_eq!(spec.fs, 96_000.0);
    let mut decimator = Decimator::new(96_000.0, 2).unwrap();
    let decimated = decimator.process(&rendered.samples);
    let trusted_band = trusted.then(|| {
        let half = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        (-half, half)
    });
    let src = RenderedSource {
        samples: decimated,
        cursor: 0,
        trusted_band_hz: trusted_band,
    };
    let mut promoted = 0usize;
    listen(
        Box::new(src),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        |ev| {
            if matches!(ev, DecoderEvent::TrackPromoted { .. }) {
                promoted += 1;
            }
        },
        |_| {},
    )
    .unwrap();
    promoted
}

/// The duplicate-track regression cell: a real, detectable CW signal at
/// +23.5 kHz / SNR 10 dB (the -22.8 dB point of the transition band)
/// spawns zero tracks once the trusted band is in force, where the
/// measured behaviour without it was a duplicate pair (tracks 1 and 12
/// re-decoding the same W1AW text). The control keeps >= 1, not == 2:
/// the gate pins detectability, not the duplicate itself.
#[test]
fn beyond_the_trusted_band_a_decimated_signal_spawns_no_tracks() {
    let mut spec = v1();
    spec.duration_s = 30.0;
    spec.signals[0].offset_hz = 23_500.0;
    spec.signals[0].snr_2500_db = 10.0;
    assert!(
        promotions_after_decimation(&spec, false) >= 1,
        "control (no trusted band): the signal is real and detectable -- \
         the native behaviour spawns at least one track"
    );
    assert_eq!(
        promotions_after_decimation(&spec, true),
        0,
        "with the trusted band: no track may spawn on the transition band"
    );
}

/// A steady complex tone at `offset_hz`, `amp_dbfs` dBFS, `seconds`
/// long at 96 kS/s -- the probe's carrier construction (no keying, no
/// noise; digital silence everywhere else).
fn tone_scene(offset_hz: f64, seconds: f64, amp_dbfs: f32) -> Vec<Complex32> {
    let fs = 96_000.0;
    let amp = 10f32.powf(amp_dbfs / 20.0);
    let n = (seconds * fs).round() as usize;
    (0..n)
        .map(|i| {
            let phi = 2.0 * std::f64::consts::PI * offset_hz * i as f64 / fs;
            Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
        })
        .collect()
}

/// Count `TrackPromoted` events for a raw 96 kS/s capture pushed through
/// the real 96k -> 48k decimation and the real decode pipeline, with the
/// source's trusted band set per `trusted`.
fn carrier_promotions(iq: &[Complex32], trusted: bool) -> usize {
    let mut decimator = Decimator::new(96_000.0, 2).unwrap();
    let decimated = decimator.process(iq);
    let trusted_band = trusted.then(|| {
        let half = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        (-half, half)
    });
    let src = RenderedSource {
        samples: decimated,
        cursor: 0,
        trusted_band_hz: trusted_band,
    };
    let mut promoted = 0usize;
    listen(
        Box::new(src),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        |ev| {
            if matches!(ev, DecoderEvent::TrackPromoted { .. }) {
                promoted += 1;
            }
        },
        |_| {},
    )
    .unwrap();
    promoted
}

/// The foldover cell: a steady -20 dBFS carrier at +25 kHz (above the
/// 48 kS/s output Nyquist) leaves a residue through the 96k -> 48k stage
/// that promoted a track in digital silence (measured: -100.6 dBFS
/// residue, PROMOTED, no decode). With the trusted band it promotes
/// nothing. The in-band control at +20 kHz still promotes: a carrier is
/// a signal, expected then and now.
#[test]
fn a_carrier_above_the_decimated_nyquist_promotes_nothing() {
    assert!(
        carrier_promotions(&tone_scene(20_000.0, 20.0, -20.0), true) >= 1,
        "in-band control at +20 kHz: a carrier is a signal and must still \
         promote, trusted band or not"
    );
    assert_eq!(
        carrier_promotions(&tone_scene(25_000.0, 20.0, -20.0), true),
        0,
        "a carrier above the decimated Nyquist must promote nothing once \
         the trusted band is in force (its folded residue lands beyond \
         the boundary)"
    );
}

/// The in-band foldover cell (round-1 review on PR #300): a carrier whose
/// alias folds INSIDE the trusted band lands on trusted channels the
/// spawn veto cannot see -- only the decimator's stopband (>= 80 dB by
/// design, measured >= ~-86.5 dB over the whole fold-in region; see
/// `halfband_stopband_covers_frequencies_folding_inside_the_trusted_band`)
/// stands between it and a mirrored track. Measured boundary on this
/// codebase: a -35 dBFS carrier at +26 343.75 Hz (folding to exactly
/// 21 656.25 Hz -- trusted channel 231's centre, the deterministic worst
/// case) still promotes in digital silence; at -40 dBFS its residue sits
/// below the detector's floor and promotes nothing. The >= -35 dBFS
/// window is the documented residual of the committed 80 dB stopband
/// target: closing it needs a deeper stopband (KAISER_BETA/HALFBAND_TAPS
/// redesign), which the plan's "no stopband work" scope excludes and this
/// round raised as a decision ticket instead. This cell pins the boundary
/// the committed design actually guarantees.
#[test]
fn an_in_band_foldover_residue_promotes_only_above_the_stopband_margin() {
    // The residual, pinned so the boundary is executable: a strong carrier
    // above Nyquist whose alias folds onto a trusted channel centre still
    // promotes (>= 1, measured exactly 1) -- updating this pin to 0 is
    // what landing a deeper stopband looks like.
    assert!(
        carrier_promotions(&tone_scene(26_343.75, 20.0, -20.0), true) >= 1,
        "strong in-band-folded carriers are the documented residual of the 80 dB \
         stopband target (decision ticket pending); if this stops promoting, the \
         stopband deepened -- flip this pin and update the decision record"
    );
    // The guarantee: past the design's suppression margin the folded
    // residue is below the digital-silence floor and promotes nothing.
    assert_eq!(
        carrier_promotions(&tone_scene(26_343.75, 20.0, -40.0), true),
        0,
        "a -40 dBFS carrier's folded residue (~-127 dBFS after >= ~86 dB of \
         stopband) must stay below the detector's floor: this is the level the \
         committed design honestly removes from detectability"
    );
}
