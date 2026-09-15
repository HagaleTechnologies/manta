---
id: golden-vector-freeze
title: Why must the golden test vectors pin an exact coppa commit?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#7-golden-test-vectors-m0-m1-acceptance
  - CLAUDE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - watterson-dependency
  - coppa-reuse
---
The V1–V10 golden vectors are the M0–M2 acceptance gates, and the fading ones (V4, V5, V8w) are generated through coppa's Watterson model — so their expected outputs are only reproducible if the exact coppa commit used to generate them is pinned in the fixture manifest. The decision: **freeze fading vectors against a named coppa commit, and do not regenerate them casually**, because the impairment model is an external dependency that has already changed under the spec once. The authoritative vector table and generation recipe are in SPEC §7 — this page is the rationale pointer.

## Digest

The freeze was *blocked* through most of design phase because coppa's Watterson had two bugs (see [[watterson-dependency]]); regenerating vectors against a buggy model would have baked wrong ground truth into CI. It was **unblocked on 2026-07-07** once the coppa fixes landed. The standing rules that fell out of this:

- Fading vectors record the exact coppa commit they were generated against (fixture manifest), so a coppa change can never silently invalidate CI.
- SNR is quoted in 2500 Hz for these vectors; the 2500 Hz vs benchmark-3 kHz reconciliation is still live and tracked in CLAUDE.md.
- Watterson uses the streaming `WattersonChannel` API, not the deprecated one-shot helper (SPEC §7 note).

For the exact vectors, seeds, `fs`, and pass criteria (e.g. V8w: 0 bogus, 0 cross-channel ghost decodes), read SPEC §7 — never restated here.

## Gotcha: `#[ignore]` is not the only way a golden-vector assertion can be inert

MAN-101 found that V8w's three SPEC §7 criteria lived in one `#[ignore]`d test function, ordered so the (separately-tracked, failing) CER `assert!` ran first. A Rust `assert!` panics and unwinds the whole function, so the "0 bogus callsigns" and "0 ghost decodes" checks after it never executed even on a run with `--ignored` passed — removing `#[ignore]` alone would not have made them run. Any multi-assertion golden-vector test has this hazard: an earlier failing assertion silently disables every check written after it in the same function, independent of `#[ignore]`. See `docs/DECISIONS/2026-09-07-man101-v8w-bogus-call-gate.md` for the fix (split into an independent, always-run test) and its CI-cost tradeoff.
