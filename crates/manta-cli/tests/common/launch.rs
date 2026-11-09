//! MAN-549: the one `manta` launcher and the one port reservation the
//! integration tests share. A leaf that needs only `std`: a root that wants
//! nothing else from `common` includes it alone with
//! `#[path = "common/launch.rs"] mod launch;`, and so does not link what the
//! rest of `common` pulls in (`manta_input`, `regex`, `tungstenite`).
//! `common/mod.rs` re-exports all three functions for the roots that
//! already say `mod common;`.
// Each test binary compiles this file separately and uses only part of it.
#![allow(dead_code)]

use std::net::TcpListener;
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

/// `N` distinct loopback TCP ports nothing listens on right now. All `N`
/// listeners are held until every port is read: releasing each before
/// binding the next lets the OS hand the same port out twice (Windows
/// does, and then the config is rejected as "two servers cannot share a
/// port", MAN-371). The ports are free only until this returns.
/// `doctor_checks.rs` keeps its own band allocator (MAN-333, MAN-410).
pub fn free_ports<const N: usize>() -> [u16; N] {
    let held: [TcpListener; N] = std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    held.map(|l| l.local_addr().unwrap().port())
}
