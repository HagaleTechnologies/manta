//! Config resolution for `run` and its relatives: `CliOverrides`, the
//! `Resolved` result, input selection, `spec_from_file` and replay-mode
//! handling. MAN-548 moved it out of `main.rs`.

use crate::cli_parse::parse_hpsdr_mac;
use crate::config;
use crate::fmt;
use crate::logging;
use crate::source_spec::attenuation_line;
use crate::source_spec::open_iq_source_file;
use crate::source_spec::HpsdrOpts;
use crate::source_spec::Ka9qOpts;
use crate::source_spec::KiwiOpts;
use crate::source_spec::LiveSourceSpec;
use crate::source_spec::RawIq;
use crate::RbnCompareOpts;
use anyhow::{bail, Result};
use manta_input::IqSource;
use std::path::PathBuf;

/// The command line's say in source/input/spot selection, before it is
/// merged over the config file and environment (`resolve`, MAN-261).
/// `Clone` so a SIGHUP reload (MAN-78) re-resolves with the same flags.
#[derive(Clone)]
pub(crate) struct CliOverrides {
    pub(crate) prior: Option<manta_decode::config_file::PriorSwitch>,
    pub(crate) device: Option<String>,
    pub(crate) source: Option<PathBuf>,
    pub(crate) source_iq: bool,
    /// `run --realtime`/`--loop` (MAN-269); always false for soak/doctor.
    pub(crate) realtime: bool,
    pub(crate) loop_replay: bool,
    pub(crate) kiwi: KiwiOpts,
    pub(crate) soapy: crate::source_spec::SoapyOpts,
    pub(crate) hpsdr: HpsdrOpts,
    pub(crate) ka9q: Ka9qOpts,
    pub(crate) freq_correction_ppm: Option<f64>,
    pub(crate) dial_freq_hz: Option<f64>,
    pub(crate) capture_rate_hz: Option<f64>,
    pub(crate) replay_epoch: Option<i64>,
    pub(crate) allowlist: Vec<String>,
    pub(crate) blocklist: Option<PathBuf>,
    pub(crate) notch: Option<PathBuf>,
    pub(crate) cty: Option<PathBuf>,
    pub(crate) scp: Option<PathBuf>,
    /// MAN-316: `--cty-sha256`/`--scp-sha256`.
    pub(crate) cty_sha256: Option<String>,
    pub(crate) scp_sha256: Option<String>,
    pub(crate) patterns: Option<PathBuf>,
    /// MAN-301: `--rbn-compare`/`--rbn-spotter` (`run` only).
    pub(crate) rbn: RbnCompareOpts,
}

impl CliOverrides {
    /// No command-line say at all: what `manta config check` resolves with,
    /// so it validates exactly what the file and environment give `run`.
    pub(crate) fn none() -> Self {
        CliOverrides {
            prior: None,
            device: None,
            source: None,
            source_iq: false,
            realtime: false,
            loop_replay: false,
            kiwi: KiwiOpts {
                host: None,
                port: 8073,
                freq: None,
                password: String::new(),
            },
            soapy: crate::source_spec::SoapyOpts::default(),
            hpsdr: HpsdrOpts {
                host: None,
                mac: None,
                port: None,
                protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
                freq: None,
                rate: None,
                auto_attenuation: None,
            },
            ka9q: Ka9qOpts::unset(),
            freq_correction_ppm: None,
            dial_freq_hz: None,
            capture_rate_hz: None,
            replay_epoch: None,
            allowlist: Vec::new(),
            blocklist: None,
            notch: None,
            cty: None,
            scp: None,
            cty_sha256: None,
            scp_sha256: None,
            patterns: None,
            rbn: RbnCompareOpts::default(),
        }
    }

    /// The flag that selects a source, if any: given one, the command line
    /// defines the whole source (D6).
    pub(crate) fn source_selector(&self) -> Option<&'static str> {
        if self.ka9q.group.is_some() {
            return Some("--ka9q-group");
        }
        if self.hpsdr.selected() {
            return self.hpsdr.selector();
        }
        if self.soapy.driver.is_some() {
            return Some("--soapy-driver");
        }
        if self.kiwi.host.is_some() {
            Some("--kiwi-host")
        } else if self.source.is_some() {
            Some("--source")
        } else if self.device.is_some() {
            Some("--device")
        } else {
            None
        }
    }

    /// The source the flags describe, in today's priority order: ka9q,
    /// then hpsdr, then kiwi, then soapy, then a WAV file or audio device.
    /// clap lets at most one selector through, so the order only matters
    /// for a hand-built `CliOverrides`.
    pub(crate) fn into_spec(self) -> LiveSourceSpec {
        if self.ka9q.group.is_some() {
            return LiveSourceSpec::Ka9q(self.ka9q);
        }
        if self.hpsdr.selected() {
            return LiveSourceSpec::Hpsdr(self.hpsdr);
        }
        if self.kiwi.host.is_some() {
            return LiveSourceSpec::Kiwi(self.kiwi);
        }
        if self.soapy.driver.is_some() {
            return LiveSourceSpec::Soapy(self.soapy);
        }
        match self.source {
            Some(path) => LiveSourceSpec::File {
                path,
                source_iq: self.source_iq,
                raw: None,
            },
            None => LiveSourceSpec::AudioDevice(self.device),
        }
    }
}

/// `[spot]` after the CLI is merged over the file/env layer (D6).
#[derive(Debug, PartialEq)]
pub(crate) struct SpotResolved {
    pub(crate) validation_level: manta_spot::ValidationLevel,
    pub(crate) allowlist: Vec<String>,
    pub(crate) blocklist: Option<PathBuf>,
    pub(crate) notch: Option<PathBuf>,
    pub(crate) cty: Option<PathBuf>,
    pub(crate) scp: Option<PathBuf>,
    pub(crate) calls_heard_path: Option<PathBuf>,
    /// MAN-316: hash pins, CLI > `MANTA_SPOT_*_SHA256` > file.
    pub(crate) cty_sha256: Option<String>,
    pub(crate) scp_sha256: Option<String>,
    /// MAN-316: the refresh settings; no flags, so straight from `[spot]`.
    pub(crate) refresh_lists: bool,
    pub(crate) list_cache_dir: Option<PathBuf>,
    pub(crate) cty_url: String,
    pub(crate) scp_url: String,
    /// MAN-542: `[spot] pattern_url`; `None` means the pattern list is never
    /// downloaded.
    pub(crate) pattern_url: Option<String>,
    pub(crate) patterns: Option<PathBuf>,
    /// MAN-200: `[spot] repetition_fusion`; no flag, so straight from the
    /// file/env layer.
    pub(crate) repetition_fusion: bool,
}

/// MAN-316: `--cty-sha256`/`--scp-sha256`, the only list flags beyond
/// `resolve_spot`'s paths.
#[derive(Debug, Default, Clone)]
pub(crate) struct CliPins {
    pub(crate) cty_sha256: Option<String>,
    pub(crate) scp_sha256: Option<String>,
}

/// A non-empty `--allowlist` replaces the file's list (never concatenated);
/// `--blocklist`/`--notch` beat `blocklist_path`/`notch_path`; a pin flag
/// beats `cty_sha256`/`scp_sha256` (MAN-316).
#[cfg(test)]
pub(crate) fn resolve_spot(
    cli_allowlist: Vec<String>,
    cli_blocklist: Option<PathBuf>,
    cli_notch: Option<PathBuf>,
    cli_cty: Option<PathBuf>,
    cli_scp: Option<PathBuf>,
    cli_patterns: Option<PathBuf>,
    spot: &config::SpotFile,
) -> SpotResolved {
    resolve_spot_pinned(
        cli_allowlist,
        cli_blocklist,
        cli_notch,
        cli_cty,
        cli_scp,
        CliPins::default(),
        cli_patterns,
        spot,
    )
}

/// [`resolve_spot`] with the MAN-316 pin flags.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_spot_pinned(
    cli_allowlist: Vec<String>,
    cli_blocklist: Option<PathBuf>,
    cli_notch: Option<PathBuf>,
    cli_cty: Option<PathBuf>,
    cli_scp: Option<PathBuf>,
    pins: CliPins,
    cli_patterns: Option<PathBuf>,
    spot: &config::SpotFile,
) -> SpotResolved {
    SpotResolved {
        allowlist: if cli_allowlist.is_empty() {
            spot.allowlist.clone()
        } else {
            cli_allowlist
        },
        blocklist: cli_blocklist.or_else(|| spot.blocklist_path.clone()),
        notch: cli_notch.or_else(|| spot.notch_path.clone()),
        cty: cli_cty.or_else(|| spot.cty_path.clone()),
        scp: cli_scp.or_else(|| spot.scp_path.clone()),
        calls_heard_path: spot.calls_heard_path.clone(),
        cty_sha256: pins.cty_sha256.or_else(|| spot.cty_sha256.clone()),
        scp_sha256: pins.scp_sha256.or_else(|| spot.scp_sha256.clone()),
        refresh_lists: spot.refresh_lists,
        list_cache_dir: spot.list_cache_dir.clone(),
        cty_url: spot.cty_url().to_string(),
        scp_url: spot.scp_url().to_string(),
        pattern_url: spot.pattern_url.clone(),
        patterns: cli_patterns.or_else(|| spot.pattern_path.clone()),
        validation_level: spot.validation_level,
        repetition_fusion: spot.repetition_fusion,
    }
}

/// How a file replay is delivered (MAN-269): `paced` plays it at the
/// recording's own clock, `looped` reopens it at each end. Only `run`
/// applies it, and only to `LiveSourceSpec::File`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReplayMode {
    paced: bool,
    looped: bool,
}

impl ReplayMode {
    /// `--loop` implies `--realtime` (D2): an unpaced loop would decode
    /// endlessly and stamp spots ever further into the future.
    fn from_flags(realtime: bool, looped: bool) -> Self {
        ReplayMode {
            paced: realtime || looped,
            looped,
        }
    }
}

/// Everything a live command needs after CLI > env > file > default.
pub(crate) struct Resolved {
    pub(crate) cw_policy: manta_engine::cw_segments::Policy,
    pub(crate) conditioning: manta_dsp::condition::ConditioningConfig,
    pub(crate) spec: LiveSourceSpec,
    pub(crate) replay: ReplayMode,
    pub(crate) freq_correction_ppm: f64,
    pub(crate) dial_freq_hz: Option<f64>,
    pub(crate) capture_rate_hz: Option<f64>,
    pub(crate) replay_epoch: Option<i64>,
    pub(crate) spot: SpotResolved,
    /// MAN-301: `[rbn_compare]` with `--rbn-compare`/`--rbn-spotter`
    /// merged over it; `None` when neither the file nor the flags set it.
    pub(crate) rbn_compare: Option<manta_server::config::RbnCompareConfig>,
    /// MAN-318: `[callsign_lookup]` from the config (`None` when absent).
    pub(crate) callsign_lookup: Option<config::CallsignLookupConfig>,
    /// Printed to stderr by the caller, never stdout.
    pub(crate) notes: Vec<String>,
    /// The flag that replaced a typed `[input]` from the file, if one did:
    /// that file's receiver is then not the one being opened (MAN-127).
    pub(crate) replaced_file_source: Option<&'static str>,
    /// MAN-13: every `[[input]]` entry, resolved, when the config names two
    /// or more and no source flag replaced them; empty otherwise (the
    /// fields above then describe the one receiver). `spec`,
    /// `freq_correction_ppm`, `dial_freq_hz` and `capture_rate_hz` above
    /// are the first entry's.
    pub(crate) inputs: Vec<ResolvedInput>,
}

/// MAN-13: one `[[input]]` entry, resolved: its own source, receiver
/// correction, dial and capture rate.
#[derive(Clone)]
pub(crate) struct ResolvedInput {
    pub(crate) cw_policy: manta_engine::cw_segments::Policy,
    pub(crate) conditioning: manta_dsp::condition::ConditioningConfig,
    pub(crate) name: String,
    /// MAN-541: the config entry (`NamedInput::entry`) this input came
    /// from, so the RBN skew suggestion pools a band plan's streams.
    pub(crate) entry: Option<String>,
    pub(crate) digital_modes: Vec<manta_engine::ftx::FtxMode>,
    pub(crate) spec: LiveSourceSpec,
    pub(crate) freq_correction_ppm: f64,
    pub(crate) dial_freq_hz: Option<f64>,
    pub(crate) capture_rate_hz: Option<f64>,
    /// MAN-309: `run` records this input's IQ here (`record_iq`).
    pub(crate) record_iq: Option<PathBuf>,
    /// MAN-141: `run` sends this input's spectrum here (`spectrum_udp`).
    pub(crate) spectrum: Option<config::SpectrumOut>,
    /// MAN-310: this stream's share of a split band plan.
    pub(crate) spawn_band_hz: Option<(f64, f64)>,
}

/// MAN-13: a one-receiver command (`calibrate`, `doctor`, `soak`, `check`)
/// against a config with several inputs must say which one it examines.
/// Narrows `loaded` to the named input; with one input, `name` may name
/// it or be omitted.
pub(crate) fn select_input(
    loaded: &mut config::Loaded,
    name: Option<&str>,
    command: &str,
) -> Result<()> {
    let origin = loaded.origin.clone();
    let names: Vec<&str> = loaded.inputs.iter().map(|n| n.name.as_str()).collect();
    let index = match name {
        None if names.len() < 2 => return Ok(()),
        None => {
            let options = match names.split_last() {
                Some((last, rest)) => format!(
                    "{} or --input {last}",
                    rest.iter()
                        .map(|n| format!("--input {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => String::new(),
            };
            bail!(
                "manta {command} checks one receiver at a time and {origin} has {} inputs: pass \
                 {options}",
                names.len()
            );
        }
        Some(name) => match names.iter().position(|n| *n == name) {
            Some(index) => index,
            None => {
                // MAN-310: the entry whose `bands` planned several streams.
                let streams: Vec<&str> = loaded
                    .inputs
                    .iter()
                    .filter(|n| match &n.entry {
                        Some(entry) => entry == name,
                        // The `[input]` table's streams are `<its name>-<label>`.
                        None => n
                            .name
                            .strip_prefix(name)
                            .is_some_and(|rest| rest.starts_with('-')),
                    })
                    .map(|n| n.name.as_str())
                    .collect();
                if streams.len() >= 2 {
                    let options = match streams.split_last() {
                        Some((last, rest)) => format!(
                            "{} or --input {last}",
                            rest.iter()
                                .map(|n| format!("--input {n}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        None => String::new(),
                    };
                    bail!(
                        "--input {} is one receiver with {} streams ({}); pass {options}",
                        fmt::quoted(name),
                        streams.len(),
                        streams.join(", ")
                    );
                }
                bail!(
                    "--input {} is not one of {origin}'s inputs ({})",
                    fmt::quoted(name),
                    names.join(", ")
                );
            }
        },
    };
    let chosen = loaded.inputs[index].clone();
    loaded.input = chosen.input.clone();
    loaded.inputs = vec![chosen];
    Ok(())
}

/// MAN-301: the flag enables; a non-empty `--rbn-spotter` replaces
/// `[rbn_compare].spotters` (never concatenated, like `--allowlist`).
fn resolve_rbn_compare(
    cli: &RbnCompareOpts,
    file: Option<&manta_server::config::RbnCompareConfig>,
) -> Option<manta_server::config::RbnCompareConfig> {
    if !cli.rbn_compare && cli.rbn_spotter.is_empty() {
        return file.cloned();
    }
    let mut cfg = file.cloned().unwrap_or_default();
    cfg.enabled |= cli.rbn_compare;
    if !cli.rbn_spotter.is_empty() {
        cfg.spotters = cli.rbn_spotter.clone();
    }
    Some(cfg)
}

/// D6: merges the command line over the loaded file + environment layer.
pub(crate) fn resolve(cli: CliOverrides, loaded: &config::Loaded) -> Result<Resolved> {
    let mut notes = loaded.notes.clone();
    let rbn_compare = resolve_rbn_compare(&cli.rbn, loaded.rbn_compare.as_ref());
    let spot = resolve_spot_pinned(
        cli.allowlist.clone(),
        cli.blocklist.clone(),
        cli.notch.clone(),
        cli.cty.clone(),
        cli.scp.clone(),
        CliPins {
            cty_sha256: cli.cty_sha256.clone(),
            scp_sha256: cli.scp_sha256.clone(),
        },
        cli.patterns.clone(),
        &loaded.spot,
    );
    let mut shared = loaded.input.shared.clone();
    let (freq_correction_ppm, dial_freq_hz, capture_rate_hz, replay_epoch) = (
        cli.freq_correction_ppm,
        cli.dial_freq_hz,
        cli.capture_rate_hz,
        cli.replay_epoch,
    );
    let mut replaced_file_source = None;
    let (cli_realtime, cli_loop) = (cli.realtime, cli.loop_replay);
    // MAN-13: several `[[input]]` entries, unless a source flag picks one
    // receiver instead.
    let several = loaded.inputs.len() >= 2;
    // MAN-310: several inputs that all came from one entry's `bands`.
    let planned = several
        && loaded
            .inputs
            .iter()
            .all(|n| n.entry == loaded.inputs[0].entry);
    let what = if planned {
        let section = match &loaded.inputs[0].entry {
            Some(entry) => format!("[[input]] {}", fmt::quoted(entry)),
            None => "[input]".to_string(),
        };
        format!("{} streams {section}.bands plans", loaded.inputs.len())
    } else {
        format!("{} [[input]] entries", loaded.inputs.len())
    };
    if several {
        if let Some(flag) = cli.source_selector() {
            let from = if planned { "in" } else { "from" };
            notes.push(format!(
                "note: {flag} selects the source; ignoring the {what} {from} {}",
                loaded.origin
            ));
            let spec = cli.into_spec();
            let replay = resolve_replay_mode(cli_realtime, cli_loop, None, &spec)?;
            return Ok(Resolved {
                cw_policy: Default::default(),
                conditioning: Default::default(),
                spec,
                replay,
                freq_correction_ppm: freq_correction_ppm.unwrap_or(0.0),
                dial_freq_hz,
                capture_rate_hz,
                replay_epoch,
                spot,
                rbn_compare,
                callsign_lookup: loaded.callsign_lookup.clone(),
                notes,
                replaced_file_source: Some(flag),
                inputs: Vec::new(),
            });
        }
        for (flag, set) in [
            ("--freq-correction-ppm", freq_correction_ppm.is_some()),
            ("--dial-freq-hz", dial_freq_hz.is_some()),
            ("--capture-rate-hz", capture_rate_hz.is_some()),
        ] {
            if set {
                let key = match flag {
                    "--freq-correction-ppm" => "freq_correction_ppm",
                    "--dial-freq-hz" => "center_freq_hz",
                    _ => "capture_rate_hz",
                };
                bail!(
                    "{flag} applies to one receiver and {} has {what}; set {key} in the entry, \
                     or name one receiver with a source flag",
                    loaded.origin
                );
            }
        }
    }
    let inputs = if several {
        loaded
            .inputs
            .iter()
            .map(|n| {
                let Some(source) = &n.input.source else {
                    bail!("[[input]] {} has no type", fmt::quoted(&n.name));
                };
                Ok(ResolvedInput {
                    cw_policy: n.input.shared.cw_policy.clone(),
                    name: n.name.clone(),
                    entry: n.entry.clone(),
                    digital_modes: n.input.shared.digital_modes.clone(),
                    spec: spec_from_file(source, cli.source_iq)?,
                    freq_correction_ppm: n.input.shared.freq_correction_ppm.unwrap_or(0.0),
                    conditioning: n.input.shared.conditioning(),
                    dial_freq_hz: n.input.shared.center_freq_hz,
                    capture_rate_hz: n.input.shared.capture_rate_hz,
                    record_iq: n.input.shared.record_iq.clone(),
                    spectrum: n.input.shared.spectrum.clone(),
                    spawn_band_hz: n.spawn_band_hz,
                })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    // MAN-269 D3: a config `type = "file"` source contributes its own
    // realtime/loop only when no CLI flag selects the source.
    let file_replay = match (cli.source_selector(), &loaded.input.source) {
        (None, Some(source)) => Some(source),
        _ => None,
    };
    let spec = match (cli.source_selector(), &loaded.input.source) {
        (Some(flag), Some(file_source)) => {
            replaced_file_source = Some(flag);
            // A typed [input] describes one receiver: its ppm and dial
            // belong to it, not to whatever the command line names instead.
            notes.push(format!(
                "note: {flag} selects the source; ignoring [input] (type = \"{}\") from {}, \
                 including its freq_correction_ppm/center_freq_hz",
                file_source.kind().name(),
                loaded.origin
            ));
            shared = config::SharedInput::default();
            cli.into_spec()
        }
        (Some(_), None) | (None, None) => cli.into_spec(),
        (None, Some(file_source)) => spec_from_file(file_source, cli.source_iq)?,
    };
    let replay = resolve_replay_mode(cli_realtime, cli_loop, file_replay, &spec)?;
    Ok(Resolved {
        cw_policy: shared.cw_policy.clone(),
        conditioning: shared.conditioning(),
        spec,
        replay,
        freq_correction_ppm: freq_correction_ppm
            .or(shared.freq_correction_ppm)
            .unwrap_or(0.0),
        dial_freq_hz: dial_freq_hz.or(shared.center_freq_hz),
        capture_rate_hz: capture_rate_hz.or(shared.capture_rate_hz),
        replay_epoch: replay_epoch.or(shared.replay_epoch),
        spot,
        rbn_compare,
        callsign_lookup: loaded.callsign_lookup.clone(),
        notes,
        replaced_file_source,
        inputs,
    })
}

/// The `LiveSourceSpec` a typed `[input]` describes. `--source-iq` without
/// `--source` sets `iq` on a `type = "file"` source (D6).
fn spec_from_file(source: &config::SourceFromFile, cli_source_iq: bool) -> Result<LiveSourceSpec> {
    Ok(match source {
        config::SourceFromFile::Audio { device } => LiveSourceSpec::AudioDevice(device.clone()),
        config::SourceFromFile::File { path, iq, raw, .. } => LiveSourceSpec::File {
            path: path.clone(),
            source_iq: *iq || cli_source_iq,
            raw: raw.map(|(format, sample_rate_hz)| RawIq {
                format,
                sample_rate_hz,
            }),
        },
        config::SourceFromFile::Kiwi {
            host,
            port,
            freq_hz,
            password,
        } => LiveSourceSpec::Kiwi(KiwiOpts {
            host: Some(host.clone()),
            port: *port,
            freq: Some(*freq_hz),
            password: password.clone(),
        }),
        config::SourceFromFile::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
        } => LiveSourceSpec::Soapy(crate::source_spec::SoapyOpts::from_file(
            driver, *freq_hz, *rate_hz, *gain_db,
        )?),
        config::SourceFromFile::Hpsdr {
            host,
            mac,
            port,
            protocol,
            freq_hz,
            rate_hz,
            auto_attenuation,
            ..
        } => LiveSourceSpec::Hpsdr(HpsdrOpts {
            host: host.clone(),
            mac: mac
                .as_deref()
                .map(parse_hpsdr_mac)
                .transpose()
                .map_err(anyhow::Error::msg)?,
            port: Some(*port),
            protocol: *protocol,
            freq: Some(*freq_hz),
            rate: Some(*rate_hz),
            // MAN-308: built once here; every reopen clones the spec and so
            // shares it.
            auto_attenuation: auto_attenuation.then(|| {
                std::sync::Arc::new(manta_input::hpsdr::AutoAttenuation::new(Box::new(|c| {
                    logging::note(&attenuation_line(c))
                })))
            }),
        }),
        config::SourceFromFile::Ka9q {
            group,
            port,
            ssrc,
            interface,
        } => LiveSourceSpec::Ka9q(Ka9qOpts {
            group: Some(group.clone()),
            port: *port,
            ssrc: *ssrc,
            interface: *interface,
            ..Ka9qOpts::unset()
        }),
    })
}

/// MAN-269 D3/D4: the CLI's `--realtime`/`--loop` ORed with the config file
/// source's `realtime`/`loop` (`file_source` is `Some` only when no CLI flag
/// selected the source), then refused unless the resolved source is a file.
/// Runs inside `prepare_live`, before any source I/O.
fn resolve_replay_mode(
    cli_realtime: bool,
    cli_loop: bool,
    file_source: Option<&config::SourceFromFile>,
    spec: &LiveSourceSpec,
) -> Result<ReplayMode> {
    let (file_realtime, file_loop) = match file_source {
        Some(config::SourceFromFile::File {
            realtime, looped, ..
        }) => (*realtime, *looped),
        _ => (false, false),
    };
    let replay = ReplayMode::from_flags(cli_realtime || file_realtime, cli_loop || file_loop);
    if replay != ReplayMode::default() && !matches!(spec, LiveSourceSpec::File { .. }) {
        // `validate_input` already refuses realtime/loop on a non-file
        // [input], so only a CLI flag can get here.
        if cli_realtime {
            bail!(
                "--realtime only applies to a file replay (--source, or input.type = \"file\"); \
                 a live source already runs in real time"
            );
        }
        bail!(
            "--loop only applies to a file replay (--source, or input.type = \"file\"); a live \
             source never reaches an end to loop from"
        );
    }
    Ok(replay)
}

/// `soak`/`doctor`'s note for config-file realtime/loop they do not apply
/// (MAN-269 D5); `None` when neither is set.
pub(crate) fn replay_mode_ignored_note(
    command: &str,
    replay: ReplayMode,
    origin: &str,
) -> Option<String> {
    (replay != ReplayMode::default()).then(|| {
        format!(
            "note: {command} replays files as fast as it can; ignoring \
             input.realtime/input.loop from {origin}"
        )
    })
}

/// MAN-309: `record_iq` takes effect only in `run`; another command that
/// reads the input says so instead of recording. `None` when unset.
pub(crate) fn record_iq_ignored_note(command: &str, loaded: &config::Loaded) -> Option<String> {
    let set = loaded.input.shared.record_iq.is_some()
        || loaded
            .inputs
            .iter()
            .any(|n| n.input.shared.record_iq.is_some());
    set.then(|| {
        format!(
            "note: {command} does not record IQ; ignoring input.record_iq from {}",
            loaded.origin
        )
    })
}

/// Wraps an opened file replay for `mode` (MAN-269 D8): the loop sits inside
/// the pacing, so one clock spans every pass. `reopen` opens each later pass.
/// The default mode returns `first` unchanged. `stop` ends a pacing wait
/// early (MAN-453).
pub(crate) fn wrap_file_replay(
    first: Box<dyn IqSource>,
    mode: ReplayMode,
    label: String,
    reopen: manta_input::Reopen,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Box<dyn IqSource>> {
    let src: Box<dyn IqSource> = if mode.looped {
        Box::new(manta_input::LoopingSource::new(label, first, reopen))
    } else {
        first
    };
    Ok(if mode.paced {
        Box::new(manta_input::PacedSource::new(src)?.with_stop_flag(stop))
    } else {
        src
    })
}

/// `run`'s dial guard (MAN-34): with `[server]`, a receiver that reports no
/// real RF frequency needs a dial frequency, or every spot is a baseband
/// offset. Shared with `manta doctor`'s `config` check (MAN-126).
pub(crate) fn needs_dial_freq(has_server: bool, rf_aware: bool, dial_freq_hz: Option<f64>) -> bool {
    has_server && !rf_aware && dial_freq_hz.is_none()
}

/// MAN-269 review (P2): `is_rf_aware()` reads an IQ WAV's sidecar by
/// opening it and maps any open error to "no RF frequency". For a file
/// that exists, that error (an unsupported sample rate, a malformed
/// header) is the real problem, so a dial guard calls this first to
/// report it rather than ask for a dial frequency that would not help.
pub(crate) fn report_iq_open_error(spec: &LiveSourceSpec, dial_freq_hz: Option<f64>) -> Result<()> {
    if let LiveSourceSpec::File {
        path,
        source_iq: true,
        raw,
    } = spec
    {
        if path.exists() {
            open_iq_source_file(path, *raw, dial_freq_hz)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
