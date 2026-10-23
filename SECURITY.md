# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately via
[GitHub's private vulnerability reporting](https://github.com/HagaleTechnologies/manta/security/advisories/new)
rather than opening a public issue.

If that's unavailable, email tony@hagale.net with details and, if possible, a
reproduction. Expect an initial response within a few days.

## Scope

`manta` is receive-only SDR signal-processing software: it decodes CW from
IQ/audio input — WAV files and paced replay, rig audio, and SDR/network
receivers (KiwiSDR, HPSDR and ka9q-radio multicast ship in every build;
local-hardware SoapySDR behind the opt-in `soapy` build feature) — and serves
its output over these wire surfaces: a telnet DX-cluster server and a JSON
Lines/WebSocket stream, both public-bind by default; a loopback-by-default
Prometheus metrics endpoint; an opt-in, loopback-by-default live-state
stream; and an outbound RBN uplink that is dry-run by default. Relevant
reports include memory-safety issues, panics/DoS reachable from untrusted
input (malformed WAV/IQ files, malformed telnet/JSON client input, hostile
network peers on the SDR inputs, the `[rbn_compare]` feed or the plain-HTTP
cty.dat/MASTER.SCP refresh), and dependency vulnerabilities affecting this
project's usage. The recorded threat model and its open follow-ups live in
`docs/DECISIONS/2026-09-02-man23-threat-model.md`; network exposure is
covered in `docs/RUNBOOKS/network-exposure.md`.
