# MAN-285: status JSON preserves every served field

## Context

PR #95's round-3 review deferred the loss of additive `/status` fields to
MAN-285. Schema version 1 allows additive fields, but `manta status --json`
re-serialized `StatusDoc`, which discarded anything this build did not know.

## Decision

1. Keep the parsed `serde_json::Value` alongside the typed document in
   `StatusReply`. Print the value for `--json`, including nested unknown
   fields. Object keys are alphabetical with the workspace's existing
   serde_json configuration. The changelog and uplink runbook document this.
2. Deserialize `StatusDoc` from a reference to that same value. The typed
   document still drives the human summary and exit code. Both forms agree
   on duplicate keys because they share one parse, where the last key wins.
3. Share the existing DEL/C1 escaping loop between `StatusDoc::to_json`
   and `value_to_json`. Unknown strings receive the same terminal protection
   as known fields, and the output remains valid JSON.

Echoing the body text was rejected because it would preserve peer-supplied
whitespace, including carriage returns, and duplicate keys. A flattened
top-level wrapper would still lose nested additions. Enabling serde_json's
`preserve_order` would change key ordering for other commands through Cargo
feature unification, so the dependency configuration stays as it is.

## Evidence

- `parse_status_doc_keeps_fields_this_build_does_not_declare` checks top-level
  `foo`, nested `uplink_extra`, the typed document, and unhealthy exit code 1.
- `value_to_json_escapes_del_and_c1_in_a_field_status_doc_does_not_declare`
  checks escaped control characters and a lossless value round trip.
- `status_json_prints_a_field_this_build_does_not_know` checks the real
  binary's stdout, empty stderr, and exit code against a fake daemon.
- `status_summary_is_unchanged_by_a_field_this_build_does_not_know` compares
  real binary output with and without additions, and with `render_human`.

## Not done

The schema version, daemon response, human renderer, health command, and
exit-code rules are unchanged. JSON output still has no trailing newline.
