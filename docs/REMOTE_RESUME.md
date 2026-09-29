# Resume a run from its Git remote

In the viewer's authenticated landing page, choose **Resume run**, enter a new
run name and the tablet's Git remote, and confirm that its previous writer has
stopped. You can optionally select a branch and checkpoint. The new run continues
mirroring to that branch. Git authentication uses the destination host's normal
SSH or credential helper configuration; do not put credentials in the URL.

Trellis selects the latest supported idle checkpoint on the fetched branch's
first-parent history. The status panel shows the fetched tip, selected commit,
cycle and saved stage. An in-flight request is never cleared to create an idle
checkpoint. If the selected checkpoint precedes the tip, continuation requires an
explicit approval naming both commits: subsequent mirroring will replace those
later descendants. A changed remote tip blocks continuation. This is a
single-writer handoff, not a distributed lock; keep the original run stopped.

The first resume can take hours. Trellis rebuilds Lean outputs, observes the
resulting fingerprints, and regenerates local closure records for every eligible
Proof, Definition and Preamble owner. It separately rebuilds historical source
views that can be activated by rollback. This deterministic work does not ask
LLM verifiers or reviewers to repeat their recorded judgments. Current approvals
are rebound to rebuilt evidence; stale approvals remain stale and failures remain
failures. New edits and already scheduled future work still use normal checks.

Human holds and completed runs are restored without starting ordinary work.
They retain the complete destination launch recipe, including the pinned kernel,
parallelism, checker/runtime bindings, source identity and tmux socket, for the
ordinary run controls. Resume refuses a replaced pinned runtime executable.
For an active checkpoint, completion requires verifying the supervisor executable,
runtime/checker binding, and first progress. Retry retains the selected commit,
completed builds and probes. It never reconstructs over a committed migration or
launches another supervisor after a launch attempt. If that supervisor stopped,
inspect its runtime diagnostics and use the ordinary run controls to continue.
An initial provider-budget pause is also a completed launch: a fresh launch
receipt, kernel pause and retained pending request must agree, and no fatal halt
may be present. The run becomes visible with its pause explanation and ordinary
Resume control, even if it exited before the first process/progress poll.

## Destination setup

Use a compatible Trellis installation and the repository's pinned Lean toolchain
and dependencies. Set `TRELLIS_RESUME_PROFILE` on the viewer to an absolute path
to a local JSON profile. For example (replace the executable path and dependency
command with this host's actual settings):

```json
{
  "version": 1,
  "runtime_cli": "/opt/trellis/kernel/target/release/trellis_runtime_cli",
  "lean_parallelism": 2,
  "env": {
    "TRELLIS_SOUNDNESS_FINGERPRINT_MODE": "text",
    "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED": "1"
  },
  "dependency_prepare_argv": ["lake", "exe", "cache", "get"]
}
```

Choose fingerprint mode and other launch settings to match the source run. The
profile is copied into the job; the runtime executable's hash is pinned for
retries. Its environment must contain nonsecret string settings. It is never
loaded as shell code. `dependency_prepare_argv` must prepare exactly the tracked
dependency revisions without editing tracked files; for a Lean-only repository
without Mathlib, `["lake", "build"]` is suitable. Mathlib projects also run the
normal checker support-cache preparation. The form uses the same host Lean
parallelism recommendation as new-run creation and permits an override.

The remote must contain its config, policy, source paper, registered reference
papers, pinned dependencies, Tablet sources, canonical checkpoint and segmented
event log. Missing mathematical inputs block resume with a path diagnostic.
Destination paths and native provider contexts are rebuilt; saved shell launch
scripts and old credentials are not reused. Keep destination paths short enough
for Unix checker sockets (less than 108 bytes including the socket filename).

Initial support is current-schema **Lean math** checkpoints at Start, HumanGate
or Complete. PV, RequiredV1, challenge, revision, Isabelle and older fingerprint
schemas are rejected explicitly. They require their own migration; resume does
not discard their approval surfaces.

## Local maintenance and evidence

Jobs live under the existing `.trellis-create-jobs` directory and expose the
normal create-job status, log and retry controls. The shell entry point is:

```sh
scripts/trellis_create_run.sh start-resume NEW_NAME \
  --remote-url git@example.org:owner/tablet.git \
  --handoff-confirmed --lean-parallelism 2
```

`scripts/migrate_local_closure_records ROOT --runtime-cli /absolute/trellis_runtime_cli
--mode trusted-artifact-rebind --plan` reports required source views. The job
prepares `trusted-rebind-views.json` and scoped checker capabilities before
`--apply` and `--readiness`. Manual apply requires all source/state writers to be
stopped. This mode is separate from identity-only repair: an identity match alone
does not establish artifact admission.

Publication appends a versioned `TrustedArtifactRebind` event while retaining the
old event-prefix bytes and logical counters. State, checkpoint, canonical history
and record mirrors recover from one durable publication decision. A local Git
checkpoint records the maintenance event before launch; no new remote refs or
artifact uploads are needed. Content-addressed artifact epochs stay local and
are restored on rollback. `trusted-rebind-seed.json` is a local pre-migration
checkpoint for replay auditing; its explicit `event_count_convention` is
`record_count`, while unmarked legacy replay seeds retain their older index
convention.

Ordinary legacy checkpoints publish their post-response state before the final
response enters the Git log. Resume therefore first appends an explicit
`TrustedCheckpointBoundary` in that missing index, then the artifact-rebind
event. This adopts the exact trusted canonical snapshot; it does not invent the
missing response. The carrier binds the original Git commit and raw checkpoint,
the decoded snapshot, and the exact old log prefix. Replay verifies this evidence
even when a selected-checkpoint seed skips the boundary. Earlier and original
checkpoint seeds can both reproduce the migrated state without the local audit
seed. Already annotated record-count checkpoints need only the rebind event.
Both maintenance carriers use the existing shared-state encoding so large
snapshot and record-map values are not repeated as plain JSON in Git history.

Destructive replay through a migration requires valid local artifact epochs for
both the target state and the state needed for rollback. Missing epochs block
replay before Git reset. Success restores the exact retained log bytes, activates
the target artifacts, relocates destination configuration, and writes matching
runtime and shared canonical checkpoints with the actual record count and state.
It does not create a commit, tag, event, or idle boundary. A subsequent failure
restores the captured local carriers, including uncommitted log and configuration
bytes. Dry replay remains independent of local artifact epochs. Records beyond
the requested replay boundary may be discarded even when malformed.

The opt-in test `tests/test_remote_resume_lean.py` builds and resumes a disposable
Lean-only checkpoint through the real checker, checks replay and retry, and
restores a removed artifact from its epoch. Build the runtime CLI and
`cargo test --test remote_resume_lean --no-run` into the same target directory,
then set `TRELLIS_RUN_REMOTE_RESUME_LEAN=1` and `TRELLIS_REMOTE_RESUME_TARGET` to
that directory when running pytest. It uses its own tmux socket and starts no
provider sessions. Ordinary unit tests do not require Lean.
