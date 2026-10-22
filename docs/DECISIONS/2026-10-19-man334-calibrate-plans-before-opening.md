# MAN-334: calibrate plans its references before it opens the receiver

Date: 2026-10-19. Ticket: MAN-334, a deferred Codex P2 from MAN-127's PR #245
(round 2 and later, ticketed under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`).

## Context

MAN-127 Decision 11 lists "no usable reference in the passband" among the
refusals `manta calibrate` makes before any receiver is opened. The shipped
`calibrate_cmd::run` opened the receiver first and only then called
`plan_references` on what the opened source reported. Two consequences,
both reproduced on `b1174a0`:

- An offline KiwiSDR configured at 7.030 MHz reported
  `error: TCP connect to 127.0.0.1:1: Connection refused (os error 111)`
  instead of the retune hint. A black-holed host waited out kiwi.rs's
  connect timeout first.
- An online one got the retune hint only after manta had connected, done
  the WebSocket upgrade and the `SET auth`/`sample_rate` handshake: the
  repo's fake KiwiSDR saw a session established.

HPSDR (`send initial HPSDR C&C tuning frames: … Connection refused`) and a
sound card (`no audio input matching "NoSuchCard"`) behaved the same way.

## Decisions

1. **Plan twice; the second plan is measured.** `calibrate` runs
   `plan_references` on the centre and passband the configuration gives,
   before `LiveSourceSpec::open`, and again on what the opened source
   reports (`calibrate_cmd::open_planned`). The second plan is the one
   printed in `measuring …` and measured. It catches values the receiver
   negotiated: a SoapySDR driver may round the centre or rate. The second
   call is the call `run` always made, so a recording's report stays
   byte-identical.
2. **Same function, same messages.** The preflight is the same
   `plan_references` call, with no new message and no typed error. Every
   refusal it makes moves ahead of receiver I/O: no usable reference (the
   retune hint), `--reference-hz` outside the passband, and a
   `--search-ppm` too narrow.
3. **Unknown means no preflight, not a refusal.**
   `LiveSourceSpec::configured_center_and_passband` returns `None` when only
   opening can tell (a ka9q-radio channel, a recording) or when `open`
   itself would refuse (a missing frequency or rate, an HPSDR rate or
   frequency that `HpsdrConfig::validate` refuses before any I/O, a
   `--capture-rate-hz` that `DecimatingSource::new` refuses). `open`
   reports those errors as before. For KiwiSDR, HPSDR and a sound card the
   configured values are exactly what the opened source reports, so the
   preflight refuses nothing that opening would not.
4. **SoapySDR on the requested values.** The preflight uses
   `--soapy-freq-hz`/`--soapy-rate-hz` (or their `[input]` keys), as the
   ticket asks. A driver may round either: a requested window that holds no
   reference is refused even when the driver would have widened it, which
   the retune hint answers (it leaves at least 1 kHz of margin). A driver
   that narrows the window is caught by the second plan.

What is known before opening, per source:

| Source | Centre | Rate | Passband | Before opening |
|---|---|---|---|---|
| KiwiSDR | `freq_hz` | 96 kS/s (`kiwi::OUTPUT_RATE_HZ`) | ±5 kHz (`kiwi::IQ_PASSBAND_HZ`) | exact |
| HPSDR | `freq_hz` | `rate_hz` | Nyquist | exact |
| SoapySDR | `freq_hz` | `rate_hz` | Nyquist | requested; re-planned on the negotiated values |
| Sound card | `--dial-freq-hz` | 48 kHz | 300-3000 Hz | exact |
| ka9q-radio | radiod's status | radiod's status | radiod's status | nothing; planned once joined |
| Recording | header and sidecar | header | header | not planned before opening; planned once opened |

Decimation by `--capture-rate-hz`/`input.capture_rate_hz` narrows the
passband with `DecimatingSource::passband_for`, which shares its rate check
and its clip with `DecimatingSource::new` and `rf_passband_hz`. A
`--dial-freq-hz` override replaces the centre, as `open` does.

## The rule for future sources

`LiveSourceSpec::configured_center_and_passband` mirrors
`LiveSourceSpec::open`: the source's own values, then `maybe_decimate`
(through the shared `decimation_target`), then the `--dial-freq-hz`
override. A change to how `open` composes a source must change it too. A
source whose values are only known once open returns `None`.

## Not done

- A ka9q-radio channel's retune hint still names `--tune-hz`, which
  calibrate refuses for ka9q. That predates this ticket and is a follow-up.
- Recordings are not planned before opening: opening one holds no receiver
  and is local and deterministic.
- `doctor`, `check`, `run` and `soak` plan no references and are unchanged.

This completes MAN-127 Decision 11; that record is not rewritten.
