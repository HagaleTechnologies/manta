# First live SDRplay RSP1B run: findings

manta's SoapySDR path (`docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md`)
was implemented and unit-tested against `type=null`/`driver=rtlsdr`-absent
error paths only -- explicitly documented there as "genuinely untested"
against real RF hardware. This is the first real run, against a real
SDRplay RSP1B (hwVer 6, serial 2402041760) over USB via
`manta listen --soapy-driver "driver=sdrplay" ...`, on a Mac Mini with no
prior SoapySDR/SDRplay tooling installed.

## Setup, for the record

Nothing SDR-related was installed on this machine beforehand except the
SDRplay API itself (3.15.1, already present with its `sdrplay_apiService`
running). To get `manta listen --soapy-driver` working:

1. `brew install soapysdr cmake pkg-config`.
2. Built `pothosware/SoapySDRPlay3` from source (no Homebrew formula exists
   for it) against the installed SDRplay API, with
   `-DCMAKE_PREFIX_PATH=/opt/homebrew -DCMAKE_INSTALL_PREFIX=/opt/homebrew
   -DLIBSDRPLAY_INCLUDE_DIRS=/usr/local/include
   -DLIBSDRPLAY_LIBRARIES=/usr/local/lib/libsdrplay_api.dylib`.
3. **Required a manual fixup**: the built `libsdrPlaySupport.so` links
   `@rpath/libsdrplay_api.so.3` but the build sets no matching rpath, so
   `SoapySDRUtil --find` failed to `dlopen` it
   (`Library not loaded: @rpath/libsdrplay_api.so.3`) until patched with
   `install_name_tool -change @rpath/libsdrplay_api.so.3
   /usr/local/lib/libsdrplay_api.so.3 libsdrPlaySupport.so`. Not a manta
   issue -- a SoapySDRPlay3 build-system gap on macOS -- but anyone
   repeating this setup will hit it too.
4. `cargo build --release -p manta-cli --features soapy` built clean with
   no code changes needed beyond the fix below.

With that module installed, `SoapySDRUtil --find` correctly reported the
device (`driver = sdrplay`, `label = SDRplay Dev0 RSP1B 2402041760`).

## Finding 1 (fixed): `--soapy-gain` couldn't activate the stream at all

`SoapySdrIqSource::open` (`crates/manta-input/src/soapy.rs`) called
`device.set_gain(Rx, 0, db)` directly when `gain_db` was `Some`, without
first disabling gain mode. On this real device, AGC defaults **on**
at device-open time, and leaving it on while forcing a gain value doesn't
just silently ignore the request (as the `type=null`-only test coverage
implied) -- `stream.activate()` fails outright with `sdrplay_api_Fail`
(surfaced as SoapySDR `NotSupported`). Every `manta listen --soapy-gain
<n>` invocation against the RSP1B failed before this fix, regardless of
`<n>`.

Fix: call `device.set_gain_mode(Rx, 0, false)` first whenever
`has_gain_mode` is true and an explicit gain was requested (mirroring the
existing AGC-enable branch for the `None` case). Confirmed against the
real RSP1B: `--soapy-gain 40` on 20m now activates and streams cleanly.
`type=null` has no gain support at all (`has_gain_mode` false there), so
this path stays untestable in CI -- same real-hardware-only gap the
original pins doc already flagged for the rest of the SoapySDR surface.

## Finding 2 (upstream, not fixed): the advertised gain range's top end is invalid on this device

Isolated with a standalone probe (`soapysdr` crate directly, bypassing
manta) sweeping `set_gain(Rx, 0, <value>)` after disabling AGC, at
14.025 MHz: **0-45 dB all activate; 46-48 dB all fail** the same
`sdrplay_api_Fail`. `device.gain_range()` reports `[0, 48]` regardless of
band. This is SoapySDRPlay3 distributing a single overall gain value
across its two named elements (`IFGR` range `[20,59]`, `RFGR`/LNA-state
range `[0,9]`) without validating that the resulting LNA state is legal
for the current band on this specific hardware revision -- a known class
of issue with RSP gain modeling, not something introduced by manta's
Rust layer. Named-element gain (`set_gain_element("IFGR", ...)` /
`("RFGR", ...)`) worked fine at values the generic path rejected.

Not fixed here: reworking `--soapy-gain` from a single dB value to
per-element control is a real CLI/behavior change, out of scope for a
same-day bug fix. Noted so nobody re-discovers this by surprise: **avoid
the top ~3 dB of the advertised `--soapy-gain` range on a real RSP1B**
(this run used 40; stay at or below ~45).

## Finding 3 (not a bug, still unresolved): no confirmed off-air CW copy yet

With the gain fix in place, `manta listen` ran cleanly for multiple
sessions on both 40m (7030 kHz dial) and 20m (14025 kHz dial), AGC and
fixed gain alike, 192 kS/s passband. The decode pipeline never crashed
and correctly emitted the full `DecoderEvent` stream end-to-end against
real hardware for the first time (`CharDecoded`/`WordBoundary`/
`SpeedUpdate`/`TrackMeta`/`TrackClosed`). But every `TrackMeta.snr_2500_db`
across every run was negative (best case -0.9 dB, typically averaging
around -6 to -7 dB) -- consistent with noise-floor chatter, not real
carriers -- and zero validated `Spot`s were ever emitted. Whether that's
antenna/feedline, band conditions at the time, or something else is not
diagnosable from software alone; manta has no built-in RF-level/antenna
diagnostic today (no signal-strength sensor is exposed by
`driver=sdrplay`, and `SoapySDRUtil` has none either). This remains the
same kind of outstanding manual step the M1 W1AW live-copy run and M2's
Pi4/soak legs already are -- see CLAUDE.md Status.

## Takeaway

The SoapySDR/SDRplay code path itself is now confirmed working end-to-end
against real hardware (a real gap the previous pins doc explicitly could
not close). What's still unconfirmed is a real decoded, validated
off-air spot -- that needs either better propagation/antenna conditions
or a known strong reference signal to test against, not more code.
