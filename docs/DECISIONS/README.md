# Decision records

One row per record in this directory, newest first. A record is dated by
when it was written and is not rewritten afterwards; a later record
supersedes an earlier one by saying so. The ticket is the `MAN-` issue the
record belongs to, or `—` when it has none.

Add a row here in the same change that adds a record:
`decisions_index_links_every_record` in
`crates/manta-cli/tests/docs_consistency.rs` fails until you do.

| Date | Ticket | Title |
|---|---|---|
| 2026-10-17 | MAN-108 | [MAN-108: a fading signal keeps its track through the fade (DORMANT re-acquisition)](2026-10-17-man108-dormant-track-reacquisition.md) |
| 2026-10-17 | MAN-155 | [MAN-155: the related-projects page links other projects and makes no claims about them](2026-10-17-man155-related-projects-links-only.md) |
| 2026-10-16 | MAN-272 | [MAN-272: a spot carries its track's latest RST and a QRL? flag](2026-10-16-man272-rst-and-qrl-on-spots.md) |
| 2026-10-16 | MAN-252 | [MAN-252: a stop request abandons a source reopen in flight](2026-10-16-man252-cancellable-source-reopen.md) |
| 2026-10-16 | MAN-245 | [MAN-245: the Codex gate fails closed on an incomplete thread read, Dependabot is classified by author, and each required check names its source](2026-10-16-man245-gate-verdict-integrity.md) |
| 2026-10-16 | MAN-238 | [MAN-238: CLAUDE.md is a short standing brief; its per-ticket log is archived here](2026-10-16-man238-standing-brief.md) |
| 2026-10-15 | MAN-13 | [MAN-13: one daemon decodes several receivers into one spot output](2026-10-15-man13-multi-input-daemon.md) |
| 2026-10-15 | MAN-233 | [MAN-233: SoapySDR overflows are counted and warned, not silent](2026-10-15-man233-soapy-overflow-visibility.md) |
| 2026-10-15 | MAN-236 | [MAN-236: the soak harness measures the loop manta run executes](2026-10-15-man236-soak-metrics-wraps-listen.md) |
| 2026-10-15 | MAN-235 | [MAN-235: doctor hints a longer run, and --strict puts the verdict in the exit code](2026-10-15-man235-doctor-rerun-hint-and-strict-exit.md) |
| 2026-10-15 | MAN-234 | [MAN-234: one bad input sample is replaced and counted, not fatal](2026-10-15-man234-bad-input-samples.md) |
| 2026-10-15 | MAN-371 | [MAN-371: `test (windows-latest)` becomes a required check](2026-10-15-man371-windows-required-check.md) |
| 2026-10-14 | MAN-82 | [MAN-82: a SoapySDR release archive for Linux x86_64](2026-10-14-man82-soapy-release-archive.md) |
| 2026-10-14 | MAN-137 | [MAN-137: resumable JSON spot stream (hello, seq, WebSocket replay)](2026-10-14-man137-json-stream-resume.md) |
| 2026-10-14 | MAN-232 | [MAN-232: a closed stdout stops manta through the drain; a second stop signal exits at once](2026-10-14-man232-closed-and-stalled-output.md) |
| 2026-10-14 | MAN-248 | [MAN-248: a per-minute ceiling on spots the RBN uplink forwards](2026-10-14-man248-uplink-forwarding-ceiling.md) |
| 2026-10-14 | MAN-211 | [MAN-211: TrackMeta's frequency is captured per queued hop, like its SNR](2026-10-14-man211-trackmeta-freq-per-hop.md) |
| 2026-10-14 | MAN-197 | [MAN-197: a live track's input selection also skips cooling-down channels](2026-10-14-man197-live-window-cooldown-enforcement.md) |
| 2026-10-14 | MAN-197 | [MAN-197: the respawn cooldown gates a spawn candidate's whole ownership window](2026-10-14-man197-respawn-cooldown-ownership-window.md) |
| 2026-10-14 | MAN-196 | [MAN-196: the decimator's trusted band — advertised and spawn-vetoed](2026-10-14-man196-decimator-trusted-band.md) |
| 2026-10-14 | MAN-174 | [MAN-174: deferred beacons survive merge chains and every loss is counted](2026-10-14-man174-deferred-beacon-merge-bookkeeping.md) |
| 2026-10-14 | MAN-192 | [MAN-192: the HSMM decoder reseeds after a committed Silence, not only on a gate re-onset](2026-10-14-man192-silence-reseed.md) |
| 2026-10-14 | MAN-193 | [MAN-193: Evidence's centered maximum is a monotonic deque](2026-10-14-man193-centered-maximum-deque.md) |
| 2026-10-14 | MAN-95 | [MAN-95: callsign-pattern rarity sets the repetition requirement](2026-10-14-man95-pattern-rarity-repetition-tiers.md) |
| 2026-10-13 | MAN-313 | [Non-CW rejection: no CW track on speech, data or a swept carrier](2026-10-13-man313-non-cw-rejection.md) |
| 2026-10-13 | MAN-189 | [MAN-189: the HSMM decoder commits each stretch of time once](2026-10-13-man189-hsmm-commit-frontier.md) |
| 2026-10-13 | MAN-190 | [MAN-190: evidence anchors confirmed from actual adjacent hops, not predicted](2026-10-13-man190-confirmed-evidence-anchors.md) |
| 2026-10-13 | MAN-140 | [MAN-140: a ka9q-radio (radiod) multicast IQ input](2026-10-13-man140-ka9q-radio-input.md) |
| 2026-10-13 | MAN-152 | [MAN-152: the in-repo wiki is canonical; disabling GitHub's Wiki is pending](2026-10-13-man152-one-wiki.md) |
| 2026-10-13 | MAN-156 | [MAN-156: CI catches dependency, rustdoc, Windows-release and PR-title regressions](2026-10-13-man156-ci-regression-gates.md) |
| 2026-10-13 | MAN-133 | [MAN-133: shell completions and a man page for the manta CLI](2026-10-13-man133-shell-completions-and-man-pages.md) |
| 2026-10-13 | MAN-312 | [MAN-312: band-wide static crashes start no tracks and are not decoded](2026-10-13-man312-static-crash-filter.md) |
| 2026-10-12 | MAN-316 | [MAN-316: scheduled refresh of cty.dat and MASTER.SCP](2026-10-12-man316-reference-list-refresh.md) |
| 2026-10-12 | MAN-324 | [MAN-324: a live-state stream for dashboards](2026-10-12-man324-live-state-stream.md) |
| 2026-10-12 | MAN-129 | [MAN-129: a shipped Grafana dashboard and Prometheus alert rules](2026-10-12-man129-dashboard-and-alerts.md) |
| 2026-10-12 | MAN-118 | [MAN-118: decode-quality signals on `/metrics`](2026-10-12-man118-decode-quality-metrics.md) |
| 2026-10-11 | MAN-315 | [MAN-315: per-sender dot, dash and gap timing in the legacy decoder](2026-10-11-man315-sender-timing.md) |
| 2026-10-11 | MAN-314 | [MAN-314: RTTY recognition keeps RTTY out of CW tracks](2026-10-11-man314-rtty-recognition.md) |
| 2026-10-11 | MAN-301 | [MAN-301: grade every manta spot live against what the RBN heard](2026-10-11-man301-live-rbn-comparison.md) |
| 2026-10-11 | MAN-298 | [MAN-298: the first release (v0.1.0)](2026-10-11-man298-first-release.md) |
| 2026-10-11 | MAN-81 | [MAN-81: the Docker image runs a mounted config and reports its own health](2026-10-11-man81-docker-config-mount-and-healthcheck.md) |
| 2026-10-11 | MAN-115 | [MAN-115: start-up latency before the first decode](2026-10-11-man115-startup-latency.md) |
| 2026-10-11 | MAN-172 | [MAN-172: high-SNR track churn](2026-10-11-man172-high-snr-track-churn.md) |
| 2026-10-11 | MAN-149 | [MAN-149: config, metrics and spot-schema references, and a docs index](2026-10-11-man149-docs-reference-and-index.md) |
| 2026-10-11 | MAN-138 | [MAN-138: a JSON spot's `deGrid`/`deLat`/`deLon` are the configured station position](2026-10-11-man138-station-location.md) |
| 2026-10-11 | MAN-131 | [MAN-131: errors say what to check next](2026-10-11-man131-actionable-errors.md) |
| 2026-10-11 | MAN-90 | [MAN-90: the direct RBN uplink's protocol is unverified — known gaps and the capture that would close them](2026-10-11-man90-uplink-protocol-verification.md) |
| 2026-10-11 | MAN-127 | [MAN-127: `manta calibrate` measures the receiver's frequency offset](2026-10-11-man127-calibrate-command.md) |
| 2026-10-11 | MAN-93 | [MAN-93: a configurable heard-time window for `sh/dx` history](2026-10-11-man93-sh-dx-history-window.md) |
| 2026-10-10 | MAN-270 | [MAN-270: command output style — human units, stdout/stderr routing, one-line escaped errors](2026-10-10-man270-cli-output-style.md) |
| 2026-10-10 | MAN-269 | [MAN-269: paced and looping file replay](2026-10-10-man269-paced-looping-replay.md) |
| 2026-10-10 | MAN-126 | [MAN-126: `manta doctor` checks the whole setup and names each failing check](2026-10-10-man126-doctor-setup-checks.md) |
| 2026-10-10 | MAN-92 | [MAN-92: telnet replies for unknown commands, `sh/version`, scoped `sh/dx`](2026-10-10-man92-telnet-commands.md) |
| 2026-10-10 | MAN-91 | [MAN-91: the RBN uplink forwards CQ and beacon spots by default](2026-10-10-man91-uplink-spot-type-filter.md) |
| 2026-10-10 | MAN-83 | [MAN-83: build identity in `--version` and `decoderVersion`, and when decoder output may change](2026-10-10-man83-build-identity-and-decoder-versioning.md) |
| 2026-10-10 | MAN-80 | [MAN-80: SHA256SUMS and build-provenance attestation on every release](2026-10-10-man80-release-checksums-attestation.md) |
| 2026-10-10 | MAN-79 | [MAN-79: operator-supplied country and known-callsign tables](2026-10-10-man79-operator-cty-scp-override.md) |
| 2026-10-10 | MAN-78 | [MAN-78: live reload of the `[spot]` lists and uplink `dry_run`](2026-10-10-man78-live-reload.md) |
| 2026-10-10 | MAN-268 | [MAN-268: unattended service kit and the example-callsign startup guard](2026-10-10-man268-unattended-packaging.md) |
| 2026-10-10 | MAN-244 | [MAN-244: owner-approved releases from default-branch history](2026-10-10-man244-release-governance.md) |
| 2026-10-10 | MAN-243 | [MAN-243: unreviewed pull-request code stays out of main's CI context and build cache](2026-10-10-man243-ci-trust-boundary.md) |
| 2026-10-10 | MAN-125 | [MAN-125: device listing and source diagnostics](2026-10-10-man125-source-diagnostics.md) |
| 2026-10-10 | MAN-124 | [MAN-124: plain logs off a terminal, verbosity flags, JSON logs](2026-10-10-man124-log-output.md) |
| 2026-10-10 | MAN-123 | [MAN-123: grouped decoded text, spots on stdout, a quiet daemon](2026-10-10-man123-grouped-decoded-text.md) |
| 2026-10-10 | MAN-116 | [MAN-116: `manta bench sensitivity`, a standing recall/CER-vs-SNR curve](2026-10-10-man116-sensitivity-benchmark.md) |
| 2026-10-08 | MAN-132 | [MAN-132: the metrics listener binds loopback by default](2026-10-08-man132-metrics-loopback-bind.md) |
| 2026-10-07 | MAN-96 | [MAN-96: secondary-skimmer field node (kit)](2026-10-07-man96-secondary-skimmer-field-node.md) |
| 2026-10-07 | MAN-76 | [MAN-76: `manta config check` and `manta config init`](2026-10-07-man76-config-check-init.md) |
| 2026-10-06 | MAN-261 | [MAN-261: one TOML file drives a manta deployment](2026-10-06-man261-config-surface.md) |
| 2026-10-05 | MAN-73 | [MAN-73: live source reconnect design](2026-10-05-man73-source-reconnect.md) |
| 2026-10-05 | MAN-213 | [MAN-213: fade-tracking keying rails on top of MAN-103's unbiased edge placement](2026-10-05-man213-fade-tracking-keying-rails.md) |
| 2026-10-05 | MAN-128 | [MAN-128: node health from metrics alone](2026-10-05-man128-node-health-metrics.md) |
| 2026-09-12 | MAN-217 | [MAN-217: native merge-queue admission ignores ruleset bypass_actors](2026-09-12-man217-native-queue-admission-ignores-bypass-actors.md) |
| 2026-09-10 | — | [Synchronized RBN capture proves the detection gap is manta-side, not propagation or ADC overload; plus a new startup-transient artifact](2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md) |
| 2026-09-10 | — | [Post-antenna-fix 90-minute (wall-clock) unattended soak: real detection confirmed at scale; SDRplay stream/device failures are a real, recurring operational problem](2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md) |
| 2026-09-10 | MAN-171 | [MAN-171's "dead zone" traces to the RF/antenna path on the test rig, not manta's channelizer/detector/track-manager; the startup-transient comb is a real, absolute-frequency-locked hardware artifact, and it now has a real fix](2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md) |
| 2026-09-10 | — | [Reseating connections and adding a common-mode choke resolves the live-hardware detection gap](2026-09-10-antenna-path-fix-resolves-detection-gap.md) |
| 2026-09-09 | — | [`--soapy-gain` on this RSP1B is a gain-*reduction* (attenuation) scale, not a gain scale -- every prior fixed-gain field session used a heavily-attenuated setting](2026-09-09-soapy-gain-is-inverted-attenuation-scale.md) |
| 2026-09-09 | — | [Post-PR#154 20m daytime session: fix holds, first plausible real off-air catch](2026-09-09-post-pr154-20m-daytime-validation.md) |
| 2026-09-09 | — | [Overnight 40m SoapySDR field test: fixed a real crash, still no confirmed off-air copy](2026-09-09-overnight-40m-soapy-field-test.md) |
| 2026-09-09 | MAN-166 | [MAN-166: track_cap raised; confirm_hops investigated and deliberately left alone](2026-09-09-man166-confirm-hops-and-track-cap.md) |
| 2026-09-09 | — | [`manta doctor`: a real `TrackPromoted` event replaces decode-timing proxies](2026-09-09-doctor-track-promoted-event.md) |
| 2026-09-09 | — | [Manta: twenty investigations toward exceptional CW reception](2026-09-09-decoder-recall-research.md) |
| 2026-09-09 | — | [decode-core-v2 stage-2 gate (Hsmm engine) -- measured, FAIL](2026-09-09-decode-core-v2-stage2-gate.md) |
| 2026-09-09 | — | [Non-allowlisted Beacon spots: emission deferred from first-decode to track-close](2026-09-09-beacon-emission-deferred-to-track-close.md) |
| 2026-09-09 | — | [Dial-shift test confirms a passband-edge track-promotion artifact on 20m; the interior 20m cluster is not the same artifact (likely RTTY/digital, unconfirmed)](2026-09-09-20m-dial-shift-edge-artifact-confirmed.md) |
| 2026-09-08 | — | [First live SDRplay RSP1B run: findings](2026-09-08-first-live-rsp1b-run.md) |
| 2026-09-07 | MAN-87 | [MAN-87: telnet IAC option-negotiation policy, and login control-byte trimming](2026-09-07-man87-telnet-iac-policy.md) |
| 2026-09-07 | MAN-86 | [MAN-86: Aggregator `SKIMMER/SETT` handshake, greeting banner, `BYE`, login validation](2026-09-07-man86-aggregator-sett-handshake.md) |
| 2026-09-07 | MAN-85 | [MAN-85: one signal path for SIGINT, SIGTERM and SIGHUP](2026-09-07-man85-signal-handling.md) |
| 2026-09-07 | MAN-136 | [MAN-136: `dxDxcc`/`deDxcc` and the `UNKNOWN_*` geography sentinels](2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md) |
| 2026-09-07 | MAN-122 | [MAN-122: operator liveness logging (startup banner + periodic status line)](2026-09-07-man122-operator-liveness-logging.md) |
| 2026-09-07 | MAN-103 | [MAN-103: unbiased keying-edge placement and symmetric dit-period estimation](2026-09-07-man103-keying-edge-placement.md) |
| 2026-09-07 | MAN-102 | [MAN-102: SNR estimator and wire reference bandwidth](2026-09-07-man102-snr-reference-and-estimator.md) |
| 2026-09-07 | MAN-101 | [MAN-101: V8w's "0 bogus callsigns" criterion is now its own always-run gate](2026-09-07-man101-v8w-bogus-call-gate.md) |
| 2026-09-07 | MAN-100 | [MAN-100: cross-candidate variant arbitration and a message-aware repetition gate](2026-09-07-man100-variant-arbitration.md) |
| 2026-09-07 | MAN-89 | [MAN-89: `station_callsign` accepts RBN's `CALL-N-#` per-band SSID](2026-09-07-man-89-station-callsign-ssid-grammar.md) |
| 2026-09-06 | MAN-88 | [MAN-88: fixed-column AK1A telnet layout, plus a CW-Skimmer-native variant](2026-09-06-man88-ak1a-column-layout.md) |
| 2026-09-06 | — | [Broad UI/UX/market review: architectural decisions](2026-09-06-broad-review-decisions.md) |
| 2026-09-05 | MAN-65 | [MAN-65: release pipeline deferred review findings (PR #78)](2026-09-05-man65-release-pipeline-hardening.md) |
| 2026-09-05 | MAN-34 | [`AudioIqSource` gains an operator-supplied RF reference (MAN-34)](2026-09-05-man-34-audio-rf-reference.md) |
| 2026-09-04 | MAN-64 | [MAN-64: metrics request-rate budget and `manta_source_health`'s failure transition](2026-09-04-man64-metrics-request-rate-and-source-health.md) |
| 2026-09-04 | MAN-56 | [MAN-56: input packet-loss/malformed counters on the Prometheus `/metrics` endpoint](2026-09-04-man56-input-health-metrics.md) |
| 2026-09-04 | MAN-45 | [MAN-45: unresolved-allowlisted-geography sentinels (spot_message.rs)](2026-09-04-man45-unresolved-geography-sentinels.md) |
| 2026-09-04 | MAN-45 | [MAN-45: per-client shutdown-drain deadline (telnet.rs, json_stream.rs, tasks.rs, main.rs)](2026-09-04-man45-per-client-drain-deadline.md) |
| 2026-09-04 | MAN-44 | [MAN-44: uplink health at a glance — `manta status` + `GET /status`](2026-09-04-man44-uplink-status-surface.md) |
| 2026-09-04 | MAN-36 | [MAN-36: exact Rust toolchain pin (`rust-toolchain.toml`)](2026-09-04-man36-exact-rust-toolchain-pin.md) |
| 2026-09-04 | MAN-27 | [MAN-27 fleet checkout rename (`skimmer` → `manta`, on-disk half)](2026-09-04-man27-fleet-checkout-rename.md) |
| 2026-09-03 | MAN-63 | [MAN-63: WebSocket handshake buffering size bound — verification](2026-09-03-man63-ws-handshake-size-bound-verification.md) |
| 2026-09-03 | MAN-62 | [MAN-62: bounded `SpotBus::occurrence_counts`](2026-09-03-man62-bounded-occurrence-counts.md) |
| 2026-09-03 | MAN-61 | [MAN-61: per-source-IP connection quota](2026-09-03-man61-per-ip-connection-quota.md) |
| 2026-09-03 | MAN-59 | [MAN-59: connection/rejection audit logging](2026-09-03-man59-connection-audit-logging.md) |
| 2026-09-03 | MAN-57 | [MAN-57: per-source-IP aggregate command/Ping rate budget](2026-09-03-man57-per-ip-rate-limit.md) |
| 2026-09-03 | MAN-55 | [MAN-55 Finding 1: OpenHPSDR Protocol 1 C&C tuning packet layout](2026-09-03-man55-hpsdr-cc-tuning-protocol.md) |
| 2026-09-03 | MAN-53 | [Cross-source spot-dedup algorithm spike (MAN-53)](2026-09-03-cross-source-dedup-algorithm-spike.md) |
| 2026-09-02 | MAN-23 | [MAN-23 structured threat model: manta's network-facing surfaces](2026-09-02-man23-threat-model.md) |
| 2026-09-02 | MAN-19 | [MAN-19: `DecoderEvent::TrackClosed` teardown invariant](2026-09-02-man19-track-closed-teardown-invariant.md) |
| 2026-09-02 | MAN-18 | [MAN-18: Raspberry Pi 4 CPU-budget gate — status and estimate](2026-09-02-man18-pi4-cpu-budget-gate.md) |
| 2026-09-02 | — | [OpenHPSDR/Hermes protocol wire-behavior spike](2026-09-02-hpsdr-hermes-protocol-spike.md) |
| 2026-09-01 | — | [Rename skimmer → manta](2026-09-01-rename-to-manta.md) |
| 2026-09-01 | — | [CW Skimmer / SkimSrv / Aggregator capability matrix](2026-09-01-legacy-capability-matrix.md) |
| 2026-08-07 | — | [PR review convergence policy](2026-08-07-pr-review-convergence-policy.md) |
| 2026-08-05 | — | [Dependabot Cargo single-package unlock failures](2026-08-05-dependabot-cargo-single-package-unlock.md) |
| 2026-07-25 | — | [PR auto-merge policy](2026-07-25-pr-auto-merge-policy.md) |
| 2026-07-25 | — | [M2 SoapySDR input implementation pins](2026-07-25-m2-soapysdr-input-pins.md) |
| 2026-07-25 | — | [M2 KiwiSDR input implementation pins](2026-07-25-m2-kiwisdr-input-pins.md) |
| 2026-07-24 | — | [M2 remaining sub-project 1 (V8/V8w pileup validation + CPU-budget bench) implementation pins](2026-07-24-m2-pileup-cpu-budget-pins.md) |
| 2026-07-19 | — | [M2 sub-project 2 (detector, track manager, decoder pool) implementation pins](2026-07-19-m2-detector-track-pool-pins.md) |
| 2026-07-18 | — | [M2 sub-project 1 (PFB channelizer) implementation pins](2026-07-18-m2-pfb-channelizer-pins.md) |
| 2026-07-18 | — | [CHAR_GAP_DITS threshold fix (high-WPM inter-character gap misclassification)](2026-07-18-char-gap-threshold-fix.md) |
| 2026-07-17 | — | [M1 implementation pins](2026-07-17-m1-implementation-pins.md) |
| 2026-07-11 | — | [M0 implementation pins](2026-07-11-m0-implementation-pins.md) |
