//! MAN-549: the one `manta` launcher the integration tests share. A leaf
//! that needs only `std`: a root that wants nothing else from `common`
//! includes it alone with `#[path = "common/launch.rs"] mod launch;`, and
//! so does not link what the rest of `common` pulls in (`manta_input`,
//! `regex`, `tungstenite`). `common/mod.rs` re-exports both functions for
//! the roots that already say `mod common;`. There is no shared port
//! helper here (MAN-588): a daemon a test starts binds port 0 and reports
//! its ports through `man570/daemon.rs`'s `spawn_daemon_reporting_ports`.
// Each test binary compiles this file separately and uses only part of it.
#![allow(dead_code)]

use std::process::Command;

/// The binary under test, with every `MANTA_*` variable of the test
/// runner's environment removed.
pub fn manta() -> Command {
    without_manta_env(Command::new(env!("CARGO_BIN_EXE_manta")))
}

/// `cmd` with every `MANTA_*` variable of the test runner's environment
/// removed. MAN-261: `run`, `soak`, `doctor`, `health` and `config check`
/// read `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>` and reject unknown
/// `MANTA_*` names, so a variable in the runner's own environment must
/// never leak into the child. Tests that exercise the env tier set theirs
/// explicitly, after this.
pub fn without_manta_env(mut cmd: Command) -> Command {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}
