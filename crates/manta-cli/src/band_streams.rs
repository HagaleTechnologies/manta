//! MAN-310: how wide one stream of a band plan is, per receiver type. The
//! planner (`manta_server::band_plan`) takes the usable width as an
//! argument; the DSP and input constants it derives from live here, in the
//! crate that depends on both.

use crate::config::InputKind;

/// One stream's width, Hz: a KiwiSDR channel's IQ passband, or an HPSDR/
/// SoapySDR stream's `rate_hz`; either capped by `capture_rate_hz`.
pub(crate) fn stream_width_hz(
    kind: InputKind,
    rate_hz: Option<f64>,
    capture_rate_hz: Option<f64>,
) -> f64 {
    let (lo, hi) = manta_input::kiwi::IQ_PASSBAND_HZ;
    let width = match (kind, rate_hz) {
        (InputKind::Kiwi, _) | (_, None) => hi - lo,
        (_, Some(rate)) => rate,
    };
    capture_rate_hz.map_or(width, |c| width.min(c))
}

/// The part of a stream a plan skims: the decimator's <= 1 dB trusted band
/// (MAN-196), on both sides of the centre.
pub(crate) fn usable_width_hz(width_hz: f64) -> f64 {
    2.0 * manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * width_hz
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_server::band_plan;

    #[test]
    fn band_plan_beacons_are_the_calibrate_beacons() {
        let plan: Vec<f64> = band_plan::CW_SEGMENTS
            .iter()
            .filter_map(|s| s.beacon_hz)
            .collect();
        let calibrate: Vec<f64> = manta_engine::calibrate::REFERENCES
            .iter()
            .filter(|r| r.kind == manta_engine::calibrate::ReferenceKind::Beacon)
            .map(|r| r.hz)
            .collect();
        assert_eq!(plan, calibrate);
    }

    #[test]
    fn split_guard_is_one_channel() {
        assert_eq!(
            band_plan::SPLIT_GUARD_HZ,
            manta_dsp::channelizer::CHANNEL_SPACING_HZ
        );
    }

    #[test]
    fn usable_width_is_the_trusted_fraction() {
        assert_eq!(
            usable_width_hz(96_000.0),
            2.0 * manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 96_000.0
        );
    }

    #[test]
    fn stream_width_follows_the_receiver_type() {
        assert_eq!(stream_width_hz(InputKind::Kiwi, None, None), 10_000.0);
        assert_eq!(
            stream_width_hz(InputKind::Hpsdr, Some(192_000.0), None),
            192_000.0
        );
        assert_eq!(
            stream_width_hz(InputKind::Hpsdr, Some(192_000.0), Some(48_000.0)),
            48_000.0
        );
        assert_eq!(
            stream_width_hz(InputKind::Kiwi, None, Some(48_000.0)),
            10_000.0
        );
    }
}
