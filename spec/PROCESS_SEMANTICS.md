# Trellis protocol: process semantics

This document describes the kernel's control flow in prose, phase by phase
and stage by stage. It is intended for auditing the semantic contract:
"if the worker does X, this happens." All specific behaviors are grounded
in `kernel/src/{engine,model}.rs`.

## 1. Actors, phases, and stages

**Actors** that exchange messages with the kernel via the bridge:

- **Worker** — produces proof/theorem edits. Per-cycle, single-shot. Can
  return four outcomes: `Valid`, `Invalid`, `Stuck`, `NeedsRestructure`.
- **Verifier panels** — up to four per cycle: **paper-faithfulness**
  (target-level), **substantiveness** (node-level, "does this .tex
  statement correspond to a paper claim and not silently weaken it";
  TheoremStating + ProofFormalization only — dormant in Cleanup),
  **correspondence** (node-level, NL↔Lean meaning), **soundness**
  (node-level, NL proof). Substantiveness shares `Stage::VerifyPaper`
  with the target-level paper lane (the per-cycle scheduler emits
  exactly one frontier per Paper request — `paper_verify_targets` XOR
  `substantiveness_verify_nodes`). Canonical definitions live at
  `trellis/prompt_fragments/canonical/{FAITHFULNESS,SUBSTANTIVENESS,CORRESPONDENCE,SOUNDNESS}.md`.
  Each panel is multi-lane (V1, V2, …). Lanes vote independently.
- **Reviewer** — single adjudicator. Sees accumulated verifier evidence,
  worker rationale, and the global blocker set. Decides how to proceed.
- **Human operator** — routed to via `HumanGate` after a NeedInputAuditor
  confirms a reviewer `NeedInput` escalation, or for phase-advance
  approval.

**Phases** are coarse-grained project stages:

- `TheoremStating` — the tablet's theorem statements and their NL proofs
  are still being drafted; the Lean side is a placeholder `sorry`.
- `ProofFormalization` — NL is frozen (per target); workers close Lean
  `sorry`s and may add helper nodes when the selected proof mode authorizes
  them.
- `Cleanup` — formalization complete; final pass removes unused
  machinery, tightens proofs, etc.
- `Complete` — terminal; kernel produces no further requests.

**Stages** are the intra-cycle state machine:

- `Start` — between cycles; waiting for `StartCycle` event.
- `Worker` — worker request in flight.
- `VerifyPaper` / `VerifyCorr` / `VerifySound` — verifier panel in
  flight.
- `Reviewer` — reviewer request in flight.
- `HumanGate` — human-facing prompt.
- `StuckMathAudit` — stuck-math or NeedInputAuditor read-only audit role
  in flight (§4.8). `RequestKind::StuckMathAudit`.
- `CleanupAudit` — cleanup-v2 audit role in flight (§4.7). Sub-stage of
  `Phase::Cleanup`; `RequestKind::Audit`.

## 2. State and fingerprints

Four parallel status maps track verification verdicts:

- `paper_status[target] ∈ {Unknown, Pass, Fail}`
- `substantiveness_status[node] ∈ {Unknown, Pass, Fail}` (per-node
  paper-lane verdict; lane vote admits a fourth `NotDoneYet` value at
  the response level — see `SubstantivenessStatus` at model.rs —
  which the reconciler folds back into `Unknown` with the kernel queuing
  another Paper request, bounded by `substantiveness_consecutive_no_progress_requests`).
- `corr_status[node] ∈ {Unknown, Pass, Fail}`
- `sound_status[node] ∈ {Unknown, Pass, Fail, Structural}`

Each is paired with two fingerprint maps: `*_current_fingerprints` (hash
of the content the kernel observed last) and `*_approved_fingerprints`
(hash pinned at the moment a verdict became durable). The derived
predicate `current_*_pass(n)` returns true **iff** `status = Pass` AND
`current == approved`. So a verdict "sticks" only while the content
hasn't drifted since approval — if the worker edits a passed node later,
current diverges, `current_*_pass` flips to false, and the blocker
reappears. This is the chief mechanism that forces re-verification after
edits.

### 2.1 What composes each lane's fingerprint — and why Substantiveness is dep-independent

The reopen story above hides a deliberate per-lane asymmetry in *what
counts as a drift event*. Each lane's fingerprint is composed differently,
which dictates whether an edit to a node's deps reopens that lane on the
node itself:

- **Correspondence** (`CorrespondenceFingerprint` in
  `runtime_cli_observations.rs`) includes `own_tex`, `lean_semantic_closure`
  (Lean type-surface), and `lean_relevant_definition_descendants` (def-deps'
  tex hashes). Dep edits propagate.
- **Paper-faithfulness** (`PaperTargetFingerprint`) includes covering-node
  tex statements + def-dep tex + preamble defs. Dep edits propagate.
- **Soundness** (`SoundFingerprintParts`) includes `own_tex` and
  `dep_statement_hashes` (per-dep tex statement block, `\noderef`-cited
  deps only). Dep edits propagate via the rich `SoundAssessmentStatus`
  taxonomy (`DepEditOnlyStaleFail` / `DepEditOnlyStalePassDeferred`),
  which preserves the prior verdict as evidence while flagging the
  drift.
- **Substantiveness** (`SubstantivenessFingerprint`) includes `own_tex`
  (statement block hash), `paper_source_sha`, `node_kind`, and
  `claimed_deviation_fingerprints`. **It does NOT include any signal of
  dep substantiveness, dep correspondence, or dep tex content.** Dep
  edits do *not* drift this node's substantiveness fingerprint.
- **Deviation** is per-file, not per-node; the notion of "dep edit" does
  not apply.

The Substantiveness dep-independence is a deliberate pragmatic choice,
not an oversight. The rationale: Substantiveness asks "does this node's
statement express a substantive paper claim?" — a property of the node's
own text, not of what its `\noderef` citations resolve to. If a dep is
renamed or its claim is rewritten, the dep's own Substantiveness verdict
may flip and surface as a blocker on the dep, but the parent's
Substantiveness Pass remains correct: the parent's statement still
expresses the same paper claim, regardless of how the dep is now
formulated.

The broader argument for this asymmetry: **Correspondence and Soundness
are sufficient to keep the DAG formalizable.** Correspondence catches
signature-level mismatch under dep drift (its fingerprint composes
dep type-surfaces); Soundness catches prose-proof breakage under dep
drift (its fingerprint composes `dep_statement_hashes`). Reopening
Substantiveness on every downstream change would be redundant work —
the verifier would re-confirm the same paper-claim-expression judgment
without new information. So Substantiveness is content-local by design.

This means a parent node whose dep has been re-stated may carry a
Substantiveness Pass alongside a fresh Correspondence or Soundness blocker
on itself; the formalization gets resolved through the Correspondence
or Soundness lane without redundant Substantiveness churn. Subtle
invariant; future readers should not "fix" it by adding a dep signal
to `SubstantivenessFingerprint`.

**Global blockers** (`global_blockers()` at model.rs) is the set of
unresolved checks:

- For each configured target with `¬current_paper_pass(target)`:
  a `PaperFaithfulness` blocker.
- For each live present node with
  `needs_substantiveness(node) ∧ ¬current_substantiveness_pass(node)`:
  a `Substantiveness` blocker (TheoremStating + ProofFormalization;
  dormant — short-circuits to Pass — in Cleanup/Complete).
- For each live present node with `¬current_corr_pass(node)`:
  a `NodeCorr` blocker.
- For each live present node with `needs_sound(node) ∧ ¬current_sound_pass(node)`:
  a `Soundness` blocker.

Fingerprints of the blockers (carried as opaque identifiers in the
reviewer's partition response) are drawn from `live.*_current_fingerprints`.

### 2.2 Local-closure tier coverage invariant (Audit C-3)

Beyond the four lane status maps, the kernel maintains a fifth tier:
the **local-closure tier**, modeled as three maps:

- `local_closure_records: BTreeMap<NodeId, LocalClosureRecord>` — passed
  probes (`LocalClosureRecord` holds toolchain / lake_manifest /
  preamble / approved-axioms / per-decl hashes, per-dep
  `kernel_semantic_hashes`, and an `axcheck_status: AxcheckStatus`
  enum tracking whether the secondary axcheck collector ran).
- `local_closure_unverified_nodes: BTreeSet<NodeId>` — pending re-probes.
- `local_closure_failures: BTreeMap<NodeId, ErrorSummary>` — failure
  diagnostics keyed by unverified entries.

The four-clause `formalization_complete()` gate requires every present
sorry-free proof_node to hold a record (NOT in unverified, NOT in
failures). Without continuous coverage, an operator hand-edit between
bursts can produce a sorry-free node with neither a record nor an
unverified entry — and the gate silently fails ever after.

**The continuous coverage invariant.** After every structural
transition (worker accept, cone-clean reset, LastClean rewind,
rejection rollback), `ensure_local_closure_coverage()` pins orphan
sorry-free present proof_nodes into `local_closure_unverified_nodes`
so the next probe pass refreshes them. `validate()` enforces the
contract whenever the closure tier is non-empty: every sorry-free
present proof_node must appear in `records ∪ unverified`.

This is the "design choice" note for the C-3 audit fix — the gate
is the **terminal** consistency check; the continuous scan is the
**ongoing** consistency guarantor. Both are required because the
gate-only model deadlocks on orphan introductions.

The intent spec (`SupervisorCore.tla`) collapses the closure tier to
a single total function `localClosureStatus : Nodes -> {verified,
unverified}` whose totality structurally encodes coverage; the big
spec (`SupervisorProtocol.tla`) tracks `localClosureUnverified ⊆
presentNodes ∩ currentProofNodes` and treats record presence as
`(present ∩ proof_node) \ (unverified ∪ open)`. Both spec layers
ratify the continuous-scan invariant via TypeOK constraints.

## 3. The cycle skeleton

A cycle begins at `Start` stage. The `StartCycle` event (the runtime
emits this between cycles) bumps `cycle`, picks the first request kind,
and transitions to the corresponding stage. The rest of the cycle
alternates between "kernel emits request → bridge delivers response →
kernel processes" until control reaches `Reviewer`, who decides whether
the cycle commits (returning to `Start`) or immediately re-issues
(retrying).

The standard in-cycle sequence is:

```
Start → Worker
     └→ (worker Valid with delta)
        → VerifyPaper → VerifyCorr → VerifySound → Reviewer
     └→ (worker Valid without delta)
        → Reviewer
     └→ (worker Stuck/NeedsRestructure)
        → Reviewer (retry-flagged)
     └→ (worker Invalid)
        → Worker again (same cycle, attempt + 1) until threshold,
          then Reviewer (retry-flagged)
```

The above arrows are typical paths. Deviations worth knowing about:

- **Malformed worker/verifier response**: the handler stutters
  (re-issues the same request with no state change) at
  engine.rs. The bridge eventually produces a
  parseable response or times out.
- **Paper-Fail precedence with verifier drain**: when a paper verifier
  response leaves a current `PaperFaithfulness` or `Substantiveness`
  Fail blocker live, the paper-accept handler normally routes to
  Reviewer. Exception: if some non-adjudicable Unknown blocker has a
  live verifier frontier, that verifier runs first so a freshly changed
  fingerprint cannot be pinned by reviewer tasking without verifier
  evidence (see Section 4.3).
- **Verifier ordering invariant**: per-cycle verifier order is
  paper-target → substantiveness → corr → sound → review. Both paper
  variants share `Stage::VerifyPaper`; `apply_theorem_paper_accept` /
  `apply_proof_paper_accept` (engine.rs) drain both paper
  frontiers before transitioning to VerifyCorr/VerifySound.
- **StuckMathAudit substitution**: in `Phase::ProofFormalization`, any
  routing path that would issue Reviewer goes through
  `issue_review_or_stuck_math_audit` (engine.rs), which
  substitutes a `StuckMathAudit` request (and `Stage::StuckMathAudit`)
  when the audit latch is active and the dispatch cadence permits
  (§4.8). StuckMathAudit then writes a durable `audit_plan` and routes
  to Reviewer.
- **Cleanup audit detour**: `Phase::Cleanup` starts with
  `Stage::CleanupAudit` driving the audit role over one or more bursts
  (§4.7); the reviewer only sees the resulting `cleanup_audit_tasks`
  list. Subsequent re-entries are reviewer-driven.
- **Theorem-stating start without Worker**: `start_cycle` consults
  `theorem_start_request_kind` (model.rs); if paper/corr/sound
  verification is needed before a worker turn, the cycle jumps
  directly to a verifier stage.
- **Orphan detour**: if a Valid-with-delta introduces orphans, the
  kernel schedules an orphan-cleanup worker before verifiers run.

A request is considered **in flight** while its response hasn't arrived;
`state.in_flight_request` holds it. On runtime restart, the kernel
regenerates the in-flight request from state (`runtime.rs`), so the
bridge can re-issue the same logical request after a crash.

## 4. Stage walkthrough

### 4.1 `Start` → `StartCycle`

`start_cycle` (engine.rs) increments `cycle`, sets `attempt = 1`,
and chooses the first request kind. The branches, in priority order:

- `Phase::ProofFormalization` + `force_review_after_cone_clean`: jump
  directly to a `Reviewer` cycle. The cone-clean acceptance
  synchronously hands off without a verifier drain (§4.9).
- `Phase::ProofFormalization` + `force_stuck_math_audit_after_rewind`:
  jump to a `Reviewer` cycle through `issue_review_or_stuck_math_audit`,
  which substitutes `StuckMathAudit` if the audit latch and dispatch
  cadence permit (§4.8).
- Protected-reapproval routing (`maybe_issue_protected_reapproval`,
  engine.rs): if any target has a pending protected-closure
  reapproval, issue `RequestKind::Review` with the
  `Stage::ProtectedReapproval` shape directly.
- `orphan_cleanup_needed()` — a present node is not reachable from any
  configured-target's coverage — schedules an orphan-cleanup worker
  (`work_style_hint = "restructure"`, `proof_edit_mode =
  CoarseRestructure`).
- `Phase::TheoremStating`: `theorem_start_request_kind()` — typically
  `Worker`, but may emit a verifier (Paper / Corr / Sound) if a
  frontier still needs draining. The TheoremStating-phase
  StuckMathAudit trigger (Sound-blocker node-set stagnation) also
  preempts a would-be `Worker` here (§4.8).
- `Phase::ProofFormalization`: `proof_start_request_kind()` —
  symmetrically may emit Paper / Corr / Sound before Worker (each
  verifier lane can have a non-empty frontier post-Cleanup or on
  cross-cycle resume).
- `Phase::Cleanup` with an active or pending worker task
  (`cleanup_active_task.is_some() || pending_task.is_some()`):
  `RequestKind::Worker` runs the next cleanup task.
- `Phase::Cleanup` first-burst (`cleanup_audit_burst_count == 0 &&
  cleanup_active_task.is_none()`): `RequestKind::Audit` enters the
  cleanup-v2 audit lane (§4.7).
- `Phase::Cleanup` subsequent burst with no active or pending task:
  `RequestKind::Review` (defense-in-depth). The legitimate cleanup-v2
  control flow re-issues subsequent audit bursts from
  `apply_audit_response` and drives worker dispatch from the
  reviewer's Continue, so this branch is unreachable under normal
  flow. State load from disk, recovery paths, or future code changes
  can reach it — routing to Reviewer (rather than the legacy Worker
  fallback that would emit an empty-pending-task burst) lets the
  reviewer choose Continue (next dispatch from a Pending task) or
  Done (advances to `Phase::Complete`).

If we're going into `Worker` in `ProofFormalization` and `active_node =
None`, `select_initial_proof_active_node()` picks the highest-rank open
proof node.

### 4.2 `Worker` stage

Worker responses (`apply_worker_response` engine.rs) are dispatched
by `worker_context.validation_kind` to one of three phase-specific
handlers.

The **four worker outcomes** are handled as follows:

**`Valid`** — worker closed the proof (or made progress).

"Semantic delta" here means `worker_semantic_delta(response) == true`
(model.rs): either the snapshot content changed, or any of the
four structural-update maps (`proof_node_updates`, `node_kind_updates`,
`dep_updates`, `target_claim_updates`) has a non-`Same` entry. A
worker that marks a node closed without editing tablet content still
counts as a delta and triggers verifier stages.

- In theorem-stating (engine.rs): snapshot is applied, structure
  updates applied, difficulty reset. If Valid-with-delta, proceed to
  verifier stages in order paper → corr → sound; else go straight to
  `Reviewer`. Any retry context is cleared.
- In proof-formalization (engine.rs, accepted-Valid branch starting
  at 1354): same shape — delta triggers verification, else directly to
  `Reviewer`. Easy-attempts counter for the node is reset.
- In cleanup: similar flow; cleanup has its own validation step that
  rejects irrelevant edits.
- In all three, if the delta introduces new orphans, the kernel
  schedules an orphan-cleanup worker instead of going to verifiers.

**`Invalid`** — worker failed to satisfy its contract. The kernel has
already run deterministic checks (see `runtime_cli_observations.rs`);
`response.deterministic_rejection_reasons` will be surfaced to the next
reviewer. Behavior:

- Same-phase Invalid: `continue_worker_retry` (engine.rs) consults
  `worker_retry_threshold` — 2 for both theorem-stating and
  proof-formalization. If `attempt < threshold`, increment and re-issue
  another `Worker` request (same cycle, invalid-attempt flag set). If
  threshold reached, escalate to `Reviewer` with retry context
  populated. `invalid_attempt = true`, `retry_outcome_kind = Invalid`.
- Cleanup phase: no retry threshold. Invalid immediately escalates to
  `Reviewer`.

**`Stuck`** — worker cannot close the proof in one attempt under the
current scope. Worker may have explored via tablet edits before
concluding it's stuck; the kernel honours the outcome regardless of
snapshot delta. Safety: the engine calls `state.restore_committed()`
to revert in-memory protocol state, emits `RestoreWorktreeToActiveWorkerBase`
to revert disk, and the runtime captures `last_invalid` Tablet WIP
for the next worker. The kernel either retries (same logic as Invalid)
or escalates to Reviewer with `retry_outcome_kind = Stuck`.

**`NeedsRestructure`** — worker concludes the existing decomposition
is wrong for the active task (e.g. an imported helper's statement is
too weak, the active node's signature/`.tex` needs to change, a
sibling node needs repair). As with Stuck, snapshot delta is allowed
— same engine-side `restore_committed()` + worktree restore + last_invalid
capture. No retry — immediately escalates to `Reviewer` with
`retry_outcome_kind = NeedsRestructure`. The reviewer typically grants
restructure authority in the next worker cycle via `next_mode`; when the
needed non-protected node is outside the current active-coarse cone, the
reviewer uses the audit-gated `global_repair_request` path. `NeedInput`
is reserved for genuine paper gaps.

**Note on Stuck/NeedsRestructure with snapshot deltas.** Workers exploring
via tablet edits before concluding Stuck or NeedsRestructure are honoured
regardless of snapshot delta; the engine-side `restore_committed()` plus the
emitted `RestoreWorktreeToActiveWorkerBase` revert any exploratory edits.
The rollback path is the safety mechanism — there is no separate
reclassification of delta-bearing Stuck/NeedsRestructure to Invalid.

### 4.3 Verifier stages — panel reconciliation

Each verifier panel (paper-target / substantiveness / corr / sound) is
multi-lane. Lane votes are reconciled via `reconcile_votes`
(engine.rs):

- **Unanimous** vote (all lanes agree on Pass/Fail/Structural/same) →
  `sound_status[node] = that_verdict` (or unchanged if all "same").
  On a decisive verdict (Pass/Fail/Structural), `*_approved_fingerprint`
  is pinned to `*_current_fingerprint` — the verdict durably binds to
  the content hash.
- **Split** — any two lanes disagree → `reconcile_votes` returns `Same`
  → `*_status` stays Unknown and `*_approved_fingerprint` is not pinned.
  The blocker surfaces to the reviewer as `Soundness` / `NodeCorr` /
  `PaperFaithfulness` with `status = Unknown`.

After a verifier response commits to state, `latest_*_review_nodes` (or
`latest_*_review_targets`) is populated with the set of nodes the panel
voted on (engine.rs). This is the "scope of adjudication" gate — the
reviewer can only adjudicate blockers on nodes/targets in this set.

**Phase routing after a verifier response**:

- Paper (both target-level and per-node substantiveness modes — same
  `Stage::VerifyPaper`, response dispatched on which of
  `paper_verify_targets` / `substantiveness_verify_nodes` was
  non-empty, engine.rs): if in theorem-stating,
  `apply_theorem_paper_accept`; if in proof, `apply_proof_paper_accept`.
  Both handlers enforce the paper drain loop — clear all paper
  Unknowns (target + per-node) before transitioning to VerifyCorr/
  VerifySound. The per-node mode has a safety bound:
  `substantiveness_consecutive_no_progress_requests >=
  SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS` escalates to Reviewer
  with the stuck frontier pinned in `latest_substantiveness_review_nodes`
  (engine.rs). **Paper-Fail precedence**: if any current
  `PaperFaithfulness` or `Substantiveness` Fail blocker survives, the
  handler routes to `Reviewer` unless a non-adjudicable Unknown blocker
  has a live verifier frontier. In that exception, the handler drains
  verifier work in priority order `VerifyPaper → VerifyCorr → VerifySound`
  before Reviewer. This prevents reviewer tasking from pinning
  `approved_fp = current_fp` for a freshly reopened fingerprint before a
  verifier adjudicates it. Substantiveness Fail on a node blocks the
  Corr verifier on that node from advancing the workflow: a
  Substantiveness Fail satisfies the Fail-escalation predicate in
  `apply_*_corr_accept` (engine.rs), so once Substantiveness
  has failed for a node, no `VerifyCorr` round can fall through to
  Reviewer without first surfacing the Substantiveness Fail.
- Corr: same verifier-drain exception before Fail escalation, then drains
  `VerifyPaper → VerifyCorr → VerifySound` as needed; otherwise continues to
  `Reviewer`.
- Sound: `apply_sound_response` (engine.rs) — always goes to
  `Reviewer` next, except in proof-formalization when formalization
  is complete and all blockers cleared (advances to `Cleanup`).

### 4.4 `Reviewer` stage

The reviewer's response is a single record with the following key
fields:

- `decision ∈ {Continue, NeedInput, AdvancePhase, Done}` — high-level
  action.
- `task_blockers`, `override_blockers`, `reset_blockers` — **three-way
  partition of the request's `blockers` field**. Every blocker in
  `request.blockers` must land in exactly one bucket, but
  `override_blockers` may only use ids from
  `request.review_contract.blocker_partition.allowed_override_ids`.
- `reset ∈ {None, LastCommit, LastClean}` — whole-state rewind (distinct
  from the per-blocker `reset_blockers`).
- `next_active`, `next_mode` — routing for the next worker cycle.
- `next_active_coarse` (proof-formalization only) — optional active
  coarse-anchor change. Legal only on Continue+reset=None+
  `retry_outcome_kind=None`, and only when the chosen node is in
  `kernel_hinted_next_active_coarse_nodes` (empty when the anchor is
  locked — see §4.10).
- `global_repair_request` / `consume_global_repair_grant`
  (proof-formalization only) — audit-gated escape hatch when no
  `(next_active, next_mode)` choice inside the current active-coarse cone
  can cover a needed non-protected edit. This route is independent of
  retry status: a reviewer reached after `Stuck` or `NeedsRestructure`
  must still be able to request or consume a global-repair grant.
- `authorized_nodes` — explicit edit permission set for the next
  worker. Required non-empty for Restructure/CoarseRestructure,
  required empty for Local. Must lie inside the scope envelope AND
  the effective anchor's cone, except that
  `consume_global_repair_grant=true` may additionally authorize nodes in
  the pending grant's approved extension set. Nodes in the
  paper-protected semantic closure are not covered by this escape hatch;
  they require the protected semantic-change confirmation/reapproval path.
- `stuck_math_audit` (optional `StuckMathAuditReviewReport`) — reviewer
  notes + optional `reviewer_lean_product` JSON. Required to carry
  content on Continue+reset=None whenever the audit latch is active
  (model.rs); rejected otherwise (§4.8).
- `dismiss_audit_plan` / `dismissed_tasks` — reviewer-driven retirement
  of a StuckMathAudit `audit_plan` or individual tasks within it.
- `difficulty_updates` — per-node easy/hard advisory toggles.
- `allow_new_obligations`, `must_close_active` — proof-formalization
  closure gates. The first controls whether new helpers may remain open
  with `sorry`; the second controls whether the active node must close in
  the next worker burst. `must_close_active = true` also triggers a
  Lean-native local-closure probe of the active node at the burst's
  `proof_worker_delta_step_result` site; the gate rejects the burst
  (with `[axiom]` / `[strict]` / `[shallow]` diagnostic) if the probe
  surfaces non-canonical kernel axioms or open strict deps.
- `comments`, `next_worker_context_mode`, `paper_focus_ranges`,
  `work_style_hint` — advisory hints forwarded to the next worker.
- `clear_human_input` — optional flag to clear the human-input-outstanding
  flag.

**The three-way partition** is how the reviewer resolves the blockers
surfaced by the verifier panels. Each bucket has a specific effect:

- **`task_blockers`** — "this lane verdict is `Fail`; next worker's job
  is to address it." Effect: `apply_review_blocker_adjudication` pins
  `*_status[n] = Fail` + `*_approved_fingerprint[n] = current`. The
  blocker persists in `global_blockers()` (Fail is still a blocker),
  and flows into `pending_task.task_blockers` so the next worker's
  request carries it.
- **`override_blockers`** — "this lane verdict is `Pass`; I side with
  the approving lane." Only blockers listed in
  `allowed_override_ids` are eligible. Effect: pins
  `*_status[n] = Pass` + `*_approved_fingerprint[n] = current`. The
  blocker is removed from `global_blockers()`.
- **`reset_blockers`** — "I want the panel to re-vote on this." Effect:
  `apply_review_blocker_reset` sets `*_status[n] = Unknown` and clears
  `*_approved_fingerprint[n]`. Next cycle's verifier pass picks the
  node up and runs fresh. **Scope restriction**: `reset_blockers` is
  *only* legal in theorem-stating review responses
  (`request_allowed_reset_blockers` at model.rs) — the allowed set
  is empty in proof-formalization and cleanup. Further, even in
  theorem-stating, only blockers whose current state is `Fail`
  (`current_failed_blockers` at model.rs) are eligible for reset;
  split-induced `Unknown` blockers cannot be reset — they must be
  resolved via task or override.

  Practical consequence: in proof-formalization and cleanup, the
  partition reduces to `task ∪ override == request.blockers`. If the
  reviewer disagrees with a unanimous-Fail verdict in those phases,
  the only recourse is a whole-state reset (`LastCommit`/`LastClean`)
  or to accept the Fail and issue the worker a task to address it.

**Guards on adjudication** (`apply_review_blocker_adjudication` at
model.rs):

1. The target of the blocker must be in the relevant
   `latest_*_review_{nodes,targets}` set. This ensures the reviewer can
   only adjudicate things the panel just voted on — not random leftover
   blockers from prior cycles.
2. The current status must be `Unknown`. This makes **unanimous
   verdicts decisive**: if the lanes unanimously said `Fail`, status is
   already `Fail`, the blocker is omitted from `allowed_override_ids`,
   and a reviewer override is illegal. Only split-produced-Unknown
   statuses can be promoted. A reviewer cannot "soft-override" a
   unanimous-Fail into Pass.
3. **No fallback approvedFp**: when `live.*_current_fingerprints` lacks
   an entry for the node, `apply_review_blocker_adjudication` (model.rs)
   skips the `approved_fp` write entirely — matching the verifier-driven
   `apply_corr_updates` / `apply_sound_updates` /
   `apply_target_corr_updates` semantics. `approved_fp = current_fp` is
   the only contract; an adjudication against a missing current never
   produces a definitive lane state.

**`Continue`** decisions routed to `apply_{theorem,proof,cleanup}_review_response`:

- Reviewer's `reset` choice (LastCommit / LastClean) and the three-way
  blocker partition are **mutually exclusive**: `review_response_legal`
  rejects any response with `reset != None` and non-empty
  task/override/reset blocker sets. So in practice the reviewer chooses
  EITHER a whole-state rewind (no partition) OR a partition of the
  current state's blockers (no rewind), never both.
- If `reset != None`: applied first.
  - `LastCommit` restores `live` from `committed` — discarding the
    cycle's in-progress edits. Corresponds at runtime to `git reset
    --hard` of the worker's scratch worktree.
  - `LastClean` goes further: wipes `*_status` and `live.*_current_fingerprints`
    to force re-verification on the rewound worktree. Tied to the
    `supervisor2/clean-NNNNNN` git tag from the last all-blockers-empty
    checkpoint. Intended as "break out of a spiral" — only offered when
    `has_ever_been_clean ∧ cycles_since_clean ≥ 1`. Rejected at legality
    time when combined with `Done` (incoherent) or `AdvancePhase`
    (incoherent: phase advance says "leave this state behind", LastClean
    says "rewind").
- If `reset == None`: the reviewer's three-way partition is applied via
  `apply_review_blocker_resets` (reset → Unknown) then
  `apply_review_blocker_adjudications` (task → Fail, override → Pass).
- Difficulty updates applied.
- `pending_task` is populated with active_node, mode, task_blockers
  filter, next_worker_context_mode, paper_focus_ranges, work_style_hint.
- Proof-formalization global repair:
  - Step A (`global_repair_request != None`) is a non-worker Continue.
    It packages the requested extension nodes and routes to
    `StuckMathAudit`; it is legal on ordinary reviews and on retry
    reviews (`retry_outcome_kind ∈ {Invalid, Stuck, NeedsRestructure,
    Transport}`) because it is the reviewer's universal non-protected
    scope escape hatch.
  - Step B is read-only audit. The auditor may approve a non-empty subset
    of the dependency-neighborhood of the proposed extension nodes, or
    decline with a reason. The kernel must reject approvals that touch the
    paper-protected semantic closure.
  - Step C (`consume_global_repair_grant = true`) is the following
    Restructure/CoarseRestructure Continue. Its `authorized_nodes` may
    include ordinary envelope/cone nodes plus the approved extension
    nodes. If Step C occurs while a retry context is active, it follows
    the retry Continue routing (directly to Worker, same cycle, no
    checkpoint commit); the grant widens scope but does not make the
    retry into a clean new cycle and does not move the active coarse
    anchor.
- Stage transitions:
  - Theorem-stating Continue after Valid: → `Start` (new cycle on next
    `StartCycle`). `commit_live` runs.
  - Proof-formalization Continue: similar. `commit_live` runs on non-retry.
  - Retry continues (`retry_outcome_kind ≠ None`): → `Worker` directly
    (same cycle, `attempt + 1`). `commit_live` does NOT run; retry
    context carried.

**`NeedInput`** — reviewer escalates to a human. Only applies `resets`
(not adjudications; escalating isn't the moment to fix blockers).

- `task_blockers = ∅`
- `override_blockers = ∅`
- `reset_blockers` may still be used when the contract allows them
- `next_active = None`
- `next_mode = request.mode` (neutral / unchanged)

Transitions first to `StuckMathAudit` using the NeedInputAuditor
scenario on the existing stuck-math-audit lane. The auditor sees the
reviewer's reason/comments plus current tablet/history, and either:

- sets `confirm_need_input = true`, which transitions to `HumanGate`
  with `gate_kind = NeedInput`; or
- sets `confirm_need_input = false` and writes an audit plan with
  recovery tasks, which routes back to Review.

The human's response routes back through `apply_human_gate_response`
only after the auditor confirms the escalation.

**`AdvancePhase`** — reviewer declares the current phase complete and
wants to advance.

- Only legal when `request.blockers.is_empty()` (no open blockers).
- Transitions directly to `HumanGate` with `gate_kind = Advance`.
  There is no intermediate combined-panel re-verification stage in the
  live protocol. The human (via `HumanApproveAdvance`) promotes the
  current committed approvals:
  `approved_targets.configured_targets`, `approved_targets.coverage`,
  and `paper_approved_fingerprints`. Phase advances.

**Protected semantic scope** — proof-formalization only. A reviewer may
exceptionally authorize a `coarse_restructure` worker to reopen the
correspondence meaning of specific nodes in the approved target/protected
closure set, but only by naming `protected_semantic_change_node_ids`.
The first such response does not dispatch a worker; the kernel reissues
Review with a warning that this will force human reapproval if the worker
actually reopens those nodes. The reviewer must repeat the same node set,
`next_active`, and `next_mode`, and set
`confirm_protected_semantic_change_scope = true`.

If the accepted worker delta actually reopens an explicitly scoped protected
node, the normal verifier lanes drain first. Once `global_blockers()` is
empty, the kernel routes to `HumanGate` with
`gate_kind = ProtectedReapproval`. Human approval refreshes the approved
coverage/protected-closure/fingerprint snapshot; human feedback returns to
Review with `human_input_outstanding = true`. While
`pending_protected_reapproval_nodes` is non-empty, a checkpoint is not a
clean checkpoint and must not refresh the `LastClean` mirrors or clean git
tag. The approved snapshot becomes clean again only after human approval
clears the pending set and commits the refreshed snapshot.

**`Done`** — cleanup-phase only, signals terminal completion. Requires
`blockers.is_empty()`. Transitions phase to `Complete`. The legality
check rejects Done with non-empty blockers OR non-empty
task/override/reset partition (belt-and-braces under the Cleanup-entry
gate).

### 4.6 Cleanup invariant

**Cleanup is "polish only".** The protocol may only enter Cleanup with
an empty global blocker set, and Cleanup workers may edit only Lean
proof bodies and import structure — never `.tex`, never signatures,
never anything that would re-open a verifier lane (correspondence,
soundness, paper-faithfulness). This invariant guarantees that at every
protocol pause point in Cleanup, the run could legally terminate
("happy stop"). Enforced at three layers:

- **Entry gate**: `formalization_complete()` requires four clauses:
  1. **Textual clean**: every proof_node is closed (not in `live.open_nodes`).
  2. **Blockers clean**: `global_blockers().is_empty()`.
  3. **Local-closure clean**: no proof_node is in `local_closure_unverified_nodes`.
     A sorry-free proof_node whose local-closure record was invalidated (dep
     statement changed, etc.) blocks the transition until the deterministic-
     revalidation pass either refreshes the record or surfaces a real failure
     for reviewer adjudication.
  4. **Records present**: every sorry-free proof_node has an entry in
     `local_closure_records`. The records map invariant
     (`contains_key(N) ⇔ N is sorry-free AND was probed since the most
     recent sorry-free transition`) backstops the stale-pass gap: a
     sorry-free node can never reach the Cleanup gate without a
     post-transition probe record.

  The three sites that transition to `Phase::Cleanup` (engine.rs) all
  gate on `formalization_complete()`, so the precondition propagates
  uniformly.
- **FinalCleanup validator** (`final_cleanup_preserving_step_result`):
  rejects file creation/deletion, `.tex` modifications, modifications
  to non-retained nodes, declaration-hash changes (signature drift),
  and correspondence-fingerprint changes. Build must pass via
  `evaluate_tablet_observation`.
- **Orphan-cleanup validator** (`cleanup_preserving_step_result`):
  this is a proof/theorem-phase cleanup task, not final cleanup. It
  captures a cheap pre-worker Tablet text snapshot, permits deletion only
  of current orphan nodes, and permits retained-node `.lean` edits only
  when removing current-orphan `import Tablet.*` lines makes the before
  and after files identical. It must not do final-cleanup-style all-node
  declaration/correspondence baseline capture or whole-tablet observation;
  Lean checks are limited to retained nodes actually edited by the cleanup.
  If the cheap pre-worker text snapshot is missing, validation fails fast
  and the task must be regenerated; it must not fall back to whole-tablet
  observation.
- **Scoped-tablet validator** (`scoped_tablet_step_result` /
  `check_tablet_scoped`): when the scope is explicit or authorized-node
  scoped, validation observes only those nodes. The only scoped-tablet
  mode allowed to observe every present node is the explicit
  `all_present` mode.
- **Done legality**: rejects with non-empty blockers / partition.

TLA invariant (model-checked under simulation): `phase = "cleanup" =>
GlobalBlockers = {}`.

### 4.7 Cleanup-v2 audit lane (`Stage::CleanupAudit`)

`Phase::Cleanup` enters `Stage::CleanupAudit` instead of going straight
to Reviewer. The kernel dispatches `RequestKind::Audit` to an audit
role that proposes target nodes for substitution / lint-fix tasks. The
audit response (`AuditResponse`, model.rs) carries:

- `new_tasks: Vec<NewCleanupAuditTask>` — appended (with status
  `Pending`, stamped with the current `audit_origin_round`).
- `task_modifications: Vec<CleanupAuditTaskModification>` — may only
  dismiss the audit's own Pending tasks (round-2 may dismiss leftover
  round-1 Pending; terminal-status tasks are immutable).
- `scratchpad_replace: String` — overwrites
  `cleanup_audit_scratchpad`, the audit role's persistent notes.
- `outcome ∈ {NeedToContinue, AuditDone}` — drives next-burst routing.

Multi-round structure (model.rs):

- **Per-round burst cap** `CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND = 5`.
  Reaching the cap forces `AuditDone` regardless of the response's
  outcome field.
- **Total rounds per Cleanup entry** `CLEANUP_AUDIT_MAX_ROUNDS = 2`.
  The reviewer may request a re-audit once (round 1 → round 2); a
  second re-audit is ignored.
- **Force-Done latch on repeated failure**: a Malformed audit response,
  or a validation failure on `new_tasks` / `task_modifications`,
  retries once (`audit_burst_retry_count >= 1`). A second consecutive
  failure forces `AuditDone`, increments `cleanup_audit_burst_count`
  (so the reviewer Continue branch routes through cleanup-v2 sub-cases,
  not the legacy fallback), and stamps
  `latest_audit_rejection_reason` (engine.rs). The same
  force-Done path fires on second consecutive validation failure
  (engine.rs).

`AuditDone` (or the cap / force-Done latch) transitions to
`Stage::Reviewer` via `transition_audit_to_reviewer`
(engine.rs). The reviewer inspects `cleanup_audit_tasks` and
either dispatches a task to the cleanup worker, dismisses one, or
finalizes.

`enter_cleanup_phase` (engine.rs) resets all audit-round state on
phase entry (`cleanup_audit_tasks`, `cleanup_audit_scratchpad`,
`cleanup_audit_burst_count`, `cleanup_audit_round = 1`,
`cleanup_active_task`, `cleanup_force_done`, `latest_audit_rejection_reason`,
`audit_burst_retry_count`).

A consecutive-Invalid worker safety bound (`CLEANUP_CONSECUTIVE_INVALID_THRESHOLD = 3`,
model.rs) forces auto-Done if cleanup workers keep returning
Invalid — this prevents a wedged cleanup task from burning the
reviewer budget indefinitely.

### 4.8 `StuckMathAudit` / NeedInputAuditor role (`Stage::StuckMathAudit`)

An independent **read-only** audit lane used for repeated
proof-formalization no-progress and for NeedInputAuditor review of a
reviewer's proposed `NeedInput` escalation. Distinct from cleanup-v2
audit: this is a deeper-analysis prompt that emits a durable artifact
handed forward to subsequent reviewers/workers, not a cleanup task-list
builder.

**Activation triggers** (model.rs, `refresh_stuck_math_audit_latch`,
called from `expected_request` and on every checkpoint commit). Either
trigger activates the sticky `stuck_math_audit.active` latch:

- **Trigger A** — `cycles_since_clean >=
  stuck_math_audit_cycles_since_clean_threshold()` (default 5,
  override via `TRELLIS_STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD`)
  AND at least one open soundness/substantiveness blocker in
  ProofFormalization.
- **Trigger B** — `cycles_since_shallow_coarse_closed_count_increase >=
  stuck_math_audit_shallow_coarse_no_progress_threshold()` (default 5,
  override via `TRELLIS_STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD`).
  The coarse-DAG shallow-closure progress metric has not improved for
  N checkpoint cycles.

The latch is cleared by `refresh_stuck_math_audit_latch` only when
`global_blockers().is_empty() AND last_clean_rewind_count == 0 AND
!force_review_after_cone_clean` — i.e. a genuinely new clean
checkpoint (not a LastClean rewind landing on a known clean tag). A
NeedInputAuditor context or NeedInputAuditor-produced plan also keeps
the latch visible until that escalation path is consumed/dismissed.

**NeedInputAuditor activation** is direct, not threshold based: any
reviewer `NeedInput` response routes to `Stage::StuckMathAudit` with a
`need_input_audit` context recording the review request/cycle, phase,
active/held node, mode, reviewer reason/comments, and whether the
event came from an invalid retry.

**Dispatch** (`should_dispatch_stuck_math_audit` engine.rs,
called from `issue_review_or_stuck_math_audit` engine.rs). When
the latch is active, the kernel substitutes a `RequestKind::StuckMathAudit`
for what would have been a Reviewer dispatch, subject to a cadence
gate:

- Cooldown when no `audit_plan` exists yet: at least
  `stuck_math_audit_dispatch_cooldown_cycles()` cycles since the last
  dispatch (default 1, override `TRELLIS_AUDIT_DISPATCH_COOLDOWN_CYCLES`).
- Re-audit interval when an `audit_plan` is on the table: at least
  `stuck_math_audit_reaudit_interval_cycles()` cycles (default 4,
  override `TRELLIS_STUCK_MATH_AUDIT_REAUDIT_INTERVAL_CYCLES`) — the
  re-audit sees the prior plan and may confirm, refine, or replace it.
- **Forced after reset/rewind**: `force_stuck_math_audit_after_rewind`
  is consumed once after LastClean / cone-clean / audit-authorized
  reset, dispatching the audit role exactly once on the restored state
  even if the usual triggers don't fit.

**Response shape** (`StuckMathAuditResponse`, model.rs-…):

- `confirm_need_input: bool` — legal only for NeedInputAuditor
  contexts. If true, the auditor confirms the human escalation. If
  false in a NeedInputAuditor context, at least one recovery task is
  required.
- `report: String` — 200..20_000 chars (`AUDIT_REPORT_TEXT_{MIN,MAX}_CHARS`).
- `tasks: Vec<AuditTask>` — each task has unique non-empty id, title,
  body, with the reviewer-dismissal fields required absent.
- `probe_paths: Vec<String>` — disk paths to scratch Lean/math probes.
- `cone_clean_node: Option<NodeId>` — when present, MUST be a
  `resettable_theorem_stating_nodes` member; triggers an
  audit-authorized cone clean (§4.9).

Validation failure or Malformed retries once
(`STUCK_MATH_AUDIT_BURST_RETRY_LIMIT = 1`). For ordinary StuckMathAudit,
a second failure transitions to Reviewer without a new plan and pins
`latest_stuck_math_audit_rejection_reason`; for NeedInputAuditor, a
second failure routes to HumanGate rather than dropping the reviewer's
escalation on the floor.

On success, the plan replaces `state.audit_plan` (the prior plan moves
to `superseded_audit_plan`). If `cone_clean_node` is set, the kernel
fires `apply_audit_authorized_theorem_stating_node_reset` (§4.9).
Otherwise, ordinary StuckMathAudit dispatches Reviewer next.
NeedInputAuditor dispatches HumanGate when `confirm_need_input = true`,
and dispatches Reviewer with the recovery `audit_plan` when false.

**Reviewer contract under audit latch** (model.rs): on
Continue+reset=None while `stuck_math_audit.active`, the reviewer's
`stuck_math_audit` field must carry `notes` or a meaningful
`reviewer_lean_product` (JSON ≤ `STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
= 20_000`). On other decisions (reset, NeedInput, AdvancePhase) the
field must be empty. This makes the reviewer's audit-aware reasoning a
durable per-cycle artifact while the latch is on.

### 4.9 Cone clean reset (audit-authorized)

A **cone clean** is a targeted reset of a single coarse-DAG node's
down-closure cone — `RestoreTheoremStatingNodeAndPruneOrphans { node }`
in `apply_audit_authorized_theorem_stating_node_reset` (engine.rs).
Used to recover from accumulated drift in a specific coarse subtree
without `LastClean`'s full rollback. Authorized only by a
StuckMathAudit response's `cone_clean_node` field — there is no
reviewer-facing menu item for it.

Effects:

- Disk: restores the named coarse node to its theorem-stating snapshot
  and prunes the orphaned helper cone.
- State: clears `held_target`, `active_node`, `target_edit_mode`,
  `proof_edit_mode`, retry context, `pending_task`, all
  `latest_*_review_*` contexts, `pending_protected_*`.
- Anchor: if the cleaned node IS the current `active_coarse_node`,
  clears it so the next Reviewer reseeds via
  `kernel_hinted_next_active_coarse_nodes`. `cycles_in_coarse_repair_mode`
  always resets to 0 (cycle context changed).
- Sets `force_review_after_cone_clean = true` so the next dispatch
  routes to Reviewer (not back to StuckMathAudit), and the audit latch
  stays visible (refresh helper preserves it under this flag).
- Emits `CommitCheckpoint`.

The reset is not a clean checkpoint (a Substantiveness or Corr blocker
on the restored cone is expected); the cycle proceeds through the
normal verifier drain on the next Worker burst.

### 4.10 Active coarse anchor (proposal v32)

In `Phase::ProofFormalization`, the kernel locks the worker / reviewer
into a single **coarse-DAG anchor** at a time, with a starvation
escape valve. The mechanism is dormant whenever `coarse_dag_nodes`
is empty (pre-implementation runs, or before AdvancePhase populates
the coarse DAG).

**State** (model.rs, mirrored to the request envelope):

- `active_coarse_node: Option<NodeId>` — the locked anchor. `None`
  outside ProofFormalization (TypeOK invariant SupervisorProtocol.tla);
  always cleared at the four cleanup-entry sites (engine.rs) and
  on cone-clean of the anchor (§4.9).
- `cycles_in_coarse_repair_mode: u32` — starvation-guard counter.

**Derived predicates** (model.rs):

- `coarse_repair_mode()` — TRUE iff some task-blocker carrier lies
  outside `coarse_node_support_cone(active_coarse_node, ...)`. Tells
  the reviewer prompt to reframe the cycle as "repair these blockers,
  not new formalization."
- `coarse_legal_active_set()` — base case is the anchor's down-cone;
  in `coarse_repair_mode()` widens to include each task-blocker
  carrier and its own down-cone. The `active_node_legal` predicate
  intersects with this set in ProofFormalization (model.rs,
  SupervisorProtocol.tla). Dormant set is `live.present_nodes`.
- `active_coarse_change_allowed()` — TRUE under four conditions:
  (1) `coarse_dag_nodes` empty (dormant); (2) no anchor yet; (3)
  **clean unlock**: anchor is shallow-closed AND no global blockers;
  (4) **starvation escape**: `cycles_in_coarse_repair_mode >=
  stuck_coarse_repair_threshold()` (default 8, override
  `TRELLIS_STUCK_COARSE_REPAIR_THRESHOLD`).
- `kernel_hinted_next_active_coarse_nodes()` — candidate anchors
  surfaced on Review requests. Empty whenever change is not allowed
  (so the reviewer's `next_active_coarse` validator rejects every
  switch attempt — the anchor is locked).

**Request surface fields on Review** (model.rs):

- `active_coarse_node` — current anchor (also on Worker requests).
- `kernel_hinted_next_active_coarse_nodes` — Review-only.
- `coarse_repair_mode: bool` — true iff any task-blocker carrier is
  outside the anchor's cone.
- `cycles_in_coarse_repair_mode: u32` — starvation counter, surfaced
  so the reviewer prompt can mention "anchor lock has been forced
  open by the starvation guard" if relevant.
- `coarse_anchor_starvation_unlocked: bool` — true iff the lock is
  currently open ONLY because the starvation guard fired (audit-2
  followup #8). Distinguishes "clean unlock — anchor work done, pick
  next coarse goal" from "forced unlock — switch anchor to break the
  spin."

**Reviewer move on ReviewResponse**: `next_active_coarse: Option<NodeId>`.
Legal only on Continue + reset=None + `retry_outcome_kind=None` in
ProofFormalization, and only when the chosen node is in
`kernel_hinted_next_active_coarse_nodes`
(`review_next_active_coarse_legal_for_response` model.rs).
This anchor-switch gate does not constrain `global_repair_request` or
`consume_global_repair_grant`: retry reviews may be unable to move the
anchor directly, but they must still be able to request audit-gated
authorization for a non-protected out-of-cone node.

When the reviewer simultaneously sets `next_active` and
`next_active_coarse`, the `next_active` legality is evaluated against
the PROSPECTIVE anchor's cone — using
`proof_active_node_base_legal_candidates` (model.rs) — so a
legal one-cycle anchor switch isn't rejected by the OLD anchor's cone
narrowing (audit-2 followup #3).

**Cleanup-entry clears**: leaving ProofFormalization clears the anchor
and the starvation counter (engine.rs). The four production
transition sites that set `phase = Phase::Cleanup` (
`apply_proof_paper_accept`, `apply_proof_corr_accept`,
`apply_proof_sound_response`, `apply_proof_review_response`'s
formalization-complete branch) all route through `enter_cleanup_phase`
(engine.rs) which performs the clear. The TLA invariant
`phase ≠ proof_formalization ⇒ activeCoarseNode = NoNode`
(SupervisorProtocol.tla) is enforced at this single choke point.

A LastClean / cone-clean rewind may land the live state in a
configuration where `active_coarse_node` is no longer present;
`relegalize_active_coarse_anchor` (engine.rs) clears the anchor in
that case so the next Review reseeds, and always resets the starvation
counter since the cycle context changed.

### 4.5 `HumanGate`

Two flavors:

- `gate_kind = Advance`: human approves or rejects phase advance.
  Approval promotes `approved_*` structures; rejection routes back to
  `Reviewer` with `humanInputOutstanding = true`.
- `gate_kind = NeedInput`: reached only after the NeedInputAuditor confirms
  a reviewer `NeedInput` escalation. The human provides guidance (via
  `HUMAN_INPUT.md`); the kernel routes back to the phase's `Reviewer`.
- `gate_kind = ProtectedReapproval`: human reapproves an explicitly scoped
  protected semantic reopen after verifier drain, or sends it back to Review.

## 5. Reviewer blocker actions: durable status writes

**Every reviewer action on a split panel produces a durable status
write.** There is no ephemeral "this-cycle bypass" state. A reviewer
facing a split:

- Picks `task_blockers` → status pins to Fail; next worker sees the
  blocker as a task. Available in all three phases.
- Picks `override_blockers` → status pins to Pass; blocker cleared.
  Available in all three phases.
- Picks `reset_blockers` → status goes back to Unknown; next verify
  re-runs the panel. **Theorem-stating only**, and only for blockers
  whose current state is `Fail` — see Section 4.4.

Each choice produces a permanent state transition (modulo fingerprint
drift later invalidating it).

The three buckets are **partial actions** over the request's blocker
set, not a complete partition. The kernel's
`review_response_rejection_reasons` (model.rs) enforces only
`task_blockers ⊆ request.blockers`, the bucket-source subset checks
for `override_blockers` / `reset_blockers`, and pairwise disjointness;
it does NOT require `task ∪ override ∪ reset == request.blockers`.
Omitted blockers stay live — they simply persist into the next cycle
under their current status. The reviewer can thus "defer" a blocker
by leaving it out of all three buckets when a single-cycle worker
burst can only realistically address a subset of what's open.

**Note on unanimous verdicts**: these never reach the reviewer as an
override-eligible blocker because the panel's unanimous vote already
pinned status + approvedFp. A unanimous `Fail` does leave the blocker in
`global_blockers` (because status≠Pass), but that blocker is omitted
from `allowed_override_ids`, so an override is illegal. In
theorem-stating, the reviewer's recourse for a unanimous-Fail is
`reset_blockers` — forcing a re-vote after (presumably) editing the
content. In proof-formalization and cleanup, `reset_blockers` is
unavailable; the reviewer must either accept the Fail (assign the
blocker to `task_blockers` so the worker addresses it), leave it live
for a later cycle to handle, or fall back to `LastCommit`/`LastClean`
whole-state resets.

### 5.1 Blocker action asymmetry

The three buckets have **different downstream effects** beyond their
status writes:

- **`task_blockers`** — durable status write (Fail) AND routed forward
  to the worker via `pending_task.task_blockers`. The next Worker
  request's `blockers` field carries exactly this set (model.rs).
  Worker prompts render the blocker as a task to address. The reviewer
  must also satisfy `task_blockers_outside_review_worker_scope == {}`
  (model.rs): every task-bound blocker must be coverable
  by the next worker's authorized edit envelope. Node-bound task
  blockers route to the worker only if the carrier node is in
  `authorized_nodes` (or, for Local, equals the active node). Target-
  bound `PaperFaithfulness` task blockers must have some covering node
  in the authorized set.
- **`override_blockers`** — durable status write (Pass) only. The
  worker does NOT see overridden blockers in its `blockers` field
  (they're cleared from `global_blockers()` once status flips to
  Pass). The override is purely a kernel-internal state change; no
  prompt-context breadcrumb is generated.
- **`reset_blockers`** — durable status write (Unknown, clear
  approvedFp). Triggers next-cycle verifier re-vote on the named
  blocker; no worker routing. Limited to theorem-stating + current
  state == Fail (§4.4).

**Soundness carve-out (cross-mode case)** (`review_response_legal`
at model.rs). Normally `task_blockers` under Local mode are
rejected — Local doesn't authorize any cross-node edits, so it can't
cover most blockers. The exception: Soundness on the active node.
Soundness auto-clears when the active node becomes sorry-free
(`needs_sound = false`), and closing the proof IS within Local's
scope (a `.lean`-proof-body edit). So `Local + must_close_active +
task_blockers = [active_node_soundness_id]` is legal — the worker is
expected to close the proof, which simultaneously clears the
Soundness blocker. Non-Soundness task blockers under Local remain
illegal (`task_blockers.iter().any(|b| !matches!(b.kind,
BlockerKind::Soundness)) ⇒ reject`).

`worker_authorized_nodes_for_request_assignment` for ProofLocal
returns an empty envelope, but `task_blockers_outside_review_worker_scope`
treats the active node itself as in-scope Local coverage
(model.rs) so the carve-out passes the scope check.

## 6. Retry and escalation

**Proof-formalization invalid-retry ladder** (per node):

1. Worker produces Invalid. `attempt` goes 1 → 2. Same worker
   configuration re-issued.
2. If 2nd attempt also Invalid: escalate to Reviewer with
   `retry_outcome_kind = Invalid`. Reviewer sees the invalid-context
   prompt variant, may choose to swap active_node, escalate difficulty,
   or NeedInput.
3. Reviewer Continue from retry-Invalid → new Worker cycle with fresh
   context (if `next_worker_context_mode = "fresh"`) or resumed context
   (if `"resume"`).

**Difficulty hinting**. A node's `node_difficulty ∈ {Easy, Hard}` is an
advisory worker-selection hint, not a scope rule. It does not decide
whether helper obligations may remain open or whether the active node must
close; those are reviewer-selected closure gates. Easy invalid attempts may
still increment `easy_attempts[node]` and auto-escalate the hint to Hard at
`easy_max_retries` (default 2). Reviewer can override difficulty via
`difficulty_updates`.

**Stuck / NeedsRestructure retry**: Stuck has the same threshold logic
as Invalid. NeedsRestructure always escalates to Reviewer — it's a
proposal, not a failure.

## 7. Resets

Four kinds, increasingly aggressive (cone clean is a peer of LastClean
but narrower in scope):

**`reset_blockers` (per-lane)**: resets `*_status[node] = Unknown`,
clears `*_approved_fingerprint[node]`. Next verify re-runs that lane.
Worktree untouched.

**`ResetChoice::LastCommit`**: `restore_committed()` (model.rs)
copies `committed` → `live` (including node_kinds, proof_nodes, deps,
target_claims, present_nodes, open_nodes, coverage, all current
fingerprints). At runtime this corresponds to a `git reset --hard` of
the worker scratch worktree to the last committed checkpoint. Verifier
status maps are NOT cleared (they still reference what was approved
against those committed fingerprints). Only offered when
`retry_outcome_kind != None` (`request_allowed_resets` at
model.rs) — LastCommit is a retry-flow tool.

**`ResetChoice::LastClean`**: `apply_last_clean_reset()` (model.rs)
wipes all three `*_status` maps AND the live + committed
`*_current_fingerprints` maps (paper is preserved to satisfy an
invariant). This forces re-verification of every present node against
whatever's in the rewound worktree. At runtime, the runtime performs
`git reset --hard <supervisor2/clean-NNNNNN>` (the most recent clean
tag). `cycles_since_clean` resets to 0. Only offered when
`has_ever_been_clean` (at least one prior clean checkpoint) and
`cycles_since_clean ≥ 1`. Done + LastClean is explicitly rejected as
incoherent. Note that `apply_last_clean_reset` does NOT
touch `live.present_nodes` / `live.deps` / `live.target_claims` or
their committed counterparts — these may be briefly out of sync with
the actually-rewound repo, until the next worker/verifier normalize
pass re-observes structural state from disk (comment at
model.rs).

All three resets preserve `*_approved_fingerprints` (advance-gate
approvals survive worktree rewinds). Only `reset_blockers` clears them,
and only for the named node.

**Audit-authorized cone clean** (`apply_audit_authorized_theorem_stating_node_reset`,
engine.rs). Narrower than LastClean: restores a single coarse-DAG
node's theorem-stating snapshot and prunes orphans. Authorized only
by a StuckMathAudit response's `cone_clean_node` field — never
reviewer-driven, never offered on the reset menu. See §4.9 for full
state effects.

## 8. Orphan cleanup

An **orphan** is a node in `present_nodes` not reachable from any
configured target via the coverage closure. Orphans arise when a worker
adds helpers via restructure but the reviewer later drops the parent
node, or when a worker deletes a parent but not its helpers.

`orphan_cleanup_needed()` returns true if any orphan exists. When true,
`schedule_orphan_cleanup` takes control of the next worker cycle:

- Sets `active_node = None`, `proof_edit_mode = CoarseRestructure` (so
  the worker can delete nodes), `work_style_hint = "restructure"`.
- `pending_task.orphan_cleanup_nodes` populated with the orphan set.
- Worker is expected to remove or re-link orphans.
- Validation is intentionally narrower than final cleanup: it checks the
  cleanup delta and any retained node whose imports changed, but it does
  not re-observe every Tablet node.

Orphan cleanup can trigger at multiple `schedule_orphan_cleanup` call
sites in engine.rs:

- `reject_theorem_worker_response` invalid-retry continuation
- `reject_cleanup_worker_response`
- `restore_retryable_worker_task` (proof-worker retry path)
- `start_cycle` (orphans present when a cycle begins)
- theorem-stating Valid-with-delta that introduced orphans
- theorem Stuck retry branch
- proof-formalization Valid-with-delta that introduced orphans
- cleanup Valid branch
- `apply_proof_review_response` retry flow when
  `active_node.is_none()` and orphans exist

When `schedule_orphan_cleanup` fires inside `apply_proof_review_response`
retry flow, it **overwrites** `pending_task` including any
`task_blockers` the reviewer just set. This is intentional — orphan
cleanup is a distinct work stream — but note: the reviewer's blocker
adjudications (status + approvedFp writes) have already landed by then,
so those blockers persist in `global_blockers()` and will re-surface on
the next review cycle after cleanup completes.

## 9. Invariants enforced by `validate()`

Called at the end of every `apply_event` (engine.rs). Any failure
aborts the transition with `InvariantViolation`.

Selected invariants:

- `active_node` is legal in `live` (present, and in proof-formalization,
  also in `open_nodes`).
- `held_target` only exists in theorem-stating, and only on a node that
  is present, open, a proof-node, and has `current_corr_pass`.
- `target_edit_mode` must be `Global` outside theorem-stating.
- `proof_edit_mode` must be `Local` outside proof-formalization.
- `pending_task`, if present, has `task_blockers ⊆ global_blockers()`,
  `node = active_node`, `mode = current_mode()`.
- `pending_task` may only exist in `Start` or `Worker` stage.
- Every live or committed present node has `node_difficulty` and
  `easy_attempts` entries (model.rs). Hard-difficulty nodes
  must have zero `easy_attempts`.

Before the recent refactor, `validate()` also enforced
`stored_overrides ⊆ global_blockers()`. This was removed alongside
the field.

## 10. Edge cases and runtime-observable scenarios

**Worker returns `Valid` with no semantic delta**: the kernel skips
verifier stages and routes directly to `Reviewer`. This is the "worker
confirms current state is correct" path, common for proof closure when
verification already ran on the same fingerprints.

**Worker returns `Valid` that introduces orphans**: orphan-cleanup is
scheduled before verifiers run, so the verifiers never see the
transient orphan state.

**Worker returns `Valid` with a delta that CHANGES an approved node's
fingerprint**: the next `global_blockers()` call will observe
`current_corr_pass = false` for that node (approved ≠ current), which
surfaces a blocker to the reviewer. This is the "fingerprint drift
invalidates prior approval" mechanism.

**Verifier panel malformed**: response ignored, request is re-issued
(stutter in the current stage). The bridge should eventually produce
a parseable response or time out.

**Review response illegal**: reviewer's response fails `review_response_legal`
(missing fields, partition doesn't cover all blockers, next_active not
in allowed set, etc.). Kernel re-issues the review request. No state
change.

**Runtime restart mid-cycle**: kernel reads protocol_state.json, sees
`in_flight_request = Some(req)`, and expects the bridge to deliver a
response for that exact request id. The bridge in turn either reads the
response file from staging (if the agent had produced one before the
restart) or re-issues the agent call. In-flight requests are
idempotent-by-design.

**Canary edge: stored_overrides in old checkpoints**. Pre-refactor
checkpoints have a `stored_overrides: [...]` field. On the new kernel,
serde silently ignores unknown fields on deserialize; the field is not
referenced anywhere; old checkpoints load fine. The file is rewritten
cleanly (field absent) on the next `commit_live`.

**Mid-cycle phase advance**: never happens. `Phase` only transitions at
well-defined points — `HumanApproveAdvance` (TheoremStating →
ProofFormalization), `apply_sound_response` or `apply_proof_paper_accept`
(ProofFormalization → Cleanup when formalization complete), and
`ReviewDoneCleanup` (Cleanup → Complete). All three happen between
cycles, not inside one.

**Paper-target correspondence reopen guard**: at worker-delta commit
time (in `runtime_cli_observations.rs`), the kernel checks whether
any paper-target-covering node's `CorrespondenceFingerprint` has drifted
on multiple axes (`own_tex`, `lean_semantic_closure`, `preamble_tex`,
`definition_descendants`). If so, the worker's delta is rejected at the
commit gate, producing an Invalid outcome. This is the replacement for
the retired `ProtectedNodes`/`ProtectedSnapshot` mechanism and ensures
that already-approved target correspondence cannot be silently reopened
by a worker edit.
