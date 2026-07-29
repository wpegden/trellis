# Trellis Process Rules

A capability-and-mechanism reference for every agent in a trellis run: what each role can
legally do, and every process mechanism available. Each statement maps to kernel behavior
(anchors named in parentheses; source in `kernel/src/`). Tunable constants are named, never
inlined as values — the kernel's current value is authoritative.

## The run loop

Each cycle the kernel dispatches exactly one request — Worker, Review, a verifier panel,
StuckMathAudit, cleanup Audit, or HumanGate — and applies the response (`start_cycle`,
`apply_event`). A burst sees its structured request file, its role's prompt fragments, and
the repository worktree. Every applied response is checkpointed as a git commit
(`commit_live`); a checkpoint with an empty blocker set is additionally tagged as a clean
checkpoint. A `LastClean` rewind is a `git reset --hard` plus untracked-file sweep to the
newest clean tag — files dropped into the repo after that tag are deleted by the rewind.

## Phases

- **TheoremStating** — statement authoring: workers build the `.tex`+Lean statement DAG;
  all verifier lanes run; ends at a human-approved phase advance.
- **RevisionStating** — statement editing over an imported, populated tablet (revision and
  add-targets modes); shares TheoremStating gating (`is_theorem_stating_like`) and begins
  with a revision-planner audit.
- **ProofFormalization** — proof closure over the stated targets; coarse-anchor routing,
  cone clean, global repair, and node retirement are live here.
- **Cleanup** — audit-driven cleanup rounds (`RequestKind::Audit` + worker tasks).
- **Complete** — terminal; revivable by add-targets mode.

The `AdvancePhase` decision is legal only when the global blocker set is empty
(`global_blockers`), and the advance itself is human-approved (`GateKind::Advance`). After
the advance, the first request of the new phase is a routing Review
(`post_advance_routing_pending`). Human gates also serve protected-node reapproval
(`GateKind::ProtectedReapproval`) and NeedInput escalations (`GateKind::NeedInput`).

## Worker

Edit scope is set by the reviewer's routing: `next_mode` (`TaskMode`: Global / Targeted /
Local / Restructure / CoarseRestructure / Cleanup), `authorized_nodes`,
`must_close_active` (the active node must arrive Lean-closed — sorry-free with its local
closure verified), and `allow_new_obligations` (whether the burst may add new open nodes).
A worker may:

- create, edit, and delete Tablet nodes within the task's scope; `deleted_nodes` must list
  exactly the nodes removed;
- claim targets: `target_claim_updates` (paper targets), `challenge_claim_updates` (PV
  challenge targets; at most one challenge target per node);
- create new live orphan nodes only while the uncovered-target construction window is open
  (`orphan_construction_window_open`: some configured paper target has an empty covering
  set, judged on the pre-burst snapshot). With the window closed, the same-burst orphan
  gates reject a valid response that leaves new live orphans; in Cleanup the orphan-free
  requirement is absolute;
- request deviations (`deviation_requests`), claim authorized deviations on nodes
  (`node_deviation_claims`), delete obsolete deviation records (`deviation_deletions`),
  and ground statements in reference papers (`node_reference_grounds`);
- declare a semantic change to protected nodes (`protected_semantic_change_nodes` +
  `confirm_protected_semantic_change_scope`), which routes through the reapproval gate;
- call for an audit (`audit_request`, advisory — surfaced to the next reviewer, who
  forwards it);
- challenge process-memory entries (`memory_challenges`); writing entries is the audit
  lane's authority;
- report outcome `Valid` / `Invalid` / `Stuck` / `NeedsRestructure` (with
  `needs_restructure_suggested_nodes`) / PV `TargetFalseUnderModel` (with
  `under_model_disproof`; the audit lane adjudicates it);
- PV: author a candidate under-model assumption `C` when the reviewer has enacted
  authoring (`assumption_authoring_request`); the assumptions lane and a human gate decide
  its fate — authoring and gating are separate authorities.

## Reviewer

Decisions (`ReviewDecisionKind`, gated per phase by `request_allowed_decisions`):
`Continue`, `AdvancePhase`, `NeedInput`, `Done` (Cleanup). On a `Continue` the reviewer
may:

- route the next worker burst: `next_active`, `next_mode`, `must_close_active`,
  `allow_new_obligations`, `authorized_nodes`; move the active coarse anchor
  (`next_active_coarse`, drawn from `kernel_hinted_next_active_coarse_nodes`);
- act on blockers: `task_blockers` (hand a blocker set to a worker task), `reset_blockers`
  (mark lanes Unknown for re-verification);
- rewind: `reset` ∈ `LastCommit` / `LastClean` (with `preserve_process_memory`, default
  true, controlling whether `process-memory/` survives the rewind) /
  `TheoremStatingNode` (ProofFormalization Continue only: single-node theorem-stating
  reset, node from `resettable_theorem_stating_nodes`);
- work the live audit plan: dismiss individual tasks (`dismissed_tasks`) and retire the
  whole plan (`dismiss_audit_plan`) once nothing live remains;
- attest paper grounding (`paper_grounding`, required in friction states and whenever
  `paper_focus_ranges` is nonempty);
- update node difficulty (`difficulty_updates`) and clear served human input
  (`clear_human_input`);
- Cleanup: dismiss cleanup tasks (`cleanup_dismiss_tasks`), pick the next task
  (`cleanup_next_task`), request a re-audit (`cleanup_request_reaudit`);
- request Sound verification (`request_sound_verifier_nodes`);
- call for an audit (`audit_request`, own or forwarding a worker's; rate-limited by
  `stuck_math_audit_dispatch_cooldown_cycles`);
- open a global-repair request (Step A, `global_repair_request`) and later consume a grant
  (Step C, `consume_global_repair_grant`);
- enact or decline a pending node retirement (`dispatch_node_retirement` /
  `node_retirement_decline_reason`);
- PV: enact assumption authoring (`assumption_authoring_request`);
- challenge process-memory entries (`memory_challenges`).

## Verifier lanes

Each lane's canonical rubric lives at the repository root; the rubric is the lane's
definition.

- **Correspondence** (`RequestKind::Corr`) — the node's Lean matches its own `.tex`
  statement: `CORRESPONDENCE.md`.
- **Paper-Faithfulness** (`RequestKind::Paper`, target-bound) — the target's covering set
  expresses the paper claim; empty coverage is a definite Fail: `FAITHFULNESS.md`.
- **Soundness** (`RequestKind::Sound`) — the natural-language proof is mathematically
  valid. This is a prose-proof lane; Lean closure is a separate mechanical gate:
  `SOUNDNESS.md`.
- **Substantiveness** (rides the Paper request, node-bound) — the statement is a
  substantive, unweakened paper claim: `SUBSTANTIVENESS.md`.
- **Deviation** (rides the Paper request) — authorizes a requested minor deviation:
  `DEVIATIONS.md`.

Lane verdicts open and close blockers (`BlockerKind`); the reviewer adjudicates definite
Fails through the blocker actions above.

## Audit lane (StuckMathAudit)

One lane burst at a time (the audit-role mutex in `ProtocolState::validate`). Triggers:
stagnation gates (no-Sound-progress window, `cycles_since_clean`, shallow-coarse
no-progress), an on-demand request, the forced-after-rewind flag, and the coverage
re-planning cadence. The eight scenarios and what each may output:

1. **Structural audit** (no carrier) — `report` + `tasks` + `probe_paths`; may order a
   cone clean (`cone_clean_node`) or, in ProofFormalization, node retirement
   (`node_retirement_request`).
2. **NeedInput audit** (`need_input_audit`) — adversarially re-derives a reviewer's
   escalation: `confirm_need_input` true opens the human gate; false returns recovery
   tasks.
3. **Global-repair adjudication** (`pending_global_repair_request`) —
   `global_repair_approve` + `global_repair_approved_extension_node_ids` (a minimal
   subset) or a decline reason.
4. **Gap-research planner** (`gap_research`) — a natural-language proof route
   (`route_tex`); sets `route_needs_human` only when the gap needs genuinely new
   mathematics.
5. **Gap-plan critic** (`gap_plan_critique`) — independent `gap_decision`
   accept/reject over the route (context-isolated from the planner); on accept writes the
   implementing plan.
6. **Revision planner** (`revision_planning`) — the revision route: `report` + `tasks` +
   structured `revision_actions`.
7. **Initial / coverage planner** (`initial_planning`) — the advisory construction plan
   (`report` + `tasks`); runs at cycle 1 on every fresh run and re-runs on the coverage
   cadence while configured paper targets remain uncovered (challenge targets never
   drive the cadence).
8. **Assumptions lane** (PV, `assumptions_lane`) — gates a worker-authored `C`:
   `assumptions_lane_verdict` pass/reject with the recorded hunt result.

PV Decide runs: the audit lane is the sole authority for flipping a pair's live polarity
(`set_live_polarity`), and for ruling a worker's under-model claim (`under_model_ruling`:
bug / deviation / reject).

**Plan lifecycle:** trigger → accepted plan (`AuditPlan`: report + tasks) → the reviewer
works the tasks, dismissing each as completed or stale → `dismiss_audit_plan` retires the
plan → a newer accepted planner plan supersedes a live one, with the prior plan and its
dismissal trail kept in `superseded_audit_plan`. While a planner-origin plan is live, the
stagnation triggers are suppressed — the lane defers to the plan rather than
second-guessing it, and the reviewer's `dismiss_audit_plan` is what re-enables ordinary
stagnation audits (`should_dispatch_stuck_math_audit`); the coverage cadence, where
armed, is the exception and keeps running so the planner can amend its own plan.

**Task sequencing:** Sound dispatch — including reviewer-requested re-verification,
which is remembered rather than dropped — waits until every statement lane
(Correspondence, Substantiveness, Paper-Faithfulness, Deviation) is clear across the
run (`sound_dispatch_surface_blocked`); order plan tasks so statement-lane repairs
precede the Sound certifications that depend on them.

## Mechanism inventory

- **Deviations lane** — a worker requests an authorized minor departure from the paper
  (`deviation_requests`); the Deviation verifier authorizes it against `DEVIATIONS.md`.
- **Global repair** — reviewer requests out-of-cone authorization (Step A), the audit lane
  adjudicates (Step B), the reviewer consumes the grant (Step C,
  `pending_global_repair_grant`; TTL `global_repair_grant_ttl_cycles`).
- **Cone clean** — a structural audit restores a coarse node to its theorem-stating
  snapshot and prunes its orphaned helper cone (`cone_clean_node`); the next slot is
  forced to Review (`force_review_after_cone_clean`).
- **Process memory** — durable cross-burst findings under `process-memory/`
  (`MemoryOperation`: add / supersede / retire). The structural, need-input, and
  global-repair audit scenarios write `memory_operations`; operations from any other
  lane are rejected (`stuck_math_audit_validation_failure`). Workers and reviewers
  challenge (`memory_challenges`), and the next audit adjudicates the pending
  challenges.
- **Protected-node reapproval** — a worker or reviewer declaring a semantic change to
  protected nodes (`protected_semantic_change_nodes` +
  `confirm_protected_semantic_change_scope`) interposes a human gate before the change
  stands (`maybe_issue_protected_reapproval`, checked in `start_cycle` right after the
  planner guards; `GateKind::ProtectedReapproval`).
- **Node retirement** — a ProofFormalization audit orders deletion of named present,
  non-coarse, non-protected nodes (`node_retirement_request`); the reviewer enacts it as
  the next worker task or declines with a reason.
- **Assumptions lane (PV)** — the authoring-then-gating pipeline for under-model
  assumptions: reviewer enacts authoring, worker authors `C`, the assumptions-lane burst
  gates it, a human `AssumptionReview` gate approves it.
- **On-demand audit requests** — workers (advisory) and reviewers (dispatching) summon an
  audit with a reason (`AuditRequest`: approach or re-examine-prior-report); the dispatch
  bypasses the ordinary audit interval.
- **Revision mode** — `import_revision_project` starts a `RevisionStating` run against a
  newer paper version: frozen/editable node envelope, revision planner first, structured
  `revision_actions`.
- **Add-targets mode** — `add_paper_targets_to_state` revives a Complete run into
  `RevisionStating` to state additional targets from the same paper; existing approvals
  stay byte-untouched.
- **Reference papers** — a configured registry of auxiliary papers
  (`configured_reference_papers`); workers ground statements in them via
  `node_reference_grounds`.
- **Challenge targets (PV)** — byte-pinned goal statements
  (`configured_challenge_targets`) covered through worker claims; empty challenge
  coverage is a self-clearing blocker (`ChallengeCoverage`) with no verifier lane.
- **Decide polarity flips (PV)** — a Decide pair's live polarity (prove vs disprove) is
  flipped only by the audit lane (`set_live_polarity`).
- **Phase-advance gate** — empty blocker set + human approval (`GateKind::Advance`), then
  a routing Review.
- **Coverage re-planning cadence** — while any configured paper target has empty
  coverage in TheoremStating, the coverage planner re-runs every
  `COVERAGE_REPLAN_INTERVAL_CYCLES` cycles; each accepted plan supersedes the live one.
  Live only on runs whose coverage-replanning source was armed at initialization
  (`coverage_replanning_source`, seeded on post-feature fresh runs).
- **Checkpoints and rewinds** — the run-loop rewind semantics above; the abandoned line
  is preserved on a `trellis-rewound/*` branch, and rewound state gets a forced fresh
  audit (`force_stuck_math_audit_after_rewind`).
- **Blockers** — verifier-lane verdicts and derived conditions
  (`BlockerKind`: PaperFaithfulness, Substantiveness, NodeCorr, Soundness, Deviation,
  plus PV ChallengeCoverage) whose union (`global_blockers`) gates cleanliness and phase
  advance; the reviewer adjudicates them via task/reset blocker actions.

## Escalation (NeedInput / HumanGate)

Escalation to a human is a process failure — the last resort, taken only when the
mechanisms above are exhausted. The main path is deliberately adversarial: a reviewer
`NeedInput` decision dispatches a NeedInput audit, whose `confirm_need_input: true` — a
confirmed fundamental problem no internal mechanism can repair — opens the human gate
(`GateKind::NeedInput`, served through `INPUT_REQUEST.md` / `HUMAN_INPUT.md`). Two
further arrivals are themselves exhaustion signals: a NeedInputAuditor that exhausts its
retry budget parks directly on the gate
(`retry_or_transition_stuck_math_audit_to_reviewer`), and a gap-research loop whose
route rejections converge escalates through `escalate_gap_to_human`. Before escalating,
work the inventory: deviations for minor paper mismatches, gap research for missing
route mathematics, global repair for out-of-cone fixes, rewinds for poisoned state,
audits for structural stuckness.
