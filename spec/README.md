# Supervisor Protocol Spec

This directory contains a TLA+ specification of the intended deterministic
supervisor protocol.

It is deliberately **not** a line-by-line model of the current Python code.
The point is to state the protocol we want, especially where the current code
has grown patchwork semantics or known bugs.

## Scope

The current spec now covers the full high-level supervisor protocol:

- theorem-stating
- proof-formalization
- cleanup
- the theorem-stating expert/human gate before proof-formalization
- live worktree state versus committed checkpoint state
- restart-safe wrapper request/result flow at the protocol boundary

Within that full scope, theorem-stating is still the richest and most heavily
tested part of the model. Proof-formalization and cleanup are also modeled, but
some finer authorization details are still intentionally simplified.

## What The Spec Treats As Authoritative

The spec uses the intended redesign semantics, not current implementation
accidents.

In particular:

- There is one derived **global verification ledger**.
- The theorem-stating ledger is exactly:
  - paper-faithfulness blockers
  - node correspondence blockers
  - soundness blockers, where Lean-closed nodes count as settled
- Reviewer-facing task blockers live only on the current **pending task**
  assignment, not as a durable semantic ledger.
- If a pending task exists, its task blockers must satisfy:
  - `pendingTask.taskBlockers ⊆ globalBlockers`
- `ADVANCE_PHASE` is legal only when `globalBlockers = ∅`.
- After an `INVALID` theorem-stating attempt, the reviewer may only choose:
  - `CONTINUE`
  - `NEED_INPUT`
  - optional `reset_to_checkpoint`
- Success-shaped decisions after `INVALID` are illegal.
- A theorem-stating worker `STUCK` outcome is first-class:
  - no state change
  - no verification
  - return directly to reviewer
- In proof-formalization, invalid worker attempts retry internally from the last
  committed checkpoint until `ProofInvalidReviewThreshold`, then escalate to the
  reviewer.
- A valid worker result that makes no semantic delta also skips verification and
  returns directly to reviewer.
- Proof-formalization advances to cleanup automatically once the remaining open
  proof-bearing work is complete and verification is clean.
- Cleanup termination is reviewer-mediated via `DONE`.
- In theorem-stating, there is a single targeted task mode.
  - New dependencies inside the active paper-target support cone are always
    allowed by the core protocol.
  - The old `repair` versus `restructure` distinction is treated as prompt or
    planner metadata, not a semantic protocol split.
- The authoritative expert gate uses **Option A**:
  - configured target set unchanged
  - coverage of configured targets unchanged
  - approved paper-target fingerprints unchanged
- Reviewer overrides are allowed, but only as stored justifications tied to
  live blockers.
- Human input is carried by an explicit reviewer-managed
  `humanInputOutstanding` toggle and must be cleared once that input has been
  addressed.

## Why This Spec Exists

This spec is driven directly by the recent redesign/bug list, not just the
current code. The main issues it is meant to clarify or prevent are:

- global blockers versus task blockers were conflated
- reviewer routing state was persisted too long and drifted from the live global
  ledger
- `ADVANCE_PHASE` could be proposed or allowed while blockers remained
- invalid worker attempts were still able to flow into success-shaped reviewer
  outcomes
- human/expert gates were mishandled on resume
- trust/expert review was keyed to over-broad package drift instead of the
  approved target package
- stale artifacts and dirty worktree resets were hard to reason about

## Main Abstractions

The model abstracts away:

- prompts
- agent backends
- ports, tmux, `codex exec`
- concrete file contents
- actual git commands

Instead it models:

- an explicit wrapper request/result boundary
- a single normalized typed `response`
- live worktree state versus committed checkpoint state
- an explicit node dependency DAG
- a separate semantic dependency relation
- configured paper targets as first-class objects
- current target-coverage claims for those paper targets
- per-node substantiveness, correspondence, and soundness status
- per-target paper-faithfulness status
- current and approved semantic fingerprints
- per-node local-closure verification state (`localClosureUnverified` set
  + committed mirror), introduced by the local-closure feature to enforce
  that every sorry-free proof_node has a fresh kernel-axiom-clean closure
  record before `Phase::Cleanup` can be entered (`StalePassClosurePreventsCleanupTransition`
  invariant). The records map itself is below the TLA abstraction level;
  the abstract spec tracks only membership in the unverified set.
- reviewer decisions, pending worker assignments, stored override
  justifications, and human signals

Low-level provenance hygiene such as matching a paper `\\label{...}` onto a
tablet statement environment is intentionally not modeled as an agent-reviewed
protocol concern here. The intended design is that such issues are handled by
deterministic validation outside the core supervisor protocol.

The wrapper boundary is now explicit in the protocol:

- the supervisor issues abstract requests
  - `worker`
  - `corr`
  - `sound`
  - `review`
  - `advance_gate`
  - `human_gate`
- the environment may only produce a normalized response when a matching
  request is in flight
- consuming a response clears the in-flight request
- for `corr`, `sound`, and `advance_gate`, the response is raw per-lane output
  and deterministic reconciliation happens inside the protocol, not in Python

Those requests now carry explicit protocol-owned payload, not just a request
kind. The payload includes the exact semantic planning information the bridge is
allowed to use, such as:

- current phase, active node, and held target
- current task mode
- current live blockers and stored overrides
- blocked paper targets
- exact node and target verification frontiers
- exact verifier lanes
- proof-phase protected-package set
- invalid-attempt and human-input flags

This is intentional. The future Python bridge is not supposed to reconstruct
planning from raw supervisor state. Rust, as a refinement of this TLA+ spec,
should compute these request payloads exactly, and Python should only:

- render prompt text from the provided payload
- invoke the shared wrapper/agent helper
- return a normalized response

The build-critical tablet support surface is also protocol-owned:

- the modeled support artifact is the generated `Tablet.lean` root import surface
- whenever the protocol needs that support artifact for validation or
  verification, it is assumed current
- in the deployed runtime this also covers the local compiled Tablet module
  artifacts needed by compilation-based checks
- Python may host auxiliary support-file generation actions, but Rust decides
  when support sync happens

The intended trust boundary for deterministic checking is two-world:

- workers edit and may locally check in the worker repo using worker-writable
  caches
- the supervisor syncs the semantic source snapshot into a separate
  supervisor-owned workspace and reruns the exact deterministic checker there

Worker-side checker results are advisory evidence only. Supervisor-side checker
results are authoritative. If the worker-side and supervisor-side checker
results disagree on the same attempted artifact, the protocol treats that as a
deterministic invalid attempt, logs the discrepancy, exposes it to the viewer,
restores the last committed snapshot, and retries the worker step with fresh
context.

This is the intended boundary for the future implementation. The supervisor
should not know about raw/done files, ports, tmux sessions, or provider-specific
completion logic.

## Live State Versus Committed State

The model explicitly separates:

- live theorem-stating worktree state
- last committed checkpoint state

This is important for:

- invalid attempts that leave a dirty worktree visible to the reviewer
- reviewer-selected reset-to-checkpoint
- human/expert gates that occur after a committed cycle boundary

## Target Objects And Paper Faithfulness

Theorem-stating does not only verify individual nodes. It also tracks configured
paper targets as first-class objects.

For each configured paper target, the protocol carries:

- the current set of tablet nodes claiming to cover that target
- a target-level paper-faithfulness status
- current and approved paper-faithfulness fingerprints

This is how the spec distinguishes:

- a node whose local Lean/NL correspondence is fine
- from a target package whose claimed paper coverage is still not faithful

The intended verifier terminology is fixed:

- `paper-faithfulness`: target-level NL-only coverage of a configured paper target by the covering node statements and cited definition statements
- `correspondence`: node-level Lean-vs-TeX statement alignment
- `NL soundness`: rigor of the current NL proof from child statements

That split is the main intended fix for the old “local task versus real global
blocker” drift.

`Preamble` still uses the ordinary node-level correspondence blocker in the
protocol, but its underlying check is special:

- it is one-way support from `Preamble.tex` into the Lean interface exposed by
  `Preamble.lean`
- if `Preamble.tex` has no structured items, that correspondence check is
  vacuous and should carry no current blocker/fingerprint

## Expert Gate Semantics

The authoritative expert gate is target-based.

Re-review is required iff any of these change relative to the approved target
snapshot:

- configured main-result targets
- coverage of those configured targets
- paper-faithfulness fingerprint of any approved target package

The expert gate itself remains target-based, but the spec also derives
`ProtectedNodes` from the semantic-dependency closure of the approved target
package. That set is used only for the proof-phase protected-package guard:

- outside `coarse_restructure`, proof work may not drift the approved protected
  package’s semantic fingerprints or target-coverage claim
- theorem-stating completion and expert-gate legality still depend on the target
  package directly, not on `ProtectedNodes` as a second authority

For proof-formalization worker scope, the intended split is now explicit:

- `proof_local`: the active node may edit its Lean proof body/imports and may
  introduce new helper nodes inside its final support cone
- `proof_restructure` / `proof_coarse_restructure`: broader local branch edits
  are allowed under reviewer authorization
- `allow_new_obligations`: when false, every new helper must be Lean-closed in
  the same burst; when true, otherwise-scoped helpers may carry `sorry` with an
  NL proof obligation that will go through verifier lanes and later proof work
- `must_close_active`: when true, the active node must be Lean-closed in the
  burst; when false, the active node may remain open if every other gate passes

The easy/hard difficulty label remains only an advisory work hint.

## Reviewer Overrides And Human Input

Reviewer disagreement overrides are modeled as stored justifications on specific
live blockers. Abstractly, the protocol records which blockers have been
explicitly overridden; the content of the prose justification is outside the TLA+
model, but the requirement that it exist is not.

The reviewer must account for every live blocker in theorem-stating by either:

- keeping it in the next task’s blocker set, or
- overriding it explicitly

Human input is modeled separately:

- a `NEED_INPUT` decision opens the human gate
- once human feedback arrives, `humanInputOutstanding` becomes true
- reviewer decisions may later clear it with `clearHumanInput`
- once cleared, later cycles should no longer treat that human input as active

## Current Simplifications

The spec is cleaner than the current Python code, but it still has a few known
simplifications:

- The harnesses currently instantiate `SemanticDeps = Deps`. That is fine for
  protocol testing, but the real implementation should be able to distinguish
  semantic dependencies from ordinary proof-DAG edges.
- The content of reviewer override justifications is abstracted away. The spec
  models that a justification is stored for a blocker, not the prose itself.
- The wrapper protocol currently models normalized result delivery and malformed
  responses, but not yet richer wrapper failure taxonomy or backend retry
  policy.
- The spec models protocol legality, not prompt text or Lean/file-content
  semantics.

## Files

- `SupervisorProtocol.tla`
  - parameterized full supervisor protocol
- `SupervisorProtocolSim.tla`
  - concrete harness for random simulation (the only harness we run)
- `SupervisorProtocol.sim.cfg`
  - invariants for the simulation harness

The previous BFS exhaustive harnesses (`SupervisorProtocolSmall.tla`,
`SupervisorProtocolMedium.tla`) and their configs (`SupervisorProtocol.cfg`,
`SupervisorProtocol.medium.cfg`) were removed deliberately because every TLC
run on this spec is RAM-heavy enough that exhaustive search regularly OOMs
and even sim mode warrants a strict concurrency cap. Do not re-add them.

## Running TLC

Sim mode only. Invoke `tla2tools.jar` against `SupervisorProtocolSim.tla` and
`SupervisorProtocol.sim.cfg`, for example:

```bash
java -XX:+UseParallelGC -jar tla2tools.jar \
    -simulate num=100000 -depth 40 -workers 2 \
    -config SupervisorProtocol.sim.cfg SupervisorProtocolSim.tla
```

Reasonable defaults are around 100000 simulated traces at depth 40 with two
workers; drop to `num=2000 -workers 1` for a cheap smoke run.

**The JVM heap can blow past 80% of host RAM under default params.** Cap
concurrent sims accordingly, watch `free -h` while a run is in progress, and
kill it before the machine starts swapping.

The spec does not yet model real file/content semantics, but it does model an
explicit abstract dependency DAG via `Deps` and `NodeRank`, and the environment
is restricted to local neighbor steps and small reviewer-choice sets rather
than arbitrary whole-state rewrites — useful for keeping sim runs informative
without making the state space unbounded.

## Protocol Constants

The current spec has only a small number of protocol-shaping constants:

- `ProofInvalidReviewThreshold`
  - proof-formalization invalid worker retries within one cycle before reviewer
    escalation

The remaining bounded constants are model-checking bounds, not intended runtime
policy:

- `MaxCycle`
- `MaxAttempt`
  - theorem-stating invalid-attempt reviewer-loop retry bound in the current
    model harnesses

## Rust Parity Rule

For the trellis supervisor, the Rust kernel is intended to be a refinement of
this TLA+ protocol, not a second independent design.

That means protocol parity is bidirectional.

If Rust changes protocol semantics, the spec must move with it. If the spec is
patched because TLC or review exposed a protocol issue, the corresponding Rust
code must be inspected in the same change and either confirmed to match or
updated too.

In practice, this includes changes to:

- protocol state fields
- `WrapperRequest` or `WrapperResponse` payload shape
- transition legality
- verifier-lane reconciliation
- checkpoint / rollback semantics
- reviewer override or human-gate semantics
- difficulty / retry semantics

Operational rule:

- if you change protocol semantics in `kernel/`, update
  `spec/SupervisorProtocol.tla` and the sim harness/config in the same change
- if you patch protocol semantics in `spec/`, inspect the corresponding
  `kernel/` path in the same change and either keep it in sync or
  fix it too
- run TLC in sim mode against `SupervisorProtocolSim.tla` before treating
  either change as complete (subject to the host-RAM caveat noted above)
