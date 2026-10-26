//! MAN-126 acceptance against the real `manta doctor` binary: a healthy
//! setup passes every check (scenario 1), and each real problem is named
//! with its fix (scenario 2). Every network check runs against a local fake
//! (TCP listener, SNTP responder, held or closed port), and every run
//! passes `--ntp-server 127.0.0.1:<fake>`, so no test reaches the internet.
//!
//! Helpers are copied from `metrics_bind.rs`; the repo's test files are
//! self-contained.

use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// `seconds` of 48 kHz mono silence: enough for doctor's 3 s minimum.
fn silent_48k_wav(dir: &Path, seconds: u32) -> PathBuf {
    let path = dir.join("silence.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for _ in 0..(48_000 * seconds) {
        w.write_sample(0i16).unwrap();
    }
    w.finalize().unwrap();
    path
}

/// The private band every "nothing is here" port below is drawn from:
/// 20000-20255. It sits below the kernel's ephemeral range on every
/// default (Linux starts at 32768, macOS and Windows at 49152), so no
/// sibling test's `bind(…, 0)` — which the OS feeds from that ephemeral
/// pool — can ever be handed a number this file has handed out, and
/// nothing else in the suite binds this band (MAN-333). A runner that
/// runs test *binaries* in parallel (cargo-nextest, unused in this repo)
/// would run this binary twice at once and needs a per-binary offset
/// into the band.
const BAND_START: u16 = 20_000;
const BAND_LEN: u16 = 256;

/// Allocation state for the band: a monotonic cursor, so each band port
/// is issued at most once per process, behind a process-global mutex.
struct Band {
    next: u16,
    range_checked: bool,
}

static BAND: Mutex<Band> = Mutex::new(Band {
    next: 0,
    range_checked: false,
});

impl Band {
    /// Fail loudly, once, if this host's ephemeral port range reaches into
    /// the band: a widened range would hand `bind(…, 0)` ports inside the
    /// band and silently reintroduce the race the band exists to remove.
    /// Linux states its range in `/proc/sys/net/ipv4/ip_local_port_range`;
    /// macOS and Windows default to 49152-65535, far above the band, and
    /// are not checked.
    fn check_ephemeral_range_once(&mut self) {
        if self.range_checked {
            return;
        }
        self.range_checked = true;
        #[cfg(target_os = "linux")]
        {
            let text = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
                .expect("read /proc/sys/net/ipv4/ip_local_port_range");
            let ends: [u32; 2] = text
                .split_whitespace()
                .map(|t| t.parse().expect("ip_local_port_range endpoint"))
                .collect::<Vec<_>>()
                .try_into()
                .expect("ip_local_port_range holds two numbers");
            let [lo, hi] = ends;
            let band_lo = u32::from(BAND_START);
            let band_hi = band_lo + u32::from(BAND_LEN) - 1;
            assert!(
                hi < band_lo || lo > band_hi,
                "this host's ephemeral port range {lo}-{hi} overlaps the test port band \
                 {band_lo}-{band_hi}: move the band out from under it"
            );
        }
    }
}

/// The next unissued band port whose `free` probe succeeds. Candidates a
/// foreign listener already holds are skipped (bind errors, including the
/// port ranges Windows excludes); a port is never issued twice, so two
/// tests in this binary cannot be handed the same number. Exhaustion
/// panics: one full binary run needs 26 ports (8 `Setup` tests × 3, one
/// uplink, one NTP), so the 256-port band holds ~10× headroom.
fn next_band_port(free: impl Fn(u16) -> bool) -> u16 {
    let mut band = BAND.lock().unwrap();
    band.check_ephemeral_range_once();
    loop {
        if band.next >= BAND_LEN {
            panic!(
                "test port band {}-{} exhausted",
                BAND_START,
                BAND_START + BAND_LEN - 1
            );
        }
        let port = BAND_START + band.next;
        band.next += 1;
        if free(port) {
            return port;
        }
    }
}

/// A TCP port nothing listens on for this test (see `free_ports`).
fn free_port() -> u16 {
    let [port] = free_ports();
    port
}

/// `N` distinct TCP ports nothing listens on. They come from the private
/// band above — distinct by construction (one cursor under one mutex
/// hands out each port at most once, where a released ephemeral port can
/// be handed straight to the next `bind(0)`, as Windows does), and never
/// from the kernel's ephemeral pool, so no concurrently running test can
/// claim one between allocation and the moment `manta doctor` probes it.
/// Holding a socket instead cannot express "nothing here": a
/// listening-but-never-accepted socket still accepts connections, and a
/// held listener is exactly what doctor's own bind probe reports as
/// already in use.
fn free_ports<const N: usize>() -> [u16; N] {
    std::array::from_fn(|_| next_band_port(|p| TcpListener::bind(("127.0.0.1", p)).is_ok()))
}

/// A UDP port nothing answers on, for an NTP server that refuses — same
/// band contract as `free_ports`, so the port stays unanswered until the
/// check runs. The socket is not held: a bound-but-unread UDP socket
/// swallows the request and would cost doctor's clock check its full 3 s
/// budget where nothing bound gets the instant ICMP refusal this test
/// wants.
fn closed_port() -> u16 {
    next_band_port(|p| UdpSocket::bind(("127.0.0.1", p)).is_ok())
}

const NTP_UNIX_OFFSET: f64 = 2_208_988_800.0;

fn ntp_bytes(unix: f64) -> [u8; 8] {
    let ntp = unix + NTP_UNIX_OFFSET;
    let secs = ntp.trunc() as u64 as u32;
    let frac = (ntp.fract() * 4_294_967_296.0) as u32;
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&secs.to_be_bytes());
    out[4..].copy_from_slice(&frac.to_be_bytes());
    out
}

/// A loopback SNTP server whose clock runs `offset_s` ahead of this one,
/// echoing each request's transmit time as the originate time.
fn fake_sntp(offset_s: f64) -> (u16, std::thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = sock.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if n < 48 {
                continue;
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
                + offset_s;
            let mut reply = [0u8; 48];
            reply[0] = 0x24; // LI 0, VN 4, mode 4 (server)
            reply[1] = 1; // stratum 1
            reply[24..32].copy_from_slice(&buf[40..48]);
            reply[32..40].copy_from_slice(&ntp_bytes(now));
            reply[40..48].copy_from_slice(&ntp_bytes(now));
            let _ = sock.send_to(&reply, from);
        }
    });
    (port, handle)
}

/// The host kernel's own NTP-sync state, read the way doctor reads it, so
/// no expectation depends on the CI runner's time daemon. `None` off Linux.
fn host_synced() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: all-zero is a valid `timex`; `modes = 0` only reads.
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        let state = unsafe { libc::adjtimex(&mut tx) };
        if state < 0 {
            return None;
        }
        Some(state != libc::TIME_ERROR && tx.status & libc::STA_UNSYNC == 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Everything a healthy setup has: three free loopback ports, one enabled
/// uplink at a live local listener, and a WAV receiver with a dial
/// frequency.
struct Setup {
    dir: tempfile::TempDir,
    ports: [u16; 3],
    uplink: TcpListener,
    wav: PathBuf,
    station: &'static str,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wav = silent_48k_wav(dir.path(), 4);
        Self {
            dir,
            ports: free_ports(),
            uplink: TcpListener::bind("127.0.0.1:0").unwrap(),
            wav,
            station: "W1AW",
        }
    }

    fn uplink_port(&self) -> u16 {
        self.uplink.local_addr().unwrap().port()
    }

    fn config(&self, uplink_port: u16) -> PathBuf {
        let [telnet, json, metrics] = self.ports;
        let body = format!(
            "[server]\nstation_callsign = \"{}\"\nbind_addr = \"127.0.0.1\"\n\
             metrics_bind_addr = \"127.0.0.1\"\ntelnet_port = {telnet}\njson_port = {json}\n\
             metrics_port = {metrics}\n\n[[rbn_uplink]]\nenabled = true\n\
             target_host = \"127.0.0.1\"\ntarget_port = {uplink_port}\n\n[input]\n\
             type = \"file\"\npath = \"{}\"\ncenter_freq_hz = 14030000.0\n",
            self.station,
            self.wav.display().to_string().replace('\\', "\\\\")
        );
        let path = self.dir.path().join("manta.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn doctor(&self, cfg: &Path, ntp_port: u16, extra: &[&str]) -> Output {
        manta()
            .args(["doctor", "--duration", "3", "--ntp-server"])
            .arg(format!("127.0.0.1:{ntp_port}"))
            .arg("--config")
            .arg(cfg)
            .args(extra)
            .output()
            .unwrap()
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The clock line a fake server at offset 0 earns on this host.
fn assert_clock_line_for_a_good_server(stdout: &str) {
    if host_synced() == Some(false) {
        assert!(
            stdout.contains("WARN  clock: within")
                && stdout.contains("nothing is keeping it in sync"),
            "{stdout}"
        );
    } else {
        assert!(stdout.contains("PASS  clock: within"), "{stdout}");
    }
}

/// The line right after the first line starting with `prefix`.
fn line_after<'a>(stdout: &'a str, prefix: &str) -> &'a str {
    let mut lines = stdout.lines();
    lines
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {stdout}"));
    lines.next().unwrap_or_default()
}

#[test]
fn healthy_setup_passes_every_check() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let [telnet, json, metrics] = setup.ports;
    for needle in [
        "PASS  config:".to_string(),
        "PASS  audio:".to_string(),
        format!("PASS  telnet port: 127.0.0.1:{telnet} is free"),
        format!("PASS  json port: 127.0.0.1:{json} is free"),
        format!("PASS  metrics port: 127.0.0.1:{metrics} is free"),
        format!(
            "PASS  rbn uplink 127.0.0.1:{}: accepted a connection",
            setup.uplink_port()
        ),
        "PASS  receiver: opened the WAV file".to_string(),
        "verdict: ".to_string(),
    ] {
        assert!(stdout.contains(&needle), "no {needle:?} in {stdout}");
    }
    assert_clock_line_for_a_good_server(&stdout);
    if host_synced() == Some(false) {
        assert!(stdout.contains("doctor: 1 warning: clock"), "{stdout}");
    } else {
        assert!(stdout.contains("doctor: all checks passed"), "{stdout}");
    }
    assert!(!stderr.contains("ignoring [server]"), "{stderr}");
}

#[test]
fn a_held_port_is_named_with_its_fix() {
    let mut setup = Setup::new();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    setup.ports[0] = held.local_addr().unwrap().port();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let line = format!(
        "FAIL  telnet port: 127.0.0.1:{} is already in use",
        setup.ports[0]
    );
    assert!(stdout.contains(&line), "{stdout}");
    let fix = line_after(&stdout, &line);
    assert!(
        fix.starts_with("      fix: ") && fix.contains("telnet_port"),
        "{fix}"
    );
    assert!(stdout.contains("failed: telnet port"), "{stdout}");
    assert!(stdout.contains("verdict: "), "{stdout}");
    assert!(!stderr.contains("Error:"), "{stderr}");
    drop(held);
}

#[test]
fn an_unreachable_uplink_is_named_with_its_fix() {
    let setup = Setup::new();
    let closed = free_port();
    let cfg = setup.config(closed);
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let prefix = format!("FAIL  rbn uplink 127.0.0.1:{closed}: refused the connection");
    assert!(stdout.contains(&prefix), "{stdout}");
    assert!(
        line_after(&stdout, &prefix).contains("target_port"),
        "{stdout}"
    );
}

#[test]
fn a_clock_two_minutes_off_fails() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(120.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let line = stdout
        .lines()
        .find(|l| l.starts_with("FAIL  clock: this clock is "))
        .unwrap_or_else(|| panic!("no clock FAIL in {stdout}"));
    assert!(
        line.contains(&format!(
            " s behind 127.0.0.1:{ntp}, so every spot's time is wrong"
        )),
        "{line}"
    );
    let secs: f64 = line["FAIL  clock: this clock is ".len()..]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((119.0..121.0).contains(&secs), "{line}");
}

#[test]
fn an_unreachable_ntp_server_is_a_warning_not_a_failure() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let out = setup.doctor(&cfg, closed_port(), &[]);
    let (stdout, stderr) = text(&out);
    if host_synced() == Some(true) {
        assert!(
            stdout.contains("PASS  clock: ")
                && stdout.contains("NTP is keeping this clock in sync"),
            "{stdout}"
        );
    } else {
        assert!(
            stdout.contains("WARN  clock: could not measure the clock: 127.0.0.1:"),
            "{stdout}"
        );
    }
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
}

#[test]
fn a_receiver_that_cannot_open_is_a_named_check() {
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args(["doctor", "--duration", "3", "--source", "/nonexistent.wav"])
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let prefix = "FAIL  receiver: could not open the WAV file /nonexistent.wav:";
    assert!(stdout.contains(prefix), "{stdout}");
    assert!(line_after(&stdout, prefix).contains("--source"), "{stdout}");
    assert!(!stdout.contains("verdict:"), "{stdout}");
    assert!(!stderr.contains("Error:"), "{stderr}");
}

#[test]
fn the_example_callsign_fails_the_config_check() {
    let mut setup = Setup::new();
    setup.station = "N0CALL";
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("FAIL  config: server.station_callsign is still the example \"N0CALL\""),
        "{stdout}"
    );
}

/// The keys `manta doctor --json` printed before MAN-126, plus MAN-313's
/// `detectorConfirmations`, under MAN-139's camelCase names, sorted.
const REPORT_KEYS: &[&str] = &[
    "centerFreqHz",
    "charsDecoded",
    "detectorConfirmations",
    "distinctChars",
    "durationS",
    "sampleRateHz",
    "snrMax",
    "snrMedian",
    "snrMin",
    "spotsConfirmed",
    "trackMetaCount",
    "tracksClosed",
    "tracksPromoted",
    "verdict",
];

fn json_object(stdout: &str) -> serde_json::Map<String, serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("doctor --json not one JSON object ({e}): {stdout}"));
    value.as_object().unwrap().clone()
}

#[test]
fn json_adds_checks_and_keeps_every_existing_key() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(0.0);
    let out = setup.doctor(&cfg, ntp, &["--json"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let map = json_object(&stdout);
    for key in REPORT_KEYS {
        assert!(map.contains_key(*key), "no {key} in {stdout}");
    }
    assert_eq!(map.len(), REPORT_KEYS.len() + 2, "{stdout}");
    let checks = map["checks"].as_array().unwrap();
    assert!(!checks.is_empty());
    for check in checks {
        let mut keys: Vec<&str> = check
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "fix", "name", "status"], "{check}");
        let status = check["status"].as_str().unwrap();
        assert!(
            ["PASS", "WARN", "FAIL", "SKIP"].contains(&status),
            "{check}"
        );
    }
    let expected = if host_synced() == Some(false) {
        "WARN"
    } else {
        "PASS"
    };
    assert_eq!(map["checksStatus"], expected, "{stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("PASS")), "{stdout}");
}

#[test]
fn json_without_a_receiver_has_only_checks() {
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args([
            "doctor",
            "--duration",
            "3",
            "--json",
            "--source",
            "/nonexistent.wav",
        ])
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let map = json_object(&stdout);
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["checks", "checksStatus"], "{stdout}");
    assert_eq!(map["checksStatus"], "FAIL");
}

#[test]
fn no_server_and_no_uplink_are_single_skip_rows() {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 4);
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[input]\ntype = \"file\"\npath = \"{}\"\n",
            wav.display().to_string().replace('\\', "\\\\")
        ),
    )
    .unwrap();
    let (ntp, _h) = fake_sntp(0.0);
    let out = manta()
        .args(["doctor", "--duration", "3", "--config"])
        .arg(&cfg)
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("SKIP  ports: no [server] table, so manta run starts no servers\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("SKIP  rbn uplink: no [[rbn_uplink]] block to check\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("telnet port"), "{stdout}");
}

/// MAN-235: `manta doctor` on `silent_48k_wav` (4 s, `--duration 3`) with
/// a fake SNTP server at offset 0, plus `extra`.
fn doctor_on_silence(extra: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let wav = silent_48k_wav(dir.path(), 4);
    let (ntp, _h) = fake_sntp(0.0);
    manta()
        .args(["doctor", "--duration", "3", "--source"])
        .arg(&wav)
        .arg("--ntp-server")
        .arg(format!("127.0.0.1:{ntp}"))
        .args(extra)
        .output()
        .unwrap()
}

/// MAN-235 scenario 1: the README's demo signal, at the default
/// `--duration`, reads DECODING, or WEAK_NO_DECODE followed by a line that
/// says to rerun longer; the suggested rerun reads DECODING.
#[test]
fn the_demo_signal_reads_decoding_or_hints_a_longer_run() {
    let dir = tempfile::tempdir().unwrap();
    let gen = manta()
        .args(["gen", "v1", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(gen.status.success(), "{:?}", text(&gen));
    let wav = dir.path().join("v1.wav");
    let (ntp, _h) = fake_sntp(0.0);
    let doctor = |extra: &[&str]| {
        manta()
            .args(["doctor", "--source-iq", "--source"])
            .arg(&wav)
            .arg("--ntp-server")
            .arg(format!("127.0.0.1:{ntp}"))
            .args(extra)
            .output()
            .unwrap()
    };

    let out = doctor(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    if !stdout.contains("verdict: DECODING") {
        let weak = "verdict: WEAK_NO_DECODE";
        assert!(stdout.contains(weak), "{stdout}");
        let hint = line_after(&stdout, weak);
        assert!(
            hint.starts_with("hint: ") && hint.contains("--duration 60"),
            "{stdout}"
        );
    }

    let out = doctor(&["--duration", "60", "--strict"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("verdict: DECODING"), "{stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("hint:")), "{stdout}");
}

/// MAN-235 scenario 2: without `--strict`, an all-zero file is NO_SIGNAL
/// and still exits 0.
#[test]
fn silence_is_no_signal_and_exits_0_without_strict() {
    let out = doctor_on_silence(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("verdict: NO_SIGNAL"), "{stdout}");
    assert!(!stdout.lines().any(|l| l.starts_with("hint:")), "{stdout}");
}

/// MAN-235 scenario 3: `--strict` turns the same NO_SIGNAL run into exit 3
/// and changes nothing else it prints, in text or JSON.
#[test]
fn strict_exits_3_on_silence_and_changes_nothing_else() {
    let out = doctor_on_silence(&["--strict"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(3),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("verdict: NO_SIGNAL"), "{stdout}");
    if host_synced() != Some(false) {
        assert!(stdout.contains("doctor: all checks passed"), "{stdout}");
    }
    assert!(!stdout.contains("failed:"), "{stdout}");
    assert!(
        !stderr.lines().any(|l| l.starts_with("error:")) && !stderr.contains("Error:"),
        "{stderr}"
    );

    let out = doctor_on_silence(&["--strict", "--json"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(3),
        "stdout={stdout}\nstderr={stderr}"
    );
    let map = json_object(&stdout);
    assert_eq!(map["verdict"], "NO_SIGNAL", "{stdout}");
    assert_ne!(map["checksStatus"], "FAIL", "{stdout}");
}

/// MAN-235: a failed check exits 1 with `--strict` too, whatever the
/// verdict.
#[test]
fn strict_keeps_exit_1_for_a_failed_check() {
    let setup = Setup::new();
    let cfg = setup.config(setup.uplink_port());
    let (ntp, _h) = fake_sntp(120.0);
    let out = setup.doctor(&cfg, ntp, &["--strict"]);
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("FAIL  clock: "), "{stdout}");
    assert!(stdout.contains("verdict: NO_SIGNAL"), "{stdout}");
}

/// MAN-333 scenario 2: tests drawing "nothing here" ports concurrently
/// never collide — every port comes from the band, is issued at most once
/// per process, and stays clear of the kernel's ephemeral pool.
#[test]
fn concurrent_port_handouts_are_distinct_and_inside_the_band() {
    const THREADS: usize = 8;
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                let trio = free_ports::<3>();
                let tcp = free_port();
                let udp = closed_port();
                [trio[0], trio[1], trio[2], tcp, udp]
            })
        })
        .collect();
    let mut ports = Vec::new();
    for handle in handles {
        ports.extend(handle.join().unwrap());
    }
    let band = BAND_START..BAND_START + BAND_LEN;
    for &port in &ports {
        assert!(band.contains(&port), "{port} outside the band");
    }
    let total = ports.len();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), total, "a port was handed out twice");
}
