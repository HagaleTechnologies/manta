# MAN-249: public docs name roles, not machines, people or receiver hosts

Date: 2026-10-26. Ticket: MAN-249, from repository audit finding
MAN-SEC-009 (2026-10-05). The owner chose option `man-public-disclosure` A
on 2026-10-24: take the ticket's default direction for each of six items.
Related: MAN-150, MAN-152, MAN-238.

This record holds none of the removed values. History is not rewritten:
earlier commits still carry them, and rollback is a revert of this change.

## Decisions and outcomes

1. **Internal machine names become roles.** Four docs named fleet hosts:
   the MAN-18 Pi 4 decision record and runbook, which the ticket named, and
   the MAN-27 fleet-rename decision record and runbook, which the guard
   below also covers. They now say "the session's build host", "roster
   host 1" and so on. The Pi 4 passage that listed the remote-login client
   config consulted across the fleet now keeps only the fact the gate needs:
   no Pi 4 or other Linux/aarch64 board was reachable, and the only aarch64
   machines in reach were Apple Silicon Macs (D2).
   `public_docs_name_no_internal_machine` in
   `crates/manta-cli/tests/docs_consistency.rs` fails if a denied name
   comes back anywhere under `docs/` or `wiki/` (D1).
2. **The volunteer behind the test recordings is attributed neutrally.**
   The corpus README says "a volunteer operator" and then "the operator".
   The maintainers hold the contact; consent to be named is not recorded.
   The README still says **Redistribution rights are not established**, and
   the operator's permission is still the step before any upload. The same
   wording replaces the name in the corpus runbook, the MAN-119 and MAN-179
   records, the Aggregator-capture runbook, and the HPSDR hardware notes
   (the spike record, MAN-55 and two `hpsdr.rs` comments) (D3).
   `manta oracle --spotter` lost its default, which was the volunteer's
   callsign, and is now required (D4). `.github/workflows/release.yml`,
   which the ticket names, no longer exists: MAN-241 removed it, and
   `release-publish.yml` names no one.
3. **The RBN spot extract leaves the tree.**
   `crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv`
   is untracked and `.gitignore` now ignores `ground-truth/*.rbn.csv`; hand
   transcripts there stay tracked (D5). The directory was not ignored
   before, despite the ticket's wording: `.gitignore` said it was tracked.
   The corpus README's *Ground truth* section gives the command that
   rebuilds the file from RBN's public daily archive, with its SHA-256; the
   rebuilt file is byte-identical to the copy that was tracked.
4. **Receiver hosts in docs become placeholders.** The KiwiSDR pins record,
   plan and design spec show `<public-kiwisdr-host>:8073`. The ignored live
   test in `crates/manta-input/src/kiwi.rs` keeps its host (D6).
5. **Personal home paths leave the two planning docs.** The refiner
   burst-drain and SoapySDR input plans use `$HOME/` (D7). The
   service-account home paths in `packaging/` and the `Dockerfile` are not
   personal and stay.
6. **The wiki configuration matches reality.** `wiki/wiki.toml` already
   says `visibility = "public"`, set by MAN-152 (`d95aec38`, #285). It is
   unchanged. The ticket cited `40d275a4`, which is MAN-277.

## Rationale

- **D1. The deny-list holds hashes, not names.** A plain-text list would
  put the names back in the tree it cleans. The test hashes each lowercase
  ASCII-alphanumeric word of every UTF-8 file under `docs/` and `wiki/`
  with FNV-1a 64 and reports `path:line` only, so a CI log never echoes a
  name. `deny_list_matches_whole_words_in_any_case` checks the matcher on a
  dummy word. To deny another host, add the output of this command to
  `INTERNAL_HOST_FNV1A`:

  ```sh
  python3 -c 'import sys;h=0xcbf29ce484222325;[h:=((h^b)*0x100000001b3)%2**64 for b in sys.argv[1].lower().encode()];print(hex(h))' NAME
  ```

- **D2. The remote-login passage keeps reachability only.** The gate needs
  to know that no Pi-class board was reachable. Which config files were
  read, on which hosts, adds nothing to that and describes the fleet.
- **D3. The given name goes everywhere; the callsign goes where it credits
  the recordings.** It left the corpus README, `main.rs`, and every
  sentence that names it as the source of the recordings or the permission
  holder. Where it is only the RBN spotter that scoring filters on
  (`manifest.json`, `grade` fixtures and tests, config examples, dated
  measurement records), it stays: renaming those would move golden
  fixtures, and the call is the reference skimmer's public RBN identity.
  The HPSDR notes name the same person in another role (hardware owner),
  with the same unknown consent, so the given name went there too.
- **D4. `oracle --spotter` is required.** No neutral default can be right:
  the reference set is whichever skimmer sat next to the recording's
  receiver. A wrong default would score against an empty or wrong set
  without saying so; a missing flag fails with exit 2 and a usage error.
  Test callsigns in `main.rs` are `N0ABC`/`n0abc`. The MAN-166 stage-2
  record's `oracle` re-run command gained a note that the flag is now
  required, beside its existing note about `--capture-start`.
- **D5. Ignore `ground-truth/*.rbn.csv`, not the directory.** The README
  documents `ground-truth/<stem>.transcript.csv` as the place for hand
  transcripts, which must stay tracked. `scripts/tests/test_corpus.py`
  checks an RBN truth entry's path shape instead of the file on disk, and
  still requires transcript truth to exist.
- **D6. `kiwi.rs` is untouched.** An environment variable with the real
  host as fallback keeps the literal; one without a fallback changes how
  `cargo test -- --ignored` behaves. Neither removes the host without a
  behaviour change, which the ticket made the condition for touching code.
- **D7. `$HOME/` replaces the home directory,** as the ticket allows. The
  commands stay copyable.
- **D8. Where the outcome is recorded:** this record and its
  `docs/DECISIONS/README.md` row, a `CHANGELOG.md` `### Changed` line for
  the `oracle` flag (not decoder output), and one `wiki/INDEX.md` pointer.

## Owner review

The change touches three `.github/CODEOWNERS` paths: `/.gitignore`,
`/scripts/` (`test_corpus.py`) and the KiwiSDR pins record, so @thagale
approves it. The Pi 4 runbook and the pins record ship in release archives
(`scripts/package-release.py`); the next release carries the role-level
text.

## Follow-ups

- **FU-1.** Before `MANTA_CORPUS_BASE_URL` is set, `corpus.yml` needs a
  step that builds B2's truth CSV; without it, `scripts/corpus.py score`
  reports B2 as `cannot score against …`. The corpus runbook's §3 lists
  that step.
- **FU-2.** A tree-wide sweep of the callsign where it is only a spotter
  filter value, if the owner wants one. It would rename golden fixtures.
- **FU-3.** The live-test host in `crates/manta-input/src/kiwi.rs`, if a
  way to drop it without a behaviour change turns up.
