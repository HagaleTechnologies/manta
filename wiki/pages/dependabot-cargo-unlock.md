# Why is the weekly Cargo Dependabot run red with `unknown_error` / `null`?

This can mean Cargo's single-package unlock cannot reach an otherwise valid
dependency update. Run `scripts/check-dependabot-cargo-unlock.sh` to identify
the affected crate and remedy version. The normative mechanism, response, and
verification record are in
[`docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md`](../../docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md);
the incident investigation is in
[`thoughts/shared/research/2026-08-05-ski-1.md`](../../thoughts/shared/research/2026-08-05-ski-1.md).
