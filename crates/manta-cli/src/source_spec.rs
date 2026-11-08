//! Live and file source flags (`KiwiOpts`, `HpsdrOpts`, ...), `LiveSourceSpec`,
//! opening and wrapping sources, and the source warnings printed at start-up.
//! MAN-548 moved them out of `main.rs`.

use crate::cli_parse::HPSDR_CONTROL_PORT;
use crate::logging;
use crate::reconnect;
use crate::resolve::Resolved;
use anyhow::{anyhow, bail, Result};
use manta_input::IqSource;
use std::path::Path;
use std::path::PathBuf;

/// KiwiSDR connection flags, grouped to keep `open_source`'s arity down.
/// `Clone`: MAN-73's `LiveSourceSpec` carries one of these so a live source
/// can be re-opened by `ReconnectingSource` after a loss.
#[derive(Clone)]
pub(crate) struct KiwiOpts {
    pub(crate) host: Option<String>,
    pub(crate) port: u16,
    pub(crate) freq: Option<f64>,
    pub(crate) password: String,
}

/// SoapySDR connection flags (feature `soapy`), grouped for the same reason.
#[cfg(feature = "soapy")]
#[derive(Clone)]
struct SoapyOpts {
    driver: Option<String>,
    freq: Option<f64>,
    rate: Option<f64>,
    gain: Option<f64>,
}

/// HPSDR/Hermes connection flags, grouped for the same reason.
#[derive(Clone)]
pub(crate) struct HpsdrOpts {
    pub(crate) host: Option<String>,
    pub(crate) mac: Option<[u8; 6]>,
    pub(crate) port: Option<u16>,
    /// `--hpsdr-protocol` or `input.protocol` (MAN-305 D2).
    pub(crate) protocol: manta_input::hpsdr::HpsdrProtocol,
    pub(crate) freq: Option<f64>,
    pub(crate) rate: Option<f64>,
    /// MAN-308: `input.auto_attenuation`; one handle shared by every
    /// reopen of this input.
    pub(crate) auto_attenuation: Option<std::sync::Arc<manta_input::hpsdr::AutoAttenuation>>,
}

impl HpsdrOpts {
    pub(crate) fn selector(&self) -> Option<&'static str> {
        [
            (self.host.is_some(), "--hpsdr-host"),
            (self.mac.is_some(), "--hpsdr-mac"),
            (self.freq.is_some(), "--hpsdr-freq-hz"),
            (self.rate.is_some(), "--hpsdr-rate-hz"),
            (self.port.is_some(), "--hpsdr-port"),
        ]
        .into_iter()
        .find_map(|(set, name)| set.then_some(name))
    }

    pub(crate) fn selected(&self) -> bool {
        self.selector().is_some()
    }

    /// Resolve the radio (discovering a hostless one) and open it, with
    /// the source's acquisition offset measured from before discovery:
    /// `ReconnectingSource` anchors the offset at entry to the whole
    /// reopen, so discovery is outage like the open's own probe (MAN-259,
    /// PR #359 review).
    pub(crate) fn open_with(
        &self,
        freq: f64,
        rate: f64,
        discover: impl FnOnce(u16, Option<[u8; 6]>) -> Result<String>,
        open: impl FnOnce(
            &manta_input::hpsdr::HpsdrConfig,
            manta_input::hpsdr::HpsdrProtocol,
            std::time::Instant,
        ) -> Result<manta_input::hpsdr::OpenedHpsdr>,
    ) -> Result<(
        manta_input::hpsdr::HpsdrConfig,
        manta_input::hpsdr::OpenedHpsdr,
    )> {
        let entered = std::time::Instant::now();
        let cfg = hpsdr_config(self, self.resolve_host_with(discover)?, freq, rate);
        let opened = open(&cfg, self.protocol, entered)?;
        Ok((cfg, opened))
    }

    pub(crate) fn resolve_host_with(
        &self,
        discover: impl FnOnce(u16, Option<[u8; 6]>) -> Result<String>,
    ) -> Result<String> {
        if let Some(host) = &self.host {
            if self.mac.is_some() {
                bail!("input.host and input.mac cannot both be set");
            }
            return Ok(host.clone());
        }
        discover(self.port.unwrap_or(HPSDR_CONTROL_PORT), self.mac)
    }
}

/// `--hpsdr-protocol` values (MAN-305 §1): `auto`, `1`, `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum HpsdrProtocolArg {
    Auto,
    #[value(name = "1")]
    One,
    #[value(name = "2")]
    Two,
}

impl From<HpsdrProtocolArg> for manta_input::hpsdr::HpsdrProtocol {
    fn from(arg: HpsdrProtocolArg) -> Self {
        match arg {
            HpsdrProtocolArg::Auto => Self::Auto,
            HpsdrProtocolArg::One => Self::One,
            HpsdrProtocolArg::Two => Self::Two,
        }
    }
}

/// ka9q-radio (radiod) multicast flags, grouped for the same reason. No
/// build feature: the source is in every build (MAN-140 D2).
#[derive(Clone)]
pub(crate) struct Ka9qOpts {
    pub(crate) group: Option<String>,
    pub(crate) port: u16,
    pub(crate) ssrc: Option<u32>,
    pub(crate) interface: Option<std::net::Ipv4Addr>,
    /// radiod's frequency at the first open, shared with the clone the
    /// reconnect opener holds, so a reopen under `--dial-freq-hz` can still
    /// see a retune (MAN-140 D13; see `LiveSourceSpec::open`).
    pub(crate) first_center_hz: std::sync::Arc<std::sync::OnceLock<f64>>,
}

impl Ka9qOpts {
    /// No `--ka9q-*` flag given: the defaults `CliOverrides::none` uses.
    pub(crate) fn unset() -> Self {
        Ka9qOpts {
            group: None,
            port: manta_input::ka9q::wire::DATA_PORT,
            ssrc: None,
            interface: None,
            first_center_hz: Default::default(),
        }
    }
}

/// Everything needed to (re)open the configured live input. MAN-73: a
/// reconnectable kind (every variant but `File`) is re-opened by
/// `ReconnectingSource` after a loss; `File` (deterministic replay) never
/// is. `name()` must keep returning the same label `set_source_health`
/// always used for that kind, since dashboards/scrapes key on it.
#[derive(Clone)]
pub(crate) enum LiveSourceSpec {
    Kiwi(KiwiOpts),
    #[cfg(feature = "soapy")]
    Soapy(SoapyOpts),
    Hpsdr(HpsdrOpts),
    Ka9q(Ka9qOpts),
    AudioDevice(Option<String>),
    /// `source_iq` selects `WavIqSource` over `AudioIqSource`; see
    /// `open_audio_source`.
    File {
        path: PathBuf,
        source_iq: bool,
        /// MAN-309: `--iq-format`/`--sample-rate-hz` for headerless IQ.
        raw: Option<RawIq>,
    },
}

/// MAN-309: how to read a headerless IQ `--source` (`--iq-format`,
/// `--sample-rate-hz`); its centre is `--dial-freq-hz`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawIq {
    pub(crate) format: manta_input::RawIqFormat,
    pub(crate) sample_rate_hz: f64,
}

impl LiveSourceSpec {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            LiveSourceSpec::Kiwi(_) => "kiwi",
            #[cfg(feature = "soapy")]
            LiveSourceSpec::Soapy(_) => "soapy",
            LiveSourceSpec::Hpsdr(_) => "hpsdr",
            LiveSourceSpec::Ka9q(_) => "ka9q",
            LiveSourceSpec::AudioDevice(_) => "audio",
            LiveSourceSpec::File { .. } => "file",
        }
    }

    /// MAN-308: the input's automatic attenuation handle, `Some` only for an
    /// hpsdr input with `input.auto_attenuation`.
    pub(crate) fn auto_attenuation(
        &self,
    ) -> Option<std::sync::Arc<manta_input::hpsdr::AutoAttenuation>> {
        match self {
            LiveSourceSpec::Hpsdr(hpsdr) => hpsdr.auto_attenuation.clone(),
            _ => None,
        }
    }

    /// MAN-234: how the bad-sample warnings name this input -- the file's
    /// path, or `the <kind> input` for a live source.
    pub(crate) fn input_label(&self) -> String {
        match self {
            LiveSourceSpec::File { path, .. } => path.display().to_string(),
            _ => format!("the {} input", self.name()),
        }
    }

    /// Kiwi/Soapy/Hpsdr/Ka9q, or an IQ WAV whose sidecar reports a real (> 0)
    /// center frequency: a source that already knows its own tuned
    /// frequency, so `--dial-freq-hz` is an override and not a requirement.
    pub(crate) fn is_rf_aware(&self) -> bool {
        match self {
            LiveSourceSpec::AudioDevice(_) => false,
            LiveSourceSpec::File {
                path,
                source_iq,
                raw,
            } => raw.is_none() && source_iq_has_real_rf_center(path, *source_iq),
            _ => true,
        }
    }

    /// Whether a loss of this source should be retried by
    /// `ReconnectingSource` (MAN-73) rather than ending the daemon. File
    /// replay's errors and EOF are deterministic and must reach `listen()`
    /// unchanged -- see AGENTS.md's byte-identical-replay requirement.
    pub(crate) fn is_reconnectable(&self) -> bool {
        !matches!(self, LiveSourceSpec::File { .. })
    }

    /// Open (or re-open) the described source, applying `maybe_decimate`
    /// for `capture_rate_hz` and then `FixedCenterFreqSource` when
    /// `dial_freq_hz` is set -- so every call through this one method
    /// produces a fully-composed source and nothing above it (in particular
    /// `ReconnectingSource`) needs to know about either wrapper. A reopen
    /// therefore also starts a fresh decimator, with no filter state carried
    /// across the outage. A reopen runs on `ReconnectingSource`'s reopen
    /// thread (MAN-252), so everything this builds must be `Send`.
    pub(crate) fn open(
        &self,
        capture_rate_hz: Option<f64>,
        dial_freq_hz: Option<f64>,
    ) -> Result<reconnect::SendSource> {
        // How far radiod has moved the channel since the first open; only
        // ka9q sets it (MAN-140 D13).
        let mut retune_hz = 0.0;
        let src: reconnect::SendSource = match self {
            LiveSourceSpec::Kiwi(kiwi) => {
                let host = kiwi
                    .host
                    .as_deref()
                    .expect("LiveSourceSpec::Kiwi always carries a host");
                let freq = kiwi
                    .freq
                    .ok_or_else(|| anyhow!("--kiwi-freq-hz is required with --kiwi-host"))?;
                Box::new(manta_input::kiwi::KiwiIqSource::connect(
                    host,
                    kiwi.port,
                    freq,
                    &kiwi.password,
                )?)
            }
            #[cfg(feature = "soapy")]
            LiveSourceSpec::Soapy(soapy) => {
                let driver = soapy
                    .driver
                    .as_deref()
                    .expect("LiveSourceSpec::Soapy always carries a driver");
                let freq = soapy
                    .freq
                    .ok_or_else(|| anyhow!("--soapy-freq-hz is required with --soapy-driver"))?;
                let rate = soapy
                    .rate
                    .ok_or_else(|| anyhow!("--soapy-rate-hz is required with --soapy-driver"))?;
                // Each (re)open gets a fresh rate-limit state, mirroring
                // the audio silence notice (MAN-233; the reconnect itself
                // already logs per-attempt).
                let warn_driver = driver.to_string();
                Box::new(
                    manta_input::soapy::SoapySdrIqSource::open(driver, rate, freq, soapy.gain)?
                        .with_overflow_notice(Box::new(move || {
                            logging::warning(&soapy_overflow_warning(&warn_driver))
                        })),
                )
            }
            LiveSourceSpec::Hpsdr(hpsdr) => {
                let freq = hpsdr
                    .freq
                    .ok_or_else(|| anyhow!("--hpsdr-freq-hz is required with the hpsdr source"))?;
                let rate = hpsdr
                    .rate
                    .ok_or_else(|| anyhow!("--hpsdr-rate-hz is required with the hpsdr source"))?;
                // MAN-306: a hostless entry discovers its radio here, on
                // each (re)open, so a radio that moved address is found again.
                let (cfg, opened) = hpsdr.open_with(
                    freq,
                    rate,
                    |port, mac| {
                        use manta_input::hpsdr::discovery::{broadcast_destinations, select_radio};
                        Ok(select_radio(port, &broadcast_destinations(port), mac)?
                            .ip
                            .to_string())
                    },
                    manta_input::hpsdr::open_receiver_since,
                )?;
                // MAN-305 D2/D17: run, listen, soak, calibrate, check and
                // doctor all open here, so each prints the same §4 line.
                log_hpsdr_open_line(&cfg.host, cfg.port, &opened);
                warn_if_attenuation_needs_protocol_2(&cfg, &opened);
                opened.source
            }
            LiveSourceSpec::Ka9q(ka9q) => {
                let group = ka9q
                    .group
                    .clone()
                    .expect("LiveSourceSpec::Ka9q always carries a group");
                let mut cfg = manta_input::ka9q::Ka9qConfig::new(group);
                cfg.port = ka9q.port;
                cfg.ssrc = ka9q.ssrc;
                cfg.interface = ka9q.interface;
                let src = manta_input::ka9q::Ka9qIqSource::open(cfg)?;
                // MAN-140 D18: printed on each (re)open, as the audio
                // silence notice is; manta still starts.
                if src.agc_on() {
                    logging::warning(&ka9q_agc_warning(src.ssrc()));
                }
                let first_hz = *ka9q.first_center_hz.get_or_init(|| src.center_freq_hz());
                retune_hz = src.center_freq_hz() - first_hz;
                Box::new(src)
            }
            // Each (re)open gets its own one-shot silence notice (MAN-131).
            LiveSourceSpec::AudioDevice(device) => Box::new(
                manta_input::AudioIqSource::from_device(device.as_deref())?.with_silence_notice(
                    Box::new(|device| logging::warning(&audio_silence_warning(device))),
                ),
            ),
            LiveSourceSpec::File {
                path,
                source_iq: true,
                raw,
            } => {
                let src = open_iq_source_file(path, *raw, dial_freq_hz)?;
                refuse_gapped_decimation(path, src.sample_rate(), capture_rate_hz)?;
                src
            }
            LiveSourceSpec::File { path, .. } => {
                open_audio_source(None, Some(path.clone()), false, None)?
            }
        };
        transform_source(src, capture_rate_hz, dial_freq_hz.map(|hz| hz + retune_hz))
    }

    /// MAN-334: the centre and RF passband `open(capture_rate_hz,
    /// dial_freq_hz)` will report, from configuration alone and without
    /// touching the receiver, so `calibrate` can refuse a passband that holds
    /// no reference before any receiver I/O (MAN-127 Decision 11). It mirrors
    /// `open`: the source's own values, then `maybe_decimate`, then the
    /// `--dial-freq-hz` override; a change to how `open` composes a source
    /// must change this too. `None` when only opening can tell -- a ka9q-radio
    /// channel (radiod reports both), a recording (its header and sidecar do)
    /// -- or when `open` itself would refuse (a missing frequency or rate, an
    /// HPSDR rate or frequency `hpsdr::validate_before_open` refuses for the
    /// configured protocol -- protocol 1's under `Auto`, which discovery
    /// resolves only by I/O -- a capture rate the source cannot decimate
    /// to), so `open` keeps reporting its own errors. KiwiSDR, HPSDR and
    /// sound-card values are exact, so the preflight refuses nothing the
    /// opened source would not. SoapySDR values
    /// are the requested ones (MAN-334 decision 4): a driver may round them,
    /// so a window the request leaves without a reference is refused even if
    /// the driver would have widened it, and a narrower negotiated window is
    /// caught by `calibrate`'s re-plan once open.
    pub(crate) fn configured_center_and_passband(
        &self,
        capture_rate_hz: Option<f64>,
        dial_freq_hz: Option<f64>,
    ) -> Option<(f64, (f64, f64))> {
        let (center_hz, rate_hz, passband) = match self {
            LiveSourceSpec::Kiwi(kiwi) => (
                kiwi.freq?,
                manta_input::kiwi::OUTPUT_RATE_HZ,
                manta_input::kiwi::IQ_PASSBAND_HZ,
            ),
            #[cfg(feature = "soapy")]
            LiveSourceSpec::Soapy(soapy) => {
                let rate = soapy.rate?;
                (soapy.freq?, rate, (-rate / 2.0, rate / 2.0))
            }
            LiveSourceSpec::Hpsdr(hpsdr) => {
                let (freq, rate) = (hpsdr.freq?, hpsdr.rate?);
                // `HpsdrDevice::open` (protocol 1) and `Protocol2IqSource::
                // open` validate the rate and frequency before their own
                // I/O; a refusal there stays `open`'s to report.
                manta_input::hpsdr::validate_before_open(
                    // Validation reads only rate and frequency: no discovery
                    // runs while planning (MAN-306).
                    &hpsdr_config(hpsdr, String::new(), freq, rate),
                    hpsdr.protocol,
                )
                .ok()?;
                (freq, rate, (-rate / 2.0, rate / 2.0))
            }
            // A sound card reports no frequency of its own: the dial is its
            // centre.
            LiveSourceSpec::AudioDevice(_) => (
                dial_freq_hz?,
                f64::from(manta_input::TARGET_RATE_HZ),
                (
                    manta_input::AUDIO_PASSBAND_LO_HZ,
                    manta_input::AUDIO_PASSBAND_HI_HZ,
                ),
            ),
            LiveSourceSpec::Ka9q(_) | LiveSourceSpec::File { .. } => return None,
        };
        let passband = match decimation_target(rate_hz, capture_rate_hz) {
            Some(target) => {
                manta_input::DecimatingSource::passband_for(rate_hz, target, passband).ok()?
            }
            None => passband,
        };
        Some((dial_freq_hz.unwrap_or(center_hz), passband))
    }
}

/// Apply identical wrappers to standalone and grouped initial/reopened sources.
pub(crate) fn transform_source(
    src: reconnect::SendSource,
    capture_rate_hz: Option<f64>,
    dial_freq_hz: Option<f64>,
) -> Result<reconnect::SendSource> {
    let src = maybe_decimate(src, capture_rate_hz)?;
    Ok(match dial_freq_hz {
        Some(freq_hz) => Box::new(FixedCenterFreqSource {
            inner: src,
            freq_hz,
        }),
        None => src,
    })
}

/// `--source <path>.wav` covers two distinct file formats sharing the same
/// flag: a mono real-audio recording (M1 "Audio passband" input, e.g.
/// captured from a rig's RX line-out -- decoded via `AudioIqSource`'s
/// Hilbert transform, hard-pinned to 48000 Hz) and a 2-channel raw complex-
/// IQ recording at any rate (`WavIqSource`, the same format `decode`/
/// `oracle` already read directly, and the only format that can feed
/// `--capture-rate-hz` decimation for file replay -- MAN-169 round-2 Codex
/// finding: routing every `--source` WAV through `AudioIqSource`
/// unconditionally meant a 96/192 kS/s IQ replay could never reach here,
/// since `AudioIqSource::from_wav_file` rejects every rate but 48000).
/// Disambiguated by the explicit `--source-iq` flag, not channel count
/// (MAN-169 round-4 Codex finding: a 2-channel WAV is ambiguous between a
/// genuine IQ capture and an ordinary stereo real-audio recording -- header
/// shape alone can't tell them apart, so guessing from it silently
/// misinterpreted stereo audio as IQ). `--source-iq` set routes through
/// `WavIqSource`; unset (the default, matching this flag's pre-round-2
/// behavior exactly) always falls through to `AudioIqSource::from_wav_file`,
/// so its own existing validation error (not a new one invented here) is
/// what the operator sees for a rate/format mismatch.
fn open_audio_source(
    device: Option<String>,
    source: Option<PathBuf>,
    source_iq: bool,
    dial_freq_hz: Option<f64>,
) -> Result<reconnect::SendSource> {
    Ok(match source {
        // A raw-IQ WAV is a `WavIqSource`, not an `AudioIqSource`, so it
        // has no `with_center_freq_hz`: `--dial-freq-hz` is applied with
        // the same override wrapper RF-aware sources use. (MAN-34 moved
        // audio sources to a native dial frequency; this keeps the IQ
        // replay path honoring `--dial-freq-hz` exactly as before.)
        Some(path) if source_iq => {
            let src = open_iq_source_file(&path, None, None)?;
            match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            }
        }
        Some(path) => audio_with_dial(
            manta_input::AudioIqSource::from_wav_file(&path)?,
            dial_freq_hz,
        )?,
        None => audio_with_dial(
            manta_input::AudioIqSource::from_device(device.as_deref())?,
            dial_freq_hz,
        )?,
    })
}

/// Gives an `AudioIqSource` the operator's dial frequency natively
/// (`AudioIqSource::with_center_freq_hz`, MAN-34), or returns it unchanged.
fn audio_with_dial(
    src: manta_input::AudioIqSource,
    dial_freq_hz: Option<f64>,
) -> Result<reconnect::SendSource> {
    Ok(Box::new(match dial_freq_hz {
        Some(hz) => src.with_center_freq_hz(hz)?,
        None => src,
    }))
}

/// Whether `source` (only meaningful when `source_iq` is set -- see
/// `open_audio_source`) carries a real RF center frequency once actually
/// opened and its sidecar parsed, not merely because a `<stem>.json` file
/// happens to exist (MAN-169 round-4 Codex finding: a sidecar existing
/// with `center_freq_hz: 0.0` -- IqSource's own "unknown center" sentinel
/// -- is indistinguishable from "no sidecar" once parsed, so existence
/// alone isn't enough to bypass the --dial-freq-hz guard below). Requires
/// strictly positive, not just nonzero (MAN-169 round-5 Codex finding: a
/// negative `center_freq_hz` passed the old `!= 0.0` check and would have
/// published negative/invalid RF frequencies through spot outputs) -- an
/// RF dial frequency in this domain is never zero or negative.
fn source_iq_has_real_rf_center(path: &Path, source_iq: bool) -> bool {
    if !source_iq {
        return false;
    }
    open_iq_source_file(path, None, None)
        .map(|src| src.center_freq_hz() > 0.0)
        .unwrap_or(false)
}

/// MAN-309: opens an IQ `--source`: a manta recording (by its metadata), a
/// headerless file described by `raw` (centred on `dial_freq_hz`), or an IQ
/// WAV. The dial override of a recording or WAV is applied by the caller.
pub(crate) fn open_iq_source_file(
    path: &Path,
    raw: Option<RawIq>,
    dial_freq_hz: Option<f64>,
) -> Result<reconnect::SendSource> {
    let opts = match raw {
        Some(raw) => manta_input::IqFileOptions {
            raw_format: Some(raw.format),
            sample_rate_hz: Some(raw.sample_rate_hz),
            center_freq_hz: Some(dial_freq_hz.ok_or_else(|| {
                anyhow!("headerless IQ requires --sample-rate-hz and --dial-freq-hz")
            })?),
        },
        None => manta_input::IqFileOptions::default(),
    };
    manta_input::open_iq_file(path, opts)
}

/// Overrides an inner source's `center_freq_hz()` with a fixed value.
/// Audio sources now carry the operator's dial frequency natively
/// (`open_audio_source` -> `AudioIqSource::with_center_freq_hz`, MAN-34);
/// this decorator remains for the RF-aware sources (KiwiSDR/SoapySDR/HPSDR,
/// and IQ WAVs with a real sidecar frequency), which already report a tuned
/// frequency of their own that `--dial-freq-hz` is allowed to supersede, and
/// for raw-IQ WAV replay, which has no native dial setter. See
/// `--dial-freq-hz`.
pub(crate) struct FixedCenterFreqSource {
    pub(crate) inner: reconnect::SendSource,
    pub(crate) freq_hz: f64,
}

impl IqSource for FixedCenterFreqSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.freq_hz
    }

    /// Only the centre frequency is overridden -- the wrapped source's own
    /// RF passband must still reach `SKIMMER/SETT`, or wrapping a
    /// resampling source (KiwiSDR) or a rig-audio source in
    /// `--dial-freq-hz` would silently re-introduce the "advertise the
    /// processing rate" bug this method exists to prevent (MAN-86 review).
    /// The wrapped bounds are offsets from the centre, so overriding the
    /// centre alone relocates them correctly.
    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    /// MAN-196: same forwarding duty as `rf_passband_hz` above -- the
    /// wrapped decimator's trusted band must reach the run/soak startup
    /// notice and the track manager's spawn veto; the override relocates
    /// the centre only, and the band is offsets from the centre.
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }

    /// MAN-273: forwarded with `trusted_band_hz`.
    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }

    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        self.inner.read(buf)
    }

    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }

    /// MAN-305: forwarded as `take_discontinuity` is.
    fn take_missing_samples(&mut self) -> Option<u64> {
        self.inner.take_missing_samples()
    }

    /// MAN-202: forwarded as `take_discontinuity` is (a SoapySDR overflow
    /// reaches `listen()` through this wrapper).
    fn take_unknown_gap(&mut self) -> bool {
        self.inner.take_unknown_gap()
    }

    /// MAN-201: forwarded as the gap hooks are.
    fn take_stalled(&mut self) -> bool {
        self.inner.take_stalled()
    }

    /// MAN-259: `ReconnectingSource` reads this off the opener's result to
    /// end an outage where acquisition began.
    fn acquisition_start_offset(&self) -> std::time::Duration {
        self.inner.acquisition_start_offset()
    }

    fn health_counters(&self) -> Option<std::sync::Arc<manta_input::InputHealthCounters>> {
        self.inner.health_counters()
    }
}

/// Warn on stderr when the opened source is audio-derived (not RF-aware)
/// and no `--dial-freq-hz` was given -- MAN-34. A missing dial frequency in
/// this case means every downstream frequency is a bare baseband offset,
/// not an absolute RF frequency. This is a warning, not a `bail!`: a bare
/// `manta listen --device` watching decoded text locally, with no interest
/// in frequency, is a legitimate and documented use (README.md, the M1
/// manual-acceptance runbook). The harm this ticket names -- a wrong
/// frequency reaching the network -- is already a hard error via
/// `--config`'s own check; this covers the local case without
/// regressing it.
pub(crate) fn warn_if_audio_source_has_no_rf_reference(
    has_rf_aware_source: bool,
    dial_freq_hz: Option<f64>,
) {
    if !has_rf_aware_source && dial_freq_hz.is_none() {
        logging::warning(
            "warning: no --dial-freq-hz given for an audio source -- \
             reported frequencies are baseband offsets within the \
             audio passband, not absolute RF frequencies. Pass the \
             rig's dial frequency, e.g. --dial-freq-hz 14030000.",
        );
    }
}

/// MAN-196: the one startup line a decimated capture prints, naming its
/// usable passband. `lo_hz`/`hi_hz` are the source's *intersected* RF
/// passband (`rf_passband_hz()` -- the inner passband clipped to the
/// decimator's <= 1 dB trusted band), so the line never claims spectrum
/// the source does not deliver. `run` logs it through `tracing::info!`;
/// `soak`, which installs no subscriber, writes it straight to stderr.
pub(crate) fn trusted_band_notice(fs_out_hz: f64, lo_hz: f64, hi_hz: f64) -> String {
    format!(
        "decimated capture {fs_out_hz:.0} Hz: usable passband {lo_hz:.0} to {hi_hz:.0} Hz; \
         channels beyond (decimator transition band) are not tracked"
    )
}

/// MAN-131: a live audio input delivering exact zeros is, on macOS, the
/// usual sign of a denied microphone permission (cpal cannot see TCC; the
/// stream opens and the callback hands over silence). A warning, not an
/// error: a muted input is a legitimate state, and `doctor` still reports
/// NO_SIGNAL on its own.
pub(crate) fn audio_silence_warning(device: &str) -> String {
    format!(
        "warning: audio input {device} is delivering digital silence at {} Hz \
          (every sample is exactly zero) -- {}",
        manta_input::TARGET_RATE_HZ,
        manta_input::audio::audio_input_hint()
    )
}

/// MAN-233: a SoapySDR stream overflow error means this chunk was dropped
/// by a full driver ring buffer -- the host is not keeping up. Counted, and
/// rate-limited so a sustained overflow burst logs one line per 60 s. An
/// overflow flagged `SOAPY_SDR_END_ABRUPT` on a successful read never
/// reaches here (see `SoapySdrIqSource::read`), hence "overflow error".
#[cfg(feature = "soapy")]
fn soapy_overflow_warning(driver: &str) -> String {
    format!(
        "warning: SoapySDR input ({driver}) dropped samples: the driver reported a \
         stream overflow -- the host is not keeping up; every overflow error is counted in \
         manta_input_gaps_detected_total{{source=\"soapy\"}} (this warning is \
         rate-limited to once per 60 s)"
    )
}

/// MAN-308 D8: the `note:` line for one automatic attenuation change.
pub(crate) fn attenuation_line(c: &manta_input::hpsdr::AttenuationChange) -> String {
    use manta_input::hpsdr::proto2::attenuator::{MAX_DB, RELEASE_AFTER, TRIP_AFTER};
    use manta_input::hpsdr::AttenuationCause;
    let (why, verb) = match c.cause {
        AttenuationCause::Overload => (
            format!("ADC overload for {} s", TRIP_AFTER.as_secs()),
            "raised",
        ),
        AttenuationCause::Quiet => (
            format!("no ADC overload for {} min", RELEASE_AFTER.as_secs() / 60),
            "lowered",
        ),
    };
    let tail = if c.to_db == MAX_DB {
        ", its maximum"
    } else {
        ""
    };
    format!(
        "note: HPSDR radio {}: {why}; automatic attenuation {verb} the step attenuator from {} dB \
         to {} dB{tail}",
        c.radio, c.from_db, c.to_db
    )
}

/// MAN-308 D1: `input.auto_attenuation` on a radio `"auto"` found speaking
/// protocol 1.
pub(crate) fn protocol_1_attenuation_warning(host: &str, port: u16) -> String {
    format!(
        "warning: HPSDR radio {host}:{port} speaks openHPSDR protocol 1; input.auto_attenuation \
         needs protocol 2, so manta leaves this radio's attenuator alone"
    )
}

/// MAN-308 D1: once per open, warn when automatic attenuation is on but the
/// radio opened as protocol 1, and zero the published level (manta sets
/// nothing on that radio).
pub(crate) fn warn_if_attenuation_needs_protocol_2(
    cfg: &manta_input::hpsdr::HpsdrConfig,
    opened: &manta_input::hpsdr::OpenedHpsdr,
) {
    if let Some(handle) = &cfg.auto_attenuation {
        if opened.protocol == manta_input::hpsdr::HpsdrProtocol::One {
            handle.clear();
            logging::warning(&protocol_1_attenuation_warning(&cfg.host, cfg.port));
        }
    }
}

/// MAN-140 D18: a radiod channel with AGC on rescales every signal in its
/// passband whenever one strong signal comes and goes. A warning on each
/// open, not an error: some operators run AGC on purpose.
pub(crate) fn ka9q_agc_warning(ssrc: u32) -> String {
    format!(
        "warning: ka9q-radio channel ssrc {ssrc} has AGC on; set agc = off and a fixed gain \
         in its radiod section, or one strong signal changes the gain for every signal in \
         the passband"
    )
}

/// A stderr line printed when an HPSDR radio opens (MAN-305 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HpsdrOpenLine {
    Note(String),
    Warning(String),
}

/// MAN-305 §4: how the protocol was chosen, rendered from what
/// `open_receiver` returned. `None` under `--hpsdr-protocol 1`, which
/// prints nothing new.
pub(crate) fn hpsdr_open_line(
    host: &str,
    port: u16,
    protocol: manta_input::hpsdr::HpsdrProtocol,
    discovery: &manta_input::hpsdr::Discovery,
) -> Option<HpsdrOpenLine> {
    use manta_input::hpsdr::{Discovery, DiscoveryReply, HpsdrProtocol};
    match discovery {
        Discovery::Answered(DiscoveryReply::Protocol2 {
            board,
            protocol_version,
            firmware,
            ..
        }) => {
            let (name, tail) = match manta_input::hpsdr::proto2::wire::board_info(Some(*board)).name
            {
                Some(name) => (name, ""),
                None => ("unknown board", ", front-end filters bypassed"),
            };
            Some(HpsdrOpenLine::Note(format!(
                "note: HPSDR radio {host}:{port} answered discovery as openHPSDR protocol 2 \
                 (board {board}, {name}; protocol version {protocol_version}; firmware \
                 {firmware}); connecting with protocol 2{tail}"
            )))
        }
        Discovery::Answered(DiscoveryReply::Protocol1 {
            board, firmware, ..
        }) => Some(HpsdrOpenLine::Note(format!(
            "note: HPSDR radio {host}:{port} answered discovery as openHPSDR protocol 1 \
             (board {board}; firmware {firmware}); connecting with protocol 1"
        ))),
        Discovery::NoAnswer => Some(HpsdrOpenLine::Warning(format!(
            "warning: HPSDR radio {host}:{port} did not answer discovery within 1 s; connecting \
             with protocol 1 -- pass --hpsdr-protocol 2 (protocol = \"2\" under [input]) for a \
             protocol-2 radio"
        ))),
        Discovery::Skipped => match protocol {
            HpsdrProtocol::Two => Some(HpsdrOpenLine::Note(format!(
                "note: HPSDR radio {host}:{port}: protocol 2 as configured; discovery skipped, \
                 board unknown, front-end filters bypassed"
            ))),
            HpsdrProtocol::One | HpsdrProtocol::Auto => None,
        },
    }
}

/// Print [`hpsdr_open_line`] for a radio `open_receiver` just opened.
pub(crate) fn log_hpsdr_open_line(host: &str, port: u16, opened: &manta_input::hpsdr::OpenedHpsdr) {
    match hpsdr_open_line(host, port, opened.protocol, &opened.discovery) {
        Some(HpsdrOpenLine::Note(line)) => logging::note(&line),
        Some(HpsdrOpenLine::Warning(line)) => logging::warning(&line),
        None => {}
    }
}

/// MAN-131: `decode`'s analogue of `warn_if_audio_source_has_no_rf_reference`.
/// `sidecar_center_hz` is `None` when `<stem>.json` does not exist. When this
/// returns `Some`, the `Decode` handler decodes at a 0.0 centre, so a
/// negative sidecar value cannot contradict the warning.
pub(crate) fn recording_center_warning(
    wav: &Path,
    sidecar_center_hz: Option<f64>,
) -> Option<String> {
    let sidecar = manta_input::sidecar_path(wav);
    let why = match sidecar_center_hz {
        Some(hz) if hz > 0.0 => return None,
        Some(hz) => format!(
            "sidecar {} gives center_freq_hz = {hz}, not an RF frequency",
            sidecar.display()
        ),
        None => format!("no sidecar {} next to {}", sidecar.display(), wav.display()),
    };
    Some(format!(
        "warning: {why} -- reported frequencies are baseband offsets from the \
         recording's centre, not absolute RF frequencies. Pass the centre \
         frequency, e.g. --center-freq-hz 14000000."
    ))
}

/// Wrap `src` in a `DecimatingSource` targeting `capture_rate_hz`, unless
/// it's `None` or already matches the source's native rate (a no-op in
/// either case -- omitting `--capture-rate-hz` reproduces today's exact
/// behavior). Applied uniformly regardless of source type (kiwi/soapy/
/// hpsdr/audio/file replay), mirroring how `dial_freq_hz`'s
/// `FixedCenterFreqSource` wrap is already applied uniformly below.
fn maybe_decimate(
    src: reconnect::SendSource,
    capture_rate_hz: Option<f64>,
) -> Result<reconnect::SendSource> {
    match decimation_target(src.sample_rate(), capture_rate_hz) {
        Some(target) => Ok(Box::new(manta_input::DecimatingSource::new(src, target)?)),
        None => Ok(src),
    }
}

/// The rate `maybe_decimate` decimates a `native_rate_hz` source to, or
/// `None` when it leaves it alone. Shared with
/// `LiveSourceSpec::configured_center_and_passband` (MAN-334), so the
/// preflight and the opened source never disagree on whether it decimates.
fn decimation_target(native_rate_hz: f64, capture_rate_hz: Option<f64>) -> Option<f64> {
    capture_rate_hz.filter(|target| (target - native_rate_hz).abs() > 1e-6)
}

/// The one-DDC `HpsdrConfig` `LiveSourceSpec::open` opens an HPSDR receiver
/// with. Shared with `LiveSourceSpec::configured_center_and_passband`
/// (MAN-334), which plans on it only when `hpsdr::validate_before_open` --
/// the I/O-free check the protocol's own open makes first -- accepts it.
fn hpsdr_config(
    hpsdr: &HpsdrOpts,
    host: String,
    freq: f64,
    rate: f64,
) -> manta_input::hpsdr::HpsdrConfig {
    manta_input::hpsdr::HpsdrConfig {
        host,
        port: hpsdr.port.unwrap_or(HPSDR_CONTROL_PORT),
        ddc_count: 1,
        sample_rate_hz: rate,
        center_freq_hz: vec![freq],
        auto_attenuation: hpsdr.auto_attenuation.clone(),
    }
}

/// MAN-309: the decimator does not carry discontinuities, so a manta IQ
/// recording with any gap record replays only at its recorded rate.
fn refuse_gapped_decimation(path: &Path, fs: f64, capture_rate_hz: Option<f64>) -> Result<()> {
    let rate_change = capture_rate_hz.is_some_and(|target| (target - fs).abs() > 1e-6);
    if rate_change
        && manta_input::is_native_recording(path)
        && manta_input::read_recording_summary(path)?.has_gaps()
    {
        bail!("cannot decimate an IQ recording with gaps; replay at its recorded sample rate");
    }
    Ok(())
}

/// MAN-309: `--iq-format`/`--sample-rate-hz` (or `input.iq_format`/
/// `input.sample_rate_hz`) describe a headerless IQ file source; with them,
/// `--dial-freq-hz` (or `input.center_freq_hz`) is its centre and required.
pub(crate) fn apply_raw_iq(
    resolved: &mut Resolved,
    iq_format: Option<manta_input::RawIqFormat>,
    sample_rate_hz: Option<f64>,
) -> Result<()> {
    let Some(format) = iq_format else {
        if let LiveSourceSpec::File { raw: Some(_), .. } = &resolved.spec {
            if resolved.dial_freq_hz.is_none() {
                bail!("headerless IQ requires --sample-rate-hz and --dial-freq-hz");
            }
        }
        return Ok(());
    };
    let (Some(sample_rate_hz), Some(_)) = (sample_rate_hz, resolved.dial_freq_hz) else {
        bail!("headerless IQ requires --sample-rate-hz and --dial-freq-hz");
    };
    match &mut resolved.spec {
        LiveSourceSpec::File {
            source_iq: true,
            raw,
            ..
        } if resolved.inputs.is_empty() => {
            *raw = Some(RawIq {
                format,
                sample_rate_hz,
            });
            Ok(())
        }
        _ => bail!("--iq-format applies to one IQ file source (--source PATH --source-iq)"),
    }
}
