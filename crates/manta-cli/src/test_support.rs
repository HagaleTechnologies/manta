//! Fixtures shared by more than one module's unit tests (MAN-548).

use crate::config;
use crate::resolve::CliOverrides;
use crate::source_spec::Ka9qOpts;

pub(crate) const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

/// The built-in cty.dat and MASTER.SCP as the shared lists a test's
/// `start_spot_server` needs.
pub(crate) fn bundled_reference_lists() -> std::sync::Arc<manta_spot::reference::ReferenceLists> {
    std::sync::Arc::new(manta_spot::reference::ReferenceLists::new(
        std::sync::Arc::new(manta_spot::cty::Table::bundled()),
        std::sync::Arc::new(manta_spot::scp::Set::bundled()),
        std::sync::Arc::new(manta_spot::pattern::Table::bundled()),
    ))
}

pub(crate) fn write_temp_file(contents: &[u8]) -> tempfile::NamedTempFile {
    use std::io::Write as _;
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(contents).unwrap();
    f.flush().unwrap();
    f
}

pub(crate) const GEOGRAPHY_CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N;
";

/// One `dxcc.tsv` row for the fixture above, in the vendored file's
/// `<primary-prefix>\t<adif-number>\t<name>` shape.
pub(crate) const GEOGRAPHY_DXCC_FIXTURE: &str = "K\t291\tUnited States\n";

pub(crate) fn cli_overrides() -> CliOverrides {
    CliOverrides::none()
}

pub(crate) fn loaded_from(body: &str) -> config::Loaded {
    let f = write_temp_file(body.as_bytes());
    config::load(Some(f.path()), config::Env::Ignore).unwrap()
}

pub(crate) const KIWI_INPUT: &str =
    "[input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nfreq_hz = 14025000.0\n\
                              freq_correction_ppm = 2.5\ncenter_freq_hz = 14000000.0\n";

pub(crate) fn ka9q_opts(ssrc: Option<u32>) -> Ka9qOpts {
    Ka9qOpts {
        group: Some("239.101.6.1".into()),
        port: manta_input::ka9q::wire::DATA_PORT,
        ssrc,
        interface: None,
        ..Ka9qOpts::unset()
    }
}

pub(crate) fn test_server_config(callsign: &str) -> manta_server::config::ServerConfig {
    let cfg_file = write_temp_file(
        format!(
            r#"
                [server]
                station_callsign = "{callsign}"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0
                "#
        )
        .as_bytes(),
    );
    let text = std::fs::read_to_string(cfg_file.path()).unwrap();
    let file: manta_server::config::DaemonConfigFile = toml::from_str(&text).unwrap();
    file.server
}
