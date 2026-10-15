---
id: dependabot-cargo-unlock
title: Why is the weekly Cargo Dependabot run red with `unknown_error` / `null`?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md
  - scripts/check-dependabot-cargo-unlock.sh
  - .github/dependabot.yml
verified:
  commit: d286147
  date: 2026-10-13
links: []
---
# Why is the weekly Cargo Dependabot run red with `unknown_error` / `null`?

This can mean Cargo's single-package unlock cannot reach an otherwise valid
dependency update. Run `scripts/check-dependabot-cargo-unlock.sh` to identify
the affected crate and remedy version. The normative mechanism, response, and
verification record are in
[`docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md`](../../docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md);
the incident investigation is the separate thoughts-repository document
`thoughts/shared/research/2026-08-05-ski-1.md`.
