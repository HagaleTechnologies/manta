# MAN-155: the related-projects page links other projects and makes no claims about them

Date: 2026-10-17. Ticket: MAN-155, scoped by the owner's answer to MAN-360
(2026-10-13).

## Problem

MAN-155 asked for a comparison page that stated what each of the closest
alternatives (SparkSDR, sdroxide, OpenWebRX+/csdr-skimmer, sdrainer) does and
does not do, for example "SparkSDR has no CW skimmer of its own", and for the
README's FT8/RTTY non-goal to be explained. Statements about other people's
software go stale as those projects ship, and a wrong one published by manta
reads as a misrepresentation of a peer project. A first draft of the page
(git history, `3da6df9:docs/COMPARISON.md`) had a capability table and a
section per project, each cell sourced from that project's own documentation
on the day it was written; nothing would keep those cells current.

## Decision

MAN-360 offered three options: (A) the full comparison page, with the owner
reviewing the text before merge; (B) a neutral related-projects list with
links only and no capability claims; (C) cancel MAN-155. The owner chose B.

- `docs/COMPARISON.md` names each project and links its own home page and
  documentation. It says that manta makes no capability claims about them and
  sends the reader to each project's own source. What manta says about itself
  stays in the README's Status and Non-goals sections.
- Links go to each project's canonical upstream. OpenWebRX+ links
  `luarvique/openwebrx` and its documentation at `fms.komkon.org/OWRX`, not
  the `0xAF/openwebrxplus` fork, which describes itself as unofficial and
  names `luarvique/openwebrx` as upstream.
- The README's `## Why` and `## Related projects` sections and the docs index
  link the page.
- The FT8/RTTY non-goal explanation (the ticket's scenario 2) is out of scope
  here: the 2026-10-13 grooming moved RTTY and FT8 into scope under MAN-321
  and MAN-323, and the README's Non-goals section is unchanged.

`related_projects_page_links_each_alternative_without_capability_claims` in
`crates/manta-cli/tests/docs_consistency.rs` pins the four names, the five
upstream links, the neutrality statement, the absence of a table and the
three inbound links.

## What this rules out

- A capability table or per-project feature summary on this page. Adding one
  means reopening MAN-360 with the owner, not a docs edit.
- Linking a fork or mirror in place of a project's own upstream.
