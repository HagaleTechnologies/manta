//! The clap `value_parser` functions for `manta`'s flags and the range
//! constants they check against. MAN-548 moved them out of `main.rs`.

use manta_decode::decoder::Engine;

/// `--rbn-spotter`: the same callsign rule and uppercasing as an entry of
/// `[rbn_compare].spotters`.
pub(crate) fn parse_rbn_spotter(s: &str) -> std::result::Result<String, String> {
    manta_server::config::normalize_rbn_spotter(s)
}

pub(crate) fn parse_hpsdr_mac(text: &str) -> std::result::Result<[u8; 6], String> {
    manta_input::hpsdr::discovery::parse_mac(text).ok_or_else(|| {
        "MAC must contain twelve hexadecimal digits, for example AA:BB:CC:DD:EE:FF".to_string()
    })
}

/// Lower bound for `--capture-rate-hz`. `manta_dsp::decimate::Decimator::
/// new` rejects any target rate whose channel count (`fs_out/93.75`) is
/// below 4 (a `Channelizer` with `hop = n/4 == 0` never terminates its
/// read-advancing loop -- MAN-169 whole-branch review finding). That floor
/// alone is 4*93.75 = 375 Hz, but this constant is set well above it (same
/// 1000 Hz floor `--hpsdr-rate` already uses via `MIN_HPSDR_RATE_HZ`) as a
/// second, earlier layer of defense: rejecting a degenerate
/// `--capture-rate-hz` here, at CLI-parse time, fails before any live SDR
/// device is opened/activated and produces a clearer error message than
/// the DSP-layer construction error would.
const MIN_CAPTURE_RATE_HZ: f64 = 1_000.0;

/// Clap value parser for `--capture-rate-hz`: rejects non-finite (NaN/
/// infinity) and implausibly small values at CLI-parse time, before any
/// live SDR device is opened -- matching `parse_hpsdr_rate_hz`'s pattern.
/// The full validation (evenly divides the source's native rate by a power
/// of two, and the result is itself a valid channelizer table rate) still
/// happens later in `DecimatingSource::new`/`Decimator::new`, once the
/// source's actual native rate is known; this is a cheap, early rejection
/// of obviously-bad input (e.g. a negative or degenerately tiny value that
/// would otherwise reach `Channelizer::new` with `hop == 0` and hang).
pub(crate) fn parse_capture_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s.parse().map_err(|e| format!("{e}"))?;
    check_capture_rate_hz("--capture-rate-hz", hz)
}

/// `parse_capture_rate_hz`'s check, shared with `input.capture_rate_hz`
/// (MAN-261); `name` is the flag or config key the message cites.
pub(crate) fn check_capture_rate_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz < MIN_CAPTURE_RATE_HZ {
        return Err(format!(
            "{name} must be a finite number of Hz >= {MIN_CAPTURE_RATE_HZ}, got {hz}"
        ));
    }
    Ok(hz)
}

/// Clap value parser for `--freq-correction-ppm`: fails at CLI-parse time
/// (before opening any source) rather than deep in the pipeline, using the
/// same validation `manta_spot::calibration_factor_from_ppm` applies
/// (MAN-29 review).
pub(crate) fn parse_freq_correction_ppm(s: &str) -> std::result::Result<f64, String> {
    let ppm: f64 = s.parse().map_err(|e| format!("{e}"))?;
    check_freq_correction_ppm(ppm)
}

/// `parse_freq_correction_ppm`'s check, shared with
/// `input.freq_correction_ppm` (MAN-261). The message already names
/// `freq_correction_ppm`, so it serves both spellings unchanged.
pub(crate) fn check_freq_correction_ppm(ppm: f64) -> std::result::Result<f64, String> {
    manta_spot::calibration_factor_from_ppm(ppm).map_err(|e| e.to_string())?;
    Ok(ppm)
}

/// `Engine::Hsmm` (Task 8: `TrackDecoder::push_hop_hsmm`) is a real, fully
/// implemented and reviewed engine as of Task 11 -- it parses through like
/// `legacy`/`edge-legacy`. It remains experimental (its stage-2 gate was
/// measured and failed, docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md),
/// but that's a deployment/support-posture question for operators choosing
/// `--engine hsmm` explicitly, not a reason to reject it at the CLI.
pub(crate) fn parse_engine(s: &str) -> std::result::Result<Engine, String> {
    s.parse()
}

/// Clap value parser for `--dial-freq-hz`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, before they're baked into
/// `FixedCenterFreqSource` and silently propagate into malformed RBN/JSON
/// frequency fields (e.g. a literal `NaN`, or a "0"/`band: "unknown"` from
/// a zero or negative dial frequency).
/// `--capture-start` for `Command::Oracle`: ISO-8601 UTC (e.g.
/// "2025-11-29T00:00:00Z"), parsed to Unix epoch seconds via
/// `manta_testkit::oracle::parse_utc_timestamp` -- anchors RBN spot times
/// to the actual recording start (Codex review, PR #161).
pub(crate) fn parse_capture_start(s: &str) -> std::result::Result<i64, String> {
    manta_testkit::oracle::parse_utc_timestamp(s)
        .map_err(|e| manta_server::status_doc::escape_for_terminal(&e.to_string()))
}

// Codex review, PR #161 round 2: a negative --window-s makes run_oracle's
// s1 < s0, panicking on the iq[s0..s1] slice; zero/NaN silently produces an
// empty window; infinity decodes the whole capture per spot. Reject at the
// CLI boundary too (run_oracle itself now also validates -- see that
// function's doc comment -- but a CLI-level rejection gives a clap usage
// error instead of a bail! from inside the command's execution path).
pub(crate) fn parse_window_s(s: &str) -> std::result::Result<f64, String> {
    let window_s: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !window_s.is_finite() || window_s <= 0.0 {
        return Err(format!(
            "--window-s must be finite and positive, got {window_s}"
        ));
    }
    Ok(window_s)
}

/// MAN-198/MAN-493: `--funnel-bypass` names. Only the bypasses RBN truth
/// can feed are offered; known-frequency and known-timing are built. The
/// two keying-truth experiments (known-transitions, known-transcript)
/// are engine-API/example-only and keep exiting 2 here.
pub(crate) fn parse_funnel_bypass(
    s: &str,
) -> std::result::Result<manta_engine::funnel::Bypass, String> {
    match s {
        "known-frequency" => Ok(manta_engine::funnel::Bypass::KnownFrequency),
        "known-timing" => Ok(manta_engine::funnel::Bypass::KnownTiming),
        _ => Err(format!(
            "unknown bypass `{s}`: expected known-frequency or known-timing"
        )),
    }
}

/// MAN-198: the oracle's `--funnel-*` tolerances: finite and non-negative.
pub(crate) fn parse_funnel_tol(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("must be finite and non-negative, got {s}"));
    }
    Ok(v)
}

/// MAN-309: `--sample-rate-hz` for headerless IQ: finite, above 0 and at
/// most `MAX_IQ_WAV_RATE_HZ`, before any rate-sized allocation.
pub(crate) fn parse_iq_sample_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if !hz.is_finite() || hz <= 0.0 || hz > manta_input::MAX_IQ_WAV_RATE_HZ {
        return Err(format!(
            "--sample-rate-hz must be above 0 and at most {} Hz",
            manta_input::MAX_IQ_WAV_RATE_HZ as u64
        ));
    }
    Ok(hz)
}

pub(crate) fn parse_dial_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s.parse().map_err(|e| format!("{e}"))?;
    check_dial_freq_hz("--dial-freq-hz", hz)
}

// MAN-127: `calibrate`'s frequency/ppm flags.
pub(crate) fn parse_reference_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    check_dial_freq_hz("--reference-hz", hz)
}

pub(crate) fn parse_tune_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    check_dial_freq_hz("--tune-hz", hz)
}

pub(crate) fn parse_search_ppm(s: &str) -> std::result::Result<f64, String> {
    use manta_engine::calibrate::{MAX_SEARCH_PPM, MIN_SEARCH_PPM};
    let ppm: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    if !ppm.is_finite() || !(MIN_SEARCH_PPM..=MAX_SEARCH_PPM).contains(&ppm) {
        return Err(format!(
            "--search-ppm must be between {MIN_SEARCH_PPM} and {MAX_SEARCH_PPM}, got {s}"
        ));
    }
    Ok(ppm)
}

/// `decode --center-freq-hz` (MAN-131). The parse error is just the
/// `ParseFloatError` text: clap already prints the flag and the value.
pub(crate) fn parse_center_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    check_dial_freq_hz("--center-freq-hz", hz)
}

/// `parse_dial_freq_hz`'s check, shared with `input.center_freq_hz`
/// (MAN-261) and `decode --center-freq-hz` (MAN-131); `name` is the flag
/// or config key the message cites.
pub(crate) fn check_dial_freq_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "{name} must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Lower bound for `--hpsdr-rate-hz`: comfortably below every real HPSDR/
/// Hermes sample rate (48 kHz-1.536 MHz) while still guaranteeing
/// `GapDetector::new`'s `Duration::from_secs_f64(126.0 / sample_rate_hz)`
/// (126 = `USB_FRAMES_PER_PACKET * samples_per_usb_frame(1)`, this CLI's
/// fixed single-DDC case) stays far inside `Duration`'s representable range
/// -- a finite, positive but tiny rate like `1e-20` still overflows it and
/// panics (round-2 review finding: the round-1 fix rejected NaN/inf/<=0 but
/// not an unrealistically small positive value).
const MIN_HPSDR_RATE_HZ: f64 = 1_000.0;

/// Upper bound for `--hpsdr-rate-hz`: generous headroom above any real
/// HPSDR/Hermes rate, purely to keep the range symmetric and reject
/// obviously-wrong input (e.g. a value with stray zeros) rather than to
/// pin an exact hardware ceiling this CLI layer has no authority over.
const MAX_HPSDR_RATE_HZ: f64 = 10_000_000.0;

/// Default HPSDR/Hermes control port for `[input] type = "hpsdr"`
/// (MAN-261); pinned to manta-input's own constant below.
pub(crate) const HPSDR_CONTROL_PORT: u16 = 1024;

const _: () = assert!(HPSDR_CONTROL_PORT == manta_input::hpsdr::CONTROL_PORT);

/// Clap value parser for `--hpsdr-rate-hz`: rejects non-finite (NaN/infinity)
/// and out-of-range values at CLI-parse time. `HpsdrConfig::validate`'s own
/// `validate_ddc_config` bandwidth check silently passes a NaN rate
/// (comparisons against NaN are always false), and the value then reaches
/// `GapDetector::new`'s `Duration::from_secs_f64(samples_per_packet as f64
/// / sample_rate_hz)`, which panics on NaN or an unrepresentable Duration
/// -- caught here instead, before any source is opened, matching
/// `parse_dial_freq_hz`'s pattern.
pub(crate) fn parse_hpsdr_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s.parse().map_err(|e| format!("{e}"))?;
    check_hpsdr_rate_hz("--hpsdr-rate-hz", hz)
}

/// `parse_hpsdr_rate_hz`'s check, un-gated so `[input] type = "hpsdr"`
/// validates identically on every build (MAN-261).
pub(crate) fn check_hpsdr_rate_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || !(MIN_HPSDR_RATE_HZ..=MAX_HPSDR_RATE_HZ).contains(&hz) {
        return Err(format!(
            "{name} must be a finite number of Hz between {MIN_HPSDR_RATE_HZ} and \
             {MAX_HPSDR_RATE_HZ}, got {hz}"
        ));
    }
    Ok(hz)
}

/// Clap value parser for `--hpsdr-freq-hz`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, matching
/// `parse_dial_freq_hz`'s pattern (round-2 review finding: an unvalidated
/// `--hpsdr-freq-hz NaN`/`inf` reaches `HpsdrConfig.center_freq_hz`, which
/// is only length-checked, not value-checked, and then propagates into
/// every emitted spot's frequency field).
pub(crate) fn parse_hpsdr_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s.parse().map_err(|e| format!("{e}"))?;
    check_hpsdr_freq_hz("--hpsdr-freq-hz", hz)
}

/// Clap value parser for `--ka9q-group`: the same check `input.group` gets
/// at load (MAN-140 D16), an IPv4 multicast literal or a host name, never
/// resolved here.
pub(crate) fn parse_ka9q_group(s: &str) -> std::result::Result<String, String> {
    manta_input::ka9q::check_group(s)?;
    Ok(s.to_string())
}

/// `parse_hpsdr_freq_hz`'s check, un-gated like `check_hpsdr_rate_hz`.
pub(crate) fn check_hpsdr_freq_hz(name: &str, hz: f64) -> std::result::Result<f64, String> {
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "{name} must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Upper bound for `--replay-epoch`: 2100-01-01T00:00:00Z in Unix seconds.
/// No real recording needs an epoch beyond this; the bound exists purely
/// to keep `secs` far away from the range where `SpotBus::unix_ts_for`'s
/// `epoch + elapsed` (`SystemTime` arithmetic) could overflow and panic on
/// the first spot delivered to any client (round-9 review finding) --
/// generous, not tight, since the actual overflow point depends on the
/// platform's `SystemTime` representation and isn't worth pinning exactly.
const MAX_REPLAY_EPOCH_SECS: i64 = 4_102_444_800;

/// Clap value parser for `--replay-epoch`: Unix seconds, bounded to a
/// plausible calendar range (non-negative, before `MAX_REPLAY_EPOCH_SECS`)
/// -- a `SystemTime` before `UNIX_EPOCH` isn't representable via the
/// `UNIX_EPOCH + Duration` construction this flag feeds, and an
/// unrealistically large value risks overflowing later `SystemTime`
/// arithmetic instead of failing cleanly here. Deliberately a plain
/// integer, not RFC3339 or similar -- avoids pulling in a date/time-
/// parsing dependency for one CLI flag; any real timestamp source (a
/// recording tool's own metadata, `date +%s`) can produce Unix seconds
/// directly.
pub(crate) fn parse_replay_epoch(s: &str) -> std::result::Result<i64, String> {
    let secs: i64 = s.parse().map_err(|e| format!("{e}"))?;
    check_replay_epoch("--replay-epoch", secs)
}

/// `parse_replay_epoch`'s check, shared with `input.replay_epoch`
/// (MAN-261); `name` is the flag or config key the message cites.
pub(crate) fn check_replay_epoch(name: &str, secs: i64) -> std::result::Result<i64, String> {
    if !(0..=MAX_REPLAY_EPOCH_SECS).contains(&secs) {
        return Err(format!(
            "{name} must be Unix seconds between 0 and {MAX_REPLAY_EPOCH_SECS} \
             (2100-01-01), got {secs}"
        ));
    }
    Ok(secs)
}

pub(crate) fn parse_prior(
    value: &str,
) -> std::result::Result<manta_decode::config_file::PriorSwitch, String> {
    match value {
        "on" => Ok(manta_decode::config_file::PriorSwitch::On),
        "off" => Ok(manta_decode::config_file::PriorSwitch::Off),
        _ => Err("prior must be on or off".into()),
    }
}
