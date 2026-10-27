# MAN-304: manta as an UberSDR add-on; the receiver's add-on contract, and packaging waits on the UberSDR input

**Status:** accepted. The receiver documents how an add-on publishes metrics, and its CW Spots view reads a CW Skimmer telnet feed that manta's telnet port already serves. manta still cannot read the receiver's IQ, the registry entry's fields are undocumented, nothing documents an install pointing the CW Spots view at an add-on, and the receiver's admin pages do not show add-on metrics. No add-on code, image change or registry file was written.

## Context

MAN-304 asks that a service install manta from the UberSDR receiver's add-on manager, that manta start on the receiver's Docker network and skim the bands the service selected from the receiver's own IQ, that the receiver's CW spot views show manta's spots with no configuration file edited, and that the receiver's add-on status show manta's running state, spots per hour and input status. Its technical notes name a container image, an add-on registry entry, health reporting through the receiver's add-on metrics ingestion (MQTT), and configuration through environment variables mapped onto manta's TOML.

The ticket depends on two things. The wide-IQ input is MAN-302, whose [clean-room note](2026-10-26-man302-ubersdr-iq-clean-room.md) stopped: there is no UberSDR input. One process skimming several bands shipped with MAN-13 (several `[[input]]` entries) and MAN-310 (`bands`). Submitting the registry entry upstream is outward-facing and is the owner's step.

## Inputs used

| ID | Input | Version and URL | Integrity and licence | Used for |
|---|---|---|---|---|
| U1 | Receiver `addon_mqtt.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/addon_mqtt.md), commit `5a261df82b5e491d9717239fb3e50dc49a4e93a6` (2026-10-27); 858 lines; checked 2026-10-27 | SHA-256 `686aadbfab614278efa6d51a600dac77fbc6ad2a1d74158858a3929c9a7467c6`; byte-identical at `7192bfd615830eab26bda0fa3863fcf26265c8ee`, MAN-302's pin; published documentation in a GPL-3.0 repository | H1–H11 |
| U2 | Receiver `static/extensions/cw-spots/README.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/static/extensions/cw-spots/README.md), same commit; 104 lines | SHA-256 `a03991527a90fcab06d5643248056d4fa07a194b33fe7e7c9df5da36cc473392` | S1, S2 |
| U3 | Receiver `static/extensions/dx-cluster/README.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/static/extensions/dx-cluster/README.md), same commit; 87 lines | SHA-256 `8267854eba30d5d72ff4226cad5f3d7d6db879db15e5bb8aecf3433b14fd0ff0` | S3 |
| U4 | Receiver `home_assistant.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/home_assistant.md), same commit; 351 lines | SHA-256 `fe8938ca7c882abf30ba3451bcc862b9b9bd8656120e02d264d8da96060bb5d9` | H8 context |
| U5 | Receiver `release_notes.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/release_notes.md), same commit; 445 lines | SHA-256 `e3874b578c3f25b9f033e257f3cfc1002225a2e769fb18d2528781fcd44e5ac0` (MAN-302's I5) | S3, S4 |
| U6 | Receiver `README.md` | [Raw documentation](https://raw.githubusercontent.com/madpsy/ka9q_ubersdr/5a261df82b5e491d9717239fb3e50dc49a4e93a6/README.md), same commit; 214 lines | SHA-256 `f600112886b01f1660b4c49124b422809eb77d4820648212baaee3ff8c143757` (MAN-302's I4) | The installer creates `sdr-network` |
| U7 | UberSDR website | [Published receiver site](https://ubersdr.org/), checked 2026-10-27 | Website, not source code; fetched page SHA-256 `55ab1dff22f8ca9daf817d65c14ca8b198603ba99d8a5af7f7074b87e5acc45f` on that day | Admin → Addons; S4 |
| U8 | GitHub API tree listing | `madpsy/ka9q_ubersdr` at the same commit, consulted 2026-10-27 in planning | File names only; the repository's licence from API metadata | Locating U1–U6; `addon_mqtt.md` is the only add-on Markdown document |
| U9 | manta's own commands and tests | This repository | — | M1–M6 |

No UberSDR source code was read. No Dockerfile, Compose file, shell script, registry YAML or JSON, or `testdata/` file was opened, and the 2026-10-08 competitive analysis was not used.

## Off-limits for MAN-304 work

- Everything [MAN-302's note](2026-10-26-man302-ubersdr-iq-clean-room.md) puts off-limits: the `madpsy/ubersdr-iq` and `madpsy/ka9q_ubersdr` repository pages and every file other than their published Markdown and `LICENSE-DOCS`; their `testdata/` and `conformance/` directories; Dockerfiles, Compose files and `install.sh`; the 2026-10-08 competitive analysis and the two MAN-140 research lines it names.
- The receiver's add-on registry and its tooling: `addons.json`, `config/addons.yaml.example` and `manage_addons.sh`.
- The add-on ingest and proxy source: the `addon_proxy*` and `mqtt_addon_*` files.
- The web front end: `static/v2/src/` and every file under `static/extensions/` other than its `README.md` files.
- The `madpsy/ubersdr_lightning` add-on source that U1 points to as an example.

## Gate

| ID | Check | Command | Output | Result |
|---|---|---|---|---|
| A1 | manta can read the receiver's IQ | `test -d crates/manta-input/src/ubersdr && echo present \|\| echo absent; grep -l -x '\*\*Verdict: GO\*\*' docs/DECISIONS/*-man302-*.md \|\| echo 'no MAN-302 GO record'` | `absent`; `no MAN-302 GO record` | FAIL |
| A2 | The registry entry's fields are documented | `grep -n 'addons\.yaml'` over U1–U6 | Only U1 names `addons.yaml` (lines 13, 163, 776); it documents `host`, `name` and `enabled` and nothing else | FAIL |
| A3 | Installing an add-on points the CW Spots view at it with no file edited | `grep -n 'cwskimmer\.yaml'` over U1–U6 | Only U2 names `cwskimmer.yaml` (lines 23, 39, 92), as operator settings; no permitted document says an install writes it | FAIL |
| A4 | The add-on metrics ingestion is documented | `` grep -n -E '^\| `(POST\|DELETE\|GET)`' `` over U1 | Lines 735–739: publish, clear, discovery, discovery removal and health endpoints | PASS |
| A5 | The receiver's admin pages show an add-on's metrics | `grep -n 'does not yet render'` over U1 | Line 857: the admin Monitor page does not yet render the ingest block | FAIL |
| A6 | A pullable manta image exists | `git ls-remote --tags origin 'refs/tags/v*' \| wc -l` | `0` | FAIL |

**Verdict: STOP**

GO requires A1–A5 to pass. A6 gates only the registry submission. Every check ran on 2026-10-27, in the planning container and again in the implement-phase container; the implement phase re-fetched U1–U6 at the pinned commit into a scratch directory outside the checkout, and every SHA-256 above matched. No container runtime was available in either container, so no receiver was started and no receiver was contacted.

## What the receiver documents

These rows paraphrase U1–U7. Line numbers are in U1 at the pinned commit unless the row names another source.

| ID | Fact | Source |
|---|---|---|
| H1 | Add-ons run as their own containers on `sdr-network`. They get no broker address and no MQTT credentials, by design. | U1 3–6; U6 |
| H2 | An add-on makes plain HTTP requests to the ingest listener at `ubersdr:6926`; the receiver builds the topic and publishes it on its own MQTT client. An add-on never speaks MQTT. | U1 86–88 |
| H3 | The receiver identifies the caller from the raw TCP source address, matched against the container hostname (`host`) of an *enabled* entry in the operator's `addons.yaml`. `name` is the URL slug and is often, not always, the same. There is no token; a renamed or removed container loses access, and a newly installed one is recognised within seconds. | U1 147–164, 775–780 |
| H4 | `POST /publish/{sub_topic}` lands on `{prefix}/addons/{addon}/{sub_topic}`, `{prefix}` defaulting to `ubersdr/metrics`. A sub-topic matches `^[a-z0-9][a-z0-9_-]*(/[a-z0-9][a-z0-9_-]*)*$`: lowercase, no `.`, at most 64 characters and 4 segments. `status` is reserved. | U1 111–125, 174–189, 229–232 |
| H5 | A payload is JSON, or UTF-8 text under `text/plain`, without control characters, up to 64 KB. `retain` and `qos` are query parameters; `DELETE` clears a retained topic. Each add-on gets 120 publishes a minute from a token bucket that refills at 2 a second. | U1 234–282, 741–750 |
| H6 | `400` means fix the request and do not retry, `403` means the caller is not a recognised add-on, `429` means back off, and `503` means the broker is unreachable: retry with backoff, without crashing. | U1 284–308 |
| H7 | The receiver keeps a retained `…/{addon}/status` topic for each add-on: `online` on a publish, `offline` after 300 s (`offline_after_sec`) without one. A receiver restart grants a full grace period. | U1 499–528 |
| H8 | `POST /discovery` declares Home Assistant `sensor` and `binary_sensor` entities, idempotently, up to 20 per add-on. The receiver computes every identity and topic field; the add-on appears as its own device under the receiver. U4 says installed add-ons that publish appear in Home Assistant with no extra operator setup. | U1 312–476, 749; U4 |
| H9 | `GET /health` returns the name the receiver resolved the caller to and the live limits. | U1 56–80 |
| H10 | The listener runs only while MQTT is enabled on the receiver. When every request fails to connect, ingest is off and the add-on should keep working. An operator may narrow the permitted add-ons with `allowed_containers`. | U1 12–14, 785–786, 812 |
| H11 | Per-add-on ingest activity (publish and rejection counts, online state, declared entities) comes from `GET /admin/mqtt-health` under `addon_ingest`. The admin Monitor page does not yet render it. | U1 826–858 |
| S1 | The CW Spots view shows spots from the CW Skimmer server named in `cwskimmer.yaml`: `enabled`, `host`, `port` (default 7300) and a `callsign` used to log in. The view itself is enabled in `extensions.yaml`. | U2 Installation, Configuration (lines 23–44) |
| S2 | The receiver relays each spot to the browser as a `cw_spot` WebSocket message with frequency, call, SNR, speed, comment, time and band. | U2 Message Format |
| S3 | The DX Cluster view reads a `dxcluster.server` host and port from `config.yaml`. Release 0.1.57 added a DX Cluster Combiner add-on that merges CW and other spots and serves them over telnet on port 7300. | U3 Configuration; U5 0.1.57 |
| S4 | Release 0.1.48 added the Add-on Manager in the admin Addon tab; 0.1.49 says the CW Skimmer does not work on ARM64; 0.1.67 announces a Skimmer add-on, still in development, meant to replace the built-in skimmers. The website puts add-on installation and activation under Admin → Addons, describes the current CW Skimmer add-on as Afreet SkimServ in a Linux desktop, not supported on ARM, and names the RX-888 MkII as the only supported hardware. | U5 0.1.48, 0.1.49, 0.1.67; U7 |

## manta today

| ID | Fact | Evidence |
|---|---|---|
| M1 | manta's telnet port 7300 greets like CW Skimmer, with a callsign prompt, and sends RBN-layout `DX de` lines after login. | `a_connecting_client_gets_the_cw_skimmer_shaped_banner_and_callsign_prompt` and `standard_client_receives_spot_in_rbn_format_after_login` in `crates/manta-server/tests/telnet_acceptance.rs`: 2 passed on 2026-10-27 |
| M2 | One input can come from the environment alone: `MANTA_CONFIG=/dev/null` with `MANTA_SERVER_STATION_CALLSIGN`, `MANTA_INPUT_TYPE=kiwi`, `MANTA_INPUT_HOST` and `MANTA_INPUT_FREQ_HZ` passes `manta config check` (`/dev/null: valid (from MANTA_CONFIG)`). Several bands cannot: with two `[[input]]` entries, `MANTA_INPUT_FREQ_HZ` is refused with `error: MANTA_INPUT_FREQ_HZ cannot say which of the 2 [[input]] entries it sets; set freq_hz in that entry instead` (MAN-13 D10). `bands` is accepted only for kiwi, soapy and hpsdr inputs. | `manta config check`, run 2026-10-27; `crates/manta-cli/src/config.rs` |
| M3 | With no arguments, no `MANTA_CONFIG` and no file at `/etc/manta/manta.toml`, the image's entrypoint runs `manta --help` (MAN-81 decision 2). The image's `HEALTHCHECK` passes `--config "${MANTA_CONFIG:-/etc/manta/manta.toml}"`, so in an env-only container it fails with `reading config file /etc/manta/manta.toml: No such file or directory`, while bare `manta health` would ask the daemon on 127.0.0.1:7302. | `packaging/docker/docker-entrypoint.sh`; `Dockerfile`; `manta health`, run 2026-10-27 |
| M4 | `/status` carries the cumulative `spots_total` and the MAN-14 `inputs` array. The only rate is `spots_per_min` in the periodic status log line. manta has no spots-per-hour figure. | `crates/manta-server/src/status_doc.rs`; `crates/manta-server/src/status.rs` |
| M5 | `[input].name` allows letters, digits, `.`, `_` and `-`. U1's sub-topic charset allows neither uppercase nor `.` (H4). | `docs/CONFIG.md` `[input]` table |
| M6 | No `v*` tag exists, so no `ghcr.io/hagaletechnologies/manta` image has been published; `packaging/README.md` says the image does not exist yet. | A6; `packaging/README.md` |

## Open claims

| ID | Claim the GO track needs | Status |
|---|---|---|
| C1 | The fields of an add-on registry entry beyond `host`, `name` and `enabled`, and the image reference format | open |
| C2 | Whether an operator can add an entry for a self-built add-on, or only from the receiver's catalogue | open |
| C3 | Whether installing an add-on points the CW Spots view (`cwskimmer.yaml`) at it | open |
| C4 | Whether the receiver's CW Skimmer client reads manta's `DX de` lines as it reads SkimServ's | open |
| C5 | Where the receiver's admin pages show an add-on's running state and metrics | open |
| C6 | How an add-on container reaches the receiver's IQ and learns the bands the service selected | open |
| C7 | Whether the add-on manager gives the container environment variables, a command or a mounted file | open |

A claim's status is `open`, `documented: <raw URL at a pinned commit>` or `observed: <path in this repository>`. `man304_addon_note_records_sources_gate_and_stop_rule` in `crates/manta-cli/tests/docs_consistency.rs` refuses any other status.

## Decisions

- **D1 Source boundary.** MAN-304 keeps MAN-302's boundary for UberSDR material: the website, the receiver's published Markdown at a pinned commit, and file names from the GitHub API. The off-limits list above applies. The receiver is GPL-3.0; manta stays MIT OR Apache-2.0 clean.
- **D2 Verdict and stop rule.** STOP. While this record says STOP, no add-on wrapper, image change, publisher or registry file lands. The GO track keeps its files under `packaging/ubersdr/`, and `man304_addon_note_records_sources_gate_and_stop_rule` fails if that directory exists without a GO record. A MAN-304 GO also needs a MAN-302 GO; the same test fails on a MAN-304 GO without one.
- **D3 Health through the ingest port, not an MQTT client.** An add-on has no broker (H1, H2). manta's GO-track publisher POSTs to the ingest port and adds no MQTT dependency. It runs in-process, like the RBN uplink task, so the image needs no `curl` or Python and, as with MAN-81 decision 4, gains no apt packages. It treats a connection failure, `403` or `503` as "ingest unavailable" and keeps skimming (H6, H10).
- **D4 Spots over the existing telnet port.** The CW Spots view reads a CW Skimmer telnet server (S1), and manta's telnet port already is one (M1). The GO track adds no spot sink; the wiring is C3 and C4.
- **D5 Band selection renders a file; the base image is unchanged.** `MANTA_INPUT_*` covers one input, and several bands need one `[[input]]` per band (M2). `bands` does not cover `ubersdr` in MAN-302's plan. The GO add-on wrapper renders a TOML file from its environment and runs `manta run --config` on it. The base image keeps MAN-81's entrypoint. An env-only bare start would also need the `HEALTHCHECK`'s `--config` default changed (M3), and nothing needs that before the GO track.
- **D6 Topic names and cadence.** The GO publisher derives sub-topics from input names by lowercasing and mapping `.` to `_` (M5, H4), and refuses a collision at config check. It publishes one retained summary at most once a minute, well under 120 a minute (H5) and inside the 300 s offline threshold (H7).
- **D7 Spots per hour is manta's number.** The GO track computes it in manta from its own spot counter over a trailing 60-minute window (M4). The exact field name and Home Assistant entity are defined and design-reviewed in the GO plan.
- **D8 KiwiSDR emulation is not a stand-in.** The ticket names the wide-IQ input, the emulation is untested (MAN-302 note), and covering one band's CW segment takes several narrow sessions.
- **D9 The draft registry entry stays in this record.** Nothing is submitted upstream or committed as a registry file; the owner submits once A6 and C1 allow.

## Draft registry entry (not submitted)

The owner submits it; field names other than `name`, `host` and `enabled` wait on C1, and the image on A6.

| What the entry needs | manta's value | Documented field |
|---|---|---|
| URL slug | `manta` | `name` (U1) |
| Container hostname the ingest port matches | `manta` | `host` (U1) |
| Enabled | yes | `enabled: true` (U1) |
| Image | none published yet (A6) | open (C1) |
| Spot feed for the CW Spots view | host `manta`, port 7300, any login callsign | open (C3) |
| Bands | the service's selection, one `[[input]]` each | open (C6, C7) |
| Station callsign | `[server].station_callsign` | open (C7) |

## What would unblock it

- MAN-302 reaching GO (its note's own "What would unblock it").
- Published receiver documentation, or a local receiver observation, settling C1–C7.
- The owner's first `v*` tag publishing an image (A6), before submission.

Then a MAN-304 GO record and a revised plan for the GO track, design-reviewed for its environment variables, registry entry and Home Assistant entities.

## Consequences

There is no new input, flag, configuration key, metric, image behaviour or registry file. MAN-304 stays blocked on MAN-302. Manta remains MIT OR Apache-2.0 clean.

## References

MAN-304 ticket and its 2026-10-27 plan; [MAN-302 clean-room note](2026-10-26-man302-ubersdr-iq-clean-room.md), [MAN-81 Docker decision](2026-10-11-man81-docker-config-mount-and-healthcheck.md), [MAN-13 multi-input decision](2026-10-15-man13-multi-input-daemon.md), [MAN-310 band plan](2026-10-17-man310-band-plan.md), [MAN-14 hearing visibility](2026-10-19-man14-per-receiver-hearing-visibility.md), [MAN-319 capture-dependent decision](2026-10-19-man319-uplink-registration-exchange.md).
