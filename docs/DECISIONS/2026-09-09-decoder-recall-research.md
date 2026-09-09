# Manta decoder improvement investigations

Research handoff in progress. This document will contain twenty stack-ranked investigations of the complete input-to-spot pipeline, with evidence, independent algorithm designs, implementation boundaries, experiments, and acceptance criteria.

Baseline: Manta `45cf11444979e9c1d48aa6f90164aea7b7c3b695`. Concurrent decoder PRs are dependencies to inspect, not implementation work claimed by this document. Scope is research and documentation only.

The intended outcome is higher callsign and transmitter-attribution recall at a measured false-spot rate, demonstrated on held-out real recordings and synthetic stress cases. The reported approximately 30% recall remains to be traced to its exact dataset, denominator, and revision.
