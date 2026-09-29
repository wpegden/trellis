# Closure identity diagnosis and repair

Use the released script and an **explicit, absolute, pinned runtime CLI**. The
binary embeds the exact collector bytes; runtime checkout location is no longer
an identity input. Keep the supervisor and every source/state writer stopped
while applying a repair. Planning is read-only and does not contact the checker.

Set `RUN_ROOT` to the affected runtime directory, `REPAIR` to the installed
`scripts/migrate_local_closure_records`, and `NEW_CLI` to the pinned new binary.
The readiness diagnostic supplies the complete command with both paths.

Diagnosis one-liner:

```sh
"$REPAIR" "$RUN_ROOT" --mode identity-only --plan --runtime-cli "$NEW_CLI"
```

Read `direct_stale`, `expanded_stale`, and each owner's tier and findings.
The normal plan checks identity and policy; it does not compare owner source
or audit artifact contents. Source findings are classified separately during
full admission and explicit repair. An empty plan is not a full source/artifact
validation. A missing required identity is an error, including
an unresolved Lean or Lake executable. An absent Lean manifest is allowed;
unreadable manifests are errors. Policy growth alone does not invalidate a record.

If the plan is empty, skip apply. For a nonempty reviewed repair set, stop the
writers and repair with:

```sh
"$REPAIR" "$RUN_ROOT" --mode identity-only --apply --runtime-cli "$NEW_CLI"
```

The wrapper reads selected non-secret settings from `launch_env.json` as JSON,
mints and registers an operation token through `trellis.runtime.bridge`, and
removes only that token afterward. `TRELLIS_CHECKER_SOCKET` defaults to
`$RUN_ROOT/sockets/checker.sock`. Its checker must be running the new identity
protocol. The command prints its plan before acquiring repair ownership,
collects fresh evidence into a candidate state, checks semantic stability,
source/policy/identity generations, dependency roots and matching rollback
tiers, then publishes. A failed probe never publishes a removed record.

This mode uses an existing replay-attested artifact epoch. It refuses cold
materialization, changed artifacts, changed semantic projections, and historical
tiers that cannot receive the new records in their paired source view. Resolve
those through the ordinary maintenance/revalidation procedure in an isolated
rehearsal. There is no automatic repair inside a paired restore.

A durable `closure-identity-publication/commit.json` means publication was
committed but interrupted. Normal runtime loading finishes the same complete
candidate, its record mirrors and replay marker before loading state. Keep the
journal and candidate together; do not delete a pending journal to force a
resume. Failure before that commit decision leaves the old authoritative state.

Verification:

```sh
"$REPAIR" "$RUN_ROOT" --mode identity-only --plan --runtime-cli "$NEW_CLI"
```

Require an empty stale set in every activatable tier. A no-op plan performs no
probes or writes. Successful repair validates full admission of each new record
and all tier roots. Preserve pending requests/responses and the active-worker
base; do not clear them or discard source edits to get a green plan. Resume only
on the pinned binary after the guard is ready. Repair-required exits have status
`closure_identity_repair_required`, exit code 3, and no runtime-error breadcrumb.

For immutable incident JSON with a captured identity manifest:

```sh
"$REPAIR" COPY_DIRECTORY --plan --runtime-cli "$NEW_CLI" \
  --state-file COPY_DIRECTORY/protocol_state.pre_repair.json \
  --identity-manifest CAPTURED_IDENTITY.json
```

This audit does not load runtime metadata, follow archived repository paths,
execute a prover, contact a checker, or migrate state. It explicitly reports
source, policy and artifact disk admission as unchecked. The supplied manifest
is audit input, never authorization to issue evidence; apply rejects audit
input overrides.

The old source-baseline migration remains available only through the explicitly
named Rust action mode `legacy-baseline`. It retains its historical incremental
semantics and is **not** the identity repair command.

## Deployment order

Use two separate maintenance windows. First update the checker Python and any
materialized clients, retaining the old pinned kernel for the supervisor,
acceptance subprocesses and sidecar clients. Restart only the checker in that
window using the normal approved deployment procedure. The unchanged collector
bytes and optional response identity envelope do not themselves require record
migration. Exercise legacy request shapes, including source-policy scans,
principal probes and Preamble module-owner probes, and concurrent first use.

For the later kernel window, stop all source/state writers (including bursts and
sidecar writers), keep the compatible checker available, and take a coherent
backup. Rehearse on a complete isolated copy. Invoke the new pinned CLI explicitly
for planning before changing the normal launch selection. An unchanged
post-repair Goldberg–Seymour state should have empty sets: skip apply. Only the
pre-repair generation with exactly `{Preamble}` direct and expanded needs the
one-record repair above. Investigate unexpected findings before proceeding.

After verifying identity obligations and source/artifact/base pairing, select the
new pinned kernel consistently for the supervisor and every acceptance/sidecar
subprocess. Preserve the actual pending request and response, and retain the
acceptance-digest and normalization checks. Confirm the pending restore path in
rehearsal before production resume. These are operator deployment steps, not
operations performed by the implementation or its offline tests.
