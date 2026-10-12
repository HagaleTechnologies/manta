//! MAN-81: the published image runs a mounted config with no arguments,
//! reports its health through `manta health`, and is built for the same
//! three platforms everywhere. CI never builds the image on a PR, so the
//! entrypoint script runs here under `/bin/sh` with a fake `manta` first on
//! `PATH`, and once end to end against the real binary with the Dockerfile's
//! own `HEALTHCHECK` command. See
//! docs/DECISIONS/2026-10-11-man81-docker-config-mount-and-healthcheck.md.

mod common;

use std::path::{Path, PathBuf};

const DOCKERFILE: &str = "Dockerfile";
const ENTRYPOINT_SCRIPT: &str = "packaging/docker/docker-entrypoint.sh";
const INSTALLED_ENTRYPOINT: &str = "/usr/local/bin/docker-entrypoint.sh";
/// Where the image looks for a mounted config, and Compose mounts one.
const CONFIG_PATH: &str = "/etc/manta/manta.toml";
/// The script's one line that names `CONFIG_PATH`; tests rewrite it to a
/// temporary directory.
const CONFIG_LINE: &str = "config=/etc/manta/manta.toml";
const PLATFORMS: [&str; 3] = ["linux/amd64", "linux/arm64", "linux/arm/v7"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// A repository file, with CRLF (a Windows checkout) folded to LF.
fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
        .replace("\r\n", "\n")
}

/// One Dockerfile instruction: the keyword upper-cased (instructions are
/// case-insensitive) and its arguments with `\` continuations joined.
#[derive(Debug, Clone)]
struct Directive {
    keyword: String,
    args: String,
}

/// Every instruction in `text`, comments and blank lines dropped.
fn directives(text: &str) -> Vec<Directive> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        // A comment line is dropped even inside a continuation, as Docker
        // does.
        if trimmed.starts_with('#') || (trimmed.is_empty() && pending.is_empty()) {
            continue;
        }
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head.trim_end());
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        let joined = std::mem::take(&mut pending);
        let joined = joined.trim();
        if joined.is_empty() {
            continue;
        }
        let (keyword, args) = joined
            .split_once(char::is_whitespace)
            .unwrap_or((joined, ""));
        out.push(Directive {
            keyword: keyword.to_ascii_uppercase(),
            args: args.trim().to_string(),
        });
    }
    out
}

/// The instructions after the Dockerfile's last `FROM`: the published image.
fn final_stage() -> Vec<Directive> {
    let all = directives(&doc(DOCKERFILE));
    let last_from = all
        .iter()
        .rposition(|d| d.keyword == "FROM")
        .expect("the Dockerfile has a FROM");
    all[last_from + 1..].to_vec()
}

fn only(stage: &[Directive], keyword: &str) -> Directive {
    let found: Vec<_> = stage.iter().filter(|d| d.keyword == keyword).collect();
    assert_eq!(
        found.len(),
        1,
        "the final stage must have exactly one {keyword}: {found:?}"
    );
    found[0].clone()
}

/// `HEALTHCHECK`'s `--name=value` options and its shell-form command.
fn healthcheck() -> (Vec<(String, String)>, String) {
    let hc = only(&final_stage(), "HEALTHCHECK");
    let mut options = Vec::new();
    let mut rest = hc.args.as_str();
    while let Some(opt) = rest.strip_prefix("--") {
        let (word, tail) = opt.split_once(char::is_whitespace).expect("a CMD follows");
        let (name, value) = word.split_once('=').expect("--name=value");
        options.push((name.to_string(), value.to_string()));
        rest = tail.trim_start();
    }
    let (cmd_kw, cmd) = rest.split_once(char::is_whitespace).expect("CMD <command>");
    assert_eq!(
        cmd_kw.to_ascii_uppercase(),
        "CMD",
        "HEALTHCHECK {}",
        hc.args
    );
    let cmd = cmd.trim().to_string();
    assert!(
        !cmd.starts_with('['),
        "HEALTHCHECK must use the shell form, so /bin/sh expands ${{MANTA_CONFIG:-...}}: {cmd}"
    );
    (options, cmd)
}

/// `30s`/`1m30s`-style Docker durations, in seconds.
fn docker_secs(v: &str) -> u64 {
    let mut total = 0;
    let mut num = String::new();
    for c in v.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().unwrap_or_else(|_| panic!("duration {v:?}"));
        num.clear();
        total += match c {
            's' => n,
            'm' => n * 60,
            'h' => n * 3600,
            _ => panic!("unsupported unit in {v:?}"),
        };
    }
    assert!(num.is_empty(), "duration {v:?} lacks a unit");
    total
}

/// kiwi.rs's `OVERALL_CONNECT_TIMEOUT`: the longest a KiwiSDR source may
/// take to connect before `run` fails.
fn kiwi_connect_bound_secs() -> u64 {
    let src = doc("crates/manta-input/src/kiwi.rs");
    let line = src
        .lines()
        .find(|l| l.contains("const OVERALL_CONNECT_TIMEOUT"))
        .expect("kiwi.rs defines OVERALL_CONNECT_TIMEOUT");
    let start = line.find("from_secs(").expect("from_secs(N)") + "from_secs(".len();
    let len = line[start..].find(')').expect("closed");
    line[start..start + len].trim().parse().unwrap()
}

#[test]
fn dockerfile_entrypoint_is_the_installed_script() {
    let stage = final_stage();
    let entrypoint = only(&stage, "ENTRYPOINT");
    let argv: Vec<String> =
        serde_json::from_str(&entrypoint.args).expect("an exec-form (JSON) ENTRYPOINT");
    assert_eq!(argv, [INSTALLED_ENTRYPOINT]);

    assert!(
        stage.iter().any(|d| {
            let words: Vec<&str> = d.args.split_whitespace().collect();
            d.keyword == "COPY" && words.ends_with(&[ENTRYPOINT_SCRIPT, INSTALLED_ENTRYPOINT])
        }),
        "the final stage must COPY {ENTRYPOINT_SCRIPT} to {INSTALLED_ENTRYPOINT}"
    );
    // A CMD would be passed to the script as arguments, and arguments always
    // run `manta <arguments>`: a bare start would never reach the config.
    assert!(
        !stage.iter().any(|d| d.keyword == "CMD"),
        "no CMD in the final stage"
    );
    let pos = |kw: &str, args: &str| {
        stage
            .iter()
            .position(|d| d.keyword == kw && d.args == args)
            .unwrap_or_else(|| panic!("no `{kw} {args}`"))
    };
    assert!(
        pos("USER", "manta") < pos("ENTRYPOINT", &entrypoint.args),
        "the entrypoint runs as the unprivileged manta user"
    );
}

#[cfg(unix)]
#[test]
fn entrypoint_script_is_an_executable_posix_sh_script() {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(repo_root().join(ENTRYPOINT_SCRIPT))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(
        mode & 0o111,
        0,
        "{ENTRYPOINT_SCRIPT} must be executable (COPY keeps the mode)"
    );
    let text = doc(ENTRYPOINT_SCRIPT);
    assert_eq!(text.lines().next(), Some("#!/bin/sh"));
    assert!(
        text.lines().any(|l| l.trim() == "set -eu"),
        "{ENTRYPOINT_SCRIPT} must `set -eu`"
    );
}

#[test]
fn dockerignore_keeps_an_operator_config_out_of_the_build_context() {
    assert!(
        doc(".dockerignore")
            .lines()
            .any(|l| l.trim() == "manta.toml"),
        ".dockerignore must exclude an operator's manta.toml from `COPY . .`"
    );
}

#[test]
fn dockerfile_healthcheck_probes_the_daemon_config() {
    let (options, cmd) = healthcheck();
    let opt = |name: &str| {
        options
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(opt("interval").map(docker_secs), Some(10));
    assert_eq!(opt("retries"), Some("3"));
    assert!(
        opt("start-interval").is_none(),
        "--start-interval needs Docker Engine 25+; older builders reject it"
    );
    let start_period = docker_secs(opt("start-period").expect("--start-period"));
    let kiwi = kiwi_connect_bound_secs();
    assert!(
        start_period >= kiwi + 10,
        "--start-period {start_period}s must cover the KiwiSDR connect bound ({kiwi}s) + 10s"
    );
    let timeout = docker_secs(opt("timeout").expect("--timeout"));

    let prefix =
        format!("manta health --config \"${{MANTA_CONFIG:-{CONFIG_PATH}}}\" --timeout-secs ");
    let probe_secs: u64 = cmd
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("HEALTHCHECK CMD must be `{prefix}<n>`: {cmd}"))
        .parse()
        .expect("--timeout-secs <n>");
    assert!(
        probe_secs < timeout,
        "manta health's own bound ({probe_secs}s) must end before Docker's --timeout \
         ({timeout}s), so the probe reports its own reason"
    );

    // The entrypoint's default path, the health check's and Compose's mount
    // target are one path.
    assert_eq!(
        doc(ENTRYPOINT_SCRIPT)
            .lines()
            .filter(|l| l.trim() == CONFIG_LINE)
            .count(),
        1
    );
    let compose: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&doc("docker-compose.yml")).unwrap();
    let targets: Vec<&str> = compose["services"]["manta"]["volumes"]
        .as_sequence()
        .expect("a volumes list")
        .iter()
        .filter_map(|v| v["target"].as_str())
        .collect();
    assert_eq!(targets, [CONFIG_PATH]);
}

#[test]
fn every_buildx_call_site_builds_the_same_three_platforms() {
    for workflow in [
        ".github/workflows/release.yml",
        ".github/workflows/release-publish.yml",
    ] {
        let text = doc(workflow);
        let lines: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("platforms:"))
            .collect();
        assert!(!lines.is_empty(), "{workflow} has no `platforms:` line");
        for line in lines {
            let mut got: Vec<&str> = line.split(',').map(str::trim).collect();
            got.sort_unstable();
            let mut want = PLATFORMS.to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "{workflow}: platforms:{line}");
        }
    }
}

#[test]
fn docs_list_every_image_platform() {
    for file in [DOCKERFILE, "README.md", "docs/RUNBOOKS/release.md"] {
        let text = doc(file);
        assert!(
            text.contains("linux/arm64"),
            "{file} no longer names the image platforms; update this test"
        );
        assert!(
            text.contains("linux/arm/v7"),
            "{file} names linux/arm64 but not linux/arm/v7"
        );
    }
}

#[cfg(unix)]
mod script {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Output};

    /// The entrypoint, copied into `dir` with its config path moved under
    /// `dir` (so a test can "mount" a file there), mode 0755.
    pub(super) fn entrypoint_copy(dir: &Path) -> PathBuf {
        let text = doc(ENTRYPOINT_SCRIPT);
        assert_eq!(
            text.lines().filter(|l| *l == CONFIG_LINE).count(),
            1,
            "{ENTRYPOINT_SCRIPT} must have exactly one `{CONFIG_LINE}` line"
        );
        let text = text.replacen(
            CONFIG_LINE,
            &format!("config={}", config_at(dir).display()),
            1,
        );
        let path = dir.join("docker-entrypoint.sh");
        write_executable(&path, &text);
        path
    }

    /// Where `entrypoint_copy(dir)` looks for a mounted config.
    pub(super) fn config_at(dir: &Path) -> PathBuf {
        dir.join(CONFIG_PATH.trim_start_matches('/'))
    }

    pub(super) fn write_executable(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `PATH` for a child of the script: `dir/bin` (the `manta` it should
    /// exec) ahead of the system's own `sh`.
    pub(super) fn path_env(dir: &Path) -> String {
        format!("{}:/usr/bin:/bin", dir.join("bin").display())
    }

    /// A command with no inherited `MANTA_*` variable.
    pub(super) fn clean(program: &Path) -> Command {
        let mut cmd = Command::new(program);
        for (key, _) in std::env::vars_os() {
            if key.as_encoded_bytes().starts_with(b"MANTA_") {
                cmd.env_remove(key);
            }
        }
        cmd
    }

    /// Runs `make()`'s command to completion, retrying while the kernel
    /// reports a just-written executable busy (ETXTBSY): another test
    /// thread's fork may briefly hold its write descriptor.
    fn output_retrying(mut make: impl FnMut() -> Command) -> (Output, u32) {
        for _ in 0..50 {
            match make().spawn() {
                Ok(child) => {
                    let pid = child.id();
                    let out = child.wait_with_output().unwrap();
                    if String::from_utf8_lossy(&out.stderr).contains("Text file busy") {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        continue;
                    }
                    return (out, pid);
                }
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("spawning the entrypoint: {e}"),
            }
        }
        panic!("the entrypoint stayed busy (ETXTBSY)");
    }

    /// What the fake `manta` saw.
    #[derive(Debug)]
    struct Seen {
        argv: Vec<String>,
        /// The process the script ran in equals the fake's: it exec'd.
        same_pid: bool,
    }

    /// A scratch dir holding the entrypoint copy and a fake `manta` on its
    /// `PATH` that records its PID and argv.
    struct Harness {
        dir: tempfile::TempDir,
        script: PathBuf,
    }

    impl Harness {
        fn new() -> Harness {
            let dir = tempfile::tempdir().unwrap();
            let script = entrypoint_copy(dir.path());
            write_executable(
                &dir.path().join("bin/manta"),
                "#!/bin/sh\n{ echo \"$$\"; for a in \"$@\"; do printf '%s\\n' \"$a\"; done; } \
                 > \"$FAKE_MANTA_OUT\"\n",
            );
            Harness { dir, script }
        }

        fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Seen {
            let out_file = self.dir.path().join("argv.txt");
            let _ = std::fs::remove_file(&out_file);
            let (out, pid) = output_retrying(|| {
                let mut cmd = clean(&self.script);
                cmd.args(args)
                    .env("PATH", path_env(self.dir.path()))
                    .env("FAKE_MANTA_OUT", &out_file);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                cmd
            });
            assert!(
                out.status.success(),
                "entrypoint {args:?} failed: {:?}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            let recorded = std::fs::read_to_string(&out_file)
                .unwrap_or_else(|e| panic!("the fake manta never ran for {args:?}: {e}"));
            let mut lines = recorded.lines().map(str::to_string);
            let seen_pid: u32 = lines.next().unwrap().parse().unwrap();
            Seen {
                argv: lines.collect(),
                same_pid: seen_pid == pid,
            }
        }

        fn config(&self) -> PathBuf {
            config_at(self.dir.path())
        }
    }

    /// Every case also proves the exec: manta is PID 1 in the container and
    /// gets `docker stop`'s SIGTERM itself (MAN-85).
    fn assert_execs(seen: &Seen, want: &[&str]) {
        assert_eq!(seen.argv, want);
        assert!(
            seen.same_pid,
            "the entrypoint forked instead of exec'ing manta: {seen:?}"
        );
    }

    #[test]
    fn bare_start_without_a_config_prints_help() {
        let h = Harness::new();
        assert_execs(&h.run(&[], &[]), &["--help"]);
    }

    #[test]
    fn bare_start_with_a_mounted_config_runs_it() {
        let h = Harness::new();
        let cfg = h.config();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[server]\n").unwrap();
        let cfg = cfg.to_str().unwrap();
        assert_execs(&h.run(&[], &[]), &["run", "--config", cfg]);
    }

    /// `docker run -v` with a missing host file mounts a directory: manta
    /// must get it and say so, not have the container quietly print help.
    #[test]
    fn bare_start_with_a_directory_at_the_config_path_hands_it_to_manta() {
        let h = Harness::new();
        let cfg = h.config();
        std::fs::create_dir_all(&cfg).unwrap();
        let cfg = cfg.to_str().unwrap();
        assert_execs(&h.run(&[], &[]), &["run", "--config", cfg]);
    }

    #[test]
    fn bare_start_with_manta_config_runs_the_daemon() {
        let h = Harness::new();
        assert_execs(&h.run(&[], &[("MANTA_CONFIG", "/x/manta.toml")]), &["run"]);
        let cfg = h.config();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[server]\n").unwrap();
        assert_execs(&h.run(&[], &[("MANTA_CONFIG", "/x/manta.toml")]), &["run"]);
        // An empty MANTA_CONFIG is unset, as `run` treats it.
        let cfg = cfg.to_str().unwrap();
        assert_execs(
            &h.run(&[], &[("MANTA_CONFIG", "")]),
            &["run", "--config", cfg],
        );
    }

    #[test]
    fn arguments_pass_through_unchanged() {
        let h = Harness::new();
        // A mounted config and MANTA_CONFIG must not change what arguments do.
        let cfg = h.config();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[server]\n").unwrap();

        let compose: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&doc("docker-compose.yml")).unwrap();
        let command: Vec<String> = compose["services"]["manta"]["command"]
            .as_sequence()
            .expect("command is an exec-form list")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let command: Vec<&str> = command.iter().map(String::as_str).collect();

        for args in [
            &["--help"][..],
            &command[..],
            &["decode", "a b.wav", "--json"][..],
            &[""][..],
        ] {
            assert_execs(&h.run(args, &[]), args);
            assert_execs(&h.run(args, &[("MANTA_CONFIG", "/x/manta.toml")]), args);
        }
    }
}

/// Scenario 1 and Scenario 2 end to end, without Docker: the entrypoint
/// started with no arguments runs the real daemon from the "mounted"
/// config, and the Dockerfile's own HEALTHCHECK command tracks it.
#[cfg(unix)]
#[test]
fn bare_start_runs_the_mounted_config_and_its_health_check_tracks_it() {
    use script::{clean, config_at, entrypoint_copy, path_env};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_manta"), dir.path().join("bin/manta")).unwrap();
    let script = entrypoint_copy(dir.path());

    // The shipped example, edited the way packaging/README.md says, plus a
    // file source and a TOML-set metrics port a fixed-port probe would miss.
    let metrics_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut text = doc("manta.example.toml");
    let station = "\nstation_callsign = \"N0CALL\"\n";
    assert_eq!(text.matches(station).count(), 1);
    text = text.replacen(
        station,
        &format!("\nstation_callsign = \"W3XYZ\"\nmetrics_port = {metrics_port}\n"),
        1,
    );
    assert_eq!(text.matches("\n[input]\n").count(), 1);
    text = text.replacen(
        "\n[input]\n",
        &format!(
            "\n[input]\ntype = \"file\"\npath = {:?}\ncenter_freq_hz = 14060000.0\n",
            common::fixture_wav().to_str().unwrap()
        ),
        1,
    );
    let cfg = config_at(dir.path());
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, text).unwrap();

    // The container's environment: the daemon and its health check share it.
    let env = [
        ("PATH", path_env(dir.path())),
        ("MANTA_SERVER_BIND_ADDR", "127.0.0.1".to_string()),
        ("MANTA_SERVER_TELNET_PORT", "0".to_string()),
        ("MANTA_SERVER_JSON_PORT", "0".to_string()),
        ("MANTA_SERVER_STATUS_INTERVAL_SECS", "0".to_string()),
    ];
    let spawn = || {
        let mut cmd = clean(&script);
        cmd.envs(env.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for _ in 0..50 {
            match cmd.spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => return other.expect("spawning the entrypoint"),
            }
        }
        panic!("the entrypoint stayed busy (ETXTBSY)");
    };
    let mut child = spawn();
    let lines = common::line_channel(child.stderr.take().unwrap());
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut daemon = Guard(child);
    // Scenario 1: the mounted config ran, not --help.
    let banner = common::expect_line(&lines, &["listening:"], Duration::from_secs(60));
    assert!(banner.contains("station=W3XYZ"), "{banner}");

    // Scenario 2: the Dockerfile's own HEALTHCHECK command, with the default
    // path moved to this test's "mount".
    let (_, hc) = healthcheck();
    assert_eq!(hc.matches(CONFIG_PATH).count(), 1, "{hc}");
    let hc = hc.replace(CONFIG_PATH, cfg.to_str().unwrap());
    let check = || {
        let mut cmd = clean(Path::new("/bin/sh"));
        cmd.arg("-c").arg(&hc).envs(env.iter().map(|(k, v)| (k, v)));
        cmd.output().unwrap()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let healthy = loop {
        let o = check();
        if o.status.code() == Some(0) {
            break o;
        }
        assert!(
            Instant::now() < deadline,
            "HEALTHCHECK never passed: exit {:?}, stdout {:?}, stderr {:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        String::from_utf8_lossy(&healthy.stdout).lines().next(),
        Some("ok")
    );

    // SIGTERM to the entrypoint's PID reaches manta itself (it exec'd).
    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) },
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );
    let status = common::wait_bounded(&mut daemon.0, Duration::from_secs(15))
        .expect("SIGTERM did not stop manta within 15 s");
    assert_eq!(status.code(), Some(0), "drain must exit 0: {status:?}");

    let o = check();
    assert_eq!(
        o.status.code(),
        Some(1),
        "a stopped daemon must fail the check: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}
