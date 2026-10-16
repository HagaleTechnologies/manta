//! The public docs must match the shipped binary. MAN-145.
//!
//! These assertions exist because README.md, ARCHITECTURE.md, CLAUDE.md and
//! wiki/pages/overview.md independently drifted to three different crate
//! counts and a two-milestone-stale description of the output layer. Each
//! test below pins one claim a first-time visitor reads.

use manta_dsp::channelizer::Channelizer;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
        // Windows checkouts may convert LF to CRLF; the tests below parse LF-delimited
        // frontmatter and line structure.
        .replace("\r\n", "\n")
}

/// Every run of whitespace collapsed to a single space, so a phrase that a
/// Markdown reflow split across two lines still matches.
fn squash_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text between a `## <heading>` and the next `## ` heading.
fn section<'a>(md: &'a str, heading: &str) -> &'a str {
    let start = md
        .find(&format!("\n## {heading}"))
        .unwrap_or_else(|| panic!("no `## {heading}` section"));
    let rest = &md[start + 1..];
    let end = rest[3..].find("\n## ").map(|i| i + 4).unwrap_or(rest.len());
    &rest[..end]
}

fn workspace_members() -> Vec<String> {
    let manifest: toml::Value = toml::from_str(&doc("Cargo.toml")).expect("parse Cargo.toml");
    manifest["workspace"]["members"]
        .as_array()
        .expect("members array")
        .iter()
        .map(|m| {
            m.as_str()
                .expect("member string")
                .rsplit('/')
                .next()
                .expect("crate name")
                .to_string()
        })
        .collect()
}

// ---- Scenario 3: the crate count is accurate everywhere ----

/// Every crate in the workspace must be named somewhere in ARCHITECTURE.md
/// (the assertion searches the whole document, not only the crate tree --
/// a crate dropped from the tree but still named in prose passes here, and
/// that is deliberate: prose mentions are a legitimate home for a crate the
/// tree does not draw). `manta-soak-harness` was named nowhere at all for
/// the whole of M2/M3.
#[test]
fn architecture_lists_every_workspace_crate() {
    let arch = doc("ARCHITECTURE.md");
    for krate in workspace_members() {
        assert!(
            arch.contains(&krate),
            "ARCHITECTURE.md never mentions workspace member `{krate}`"
        );
    }
}

/// README, CLAUDE.md and the wiki overview must all state the same crate
/// count, and it must be the real one. ARCHITECTURE.md is deliberately not
/// in the list: it states no count in words, and is covered for crate
/// *names* by `architecture_lists_every_workspace_crate` above. Guard-
/// listing the stale spellings keeps the failure message actionable when a
/// crate is added.
#[test]
fn every_doc_states_the_real_crate_count() {
    let n = workspace_members().len();
    assert_eq!(
        n, 9,
        "crate count changed to {n}: update the docs below and this test"
    );

    let stale = [
        "seven-crate",
        "eight-crate",
        "8-crate",
        "7-crate",
        "six-crate",
    ];
    let fresh = ["nine-crate", "9-crate"];

    for file in ["README.md", "CLAUDE.md", "wiki/pages/overview.md"] {
        let text = doc(file);
        for s in stale {
            assert!(
                !text.contains(s),
                "{file} still says `{s}` (workspace has {n} crates)"
            );
        }
        assert!(
            fresh.iter().any(|f| text.contains(f)),
            "{file} states no crate count; expected one of {fresh:?}"
        );
    }
}

// ---- Scenario 2: the Inputs table lists every shipped input ----

/// Every input the CLI can be built with needs a row. HPSDR/Hermes shipped
/// (crates/manta-input/src/hpsdr.rs) and had no row at all.
#[test]
fn readme_inputs_table_covers_every_shipped_input() {
    let readme = doc("README.md");
    let inputs = squash_whitespace(section(&readme, "Inputs"));
    for needle in [
        "WAV file",
        "--device",
        "--kiwi-host",
        "--soapy-driver",
        "--hpsdr-host",
        "--ka9q-group",
    ] {
        assert!(
            inputs.contains(needle),
            "README Inputs table has no row for `{needle}`"
        );
    }
}

// ---- Scenario 1: Outputs and Status describe shipped capability ----

/// The telnet server, JSON/WebSocket stream, RBN uplink and Prometheus
/// endpoint have all shipped and are covered by manta-server's test suite.
#[test]
fn readme_outputs_describes_shipped_servers() {
    let readme = doc("README.md");
    // Squashed like the ROADMAP guard below: a reflow that split one of
    // these phrases across two lines would otherwise disable the assertion
    // silently instead of reporting real drift.
    let outputs = squash_whitespace(section(&readme, "Outputs"));
    assert!(
        !outputs.contains("in progress"),
        "README Outputs still calls a shipped server \"in progress\""
    );
    for needle in ["7300", "7301", "7302", "7303", "uplink", "--config"] {
        assert!(
            outputs.contains(needle),
            "README Outputs never mentions `{needle}`"
        );
    }
}

/// MAN-123: README Outputs says where `run`'s human output goes.
#[test]
fn readme_outputs_says_where_run_prints_spots_and_decoded_text() {
    let outputs = squash_whitespace(section(&doc("README.md"), "Outputs"));
    for needle in ["stdout", "stderr", "one line per track", "--decoded-text"] {
        assert!(
            outputs.contains(needle),
            "README Outputs never mentions `{needle}`"
        );
    }
}

/// The Status block must not list shipped work as upcoming.
#[test]
fn readme_status_does_not_promise_shipped_work() {
    let readme = doc("README.md");
    let status = squash_whitespace(section(&readme, "Status"));
    for stale in ["the telnet and JSON spot servers", "TOML config, metrics"] {
        assert!(
            !status.contains(stale),
            "README Status still lists `{stale}` as future work"
        );
    }
}

/// MAN-298 / decision D7: until manta clears its M2/M3 acceptance gates, the
/// Status block says how far to trust a release.
#[test]
fn readme_status_says_pre_stability_alpha() {
    let status = squash_whitespace(section(&doc("README.md"), "Status")).to_lowercase();
    assert!(
        status.contains("pre-stability alpha, expect breakage"),
        "README Status must say \"pre-stability alpha, expect breakage\" (decision D7)"
    );
}

/// MAN-298: Installation offers a download of every archive the release
/// workflow builds, so a platform added to the build matrix is documented too.
/// MAN-82: that includes a standalone job's literal upload name
/// (`build-soapy`'s `name: manta-linux-x86_64-soapy`), not only matrix
/// `artifact:` entries.
#[test]
fn readme_installation_offers_every_release_archive() {
    let install = squash_whitespace(section(&doc("README.md"), "Installation"));
    assert!(
        !install.contains("There is no tagged release yet"),
        "README Installation still says no release exists"
    );
    assert!(
        install.contains("releases/latest/download/"),
        "README Installation gives no download command"
    );
    let workflow = doc(".github/workflows/release-publish.yml");
    let mut artifacts: Vec<&str> = workflow
        .lines()
        .filter_map(|l| l.trim().strip_prefix("artifact: "))
        .collect();
    assert!(
        !artifacts.is_empty(),
        "no `artifact:` entries in the release build matrix"
    );
    let uploads: Vec<&str> = workflow
        .lines()
        .filter_map(|l| l.trim().strip_prefix("name: "))
        .filter(|name| name.starts_with("manta-") && !name.contains("${{"))
        .collect();
    assert!(
        uploads.contains(&"manta-linux-x86_64-soapy"),
        "release-publish.yml no longer uploads `manta-linux-x86_64-soapy` by name"
    );
    artifacts.extend(uploads);
    for artifact in artifacts {
        let ext = if artifact.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let archive = format!("{artifact}.{ext}");
        assert!(
            install.contains(&archive),
            "README Installation never names release archive `{archive}`"
        );
    }
}

/// Every command in the Quickstart must run on a build the reader was told
/// to make. `--soapy-driver` needs `--features soapy`, which is in neither
/// the default build nor any default release binary.
#[test]
fn readme_quickstart_only_uses_default_build_flags() {
    let readme = doc("README.md");
    let quickstart = squash_whitespace(section(&readme, "60-second demo"));
    for gated in ["--soapy-driver", "--soapy-freq", "--soapy-rate"] {
        assert!(
            !quickstart.contains(gated),
            "60-second demo uses feature-gated flag `{gated}`; it fails with \
             `error: unexpected argument` on a default build or default release binary"
        );
    }
}

/// MAN-82: a SoapySDR operator can pick the Soapy archive from Installation
/// and install what it needs to start, without a Rust toolchain.
#[test]
fn readme_installation_documents_the_soapy_archive() {
    let install = squash_whitespace(section(&doc("README.md"), "Installation"));
    assert!(
        install.contains(
            "| Linux x86-64 with SoapySDR, Ubuntu 24.04 | `manta-linux-x86_64-soapy.tar.gz` |"
        ),
        "README Installation has no archive-table row for the Soapy archive"
    );
    assert!(
        !install.contains("SoapySDR needs a build from source"),
        "README Installation still says SoapySDR needs a build from source"
    );
    for needle in [
        "### SoapySDR on Linux x86_64",
        "Ubuntu 24.04",
        "libsoapysdr0.8",
        "libasound2t64",
        "soapysdr0.8-module-rtlsdr",
        "soapysdr0.8-module-airspy",
        "soapysdr0.8-module-hackrf",
        "releases/latest/download/manta-linux-x86_64-soapy.tar.gz",
        "features: soapy",
        "libSoapySDR.so.0.8: cannot open shared object file",
        "SoapySDR::Device::make() no match",
        "unexpected argument '--soapy-driver'",
        "https://github.com/pothosware/SoapySDRPlay3/blob/master/README.md",
        "You do not need Rust",
    ] {
        assert!(
            install.contains(needle),
            "README Installation's Soapy setup never says `{needle}`"
        );
    }
    let notes = install.to_lowercase();
    assert!(
        notes.contains("bundles neither the vendor api nor driver modules"),
        "README must say the Soapy archive bundles no SDRplay API or device modules"
    );
}

/// MAN-82: `soapysdr0.8-module-airspy` drives the Airspy R2/Mini only. The
/// Airspy HF+ (manta's reference device) needs SoapyAirspyHF, which Ubuntu
/// 24.04 does not package, so the module table must not send HF+ users to
/// the wrong package.
#[test]
fn readme_soapy_setup_separates_airspy_hf_plus() {
    let install = squash_whitespace(section(&doc("README.md"), "Installation"));
    for needle in [
        "| Airspy R2 or Airspy Mini | `soapysdr0.8-module-airspy` |",
        "| Airspy HF+ |",
        "libairspyhf-dev",
        "https://github.com/pothosware/SoapyAirspyHF",
    ] {
        assert!(
            install.contains(needle),
            "README Installation's Soapy module table never says `{needle}`"
        );
    }
    assert!(
        !install.contains("| Airspy | `soapysdr0.8-module-airspy` |"),
        "README Installation still offers `soapysdr0.8-module-airspy` for every Airspy"
    );
}

// ---- Scenario 4: ROADMAP's RBN-admission item ----

/// RBN admission is active P0 work (MAN-40; decision D1, 2026-09-06), not a
/// deferred post-1.0 idea.
#[test]
fn roadmap_does_not_defer_rbn_admission() {
    let roadmap = doc("ROADMAP.md");
    let post = squash_whitespace(section(&roadmap, "Post-1.0 candidates"));
    assert!(
        !post.contains("RBN operators"),
        "ROADMAP still defers RBN-operator admission to post-1.0"
    );
}

/// M3's own prose must not list the shipped manta-server as remaining.
#[test]
fn roadmap_m3_does_not_list_shipped_server_as_remaining() {
    let roadmap = doc("ROADMAP.md");
    // Collapse runs of whitespace so the search survives a paragraph reflow:
    // the phrase is line-wrapped in the source Markdown, and matching the raw
    // text would panic on `.expect()` instead of reporting a real drift.
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    assert!(
        !m3[idx..].contains("manta-server"),
        "ROADMAP M3 still lists `manta-server` as a remaining sub-project"
    );
}

// ---- Runnable-demo hygiene: no unmarked placeholder receivers ----

/// Lines inside ```-fenced blocks, so prose and table cells (which name
/// `--kiwi-host` without an argument) are not mistaken for commands.
fn fenced_lines(md: &str) -> Vec<&str> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in md.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out
}

/// A command a newcomer is invited to copy must not carry a hostname that
/// can never resolve to a receiver. The demo shipped `--kiwi-host
/// kiwi.example.org` -- reserved by RFC 2606, so the advertised
/// hardware-free live path dead-ends on a connection error with nothing in
/// the README saying the host was yours to supply. Every `--kiwi-host`
/// argument in a copyable block must be an angle-bracket placeholder, and
/// the demo must point at somewhere real receivers are listed.
#[test]
fn readme_kiwi_commands_use_a_marked_placeholder_host() {
    let readme = doc("README.md");
    for line in fenced_lines(&readme) {
        let mut toks = line.split_whitespace();
        while let Some(tok) = toks.next() {
            if tok != "--kiwi-host" {
                continue;
            }
            let host = toks
                .next()
                .unwrap_or_else(|| panic!("`--kiwi-host` with no argument in: {line}"));
            assert!(
                host.starts_with('<') && host.ends_with('>'),
                "README command uses `--kiwi-host {host}`; a copyable command needs an \
                 explicit `<placeholder>` the reader must replace, not a host that \
                 cannot connect"
            );
        }
    }

    let demo = squash_whitespace(section(&readme, "60-second demo"));
    assert!(
        demo.contains("kiwisdr.com/public") || demo.contains("rx.linkfanel.net"),
        "60-second demo names a KiwiSDR placeholder but points nowhere the reader \
         can find a real public receiver"
    );
}

// ---- Review round 1 (PR #142): the documented commands must stay coherent ----

/// Fenced lines with shell line-continuations joined, so a command split
/// across two lines with a trailing `\` is still inspected as one command.
fn fenced_commands(md: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in fenced_lines(md) {
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(trimmed);
        out.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

/// The Kiwi prose tells the reader to substitute the receiver's hostname
/// *and port*, and `--kiwi-port` defaults to 8073 in silence -- so a command
/// that offers only a host placeholder cannot reach a receiver on any other
/// port. Both copyable Kiwi commands need a marked port placeholder too.
#[test]
fn readme_kiwi_commands_offer_a_marked_placeholder_port() {
    let readme = doc("README.md");
    for cmd in fenced_commands(&readme) {
        if !cmd.contains("--kiwi-host") {
            continue;
        }
        let mut toks = cmd.split_whitespace();
        let mut port = None;
        while let Some(tok) = toks.next() {
            if tok == "--kiwi-port" {
                port = toks.next();
            }
        }
        let port = port.unwrap_or_else(|| {
            panic!(
                "README Kiwi command has no `--kiwi-port`, so it silently uses the 8073 \
                 default the surrounding prose tells the reader to replace: {cmd}"
            )
        });
        assert!(
            port.starts_with('<') && port.ends_with('>'),
            "README command uses `--kiwi-port {port}`; the reader must be shown a \
             `<placeholder>` to replace, not a fixed port"
        );
    }
}

/// M3's remaining-work prose must not quote a subset of the accept-when list.
/// It previously named the 7-day soak but not the stock-DX-cluster-client
/// session, which this branch's own verification notes leave open -- making
/// M3 read as closer to acceptance than it is.
#[test]
fn roadmap_m3_remaining_work_names_every_open_acceptance_gate() {
    let roadmap = doc("ROADMAP.md");
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Remaining M3 sub-projects")
        .expect("M3 remaining-work sentence");
    let remaining = &m3[idx..];
    for gate in [
        "parity benchmark",
        "7-day unattended soak",
        "cqdx",
        "stock DX-cluster client",
    ] {
        assert!(
            remaining.contains(gate),
            "ROADMAP M3's remaining-work prose never mentions the open accept-when gate \
             `{gate}`"
        );
    }
}

/// MAN-269: ROADMAP M3 points at the README's paced-replay demo. "60-second
/// demo" is that README section's title, not the replay's length -- the
/// `gen v1` replay keeps the server up for 120 seconds -- so the pointer must
/// link the section and state the real length rather than read as a
/// 60-second run.
#[test]
fn roadmap_m3_replay_demo_pointer_states_the_real_length() {
    let roadmap = doc("ROADMAP.md");
    let m3 = squash_whitespace(section(&roadmap, "M3"));
    let idx = m3
        .find("Since MAN-269")
        .expect("M3 MAN-269 replay sentence");
    let sentence = &m3[idx..idx + m3[idx..].find(").").expect("sentence end") + 2];
    assert!(
        sentence.contains("(README.md#60-second-demo)"),
        "ROADMAP M3's replay pointer does not link the README section: {sentence}"
    );
    assert!(
        sentence.contains("120 seconds"),
        "ROADMAP M3's replay pointer does not state the 120-second replay: {sentence}"
    );
    let readme = doc("README.md");
    // `section()` prefix-matches; the anchor only resolves on this exact heading.
    assert!(
        readme.contains("\n## 60-second demo\n"),
        "README's `## 60-second demo` heading changed; ROADMAP M3's #60-second-demo link is broken"
    );
    assert!(
        squash_whitespace(section(&readme, "60-second demo")).contains("full 120 seconds"),
        "README's demo no longer says the replay runs 120 seconds; update ROADMAP M3 too"
    );
}

/// The input rates of the normative rate table in `docs/SPEC-decode-core.md`
/// §1.1, in kS/s (`96`, `192`, `384`, `768`). The README's supported-rate
/// list must name every one of them: a rate the SPEC admits but the README
/// omits reads to a visitor as "your recording is unsupported" when the
/// channelizer would in fact have accepted it.
fn spec_table_rates_ks() -> Vec<u32> {
    let spec = doc("docs/SPEC-decode-core.md");
    let start = spec.find("### 1.1 Dimensions").expect("SPEC §1.1 heading");
    let body = &spec[start..];
    let end = body[1..]
        .find("\n### ")
        .map(|i| i + 1)
        .unwrap_or(body.len());
    let mut rates = Vec::new();
    for line in body[..end].lines() {
        let Some(cell) = line.strip_prefix('|').and_then(|l| l.split('|').next()) else {
            continue;
        };
        let digits: String = cell.chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(hz) = digits.parse::<u32>() {
            rates.push(hz / 1000);
        }
    }
    assert!(
        rates.contains(&768),
        "SPEC §1.1 table parsed as {rates:?}; expected it to name 768 kS/s"
    );
    rates
}

/// `--source-iq` only changes how the WAV's samples are *interpreted*; it is
/// not a resampler. `Channelizer::new` rejects any rate whose `fs / 93.75`
/// is not a power of two (`crates/manta-dsp/src/channelizer.rs`), and
/// `--capture-rate-hz` only decimates by a power of two into another table
/// rate (`manta_dsp::decimate::Decimator::new`) -- so a 100 or 250 kS/s
/// recording cannot be replayed at all, by either route. The README must
/// therefore not promise IQ replay "at any rate", and must name the table
/// constraint wherever it describes IQ replay.
#[test]
fn readme_does_not_promise_iq_replay_at_any_rate() {
    let readme = squash_whitespace(&doc("README.md"));
    assert!(
        !readme.contains("at any rate"),
        "README promises IQ file replay `at any rate`, but the channelizer only accepts \
         rates where fs/93.75 is a power of two and --capture-rate-hz only decimates by a \
         power of two -- a 100 kS/s recording is rejected outright"
    );
    // ...nor may it frame the rates it *does* name as the closed set. The
    // rule admits every `fs` with `fs / 93.75` a power of two, which is
    // unbounded in both directions (24 kS/s and 1536 kS/s both construct);
    // "any other rate is rejected" reads to a reader holding one of the
    // unlisted-but-valid rates as a flat no.
    assert!(
        !readme.contains("at any other rate"),
        "README calls every rate outside its own example list rejected, but the list is \
         not the whole set -- 24 kS/s is admitted and unlisted in SPEC §1.1's table"
    );
    assert!(
        readme.contains("fs / 93.75"),
        "README never states the channelizer's supported-rate rule (fs / 93.75 a power of \
         two), so a reader with an off-table recording has nothing to check their file \
         against"
    );
    // Each place the rule is stated must also spell out the rates it admits
    // -- the rule alone ("a power of two") makes the reader do the
    // arithmetic before they can tell whether their own recording is
    // replayable.
    // 24 and 48 kS/s are admitted rates SPEC §1.1's *table* leaves out: 48
    // kS/s is the audio-passband rate the section names in prose, and 24 kS/s
    // is the decimated rate `crates/manta-cli/tests/cli.rs`'s
    // `capture_rate_hz_that_divides_evenly_decimates_and_still_decodes`
    // already decodes end-to-end (24000 / 93.75 = 256, a power of two). The
    // rest come from the table itself.
    let mut admitted = vec![24, 48];
    admitted.extend(spec_table_rates_ks());
    admitted.dedup();
    for ks in &admitted {
        let fs = f64::from(*ks) * 1000.0;
        assert!(
            Channelizer::new(fs, 14_000_000.0).is_ok(),
            "this test claims the README must advertise {ks} kS/s, but \
             Channelizer::new rejects it"
        );
    }
    for (i, _) in readme.match_indices("fs / 93.75") {
        let window = &readme[i..readme.len().min(i + 400)];
        for ks in &admitted {
            assert!(
                window.contains(&ks.to_string()),
                "README states the fs / 93.75 rule without naming the supported table \
                 rate {ks} kS/s near it: {window}"
            );
        }
        assert!(
            window.contains("kS/s"),
            "README states the fs / 93.75 rule without units on the rates it admits: \
             {window}"
        );
    }
}

// ---- Review round 2 (PR #142): the wiki's freshness stamp must not lag ----

/// `wiki/pages/overview.md` carries a `verified: {commit, date}` frontmatter
/// stamp that readers use to judge how stale the page is. This branch
/// rewrote the page's body (nine-crate layout, "implementation is well
/// underway" replacing "no implementation has started") while the stamp
/// still pointed at `e68b106` / `2026-07-07` -- a revision predating every
/// one of those claims, so the metadata vouched for content that did not
/// exist when it was written. The in-repo precedent for a substantive
/// correction is to refresh the stamp with the change
/// (`wiki/pages/detector-tracks.md`, `wiki/pages/spot-validation.md`).
#[test]
fn wiki_overview_verified_stamp_is_not_the_pre_rewrite_one() {
    let page = doc("wiki/pages/overview.md");
    let front = page
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---").map(|(f, _)| f))
        .expect("overview.md frontmatter block");

    let field = |key: &str| -> String {
        front
            .lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
            .unwrap_or_else(|| panic!("overview.md frontmatter has no `{key}` field"))
    };

    // The body claims this branch introduced; if they are present the stamp
    // must have moved with them.
    let claims = ["nine-crate", "Implementation is well underway"];
    for claim in claims {
        assert!(
            page.contains(claim),
            "overview.md no longer claims `{claim}`; this test pins the stamp to those \
             claims and needs updating with them"
        );
    }

    let (commit, date) = (field("commit:"), field("date:"));
    assert_ne!(
        (commit.as_str(), date.as_str()),
        ("e68b106", "2026-07-07"),
        "overview.md still stamps `verified: e68b106 / 2026-07-07`, which predates the \
         nine-crate and shipped-status claims now on the page: refresh \
         `verified.commit`/`verified.date` whenever the body is substantively corrected"
    );
    assert!(
        date.as_str() >= "2026-09-05",
        "overview.md's `verified.date` ({date}) predates the 2026-09-05 broad review that \
         found the page's claims stale, so the stamp cannot be vouching for the current body"
    );
}

// ---- MAN-152: wiki/manifest.json is a true index of wiki/pages ----

/// The `verified: {commit, date}` stamp in a wiki page's frontmatter, as
/// written (not YAML-typed, so an all-digit or `1e5`-shaped short hash stays
/// text).
fn wiki_verified_stamp(rel: &str) -> (String, String) {
    let page = doc(rel);
    let front = page
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---").map(|(f, _)| f))
        .unwrap_or_else(|| panic!("{rel} has no `---` frontmatter block"));
    let verified: Vec<&str> = front
        .lines()
        .skip_while(|l| *l != "verified:")
        .skip(1)
        .take_while(|l| l.starts_with(' '))
        .collect();
    let field = |key: &str| -> String {
        verified
            .iter()
            .find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()))
            .unwrap_or_else(|| panic!("{rel} frontmatter has no `verified.{key}` field"))
    };
    (field("commit:"), field("date:"))
}

fn wiki_page_body(page: &str) -> &str {
    page.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body)
        .expect("wiki page has a complete frontmatter block")
}

/// The frontmatter fields a `wiki/manifest.json` entry copies from its page,
/// in the manifest's shape: `locked` is `false` unless the page says
/// otherwise, and `links: []` is an empty list.
fn wiki_mirrored_frontmatter(rel: &str, page: &str) -> serde_json::Value {
    let front = page
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(front, _)| front)
        .unwrap_or_else(|| panic!("{rel} has no complete `---` frontmatter block"));
    let lines: Vec<&str> = front.lines().collect();
    let scalar = |key: &str| -> Option<&str> {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':').map(str::trim))
    };
    let required = |key: &str| -> &str {
        scalar(key).unwrap_or_else(|| panic!("{rel} frontmatter has no `{key}` field"))
    };
    let list = |key: &str| -> Vec<&str> {
        match required(key) {
            "[]" => Vec::new(),
            "" => lines
                .iter()
                .skip_while(|l| {
                    l.strip_prefix(key)
                        .and_then(|r| r.strip_prefix(':'))
                        .is_none()
                })
                .skip(1)
                .map_while(|l| l.strip_prefix("  - "))
                .map(str::trim)
                .collect(),
            other => panic!("{rel} frontmatter `{key}: {other}` is not a list"),
        }
    };
    let locked = match scalar("locked") {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => panic!("{rel} frontmatter `locked: {other}` is not a boolean"),
    };
    serde_json::json!({
        "id": required("id"),
        "title": required("title"),
        "kind": required("kind"),
        "status": required("status"),
        "maintainer": required("maintainer"),
        "locked": locked,
        "sources": list("sources"),
        "links": list("links"),
    })
}

fn wiki_page_stems(names: Vec<String>) -> Vec<String> {
    let mut stems: Vec<String> = names
        .into_iter()
        .map(|n| n.trim_end_matches(".md").to_string())
        .collect();
    stems.sort();
    stems
}

#[test]
fn wiki_manifest_hash_input_excludes_frontmatter() {
    for page in [
        "---\nid: example\n---\nBody.\n",
        "---\nid: renamed\n---\nBody.\n",
    ] {
        assert_eq!(wiki_page_body(page), "Body.\n");
    }
}

#[test]
fn wiki_manifest_mirrors_every_copied_frontmatter_field() {
    let page = "---\nid: example\ntitle: What is this?\nkind: gotcha\nstatus: current\n\
                maintainer: agent\nsources:\n  - README.md\n  - docs/X.md#anchor\n\
                verified:\n  commit: abc1234\n  date: 2026-10-13\nlinks: []\n---\nBody.\n";
    assert_eq!(
        wiki_mirrored_frontmatter("example.md", page),
        serde_json::json!({
            "id": "example",
            "title": "What is this?",
            "kind": "gotcha",
            "status": "current",
            "maintainer": "agent",
            "locked": false,
            "sources": ["README.md", "docs/X.md#anchor"],
            "links": [],
        })
    );
    let locked = page.replace("links: []\n", "locked: true\nlinks: \n  - overview\n");
    let fields = wiki_mirrored_frontmatter("example.md", &locked);
    assert_eq!(fields["locked"], true);
    assert_eq!(fields["links"], serde_json::json!(["overview"]));
}

#[test]
fn wiki_manifest_orders_prefix_ids_by_stem() {
    // md_files_in sorts these filenames in this order, before removing .md.
    let names = vec!["determinism-ci.md".into(), "determinism.md".into()];
    assert_eq!(wiki_page_stems(names), ["determinism", "determinism-ci"]);
}

/// `wiki/manifest.json` is the wiki's index, and each page's `verified`
/// stamp is what a reader uses to judge how stale it is. The 2026-09-05
/// broad review found both three months old: all ten manifest hashes
/// stale, seven pages missing from it, two pages with no frontmatter, and
/// stamps older than the decision records their own pages cite. A page
/// edit now has to carry the manifest with it. A hash is the SHA-256 of
/// the body after the closing frontmatter delimiter, matching wiki-manifest/v1.
/// The hash does not cover the frontmatter, so each entry's `id`, `title`,
/// `kind`, `status`, `maintainer`, `locked`, `sources`, `links` and
/// `verified_*` are compared with the page's frontmatter separately.
/// `doc()` normalises CRLF to LF, so a Windows checkout hashes the same.
#[test]
fn wiki_manifest_lists_every_page_with_a_matching_hash() {
    use sha2::{Digest, Sha256};

    let manifest: serde_json::Value =
        serde_json::from_str(&doc("wiki/manifest.json")).expect("parse wiki/manifest.json");
    assert_eq!(manifest["schema"], "wiki-manifest/v1");
    assert_eq!(manifest["repo"], "manta");
    assert_eq!(manifest["visibility"], "public");
    let entries = manifest["pages"]
        .as_array()
        .expect("wiki/manifest.json has a `pages` array");
    let str_field = |entry: &serde_json::Value, key: &str| -> String {
        entry[key]
            .as_str()
            .unwrap_or_else(|| panic!("wiki/manifest.json entry has no string `{key}`: {entry}"))
            .to_string()
    };

    let mut ids: Vec<String> = entries.iter().map(|e| str_field(e, "id")).collect();
    ids.sort();
    let stems = wiki_page_stems(md_files_in("wiki/pages"));
    for stem in &stems {
        assert!(
            ids.contains(stem),
            "wiki/pages/{stem}.md has no wiki/manifest.json entry: add one"
        );
    }
    assert_eq!(
        ids, stems,
        "wiki/manifest.json ids must be exactly the wiki/pages/*.md file stems, once each"
    );

    for entry in entries {
        let id = str_field(entry, "id");
        let path = str_field(entry, "path");
        assert_eq!(
            path,
            format!("pages/{id}.md"),
            "wiki/manifest.json entry `{id}` names the wrong path"
        );
        let rel = format!("wiki/{path}");
        assert!(
            repo_root().join(&rel).is_file(),
            "wiki/manifest.json entry `{id}` names {rel}, which does not exist"
        );

        let page = doc(&rel);
        let want = format!(
            "sha256:{}",
            Sha256::digest(wiki_page_body(&page).as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(
            str_field(entry, "hash"),
            want,
            "wiki/manifest.json's hash for `{id}` does not match {rel}: the page changed \
             without the manifest; regenerate its entry"
        );

        let mirrored = wiki_mirrored_frontmatter(&rel, &page);
        for (key, want) in mirrored.as_object().expect("mirrored fields are an object") {
            assert_eq!(
                &entry[key.as_str()],
                want,
                "wiki/manifest.json's `{key}` for `{id}` disagrees with {rel}'s frontmatter: \
                 the page changed without the manifest; regenerate its entry"
            );
        }
        // A public repository's pages are all public, whatever a page asks for.
        assert_eq!(
            entry["visibility"], manifest["visibility"],
            "wiki/manifest.json entry `{id}` must carry the repository's visibility"
        );

        let (commit, date) = wiki_verified_stamp(&rel);
        assert_eq!(
            (
                str_field(entry, "verified_commit"),
                str_field(entry, "verified_date")
            ),
            (commit, date),
            "wiki/manifest.json's verified_commit/verified_date for `{id}` disagree with \
             {rel}'s `verified` frontmatter"
        );
    }
}

/// `git` run in the repository root, or `None` when git is missing or the
/// command fails.
fn git_in_repo(args: &[&str]) -> Option<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo_root())
        .output()
        .ok()
}

/// `wiki/manifest.json`'s `head_commit` names the snapshot the index was
/// generated from; each page was verified at its own `verified_commit`,
/// which must therefore be in `head_commit`'s history. A merge kept main's own `de525d1` stamp on windows-build-setup
/// while `head_commit` stayed at `d286147`, which does not contain it.
/// Ordering commits needs git history: the test skips in a shallow clone
/// (ci-full.yml's test checkout is one) and skips any commit this clone
/// cannot resolve.
#[test]
fn wiki_manifest_head_commit_contains_every_page_stamp() {
    let shallow = git_in_repo(&["rev-parse", "--is-shallow-repository"]);
    match shallow {
        Some(out) if out.status.success() && out.stdout.starts_with(b"false") => {}
        _ => {
            eprintln!("skipping: needs a full (non-shallow) git clone");
            return;
        }
    }
    let resolves = |commit: &str| {
        git_in_repo(&["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .is_some_and(|out| out.status.success())
    };

    let manifest: serde_json::Value =
        serde_json::from_str(&doc("wiki/manifest.json")).expect("parse wiki/manifest.json");
    let head = manifest["head_commit"]
        .as_str()
        .expect("wiki/manifest.json has a string `head_commit`");
    if !resolves(head) {
        eprintln!("skipping: head_commit {head} is not in this clone");
        return;
    }
    for entry in manifest["pages"]
        .as_array()
        .expect("wiki/manifest.json has a `pages` array")
    {
        let id = entry["id"].as_str().unwrap_or("?");
        let stamp = entry["verified_commit"]
            .as_str()
            .unwrap_or_else(|| panic!("wiki/manifest.json entry `{id}` has no verified_commit"));
        if !resolves(stamp) {
            eprintln!("skipping `{id}`: verified_commit {stamp} is not in this clone");
            continue;
        }
        let contained = git_in_repo(&["merge-base", "--is-ancestor", stamp, head])
            .is_some_and(|out| out.status.success());
        assert!(
            contained,
            "wiki/manifest.json's head_commit {head} does not contain verified_commit {stamp} \
             of entry `{id}`: re-anchor head_commit on a commit that contains every page stamp"
        );
    }
}

/// MAN-152 re-stamped spot-output-contract.md as verified, but its opening
/// still said manta "consumes no external spot contract". MAN-301's
/// `[rbn_compare]` parses RBN's `DX de` feed, so the page names that parser.
#[test]
fn spot_output_contract_names_the_rbn_feed_manta_reads() {
    let page = squash_whitespace(&doc("wiki/pages/spot-output-contract.md"));
    assert!(
        !page.contains("consumes no external spot contract"),
        "wiki/pages/spot-output-contract.md says manta consumes no external spot \
         contract, but `[rbn_compare]` parses RBN's `DX de` feed (MAN-301)"
    );
    let parser = "crates/manta-server/src/rbn_compare/ak1a.rs";
    assert!(
        repo_root().join(parser).is_file(),
        "{parser} is gone: point wiki/pages/spot-output-contract.md at the RBN feed parser"
    );
    assert!(
        page.contains(parser),
        "wiki/pages/spot-output-contract.md must name {parser}, the RBN feed parser"
    );
}

// ---- MAN-261: SPEC §9 is the loadable key table; README's node is one file ----

fn manta() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
}

/// A 15 s V1 render (fast, still one W1AW CQ), written into `dir`; returns
/// the WAV path. Same shortened spec `tests/cli.rs` uses.
fn short_v1_wav(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 15.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

/// The body of the first ```toml fenced block in `md`.
fn toml_block(md: &str) -> String {
    let body: Vec<&str> = md
        .lines()
        .skip_while(|l| l.trim() != "```toml")
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("```"))
        .collect();
    assert!(!body.is_empty(), "no ```toml fenced block found");
    body.join("\n") + "\n"
}

/// SPEC §9's key block, verbatim.
fn spec_section_9_toml() -> String {
    let spec = doc("docs/SPEC-decode-core.md");
    toml_block(section(&spec, "9. Configuration keys"))
}

/// The lines of SPEC §9's `[<table>]`: from its header up to the next
/// table header or the end of the fenced block.
fn spec_table_lines(table: &str) -> Vec<String> {
    let header = format!("[{table}]");
    spec_section_9_toml()
        .lines()
        .skip_while(|l| l.trim() != header)
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .map(str::to_string)
        .collect()
}

/// `key = …` as a live line or a commented-out `# key = …` line.
fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    let t = t.strip_prefix('#').map(str::trim_start).unwrap_or(t);
    t.strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

const SERVER_TABLE: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                            telnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

/// MAN-34 left `center_freq_hz` CLI-only: `DaemonConfigFile` did not model
/// `[input]`, so a daemon TOML setting it still failed the `--dial-freq-hz
/// is required with --config` guard. MAN-261 made `[input]` live -- an
/// untyped `[input]` (shared keys only) survives a CLI `--source`, so the
/// file's `center_freq_hz` now satisfies the guard and the run gets as far
/// as opening the (missing) WAV. The SPEC comment must say so, not still
/// call the key CLI-only.
#[test]
fn spec_input_center_freq_hz_is_a_live_config_key() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("daemon.toml");
    fs::write(
        &cfg,
        format!("{SERVER_TABLE}\n[input]\ncenter_freq_hz = 14030000.0\n"),
    )
    .unwrap();
    let out = manta()
        .args(["run", "--source", "/nonexistent.wav", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "`run --source /nonexistent.wav` succeeded? stderr: {stderr}"
    );
    assert!(
        !stderr.contains("--dial-freq-hz is required"),
        "[input] center_freq_hz in the config file does not satisfy the dial guard: \
         {stderr}"
    );
    assert!(
        stderr.contains("Failed to open WAV"),
        "expected the run to get past the dial guard and fail opening the WAV: {stderr}"
    );

    let input = spec_table_lines("input");
    assert!(!input.is_empty(), "SPEC §9 has no `[input]` table to check");
    let key = input
        .iter()
        .position(|l| is_key_line(l, "center_freq_hz"))
        .expect("SPEC §9 [input] no longer documents center_freq_hz");
    let mut comment: Vec<&str> = input[..key]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with('#'))
        .map(String::as_str)
        .collect();
    comment.reverse();
    comment.push(&input[key]);
    let comment = comment.join(" ");
    for stale in ["CLI-only", "has no effect"] {
        assert!(
            !comment.contains(stale),
            "SPEC §9 [input] center_freq_hz comment still says `{stale}`: {comment}"
        );
    }
    assert!(
        comment.contains("--dial-freq-hz"),
        "SPEC §9 [input] center_freq_hz comment never names its `--dial-freq-hz` \
         override: {comment}"
    );
}

/// SPEC §9's block used to put two `key = value` pairs on one line, so it
/// could not be copied into a config file at all. It must now be a file
/// `manta` loads -- and since it states the defaults, loading it must change
/// nothing: `decode --json` output is byte-identical with and without it.
#[test]
fn spec_section_9_block_is_valid_toml_that_manta_loads() {
    let block = spec_section_9_toml();
    if let Err(e) = toml::from_str::<toml::Table>(&block) {
        panic!("SPEC §9's block is not valid TOML: {e}\n{block}");
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = short_v1_wav(dir.path());
    let cfg = dir.path().join("spec9.toml");
    fs::write(&cfg, &block).unwrap();

    let with = manta()
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        with.status.success(),
        "`manta decode --config <SPEC §9 block>` failed: {}",
        String::from_utf8_lossy(&with.stderr)
    );
    let without = manta()
        .args(["decode", "--json"])
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        without.status.success(),
        "baseline `manta decode --json` failed: {}",
        String::from_utf8_lossy(&without.stderr)
    );
    assert!(
        !without.stdout.is_empty(),
        "baseline decode printed nothing"
    );
    assert!(
        with.stdout == without.stdout,
        "SPEC §9's block changes decode output, so its values are not the defaults\n\
         with:    {}\nwithout: {}",
        String::from_utf8_lossy(&with.stdout),
        String::from_utf8_lossy(&without.stdout)
    );
}

/// The SPEC §9 table as a parsed TOML document.
fn spec_section_9_doc() -> toml::Table {
    toml::from_str(&spec_section_9_toml()).expect("SPEC §9 block parses as TOML")
}

/// The values SPEC §9 states for `[detector]` must be the code's defaults.
/// SPEC v1 said `on_snr_db = 6.0` for months after the code moved to 12.0
/// (docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md item 2).
#[test]
fn spec_detector_values_equal_the_code_defaults() {
    let doc = spec_section_9_doc();
    let detector = doc
        .get("detector")
        .expect("SPEC §9 block has no [detector] table")
        .clone();
    let table = detector.as_table().expect("[detector] is a table");
    for key in [
        "on_snr_db",
        "off_snr_db",
        "confirm_ms",
        "hang_ms",
        "gc_ms",
        "warmup_ms",
        "track_cap",
        "silent_respawn_cooldown_ms",
        "crash_filter",
        "rtty_filter",
        "rtty_dwell_ms",
        "non_cw_filter",
        "unkeyed_close_ms",
    ] {
        assert!(
            table.contains_key(key),
            "SPEC §9 [detector] does not state `{key}`, so its default goes unchecked"
        );
    }
    let parsed: manta_engine::config_file::DetectorConfigToml = detector
        .try_into()
        .unwrap_or_else(|e| panic!("SPEC §9 [detector] does not parse: {e}"));
    let cfg = parsed
        .into_detector_config()
        .unwrap_or_else(|e| panic!("SPEC §9 [detector] is rejected: {e}"));
    assert_eq!(
        cfg,
        manta_engine::DetectorConfig::default(),
        "SPEC §9 [detector] values differ from DetectorConfig::default()"
    );
}

/// Same for `[decode]`. `DecodeConfig` has no `PartialEq`, so compare the
/// `Debug` rendering.
#[test]
fn spec_decode_values_equal_the_code_defaults() {
    let doc = spec_section_9_doc();
    let decode = doc
        .get("decode")
        .expect("SPEC §9 block has no [decode] table")
        .clone();
    let table = decode.as_table().expect("[decode] is a table");
    for key in [
        "engine",
        "timing_sigma",
        "beam_width",
        "debounce_ms",
        "hyst_frac",
        "tau_lo_ms",
        "tau_hi_bounds_ms",
        "flush_gap_dits",
    ] {
        assert!(
            table.contains_key(key),
            "SPEC §9 [decode] does not state `{key}`, so its default goes unchecked"
        );
    }
    let parsed: manta_decode::config_file::DecodeConfigToml = decode
        .try_into()
        .unwrap_or_else(|e| panic!("SPEC §9 [decode] does not parse: {e}"));
    assert_eq!(
        format!("{:?}", parsed.into_decode_config()),
        format!("{:?}", manta_decode::decoder::DecodeConfig::default()),
        "SPEC §9 [decode] values differ from DecodeConfig::default()"
    );
}

/// Every `[input]` and `[spot]` key the loader accepts has a line (live or
/// commented out) in SPEC §9, so the SPEC is the one place to look a key up.
#[test]
fn spec_documents_every_input_and_spot_key() {
    let tables: [(&str, &[&str]); 2] = [
        (
            "input",
            &[
                "type",
                "device",
                "path",
                "iq",
                "realtime",
                "loop",
                "host",
                "port",
                "freq_hz",
                "password",
                "driver",
                "rate_hz",
                "gain_db",
                "group",
                "ssrc",
                "interface",
                "freq_correction_ppm",
                "center_freq_hz",
                "capture_rate_hz",
                "replay_epoch",
            ],
        ),
        (
            "spot",
            &[
                "allowlist",
                "blocklist_path",
                "notch_path",
                "cty_path",
                "scp_path",
                "refresh_lists",
                "list_cache_dir",
                "cty_url",
                "scp_url",
                "cty_sha256",
                "scp_sha256",
                "pattern_path",
                "validation_level",
            ],
        ),
    ];
    for (table, keys) in tables {
        let lines = spec_table_lines(table);
        assert!(!lines.is_empty(), "SPEC §9 has no `[{table}]` table");
        for key in keys {
            assert!(
                lines.iter().any(|l| is_key_line(l, key)),
                "SPEC §9 [{table}] has no `{key} = …` line (live or commented out)"
            );
        }
    }
}

/// SPEC §9 lists eight keys the code still hard-codes. A live line for any
/// of them would be a config file the loader rejects, so each may appear
/// only commented out, marked as not configurable yet.
#[test]
fn spec_constant_only_keys_are_commented_out_as_not_configurable() {
    let block = spec_section_9_toml();
    for key in [
        "floor_quantile",
        "floor_window_ms",
        "block_channels",
        "block_allowance_db",
        "mu_ratio_bounds",
        "char_gap_dits",
        "word_gap_dits",
        "cluster_alpha",
    ] {
        let lines: Vec<&str> = block.lines().filter(|l| l.contains(key)).collect();
        assert!(!lines.is_empty(), "SPEC §9 no longer lists `{key}`");
        for line in lines {
            assert!(
                line.trim_start().starts_with('#') && line.contains("not configurable yet"),
                "SPEC §9 mentions constant-only `{key}` outside a `# … not configurable \
                 yet` line: {line}"
            );
        }
    }
}

/// The node section must be one config file an operator can copy: the
/// receiver lives in `[input]` next to `[server]`, the command carries no
/// source flags, and the file actually loads. The Kiwi host is a marked
/// placeholder, and since a bare `<…>` is not a TOML integer, the port's
/// placeholder marker lives in its comment (the intent of
/// `readme_kiwi_commands_use_a_marked_placeholder_host` and
/// `readme_kiwi_commands_offer_a_marked_placeholder_port`, carried to TOML).
#[test]
fn readme_node_section_is_a_single_config_file() {
    let readme = doc("README.md");
    let node = section(&readme, "Run it as a node");
    let text = toml_block(node);
    let table: toml::Table = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("README node config is not valid TOML: {e}\n{text}"));
    for t in ["server", "input"] {
        assert!(
            table.contains_key(t),
            "README node config has no [{t}] table"
        );
    }
    let host = table["input"]
        .get("host")
        .and_then(toml::Value::as_str)
        .expect("README node config's [input] has no string `host`");
    assert!(
        host.starts_with('<') && host.ends_with('>'),
        "README node config uses host `{host}`; it needs a `<placeholder>` the reader \
         must replace"
    );
    let port_line = text
        .lines()
        .find(|l| is_key_line(l, "port"))
        .expect("README node config's [input] has no `port` line");
    assert!(
        port_line.contains("<your-kiwi-port>"),
        "README node config's port line carries no `<your-kiwi-port>` marker: {port_line}"
    );

    let runs: Vec<String> = fenced_commands(node)
        .into_iter()
        .filter(|c| c.contains("manta run"))
        .collect();
    assert!(
        !runs.is_empty(),
        "README node section has no `manta run` command"
    );
    for cmd in &runs {
        assert!(
            cmd.contains("--config"),
            "README node command does not use the config file: {cmd}"
        );
        for flag in ["--kiwi-host", "--source", "--device"] {
            assert!(
                !cmd.split_whitespace().any(|t| t.starts_with(flag)),
                "README node command still selects the source with `{flag}`: {cmd}"
            );
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let wav = short_v1_wav(dir.path());
    let cfg = dir.path().join("manta.toml");
    fs::write(&cfg, &text).unwrap();
    let out = manta()
        .args(["decode", "--json", "--config"])
        .arg(&cfg)
        .arg(&wav)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "README node config does not load (`manta decode --config`): {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The metrics endpoint (and every server) starts when the resolved config
/// has a `[server]` table -- from the file or `MANTA_SERVER_*` -- not
/// whenever `--config` is passed: a config without `[server]` runs with no
/// servers.
#[test]
fn architecture_section_8_does_not_tie_servers_to_the_flag() {
    let arch = doc("ARCHITECTURE.md");
    let s8 = squash_whitespace(section(&arch, "8. Configuration"));
    assert!(
        !s8.contains("whenever `--config` is set"),
        "ARCHITECTURE §8 still says the servers run whenever `--config` is set"
    );
    assert!(
        s8.contains("whenever the resolved config has a `[server]` table"),
        "ARCHITECTURE §8 no longer says when the servers run"
    );
}

/// MAN-76: the node section walks an operator through scaffolding and
/// checking the config file before `manta run`, as copyable commands.
#[test]
fn readme_node_section_mentions_config_init_and_check() {
    let readme = doc("README.md");
    let commands = fenced_commands(section(&readme, "Run it as a node"));
    for wanted in ["manta config init", "manta config check"] {
        assert!(
            commands.iter().any(|c| c.trim_start().starts_with(wanted)),
            "README node section has no `{wanted}` command: {commands:?}"
        );
    }
}

/// MAN-65 finding 2: CI cannot prove `manta.exe` starts on a machine without
/// the Visual C++ Redistributable, so the release-pipeline decision doc hands
/// that check to the release runbook by section name. The section must exist
/// and hold the actual procedure, not just a heading.
#[test]
fn release_runbook_has_the_clean_windows_check_the_decision_doc_cites() {
    const HEADING: &str =
        "Manual check: the Windows ZIP starts without the Visual C++ Redistributable";
    let decision = squash_whitespace(&doc(
        "docs/DECISIONS/2026-09-05-man65-release-pipeline-hardening.md",
    ));
    assert!(
        decision.contains(&format!("`docs/RUNBOOKS/release.md`, \"{HEADING}\"")),
        "the MAN-65 decision doc no longer cites the runbook's clean-Windows check by name"
    );
    let runbook = doc("docs/RUNBOOKS/release.md");
    let check = section(&runbook, HEADING);
    for needle in ["manta.exe --help", "vcruntime140.dll"] {
        assert!(
            check.contains(needle),
            "the runbook's clean-Windows check never says `{needle}`"
        );
    }
}

/// MAN-76: ARCHITECTURE §8's config bullet names the command that validates
/// a config without starting the daemon.
#[test]
fn architecture_section_8_mentions_config_check() {
    let arch = doc("ARCHITECTURE.md");
    let s8 = squash_whitespace(section(&arch, "8. Configuration"));
    assert!(
        s8.contains("`manta config check`"),
        "ARCHITECTURE §8 does not mention `manta config check`"
    );
    assert!(
        s8.contains("`manta config init`"),
        "ARCHITECTURE §8 does not mention `manta config init`"
    );
}

/// MAN-132: metrics has its own bind address (default 127.0.0.1). No
/// operator-facing doc may still say it shares `bind_addr` or that no
/// per-listener option exists.
#[test]
fn docs_describe_the_separate_metrics_bind_addr() {
    for rel in [
        "README.md",
        "ARCHITECTURE.md",
        "docs/RUNBOOKS/network-exposure.md",
        "docs/RUNBOOKS/node-health.md",
    ] {
        let text = squash_whitespace(&doc(rel));
        assert!(
            text.contains("metrics_bind_addr"),
            "{rel} never mentions metrics_bind_addr"
        );
        for stale in [
            "A per-listener bind option doesn't exist yet",
            "omit it and all three servers bind",
            "shares the same publicly-bound-by-default posture",
            "default bind `[server].bind_addr`",
        ] {
            assert!(!text.contains(stale), "{rel} still says {stale:?}");
        }
    }
}

/// MAN-116: the README and ARCHITECTURE name the sensitivity benchmark so a
/// reader can find how a sensitivity claim was produced.
#[test]
fn docs_name_the_sensitivity_benchmark() {
    assert!(doc("README.md").contains("manta bench sensitivity"));
    assert!(doc("ARCHITECTURE.md").contains("manta bench sensitivity"));
}

/// MAN-83: where `decoderVersion`'s format and the decoder-output versioning
/// rule are decided.
const MAN83_DECISION: &str =
    "docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md";

/// MAN-83: CHANGELOG.md is Keep a Changelog-shaped (Unreleased first) and
/// states the decoder-output rule downstream benchmark comparisons rely on,
/// linking the decision doc that makes it normative.
#[test]
fn changelog_opens_with_unreleased_and_states_the_decoder_output_rule() {
    let changelog = doc("CHANGELOG.md");
    assert_eq!(
        changelog.lines().find(|l| l.starts_with("## ")),
        Some("## [Unreleased]"),
        "CHANGELOG.md's first `## ` heading must be `## [Unreleased]`"
    );
    let text = squash_whitespace(&changelog);
    for needle in [
        "Keep a Changelog",
        "Semantic Versioning",
        "### Decoder output",
        "MINOR",
        "PATCH",
        MAN83_DECISION,
    ] {
        assert!(text.contains(needle), "CHANGELOG.md never says {needle:?}");
    }
    assert!(
        repo_root().join(MAN83_DECISION).is_file(),
        "CHANGELOG.md links {MAN83_DECISION}, which does not exist"
    );
}

/// MAN-83: the rule is only a promise if cutting a release applies it.
#[test]
fn release_runbook_applies_the_decoder_output_rule() {
    let runbook = doc("docs/RUNBOOKS/release.md");
    let cutting = squash_whitespace(section(&runbook, "Cutting a release"));
    for needle in ["CHANGELOG.md", "### Decoder output", "MINOR"] {
        assert!(
            cutting.contains(needle),
            "release runbook's \"Cutting a release\" never says {needle:?}"
        );
    }
}

/// MAN-83: ARCHITECTURE §7 names `decoderVersion`'s build-identity form and
/// points at its decision doc.
#[test]
fn architecture_section_7_describes_decoder_version() {
    let arch = doc("ARCHITECTURE.md");
    let s7 = squash_whitespace(section(&arch, "7. Output layer"));
    for needle in ["decoderVersion", "manta-<version>+<commit>", MAN83_DECISION] {
        assert!(s7.contains(needle), "ARCHITECTURE §7 never says {needle:?}");
    }
}

/// MAN-127: the README's node section tells an operator how to measure
/// `freq_correction_ppm`, and ARCHITECTURE §8 says which command edits the
/// config file in place.
#[test]
fn readme_and_architecture_mention_calibrate() {
    let readme = doc("README.md");
    let commands = fenced_commands(section(&readme, "Run it as a node"));
    assert!(
        commands
            .iter()
            .any(|c| c.trim_start().starts_with("manta calibrate")),
        "README node section has no `manta calibrate` command: {commands:?}"
    );
    let arch = doc("ARCHITECTURE.md");
    let s8 = squash_whitespace(section(&arch, "8. Configuration"));
    for needle in [
        "`manta calibrate`",
        "input.freq_correction_ppm",
        "docs/DECISIONS/2026-10-11-man127-calibrate-command.md",
    ] {
        assert!(s8.contains(needle), "ARCHITECTURE §8 never says {needle:?}");
    }
    assert!(
        repo_root()
            .join("docs/DECISIONS/2026-10-11-man127-calibrate-command.md")
            .is_file(),
        "ARCHITECTURE §8 links the MAN-127 decision doc, which does not exist"
    );
}

// ---- MAN-124: the log's level and format controls, documented and real ----

/// MAN-124: README's `## Logs` documents `RUST_LOG` as the underlying
/// mechanism, the four flags and `NO_COLOR`.
#[test]
fn readme_logs_section_documents_rust_log_and_the_flags() {
    let readme = doc("README.md");
    let logs = squash_whitespace(section(&readme, "Logs"));
    for needle in [
        "RUST_LOG",
        "-v",
        "-q",
        "--log-level",
        "--log-format json",
        "NO_COLOR",
    ] {
        assert!(
            logs.contains(needle),
            "README `## Logs` never says {needle:?}"
        );
    }
}

/// MAN-124: the flags README's `## Logs` names exist on `manta run`.
#[test]
fn readme_logs_flags_exist_in_run_help() {
    let out = manta().args(["run", "--help"]).output().unwrap();
    assert!(out.status.success(), "manta run --help: {:?}", out.status);
    let help = String::from_utf8(out.stdout).unwrap();
    for flag in ["--verbose", "--quiet", "--log-level", "--log-format"] {
        assert!(help.contains(flag), "`manta run --help` has no {flag}");
    }
}

/// MAN-124: the packaging guide says how to change the log level and format
/// under each service manager.
#[test]
fn packaging_readme_says_how_to_change_log_level_and_format() {
    let guide = doc("packaging/README.md");
    let systemd = squash_whitespace(section(&guide, "Linux: systemd"));
    for needle in ["--log-format json", "RUST_LOG"] {
        assert!(
            systemd.contains(needle),
            "systemd section never says {needle:?}"
        );
    }
    let launchd = squash_whitespace(section(&guide, "macOS: LaunchDaemon"));
    assert!(
        launchd.contains("--log-format json"),
        "launchd section never says --log-format json"
    );
    let docker = squash_whitespace(section(&guide, "Docker Compose"));
    assert!(
        docker.contains("--log-format"),
        "Docker section never says --log-format"
    );
}

/// MAN-124: ARCHITECTURE.md no longer calls the log plain `fmt` output.
#[test]
fn architecture_logging_bullet_is_current() {
    let arch = squash_whitespace(&doc("ARCHITECTURE.md"));
    assert!(
        !arch.contains("plain `fmt` output"),
        "ARCHITECTURE.md still says plain `fmt` output"
    );
    assert!(
        arch.contains("--log-format json"),
        "ARCHITECTURE.md never says --log-format json"
    );
}

// ---- MAN-301: live RBN comparison, documented ----

/// Every `manta_rbn_compare_*` family the design names (MAN-301 plan §7).
/// The runbook test below also checks the rendered families against this
/// list, so a stubbed or partial renderer cannot pass it vacuously.
const RBN_COMPARE_FAMILIES: [&str; 10] = [
    "manta_rbn_compare_grades_total",
    "manta_rbn_compare_grades_last_hour",
    "manta_rbn_compare_disagreements_last_hour",
    "manta_rbn_compare_freq_offset_median_hz",
    "manta_rbn_compare_freq_offset_samples",
    "manta_rbn_compare_feed_connected",
    "manta_rbn_compare_feed_reports_total",
    "manta_rbn_compare_feed_reconnects_total",
    "manta_rbn_compare_pending",
    "manta_rbn_compare_ungraded_total",
];

/// One grade of `grade` for `call` on 20m, with same-band statistics when
/// the grade has them.
fn rbn_grade(
    grade: manta_server::rbn_compare::grade::Grade,
    disagreements: Vec<manta_server::rbn_compare::grade::Disagreement>,
) -> manta_server::rbn_compare::grade::SpotGrade {
    use manta_server::rbn_compare::grade::{Grade, SpotGrade};
    let same_band = matches!(grade, Grade::Match | Grade::Disagree);
    SpotGrade {
        kind: "spotGrade",
        seq: None,
        spot_id: format!("W3XYZ-1:1:1:960000:W1AW-{grade:?}"),
        call: "W1AW".to_string(),
        band: "20m",
        spot_time: 1_791_720_000,
        window_close: 1_791_720_180,
        window_s: 180,
        grade,
        disagreements,
        manta_freq_hz: 14_025_100,
        manta_wpm: Some(25),
        rbn_modal_freq_hz: same_band.then_some(14_025_000),
        rbn_median_wpm: same_band.then_some(24.0),
        rbn_skimmers: if same_band { 3 } else { 0 },
        rbn_reports: if same_band { 3 } else { 0 },
        freq_offset_hz: same_band.then_some(100),
        rbn_bands: match grade {
            Grade::Match | Grade::Disagree => vec!["20m"],
            Grade::Elsewhere => vec!["40m"],
            Grade::None | Grade::Unknown => vec![],
        },
        spotter_filter: vec![],
    }
}

/// MAN-301: `docs/RUNBOOKS/rbn-comparison.md` explains every
/// `manta_rbn_compare_*` family `/metrics` renders once comparison is on.
/// The family list comes from the real renderer, fed one grade of each
/// kind plus the feed and pending setters, so a family added later without
/// a runbook entry fails here.
#[test]
fn rbn_comparison_runbook_names_every_rbn_compare_metric() {
    use manta_server::rbn_compare::grade::{Disagreement, Grade};
    let metrics = manta_server::metrics::Metrics::new();
    metrics.enable_rbn_compare();
    for (grade, disagreements) in [
        (Grade::Match, vec![]),
        (
            Grade::Disagree,
            vec![Disagreement::Frequency, Disagreement::Speed],
        ),
        (Grade::Elsewhere, vec![]),
        (Grade::None, vec![]),
        (Grade::Unknown, vec![]),
    ] {
        metrics.record_rbn_grade(&rbn_grade(grade, disagreements));
    }
    metrics.set_rbn_feed_connected(true);
    metrics.inc_rbn_feed_reports();
    metrics.inc_rbn_feed_reconnects();
    metrics.set_rbn_pending(1);
    metrics.add_rbn_ungraded(1);
    let text = metrics.render_prometheus_text();

    let mut rendered: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| name.starts_with("manta_rbn_compare_"))
        .collect();
    rendered.dedup();
    assert!(
        !rendered.is_empty(),
        "an enabled Metrics renders no manta_rbn_compare_* family"
    );
    for family in RBN_COMPARE_FAMILIES {
        assert!(
            rendered.contains(&family),
            "an enabled Metrics with one grade of each kind does not render {family}"
        );
    }

    let runbook = doc("docs/RUNBOOKS/rbn-comparison.md");
    for family in &rendered {
        assert!(
            runbook.contains(family),
            "docs/RUNBOOKS/rbn-comparison.md never names {family}"
        );
    }
}

/// MAN-301: README documents the `[rbn_compare]` table, and ARCHITECTURE §7
/// says the JSON stream can carry `spotGrade` records.
#[test]
fn readme_and_architecture_mention_rbn_compare() {
    let readme = doc("README.md");
    assert!(
        readme.contains("[rbn_compare]"),
        "README.md never mentions the [rbn_compare] table"
    );
    let arch = doc("ARCHITECTURE.md");
    let s7 = section(&arch, "7. Output layer");
    assert!(
        s7.contains("spotGrade"),
        "ARCHITECTURE §7 never mentions spotGrade records"
    );
}

/// MAN-301: SPEC §9's precedence paragraph lists the `RBN_COMPARE`
/// environment prefix and `[rbn_compare]`'s two string-typed keys.
#[test]
fn spec_precedence_paragraph_lists_rbn_compare_env_prefix() {
    let spec = doc("docs/SPEC-decode-core.md");
    let start = spec
        .find("**Precedence and the environment.**")
        .expect("SPEC §9 has no precedence paragraph");
    let rest = &spec[start..];
    let paragraph = squash_whitespace(&rest[..rest.find("\n\n").unwrap_or(rest.len())]);
    for needle in [
        "RBN_COMPARE",
        "rbn_compare.feed_host",
        "rbn_compare.login_callsign",
    ] {
        assert!(
            paragraph.contains(needle),
            "SPEC §9's precedence paragraph never says {needle:?}"
        );
    }
}

// ---- MAN-90 ----

const MAN90_DECISION: &str = "docs/DECISIONS/2026-10-11-man90-uplink-protocol-verification.md";
const MAN90_RUNBOOK: &str = "docs/RUNBOOKS/aggregator-rbn-capture.md";
/// Same literal as uplink_acceptance.rs's MAN90_REFERENCE_SPOT (minus CRLF):
/// the decision record must quote the bytes the uplink really writes.
const MAN90_REFERENCE_SPOT: &str =
    "DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      2213Z";

/// MAN-90: the decision record lists every open claim, quotes the uplink's
/// real bytes, and it and the capture runbook link each other.
#[test]
fn man90_records_list_every_open_claim_and_link_each_other() {
    let decision = doc(MAN90_DECISION);
    let runbook = doc(MAN90_RUNBOOK);
    for id in ["C1", "C2", "C3", "C4", "C5", "C6", "C7"] {
        assert!(
            decision.contains(&format!("| {id} |")),
            "decision record has no {id} row"
        );
        assert!(runbook.contains(id), "capture runbook never mentions {id}");
    }
    assert!(
        decision.contains(MAN90_REFERENCE_SPOT),
        "decision record misquotes the uplink's bytes"
    );
    assert!(decision.contains("aggregator-rbn-capture.md"));
    assert!(runbook.contains("2026-10-11-man90-uplink-protocol-verification.md"));
    assert!(
        runbook.contains("\n## Captures"),
        "runbook has no Captures log"
    );
}

/// MAN-90: every relative link in the two records resolves.
#[test]
fn man90_records_relative_links_resolve() {
    // Same link grammar as operator_guides.rs::guide_relative_links_resolve.
    let link = regex::Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    for rel in [MAN90_DECISION, MAN90_RUNBOOK] {
        let base = repo_root().join(rel).parent().unwrap().to_path_buf();
        let mut checked = 0;
        for cap in link.captures_iter(&doc(rel)) {
            let target = &cap[1];
            if target.starts_with("http")
                || target.starts_with('#')
                || target.starts_with("mailto:")
            {
                continue;
            }
            let path = target.split('#').next().unwrap();
            assert!(base.join(path).exists(), "{rel}: broken link {target}");
            checked += 1;
        }
        assert!(checked > 0, "{rel}: no relative links checked");
    }
}

/// MAN-90: the capture starts before Aggregator restarts, so the runbook's
/// Developer steps must list stream indices from SYN packets and check and
/// follow a chosen stream across the whole file. They must not cap packets
/// (`-c` counts every packet read) or follow stream 0, which can be the old
/// connection. Checks text in that section, not shell semantics.
#[test]
fn man90_runbook_checks_the_restarted_connection() {
    let runbook = doc(MAN90_RUNBOOK);
    let steps = section(&runbook, "Developer steps");
    for needle in [
        "-Y 'tcp.flags.syn == 1 && tcp.flags.ack == 0'",
        "-e tcp.stream",
        "-Y \"tcp.stream == $N && (tls || tcp.payload[0:2] == 16:03)\"",
        "-z follow,tcp,ascii,$N",
        "-e udp.stream",
    ] {
        assert!(
            steps.contains(needle),
            "Developer steps never run `{needle}`"
        );
    }
    for stale in [" -c ", "follow,tcp,ascii,0"] {
        assert!(
            !steps.contains(stale),
            "Developer steps still run `{stale}`"
        );
    }
}

/// MAN-90: after its restart Aggregator may reach RBN at a different address,
/// a second endpoint or over UDP. A `host <address from step 1>` capture
/// filter drops all of that, yet the file settles C1 and C2. The operator
/// steps must capture without that filter, list Aggregator's addresses again
/// after the restart, and send only Aggregator's packets. The developer
/// steps must use every listed address. Checks text, not Wireshark behaviour.
#[test]
fn man90_runbook_keeps_every_endpoint_aggregator_uses() {
    let runbook = doc(MAN90_RUNBOOK);
    let operator = squash_whitespace(section(&runbook, "Operator steps"));
    for stale in ["host <address from step 1>", "capture filter `"] {
        assert!(
            !operator.contains(stale),
            "Operator steps still set a capture filter (`{stale}`)"
        );
    }
    for needle in [
        "no capture filter",
        "after the restart",
        "Export Specified Packets",
    ] {
        assert!(
            operator.contains(needle),
            "Operator steps never say `{needle}`"
        );
    }
    let steps = squash_whitespace(section(&runbook, "Developer steps"));
    assert!(
        steps.contains("every address the operator noted"),
        "Developer steps never use every address the operator noted"
    );
}

/// MAN-90: `ip.addr == <address>` keeps every packet to or from that address,
/// whichever program sent it, so a browser open on an RBN page that shares the
/// address would pass step 5 and could be read as Aggregator's connection.
/// Steps 1 and 3 must note each connection's ports, and step 5 must pair every
/// `ip.addr`/`ipv6.addr` term with ports, then check the Conversations list.
/// Checks text, not Wireshark behaviour.
#[test]
fn man90_runbook_exports_only_aggregators_connections() {
    let runbook = doc(MAN90_RUNBOOK);
    let operator = squash_whitespace(section(&runbook, "Operator steps"));
    let step = |start: &str, end: &str| -> String {
        let from = operator
            .find(start)
            .unwrap_or_else(|| panic!("Operator steps have no `{start}` step"));
        let to = operator[from..]
            .find(end)
            .map_or(operator.len(), |i| from + i);
        operator[from..to].to_string()
    };
    let find = step("1. **Find", "2. **");
    let restart = step("3. **Restart", "4. **");
    // Step 5 is the last step, so it runs to the end of the section.
    let export = step("5. **Stop", "6. **");
    for (name, text, needles) in [
        (
            "Step 1",
            &find,
            &[
                "LocalPort, RemoteAddress and RemotePort",
                "Get-NetUDPEndpoint -OwningProcess $p.Id",
            ][..],
        ),
        (
            "Step 3",
            &restart,
            &["LocalPort, RemoteAddress and RemotePort", "TCP Connections"][..],
        ),
        (
            "Step 5",
            &export,
            &[
                "(ip.addr == <RemoteAddress> && tcp.port == <LocalPort> && tcp.port == <RemotePort>)",
                "`udp.port == <LocalPort>`",
                "Statistics → Conversations",
            ][..],
        ),
    ] {
        for needle in needles {
            assert!(text.contains(needle), "{name} never says `{needle}`");
        }
    }
    for field in ["ip.addr == <", "ipv6.addr == <"] {
        for (i, _) in export.match_indices(field) {
            let rest = &export[i + field.len()..];
            let close = rest.find('>').expect("placeholder closes");
            assert!(
                rest[close + 1..].starts_with(" && "),
                "Step 5 exports by address alone: {}",
                &export[i..]
            );
        }
    }
}

/// MAN-90: `Get-NetUDPEndpoint` lists Aggregator's UDP LocalPorts, but its UDP
/// peers come only from Resource Monitor's live view, which can miss one. Step
/// 5 must keep UDP by the restarted Aggregator's ports alone, not by a noted
/// address and port, and the developer steps must follow every UDP flow in
/// the file, not only flows to noted addresses. A port-only term would keep a
/// step 1 port another program takes after the restart, so step 5 must use
/// step 3's ports only and check the saved file. Checks text, not Wireshark
/// behaviour.
#[test]
fn man90_runbook_keeps_udp_to_unnoted_addresses() {
    let runbook = doc(MAN90_RUNBOOK);
    let operator = squash_whitespace(section(&runbook, "Operator steps"));
    // Step 5 is the last step, so it runs to the end of the section.
    let export = &operator[operator
        .find("5. **Stop")
        .expect("Operator steps have no `5. **Stop` step")..];
    for needle in [
        "one per UDP LocalPort from step 3, `udp.port == <LocalPort>`",
        "Open the saved file and check",
    ] {
        assert!(export.contains(needle), "Step 5 never says `{needle}`");
    }
    assert!(
        !export.contains("&& udp.port =="),
        "Step 5 keeps UDP only to an address the operator noted"
    );
    let steps = squash_whitespace(section(&runbook, "Developer steps"));
    assert!(
        steps.contains("including one to an address the operator did not note"),
        "Developer steps never follow UDP flows to addresses the operator did not note"
    );
}

/// MAN-90: steps 1 and 3 note only the connections the operator sees in
/// PowerShell or Resource Monitor, so a TCP connection Aggregator opens and
/// closes between two looks would be left out of step 5's export. Step 2 must
/// turn on Windows' per-program connection log (Security event 5156), step 5
/// must take TCP connections from it as well and restore the audit settings,
/// and the developer steps must check every logged connection is in the file.
/// Checks text, not Windows behaviour.
#[test]
fn man90_runbook_takes_aggregators_connections_from_the_windows_log() {
    let runbook = doc(MAN90_RUNBOOK);
    let operator = squash_whitespace(section(&runbook, "Operator steps"));
    let start = operator
        .find("2. **Start")
        .expect("Operator steps have no `2. **Start` step");
    let record = &operator[start..start + operator[start..].find("3. **").unwrap()];
    for needle in [
        "auditpol /backup /file:",
        "auditpol /set /subcategory:'{0CCE9226-69AE-11D9-BED3-505054503030}' /success:enable",
    ] {
        assert!(record.contains(needle), "Step 2 never runs `{needle}`");
    }
    // Step 5 is the last step, so it runs to the end of the section.
    let export = &operator[operator
        .find("5. **Stop")
        .expect("Operator steps have no `5. **Stop` step")..];
    for needle in [
        "Id=5156",
        "auditpol /restore /file:",
        "one term per TCP connection from step 1, step 3 or the saved list",
    ] {
        assert!(export.contains(needle), "Step 5 never says `{needle}`");
    }
    let steps = squash_whitespace(section(&runbook, "Developer steps"));
    assert!(
        steps.contains(
            "a row of `aggregator-connections.csv`, with no conversation in the file was not recorded"
        ),
        "Developer steps never check the file against the connection log"
    );
}

/// MAN-90: event 5156 logs inbound connections as well as outbound ones, and
/// Microsoft's page for it disagrees with itself on which end of an inbound
/// row is the Source. Step 5 reads SourcePort as Aggregator's LocalPort and
/// DestAddress as the remote peer, which holds only for a connection
/// Aggregator opened, so its query must keep outbound rows (`%%14593`) only
/// (Codex P2 on PR #259). Checks text, not Windows behaviour.
#[test]
fn man90_runbook_takes_only_outbound_connections_from_the_windows_log() {
    let runbook = doc(MAN90_RUNBOOK);
    let filter = section(&runbook, "Operator steps")
        .lines()
        .find(|line| line.contains("Split-Path $_.Application"))
        .expect("Step 5 never filters the log by Aggregator's executable");
    assert!(
        filter.contains("$_.Direction -eq '%%14593'"),
        "Step 5 keeps inbound 5156 rows: `{}`",
        filter.trim()
    );
}

/// MAN-90: a TLS session leaves only ciphertext, so the developer steps must
/// say what a TLS result records for C2–C7 and hand the rest to D-5's rework
/// ticket rather than just "stop" (Codex P2 on PR #259). Checks text only.
#[test]
fn man90_runbook_says_what_a_tls_session_records() {
    let steps = squash_whitespace(section(&doc(MAN90_RUNBOOK), "Developer steps"));
    for needle in [
        "`differs: TLS`",
        "`not observable: TLS`",
        "D-5",
        "session keys",
    ] {
        assert!(
            steps.contains(needle),
            "the runbook's developer steps never say {needle:?} for a TLS session"
        );
    }
    assert!(
        !steps.contains("stop at C2"),
        "the runbook still says only \"stop at C2\" for a TLS session"
    );
}

/// MAN-90: C3, C4 and C7 compare exact bytes. tshark's `ascii` follow mode
/// turns non-printable bytes into dots, so the developer steps must also write
/// a `raw` (hex) transcript of each TCP stream and UDP flow.
#[test]
fn man90_runbook_writes_exact_byte_transcripts() {
    let runbook = doc(MAN90_RUNBOOK);
    let steps = section(&runbook, "Developer steps");
    for needle in ["-z follow,tcp,raw,$N", "-z follow,udp,raw,$N"] {
        assert!(
            steps.contains(needle),
            "Developer steps never run `{needle}`"
        );
    }
}

/// MAN-90: step 5 exports every Aggregator connection outside the LAN and the
/// Skimmers, not only RBN's, so the consent text must say so (Codex P2 on
/// PR #259).
#[test]
fn man90_runbook_consent_covers_every_connection_aggregator_has_open() {
    let runbook = doc(MAN90_RUNBOOK);
    // The request is a blockquote; drop the `>` markers before matching.
    let consent = squash_whitespace(&section(&runbook, "Asking for consent").replace("\n>", "\n"));
    assert!(
        !consent.contains("traffic with RBN's servers")
            && !consent.contains("traffic with the RBN server"),
        "consent text still promises only RBN traffic"
    );
    for needle in [
        "every internet connection Aggregator has open while you record",
        "not only its connection to RBN",
    ] {
        assert!(
            consent.contains(needle),
            "consent text never says `{needle}`"
        );
    }
}

/// MAN-90: README calls the uplink experimental everywhere it offers it, and
/// "Run it as a node" names the Aggregator route before the uplink block.
#[test]
fn readme_marks_the_uplink_experimental_and_leads_with_the_aggregator_route() {
    let readme = doc("README.md");
    let outputs = squash_whitespace(section(&readme, "Outputs"));
    for needle in [
        "**Outbound RBN uplink (experimental)**",
        "**Experimental.**",
        "supported route into RBN",
        MAN90_DECISION,
        MAN90_RUNBOOK,
    ] {
        assert!(
            outputs.contains(needle),
            "README Outputs never says `{needle}`"
        );
    }
    let node = squash_whitespace(section(&readme, "Run it as a node"));
    let aggregator = node
        .find("RBN Aggregator")
        .expect("Run it as a node never names the Aggregator route");
    let uplink = node
        .find("`[[rbn_uplink]]`")
        .expect("Run it as a node never names the uplink block");
    assert!(
        aggregator < uplink,
        "Run it as a node offers the uplink before the Aggregator route"
    );
    assert!(
        node.contains("experimental"),
        "Run it as a node never calls the uplink experimental"
    );
    let status = squash_whitespace(section(&readme, "Status"));
    assert!(
        status.contains("outbound RBN uplink is experimental"),
        "README Status"
    );
}

/// MAN-90: ARCHITECTURE §7 enumerates the uplink as an output surface.
#[test]
fn architecture_output_layer_describes_the_experimental_uplink() {
    let s7 = squash_whitespace(section(&doc("ARCHITECTURE.md"), "7. Output layer"));
    for needle in [
        "**Outbound RBN uplink (experimental)**",
        "`uplink.rs`",
        "Aggregator",
        MAN90_DECISION,
    ] {
        assert!(s7.contains(needle), "ARCHITECTURE §7 never says `{needle}`");
    }
}

/// MAN-90: no uplink example names an RBN server as its target.
#[test]
fn uplink_health_runbook_example_is_not_an_rbn_server() {
    assert!(!doc("docs/RUNBOOKS/uplink-health.md").contains("reversebeacon.net"));
}

/// MAN-81: an operator reading the Docker docs learns that a bare
/// `docker run` runs a mounted config, that the image reports health through
/// `manta health`, and how to turn the check off.
#[test]
fn docker_docs_describe_the_bare_run_and_the_health_check() {
    let norm = |s: &str| squash_whitespace(s).to_lowercase();
    let guide = doc("packaging/README.md");
    let docker = norm(section(&guide, "Docker Compose"));
    for needle in [
        "/etc/manta/manta.toml",
        "manta health",
        "--no-healthcheck",
        "--mount",
    ] {
        assert!(
            docker.contains(needle),
            "packaging/README.md's Docker section never says {needle:?}"
        );
    }
    let runbook = norm(&doc("docs/RUNBOOKS/node-health.md"));
    for needle in ["manta health", "healthcheck"] {
        assert!(
            runbook.contains(needle),
            "docs/RUNBOOKS/node-health.md never says {needle:?}"
        );
    }
    assert!(
        norm(&doc("README.md")).contains("manta health"),
        "README.md never says manta health"
    );
}

/// MAN-81: no command line in packaging/README.md's ```sh blocks ends in a
/// `\` continuation. scripts/tests/test_packaging.py's `sh_blocks` runs
/// `shlex.split` on each of those lines on its own, which fails on an
/// unescaped trailing `\` (`No escaped character`), and that test only runs
/// in CI, not in `cargo test`. Fences, blank lines and comment lines are
/// read the way `sh_blocks` reads them.
#[test]
fn packaging_readme_sh_blocks_have_no_line_continuations() {
    let guide = doc("packaging/README.md");
    let mut inside = false;
    let mut checked = 0;
    for line in guide.lines() {
        if inside && line == "```" {
            inside = false;
        } else if !inside && line == "```sh" {
            inside = true;
        } else if inside {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let trailing = line.len() - line.trim_end_matches('\\').len();
            assert!(
                trailing.is_multiple_of(2),
                "packaging/README.md: a ```sh line continues onto the next: {line}"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no ```sh commands in packaging/README.md");
}

// ---- MAN-149: the reference docs and indexes cover everything they list ----

const CONFIG_REF: &str = "docs/CONFIG.md";
const METRICS_REF: &str = "docs/METRICS.md";
const DOCS_INDEX: &str = "docs/README.md";
const DECISIONS_INDEX: &str = "docs/DECISIONS/README.md";

/// MAN-149 M1: config::load creates [server] from MANTA_SERVER_* too.
#[test]
fn config_reference_documents_environment_server_activation() {
    let md = doc(CONFIG_REF);
    let commands = squash_whitespace(section(&md, "Which commands read which tables"));
    assert!(
        commands.contains("only when the resolved config has a `[server]` table"),
        "{CONFIG_REF} must tie server startup to the resolved config"
    );
    assert!(
        commands.contains("`MANTA_SERVER_*`"),
        "{CONFIG_REF} must say the environment can supply [server]"
    );
    assert!(
        !commands.contains("only when the file has"),
        "{CONFIG_REF} still restricts server startup to a table in the file"
    );
}

/// MAN-149 PR #251 review: run_status loads [server] from --config or
/// MANTA_CONFIG and dials metrics_bind_addr:metrics_port, so `run` is not
/// the table's only reader. With no config path it skips the MANTA_*
/// overlay and resolve_status_addr dials 127.0.0.1:7302.
#[test]
fn config_reference_names_status_as_a_server_reader() {
    let md = doc(CONFIG_REF);
    let server = section(&md, "[server]");
    let intro = squash_whitespace(&server[..server.find("\n|").unwrap_or(server.len())]);
    assert!(
        !intro.contains("Only `run` reads this table"),
        "{CONFIG_REF} [server] still says only `run` reads the table"
    );
    assert!(
        intro.contains("`status` reads `metrics_bind_addr` and `metrics_port`"),
        "{CONFIG_REF} [server] must say `status` reads metrics_bind_addr and metrics_port"
    );
    assert!(
        intro.contains("with neither it dials `127.0.0.1:7302`"),
        "{CONFIG_REF} [server] must say `status` without a config file dials the default"
    );
}

/// MAN-81 PR #257 review: health_cmd::probe loads [server] through the same
/// daemon_server_config as `status`, so the reference must name `health`
/// wherever it names `status` as a reader of the metrics keys -- and say
/// that a resolved config with no [server] table is an error for it.
#[test]
fn config_reference_names_health_as_a_server_reader() {
    let md = doc(CONFIG_REF);
    let precedence = squash_whitespace(section(&md, "Precedence"));
    assert!(
        precedence
            .contains("`status`, `health` and `config check` read the path from `MANTA_CONFIG`"),
        "{CONFIG_REF} Precedence must list `health` among the MANTA_CONFIG readers"
    );
    let matrix = backticked_rows(section(&md, "Which commands read which tables"));
    let row = matrix
        .iter()
        .find(|r| r[0] == "`health`")
        .unwrap_or_else(|| panic!("{CONFIG_REF} command matrix has no `health` row"));
    assert!(
        row[1].contains("`metrics_bind_addr` and `metrics_port`"),
        "{CONFIG_REF} `health` row must name metrics_bind_addr and metrics_port"
    );
    assert!(
        row[1].contains("resolves to no `[server]` table is an error"),
        "{CONFIG_REF} `health` row must say a config with no [server] table is an error"
    );
    assert!(
        row[2].contains("`MANTA_SERVER_*` only alongside a config file"),
        "{CONFIG_REF} `health` row must say MANTA_SERVER_* needs a config file"
    );
    let server = section(&md, "[server]");
    let intro = squash_whitespace(&server[..server.find("\n|").unwrap_or(server.len())]);
    assert!(
        intro.contains("`health` reads the same two keys"),
        "{CONFIG_REF} [server] must say `health` reads metrics_bind_addr and metrics_port"
    );
    assert!(
        intro.contains("exits 1 when the named file plus overrides resolve to no `[server]` table"),
        "{CONFIG_REF} [server] must say `health` rejects a config with no [server] table"
    );
    let rows = backticked_rows(server);
    for key in ["metrics_bind_addr", "metrics_port"] {
        let row = rows
            .iter()
            .find(|r| r[0] == format!("`{key}`"))
            .unwrap_or_else(|| panic!("{CONFIG_REF} [server] has no `{key}` row"));
        assert!(
            row[2].contains("`health`"),
            "{CONFIG_REF} [server] `{key}`'s Wired cell must name `health`"
        );
    }
}

/// MAN-149 PR #251 review: config::load joins a relative path to the
/// config file's directory only when the file set it; a `MANTA_*` value
/// or a flag stays relative to the working directory.
#[test]
fn config_reference_documents_path_resolution_by_source() {
    let md = doc(CONFIG_REF);
    for (table, key, flag) in [
        ("input", "path", "--source"),
        ("spot", "blocklist_path", "--blocklist"),
        ("spot", "notch_path", "--notch"),
        ("spot", "cty_path", "--cty"),
        ("spot", "scp_path", "--scp"),
        ("spot", "pattern_path", "--patterns"),
    ] {
        let rows = backticked_rows(config_ref_table_section(&md, table));
        let row = rows
            .iter()
            .find(|r| r[0] == format!("`{key}`"))
            .unwrap_or_else(|| panic!("{CONFIG_REF} has no [{table}] `{key}` row"));
        let notes = squash_whitespace(row.last().unwrap());
        let var = format!("`MANTA_{}_{}`", table.to_uppercase(), key.to_uppercase());
        for wanted in [
            "the file's directory",
            "the working directory",
            &var,
            &format!("`{flag}`"),
        ] {
            assert!(
                notes.contains(wanted),
                "{CONFIG_REF} [{table}] `{key}` must say how a relative path resolves \
                 from the file versus {flag}/{var}; its Notes cell lacks {wanted}"
            );
        }
    }
}

/// MAN-149 PR #251 review: metrics_http::handle_request charges EOF,
/// header-timeout and header-read-error connections against
/// metrics_max_requests_per_ip, but answers only the request path with 429.
#[test]
fn metrics_reference_documents_non_request_budget_charges() {
    let md = doc(METRICS_REF);
    let scraping = squash_whitespace(section(&md, "Scraping"));
    assert!(
        !scraping.contains("caps completed requests"),
        "{METRICS_REF} still describes metrics_max_requests_per_ip as a completed-request cap"
    );
    for charged in [
        "closes without sending a byte",
        "times out reading its headers",
        "hits a read error",
    ] {
        assert!(
            scraping.contains(charged),
            "{METRICS_REF} must say a connection that {charged} is charged"
        );
    }
    assert!(
        scraping.contains("is charged but never refused"),
        "{METRICS_REF} must say those connections are never answered with 429"
    );
}

/// MAN-149 M2: config deserialization uppercases the station identity;
/// SpotMessage uses that identity for both deCall and the id prefix.
#[test]
fn spot_schema_documents_uppercased_station_identity() {
    let md = doc("docs/SPOT-SCHEMA.md");
    let rows = backticked_rows(section(&md, "Fields"));
    for field in ["deCall", "id"] {
        let row = rows
            .iter()
            .find(|r| r[0] == format!("`{field}`"))
            .unwrap_or_else(|| panic!("SPOT-SCHEMA.md is missing {field}"));
        assert!(
            row.last().unwrap().contains("uppercased"),
            "SPOT-SCHEMA.md {field} must describe the uppercased station callsign"
        );
        assert!(
            !row.last().unwrap().contains("exactly as configured"),
            "SPOT-SCHEMA.md {field} still promises to preserve config casing"
        );
    }
}

/// Every `#key = value` line of the `manta config init` scaffold, as
/// `(table, key, value_src)` -- the same two patterns `config_cmd.rs`'s
/// scaffold drift tests parse it with. `value_src` keeps any trailing
/// `# comment`, which TOML ignores.
fn scaffold_settings() -> Vec<(String, String, String)> {
    let header = regex::Regex::new(r"^#?\[\[?([a-z_]+)\]\]?\s*$").unwrap();
    let setting = regex::Regex::new(r"^#([a-z][a-z0-9_]*) = (.*)$").unwrap();
    let mut table = None;
    let mut out = Vec::new();
    for line in doc("crates/manta-cli/src/config_init.toml").lines() {
        if let Some(c) = header.captures(line) {
            table = Some(c[1].to_string());
        } else if let Some(c) = setting.captures(line) {
            let t = table
                .clone()
                .unwrap_or_else(|| panic!("scaffold setting `{line}` before any table"));
            out.push((t, c[1].to_string(), c[2].to_string()));
        }
    }
    assert!(!out.is_empty(), "no settings parsed from the scaffold");
    out
}

/// `src` as the TOML value of `v = <src>`.
fn toml_value(src: &str) -> toml::Value {
    let mut t: toml::Table = toml::from_str(&format!("v = {src}"))
        .unwrap_or_else(|e| panic!("`{src}` is not a TOML value: {e}"));
    t.remove("v").expect("v")
}

/// The cells of every `` | `name` | … `` table row in `md`, trimmed, with
/// the leading and trailing empty cells dropped.
fn backticked_rows(md: &str) -> Vec<Vec<String>> {
    md.lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            let cells: Vec<String> = l.split('|').map(|c| c.trim().to_string()).collect();
            cells[1..cells.len() - 1].to_vec()
        })
        .collect()
}

/// The first `` `code span` `` in `cell`.
fn first_code_span(cell: &str) -> Option<&str> {
    let start = cell.find('`')? + 1;
    let len = cell[start..].find('`')?;
    Some(&cell[start..start + len])
}

/// `docs/CONFIG.md`'s section for a TOML table: `## [[t]]` when it has one,
/// else `## [t]`.
fn config_ref_table_section<'a>(md: &'a str, table: &str) -> &'a str {
    if md.contains(&format!("\n## [[{table}]]")) {
        section(md, &format!("[[{table}]]"))
    } else {
        section(md, &format!("[{table}]"))
    }
}

/// Scenario 1: an operator looking a key up finds its default and whether
/// it is wired to behaviour. The Default column is pinned to the scaffold,
/// which `config_cmd.rs` pins to the code, and a row for a key the loader
/// no longer accepts fails too.
#[test]
fn config_reference_lists_every_scaffold_key_with_its_default() {
    let md = doc(CONFIG_REF);
    let settings = scaffold_settings();
    let mut tables: Vec<&str> = settings.iter().map(|(t, _, _)| t.as_str()).collect();
    tables.dedup();
    for table in tables {
        let rows = backticked_rows(config_ref_table_section(&md, table));
        for (_, key, value) in settings.iter().filter(|(t, _, _)| t == table) {
            let wanted = format!("`{key}`");
            let row = rows.iter().find(|r| r[0] == wanted).unwrap_or_else(|| {
                panic!("{CONFIG_REF} has no `| `{key}` |` row under its [{table}] heading")
            });
            assert!(
                row.len() >= 3,
                "{CONFIG_REF} [{table}] `{key}` row needs Key, Default and Wired cells"
            );
            let span = first_code_span(&row[1]).unwrap_or_else(|| {
                panic!("{CONFIG_REF} [{table}] `{key}`: the Default cell has no `code span`")
            });
            assert_eq!(
                toml_value(span),
                toml_value(value),
                "{CONFIG_REF} [{table}] `{key}`: the Default cell's first code span `{span}` \
                 is not the scaffold's value `{value}` (crates/manta-cli/src/config_init.toml)"
            );
            assert!(
                !row[2].is_empty(),
                "{CONFIG_REF} [{table}] `{key}`: the Wired cell is empty"
            );
        }
        for row in &rows {
            assert!(
                settings
                    .iter()
                    .any(|(t, k, _)| t == table && row[0] == format!("`{k}`")),
                "{CONFIG_REF} [{table}] documents {}, which the scaffold (and so the loader) \
                 does not list",
                row[0]
            );
        }
    }
}

/// SPEC §9's `not configurable yet` keys are names an operator may try;
/// CONFIG.md must say the loader rejects them.
#[test]
fn config_reference_lists_every_constant_only_key() {
    let md = doc(CONFIG_REF);
    let not_settable = section(&md, "Not settable");
    let key = regex::Regex::new(r"^#\s*([a-z][a-z0-9_]*)\s*=").unwrap();
    let constants: Vec<String> = spec_section_9_toml()
        .lines()
        .filter(|l| l.contains("not configurable yet"))
        .filter_map(|l| key.captures(l.trim_start()).map(|c| c[1].to_string()))
        .collect();
    assert!(
        !constants.is_empty(),
        "SPEC §9 has no `not configurable yet` keys"
    );
    for k in constants {
        assert!(
            not_settable.contains(&format!("| `{k}` |")),
            "{CONFIG_REF}'s \"Not settable\" section has no `| `{k}` |` row"
        );
    }
}

/// Every family `metrics.rs` renders has a row with its Prometheus type,
/// and the page names no family that does not exist.
#[test]
fn metrics_reference_lists_every_rendered_family() {
    let md = doc(METRICS_REF);
    let src = doc("crates/manta-server/src/metrics.rs");
    let type_line = regex::Regex::new(r"# TYPE (manta_[a-z0-9_]+) ([a-z]+)").unwrap();
    let mut families: Vec<(String, String)> = type_line
        .captures_iter(&src)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect();
    families.sort();
    families.dedup();
    assert!(
        !families.is_empty(),
        "no `# TYPE` lines found in metrics.rs"
    );
    for (name, kind) in &families {
        assert!(
            md.contains(&format!("| `{name}` | {kind} |")),
            "{METRICS_REF} has no `| `{name}` | {kind} |` row"
        );
    }
    let token = regex::Regex::new(r"\bmanta_[a-z0-9_]+").unwrap();
    let mut unknown: Vec<&str> = token
        .find_iter(&md)
        .map(|m| m.as_str())
        .filter(|raw| {
            let name = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| raw.strip_suffix(s))
                .unwrap_or(raw);
            !families.iter().any(|(f, _)| f == name)
        })
        .collect();
    unknown.sort();
    unknown.dedup();
    assert!(
        unknown.is_empty(),
        "{METRICS_REF} names metrics crates/manta-server/src/metrics.rs does not render: \
         {unknown:?}"
    );
}

/// The `[text](target)` link targets in `md`.
fn link_targets(md: &str) -> Vec<String> {
    let link = regex::Regex::new(r#"\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    link.captures_iter(md).map(|c| c[1].to_string()).collect()
}

/// Whether a relative link target in `from` (a repo-relative path) names
/// a file or directory that exists; `#anchors` are ignored.
fn relative_link_resolves(from: &str, target: &str) -> bool {
    let path = target.split('#').next().unwrap_or(target);
    let base = repo_root().join(from);
    base.parent().expect("a directory").join(path).exists()
}

/// Scenario 3: each audience has one entry point, and it opens.
#[test]
fn docs_index_names_a_start_here_per_audience() {
    let md = doc(DOCS_INDEX);
    for audience in ["Operators", "Contributors", "Researchers"] {
        let body = section(&md, audience);
        let line = body
            .lines()
            .find(|l| l.starts_with("Start here:"))
            .unwrap_or_else(|| panic!("{DOCS_INDEX} `## {audience}` has no `Start here:` line"));
        let targets = link_targets(line);
        let target = targets.first().unwrap_or_else(|| {
            panic!("{DOCS_INDEX} `## {audience}`'s `Start here:` line has no link")
        });
        assert!(
            relative_link_resolves(DOCS_INDEX, target),
            "{DOCS_INDEX} `## {audience}`'s `Start here:` link `{target}` does not resolve"
        );
    }
}

/// The `.md` file names directly inside a repo-relative directory.
fn md_files_in(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repo_root().join(dir))
        .unwrap_or_else(|e| panic!("reading {dir}: {e}"))
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n.ends_with(".md") && n != "README.md")
        .collect();
    names.sort();
    names
}

#[test]
fn docs_index_links_every_runbook() {
    let targets = link_targets(&doc(DOCS_INDEX));
    let runbooks = md_files_in("docs/RUNBOOKS");
    assert!(!runbooks.is_empty(), "docs/RUNBOOKS has no runbooks");
    for name in runbooks {
        let want = format!("RUNBOOKS/{name}");
        assert!(
            targets
                .iter()
                .any(|t| t.split('#').next() == Some(want.as_str())),
            "{DOCS_INDEX} does not link docs/{want}: add a row to its Runbooks table"
        );
    }
}

#[test]
fn decisions_index_links_every_record() {
    let targets = link_targets(&doc(DECISIONS_INDEX));
    let records = md_files_in("docs/DECISIONS");
    assert!(!records.is_empty(), "docs/DECISIONS has no records");
    for name in records {
        assert!(
            targets.iter().any(|t| t.split('#').next() == Some(name.as_str())),
            "{DECISIONS_INDEX} does not link {name}: add a `| date | ticket | [title]({name}) |` row"
        );
    }
}

#[test]
fn reference_docs_relative_links_resolve() {
    for file in [
        CONFIG_REF,
        METRICS_REF,
        "docs/SPOT-SCHEMA.md",
        "docs/LIVE-STATE.md",
        DOCS_INDEX,
        DECISIONS_INDEX,
    ] {
        let broken: Vec<String> = link_targets(&doc(file))
            .into_iter()
            .filter(|t| {
                !(t.starts_with("http://")
                    || t.starts_with("https://")
                    || t.starts_with("mailto:")
                    || t.starts_with('#'))
            })
            .filter(|t| !relative_link_resolves(file, t))
            .collect();
        assert!(
            broken.is_empty(),
            "{file} has relative links that do not resolve: {broken:?}"
        );
    }
}

// ---- MAN-133: completions and man pages, documented and real ----

/// MAN-133: README's Installation documents how to load the completion
/// script for every shell the binary supports and how to read `man manta`,
/// and never suggests `source <(…)` (a silent no-op on macOS's bash 3.2;
/// decision D11).
#[test]
fn readme_installation_documents_completions_and_man_pages() {
    let install = squash_whitespace(section(&doc("README.md"), "Installation"));
    assert!(
        install.contains("### Tab completion and man pages"),
        "README Installation has no `### Tab completion and man pages` subsection"
    );
    let completions_help = manta().args(["completions", "--help"]).output().unwrap();
    assert!(completions_help.status.success());
    let help = String::from_utf8(completions_help.stdout).unwrap();
    let values = help
        .lines()
        .find(|l| l.contains("[possible values: "))
        .expect("`manta completions --help` lists no possible values");
    for shell in values
        .split("[possible values: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(']')
        .split(", ")
    {
        assert!(
            install.contains(&format!("manta completions {shell}")),
            "README Installation never shows `manta completions {shell}`"
        );
    }
    for needle in ["man/man1", "manta man-pages --out", "man manta"] {
        assert!(
            install.contains(needle),
            "README Installation never says {needle:?}"
        );
    }
    assert!(
        install.contains("eval \"$(manta completions bash)\""),
        "README's bash row must use eval \"$(…)\", not source <(…)"
    );
    assert!(
        !install.contains("source <(manta completions"),
        "README still suggests `source <(manta completions …)`, a silent no-op on \
         macOS's bash 3.2 (MAN-133 D11)"
    );
}

/// MAN-133: the release runbook names the steps that render the pages and
/// where they land in each archive.
#[test]
fn release_runbook_lists_the_man_pages() {
    let runbook = squash_whitespace(&doc("docs/RUNBOOKS/release.md"));
    for needle in ["man/man1", "the `validate-tag` job's man-page steps"] {
        assert!(
            runbook.contains(needle),
            "docs/RUNBOOKS/release.md never says {needle:?}"
        );
    }
}

/// No `docs/DECISIONS/` path CLAUDE.md cites is glued to the word before it,
/// and each one that is not a glob names a `.md` file that exists. MAN-133's
/// reflow once joined `See` onto the MAN-314 record's path
/// (`Seedocs/DECISIONS/…`).
#[test]
fn claude_md_decision_paths_start_a_word_and_resolve() {
    let claude = doc("CLAUDE.md");
    for (at, _) in claude.match_indices("docs/DECISIONS/") {
        let line = claude[..at].matches('\n').count() + 1;
        assert!(
            !claude[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric),
            "CLAUDE.md:{line}: a `docs/DECISIONS/` path is glued to the word before it"
        );
        let token = claude[at..].split(char::is_whitespace).next().unwrap();
        let path = token
            .find(".md")
            .map(|end| &token[..end + 3])
            .unwrap_or_else(|| {
                panic!("CLAUDE.md:{line}: `{token}` is not a whole .md path (split across lines?)")
            });
        if path.contains(['[', '*']) {
            continue;
        }
        assert!(
            repo_root().join(path).is_file(),
            "CLAUDE.md:{line} cites {path}, which does not exist"
        );
    }
}

#[test]
fn spot_schema_transport_points_at_the_stream_contract() {
    let schema = include_str!("../../../docs/SPOT-SCHEMA.md").replace("\r\n", "\n");
    let transport = schema
        .split_once("\n## Transport\n")
        .unwrap()
        .1
        .split("\n## ")
        .next()
        .unwrap();
    assert!(transport.contains("contracts/spots/stream.v1.md"));
    assert!(!transport.contains("no history"));
}

/// SPEC §5 is the normative event schema, and `TrackClosed` carries a
/// `closure` whose kind decides whether deferred evidence is judged,
/// migrated or discarded (`manta_decode::events::ClosureKind`). PR #154
/// added the field without updating §5, which kept showing
/// `TrackClosed { track_id }` (MAN-174, finding 7).
#[test]
fn spec_section_5_track_closed_names_its_closure_kinds() {
    let spec = doc("docs/SPEC-decode-core.md");
    let s5 = section(&spec, "5. Decoder output");
    for needle in ["closure", "SignalEnded", "Bookkeeping", "survivor_track_id"] {
        assert!(
            s5.contains(needle),
            "SPEC §5's `TrackClosed` entry never names `{needle}`"
        );
    }
}

// ---- MAN-190 (PR #290 review): the docs follow the pending-hop redesign ----

/// MAN-190 added a gotcha to `wiki/pages/decode-chain.md` (v2 evidence
/// anchors are confirmed from two actual adjacent hops) but left the page
/// stamped `60c4339 / 2026-10-14`: MAN-152's re-verification, made before
/// main carried that claim. Same rule as the overview stamp test above.
#[test]
fn wiki_decode_chain_verified_stamp_is_not_the_pre_man190_one() {
    let rel = "wiki/pages/decode-chain.md";
    let claim = "anchor decisions are confirmed from two *actual* adjacent hops";
    assert!(
        doc(rel).contains(claim),
        "{rel} no longer says `{claim}`; this test pins the stamp to that MAN-190 \
         gotcha and needs updating with it"
    );
    let (commit, date) = wiki_verified_stamp(rel);
    assert!(
        commit != "60c4339" && date.as_str() > "2026-10-14",
        "{rel} still stamps `verified: {commit} / {date}`, MAN-152's re-verification \
         from before the MAN-190 gotcha: refresh `verified.commit`/`verified.date` (and \
         the manifest entry) whenever the body is substantively changed"
    );
}

/// MAN-190 computes every Evidence center for real before using its
/// maximum, so `CenteredMax::peek_next` became test-only. SPEC v2 §1.2's
/// MAN-193 implementation note still said the §1.7 lookahead reads the next
/// center's `M` from the deque without advancing it. This pins that one
/// sentence's removal; a design that peeks ahead again updates it with the
/// spec.
#[test]
fn spec_v2_drops_the_man193_lookahead_sentence() {
    let spec = squash_whitespace(&doc("docs/SPEC-decode-core-v2.md"));
    for stale in [
        "lookahead reads the next center's `M`",
        "from the same deque without advancing it",
    ] {
        assert!(
            !spec.contains(stale),
            "SPEC-decode-core-v2.md still says `{stale}`, but `CenteredMax::peek_next` is \
             test-only since MAN-190: each center's `M` comes from the query that computes it"
        );
    }
}
