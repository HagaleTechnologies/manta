//! MAN-323: FT8/FT4 skimming from the IQ streams manta already receives.
//!
//! This module holds the decoder-independent core: the modes, their dial
//! windows (from the MAN-311 table), which windows a stream covers, the
//! streaming USB demodulator ([`demod`]) and the UTC slot slicer
//! ([`slicer`]). The decoder itself is pancetta's `pancetta-ft8`, linked only
//! with the `ftx` feature ([`new_decoder`]). Decisions are in
//! docs/DECISIONS/2026-11-01-man323-ft8-ft4-from-band-streams.md.

pub mod demod;
pub mod slicer;
pub mod tap;

use crate::digital_windows;

/// The digital modes manta decodes. The config spells them `"FT8"`/`"FT4"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FtxMode {
    Ft8,
    Ft4,
}

impl FtxMode {
    /// Both modes, in the order the startup summary reports them.
    pub const ALL: [FtxMode; 2] = [FtxMode::Ft8, FtxMode::Ft4];

    /// The wire and config spelling.
    pub fn label(self) -> &'static str {
        match self {
            FtxMode::Ft8 => "FT8",
            FtxMode::Ft4 => "FT4",
        }
    }

    /// Slot length in milliseconds: 15 s for FT8, 7.5 s for FT4.
    pub fn slot_ms(self) -> i64 {
        match self {
            FtxMode::Ft8 => 15_000,
            FtxMode::Ft4 => 7_500,
        }
    }

    /// Samples per slot at [`AUDIO_RATE_HZ`] (pancetta's cycle length).
    pub fn slot_samples(self) -> usize {
        match self {
            FtxMode::Ft8 => 180_000,
            FtxMode::Ft4 => 90_000,
        }
    }

    /// Parse the exact config spelling; anything else is `None`.
    pub fn parse(s: &str) -> Option<FtxMode> {
        match s {
            "FT8" => Some(FtxMode::Ft8),
            "FT4" => Some(FtxMode::Ft4),
            _ => None,
        }
    }

    /// The `DIALS_HZ` column this mode reads.
    fn dial_column(self) -> usize {
        match self {
            FtxMode::Ft8 => 0,
            FtxMode::Ft4 => 1,
        }
    }
}

/// One mode's window on one band: its USB dial frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FtxWindow {
    pub mode: FtxMode,
    pub dial_hz: u32,
}

impl FtxWindow {
    /// The RF span `[dial + 300, dial + 3000]` Hz the decoder searches.
    pub fn rf_span_hz(&self) -> (f64, f64) {
        let dial = f64::from(self.dial_hz);
        (dial + AUDIO_SPAN_HZ.0, dial + AUDIO_SPAN_HZ.1)
    }
}

/// The audio span above the dial a window must cover, Hz. The 300 Hz floor
/// is the sound-card input's declared passband, so rig audio tuned to the
/// FT8 dial qualifies.
pub const AUDIO_SPAN_HZ: (f64, f64) = (300.0, 3000.0);

/// The decoder's input rate (pancetta's contract: 12 kHz mono `f32`).
pub const AUDIO_RATE_HZ: u32 = 12_000;

/// Stream rates the two-stage demodulator accepts are multiples of this.
pub const RATE_STEP_HZ: u32 = 24_000;

/// Whether a stream of `fs` S/s covers a window, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// The whole span lies inside the stream's usable band: decode it.
    Inside,
    /// Part of the span lies inside: skipped.
    Partial,
    /// None of the span lies inside: skipped.
    Outside,
    /// The stream has no RF reference (`center_freq_hz() == 0`).
    NoRfReference,
    /// The rate is not a multiple of [`RATE_STEP_HZ`].
    UnsupportedRate,
}

/// Every window of `mode` in the digital-window table, low band first.
pub fn windows(mode: FtxMode) -> impl Iterator<Item = FtxWindow> {
    digital_windows::dials(mode.dial_column()).map(move |dial_hz| FtxWindow { mode, dial_hz })
}

/// Whether `fs` is a rate the demodulator accepts.
pub fn rate_supported(fs: f64) -> bool {
    fs >= f64::from(RATE_STEP_HZ)
        && fs.fract() == 0.0
        && (fs as u64).is_multiple_of(u64::from(RATE_STEP_HZ))
}

/// The usable absolute band of a stream: its centre plus its RF passband,
/// narrowed by its trusted band when it has one (both relative to centre).
pub fn usable_band_hz(
    centre_hz: f64,
    passband: (f64, f64),
    trusted: Option<(f64, f64)>,
) -> (f64, f64) {
    let (mut lo, mut hi) = passband;
    if let Some((tlo, thi)) = trusted {
        lo = lo.max(tlo);
        hi = hi.min(thi);
    }
    (centre_hz + lo, centre_hz + hi)
}

/// Whether a stream centred on `centre_hz` with the given passband and
/// trusted band (relative to centre) and rate `fs` covers window `w`.
pub fn coverage(
    w: &FtxWindow,
    centre_hz: f64,
    passband: (f64, f64),
    trusted: Option<(f64, f64)>,
    fs: f64,
) -> Coverage {
    if centre_hz == 0.0 {
        return Coverage::NoRfReference;
    }
    if !rate_supported(fs) {
        return Coverage::UnsupportedRate;
    }
    let (band_lo, band_hi) = usable_band_hz(centre_hz, passband, trusted);
    let (lo, hi) = w.rf_span_hz();
    if lo >= band_lo && hi <= band_hi {
        Coverage::Inside
    } else if hi > band_lo && lo < band_hi {
        Coverage::Partial
    } else {
        Coverage::Outside
    }
}

/// One decoded message, in manta's terms.
#[derive(Clone, Debug, PartialEq)]
pub struct FtxMessage {
    /// The message text (WSJT-X rendering), e.g. `CQ W1AW FN31`.
    pub text: String,
    /// Audio frequency above the dial, Hz.
    pub audio_hz: f64,
    /// The decoder's SNR estimate, dB in WSJT-X's 2500 Hz reference.
    pub snr_db: f32,
    /// DT: seconds after the slot boundary the signal started.
    pub dt_s: f64,
}

/// A whole-slot decoder: 12 kHz `f32` audio with sample 0 on the slot
/// boundary in, messages out. pancetta implements it with the `ftx`
/// feature; tests substitute fakes.
pub trait CycleDecode: Send {
    fn decode(&mut self, audio: &[f32]) -> anyhow::Result<Vec<FtxMessage>>;
}

/// The real decoder for `mode`: `Some` only in a build with the `ftx`
/// feature (and when pancetta constructs it).
pub fn new_decoder(mode: FtxMode) -> Option<Box<dyn CycleDecode>> {
    #[cfg(feature = "ftx")]
    {
        let protocol = match mode {
            FtxMode::Ft8 => pancetta_ft8::Protocol::Ft8,
            FtxMode::Ft4 => pancetta_ft8::Protocol::Ft4,
        };
        pancetta_ft8::CycleDecoder::new(protocol)
            .ok()
            .map(|d| Box::new(Pancetta(d)) as Box<dyn CycleDecode>)
    }
    #[cfg(not(feature = "ftx"))]
    {
        let _ = mode;
        None
    }
}

/// Whether this build links the FT8/FT4 decoder.
pub const fn decoder_compiled() -> bool {
    cfg!(feature = "ftx")
}

/// pancetta's reusable cycle decoder (the contract's reuse form).
#[cfg(feature = "ftx")]
struct Pancetta(pancetta_ft8::CycleDecoder);

#[cfg(feature = "ftx")]
impl CycleDecode for Pancetta {
    fn decode(&mut self, audio: &[f32]) -> anyhow::Result<Vec<FtxMessage>> {
        let decoded = self.0.decode_cycle(audio)?;
        Ok(decoded
            .into_iter()
            .map(|m| FtxMessage {
                text: m.text,
                audio_hz: m.frequency_offset,
                snr_db: m.snr_db,
                dt_s: m.time_offset,
            })
            .collect())
    }
}

#[cfg(all(test, feature = "ftx"))]
mod pancetta_link_tests {
    #[test]
    fn the_ft8_lib_c_decoder_is_linked_not_stubbed() {
        // pancetta's build.rs silently falls back to cfg(ft8lib_stub) when the
        // submodule is missing; that build decodes with reduced recall.
        assert!(pancetta_ft8::ft8lib_is_available());
    }

    #[test]
    fn the_adapter_constructs_both_decoders() {
        assert!(super::new_decoder(super::FtxMode::Ft8).is_some());
        assert!(super::new_decoder(super::FtxMode::Ft4).is_some());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIWI_PASSBAND: (f64, f64) = (-5000.0, 5000.0);

    fn w(mode: FtxMode, dial_hz: u32) -> FtxWindow {
        FtxWindow { mode, dial_hz }
    }

    #[test]
    fn ft8_and_ft4_dials_come_from_the_digital_window_table() {
        let ft8: Vec<u32> = windows(FtxMode::Ft8).map(|w| w.dial_hz).collect();
        assert_eq!(ft8.len(), 10);
        assert_eq!(ft8[0], 1_840_000);
        assert!(ft8.contains(&14_074_000));
        assert!(windows(FtxMode::Ft8).all(|w| w.mode == FtxMode::Ft8));
        let ft4: Vec<u32> = windows(FtxMode::Ft4).map(|w| w.dial_hz).collect();
        assert_eq!(ft4.len(), 9, "160 m has no FT4 dial");
        assert!(!ft4.contains(&1_840_000));
        assert!(ft4.contains(&14_080_000));
        assert!(ft4.contains(&7_047_500));
    }

    #[test]
    fn coverage_on_the_kiwi_20m_plan() {
        let ft8 = w(FtxMode::Ft8, 14_074_000);
        let ft4 = w(FtxMode::Ft4, 14_080_000);
        let cov =
            |win: &FtxWindow, centre: f64| coverage(win, centre, KIWI_PASSBAND, None, 96_000.0);
        assert_eq!(cov(&ft8, 14_079_167.0), Coverage::Inside);
        assert_eq!(cov(&ft4, 14_079_167.0), Coverage::Inside);
        assert_eq!(cov(&ft8, 14_070_833.0), Coverage::Partial);
        assert_eq!(cov(&ft4, 14_070_833.0), Coverage::Outside);
        assert_eq!(cov(&ft8, 14_087_500.0), Coverage::Outside);
        assert_eq!(cov(&ft4, 14_087_500.0), Coverage::Partial);
    }

    #[test]
    fn coverage_uses_the_trusted_band_when_narrower() {
        let ft8 = w(FtxMode::Ft8, 14_074_000);
        // 192 kS/s centred on 14 050 kHz covers [13 954, 14 146] kHz, but a
        // trusted band of +/-20 kHz ends at 14 070 kHz, below the FT8 span.
        let full = (-96_000.0, 96_000.0);
        assert_eq!(
            coverage(&ft8, 14_050_000.0, full, None, 192_000.0),
            Coverage::Inside
        );
        assert_eq!(
            coverage(
                &ft8,
                14_050_000.0,
                full,
                Some((-20_000.0, 20_000.0)),
                192_000.0
            ),
            Coverage::Outside
        );
        assert_eq!(
            coverage(
                &ft8,
                14_050_000.0,
                full,
                Some((-25_500.0, 25_500.0)),
                192_000.0
            ),
            Coverage::Partial
        );
    }

    #[test]
    fn zero_centre_has_no_rf_reference() {
        let ft8 = w(FtxMode::Ft8, 14_074_000);
        assert_eq!(
            coverage(&ft8, 0.0, (300.0, 3000.0), None, 48_000.0),
            Coverage::NoRfReference
        );
    }

    #[test]
    fn rig_audio_on_the_ft8_dial_covers_ft8_only() {
        let rig = (300.0, 3000.0);
        let ft8 = w(FtxMode::Ft8, 14_074_000);
        let ft4 = w(FtxMode::Ft4, 14_080_000);
        assert_eq!(
            coverage(&ft8, 14_074_000.0, rig, None, 48_000.0),
            Coverage::Inside
        );
        assert_eq!(
            coverage(&ft4, 14_074_000.0, rig, None, 48_000.0),
            Coverage::Outside
        );
    }

    #[test]
    fn rate_must_be_a_multiple_of_24_khz() {
        for fs in [48_000.0, 96_000.0, 192_000.0, 384_000.0, 2_400_000.0] {
            assert!(rate_supported(fs), "{fs}");
        }
        for fs in [250_000.0, 44_100.0, 12_000.0, 48_000.5] {
            assert!(!rate_supported(fs), "{fs}");
        }
        let ft8 = w(FtxMode::Ft8, 14_074_000);
        assert_eq!(
            coverage(&ft8, 14_050_000.0, (-125_000.0, 125_000.0), None, 250_000.0),
            Coverage::UnsupportedRate
        );
    }

    #[test]
    fn mode_constants_match_the_decoder_contract() {
        assert_eq!(
            FtxMode::Ft8.slot_samples() as i64,
            FtxMode::Ft8.slot_ms() * 12
        );
        assert_eq!(
            FtxMode::Ft4.slot_samples() as i64,
            FtxMode::Ft4.slot_ms() * 12
        );
        assert_eq!(FtxMode::parse("FT8"), Some(FtxMode::Ft8));
        assert_eq!(FtxMode::parse("FT4"), Some(FtxMode::Ft4));
        assert_eq!(FtxMode::parse("ft8"), None);
        assert_eq!(FtxMode::parse("JT65"), None);
        assert_eq!(FtxMode::ALL.map(FtxMode::label), ["FT8", "FT4"]);
    }
}
