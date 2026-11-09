//! `manta calibrate`: measure the receiver's frequency error against a
//! known carrier and, once the operator confirms, save the
//! `freq_correction_ppm` that cancels it to `[input]` in their config file.

// MAN-127. The measurement is manta_engine::calibrate; the file edit is
// config_edit.rs. See docs/DECISIONS/2026-10-11-man127-calibrate-command.md.

use crate::{
    config, config_edit, prepare_source, resolve::CliOverrides, resolve::Resolved,
    source_spec::LiveSourceSpec, SourcePrepared,
};
use anyhow::{bail, ensure, Result};
use manta_engine::calibrate::{
    check_duration, plan_references, CalibrateOptions, CalibrationReport, PlannedReference,
    ReferenceResult, ReferenceStatus, MIN_AMBIGUITY_MARGIN_DB, MIN_VALID_SEGMENTS, SPREAD_WARN_HZ,
};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PPM_ENV_VAR: &str = "MANTA_INPUT_FREQ_CORRECTION_PPM";
/// Every environment variable with this prefix shapes the input `[input]`
/// describes; `calibrate --write` refuses when any but [`PPM_ENV_VAR`] is
/// set. Matched by prefix, not listed, so a variable added later is covered.
const INPUT_ENV_PREFIX: &str = "MANTA_INPUT_";

/// Everything `manta calibrate` was given on the command line.
pub(crate) struct CalibrateArgs {
    pub duration_secs: u64,
    pub reference_hz: Option<f64>,
    pub search_ppm: f64,
    pub tune_hz: Option<f64>,
    pub write: bool,
    pub json: bool,
    pub config: Option<PathBuf>,
    pub cli: CliOverrides,
    /// MAN-13: `--input NAME`, the `[[input]]` entry to measure.
    pub input: Option<String>,
    /// MAN-541: take the correction from a running daemon's RBN
    /// comparison instead of measuring a carrier.
    pub from_rbn: bool,
    /// MAN-541: with `--from-rbn`, the daemon's metrics listener.
    pub addr: Option<String>,
}

/// What happens to a measured value.
#[derive(Debug, PartialEq)]
pub(crate) enum SaveDecision {
    /// No config file was given.
    NoConfig,
    /// Input overrides (named, comma-separated) were in effect, so the
    /// measurement may not be of the input the file describes.
    Overridden(String),
    /// The file already holds the value.
    AlreadySet,
    /// Save without asking (`--write`).
    Save,
    /// `--json` without `--write`: report only.
    Skip,
    /// Nobody to ask.
    NotInteractive,
    /// Ask on the terminal.
    Ask,
}

/// The save rule, in priority order.
pub(crate) fn save_decision(
    has_config: bool,
    overrides: &[String],
    already_set: bool,
    write: bool,
    json: bool,
    interactive: bool,
) -> SaveDecision {
    if !has_config {
        SaveDecision::NoConfig
    } else if !overrides.is_empty() {
        SaveDecision::Overridden(overrides.join(", "))
    } else if already_set {
        SaveDecision::AlreadySet
    } else if write {
        SaveDecision::Save
    } else if json {
        SaveDecision::Skip
    } else if !interactive {
        SaveDecision::NotInteractive
    } else {
        SaveDecision::Ask
    }
}

/// Write `prompt` and read one answer: true only for `y` or `yes`, in any
/// case, surrounded by any whitespace. End of input is a no.
pub(crate) fn confirm(out: &mut impl Write, input: &mut impl BufRead, prompt: &str) -> bool {
    let _ = write!(out, "{prompt} [y/N] ");
    let _ = out.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

/// The value that is saved: rounded to 0.01 ppm, never `-0.0`. One copy,
/// shared with the daemon's RBN skew suggestion (MAN-541).
pub(crate) use manta_spot::round_ppm;

/// Whole dB, half away from zero, never `-0`.
fn db(v: f64) -> String {
    let r = v.round();
    format!("{}", if r == 0.0 { 0.0 } else { r })
}

/// Apply `--tune-hz` to the configured receiver's spec. A dial-frequency
/// override describes the old tuning, so it is dropped with it.
pub(crate) fn apply_tune_hz(resolved: &mut Resolved, tune_hz: f64) -> Result<()> {
    match &mut resolved.spec {
        LiveSourceSpec::Kiwi(kiwi) => kiwi.freq = Some(tune_hz),
        LiveSourceSpec::Soapy(soapy) => soapy.freq = Some(tune_hz),
        LiveSourceSpec::Hpsdr(hpsdr) => {
            hpsdr.freq = Some(
                crate::cli_parse::check_hpsdr_freq_hz("--tune-hz", tune_hz)
                    .map_err(|e| anyhow::anyhow!(e))?,
            )
        }
        // MAN-140 D17: manta joins a radiod channel, it never retunes one.
        LiveSourceSpec::Ka9q(_) => bail!(
            "--tune-hz cannot retune a ka9q-radio channel; retune it in radiod so the reference \
             sits inside its passband, then run calibrate without --tune-hz"
        ),
        LiveSourceSpec::AudioDevice(_) => bail!(
            "--tune-hz cannot retune a sound card; tune the radio so the reference sits 1 to 2 \
             kHz above its dial and pass --dial-freq-hz"
        ),
        LiveSourceSpec::File { .. } => bail!(
            "--tune-hz cannot retune a recording; record the receiver tuned near a reference \
             instead, or pass --reference-hz for a carrier the recording holds"
        ),
    }
    resolved.dial_freq_hz = None;
    Ok(())
}

/// Where the value `run` with the same flags would apply comes from.
fn configured_origin(
    resolved: &Resolved,
    loaded: &config::Loaded,
    config_path: Option<&Path>,
    file_value: Option<f64>,
) -> String {
    if resolved.replaced_file_source.is_none() && loaded.env_vars.iter().any(|v| v == PPM_ENV_VAR) {
        format!("from {PPM_ENV_VAR}")
    } else if let (None, Some(path), Some(_)) =
        (resolved.replaced_file_source, config_path, file_value)
    {
        format!("from {}", path.display())
    } else {
        "nothing set".to_string()
    }
}

fn reference_name(r: &ReferenceResult) -> String {
    format!("{:.1} Hz, {}", r.reference_hz, r.label)
}

fn segments_text(r: &ReferenceResult) -> String {
    format!(
        "carrier in {} of {} one-second segments",
        r.segments_valid, r.segments_total
    )
}

/// Why `r` was not measured.
fn failure_text(r: &ReferenceResult) -> String {
    match r.status {
        ReferenceStatus::Ambiguous if r.peaks_disagree => format!(
            "two carriers alternate in the ±{:.1} Hz search window: most one-second segments \
             put it at {:+.1} Hz, the average over the run at {:+.1} Hz; pass --reference-hz \
             to use another reference, or measure at another time",
            r.search_half_width_hz,
            r.peak_offset_hz.unwrap_or(0.0),
            r.runner_up_offset_hz.unwrap_or(0.0)
        ),
        ReferenceStatus::Ambiguous => format!(
            "two carriers within {MIN_AMBIGUITY_MARGIN_DB:.0} dB of each other in the \
             ±{:.1} Hz search window ({:+.1} Hz and {:+.1} Hz); narrow --search-ppm or pass \
             --reference-hz",
            r.search_half_width_hz,
            r.peak_offset_hz.unwrap_or(0.0),
            r.runner_up_offset_hz.unwrap_or(0.0)
        ),
        _ => format!("{}, need {MIN_VALID_SEGMENTS}", segments_text(r)),
    }
}

/// The human report: everything but the prompt and the saved/not-saved line.
pub(crate) fn render_human(
    report: &CalibrationReport,
    configured_ppm: f64,
    configured_origin: &str,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!(
        "source: {:.0} Hz sample rate, centre {:.1} Hz, observed for {:.1} s",
        report.sample_rate_hz, report.center_freq_hz, report.duration_s
    ));
    let result = report.result();
    match result {
        Some(r) => {
            let error = r.error_hz.unwrap_or(0.0);
            line(format!("reference: {}", reference_name(r)));
            line(format!(
                "measured: {:.1} Hz, the receiver reads {:.1} Hz {}",
                r.measured_hz.unwrap_or(r.reference_hz),
                error.abs(),
                if error < 0.0 { "low" } else { "high" }
            ));
            line(format!(
                "correction: freq_correction_ppm = {:+.2} (±{:.2} ppm)",
                round_ppm(r.freq_correction_ppm.unwrap_or(0.0)),
                r.uncertainty_ppm.unwrap_or(0.0).max(0.01)
            ));
            line(format!(
                "evidence: {}, {} dB over the noise floor, spread {:.1} Hz",
                segments_text(r),
                db(r.peak_over_floor_db.unwrap_or(0.0)),
                r.spread_hz.unwrap_or(0.0)
            ));
            for other in report.references.iter().filter(|o| !std::ptr::eq(*o, r)) {
                match other.freq_correction_ppm {
                    Some(ppm) => line(format!(
                        "cross-check: {}: {:+.2} ppm, {}",
                        reference_name(other),
                        round_ppm(ppm),
                        segments_text(other)
                    )),
                    None => line(format!(
                        "cross-check: {}: not measured, {}",
                        reference_name(other),
                        failure_text(other)
                    )),
                }
            }
        }
        None => {
            line("reference: none measured".to_string());
            for r in &report.references {
                line(format!("tried: {}: {}", reference_name(r), failure_text(r)));
            }
        }
    }
    let configured = if configured_ppm == 0.0 {
        0.0
    } else {
        configured_ppm
    };
    line(format!(
        "configured: {configured:.2} ppm, {configured_origin}"
    ));
    out
}

/// The `--json` report: the engine's report plus the outcome.
fn render_json(
    report: &CalibrationReport,
    configured_ppm: f64,
    config_path: Option<&Path>,
    written: bool,
) -> Result<String> {
    let mut value = serde_json::to_value(report)?;
    if let serde_json::Value::Object(ref mut map) = value {
        let result = match report.result() {
            Some(r) => serde_json::json!({
                "reference_hz": r.reference_hz,
                "freq_correction_ppm": round_ppm(r.freq_correction_ppm.unwrap_or(0.0)),
            }),
            None => serde_json::Value::Null,
        };
        map.insert("result".to_string(), result);
        map.insert(
            "configured_ppm".to_string(),
            serde_json::json!(configured_ppm),
        );
        map.insert(
            "config_path".to_string(),
            serde_json::json!(config_path.map(|p| p.display().to_string())),
        );
        map.insert("written".to_string(), serde_json::json!(written));
    }
    Ok(serde_json::to_string(&value)?)
}

fn measuring_line(planned: &[PlannedReference], duration: Duration) -> String {
    let name = |p: &PlannedReference| format!("{:.1} Hz, {}", p.reference.hz, p.reference.label);
    let mut s = format!(
        "measuring {} for {} s",
        name(&planned[0]),
        duration.as_secs()
    );
    if planned.len() > 1 {
        let rest: Vec<String> = planned[1..].iter().map(name).collect();
        s.push_str(&format!(", cross-checking {}", rest.join("; ")));
    }
    s
}

fn is_audio(spec: &LiveSourceSpec) -> bool {
    match spec {
        LiveSourceSpec::AudioDevice(_) => true,
        LiveSourceSpec::File { source_iq, .. } => !source_iq,
        _ => false,
    }
}

/// MAN-127 Decision 11, MAN-334: plan on `configured` -- the centre and
/// passband the configuration gives, when it gives them -- before `open`
/// touches the receiver, so a passband that holds no reference gets the
/// retune hint and no receiver I/O. Then plan again on what the opened
/// source reports: a SoapySDR driver may round the centre or rate, and a
/// ka9q-radio channel or a recording only says once open. The second plan
/// is the one measured.
fn open_planned(
    configured: Option<(f64, (f64, f64))>,
    is_audio: bool,
    opts: &CalibrateOptions,
    open: impl FnOnce() -> Result<crate::reconnect::SendSource>,
) -> Result<(crate::reconnect::SendSource, Vec<PlannedReference>)> {
    if let Some((center_hz, passband)) = configured {
        plan_references(center_hz, passband, is_audio, opts)?;
    }
    let src = open()?;
    let planned = plan_references(src.center_freq_hz(), src.rf_passband_hz(), is_audio, opts)?;
    Ok((src, planned))
}

/// Every input override in effect, deny-by-default: each command-line flag
/// that changes the source, receiver, centre/dial frequency, IQ mode, sample
/// rate, device or file, and every `MANTA_INPUT_*` variable but
/// [`PPM_ENV_VAR`]. A measurement taken with any of them may not be of the
/// input the config opens, so it is never saved there (MAN-127).
///
/// `CliOverrides` is destructured without `..`, so a field added to it does
/// not compile until it is classified here.
fn input_overrides(cli: &CliOverrides, vars: &[(OsString, OsString)]) -> Vec<String> {
    let CliOverrides {
        prior: _,
        device,
        source,
        source_iq,
        kiwi,
        soapy,
        hpsdr,
        ka9q,
        dial_freq_hz,
        capture_rate_hz,
        replay_epoch,
        realtime,
        loop_replay,
        // Exempt: the value being calibrated (calibrate has no such flag).
        freq_correction_ppm: _,
        // Exempt: spot filtering after decoding, not the input (calibrate
        // has none of these flags).
        allowlist: _,
        blocklist: _,
        notch: _,
        cty: _,
        scp: _,
        cty_sha256: _,
        scp_sha256: _,
        patterns: _,
        // Exempt: RBN comparison grades spots after decoding, not the
        // input (calibrate has no --rbn-compare/--rbn-spotter flags).
        rbn: _,
    } = cli;
    let mut found: Vec<String> = Vec::new();
    let mut flag = |set: bool, name: &str| {
        if set {
            found.push(name.to_string());
        }
    };
    flag(device.is_some(), "--device");
    flag(source.is_some(), "--source");
    flag(*source_iq, "--source-iq");
    // --kiwi-port and --kiwi-password require --kiwi-host, so the host
    // stands for them.
    flag(kiwi.host.is_some(), "--kiwi-host");
    flag(kiwi.freq.is_some(), "--kiwi-freq-hz");
    // Every other --soapy-* flag requires the driver.
    flag(soapy.driver.is_some(), "--soapy-driver");
    flag(hpsdr.host.is_some(), "--hpsdr-host");
    flag(hpsdr.mac.is_some(), "--hpsdr-mac");
    flag(hpsdr.freq.is_some(), "--hpsdr-freq-hz");
    flag(hpsdr.rate.is_some(), "--hpsdr-rate-hz");
    flag(hpsdr.port.is_some(), "--hpsdr-port");
    // --ka9q-port, --ka9q-ssrc and --ka9q-interface require --ka9q-group.
    flag(ka9q.group.is_some(), "--ka9q-group");
    flag(dial_freq_hz.is_some(), "--dial-freq-hz");
    flag(capture_rate_hz.is_some(), "--capture-rate-hz");
    flag(replay_epoch.is_some(), "--replay-epoch");
    // `run`'s file pacing (MAN-269); calibrate has no such flags today.
    flag(*realtime, "--realtime");
    flag(*loop_replay, "--loop");
    // Not overrides, each by design: --duration, --reference-hz and
    // --search-ppm shape the measurement, not the input; --json and --write
    // shape the output; --config names the file being saved to. --tune-hz
    // retunes the configured receiver itself (the source reports the centre
    // it actually tuned, nothing is relabelled), and a ppm correction is a
    // property of the receiver's oscillator that holds at every frequency,
    // so retuning to reach a reference does not change what is measured.
    let mut env: Vec<String> = vars
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| k.starts_with(INPUT_ENV_PREFIX) && k != PPM_ENV_VAR)
        .collect();
    env.sort();
    env.dedup();
    found.extend(env);
    found
}

pub(crate) fn run(args: CalibrateArgs) -> Result<()> {
    if args.from_rbn {
        return run_from_rbn(args);
    }
    let duration = Duration::from_secs(args.duration_secs);
    check_duration(duration)?;
    let vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    // Before the config is even loaded, so an unknown MANTA_INPUT_* name is
    // refused for what it is.
    let overrides = input_overrides(&args.cli, &vars);
    let config_flag_or_env = args
        .config
        .clone()
        .or_else(|| config::config_path_from_env(&vars));
    if args.write && config_flag_or_env.is_none() {
        bail!("--write needs a config file: pass --config or set MANTA_CONFIG");
    }
    if args.write && !overrides.is_empty() {
        bail!(
            "--write refuses to save a measurement taken with input overrides: {}. The saved \
             value would apply to the input the config describes, not the one measured; \
             persist these settings in [input] of the config file, then re-run calibrate \
             without them",
            overrides.join(", ")
        );
    }
    // Snapshot the config before and after prepare_source loads it, so the
    // receiver `resolved` describes and the text the save is checked against
    // come from the same file and bytes.
    let snapshot_before = config_flag_or_env.map(|path| config_edit::ReadBefore::read(&path).ok());
    let SourcePrepared {
        config_path,
        loaded,
        mut resolved,
    } = prepare_source(
        args.cli,
        args.config,
        crate::OneInput {
            command: "calibrate",
            name: args.input.as_deref(),
        },
    )?;
    // MAN-309: only `run` records; say so rather than ignore it silently.
    if let Some(note) = crate::resolve::record_iq_ignored_note("calibrate", &loaded) {
        crate::logging::note(&note);
    }
    // MAN-310: a stream of a band plan saves into the entry it came from.
    let input_name = save_entry_name(&loaded.inputs, args.input.as_deref());
    let opts = CalibrateOptions {
        duration,
        search_ppm: args.search_ppm,
        reference_hz: args.reference_hz,
        live: !matches!(resolved.spec, LiveSourceSpec::File { .. }),
    };
    // The config's canonical target and text before measuring. The file's
    // own value comes from that text, and the save refuses a path that no
    // longer resolves to that file holding it: a config rewritten or
    // retargeted during the measurement may describe another receiver.
    let read_before = config_path
        .as_deref()
        .map(config_edit::ReadBefore::read)
        .transpose()?;
    if let (Some(path), Some(before)) = (config_path.as_deref(), snapshot_before) {
        ensure!(
            before.as_ref() == read_before.as_ref(),
            "config file {} changed while calibrate was loading it; re-run calibrate",
            path.display()
        );
    }
    let config_text = read_before.as_ref().map(|r| r.text.as_str());
    if let Some(tune_hz) = args.tune_hz {
        apply_tune_hz(&mut resolved, tune_hz)?;
    }
    let spec = &resolved.spec;
    if !spec.is_rf_aware() && resolved.dial_freq_hz.is_none() {
        bail!(
            "calibrate needs the receiver's absolute frequency; pass --dial-freq-hz with the \
             radio's dial frequency"
        );
    }
    let (src, planned) = open_planned(
        spec.configured_center_and_passband(resolved.capture_rate_hz, resolved.dial_freq_hz),
        is_audio(spec),
        &opts,
        || spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz),
    )?;
    eprintln!("{}", measuring_line(&planned, duration));
    let report = manta_engine::calibrate(src, &planned, &opts)?;
    if report.bad_samples > 0 {
        eprintln!(
            "{}",
            crate::fmt::bad_samples_warning(&spec.input_label(), report.bad_samples)
        );
    }

    // The file's own value, read from its text: the loaded config already
    // carries the environment overlay. None when a source flag replaced the
    // file's receiver: the value is not the measured receiver's, and with
    // several [[input]] entries there is no entry to read it from (MAN-392).
    let file_value = match (config_text, resolved.replaced_file_source) {
        (Some(text), None) => {
            config_edit::set_freq_correction_ppm(text, 0.0, input_name.as_deref())?.1
        }
        _ => None,
    };
    let origin = configured_origin(&resolved, &loaded, config_path.as_deref(), file_value);
    let configured = resolved.freq_correction_ppm;
    let measured = report
        .result()
        .and_then(|r| r.freq_correction_ppm)
        .map(round_ppm);

    if !args.json {
        print!("{}", render_human(&report, configured, &origin));
    }
    let mut written = false;
    let mut save_error = None;
    if let Some(value) = measured {
        if let Some(spread) = report.result().and_then(|r| r.spread_hz) {
            if spread > SPREAD_WARN_HZ {
                eprintln!(
                    "warning: the measured frequency wandered {spread:.1} Hz across the run; the \
                     receiver may still be warming up, or another signal shares the search window"
                );
            }
        }
        if let Some(path) = config_path.as_deref() {
            if overrides.is_empty() && loaded.env_vars.iter().any(|v| v == PPM_ENV_VAR) {
                eprintln!(
                    "warning: {PPM_ENV_VAR} is set and overrides freq_correction_ppm from {} for \
                     run, soak and doctor; unset it to use the saved value",
                    path.display()
                );
            }
        }
        let outcome = offer_save(
            &SaveContext {
                value,
                config_path: config_path.as_deref(),
                config_text,
                read_before: read_before.as_ref(),
                file_value,
                overrides: &overrides,
                input_name: input_name.as_deref(),
                write: args.write,
                json: args.json,
            },
            &mut written,
            &mut save_error,
        );
        if let (Some(line), false) = (outcome, args.json) {
            println!("{line}");
        }
    }
    if args.json {
        println!(
            "{}",
            render_json(&report, configured, config_path.as_deref(), written)?
        );
    }
    if let Some(e) = save_error {
        return Err(e);
    }
    if measured.is_none() {
        bail!(
            "no reference carrier was measured; try a longer --duration, another time of day, \
             or a reference nearer your receiver"
        );
    }
    Ok(())
}

/// What [`offer_save`] needs to save a value the way `calibrate` saves it.
struct SaveContext<'a> {
    value: f64,
    config_path: Option<&'a Path>,
    config_text: Option<&'a str>,
    read_before: Option<&'a config_edit::ReadBefore>,
    file_value: Option<f64>,
    overrides: &'a [String],
    input_name: Option<&'a str>,
    write: bool,
    json: bool,
}

/// The save tail both sources share (the WWV measurement and, MAN-541,
/// the daemon's RBN reading): the decision, the prompt, the save and the
/// outcome line to print (`None`: nothing to say).
fn offer_save(
    ctx: &SaveContext<'_>,
    written: &mut bool,
    save_error: &mut Option<anyhow::Error>,
) -> Option<String> {
    let value = ctx.value;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let decision = save_decision(
        ctx.config_path.is_some(),
        ctx.overrides,
        ctx.file_value == Some(value),
        ctx.write,
        ctx.json,
        interactive,
    );
    // The TOML literal, as saved: `2.0`, never `2`.
    let value_lit = config_edit::literal(value);
    let run_hint = format!("pass --freq-correction-ppm {value_lit} to manta run");
    // MAN-13: `[input]`, or the `[[input]]` entry `--input` named.
    let entry = Entry {
        name: ctx.input_name,
        section: ctx.config_text.map_or_else(
            || "[input]".to_string(),
            |text| config_edit::input_section(text, ctx.input_name),
        ),
    };
    match (decision, ctx.config_path) {
        (SaveDecision::Save, Some(path)) => Some(save(
            path,
            value,
            ctx.read_before,
            &entry,
            written,
            save_error,
        )),
        (SaveDecision::Ask, Some(path)) => {
            let streams = entry_stream_count(path, ctx.input_name);
            let prompt = format!(
                "Save freq_correction_ppm = {value_lit} to {} in {}{}?",
                entry.section,
                path.display(),
                match streams {
                    n if n > 1 => format!(" (all {n} streams of this receiver)"),
                    _ => String::new(),
                }
            );
            if confirm(
                &mut std::io::stderr(),
                &mut std::io::stdin().lock(),
                &prompt,
            ) {
                Some(save(
                    path,
                    value,
                    ctx.read_before,
                    &entry,
                    written,
                    save_error,
                ))
            } else {
                Some("not saved: answered no".to_string())
            }
        }
        (SaveDecision::Skip, _) => None,
        (SaveDecision::AlreadySet, Some(path)) => Some(format!(
            "not saved: {} already has freq_correction_ppm = {value_lit}",
            path.display()
        )),
        (SaveDecision::Overridden(list), Some(path)) => Some(format!(
            "not saved: input overrides were in effect ({list}); persist them in [input] of \
             {} and re-run calibrate without them, or {run_hint} with the same flags and \
             environment",
            path.display()
        )),
        (SaveDecision::NotInteractive, _) => Some(format!(
            "not saved: no terminal to confirm on; re-run with --write to save it, or \
             {run_hint}"
        )),
        _ => Some(format!(
            "not saved: no config file; pass --config or set MANTA_CONFIG to save it, or \
             {run_hint}"
        )),
    }
}

// ---- MAN-541: `calibrate --from-rbn` ----

/// How the report names a daemon row's config entry.
fn entry_label(entry: Option<&str>) -> String {
    match entry {
        None => "[input]".to_string(),
        Some(name) => format!("the [[input]] entry {}", crate::fmt::quoted(name)),
    }
}

/// The daemon's row for the config entry being calibrated. With a config,
/// `config_entry` is the entry `save_entry_name` chose (`None`: the
/// `[input]` table). Without one, `--input` is matched against the entry
/// or its stream names, and a lone row needs no `--input`.
pub(crate) fn pick_entry<'a>(
    rows: &'a [manta_server::status_doc::RbnSkewEntry],
    config_entry: Option<Option<&str>>,
    input: Option<&str>,
    daemon: &str,
    config_path: Option<&Path>,
) -> Result<&'a manta_server::status_doc::RbnSkewEntry> {
    let reported = || {
        rows.iter()
            .map(|r| entry_label(r.entry.as_deref()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if rows.is_empty() {
        bail!(
            "the daemon at {daemon} has not registered its inputs yet; wait a moment and run \
             calibrate --from-rbn again"
        );
    }
    let found = match (config_entry, input) {
        (Some(entry), _) => rows.iter().find(|r| r.entry.as_deref() == entry),
        (None, Some(name)) => rows
            .iter()
            .find(|r| r.entry.as_deref() == Some(name) || r.names.iter().any(|n| n == name)),
        (None, None) if rows.len() == 1 => rows.first(),
        (None, None) => bail!(
            "the daemon at {daemon} reports several inputs ({}); pass --input NAME",
            reported()
        ),
    };
    found.ok_or_else(|| {
        let wanted = match (config_entry, input) {
            (Some(entry), _) => entry_label(entry),
            (None, name) => entry_label(name),
        };
        let hint = match config_path {
            Some(path) => format!("point --addr at the daemon that runs {}", path.display()),
            None => "point --addr at the daemon that runs it".to_string(),
        };
        anyhow::anyhow!(
            "the daemon at {daemon} reports no RBN reading for {wanted}; it reports {}; {hint}",
            reported()
        )
    })
}

/// The `--from-rbn` report through the `correction:` line.
pub(crate) fn render_rbn_human(
    daemon: &str,
    window_seconds: u64,
    min_samples: usize,
    row: &manta_server::status_doc::RbnSkewEntry,
    section: &str,
) -> String {
    use manta_server::rbn_compare::skew::{offset_text, skew_text};
    let mut out = format!(
        "source: RBN comparison of the daemon at {daemon} over the last {window_seconds} s, for \
         {section}\n"
    );
    for b in &row.bands {
        let band = manta_server::status_doc::escape_for_terminal(&b.band);
        match (b.median_ppm, b.ppm_error) {
            (Some(ppm), Some(err)) => out.push_str(&format!(
                "evidence: {band}: {}, median {} over {} spots\n",
                skew_text(ppm, err),
                offset_text(b.median_offset_hz),
                b.samples
            )),
            _ => out.push_str(&format!(
                "evidence: {band}: median {} over {} spots, needs {min_samples}\n",
                offset_text(b.median_offset_hz),
                b.samples
            )),
        }
    }
    if let (Some(observed), Some(err), Some(set)) =
        (row.observed_ppm, row.ppm_error, row.suggested_ppm)
    {
        out.push_str(&format!(
            "measured: the receiver reads {:.2} ppm {} against the RBN, with {:.2} ppm applied\n",
            observed.abs(),
            if observed < 0.0 { "low" } else { "high" },
            row.applied_ppm
        ));
        out.push_str(&format!(
            "correction: freq_correction_ppm = {} (±{err:.2} ppm)\n",
            config_edit::literal(set)
        ));
    }
    out
}

/// The `--from-rbn --json` object.
fn render_rbn_json(
    daemon: &str,
    window_seconds: u64,
    row: &manta_server::status_doc::RbnSkewEntry,
    saveable: Option<f64>,
    configured_ppm: f64,
    config_path: Option<&Path>,
    written: bool,
) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "source": "rbn",
        "daemon": daemon,
        "window_seconds": window_seconds,
        "input": row.entry,
        "applied_ppm": row.applied_ppm,
        "bands": row.bands,
        "observed_ppm": row.observed_ppm,
        "ppm_error": row.ppm_error,
        "result": saveable.map(|v| serde_json::json!({ "freq_correction_ppm": v })),
        "configured_ppm": configured_ppm,
        "config_path": config_path.map(|p| p.display().to_string()),
        "written": written,
    }))?)
}

/// `calibrate --from-rbn`: reads the daemon's `GET /status`, opens no
/// receiver, and saves its suggestion through the WWV path's save tail.
fn run_from_rbn(args: CalibrateArgs) -> Result<()> {
    use manta_server::rbn_compare::skew::MAX_SAVE_ERROR_PPM;
    let vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let config_path = args
        .config
        .clone()
        .or_else(|| config::config_path_from_env(&vars));
    if args.write && config_path.is_none() {
        bail!("--write needs a config file: pass --config or set MANTA_CONFIG");
    }
    // The config entry to save into, its text and its own value.
    let mut read_before = None;
    let mut file_value = None;
    let mut input_name = None;
    // The entry the daemon reports the selected input under: `None` for
    // the `[input]` table, else its `[[input]]` entry (`NamedInput::entry`,
    // as `run` registers it), which a lone `[[input]]` entry has even
    // without `--input`.
    let mut daemon_entry: Option<String> = None;
    let mut env_vars: Vec<String> = Vec::new();
    let mut configured_ppm = 0.0;
    if let Some(path) = config_path.as_deref() {
        let before = config_edit::ReadBefore::read(path)?;
        let mut loaded = config::load(Some(path), config::Env::Read(&vars))?;
        crate::resolve::select_input(&mut loaded, args.input.as_deref(), "calibrate")?;
        input_name = save_entry_name(&loaded.inputs, args.input.as_deref());
        daemon_entry = loaded.inputs.first().and_then(|n| n.entry.clone());
        file_value =
            config_edit::set_freq_correction_ppm(&before.text, 0.0, input_name.as_deref())?.1;
        configured_ppm = loaded
            .inputs
            .first()
            .and_then(|n| n.input.shared.freq_correction_ppm)
            .unwrap_or(0.0);
        env_vars = loaded.env_vars.clone();
        read_before = Some(before);
    }
    let server = if args.addr.is_some() {
        None
    } else {
        crate::daemon_client::daemon_server_config(config_path.as_deref())?.1
    };
    let daemon = crate::daemon_client::format_addrs(&crate::daemon_client::resolve_status_addr(
        args.addr.as_deref(),
        server.as_ref(),
    )?);
    let reply = crate::daemon_client::run_status(config_path.as_deref(), args.addr.as_deref(), 5)?;
    let Some(rc) = reply.doc.rbn_compare.as_ref() else {
        bail!(
            "the daemon at {daemon} is not comparing its spots with the RBN; enable \
             [rbn_compare] (or run it with --rbn-compare), let it grade spots for an hour, then \
             run calibrate --from-rbn again"
        );
    };
    let row = pick_entry(
        &rc.inputs,
        config_path.as_ref().map(|_| daemon_entry.as_deref()),
        args.input.as_deref(),
        &daemon,
        config_path.as_deref(),
    )?;
    let config_text = read_before.as_ref().map(|r| r.text.as_str());
    let section = config_text.map_or_else(
        || entry_label(row.entry.as_deref()),
        |text| config_edit::input_section(text, input_name.as_deref()),
    );
    if args.write && row.replaced_by_flag {
        bail!(
            "--write refuses to save the RBN reading of a daemon whose receiver a command-line \
             source flag chose: the saved value would apply to the receiver {section} of {} \
             describes, not the one the daemon measured; run the daemon from the config, then \
             re-run calibrate --from-rbn",
            config_path
                .as_deref()
                .map_or_else(String::new, |p| p.display().to_string())
        );
    }
    // The value comes from a peer: never save one `run` would refuse.
    if let Some(v) = row.suggested_ppm {
        ensure!(
            v.is_finite() && v.abs() <= manta_spot::MAX_ABS_PPM,
            "the daemon at {daemon} suggests freq_correction_ppm = {v}, outside the supported \
             range [-{max}, {max}]; nothing saved",
            max = manta_spot::MAX_ABS_PPM
        );
    }
    let saveable = match (row.suggested_ppm, row.ppm_error) {
        (Some(v), Some(err)) if err <= MAX_SAVE_ERROR_PPM => Some(v),
        _ => None,
    };
    if !args.json {
        print!(
            "{}",
            render_rbn_human(&daemon, rc.window_seconds, rc.min_samples, row, &section)
        );
    }
    let mut written = false;
    let mut save_error = None;
    if let Some(value) = saveable {
        let origin = if env_vars.iter().any(|v| v == PPM_ENV_VAR) {
            format!("from {PPM_ENV_VAR}")
        } else if let (Some(path), Some(_)) = (config_path.as_deref(), file_value) {
            format!("from {}", path.display())
        } else {
            "nothing set".to_string()
        };
        if !args.json {
            println!("configured: {configured_ppm:.2} ppm, {origin}");
        }
        if let Some(path) = config_path.as_deref() {
            if env_vars.iter().any(|v| v == PPM_ENV_VAR) {
                eprintln!(
                    "warning: {PPM_ENV_VAR} is set and overrides freq_correction_ppm from {} for \
                     run, soak and doctor; unset it to use the saved value",
                    path.display()
                );
            }
        }
        let outcome = if row.replaced_by_flag {
            Some(format!(
                "not saved: the daemon's receiver came from a command-line source flag, not \
                 from {}",
                config_path
                    .as_deref()
                    .map_or_else(|| "a config file".to_string(), |p| p.display().to_string())
            ))
        } else {
            offer_save(
                &SaveContext {
                    value,
                    config_path: config_path.as_deref(),
                    config_text,
                    read_before: read_before.as_ref(),
                    file_value,
                    overrides: &[],
                    input_name: input_name.as_deref(),
                    write: args.write,
                    json: args.json,
                },
                &mut written,
                &mut save_error,
            )
        };
        if let (Some(line), false) = (outcome, args.json) {
            println!("{line}");
        }
    }
    if args.json {
        println!(
            "{}",
            render_rbn_json(
                &daemon,
                rc.window_seconds,
                row,
                saveable,
                configured_ppm,
                config_path.as_deref(),
                written
            )?
        );
    }
    if let Some(e) = save_error {
        return Err(e);
    }
    match (row.suggested_ppm, row.ppm_error) {
        (Some(_), Some(err)) if err > MAX_SAVE_ERROR_PPM => bail!(
            "the RBN reading for {section} is ±{err:.2} ppm, too coarse to save (needs \
             ±{MAX_SAVE_ERROR_PPM:.2} or better); wait for more spots, ideally on 20m or higher \
             bands, or run manta calibrate against WWV"
        ),
        (Some(_), Some(_)) => Ok(()),
        _ => bail!(
            "no RBN reading for {section} yet: a band needs {} graded spots in the last hour; \
             wait for more spots, or run manta calibrate against WWV",
            rc.min_samples
        ),
    }
}

/// The table a save writes into: `--input`'s name and how the lines
/// print it (`[input]` or `the [[input]] entry "20m"`).
struct Entry<'a> {
    name: Option<&'a str>,
    section: String,
}

/// Save `value` to `path`, returning the line to print.
fn save(
    path: &Path,
    value: f64,
    read_before: Option<&config_edit::ReadBefore>,
    entry: &Entry<'_>,
    written: &mut bool,
    save_error: &mut Option<anyhow::Error>,
) -> String {
    let value_lit = config_edit::literal(value);
    match config_edit::save_freq_correction_ppm(path, value, read_before, entry.name) {
        Ok(previous) => {
            *written = true;
            format!(
                "saved: freq_correction_ppm = {value_lit} in {} of {} (was {}); restart manta \
                 to apply it",
                entry.section,
                path.display(),
                previous.map_or_else(|| "not set".to_string(), config_edit::literal)
            )
        }
        Err(e) => {
            *save_error = Some(e.context(format!(
                "could not save freq_correction_ppm = {value_lit}; set it in {} of {} by hand",
                entry.section,
                path.display()
            )));
            "not saved: the config file could not be written".to_string()
        }
    }
}

/// MAN-310: the config entry a calibrated input saves into: the entry
/// whose `bands` planned the selected stream, else the entry `--input`
/// named (`None`: the single `[input]` table).
fn save_entry_name(selected: &[config::NamedInput], input: Option<&str>) -> Option<String> {
    match selected {
        [one] if input.is_some() && one.entry.as_deref() != Some(one.name.as_str()) => {
            one.entry.clone()
        }
        _ => input.map(str::to_string),
    }
}

/// MAN-310: how many streams the entry a save targets plans (1 when the
/// file no longer loads; the save itself then reports why).
fn entry_stream_count(path: &std::path::Path, entry: Option<&str>) -> usize {
    match config::load(Some(path), config::Env::Ignore) {
        Ok(l) if l.inputs.len() > 1 => l
            .inputs
            .iter()
            .filter(|n| n.entry.as_deref() == entry)
            .count(),
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn calibrating_a_stream_saves_into_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        for (text, stream, want) in [
            (
                "[input]\ntype = \"kiwi\"\nhost = \"k\"\nbands = [\"60m\"]\n",
                "kiwi-60m-1",
                None,
            ),
            (
                "[[input]]\nname = \"kiwi-a\"\ntype = \"kiwi\"\nhost = \"k\"\n\
                 bands = [\"60m\"]\n\n[[input]]\nname = \"b\"\ntype = \"kiwi\"\n\
                 host = \"k\"\nfreq_hz = 7030000.0\n",
                "kiwi-a-60m-2",
                Some("kiwi-a"),
            ),
        ] {
            let path = dir.path().join("m.toml");
            std::fs::write(&path, text).unwrap();
            let mut loaded = crate::config::load(Some(&path), crate::config::Env::Ignore).unwrap();
            crate::resolve::select_input(&mut loaded, Some(stream), "calibrate").unwrap();
            let target = super::save_entry_name(&loaded.inputs, Some(stream));
            assert_eq!(target.as_deref(), want);
            assert_eq!(super::entry_stream_count(&path, want), 2);
            let (saved, _) =
                crate::config_edit::set_freq_correction_ppm(text, -1.1, target.as_deref()).unwrap();
            assert!(
                saved.contains("bands = [\"60m\"]\n"),
                "the bands line is kept: {saved}"
            );
            std::fs::write(&path, &saved).unwrap();
            let reloaded = crate::config::load(Some(&path), crate::config::Env::Ignore).unwrap();
            let streams: Vec<_> = reloaded
                .inputs
                .iter()
                .filter(|n| n.entry.as_deref() == want)
                .collect();
            assert_eq!(streams.len(), 2);
            assert!(streams
                .iter()
                .all(|n| n.input.shared.freq_correction_ppm == Some(-1.1)));
        }
    }

    use super::*;
    use manta_engine::calibrate::ReferenceKind;

    #[test]
    fn confirm_accepts_only_y_or_yes() {
        for yes in ["y\n", "YES\n", " y \n", "Yes"] {
            let mut out = Vec::new();
            assert!(confirm(&mut out, &mut yes.as_bytes(), "Save?"), "{yes:?}");
            assert_eq!(String::from_utf8(out).unwrap(), "Save? [y/N] ");
        }
        for no in ["\n", "n\n", "yep\n", ""] {
            let mut out = Vec::new();
            assert!(!confirm(&mut out, &mut no.as_bytes(), "Save?"), "{no:?}");
        }
    }

    fn result(
        hz: f64,
        label: &'static str,
        status: ReferenceStatus,
        total: usize,
        valid: usize,
    ) -> ReferenceResult {
        ReferenceResult {
            reference_hz: hz,
            label,
            kind: ReferenceKind::TimeStandard,
            search_half_width_hz: 500.0,
            status,
            segments_total: total,
            segments_valid: valid,
            measured_hz: None,
            error_hz: None,
            freq_correction_ppm: None,
            uncertainty_ppm: None,
            peak_over_floor_db: None,
            spread_hz: None,
            ambiguity_margin_db: None,
            peak_offset_hz: None,
            runner_up_offset_hz: None,
            peaks_disagree: false,
        }
    }

    fn measured(
        hz: f64,
        label: &'static str,
        valid: usize,
        error: f64,
        ppm: f64,
    ) -> ReferenceResult {
        ReferenceResult {
            measured_hz: Some(hz + error),
            error_hz: Some(error),
            freq_correction_ppm: Some(ppm),
            uncertainty_ppm: Some(0.0035),
            peak_over_floor_db: Some(38.1),
            spread_hz: Some(0.21),
            ambiguity_margin_db: Some(24.5),
            ..result(hz, label, ReferenceStatus::Measured, 60, valid)
        }
    }

    fn report(references: Vec<ReferenceResult>) -> CalibrationReport {
        CalibrationReport {
            sample_rate_hz: 96_000.0,
            center_freq_hz: 9_998_500.0,
            duration_s: 60.0,
            search_ppm: 50.0,
            references,
            bad_samples: 0,
        }
    }

    #[test]
    fn render_human_matches_the_design() {
        let ok = report(vec![
            measured(
                10_000_000.0,
                "time standard (WWV, WWVH, BPM)",
                58,
                14.2,
                -1.41999,
            ),
            measured(9_996_000.0, "time standard (RWM)", 41, 14.1, -1.41056),
        ]);
        assert_eq!(
            render_human(&ok, 0.0, "from /etc/manta/manta.toml"),
            "source: 96000 Hz sample rate, centre 9998500.0 Hz, observed for 60.0 s\n\
             reference: 10000000.0 Hz, time standard (WWV, WWVH, BPM)\n\
             measured: 10000014.2 Hz, the receiver reads 14.2 Hz high\n\
             correction: freq_correction_ppm = -1.42 (±0.01 ppm)\n\
             evidence: carrier in 58 of 60 one-second segments, 38 dB over the noise floor, \
             spread 0.2 Hz\n\
             cross-check: 9996000.0 Hz, time standard (RWM): -1.41 ppm, carrier in 41 of 60 \
             one-second segments\n\
             configured: 0.00 ppm, from /etc/manta/manta.toml\n"
        );
        let failed = report(vec![
            result(
                10_000_000.0,
                "time standard (WWV, WWVH, BPM)",
                ReferenceStatus::TooFewSegments,
                60,
                3,
            ),
            result(
                9_996_000.0,
                "time standard (RWM)",
                ReferenceStatus::TooFewSegments,
                60,
                0,
            ),
        ]);
        assert_eq!(
            render_human(&failed, 0.0, "from manta.toml"),
            "source: 96000 Hz sample rate, centre 9998500.0 Hz, observed for 60.0 s\n\
             reference: none measured\n\
             tried: 10000000.0 Hz, time standard (WWV, WWVH, BPM): carrier in 3 of 60 \
             one-second segments, need 10\n\
             tried: 9996000.0 Hz, time standard (RWM): carrier in 0 of 60 one-second \
             segments, need 10\n\
             configured: 0.00 ppm, from manta.toml\n"
        );
        let ambiguous = report(vec![ReferenceResult {
            search_half_width_hz: 705.0,
            ambiguity_margin_db: Some(2.6),
            peak_offset_hz: Some(12.3),
            runner_up_offset_hz: Some(410.8),
            ..result(
                14_100_000.0,
                "NCDXF beacon",
                ReferenceStatus::Ambiguous,
                60,
                40,
            )
        }]);
        assert!(render_human(&ambiguous, 0.0, "nothing set").contains(
            "tried: 14100000.0 Hz, NCDXF beacon: two carriers within 6 dB of each other in the \
             ±705.0 Hz search window (+12.3 Hz and +410.8 Hz); narrow --search-ppm or pass \
             --reference-hz"
        ));
        // The median/average disagreement, with a margin over 6 dB, is not
        // reported as two carriers within 6 dB.
        let disagree = report(vec![ReferenceResult {
            ambiguity_margin_db: Some(11.6),
            peaks_disagree: true,
            ..ambiguous.references[0].clone()
        }]);
        let text = render_human(&disagree, 0.0, "nothing set");
        assert!(
            text.contains(
                "tried: 14100000.0 Hz, NCDXF beacon: two carriers alternate in the ±705.0 Hz \
                 search window: most one-second segments put it at +12.3 Hz, the average over \
                 the run at +410.8 Hz; pass --reference-hz to use another reference, or measure \
                 at another time"
            ),
            "{text}"
        );
        assert!(!text.contains("within 6 dB"), "{text}");
    }

    #[test]
    fn save_decision_table() {
        use SaveDecision::*;
        assert_eq!(
            save_decision(false, &[], false, true, false, true),
            NoConfig
        );
        assert_eq!(
            save_decision(
                true,
                &["--kiwi-host".to_string(), "MANTA_INPUT_FOO".to_string()],
                false,
                true,
                false,
                true
            ),
            Overridden("--kiwi-host, MANTA_INPUT_FOO".to_string())
        );
        assert_eq!(
            save_decision(true, &[], true, true, false, true),
            AlreadySet
        );
        assert_eq!(save_decision(true, &[], false, true, true, false), Save);
        assert_eq!(save_decision(true, &[], false, false, true, true), Skip);
        assert_eq!(
            save_decision(true, &[], false, false, false, false),
            NotInteractive
        );
        assert_eq!(save_decision(true, &[], false, false, false, true), Ask);
    }

    fn resolved_with(spec: LiveSourceSpec) -> Resolved {
        Resolved {
            cw_policy: Default::default(),
            conditioning: Default::default(),
            spec,
            freq_correction_ppm: 0.0,
            dial_freq_hz: Some(7_000_000.0),
            capture_rate_hz: None,
            replay_epoch: None,
            spot: crate::resolve::resolve_spot(
                Vec::new(),
                None,
                None,
                None,
                None,
                None,
                &Default::default(),
            ),
            rbn_compare: None,
            callsign_lookup: None,
            notes: Vec::new(),
            replaced_file_source: None,
            replay: crate::resolve::ReplayMode::default(),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn tune_hz_applies_to_kiwi_soapy_hpsdr_and_refuses_audio_and_file() {
        let mut r = resolved_with(LiveSourceSpec::Kiwi(crate::source_spec::KiwiOpts {
            host: Some("h".into()),
            port: 8073,
            freq: Some(7_030_000.0),
            password: String::new(),
        }));
        apply_tune_hz(&mut r, 9_998_500.0).unwrap();
        match &r.spec {
            LiveSourceSpec::Kiwi(k) => assert_eq!(k.freq, Some(9_998_500.0)),
            _ => unreachable!(),
        }
        assert_eq!(r.dial_freq_hz, None);
        {
            let mut r = resolved_with(LiveSourceSpec::Soapy(crate::source_spec::SoapyOpts {
                driver: Some("driver=rtlsdr".into()),
                freq: Some(7_030_000.0),
                rate: Some(240_000.0),
                gain: None,
            }));
            apply_tune_hz(&mut r, 9_998_500.0).unwrap();
            match &r.spec {
                LiveSourceSpec::Soapy(s) => assert_eq!(s.freq, Some(9_998_500.0)),
                _ => unreachable!(),
            }
        }
        {
            let mut r = resolved_with(LiveSourceSpec::Hpsdr(crate::source_spec::HpsdrOpts {
                host: Some("h".into()),
                mac: None,
                port: Some(manta_input::hpsdr::CONTROL_PORT),
                protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
                freq: Some(7_030_000.0),
                rate: Some(192_000.0),
                auto_attenuation: None,
            }));
            apply_tune_hz(&mut r, 9_998_500.0).unwrap();
            match &r.spec {
                LiveSourceSpec::Hpsdr(h) => assert_eq!(h.freq, Some(9_998_500.0)),
                _ => unreachable!(),
            }
        }
        let mut r = resolved_with(LiveSourceSpec::AudioDevice(None));
        let e = apply_tune_hz(&mut r, 9_998_500.0).unwrap_err();
        assert!(e.to_string().contains("cannot retune a sound card"), "{e}");
        let mut r = resolved_with(LiveSourceSpec::File {
            path: "x.wav".into(),
            source_iq: true,
            raw: None,
        });
        let e = apply_tune_hz(&mut r, 9_998_500.0).unwrap_err();
        assert!(e.to_string().contains("cannot retune a recording"), "{e}");
    }

    /// MAN-140 D17: a radiod channel is retuned in radiod, never by manta.
    #[test]
    fn tune_hz_refuses_a_ka9q_source() {
        let mut r = resolved_with(LiveSourceSpec::Ka9q(crate::source_spec::Ka9qOpts {
            group: Some("239.101.6.1".into()),
            port: 5004,
            ssrc: None,
            interface: None,
            ..crate::source_spec::Ka9qOpts::unset()
        }));
        let e = apply_tune_hz(&mut r, 9_998_500.0).unwrap_err();
        assert_eq!(
            e.to_string(),
            "--tune-hz cannot retune a ka9q-radio channel; retune it in radiod so the reference \
             sits inside its passband, then run calibrate without --tune-hz"
        );
        // The configured receiver is left as it was.
        assert_eq!(r.dial_freq_hz, Some(7_000_000.0));
        // `input_overrides` classifies the ka9q flags: --ka9q-group stands
        // for the three flags that require it.
        let cli = CliOverrides {
            ka9q: crate::source_spec::Ka9qOpts {
                group: Some("239.101.6.1".into()),
                port: 5010,
                ssrc: Some(14_100_000),
                interface: None,
                ..crate::source_spec::Ka9qOpts::unset()
            },
            ..CliOverrides::none()
        };
        assert_eq!(input_overrides(&cli, &[]), vec!["--ka9q-group".to_string()]);
    }

    /// A receiver already open, reporting `center_hz` and `passband`
    /// (MAN-334's `open_planned` tests).
    struct Tuned {
        center_hz: f64,
        passband: (f64, f64),
    }

    impl manta_input::IqSource for Tuned {
        fn sample_rate(&self) -> f64 {
            96_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_hz
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            self.passband
        }
        fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
            Ok(0)
        }
    }

    fn tuned(center_hz: f64, passband: (f64, f64)) -> crate::reconnect::SendSource {
        Box::new(Tuned {
            center_hz,
            passband,
        })
    }

    /// A KiwiSDR's passband, `manta_input::kiwi::IQ_PASSBAND_HZ`.
    const KIWI: (f64, f64) = (-5_000.0, 5_000.0);

    /// The retune hint for a KiwiSDR window centred at 7.030 MHz.
    const KIWI_7030_HINT: &str = "no known reference frequency is usable in this receiver's \
        passband (7025000.0 to 7035000.0 Hz); retune it for the measurement, e.g. --tune-hz \
        4998500 for the 5000000.0 Hz time standard (WWV, WWVH, BPM) or --tune-hz 9998500 for \
        the 10000000.0 Hz time standard (WWV, WWVH, BPM), or name a carrier you know with \
        --reference-hz";

    fn planned_err(r: Result<(crate::reconnect::SendSource, Vec<PlannedReference>)>) -> String {
        match r {
            Ok(_) => panic!("expected open_planned to refuse"),
            Err(e) => e.to_string(),
        }
    }

    /// MAN-334 scenarios 1 and 2: a configured passband that holds no
    /// reference is refused with the retune hint, and the receiver is never
    /// opened.
    #[test]
    fn open_planned_refuses_an_unusable_configured_passband_without_opening() {
        let opened = std::cell::Cell::new(false);
        let r = open_planned(
            Some((7_030_000.0, KIWI)),
            false,
            &CalibrateOptions::default(),
            || {
                opened.set(true);
                Ok(tuned(7_030_000.0, KIWI))
            },
        );
        assert_eq!(planned_err(r), KIWI_7030_HINT);
        assert!(!opened.get(), "the receiver was opened");
    }

    /// MAN-334 scenario 3: the preflight passed on the configured values, but
    /// the receiver negotiated a centre with no reference in reach.
    #[test]
    fn open_planned_replans_on_the_values_the_receiver_negotiated() {
        let r = open_planned(
            Some((9_998_500.0, KIWI)),
            false,
            &CalibrateOptions::default(),
            || Ok(tuned(7_030_000.0, KIWI)),
        );
        assert_eq!(planned_err(r), KIWI_7030_HINT);
    }

    /// MAN-334 scenario 3: the plan measured is the opened source's, not the
    /// configured one's.
    #[test]
    fn open_planned_measures_the_plan_of_the_opened_source() {
        let opts = CalibrateOptions::default();
        // 10 MHz sits 300 Hz above the configured centre: half-width
        // 300 - DC_GUARD_HZ = 275 Hz.
        let configured = plan_references(9_999_700.0, KIWI, false, &opts).unwrap();
        assert_eq!(configured[0].reference.hz, 10_000_000.0);
        assert_eq!(configured[0].half_width_hz, 275.0);
        let (_src, planned) = open_planned(Some((9_999_700.0, KIWI)), false, &opts, || {
            Ok(tuned(9_999_650.0, KIWI))
        })
        .unwrap();
        assert_eq!(
            planned,
            plan_references(9_999_650.0, KIWI, false, &opts).unwrap()
        );
        assert_eq!(planned[0].reference.hz, 10_000_000.0);
        assert_eq!(planned[0].half_width_hz, 325.0);
    }

    /// MAN-334: a ka9q-radio channel or a recording has no configured
    /// passband, so it is opened and planned once, on what it reports.
    #[test]
    fn open_planned_without_a_configured_passband_plans_once_open() {
        let opened = std::cell::Cell::new(false);
        let r = open_planned(None, false, &CalibrateOptions::default(), || {
            opened.set(true);
            Ok(tuned(7_030_000.0, KIWI))
        });
        assert_eq!(planned_err(r), KIWI_7030_HINT);
        assert!(opened.get(), "the receiver was not opened");
    }

    /// MAN-334: a usable configured passband still opens the receiver, so
    /// an offline one keeps reporting why it could not be opened.
    #[test]
    fn open_planned_reports_the_open_error_when_the_preflight_passes() {
        let r = open_planned(
            Some((9_998_500.0, KIWI)),
            false,
            &CalibrateOptions::default(),
            || {
                Err(anyhow::anyhow!(
                    "TCP connect to 127.0.0.1:1: Connection refused"
                ))
            },
        );
        assert_eq!(
            planned_err(r),
            "TCP connect to 127.0.0.1:1: Connection refused"
        );
    }

    // ---- MAN-541: --from-rbn ----

    fn rbn_row(entry: Option<&str>, names: &[&str]) -> manta_server::status_doc::RbnSkewEntry {
        use manta_server::status_doc::{RbnSkewEntry, RbnSkewReading};
        let reading = |band: &str, hz: f64, n: usize, r: Option<(f64, f64)>| RbnSkewReading {
            band: band.to_string(),
            median_offset_hz: hz,
            samples: n,
            median_ppm: r.map(|r| r.0),
            ppm_error: r.map(|r| r.1),
        };
        RbnSkewEntry {
            entry: entry.map(str::to_string),
            names: names.iter().map(|n| n.to_string()).collect(),
            applied_ppm: 0.0,
            replaced_by_flag: false,
            bands: vec![
                reading("80m", 13.5, 41, Some((3.803, 2.2))),
                reading("40m", 21.0, 8, None),
                reading("20m", 42.0, 40, Some((2.995, 0.564))),
                reading("15m", 63.0, 24, Some((2.996, 0.485))),
            ],
            observed_ppm: Some(3.017),
            ppm_error: Some(0.363),
            suggested_ppm: Some(-3.02),
        }
    }

    #[test]
    fn pick_entry() {
        let d = "127.0.0.1:7302";
        let one = [rbn_row(None, &["kiwi"])];
        assert_eq!(
            super::pick_entry(&one, None, None, d, None).unwrap().entry,
            None
        );
        assert_eq!(
            super::pick_entry(&one, Some(None), None, d, None)
                .unwrap()
                .entry,
            None
        );
        let two = [
            rbn_row(Some("kiwi"), &["kiwi-20m", "kiwi-15m"]),
            rbn_row(Some("hl2"), &["hl2"]),
        ];
        let err = super::pick_entry(&two, None, None, d, None).unwrap_err();
        assert!(format!("{err}").contains("pass --input NAME"), "{err}");
        for name in ["kiwi", "kiwi-15m"] {
            assert_eq!(
                super::pick_entry(&two, None, Some(name), d, None)
                    .unwrap()
                    .entry
                    .as_deref(),
                Some("kiwi")
            );
        }
        assert_eq!(
            super::pick_entry(&two, Some(Some("hl2")), Some("hl2"), d, None)
                .unwrap()
                .entry
                .as_deref(),
            Some("hl2")
        );
        let err = super::pick_entry(&one, Some(Some("hl2")), Some("hl2"), d, None).unwrap_err();
        assert_eq!(
            format!("{err}"),
            "the daemon at 127.0.0.1:7302 reports no RBN reading for the [[input]] entry \"hl2\"; \
             it reports [input]; point --addr at the daemon that runs it"
        );
        assert!(super::pick_entry(&[], None, None, d, None).is_err());
    }

    #[test]
    fn render_rbn_human() {
        assert_eq!(
            super::render_rbn_human(
                "127.0.0.1:7302",
                3600,
                20,
                &rbn_row(None, &["kiwi"]),
                "[input]"
            ),
            "source: RBN comparison of the daemon at 127.0.0.1:7302 over the last 3600 s, for \
             [input]\n\
             evidence: 80m: +3.8 ±2.2 ppm, median +14 Hz over 41 spots\n\
             evidence: 40m: median +21 Hz over 8 spots, needs 20\n\
             evidence: 20m: +3.0 ±0.6 ppm, median +42 Hz over 40 spots\n\
             evidence: 15m: +3.0 ±0.5 ppm, median +63 Hz over 24 spots\n\
             measured: the receiver reads 3.02 ppm high against the RBN, with 0.00 ppm applied\n\
             correction: freq_correction_ppm = -3.02 (±0.36 ppm)\n"
        );
    }
}
