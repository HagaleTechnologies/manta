# MAN-180: `--soapy-gain` help warns that an SDRplay reads it as attenuation

Date: 2026-11-03. Ticket: MAN-180, narrowed on 2026-10-31 to help and
docs. Source: the [gain record](2026-09-09-soapy-gain-is-inverted-attenuation-scale.md)
and its MAN-226 correction. Baseline: main `33340f5`.

## Gap

On an SDRplay through SoapySDRPlay3, a higher `--soapy-gain` means more
IF and RF gain reduction; `0` is the most sensitive setting. manta passes
the value straight to SoapySDR's `set_gain`, so the direction is the
driver's. The five declarations (`run`/`listen`, `soak`, `doctor`,
`calibrate`, `check`) said only "Receiver gain, in dB. Omit to let the
device use AGC." The scaffold, `manta.example.toml`, `docs/CONFIG.md` and
the README said nothing about direction either.

## Decisions

- **The warning sits in the first paragraph.** clap prints only that
  paragraph under `-h`; the measured numbers go in the second.
- **Direction for SDRplay, numbers for the RSP1B.** IFGR and RFGR are
  SoapySDRPlay3's gain-reduction elements on every SDRplay model, so the
  direction is stated for the family. Only one RSP1B was measured, so the
  range (0 to 48, IF reduction at its maximum by 40, 46 to 48 failing to
  start the stream) is attributed to it.
- **No recommended value.** MAN-226 found the 60 s sweep does not
  establish an optimum. The help says to compare settings on a signal
  known to be present.
- **No ticket IDs, spec sections or paths in the help** (MAN-135). The
  docs link the gain record, as `docs/CONFIG.md` does elsewhere.

## Guard

`soapy_gain_help_warns_that_sdrplay_reads_it_as_attenuation`
(`crates/manta-cli/tests/cli.rs`) checks `-h` and `--help` for each
command that takes the flag, including `listen`, which is `run`'s alias
and shares its declaration. It needs the `soapy` feature, so only CI's
`test-soapy` job (not a required check) runs it.

## Out of scope

No runtime warning or read-back of the applied IFGR/RFGR, no claim about
other drivers, and no change to defaults or the scaffold's example
`30.0`. The original ticket's service restart, dial-shift re-run and wiki
retune are human-only or already done.
