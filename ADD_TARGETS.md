# Add-targets mode (`add_paper_targets`)

Adds paper targets (tex labels from the SAME paper) to a run that has fully
COMPLETED (`Phase::Complete` after Cleanup), reviving it into the existing
RevisionStating phase to state the new targets. All existing approvals are
byte-untouched; the normal RevisionStating → Advance HumanGate →
ProofFormalization → Cleanup → Complete pipeline then runs (a fresh full
Cleanup round at the end is by design).

## Operator flow

1. The run is stopped (it is Complete, hence quiescent).
2. Append the new tex labels to `workflow.main_result_labels` in
   `trellis.config.json`. Labels must name labeled `theorem`/`corollary`
   statement blocks in the configured `workflow.paper_tex_path` —
   main-result resolution scans those environments only, so a label on a
   `lemma` block will not resolve. The same restriction applies to the
   UNION re-resolution: a run whose pre-existing targets were configured
   via explicit `main_result_targets` entries on non-theorem/corollary
   blocks fails the action loudly (by design — relabel the statement in
   the paper or migrate the config first).
3. Run the action (offline, against the stopped run):

       echo '{"action":"add_paper_targets",
              "root":"<runtime-root>",
              "config_path":"<repo>/trellis.config.json"}' | trellis_runtime_cli

4. Relaunch the supervisor normally. The first cycle dispatches the revision
   planner (planner ON), which routes the statement work for the new targets.

## What the action does

- Labels-only re-resolution of the UNION label list (existing configured
  labels ∪ `main_result_labels`) against the configured paper; hard-errors if
  the paper is missing, any label does not resolve to a positive-line block,
  or no NEW label was supplied (re-running is therefore a hard error, not a
  silent no-op).
- Rewrites `workflow.main_result_targets` with the full resolved list and
  COMMITS the config in the run repo's git. The config is a tracked file:
  checkpoint `git reset --hard` reverts uncommitted edits, so if the action
  reports it could not commit (see its `notes`), commit manually before
  relaunching. If `TRELLIS_AB_TEMPLATES_DIR` is in use, apply the same edit
  to every A/B template (the action warns loudly but does not patch them).
- Revives the state: phase → RevisionStating, revision-planning
  StuckMathAudit seeded, cycle + event log CONTINUOUS, frozen set = existing
  targets' coverage ∪ protected closures ∪ Preamble/Axioms/challenge-pinned.

## Preconditions (all asserted; nothing mutates on failure)

Phase Complete; no in-flight request, human gate, pending task, or pending
protected re-approvals; a mode-A run (non-empty configured targets, not a PV
run); every added label resolves; no added label already configured.

## Semantics to expect

- Frozen nodes may gain `target_claim_updates` on any target — reuse via
  claim is the mechanism; their statements stay byte-frozen.
- Existing targets do NOT re-verify when their nodes join new targets'
  closures (per-target fingerprints).
- Added targets must admit substantively distinct statements: a covering
  node that merely cites an existing frozen theorem will fail the
  substantiveness lane. That is by design — pick genuinely new results.
- Layered construction is permitted while any configured target is
  uncovered (the "orphan construction window"): worker bursts may state
  support nodes before anything claims the new target, leaving them
  temporarily unattached. The covering statement closes the window, after
  which the same-burst orphan contract binds again; leftover unattached
  nodes are tolerated as pre-existing orphans and swept afterwards by
  later worker deletions (reviewer-routed work — deleting orphans stays
  legal in any burst).
