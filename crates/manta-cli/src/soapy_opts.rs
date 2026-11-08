//! MAN-550: everything manta-cli knows about SoapySDR input, so the rest of
//! the CLI names no `soapy` feature.
//!
//! `SoapyOpts`, the connection settings a flag or `[input]` gives, exists in
//! every build. The `--soapy-*` flags (`SoapyArgs`) and the device exist only
//! in a build made with `--features soapy`; without it `SoapyArgs` has no
//! fields, so no `--soapy-*` flag parses or shows in `--help`, and
//! `SoapyOpts::from_file` and `open` fail with the error a config check has
//! always given.

use anyhow::Result;

/// SoapySDR connection settings, grouped like `KiwiOpts`. `Clone`: MAN-73's
/// `LiveSourceSpec` carries one so `ReconnectingSource` can re-open it.
#[derive(Clone, Debug, Default)]
pub(crate) struct SoapyOpts {
    pub(crate) driver: Option<String>,
    pub(crate) freq: Option<f64>,
    pub(crate) rate: Option<f64>,
    #[cfg_attr(
        not(feature = "soapy"),
        expect(dead_code, reason = "only the soapy build's `open` reads the gain")
    )]
    pub(crate) gain: Option<f64>,
}

impl SoapyOpts {
    /// The settings an `[input]` with `type = "soapy"` gives; an error in a
    /// build without the feature, so `config check` refuses it.
    pub(crate) fn from_file(
        driver: &str,
        freq_hz: f64,
        rate_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<Self> {
        imp::ensure_built()?;
        Ok(SoapyOpts {
            driver: Some(driver.to_owned()),
            freq: Some(freq_hz),
            rate: Some(rate_hz),
            gain: gain_db,
        })
    }
}

/// `conflicts_with_all` for a source flag that excludes `--soapy-driver`:
/// `ids`, plus `soapy_driver` in a build that has the flag. clap rejects a
/// conflict naming an argument the command lacks, and zsh completion prints
/// each flag's own list, so the id is appended here rather than dropped.
pub(crate) fn and_driver<const N: usize>(ids: [&'static str; N]) -> Vec<&'static str> {
    ids.into_iter().chain(imp::DRIVER_ID).collect()
}

/// For `check`'s `mut_args`: puts `--soapy-driver` in the `source_selector`
/// group, as every other `check` selector is. No argument has that id in a
/// build without the feature, so there it changes nothing.
pub(crate) fn join_source_selector(arg: clap::Arg) -> clap::Arg {
    if arg.get_id() == "soapy_driver" {
        arg.group("source_selector")
    } else {
        arg
    }
}

pub(crate) use imp::{open, SoapyArgs};

#[cfg(feature = "soapy")]
mod imp {
    use super::SoapyOpts;
    use anyhow::{anyhow, Result};

    pub(super) const DRIVER_ID: Option<&str> = Some("soapy_driver");

    // The `--soapy-*` flags, flattened into `run`, `soak`, `doctor`,
    // `calibrate` and `check` where the four fields used to sit (MAN-550).
    // A `//` comment, not a doc comment, so nothing here can reach a
    // command's `--help`.
    #[derive(clap::Args, Clone, Debug, Default)]
    pub(crate) struct SoapyArgs {
        /// SoapySDR device arguments, e.g. "driver=rtlsdr".
        ///
        /// Requires --soapy-freq-hz and --soapy-rate-hz. Available only in
        /// builds made with the `soapy` feature.
        #[arg(help_heading = "SoapySDR source")]
        #[arg(long, conflicts_with_all = ["device", "source", "ka9q_group", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port", "kiwi_host"])]
        soapy_driver: Option<String>,
        /// Receiver centre frequency, in Hz. Required with --soapy-driver.
        #[arg(
            long = "soapy-freq-hz",
            alias = "soapy-freq",
            requires = "soapy_driver",
            help_heading = "SoapySDR source"
        )]
        soapy_freq_hz: Option<f64>,
        /// Sample rate, in Hz. Required with --soapy-driver.
        #[arg(
            long = "soapy-rate-hz",
            alias = "soapy-rate",
            requires = "soapy_driver",
            help_heading = "SoapySDR source"
        )]
        soapy_rate_hz: Option<f64>,
        /// Receiver gain, in dB. On an SDRplay a higher value means more attenuation, not more
        /// gain. Omit to let the device use AGC.
        ///
        /// SDRplay's SoapySDR driver turns this one value into IF and RF gain reduction. On the RSP1B
        /// measured, 0 is the most sensitive setting and 48 the least, 40 already puts the IF stage at
        /// its maximum reduction, and 46 to 48 failed to start the stream. Compare settings on a
        /// signal you know is present.
        #[arg(long, requires = "soapy_driver", help_heading = "SoapySDR source")]
        soapy_gain: Option<f64>,
    }

    impl From<SoapyArgs> for SoapyOpts {
        fn from(args: SoapyArgs) -> Self {
            SoapyOpts {
                driver: args.soapy_driver,
                freq: args.soapy_freq_hz,
                rate: args.soapy_rate_hz,
                gain: args.soapy_gain,
            }
        }
    }

    pub(super) fn ensure_built() -> Result<()> {
        Ok(())
    }

    /// Opens the device `soapy` names; `LiveSourceSpec::open`'s Soapy arm.
    pub(crate) fn open(soapy: &SoapyOpts) -> Result<crate::reconnect::SendSource> {
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
        Ok(Box::new(
            manta_input::soapy::SoapySdrIqSource::open(driver, rate, freq, soapy.gain)?
                .with_overflow_notice(Box::new(move || {
                    crate::logging::warning(&overflow_warning(&warn_driver))
                })),
        ))
    }

    /// MAN-233: a SoapySDR stream overflow error means this chunk was dropped
    /// by a full driver ring buffer -- the host is not keeping up. Counted, and
    /// rate-limited so a sustained overflow burst logs one line per 60 s. An
    /// overflow flagged `SOAPY_SDR_END_ABRUPT` on a successful read never
    /// reaches here (see `SoapySdrIqSource::read`), hence "overflow error".
    fn overflow_warning(driver: &str) -> String {
        format!(
            "warning: SoapySDR input ({driver}) dropped samples: the driver reported a \
             stream overflow -- the host is not keeping up; every overflow error is counted in \
             manta_input_gaps_detected_total{{source=\"soapy\"}} (this warning is \
             rate-limited to once per 60 s)"
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// MAN-233: pins the design section's exact wording, so a future edit
        /// cannot quietly change what the operator is told to watch
        /// (`ka9q_agc_warning_is_design_section_3s_text` precedent).
        #[test]
        fn soapy_overflow_warning_names_the_driver_the_counter_and_the_interval() {
            assert_eq!(
                overflow_warning("driver=rtlsdr"),
                "warning: SoapySDR input (driver=rtlsdr) dropped samples: the driver reported a \
                 stream overflow -- the host is not keeping up; every overflow error is counted in \
                 manta_input_gaps_detected_total{source=\"soapy\"} (this warning is \
                 rate-limited to once per 60 s)"
            );
        }
    }
}

#[cfg(not(feature = "soapy"))]
mod imp {
    use super::SoapyOpts;
    use anyhow::{bail, Result};

    /// What a build without `--features soapy` says about a SoapySDR input.
    const NOT_BUILT: &str = "input.type = \"soapy\" needs a manta built with --features soapy";

    pub(super) const DRIVER_ID: Option<&str> = None;

    // No `--soapy-*` flags in a build without the feature: flattening this
    // adds nothing to the command or its `--help`.
    #[derive(clap::Args, Clone, Debug, Default)]
    pub(crate) struct SoapyArgs {}

    impl From<SoapyArgs> for SoapyOpts {
        fn from(_: SoapyArgs) -> Self {
            SoapyOpts::default()
        }
    }

    pub(super) fn ensure_built() -> Result<()> {
        bail!(NOT_BUILT)
    }

    /// Never reached from the command line or a config file (both refuse a
    /// SoapySDR input first); a hand-built spec gets the same refusal.
    pub(crate) fn open(_: &SoapyOpts) -> Result<crate::reconnect::SendSource> {
        bail!(NOT_BUILT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_file_succeeds_only_in_a_soapy_build() {
        match SoapyOpts::from_file("driver=rtlsdr", 7e6, 48e3, Some(20.0)) {
            Ok(opts) => {
                assert!(imp::DRIVER_ID.is_some());
                assert_eq!(opts.driver.as_deref(), Some("driver=rtlsdr"));
                assert_eq!((opts.freq, opts.rate), (Some(7e6), Some(48e3)));
            }
            Err(e) => {
                assert!(imp::DRIVER_ID.is_none());
                assert_eq!(
                    e.to_string(),
                    "input.type = \"soapy\" needs a manta built with --features soapy"
                );
            }
        }
    }

    #[test]
    fn and_driver_appends_the_driver_only_when_the_flag_exists() {
        let ids = and_driver(["device", "source"]);
        assert_eq!(&ids[..2], ["device", "source"]);
        assert_eq!(ids.get(2).copied(), imp::DRIVER_ID);
        assert_eq!(ids.len(), 2 + usize::from(imp::DRIVER_ID.is_some()));
    }

    #[test]
    fn join_source_selector_groups_only_the_driver() {
        let mut cmd = clap::Command::new("t")
            .arg(clap::Arg::new("soapy_driver").long("soapy-driver"))
            .arg(clap::Arg::new("kiwi_host").long("kiwi-host"))
            .mut_args(join_source_selector);
        cmd.build();
        let group = cmd
            .get_groups()
            .find(|g| g.get_id() == "source_selector")
            .expect("the driver's group exists");
        let members: Vec<&str> = group.get_args().map(|id| id.as_str()).collect();
        assert_eq!(members, ["soapy_driver"]);
    }

    /// MAN-550 (D8): `src` names the soapy feature fewer than 15 times, and
    /// outside this file only once, in main.rs (its feature-off config test).
    #[test]
    fn only_soapy_opts_names_the_soapy_feature() {
        // Matched with whitespace stripped, so a gate spelled without spaces
        // counts too; built with concat! so this line does not count itself.
        const NEEDLE: &str = concat!("feature", "=\"soapy\"");
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        let (mut total, mut outside) = (0, Vec::new());
        for file in &files {
            let rel = file.strip_prefix(&src).unwrap().display().to_string();
            let text = std::fs::read_to_string(file).unwrap();
            for (n, line) in text.lines().enumerate() {
                if line.split_whitespace().collect::<String>().contains(NEEDLE) {
                    total += 1;
                    if rel != "soapy_opts.rs" {
                        outside.push(format!("{rel}:{}", n + 1));
                    }
                }
            }
        }
        assert!(total < 15, "{total} soapy feature gates in src");
        assert!(
            outside.len() <= 1 && outside.iter().all(|at| at.starts_with("main.rs:")),
            "gate SoapySDR inside soapy_opts.rs, not at {outside:?}"
        );
    }
}
