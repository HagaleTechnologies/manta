---
id: watterson-dependency
title: What will bite you about depending on coppa's Watterson HF fading model?
kind: gotcha
status: current
maintainer: agent
sources:
  - CLAUDE.md
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - golden-vector-freeze
  - coppa-reuse
---
skimmer quotes decode accuracy *under Watterson CCIR-poor fading*, and reuses that model from the sibling coppa repo (`coppa-channel`) rather than rebuilding it — but that dependency was a moving target for most of design phase, and the reuse claim in the docs was corrected twice before it settled. If you treat coppa's Watterson as a stable given, you will generate golden vectors against a buggy or nonexistent model. As of coppa main 2026-07-07 it is fixed and usable; the standing rule is to **pin the exact coppa commit** whenever fading vectors are frozen. See [[golden-vector-freeze]].

## Symptom

Fading-impaired golden vectors (V4/V5/V8w) that decode differently than expected, or a spec that assumes a Watterson API/convention coppa does not actually provide. The git history shows the thrash: the reuse claim went "fading does not exist yet" → "exists but has two bugs" → "fixes landed, freeze unblocked".

## Cause and workaround

Two real bugs existed in coppa's model (Doppler spread too fast vs ITU-R F.1487; per-block SNR renormalization erasing fading dynamics) — both fixed in coppa main on 2026-07-07. The specific coppa commits and the verified conventions (2σ Doppler-spread width, ensemble-only normalization, and the still-live SNR-in-2500 Hz vs 3 kHz reconciliation) are recorded in skimmer's CLAUDE.md "Key constraints" — read it before generating any fading vector, and pin the coppa commit you generated against. Cross-repo details live in coppa's `watterson.rs` and the SPEC-watterson audit (plain-text references; not linkable from this wiki).
