# MAN-482: PR #469 fixed the skirt-wrap residual; the spec and Gate docs now match the rise rules

MAN-109's 2026-11-01 nonce-3 recheck cited the clamped ±2 scan at
`crates/manta-dsp/src/floor.rs:393`. PR #469's remediate round closed it with
`rem_euclid` wrapping and `two_look_skirt_check_wraps_across_dc`, covering a
strong carrier at channel 1 or n-2 and its skirt across DC. See the
[MAN-109 repair record](2026-10-30-man109-dit-scale-two-look-detector.md#repair-after-validation).

On base `5c3c31502b300b477e2d733f4afad288b6cc2cc0`,
`cargo test -p manta-dsp --lib` passed with 204 passed, 0 failed, 1 ignored,
including that regression test. `git diff d4f9220ca5e8a154e7c5d151b089d349c8456cf3 HEAD -- crates/manta-dsp/src/floor.rs`
was empty before these doc edits, confirming the file still matched PR #469.

SPEC §2.4 now names the HANG-to-ACTIVE EMA threshold `max(on_snr_db, 12 dB)`.
`Gate::update` now documents EMA OR window rise and the wrapping skirt guard
on the window path. Executable code and decoder output are unchanged.

Follow-up candidates remain outside this ticket: the same-hop arbitration
seam at `crates/manta-engine/src/track.rs:3773`, and detector clamps at
`crates/manta-dsp/src/fsk.rs:348,457-458` and
`crates/manta-dsp/src/comod.rs:580-581,592-593,642-643`.
