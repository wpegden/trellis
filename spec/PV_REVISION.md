# PV revision after formalization

The optional PV revision loop turns completed refutations into proposed source
or requirement changes. The previous program, requirements, meanings, proofs,
and approvals remain evidence about the previous generation. They are never
rewritten to make that generation appear successful.

## Boundary and sequence

Set `pv_tablet.revision.enabled` to `true` before initializing a native PV run.
The native PV example enables it. Missing configuration defaults to false so
historical runs and event logs retain their behavior.

When the ordinary kernel completion predicate succeeds in ProofFormalization,
the kernel inspects the live Decide targets. If any are formally refuted, it
commits a `pv_revision_handoff` and returns `PvRevisionReady` before Cleanup.
The handoff requires current closure evidence. Source-bug witness eligibility
is independent: a literal Lean refutation remains relevant even when its
witness does not establish a reachable source bug.

The parked kernel rejects further proof events, sidecar applications, and
startup reconciliation. A lightweight `pv_revision_ready.json` projects the
committed handoff for the controller and viewer. The native controller then
uses `runtime/pv-revision` for the following workflow:

1. An audit covers every refuted goal exactly once. It supplies replacement
   natural-language requirements, explicit retirements and any surviving
   conditional obligations, or source repair tasks. A modeling mismatch stops
   for operator discussion rather than assuming which meaning should change.
   The audit may also propose changes to other existing goals affected by the
   remedy, with a reason for each. Unmentioned goals stay unchanged. These
   proposals do not classify previously proved goals as refuted or carry their
   approvals forward; the same aggregate review and human gate apply.
2. A reviewer assigns the authorized repair tasks to workers. Each worker
   receives a separate writable source copy. Rejected copies remain evidence;
   they do not become the next accepted source.
3. Independent checks and a scope agent assess each attempted repair against
   its precise authorization. Optional path restrictions are also enforced by
   the controller. Failed or exhausted work cannot complete a task.
4. All authorized checks run again on the final candidate. A regression
   reopens the affected task. An aggregate scope review assesses the combined
   source and requirement changes.
5. The human accepts, denies, or requests amendments to the exact review
   package. Goal-only modifications and retirements require this gate too.
6. Acceptance creates a distinct native preparation and runtime generation.
   It goes through setup, extraction, Modeling review, normal verifier lanes,
   argument development and formalization. If that generation still contains
   refuted goals, the loop repeats; otherwise ordinary Cleanup can complete.

Retiring every requirement produces `all_requirements_retired`, with an empty
active inventory explicitly recorded. It does not report that the original
requirements were proved.

## Source and evidence

Full regular-file source snapshots, identified by the existing canonical PV
source manifest, are authoritative. Unified patches are review aids. Binary
changes and deletions retain the original bytes in snapshots. The successor's
upstream manifest must equal the ratified source manifest; input filtering
cannot silently change the accepted program.

The controller checks native extraction provenance and compares preserved Lean
files with the kernel closure records' SHA256 identities. It retains the
completed protocol state and Tablet snapshot. Old proofs and extraction setup
are available to later agents as candidates and references.

Audit alternatives explain the choice between requirement changes and source
repairs. Worker alternatives explain implementation choices and distinguish
considered approaches from tested ones. Evidence includes task authorizations,
source identities, attempts, check outputs, scope decisions, and operator
recovery actions. A shared defect can explain multiple failed goals.

Task checks use the existing sealed source-checker sandbox, with separate
scratch copies and no network. `argv` is an argument array. `{source}` names
the tested copy and `{scratch}` writable scratch storage. A check can specify
`baseline_expected_exit` to reproduce the old behavior. `harness_paths` copies
listed candidate test files into `{harness}` for both baseline and candidate;
this allows new tests to exercise the old implementation without replacing it.
Check logs and receipts are hashed and revalidated before approval and launch.
Tests provide evidence and regression protection; they are not formal proofs.

The viewer offers a readable review, full JSON, patches, and a self-contained
ZIP with source snapshots and recorded evidence. Package identity binds the
entire candidate and its review evidence. A changed candidate or receipt makes
the prior decision stale. The ZIP omits runtime configuration, credentials,
tool installations, build caches, and unrelated runtime logs.

## Final archive and extraction history

The final campaign archive follows the linked generations, including earlier
preparation reopenings. Each generation retains its source, requirement
inventory, Lean statements, proof and disproof sources, protocol records and
recorded human decisions. The index distinguishes results for earlier source
versions from those for the final version. A replaced or retired requirement
remains in that history; retirement does not turn its refutation into a proof.
Revision packages retain the proposed changes, alternatives, failed attempts,
checks and scope assessments. Any existing final trust archive is included
unchanged, with its own identity and trust requirements.

Extraction history separates the upstream program, the source adapted for
extraction, the raw mechanical output, the packaged model at native bootstrap,
and the model Trellis subsequently used. Recorded extraction attempts include
failed and superseded attempts, invocation and tool identities, inputs and
available outputs. Full snapshots preserve the bytes; diffs show the changes.
An attempt is recorded before tool execution. A missing completion record
remains visible as interrupted or unfinished, with available partial output
retained and no claim that the extraction completed.
The model diff uses the configured extraction/model-support registries. It
does not infer Lean semantics by parsing declarations in Python.

Every accepted revision with remaining requirements returns through setup,
extraction and Modeling. The new generation has a fresh model baseline and
fresh review of its source-model relationship and Lean/NL meanings. Earlier
models and proofs remain reference material available for reuse through the
normal checks. Revision approval and Modeling approval remain distinct.

Older runs can contribute retained observations and extraction output to this
archive. Recovered evidence is labeled as such; missing attempts or a missing
original model baseline remain explicit historical coverage gaps. The archive
does not reconstruct an imagined extraction from the final edited model.
Missing linked generations or changed evidence block final export. An explicit
unfinished snapshot can expose such gaps for diagnosis without claiming a
complete final result.

The ordinary native completion callback publishes the final archive. An
approved retirement of all requirements produces an archive labeled
`all_requirements_retired`, with no claim of a final proof. Export errors remain
visible and can be retried without changing the underlying proof verdict.
Before completion, the viewer offers an explicitly incomplete history
snapshot. The archive includes toolchain and library identity records but not
tool installations or library build caches; reproducing a build requires the
recorded tools and dependencies, and may require relocating recorded local
library paths.

## Approval and revalidation

| Decision or evidence | Meaning |
| --- | --- |
| Previous formalization | Proof or refutation of the recorded previous Lean statements |
| Audit classification | Evidence-based diagnosis and proposed remedy; no human ratification |
| Scope pass | Agent assessment that the particular changes respect the authorized scope |
| Revision human approval | Acceptance of the specified source and natural-language requirement changes, including retirements |
| Successor Modeling human approval | Ratification of the Lean/NL pairings, supporting meanings, source-model claims and limitations explicitly presented by that instrument |
| Successor formalization | Checked results for that generation after its ordinary review and closure checks |

Revision approval does not ratify future Lean statements. The audit records
affected nodes and lanes (`correspondence`, `soundness`, `goal_faithfulness`,
`substantiveness`, `closure`) with reasons. The controller unions that proposal
with mechanical source/goal changes and dependency closure. The first
implementation uses a fresh generation and carries **no old verification
verdicts or human meaning approvals** into it. This conservative strategy can
repeat checks outside the minimal invalidation set. Old proof bodies are
reusable candidates through ordinary acceptance; they are not automatically
imported as verified nodes.

The kernel owns formal completion and the durable handoff. The operational
revision controller follows the existing native preparation pattern for source
work and review packages. It cannot install proof verdicts or approve the
successor's human gates. `PvRevision.tla` is a bounded abstraction of these
generation and approval boundaries; it is not a model of the whole checker or
agent implementation.

## Operation and recovery

`python3 scripts/pv_revision.py status RUNTIME` shows revision status.
`package` returns the exact review JSON; `export` returns the ZIP path and
package digest. Managed runs advance through the existing native controller.
`schedule` uses its existing launcher to resume that controller. Standalone
inspection, audit and task execution use `init`, `step`, or `run`; automatic
successor publication currently requires a managed native run.

`campaign-status RUNTIME` reports archive availability. `campaign-export
RUNTIME` exports the final campaign ZIP and reports its SHA256; add
`--allow-incomplete` for an explicitly unfinished snapshot. These commands
record no approvals and do not change verification state. The viewer exposes
the same downloads under Verification history.

`decide RUNTIME --action accept|deny|amend --digest DIGEST --reason TEXT`
records a human decision; amendments also require `--amendment TEXT`. The
viewer persists this decision before scheduling more work. Agent roles never
call this human control.

Nonempty agent `system_feedback` or three consecutive failed/invalid responses
halt revision work. After diagnosis, `retry RUNTIME --reason TEXT` records
explicit recovery at the same assignment. `reaudit RUNTIME --reason TEXT`
starts a new plan from the original source and requirements after a failed,
denied, or semantics-blocked round. A new plan requires new scope review and
human approval. Runtime halt markers must first be resolved through the
existing operator mechanisms. Pauses and interrupted launches preserve the
same pending generation rather than allocating another.

To opt an existing run in, stop it at `Stage::Start`, with no in-flight request,
after cycle zero and before Cleanup. With the ordinary repository checkpoint
hook configured, call the kernel CLI action:

```json
{"action":"activate_pv_revision","root":"/path/to/runtime","confirm_runtime_stopped":true}
```

The action checks the controller lock and supervisor liveness, and journals an
additive event through the normal checkpoint path. It changes no source,
existing approval, or historical event. Activation makes the next eligible
completion enter revision; it does not rewind or redeploy the run. Upgrade the
controller and viewer together before opting in a live deployment.
