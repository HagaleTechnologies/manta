# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately via
[GitHub's private vulnerability reporting](https://github.com/HagaleTechnologies/manta/security/advisories/new)
rather than opening a public issue.

If that's unavailable, email tony@hagale.net with details and, if possible, a
reproduction. Expect an initial response within a few days.

## Scope

`manta` is receive-only SDR signal-processing software: it decodes CW from
IQ/audio input (files, audio devices, and — once implemented — SDR/network
sources) and emits spots over a local telnet/JSON server. Relevant reports
include memory-safety issues, panics/DoS reachable from untrusted input
(malformed WAV/IQ files, malformed telnet/JSON client input), and dependency
vulnerabilities affecting this project's usage.
