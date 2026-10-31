# Launching a callable-contract PV run

The viewer supports two GOAL formats. **Independent goals (GOAL.json)** runs
native modeling with one target per explicit ID; see
[the modeling launcher guide](examples/pv_modeling/README.md). **Function
contracts (GOAL.md)** uses the callable-contract procedure below. Both accept
unmodified source archives. See [GOAL authoring](PV_GOAL_AUTHORING.md) for
the input structures.


Use [PV_GOAL_AUTHORING.md](PV_GOAL_AUTHORING.md) to write and audit the goal
document, and [PV_SEED_AUTHORING.md](PV_SEED_AUTHORING.md) for seed contents and
trust boundaries. This runbook covers the callable-contract upload route;
native modeling uses the structured goal collection described in the authoring
guide.

The upload rule is exact:

> Upload the upstream source, unmodified: the module or crate you want verified,
> plus any sibling files in the same crate that its `use` lines import. Never
> edit, never add a manifest.

Prepare `GOAL.md` with backticked level-three headings that name functions in
the target upload. Put the upstream module directory or crate in the required
target archive. Put any imported sibling files in the optional supporting
archive. Do not include the example's `PROVENANCE.md`.

For the repository example, build both archives deterministically from the
workspace root:

```sh
mkdir -p scratch/upload-model/example-archives
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -czf scratch/upload-model/example-archives/target.tar.gz \
  -C examples/pv_dec2flt/source/target dec2flt
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
  -czf scratch/upload-model/example-archives/support.tar.gz \
  -C examples/pv_dec2flt/source/supporting traits.rs
```

Open the private viewer launcher, select the example `GOAL.md`, then select
`target.tar.gz` in **Target source** and `support.tar.gz` in **Supporting upstream
files**. The import list beneath the slots is a mechanical convenience only;
it is never a gate or an adaptation-routing input.

Before launch, review the shared run-settings panel. It leads with a launch
preset (astra-guided sol, all sol, sol-guided luna, all luna) and exposes model
and effort for every in-run role and for the Phase-0 adapting worker,
seam-repair auditor, and source-correspondence lane behind "show per-role
settings", plus an optional git remote. Phase-0
audit lanes may share the adapting worker's provider/model pair;
`phase0.allow_same_model_lanes` is on in the template and the launcher, and
genesis records it.

The equivalent command-line launch is:

```sh
bash scripts/pv_create_run.sh start <slug> \
  --goal examples/pv_dec2flt/prose/GOAL.md \
  --target scratch/upload-model/example-archives/target.tar.gz \
  --support scratch/upload-model/example-archives/support.tar.gz
```

To end a creation that is still running, use

```sh
bash scripts/pv_create_run.sh stop <slug>
```

It kills the create session and every Phase-0 lane session by their exact
names, records the job as failed with `stopped by operator`, and refuses a job
that already completed. The viewer exposes the same verb while a job is
creating.

Omit `--support` when there are no supporting files. Intake validates both
archives independently, refuses overlapping relative paths, and records the
target/supporting partition manifest next to the canonical union-tree manifest.
The run is offline; missing imported code must be authored as a fully ledgered
and audited shim during Phase 0.

## When a worker delivery is refused

The worker's response may use any JSON formatting; the bridge canonicalizes it
before sealing. A delivery the bridge or kernel refuses (unknown fields, an
`affected_targets` id outside the request's `goal_targets`, a ledger that does
not account for the tree delta) is rolled back and the same request is
re-dispatched with the refusal reason appended to the prompt, at most three
times, after which the job fails and the reason is in `create.log`. Kernel
refusals of anything other than a worker revision stop the job at once.

## Per-burst logs

Every burst of a lane reuses one set of log names, so the next burst of the
same lane truncates the transcript of the one before it. When a burst ends, its
set under `<attempt>/agent-output/logs/` is copied to
`logs/history/<counter>-<request id>/`, and a delivery the bridge refuses adds
`refused-draft.json` (the reason and the delivered bytes) to that same set. The
live `.started`/`.exit` markers stay at their usual path for the burst that is
running. The last 200 sets are kept.

## Revision history in the worker prompt

Every worker burst runs on a fresh provider thread, so the bridge appends a
revision-history note after the kernel contract bytes: one item per accepted
generation that changed the ledger, listing the changed, replaced and dropped
entries with their inserted text, and the first failing stage's diagnostic
afterwards. The note is derived from kernel state alone, is byte-identical for
identical state, never alters the contract bytes, and reaches the adapting
worker only, never a reviewer or audit lane. It is bounded at the twelve most
recent changed generations, 32768 bytes overall with the oldest generations
dropped first, 1200 bytes of inserted text per entry, and twelve lines or 2000
bytes of diagnostic per generation.

## How a quoted diagnostic is matched again

At `check_complete` the kernel re-runs the checker with the entry omitted and
requires the quoted diagnostic back. The quoted line is compared together with
the file its location line names, and the location's line and column numbers
are ignored, so an entry stays bound to the one diagnostic instance it cites
while later entries move that instance within its file. A quote whose location
line the kernel cannot read is compared by its text alone.
