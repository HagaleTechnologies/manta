---
name: Hardware compatibility report
about: Share results from an SDR or audio device.
---

## Hardware and connection

- Manufacturer and model:
- Firmware and driver versions:
- Connection: USB, network or audio:
- Date tested:

## Host and manta version

- Output of `manta --version`:
- Operating system and CPU architecture:

## Source and configuration

- Backend: audio, KiwiSDR, HPSDR or SoapySDR:
- Sample rate and tuned frequency, with units:
- Command and relevant configuration:

Remove credentials, serial numbers and private host details before posting.

## What worked?

Describe what you verified: device discovery, samples arriving, decoded text
or validated spots. Include the test duration. Say if nothing worked.

## What failed?

Describe errors, dropouts or limitations. Say if none were observed.

## Diagnostics and reproduction steps

Include available logs and the steps another person could repeat. You may
include output from `manta devices` or `manta check` for your source. State
which checks you did not run; a successful sample check alone does not prove
that CW decoding works.
