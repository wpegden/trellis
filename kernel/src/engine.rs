use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Audit Fix MEDIUM (defensive accept-time record install) — canonical
/// kernel axioms the engine trusts WITHOUT consulting per-node
/// `APPROVED_AXIOMS.json`. Now an alias for the kernel-wide
/// `model::CANONICAL_APPROVED_AXIOMS` (Audit M-2: single source of truth
/// so engine accept / runtime-CLI default / public-viewer export cannot
/// drift). The engine accept path refuses to install a
/// `LocalClosureRecord` whose `kernel_axioms` set escapes this canonical
/// four; any probe carrying a non-canonical axiom is forced into the
/// failure path even if the `must_close_active` gate (which has disk
/// access and can read per-node approved sets) accepted it. The
/// deterministic-revalidation pass (Patch C-D) re-installs records via
/// `apply_revalidation_batch`, which is trusted because the runtime CLI
/// computes per-node approved sets before constructing the batch. The
/// defensive engine check is a safety net for the raw-probe accept
/// path: replay traces / test payloads / future malformed worker
/// responses cannot blessed an unapproved axiom through this path.
const ENGINE_CANONICAL_APPROVED_AXIOMS: &[&str] = crate::model::CANONICAL_APPROVED_AXIOMS;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProtocolEvent {
    StartCycle,
    WrapperResponse { response: WrapperResponse },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ProtocolCommand {
    IssueRequest {
        request: WrapperRequest,
    },
    CommitCheckpoint,
    /// Restore the worker repo's worktree to the active worker base
    /// snapshot (or HEAD if no active base exists). Emitted by worker-
    /// reject paths. The restore mutates disk; kernel state is already
    /// updated by `restore_committed()`. (#54)
    RestoreWorktreeToActiveWorkerBase,
    /// Restore the worker repo's worktree to git HEAD via `git reset
    /// --hard HEAD; git clean -fd`. Emitted by reviewer reset paths
    /// where `ResetChoice::LastCommit` was applied. (#54)
    RestoreWorktreeToHead,
    /// Restore the worker repo's worktree to the most recent
    /// `supervisor2/clean-NNNNNN` git tag. Emitted by reviewer reset
    /// paths where `ResetChoice::LastClean` was applied. (#54)
    RestoreWorktreeToLastClean,
    /// Restore one coarse node to its theorem-stating checkpoint and
    /// recursively delete helper nodes that become orphaned. Runtime
    /// re-observes structural state and fingerprints from disk before
    /// the accompanying checkpoint is persisted.
    RestoreTheoremStatingNodeAndPruneOrphans {
        node: NodeId,
    },
    /// Patch C-O HIGH 1 (c) — delete the persisted local-closure record
    /// file `<runtime_root>/checker-state/local-closure-records/<node>.json`
    /// from disk because the engine just invalidated the in-memory
    /// record for `node`. Without this delete, a supervisor restart
    /// would re-read the stale disk record during migration; with the
    /// in-memory tombstone-respect guard from Part (a) the disk record
    /// is at least ignored, but stale files lingering forever is its
    /// own hygiene problem. Engine stays deterministic-state-only; the
    /// runtime CLI honors the command via filesystem I/O.
    DeleteLocalClosureRecord {
        node: NodeId,
    },
    /// Circuit-breaker emit (2026-05-12): write the
    /// `.trellis-stop-after-checkpoint` sentinel file to the
    /// supervisor repo so the outer driver halts cleanly at the next
    /// checkpoint boundary. Used when the engine detects a persistent
    /// worker transport-failure loop on the same node (see
    /// `consecutive_transport_failure_*` state fields). Engine stays
    /// deterministic-state-only; the runtime CLI honors the command via
    /// filesystem I/O. `reason` is recorded both in the sentinel and
    /// in stderr so the operator can diagnose without scraping logs.
    WriteHaltSentinel {
        reason: String,
    },
    /// Rewrite `<repo>/Tablet.lean` to the paper-target umbrella when
    /// the supervisor advances from `Phase::ProofFormalization` into
    /// `Phase::Cleanup`. Emitted exactly once per PF→Cleanup transition
    /// by `enter_cleanup_phase`; carries the resolved
    /// `BTreeSet<NodeId>` (paper-target covering nodes ∪ {Preamble}) so
    /// the runtime dispatcher needs only the payload + `repo_path`.
    /// Engine stays deterministic-state-only; the runtime CLI honors
    /// the command via filesystem I/O (delegating to
    /// `tablet_root::sync_tablet_root`). The legacy
    /// `sync_tablet_root_from_repo` API is retained for `setup_repo.sh`
    /// and the TheoremStating-reset path which operate before
    /// `approved_targets.coverage` is frozen.
    SyncTabletRootForPaperTargets {
        node_names: BTreeSet<NodeId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub state: ProtocolState,
    pub commands: Vec<ProtocolCommand>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionError {
    InvalidStage {
        expected: &'static str,
        found: Stage,
    },
    InvalidPhase {
        expected: Phase,
        found: Phase,
    },
    CycleMismatch {
        expected: u32,
        found: u32,
    },
    RequestMismatch {
        expected: Option<WrapperRequest>,
        found_kind: RequestKind,
        found_request_id: u32,
        found_cycle: u32,
    },
    IllegalReviewerDecision,
    IllegalResponse(String),
    InvariantViolation(String),
}

pub fn apply_event(
    mut state: ProtocolState,
    event: ProtocolEvent,
) -> Result<TransitionOutcome, TransitionError> {
    state.ensure_node_metadata();
    let commands = match event {
        ProtocolEvent::StartCycle => start_cycle(&mut state)?,
        ProtocolEvent::WrapperResponse { response } => {
            apply_wrapper_response(&mut state, response)?
        }
    };
    state.ensure_node_metadata();
    state
        .validate()
        .map_err(TransitionError::InvariantViolation)?;
    Ok(TransitionOutcome { state, commands })
}

fn expect_stage(
    state: &ProtocolState,
    expected: Stage,
    expected_name: &'static str,
) -> Result<(), TransitionError> {
    if state.stage != expected {
        return Err(TransitionError::InvalidStage {
            expected: expected_name,
            found: state.stage,
        });
    }
    Ok(())
}

fn issue_request(state: &mut ProtocolState, kind: RequestKind) -> ProtocolCommand {
    let request = state.issue_request(kind);
    ProtocolCommand::IssueRequest { request }
}

fn record_latest_worker_rationale(state: &mut ProtocolState, response: &WorkerResponse) {
    state.latest_worker_summary = response.summary.trim().to_string();
    state.latest_worker_comments = response.comments.trim().to_string();
    // Mirror worker's named broader-scope suggestions only on NeedsRestructure;
    // clear it on every other outcome so a stale suggestion doesn't leak into
    // a later reviewer prompt.
    if response.outcome == WorkerOutcome::NeedsRestructure {
        state.latest_worker_needs_restructure_suggested_nodes =
            response.needs_restructure_suggested_nodes.clone();
    } else {
        state
            .latest_worker_needs_restructure_suggested_nodes
            .clear();
    }
}

fn request_stage(kind: RequestKind) -> Stage {
    match kind {
        RequestKind::Worker => Stage::Worker,
        RequestKind::Paper => Stage::VerifyPaper,
        RequestKind::Corr => Stage::VerifyCorr,
        RequestKind::Sound => Stage::VerifySound,
        RequestKind::Review => Stage::Reviewer,
        RequestKind::HumanGate => Stage::HumanGate,
        // Cleanup-v2 audit lane (Step 5). The handler
        // (`apply_audit_response`) is added in a later step; until that
        // lands the kernel never issues Audit requests, so this arm is
        // unreachable in practice but required for exhaustive matching.
        RequestKind::Audit => Stage::CleanupAudit,
        RequestKind::StuckMathAudit => Stage::StuckMathAudit,
    }
}

fn worker_retry_threshold(
    phase: Phase,
    kind: RetryOutcomeKind,
    state: &ProtocolState,
) -> Option<u32> {
    match (phase, kind) {
        (Phase::TheoremStating, RetryOutcomeKind::Invalid) => Some(2),
        (Phase::TheoremStating, RetryOutcomeKind::Stuck) => Some(2),
        (Phase::ProofFormalization, RetryOutcomeKind::Invalid) => Some(2),
        (Phase::ProofFormalization, RetryOutcomeKind::Stuck) => Some(2),
        // Bug X principled fix: transport failures get their own threshold,
        // configurable via `transport_invalid_review_threshold`. Apply this
        // to ANY phase where workers run (including Cleanup) — a flaky tmux
        // session can hit any worker request, and we want the kernel to
        // silently absorb a small number before escalating.
        (_, RetryOutcomeKind::Transport) => Some(state.transport_invalid_review_threshold),
        _ => None,
    }
}

fn current_retry_attempt(state: &ProtocolState, kind: RetryOutcomeKind) -> u32 {
    if state.retry_outcome_kind == kind {
        // Bug X principled fix: transport retries track their own counter so
        // they don't burn the work-quality budget. Other retry kinds continue
        // to share `state.attempt`.
        if kind == RetryOutcomeKind::Transport {
            state.transport_attempt
        } else {
            state.attempt
        }
    } else {
        1
    }
}

fn store_retry_attempt(state: &mut ProtocolState, kind: RetryOutcomeKind, value: u32) {
    if kind == RetryOutcomeKind::Transport {
        state.transport_attempt = value;
    } else {
        state.attempt = value;
    }
}

fn begin_retry_review(state: &mut ProtocolState, kind: RetryOutcomeKind) {
    let attempt = current_retry_attempt(state, kind);
    state.retry_outcome_kind = kind;
    store_retry_attempt(state, kind, attempt);
    state.invalid_attempt = kind == RetryOutcomeKind::Invalid;
}

fn continue_worker_retry(state: &mut ProtocolState, kind: RetryOutcomeKind) -> bool {
    let Some(threshold) = worker_retry_threshold(state.phase, kind, state) else {
        return false;
    };
    let attempt = current_retry_attempt(state, kind);
    state.retry_outcome_kind = kind;
    if attempt < threshold {
        store_retry_attempt(state, kind, attempt + 1);
        state.invalid_attempt = kind == RetryOutcomeKind::Invalid;
        true
    } else {
        store_retry_attempt(state, kind, attempt);
        state.invalid_attempt = kind == RetryOutcomeKind::Invalid;
        false
    }
}

fn clear_retry_context(state: &mut ProtocolState) {
    state.retry_outcome_kind = RetryOutcomeKind::None;
    state.invalid_attempt = false;
    state.deterministic_worker_rejection_reasons.clear();
    // Bug X principled fix: clear transport budget whenever we clear retry
    // context, mirroring how `attempt` resets implicitly via the next
    // retry's `current_retry_attempt` returning 1.
    state.transport_attempt = 0;
    // Circuit-breaker reset: any path that clears retry context is one
    // where the worker produced a non-transport-failure outcome (Valid
    // accept, or a reviewer-driven advance off the failing node).
    // Either way, the consecutive-transport-failure run is broken.
    state.consecutive_transport_failure_node = None;
    state.consecutive_transport_failure_count = 0;
}

/// Clear both retry context and pending task in a single call. Proof-phase
/// verifier accepts (and several reset/cleanup sites) issue this pair
/// before every early return; keeping them adjacent prevents drift where
/// one clear is added without the other.
fn clear_retry_and_pending_task(state: &mut ProtocolState) {
    clear_retry_context(state);
    state.clear_pending_task();
}

/// Cleanup-v2 (Step 4): factor out the common per-transition state-mutation
/// for Phase::Cleanup entry. Resets all cleanup-v2 audit/task/counter
/// fields to their fresh-entry values so a re-entry (e.g. from
/// AdvancePhase Approve or a re-audit branch) doesn't leak prior
/// state.
///
/// The caller is responsible for the existing transition bookkeeping
/// that already lives at the call site (`state.attempt = 0`,
/// `clear_retry_context`, `commit_live`, `relegalize_active_fields`,
/// `clear_pending_task`, etc.) — this helper handles ONLY the new
/// cleanup-v2 fields. Kept as a separate step so the introduction is
/// a no-op against the legacy lint-only Cleanup flow until the audit
/// lane lights up in later steps.
///
/// Sites: 4 production transitions in `engine.rs` (verified
/// 2026-05-14 at HEAD via `rg 'state\.phase\s*=\s*Phase::Cleanup'`).
/// The 5 test-fixture sites inside `mod tests` (line 4134+) set up
/// state directly; they call the helper too if needed but are not
/// transitions.
fn enter_cleanup_phase(state: &mut ProtocolState) -> Vec<ProtocolCommand> {
    state.phase = Phase::Cleanup;
    state.stage = Stage::Start;
    state.cleanup_audit_tasks.clear();
    state.cleanup_audit_scratchpad.clear();
    state.cleanup_audit_burst_count = 0;
    state.cleanup_audit_round = 1;
    state.cleanup_consecutive_invalid_workers = 0;
    state.cleanup_active_task = None;
    state.cleanup_force_done = false;
    state.latest_audit_rejection_reason.clear();
    state.audit_burst_retry_count = 0;
    // Proposal v32: leaving ProofFormalization clears the active
    // coarse anchor and the starvation counter. The invariant
    // `phase != ProofFormalization => active_coarse_node = None` is
    // enforced here.
    state.active_coarse_node = None;
    state.cycles_in_coarse_repair_mode = 0;
    // global_repair_mode: phase exit invalidates any pending request /
    // grant / decline reason.
    state.pending_global_repair_request = None;
    state.pending_global_repair_grant = None;
    state.latest_global_repair_audit_decline_reason.clear();
    state.latest_global_repair_audit_decline_cycle = None;
    state.ever_shallow_coarse_closed.clear();
    // Paper-target umbrella sync (2026-05-29): rewrite
    // `<repo>/Tablet.lean` to import the paper-target covering nodes
    // (∪ {Preamble}) so downstream `import Tablet` consumers see the
    // five paper theorems as first-class library exports. Fires
    // exactly once per PF→Cleanup transition (this helper is the sole
    // production path that writes `Phase::Cleanup`). Legacy
    // `sync_tablet_root_from_repo` is retained for setup_repo.sh and
    // the TheoremStating-reset path which operate before
    // `approved_targets.coverage` is frozen.
    let umbrella_nodes =
        crate::tablet_root::paper_target_umbrella_nodes(&state.approved_targets);
    vec![ProtocolCommand::SyncTabletRootForPaperTargets {
        node_names: umbrella_nodes,
    }]
}

fn append_rejection_reason(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|item| item == reason) {
        reasons.push(reason.to_string());
    }
}

/// Patch C-N item 2: wrap `apply_last_clean_reset` + its paired
/// `RestoreWorktreeToLastClean` emission. The model's reset returns:
///   * `Ok(true)`  — state restored from mirrors; push the command so
///                   disk follows.
///   * `Ok(false)` — mirrors not ready (migration window); state stays
///                   put. MUST NOT push the command, or disk would
///                   reset to the supervisor2/clean tag while kernel
///                   state still reflects the post-clean burst,
///                   producing state/disk divergence (the residual hole
///                   audit-fix C-I closed only at the menu level).
///   * `Err(_)`    — unexpected; surfaced as InvariantViolation.
///
/// Defense-in-depth: production paths SHOULD already be gated by
/// `request_allowed_resets` (which hides LastClean from the menu when
/// mirrors aren't ready). This helper closes the residual hole on any
/// path that ever bypasses that menu — defensive, expected to no-op in
/// well-formed runs.
fn apply_last_clean_reset_and_emit(
    state: &mut ProtocolState,
    commands: &mut Vec<ProtocolCommand>,
) -> Result<(), TransitionError> {
    match state.apply_last_clean_reset() {
        Ok(true) => {
            commands.push(ProtocolCommand::RestoreWorktreeToLastClean);
            relegalize_active_coarse_anchor(state);
            Ok(())
        }
        Ok(false) => {
            // Mirror-ready gate refused; keep state and disk in
            // lockstep by suppressing the disk-reset command.
            eprintln!(
                "[engine] LastClean reset refused: closure mirrors not ready; \
                 suppressing RestoreWorktreeToLastClean to preserve state/disk \
                 lockstep (Patch C-N item 2)."
            );
            Ok(())
        }
        Err(msg) => Err(TransitionError::InvariantViolation(format!(
            "apply_last_clean_reset failed: {msg}"
        ))),
    }
}

/// Proposal v32 audit-2 followup #1: a rewind can land the live state
/// in a configuration where `active_coarse_node` no longer exists in
/// `live.present_nodes` (e.g. CoarseRestructure deleted the anchor and
/// the rewind tag predates the re-creation). In that state
/// `coarse_legal_active_set()` returns `{}` outside repair-mode,
/// rejecting every `next_active` — a deadlock until the starvation
/// guard fires 8 cycles later. Clearing the anchor here forces the
/// next Review to reseed via `kernel_hinted_next_active_coarse_nodes`.
/// Counter is always reset since the cycle context just changed.
fn relegalize_active_coarse_anchor(state: &mut ProtocolState) {
    let needs_clear = match state.active_coarse_node.as_ref() {
        Some(anchor) => !state.live.present_nodes.contains(anchor),
        None => false,
    };
    if needs_clear {
        state.active_coarse_node = None;
    }
    state.cycles_in_coarse_repair_mode = 0;
}

/// Apply a `Continue + LastClean` reviewer response by performing the
/// rewind and re-issuing a Review request from the restored state.
///
/// LastClean is a pure rewind: the reviewer's next routing decision
/// (`next_active`, `next_mode`, blocker adjudications, difficulty
/// updates) is made on the *next* Review turn against the post-reset
/// state — not absorbed from the response that triggered the reset.
/// This avoids the "intersection-empty" hazard where the reviewer's
/// chosen `next_active` had to be legal under both the pre-reset
/// next-active hint set AND the post-reset
/// `active_node_legal` predicate; that intersection could be empty
/// even when LastClean was structurally offered, locking the reviewer
/// into NeedInput as the only escape.
///
/// Caller has already captured `reviewer_comments` from the response;
/// those persist into the next Review prompt as the reviewer's
/// recorded reason for the rewind. All other reviewer-supplied fields
/// (`reset_blockers`, `task_blockers`, `difficulty_updates`,
/// `next_active`, `next_mode`) are discarded — the reviewer can re-pick
/// any still-relevant choice on the next turn.
fn apply_continue_last_clean_reissue_review(
    state: &mut ProtocolState,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    // Patch C-N item 2: route through `apply_last_clean_reset_and_emit`
    // so the `RestoreWorktreeToLastClean` command is suppressed if the
    // model's reset short-circuits on the mirror-ready gate (Ok(false)).
    let mut commands: Vec<ProtocolCommand> = Vec::new();
    apply_last_clean_reset_and_emit(state, &mut commands)?;
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.proof_edit_mode = ProofEditMode::Local;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    state.attempt = 0;
    clear_retry_and_pending_task(state);
    state.pending_protected_semantic_scope_confirmation = None;
    state.pending_protected_reapproval_nodes.clear();
    clear_latest_verifier_review_contexts(state);
    // External-audit Finding 2 (recurrence of K-1 antipattern at non-
    // post-cleanup-Valid Reviewer-dispatch sites): just cleared all
    // `latest_*_review_*` contexts, so any Unknown blocker surviving
    // the LastClean reset (statuses restored from `last_clean_*_status`
    // mirrors that may still hold Unknowns) is non-adjudicable. Direct
    // `Stage::Reviewer` would replay the K-1 deadlock — reviewer's only
    // legal Continue would be task→Fail, pinning Fail+approved=current
    // and starving verifier dispatch on the next cycle. Route through
    // the same `route_after_progress` helper that the post-cleanup-Valid
    // path uses (engine.rs:935): preempts Reviewer dispatch with a
    // verifier when any non-adjudicable Unknown exists; otherwise
    // dispatches Reviewer as before.
    commands.extend(route_after_progress(state));
    Ok(commands)
}

fn apply_continue_theorem_stating_node_reset(
    state: &mut ProtocolState,
    node: NodeId,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    // Reserved for a future reviewer-authorized flow. Normal request
    // construction exposes cone clean only to StuckMathAudit.
    state.held_target = None;
    state.active_node = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.proof_edit_mode = ProofEditMode::Local;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    state.stage = Stage::Start;
    state.attempt = 0;
    state.force_stuck_math_audit_after_rewind = true;
    // Proposal v32 audit-2 followup #2: mirror the audit-authorized
    // sibling's anchor-clearing rule even though this reset path is
    // not yet wired in. When wired, a cone-clean targeting the
    // current coarse anchor must force re-seeding on the next
    // Review; non-anchor cone-cleans preserve the anchor.
    if state.active_coarse_node.as_ref() == Some(&node) {
        state.active_coarse_node = None;
    }
    state.cycles_in_coarse_repair_mode = 0;
    clear_retry_and_pending_task(state);
    state.pending_protected_semantic_scope_confirmation = None;
    state.pending_protected_reapproval_nodes.clear();
    clear_latest_verifier_review_contexts(state);
    Ok(vec![
        ProtocolCommand::RestoreTheoremStatingNodeAndPruneOrphans { node },
        ProtocolCommand::CommitCheckpoint,
    ])
}

fn apply_audit_authorized_theorem_stating_node_reset(
    state: &mut ProtocolState,
    node: NodeId,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    state.held_target = None;
    state.active_node = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.proof_edit_mode = ProofEditMode::Local;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    state.stage = Stage::Start;
    state.attempt = 0;
    state.force_stuck_math_audit_after_rewind = false;
    state.force_review_after_cone_clean = true;
    // Proposal v32: cone-clean targeting the active coarse anchor
    // forces re-seeding on the next Review. We're conservative and
    // clear whenever the cleaned node IS the current anchor (the
    // common case for audit-driven cone-clean); the reviewer's next
    // Continue picks a new anchor from
    // `kernel_hinted_next_active_coarse_nodes`. Always reset the
    // starvation counter since the cycle context has changed.
    if state.active_coarse_node.as_ref() == Some(&node) {
        state.active_coarse_node = None;
    }
    state.cycles_in_coarse_repair_mode = 0;
    clear_retry_and_pending_task(state);
    state.pending_protected_semantic_scope_confirmation = None;
    state.pending_protected_reapproval_nodes.clear();
    clear_latest_verifier_review_contexts(state);
    Ok(vec![
        ProtocolCommand::RestoreTheoremStatingNodeAndPruneOrphans { node },
        ProtocolCommand::CommitCheckpoint,
    ])
}

/// Bug X principled fix: classify a Malformed/Invalid worker response so
/// the kernel knows whether it consumes the transport budget or the
/// work-quality budget. A `transport_failure=true` Malformed response means
/// the bridge / agent never produced any meaningful output (timeout, hang,
/// missing done file, etc.) and we should retry against
/// `transport_invalid_review_threshold`. Anything else (Malformed JSON the
/// agent actually produced, Invalid outcome, etc.) bumps the regular
/// `attempt` counter.
fn classify_worker_rejection(response: &WorkerResponse) -> RetryOutcomeKind {
    if response.status == ResponseStatus::Malformed && response.transport_failure {
        RetryOutcomeKind::Transport
    } else {
        RetryOutcomeKind::Invalid
    }
}

/// Circuit-breaker (2026-05-12): account a worker rejection against
/// `consecutive_transport_failure_*`. Returns `Some(reason)` if the
/// threshold tripped this call, in which case the caller should append
/// `ProtocolCommand::WriteHaltSentinel { reason }` to the emitted
/// command list AFTER the regular retry/review routing. Non-transport
/// rejections clear the counter (real worker output, even when
/// invalid, is evidence that the bridge isn't wedged). A transport
/// rejection on a DIFFERENT node than the last one resets to 1, which
/// is the "we made it to a new node, the previous failure was about
/// the old node" case.
fn account_transport_failure_circuit_breaker(
    state: &mut ProtocolState,
    retry_kind: RetryOutcomeKind,
) -> Option<String> {
    if retry_kind != RetryOutcomeKind::Transport {
        state.consecutive_transport_failure_node = None;
        state.consecutive_transport_failure_count = 0;
        return None;
    }
    let active = state.active_node.clone();
    if state.consecutive_transport_failure_node == active && active.is_some() {
        state.consecutive_transport_failure_count =
            state.consecutive_transport_failure_count.saturating_add(1);
    } else {
        state.consecutive_transport_failure_node = active;
        state.consecutive_transport_failure_count = 1;
    }
    if state.consecutive_transport_failure_count
        >= state.consecutive_transport_failure_halt_threshold
    {
        let node_label = state
            .consecutive_transport_failure_node
            .as_ref()
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "<no-active-node>".to_string());
        Some(format!(
            "{} consecutive transport_failure=true worker responses on node {} \
             (threshold {}). Halting via .trellis-stop-after-checkpoint sentinel \
             at next checkpoint boundary to prevent reviewer-cost burn loop. \
             Operator action: diagnose the worker's failure mode (see live tmux \
             session, kernel CLI errors[]) before resuming.",
            state.consecutive_transport_failure_count,
            node_label,
            state.consecutive_transport_failure_halt_threshold,
        ))
    } else {
        None
    }
}

/// Clear the five verifier-lane "latest review context" mirrors in one
/// shot. The five clears appear together at every reject/retry tail and
/// at every reviewer Continue/NeedInput/Done accept boundary; consolidating
/// them here keeps the call sites readable and prevents drift if a new
/// verifier lane is ever added.
fn clear_latest_verifier_review_contexts(state: &mut ProtocolState) {
    state.clear_latest_paper_review_context();
    state.clear_latest_substantiveness_review_context();
    state.clear_latest_corr_review_context();
    state.clear_latest_sound_review_context();
    state.clear_latest_deviation_review_context();
}

/// What the retry/reviewer arms of a worker-reject pipeline do with the
/// in-flight `pending_task`. Theorem clears up-front (so the retry/reviewer
/// arms are no-ops); proof restores the captured template; cleanup leaves
/// the task alone in retry and clears it in the reviewer arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTaskRetryAction {
    ClearBefore,
    Leave,
    RestoreRetryable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTaskReviewerAction {
    AlreadyCleared,
    Clear,
}

/// Per-phase tuning for `reject_worker_response_generic`. Each field
/// captures one delta between the theorem / proof / cleanup pipelines.
struct WorkerRejectConfig {
    /// `restore_committed()` runs unless this is false (cleanup carves
    /// out the proof-cleanup-validation pass per rev10/rev11 history;
    /// see `for_cleanup`).
    restore_committed: bool,
    /// `proof_failure_bump(active)` for non-Transport rejections. Only
    /// the proof reject pipeline; theorem/cleanup don't feed Easy/Hard.
    bump_proof_failure: bool,
    /// `ensure_node_metadata()` after assigning rejection reasons.
    /// Cleanup-path-only.
    ensure_node_metadata: bool,
    pending_task_retry_action: PendingTaskRetryAction,
    pending_task_reviewer_action: PendingTaskReviewerAction,
    /// Clear `gate_kind` / `gate_from_invalid_attempt` and `pending_task`
    /// up-front after relegalize (theorem reject path).
    theorem_pre_clears: bool,
    /// In the reviewer arm, reset `held_target=None`,
    /// `target_edit_mode=Global`, `proof_edit_mode=Local` before
    /// `begin_retry_review`. Cleanup-path-only.
    cleanup_reviewer_mode_reset: bool,
    /// Bug X principled fix: cleanup retries Transport on first attempt
    /// regardless of `is_proof_cleanup_validation_pass`, whereas Invalid
    /// retries require the proof-cleanup-validation pass to avoid
    /// escalating cleanup-style workers immediately. Cleanup-path-only.
    cleanup_transport_first_attempt_retries: bool,
    /// Cleanup-path-only. Caller sets from `worker_context.validation_kind`.
    is_proof_cleanup_validation_pass: bool,
}

impl WorkerRejectConfig {
    fn for_theorem() -> Self {
        Self {
            restore_committed: true,
            bump_proof_failure: false,
            ensure_node_metadata: false,
            pending_task_retry_action: PendingTaskRetryAction::ClearBefore,
            pending_task_reviewer_action: PendingTaskReviewerAction::AlreadyCleared,
            theorem_pre_clears: true,
            cleanup_reviewer_mode_reset: false,
            cleanup_transport_first_attempt_retries: false,
            is_proof_cleanup_validation_pass: false,
        }
    }

    fn for_proof() -> Self {
        Self {
            restore_committed: true,
            bump_proof_failure: true,
            ensure_node_metadata: false,
            pending_task_retry_action: PendingTaskRetryAction::RestoreRetryable,
            pending_task_reviewer_action: PendingTaskReviewerAction::Clear,
            theorem_pre_clears: false,
            cleanup_reviewer_mode_reset: false,
            cleanup_transport_first_attempt_retries: false,
            is_proof_cleanup_validation_pass: false,
        }
    }

    fn for_cleanup(is_proof_cleanup_validation_pass: bool) -> Self {
        // #54 (rev11): rev10's Step 3.5 proposed suppressing restore on
        // cleanup-retry rejection unconditionally, but that left disk
        // and state.live out of sync (the worker mutates disk; if the
        // kernel skips restore_committed AND skips applying the
        // snapshot, state.live stays pre-burst while disk reflects the
        // worker's destructive edits, breaking the next worker prep).
        // RestoreWorktreeToActiveWorkerBase always fires (in the
        // generic body); restore_committed is suppressed only inside
        // the proof-cleanup-validation pass so that pass's intentional
        // disk mutations can land.
        Self {
            restore_committed: !is_proof_cleanup_validation_pass,
            bump_proof_failure: false,
            ensure_node_metadata: true,
            pending_task_retry_action: PendingTaskRetryAction::Leave,
            pending_task_reviewer_action: PendingTaskReviewerAction::Clear,
            theorem_pre_clears: false,
            cleanup_reviewer_mode_reset: true,
            cleanup_transport_first_attempt_retries: true,
            is_proof_cleanup_validation_pass,
        }
    }
}

/// Shared body for the three `reject_*_worker_response` helpers and the
/// four inline Stuck/NeedsRestructure arms in
/// `apply_theorem_worker_response` / `apply_proof_worker_response`.
/// Pipeline: circuit_breaker → restore_committed → push restore-worktree +
/// halt-sentinel commands → apply/clear reasons → phase mutations
/// (ensure_node_metadata, proof_failure_bump, relegalize, mode resets) →
/// continue_worker_retry (with cleanup transport-first carve-out) →
/// retry arm (issue Worker) OR reviewer arm (begin_retry_review +
/// route_after_progress). `suppress_proof_failure_bump=true` is used by
/// the inline Stuck/NR arms (worker's self-reported couldn't-progress
/// signal is reviewer's call, not a difficulty escalation trigger).
fn reject_worker_response_generic(
    state: &mut ProtocolState,
    response: &WorkerResponse,
    cfg: &WorkerRejectConfig,
    retry_kind: RetryOutcomeKind,
    extra_reason: Option<&str>,
    retry_task_template: Option<PendingTask>,
    keep_response_reasons: bool,
    transport_circuit_breaker: bool,
    suppress_proof_failure_bump: bool,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    let halt_reason = if transport_circuit_breaker {
        account_transport_failure_circuit_breaker(state, retry_kind)
    } else {
        None
    };

    if cfg.restore_committed {
        state.restore_committed();
    }
    let mut commands: Vec<ProtocolCommand> =
        vec![ProtocolCommand::RestoreWorktreeToActiveWorkerBase];

    // Halt sentinel (when the circuit breaker tripped) co-exists with
    // the normal retry/review routing so the active cycle ends in a
    // consistent state; the halt fires at the next checkpoint boundary.
    if let Some(reason) = halt_reason.as_deref() {
        commands.push(ProtocolCommand::WriteHaltSentinel {
            reason: reason.to_string(),
        });
    }

    if keep_response_reasons {
        let mut reasons = response.deterministic_rejection_reasons.clone();
        if let Some(reason) = extra_reason {
            append_rejection_reason(&mut reasons, reason);
        }
        state.deterministic_worker_rejection_reasons = reasons;
    } else {
        state.deterministic_worker_rejection_reasons.clear();
    }

    if cfg.ensure_node_metadata {
        state.ensure_node_metadata();
    }

    // Bug X principled fix: transport failures are infra, not proof-
    // quality. Don't feed Easy/Hard difficulty escalation. Stuck/NR
    // arms suppress the bump too (worker's self-reported signal is
    // the reviewer's call, not a counter trigger).
    if cfg.bump_proof_failure
        && !suppress_proof_failure_bump
        && retry_kind != RetryOutcomeKind::Transport
    {
        let active = state.active_node.clone();
        state.proof_failure_bump(active.as_ref());
    }

    state.relegalize_active_fields();

    if cfg.theorem_pre_clears {
        state.clear_pending_task();
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
    }

    // Proof retry/reviewer arms both need held_target/target_edit_mode
    // reset; do it once here so the retry arm's
    // `restore_retryable_worker_task` sees the canonical Global mode.
    if matches!(
        cfg.pending_task_retry_action,
        PendingTaskRetryAction::RestoreRetryable
    ) {
        state.held_target = None;
        state.target_edit_mode = TargetEditMode::Global;
    }

    // Cleanup carve-out (Bug X principled fix): Transport retries on
    // first attempt regardless of pass kind; Invalid only retries inside
    // the proof-cleanup-validation pass (otherwise cleanup-style workers
    // escalate to the reviewer immediately). Theorem/proof take the
    // normal path.
    let should_retry = if cfg.cleanup_transport_first_attempt_retries {
        if retry_kind == RetryOutcomeKind::Transport {
            continue_worker_retry(state, retry_kind)
        } else {
            cfg.is_proof_cleanup_validation_pass && continue_worker_retry(state, retry_kind)
        }
    } else {
        continue_worker_retry(state, retry_kind)
    };

    if should_retry {
        match cfg.pending_task_retry_action {
            // Theorem: pending_task already cleared by theorem_pre_clears.
            // Cleanup: leave pending_task in place; the retry burst
            // re-uses the same task.
            PendingTaskRetryAction::ClearBefore | PendingTaskRetryAction::Leave => {}
            PendingTaskRetryAction::RestoreRetryable => {
                let template = retry_task_template.unwrap_or_default();
                restore_retryable_worker_task(state, template);
            }
        }
        match cfg.pending_task_retry_action {
            // Cleanup: unconditional. Theorem: gated. Proof: handled
            // inside restore_retryable_worker_task.
            PendingTaskRetryAction::Leave => schedule_orphan_cleanup(state),
            PendingTaskRetryAction::ClearBefore => {
                if state.orphan_cleanup_needed() {
                    schedule_orphan_cleanup(state);
                }
            }
            PendingTaskRetryAction::RestoreRetryable => {}
        }
        state.stage = Stage::Worker;
        clear_latest_verifier_review_contexts(state);
        commands.push(issue_request(state, RequestKind::Worker));
        return Ok(commands);
    }

    if cfg.cleanup_reviewer_mode_reset {
        state.held_target = None;
        state.target_edit_mode = TargetEditMode::Global;
        state.proof_edit_mode = ProofEditMode::Local;
    }

    begin_retry_review(state, retry_kind);
    if matches!(
        cfg.pending_task_reviewer_action,
        PendingTaskReviewerAction::Clear
    ) {
        state.clear_pending_task();
    }
    clear_latest_verifier_review_contexts(state);
    // External-audit Finding 2: `route_after_progress` preempts Reviewer
    // dispatch with a verifier on any non-adjudicable Unknown surviving
    // the worker rejection (see `apply_continue_last_clean_reissue_review`).
    commands.extend(route_after_progress(state));
    Ok(commands)
}

fn reject_theorem_worker_response(
    state: &mut ProtocolState,
    response: &WorkerResponse,
    extra_reason: Option<&str>,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    let retry_kind = classify_worker_rejection(response);
    let cfg = WorkerRejectConfig::for_theorem();
    reject_worker_response_generic(
        state,
        response,
        &cfg,
        retry_kind,
        extra_reason,
        None,
        /*keep_response_reasons=*/ true,
        /*transport_circuit_breaker=*/ true,
        /*suppress_proof_failure_bump=*/ false,
    )
}

fn reject_proof_worker_response(
    state: &mut ProtocolState,
    response: &WorkerResponse,
    retry_task: PendingTask,
    extra_reason: Option<&str>,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    let retry_kind = classify_worker_rejection(response);
    let cfg = WorkerRejectConfig::for_proof();
    reject_worker_response_generic(
        state,
        response,
        &cfg,
        retry_kind,
        extra_reason,
        Some(retry_task),
        /*keep_response_reasons=*/ true,
        /*transport_circuit_breaker=*/ true,
        /*suppress_proof_failure_bump=*/ false,
    )
}

fn reject_cleanup_worker_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    response: &WorkerResponse,
    extra_reason: Option<&str>,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    let retry_kind = classify_worker_rejection(response);
    // `is_proof_cleanup_validation_pass` = true iff this rejection is
    // for a worker burst whose validation_kind is Cleanup — i.e. the
    // ProofFormalization-phase cleanup-style validation pass (NOT
    // Phase::Cleanup, which uses validation_kind=FinalCleanup). The
    // distinction matters because that pass intends to retry the
    // worker in place; other paths escalate to the reviewer instead.
    // Earlier name `cleanup_retry` invited misreading as "we are in
    // Phase::Cleanup".
    let is_proof_cleanup_validation_pass =
        request.worker_context.validation_kind == WorkerValidationKind::Cleanup;
    let cfg = WorkerRejectConfig::for_cleanup(is_proof_cleanup_validation_pass);
    reject_worker_response_generic(
        state,
        response,
        &cfg,
        retry_kind,
        extra_reason,
        None,
        /*keep_response_reasons=*/ true,
        /*transport_circuit_breaker=*/ true,
        /*suppress_proof_failure_bump=*/ false,
    )
}

/// Reject path used by the four inline Stuck/NeedsRestructure arms in
/// `apply_theorem_worker_response` / `apply_proof_worker_response`.
/// Differences from the regular `reject_*_worker_response` helpers:
///   - the retry kind is fixed by the worker outcome (Stuck or NR), not
///     classified from the response status;
///   - the circuit breaker is suppressed (Stuck/NR aren't transport
///     failures);
///   - `deterministic_worker_rejection_reasons` is cleared rather than
///     populated from the response (the inline arms historically
///     produced no deterministic reason text);
///   - the proof-phase proof_failure counter is NOT bumped (workers
///     explicitly signal couldn't-progress; the reviewer arbitrates).
fn reject_worker_response_for_stuck_or_nr(
    state: &mut ProtocolState,
    response: &WorkerResponse,
    cfg: &WorkerRejectConfig,
    retry_kind: RetryOutcomeKind,
    retry_task_template: Option<PendingTask>,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    debug_assert!(matches!(
        retry_kind,
        RetryOutcomeKind::Stuck | RetryOutcomeKind::NeedsRestructure
    ));
    reject_worker_response_generic(
        state,
        response,
        cfg,
        retry_kind,
        None,
        retry_task_template,
        /*keep_response_reasons=*/ false,
        /*transport_circuit_breaker=*/ false,
        /*suppress_proof_failure_bump=*/ true,
    )
}

fn orphan_cleanup_comment(orphan_nodes: &BTreeSet<NodeId>) -> String {
    let listed = orphan_nodes.iter().cloned().collect::<Vec<_>>().join(", ");
    format!(
        "Automatic supervisor orphan-cleanup task. The accepted live snapshot contains orphan nodes that are not in the import down-closure of any paper-target-claiming node: {listed}. Remove at least one orphan node, or attach it by making an existing supported consumer add a genuine `import Tablet.<Orphan>` edge to that orphan. Keep the changes focused on orphan cleanup."
    )
}

fn schedule_orphan_cleanup(state: &mut ProtocolState) {
    let orphan_nodes = state.orphan_nodes(&state.live);
    if orphan_nodes.is_empty() {
        return;
    }
    state.reviewer_comments = orphan_cleanup_comment(&orphan_nodes);
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    match state.phase {
        Phase::ProofFormalization => {
            state.active_node = None;
            state.proof_edit_mode = ProofEditMode::CoarseRestructure;
        }
        Phase::TheoremStating => {
            state.active_node = None;
        }
        Phase::Cleanup | Phase::Complete => {}
    }
    state.pending_task = Some(PendingTask {
        task_blockers: BTreeSet::new(),
        node: state.active_node.clone(),
        mode: state.current_mode(),
        orphan_cleanup_nodes: orphan_nodes,
        protected_semantic_change_nodes: BTreeSet::new(),
        authorized_nodes: BTreeSet::new(),
        allow_new_obligations: true,
        must_close_active: false,
        next_worker_context_mode: WorkerContextMode::Resume,
        paper_focus_ranges: Vec::new(),
        work_style_hint: WorkerWorkStyleHint::Restructure,
    consumed_global_repair_grant: false,
    });
}

fn restore_retryable_worker_task(state: &mut ProtocolState, template: PendingTask) {
    if state.orphan_cleanup_needed() {
        schedule_orphan_cleanup(state);
        return;
    }
    if state.phase == Phase::ProofFormalization && state.active_node.is_none() {
        state.active_node = state.select_initial_proof_active_node();
    }
    let global = state.global_blockers();
    let next_worker_context_mode = if state.phase == Phase::ProofFormalization
        && matches!(
            state.retry_outcome_kind,
            RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
        )
        && state.current_worker_validation_kind() == WorkerValidationKind::ProofLocal
    {
        WorkerContextMode::Fresh
    } else {
        template.next_worker_context_mode
    };
    state.pending_task = Some(PendingTask {
        task_blockers: template
            .task_blockers
            .into_iter()
            .filter(|blocker| global.contains(blocker))
            .collect(),
        node: state.active_node.clone(),
        mode: state.current_mode(),
        orphan_cleanup_nodes: BTreeSet::new(),
        protected_semantic_change_nodes: template.protected_semantic_change_nodes,
        authorized_nodes: template.authorized_nodes,
        allow_new_obligations: template.allow_new_obligations,
        must_close_active: template.must_close_active,
        next_worker_context_mode,
        paper_focus_ranges: template.paper_focus_ranges,
        work_style_hint: template.work_style_hint,
    consumed_global_repair_grant: false,
    });
}

fn start_cycle(state: &mut ProtocolState) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::Start, "Start")?;
    if matches!(state.phase, Phase::Complete) {
        return Err(TransitionError::InvalidPhase {
            expected: Phase::TheoremStating,
            found: state.phase,
        });
    }
    if state.in_flight_request.is_some() {
        return Err(TransitionError::InvariantViolation(
            "cannot start a cycle with an in-flight request".into(),
        ));
    }
    state.cycle += 1;
    state.attempt = 1;
    if state.phase == Phase::ProofFormalization && state.force_review_after_cone_clean {
        state.force_stuck_math_audit_after_rewind = false;
        state.clear_pending_task();
        clear_retry_context(state);
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
        state.stage = Stage::Reviewer;
        let command = issue_request(state, RequestKind::Review);
        state.force_review_after_cone_clean = false;
        return Ok(vec![command]);
    }
    // Post-advance routing Review: after a human-approved phase advance
    // into ProofFormalization, the first request of the new phase must be
    // a Reviewer routing burst so the reviewer chooses `next_active`,
    // `must_close_active`, `allow_new_obligations`, and friends explicitly
    // — rather than letting the engine auto-pick active_node via
    // `select_initial_proof_active_node` and dispatch a worker with
    // permissive kernel defaults. The reviewer's `allowed_decisions` here
    // is `{Continue, NeedInput}` (see `request_allowed_decisions`'s
    // ProofFormalization arm), so it cannot re-advance the phase.
    if state.phase == Phase::ProofFormalization && state.post_advance_routing_pending {
        state.clear_pending_task();
        clear_retry_context(state);
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
        state.stage = Stage::Reviewer;
        // Latch stays set until the routing Review's response is applied
        // (cleared in apply_proof_review_response). This keeps the in-flight
        // Review's `post_advance_routing: true` consistent with what
        // `expected_request` derives, satisfying the in-flight invariant
        // check throughout the routing Review's lifetime — including any
        // re-issue on a malformed response.
        return Ok(vec![issue_request(state, RequestKind::Review)]);
    }
    if state.phase == Phase::ProofFormalization && state.force_stuck_math_audit_after_rewind {
        state.clear_pending_task();
        clear_retry_context(state);
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
        return Ok(vec![issue_review_or_stuck_math_audit(state)]);
    }
    if !state.orphan_cleanup_needed() {
        if let Some(commands) = maybe_issue_protected_reapproval(state) {
            return Ok(commands);
        }
    }
    let request_kind = if state.orphan_cleanup_needed() {
        schedule_orphan_cleanup(state);
        RequestKind::Worker
    } else if state.phase == Phase::TheoremStating {
        state.theorem_start_request_kind()
    } else if state.phase == Phase::ProofFormalization {
        state.proof_start_request_kind()
    } else if state.phase == Phase::Cleanup {
        if state.cleanup_active_task.is_some() || state.pending_task.is_some() {
            // Active worker task: dispatch the worker burst.
            RequestKind::Worker
        } else if state.cleanup_audit_burst_count == 0 {
            // Cleanup-v2 Step 11. First audit burst per round. Conditions:
            //   - Phase::Cleanup,
            //   - no audit bursts yet this round (`burst_count == 0`), and
            //   - no in-flight cleanup worker task.
            // After the audit dispatch lights up, subsequent bursts within
            // a round are re-issued from `apply_audit_response`
            // (continuation) and the reviewer cycle drives worker
            // dispatch — neither path comes back through `start_cycle`,
            // so we only need to catch the round-entry case here.
            //
            // The `cleanup_force_done` latch (counter-threshold) is checked
            // by the reviewer's Done arm rather than here — at audit
            // entry it can't be set yet (no worker bursts have run in
            // this round).
            RequestKind::Audit
        } else {
            // Defense-in-depth (audit follow-up): Cleanup, no active or
            // pending worker task, audit has already run this round
            // (`burst_count > 0`). The legitimate cleanup-v2 control
            // flow re-issues subsequent audit bursts via
            // `apply_audit_response`, and the reviewer cycle drives
            // worker dispatch — so under normal flow this branch is
            // not reached. State load from disk, recovery paths, or
            // future code changes could reach it. Falling through to
            // `RequestKind::Worker` would emit an empty-pending-task
            // Worker request, contradicting the cleanup-v2 design
            // (see `apply_cleanup_review_response` workaround
            // comments around engine.rs:4332-4335 and 4385-4390).
            // Route to Reviewer instead: the reviewer's blocker_choices
            // contract is empty (auto-Done shape or remaining-Pending
            // shape) and the reviewer chooses Continue (next dispatch
            // from a Pending task) or Done (advances to Phase::Complete).
            RequestKind::Review
        }
    } else {
        RequestKind::Worker
    };
    // TheoremStating-phase StuckMathAudit trigger (Trigger C). When the
    // Sound-blocker NODE set has stagnated for >= the configured
    // threshold and the would-be dispatch is Worker (i.e. no verifier
    // frontier remains to drain — verifier work is how the blocker set
    // might shrink and so MUST run first), preempt with StuckMathAudit.
    // Mirrors `issue_review_or_stuck_math_audit` (which handles the
    // ProofFormalization Review-vs-audit decision); here the would-be
    // dispatch in TheoremStating is Worker, not Review. The audit
    // dispatch zeroes the stagnation counter so a single stagnation
    // streak fires the audit once; the latch and `last_stuck_math_audit_
    // dispatched_cycle` cooldown handle subsequent cycles.
    let theorem_stating_audit_preempt = state.phase == Phase::TheoremStating
        && request_kind == RequestKind::Worker
        && should_dispatch_stuck_math_audit(state);
    if theorem_stating_audit_preempt {
        state.stage = Stage::StuckMathAudit;
        state.last_stuck_math_audit_dispatched_cycle = Some(state.cycle);
        state.progress_history.note_dispatched();
        state.clear_pending_task();
        clear_retry_context(state);
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
        return Ok(vec![issue_request(state, RequestKind::StuckMathAudit)]);
    }
    if state.phase == Phase::TheoremStating {
        state.held_target = state.select_theorem_held_target();
    } else if state.phase == Phase::ProofFormalization
        && request_kind == RequestKind::Worker
        && state.active_node.is_none()
    {
        state.active_node = state.select_initial_proof_active_node();
        // Audit-2 followup #4: when an active coarse anchor is set,
        // `select_initial_proof_active_node` restricts candidates to
        // the anchor's cone. If the cone has no work-needing
        // candidates the function returns None, which would dispatch
        // a degenerate Worker burst with `active_node = None`. The
        // anchor is "stale" in this snapshot — clear it (and the
        // repair-mode counter, per the TLA TypeOK invariant
        // `active_coarse_node = None ⇒ cycles_in_coarse_repair_mode = 0`)
        // and re-select from the unrestricted candidate set. The next
        // Review will reseed the anchor via
        // `kernel_hinted_next_active_coarse_nodes`. Mirrors
        // `relegalize_active_coarse_anchor`'s "dangling anchor"
        // recovery — same shape, different trigger.
        if state.active_node.is_none() && state.active_coarse_node.is_some() {
            state.active_coarse_node = None;
            state.cycles_in_coarse_repair_mode = 0;
            state.active_node = state.select_initial_proof_active_node();
        }
        // The reviewer may have left `next_active = None` (e.g. when
        // switching the coarse anchor and intending the kernel to
        // auto-pick from the new cone). The reviewer-apply path then
        // created `pending_task` with `node = None` (matching the
        // then-current `active_node`). Now that start_cycle has
        // auto-resolved `active_node` to a concrete node, sync
        // `pending_task.node` so the
        // `pending_task.node == active_node` invariant holds.
        if let Some(task) = state.pending_task.as_mut() {
            if task.node != state.active_node {
                task.node = state.active_node.clone();
            }
        }
    }
    state.stage = request_stage(request_kind);
    if request_kind != RequestKind::Worker {
        state.clear_pending_task();
    }
    // Patch C plan §7.4.1: when `select_initial_proof_active_node`
    // auto-schedules a worker burst on a node that is in
    // `local_closure_unverified_nodes` (and NOT in
    // `live.open_nodes` — by the sorry-free-only invariant), the
    // burst would otherwise run with `must_close_active=false` and
    // `allow_new_obligations=true`, neither enforcing closure nor
    // restricting scope. Synthesize a pending_task with the
    // strictest reasonable defaults so the worker is instructed to
    // attempt closure on the failed dep / failure category from
    // `local_closure_failures[N]`.
    if state.phase == Phase::ProofFormalization && request_kind == RequestKind::Worker {
        maybe_synthesize_unverified_pending_task(state);
    }
    clear_retry_context(state);
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    Ok(vec![issue_request(state, request_kind)])
}

/// Patch C plan §7.4.1 — when `select_initial_proof_active_node`
/// returns a node N in `local_closure_unverified_nodes` that is not in
/// `live.open_nodes` (sorry-free-only invariant), and N's failure
/// category is NOT solely `transport_error` (transport errors retry
/// via the deterministic-revalidation pass per §7.0/§7.4.1; they
/// don't need a worker burst), synthesize a `PendingTask` pinning
/// closure-revalidation defaults: `must_close_active=true`,
/// `allow_new_obligations=false`, `mode=Local`, and a non-empty
/// `task_blockers=∅` (the task isn't blocker-driven; it's closure-
/// driven). Only fires when no `task_blockers` are pending — the
/// existing `task_blockers != ∅` route is verifier-blocker repair and
/// has its own pending_task already (preserved upstream by
/// `proof_start_request_kind`).
fn maybe_synthesize_unverified_pending_task(state: &mut ProtocolState) {
    let Some(node) = state.active_node.clone() else {
        return;
    };
    if state.live.open_nodes.contains(&node) {
        return;
    }
    if !state.local_closure_unverified_nodes.contains(&node) {
        return;
    }
    if let Some(summary) = state.local_closure_failures.get(&node) {
        if summary.status == "transport_error" {
            return;
        }
    }
    // Preserve any existing reviewer-set pending_task with non-empty
    // blockers — that route is the verifier-blocker repair path
    // (`proof_start_request_kind` returns `Worker` for non-empty
    // `task_blockers` per model.rs:4308). Only synthesize when the
    // pending_task is absent or empty-blockered.
    if let Some(task) = state.pending_task.as_ref() {
        if !task.task_blockers.is_empty() {
            return;
        }
    }
    let diagnostic =
        unverified_pending_task_diagnostic(&node, state.local_closure_failures.get(&node));
    state.reviewer_comments = diagnostic;
    state.proof_edit_mode = ProofEditMode::Local;
    state.pending_task = Some(PendingTask {
        task_blockers: BTreeSet::new(),
        node: Some(node),
        mode: TaskMode::Local,
        orphan_cleanup_nodes: BTreeSet::new(),
        protected_semantic_change_nodes: BTreeSet::new(),
        authorized_nodes: BTreeSet::new(),
        allow_new_obligations: false,
        must_close_active: true,
        next_worker_context_mode: WorkerContextMode::Resume,
        paper_focus_ranges: Vec::new(),
        work_style_hint: WorkerWorkStyleHint::None,
    consumed_global_repair_grant: false,
    });
}

fn unverified_pending_task_diagnostic(node: &NodeId, summary: Option<&ErrorSummary>) -> String {
    match summary {
        Some(summary) => {
            let category = if !summary.axiom_violations.is_empty() {
                "axiom"
            } else if !summary.strict_errors.is_empty() {
                "strict"
            } else {
                summary.status.as_str()
            };
            let excerpt = summary.stderr_excerpt.trim();
            let excerpt_display = if excerpt.is_empty() {
                "<no stderr captured>".to_string()
            } else {
                excerpt.to_string()
            };
            format!(
                "Node `{node}` has a stale local-closure record. \
                 Re-closing the node against the current boundary / strict-dep \
                 statements is required. Original failure: [{category}] {excerpt_display}",
                node = node,
                category = category,
                excerpt_display = excerpt_display,
            )
        }
        None => format!(
            "Node `{node}` has a stale local-closure record (no failure \
             summary recorded). Re-closing the node against the current \
             boundary / strict-dep statements is required.",
            node = node,
        ),
    }
}

fn apply_wrapper_response(
    state: &mut ProtocolState,
    response: WrapperResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    ensure_matching_request(state, &response)?;
    let request = state.in_flight_request.clone().ok_or_else(|| {
        TransitionError::InvariantViolation("missing in-flight request after match".into())
    })?;
    state.clear_in_flight_request();
    match response {
        WrapperResponse::Worker(response) => apply_worker_response(state, &request, response),
        WrapperResponse::Paper(response) => apply_paper_response(state, &request, response),
        WrapperResponse::Corr(response) => apply_corr_response(state, &request, response),
        WrapperResponse::Sound(response) => apply_sound_response(state, &request, response),
        WrapperResponse::Review(response) => apply_review_response(state, response),
        WrapperResponse::HumanGate(response) => apply_human_gate_response(state, response),
        // Cleanup-v2 Step 13: audit lane handler.
        WrapperResponse::Audit(response) => apply_audit_response(state, response),
        WrapperResponse::StuckMathAudit(response) => {
            apply_stuck_math_audit_response(state, response)
        }
    }
}

fn ensure_matching_request(
    state: &ProtocolState,
    response: &WrapperResponse,
) -> Result<(), TransitionError> {
    let expected = state.in_flight_request.clone();
    let Some(request) = expected.clone() else {
        return Err(TransitionError::RequestMismatch {
            expected,
            found_kind: response.kind(),
            found_request_id: response.request_id(),
            found_cycle: response.cycle(),
        });
    };
    if request.kind != response.kind()
        || request.id != response.request_id()
        || request.cycle != response.cycle()
    {
        return Err(TransitionError::RequestMismatch {
            expected: Some(request),
            found_kind: response.kind(),
            found_request_id: response.request_id(),
            found_cycle: response.cycle(),
        });
    }
    Ok(())
}

fn apply_worker_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    response: WorkerResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::Worker, "Worker")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    match request.worker_context.validation_kind {
        WorkerValidationKind::Cleanup | WorkerValidationKind::FinalCleanup => {
            apply_cleanup_worker_response(state, request, response)
        }
        WorkerValidationKind::TheoremGlobal | WorkerValidationKind::TheoremTargeted => {
            match state.phase {
                Phase::TheoremStating => apply_theorem_worker_response(state, response),
                Phase::ProofFormalization | Phase::Cleanup | Phase::Complete => {
                    Err(TransitionError::InvalidPhase {
                        expected: Phase::TheoremStating,
                        found: state.phase,
                    })
                }
            }
        }
        WorkerValidationKind::ProofEasy
        | WorkerValidationKind::ProofLocal
        | WorkerValidationKind::ProofRestructure
        | WorkerValidationKind::ProofCoarseRestructure => match state.phase {
            Phase::ProofFormalization => apply_proof_worker_response(state, response),
            Phase::TheoremStating | Phase::Cleanup | Phase::Complete => {
                Err(TransitionError::InvalidPhase {
                    expected: Phase::ProofFormalization,
                    found: state.phase,
                })
            }
        },
        WorkerValidationKind::None => Err(TransitionError::IllegalResponse(
            "worker response received for request with no worker validation kind".into(),
        )),
    }
}

fn apply_theorem_worker_response(
    state: &mut ProtocolState,
    mut response: WorkerResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    record_latest_worker_rationale(state, &response);
    if response.status == ResponseStatus::Malformed {
        return reject_theorem_worker_response(state, &response, None);
    }
    if response.outcome == WorkerOutcome::Invalid {
        return reject_theorem_worker_response(state, &response, None);
    }
    // Stuck and NeedsRestructure are handled BEFORE applying the worker's
    // snapshot. Otherwise `state.live = snapshot` + `relegalize_active_fields`
    // below would null `active_node` / `held_target` against the worker's
    // (potentially node-dropping) snapshot, and the subsequent
    // `restore_committed` only restores live structural state — not the
    // routing fields. Mirrors the proof-formalization shape and matches the
    // TLA `AcceptStuckWorkerTheorem` / `AcceptNeedsRestructureWorkerTheorem`
    // UNCHANGED clauses (which keep activeNode / heldTarget / modes).
    //
    // #54: Stuck/NeedsRestructure non-Valid outcomes also need a worktree
    // rollback to keep next attempt's snapshot clean —
    // worker_response_should_preserve_attempt covers all non-Valid. The
    // RestoreWorktreeToActiveWorkerBase command is emitted by the
    // shared reject pipeline.
    if response.outcome == WorkerOutcome::Stuck {
        let cfg = WorkerRejectConfig::for_theorem();
        return reject_worker_response_for_stuck_or_nr(
            state,
            &response,
            &cfg,
            RetryOutcomeKind::Stuck,
            None,
        );
    }
    if response.outcome == WorkerOutcome::NeedsRestructure {
        let cfg = WorkerRejectConfig::for_theorem();
        return reject_worker_response_for_stuck_or_nr(
            state,
            &response,
            &cfg,
            RetryOutcomeKind::NeedsRestructure,
            None,
        );
    }
    // Valid path: now safe to apply the worker's snapshot and structural
    // updates because we will commit them as the new state.
    let has_delta = state.worker_semantic_delta(&response);
    // Patch C-B: capture pre-delta live snapshot BEFORE the snapshot
    // assignment so the local-closure bookkeeping can detect sorry-free →
    // sorryd transitions (record deletion contract per plan §7.0), node
    // deletions (orphan-cleanup prune per plan §7.0), AND fingerprint /
    // content-only changes per audit Fix HIGH 2.
    let pre_live = state.live.clone();
    let snapshot = response.snapshot.clone();
    state.live = snapshot;
    state.apply_worker_structure_updates(&response);
    // Patch C-O HIGH 1 (c): collect `DeleteLocalClosureRecord` commands
    // for any record invalidated by bookkeeping and merge into every
    // return path below.
    let mut delete_commands: Vec<ProtocolCommand> = Vec::new();
    apply_local_closure_acceptance_bookkeeping(
        state,
        &mut response,
        &pre_live,
        &mut delete_commands,
    );
    state.apply_difficulty_updates(&response.difficulty_updates);
    state.relegalize_active_fields();
    state.clear_pending_task();
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    match response.outcome {
        WorkerOutcome::Valid => {
            clear_retry_context(state);
            clear_latest_verifier_review_contexts(state);
            if has_delta && state.orphan_cleanup_needed() {
                schedule_orphan_cleanup(state);
                state.stage = Stage::Worker;
                let mut out = delete_commands;
                out.push(issue_request(state, RequestKind::Worker));
                return Ok(out);
            }
            if has_delta {
                // Paper lane fires whenever EITHER frontier is non-empty.
                // Both share `Stage::VerifyPaper`; the cycle scheduler
                // (`request_paper_verify_*`) picks target-first.
                if state.paper_verify_targets().is_empty()
                    && state.deviation_verify_ids().is_empty()
                    && state.substantiveness_verify_nodes().is_empty()
                {
                    let mut out = delete_commands;
                    out.extend(apply_theorem_paper_accept(state)?);
                    return Ok(out);
                }
                state.stage = Stage::VerifyPaper;
                state.substantiveness_consecutive_no_progress_requests = 0;
                let mut out = delete_commands;
                out.push(issue_request(state, RequestKind::Paper));
                return Ok(out);
            }
            // No-delta: verifier evidence was just cleared above, so any
            // surviving Unknown blocker is non-adjudicable. Route through
            // `route_after_progress` to preempt Reviewer dispatch with a
            // verifier whenever needed (external-audit Finding 2).
            let mut out = delete_commands;
            out.extend(route_after_progress(state));
            Ok(out)
        }
        WorkerOutcome::Invalid | WorkerOutcome::Stuck | WorkerOutcome::NeedsRestructure => {
            Err(TransitionError::InvariantViolation(
                "non-Valid theorem worker outcomes are handled before snapshot apply".into(),
            ))
        }
    }
}

fn apply_proof_worker_response(
    state: &mut ProtocolState,
    mut response: WorkerResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    record_latest_worker_rationale(state, &response);
    if response.status == ResponseStatus::Malformed || response.outcome == WorkerOutcome::Invalid {
        let retry_task = state.pending_task.clone().unwrap_or_default();
        return reject_proof_worker_response(state, &response, retry_task, None);
    }
    if response.outcome == WorkerOutcome::Stuck {
        // Stuck honoured even when the worker left a tablet delta — the
        // restore_committed (in the shared reject pipeline) +
        // RestoreWorktreeToActiveWorkerBase command + last_invalid
        // capture preserve safety. Worker's verdict signal reaches the
        // reviewer untranslated. See
        // CLAUDES_NOTES_remove_stuck_nr_no_delta_rule.md.
        let retry_task = state.pending_task.clone().unwrap_or_default();
        let cfg = WorkerRejectConfig::for_proof();
        return reject_worker_response_for_stuck_or_nr(
            state,
            &response,
            &cfg,
            RetryOutcomeKind::Stuck,
            Some(retry_task),
        );
    }
    if response.outcome == WorkerOutcome::NeedsRestructure {
        // NeedsRestructure honoured even when the worker left a tablet
        // delta — see Stuck branch above for the rationale.
        let cfg = WorkerRejectConfig::for_proof();
        return reject_worker_response_for_stuck_or_nr(
            state,
            &response,
            &cfg,
            RetryOutcomeKind::NeedsRestructure,
            None,
        );
    }
    let has_delta = state.worker_semantic_delta(&response);
    // Patch C-B: capture pre-delta live snapshot before the snapshot
    // assignment for local-closure bookkeeping (plan §7.0); audit Fix
    // HIGH 2 widens the captured data from open/present sets to the
    // full WorkingSnapshot so fingerprint-only changes are detectable.
    let pre_live = state.live.clone();
    let snapshot = response.snapshot.clone();
    state.live = snapshot;
    state.apply_worker_structure_updates(&response);
    // Patch C-O HIGH 1 (c): bookkeeping appends
    // `DeleteLocalClosureRecord` commands for each invalidated record;
    // merged into every return path below.
    let mut delete_commands: Vec<ProtocolCommand> = Vec::new();
    apply_local_closure_acceptance_bookkeeping(
        state,
        &mut response,
        &pre_live,
        &mut delete_commands,
    );
    state.apply_difficulty_updates(&response.difficulty_updates);
    state
        .pending_protected_reapproval_nodes
        .extend(response.protected_semantic_change_nodes.iter().cloned());
    state.relegalize_active_fields();
    let active = state.active_node.clone();
    state.reset_easy_attempt_for_node(active.as_ref());
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.clear_pending_task();
    clear_retry_context(state);
    clear_latest_verifier_review_contexts(state);
    // global_repair_mode S6: clear the audit grant once a worker burst
    // has been accepted (Valid). On rejection, restore_committed runs
    // earlier in the reject pipeline and the grant survives untouched.
    state.pending_global_repair_grant = None;
    if has_delta && state.orphan_cleanup_needed() {
        schedule_orphan_cleanup(state);
        state.stage = Stage::Worker;
        let mut out = delete_commands;
        out.push(issue_request(state, RequestKind::Worker));
        return Ok(out);
    }
    if has_delta {
        if state.paper_verify_targets().is_empty()
            && state.deviation_verify_ids().is_empty()
            && state.substantiveness_verify_nodes().is_empty()
        {
            let mut out = delete_commands;
            out.extend(apply_proof_paper_accept(state)?);
            return Ok(out);
        }
        state.stage = Stage::VerifyPaper;
        let mut out = delete_commands;
        out.push(issue_request(state, RequestKind::Paper));
        return Ok(out);
    }
    // No-delta: verifier evidence was just cleared above, so any
    // surviving Unknown blocker is non-adjudicable. Route through
    // `route_after_progress` to preempt Reviewer dispatch with a
    // verifier whenever needed (external-audit Finding 2).
    let mut out = delete_commands;
    out.extend(route_after_progress(state));
    Ok(out)
}

/// Cleanup-v2 Step 7: mark the in-flight cleanup task as Failed
/// (with `reason`), increment the consecutive-invalid counter, latch
/// `cleanup_force_done` when the counter reaches threshold, and clear
/// `cleanup_active_task`.
///
/// Idempotent on `cleanup_active_task = None` (legacy lint-only mode
/// never sets the active task — see step 6's dispatch branch). Called
/// from every cleanup-worker rejection path AFTER the reject helper
/// has run.
///
/// The `restore_committed` inside the reject helper does NOT roll back
/// the `cleanup_audit_tasks` Vec (it's not in the committed mirror —
/// task transitions are append-only / Pending→terminal, by design),
/// so it is safe to mutate after the reject call.
fn mark_cleanup_task_failed(state: &mut ProtocolState, reason: String) {
    let Some(idx) = state.cleanup_active_task else {
        return;
    };
    let i = idx as usize;
    if i < state.cleanup_audit_tasks.len()
        && matches!(
            state.cleanup_audit_tasks[i].status,
            CleanupTaskStatus::Pending
        )
    {
        state.cleanup_audit_tasks[i].status = CleanupTaskStatus::Failed { reason };
    }
    state.cleanup_consecutive_invalid_workers =
        state.cleanup_consecutive_invalid_workers.saturating_add(1);
    if state.cleanup_consecutive_invalid_workers >= CLEANUP_CONSECUTIVE_INVALID_THRESHOLD {
        state.cleanup_force_done = true;
    }
    state.cleanup_active_task = None;
}

/// Cleanup-v2 Step 7: mark the in-flight cleanup task as Completed,
/// reset the consecutive-invalid counter, and clear
/// `cleanup_active_task`. Idempotent on `cleanup_active_task = None`.
fn mark_cleanup_task_completed(state: &mut ProtocolState) {
    let Some(idx) = state.cleanup_active_task else {
        return;
    };
    let i = idx as usize;
    if i < state.cleanup_audit_tasks.len()
        && matches!(
            state.cleanup_audit_tasks[i].status,
            CleanupTaskStatus::Pending
        )
    {
        state.cleanup_audit_tasks[i].status = CleanupTaskStatus::Completed;
    }
    state.cleanup_consecutive_invalid_workers = 0;
    state.cleanup_active_task = None;
}

fn apply_cleanup_worker_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    mut response: WorkerResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    record_latest_worker_rationale(state, &response);
    // Cleanup-v2 Step 7: gate cleanup-task status updates on
    // `phase == Cleanup` so this code path remains a no-op for
    // the ProofFormalization-phase cleanup-style validation pass
    // (which also funnels through this function — see the
    // `validation_kind == WorkerValidationKind::Cleanup` branch
    // further down).
    let in_phase_cleanup = state.phase == Phase::Cleanup;
    if response.status == ResponseStatus::Malformed || response.outcome == WorkerOutcome::Invalid {
        // Cleanup-v2 (audit Finding 6): mark the task Failed BEFORE
        // `reject_cleanup_worker_response` issues the next Review.
        // `issue_request` (model.rs:6431) snapshots `in_flight_request`
        // from the current state; if we mark Failed after the issue, the
        // in-flight Review carries stale task status (still showing
        // Pending) and the reviewer renders incorrect counts.
        if in_phase_cleanup {
            mark_cleanup_task_failed(
                state,
                format!(
                    "worker {} (outcome={:?})",
                    if response.status == ResponseStatus::Malformed {
                        "malformed"
                    } else {
                        "invalid"
                    },
                    response.outcome
                ),
            );
        }
        let result = reject_cleanup_worker_response(state, request, &response, None);
        return result;
    }
    if matches!(
        response.outcome,
        WorkerOutcome::Stuck | WorkerOutcome::NeedsRestructure
    ) {
        // Cleanup-v2 Step 7: Stuck/NR are now treated as Failed for the
        // active task (mirrors Invalid in cleanup-v2 semantics). The
        // legacy "outcome not legal in cleanup" error wording is
        // retained as the reject reason because the existing
        // ProofFormalization-phase cleanup-style validation pass also
        // funnels through this function — and there, Stuck/NR truly
        // are illegal (no per-task to mark Failed against).
        //
        // Cleanup-v2 (audit Finding 6): mark the task Failed BEFORE
        // `reject_cleanup_worker_response` issues the next Review (same
        // rationale as the Malformed/Invalid path above).
        if in_phase_cleanup {
            mark_cleanup_task_failed(state, format!("worker_outcome={:?}", response.outcome));
        }
        let reason = format!(
            "cleanup worker outcome {:?} is not legal in cleanup",
            response.outcome
        );
        let result = reject_cleanup_worker_response(state, request, &response, Some(&reason));
        return result;
    }
    // Patch C-B: capture pre-delta live snapshot before the snapshot
    // assignment for local-closure bookkeeping (plan §7.0); audit Fix
    // HIGH 2 captures the full WorkingSnapshot for fingerprint-delta
    // detection.
    let pre_live = state.live.clone();
    let snapshot = response.snapshot.clone();
    state.live = snapshot;
    state.apply_worker_structure_updates(&response);
    // Patch C-O HIGH 1 (c): bookkeeping appends
    // `DeleteLocalClosureRecord` commands for each invalidated record;
    // merged into every return path below.
    let mut delete_commands: Vec<ProtocolCommand> = Vec::new();
    apply_local_closure_acceptance_bookkeeping(
        state,
        &mut response,
        &pre_live,
        &mut delete_commands,
    );
    // Cleanup invariant: every accepted cleanup-phase state must be
    // Done-valid (formalization_complete). If a cleanup worker burst
    // would re-introduce open sorrys or global blockers, reject and
    // revert to the previously accepted state — which by induction is
    // Done-valid. Only enforced when actually in Phase::Cleanup; the
    // ProofFormalization+cleanup-validation path may legitimately leave
    // open work for the next stage.
    //
    // Note: difficulty_updates and ensure_node_metadata are applied
    // BELOW (after the invariant check passes). If applied above and
    // the burst is rejected, restore_committed reverts live/structure
    // but not node_difficulty — leaking stale difficulty entries for
    // phantom nodes the worker proposed and we discarded.
    //
    // Patch C-B: closure-state mutations applied above
    // (`apply_local_closure_acceptance_bookkeeping`) are reverted by
    // `restore_committed` here too — `restore_committed` was extended in
    // Patch C-A to roll back the closure live tier from the committed
    // mirrors and recompute reverse indices.
    if state.phase == Phase::Cleanup && !state.formalization_complete() {
        state.restore_committed();
        // Patch C-O HIGH 1 (c): on cleanup-invariant rejection,
        // `restore_committed` reverts the closure live tier (records
        // are restored from `committed_local_closure_records`). The
        // delete commands collected for this rejected burst would
        // produce file deletions for records the kernel is keeping —
        // drop them.
        drop(delete_commands);
        // Cleanup-v2 Step 7 + audit Finding 6: the formalization-invariant-
        // break path is an Invalid-equivalent in cleanup-v2. Mark the active
        // task Failed + bump counter BEFORE `reject_cleanup_worker_response`
        // issues the next Review (else the in-flight Review carries stale
        // task status).
        mark_cleanup_task_failed(
            state,
            "formalization_complete invariant broken (open sorrys or global blockers reintroduced)"
                .to_string(),
        );
        let result = reject_cleanup_worker_response(
            state,
            request,
            &response,
            Some("cleanup worker burst would break formalization_complete invariant (open sorrys or global blockers reintroduced)"),
        );
        return result;
    }
    state.apply_difficulty_updates(&response.difficulty_updates);
    state.ensure_node_metadata();
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.proof_edit_mode = ProofEditMode::Local;
    clear_retry_and_pending_task(state);
    clear_latest_verifier_review_contexts(state);
    if state.orphan_cleanup_needed() {
        schedule_orphan_cleanup(state);
        state.stage = Stage::Worker;
        let mut out = delete_commands;
        out.push(issue_request(state, RequestKind::Worker));
        return Ok(out);
    }
    if request.worker_context.validation_kind == WorkerValidationKind::Cleanup
        && matches!(
            state.phase,
            Phase::TheoremStating | Phase::ProofFormalization
        )
    {
        // ProofFormalization-only: when the paper frontier is already
        // drained, fall through to `apply_proof_paper_accept` so the
        // `formalization_complete()` → `Phase::Cleanup` transition fires
        // (route_after_progress alone wouldn't issue the
        // `CommitCheckpoint` command + phase flip).
        if state.phase == Phase::ProofFormalization && state.paper_verify_targets().is_empty() {
            let mut out = delete_commands;
            out.extend(apply_proof_paper_accept(state)?);
            return Ok(out);
        }
        // K-1 verifier-starvation deadlock: post-cleanup-Valid in
        // TheoremStating can fall through to `Stage::Reviewer` with all
        // paper-target + substantiveness + corr + sound blockers in
        // structural-Unknown state (no verifier evidence anywhere —
        // `latest_*_review_*` were just cleared at lines above).
        // `review_response_legal`'s blocker-action contract then leaves
        // task→Fail as the only meaningful action the reviewer could take
        // on any Unknown blocker (no verifier evidence to override, not a
        // current Fail to reset), pinning
        // `status=Fail+approved_fp=current_fp`; the next cycle's
        // `theorem_start_request_kind` would read `current==approved` as
        // "verifier ran" and dispatch a Worker burst instead of a
        // verifier, starving verifier dispatch indefinitely.
        //
        // `e6320f6` fixed this for the theorem-cleanup-Valid path by
        // routing through `apply_theorem_paper_accept`. This generalises
        // via `route_after_progress`, which preempts Reviewer dispatch
        // with a verifier whenever any Unknown blocker is
        // *non-adjudicable* (its object is not in
        // `latest_*_review_*` for its lane). Post-cleanup all
        // `latest_*_review_*` are cleared, so all Unknowns are
        // non-adjudicable and the helper dispatches the highest-priority
        // verifier whose frontier is non-empty (paper → corr → sound).
        // Mirrors the K-1 fix's behaviour at this site, with the proof-
        // side analog now sharing the same routing logic.
        let mut out = delete_commands;
        out.extend(route_after_progress(state));
        return Ok(out);
    }
    // Phase::Cleanup polish-Valid (FinalCleanup validation_kind):
    // verifier-evidence contexts were just cleared above. Cleanup
    // invariant guarantees `formalization_complete()` (no global
    // blockers), so `route_after_progress` will dispatch Reviewer here;
    // routing through it keeps the safety net symmetric with the
    // TheoremStating / ProofFormalization branches above and preserves
    // the K-1 / external-audit-Finding-2 protection if a future change
    // ever lets this fall through with surviving Unknowns.
    //
    // Cleanup-v2 Step 7: mark the active task Completed (reset
    // consecutive-invalid counter, clear cleanup_active_task). Idempotent
    // when there's no active task (legacy lint-only flow).
    if state.phase == Phase::Cleanup {
        mark_cleanup_task_completed(state);
    }
    let mut out = delete_commands;
    out.extend(route_after_progress(state));
    Ok(out)
}

/// Patch C-Q Q10 — classification of a node for closure-record
/// eligibility. The sorry-free-only invariant (plan §7.2) admits records
/// only for nodes that are simultaneously (a) present, (b) proof-bearing,
/// and (c) NOT in `live.open_nodes`. Four engine sites historically
/// re-implemented this triplet check inline; the enum centralizes the
/// classification so the call sites can pattern-match on the outcome
/// (drop / drop / drop / install) rather than thread three booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordEligibility {
    /// Node is no longer in `live.present_nodes` — caller should drop
    /// any record/failure/unverified entry without inserting anything.
    NotPresent,
    /// Node is no longer in `proof_nodes` (kind-flipped to Definition,
    /// for instance) — drop records but do NOT insert unverified
    /// (non-proof nodes don't enter the closure-records lifecycle).
    NotProof,
    /// Node has sorry (`live.open_nodes.contains(node)`); records and
    /// unverified entries are mutually exclusive with open membership
    /// per §7.2. Drop record; do not install.
    Open,
    /// Node is sorry-free, present, and proof-bearing — eligible to
    /// hold a closure record or an unverified-set entry.
    Eligible,
}

/// Patch C-Q Q10 — classify a node against the sorry-free-only invariant.
/// Returns one of `NotPresent`, `NotProof`, `Open`, `Eligible`. Callers
/// match on the variant to decide whether to drop a record outright or
/// insert one. Centralizes the triplet (present / proof-bearing / open)
/// that previously lived inline at four engine sites.
pub(crate) fn classify_record_eligibility(
    state: &ProtocolState,
    node: &NodeId,
) -> RecordEligibility {
    if !state.live.present_nodes.contains(node) {
        return RecordEligibility::NotPresent;
    }
    if !state.proof_nodes.contains(node) {
        return RecordEligibility::NotProof;
    }
    if state.live.open_nodes.contains(node) {
        return RecordEligibility::Open;
    }
    RecordEligibility::Eligible
}

/// Patch C-B — pure-state engine API installing a deterministic-revalidation
/// batch (plan §7.5). Refreshed records replace any existing entry and
/// clear the node from `local_closure_unverified_nodes` and
/// `local_closure_failures`. Still-unverified entries land in
/// `local_closure_failures` and are added (or kept) in
/// `local_closure_unverified_nodes`. Reverse indices are recomputed at
/// the end so a refreshed record's helper / strict-dep keys surface
/// immediately for the next invalidation walk.
///
/// The runtime CLI (Patch C-D) is responsible for actually running the
/// probes and constructing the batch; this engine API is the pure-state
/// install step.
///
/// Audit Fix HIGH 6: every entry is filtered against current state before
/// install. A batch entry survives only if its node is currently:
///   * `live.present_nodes.contains(node)` (still a present node),
///   * `proof_nodes.contains(node)` (still proof-bearing — definitions /
///     non-proof nodes do NOT enter the closure-records lifecycle), AND
///   * `!live.open_nodes.contains(node)` (sorry-free at present).
/// Entries failing any of the three are silently dropped. The previous
/// blind insert could reintroduce records or unverified-set membership
/// for nodes that had since been opened, deleted, or flipped to non-
/// proof, violating the sorry-free-only invariant of plan §7.2. The
/// debug-build mutual-exclusion assertion mirrors the one in
/// `apply_local_closure_acceptance_bookkeeping`.
///
/// Patch C-Q Q10: the triplet check is now centralized in
/// `classify_record_eligibility`.
pub fn apply_revalidation_batch(state: &mut ProtocolState, batch: RevalidationBatch) {
    apply_revalidation_batch_with_exclusions(state, batch, &BTreeSet::new());
}

/// Audit C-1 / H-3 — apply a revalidation batch while EXCLUDING entries
/// for nodes that received a same-burst probe-result install. The
/// cleanup-revalidation batch is built from a pre-burst state snapshot
/// inside `CleanupRevalidationAdapter::dispatch`; if the same burst's
/// `response.local_closure_results` already installed a record for a
/// node, the local probe ran AFTER the worker mutated disk, so its
/// post-burst record wins over the staler batch entry.
///
/// Additionally runs the canonical consistency predicate against the
/// CURRENT (post-burst) live state for every batch entry: any record
/// whose `kernel_semantic_hashes` no longer match
/// `live.corr_current_fingerprints` (drift introduced by this burst's
/// delta), whose owner went sorryd / vanished, or whose deps vanished
/// is dropped and the node is forced into
/// `local_closure_unverified_nodes` so the next probe pass refreshes
/// it.
pub fn apply_revalidation_batch_with_exclusions(
    state: &mut ProtocolState,
    batch: RevalidationBatch,
    excluded_nodes: &BTreeSet<NodeId>,
) {
    for (node, record) in batch.refreshed {
        // Patch C-Q Q10: triplet check via `classify_record_eligibility`.
        // The three drop arms map to the §7.2 sorry-free-only invariant.
        if !matches!(
            classify_record_eligibility(state, &node),
            RecordEligibility::Eligible
        ) {
            continue;
        }
        // Audit H-3: refuse to overwrite same-burst probe-derived
        // records — the local probe ran against post-burst disk;
        // the cleanup batch is pre-burst.
        if excluded_nodes.contains(&node) {
            continue;
        }
        // Audit C-1: refuse to install a batch record that fails the
        // canonical predicate against the CURRENT (post-burst) state.
        // The most common stale case: a dep helper's
        // `corr_current_fingerprints` value drifted during this burst,
        // but the batch entry's `kernel_semantic_hashes[helper]` still
        // points at the pre-burst value. The pure-state pass uses
        // `axcheck_required = false`; the runtime-CLI per-step rescission
        // hooks (H-2 / H-4) cover policy-tier checks separately.
        if record.is_consistent_with_state(state, false).is_err() {
            // Mark the node unverified so the next deterministic-
            // revalidation pass re-probes it. Don't write a failure
            // summary — staleness is "needs re-probe", not "probed
            // and failed".
            state.local_closure_records.remove(&node);
            state.local_closure_failures.remove(&node);
            state.local_closure_unverified_nodes.insert(node);
            continue;
        }
        state.local_closure_records.insert(node.clone(), record);
        state.local_closure_unverified_nodes.remove(&node);
        state.local_closure_failures.remove(&node);
    }
    for (node, summary) in batch.still_unverified {
        if !matches!(
            classify_record_eligibility(state, &node),
            RecordEligibility::Eligible
        ) {
            continue;
        }
        // Audit H-3: refuse to overwrite same-burst probe outcomes —
        // the local probe's verdict (Pass or Fail) wins.
        if excluded_nodes.contains(&node) {
            continue;
        }
        state.local_closure_failures.insert(node.clone(), summary);
        state.local_closure_unverified_nodes.insert(node);
    }
    crate::model::recompute_local_closure_reverse_indices(state);
    // Audit Fix HIGH 6: mutual-exclusion invariant assertion mirroring
    // `apply_local_closure_acceptance_bookkeeping`. The filters above
    // already drop entries for open / absent / non-proof nodes, so the
    // assertion should never fire — it catches future regressions that
    // bypass the filters.
    #[cfg(debug_assertions)]
    {
        let intersection: Vec<&NodeId> = state
            .live
            .open_nodes
            .intersection(&state.local_closure_unverified_nodes)
            .collect();
        debug_assert!(
            intersection.is_empty(),
            "live.open_nodes ∩ local_closure_unverified_nodes = {:?} after apply_revalidation_batch",
            intersection
        );
    }
}

/// Audit Fix HIGH 2: per-node fingerprint-delta detection. Returns the
/// set of nodes whose `WorkingSnapshot` per-node fingerprint changed
/// between `pre_live` and `post_live` across every fingerprint map.
/// `worker_semantic_delta` treats fingerprint changes as semantic
/// deltas; this helper exposes the per-node granularity the accept-time
/// invalidation walk needs to flag content-only edits (Lean proof body
/// or statement text changes that leave structural maps untouched).
///
/// The paper-current map is keyed by `TargetId`, not `NodeId`; when a
/// paper-current fingerprint changes we add the covering nodes for
/// that target (per `pre_live.coverage`) to the changed-node set, so
/// consumers reading a target's statement see the propagation.
fn fingerprint_changed_nodes(
    pre_live: &WorkingSnapshot,
    post_live: &WorkingSnapshot,
) -> BTreeSet<NodeId> {
    let mut changed: BTreeSet<NodeId> = BTreeSet::new();
    let node_keyed_maps: &[(
        &BTreeMap<NodeId, Fingerprint>,
        &BTreeMap<NodeId, Fingerprint>,
    )] = &[
        (
            &pre_live.target_fingerprints,
            &post_live.target_fingerprints,
        ),
        (
            &pre_live.corr_current_fingerprints,
            &post_live.corr_current_fingerprints,
        ),
        (
            &pre_live.sound_current_fingerprints,
            &post_live.sound_current_fingerprints,
        ),
        (
            &pre_live.substantiveness_current_fingerprints,
            &post_live.substantiveness_current_fingerprints,
        ),
    ];
    for (pre_map, post_map) in node_keyed_maps {
        let all_keys: BTreeSet<&NodeId> = pre_map.keys().chain(post_map.keys()).collect();
        for key in all_keys {
            if pre_map.get(key) != post_map.get(key) {
                changed.insert(key.clone());
            }
        }
    }
    // Paper-current fingerprints are keyed by TargetId; project the
    // changed targets to their covering nodes (using the pre-delta
    // coverage map, since the reverse-index invalidation walk operates
    // against pre-delta record contents). Newly-added / removed targets
    // are caught by key-mismatch comparison.
    let paper_pre = &pre_live.paper_current_fingerprints;
    let paper_post = &post_live.paper_current_fingerprints;
    let all_targets: BTreeSet<&TargetId> = paper_pre.keys().chain(paper_post.keys()).collect();
    for target in all_targets {
        if paper_pre.get(target) != paper_post.get(target) {
            if let Some(covering) = pre_live.coverage.get(target) {
                for node in covering {
                    changed.insert(node.clone());
                }
            }
            if let Some(covering) = post_live.coverage.get(target) {
                for node in covering {
                    changed.insert(node.clone());
                }
            }
        }
    }
    changed
}

/// Patch C-B — accept-time local-closure bookkeeping. Called by every
/// `apply_*_worker_response` Valid path AFTER `state.live = snapshot`
/// and `state.apply_worker_structure_updates(&response)` so the helper
/// reads the post-delta live snapshot (`state.live.open_nodes` /
/// `state.live.present_nodes` / `state.proof_nodes`) and references the
/// pre-delta `pre_live` parameter captured by the caller before the
/// assignment.
///
/// Implements plan §7.0 contract:
///
/// 1. Sorry-free → sorryd transitions: nodes that NOW have sorry but did
///    not before lose their `local_closure_records[N]` entry plus any
///    failure / unverified-set membership. The records-map invariant
///    `records.contains_key(N) ⇔ N is sorry-free since most-recent
///    sorry-free transition` is the load-bearing property.
///
/// 2. Sorryd → sorry-free transitions: every node N appearing in
///    `response.local_closure_results` that is now sorry-free has a
///    `LocalClosureRecord` (probe `status == "ok"`) or an `ErrorSummary`
///    (probe failure) installed. Hash inputs are placeholders for now
///    (the runtime CLI in Patch C-D computes real toolchain / manifest
///    / preamble / approved-axioms / per-decl hashes). The approved-
///    kernel-axioms subset check is permissively stubbed at C-B because
///    the Patch B `must_close_active` rejection gate already enforced
///    it for any `must_close_active=true` accept; non-MCA accepts and
///    the Patch C-D non-active-node sweep are handled rigorously by the
///    runtime CLI per-node `load_approved_axioms` call before record
///    installation. (Patch C-Q Q7: prior TODO referred to C-D, which
///    shipped. The placeholders are now intentional pure-state
///    sentinels; the runtime CLI's `backfill_local_closure_record_hashes`
///    pass replaces them with real on-disk hashes after every step.)
///
/// 3. Node deletions: every node N that was in `pre_present` but is no
///    longer in `state.live.present_nodes` has all closure state purged
///    — records, failures, unverified-set membership, AND every reverse-
///    index entry where N appears as either key or value-set element.
///    Mirrors plan §7.0's orphan-cleanup contract.
///
/// 4. Conservative invalidation: in Patch C-B, any node whose .lean
///    content might have changed (any node appearing in the structure
///    deltas) marks its consumers stale via the pre-delta reverse
///    indices captured BEFORE `apply_worker_structure_updates` mutated
///    the indices; consumers are removed from records and added to
///    unverified (with no failure summary — they're just stale, not
///    failed). (Patch C-Q Q7: prior TODO referred to C-D's hash-precise
///    diffing; the conservative walk is intentional today — the
///    deterministic-revalidation pass at the CLI cheaply re-probes
///    flagged consumers, so we trade work-budget for engine simplicity.)
///
/// 5. Apply revalidation batch (`response.local_closure_revalidation`)
///    if present.
///
/// 6. Recompute reverse indices from the post-bookkeeping records map
///    (the safety-net rebuild after the per-step incremental mutations).
///
/// 7. Debug assertion: `live.open_nodes ∩
///    local_closure_unverified_nodes` must be empty (sorry-free-only
///    invariant per plan §7.2 / mutual-exclusion enforcement per §7.0).
fn apply_local_closure_acceptance_bookkeeping(
    state: &mut ProtocolState,
    response: &mut WorkerResponse,
    pre_live: &WorkingSnapshot,
    commands: &mut Vec<ProtocolCommand>,
) {
    // Patch C-O HIGH 1 (c): collect every node whose persisted disk
    // record should be deleted because we just invalidated the
    // in-memory copy. Appended to `commands` as
    // `DeleteLocalClosureRecord` so the runtime CLI removes
    // `<runtime_root>/checker-state/local-closure-records/<node>.json`.
    let mut deleted_records: Vec<NodeId> = Vec::new();
    let pre_open = &pre_live.open_nodes;
    let pre_present = &pre_live.present_nodes;
    // (a) Capture pre-delta reverse indices for the conservative
    // invalidation walk in step (d). The indices were computed against
    // the records BEFORE this delta, so they correctly identify
    // consumers of helpers/deps as they were named in the records being
    // invalidated. (Inside the helper we mutate records, which would
    // de-sync state.boundary_statement_consumers / .strict_dep_consumers
    // mid-walk — the snapshot here is the load-bearing prerequisite of
    // §7.3's snapshot-then-mutate algorithm.)
    let pre_boundary_consumers = state.boundary_statement_consumers.clone();
    let pre_strict_consumers = state.strict_dep_consumers.clone();

    // (b) Sorry-free → sorryd transition deletions (plan §7.0).
    // A node was sorry-free at pre-delta time iff it was NOT in pre_open
    // (textual has_sorry from worker_normalization::open_nodes_from_repo);
    // it is sorryd at post-delta time iff it IS in state.live.open_nodes.
    // The transition fires for the intersection (sorryd ∩ ¬pre_open).
    // The records-invariant requires deleting any record / failure /
    // unverified-set membership for these nodes so a future sorry-free
    // transition installs a fresh record rather than silently inheriting
    // the survivor.
    let sorry_free_to_sorryd: Vec<NodeId> = state
        .live
        .open_nodes
        .iter()
        .filter(|n| !pre_open.contains(*n))
        .cloned()
        .collect();
    for node in &sorry_free_to_sorryd {
        if state.local_closure_records.remove(node).is_some() {
            deleted_records.push(node.clone());
        }
        state.local_closure_failures.remove(node);
        state.local_closure_unverified_nodes.remove(node);
    }

    // (c) Node deletions (plan §7.0). Nodes that were present pre-delta
    // and are no longer present post-delta have ALL closure state
    // purged. Reverse-index value-set membership is also stripped: a
    // deleted node N may have been a CONSUMER of some helper H or strict
    // dep D; the consumer entry for N must vanish from H's / D's value
    // sets so a future invalidation walk doesn't surface a phantom N.
    // Per-key removal where N is the KEY (i.e. N was a producer) also
    // happens here.
    let removed_nodes: Vec<NodeId> = pre_present
        .iter()
        .filter(|n| !state.live.present_nodes.contains(*n))
        .cloned()
        .collect();
    for node in &removed_nodes {
        if state.local_closure_records.remove(node).is_some() {
            deleted_records.push(node.clone());
        }
        state.local_closure_failures.remove(node);
        state.local_closure_unverified_nodes.remove(node);
        state.boundary_statement_consumers.remove(node);
        state.strict_dep_consumers.remove(node);
        for consumers in state.boundary_statement_consumers.values_mut() {
            consumers.remove(node);
        }
        for consumers in state.strict_dep_consumers.values_mut() {
            consumers.remove(node);
        }
    }

    // (d) Conservative invalidation (Patch C-B simplification of plan
    // §7.3). Any node touched by the structure deltas might have its
    // .lean content changed; consumers of such nodes (per the pre-delta
    // reverse indices) are marked stale. We don't write a failure
    // summary — staleness is "needs re-probe", not "probed and failed".
    //
    // Audit Fix HIGH 2: the structural delta lists miss content-only
    // changes (a worker that edits the Lean proof body or statement
    // text WITHOUT touching node-kind / dep / target_claim maps still
    // produces a fingerprint delta in the response snapshot, since the
    // runtime observation layer recomputes fingerprints from the worker
    // edit). We therefore include any node whose WorkingSnapshot
    // fingerprint changed between `pre_live` and `state.live` in
    // `potentially_changed`. We additionally invalidate the producer's
    // OWN record when its own content / fingerprint changed (the
    // structural-only walk previously only flagged CONSUMERS via the
    // reverse indices; the producer's record was left intact even
    // though its closure-record-input hash had drifted).
    //
    // Patch C-Q Q7: prior TODO referred to C-D's hash-precise diffing
    // (now shipped). The conservative walk remains intentional —
    // engine-side hash diffing would require pulling on-disk inputs
    // into pure state. The deterministic-revalidation pass at the CLI
    // re-probes consumers cheaply, so the conservative invalidation
    // exchanges a small extra-probe budget for engine simplicity.
    let mut potentially_changed: BTreeSet<NodeId> = BTreeSet::new();
    for (node, update) in &response.proof_node_updates {
        if !matches!(update, Update::Same) {
            potentially_changed.insert(node.clone());
        }
    }
    for (node, update) in &response.node_kind_updates {
        if !matches!(update, Update::Same) {
            potentially_changed.insert(node.clone());
        }
    }
    for (node, update) in &response.dep_updates {
        if !matches!(update, Update::Same) {
            potentially_changed.insert(node.clone());
        }
    }
    for (node, update) in &response.target_claim_updates {
        if !matches!(update, Update::Same) {
            potentially_changed.insert(node.clone());
        }
    }
    // Newly-present nodes don't have records to invalidate themselves,
    // but their introduction may have structurally rebound consumers'
    // boundary or strict-dep references. The conservative inclusion is
    // already covered by their appearance in dep_updates / kind_updates;
    // we add the symmetric difference of present_nodes anyway as a
    // belt-and-suspenders against carrier deltas the worker normalisation
    // emits implicitly. Removed nodes are handled by step (c) (and their
    // consumers are also flagged here for invalidation).
    for node in pre_present.symmetric_difference(&state.live.present_nodes) {
        potentially_changed.insert(node.clone());
    }
    // Audit Fix HIGH 2: per-node fingerprint deltas in the post-delta
    // `WorkingSnapshot`. `worker_semantic_delta` already treats these
    // changes as semantic deltas; we now mirror the per-node granularity
    // into the invalidation walk.
    let fingerprint_changed: BTreeSet<NodeId> = fingerprint_changed_nodes(pre_live, &state.live);
    potentially_changed.extend(fingerprint_changed.iter().cloned());

    let mut invalidation_set: BTreeSet<NodeId> = BTreeSet::new();
    for node in &potentially_changed {
        if let Some(consumers) = pre_boundary_consumers.get(node) {
            invalidation_set.extend(consumers.iter().cloned());
        }
        if let Some(consumers) = pre_strict_consumers.get(node) {
            invalidation_set.extend(consumers.iter().cloned());
        }
    }
    for consumer in &invalidation_set {
        // Skip consumers that vanished in this delta (already pruned by
        // step (c)) and consumers that are now sorryd (live.open_nodes
        // membership is the textual sorryd predicate; sorryd nodes
        // belong in the open-nodes flow, not the unverified set, per the
        // sorry-free-only invariant of §7.2).
        //
        // Patch C-Q Q10: triplet via `classify_record_eligibility`.
        match classify_record_eligibility(state, consumer) {
            RecordEligibility::NotPresent => {
                // Already pruned by step (c); nothing more to do.
            }
            RecordEligibility::Open | RecordEligibility::NotProof => {
                // Either case: drop the record but do NOT enter
                // unverified. For `Open`, the sorry-free → sorryd
                // contract via step (b) handles new sorrys; a consumer
                // marked stale that was already sorryd also has no
                // business holding a record. For `NotProof` (e.g.
                // kind-flipped to Definition), non-proof nodes do not
                // enter the unverified set.
                if state.local_closure_records.remove(consumer).is_some() {
                    deleted_records.push(consumer.clone());
                }
            }
            RecordEligibility::Eligible => {
                // Sorry-free, present, proof-bearing: mark stale.
                if state.local_closure_records.remove(consumer).is_some() {
                    deleted_records.push(consumer.clone());
                }
                state
                    .local_closure_unverified_nodes
                    .insert(consumer.clone());
                // We deliberately do NOT write a failure summary here.
                // Stale-because-dep-changed is "needs re-probe", not
                // "probed and failed". The deterministic-revalidation
                // pass refreshes the record on success or writes a
                // failure summary on probe failure.
            }
        }
    }
    // Audit Fix HIGH 2: invalidate the producer's OWN record when its
    // own closure-record-input changed. Only fingerprint-driven
    // entries trigger this; structural deltas (kind / dep / target_claim
    // updates) often shuffle a node's metadata without changing its
    // closure-record-input hashes, and step (e) below handles those by
    // installing fresh records from the probe payload. By contrast, a
    // content/fingerprint change without a fresh probe leaves the
    // record stale on disk — drop it and mark unverified.
    for node in &fingerprint_changed {
        // Patch C-Q Q10: triplet via `classify_record_eligibility`.
        match classify_record_eligibility(state, node) {
            RecordEligibility::NotPresent => {
                // Vanished in this delta; nothing to do.
            }
            RecordEligibility::Open | RecordEligibility::NotProof => {
                if state.local_closure_records.remove(node).is_some() {
                    deleted_records.push(node.clone());
                }
            }
            RecordEligibility::Eligible => {
                // Only act if we currently hold a record — the producer
                // own-record invalidation is narrowly scoped to records
                // present at this point.
                if !state.local_closure_records.contains_key(node) {
                    continue;
                }
                state.local_closure_records.remove(node);
                deleted_records.push(node.clone());
                state.local_closure_unverified_nodes.insert(node.clone());
            }
        }
    }

    // (e) Sorryd → sorry-free record creation (plan §7.0).
    // Every node N in response.local_closure_results that is now sorry-
    // free gets a record (probe ok) or a failure summary (probe failed).
    // We use the sorry-free-after invariant: any node in results that
    // is sorry-free at post-delta time is recorded. Nodes that ended
    // up sorryd (e.g. probe ran pre-auto-fix and auto-fix reintroduced
    // sorry) are skipped; those are caught by step (b).
    //
    // Patch C-Q Q8 (doc refresh): post-C-O, the runtime CLI emits
    // results from two distinct channels: (1) per-node probe results
    // attached to a worker's `WorkerResponse.local_closure_results`
    // map by the worker pipeline, and (2) a deterministic-revalidation
    // batch attached via `WorkerResponse.local_closure_revalidation`
    // by the `CleanupRevalidationAdapter`. This step (e) loop handles
    // channel (1); step (f) below handles channel (2) via
    // `apply_revalidation_batch`.
    //
    // Patch C-Q Q7: prior TODO(Patch C-D) referred to backfilling
    // toolchain / manifest / preamble / approved-axioms / per-decl
    // hashes from runtime-CLI side. C-D shipped: the engine
    // intentionally writes sentinel placeholders here (pure-state
    // boundary), and the runtime CLI's `backfill_local_closure_record_hashes`
    // pass replaces them with real hashes after every step. Records
    // are present-shaped (so §7.6's records_present clause sees them);
    // drift detection becomes live the moment backfill runs.
    //
    // Patch C-Q Q7: prior TODO(Patch C-D) for per-node approved-axioms
    // recheck — also shipped. The deterministic-revalidation pass at
    // the CLI invokes `load_approved_axioms` before installing any
    // refreshed record; the Patch B MCA gate still enforces it for
    // active-node accepts; this engine path covers the sorryd→sorry-
    // free transition with backfill-time validation.
    let cycle = response.cycle as u64;
    let probe_results = std::mem::take(&mut response.local_closure_results);
    // Audit H-3 — track nodes that received a same-burst probe-result
    // record install (step (e) below). The cleanup-revalidation batch
    // in step (f) MUST NOT overwrite those: the local probe ran
    // against post-burst disk; the cleanup batch was built from a
    // pre-burst snapshot inside `CleanupRevalidationAdapter::dispatch`
    // (see `bin/runtime_cli.rs::step_runtime`), so its records are
    // staler.
    let mut probe_installed_nodes: BTreeSet<NodeId> = BTreeSet::new();
    for (node, probe) in probe_results {
        // Skip nodes that became sorryd or vanished in this delta, or
        // are not proof-bearing.
        //
        // Patch C-Q Q10: triplet via `classify_record_eligibility`.
        // The original code only checked present + open here; adding
        // the proof-bearing check is a defense-in-depth tightening,
        // since `probe_results` is keyed by proof nodes by construction
        // but a kind-flip during the delta could have moved a node
        // out of `proof_nodes` between probe emission and accept.
        if !matches!(
            classify_record_eligibility(state, &node),
            RecordEligibility::Eligible
        ) {
            continue;
        }
        // Audit Fix MEDIUM (defensive accept-time record install): even
        // when the probe reports `status == "ok"` with no errors, verify
        // `kernel_axioms ⊆ ENGINE_CANONICAL_APPROVED_AXIOMS` before
        // installing a record. The `must_close_active` gate validates
        // against the broader per-node approved set (which can include
        // additional axioms via `APPROVED_AXIOMS.json`), but the engine
        // does not have disk access here; the canonical four are the
        // safety-net ceiling. Probes that report non-canonical axioms
        // (even if accepted by the disk gate) are routed through the
        // failure path and re-installed by the deterministic-
        // revalidation pass (which constructs `RevalidationBatch`
        // entries with per-node approved sets validated).
        let axiom_violations: Vec<String> = probe
            .kernel_axioms
            .iter()
            .filter(|axiom| !ENGINE_CANONICAL_APPROVED_AXIOMS.contains(&axiom.as_str()))
            .cloned()
            .collect();
        let probe_ok =
            probe.status == "ok" && probe.errors.is_empty() && axiom_violations.is_empty();
        if probe_ok {
            // Patch C-Q Q7: prior TODO(Patch C-D) referred to per-node
            // approved-axioms recheck — shipped. The CLI-side backfill
            // and deterministic-revalidation passes do the per-node
            // recheck; this engine path writes sentinel hash fields
            // (real values supplied by the backfill pass) but is no
            // longer the gate.
            //
            // Patch C-P HIGH 1 (b) — capture the kernel's current
            // `semantic_hash` (i.e. `corr_current_fingerprints` value)
            // for every dep across all three categories. Migration-time
            // comparison in `record_hashes_match_current` rejects the
            // record on any drift. Empty string for deps the kernel has
            // not yet fingerprinted (rare; typically only at the very
            // first burst before observations have populated the map).
            //
            // Patch C-Q Q11 — the loop body lives in
            // `model::populate_kernel_semantic_hashes` so the
            // runtime-CLI deterministic-revalidation site shares one
            // implementation. We build a partial record with the dep
            // maps populated, run the shared helper, then keep the
            // resulting `kernel_semantic_hashes` map.
            // Audit H-4: derive axcheck status from the probe's
            // `axiomization_check` sub-object. `Some(ax)` with
            // `ax.skipped` → `Skipped`; `Some(ax)` with `ax.agreed`
            // → `Agreed`; everything else (including `None` from
            // pre-merge fixtures) defaults to `Skipped` so the
            // canonical predicate flags it when axcheck is required.
            let axcheck_status = match &probe.axiomization_check {
                Some(ax) if ax.skipped => crate::model::AxcheckStatus::Skipped,
                Some(ax) if ax.agreed => crate::model::AxcheckStatus::Agreed,
                Some(_) => crate::model::AxcheckStatus::Disagreed,
                None => crate::model::AxcheckStatus::Skipped,
            };
            let mut record = LocalClosureRecord {
                node: node.clone(),
                closure_version: "TODO_PATCH_C_D_VERSION".to_string(),
                toolchain_hash: "TODO_PATCH_C_D_HASH".to_string(),
                lake_manifest_hash: "TODO_PATCH_C_D_HASH".to_string(),
                preamble_hash: "TODO_PATCH_C_D_HASH".to_string(),
                approved_axioms_hash: "TODO_PATCH_C_D_HASH".to_string(),
                active_decl_hash: "TODO_PATCH_C_D_HASH".to_string(),
                active_statement_hash: "TODO_PATCH_C_D_HASH".to_string(),
                kernel_axioms: probe.kernel_axioms.clone(),
                boundary_theorems: probe.boundary_theorems.clone(),
                strict_theorem_deps: probe.strict_theorem_deps.clone(),
                strict_definition_deps: probe.strict_definition_deps.clone(),
                kernel_semantic_hashes: BTreeMap::new(),
                accepted_at_snapshot_id: format!("cycle-{}", cycle),
                axcheck_status,
            };
            crate::model::populate_kernel_semantic_hashes(&mut record, state);
            state.local_closure_records.insert(node.clone(), record);
            state.local_closure_unverified_nodes.remove(&node);
            state.local_closure_failures.remove(&node);
            // Audit H-3 — register this node so step (f)'s batch
            // installer does not overwrite the post-burst probe-derived
            // record with a potentially staler cleanup-batch entry.
            probe_installed_nodes.insert(node.clone());
        } else if probe.status == "ok" && probe.errors.is_empty() && !axiom_violations.is_empty() {
            // Probe reported "ok" but escaped the canonical axiom
            // ceiling. Synthesize a defensive failure summary with
            // `status = "axiom_violation"` so the diagnostic UI surfaces
            // the underlying issue.
            let summary = ErrorSummary {
                status: "axiom_violation".to_string(),
                returncode: probe.returncode,
                timed_out: probe.timed_out,
                stderr_excerpt: format!(
                    "[engine-defensive] probe reported ok but kernel_axioms {:?} escape canonical set; refusing record install",
                    axiom_violations
                ),
                axiom_violations: axiom_violations.clone(),
                strict_errors: Vec::new(),
                captured_at_cycle: cycle,
                retry_count: 0,
                last_attempt_cycle: cycle,
                next_retry_cycle: 0,
                retry_exhausted: false,
            };
            if state.local_closure_records.remove(&node).is_some() {
                deleted_records.push(node.clone());
            }
            state.local_closure_failures.insert(node.clone(), summary);
            state.local_closure_unverified_nodes.insert(node.clone());
            // Audit H-3 — the cleanup batch must not "rehabilitate"
            // an axiom-violation node with a stale Pass.
            probe_installed_nodes.insert(node);
        } else {
            // Probe failure: write a failure summary, mark unverified,
            // remove any stale record. Status string is verbatim from
            // the script (or "transport_error" if the runtime CLI
            // synthesized it for IPC failure per plan §7.0). Errors
            // / kernel_axioms violations get serialized for diagnostics.
            let summary = ErrorSummary {
                status: probe.status.clone(),
                returncode: probe.returncode,
                timed_out: probe.timed_out,
                stderr_excerpt: if probe.raw_stderr.len() > 1024 {
                    probe.raw_stderr[..1024].to_string()
                } else {
                    probe.raw_stderr.clone()
                },
                axiom_violations: probe.kernel_axioms.iter().cloned().collect(),
                strict_errors: probe.errors.clone(),
                captured_at_cycle: cycle,
                retry_count: 0,
                last_attempt_cycle: cycle,
                next_retry_cycle: 0,
                retry_exhausted: false,
            };
            if state.local_closure_records.remove(&node).is_some() {
                deleted_records.push(node.clone());
            }
            state.local_closure_failures.insert(node.clone(), summary);
            state.local_closure_unverified_nodes.insert(node.clone());
            // Audit H-3 — same protection for failure outcomes: the
            // stale cleanup batch cannot post-hoc convert a fresh
            // probe failure into a Pass record.
            probe_installed_nodes.insert(node);
        }
    }

    // (f) Apply the deterministic-revalidation batch if present.
    //
    // Patch C-O HIGH 3: route through `apply_revalidation_batch` so the
    // present-node / proof-node / not-open filters defined in C-G run.
    // The cleanup adapter builds the batch from a snapshot of state
    // *before* the worker response is applied; the worker response may
    // simultaneously open, delete, or kind-flip a node, and the
    // direct-insertion path used to install a record or failure for a
    // node that is no longer eligible for local-closure state.
    //
    // Audit H-3 / C-1 — pass `probe_installed_nodes` so the batch
    // installer refuses to overwrite same-burst probe-derived records
    // with potentially staler cleanup-batch entries. The batch is built
    // against a pre-burst state snapshot inside `CleanupRevalidationAdapter`;
    // its records' `kernel_semantic_hashes` are stamped against pre-burst
    // `corr_current_fingerprints`. The canonical predicate check inside
    // the installer also rejects batch entries that fail consistency
    // against the post-burst state (e.g. helper fingerprint that drifted
    // during this burst).
    if let Some(batch) = response.local_closure_revalidation.take() {
        apply_revalidation_batch_with_exclusions(state, batch, &probe_installed_nodes);
    }

    // (g) Recompute reverse indices from the post-bookkeeping records
    // map. Per-step incremental updates above keep the indices roughly
    // aligned, but a full rebuild is cheap and forecloses any drift
    // (especially around the conservative-invalidation walk and the
    // node-deletion value-set scrub which intentionally don't try to
    // surgically rebuild).
    //
    // Audit C-3 — continuous coverage scan. After any worker accept
    // a sorry-free present proof_node that lacks a record AND an
    // unverified entry would silently fail the
    // `formalization_complete` gate. The conservative invalidation
    // walk above doesn't always pick up nodes that materialized
    // sorry-free in this delta (e.g. a worker added a fresh node
    // sorry-free without producing a probe result for it). Pin those
    // orphans into unverified so the next deterministic-revalidation
    // pass refreshes them.
    state.ensure_local_closure_coverage();
    crate::model::recompute_local_closure_reverse_indices(state);

    // (h) Debug-build mutual-exclusion assertion (plan §7.0).
    // `live.open_nodes ∩ local_closure_unverified_nodes` must be empty
    // after every accept — the sorry-free-only invariant of §7.2 says a
    // node has sorry ⇒ it lives in open_nodes (and NOT in unverified);
    // a node is sorry-free and lacks a fresh record ⇒ in unverified
    // (and NOT in open_nodes). Mutually exclusive by construction; this
    // assertion catches accidental violations introduced by future
    // edits.
    #[cfg(debug_assertions)]
    {
        let intersection: Vec<&NodeId> = state
            .live
            .open_nodes
            .intersection(&state.local_closure_unverified_nodes)
            .collect();
        debug_assert!(
            intersection.is_empty(),
            "live.open_nodes ∩ local_closure_unverified_nodes = {:?}",
            intersection
        );
    }

    // Patch C-O HIGH 1 (c): append `DeleteLocalClosureRecord` commands
    // for every node whose record we just invalidated. The runtime CLI
    // honors the command by removing
    // `<runtime_root>/checker-state/local-closure-records/<node>.json`.
    // Dedup so we don't emit duplicate commands when the same node is
    // visited by more than one phase of the bookkeeping pass.
    deleted_records.sort();
    deleted_records.dedup();
    for node in deleted_records {
        commands.push(ProtocolCommand::DeleteLocalClosureRecord { node });
    }
}

fn apply_paper_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    response: PaperResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::VerifyPaper, "VerifyPaper")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed {
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    validate_paper_lane_updates(request, &response)?;

    // Per-cycle scheduling guarantee: exactly one of `paper_verify_targets`
    // / `substantiveness_verify_nodes` is non-empty per Paper request (see
    // `request_paper_verify_targets` and `request_substantiveness_verify_nodes`).
    // We branch on that to drive the right reconciler / status mirror,
    // then fall through to the phase-specific accept.
    let is_deviation_scenario = request.deviation_verify_id.is_some();
    let is_per_node_scenario = request.paper_verify_targets.is_empty()
        && !request.substantiveness_verify_nodes.is_empty()
        && !is_deviation_scenario;

    if is_deviation_scenario {
        let deviation_updates = reconcile_deviation_lane_updates(request, &response);
        apply_deviation_updates(
            &state.deviation_files,
            &mut state.deviation_status,
            &mut state.deviation_approved_fingerprints,
            &state.live.deviation_current_fingerprints,
            deviation_updates,
        );
    } else if is_per_node_scenario {
        // Substantiveness lane (TheoremStating + ProofFormalization). Drive
        // the per-node status and approved-fingerprint mirrors, plus
        // track no-progress for the safety bound.
        let unknown_before = state.substantiveness_verify_nodes();
        let node_updates = reconcile_substantiveness_lane_updates(request, &response);
        apply_substantiveness_updates(
            &mut state.substantiveness_status,
            &mut state.substantiveness_approved_fingerprints,
            &state.live.substantiveness_current_fingerprints,
            node_updates,
        );
        let unknown_after = state.substantiveness_verify_nodes();
        if unknown_after.len() < unknown_before.len() {
            // Frontier shrank — kernel made progress, reset the
            // no-progress counter.
            state.substantiveness_consecutive_no_progress_requests = 0;
        } else if !unknown_after.is_empty() {
            state.substantiveness_consecutive_no_progress_requests = state
                .substantiveness_consecutive_no_progress_requests
                .saturating_add(1);
        } else {
            state.substantiveness_consecutive_no_progress_requests = 0;
        }
        state.latest_substantiveness_review_nodes = request.substantiveness_verify_nodes.clone();
        // Accumulate per-node evidence across the drain loop (parallel
        // to Sound). `latest_substantiveness_reviewer_evidence` is the
        // reviewer-facing surface; `previous_substantiveness_lane_findings`
        // mirrors the most recent response so the next Paper request's
        // revisit fragment renders correctly.
        for (node, lane_evidence) in &response.node_reviewer_evidence {
            state
                .latest_substantiveness_reviewer_evidence
                .entry(node.clone())
                .or_default()
                .extend(lane_evidence.clone());
        }
        state.previous_substantiveness_lane_findings = response.node_reviewer_evidence.clone();
    } else {
        // Target-level paper lane (existing behaviour, unchanged).
        let target_updates = reconcile_paper_target_lane_updates(request, &response);
        apply_target_corr_updates(
            &mut state.paper_status,
            &mut state.paper_approved_fingerprints,
            &state.live.paper_current_fingerprints,
            target_updates,
        );
        state.latest_paper_review_targets = request.paper_verify_targets.clone();
        state.latest_paper_reviewer_evidence = response.reviewer_evidence.clone();
        state.previous_paper_lane_findings = response.reviewer_evidence.clone();
    }

    // Audit-fix #3: clear the OPPOSITE mode's lingering review context so
    // target-mode, deviation-mode, and per-node-mode don't leak evidence across cycles.
    // Within one cycle only one mode runs (the kernel scheduler guarantees
    // exactly one of paper_verify_targets / substantiveness_verify_nodes is
    // non-empty per Paper request), but the kernel may schedule different
    // modes in subsequent cycles; without these clears the reviewer's
    // `request.review_verifier_evidence.paper` would carry stale target-mode
    // evidence into a per-node reviewer cycle (and vice versa for
    // `.substantiveness`). The corresponding `previous_*_lane_findings`
    // fields drive the next verifier-prompt revisit fragment, so they too
    // are scoped to the active mode only.
    if is_deviation_scenario {
        state.latest_paper_review_targets.clear();
        state.latest_paper_reviewer_evidence.clear();
        state.previous_paper_lane_findings.clear();
        state.latest_substantiveness_review_nodes.clear();
        state.latest_substantiveness_reviewer_evidence.clear();
        state.previous_substantiveness_lane_findings.clear();
    } else if is_per_node_scenario {
        state.latest_paper_review_targets.clear();
        state.latest_paper_reviewer_evidence.clear();
        state.previous_paper_lane_findings.clear();
        state.clear_latest_deviation_review_context();
    } else {
        state.latest_substantiveness_review_nodes.clear();
        state.latest_substantiveness_reviewer_evidence.clear();
        state.previous_substantiveness_lane_findings.clear();
        state.clear_latest_deviation_review_context();
    }

    state.clear_latest_corr_review_context();
    state.clear_latest_sound_review_context();
    state.clear_latest_deviation_review_context();
    if is_deviation_scenario {
        state.latest_deviation_review_ids = request.deviation_verify_id.iter().cloned().collect();
        state.latest_deviation_reviewer_evidence = response.reviewer_evidence.clone();
    }
    match state.phase {
        Phase::TheoremStating => apply_theorem_paper_accept(state),
        Phase::ProofFormalization => apply_proof_paper_accept(state),
        Phase::Cleanup | Phase::Complete => Err(TransitionError::InvalidPhase {
            expected: Phase::TheoremStating,
            found: state.phase,
        }),
    }
}

fn apply_theorem_paper_accept(
    state: &mut ProtocolState,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    state.held_target = state.select_theorem_held_target();
    state.clear_pending_task();

    // Verifier ordering invariant (paper-target → substantiveness → corr →
    // sound → review): both paper variants share `Stage::VerifyPaper`,
    // so this drain loop must clear ALL paper Unknowns before
    // transitioning to VerifyCorr / VerifySound. See plan §0 — this
    // function is the choke-point that enforces the cycle ordering;
    // future refactors must preserve it.
    let paper_target_blocker_fail = state.global_blockers().iter().any(|b| {
        matches!(b.kind, BlockerKind::PaperFaithfulness)
            && state.current_failed_blockers().contains(b)
    });
    let deviation_blocker_fail = state
        .current_failed_blockers()
        .iter()
        .any(|b| matches!(b.kind, BlockerKind::Deviation));
    let substantiveness_blocker_fail = state
        .current_failed_blockers()
        .iter()
        .any(|b| matches!(b.kind, BlockerKind::Substantiveness));

    // 1. If any Fail blocker exists in the paper lane (target or
    //    per-node), escalate to Reviewer — UNLESS a fresh non-adjudicable
    //    Unknown elsewhere has a live verifier frontier. In that case
    //    let the verifier weigh in first; otherwise the reviewer can
    //    pin the new fingerprint via task→Fail without verifier evidence.
    //    This arises when a worker rewrites a node's Lean statement: an
    //    existing paper-Fail on a target node can otherwise preempt the
    //    corr verifier dispatch that should re-evaluate the rewritten
    //    statement first.
    if paper_target_blocker_fail || deviation_blocker_fail || substantiveness_blocker_fail {
        // Reset no-progress counter — escalation supersedes the drain.
        state.substantiveness_consecutive_no_progress_requests = 0;
        if let Some(kind) = route_non_adjudicable_unknown_verifier(state) {
            state.stage = match kind {
                RequestKind::Paper => Stage::VerifyPaper,
                RequestKind::Corr => Stage::VerifyCorr,
                RequestKind::Sound => Stage::VerifySound,
                _ => unreachable!("route helper returns only verifier kinds"),
            };
            return Ok(vec![issue_request(state, kind)]);
        }
        state.stage = Stage::Reviewer;
        return Ok(vec![issue_request(state, RequestKind::Review)]);
    }

    // 2. Drain target frontier first (mirrors the existing scheduler
    //    priority; nothing functional changed here).
    if !state.paper_verify_targets().is_empty() {
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 3. Authorize pending deviation files before per-node substantiveness.
    if !state.deviation_verify_ids().is_empty() {
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 4. Per-node frontier safety bound: if the verifier has burned
    //    `SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS` consecutive requests
    //    without shrinking the Unknown set (i.e. all responses came
    //    back NotDoneYet), escalate to Reviewer with the diagnostic
    //    pinned via `latest_substantiveness_review_nodes`. Reviewer can
    //    then choose to NeedInput, override-blocker the stuck nodes,
    //    or task the worker to restructure them.
    let node_frontier = state.substantiveness_verify_nodes();
    if !node_frontier.is_empty() {
        if state.substantiveness_consecutive_no_progress_requests
            >= crate::SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS
        {
            state.substantiveness_consecutive_no_progress_requests = 0;
            state.stage = Stage::Reviewer;
            return Ok(vec![issue_request(state, RequestKind::Review)]);
        }
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 5. All paper frontiers drained. Reset no-progress counter for
    //    the next cycle and fall through to corr/sound/review.
    state.substantiveness_consecutive_no_progress_requests = 0;
    let has_corr_verification = !state.corr_verify_nodes().is_empty();
    let has_sound_verification = !state.sound_verify_nodes().is_empty();
    state.stage = if has_corr_verification {
        Stage::VerifyCorr
    } else if has_sound_verification {
        Stage::VerifySound
    } else {
        Stage::Reviewer
    };
    Ok(vec![issue_request(
        state,
        match state.stage {
            Stage::VerifyCorr => RequestKind::Corr,
            Stage::VerifySound => RequestKind::Sound,
            _ => RequestKind::Review,
        },
    )])
}

fn apply_proof_paper_accept(
    state: &mut ProtocolState,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    prepare_proof_verifier_accept(state);

    // Verifier ordering invariant (paper-target → substantiveness → corr →
    // sound → review): both paper variants share `Stage::VerifyPaper`,
    // so this drain loop must clear ALL paper Unknowns before
    // transitioning to VerifyCorr / VerifySound. Mirrors
    // `apply_theorem_paper_accept`; see plan §0 — this function is the
    // proof-side choke-point that enforces the cycle ordering.
    let paper_target_blocker_fail = state.global_blockers().iter().any(|b| {
        matches!(b.kind, BlockerKind::PaperFaithfulness)
            && state.current_failed_blockers().contains(b)
    });
    let deviation_blocker_fail = state
        .current_failed_blockers()
        .iter()
        .any(|b| matches!(b.kind, BlockerKind::Deviation));
    let substantiveness_blocker_fail = state
        .current_failed_blockers()
        .iter()
        .any(|b| matches!(b.kind, BlockerKind::Substantiveness));

    // 1. Any Fail blocker in the paper lane (target or per-node) escalates
    //    to Reviewer — UNLESS a fresh non-adjudicable Unknown elsewhere has
    //    a live verifier frontier; in that case let the verifier weigh in
    //    first so the reviewer doesn't task→Fail-pin a fingerprint the
    //    verifier never adjudicated. Mirrors apply_theorem_paper_accept.
    if paper_target_blocker_fail || deviation_blocker_fail || substantiveness_blocker_fail {
        state.substantiveness_consecutive_no_progress_requests = 0;
        if let Some(kind) = route_non_adjudicable_unknown_verifier(state) {
            state.stage = match kind {
                RequestKind::Paper => Stage::VerifyPaper,
                RequestKind::Corr => Stage::VerifyCorr,
                RequestKind::Sound => Stage::VerifySound,
                _ => unreachable!("route helper returns only verifier kinds"),
            };
            clear_retry_and_pending_task(state);
            return Ok(vec![issue_request(state, kind)]);
        }
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_review_or_stuck_math_audit(state)]);
    }

    // 2. Drain target frontier first (per-cycle scheduler picks
    //    target-first). Both target and per-node share VerifyPaper.
    if !state.paper_verify_targets().is_empty() {
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 3. Authorize pending deviation files before per-node substantiveness.
    if !state.deviation_verify_ids().is_empty() {
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 4. Per-node frontier safety bound (mirror of theorem-stating drain):
    //    if the verifier has burned `SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS`
    //    consecutive requests without shrinking the Unknown set, escalate
    //    to Reviewer. Otherwise re-fire VerifyPaper.
    let node_frontier = state.substantiveness_verify_nodes();
    if !node_frontier.is_empty() {
        if state.substantiveness_consecutive_no_progress_requests
            >= crate::SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS
        {
            state.substantiveness_consecutive_no_progress_requests = 0;
            clear_retry_and_pending_task(state);
            return Ok(vec![issue_review_or_stuck_math_audit(state)]);
        }
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }

    // 5. All paper frontiers drained. Reset no-progress counter and fall
    //    through to corr → cleanup-or-sound → review.
    state.substantiveness_consecutive_no_progress_requests = 0;

    if !state.corr_verify_nodes().is_empty() {
        state.stage = Stage::VerifyCorr;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Corr)]);
    }
    if let Some(commands) = maybe_issue_protected_reapproval(state) {
        return Ok(commands);
    }
    if state.formalization_complete() {
        // Cleanup-v2 Step 4: enter_cleanup_phase handles the
        // phase + stage flip AND resets the cleanup-v2 audit/task/
        // counter fields so a re-entry doesn't leak prior state.
        let mut commands = enter_cleanup_phase(state);
        state.attempt = 0;
        clear_retry_context(state);
        state.commit_live();
        state.relegalize_active_fields();
        state.clear_pending_task();
        // SyncTabletRootForPaperTargets is emitted first by
        // enter_cleanup_phase so the umbrella rewrite is observable in
        // the same CommitCheckpoint diff.
        commands.push(ProtocolCommand::CommitCheckpoint);
        return Ok(commands);
    }
    if state.sound_verify_nodes().is_empty() {
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_review_or_stuck_math_audit(state)]);
    }
    state.stage = Stage::VerifySound;
    clear_retry_and_pending_task(state);
    Ok(vec![issue_request(state, RequestKind::Sound)])
}

/// Routing helper applied at post-progress sites that would otherwise
/// transition to `Stage::Reviewer`. Preempts the reviewer dispatch with a
/// verifier when any global Unknown blocker is *non-adjudicable* (object
/// not in the corresponding `latest_*_review_*` for its lane).
///
/// Why preempt: a non-adjudicable Unknown has no legitimate action bucket
/// in the reviewer's contract — no verifier evidence to override, not a
/// current Fail to reset — so the reviewer's only meaningful `Continue`
/// move on that blocker is `task→Fail`, which pins
/// `status=Fail+approved_fp=current_fp`. On the next cycle,
/// `theorem_start_request_kind` reads `current==approved` as "verifier
/// ran" and dispatches a Worker burst, starving verifier dispatch
/// indefinitely. This is the K-1 verifier-starvation deadlock, also
/// fixed at the theorem-cleanup-Valid site by `e6320f6` via routing
/// through `apply_theorem_paper_accept`.
///
/// When all Unknowns ARE in their respective `latest_*_review_*`,
/// routing to Reviewer is correct: the verifier weighed in, and the
/// reviewer is overriding/re-adjudicating (`review_blocker_adjudicable`
/// in model.rs:3779 endorses this). The previous K-2 attempt
/// (commit `0d9db6d`, reverted at `ff73a40`) tried to enforce "no
/// Unknown ever reaches Reviewer", which was incompatible with override
/// adjudication; this helper is the corrected, narrower form.
///
/// Lane priority mirrors `theorem_start_request_kind`: paper / target →
/// substantiveness → corr → sound → review. Both paper variants share
/// `Stage::VerifyPaper`; per-cycle scheduling (`request_paper_verify_targets`
/// + `request_substantiveness_verify_nodes`) emits exactly one frontier
/// at a time.
///
/// Caller contract (audit follow-ups #4 and #5 on commit `5539650`):
///
/// Originally this helper had a single caller — `apply_cleanup_worker_response`
/// — and the analysis below was written for that site. It is now also called
/// from theorem and proof Stuck/NeedsRestructure routing and from several
/// Valid no-delta paths, plus a few cleanup/verifier post-processing sites.
/// Each new caller must independently satisfy the two contracts below.
///
/// - `held_target` policy is the caller's responsibility. The helper does
///   NOT call `select_theorem_held_target()` — unlike
///   `apply_theorem_paper_accept` (engine.rs:1052) and
///   `apply_theorem_corr_accept` (engine.rs:1316), which select the held
///   target as part of their own state-mutation contract. The original
///   caller (`apply_cleanup_worker_response`) explicitly sets
///   `held_target = None` because cleanup is a structural reset. The
///   Stuck/NR callers run `restore_committed` + `relegalize_active_fields`
///   (and proof-formalization additionally sets `held_target = None`)
///   before reaching this helper, so they enter with a held_target that
///   is either None or legal-against-committed. This is safe because
///   `select_theorem_held_target()` at `model.rs:2595-2628` uses
///   `held_target` only as a preference among candidates, not as a gate,
///   so it still returns a candidate when one exists. The dispatch is
///   safe because all `latest_*_review_*` are cleared by the caller
///   before this helper runs (cleanup at engine.rs:885-888; Stuck/NR
///   arms invoke the four `clear_latest_*_review_context` helpers), so
///   any branch that fires (paper / substantiveness / corr / sound)
///   dispatches the appropriate verifier — never Reviewer with a
///   non-adjudicable Unknown — and the K-1 task→Fail-everything deadlock
///   cannot recur. Future migration: any new caller invoking this helper
///   from a state where `latest_*_review_*` is NOT empty for the
///   relevant lane must re-verify that the helper's branch behavior
///   produces the desired routing for ITS preconditions.
///
/// - The `SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS` safety bound is
///   NOT enforced here. It lives in `apply_theorem_paper_accept`
///   (engine.rs:1095) and `apply_proof_paper_accept` (engine.rs:1175),
///   which escalate to Reviewer when the per-node substantiveness
///   verifier has burned the budget without shrinking the Unknown set.
///   The original caller (`apply_cleanup_worker_response`) sits on a
///   path where `substantiveness_consecutive_no_progress_requests` is
///   effectively 0 (either reset by the upstream
///   `apply_paper_response` / `apply_*_paper_accept` / `apply_*_review_response`
///   that drove the cleanup-worker dispatch, or moot because the cleanup
///   delta will mutate substantiveness fingerprints — making the next
///   verifier round shrink the frontier and reset the counter again).
///   Stuck/NR callers don't mutate the substantiveness frontier, but
///   they also don't drive the `consecutive_no_progress` counter forward
///   (no substantiveness verifier ran in their path), so the bound is
///   not bypassed — it just isn't refreshed at this site. Future
///   migration: any new caller migrated from `apply_*_paper_accept`
///   MUST first verify that the no-progress bound is either irrelevant
///   on that path or is re-asserted upstream of the helper invocation —
///   otherwise the bound is silently bypassed and the substantiveness
///   verifier could re-fire indefinitely on a stuck per-node frontier.

/// Picks the highest-priority verifier lane (paper / corr / sound) whose
/// frontier holds a non-adjudicable Unknown blocker. Used by the
/// paper-accept Fail-escalation arms: when a Fail blocker would normally
/// preempt to Reviewer, but a fresh Unknown elsewhere has a live verifier
/// frontier, run that verifier first. Without this the reviewer can
/// task→Fail-pin the new fingerprint via `apply_review_blocker_adjudication`
/// without verifier evidence, breaking the
/// "tex/lean change must reopen correspondence until a verifier adjudicates"
/// contract. The regression class: a worker rewrites a node's Lean
/// statement, and an existing Fail blocker preempts the corr verifier
/// that should re-adjudicate the changed fingerprint first.
///
/// Pure over `&ProtocolState`; caller sets `state.stage` and emits the
/// request. Returns only verifier `RequestKind`s (Paper / Corr / Sound).
fn route_non_adjudicable_unknown_verifier(state: &ProtocolState) -> Option<RequestKind> {
    if (state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness)
        && !state.paper_verify_targets().is_empty())
        || (state.has_non_adjudicable_unknown_blocker(BlockerKind::Deviation)
            && !state.deviation_verify_ids().is_empty())
        || (state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness)
            && !state.substantiveness_verify_nodes().is_empty())
    {
        return Some(RequestKind::Paper);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr)
        && !state.corr_verify_nodes().is_empty()
    {
        return Some(RequestKind::Corr);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness)
        && !state.sound_verify_nodes().is_empty()
    {
        return Some(RequestKind::Sound);
    }
    None
}

fn should_dispatch_stuck_math_audit(state: &mut ProtocolState) -> bool {
    state.refresh_stuck_math_audit_latch();
    if !matches!(
        state.phase,
        Phase::ProofFormalization | Phase::TheoremStating
    ) {
        return false;
    }
    // Forced audit after a reset/rewind: the restored state earns a
    // fresh adversarial look before any Reviewer touches it. Consumed
    // here so the dispatch happens exactly once per rewind, and the
    // latch is auto-activated so the audit fires even if its usual
    // triggers don't fit the just-restored state. The force flag is
    // ProofFormalization-only today (set on `apply_last_clean_reset` /
    // `apply_audit_authorized_theorem_stating_node_reset`), but
    // consume it uniformly in case a future caller sets it under
    // TheoremStating.
    let forced = std::mem::take(&mut state.force_stuck_math_audit_after_rewind);
    if forced {
        if !state.stuck_math_audit.active {
            state.activate_stuck_math_audit_latch("forced after reset/rewind".to_string());
        }
        return true;
    }
    if !state.stuck_math_audit.active {
        return false;
    }
    let cycles_since_last = match state.last_stuck_math_audit_dispatched_cycle {
        Some(last) => state.cycle.saturating_sub(last),
        None => return true,
    };
    if state.audit_plan.is_some() {
        cycles_since_last >= stuck_math_audit_reaudit_interval_cycles()
    } else {
        cycles_since_last >= stuck_math_audit_dispatch_cooldown_cycles()
    }
}

fn issue_review_or_stuck_math_audit(state: &mut ProtocolState) -> ProtocolCommand {
    if should_dispatch_stuck_math_audit(state) {
        state.stage = Stage::StuckMathAudit;
        state.last_stuck_math_audit_dispatched_cycle = Some(state.cycle);
        // Debounce the no-Sound-progress gate so a single stagnation
        // streak does not re-fire on subsequent cycles; the latch +
        // dispatch cooldown handle re-firing semantics. Idempotent and
        // safe for non-no-progress triggers (cycles_since_clean etc.).
        state.progress_history.note_dispatched();
        issue_request(state, RequestKind::StuckMathAudit)
    } else {
        state.stage = Stage::Reviewer;
        issue_request(state, RequestKind::Review)
    }
}

fn route_need_input_to_auditor(
    state: &mut ProtocolState,
    response: &ReviewResponse,
    commands: &mut Vec<ProtocolCommand>,
    gate_from_invalid_attempt: bool,
) {
    let reviewer_reason = response.reason.trim().to_string();
    let reviewer_comments = response.comments.trim().to_string();
    let trigger_detail = if !reviewer_reason.is_empty() {
        reviewer_reason.clone()
    } else if !reviewer_comments.is_empty() {
        reviewer_comments.clone()
    } else {
        "reviewer requested need_input without a detailed reason".to_string()
    };
    let trigger_detail: String = trigger_detail.chars().take(240).collect();

    // Mutex preserved against the GlobalRepairAuditor lane: both reuse
    // `Stage::StuckMathAudit`, but their auditor contracts and role
    // fragments differ. In normal flow there is no in-flight
    // `pending_global_repair_request` when the reviewer can next emit a
    // NeedInput (Step B already cleared it). If one IS still set — the
    // only known reachable trace is the retry-exhaust fall-back from a
    // GR-lane audit dispatch — pre-empt it with an auto-decline so the
    // new NeedInput burst is self-contained and the role/contract pair
    // is coherent.
    if let Some(pending) = state.pending_global_repair_request.take() {
        state.latest_global_repair_audit_decline_reason =
            "auto-declined: NeedInput escalation pre-empted in-flight global_repair_request"
                .to_string();
        state.latest_global_repair_audit_decline_cycle = Some(state.cycle);
        state.pending_global_repair_grant = None;
        // Touch `pending` only to satisfy the move; the cleared value is
        // observable via the decline reason / cycle above.
        let _ = pending;
    }

    // A reviewer NeedInput escalation supersedes any ordinary stuck-math
    // latch context. Keep the new audit self-contained so the auditor
    // does not inherit stale proof-product context from an unrelated audit.
    state.stuck_math_audit.active = true;
    state.stuck_math_audit.trigger = format!("reviewer requested NeedInput: {trigger_detail}");
    state.stuck_math_audit.active_since_cycle = state.cycle;
    state.stuck_math_audit.trigger_blockers = state.request_blockers(RequestKind::Review);
    state.stuck_math_audit.last_reviewer_lean_product = None;
    state.stuck_math_audit.need_input_audit = Some(NeedInputAuditContext {
        phase: state.phase,
        active_node: state.active_node.clone(),
        held_target: state.held_target.clone(),
        mode: state.current_mode(),
        reviewer_reason,
        reviewer_comments,
        review_request_id: response.request_id,
        review_cycle: response.cycle,
        gate_from_invalid_attempt,
    });
    state.last_stuck_math_audit_dispatched_cycle = Some(state.cycle);
    state.stuck_math_audit_burst_retry_count = 0;
    state.latest_stuck_math_audit_rejection_reason.clear();
    state.stage = Stage::StuckMathAudit;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    if response.clear_human_input {
        state.human_input_outstanding = false;
    }
    state.pending_task = None;
    clear_latest_verifier_review_contexts(state);
    commands.push(issue_request(state, RequestKind::StuckMathAudit));
}

/// global_repair_mode Step A dispatcher: package the reviewer's
/// `global_repair_request` into `state.pending_global_repair_request`
/// and route to a fresh StuckMathAudit lane (Step B). Mirrors
/// `route_need_input_to_auditor` but uses a distinct context carrier.
fn route_global_repair_request_to_auditor(
    state: &mut ProtocolState,
    response: &ReviewResponse,
    commands: &mut Vec<ProtocolCommand>,
) {
    let gr = response
        .global_repair_request
        .as_ref()
        .expect("caller checked is_some");
    let trigger_detail: String = gr.reason.chars().take(240).collect();
    // Mutex preserved against the NeedInputAuditor lane (symmetric pair
    // of the clear in `route_need_input_to_auditor`). Normal flow leaves
    // `need_input_audit` cleared on Reviewer; this clear fires only on a
    // pathological corner case where a prior NeedInput audit lane wedged
    // a residual context onto a Reviewer-stage configuration. Drop it
    // silently rather than carrying it into the GR auditor's context.
    let _ = state.stuck_math_audit.need_input_audit.take();
    state.stuck_math_audit.active = true;
    state.stuck_math_audit.trigger =
        format!("reviewer requested global_repair audit: {trigger_detail}");
    state.stuck_math_audit.active_since_cycle = state.cycle;
    state.stuck_math_audit.trigger_blockers = state.request_blockers(RequestKind::Review);
    state.stuck_math_audit.last_reviewer_lean_product = None;
    state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
        proposed_extension_nodes: gr.proposed_extension_nodes.clone(),
        reviewer_reason: gr.reason.clone(),
        review_request_id: response.request_id,
        review_cycle: response.cycle,
        dispatched_at_cycle: state.cycle,
    });
    state.last_reviewer_global_repair_request_cycle = Some(state.cycle);
    state.last_stuck_math_audit_dispatched_cycle = Some(state.cycle);
    state.stuck_math_audit_burst_retry_count = 0;
    state.latest_stuck_math_audit_rejection_reason.clear();
    state.stage = Stage::StuckMathAudit;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    state.pending_task = None;
    clear_latest_verifier_review_contexts(state);
    commands.push(issue_request(state, RequestKind::StuckMathAudit));
}

fn route_after_progress(state: &mut ProtocolState) -> Vec<ProtocolCommand> {
    if (state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness)
        && !state.paper_verify_targets().is_empty())
        || (state.has_non_adjudicable_unknown_blocker(BlockerKind::Deviation)
            && !state.deviation_verify_ids().is_empty())
        || (state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness)
            && !state.substantiveness_verify_nodes().is_empty())
    {
        state.stage = Stage::VerifyPaper;
        return vec![issue_request(state, RequestKind::Paper)];
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr)
        && !state.corr_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifyCorr;
        return vec![issue_request(state, RequestKind::Corr)];
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness)
        && !state.sound_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifySound;
        return vec![issue_request(state, RequestKind::Sound)];
    }
    if let Some(commands) = maybe_issue_protected_reapproval(state) {
        return commands;
    }
    vec![issue_review_or_stuck_math_audit(state)]
}

fn apply_corr_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    response: CorrResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::VerifyCorr, "VerifyCorr")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed {
        return Ok(vec![issue_request(state, RequestKind::Corr)]);
    }
    validate_corr_lane_updates(request, &response)?;
    let node_updates = reconcile_corr_node_lane_updates(request, &response);
    apply_corr_updates(
        &mut state.corr_status,
        &mut state.corr_approved_fingerprints,
        &state.live.corr_current_fingerprints,
        node_updates,
    );
    state.latest_corr_review_nodes = request.corr_verify_nodes.clone();
    state.latest_corr_reviewer_evidence = response.reviewer_evidence.clone();
    state.previous_corr_lane_findings = response.reviewer_evidence;
    state.clear_latest_sound_review_context();
    state.clear_latest_deviation_review_context();
    match state.phase {
        Phase::TheoremStating => apply_theorem_corr_accept(state),
        Phase::ProofFormalization => apply_proof_corr_accept(state),
        Phase::Cleanup | Phase::Complete => Err(TransitionError::InvalidPhase {
            expected: Phase::TheoremStating,
            found: state.phase,
        }),
    }
}

fn apply_theorem_corr_accept(
    state: &mut ProtocolState,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    state.held_target = state.select_theorem_held_target();
    state.clear_pending_task();
    // Fail blockers escalate to Reviewer immediately. Verifiers don't
    // move Fail status — only the reviewer can reset / override / re-task.
    // Mirrors the corresponding escalation in `apply_theorem_paper_accept`
    // (paper / substantiveness Fail). Without this step a corr Fail could
    // be delayed behind sound work or a `formalization_complete` check.
    let fail_escalation_kinds = state.current_failed_blockers().iter().any(|b| {
        matches!(
            b.kind,
            BlockerKind::NodeCorr
                | BlockerKind::PaperFaithfulness
                | BlockerKind::Deviation
                | BlockerKind::Substantiveness
        )
    });
    if fail_escalation_kinds {
        if let Some(kind) = route_non_adjudicable_unknown_verifier(state) {
            state.stage = match kind {
                RequestKind::Paper => Stage::VerifyPaper,
                RequestKind::Corr => Stage::VerifyCorr,
                RequestKind::Sound => Stage::VerifySound,
                _ => Stage::Reviewer,
            };
            return Ok(vec![issue_request(state, kind)]);
        }
        state.stage = Stage::Reviewer;
        return Ok(vec![issue_request(state, RequestKind::Review)]);
    }
    // Drain eligible verifier frontiers before falling through to
    // Reviewer. Pre-topological-dispatch this routed straight to Reviewer
    // whenever any corr/paper/substantiveness blocker existed — but under
    // topological dispatch a leaf-passes-then-parent-becomes-eligible
    // transition would surface a dispatch-eligible parent that the
    // Reviewer can't usefully adjudicate. Drain the now-eligible
    // frontiers first.
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness)
        && !state.paper_verify_targets().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Deviation)
        && !state.deviation_verify_ids().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness)
        && !state.substantiveness_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr)
        && !state.corr_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifyCorr;
        return Ok(vec![issue_request(state, RequestKind::Corr)]);
    }
    let has_sound_verification = !state.sound_verify_nodes().is_empty();
    state.stage = if has_sound_verification {
        Stage::VerifySound
    } else {
        Stage::Reviewer
    };
    Ok(vec![issue_request(
        state,
        if state.stage == Stage::VerifySound {
            RequestKind::Sound
        } else {
            RequestKind::Review
        },
    )])
}

fn apply_proof_corr_accept(
    state: &mut ProtocolState,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    prepare_proof_verifier_accept(state);
    // Fail blockers escalate to Reviewer immediately. Verifiers don't
    // move Fail status — only the reviewer can reset / override / re-task.
    // Mirrors the corresponding escalation in `apply_proof_paper_accept`.
    // Without this step a corr Fail could be delayed behind sound work
    // or a `formalization_complete` check.
    let fail_escalation_kinds = state.current_failed_blockers().iter().any(|b| {
        matches!(
            b.kind,
            BlockerKind::NodeCorr
                | BlockerKind::PaperFaithfulness
                | BlockerKind::Deviation
                | BlockerKind::Substantiveness
        )
    });
    if fail_escalation_kinds {
        if let Some(kind) = route_non_adjudicable_unknown_verifier(state) {
            state.stage = match kind {
                RequestKind::Paper => Stage::VerifyPaper,
                RequestKind::Corr => Stage::VerifyCorr,
                RequestKind::Sound => Stage::VerifySound,
                _ => Stage::Reviewer,
            };
            clear_retry_and_pending_task(state);
            return Ok(vec![issue_request(state, kind)]);
        }
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_review_or_stuck_math_audit(state)]);
    }
    // Drain eligible verifier frontiers before falling through to other
    // routing paths. Pre-topological-dispatch this routed straight to
    // Reviewer whenever any corr/paper/substantiveness blocker existed —
    // but under topological dispatch a leaf-passes-then-parent-becomes-
    // eligible transition would surface a dispatch-eligible parent that
    // the Reviewer can't usefully adjudicate.
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness)
        && !state.paper_verify_targets().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Deviation)
        && !state.deviation_verify_ids().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness)
        && !state.substantiveness_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifyPaper;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Paper)]);
    }
    if state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr)
        && !state.corr_verify_nodes().is_empty()
    {
        state.stage = Stage::VerifyCorr;
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_request(state, RequestKind::Corr)]);
    }
    if let Some(commands) = maybe_issue_protected_reapproval(state) {
        return Ok(commands);
    }
    if state.formalization_complete() {
        // Cleanup-v2 Step 4: factor through enter_cleanup_phase helper.
        let mut commands = enter_cleanup_phase(state);
        state.attempt = 0;
        clear_retry_context(state);
        state.commit_live();
        state.relegalize_active_fields();
        state.clear_pending_task();
        commands.push(ProtocolCommand::CommitCheckpoint);
        return Ok(commands);
    }
    if state.sound_verify_nodes().is_empty() {
        clear_retry_and_pending_task(state);
        return Ok(vec![issue_review_or_stuck_math_audit(state)]);
    }
    state.stage = Stage::VerifySound;
    clear_retry_and_pending_task(state);
    Ok(vec![issue_request(state, RequestKind::Sound)])
}

fn apply_sound_response(
    state: &mut ProtocolState,
    request: &WrapperRequest,
    response: SoundResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::VerifySound, "VerifySound")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed {
        return Ok(vec![issue_request(state, RequestKind::Sound)]);
    }
    validate_sound_lane_updates(request, &response)?;
    let updates = reconcile_sound_lane_updates(request, &response);
    apply_sound_updates(
        &mut state.sound_status,
        &mut state.sound_approved_fingerprints,
        &mut state.sound_assessments,
        &state.live.sound_current_fingerprints,
        &state.live.sound_current_fingerprint_parts,
        &response.lane_updates,
        updates,
    );
    for node in &request.sound_verify_nodes {
        state.reviewer_requested_sound_verifier_nodes.remove(node);
    }
    // Union (not replace) so the reviewer can adjudicate every node touched
    // by the drain run that precedes this reviewer cycle. Per-node Sound
    // dispatches across one cycle accumulate into a single
    // `latest_sound_review_nodes` set; it is cleared at the next worker
    // dispatch via `clear_latest_sound_review_context`.
    state
        .latest_sound_review_nodes
        .extend(request.sound_verify_nodes.iter().cloned());
    // Audit Finding 3: Sound has a proof-phase drain loop (below) that
    // self-loops VerifySound until every Unknown node is verified, so a
    // single reviewer cycle accumulates evidence across MULTIPLE nodes.
    // Storing reviewer_evidence keyed by lane (overwrite) lost prior
    // nodes' evidence on each loop iteration; that left the reviewer
    // authorized (via `latest_sound_review_nodes`) to override→Pass
    // a node whose evidence was no longer in the request — a soundness
    // risk (false-Pass). Group by node so each node's per-lane evidence
    // is preserved across the drain loop.
    let mut by_node: BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>> =
        BTreeMap::new();
    for (lane, evidence) in &response.reviewer_evidence {
        if !evidence.node.is_empty() {
            by_node
                .entry(evidence.node.clone())
                .or_default()
                .insert(lane.clone(), evidence.clone());
        }
    }
    for (node, lane_evidence) in &by_node {
        state
            .latest_sound_reviewer_evidence
            .entry(node.clone())
            .or_default()
            .extend(lane_evidence.clone());
    }
    // `previous_sound_lane_findings` mirrors the most recent Sound
    // response (not the cycle accumulator), preserving the existing
    // semantics: the next Sound prompt's "revisit_target" fragment is
    // gated on whether THIS response had prior findings, not on what
    // accumulated earlier in the drain.
    state.previous_sound_lane_findings = by_node;
    match state.phase {
        Phase::TheoremStating => {
            let reviewed_nodes_passed = !request.sound_verify_nodes.is_empty()
                && request
                    .sound_verify_nodes
                    .iter()
                    .all(|node| state.current_sound_pass(node));
            state.held_target = state.select_theorem_held_target();
            state.clear_pending_task();
            if reviewed_nodes_passed && !state.sound_verify_nodes().is_empty() {
                state.stage = Stage::VerifySound;
                Ok(vec![issue_request(state, RequestKind::Sound)])
            } else {
                state.stage = Stage::Reviewer;
                Ok(vec![issue_request(state, RequestKind::Review)])
            }
        }
        Phase::ProofFormalization => {
            prepare_proof_verifier_accept(state);
            if let Some(commands) = maybe_issue_protected_reapproval(state) {
                return Ok(commands);
            }
            if state.formalization_complete() && state.global_blockers().is_empty() {
                // Cleanup-v2 Step 4: factor through enter_cleanup_phase helper.
                let mut commands = enter_cleanup_phase(state);
                state.attempt = 0;
                clear_retry_context(state);
                state.commit_live();
                state.relegalize_active_fields();
                state.clear_pending_task();
                commands.push(ProtocolCommand::CommitCheckpoint);
                Ok(commands)
            } else if !state.sound_verify_nodes().is_empty() {
                // Drain remaining Unknown sound nodes before handing to the
                // reviewer. Per-dispatch cardinality is still 1 (enforced by
                // `verification_normalization`); we simply self-loop the
                // Sound stage until every Unknown node has a verdict, so the
                // reviewer sees a stable, fully-verified sound state and can
                // adjudicate every Sound blocker that surfaces. Mirrors the
                // post-Worker routing at lines 794-803 which already drains
                // sound before handing off to the reviewer; without this the
                // post-Sound path was leaking unverified blockers into the
                // reviewer's view, leading to bogus task_blockers.
                state.stage = Stage::VerifySound;
                clear_retry_and_pending_task(state);
                Ok(vec![issue_request(state, RequestKind::Sound)])
            } else {
                clear_retry_and_pending_task(state);
                Ok(vec![issue_review_or_stuck_math_audit(state)])
            }
        }
        Phase::Cleanup | Phase::Complete => Err(TransitionError::InvalidPhase {
            expected: Phase::TheoremStating,
            found: state.phase,
        }),
    }
}

fn prepare_proof_verifier_accept(state: &mut ProtocolState) {
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    // A verifier pass can clear the last blocker that made a closed proof
    // node legal as the active focus. Keep the invariant strict by dropping
    // stale active nodes before routing to the next verifier/reviewer.
    state.relegalize_active_fields();
}

fn protected_semantic_change_confirmation_for(
    response: &ReviewResponse,
) -> ProtectedSemanticChangeConfirmation {
    ProtectedSemanticChangeConfirmation {
        nodes: response.protected_semantic_change_nodes.clone(),
        next_active: response.next_active.clone(),
        next_mode: response.next_mode,
        allow_new_obligations: response.allow_new_obligations,
        must_close_active: response.must_close_active,
    }
}

fn maybe_reissue_protected_semantic_scope_confirmation(
    state: &mut ProtocolState,
    response: &ReviewResponse,
) -> Option<Vec<ProtocolCommand>> {
    if response.protected_semantic_change_nodes.is_empty() {
        state.pending_protected_semantic_scope_confirmation = None;
        return None;
    }

    let confirmation = protected_semantic_change_confirmation_for(response);
    let confirmed = response.confirm_protected_semantic_change_scope
        && state.pending_protected_semantic_scope_confirmation.as_ref() == Some(&confirmation);
    if confirmed {
        state.pending_protected_semantic_scope_confirmation = None;
        return None;
    }

    state.pending_protected_semantic_scope_confirmation = Some(confirmation);
    state.stage = Stage::Reviewer;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    Some(vec![issue_request(state, RequestKind::Review)])
}

fn maybe_issue_protected_reapproval(state: &mut ProtocolState) -> Option<Vec<ProtocolCommand>> {
    if state.phase != Phase::ProofFormalization
        || state.pending_protected_reapproval_nodes.is_empty()
        || !state.global_blockers().is_empty()
        || state.human_input_outstanding
        || state.retry_outcome_kind != RetryOutcomeKind::None
    {
        return None;
    }

    if state.orphan_cleanup_needed() {
        schedule_orphan_cleanup(state);
        state.stage = Stage::Worker;
        state.gate_kind = GateKind::None;
        state.gate_from_invalid_attempt = false;
        return Some(vec![issue_request(state, RequestKind::Worker)]);
    }

    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::ProtectedReapproval;
    state.gate_from_invalid_attempt = false;
    state.pending_protected_semantic_scope_confirmation = None;
    state.clear_pending_task();
    clear_retry_context(state);
    state.relegalize_active_fields();
    Some(vec![issue_request(state, RequestKind::HumanGate)])
}

fn freeze_approved_target_snapshot_from_live(state: &mut ProtocolState) {
    state.approved_targets.configured_targets = state.configured_targets.clone();
    state.approved_targets.coverage = state.live.coverage.clone();
    let coverage_union: BTreeSet<NodeId> = state
        .approved_targets
        .coverage
        .values()
        .flatten()
        .cloned()
        .collect();
    state.approved_targets.protected_closure_nodes = state
        .live
        .protected_closure_nodes_per_target
        .values()
        .flatten()
        .filter(|node| node.as_str() != "Preamble" && !coverage_union.contains(*node))
        .cloned()
        .collect();
    state.paper_approved_fingerprints = state.live.paper_current_fingerprints.clone();
}

fn clear_pending_protected_reapproval_after_reset(state: &mut ProtocolState) {
    state.pending_protected_reapproval_nodes.clear();
    state.pending_protected_semantic_scope_confirmation = None;
}

fn apply_review_response(
    state: &mut ProtocolState,
    response: ReviewResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::Reviewer, "Reviewer")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed || !state.review_response_legal(&response) {
        let mut reasons = state.review_response_rejection_reasons(&response);
        if response.status == ResponseStatus::Malformed {
            reasons.insert(0, "review response status was Malformed".into());
        }
        state.latest_review_rejection_reasons = prompt_safe_rejection_reasons(&reasons);
        return Ok(vec![issue_request(state, RequestKind::Review)]);
    }
    state.latest_review_rejection_reasons.clear();
    apply_review_audit_plan_actions(state, &response);
    if response.decision == ReviewDecisionKind::Continue && response.reset == ResetChoice::None {
        queue_reviewer_requested_sound_verifiers(state, &response);
    }
    match state.phase {
        Phase::TheoremStating => apply_theorem_review_response(state, response),
        Phase::ProofFormalization => apply_proof_review_response(state, response),
        Phase::Cleanup => apply_cleanup_review_response(state, response),
        Phase::Complete => Err(TransitionError::InvalidPhase {
            expected: Phase::TheoremStating,
            found: state.phase,
        }),
    }
}

fn apply_review_audit_plan_actions(state: &mut ProtocolState, response: &ReviewResponse) {
    // Apply individual task dismissals first so the reasons are recorded
    // in the plan's task list. If `dismiss_audit_plan` is also set, the
    // plan is then taken into `superseded_audit_plan` carrying those
    // dismissals — preserving the audit trail of what was closed and
    // why, rather than silently dropping the dismissed_tasks reasons.
    if let Some(plan) = state.audit_plan.as_mut() {
        for dismissal in &response.dismissed_tasks {
            if let Some(task) = plan.tasks.iter_mut().find(|task| task.id == dismissal.id) {
                task.dismissed = true;
                task.dismissed_reason = dismissal.reason.trim().to_string();
                task.dismissed_at_cycle = Some(state.cycle);
            }
        }
    }
    if response.dismiss_audit_plan {
        state.superseded_audit_plan = state.audit_plan.take();
        // Whole-plan dismissal: the reviewer has declared this audit
        // round complete. Clear the latch so the next start_cycle
        // re-evaluates the audit triggers from scratch — if no trigger
        // currently calls for an audit (e.g., the Sound-blocker NODE set
        // recently shrank because of a Pass advance), no audit fires.
        // If a trigger still fires, `refresh_stuck_math_audit_latch`
        // re-activates the latch on the next dispatch. Without this
        // clear, the latch persists across dismissal and the cooldown
        // alone gates re-fire (1 cycle when audit_plan is None), so the
        // reviewer's "audit done" signal is undone immediately.
        state.stuck_math_audit.active = false;
        state.stuck_math_audit.trigger.clear();
        state.stuck_math_audit.trigger_blockers.clear();
        state.stuck_math_audit.active_since_cycle = 0;
    }
}

fn queue_reviewer_requested_sound_verifiers(state: &mut ProtocolState, response: &ReviewResponse) {
    for node in &response.request_sound_verifier_nodes {
        if state.sound_verifier_eligible(node)
            && state.current_sound_assessment(node).status
                != crate::model::SoundAssessmentStatus::VerifierPass
        {
            state
                .reviewer_requested_sound_verifier_nodes
                .insert(node.clone());
        }
    }
}

/// Test-only re-export of `apply_review_audit_plan_actions` so the
/// audit-plan engine semantics (dismissals first, then drop) can be
/// exercised from `model.rs::tests` without routing through the full
/// reviewer-response transition.
#[cfg(test)]
pub(crate) fn apply_review_audit_plan_actions_for_test(
    state: &mut ProtocolState,
    response: &ReviewResponse,
) {
    apply_review_audit_plan_actions(state, response);
}

fn apply_theorem_review_response(
    state: &mut ProtocolState,
    response: ReviewResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    // Reviewer-pathway exit resets the substantiveness no-progress counter:
    // the reviewer's adjudication ends the prior drain loop, so the next
    // VerifyPaper sub-loop (if any) starts with a fresh budget. Without
    // this, an override-Pass of the last per-node Fail blocker would
    // carry stale counter state into the next cycle.
    state.substantiveness_consecutive_no_progress_requests = 0;
    if state.retry_outcome_kind != RetryOutcomeKind::None {
        match response.decision {
            ReviewDecisionKind::Continue => {
                state.reviewer_comments = response.comments.trim().to_string();
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                if response.reset == ResetChoice::LastClean {
                    return apply_continue_last_clean_reissue_review(state);
                }
                match response.reset {
                    ResetChoice::None => {}
                    ResetChoice::LastCommit => {
                        state.restore_committed();
                        commands.push(ProtocolCommand::RestoreWorktreeToHead);
                    }
                    ResetChoice::LastClean => {
                        unreachable!("Continue+LastClean handled above via re-issue Review")
                    }
                    ResetChoice::TheoremStatingNode => {
                        unreachable!("TheoremStatingNode reset is proof-formalization only")
                    }
                }
                state.apply_review_blocker_resets(&response.reset_blockers);
                state.apply_review_blocker_adjudications(&response.task_blockers);
                state.apply_difficulty_updates(&response.difficulty_updates);
                if response.next_active.is_some() {
                    state.active_node = response.next_active;
                }
                state.held_target = state.select_theorem_held_target();
                state.target_edit_mode = match state.active_node {
                    Some(_) => match response.next_mode {
                        TaskMode::Global => TargetEditMode::Global,
                        TaskMode::Targeted => TargetEditMode::Targeted,
                        _ => return Err(TransitionError::IllegalReviewerDecision),
                    },
                    None => TargetEditMode::Global,
                };
                state.relegalize_active_fields();
                let global = state.global_blockers();
                let task_blockers = response
                    .task_blockers
                    .into_iter()
                    .filter(|b| global.contains(b) && state.review_task_blocker_forwardable(b))
                    .collect();
                state.pending_task = Some(PendingTask {
                    task_blockers,
                    node: state.active_node.clone(),
                    mode: state.current_mode(),
                    orphan_cleanup_nodes: BTreeSet::new(),
                    protected_semantic_change_nodes: BTreeSet::new(),
                    authorized_nodes: BTreeSet::new(),
                    allow_new_obligations: true,
                    must_close_active: false,
                    next_worker_context_mode: response.next_worker_context_mode,
                    paper_focus_ranges: response.paper_focus_ranges,
                    work_style_hint: response.work_style_hint,
                consumed_global_repair_grant: false,
                });
                let request_kind = state.theorem_start_request_kind();
                state.stage = request_stage(request_kind);
                if request_kind != RequestKind::Worker {
                    state.clear_pending_task();
                }
                clear_retry_context(state);
                state.attempt = 1;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_latest_verifier_review_contexts(state);
                commands.push(issue_request(state, request_kind));
                Ok(commands)
            }
            ReviewDecisionKind::NeedInput => {
                state.reviewer_comments.clear();
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                match response.reset {
                    ResetChoice::None => {}
                    ResetChoice::LastCommit => {
                        state.restore_committed();
                        commands.push(ProtocolCommand::RestoreWorktreeToHead);
                    }
                    ResetChoice::LastClean => {
                        // Patch C-N item 2: route through the helper so
                        // the RestoreWorktreeToLastClean command is
                        // suppressed when the model refuses the reset
                        // (Ok(false), closure mirrors not ready).
                        apply_last_clean_reset_and_emit(state, &mut commands)?;
                    }
                    ResetChoice::TheoremStatingNode => {
                        unreachable!(
                            "TheoremStatingNode reset is proof-formalization Continue-only"
                        )
                    }
                }
                state.apply_review_blocker_resets(&response.reset_blockers);
                state.apply_difficulty_updates(&response.difficulty_updates);
                let gate_from_invalid_attempt =
                    state.retry_outcome_kind == RetryOutcomeKind::Invalid;
                route_need_input_to_auditor(
                    state,
                    &response,
                    &mut commands,
                    gate_from_invalid_attempt,
                );
                Ok(commands)
            }
            _ => Err(TransitionError::IllegalReviewerDecision),
        }
    } else {
        match response.decision {
            ReviewDecisionKind::Continue => {
                state.reviewer_comments = response.comments.trim().to_string();
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                if response.reset == ResetChoice::LastClean {
                    return apply_continue_last_clean_reissue_review(state);
                }
                match response.reset {
                    ResetChoice::None => {}
                    ResetChoice::LastCommit => {
                        state.restore_committed();
                        commands.push(ProtocolCommand::RestoreWorktreeToHead);
                    }
                    ResetChoice::LastClean => {
                        unreachable!("Continue+LastClean handled above via re-issue Review")
                    }
                    ResetChoice::TheoremStatingNode => {
                        unreachable!("TheoremStatingNode reset is proof-formalization only")
                    }
                }
                state.apply_review_blocker_resets(&response.reset_blockers);
                state.apply_review_blocker_adjudications(&response.task_blockers);
                state.apply_difficulty_updates(&response.difficulty_updates);
                state.active_node = response.next_active;
                // (#56-extension follow-up) Parity with proof/cleanup
                // Continue branches: defensive relegalize after the
                // post-reset active_node assignment. Non-retry theorem
                // path is normally protected by the tightened legality
                // check at model.rs (`active_node_legal` against
                // `last_clean_live`); this is belt-and-braces for the
                // migration window where mirrors are empty.
                state.relegalize_active_fields();
                state.held_target = state.select_theorem_held_target();
                state.target_edit_mode = match state.active_node {
                    Some(_) => match response.next_mode {
                        TaskMode::Global => TargetEditMode::Global,
                        TaskMode::Targeted => TargetEditMode::Targeted,
                        _ => {
                            return Err(TransitionError::IllegalReviewerDecision);
                        }
                    },
                    None => TargetEditMode::Global,
                };
                let global = state.global_blockers();
                let task_blockers = response
                    .task_blockers
                    .into_iter()
                    .filter(|blocker| {
                        global.contains(blocker) && state.review_task_blocker_forwardable(blocker)
                    })
                    .collect();
                state.pending_task = Some(PendingTask {
                    task_blockers,
                    node: state.active_node.clone(),
                    mode: state.current_mode(),
                    orphan_cleanup_nodes: BTreeSet::new(),
                    protected_semantic_change_nodes: BTreeSet::new(),
                    authorized_nodes: BTreeSet::new(),
                    allow_new_obligations: true,
                    must_close_active: false,
                    next_worker_context_mode: response.next_worker_context_mode,
                    paper_focus_ranges: response.paper_focus_ranges,
                    work_style_hint: response.work_style_hint,
                consumed_global_repair_grant: false,
                });
                if response.clear_human_input {
                    state.human_input_outstanding = false;
                }
                state.stage = Stage::Start;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.attempt = 0;
                state.commit_live();
                clear_latest_verifier_review_contexts(state);
                commands.push(ProtocolCommand::CommitCheckpoint);
                Ok(commands)
            }
            ReviewDecisionKind::NeedInput => {
                state.reviewer_comments.clear();
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                match response.reset {
                    ResetChoice::None => {}
                    ResetChoice::LastCommit => {
                        state.restore_committed();
                        commands.push(ProtocolCommand::RestoreWorktreeToHead);
                    }
                    ResetChoice::LastClean => {
                        // Patch C-N item 2: route through the helper so
                        // the RestoreWorktreeToLastClean command is
                        // suppressed when the model refuses the reset
                        // (Ok(false), closure mirrors not ready).
                        apply_last_clean_reset_and_emit(state, &mut commands)?;
                    }
                    ResetChoice::TheoremStatingNode => {
                        unreachable!(
                            "TheoremStatingNode reset is proof-formalization Continue-only"
                        )
                    }
                }
                state.apply_review_blocker_resets(&response.reset_blockers);
                state.apply_difficulty_updates(&response.difficulty_updates);
                clear_retry_context(state);
                state.commit_live();
                commands.push(ProtocolCommand::CommitCheckpoint);
                route_need_input_to_auditor(state, &response, &mut commands, false);
                Ok(commands)
            }
            ReviewDecisionKind::AdvancePhase => {
                // Defense-in-depth: AdvancePhase legality requires
                // `blockers.is_empty()` at reviewer-response time (the
                // dispatch-eligible-filtered set surfaced to the reviewer in
                // the prompt), AND this apply-time check requires
                // `global_blockers().is_empty()` (catches the
                // unreachable-under-DAG-acyclicity edge case where deferred
                // blockers exist but the filtered set is empty — e.g. after
                // state load from disk, recovery paths, or future code
                // changes). Without this assertion, the engine would advance
                // phase silently in such a corrupted state.
                if !state.global_blockers().is_empty() {
                    return Err(TransitionError::IllegalReviewerDecision);
                }
                // Phase advance goes directly to the outside-expert HumanGate:
                // the per-phase verifier panels have already pinned status +
                // approvedFp on every node, and AdvancePhase legality requires
                // `blockers.is_empty()` at reviewer-response time, so no
                // redundant combined-panel re-verification is needed. The
                // human operator is the expert gate; they approve advance
                // based on the current state plus whatever off-protocol
                // inspection they perform.
                state.reviewer_comments.clear();
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                match response.reset {
                    ResetChoice::None => {}
                    ResetChoice::LastCommit => {
                        state.restore_committed();
                        commands.push(ProtocolCommand::RestoreWorktreeToHead);
                    }
                    ResetChoice::LastClean => {
                        // Patch C-N item 2: route through the helper so
                        // the RestoreWorktreeToLastClean command is
                        // suppressed when the model refuses the reset
                        // (Ok(false), closure mirrors not ready).
                        apply_last_clean_reset_and_emit(state, &mut commands)?;
                    }
                    ResetChoice::TheoremStatingNode => {
                        unreachable!(
                            "TheoremStatingNode reset is proof-formalization Continue-only"
                        )
                    }
                }
                state.apply_difficulty_updates(&response.difficulty_updates);
                state.stage = Stage::HumanGate;
                state.gate_kind = GateKind::Advance;
                state.gate_from_invalid_attempt = false;
                state.active_node = response.next_active;
                state.held_target = None;
                state.target_edit_mode = TargetEditMode::Global;
                if response.clear_human_input {
                    state.human_input_outstanding = false;
                }
                state.pending_task = None;
                clear_retry_context(state);
                clear_latest_verifier_review_contexts(state);
                state.commit_live();
                commands.push(ProtocolCommand::CommitCheckpoint);
                commands.push(issue_request(state, RequestKind::HumanGate));
                Ok(commands)
            }
            ReviewDecisionKind::Done => Err(TransitionError::IllegalReviewerDecision),
        }
    }
}

fn apply_proof_review_response(
    state: &mut ProtocolState,
    response: ReviewResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    // Routing-review latch: clear before phase-specific logic mutates state.
    // The in-flight Review's `post_advance_routing: true` has already passed
    // the legality check at apply_event entry; from here on, subsequent
    // request issuances must derive `post_advance_routing: false`.
    state.post_advance_routing_pending = false;
    // Reviewer-pathway exit resets the substantiveness no-progress counter,
    // mirroring `apply_theorem_review_response`. The lane is active in
    // ProofFormalization (commit 7774635), so a proof-phase reviewer
    // adjudicating substantiveness blockers and Continuing must clear the
    // counter — otherwise the next per-node Paper drain inherits stale
    // state from before the reviewer cycle.
    state.substantiveness_consecutive_no_progress_requests = 0;
    let retry_review = state.retry_outcome_kind != RetryOutcomeKind::None;
    match response.decision {
        ReviewDecisionKind::Continue => {
            // global_repair_mode Step A short-circuit: a Continue
            // carrying global_repair_request is non-acting (validator
            // already enforced empty action fields) and routes to a
            // fresh StuckMathAudit lane.
            if response.global_repair_request.is_some() {
                let mut commands: Vec<ProtocolCommand> = Vec::new();
                state.reviewer_comments = response.comments.trim().to_string();
                // Clear any prior decline reason — a new request
                // supersedes the surfaced signal.
                state.latest_global_repair_audit_decline_reason.clear();
                state.latest_global_repair_audit_decline_cycle = None;
                route_global_repair_request_to_auditor(state, &response, &mut commands);
                return Ok(commands);
            }
            if let Some(commands) =
                maybe_reissue_protected_semantic_scope_confirmation(state, &response)
            {
                return Ok(commands);
            }
            state.record_stuck_math_audit_review(&response);
            let protected_semantic_change_nodes = response.protected_semantic_change_nodes.clone();
            state.reviewer_comments = response.comments.trim().to_string();
            let mut commands: Vec<ProtocolCommand> = Vec::new();
            if response.reset == ResetChoice::LastClean {
                return apply_continue_last_clean_reissue_review(state);
            }
            if response.reset == ResetChoice::TheoremStatingNode {
                let Some(node) = response.reset_node.clone() else {
                    return Err(TransitionError::IllegalReviewerDecision);
                };
                return apply_continue_theorem_stating_node_reset(state, node);
            }
            match response.reset {
                ResetChoice::None => {}
                ResetChoice::LastCommit => {
                    state.restore_committed();
                    clear_pending_protected_reapproval_after_reset(state);
                    commands.push(ProtocolCommand::RestoreWorktreeToHead);
                }
                ResetChoice::LastClean => {
                    unreachable!("Continue+LastClean handled above via re-issue Review")
                }
                ResetChoice::TheoremStatingNode => {
                    unreachable!("Continue+TheoremStatingNode handled above via runtime reset")
                }
            }
            state.apply_review_blocker_resets(&response.reset_blockers);
            state.apply_review_blocker_adjudications(&response.task_blockers);
            state.apply_difficulty_updates(&response.difficulty_updates);
            if response.next_active.is_some() {
                state.active_node = response.next_active;
            }
            // Proposal v32: apply reviewer-chosen coarse anchor.
            // The validation gate has already rejected the response if
            // `next_active_coarse` is non-None but not a member of
            // `kernel_hinted_next_active_coarse_nodes`, so here we just
            // mutate when set. Anchor change resets the starvation
            // counter; on no-change we update the counter below using
            // the still-current `coarse_repair_mode()` reading.
            let pre_change_anchor = state.active_coarse_node.clone();
            let pre_change_repair_mode = state.coarse_repair_mode();
            if response.next_active_coarse.is_some() {
                state.active_coarse_node = response.next_active_coarse.clone();
            }
            let anchor_changed = state.active_coarse_node != pre_change_anchor;
            // global_repair_mode S11: a Step C burst does not count as
            // anchor progress. Force the counter to increment even when
            // pre_change_repair_mode was false (the grant burst is by
            // construction a wide repair under the same anchor).
            let force_repair_increment = response.consume_global_repair_grant && !anchor_changed;
            state.cycles_in_coarse_repair_mode = if anchor_changed
                || state.active_coarse_node.is_none()
                || state.coarse_dag_nodes.is_empty()
            {
                0
            } else if pre_change_repair_mode || force_repair_increment {
                state.cycles_in_coarse_repair_mode.saturating_add(1)
            } else {
                0
            };
            state.proof_edit_mode = match state.active_node {
                Some(_) => match response.next_mode {
                    TaskMode::Local => ProofEditMode::Local,
                    TaskMode::Restructure => ProofEditMode::Restructure,
                    TaskMode::CoarseRestructure => ProofEditMode::CoarseRestructure,
                    _ => return Err(TransitionError::IllegalReviewerDecision),
                },
                None => ProofEditMode::Local,
            };
            // (#56-extension follow-up) Defensive: if the response chose
            // a `next_active` that was legal pre-reset but vanished post-
            // restore, the legality check at model.rs:3304+ rejects, so
            // we wouldn't reach here. This relegalize is belt-and-braces
            // for the migration window (mirrors empty → legality skips
            // the post-restore check) and any future legality bypass.
            state.relegalize_active_fields();
            state.held_target = None;
            state.target_edit_mode = TargetEditMode::Global;
            state.proof_edit_mode = match state.active_node {
                Some(_) => match response.next_mode {
                    TaskMode::Local => ProofEditMode::Local,
                    TaskMode::Restructure => ProofEditMode::Restructure,
                    TaskMode::CoarseRestructure => ProofEditMode::CoarseRestructure,
                    _ => return Err(TransitionError::IllegalReviewerDecision),
                },
                None => ProofEditMode::Local,
            };
            let global = state.global_blockers();
            state.pending_task = Some(PendingTask {
                task_blockers: response
                    .task_blockers
                    .into_iter()
                    .filter(|b| global.contains(b))
                    .collect(),
                node: state.active_node.clone(),
                mode: state.current_mode(),
                orphan_cleanup_nodes: BTreeSet::new(),
                protected_semantic_change_nodes,
                authorized_nodes: response.authorized_nodes,
                allow_new_obligations: response.allow_new_obligations,
                must_close_active: response.must_close_active,
                next_worker_context_mode: response.next_worker_context_mode,
                paper_focus_ranges: response.paper_focus_ranges,
                work_style_hint: response.work_style_hint,
                consumed_global_repair_grant: response.consume_global_repair_grant,
            });
            if response.clear_human_input {
                state.human_input_outstanding = false;
            }
            if retry_review && state.active_node.is_none() && state.orphan_cleanup_needed() {
                // `schedule_orphan_cleanup` overwrites `pending_task` with its
                // own (empty task_blockers, orphan_cleanup_nodes = set) shape,
                // discarding the reviewer's task_blockers we set above. That's
                // intentional: orphan cleanup is a distinct work stream that
                // shouldn't co-mingle with adjudicated-to-Fail blockers. The
                // adjudication writes (status + approvedFp) already landed, so
                // the blockers persist in `global_blockers()` and will surface
                // again on the next review cycle once cleanup finishes.
                schedule_orphan_cleanup(state);
                state.stage = Stage::Worker;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                state.attempt = 1;
                clear_latest_verifier_review_contexts(state);
                commands.push(issue_request(state, RequestKind::Worker));
                return Ok(commands);
            }
            if retry_review
                && state.active_node.is_none()
                && (!state.paper_verify_targets().is_empty()
                    || !state.substantiveness_verify_nodes().is_empty()
                    || !state.corr_verify_nodes().is_empty()
                    || !state.sound_verify_nodes().is_empty())
            {
                state.clear_pending_task();
                // Audit-fix #6: include substantiveness frontier in proof-
                // phase retry-resume routing. Both paper-target and
                // substantiveness ride on Stage::VerifyPaper / RequestKind::
                // Paper, so they share the first branch; the kernel scheduler
                // picks the right per-cycle scenario downstream.
                state.stage = if !state.paper_verify_targets().is_empty()
                    || !state.substantiveness_verify_nodes().is_empty()
                {
                    Stage::VerifyPaper
                } else if !state.corr_verify_nodes().is_empty() {
                    Stage::VerifyCorr
                } else {
                    Stage::VerifySound
                };
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.attempt = 0;
                clear_latest_verifier_review_contexts(state);
                commands.push(issue_request(
                    state,
                    match state.stage {
                        Stage::VerifyPaper => RequestKind::Paper,
                        Stage::VerifyCorr => RequestKind::Corr,
                        Stage::VerifySound => RequestKind::Sound,
                        _ => unreachable!("proof retry resume should issue a verifier request"),
                    },
                ));
                return Ok(commands);
            }
            state.stage = if retry_review {
                Stage::Worker
            } else {
                Stage::Start
            };
            state.gate_kind = GateKind::None;
            state.gate_from_invalid_attempt = false;
            state.attempt = if retry_review { 1 } else { 0 };
            let checkpoint_review_continue =
                !retry_review && state.pending_protected_reapproval_nodes.is_empty();
            if !retry_review {
                clear_retry_context(state);
                if checkpoint_review_continue {
                    state.commit_live();
                }
            }
            clear_latest_verifier_review_contexts(state);
            if retry_review {
                commands.push(issue_request(state, RequestKind::Worker));
            } else if checkpoint_review_continue {
                commands.push(ProtocolCommand::CommitCheckpoint);
            }
            Ok(commands)
        }
        ReviewDecisionKind::NeedInput => {
            state.record_stuck_math_audit_review(&response);
            state.reviewer_comments.clear();
            let mut commands: Vec<ProtocolCommand> = Vec::new();
            match response.reset {
                ResetChoice::None => {}
                ResetChoice::LastCommit => {
                    state.restore_committed();
                    clear_pending_protected_reapproval_after_reset(state);
                    commands.push(ProtocolCommand::RestoreWorktreeToHead);
                }
                ResetChoice::LastClean => {
                    // Patch C-N item 2: route through helper so the
                    // RestoreWorktreeToLastClean command is suppressed
                    // when the model refuses the reset (Ok(false),
                    // closure mirrors not ready). The pending-protected-
                    // reapproval clear is unconditional (state-only,
                    // idempotent) so it stays outside the helper.
                    apply_last_clean_reset_and_emit(state, &mut commands)?;
                    clear_pending_protected_reapproval_after_reset(state);
                }
                ResetChoice::TheoremStatingNode => {
                    unreachable!("TheoremStatingNode reset is proof-formalization Continue-only")
                }
            }
            state.apply_review_blocker_resets(&response.reset_blockers);
            state.apply_difficulty_updates(&response.difficulty_updates);
            let gate_from_invalid_attempt = state.retry_outcome_kind == RetryOutcomeKind::Invalid;
            state.pending_protected_semantic_scope_confirmation = None;
            let checkpoint_need_input =
                !retry_review && state.pending_protected_reapproval_nodes.is_empty();
            if !retry_review {
                clear_retry_context(state);
                if checkpoint_need_input {
                    state.commit_live();
                }
            }
            if !retry_review && checkpoint_need_input {
                commands.push(ProtocolCommand::CommitCheckpoint);
            }
            route_need_input_to_auditor(state, &response, &mut commands, gate_from_invalid_attempt);
            Ok(commands)
        }
        _ => Err(TransitionError::IllegalReviewerDecision),
    }
}

fn apply_cleanup_review_response(
    state: &mut ProtocolState,
    response: ReviewResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    let retry_review = state.retry_outcome_kind != RetryOutcomeKind::None;
    match response.decision {
        ReviewDecisionKind::Continue => {
            state.reviewer_comments = response.comments.trim().to_string();
            let mut commands: Vec<ProtocolCommand> = Vec::new();
            if response.reset == ResetChoice::LastClean {
                return apply_continue_last_clean_reissue_review(state);
            }
            match response.reset {
                ResetChoice::None => {}
                ResetChoice::LastCommit => {
                    state.restore_committed();
                    commands.push(ProtocolCommand::RestoreWorktreeToHead);
                }
                ResetChoice::LastClean => {
                    unreachable!("Continue+LastClean handled above via re-issue Review")
                }
                ResetChoice::TheoremStatingNode => {
                    unreachable!("TheoremStatingNode reset is proof-formalization only")
                }
            }
            state.apply_review_blocker_resets(&response.reset_blockers);
            state.apply_review_blocker_adjudications(&response.task_blockers);
            state.apply_difficulty_updates(&response.difficulty_updates);
            // Cleanup-v2: the worker's active node is resolved from the
            // dispatched task's `target_node` in sub-case A below. The
            // reviewer's `response.next_active` is required to be empty
            // in Phase::Cleanup (enforced by
            // `cleanup_v2_review_fields_legal`), so we do NOT propagate
            // it into `state.active_node` here.
            // (#56-extension follow-up) Defensive: see corresponding
            // comment in apply_proof_review_response.
            state.relegalize_active_fields();
            state.held_target = None;
            state.target_edit_mode = TargetEditMode::Global;
            state.proof_edit_mode = ProofEditMode::Local;

            // Cleanup-v2 Step 6 + audit Finding 3: apply reviewer's bulk
            // dismissals. Each `(task_index, reason)` transitions the
            // task Pending → Dismissed. Legality (in-range, Pending) is
            // enforced upstream by `review_response_legal`; here we
            // assert the precondition in debug builds so an unexpected
            // bypass surfaces during testing. Release builds skip the
            // bad entry silently — the upstream gate is the load-bearing
            // safety net.
            for (idx, reason) in &response.cleanup_dismiss_tasks {
                let i = *idx as usize;
                debug_assert!(
                    i < state.cleanup_audit_tasks.len(),
                    "cleanup_dismiss_tasks index {} out of range (len={}); review_response_legal should have rejected",
                    idx,
                    state.cleanup_audit_tasks.len()
                );
                if i < state.cleanup_audit_tasks.len() {
                    debug_assert!(
                        matches!(
                            state.cleanup_audit_tasks[i].status,
                            CleanupTaskStatus::Pending
                        ),
                        "cleanup_dismiss_tasks index {} points at a non-Pending task; review_response_legal should have rejected",
                        idx
                    );
                    if matches!(
                        state.cleanup_audit_tasks[i].status,
                        CleanupTaskStatus::Pending
                    ) {
                        state.cleanup_audit_tasks[i].status = CleanupTaskStatus::Dismissed {
                            reason: reason.clone(),
                        };
                    }
                }
            }

            // Cleanup-v2 Step 6: dispatch a worker against the
            // reviewer-selected task, if any. Sets `cleanup_active_task`
            // and seeds the PendingTask's authorized_nodes from the
            // reviewer's `authorized_nodes` (cleanup-v2 reuses the
            // existing field — see plan §1 code-reuse map).
            //
            // When `cleanup_next_task = None`, no worker is dispatched
            // this cycle; we cycle back to the Reviewer below.
            //
            // The kernel does not gate on `cleanup_audit_enabled` /
            // `cleanup_audit_tasks.is_empty()` here. Legacy-mode flows
            // never set `cleanup_next_task`, so they remain in the
            // existing "dispatch a Worker with empty authorized_nodes
            // every Continue" path below — preserving behavior until
            // later steps replace it.
            // Audit Finding 3: legality is enforced by
            // `review_response_legal` at request-acceptance time; the
            // debug_asserts surface unexpected bypass during testing.
            let cleanup_dispatch = response.cleanup_next_task.and_then(|idx| {
                let i = idx as usize;
                debug_assert!(
                    i < state.cleanup_audit_tasks.len(),
                    "cleanup_next_task {} out of range (len={}); review_response_legal should have rejected",
                    idx,
                    state.cleanup_audit_tasks.len()
                );
                if i < state.cleanup_audit_tasks.len() {
                    debug_assert!(
                        matches!(
                            state.cleanup_audit_tasks[i].status,
                            CleanupTaskStatus::Pending
                        ),
                        "cleanup_next_task {} is not Pending; review_response_legal should have rejected",
                        idx
                    );
                    if matches!(
                        state.cleanup_audit_tasks[i].status,
                        CleanupTaskStatus::Pending
                    ) {
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

            let global = state.global_blockers();
            // Cleanup-v2 (audit Finding 4): branch on three sub-cases.
            //
            // Sub-case A: `cleanup_next_task = Some(idx)` — dispatch a
            //   cleanup-v2 Worker against the task. Standard path.
            //
            // Sub-case B (audit Finding 4 fix): `cleanup_next_task = None`
            //   AND we are mid cleanup-v2 loop (`cleanup_audit_burst_count
            //   > 0`, i.e. the audit has already run at least one burst
            //   this round). This includes:
            //     - dismiss-only Continue (cleanup_dismiss_tasks non-empty,
            //       no dispatch) — re-issue Review so the reviewer can
            //       declare Done or pick another task.
            //     - no-op Continue with Pending tasks remaining — same
            //       outcome: re-issue Review.
            //     - Pending tasks exhausted after dismissals — auto-Done
            //       (transition to Phase::Complete). The cleanup-v2 prompt
            //       documents this auto-Done behavior.
            //
            // Sub-case C: legacy lint-only flow (audit-disabled path) —
            //   not reachable today because `start_cycle` always runs an
            //   audit first in Phase::Cleanup, so `cleanup_audit_burst_count
            //   > 0` always holds by the time we're back in Reviewer. The
            //   legacy code is preserved verbatim under the
            //   `cleanup_audit_burst_count == 0` guard for completeness /
            //   future fallback flag, though it cannot fire under the
            //   current `start_cycle` policy.
            let in_cleanup_v2_loop = state.cleanup_audit_burst_count > 0;
            let pending_tasks_remain = state
                .cleanup_audit_tasks
                .iter()
                .any(|t| matches!(t.status, CleanupTaskStatus::Pending));
            let mut auto_done = false;
            if let Some(idx) = cleanup_dispatch {
                // Sub-case A: worker dispatch. Cleanup-v2: pin the
                // worker's active node to the dispatched task's
                // `target_node`. This is the single source of truth —
                // the reviewer no longer nominates a separate
                // `next_active` (rejected by
                // `cleanup_v2_review_fields_legal`).
                let target_node = state.cleanup_audit_tasks[idx as usize].target_node.clone();
                state.active_node = Some(target_node);
                state.cleanup_active_task = Some(idx);
                state.pending_task = Some(PendingTask {
                    task_blockers: BTreeSet::new(),
                    node: state.active_node.clone(),
                    mode: TaskMode::Cleanup,
                    orphan_cleanup_nodes: BTreeSet::new(),
                    protected_semantic_change_nodes: BTreeSet::new(),
                    authorized_nodes: response.authorized_nodes.clone(),
                    allow_new_obligations: true,
                    must_close_active: false,
                    next_worker_context_mode: response.next_worker_context_mode,
                    paper_focus_ranges: response.paper_focus_ranges.clone(),
                    work_style_hint: response.work_style_hint,
                consumed_global_repair_grant: false,
                });
            } else if in_cleanup_v2_loop {
                // Sub-case B: cleanup-v2 dismiss-only / no-op Continue, or
                // tasks-exhausted auto-Done. Clear active_node so the
                // next Review request doesn't surface a stale node from
                // a previously-dispatched task. Cleanup-v2 derives
                // active_node from the dispatched task on each Sub-case A
                // entry, so leaving it cleared here is safe.
                state.pending_task = None;
                state.cleanup_active_task = None;
                state.active_node = None;
                if !pending_tasks_remain {
                    // Auto-Done: no Pending tasks remain after dismissals.
                    // The cleanup invariant guarantees the run is
                    // formalization-complete at every Cleanup boundary
                    // (see engine.rs:1252-1267), so transitioning to
                    // Phase::Complete is safe regardless of which Pending
                    // tasks were dismissed vs Completed vs Failed.
                    auto_done = true;
                }
                // else: fall through to re-issue Review (Stage::Start +
                // cleanup_audit_burst_count > 0 + active_task=None routes
                // to Worker fallback in start_cycle — but pending_task=None
                // means no worker work; we need explicit Reviewer routing).
            } else {
                // Sub-case C: legacy lint-only fallback (unreachable today
                // — `cleanup_audit_burst_count > 0` always holds after the
                // audit ran). Preserved verbatim for legacy-mode
                // compatibility under a future audit-disabled flag.
                state.pending_task = Some(PendingTask {
                    task_blockers: response
                        .task_blockers
                        .into_iter()
                        .filter(|b| global.contains(b))
                        .collect(),
                    node: state.active_node.clone(),
                    mode: TaskMode::Cleanup,
                    orphan_cleanup_nodes: BTreeSet::new(),
                    protected_semantic_change_nodes: BTreeSet::new(),
                    authorized_nodes: BTreeSet::new(),
                    allow_new_obligations: true,
                    must_close_active: false,
                    next_worker_context_mode: response.next_worker_context_mode,
                    paper_focus_ranges: response.paper_focus_ranges,
                    work_style_hint: response.work_style_hint,
                consumed_global_repair_grant: false,
                });
            }
            if response.clear_human_input {
                state.human_input_outstanding = false;
            }
            if auto_done {
                // Audit Finding 4: transition to Phase::Complete on auto-
                // Done (no Pending tasks remain).
                state.phase = Phase::Complete;
                state.stage = Stage::Complete;
                state.active_node = None;
                state.held_target = None;
                state.target_edit_mode = TargetEditMode::Global;
                state.proof_edit_mode = ProofEditMode::Local;
                state.pending_task = None;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.attempt = 0;
                state.commit_live();
                clear_latest_verifier_review_contexts(state);
                commands.push(ProtocolCommand::CommitCheckpoint);
                return Ok(commands);
            }
            // Audit Finding 4: when in cleanup-v2 loop without a dispatch,
            // re-issue Review directly rather than falling through to
            // Worker via start_cycle. start_cycle would otherwise hit the
            // RequestKind::Worker fallback (burst_count > 0, active_task
            // None) and emit an empty-pending-task Worker request, which
            // contradicts the cleanup-v2 design.
            let reissue_review_after_dismiss =
                cleanup_dispatch.is_none() && in_cleanup_v2_loop && !auto_done;
            state.stage = if retry_review {
                Stage::Worker
            } else if reissue_review_after_dismiss {
                Stage::Reviewer
            } else {
                Stage::Start
            };
            state.gate_kind = GateKind::None;
            state.gate_from_invalid_attempt = false;
            state.attempt = if retry_review { 1 } else { 0 };
            if !retry_review {
                clear_retry_context(state);
                state.commit_live();
            }
            clear_latest_verifier_review_contexts(state);
            if retry_review {
                commands.push(issue_request(state, RequestKind::Worker));
            } else if reissue_review_after_dismiss {
                // Audit Finding 4: dismiss-only / no-op Continue must
                // re-issue Review without a CommitCheckpoint here — the
                // commit happens at the *next* Reviewer accept boundary.
                // Issue the Review request immediately so the kernel
                // doesn't fall back to start_cycle's Worker path.
                commands.push(ProtocolCommand::CommitCheckpoint);
                commands.push(issue_request(state, RequestKind::Review));
            } else {
                commands.push(ProtocolCommand::CommitCheckpoint);
            }
            Ok(commands)
        }
        ReviewDecisionKind::NeedInput => {
            state.reviewer_comments.clear();
            let mut commands: Vec<ProtocolCommand> = Vec::new();
            match response.reset {
                ResetChoice::None => {}
                ResetChoice::LastCommit => {
                    state.restore_committed();
                    commands.push(ProtocolCommand::RestoreWorktreeToHead);
                }
                ResetChoice::LastClean => {
                    // Patch C-N item 2: route through helper so the
                    // RestoreWorktreeToLastClean command is suppressed
                    // when the model refuses the reset (Ok(false)).
                    apply_last_clean_reset_and_emit(state, &mut commands)?;
                }
                ResetChoice::TheoremStatingNode => {
                    unreachable!("TheoremStatingNode reset is proof-formalization Continue-only")
                }
            }
            state.apply_review_blocker_resets(&response.reset_blockers);
            state.apply_difficulty_updates(&response.difficulty_updates);
            let gate_from_invalid_attempt = state.retry_outcome_kind == RetryOutcomeKind::Invalid;
            if !retry_review {
                clear_retry_context(state);
                state.commit_live();
            }
            if !retry_review {
                commands.push(ProtocolCommand::CommitCheckpoint);
            }
            route_need_input_to_auditor(state, &response, &mut commands, gate_from_invalid_attempt);
            Ok(commands)
        }
        ReviewDecisionKind::Done => {
            state.reviewer_comments.clear();
            let mut commands: Vec<ProtocolCommand> = Vec::new();
            match response.reset {
                ResetChoice::None => {}
                ResetChoice::LastCommit => {
                    state.restore_committed();
                    commands.push(ProtocolCommand::RestoreWorktreeToHead);
                }
                // Intentionally reject: `Done` declares formalization
                // complete; `LastClean` rewinds to an earlier clean
                // checkpoint. The combination is semantically incoherent
                // (if you're rewinding, you're not done). The reviewer
                // should either pick LastClean with a different decision
                // and re-complete on a later cycle, or drop the reset.
                ResetChoice::LastClean => {
                    return Err(TransitionError::IllegalReviewerDecision);
                }
                ResetChoice::TheoremStatingNode => {
                    return Err(TransitionError::IllegalReviewerDecision);
                }
            }
            state.apply_difficulty_updates(&response.difficulty_updates);

            // Cleanup-v2 Step 14: re-audit branch. If the reviewer
            // requested a re-audit AND we have rounds left AND the
            // force-Done latch isn't set, transition back to the audit
            // sub-phase: bump round, reset scratchpad + burst counter,
            // clear cleanup_active_task, but PRESERVE the task list
            // verbatim (terminal-status tasks survive round 2; Pending
            // tasks remain Pending for round 2 to revise). Round 2
            // dispatches a fresh Audit request via start_cycle's
            // first-burst arm.
            //
            // Note: the design also says force-Done overrides
            // re-audit requests. force-Done is latched when consecutive
            // cleanup workers fail at threshold; in that case Phase::
            // Complete is mandatory regardless of `cleanup_request_reaudit`.
            if response.cleanup_request_reaudit
                && state.cleanup_audit_round < CLEANUP_AUDIT_MAX_ROUNDS
                && !state.cleanup_force_done
            {
                state.cleanup_audit_round = state.cleanup_audit_round.saturating_add(1);
                state.cleanup_audit_burst_count = 0;
                state.cleanup_audit_scratchpad.clear();
                state.cleanup_active_task = None;
                state.audit_burst_retry_count = 0;
                state.latest_audit_rejection_reason.clear();
                // Stage::Start drives the next cycle's start_cycle
                // dispatch — which now sees Phase::Cleanup + burst_count
                // == 0 and emits an Audit request (Step 11). Terminal-
                // status tasks (Completed/Failed/Dismissed) survive
                // verbatim; Pending tasks remain Pending (the round-2
                // audit may revise them via task_modifications).
                state.stage = Stage::Start;
                state.attempt = 0;
                state.pending_task = None;
                if response.clear_human_input {
                    state.human_input_outstanding = false;
                }
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.commit_live();
                clear_latest_verifier_review_contexts(state);
                commands.push(ProtocolCommand::CommitCheckpoint);
                return Ok(commands);
            }

            state.phase = Phase::Complete;
            state.stage = Stage::Complete;
            state.active_node = None;
            state.held_target = None;
            state.target_edit_mode = TargetEditMode::Global;
            state.proof_edit_mode = ProofEditMode::Local;
            state.pending_task = None;
            if response.clear_human_input {
                state.human_input_outstanding = false;
            }
            state.gate_kind = GateKind::None;
            state.gate_from_invalid_attempt = false;
            clear_retry_context(state);
            state.attempt = 0;
            state.commit_live();
            clear_latest_verifier_review_contexts(state);
            commands.push(ProtocolCommand::CommitCheckpoint);
            Ok(commands)
        }
        ReviewDecisionKind::AdvancePhase => Err(TransitionError::IllegalReviewerDecision),
    }
}

fn apply_human_gate_response(
    state: &mut ProtocolState,
    response: HumanGateResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::HumanGate, "HumanGate")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed {
        return Ok(vec![issue_request(state, RequestKind::HumanGate)]);
    }
    match state.gate_kind {
        GateKind::Advance => match response.choice {
            HumanChoice::Approve => {
                state.phase = Phase::ProofFormalization;
                state.stage = Stage::Start;
                // Post-advance routing: next `start_cycle` must issue a
                // routing Review (not a Worker) so the reviewer can pick
                // `next_active`, `must_close_active`, `allow_new_obligations`,
                // `authorized_nodes`, `paper_focus_ranges`, and
                // `next_worker_context_mode` for the first burst of the new
                // phase. The flag is cleared in `start_cycle` once the Review
                // is issued. Without this, `start_cycle` would auto-pick
                // active_node via `select_initial_proof_active_node` and
                // dispatch a worker with kernel-default permissive flags
                // (`allow_new_obligations=true, must_close_active=false`),
                // which is a combination this project explicitly discourages.
                state.post_advance_routing_pending = true;
                freeze_approved_target_snapshot_from_live(state);
                // Snapshot the coarse DAG: the set of nodes present at the
                // theorem-stating → proof-formalization transition. Drives
                // the Restructure vs CoarseRestructure distinction for
                // active-node signature edits; helpers added later aren't
                // in this set and can have signatures revised under plain
                // `restructure`.
                state.coarse_dag_nodes = state.live.present_nodes.clone();
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.held_target = None;
                state.target_edit_mode = TargetEditMode::Global;
                state.proof_edit_mode = ProofEditMode::Local;
                // Do NOT pre-seed `active_node` / `active_coarse_node`
                // here. `post_advance_routing_pending` (set above) makes
                // the next `start_cycle` issue a routing Review whose
                // entire purpose is to choose `next_active` (and
                // `next_active_coarse`) for the first burst of
                // ProofFormalization. Pre-seeding the coarse anchor
                // would narrow `kernel_hinted_next_active_nodes` to the
                // anchor's cone (via `coarse_legal_active_set` in
                // `request_kernel_hinted_next_active_nodes`'s
                // ProofFormalization arm) AND leave
                // `kernel_hinted_next_active_coarse_nodes` empty (anchor
                // locked under shallow-closure), defeating the routing
                // Review. Leaving both `None` is the legitimate state
                // documented in `30b_coarse_anchor.md`: the kernel
                // surfaces every open coarse-DAG node as a candidate in
                // `kernel_hinted_next_active_coarse_nodes`, and the
                // reviewer's Continue is required to set
                // `next_active_coarse` (validated in
                // `validate_review_response_against_request`).
                state.active_node = None;
                state.active_coarse_node = None;
                state.cycles_in_coarse_repair_mode = 0;
                state.pending_task = None;
                state.human_input_outstanding = false;
                // Audit Finding 5: phase advance freezes approved_targets,
                // paper_approved_fingerprints, coarse_dag_nodes, active_node
                // — a major semantic boundary. The runtime persists the
                // protocol_state.json regardless, but without a
                // CommitCheckpoint the external git/audit boundary is
                // missing this transition. Emit one so the checkpoint hook
                // creates a git commit (and a clean tag, since
                // global_blockers is empty by AdvancePhase legality
                // requirement) marking the phase boundary in operator-
                // visible history.
                //
                // No `commit_live()` call needed — none of the
                // human-approval mutations are in `committed_*` mirrors
                // (phase, approved_targets, paper_approved_fingerprints,
                // coarse_dag_nodes, active_node are all top-level
                // ProtocolState fields, not WorkingSnapshot fields). The
                // committed mirror was already snapshotted at the prior
                // Reviewer's AdvancePhase decision (engine.rs's
                // ReviewAdvancePhase branch's commit_live), so a future
                // LastCommit reset would land on the same content as
                // before and would not undo the phase advance (phase is
                // not in committed_*).
                Ok(vec![ProtocolCommand::CommitCheckpoint])
            }
            HumanChoice::Feedback => {
                state.phase = Phase::TheoremStating;
                state.stage = Stage::Reviewer;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.pending_task = None;
                state.human_input_outstanding = true;
                // Re-entering TheoremStating: clear the
                // progress-history buffer so a stale near-window
                // streak carried from before phase advance does not
                // fire the no-Sound-progress StuckMathAudit trigger
                // on the immediate next checkpoint. Mirrors the
                // rewind reset semantics.
                state.reset_progress_history();
                Ok(vec![issue_request(state, RequestKind::Review)])
            }
        },
        GateKind::ProtectedReapproval => match response.choice {
            HumanChoice::Approve => {
                freeze_approved_target_snapshot_from_live(state);
                state.pending_protected_reapproval_nodes.clear();
                state.pending_protected_semantic_scope_confirmation = None;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.held_target = None;
                state.target_edit_mode = TargetEditMode::Global;
                state.proof_edit_mode = ProofEditMode::Local;
                state.relegalize_active_fields();
                state.pending_task = None;
                state.human_input_outstanding = false;
                state.attempt = 0;
                let mut commands = if state.formalization_complete()
                    && state.global_blockers().is_empty()
                {
                    // Cleanup-v2 Step 4: enter_cleanup_phase sets
                    // phase + stage AND resets cleanup-v2 audit fields.
                    enter_cleanup_phase(state)
                } else {
                    state.phase = Phase::ProofFormalization;
                    state.stage = Stage::Start;
                    Vec::new()
                };
                state.commit_live();
                commands.push(ProtocolCommand::CommitCheckpoint);
                Ok(commands)
            }
            HumanChoice::Feedback => {
                state.phase = Phase::ProofFormalization;
                state.stage = Stage::Reviewer;
                state.gate_kind = GateKind::None;
                state.gate_from_invalid_attempt = false;
                clear_retry_context(state);
                state.pending_task = None;
                state.pending_protected_semantic_scope_confirmation = None;
                state.human_input_outstanding = true;
                Ok(vec![issue_request(state, RequestKind::Review)])
            }
        },
        GateKind::NeedInput => {
            state.stage = Stage::Reviewer;
            state.invalid_attempt = state.gate_from_invalid_attempt;
            state.gate_kind = GateKind::None;
            state.gate_from_invalid_attempt = false;
            state.pending_task = None;
            state.pending_protected_semantic_scope_confirmation = None;
            state.human_input_outstanding = true;
            Ok(vec![issue_request(state, RequestKind::Review)])
        }
        GateKind::None => Err(TransitionError::IllegalResponse(
            "human gate response received with no active human gate".into(),
        )),
    }
}

/// Cleanup-v2 Step 13 (2026-05-14): apply an `AuditResponse` envelope.
///
/// Three explicit sub-cases per
/// `CLAUDES_NOTES_cleanup_v2_impl_plan.md`:
///
/// 1. **Malformed retry budget exhausted** (`status == Malformed`):
///    one retry per burst slot. On the first malformed in the slot,
///    re-issue an Audit request and bump `audit_burst_retry_count`.
///    On the second consecutive malformed, force `outcome = AuditDone`
///    (route to Reviewer) with whatever has been validly accumulated.
///    No task-list / scratchpad mutation occurs on a malformed burst.
///
/// 2. **Validation-failed one-retry** (`status == Ok` but some
///    `new_tasks[i]` fails `legal_cleanup_task`, or `task_modifications`
///    references an out-of-range / non-Pending / out-of-round task):
///    set `latest_audit_rejection_reason` to a summary of the first
///    failure, bump `audit_burst_retry_count`, and re-issue Audit.
///    On second consecutive validation failure: force AuditDone.
///
/// 3. **Valid append**: append `response.new_tasks` (each mapped to a
///    Pending `CleanupAuditTask` with `audit_origin_round =
///    state.cleanup_audit_round`); apply each `task_modifications`
///    entry (Pending → Dismissed, current-round only); replace the
///    scratchpad; increment `cleanup_audit_burst_count`; reset
///    `audit_burst_retry_count`. Then route on `outcome`:
///    - `NeedToContinue && burst_count < CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND`:
///      re-issue Audit.
///    - `AuditDone` OR burst cap: route to Reviewer.
fn apply_audit_response(
    state: &mut ProtocolState,
    response: AuditResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::CleanupAudit, "CleanupAudit")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }

    // --- sub-case 1: Malformed ---
    if response.status == ResponseStatus::Malformed {
        if state.audit_burst_retry_count >= 1 {
            // Second consecutive malformed for this slot: force AuditDone.
            //
            // Cleanup-v2 (audit Finding 3): bump `cleanup_audit_burst_count`
            // before transitioning to the reviewer. The reviewer Continue
            // branch at `engine.rs:3791` uses `cleanup_audit_burst_count > 0`
            // as the discriminator between cleanup-v2 mode and the legacy
            // lint-only fallback path; if we force AuditDone on the very
            // first burst slot (no prior bursts ever incremented the
            // counter), a reviewer Continue after the forced transition
            // would mis-route into the legacy fallback, contradicting the
            // cleanup-v2 design. The increment marks "we did run the audit
            // lane this round, even if it produced no valid tasks."
            //
            // `cleanup_audit_scratchpad` is intentionally NOT cleared here:
            // a same-round prior Valid burst's accumulated auditor reasoning
            // is meaningful context for the reviewer. Only a round bump
            // (via `apply_cleanup_review_response` re-audit) clears it.
            state.cleanup_audit_burst_count = state.cleanup_audit_burst_count.saturating_add(1);
            state.audit_burst_retry_count = 0;
            state.latest_audit_rejection_reason =
                "malformed audit response (retry budget exhausted) — forcing AuditDone".to_string();
            return Ok(transition_audit_to_reviewer(state));
        }
        state.audit_burst_retry_count = state.audit_burst_retry_count.saturating_add(1);
        state.latest_audit_rejection_reason = "malformed audit response — retry once".to_string();
        return Ok(vec![issue_request(state, RequestKind::Audit)]);
    }

    // --- sub-case 2: Validation pass ---
    // Validate every new_tasks entry against legal_cleanup_task. Build
    // up a snapshot list of validated tasks; on first failure, retry
    // (or escalate to AuditDone on second consecutive failure).
    //
    // Cleanup-v2 (audit Finding 9): `legal_cleanup_task` only checks
    // duplicates against pre-existing `cleanup_audit_tasks` — it does
    // not see prior entries inside the SAME `new_tasks` list. Track an
    // intra-response "seen" set so the audit can't sneak in two
    // identical entries (same `target_node`, same `kind`) within one
    // burst.
    let mut validation_failure: Option<String> = None;
    let mut seen_this_burst: Vec<(NodeId, CleanupTaskKind)> = Vec::new();
    for (i, new_task) in response.new_tasks.iter().enumerate() {
        if let Err(err) = state.legal_cleanup_task(new_task) {
            validation_failure = Some(format!("new_tasks[{i}]: {err}"));
            break;
        }
        let key = (new_task.target_node.clone(), new_task.kind.clone());
        if seen_this_burst.contains(&key) {
            validation_failure = Some(format!(
                "new_tasks[{i}]: duplicate (target_node, kind) pair within the same audit \
                 burst — target_node {:?} already proposed in this response with the same kind",
                new_task.target_node
            ));
            break;
        }
        seen_this_burst.push(key);
    }
    if validation_failure.is_none() {
        for (i, m) in response.task_modifications.iter().enumerate() {
            let idx = m.task_index as usize;
            if idx >= state.cleanup_audit_tasks.len() {
                validation_failure = Some(format!(
                    "task_modifications[{i}]: task_index {} out of range (len={})",
                    m.task_index,
                    state.cleanup_audit_tasks.len()
                ));
                break;
            }
            let existing = &state.cleanup_audit_tasks[idx];
            if !matches!(existing.status, CleanupTaskStatus::Pending) {
                validation_failure = Some(format!(
                    "task_modifications[{i}]: task_index {} is not Pending (audit may only revise its own Pending proposals)",
                    m.task_index
                ));
                break;
            }
            // Cleanup-v2 (audit Finding 4): round-2 audit IS permitted to
            // revise round-1 leftover Pending tasks. The Pending-only
            // check above is sufficient — terminal-status tasks
            // (Completed/Failed/Dismissed) are still immutable regardless
            // of origin round, but a leftover round-1 Pending task that
            // round-2 has new information about should be dismissable
            // (matches `audit/05_loop_semantics.md:34` which promises this).
            // Dropping the prior `audit_origin_round != current_round`
            // gate aligns the kernel with the audit-prompt's documented
            // semantics.
        }
    }
    if let Some(reason) = validation_failure {
        if state.audit_burst_retry_count >= 1 {
            // Cleanup-v2 (audit Finding 3): same rationale as the malformed
            // exhausted path above — bump `cleanup_audit_burst_count` so
            // the reviewer Continue branch's cleanup-v2-vs-legacy
            // discriminator (`cleanup_audit_burst_count > 0`) correctly
            // routes through cleanup-v2 sub-cases even when the very first
            // audit burst slot failed validation twice in a row.
            //
            // `cleanup_audit_scratchpad` is intentionally NOT cleared here
            // (see the malformed-exhausted arm above for rationale).
            state.cleanup_audit_burst_count = state.cleanup_audit_burst_count.saturating_add(1);
            state.audit_burst_retry_count = 0;
            state.latest_audit_rejection_reason =
                format!("audit validation failed twice in a row — forcing AuditDone: {reason}");
            return Ok(transition_audit_to_reviewer(state));
        }
        state.audit_burst_retry_count = state.audit_burst_retry_count.saturating_add(1);
        state.latest_audit_rejection_reason = reason;
        return Ok(vec![issue_request(state, RequestKind::Audit)]);
    }

    // --- sub-case 3: Valid append ---
    let round = state.cleanup_audit_round;
    for new_task in &response.new_tasks {
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: new_task.target_node.clone(),
            rationale: new_task.rationale.clone(),
            confidence: new_task.confidence,
            kind: new_task.kind.clone(),
            status: CleanupTaskStatus::Pending,
            audit_origin_round: round,
        });
    }
    for m in &response.task_modifications {
        let idx = m.task_index as usize;
        if idx < state.cleanup_audit_tasks.len() {
            state.cleanup_audit_tasks[idx].status = CleanupTaskStatus::Dismissed {
                reason: m.reason.clone(),
            };
        }
    }
    state.cleanup_audit_scratchpad = response.scratchpad_replace.clone();
    state.cleanup_audit_burst_count = state.cleanup_audit_burst_count.saturating_add(1);
    state.audit_burst_retry_count = 0;
    state.latest_audit_rejection_reason.clear();

    // Route on outcome.
    let continue_audit = matches!(response.outcome, AuditOutcome::NeedToContinue)
        && state.cleanup_audit_burst_count < CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND;
    if continue_audit {
        Ok(vec![issue_request(state, RequestKind::Audit)])
    } else {
        Ok(transition_audit_to_reviewer(state))
    }
}

fn stuck_math_audit_validation_failure(
    state: &ProtocolState,
    response: &StuckMathAuditResponse,
) -> Option<String> {
    let report_len = response.report.trim().chars().count();
    if report_len < AUDIT_REPORT_TEXT_MIN_CHARS {
        return Some(format!(
            "report must contain at least {AUDIT_REPORT_TEXT_MIN_CHARS} non-whitespace characters"
        ));
    }
    if report_len > AUDIT_REPORT_TEXT_MAX_CHARS {
        return Some(format!(
            "report must contain at most {AUDIT_REPORT_TEXT_MAX_CHARS} characters"
        ));
    }
    let is_need_input_audit = state.stuck_math_audit.need_input_audit.is_some();
    if !is_need_input_audit && response.confirm_need_input {
        return Some("confirm_need_input is only legal for NeedInputAuditor responses".to_string());
    }
    if is_need_input_audit && !response.confirm_need_input && response.tasks.is_empty() {
        return Some(
            "NeedInputAuditor responses that do not confirm need_input must include at least one recovery task"
                .to_string(),
        );
    }
    if is_need_input_audit && response.confirm_need_input && response.cone_clean_node.is_some() {
        return Some(
            "NeedInputAuditor responses that confirm need_input must not request cone_clean_node"
                .to_string(),
        );
    }
    let mut ids = BTreeSet::new();
    for (i, task) in response.tasks.iter().enumerate() {
        if task.id.trim().is_empty() {
            return Some(format!("tasks[{i}].id must be non-empty"));
        }
        if !ids.insert(task.id.clone()) {
            return Some(format!("tasks[{i}].id duplicates an earlier task id"));
        }
        if task.title.trim().is_empty() {
            return Some(format!("tasks[{i}].title must be non-empty"));
        }
        if task.title.chars().count() > AUDIT_TASK_TITLE_MAX_CHARS {
            return Some(format!(
                "tasks[{i}].title exceeds {AUDIT_TASK_TITLE_MAX_CHARS} characters"
            ));
        }
        if task.body.trim().is_empty() {
            return Some(format!("tasks[{i}].body must be non-empty"));
        }
        if task.body.chars().count() > AUDIT_TASK_BODY_MAX_CHARS {
            return Some(format!(
                "tasks[{i}].body exceeds {AUDIT_TASK_BODY_MAX_CHARS} characters"
            ));
        }
        if task.dismissed
            || !task.dismissed_reason.trim().is_empty()
            || task.dismissed_at_cycle.is_some()
        {
            return Some(format!(
                "tasks[{i}] must not pre-populate reviewer dismissal fields"
            ));
        }
    }
    if let Some(node) = response.cone_clean_node.as_ref() {
        let resettable = state.resettable_theorem_stating_nodes();
        if !resettable.contains(node) {
            return Some(format!(
                "cone_clean_node `{}` must be one of the resettable coarse nodes: {:?}",
                node.as_str(),
                resettable
            ));
        }
    }
    // global_repair_mode Step B legality: S5 structural cap + protected
    // disjointness + presence of a pending request.
    let has_pending = state.pending_global_repair_request.is_some();
    if response.global_repair_approve && !has_pending {
        return Some(
            "global_repair_approve=true requires a pending global_repair_request".to_string(),
        );
    }
    if !response.global_repair_approved_extension_node_ids.is_empty() && !has_pending {
        return Some(
            "global_repair_approved_extension_node_ids requires a pending global_repair_request"
                .to_string(),
        );
    }
    if has_pending && response.global_repair_approve {
        let pending = state
            .pending_global_repair_request
            .as_ref()
            .expect("checked has_pending");
        let mut allowed: BTreeSet<NodeId> = BTreeSet::new();
        for seed in &pending.proposed_extension_nodes {
            allowed.extend(state.impact_region(seed, &state.live));
        }
        let approved: BTreeSet<NodeId> = response
            .global_repair_approved_extension_node_ids
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(NodeId::from)
            .collect();
        if approved.is_empty() {
            return Some(
                "global_repair_approve=true requires non-empty global_repair_approved_extension_node_ids"
                    .to_string(),
            );
        }
        if !approved.is_subset(&allowed) {
            let extras: BTreeSet<_> = approved.difference(&allowed).cloned().collect();
            return Some(format!(
                "global_repair_approved_extension_node_ids contains nodes outside the dependency neighborhood of the reviewer's proposed_extension_nodes: {extras:?}"
            ));
        }
        let protected = state.live_protected_statement_node_set();
        if !approved.is_disjoint(&protected) {
            let extras: BTreeSet<_> = approved.intersection(&protected).cloned().collect();
            return Some(format!(
                "global_repair_approved_extension_node_ids must be disjoint from live_protected_statement_node_set; offenders: {extras:?}"
            ));
        }
    }
    let plan_view = serde_json::json!({
        "confirm_need_input": response.confirm_need_input,
        "report": response.report,
        "tasks": response.tasks,
        "probe_paths": response.probe_paths,
        "need_input_audit": is_need_input_audit,
        "cone_clean_node": response.cone_clean_node,
        "global_repair_approve": response.global_repair_approve,
        "global_repair_approved_extension_node_ids": response.global_repair_approved_extension_node_ids,
        "global_repair_auditor_reason": response.global_repair_auditor_reason,
    });
    match serde_json::to_string(&plan_view) {
        Ok(text) if text.chars().count() > AUDIT_PLAN_MAX_JSON_CHARS => Some(format!(
            "stuck math audit plan exceeds {AUDIT_PLAN_MAX_JSON_CHARS} serialized JSON characters"
        )),
        Err(err) => Some(format!("stuck math audit plan failed to serialize: {err}")),
        _ => None,
    }
}

fn retry_or_transition_stuck_math_audit_to_reviewer(
    state: &mut ProtocolState,
    reason: String,
) -> Vec<ProtocolCommand> {
    if state.stuck_math_audit_burst_retry_count >= STUCK_MATH_AUDIT_BURST_RETRY_LIMIT {
        state.stuck_math_audit_burst_retry_count = 0;
        if let Some(context) = state.stuck_math_audit.need_input_audit.take() {
            state.latest_stuck_math_audit_rejection_reason = format!(
                "NeedInputAuditor failed twice in a row; routing to HumanGate without a new plan: {reason}"
            );
            state.stage = Stage::HumanGate;
            state.gate_kind = GateKind::NeedInput;
            state.gate_from_invalid_attempt = context.gate_from_invalid_attempt;
            state.pending_task = None;
            state.pending_protected_semantic_scope_confirmation = None;
            return vec![issue_request(state, RequestKind::HumanGate)];
        }
        // GlobalRepairAuditor retry-exhaust: the reviewer's pending
        // `global_repair_request` is treated as auto-declined so the
        // mutex with the NeedInput lane is preserved on the next
        // Reviewer cycle (and the validator's mutex invariant cannot
        // be tripped). The reviewer's option set on the issued Review
        // is unchanged — the auto-decline only surfaces a context
        // signal via `latest_global_repair_audit_decline_reason`.
        if state.pending_global_repair_request.take().is_some() {
            state.latest_global_repair_audit_decline_reason =
                "auditor failed twice on this dispatch; auto-declining global_repair_request"
                    .to_string();
            state.latest_global_repair_audit_decline_cycle = Some(state.cycle);
            state.pending_global_repair_grant = None;
        }
        state.latest_stuck_math_audit_rejection_reason = format!(
            "stuck math audit failed twice in a row; routing to reviewer without a new plan: {reason}"
        );
        state.stage = Stage::Reviewer;
        return vec![issue_request(state, RequestKind::Review)];
    }
    state.stuck_math_audit_burst_retry_count =
        state.stuck_math_audit_burst_retry_count.saturating_add(1);
    state.latest_stuck_math_audit_rejection_reason = reason;
    vec![issue_request(state, RequestKind::StuckMathAudit)]
}

fn apply_stuck_math_audit_response(
    state: &mut ProtocolState,
    response: StuckMathAuditResponse,
) -> Result<Vec<ProtocolCommand>, TransitionError> {
    expect_stage(state, Stage::StuckMathAudit, "StuckMathAudit")?;
    if response.cycle != state.cycle {
        return Err(TransitionError::CycleMismatch {
            expected: state.cycle,
            found: response.cycle,
        });
    }
    if response.status == ResponseStatus::Malformed {
        return Ok(retry_or_transition_stuck_math_audit_to_reviewer(
            state,
            "malformed stuck math audit response".to_string(),
        ));
    }
    if let Some(reason) = stuck_math_audit_validation_failure(state, &response) {
        return Ok(retry_or_transition_stuck_math_audit_to_reviewer(
            state, reason,
        ));
    }
    let cone_clean_node = response.cone_clean_node.clone();
    let need_input_context = state.stuck_math_audit.need_input_audit.clone();
    let confirm_need_input = response.confirm_need_input;

    // global_repair_mode Step B: if a Step A request is pending, route
    // through the grant / decline path and return to the reviewer
    // BEFORE writing an audit_plan — this audit lane is distinct.
    if let Some(pending) = state.pending_global_repair_request.take() {
        if response.global_repair_approve {
            let approved: BTreeSet<NodeId> = response
                .global_repair_approved_extension_node_ids
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(NodeId::from)
                .collect();
            state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
                approved_extension_nodes: approved,
                auditor_reason: response.global_repair_auditor_reason.trim().to_string(),
                dispatched_at_cycle: pending.dispatched_at_cycle,
                granted_at_cycle: state.cycle,
                review_request_id: pending.review_request_id,
            });
            state.latest_global_repair_audit_decline_reason.clear();
            state.latest_global_repair_audit_decline_cycle = None;
        } else {
            let reason = response.global_repair_auditor_reason.trim().to_string();
            state.latest_global_repair_audit_decline_reason = if reason.is_empty() {
                "auditor declined the global_repair_request (no reason supplied)".to_string()
            } else {
                reason
            };
            state.latest_global_repair_audit_decline_cycle = Some(state.cycle);
            state.pending_global_repair_grant = None;
        }
        state.stuck_math_audit_burst_retry_count = 0;
        state.latest_stuck_math_audit_rejection_reason.clear();
        state.stage = Stage::Reviewer;
        return Ok(vec![issue_request(state, RequestKind::Review)]);
    }

    state.superseded_audit_plan = state.audit_plan.clone();
    state.audit_plan = Some(AuditPlan {
        report: response.report.trim().to_string(),
        tasks: response
            .tasks
            .into_iter()
            .map(|mut task| {
                task.id = task.id.trim().to_string();
                task.title = task.title.trim().to_string();
                task.body = task.body.trim().to_string();
                task.dismissed = false;
                task.dismissed_reason.clear();
                task.dismissed_at_cycle = None;
                task
            })
            .collect(),
        probe_paths: response.probe_paths,
        need_input_audit: need_input_context.is_some(),
        cone_clean_node: cone_clean_node.clone(),
        written_at_cycle: state.cycle,
        written_by_request: response.request_id,
        trigger_at_write: state.stuck_math_audit.trigger.clone(),
    });
    state.stuck_math_audit_burst_retry_count = 0;
    state.latest_stuck_math_audit_rejection_reason.clear();
    state.stuck_math_audit.need_input_audit = None;
    if let Some(context) = need_input_context {
        if confirm_need_input {
            state.stage = Stage::HumanGate;
            state.gate_kind = GateKind::NeedInput;
            state.gate_from_invalid_attempt = context.gate_from_invalid_attempt;
            state.pending_task = None;
            state.pending_protected_semantic_scope_confirmation = None;
            return Ok(vec![issue_request(state, RequestKind::HumanGate)]);
        }
    }
    if let Some(node) = cone_clean_node {
        return apply_audit_authorized_theorem_stating_node_reset(state, node);
    }
    state.stage = Stage::Reviewer;
    Ok(vec![issue_request(state, RequestKind::Review)])
}

/// Cleanup-v2 Step 13: transition out of `Stage::CleanupAudit` into
/// `Stage::Reviewer`. The reviewer cycle will inspect
/// `cleanup_audit_tasks` and decide what to dispatch / dismiss /
/// finalize. Audit-round state is preserved (round counter, scratchpad
/// — the latter as context the reviewer may surface in subsequent
/// audit re-entries).
fn transition_audit_to_reviewer(state: &mut ProtocolState) -> Vec<ProtocolCommand> {
    state.audit_burst_retry_count = 0;
    state.stage = Stage::Reviewer;
    vec![issue_request(state, RequestKind::Review)]
}

fn ensure_lane_keys_match<T>(
    expected_lanes: &BTreeSet<LaneId>,
    actual_lane_updates: &BTreeMap<LaneId, T>,
    label: &str,
) -> Result<(), TransitionError> {
    let actual_lanes: BTreeSet<_> = actual_lane_updates.keys().cloned().collect();
    if &actual_lanes != expected_lanes {
        return Err(TransitionError::IllegalResponse(format!(
            "{label} lanes do not match request lanes"
        )));
    }
    Ok(())
}

fn ensure_node_lane_scope<T>(
    expected_nodes: &BTreeSet<NodeId>,
    actual_lane_updates: &BTreeMap<LaneId, BTreeMap<NodeId, T>>,
    label: &str,
) -> Result<(), TransitionError> {
    // Audit Finding 4: enforce exact coverage. Reject any lane whose key set
    // is not exactly the requested-node set. A missing entry is malformed
    // (not silently `Update::Same`); an extra entry is out-of-scope. This
    // hardens the kernel API contract — the bridge normalization always
    // fills full lane × node maps, and any future adapter that bypasses
    // normalization must do the same.
    for (lane, updates) in actual_lane_updates {
        for node in updates.keys() {
            if !expected_nodes.contains(node) {
                return Err(TransitionError::IllegalResponse(format!(
                    "{label} returned unexpected node {node}"
                )));
            }
        }
        for node in expected_nodes {
            if !updates.contains_key(node) {
                return Err(TransitionError::IllegalResponse(format!(
                    "{label} lane {lane} is missing requested node {node}"
                )));
            }
        }
    }
    Ok(())
}

fn ensure_target_lane_scope<T>(
    expected_targets: &BTreeSet<TargetId>,
    actual_lane_updates: &BTreeMap<LaneId, BTreeMap<TargetId, T>>,
    label: &str,
) -> Result<(), TransitionError> {
    // Audit Finding 4: enforce exact coverage. See `ensure_node_lane_scope`
    // for rationale — missing entries are malformed, not `Update::Same`.
    for (lane, updates) in actual_lane_updates {
        for target in updates.keys() {
            if !expected_targets.contains(target) {
                return Err(TransitionError::IllegalResponse(format!(
                    "{label} returned unexpected target {target}"
                )));
            }
        }
        for target in expected_targets {
            if !updates.contains_key(target) {
                return Err(TransitionError::IllegalResponse(format!(
                    "{label} lane {lane} is missing requested target {target}"
                )));
            }
        }
    }
    Ok(())
}

fn reconcile_votes<T: Copy + Eq>(votes: impl IntoIterator<Item = Update<T>>) -> Update<T> {
    let mut iter = votes.into_iter();
    let Some(first) = iter.next() else {
        return Update::Same;
    };
    if iter.all(|vote| vote == first) {
        first
    } else {
        Update::Same
    }
}

fn reconcile_node_lane_updates<T: Copy + Eq>(
    lanes: &BTreeSet<LaneId>,
    nodes: &BTreeSet<NodeId>,
    lane_updates: &BTreeMap<LaneId, BTreeMap<NodeId, Update<T>>>,
) -> BTreeMap<NodeId, Update<T>> {
    nodes
        .iter()
        .filter_map(|node| {
            let vote = reconcile_votes(lanes.iter().map(|lane| {
                lane_updates
                    .get(lane)
                    .and_then(|updates| updates.get(node))
                    .copied()
                    .unwrap_or(Update::Same)
            }));
            match vote {
                Update::Same => None,
                _ => Some((node.clone(), vote)),
            }
        })
        .collect()
}

fn reconcile_target_lane_updates<T: Copy + Eq>(
    lanes: &BTreeSet<LaneId>,
    targets: &BTreeSet<TargetId>,
    lane_updates: &BTreeMap<LaneId, BTreeMap<TargetId, Update<T>>>,
) -> BTreeMap<TargetId, Update<T>> {
    targets
        .iter()
        .filter_map(|target| {
            let vote = reconcile_votes(lanes.iter().map(|lane| {
                lane_updates
                    .get(lane)
                    .and_then(|updates| updates.get(target))
                    .copied()
                    .unwrap_or(Update::Same)
            }));
            match vote {
                Update::Same => None,
                _ => Some((target.clone(), vote)),
            }
        })
        .collect()
}

fn validate_corr_lane_updates(
    request: &WrapperRequest,
    response: &CorrResponse,
) -> Result<(), TransitionError> {
    ensure_lane_keys_match(
        &request.verify_lanes,
        &response.node_lane_updates,
        "correspondence node",
    )?;
    ensure_lane_keys_match(
        &request.verify_lanes,
        &response.target_lane_updates,
        "correspondence target",
    )?;
    ensure_node_lane_scope(
        &request.verify_nodes,
        &response.node_lane_updates,
        "correspondence node",
    )?;
    ensure_target_lane_scope(
        &request.verify_targets,
        &response.target_lane_updates,
        "correspondence target",
    )?;
    Ok(())
}

fn validate_paper_lane_updates(
    request: &WrapperRequest,
    response: &PaperResponse,
) -> Result<(), TransitionError> {
    ensure_lane_keys_match(
        &request.verify_lanes,
        &response.target_lane_updates,
        "paper-faithfulness",
    )?;
    ensure_target_lane_scope(
        &request.verify_targets,
        &response.target_lane_updates,
        "paper-faithfulness target",
    )?;
    // Per-node scenario: lane keys must match. Coverage is enforced
    // leniently on missing entries — the reconciler treats absent
    // entries as `Update::Same` from that lane (matching the corr /
    // sound reconcilers). Under strict unanimity, a single missing
    // entry will cause that node's verdict to fall through to
    // `Update::Same`, leaving the kernel's Unknown derivation active so
    // the next cycle re-issues a Paper request. The bridge
    // normalization always emits complete node × lane maps so this
    // lenience is a defence-in-depth, not the primary path.
    if !request.substantiveness_verify_nodes.is_empty() {
        ensure_lane_keys_match(
            &request.verify_lanes,
            &response.node_lane_updates,
            "paper-faithfulness node",
        )?;
        for (lane, updates) in &response.node_lane_updates {
            for node in updates.keys() {
                if !request.substantiveness_verify_nodes.contains(node) {
                    return Err(TransitionError::IllegalResponse(format!(
                        "paper-faithfulness node lane {lane} returned unexpected node {node}"
                    )));
                }
            }
        }
    }
    if request.deviation_verify_id.is_some() {
        ensure_lane_keys_match(
            &request.verify_lanes,
            &response.deviation_lane_updates,
            "deviation authorization",
        )?;
        for (lane, updates) in &response.deviation_lane_updates {
            for deviation in updates.keys() {
                if Some(deviation) != request.deviation_verify_id.as_ref() {
                    return Err(TransitionError::IllegalResponse(format!(
                        "deviation authorization lane {lane} returned unexpected deviation {deviation}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_sound_lane_updates(
    request: &WrapperRequest,
    response: &SoundResponse,
) -> Result<(), TransitionError> {
    ensure_lane_keys_match(&request.verify_lanes, &response.lane_updates, "soundness")?;
    ensure_node_lane_scope(&request.verify_nodes, &response.lane_updates, "soundness")?;
    Ok(())
}

fn reconcile_corr_node_lane_updates(
    request: &WrapperRequest,
    response: &CorrResponse,
) -> BTreeMap<NodeId, Update<CorrStatus>> {
    reconcile_node_lane_updates(
        &request.verify_lanes,
        &request.verify_nodes,
        &response.node_lane_updates,
    )
}

fn reconcile_paper_target_lane_updates(
    request: &WrapperRequest,
    response: &PaperResponse,
) -> BTreeMap<TargetId, Update<CorrStatus>> {
    reconcile_target_lane_updates(
        &request.verify_lanes,
        &request.verify_targets,
        &response.target_lane_updates,
    )
}

fn reconcile_deviation_lane_updates(
    request: &WrapperRequest,
    response: &PaperResponse,
) -> BTreeMap<DeviationId, Update<CorrStatus>> {
    let mut out = BTreeMap::new();
    let Some(deviation) = request.deviation_verify_id.clone() else {
        return out;
    };
    let mut vote: Option<CorrStatus> = None;
    for lane in &request.verify_lanes {
        let Some(Update::Set(status)) = response
            .deviation_lane_updates
            .get(lane)
            .and_then(|updates| updates.get(&deviation))
        else {
            out.insert(deviation, Update::Same);
            return out;
        };
        if let Some(existing) = vote {
            if existing != *status {
                out.insert(deviation, Update::Same);
                return out;
            }
        } else {
            vote = Some(*status);
        }
    }
    if let Some(status) = vote {
        out.insert(deviation, Update::Set(status));
    }
    out
}

/// Reconcile substantiveness lane updates across the verifier panel into a
/// single per-node verdict map. Voting rule is strict unanimity, matching
/// `reconcile_node_lane_updates` (corr/sound):
///   - all lanes vote `Update::Set(Pass)` → `Update::Set(CorrStatus::Pass)`.
///   - all lanes vote `Update::Set(Fail)` → `Update::Set(CorrStatus::Fail)`.
///   - all lanes vote `Update::Set(NotDoneYet)` (or `Set(Unknown)`) → leave
///     Unknown (returns `Update::Same`); `apply_substantiveness_updates`
///     interprets `Same` as "do nothing", and
///     `current_substantiveness_state` continues to derive Unknown from a
///     missing/Unknown status entry.
///   - any disagreement (e.g. Pass vs Fail, Pass vs NotDoneYet) → leave
///     Unknown (`Update::Same`), so the kernel re-dispatches.
///   - lenient-missing-entries: a requested node missing from a lane is
///     treated as `Update::Same` from that lane (mirrors
///     `reconcile_node_lane_updates`'s `unwrap_or(Update::Same)`).
///
/// The output type is `Update<CorrStatus>` because the substantiveness
/// status mirror is keyed by `CorrStatus` (Pass/Fail/Unknown). The
/// per-lane vote type is `Update<SubstantivenessStatus>`; the unanimity
/// vote runs at the `Update<SubstantivenessStatus>` level, then the
/// successful Pass/Fail outcome is mapped to `CorrStatus`. NotDoneYet /
/// Unknown unanimous wins collapse to `Update::Same` (no write).
fn reconcile_substantiveness_lane_updates(
    request: &WrapperRequest,
    response: &PaperResponse,
) -> BTreeMap<NodeId, Update<CorrStatus>> {
    let lane_votes = reconcile_node_lane_updates(
        &request.verify_lanes,
        &request.substantiveness_verify_nodes,
        &response.node_lane_updates,
    );
    let mut out: BTreeMap<NodeId, Update<CorrStatus>> = BTreeMap::new();
    for (node, vote) in lane_votes {
        let mapped = match vote {
            Update::Same => Update::Same,
            Update::Set(SubstantivenessStatus::Pass) => Update::Set(CorrStatus::Pass),
            Update::Set(SubstantivenessStatus::Fail) => Update::Set(CorrStatus::Fail),
            // Unanimous NotDoneYet / Unknown does not write a Pass/Fail
            // status; `Update::Same` keeps the kernel's Unknown derivation
            // active so the next cycle re-issues a Paper request.
            Update::Set(SubstantivenessStatus::NotDoneYet)
            | Update::Set(SubstantivenessStatus::Unknown) => Update::Same,
        };
        out.insert(node, mapped);
    }
    out
}

/// Apply reconciled substantiveness updates to the kernel
/// status mirrors. Same shape as `apply_corr_updates`: Pass/Fail writes
/// the status AND captures the current fingerprint as the approved
/// fingerprint. NotDoneYet / Unknown comes through as `Update::Same` and
/// is a no-op (the kernel treats the node as still-Unknown and
/// re-dispatches).
fn apply_substantiveness_updates(
    status_map: &mut BTreeMap<NodeId, CorrStatus>,
    approved_fingerprints: &mut BTreeMap<NodeId, Fingerprint>,
    current_fingerprints: &BTreeMap<NodeId, Fingerprint>,
    updates: BTreeMap<NodeId, Update<CorrStatus>>,
) {
    for (node, update) in updates {
        match update {
            Update::Same => {}
            Update::Set(status) => {
                status_map.insert(node.clone(), status);
                if matches!(status, CorrStatus::Pass | CorrStatus::Fail) {
                    if let Some(fp) = current_fingerprints.get(&node) {
                        approved_fingerprints.insert(node, fp.clone());
                    }
                }
            }
        }
    }
}

fn apply_deviation_updates(
    deviation_files: &BTreeMap<DeviationId, String>,
    status_map: &mut BTreeMap<DeviationId, CorrStatus>,
    approved_fingerprints: &mut BTreeMap<DeviationId, String>,
    current_fingerprints: &BTreeMap<DeviationId, String>,
    updates: BTreeMap<DeviationId, Update<CorrStatus>>,
) {
    for (deviation, update) in updates {
        // Late-response defense: a worker burst between verifier panel
        // dispatch and panel response landing could have retired this
        // deviation via `deviation_deletions`. In that case
        // `deviation_files` no longer carries the id, and writing
        // `deviation_status[id]` here would resurrect a stale entry
        // (benign, since `current_deviation_state` reads Unknown when
        // either fingerprint map is missing, but a local invariant
        // violation: `deviation_status.keys() ⊆ deviation_files.keys()`).
        // Skip any id no longer in `deviation_files` to keep the
        // invariant local.
        if !deviation_files.contains_key(&deviation) {
            continue;
        }
        if let Update::Set(status) = update {
            status_map.insert(deviation.clone(), status);
            if matches!(status, CorrStatus::Pass | CorrStatus::Fail) {
                if let Some(fp) = current_fingerprints
                    .get(&deviation)
                    .filter(|fp| !fp.is_empty())
                {
                    approved_fingerprints.insert(deviation, fp.clone());
                }
            }
        }
    }
}

fn reconcile_sound_lane_updates(
    request: &WrapperRequest,
    response: &SoundResponse,
) -> BTreeMap<NodeId, Update<SoundStatus>> {
    request
        .verify_nodes
        .iter()
        .filter_map(|node| {
            let vote = reconcile_votes(request.verify_lanes.iter().map(|lane| {
                response
                    .lane_updates
                    .get(lane)
                    .and_then(|updates| updates.get(node))
                    .copied()
                    .unwrap_or(Update::Same)
            }));
            match vote {
                Update::Same => {
                    let distinct: BTreeSet<SoundStatus> = response
                        .lane_updates
                        .values()
                        .filter_map(|updates| match updates.get(node) {
                            Some(Update::Set(status)) => Some(*status),
                            _ => None,
                        })
                        .collect();
                    if distinct.len() > 1 {
                        Some((node.clone(), Update::Same))
                    } else {
                        None
                    }
                }
                _ => Some((node.clone(), vote)),
            }
        })
        .collect()
}

fn apply_corr_updates(
    status_map: &mut BTreeMap<NodeId, CorrStatus>,
    approved_fingerprints: &mut BTreeMap<NodeId, Fingerprint>,
    current_fingerprints: &BTreeMap<NodeId, Fingerprint>,
    updates: BTreeMap<NodeId, Update<CorrStatus>>,
) {
    for (node, update) in updates {
        match update {
            Update::Same => {}
            Update::Set(status) => {
                status_map.insert(node.clone(), status);
                if matches!(status, CorrStatus::Pass | CorrStatus::Fail) {
                    if let Some(fp) = current_fingerprints.get(&node) {
                        approved_fingerprints.insert(node, fp.clone());
                    }
                }
            }
        }
    }
}

fn apply_target_corr_updates(
    status_map: &mut BTreeMap<TargetId, CorrStatus>,
    approved_fingerprints: &mut BTreeMap<TargetId, Fingerprint>,
    current_fingerprints: &BTreeMap<TargetId, Fingerprint>,
    updates: BTreeMap<TargetId, Update<CorrStatus>>,
) {
    for (target, update) in updates {
        match update {
            Update::Same => {}
            Update::Set(status) => {
                status_map.insert(target.clone(), status);
                if matches!(status, CorrStatus::Pass | CorrStatus::Fail) {
                    if let Some(fp) = current_fingerprints.get(&target) {
                        approved_fingerprints.insert(target, fp.clone());
                    }
                }
            }
        }
    }
}

fn apply_sound_updates(
    status_map: &mut BTreeMap<NodeId, SoundStatus>,
    approved_fingerprints: &mut BTreeMap<NodeId, Fingerprint>,
    assessments: &mut BTreeMap<NodeId, SoundAssessment>,
    current_fingerprints: &BTreeMap<NodeId, Fingerprint>,
    current_fingerprint_parts: &BTreeMap<NodeId, SoundFingerprintParts>,
    lane_updates: &SoundLaneUpdates,
    updates: BTreeMap<NodeId, Update<SoundStatus>>,
) {
    for (node, update) in updates {
        let lane_votes: BTreeMap<LaneId, SoundStatus> = lane_updates
            .iter()
            .filter_map(|(lane, nodes)| match nodes.get(&node) {
                Some(Update::Set(status)) => Some((lane.clone(), *status)),
                _ => None,
            })
            .collect();
        match update {
            Update::Same => {
                let distinct: BTreeSet<SoundStatus> = lane_votes.values().copied().collect();
                if distinct.len() > 1 {
                    status_map.insert(node.clone(), SoundStatus::Unknown);
                    approved_fingerprints.remove(&node);
                    assessments.insert(
                        node.clone(),
                        SoundAssessment {
                            status: SoundAssessmentStatus::SplitUnknown,
                            origin: AssessmentOrigin::VerifierPanel,
                            fingerprints: current_fingerprint_parts
                                .get(&node)
                                .cloned()
                                .unwrap_or_else(|| SoundFingerprintParts {
                                    own_tex_hash: String::new(),
                                    dep_statement_hashes: BTreeMap::new(),
                                    combined_sound_fp: current_fingerprints
                                        .get(&node)
                                        .cloned()
                                        .unwrap_or_default(),
                                }),
                            lane_votes,
                            reviewer_action_id: None,
                        },
                    );
                }
            }
            Update::Set(status) => {
                status_map.insert(node.clone(), status);
                if matches!(
                    status,
                    SoundStatus::Pass | SoundStatus::Fail | SoundStatus::Structural
                ) {
                    if let Some(fp) = current_fingerprints.get(&node) {
                        approved_fingerprints.insert(node.clone(), fp.clone());
                    }
                    let assessment_status = match status {
                        SoundStatus::Pass => SoundAssessmentStatus::VerifierPass,
                        SoundStatus::Fail => SoundAssessmentStatus::VerifierFail,
                        SoundStatus::Structural => SoundAssessmentStatus::VerifierStructural,
                        SoundStatus::Unknown => SoundAssessmentStatus::FreshUnknown,
                    };
                    assessments.insert(
                        node.clone(),
                        SoundAssessment {
                            status: assessment_status,
                            origin: AssessmentOrigin::VerifierPanel,
                            fingerprints: current_fingerprint_parts
                                .get(&node)
                                .cloned()
                                .unwrap_or_else(|| SoundFingerprintParts {
                                    own_tex_hash: String::new(),
                                    dep_statement_hashes: BTreeMap::new(),
                                    combined_sound_fp: current_fingerprints
                                        .get(&node)
                                        .cloned()
                                        .unwrap_or_default(),
                                }),
                            lane_votes,
                            reviewer_action_id: None,
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find the first IssueRequest in a commands vec, skipping any
    /// preceding RestoreWorktree*/CommitCheckpoint commands. (#54)
    fn first_issued_request(commands: &[ProtocolCommand]) -> &WrapperRequest {
        commands
            .iter()
            .find_map(|c| match c {
                ProtocolCommand::IssueRequest { request } => Some(request),
                _ => None,
            })
            .expect("expected at least one IssueRequest in commands")
    }

    fn set<T: From<String> + Ord>(items: &[&str]) -> BTreeSet<T> {
        items.iter().map(|s| T::from((*s).to_string())).collect()
    }

    /// Minimal paper-focus + paper_grounding pair that satisfies
    /// `review_response_paper_grounding_legal` on a Continue response.
    /// Use in tests whose request carries blockers or
    /// retry_outcome_kind ∈ {Stuck, NeedsRestructure}, i.e. friction
    /// reviews where Continue requires paper-grounded routing.
    fn test_paper_grounding() -> (
        Vec<crate::model::PaperFocusRange>,
        crate::model::PaperGrounding,
    ) {
        (
            vec![crate::model::PaperFocusRange {
                start_line: 1,
                end_line: 1,
                reason: "test".to_string(),
            }],
            crate::model::PaperGrounding {
                consulted_cited_ranges: true,
                basis_summary: "test basis".to_string(),
            },
        )
    }

    fn node_blockers(items: &[(&str, BlockerKind, &str)]) -> BTreeSet<Blocker> {
        items
            .iter()
            .map(|(node, kind, fp)| Blocker {
                kind: *kind,
                object: BlockerObject::Node {
                    node: NodeId::from(*node),
                },
                fingerprint: (*fp).to_string(),
                deferred: false,
            })
            .collect()
    }

    fn mark_substantiveness_pass(state: &mut ProtocolState, node: &str, fp: &str) {
        state
            .substantiveness_status
            .insert(node.into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(node.into(), fp.into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node.into(), fp.into());
    }

    fn clear_substantiveness(state: &mut ProtocolState, node: &str) {
        let node = NodeId::from(node);
        state.substantiveness_status.remove(&node);
        state.substantiveness_approved_fingerprints.remove(&node);
        state
            .live
            .substantiveness_current_fingerprints
            .remove(&node);
    }

    #[test]
    fn stuck_math_audit_cone_clean_emits_runtime_reset_then_review() {
        let node = NodeId::from("A");
        let target = TargetId::from("main");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            stage: Stage::StuckMathAudit,
            active_node: Some(node.clone()),
            configured_targets: BTreeSet::from([target.clone()]),
            coarse_dag_nodes: BTreeSet::from([node.clone()]),
            node_kinds: BTreeMap::from([(node.clone(), NodeKind::Proof)]),
            committed_node_kinds: BTreeMap::from([(node.clone(), NodeKind::Proof)]),
            proof_nodes: BTreeSet::from([node.clone()]),
            committed_proof_nodes: BTreeSet::from([node.clone()]),
            target_claims: BTreeMap::from([(node.clone(), BTreeSet::from([target.clone()]))]),
            committed_target_claims: BTreeMap::from([(
                node.clone(),
                BTreeSet::from([target.clone()]),
            )]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([node.clone()]),
                open_nodes: BTreeSet::from([node.clone()]),
                coverage: BTreeMap::from([(target.clone(), BTreeSet::from([node.clone()]))]),
                paper_current_fingerprints: BTreeMap::from([(target.clone(), "paper".into())]),
                ..WorkingSnapshot::default()
            },
            committed: WorkingSnapshot {
                present_nodes: BTreeSet::from([node.clone()]),
                open_nodes: BTreeSet::from([node.clone()]),
                coverage: BTreeMap::from([(target.clone(), BTreeSet::from([node.clone()]))]),
                paper_current_fingerprints: BTreeMap::from([(target.clone(), "paper".into())]),
                ..WorkingSnapshot::default()
            },
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            ..ProtocolState::default()
        };
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.ensure_node_metadata();
        let request = issue_request_for_test(&mut state, RequestKind::StuckMathAudit);
        assert_eq!(
            request.resettable_theorem_stating_nodes,
            BTreeSet::from([node.clone()])
        );

        let response = StuckMathAuditResponse {
            request_id: request.id,
            cycle: request.cycle,
            status: ResponseStatus::Ok,
            report: "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
            cone_clean_node: Some(node.clone()),
            ..StuckMathAuditResponse::default()
        };
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("audit-authorized theorem-stating reset should be accepted");

        assert_eq!(
            outcome.commands,
            vec![
                ProtocolCommand::RestoreTheoremStatingNodeAndPruneOrphans { node: node.clone() },
                ProtocolCommand::CommitCheckpoint,
            ]
        );
        assert_eq!(outcome.state.stage, Stage::Start);
        assert!(outcome.state.in_flight_request.is_none());
        assert!(!outcome.state.force_stuck_math_audit_after_rewind);
        assert!(outcome.state.force_review_after_cone_clean);
        assert_eq!(
            outcome
                .state
                .audit_plan
                .as_ref()
                .and_then(|plan| plan.cone_clean_node.as_ref()),
            Some(&node)
        );

        let review_outcome = apply_event(outcome.state, ProtocolEvent::StartCycle)
            .expect("post-clean start cycle should issue reviewer request");
        assert_eq!(review_outcome.state.stage, Stage::Reviewer);
        assert!(!review_outcome.state.force_review_after_cone_clean);
        match review_outcome.commands.as_slice() {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(request.kind, RequestKind::Review);
                assert_eq!(
                    request
                        .audit_plan
                        .as_ref()
                        .and_then(|plan| plan.cone_clean_node.as_ref()),
                    Some(&node)
                );
                assert!(!request
                    .allowed_resets
                    .contains(&ResetChoice::TheoremStatingNode));
            }
            other => panic!("expected Review request after cone clean, got {other:?}"),
        }
    }

    #[test]
    fn stuck_math_audit_rejects_cone_clean_for_non_coarse_node() {
        let coarse = NodeId::from("A");
        let helper = NodeId::from("AHelper");
        let target = TargetId::from("main");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            stage: Stage::StuckMathAudit,
            configured_targets: BTreeSet::from([target.clone()]),
            coarse_dag_nodes: BTreeSet::from([coarse.clone()]),
            node_kinds: BTreeMap::from([
                (coarse.clone(), NodeKind::Proof),
                (helper.clone(), NodeKind::Proof),
            ]),
            proof_nodes: BTreeSet::from([coarse.clone(), helper.clone()]),
            deps: BTreeMap::from([(coarse.clone(), BTreeSet::from([helper.clone()]))]),
            target_claims: BTreeMap::from([(coarse.clone(), BTreeSet::from([target.clone()]))]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([coarse.clone(), helper.clone()]),
                open_nodes: BTreeSet::from([coarse.clone(), helper.clone()]),
                coverage: BTreeMap::from([(target.clone(), BTreeSet::from([coarse.clone()]))]),
                paper_current_fingerprints: BTreeMap::from([(target.clone(), "paper".into())]),
                ..WorkingSnapshot::default()
            },
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            ..ProtocolState::default()
        };
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.ensure_node_metadata();
        let request = issue_request_for_test(&mut state, RequestKind::StuckMathAudit);
        assert_eq!(
            request.resettable_theorem_stating_nodes,
            BTreeSet::from([coarse])
        );

        let response = StuckMathAuditResponse {
            request_id: request.id,
            cycle: request.cycle,
            status: ResponseStatus::Ok,
            report: "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
            cone_clean_node: Some(helper),
            ..StuckMathAuditResponse::default()
        };
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("invalid cone clean target should be rejected and reissued");

        assert!(outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("resettable coarse nodes"));
        match outcome.commands.as_slice() {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(request.kind, RequestKind::StuckMathAudit);
            }
            other => panic!("expected StuckMathAudit retry, got {other:?}"),
        }
    }

    /// Install a placeholder `LocalClosureRecord` for `name` in both
    /// the live and committed tiers so `formalization_complete()`'s
    /// `records_present` clause (Patch C plan §7.6) is satisfied for a
    /// sorry-free proof_node. Used by tests that intentionally reach
    /// Cleanup or assert `formalization_complete()` directly.
    fn install_placeholder_local_closure_record(state: &mut ProtocolState, name: &str) {
        let mut record = LocalClosureRecord::default();
        record.node = NodeId::from(name);
        state
            .local_closure_records
            .insert(NodeId::from(name), record.clone());
        state
            .committed_local_closure_records
            .insert(NodeId::from(name), record);
    }

    fn base_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.max_theorem_invalid_attempt = 2;
        state.proof_invalid_review_threshold = 2;
        state.configured_targets = set(&["t"]);
        state.proof_nodes = set(&["a"]);
        state.target_claims.insert("a".into(), set(&["t"]));
        state.live.present_nodes = set(&["a", "b"]);
        state.live.open_nodes = set(&["a", "b"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta".into());
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ta".into());
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca".into());
        state
            .live
            .corr_current_fingerprints
            .insert("b".into(), "cb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa".into());
        state.corr_status.insert("a".into(), CorrStatus::Pass);
        state.corr_status.insert("b".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("a".into(), "ca".into());
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());
        state.paper_status.insert("t".into(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());
        state.sound_status.insert("a".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.node_rank.insert("a".into(), 2);
        state.node_rank.insert("b".into(), 1);
        state.committed = state.live.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state
    }

    fn issue_request_for_test(state: &mut ProtocolState, kind: RequestKind) -> WrapperRequest {
        state.issue_request(kind)
    }

    fn clean_proof_review_state_with_protected_closure() -> ProtocolState {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.approved_targets.configured_targets = state.configured_targets.clone();
        state.approved_targets.coverage = state.live.coverage.clone();
        state.approved_targets.protected_closure_nodes = set(&["b"]);
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.sound_status.insert("b".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state
    }

    fn empty_corr_node_lanes(lanes: &BTreeSet<LaneId>) -> CorrNodeLaneUpdates {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn empty_corr_target_lanes(lanes: &BTreeSet<LaneId>) -> CorrTargetLaneUpdates {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn unanimous_sound_lanes(
        lanes: &BTreeSet<LaneId>,
        node: &str,
        status: SoundStatus,
    ) -> SoundLaneUpdates {
        lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    BTreeMap::from([(NodeId::from(node), Update::Set(status))]),
                )
            })
            .collect()
    }

    fn disagree_sound_lanes(lanes: &BTreeSet<LaneId>, node: &str) -> SoundLaneUpdates {
        let mut out = BTreeMap::new();
        for (idx, lane) in lanes.iter().enumerate() {
            let status = if idx == 0 {
                SoundStatus::Pass
            } else {
                SoundStatus::Fail
            };
            out.insert(
                lane.clone(),
                BTreeMap::from([(NodeId::from(node), Update::Set(status))]),
            );
        }
        out
    }

    fn disagree_corr_node_lanes(lanes: &BTreeSet<LaneId>, node: &str) -> CorrNodeLaneUpdates {
        let mut out = BTreeMap::new();
        for (idx, lane) in lanes.iter().enumerate() {
            let status = if idx == 0 {
                CorrStatus::Pass
            } else {
                CorrStatus::Fail
            };
            out.insert(
                lane.clone(),
                BTreeMap::from([(NodeId::from(node), Update::Set(status))]),
            );
        }
        out
    }

    #[test]
    fn derives_global_blockers() {
        let mut state = base_state();
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        let got = state.global_blockers();
        let expected = node_blockers(&[
            ("a", BlockerKind::Soundness, "sa"),
            ("b", BlockerKind::NodeCorr, "cb"),
        ]);
        assert_eq!(got, expected);
    }

    #[test]
    fn start_cycle_issues_worker_request() {
        let state = base_state();
        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.cycle, 1);
        assert_eq!(outcome.commands.len(), 1);
        assert!(matches!(
            outcome.commands[0],
            ProtocolCommand::IssueRequest {
                request: WrapperRequest {
                    kind: RequestKind::Worker,
                    cycle: 1,
                    ..
                }
            }
        ));
    }

    #[test]
    fn start_cycle_request_carries_narrow_payload() {
        let mut state = base_state();
        state.held_target = Some("a".into());

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected issue_request, got {:?}", other),
        };
        assert_eq!(request.phase, Phase::TheoremStating);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.cycle, 1);
        assert_eq!(request.held_target, None);
        assert_eq!(request.mode, TaskMode::Global);
        assert!(request.verify_nodes.is_empty());
        assert!(request.verify_targets.is_empty());
        assert!(request.verify_lanes.is_empty());
        assert_eq!(request.worker_context.authorized_nodes, set(&["a", "b"]));
        assert!(request.worker_context.enabled);
    }

    #[test]
    fn v32_audit2_engine_fallback_clears_stale_anchor_and_reselects() {
        // Audit-2 round-2 test gap fix: round-1 added a fallback in
        // `start_cycle` so when `select_initial_proof_active_node`
        // returns None due to the anchor's cone having no work-needing
        // candidates, the engine clears the stale anchor + counter and
        // re-selects from the unrestricted set. Without this fallback
        // the engine would emit a Worker request with
        // `active_node = None`. This test exercises the engine path
        // end-to-end (the model.rs sibling test only covers the
        // predicate's cone restriction).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state.active_node = None;
        state.pending_task = None;
        state.active_coarse_node = Some("a".into());
        state.coarse_dag_nodes = set(&["a", "b"]);
        state.approved_targets.configured_targets = state.configured_targets.clone();
        state.approved_targets.coverage = state.live.coverage.clone();
        state.proof_nodes = set(&["a", "b"]);
        state.committed_proof_nodes = state.proof_nodes.clone();
        // `a` depends on `b` so `b` isn't an orphan — orphan cleanup
        // would otherwise fire ahead of the active-node selection and
        // synthesize a pending_task whose node doesn't track our re-
        // select, breaking validate(). The cone of `a` then becomes
        // {a, b}; to keep `b` outside the cone-narrowing we test, we
        // instead split: `a`'s deps stay empty (cone = {a}), and we
        // depend on the OUT-of-cone direction — `b` depends on `a` —
        // so `a` is not orphaned (it has the inbound edge from `b`)
        // and `b` is not orphaned (it claims target `t` via coverage).
        state.deps.insert("b".into(), set(&["a"]));
        state.committed_deps.insert("b".into(), set(&["a"]));
        state.live.coverage.insert("t".into(), set(&["a", "b"]));
        state
            .committed
            .coverage
            .insert("t".into(), set(&["a", "b"]));
        state.target_claims.insert("b".into(), set(&["t"]));
        state
            .committed_target_claims
            .insert("b".into(), set(&["t"]));
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta,b=tb".into());
        state
            .committed
            .paper_current_fingerprints
            .insert("t".into(), "a=ta,b=tb".into());
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta,b=tb".into());
        state
            .live
            .target_fingerprints
            .insert("b".into(), "tb".into());
        state
            .committed
            .target_fingerprints
            .insert("b".into(), "tb".into());
        // `a` is closed (out of open_nodes); `b` is open and supported.
        state.live.open_nodes = set(&["b"]);
        state.committed.open_nodes = set(&["b"]);
        // `b` needs sound + sub Pass to avoid spurious blockers that
        // would widen the cone via repair-mode.
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.sound_status.insert("b".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        assert!(
            state.global_blockers().is_empty(),
            "test precondition: no blockers; got {:?}",
            state.global_blockers()
        );

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();

        // Fallback fired: anchor cleared + counter zeroed.
        assert_eq!(
            outcome.state.active_coarse_node, None,
            "stale anchor must be cleared by the fallback"
        );
        assert_eq!(
            outcome.state.cycles_in_coarse_repair_mode, 0,
            "counter must follow the TypeOK invariant when anchor goes None"
        );
        // The re-select picked `b` from the (now unrestricted) candidate
        // set; the Worker request carries it.
        assert_eq!(
            outcome.state.active_node,
            Some("b".into()),
            "re-selection should pick the work-needing node outside the original cone"
        );
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.active_node, Some("b".into()));
    }

    #[test]
    fn theorem_targeted_worker_request_carries_authorized_nodes() {
        let mut state = base_state();
        state.stage = Stage::Start;
        state.active_node = Some("a".into());
        state.target_edit_mode = TargetEditMode::Targeted;
        state.deps.insert("a".into(), set(&["b"]));

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected issue_request, got {:?}", other),
        };
        let worker_ctx = &request.worker_context;
        assert_eq!(
            worker_ctx.validation_kind,
            WorkerValidationKind::TheoremTargeted
        );
        assert_eq!(worker_ctx.authorized_nodes, set(&["a", "b"]));
        assert!(worker_ctx.enabled);
    }

    #[test]
    fn theorem_stuck_routes_directly_to_reviewer_without_verification() {
        let mut state = base_state();
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.active_node = Some("a".into());
        state.target_edit_mode = TargetEditMode::Targeted;
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Stuck,
                    snapshot: base_state().live,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Stuck);
        assert!(!outcome.state.invalid_attempt);
        assert_eq!(
            first_issued_request(&outcome.commands).kind,
            RequestKind::Worker,
        );
    }

    #[test]
    fn invalid_theorem_review_request_uses_committed_blocker_payload() {
        let mut state = base_state();
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.attempt = 2;
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut dirty = WorkingSnapshot::default();
        dirty.present_nodes = set(&["b"]);
        dirty.open_nodes = set(&["b"]);
        dirty.coverage.insert("t".into(), set(&["b"]));
        dirty
            .corr_current_fingerprints
            .insert("b".into(), "cb_dirty".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: dirty,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Review);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert_eq!(request.retry_attempt, 2);
        assert!(request.invalid_attempt);
        assert!(request.blockers.is_empty());
    }

    #[test]
    fn theorem_review_continue_resolves_latest_corr_split_to_fail_and_reaches_worker_start() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 6;
        state.target_claims.insert("b".into(), set(&["t"]));
        state.live.coverage.insert("t".into(), set(&["a", "b"]));
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        state.latest_corr_review_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let blocker_b = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    blocker.object,
                    BlockerObject::Node { ref node } if node == "b"
                )
            })
            .cloned()
            .expect("missing blocker b");

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 6,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    task_blockers: BTreeSet::from([blocker_b.clone()]),
                    next_mode: TaskMode::Global,
                    paper_focus_ranges,
                    paper_grounding,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply theorem review continue");

        assert_eq!(outcome.state.stage, Stage::Start);
        assert_eq!(outcome.state.corr_status.get("b"), Some(&CorrStatus::Fail));
        assert_eq!(
            outcome.state.corr_approved_fingerprints.get("b"),
            Some(&"cb".to_string())
        );

        let started = apply_event(outcome.state, ProtocolEvent::StartCycle)
            .expect("start next theorem cycle after adjudicated corr split");
        let request = match &started.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected worker request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.blockers, BTreeSet::from([blocker_b]));
    }

    // Option C (2026-06-04): removed
    // `theorem_review_continue_override_of_split_corr_promotes_to_pass_and_clears_blocker`
    // — the override→Pass path is retired. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

    #[test]
    fn sound_task_blocker_writes_fail_only_with_verifier_scope_or_current_fail() {
        // Sound tasking must not manufacture a verifier-free Fail for a
        // fresh Unknown fingerprint. Node "a" is Unknown and in the latest
        // sound review scope, so task→Fail is allowed. Node "b" is Unknown
        // but outside that scope, so task→Fail is ignored and the kernel
        // keeps it available for a future sound verifier dispatch.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.proof_nodes.insert("b".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_approved_fingerprints.remove("b");
        state.latest_sound_review_nodes = set(&["a"]);

        let blockers = node_blockers(&[
            ("a", BlockerKind::Soundness, "sa"),
            ("b", BlockerKind::Soundness, "sb"),
        ]);
        state.apply_review_blocker_adjudications(&blockers);

        // a (in latest review scope): pins Fail with approved_fp set.
        assert_eq!(state.sound_status.get("a"), Some(&SoundStatus::Fail));
        assert_eq!(
            state.sound_approved_fingerprints.get("a"),
            Some(&"sa".to_string())
        );
        // b (OUTSIDE latest review scope): remains Unknown, so the reviewer
        // did not pin a sound fingerprint no verifier had just assessed.
        assert_eq!(state.sound_status.get("b"), Some(&SoundStatus::Unknown));
        assert!(state.sound_approved_fingerprints.get("b").is_none());
        assert!(!state.review_task_blocker_forwardable(
            &node_blockers(&[("b", BlockerKind::Soundness, "sb")])
                .into_iter()
                .next()
                .unwrap()
        ));
        // current_sound_state reports definite Fail only for "a".
        assert_eq!(
            state.current_sound_state(&NodeId::from("a")),
            CurrentCheckState::Fail
        );
        assert_eq!(
            state.current_sound_state(&NodeId::from("b")),
            CurrentCheckState::Unknown
        );
    }

    // Option C (2026-06-04): removed
    // `override_blocker_still_requires_latest_review_scope`,
    // `unknown_to_pass_override_is_refused_by_default*` (5 lane
    // variants), `deviation_fail_to_pass_override_is_refused_by_default`,
    // and `deviation_fail_to_pass_override_works_with_opt_in`. All
    // exercised the retired override→Pass path. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

    #[test]
    fn apply_deviation_updates_does_not_resurrect_retired_id() {
        // Late-response defense: a verifier panel is dispatched while
        // deviation `d_late` is alive; before the panel response lands,
        // a worker burst retires `d_late` via `deviation_deletions`
        // (model.rs:5660-5666). `deviation_files` / `deviation_status` /
        // both fingerprint maps are cleared as part of the retire. The
        // late panel response then arrives carrying a Pass verdict for
        // `d_late`. The kernel must NOT resurrect a `deviation_status`
        // entry for an id no longer present in `deviation_files`, since
        // the local invariant `deviation_status.keys() ⊆
        // deviation_files.keys()` would otherwise break (a benign break
        // — `current_deviation_state` reads Unknown when either
        // fingerprint map is missing — but still a discipline
        // violation worth fixing at the source).
        let alive_id = DeviationId::from("d_alive");
        let retired_id = DeviationId::from("d_late");

        // `deviation_files` reflects post-retire state: `d_alive` still
        // present, `d_late` deleted.
        let mut deviation_files: BTreeMap<DeviationId, String> = BTreeMap::new();
        deviation_files.insert(alive_id.clone(), "reference/d_alive.tex".into());

        // `current_fingerprints` similarly only carries the alive id —
        // the retire step at model.rs:5664 wipes
        // `live.deviation_current_fingerprints[retired_id]`.
        let mut current_fingerprints: BTreeMap<DeviationId, String> = BTreeMap::new();
        current_fingerprints.insert(alive_id.clone(), "afp".into());

        // `status_map` / `approved_fingerprints` start empty (the retire
        // also wiped any prior status/approved entries for `d_late`,
        // model.rs:5662-5663).
        let mut status_map: BTreeMap<DeviationId, CorrStatus> = BTreeMap::new();
        let mut approved_fingerprints: BTreeMap<DeviationId, String> = BTreeMap::new();

        // The late panel verdict carries Pass for both — the alive id
        // (legitimate update) and the retired id (resurrection
        // attempt). Real kernel flow gets here through
        // `reconcile_deviation_lane_updates`; we hand-build the
        // `updates` map to isolate the apply path.
        let mut updates: BTreeMap<DeviationId, Update<CorrStatus>> = BTreeMap::new();
        updates.insert(alive_id.clone(), Update::Set(CorrStatus::Pass));
        updates.insert(retired_id.clone(), Update::Set(CorrStatus::Pass));

        apply_deviation_updates(
            &deviation_files,
            &mut status_map,
            &mut approved_fingerprints,
            &current_fingerprints,
            updates,
        );

        // Retired id: not resurrected.
        assert!(
            !status_map.contains_key(&retired_id),
            "apply_deviation_updates must skip retired ids (deviation_status); got {:?}",
            status_map
        );
        assert!(
            !approved_fingerprints.contains_key(&retired_id),
            "apply_deviation_updates must skip retired ids (approved_fp); got {:?}",
            approved_fingerprints
        );
        // Alive id: regular write lands (sanity check — the filter
        // must not over-prune).
        assert_eq!(
            status_map.get(&alive_id),
            Some(&CorrStatus::Pass),
            "apply_deviation_updates must still write for alive ids; got {:?}",
            status_map
        );
        assert_eq!(
            approved_fingerprints.get(&alive_id),
            Some(&"afp".to_string()),
            "apply_deviation_updates must still pin approved_fp for alive ids; got {:?}",
            approved_fingerprints
        );
    }

    #[test]
    fn task_adjudication_does_not_overwrite_approved_fp_when_live_fp_missing() {
        // Audit follow-up (on Finding 2's audit, asymmetry between
        // adjudication and verifier paths): adjudication used to fall
        // back to `blocker.fingerprint` when
        // `live.<lane>_current_fingerprints[node]` was missing; the
        // verifier-driven path simply skips the approved_fp write in
        // that case (engine.rs apply_corr_updates / apply_sound_updates).
        // The fix aligns adjudication with verifier — if current_fp is
        // missing, no approved_fp write happens, so a stale
        // `blocker.fingerprint` value can never end up pinned as
        // approved_fp by coincidence on a future state.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        // Pre-seed approved_fp = "old-approved-value" so we can detect
        // whether adjudication overwrites it.
        state
            .sound_approved_fingerprints
            .insert("a".into(), "old-approved-value".into());
        // Status = Unknown so the lane is currently in scope to be
        // tasked. Live current_fp is intentionally MISSING for "a".
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.live.sound_current_fingerprints.remove("a");
        state.latest_sound_review_nodes = set(&["a"]);

        // Construct a blocker carrying a NON-EMPTY fingerprint that
        // the pre-fix code would have picked up via the fallback.
        let blockers = BTreeSet::from([Blocker {
            kind: BlockerKind::Soundness,
            object: BlockerObject::Node { node: "a".into() },
            fingerprint: "stale-blocker-fp".into(),
            deferred: false,
        }]);
        state.apply_review_blocker_adjudications(&blockers);

        // Status flips to Fail — durable status write per Finding 2.
        assert_eq!(state.sound_status.get("a"), Some(&SoundStatus::Fail));
        // approved_fp stays UNCHANGED — the verifier-aligned write
        // condition (current_fp present) is false, so the prior value
        // survives. Pre-fix this would have been overwritten with
        // "stale-blocker-fp", a fingerprint the verifier never produced.
        assert_eq!(
            state.sound_approved_fingerprints.get("a"),
            Some(&"old-approved-value".to_string()),
            "approved_fp must NOT be overwritten with blocker.fingerprint \
             when live current_fp is missing — that's the verifier-path \
             contract this fix aligns with",
        );
    }

    #[test]
    fn empty_coverage_paper_fail_excluded_from_reset_blockers() {
        // Audit Finding 6: empty paper coverage produces a definite Fail
        // via current_paper_state (model.rs:~1974). Before the fix,
        // request_allowed_reset_blockers offered this as a resettable
        // blocker — but resetting status maps doesn't help because the
        // derived state is still Fail (coverage is still empty post-
        // reset). Inert option, not soundness-affecting. Fix:
        // exclude empty-coverage Paper Fails from reset_blockers.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        // Configure a target with NO coverage. base_state has target "t"
        // covered by ["a"]; introduce a NEW target "u" with nothing
        // covering it.
        state.configured_targets.insert("u".into());
        state.live.coverage.insert("u".into(), BTreeSet::new());
        // Sanity precondition: current_paper_state("u") is Fail.
        assert_eq!(
            state.current_paper_state(&TargetId::from("u")),
            CurrentCheckState::Fail,
            "empty coverage on configured target must produce a definite Fail",
        );
        // Sanity precondition: the empty-coverage blocker IS in
        // current_failed_blockers (the upstream).
        let failed = state.current_failed_blockers();
        let empty_cov_blocker = Blocker {
            kind: BlockerKind::PaperFaithfulness,
            object: BlockerObject::Target { target: "u".into() },
            fingerprint: state
                .live
                .paper_current_fingerprints
                .get("u")
                .cloned()
                .unwrap_or_default(),
            deferred: false,
        };
        assert!(
            failed.contains(&empty_cov_blocker),
            "current_failed_blockers must include the empty-coverage Fail \
             (otherwise the test setup is wrong); got {:?}",
            failed,
        );

        // The actual assertion: request_allowed_reset_blockers must
        // FILTER OUT the empty-coverage Fail.
        let allowed_reset = state.request_allowed_reset_blockers(RequestKind::Review);
        assert!(
            !allowed_reset.contains(&empty_cov_blocker),
            "empty-coverage Paper Fail must NOT be offered as a reset \
             blocker — resetting can't fix structural coverage gaps; \
             got {:?}",
            allowed_reset,
        );
    }

    // Option C (2026-06-04): removed
    // `proof_review_continue_override_of_split_soundness_promotes_to_pass`
    // and `proof_review_continue_override_of_unanimous_fail_is_rejected`.
    // The override→Pass path and `ReviewerAcceptedPass` assessment writes
    // are retired. The unanimous-fail rejection is now subsumed by the
    // structural-empty `allowed_override_blockers` check (always empty
    // ⇒ any override_blockers is illegal).

    #[test]
    fn reviewer_requested_sound_verifier_dispatches_from_next_start_cycle() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 6;
        state.active_node = None;
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        state.sound_status.insert("a".into(), SoundStatus::Unknown);

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(request
            .sound_verifier_requestable_nodes
            .contains(&NodeId::from("a")));

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 6,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    request_sound_verifier_nodes: BTreeSet::from([NodeId::from("a")]),
                    next_mode: TaskMode::Local,
                    paper_focus_ranges,
                    paper_grounding,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply review verifier request");

        let outcome = apply_event(outcome.state, ProtocolEvent::StartCycle)
            .expect("start cycle should dispatch requested Sound verifier");
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Sound);
        assert_eq!(
            request.sound_verify_nodes,
            BTreeSet::from([NodeId::from("a")])
        );
    }

    #[test]
    fn proof_review_task_blocker_adjudicates_split_to_fail_and_flows_to_worker_request() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 6;
        state.active_node = Some("a".into());
        // Split soundness on node `a` — status stays Unknown, approved FP unpinned.
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.latest_sound_review_nodes = set(&["a"]);

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_a = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::Soundness) if node == "a"
                )
            })
            .cloned()
            .expect("missing soundness blocker for a");

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 6,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    task_blockers: BTreeSet::from([blocker_a.clone()]),
                    next_active: Some("a".into()),
                    next_mode: TaskMode::Restructure,
                    authorized_nodes: BTreeSet::from([NodeId::from("a")]),
                    paper_focus_ranges,
                    paper_grounding,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply proof review continue task_blocker");

        // Adjudication pinned status to Fail + approvedFp.
        assert_eq!(
            outcome.state.sound_status.get("a"),
            Some(&SoundStatus::Fail)
        );
        assert_eq!(
            outcome.state.sound_approved_fingerprints.get("a"),
            Some(&"sa".to_string())
        );
        // Blocker persists because Fail is not Pass; task_blockers flows into
        // pending_task and therefore into the next worker request.
        assert!(outcome.state.global_blockers().contains(&blocker_a));
        let pending = outcome
            .state
            .pending_task
            .as_ref()
            .expect("proof-review Continue must set a pending task");
        assert!(pending.task_blockers.contains(&blocker_a));
        // pending_task.task_blockers flows into the next worker request via
        // `request_blockers(Worker) = pending_task.task_blockers.clone()`
        // (model.rs:2165-2170) — so the worker's `blockers` field will carry
        // the same adjudicated-to-Fail entries on the next StartCycle.
    }

    #[test]
    fn proof_sound_verify_nodes_includes_drift_induced_unknowns_not_just_active_node() {
        // In proof-formalization, a non-active node whose current sound
        // fingerprint has drifted from its approved fingerprint
        // (reachable under CoarseRestructure mode, where a worker can
        // edit a helper node's NL) derives `current_sound_state = Unknown`
        // via the fingerprint-mismatch arm. If `sound_verify_nodes` were
        // scoped to `{active_node}` only, that drifted node would never
        // re-enter the verifier frontier — it would surface as a blocker
        // the reviewer cannot adjudicate (the `latest_sound_review_nodes`
        // containment guard fails) and cannot escape without a
        // whole-state reset.
        //
        // Test: sound_verify_nodes surfaces every present node with
        // `current_sound_unknown`, not just the active one.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        // Add a helper node `b` that has a previously-passed soundness
        // verdict but whose current fingerprint has drifted.
        state.proof_nodes.insert("b".into());
        state.live.open_nodes.insert("b".into());
        state.sound_status.insert("b".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb_approved".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb_current_drifted".into());
        // Sanity: derived state is Unknown on `b` (drift).
        assert!(state.current_sound_unknown(&NodeId::from("b")));
        // active_node `a` is also Unknown (no current_fp set in base_state
        // matches approved_fp exactly — but that's not what this test is
        // about). The key assertion: `b` is in sound_verify_nodes even
        // though active_node is `a`.
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            verify_nodes.contains("b"),
            "drifted non-active node must be in sound_verify_nodes; got {verify_nodes:?}"
        );
    }

    #[test]
    fn proof_sound_verify_nodes_excludes_node_whose_own_substantiveness_fails() {
        // Gate (self / Fail): if `a` itself has a current sub-Fail, running
        // Sound on `a` is wasted — the .tex statement isn't substantive, so
        // a Sound verdict on its proof isn't trustworthy.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa_drifted".into());
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Fail);
        assert!(state.current_sound_unknown(&NodeId::from("a")));
        assert!(state.current_substantiveness_fail(&NodeId::from("a")));
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            !verify_nodes.contains("a"),
            "expected `a` excluded (own sub is Fail); got {verify_nodes:?}"
        );
    }

    #[test]
    fn proof_sound_verify_nodes_excludes_node_whose_own_substantiveness_unknown() {
        // Gate (self / Unknown): polarity matches Corr's gate. If `a`'s
        // substantiveness is Unknown (not yet adjudicated), Sound on `a`
        // is also wasted — we don't even know whether the statement is
        // substantive yet.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa_drifted".into());
        // Clear `a`'s substantiveness so it becomes Unknown (no status).
        clear_substantiveness(&mut state, "a");
        assert!(state.current_sound_unknown(&NodeId::from("a")));
        assert!(state.current_substantiveness_unknown(&NodeId::from("a")));
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            !verify_nodes.contains("a"),
            "expected `a` excluded (own sub is Unknown); got {verify_nodes:?}"
        );
    }

    #[test]
    fn proof_sound_verify_nodes_excludes_node_with_direct_substantiveness_failed_dep() {
        // Gate (direct dep): node `a` imports `b` directly. If `b` is
        // sub-Failed, `a`'s proof cites a non-meaningful claim, so Sound
        // on `a` is wasted. `a` is excluded; `b` itself stays in (no
        // sub-failed direct deps of b — its own sub-Fail is the prior
        // self-gate test's concern; here we want to isolate the dep
        // gating).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        // a imports b directly. (base_state has b in present/open but
        // not as a proof_node and no deps[a] entry.)
        state.deps.insert("a".into(), set(&["b"]));
        // Make `b` a proof_node with sound drift so it would
        // ordinarily appear in sound_verify_nodes.
        state.proof_nodes.insert("b".into());
        state.sound_status.insert("b".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb_approved".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb_drifted".into());
        // Drift `a` too.
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa_drifted".into());
        // Mark `b` sub-Failed. `a`'s own sub stays Pass (from base_state).
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Fail);
        assert!(state.current_substantiveness_fail(&NodeId::from("b")));
        assert!(state.has_direct_substantiveness_unverified_dep(&NodeId::from("a")));
        assert!(!state.current_substantiveness_fail(&NodeId::from("a")));
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            !verify_nodes.contains("a"),
            "expected `a` excluded (direct dep `b` is sub-Failed); got {verify_nodes:?}"
        );
        // `b` is itself sub-Failed — covered by the self-gate test, also
        // excluded here. The point of this test is `a`'s exclusion via
        // its dep, so we don't double-assert on b.
    }

    #[test]
    fn proof_sound_verify_nodes_excludes_node_with_direct_substantiveness_unknown_dep() {
        // Gate (direct dep / Unknown): polarity matches Corr's gate. If
        // `a` imports `b` directly and `b`'s substantiveness is Unknown
        // (not yet adjudicated), Sound on `a` is gated until `b` reaches
        // Pass.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        state.deps.insert("a".into(), set(&["b"]));
        // Drift `a`'s sound so it would ordinarily be in the verify set.
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa_drifted".into());
        // Clear `b`'s substantiveness so it becomes Unknown.
        clear_substantiveness(&mut state, "b");
        assert!(state.current_sound_unknown(&NodeId::from("a")));
        assert!(state.current_substantiveness_unknown(&NodeId::from("b")));
        assert!(state.has_direct_substantiveness_unverified_dep(&NodeId::from("a")));
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            !verify_nodes.contains("a"),
            "expected `a` excluded (direct dep `b` has sub Unknown); got {verify_nodes:?}"
        );
    }

    #[test]
    fn proof_sound_verify_nodes_globally_gated_by_unrelated_substantiveness_fail() {
        // Policy: Sound dispatch is globally gated by `corr_blockers_exist`
        // — Sound only runs once paper / corr / substantiveness / deviation
        // are Pass across the board. This supersedes the prior per-node
        // gate that allowed Sound on a candidate whose direct-deps cone
        // was clean even when unrelated nodes had open work.
        //
        // Topology: a → b → c with `c` sub-Failed. Under the per-node
        // policy a was still sound-auto-dispatch-eligible (transitive
        // sub-Fail outside its direct cone); under the global policy
        // c's sub-Fail blocks Sound everywhere until c clears.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some("a".into());
        state.proof_nodes.insert("c".into());
        state.live.present_nodes.insert("c".into());
        state.live.open_nodes.insert("c".into());
        state.deps.insert("a".into(), set(&["b"]));
        state.deps.insert("b".into(), set(&["c"]));
        mark_substantiveness_pass(&mut state, "c", "sub-c");
        state
            .substantiveness_status
            .insert("c".into(), CorrStatus::Fail);
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa_drifted".into());

        // Per-node eligibility still holds — `a` would have been eligible
        // under the prior policy.
        assert!(state.current_sound_unknown(&NodeId::from("a")));
        assert!(state.current_substantiveness_fail(&NodeId::from("c")));
        assert!(
            !state.has_direct_substantiveness_unverified_dep(&NodeId::from("a")),
            "a's direct cone (just {{b}}) is clean of substantiveness failures"
        );
        assert!(state.sound_verifier_eligible(&NodeId::from("a")));

        // But the global gate fires on c's sub-Fail, so the dispatch
        // frontier is empty.
        assert!(
            state.corr_blockers_exist(),
            "c's sub-Fail must trip the global non-sound-lane predicate"
        );
        let verify_nodes = state.sound_verify_nodes();
        assert!(
            verify_nodes.is_empty(),
            "global gate must suppress Sound dispatch while substantiveness elsewhere is Fail; got {verify_nodes:?}"
        );
    }

    #[test]
    fn theorem_review_request_carries_kernel_authored_affordances() {
        let mut state = base_state();
        state.stage = Stage::Reviewer;
        state.cycle = 6;
        state.paper_status.insert("t".into(), CorrStatus::Fail);

        let request = issue_request_for_test(&mut state, RequestKind::Review);

        assert_eq!(
            request.allowed_decisions,
            BTreeSet::from([
                ReviewDecisionKind::Continue,
                ReviewDecisionKind::AdvancePhase,
                ReviewDecisionKind::NeedInput,
            ])
        );
        assert_eq!(request.kernel_hinted_next_active_nodes, set(&["a"]));
        assert_eq!(request.targeted_next_active_nodes, set(&["a"]));
        assert!(!request.allow_targeted_without_next_active);
        assert_eq!(
            request.allowed_next_modes,
            BTreeSet::from([TaskMode::Global, TaskMode::Targeted])
        );
    }

    #[test]
    fn invalid_theorem_review_request_restricts_affordances_to_current_mode() {
        let mut state = base_state();
        state.stage = Stage::Reviewer;
        state.cycle = 6;
        state.invalid_attempt = true;
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.target_edit_mode = TargetEditMode::Targeted;

        let request = issue_request_for_test(&mut state, RequestKind::Review);

        assert_eq!(
            request.allowed_decisions,
            BTreeSet::from([ReviewDecisionKind::Continue, ReviewDecisionKind::NeedInput])
        );
        assert!(request.kernel_hinted_next_active_nodes.is_empty());
        assert!(request.targeted_next_active_nodes.is_empty());
        assert!(request.allow_targeted_without_next_active);
        assert_eq!(
            request.allowed_next_modes,
            BTreeSet::from([TaskMode::Targeted])
        );
    }

    #[test]
    fn theorem_noop_valid_skips_correspondence() {
        let mut state = base_state();
        state.stage = Stage::Worker;
        state.cycle = 4;
        // Keep substantiveness lane Pass-clean so the no-op-Valid path
        // routes straight to Reviewer (post external-audit Finding 2 the
        // routing now goes through `route_after_progress`, which preempts
        // Reviewer dispatch when any non-adjudicable Substantiveness Unknown
        // exists; base_state leaves substantiveness_status empty, hence
        // every present node is Unknown unless we set Pass here).
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let snapshot = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 4,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert!(matches!(
            outcome.commands[0],
            ProtocolCommand::IssueRequest {
                request: WrapperRequest {
                    kind: RequestKind::Review,
                    ..
                }
            }
        ));
    }

    #[test]
    fn theorem_worker_delta_with_empty_paper_frontier_routes_to_corr() {
        let mut state = base_state();
        state.stage = Stage::Worker;
        state.cycle = 4;
        state.deps.insert("a".into(), set(&["b"]));
        state.committed_deps = state.deps.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut changed = state.live.clone();
        changed
            .corr_current_fingerprints
            .insert("b".into(), "cb2".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 4,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: changed,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyCorr);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected corr request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Corr);
        assert!(request.paper_verify_targets.is_empty());
        assert_eq!(request.corr_verify_nodes, set(&["b"]));
    }

    #[test]
    fn proof_worker_delta_with_empty_paper_frontier_routes_to_corr() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.deps.insert("a".into(), set(&["b"]));
        state.committed_deps = state.deps.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut changed = state.live.clone();
        changed
            .corr_current_fingerprints
            .insert("b".into(), "cb2".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: changed,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyCorr);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected corr request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Corr);
        assert!(request.paper_verify_targets.is_empty());
        assert_eq!(request.corr_verify_nodes, set(&["b"]));
    }

    #[test]
    fn proof_invalid_retries_before_reviewer() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut dirty = state.live.clone();
        dirty.present_nodes.remove("a");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: dirty,
                    deterministic_rejection_reasons: vec![
                        "protected package drifted outside proof scope".into(),
                    ],
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.attempt, 2);
        assert_eq!(outcome.state.live, outcome.state.committed);
        assert!(outcome.state.invalid_attempt);
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
        assert_eq!(
            request.deterministic_worker_rejection_reasons,
            vec!["protected package drifted outside proof scope".to_string()]
        );
    }

    #[test]
    fn proof_invalid_cleanup_retry_rebuilds_pending_task_instead_of_tripping_invariant() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        state.active_node = None;
        state.proof_edit_mode = ProofEditMode::CoarseRestructure;

        state.committed.open_nodes = set(&["a"]);
        state.committed_deps.insert("a".into(), set(&["b"]));
        state
            .committed_target_claims
            .insert("a".into(), set(&["t"]));

        state.live.present_nodes = set(&["a", "b", "c", "d"]);
        state.live.open_nodes = BTreeSet::new();
        state.live.coverage.insert("t".into(), set(&["a"]));
        state
            .live
            .target_fingerprints
            .insert("c".into(), "tc".into());
        state
            .live
            .target_fingerprints
            .insert("d".into(), "td".into());
        state
            .live
            .corr_current_fingerprints
            .insert("c".into(), "cc".into());
        state
            .live
            .corr_current_fingerprints
            .insert("d".into(), "cd".into());
        state
            .live
            .sound_current_fingerprints
            .insert("c".into(), "sc".into());
        state
            .live
            .sound_current_fingerprints
            .insert("d".into(), "sd".into());
        state.node_kinds.insert("c".into(), NodeKind::Definition);
        state.node_kinds.insert("d".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a", "d"]);
        state.deps.insert("a".into(), set(&["d"]));
        state.deps.insert("d".into(), set(&["c"]));
        state.target_claims.insert("a".into(), set(&["t"]));

        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let snapshot = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot,
                    deterministic_rejection_reasons: vec![
                        "cleanup contract rejected retained-node semantic_dep_updates".into(),
                    ],
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("invalid cleanup retry should stay in cleanup mode");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert!(outcome.state.invalid_attempt);
        assert!(outcome.state.live.present_nodes.contains("b"));
        let task = outcome
            .state
            .pending_task
            .as_ref()
            .expect("retry request should carry a cleanup task");
        assert_eq!(task.mode, TaskMode::CoarseRestructure);
        assert_eq!(task.orphan_cleanup_nodes, set(&["b"]));
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.mode, TaskMode::CoarseRestructure);
        assert!(request.invalid_attempt);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert_eq!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup
        );
        assert_eq!(request.current_present_nodes, set(&["a", "b", "c", "d"]));
    }

    #[test]
    fn theorem_invalid_cleanup_retry_reissues_cleanup_worker() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a", "b", "c"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.node_kinds.insert("b".into(), NodeKind::Proof);
        state.node_kinds.insert("c".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a", "b", "c"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.deps.insert("b".into(), set(&["Preamble"]));
        state.deps.insert("c".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::Global,
            orphan_cleanup_nodes: set(&["b", "c"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let live_before = state.live.clone();
        assert_eq!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup
        );

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: WorkingSnapshot::default(),
                    deterministic_rejection_reasons: vec![
                        "cleanup contract rejected retained-node semantic_dep_updates".into(),
                    ],
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("invalid theorem cleanup retry should stay in cleanup mode");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.live, live_before);
        assert!(outcome.state.invalid_attempt);
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
        assert_eq!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup
        );
        assert_eq!(
            request.current_present_nodes,
            set(&["Preamble", "a", "b", "c"])
        );
    }

    #[test]
    fn theorem_stuck_with_delta_takes_stuck_retry_path_with_state_restored() {
        // Post-rule-removal: theorem-stating Stuck-with-delta is honoured
        // (no longer reclassified Invalid). Engine must call
        // restore_committed before retry/review routing, otherwise the
        // worker's reported snapshot leaks into in-memory state while the
        // disk worktree is rolled back. Asserts both the routing change
        // (Stuck retry, retry_outcome_kind=Stuck) AND the state-integrity
        // requirement (live equals committed; worker's "extra" node is NOT
        // committed).
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut dirty = state.live.clone();
        dirty.present_nodes.insert("b".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Stuck,
                    snapshot: dirty,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("stuck worker response is honoured even with snapshot delta");

        // Routing: Stuck retry, NOT Invalid.
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert!(!outcome.state.invalid_attempt);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Stuck);
        assert!(
            outcome
                .state
                .deterministic_worker_rejection_reasons
                .is_empty(),
            "no reclassification reason should be set for Stuck-with-delta"
        );

        // State integrity: live restored from committed, "b" NOT leaked in.
        assert_eq!(
            outcome.state.live.present_nodes,
            set::<NodeId>(&["Preamble", "a"]),
            "in-memory live.present_nodes must be restored from committed; \
             worker's reported `b` should NOT be committed"
        );
        assert!(!outcome
            .state
            .live
            .present_nodes
            .contains(&NodeId::from("b")));

        // Worktree restore command emitted (disk-side rollback).
        let restore_emitted = outcome
            .commands
            .iter()
            .any(|cmd| matches!(cmd, ProtocolCommand::RestoreWorktreeToActiveWorkerBase));
        assert!(
            restore_emitted,
            "RestoreWorktreeToActiveWorkerBase must be emitted"
        );

        // Next request is a worker retry under Stuck context.
        let next = first_issued_request(&outcome.commands);
        assert_eq!(next.kind, RequestKind::Worker);
        assert_eq!(next.retry_outcome_kind, RetryOutcomeKind::Stuck);
    }

    #[test]
    fn theorem_needs_restructure_with_delta_routes_to_review_with_state_restored() {
        // Post-rule-removal: theorem-stating NeedsRestructure-with-delta is
        // honoured (no longer reclassified Invalid). Engine must call
        // restore_committed before review routing.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 4;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut dirty = state.live.clone();
        dirty.present_nodes.insert("b".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 4,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::NeedsRestructure,
                    snapshot: dirty,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("needs_restructure worker response is honoured even with snapshot delta");

        // Routing: NR doesn't retry; goes through begin_retry_review →
        // route_after_progress. retry_outcome_kind reflects NR.
        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
        assert!(
            outcome
                .state
                .deterministic_worker_rejection_reasons
                .is_empty(),
            "no reclassification reason should be set for NR-with-delta"
        );

        // State integrity.
        assert_eq!(
            outcome.state.live.present_nodes,
            set::<NodeId>(&["Preamble", "a"]),
            "in-memory live.present_nodes must be restored from committed; \
             worker's reported `b` should NOT be committed"
        );

        // Worktree restore command emitted.
        let restore_emitted = outcome
            .commands
            .iter()
            .any(|cmd| matches!(cmd, ProtocolCommand::RestoreWorktreeToActiveWorkerBase));
        assert!(restore_emitted);
    }

    #[test]
    fn theorem_stuck_preserves_routing_when_worker_drops_active_from_snapshot() {
        // Regression: theorem-stating Stuck/NR was routing the worker's
        // (potentially node-dropping) snapshot through `state.live = snapshot`
        // + `relegalize_active_fields` BEFORE the per-outcome restore_committed,
        // which nulled active_node if the worker's snapshot omitted it. The
        // subsequent restore_committed only restored live structural state,
        // leaving the routing fields wiped — a WIP leak through the rollback.
        // Fix: handle Stuck/NR before snapshot apply, mirroring proof worker.
        // (held_target is also vulnerable in principle, but it is also nulled
        // by `corr_blockers_exist`/etc. so it's covered by other invariants;
        // active_node is the cleaner regression check here.)
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 7;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.active_node = Some("a".into());

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        // Worker's snapshot drops "a" — exactly the case that nulled
        // active_node before the fix.
        let mut dirty = state.live.clone();
        dirty.present_nodes.remove(&NodeId::from("a"));
        dirty.open_nodes.remove(&NodeId::from("a"));
        dirty.coverage.insert("t".into(), set::<NodeId>(&[]));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 7,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Stuck,
                    snapshot: dirty,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("stuck honoured even when worker drops the active node from the snapshot");

        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Stuck);
        assert_eq!(
            outcome.state.active_node,
            Some(NodeId::from("a")),
            "active_node must survive a Stuck rollback even when the worker's \
             snapshot omitted the node"
        );
        assert!(
            outcome
                .state
                .live
                .present_nodes
                .contains(&NodeId::from("a")),
            "live.present_nodes must be restored from committed"
        );
    }

    #[test]
    fn theorem_needs_restructure_preserves_routing_when_worker_drops_active_from_snapshot() {
        // Same regression as `theorem_stuck_preserves_routing_...` but for
        // the NeedsRestructure path. TLA `AcceptNeedsRestructureWorkerTheorem`
        // lists activeNode in UNCHANGED, so the kernel must preserve it too.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 8;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.active_node = Some("a".into());

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut dirty = state.live.clone();
        dirty.present_nodes.remove(&NodeId::from("a"));
        dirty.open_nodes.remove(&NodeId::from("a"));
        dirty.coverage.insert("t".into(), set::<NodeId>(&[]));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 8,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::NeedsRestructure,
                    snapshot: dirty,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("NR honoured even when worker drops the active node from the snapshot");

        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
        assert_eq!(
            outcome.state.active_node,
            Some(NodeId::from("a")),
            "active_node must survive an NR rollback even when the worker's \
             snapshot omitted the node"
        );
        assert!(
            outcome
                .state
                .live
                .present_nodes
                .contains(&NodeId::from("a")),
            "live.present_nodes must be restored from committed"
        );
    }

    #[test]
    fn proof_paper_accept_drains_corr_verifier_before_paper_fail_escalation() {
        // Regression guard for the fingerprint re-pin race: when a paper
        // target is in Fail+pinned state but a worker just moved a node's
        // corr fingerprint, the corr verifier must run BEFORE the Reviewer
        // escalation. Otherwise the reviewer can task→Fail-pin the new
        // fingerprint without verifier evidence.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;

        // Paper-target Fail for "t": the pinned-Fail state that triggers
        // escalation in the existing apply_proof_paper_accept Fail-arm.
        state.paper_status.insert("t".into(), CorrStatus::Fail);
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());

        // Stale-Fail corr reopen for "b": raw status is still Fail from
        // an older verifier round, but the current fingerprint moved. This
        // derives current_corr_unknown(b) from current_fp != approved_fp,
        // exactly matching the live re-pin failure.
        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state
            .live
            .corr_current_fingerprints
            .insert("b".into(), "cb-new".into());
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb-old".into());

        // Substantiveness Pass for "a" and "b" so corr_verify_nodes
        // accepts "b" through the per-node substantiveness gate.
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        // Sanity: paper Fail blocker is in current_failed_blockers, paper
        // verifier frontier is empty, corr verifier frontier has "b", and
        // "b" is non-adjudicable.
        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::PaperFaithfulness));
        assert!(state.paper_verify_targets().is_empty());
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));
        assert!(state.corr_verify_nodes().contains(&NodeId::from("b")));

        let approved_before = state
            .corr_approved_fingerprints
            .get(&NodeId::from("b"))
            .cloned();

        let commands = apply_proof_paper_accept(&mut state)
            .expect("paper accept should drain corr verifier instead of escalating");

        // Post-fix: dispatch Corr verifier, NOT Reviewer.
        assert_eq!(state.stage, Stage::VerifyCorr);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(
            kind,
            RequestKind::Corr,
            "fresh non-adjudicable corr Unknown should preempt the paper-Fail escalation"
        );

        // approved_fp on "b" must remain unchanged — the verifier hasn't
        // yet adjudicated the new fingerprint, so the pin contract holds.
        assert_eq!(
            state
                .corr_approved_fingerprints
                .get(&NodeId::from("b"))
                .cloned(),
            approved_before
        );
        // current_corr_state("b") still Unknown (stale Fail + fp drift).
        assert!(state.current_corr_unknown(&NodeId::from("b")));
    }

    #[test]
    fn theorem_paper_accept_drains_corr_verifier_before_paper_fail_escalation() {
        // Mirror of the proof-side test for the theorem-stating path. The
        // bug shape and fix are symmetric across phases; both apply_*_paper_accept
        // Fail-escalation arms call `route_non_adjudicable_unknown_verifier`.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;

        state.paper_status.insert("t".into(), CorrStatus::Fail);
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());

        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");

        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::PaperFaithfulness));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));
        assert!(state.corr_verify_nodes().contains(&NodeId::from("b")));

        let commands = apply_theorem_paper_accept(&mut state)
            .expect("theorem paper accept should drain corr verifier first");

        assert_eq!(state.stage, Stage::VerifyCorr);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(kind, RequestKind::Corr);
        assert!(state.current_corr_unknown(&NodeId::from("b")));
    }

    #[test]
    fn proof_paper_accept_paper_fail_without_unknown_frontier_still_escalates_to_reviewer() {
        // Negative regression: the drain helper must NOT reroute when no
        // verifier frontier has a non-adjudicable Unknown. The escalation
        // to Reviewer is the correct destination when paper has Fail
        // blockers and no fresh evidence is pending.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;

        // Paper-target Fail for "t".
        state.paper_status.insert("t".into(), CorrStatus::Fail);
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());

        // No corr Unknown for "b": leave at base_state's Pass+pinned.
        // No sound Unknown either. paper/substantiveness frontiers also empty.

        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::PaperFaithfulness));
        // No non-adjudicable Unknowns on any verifier frontier.
        assert!(
            !state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr)
                || state.corr_verify_nodes().is_empty()
        );
        assert!(
            !state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness)
                || state.sound_verify_nodes().is_empty()
        );

        let commands = apply_proof_paper_accept(&mut state)
            .expect("paper accept should escalate to Reviewer when no Unknown frontier exists");

        assert_eq!(state.stage, Stage::Reviewer);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(
            kind,
            RequestKind::Review,
            "paper-Fail without fresh non-adjudicable Unknowns must still go to Reviewer"
        );
    }

    #[test]
    fn proof_corr_accept_with_sibling_corr_fail_routes_to_reviewer_not_sound() {
        // Rewritten for the global Sound gate. Under the prior per-node
        // policy `a`'s clean cone made it sound-auto-dispatch-eligible
        // even with sibling `b` at corr=Fail, so `apply_proof_corr_accept`
        // drained Sound before Reviewer escalation. Under the global
        // policy the kernel refuses Sound dispatch while ANY non-sound
        // lane has open work: `b`'s corr-Fail suppresses Sound, and the
        // routing escalates straight to Reviewer.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;

        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());

        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa-new".into());
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa-old".into());
        state.sound_status.insert("a".into(), SoundStatus::Pass);

        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::NodeCorr));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness));
        assert!(
            state.sound_verify_nodes().is_empty(),
            "global gate: b's corr-Fail blocks Sound dispatch even though a's direct-deps cone is clean"
        );

        let commands = apply_proof_corr_accept(&mut state)
            .expect("corr accept should escalate to Reviewer when Sound is gated");

        assert_eq!(state.stage, Stage::Reviewer);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(kind, RequestKind::Review);
    }

    #[test]
    fn theorem_corr_accept_drains_paper_verifier_before_fail_escalation() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;

        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());

        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta-new".into());
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta-old".into());
        state.paper_status.insert("t".into(), CorrStatus::Pass);

        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::NodeCorr));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness));
        assert!(state.paper_verify_targets().contains(&TargetId::from("t")));

        let commands = apply_theorem_corr_accept(&mut state)
            .expect("corr accept should drain paper verifier instead of escalating");

        assert_eq!(state.stage, Stage::VerifyPaper);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(kind, RequestKind::Paper);
        assert!(state.current_paper_unknown(&TargetId::from("t")));
    }

    #[test]
    fn proof_corr_accept_fail_without_unknown_frontier_still_escalates_to_reviewer() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;

        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());

        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::NodeCorr));
        assert!(
            !state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness)
                || state.sound_verify_nodes().is_empty()
        );

        let commands = apply_proof_corr_accept(&mut state)
            .expect("corr accept should still escalate without an Unknown frontier");

        assert_eq!(state.stage, Stage::Reviewer);
        let kind = first_issued_request(&commands).kind;
        assert_eq!(kind, RequestKind::Review);
    }

    #[test]
    fn cleanup_needs_restructure_retries_instead_of_illegal_response() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.attempt = 1;
        state.live.present_nodes = set(&["Preamble", "a", "b"]);
        state.live.open_nodes = set(&["a"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state.node_kinds.insert("a".into(), NodeKind::Proof);
        state.node_kinds.insert("b".into(), NodeKind::Proof);
        state.proof_nodes = set(&["a", "b"]);
        state.deps.insert("a".into(), set(&["Preamble"]));
        state.deps.insert("b".into(), set(&["Preamble"]));
        state.target_claims.insert("a".into(), set(&["t"]));
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::Global,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::NeedsRestructure,
                    snapshot: WorkingSnapshot::default(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("cleanup needs_restructure should be downgraded to invalid retry");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert!(outcome.state.invalid_attempt);
        assert!(outcome
            .state
            .deterministic_worker_rejection_reasons
            .iter()
            .any(|reason| reason.contains("cleanup worker outcome NeedsRestructure")));
        let request = first_issued_request(&outcome.commands);
        assert_eq!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup
        );
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
    }

    #[test]
    fn proof_invalid_escalates_after_threshold() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 2;
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.active_node = Some("a".into());
        // External-audit Finding 2: retry-to-review now routes through
        // `route_after_progress`, which preempts Reviewer dispatch when a
        // non-adjudicable Unknown exists. Substantiveness lane is active
        // in ProofFormalization; pin nodes Pass so the test focuses on
        // the threshold-escalation contract, not the K-1 preemption.
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());
        // Mirror substantiveness fingerprints into the committed snapshot;
        // `reject_proof_worker_response` calls `restore_committed` before
        // the routing helper sees state, and without these the restore
        // would leave the substantiveness fingerprints empty, re-opening
        // the K-1 preemption.
        state.committed.substantiveness_current_fingerprints =
            state.live.substantiveness_current_fingerprints.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let snapshot = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(outcome.state.invalid_attempt);
        assert_eq!(
            first_issued_request(&outcome.commands).kind,
            RequestKind::Review,
        );
    }

    #[test]
    fn proof_review_continue_after_invalid_preserves_retry_context_in_next_worker_request() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.invalid_attempt = true;
        state.attempt = 2;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: state.active_node.clone(),
            mode: TaskMode::Local,
            orphan_cleanup_nodes: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::None,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    comments: "carry the invalid retry marker".into(),
                    next_active: Some("a".into()),
                    next_mode: TaskMode::Local,
                    reset: ResetChoice::None,
                    next_worker_context_mode: WorkerContextMode::Fresh,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply proof review continue after invalid");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(outcome.state.invalid_attempt);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected worker retry request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
        assert_eq!(
            request.worker_context.next_context_mode,
            WorkerContextMode::Fresh
        );
    }

    #[test]
    fn proof_review_continue_after_invalid_cleanup_reschedules_cleanup_worker() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = None;
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.invalid_attempt = true;
        state.attempt = 2;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    comments: "retry orphan cleanup".into(),
                    next_mode: TaskMode::Local,
                    reset: ResetChoice::None,
                    next_worker_context_mode: WorkerContextMode::Fresh,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply proof review continue after invalid cleanup");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(outcome.state.invalid_attempt);
        assert!(outcome
            .state
            .pending_task
            .as_ref()
            .is_some_and(|task| task.orphan_cleanup_nodes == set(&["b"])));
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected cleanup retry request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.active_node, None);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
        assert_eq!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup
        );
        assert_eq!(
            request.worker_context.next_context_mode,
            WorkerContextMode::Resume
        );
    }

    #[test]
    fn cleanup_valid_can_remove_orphan_node() {
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Worker;
        state.cycle = 7;
        // Cleanup invariant: every accepted state must be Done-valid.
        // Close the proof node so the resulting burst keeps the invariant.
        state.live.open_nodes.remove("a");
        // Patch C plan §7.6: `formalization_complete` now requires a
        // `LocalClosureRecord` for every sorry-free proof_node.
        install_placeholder_local_closure_record(&mut state, "a");
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut changed = state.live.clone();
        changed.present_nodes.remove("b");
        changed.open_nodes.remove("b");
        changed.corr_current_fingerprints.remove("b");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 7,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: changed,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.state.live.present_nodes, set(&["a"]));
        assert!(outcome.state.live.open_nodes.is_empty());
        assert!(!outcome
            .state
            .live
            .corr_current_fingerprints
            .contains_key("b"));
        assert!(matches!(
            outcome.commands[0],
            ProtocolCommand::IssueRequest {
                request: WrapperRequest {
                    kind: RequestKind::Review,
                    ..
                }
            }
        ));
    }

    #[test]
    fn cleanup_worker_burst_that_breaks_formalization_complete_is_rejected() {
        // Cleanup invariant: every accepted state in Phase::Cleanup is
        // Done-valid. A worker burst that re-opens a proof node (or
        // re-adds a global blocker) must be rejected; state is reverted
        // to the prior committed state (which by induction is Done-valid).
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Worker;
        state.cycle = 9;
        // Establish a Done-valid baseline: close the proof node "a".
        state.live.open_nodes.remove("a");
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        // Worker proposes to re-open "a" — breaks formalization_complete.
        let mut bad_snapshot = state.live.clone();
        bad_snapshot.open_nodes.insert("a".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 9,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: bad_snapshot,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        // State reverted to committed — "a" is still closed.
        assert!(!outcome.state.live.open_nodes.contains("a"));
        // Rejection reason recorded.
        assert!(outcome
            .state
            .deterministic_worker_rejection_reasons
            .iter()
            .any(|r| r.contains("formalization_complete")));
        // Rejection emits worktree restore (matches existing
        // reject_cleanup_worker_response contract).
        assert!(outcome
            .commands
            .iter()
            .any(|c| matches!(c, ProtocolCommand::RestoreWorktreeToActiveWorkerBase)));
    }

    #[test]
    fn cleanup_worker_burst_rejection_does_not_leak_difficulty_for_phantom_nodes() {
        // Defense against silent state-bloat: a rejected cleanup worker
        // burst that tried to add difficulty for a node it also tried to
        // introduce must not leave a stale node_difficulty entry behind
        // for the (now-discarded) phantom node. The fix moves
        // apply_difficulty_updates after the invariant check.
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Worker;
        state.cycle = 13;
        state.live.open_nodes.remove("a");
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        // Worker proposes a phantom new node "z" + difficulty for it +
        // re-opens "a" (which trips the invariant check).
        let mut bad_snapshot = state.live.clone();
        bad_snapshot.present_nodes.insert("z".into());
        bad_snapshot.open_nodes.insert("a".into());
        let mut difficulty_updates = BTreeMap::new();
        difficulty_updates.insert("z".into(), Update::Set(NodeDifficulty::Easy));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 13,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: bad_snapshot,
                    difficulty_updates,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        // Phantom node "z" gone from state, no stale difficulty entry left.
        assert!(!outcome.state.live.present_nodes.contains("z"));
        assert!(!outcome.state.node_difficulty.contains_key("z"));
    }

    #[test]
    fn cleanup_review_continue_with_reset_blockers_is_illegal() {
        // Cleanup invariant: by the no-blockers rule, the reviewer has
        // no blockers to reset/task/override. Non-empty sets are
        // rejected at legality so they can never reach the apply path
        // (where apply_review_blocker_resets would re-introduce
        // verifier blockers and break the invariant).
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 12;
        state.live.open_nodes.remove("a");
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let bad_review = ReviewResponse {
            request_id: request.id,
            cycle: 12,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::NodeCorr,
                object: BlockerObject::Node { node: "a".into() },
                fingerprint: "ca".into(),
                deferred: false,
            }]),
            next_active: None,
            reset: ResetChoice::None,
            next_mode: TaskMode::Cleanup,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        };

        assert!(!state.review_response_legal(&bad_review));
    }

    #[test]
    fn proof_corr_accept_can_transition_to_cleanup() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::VerifyCorr;
        state.cycle = 8;
        state.live.open_nodes.remove("a");
        state.committed.open_nodes.remove("a");
        // Patch C plan §7.6: `formalization_complete` now requires a
        // `LocalClosureRecord` for every sorry-free proof_node before
        // the ProofFormalization → Cleanup transition fires.
        install_placeholder_local_closure_record(&mut state, "a");
        let request = issue_request_for_test(&mut state, RequestKind::Corr);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Corr(CorrResponse {
                    request_id: request.id,
                    cycle: 8,
                    status: ResponseStatus::Ok,
                    node_lane_updates: empty_corr_node_lanes(&request.verify_lanes),
                    target_lane_updates: empty_corr_target_lanes(&request.verify_lanes),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.phase, Phase::Cleanup);
        assert_eq!(outcome.state.stage, Stage::Start);
        // Paper-target umbrella sync at PF→Cleanup (2026-05-29):
        // `enter_cleanup_phase` emits a SyncTabletRootForPaperTargets
        // ahead of CommitCheckpoint so the umbrella rewrite is part
        // of the same checkpoint diff. `base_state` has no
        // approved_targets seeded, so the resolved umbrella defensively
        // collapses to {Preamble}.
        assert_eq!(
            outcome.commands,
            vec![
                ProtocolCommand::SyncTabletRootForPaperTargets {
                    node_names: BTreeSet::from([NodeId::from("Preamble")]),
                },
                ProtocolCommand::CommitCheckpoint,
            ]
        );
    }

    #[test]
    fn malformed_paper_response_stutters_in_verify_paper() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::VerifyPaper;
        state.cycle = 8;
        let request = issue_request_for_test(&mut state, RequestKind::Paper);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Paper(PaperResponse {
                    request_id: request.id,
                    cycle: 8,
                    status: ResponseStatus::Malformed,
                    ..PaperResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued paper request");
        assert_eq!(request.kind, RequestKind::Paper);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Paper
        ));
    }

    #[test]
    fn malformed_corr_response_stutters_in_verify_corr() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::VerifyCorr;
        state.cycle = 8;
        let request = issue_request_for_test(&mut state, RequestKind::Corr);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Corr(CorrResponse {
                    request_id: request.id,
                    cycle: 8,
                    status: ResponseStatus::Malformed,
                    ..CorrResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyCorr);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued corr request");
        assert_eq!(request.kind, RequestKind::Corr);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Corr
        ));
    }

    #[test]
    fn proof_corr_accept_does_not_route_sound_for_non_proof_active_node() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::VerifyCorr;
        state.cycle = 8;
        state.active_node = Some("b".into());
        let request = issue_request_for_test(&mut state, RequestKind::Corr);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Corr(CorrResponse {
                    request_id: request.id,
                    cycle: 8,
                    status: ResponseStatus::Ok,
                    node_lane_updates: empty_corr_node_lanes(&request.verify_lanes),
                    target_lane_updates: empty_corr_target_lanes(&request.verify_lanes),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.phase, Phase::ProofFormalization);
        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.state.active_node.as_deref(), Some("b"));
        assert_eq!(outcome.commands.len(), 1);
        match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => {
                assert_eq!(request.kind, RequestKind::Review);
                assert_eq!(request.active_node.as_deref(), Some("b"));
            }
            other => panic!("expected review request, got {other:?}"),
        }
    }

    #[test]
    fn human_approve_advance_enters_proof_formalization_start() {
        let mut state = base_state();
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.configured_targets = set(&["t"]);
        state.gate_kind = GateKind::Advance;
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ct-live-node".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ct-live-node".into());
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.phase, Phase::ProofFormalization);
        assert_eq!(outcome.state.stage, Stage::Start);
        assert_eq!(
            outcome.state.approved_targets.configured_targets,
            set(&["t"])
        );
        assert_eq!(
            outcome
                .state
                .paper_approved_fingerprints
                .get("t")
                .map(String::as_str),
            Some("a=ct-live-node")
        );
        // Phase entry leaves both `active_node` and `active_coarse_node`
        // as None: the post-advance routing Review (issued by the next
        // `start_cycle` because `post_advance_routing_pending=true`) is
        // responsible for choosing `next_active` and `next_active_coarse`
        // with full latitude over all open coarse-DAG nodes. Pre-seeding
        // the anchor here would narrow `kernel_hinted_next_active_nodes`
        // to the anchor's cone AND empty out
        // `kernel_hinted_next_active_coarse_nodes` (anchor locked), defeating
        // the routing Review's purpose. `30b_coarse_anchor.md` documents
        // the `active_coarse_node = None` state as legitimate: the kernel
        // surfaces every open coarse-DAG node and the reviewer's Continue
        // must set `next_active_coarse`.
        assert_eq!(
            outcome.state.active_node, None,
            "phase entry must leave active_node = None for the routing Review to choose"
        );
        assert_eq!(
            outcome.state.active_coarse_node, None,
            "phase entry must leave active_coarse_node = None for the routing Review to choose"
        );
        // Audit Finding 5: human-approved phase advance emits
        // CommitCheckpoint so the git/audit boundary records the phase
        // transition (state.phase is the only persistent record of which
        // phase we're in; without a checkpoint commit, operators can't
        // see "this commit is when we entered ProofFormalization").
        assert_eq!(
            outcome.commands,
            vec![ProtocolCommand::CommitCheckpoint],
            "human-approved phase advance must emit CommitCheckpoint"
        );
        // Phase-startup routing handoff: the next StartCycle must issue a
        // Review (not a Worker), so the reviewer chooses next_active,
        // must_close_active, allow_new_obligations, etc. for the first
        // burst of ProofFormalization.
        assert!(
            outcome.state.post_advance_routing_pending,
            "human-approved phase advance must set post_advance_routing_pending"
        );
    }

    #[test]
    fn human_approve_advance_issues_review_not_worker() {
        // Phase-startup routing: after a human-approved phase advance
        // into ProofFormalization, the very next StartCycle must dispatch
        // a Review request (not a Worker). The reviewer's allowed_decisions
        // is `{Continue, NeedInput}` here (no AdvancePhase — see
        // `request_allowed_decisions`'s ProofFormalization arm); the
        // reviewer is expected to choose `next_active`, `next_mode`,
        // `must_close_active`, `allow_new_obligations`, etc. for the first
        // burst of the new phase.
        let mut state = base_state();
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.configured_targets = set(&["t"]);
        state.gate_kind = GateKind::Advance;
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ct-live-node".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ct-live-node".into());
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let approve_outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();
        assert_eq!(approve_outcome.state.phase, Phase::ProofFormalization);
        assert_eq!(approve_outcome.state.stage, Stage::Start);
        assert!(approve_outcome.state.post_advance_routing_pending);

        let started = apply_event(approve_outcome.state, ProtocolEvent::StartCycle).unwrap();
        assert_eq!(started.state.stage, Stage::Reviewer);
        assert!(
            started.state.post_advance_routing_pending,
            "latch persists through the in-flight routing Review so the \
             in-flight invariant check keeps holding; it clears in \
             apply_proof_review_response when the response is applied"
        );
        let request = started
            .state
            .in_flight_request
            .as_ref()
            .expect("post-advance StartCycle must issue a request");
        assert_eq!(
            request.kind,
            RequestKind::Review,
            "post-advance StartCycle must issue Review, not Worker"
        );
        assert!(
            request.post_advance_routing,
            "the routing Review's WrapperRequest must carry post_advance_routing=true"
        );
        // The whole point of leaving `active_coarse_node = None` at
        // phase entry is so the routing Review has full latitude.
        // Verify the kernel surfaces every open coarse-DAG node as a
        // candidate (per `30b_coarse_anchor.md`), and that the
        // active-node hint is not narrowed to a single anchor's cone.
        assert!(
            !request.kernel_hinted_next_active_coarse_nodes.is_empty(),
            "post-advance routing Review must surface non-empty \
             kernel_hinted_next_active_coarse_nodes so the reviewer can \
             pick `next_active_coarse` from all open coarse nodes; got empty",
        );
        assert!(
            !request.kernel_hinted_next_active_nodes.is_empty(),
            "post-advance routing Review must surface non-empty \
             kernel_hinted_next_active_nodes; pre-seeding an anchor would \
             have narrowed this to a cone and defeated routing",
        );
    }

    #[test]
    fn post_advance_review_does_not_allow_advance_phase() {
        // The post-advance routing Review must NOT permit AdvancePhase
        // (the operator just signed off on advancing — re-advancing here
        // would be nonsense). Continue is required so the reviewer can
        // route the first worker burst; NeedInput is allowed so escalation
        // is still possible. AdvancePhase must be excluded.
        let mut state = base_state();
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.configured_targets = set(&["t"]);
        state.gate_kind = GateKind::Advance;
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ct-live-node".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ct-live-node".into());
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let approve_outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();
        let started = apply_event(approve_outcome.state, ProtocolEvent::StartCycle).unwrap();
        let request = started
            .state
            .in_flight_request
            .as_ref()
            .expect("post-advance StartCycle must issue a request");
        assert_eq!(request.kind, RequestKind::Review);
        assert!(
            !request
                .allowed_decisions
                .contains(&ReviewDecisionKind::AdvancePhase),
            "post-advance routing Review must NOT allow AdvancePhase"
        );
        assert!(
            request
                .allowed_decisions
                .contains(&ReviewDecisionKind::Continue),
            "post-advance routing Review must allow Continue so the reviewer can route the first worker burst"
        );
    }

    #[test]
    fn post_advance_review_response_routes_worker_normally() {
        // Phase-startup routing end-to-end: the reviewer's Continue with
        // `next_active=Some(...)`, `must_close_active=true`,
        // `allow_new_obligations=false` must seed the next worker burst
        // with those flags exactly — the standard `apply_proof_review_response`
        // routing path handles this once `post_advance_routing_pending`
        // has fired the Review on phase entry.
        let mut state = base_state();
        // Drop "b" — base_state seeds it as a present but un-covered
        // sibling, which `orphan_cleanup_needed` would otherwise pick up
        // on the post-Review StartCycle, overwriting the reviewer's
        // pending_task with an orphan-cleanup task. The post-advance
        // routing semantics we want to verify are about the reviewer's
        // flags flowing into the dispatched worker — orphan cleanup is a
        // separate pre-emption path tested elsewhere.
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.configured_targets = set(&["t"]);
        state.gate_kind = GateKind::Advance;
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ct-live-node".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ct-live-node".into());
        // Keep committed in sync with the trimmed live so the
        // validate() post-condition holds.
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let approve_outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();

        let started = apply_event(approve_outcome.state, ProtocolEvent::StartCycle).unwrap();
        let review_request = started
            .state
            .in_flight_request
            .as_ref()
            .expect("post-advance StartCycle must issue a request")
            .clone();
        assert_eq!(review_request.kind, RequestKind::Review);
        let cycle_at_review = started.state.cycle;

        // Reviewer routes the first worker burst with explicit
        // `must_close_active=true, allow_new_obligations=false` (the
        // strict combo project memory prefers over the kernel default).
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response_outcome = apply_event(
            started.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: review_request.id,
                    cycle: cycle_at_review,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    // Pin to the kernel-seeded initial active node "a" —
                    // by base_state, only "a" is a `proof_node` and the
                    // single configured target's coverage, so it's the
                    // sole legal next_active candidate post-Approve.
                    next_active: Some("a".into()),
                    next_mode: TaskMode::Local,
                    must_close_active: true,
                    allow_new_obligations: false,
                    paper_focus_ranges,
                    paper_grounding,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply post-advance routing review");

        // Reviewer-Continue lands the state at Stage::Start with the
        // reviewer's `pending_task` seeded (`apply_proof_review_response`
        // installs node/must_close_active/allow_new_obligations into
        // `state.pending_task`). The next StartCycle dispatches the
        // Worker. Drive that next cycle and assert the dispatched worker
        // request carries the reviewer's explicit flags.
        assert_eq!(response_outcome.state.stage, Stage::Start);
        // Validate the reviewer's pending_task survived through the Continue.
        let pending = response_outcome
            .state
            .pending_task
            .as_ref()
            .expect("reviewer Continue must seed pending_task");
        assert!(
            pending.must_close_active,
            "reviewer's must_close_active must land in pending_task (got pending_task={:?})",
            pending,
        );
        assert!(
            !pending.allow_new_obligations,
            "reviewer's allow_new_obligations must land in pending_task"
        );
        let worker_started = apply_event(response_outcome.state, ProtocolEvent::StartCycle)
            .expect("StartCycle after post-advance review should dispatch worker");
        assert_eq!(worker_started.state.stage, Stage::Worker);
        let worker_request = worker_started
            .commands
            .iter()
            .find_map(|c| match c {
                ProtocolCommand::IssueRequest { request }
                    if request.kind == RequestKind::Worker =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("post-advance routing Review Continue must dispatch a Worker request");
        assert_eq!(worker_request.active_node.as_deref(), Some("a"));
        assert!(
            worker_request.worker_context.must_close_active,
            "reviewer's must_close_active must propagate to the dispatched worker"
        );
        assert!(
            !worker_request.worker_context.allow_new_obligations,
            "reviewer's allow_new_obligations must propagate to the dispatched worker"
        );
    }

    #[test]
    fn human_approve_advance_snapshots_protected_closure_from_live_per_target_slot() {
        // Audit follow-up (2026-05-03 protected-closure widening): the
        // live observation slot `protected_closure_nodes_per_target` is
        // populated by worker bursts (see
        // `runtime_cli.rs:populate_response_fingerprints`). At
        // AdvancePhase Approve the engine must (a) union the per-target
        // closures, (b) drop Preamble defensively (the observation
        // helper already filters but engine belt-and-braces it), and
        // (c) drop nodes already in `coverage` (those live in
        // `approved_targets.coverage`, not in `protected_closure_nodes`).
        let mut state = base_state();
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.configured_targets = set(&["t1", "t2"]);
        state.gate_kind = GateKind::Advance;
        // Coverage requires matching target_claims for the validate()
        // post-condition that runs at the end of apply_event ("live
        // coverage must be derived from target claims"). Set both
        // sides for each covering node.
        state.live.present_nodes.insert("Cov1".into());
        state.live.present_nodes.insert("Cov2".into());
        state.target_claims.insert("Cov1".into(), set(&["t1"]));
        state.target_claims.insert("Cov2".into(), set(&["t2"]));
        state
            .live
            .target_fingerprints
            .insert("Cov1".into(), "fp-cov1".into());
        state
            .live
            .target_fingerprints
            .insert("Cov2".into(), "fp-cov2".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t1".into(), "Cov1=fp-cov1".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t2".into(), "Cov2=fp-cov2".into());
        state.live.coverage.insert("t1".into(), set(&["Cov1"]));
        state.live.coverage.insert("t2".into(), set(&["Cov2"]));
        // Closure populated by the most recent worker observation.
        // - t1's closure: Helper, Shared
        // - t2's closure: Shared, Cov1 (already in coverage), Preamble (defensive drop)
        // Expected post-Approve set: {Helper, Shared}.
        state
            .live
            .protected_closure_nodes_per_target
            .insert("t1".into(), set(&["Helper", "Shared"]));
        state
            .live
            .protected_closure_nodes_per_target
            .insert("t2".into(), set(&["Shared", "Cov1", "Preamble"]));
        // Mirror committed = live so the validate() post-condition
        // ("committed coverage must be derived from target claims")
        // passes — the existing test infra normally relies on
        // `commit_live()` having run on a prior cycle, but here we're
        // hand-building an HumanGate state without prior cycles.
        state.committed = state.live.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.phase, Phase::ProofFormalization);
        assert_eq!(
            outcome.state.approved_targets.coverage.get("t1"),
            Some(&set(&["Cov1"]))
        );
        assert_eq!(
            outcome.state.approved_targets.coverage.get("t2"),
            Some(&set(&["Cov2"]))
        );
        assert_eq!(
            outcome.state.approved_targets.protected_closure_nodes,
            set(&["Helper", "Shared"]),
            "engine should snapshot the closure union, drop covering nodes (Cov1) and Preamble"
        );
        // The widened protection set surfaces through approved_target_nodes()
        // — the worker-acceptance reopen guard reads this set.
        assert_eq!(
            outcome.state.approved_target_nodes(),
            set(&["Cov1", "Cov2", "Helper", "Shared"]),
            "approved_target_nodes() must be coverage union the protected closure"
        );
    }

    #[test]
    fn approved_target_nodes_returns_just_coverage_when_closure_empty() {
        // Backward-compat regression: legacy state files (or
        // pre-AdvancePhase fresh state) have an empty
        // `protected_closure_nodes` set and `approved_target_nodes()`
        // must keep returning just the coverage union — equivalent to
        // the pre-2026-05-03 worker protection scope.
        let mut state = base_state();
        state.approved_targets.configured_targets = set(&["t"]);
        state
            .approved_targets
            .coverage
            .insert("t".into(), set(&["Cov"]));
        // protected_closure_nodes intentionally left empty.
        assert_eq!(state.approved_target_nodes(), set(&["Cov"]));
    }

    #[test]
    fn approved_target_nodes_unions_coverage_and_protected_closure() {
        let mut state = base_state();
        state.approved_targets.configured_targets = set(&["t"]);
        state
            .approved_targets
            .coverage
            .insert("t".into(), set(&["Cov"]));
        state.approved_targets.protected_closure_nodes = set(&["Helper", "Shared"]);
        assert_eq!(
            state.approved_target_nodes(),
            set(&["Cov", "Helper", "Shared"]),
            "the closure descendants extend the protection set; cov stays"
        );
    }

    #[test]
    fn malformed_human_gate_response_stutters_in_human_gate() {
        let mut state = base_state();
        state.stage = Stage::HumanGate;
        state.cycle = 10;
        state.gate_kind = GateKind::NeedInput;
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 10,
                    status: ResponseStatus::Malformed,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::HumanGate);
        assert_eq!(outcome.state.gate_kind, GateKind::NeedInput);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued human gate request");
        assert_eq!(request.kind, RequestKind::HumanGate);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::HumanGate
        ));
    }

    #[test]
    fn cleanup_done_completes_the_protocol() {
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 11;
        // Cleanup invariant: Done-validity requires no open proof nodes.
        state.live.open_nodes.remove("a");
        // Patch C plan §7.6: `formalization_complete` now requires a
        // `LocalClosureRecord` for every sorry-free proof_node.
        install_placeholder_local_closure_record(&mut state, "a");
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 11,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Done,
                    comments: String::new(),
                    task_blockers: BTreeSet::new(),
                    override_blockers: BTreeSet::new(),
                    reset_blockers: BTreeSet::new(),
                    next_active: None,
                    reset: ResetChoice::None,
                    next_mode: TaskMode::Cleanup,
                    difficulty_updates: BTreeMap::new(),
                    clear_human_input: false,
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.phase, Phase::Complete);
        assert_eq!(outcome.state.stage, Stage::Complete);
        assert_eq!(outcome.commands, vec![ProtocolCommand::CommitCheckpoint]);
    }

    #[test]
    fn cleanup_start_cycle_routes_to_review_when_audit_ran_but_no_active_or_pending_task() {
        // Defense-in-depth (audit follow-up): the legitimate cleanup-v2
        // control flow re-issues subsequent audit bursts from
        // `apply_audit_response` and drives worker dispatch from the
        // reviewer's Continue, so `start_cycle` is normally only reached
        // in Cleanup with `cleanup_audit_burst_count == 0` (round-entry
        // first audit). State load from disk, recovery paths, or future
        // code changes can reach `start_cycle` with `burst_count > 0,
        // active_task = None, pending_task = None` — pre-fix this fell
        // through to `RequestKind::Worker`, emitting an empty-pending-
        // task Worker request (contradicting cleanup-v2 design).
        // Post-fix `start_cycle` routes this case to Reviewer so the
        // reviewer can choose Continue (next dispatch from a Pending
        // task) or Done (advances to Phase::Complete).
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Start;
        state.cycle = 7;
        // Cleanup invariant: state must be Done-valid at every Cleanup
        // boundary. Close the proof node and install the closure record.
        state.live.open_nodes.remove("a");
        install_placeholder_local_closure_record(&mut state, "a");
        // Eliminate the orphan-cleanup precondition: "b" in base_state
        // is present but doesn't claim any target (coverage["t"]={"a"}),
        // so `orphan_cleanup_needed()` would fire and short-circuit
        // start_cycle to a Worker dispatch before our explicit routing
        // sees the state. Remove "b" from the live snapshot entirely.
        state.live.present_nodes.remove("b");
        state.live.open_nodes.remove("b");
        state.live.corr_current_fingerprints.remove("b");
        state.live.sound_current_fingerprints.remove("b");
        state.live.substantiveness_current_fingerprints.remove("b");
        state.corr_status.remove("b");
        state.corr_approved_fingerprints.remove("b");
        state.sound_status.remove("b");
        state.sound_approved_fingerprints.remove("b");
        state.substantiveness_status.remove("b");
        state.substantiveness_approved_fingerprints.remove("b");
        state.committed = state.live.clone();
        // The setup we're targeting: audit has run, no active or pending
        // worker task. Pre-fix this would route to Worker; post-fix it
        // routes to Reviewer.
        state.cleanup_audit_burst_count = 1;
        state.cleanup_active_task = None;
        state.pending_task = None;

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.commands.len(), 1);
        assert!(
            matches!(
                outcome.commands[0],
                ProtocolCommand::IssueRequest {
                    request: WrapperRequest {
                        kind: RequestKind::Review,
                        ..
                    }
                },
            ),
            "Cleanup + no-active + no-pending + burst_count>0 must route to \
             Reviewer (defense-in-depth — not the legacy Worker fallback); \
             got {:?}",
            outcome.commands[0],
        );
    }

    #[test]
    fn cleanup_start_cycle_routes_to_worker_when_pending_task_set() {
        // Companion to `cleanup_start_cycle_routes_to_review_when_audit_ran_
        // but_no_active_or_pending_task`: the explicit per-case routing
        // must preserve the legitimate Worker case — Cleanup with a
        // pending worker task (dispatched by the reviewer's Continue at
        // a prior cycle boundary) still routes to Worker even when
        // `cleanup_active_task` is None (the active-task tracker is set
        // by `apply_cleanup_review_response`'s sub-case A alongside the
        // pending_task; this test pins the routing on `pending_task`
        // alone to avoid coupling to the active-task tracker semantics).
        let mut state = base_state();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Start;
        state.cycle = 7;
        state.live.open_nodes.remove("a");
        install_placeholder_local_closure_record(&mut state, "a");
        // Eliminate the orphan-cleanup precondition (see sibling test).
        state.live.present_nodes.remove("b");
        state.live.open_nodes.remove("b");
        state.live.corr_current_fingerprints.remove("b");
        state.live.sound_current_fingerprints.remove("b");
        state.live.substantiveness_current_fingerprints.remove("b");
        state.corr_status.remove("b");
        state.corr_approved_fingerprints.remove("b");
        state.sound_status.remove("b");
        state.sound_approved_fingerprints.remove("b");
        state.substantiveness_status.remove("b");
        state.substantiveness_approved_fingerprints.remove("b");
        state.committed = state.live.clone();
        state.cleanup_audit_burst_count = 1;
        state.cleanup_active_task = None;
        // PendingTask invariant: pending_task.node must match active_node.
        state.active_node = Some("a".into());
        // Same shape as above EXCEPT a pending_task is set. Must route
        // to Worker (the explicit-routing branch preserves this case).
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: Some("a".into()),
            mode: TaskMode::Cleanup,
            orphan_cleanup_nodes: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::default(),
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::default(),
        consumed_global_repair_grant: false,
        });

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.commands.len(), 1);
        assert!(
            matches!(
                outcome.commands[0],
                ProtocolCommand::IssueRequest {
                    request: WrapperRequest {
                        kind: RequestKind::Worker,
                        ..
                    }
                },
            ),
            "Cleanup + pending_task set must still route to Worker (the \
             explicit-routing branch must preserve this legitimate case); \
             got {:?}",
            outcome.commands[0],
        );
    }

    #[test]
    fn proof_start_request_uses_node_difficulty_profile() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state.active_node = Some("a".into());
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        state.committed = state.live.clone();
        state
            .node_difficulty
            .insert("a".into(), NodeDifficulty::Easy);
        state.easy_attempts.insert("a".into(), 1);
        // Seed substantiveness Pass so proof_start_request_kind falls
        // through to Worker (the dispatch surface this test exercises).
        // base_state() leaves substantiveness empty (= Unknown); without
        // this seed, proof-phase StartCycle would correctly route to
        // Paper for the substantiveness frontier (covered by
        // proof_start_with_substantiveness_unknown_dispatches_paper below).
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub_a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub_a".into());
        state.committed = state.live.clone();
        let configured_targets = state.configured_targets.clone();

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected worker request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Worker);
        let worker_ctx = &request.worker_context;
        assert_eq!(request.configured_targets, configured_targets);
        assert_eq!(worker_ctx.active_difficulty, NodeDifficulty::Easy);
        assert_eq!(worker_ctx.active_easy_attempts, 1);
        assert_eq!(worker_ctx.worker_profile, WorkerProfile::ProofEasy);
        assert_eq!(worker_ctx.validation_kind, WorkerValidationKind::ProofLocal);
        assert_eq!(worker_ctx.authorized_nodes, BTreeSet::new());
        assert!(worker_ctx.allow_new_obligations);
        assert!(!worker_ctx.must_close_active);
        assert!(worker_ctx.enabled);
    }

    /// Pins the proof-phase symmetry: when StartCycle fires in
    /// ProofFormalization with a non-empty substantiveness frontier,
    /// dispatch goes to `RequestKind::Paper` at `Stage::VerifyPaper`,
    /// not the legacy unconditional Worker. Without this assertion a
    /// future refactor could silently regress to the pre-symmetry
    /// behavior (one wasted worker dispatch per cycle on resume from
    /// disk, cleanup-cycle wrap, etc.).
    #[test]
    fn proof_start_with_substantiveness_unknown_dispatches_paper() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state.active_node = Some("a".into());
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        // Clear the clean base fixture so a is Unknown, which is the
        // scenario this regression wants to exercise.
        clear_substantiveness(&mut state, "a");
        state.committed = state.live.clone();
        state
            .node_difficulty
            .insert("a".into(), NodeDifficulty::Easy);

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected paper (substantiveness) request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Paper);
        assert!(request.paper_verify_targets.is_empty());
        assert_eq!(request.substantiveness_verify_nodes, set(&["a"]));
    }

    /// E2E coverage for the `pending_task.task_blockers # {}` early-return
    /// in `proof_start_request_kind` (model.rs:3128-3134). When a reviewer
    /// has staged a worker assignment via `pending_task.task_blockers`, the
    /// preemption rule states that the worker must dispatch first even if
    /// verifier lanes (substantiveness here) are non-empty. Without this
    /// rule, a co-occurring `reset_blocker_ids` would route to a verifier
    /// first, the kernel would clear the pending_task on issue, and the
    /// worker assignment would silently disappear.
    ///
    /// The spec patch in `f6e5c22` mirrors this kernel rule into
    /// `StartCycle` (spec/SupervisorProtocol.tla:4313+); this test pins the
    /// kernel side so a future refactor can't silently re-introduce the
    /// disappearing-worker-assignment bug.
    #[test]
    fn proof_start_pending_task_blockers_preempts_substantiveness_unknown() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state.active_node = Some("a".into());
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        // Restructure mode is required for non-empty task_blockers in
        // proof phase (Local + task_blockers is rejected by the
        // legality gate at `WrapperRequest::review_response_legal`).
        state.proof_edit_mode = ProofEditMode::Restructure;
        // Clear the clean base fixture so a is Unknown. Without
        // preemption this would route to RequestKind::Paper.
        clear_substantiveness(&mut state, "a");
        state.committed = state.live.clone();
        state
            .node_difficulty
            .insert("a".into(), NodeDifficulty::Easy);
        // The blocker placed in pending_task.task_blockers must be a
        // subset of global_blockers() (kernel invariant: an issue-time
        // check fires "pending task blockers must be a subset of global
        // blockers"). Substantiveness Unknown on "a" produces a
        // Substantiveness blocker derivatively (model.rs:2539-2550),
        // which is exactly what the reviewer would stage on the
        // (substantiveness-Unknown, task-staged-worker) scenario. Use
        // that derived blocker so the test pins the correct path.
        let blocker = Blocker {
            kind: BlockerKind::Substantiveness,
            object: BlockerObject::Node {
                node: NodeId::from("a"),
            },
            fingerprint: String::new(),
            deferred: false,
        };
        assert!(
            state.global_blockers().contains(&blocker),
            "test sanity: derived substantiveness blocker must be in global_blockers"
        );
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::from([blocker.clone()]),
            node: Some("a".into()),
            mode: TaskMode::Restructure,
            ..PendingTask::default()
        });

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected worker request, got {:?}", other),
        };
        assert_eq!(
            request.kind,
            RequestKind::Worker,
            "task_blockers must preempt substantiveness frontier"
        );
        assert_eq!(outcome.state.stage, Stage::Worker);
    }

    /// Theorem-phase mirror of the proof-phase test above. Pins
    /// `theorem_start_request_kind` (model.rs:3088-3092) — same preemption
    /// rule, same scenario (substantiveness Unknown for "a"). Together
    /// these two tests document that the symmetric-StartCycle commit
    /// (`0f37014`) preserves task-blocker preemption parity across phases.
    #[test]
    fn theorem_start_pending_task_blockers_preempts_substantiveness_unknown() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Start;
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        clear_substantiveness(&mut state, "a");
        state.committed = state.live.clone();
        // Same derived-blocker pattern as the proof-phase analog.
        let blocker = Blocker {
            kind: BlockerKind::Substantiveness,
            object: BlockerObject::Node {
                node: NodeId::from("a"),
            },
            fingerprint: String::new(),
            deferred: false,
        };
        assert!(
            state.global_blockers().contains(&blocker),
            "test sanity: derived substantiveness blocker must be in global_blockers"
        );
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::from([blocker.clone()]),
            node: None,
            mode: TaskMode::Global,
            ..PendingTask::default()
        });

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected worker request, got {:?}", other),
        };
        assert_eq!(
            request.kind,
            RequestKind::Worker,
            "theorem-phase task_blockers must preempt substantiveness frontier"
        );
        assert_eq!(outcome.state.stage, Stage::Worker);
    }

    #[test]
    fn proof_invalid_easy_node_auto_escalates_to_hard() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 12;
        state.attempt = 1;
        state.easy_max_retries = 2;
        state.active_node = Some("a".into());
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        state.committed = state.live.clone();
        state
            .node_difficulty
            .insert("a".into(), NodeDifficulty::Easy);
        state.easy_attempts.insert("a".into(), 1);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 12,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: base_state().live,
                    difficulty_updates: BTreeMap::new(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.state.node_difficulty.get("a"),
            Some(&NodeDifficulty::Hard)
        );
        assert_eq!(outcome.state.easy_attempts.get("a"), Some(&0));
        let request = first_issued_request(&outcome.commands);
        let worker_ctx = &request.worker_context;
        assert_eq!(worker_ctx.worker_profile, WorkerProfile::ProofHard);
        assert_eq!(worker_ctx.validation_kind, WorkerValidationKind::ProofLocal);
        assert_eq!(worker_ctx.active_difficulty, NodeDifficulty::Hard);
        assert_eq!(worker_ctx.active_easy_attempts, 0);
        assert_eq!(worker_ctx.authorized_nodes, BTreeSet::new());
        assert!(worker_ctx.enabled);
    }

    #[test]
    fn proof_local_invalid_worker_retry_uses_fresh_context() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 12;
        state.attempt = 1;
        state.active_node = Some("a".into());
        state.live.present_nodes = set(&["a"]);
        state.live.open_nodes = set(&["a"]);
        state.committed = state.live.clone();
        state
            .node_difficulty
            .insert("a".into(), NodeDifficulty::Hard);
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: state.active_node.clone(),
            mode: TaskMode::Local,
            orphan_cleanup_nodes: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::None,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 12,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: base_state().live,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        let request = first_issued_request(&outcome.commands);
        let worker_ctx = &request.worker_context;
        assert_eq!(worker_ctx.validation_kind, WorkerValidationKind::ProofLocal);
        assert_eq!(worker_ctx.next_context_mode, WorkerContextMode::Fresh);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
    }

    #[test]
    fn theorem_cleanup_accept_resumes_paper_verification() {
        // K-1 regression: theorem-stating orphan-cleanup Valid must route
        // via the paper-accept choke point (`apply_theorem_paper_accept`),
        // not straight to `Stage::Reviewer`. Pre-fix, the fall-through
        // landed the cycle at Reviewer with structural-Unknown blockers
        // and no verifier evidence; the blocker-action contract in
        // `review_response_legal` then left task→Fail as the only
        // meaningful action on every Unknown blocker (no verifier
        // evidence to override, not a current Fail to reset), pinning
        // `status=Fail + approved_fp=current_fp` and starving verifier
        // dispatch on the next cycle (since `theorem_start_request_kind`
        // reads current==approved as "verifier ran"). Mirrors
        // `proof_cleanup_accept_resumes_paper_verification`.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = None;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Cleanup attaches the orphan and the post-cleanup paper-target
        // fingerprint drifts off the approval baseline, so
        // `paper_verify_targets()` returns "t" — the verifier-dispatch
        // path should fire.
        let mut cleaned = state.live.clone();
        cleaned.present_nodes = set(&["a"]);
        cleaned.open_nodes = BTreeSet::new();
        cleaned.coverage.insert("t".into(), set(&["a"]));
        cleaned
            .paper_current_fingerprints
            .insert("t".into(), "a=ta+helper".into());
        cleaned.corr_current_fingerprints.remove("b");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: cleaned,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected paper verification request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Paper);
        assert_eq!(request.phase, Phase::TheoremStating);
        assert_eq!(request.paper_verify_targets, set(&["t"]));
    }

    #[test]
    fn proof_cleanup_accept_resumes_paper_verification() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 12;
        state.active_node = None;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut cleaned = state.live.clone();
        cleaned.present_nodes = set(&["a"]);
        cleaned.open_nodes = BTreeSet::new();
        cleaned.coverage.insert("t".into(), set(&["a"]));
        cleaned
            .paper_current_fingerprints
            .insert("t".into(), "a=ta+helper".into());
        cleaned.corr_current_fingerprints.remove("b");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 12,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: cleaned,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let request = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected paper verification request, got {:?}", other),
        };
        assert_eq!(request.kind, RequestKind::Paper);
        assert_eq!(request.phase, Phase::ProofFormalization);
        assert_eq!(request.paper_verify_targets, set(&["t"]));
    }

    #[test]
    fn theorem_cleanup_accept_with_no_verifier_evidence_dispatches_paper() {
        // K-1 corrected fix: post-cleanup-Valid in TheoremStating, with
        // `latest_*_review_*` cleared (cleanup unconditionally clears
        // them at engine.rs ~885), every Unknown blocker is
        // *non-adjudicable* — its object is not in any
        // `latest_*_review_*`. `route_after_progress` must preempt the
        // Reviewer dispatch with a Paper verifier (paper has the highest
        // priority and a non-empty frontier here).
        //
        // End-to-end via `apply_event`; mirrors the e6320f6 K-1
        // regression but explicitly asserts that
        // `latest_paper_review_targets` is empty post-cleanup (the
        // condition that makes Unknowns non-adjudicable).
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = None;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut cleaned = state.live.clone();
        cleaned.present_nodes = set(&["a"]);
        cleaned.open_nodes = BTreeSet::new();
        cleaned.coverage.insert("t".into(), set(&["a"]));
        cleaned
            .paper_current_fingerprints
            .insert("t".into(), "a=ta+helper".into());
        cleaned.corr_current_fingerprints.remove("b");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: cleaned,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        // Cleanup cleared `latest_paper_review_targets` ⇒ paper Unknown
        // is non-adjudicable ⇒ route_after_progress preempts Reviewer.
        assert!(outcome.state.latest_paper_review_targets.is_empty());
        assert!(outcome.state.latest_corr_review_nodes.is_empty());
        assert!(outcome.state.latest_substantiveness_review_nodes.is_empty());
        assert!(outcome.state.latest_sound_review_nodes.is_empty());
        assert!(outcome
            .state
            .has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness));
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let issued = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected paper verification request, got {:?}", other),
        };
        assert_eq!(issued.kind, RequestKind::Paper);
        assert_eq!(issued.phase, Phase::TheoremStating);
        assert_eq!(issued.paper_verify_targets, set(&["t"]));
    }

    #[test]
    fn proof_cleanup_accept_with_no_verifier_evidence_dispatches_paper() {
        // K-1 corrected fix, proof-side analog of
        // `theorem_cleanup_accept_with_no_verifier_evidence_dispatches_paper`.
        // Post-cleanup-Valid in ProofFormalization with all
        // `latest_*_review_*` cleared ⇒ all Unknowns non-adjudicable ⇒
        // `route_after_progress` preempts Reviewer dispatch with Paper.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 12;
        state.active_node = None;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::CoarseRestructure,
            orphan_cleanup_nodes: set(&["b"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::Restructure,
        consumed_global_repair_grant: false,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut cleaned = state.live.clone();
        cleaned.present_nodes = set(&["a"]);
        cleaned.open_nodes = BTreeSet::new();
        cleaned.coverage.insert("t".into(), set(&["a"]));
        cleaned
            .paper_current_fingerprints
            .insert("t".into(), "a=ta+helper".into());
        cleaned.corr_current_fingerprints.remove("b");

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 12,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: cleaned,
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        assert!(outcome.state.latest_paper_review_targets.is_empty());
        assert!(outcome
            .state
            .has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness));
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let issued = match &outcome.commands[0] {
            ProtocolCommand::IssueRequest { request } => request,
            other => panic!("expected paper verification request, got {:?}", other),
        };
        assert_eq!(issued.kind, RequestKind::Paper);
        assert_eq!(issued.phase, Phase::ProofFormalization);
        assert_eq!(issued.paper_verify_targets, set(&["t"]));
    }

    #[test]
    fn route_after_progress_with_adjudicable_corr_unknowns_dispatches_reviewer() {
        // Corrected K-1 invariant (per the audit on commit 0d9db6d's
        // wider K-2 attempt): Reviewer dispatch with Unknown blockers IS
        // legitimate when those Unknowns are in `latest_*_review_*` —
        // `review_blocker_adjudicable` (model.rs:3779) endorses
        // `latest_corr_review_nodes.contains(node) &&
        // current_corr_unknown(node)` as the override-adjudication
        // pathway, and `apply_review_blocker_adjudication` (model.rs:3799)
        // explicitly endorses task→Fail of Unknowns when they are
        // adjudicable.
        //
        // So `route_after_progress` MUST NOT preempt verifier dispatch
        // when all Unknown blockers are adjudicable. That's the wider
        // K-2 invariant the previous attempt got wrong; this test pins
        // the corrected behaviour.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        // Corr lane: drift "a"'s corr fingerprint off the approval
        // baseline so `current_corr_unknown("a")` is true and "a" appears
        // in `corr_verify_nodes()`. The verifier just ran on it (per
        // `latest_corr_review_nodes`), so the reviewer can override it.
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca-drifted".into());
        state.latest_corr_review_nodes = set(&["a"]);
        // Substantiveness needs to be Pass so corr_verify_nodes admits "a".
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        // Sanity: there's a corr Unknown blocker for "a", and it IS
        // adjudicable (review_blocker_adjudicable=true), hence not
        // non-adjudicable.
        assert!(state.corr_verify_nodes().contains(&NodeId::from("a")));
        assert!(!state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));

        // No paper drift, no sound drift, no substantiveness drift — the
        // only Unknown blocker is the adjudicable corr one. Therefore
        // route_after_progress must dispatch Reviewer.
        let commands = route_after_progress(&mut state);
        assert_eq!(state.stage, Stage::Reviewer);
        let kind = match &commands[0] {
            ProtocolCommand::IssueRequest { request } => request.kind,
            other => panic!("expected review request, got {:?}", other),
        };
        assert_eq!(kind, RequestKind::Review);
    }

    #[test]
    fn route_after_progress_with_non_adjudicable_corr_unknowns_dispatches_corr() {
        // Symmetric to the previous test but with
        // `latest_corr_review_nodes` empty — the corr Unknown for "a"
        // is now *non-adjudicable* (no verifier evidence), so
        // `route_after_progress` must preempt Reviewer dispatch with a
        // Corr verifier. Mirrors the K-1 deadlock at the corr lane.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca-drifted".into());
        // latest_corr_review_nodes intentionally empty.
        assert!(state.latest_corr_review_nodes.is_empty());
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        assert!(state.corr_verify_nodes().contains(&NodeId::from("a")));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));

        let commands = route_after_progress(&mut state);
        assert_eq!(state.stage, Stage::VerifyCorr);
        let kind = match &commands[0] {
            ProtocolCommand::IssueRequest { request } => request.kind,
            other => panic!("expected corr verification request, got {:?}", other),
        };
        assert_eq!(kind, RequestKind::Corr);
    }

    #[test]
    fn route_after_progress_with_non_adjudicable_sound_unknowns_dispatches_sound() {
        // Audit follow-up #1 on commit `5539650`: symmetric to
        // `route_after_progress_with_non_adjudicable_corr_unknowns_dispatches_corr`
        // but for the sound lane. Pins the helper's sound branch
        // (engine.rs:1266-1271): when paper / substantiveness / corr have
        // no non-adjudicable Unknowns BUT the sound lane has one (status
        // Unknown + node not in `latest_sound_review_nodes`), preempt
        // Reviewer with a Sound verifier.
        //
        // Rationale: per `theorem_start_request_kind`, sound has the
        // lowest verifier priority (paper → substantiveness → corr →
        // sound → review). With higher-priority lanes clean,
        // `route_after_progress` must not skip past sound straight to
        // Reviewer when the sound Unknown is non-adjudicable — same
        // reasoning as the corr branch (`review_blocker_adjudicable`
        // requires the node in `latest_sound_review_nodes`, which is
        // empty here, so the reviewer's only legal Continue would be
        // task→Fail, surfacing the K-1 deadlock).
        let mut state = base_state();
        state.phase = Phase::TheoremStating;

        // Sound lane: drift "a"'s sound state to Unknown so
        // `current_sound_unknown("a")` is true. Match the test pattern
        // in `theorem_held_target_is_suspended_by_node_corr_blockers`.
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        // latest_sound_review_nodes intentionally empty (default in
        // base_state) ⇒ the sound Unknown for "a" is non-adjudicable.
        assert!(state.latest_sound_review_nodes.is_empty());

        // Substantiveness Pass for both "a" and "b" so corr_verify_nodes
        // can be checked cleanly (no substantiveness Unknowns to
        // preempt). Mirrors the corr-lane test setup.
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        // Sanity: no non-adjudicable Unknown for paper / substantiveness /
        // corr; only sound has one.
        assert!(!state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness));
        assert!(!state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness));
        assert!(!state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::Soundness));
        // Sound frontier is non-empty: select_theorem_held_target picks
        // "a" (only candidate; corr/sound state lets it through), and
        // current_sound_unknown("a") is true.
        assert!(state.sound_verify_nodes().contains(&NodeId::from("a")));

        let commands = route_after_progress(&mut state);
        assert_eq!(state.stage, Stage::VerifySound);
        let kind = match &commands[0] {
            ProtocolCommand::IssueRequest { request } => request.kind,
            other => panic!("expected sound verification request, got {:?}", other),
        };
        assert_eq!(kind, RequestKind::Sound);
    }

    #[test]
    fn theorem_sound_verify_prefers_active_unknown_over_held_current_fail() {
        // Under the node-local gate, the active node's Sound Unknown wins:
        // `a`'s cone is clean (deps[a]=∅, b's Fail is outside it), so
        // sound_auto_dispatch_eligible(a) fires. select_theorem_sound_verify_node
        // returns `a` (active-node branch) ahead of the held_target branch
        // (which would have picked `b`). The held-target Fail no longer
        // defers the dispatch — that was the old global-quiescence
        // behavior. Route goes to VerifySound, not Reviewer.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.active_node = Some("a".into());
        state.held_target = Some("b".into());
        state.proof_nodes.insert("b".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_status.insert("b".into(), SoundStatus::Fail);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb".into());
        state.node_rank.insert("a".into(), 1);
        state.node_rank.insert("b".into(), 10);

        assert_eq!(state.select_theorem_held_target().as_deref(), Some("b"));
        assert_eq!(
            state.current_sound_state(&NodeId::from("b")),
            CurrentCheckState::Fail
        );
        assert!(
            state.sound_verify_nodes().contains(&NodeId::from("a")),
            "node-local gate: a is auto-dispatch-eligible (cone clean); b's Fail is outside a's cone"
        );

        let commands = route_after_progress(&mut state);
        assert_eq!(state.stage, Stage::VerifySound);
        match &commands[0] {
            ProtocolCommand::IssueRequest { request } => {
                assert_eq!(request.kind, RequestKind::Sound);
                assert_eq!(request.sound_verify_node.as_deref(), Some("a"));
            }
            other => panic!("expected sound verify request, got {other:?}"),
        }
    }

    #[test]
    fn route_after_progress_with_surviving_fail_and_non_adjudicable_unknown_dispatches_verifier() {
        // Audit follow-up #2 on commit `5539650`: pins the behaviour
        // delta vs. the previous K-1 fix at `e6320f6`. The old code
        // routed cleanup-Valid via `apply_theorem_paper_accept`, whose
        // Step 1 (engine.rs:1072) escalates to Reviewer immediately when
        // ANY Fail blocker exists in the paper lane (target or
        // substantiveness). The new `route_after_progress` helper has no
        // such early Fail-escalation branch — it dispatches a verifier
        // whenever a non-adjudicable Unknown coexists with a surviving
        // Fail.
        //
        // Defensible: the Fail blocker is already pinned
        // (status=Fail+approved_fp=current_fp, so the verifier won't
        // re-fire on it via the standard frontier guard). The next
        // verifier round will leave it untouched, and the Reviewer will
        // adjudicate it on a subsequent cycle once the non-adjudicable
        // Unknowns have verifier evidence. The K-1 deadlock is resolved
        // because we dispatch a verifier instead of forcing the reviewer
        // into task→Fail on the Unknown.
        //
        // Setup: paper-target Fail for "t" (a survives Fail blocker) +
        // corr Unknown for "b" with `latest_corr_review_nodes` empty
        // (non-adjudicable). Old code: Reviewer (Step 1 escalates on
        // paper Fail). New code: Corr verifier.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;

        // Paper-target Fail for "t": status=Fail, approved=current.
        // `current_paper_state(t)` becomes Fail; the paper blocker
        // appears in `current_failed_blockers()`, NOT among Unknowns.
        state.paper_status.insert("t".into(), CorrStatus::Fail);
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());
        // base_state already sets paper_current_fingerprints[t] = "a=ta".

        // Corr Unknown for "b": status=Unknown so
        // `current_corr_unknown("b")` is true, and "b" is not in
        // `latest_corr_review_nodes` (empty by default) ⇒ non-adjudicable.
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");

        // Substantiveness Pass for "a" and "b" so corr_verify_nodes
        // admits "b" (per-node substantiveness gate).
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());

        // Sanity: paper has a surviving Fail (not Unknown), corr has a
        // non-adjudicable Unknown. paper_verify_targets is empty (no
        // paper Unknowns), corr_verify_nodes contains "b".
        assert!(state.paper_verify_targets().is_empty());
        assert!(!state.has_non_adjudicable_unknown_blocker(BlockerKind::PaperFaithfulness));
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::NodeCorr));
        assert!(state.corr_verify_nodes().contains(&NodeId::from("b")));
        // The paper Fail blocker IS in current_failed_blockers (excluded
        // from the non-adjudicable Unknown query).
        assert!(state
            .current_failed_blockers()
            .iter()
            .any(|b| b.kind == BlockerKind::PaperFaithfulness));

        let commands = route_after_progress(&mut state);
        // New behaviour: dispatch Corr verifier despite the surviving
        // paper Fail. Old behaviour (apply_theorem_paper_accept Step 1)
        // would have dispatched Reviewer here.
        assert_eq!(state.stage, Stage::VerifyCorr);
        let kind = match &commands[0] {
            ProtocolCommand::IssueRequest { request } => request.kind,
            other => panic!("expected corr verification request, got {:?}", other),
        };
        assert_eq!(kind, RequestKind::Corr);
    }

    #[test]
    fn last_clean_reissue_review_with_unpopulated_mirrors_routes_through_progress_helper() {
        // External-audit Finding 2 regression: the LastClean-reset path
        // (`apply_continue_last_clean_reissue_review`, engine.rs:228-234)
        // clears all four `latest_*_review_*` contexts and previously
        // dispatched Reviewer directly. If non-adjudicable Unknowns
        // survive the reset (e.g. a pre-#56 state file with empty
        // mirrors so `apply_last_clean_reset` no-ops, leaving the
        // pre-reset Unknown status intact), the K-1 deadlock recurs:
        // the reviewer's only legal Continue is task→Fail, pinning
        // status=Fail+approved=current and starving verifier dispatch.
        //
        // The fix routes this site through `route_after_progress`,
        // which preempts Reviewer dispatch with the appropriate verifier
        // when any Unknown blocker is non-adjudicable.
        //
        // This test pins the behaviour by exercising the unpopulated-
        // mirror migration scenario (apply_last_clean_reset no-ops, so
        // the pre-existing Substantiveness Unknown survives the reset)
        // and asserting the helper now dispatches a Paper verifier
        // rather than Reviewer.
        //
        // Patch C-N item 2: when `apply_last_clean_reset` returns
        // Ok(false) on the migration-guard branch, the helper now
        // SUPPRESSES the `RestoreWorktreeToLastClean` command
        // (state/disk lockstep — emitting it would git-reset disk
        // while kernel state stays put). The route_after_progress
        // dispatch is unaffected: the verifier preemption tested here
        // depends on the post-reset state (Unknown still present), not
        // on whether disk was reset.
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 7;
        state.cycles_since_clean = 3;
        // Unpopulated mirrors (pre-#56 / corrupted state file): the
        // explicit readiness flag is false, so `apply_last_clean_reset`
        // takes the migration-guard early return.
        assert!(!state.last_clean_mirrors_populated());

        // Pre-existing Substantiveness Unknown for "b": not in
        // latest_substantiveness_review_nodes (default empty), so it's
        // non-adjudicable. Keep "a" at the clean base fixture's Pass
        // state and clear only "b" to isolate the Unknown.
        clear_substantiveness(&mut state, "b");
        // "b" has no substantiveness_status entry ⇒
        // current_substantiveness_state(b) = Unknown ⇒
        // global_blockers contains a Substantiveness Unknown for "b".
        assert!(state
            .global_blockers()
            .iter()
            .any(|blocker| blocker.kind == BlockerKind::Substantiveness));

        // Drive the LastClean reissue-review path directly. The post-
        // reset state must have a non-adjudicable Substantiveness
        // Unknown (the migration-guard no-op preserves it), so the
        // helper must preempt Reviewer with a Paper verifier.
        let commands = apply_continue_last_clean_reissue_review(&mut state).unwrap();
        assert_eq!(state.stage, Stage::VerifyPaper);
        // Patch C-N item 2: RestoreWorktreeToLastClean is suppressed
        // (Ok(false) on the unpopulated-mirror branch). The only
        // command emitted is the Paper IssueRequest from
        // `route_after_progress`.
        match commands.as_slice() {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(
                    request.kind,
                    RequestKind::Paper,
                    "post external-audit Finding 2: LastClean reissue must route via \
                     route_after_progress and dispatch the substantiveness verifier when a \
                     non-adjudicable Unknown survives the reset",
                );
            }
            other => panic!(
                "expected only Paper IssueRequest (RestoreWorktreeToLastClean suppressed by \
                 Patch C-N item 2), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn theorem_worker_retry_to_review_with_non_adjudicable_unknown_dispatches_verifier() {
        // External-audit Finding 2 regression: the worker retry-to-review
        // escalation path (`reject_theorem_worker_response`,
        // engine.rs:283-290) clears all four `latest_*_review_*`
        // contexts and previously dispatched Reviewer directly. With a
        // non-adjudicable Unknown blocker surviving the worker
        // rejection's `restore_committed` (which does NOT restore lane
        // statuses), the reviewer reaches the K-1 deadlock: only legal
        // Continue is task→Fail, pinning status=Fail+approved=current,
        // starving verifier dispatch on the next cycle.
        //
        // The fix routes the escalation through `route_after_progress`.
        //
        // Setup: TheoremStating worker retry has already burned its
        // Invalid budget (retry_outcome_kind=Invalid, attempt = 2 =
        // max_theorem_invalid_attempt), so `continue_worker_retry`
        // returns false and we hit the retry-to-review branch. The
        // committed state already carries a Substantiveness Unknown for
        // "b" (status entry missing, so current_substantiveness_state =
        // Unknown). latest_substantiveness_review_nodes is empty, so
        // the Unknown is non-adjudicable. Old code dispatched Reviewer;
        // new code preempts with the Paper verifier (Substantiveness
        // shares Stage::VerifyPaper / RequestKind::Paper).
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 2; // already at max_theorem_invalid_attempt
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.active_node = Some("a".into());
        // "a" is Pass on substantiveness; clear "b" so it is Unknown.
        clear_substantiveness(&mut state, "b");
        // Mirror committed substantiveness fields too — restore_committed
        // copies live from committed, so to keep the pre-reset state's
        // substantiveness Unknown for "b" (no entry) post-restore, the
        // committed snapshot must omit "b" as well.
        state.committed.substantiveness_current_fingerprints =
            state.live.substantiveness_current_fingerprints.clone();

        // Sanity: there's a non-adjudicable Substantiveness Unknown for "b".
        assert!(state.latest_substantiveness_review_nodes.is_empty());
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness));

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: WorkingSnapshot::default(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        // Threshold reached + non-adjudicable Substantiveness Unknown ⇒
        // route_after_progress preempts Reviewer dispatch with Paper
        // (paper-target/substantiveness lane has the highest priority).
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        // Retry context still records the Invalid escalation, even
        // though the verifier preempted the Reviewer dispatch.
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Paper);
    }

    #[test]
    fn proof_worker_retry_to_review_with_non_adjudicable_unknown_dispatches_verifier() {
        // External-audit Finding 2 regression, proof-side analog of
        // `theorem_worker_retry_to_review_with_non_adjudicable_unknown_dispatches_verifier`.
        // `reject_proof_worker_response` clears all four
        // `latest_*_review_*` contexts and previously dispatched
        // Reviewer directly; the fix routes through
        // `route_after_progress` so a non-adjudicable Unknown
        // surviving `restore_committed` preempts Reviewer dispatch
        // with the appropriate verifier.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 9;
        state.attempt = 2; // proof_invalid_review_threshold
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.active_node = Some("a".into());
        // Substantiveness lane is active in ProofFormalization; "a" is
        // Pass via base_state, and "b" is cleared to be Unknown.
        clear_substantiveness(&mut state, "b");
        state.committed.substantiveness_current_fingerprints =
            state.live.substantiveness_current_fingerprints.clone();
        assert!(state.has_non_adjudicable_unknown_blocker(BlockerKind::Substantiveness));

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 9,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: WorkingSnapshot::default(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .unwrap();

        // Stage must NOT be Reviewer; it's the Paper-side verifier
        // preempting the K-1 deadlock.
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Paper);
    }

    #[test]
    fn sound_request_carries_verify_lanes() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.held_target = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");

        let request = issue_request_for_test(&mut state, RequestKind::Sound);
        assert_eq!(request.verify_nodes, set(&["a"]));
        assert_eq!(request.verify_lanes, state.verifier_lanes);
    }

    #[test]
    fn malformed_sound_response_stutters_in_verify_sound() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 9;
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.held_target = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        let request = issue_request_for_test(&mut state, RequestKind::Sound);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 9,
                    status: ResponseStatus::Malformed,
                    ..SoundResponse::default()
                }),
            },
        )
        .unwrap();
        assert_eq!(outcome.state.stage, Stage::VerifySound);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued sound request");
        assert_eq!(request.kind, RequestKind::Sound);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Sound
        ));
    }

    #[test]
    fn theorem_active_node_may_focus_present_non_open_non_proof_node() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.live.open_nodes.remove("b");

        let node = NodeId::from("b");
        assert!(state.active_node_legal(Some(&node), &state.live));
    }

    #[test]
    fn theorem_held_target_is_suspended_by_node_corr_blockers() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");

        assert_eq!(state.select_theorem_held_target().as_deref(), Some("a"));

        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        assert_eq!(state.select_theorem_held_target(), None);
    }

    #[test]
    fn theorem_targeted_review_is_legal_for_corr_failing_non_proof_node() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.live.open_nodes.remove("b");
        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state.corr_approved_fingerprints.insert(
            "b".into(),
            state
                .live
                .corr_current_fingerprints
                .get("b")
                .cloned()
                .unwrap_or_default(),
        );

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let review = ReviewResponse {
            request_id: 0,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: state.global_blockers(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("b".into()),
            reset: ResetChoice::None,
            next_mode: TaskMode::Targeted,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };

        assert!(state.review_response_legal(&review));
    }

    #[test]
    fn theorem_malformed_worker_keeps_current_live_snapshot() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let live_before = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: WorkingSnapshot::default(),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("apply malformed theorem worker");

        assert_eq!(outcome.state.live, live_before);
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(outcome.state.invalid_attempt);
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
    }

    #[test]
    fn theorem_invalid_worker_restores_committed_snapshot_before_retry() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.committed = state.live.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let live_before = state.live.clone();
        let mut invalid_snapshot = state.live.clone();
        invalid_snapshot.present_nodes = set(&["Preamble", "bad"]);
        invalid_snapshot.open_nodes = set(&["bad"]);
        invalid_snapshot.coverage.insert("t".into(), set(&["bad"]));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 3,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    summary: "compile failed".into(),
                    comments: "bad import".into(),
                    snapshot: invalid_snapshot,
                    proof_node_updates: BTreeMap::from([("bad".into(), Update::Set(true))]),
                    node_kind_updates: BTreeMap::from([(
                        "bad".into(),
                        Update::Set(NodeKind::Proof),
                    )]),
                    dep_updates: BTreeMap::from([("bad".into(), Update::Set(set(&["Preamble"])))]),
                    target_claim_updates: BTreeMap::from([(
                        "bad".into(),
                        Update::Set(set(&["t"])),
                    )]),
                    deterministic_rejection_reasons: vec![
                        "Tablet/bad.lean failed to elaborate".into()
                    ],
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("apply invalid theorem worker");

        assert_eq!(outcome.state.live, live_before);
        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(outcome.state.invalid_attempt);
        assert_eq!(
            outcome.state.deterministic_worker_rejection_reasons,
            vec!["Tablet/bad.lean failed to elaborate".to_string()]
        );
        assert_eq!(outcome.state.latest_worker_summary, "compile failed");
        assert_eq!(outcome.state.latest_worker_comments, "bad import");
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.retry_outcome_kind, RetryOutcomeKind::Invalid);
        assert!(request.invalid_attempt);
        assert_eq!(request.latest_worker_summary, "compile failed");
        assert_eq!(request.latest_worker_comments, "bad import");
        assert_eq!(
            request.deterministic_worker_rejection_reasons,
            vec!["Tablet/bad.lean failed to elaborate".to_string()]
        );
    }

    #[test]
    fn malformed_review_stutters_in_reviewer_stage() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 4;
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let live_before = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 4,
                    status: ResponseStatus::Malformed,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply malformed review");

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.state.cycle, 4);
        assert_eq!(outcome.state.live, live_before);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued review request");
        assert_eq!(request.kind, RequestKind::Review);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn illegal_review_stutters_in_reviewer_stage() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 4;
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let live_before = state.live.clone();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 4,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_mode: TaskMode::Targeted,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("apply illegal review");

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(outcome.state.cycle, 4);
        assert_eq!(outcome.state.live, live_before);
        let request = outcome
            .state
            .in_flight_request
            .as_ref()
            .expect("reissued review request");
        assert_eq!(request.kind, RequestKind::Review);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn corr_disagreement_reconciles_to_same() {
        let mut state = base_state();
        state.stage = Stage::VerifyCorr;
        state.cycle = 13;
        state.corr_status.insert("a".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("a");
        let request = issue_request_for_test(&mut state, RequestKind::Corr);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Corr(CorrResponse {
                    request_id: request.id,
                    cycle: 13,
                    status: ResponseStatus::Ok,
                    node_lane_updates: disagree_corr_node_lanes(&request.verify_lanes, "a"),
                    target_lane_updates: empty_corr_target_lanes(&request.verify_lanes),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.state.corr_status.get("a"),
            Some(&CorrStatus::Unknown)
        );
    }

    #[test]
    fn sound_unanimous_lane_updates_apply() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.cycle = 14;
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates: unanimous_sound_lanes(
                        &request.verify_lanes,
                        "a",
                        SoundStatus::Pass,
                    ),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.state.sound_status.get("a"),
            Some(&SoundStatus::Pass)
        );
    }

    #[test]
    fn sound_split_lane_updates_record_split_unknown_assessment() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 14;
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.held_target = Some("a".into());
        // A reviewer-accepted pass is not verifier evidence, but it can
        // leave the legacy mirror at Pass until a verifier rerun. A split
        // verifier response must therefore clear that mirror while recording
        // the explicit split assessment.
        state.sound_status.insert("a".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa".into());
        state.sound_assessments.insert(
            "a".into(),
            SoundAssessment {
                status: SoundAssessmentStatus::ReviewerAcceptedPass,
                origin: AssessmentOrigin::ReviewerAction,
                fingerprints: SoundFingerprintParts {
                    combined_sound_fp: "sa".into(),
                    ..SoundFingerprintParts::default()
                },
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            },
        );
        assert_eq!(
            state.current_sound_assessment(&NodeId::from("a")).status,
            SoundAssessmentStatus::ReviewerAcceptedPass
        );
        let request = issue_request_for_test(&mut state, RequestKind::Sound);
        assert_eq!(request.verify_nodes, set(&["a"]));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates: disagree_sound_lanes(&request.verify_lanes, "a"),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.state.sound_status.get("a"),
            Some(&SoundStatus::Unknown)
        );
        assert!(!outcome.state.sound_approved_fingerprints.contains_key("a"));
        let assessment = outcome.state.current_sound_assessment(&NodeId::from("a"));
        assert_eq!(assessment.status, SoundAssessmentStatus::SplitUnknown);
        assert_eq!(assessment.origin, AssessmentOrigin::VerifierPanel);
        assert_eq!(
            assessment
                .lane_votes
                .values()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([SoundStatus::Pass, SoundStatus::Fail])
        );
        assert_eq!(outcome.state.stage, Stage::Reviewer);
    }

    #[test]
    fn theorem_sound_pass_drains_next_eligible_unknown_before_review() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 14;
        state.proof_nodes.insert("b".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("b");
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);
        assert_eq!(request.sound_verify_node.as_deref(), Some("a"));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates: unanimous_sound_lanes(
                        &request.verify_lanes,
                        "a",
                        SoundStatus::Pass,
                    ),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::VerifySound);
        assert_eq!(outcome.state.held_target.as_deref(), Some("b"));
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Sound);
        assert_eq!(issued.sound_verify_node.as_deref(), Some("b"));
        assert_eq!(issued.sound_verify_nodes, set(&["b"]));
    }

    #[test]
    fn theorem_sound_fail_routes_to_review_without_draining_next_unknown() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 14;
        state.proof_nodes.insert("b".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("b");
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates: unanimous_sound_lanes(
                        &request.verify_lanes,
                        "a",
                        SoundStatus::Fail,
                    ),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Review);
    }

    #[test]
    fn theorem_sound_unknown_routes_to_review_without_reissuing_same_sound() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 14;
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.proof_nodes.insert("b".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("b");
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);
        let lane_updates: SoundLaneUpdates = request
            .verify_lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    BTreeMap::from([(NodeId::from("a"), Update::Same)]),
                )
            })
            .collect();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates,
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Review);
    }

    #[test]
    fn theorem_sound_structural_routes_to_review_without_draining_next_unknown() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.phase = Phase::TheoremStating;
        state.cycle = 14;
        state.proof_nodes.insert("b".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("a");
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        state.sound_approved_fingerprints.remove("b");
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    lane_updates: unanimous_sound_lanes(
                        &request.verify_lanes,
                        "a",
                        SoundStatus::Structural,
                    ),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Review);
    }

    // -----------------------------------------------------------------------
    // Substantiveness reconciliation unanimity (audit Finding 3, 2026-05-01).
    // Pre-fix the rule was "any Fail wins"; post-fix it mirrors the corr/sound
    // rule (strict unanimity at the `Update<SubstantivenessStatus>` level,
    // disagreement → Update::Same). These tests pin the new behaviour by
    // calling `reconcile_substantiveness_lane_updates` directly, since the
    // function is intra-crate and exercising via `apply_event` would require
    // a full per-node Paper scenario setup.
    // -----------------------------------------------------------------------

    /// Build a bare `WrapperRequest` carrying the substantiveness frontier
    /// (`nodes`) and a fixed two-lane verifier panel.
    fn substantiveness_request(nodes: &[&str], lanes: &[&str]) -> WrapperRequest {
        WrapperRequest {
            substantiveness_verify_nodes: set(nodes),
            verify_lanes: lanes.iter().map(|l| LaneId::from(*l)).collect(),
            ..WrapperRequest::default()
        }
    }

    /// Build a `PaperResponse` whose per-node lane updates are exactly the
    /// vec passed in. `node_lane_updates` is a `(lane, [(node, vote)])`
    /// list.
    fn substantiveness_response(
        lane_votes: &[(&str, &[(&str, Update<SubstantivenessStatus>)])],
    ) -> PaperResponse {
        let mut node_lane_updates: SubstantivenessLaneUpdates = BTreeMap::new();
        for (lane, votes) in lane_votes {
            let mut inner = BTreeMap::new();
            for (node, vote) in *votes {
                inner.insert(NodeId::from(*node), *vote);
            }
            node_lane_updates.insert(LaneId::from(*lane), inner);
        }
        PaperResponse {
            node_lane_updates,
            ..PaperResponse::default()
        }
    }

    #[test]
    fn substantiveness_reconcile_unanimous_pass_is_pass() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            ("lane1", &[("a", Update::Set(SubstantivenessStatus::Pass))]),
            ("lane2", &[("a", Update::Set(SubstantivenessStatus::Pass))]),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        assert_eq!(
            out.get(&NodeId::from("a")),
            Some(&Update::Set(CorrStatus::Pass))
        );
    }

    #[test]
    fn substantiveness_reconcile_unanimous_fail_is_fail() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            ("lane1", &[("a", Update::Set(SubstantivenessStatus::Fail))]),
            ("lane2", &[("a", Update::Set(SubstantivenessStatus::Fail))]),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        assert_eq!(
            out.get(&NodeId::from("a")),
            Some(&Update::Set(CorrStatus::Fail))
        );
    }

    /// The headline behaviour change. Pre-fix this was Update::Set(Fail);
    /// post-fix it's Update::Same (kernel re-dispatches on disagreement).
    #[test]
    fn substantiveness_reconcile_pass_fail_disagreement_is_same() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            ("lane1", &[("a", Update::Set(SubstantivenessStatus::Pass))]),
            ("lane2", &[("a", Update::Set(SubstantivenessStatus::Fail))]),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        // Either absent (filter_map dropped Update::Same in the inner
        // reconciler — current behaviour) or present with Update::Same.
        // Both are observationally equivalent in `apply_substantiveness_updates`.
        let vote = out.get(&NodeId::from("a")).copied();
        assert!(
            matches!(vote, None | Some(Update::Same)),
            "Pass+Fail disagreement must NOT promote to Pass/Fail; got {:?}",
            vote,
        );
    }

    /// Disagreement between Pass and NotDoneYet must also collapse to
    /// Update::Same (no Pass write). Pre-fix this was Update::Same too
    /// (the `any_not_done` branch), but the rule is now uniform unanimity.
    #[test]
    fn substantiveness_reconcile_pass_notdoneyet_disagreement_is_same() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            ("lane1", &[("a", Update::Set(SubstantivenessStatus::Pass))]),
            (
                "lane2",
                &[("a", Update::Set(SubstantivenessStatus::NotDoneYet))],
            ),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        let vote = out.get(&NodeId::from("a")).copied();
        assert!(
            matches!(vote, None | Some(Update::Same)),
            "Pass+NotDoneYet disagreement must remain Unknown; got {:?}",
            vote,
        );
    }

    /// Unanimous NotDoneYet collapses to Update::Same (no write to status
    /// mirror; kernel keeps deriving Unknown from missing-entry).
    #[test]
    fn substantiveness_reconcile_unanimous_notdoneyet_is_same() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            (
                "lane1",
                &[("a", Update::Set(SubstantivenessStatus::NotDoneYet))],
            ),
            (
                "lane2",
                &[("a", Update::Set(SubstantivenessStatus::NotDoneYet))],
            ),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        let vote = out.get(&NodeId::from("a")).copied();
        assert!(
            matches!(vote, None | Some(Update::Same)),
            "Unanimous NotDoneYet must NOT write Pass/Fail; got {:?}",
            vote,
        );
    }

    /// A node missing from one lane is treated as `Update::Same` from that
    /// lane (lenient-missing-entries). Combined with the other lane's
    /// `Update::Set(Pass)`, the votes are `[Same, Set(Pass)]` — not
    /// unanimous — so the reconciler returns Update::Same (no write).
    #[test]
    fn substantiveness_reconcile_missing_entry_is_lenient_same() {
        let request = substantiveness_request(&["a"], &["lane1", "lane2"]);
        let response = substantiveness_response(&[
            ("lane1", &[("a", Update::Set(SubstantivenessStatus::Pass))]),
            // lane2 is registered (passes ensure_lane_keys_match) but
            // does not include node a.
            ("lane2", &[]),
        ]);
        let out = reconcile_substantiveness_lane_updates(&request, &response);
        let vote = out.get(&NodeId::from("a")).copied();
        assert!(
            matches!(vote, None | Some(Update::Same)),
            "Missing entry on one lane must collapse to Update::Same; got {:?}",
            vote,
        );
    }

    // -----------------------------------------------------------------------
    // Audit Finding 4: validator must reject responses that omit a requested
    // node/target on any lane. Pre-fix, `reconcile_*_lane_updates` defaulted
    // a missing entry to `Update::Same` (silent acceptance of partial maps).
    // The bridge normalization (kernel/src/verification_normalization.rs)
    // always fills full lane × node maps, so production was unaffected; the
    // gap was a kernel API contract loosening that a future adapter could
    // exploit. Post-fix, `ensure_node_lane_scope` /
    // `ensure_target_lane_scope` require exact coverage and surface
    // `TransitionError::IllegalResponse` when an entry is missing.
    // -----------------------------------------------------------------------

    #[test]
    fn corr_response_missing_lane_for_requested_node_is_rejected() {
        let mut state = base_state();
        state.stage = Stage::VerifyCorr;
        state.cycle = 13;
        // Both `a` and `b` Unknown so verify_nodes contains both.
        state.corr_status.insert("a".into(), CorrStatus::Unknown);
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("a");
        state.corr_approved_fingerprints.remove("b");
        let request = issue_request_for_test(&mut state, RequestKind::Corr);
        assert_eq!(request.verify_nodes, set(&["a", "b"]));

        // Construct lane updates that cover every lane but omit node `b`
        // on each lane. Pre-fix: silently treated as Update::Same on `b`
        // (no Unknown→Pass/Fail transition) and accepted. Post-fix: the
        // validator rejects the response as IllegalResponse.
        let node_lane_updates: CorrNodeLaneUpdates = request
            .verify_lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    BTreeMap::from([(NodeId::from("a"), Update::Set(CorrStatus::Pass))]),
                )
            })
            .collect();

        let result = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Corr(CorrResponse {
                    request_id: request.id,
                    cycle: 13,
                    status: ResponseStatus::Ok,
                    node_lane_updates,
                    target_lane_updates: empty_corr_target_lanes(&request.verify_lanes),
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        );

        match result {
            Err(TransitionError::IllegalResponse(msg)) => {
                assert!(
                    msg.contains("missing requested node b"),
                    "expected message to mention missing node b, got: {msg}"
                );
            }
            other => {
                panic!("expected IllegalResponse for partial corr lane updates, got {other:?}")
            }
        }
    }

    #[test]
    fn paper_response_missing_lane_for_requested_target_is_rejected() {
        let mut state = base_state();
        // Add a second target so verify_targets contains two entries.
        state.configured_targets.insert("u".into());
        state.target_claims.insert("a".into(), set(&["t", "u"]));
        state.live.coverage.insert("u".into(), set(&["a"]));
        state
            .live
            .paper_current_fingerprints
            .insert("u".into(), "a=ua".into());
        state.paper_status.insert("u".into(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert("u".into(), "a=ua".into());
        state.committed = state.live.clone();
        state.committed_target_claims = state.target_claims.clone();
        // Both targets need verification.
        state.paper_status.insert("t".into(), CorrStatus::Unknown);
        state.paper_status.insert("u".into(), CorrStatus::Unknown);
        state.paper_approved_fingerprints.remove("t");
        state.paper_approved_fingerprints.remove("u");
        state.stage = Stage::VerifyPaper;
        state.cycle = 14;
        let request = issue_request_for_test(&mut state, RequestKind::Paper);
        assert_eq!(request.verify_targets, set(&["t", "u"]));

        // Omit target `u` on every lane.
        let target_lane_updates: CorrTargetLaneUpdates = request
            .verify_lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    BTreeMap::from([(TargetId::from("t"), Update::Set(CorrStatus::Pass))]),
                )
            })
            .collect();

        let result = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Paper(PaperResponse {
                    request_id: request.id,
                    cycle: 14,
                    status: ResponseStatus::Ok,
                    target_lane_updates,
                    node_lane_updates: BTreeMap::new(),
                    reviewer_evidence: BTreeMap::new(),
                    node_reviewer_evidence: BTreeMap::new(),
                    ..PaperResponse::default()
                }),
            },
        );

        match result {
            Err(TransitionError::IllegalResponse(msg)) => {
                assert!(
                    msg.contains("missing requested target u"),
                    "expected message to mention missing target u, got: {msg}"
                );
            }
            other => {
                panic!("expected IllegalResponse for partial paper lane updates, got {other:?}")
            }
        }
    }

    #[test]
    fn sound_response_missing_lane_for_requested_node_is_rejected() {
        let mut state = base_state();
        state.stage = Stage::VerifySound;
        state.cycle = 15;
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.held_target = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Sound);
        assert_eq!(request.verify_nodes, set(&["a"]));

        // Cover every lane but omit the requested node `a` (empty inner
        // map per lane). Pre-fix: defaulted to Update::Same and accepted.
        // Post-fix: rejected.
        let lane_updates: SoundLaneUpdates = request
            .verify_lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect();

        let result = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request.id,
                    cycle: 15,
                    status: ResponseStatus::Ok,
                    lane_updates,
                    reviewer_evidence: BTreeMap::new(),
                }),
            },
        );

        match result {
            Err(TransitionError::IllegalResponse(msg)) => {
                assert!(
                    msg.contains("missing requested node a"),
                    "expected message to mention missing node a, got: {msg}"
                );
            }
            other => {
                panic!("expected IllegalResponse for partial sound lane updates, got {other:?}")
            }
        }
    }

    #[test]
    fn sound_drain_loop_preserves_per_node_reviewer_evidence_for_review_request() {
        // Audit Finding 3 regression: the proof-phase Sound drain loop
        // self-loops VerifySound until every Unknown sound node is
        // verified. Before the fix, `latest_sound_reviewer_evidence` was
        // overwritten by lane on each iteration, so the reviewer cycle
        // saw only the LAST node's evidence even though
        // `latest_sound_review_nodes` authorized override→Pass on every
        // node touched by the drain. That is a soundness risk: the
        // reviewer could vacate a Sound blocker without ever seeing
        // that node's evidence.
        //
        // After the fix, evidence is keyed per-node-then-per-lane and
        // unioned across the drain. The next Review request must carry
        // BOTH drained nodes' evidence in
        // `review_verifier_evidence.sound`.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::VerifySound;
        state.cycle = 9;
        state.active_node = Some("a".into());
        state.proof_nodes.insert("b".into());
        state.live.open_nodes.insert("b".into());
        // Need a current sound fingerprint so apply_sound_updates can pin
        // the approved fingerprint when the lane updates resolve.
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());
        // Both `a` and `b` are Unknown so the drain loop visits both.
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.sound_status.insert("b".into(), SoundStatus::Unknown);
        // Round 1: kernel issues Sound for one node. We make `a` the
        // first served by giving it the lower lexicographic order
        // (BTreeSet iteration order).
        let request_a = issue_request_for_test(&mut state, RequestKind::Sound);
        let node_a = request_a.sound_verify_node.clone().expect("sound node");

        let evidence_a: BTreeMap<LaneId, SoundReviewerLaneEvidence> = request_a
            .verify_lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    SoundReviewerLaneEvidence {
                        node: node_a.clone(),
                        soundness: SoundReviewerDecisionEvidence {
                            decision: "STRUCTURAL".into(),
                            explanation: format!("lane {lane} on {node_a}"),
                        },
                        overall: "REJECT".into(),
                        summary: format!("summary-for-{node_a}"),
                        comments: format!("comments-for-{node_a}"),
                    },
                )
            })
            .collect();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request_a.id,
                    cycle: 9,
                    status: ResponseStatus::Ok,
                    // Mark `a` as Pass so it leaves the Unknown frontier
                    // and the demand-driven Sound policy can drain to `b`.
                    // A Fail would now stop the drain and return known-fail
                    // work to the reviewer.
                    lane_updates: unanimous_sound_lanes(
                        &request_a.verify_lanes,
                        &node_a,
                        SoundStatus::Pass,
                    ),
                    reviewer_evidence: evidence_a,
                }),
            },
        )
        .expect("apply first sound response in drain");

        // Drain loop: kernel re-issues Sound for the remaining Unknown.
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Sound);
        let mut state = outcome.state;
        let request_b = issued.clone();
        let node_b = request_b
            .sound_verify_node
            .clone()
            .expect("second sound node");
        assert_ne!(
            node_a, node_b,
            "drain must walk to a different node on the second cycle"
        );

        let evidence_b: BTreeMap<LaneId, SoundReviewerLaneEvidence> = request_b
            .verify_lanes
            .iter()
            .map(|lane| {
                (
                    lane.clone(),
                    SoundReviewerLaneEvidence {
                        node: node_b.clone(),
                        soundness: SoundReviewerDecisionEvidence {
                            decision: "STRUCTURAL".into(),
                            explanation: format!("lane {lane} on {node_b}"),
                        },
                        overall: "REJECT".into(),
                        summary: format!("summary-for-{node_b}"),
                        comments: format!("comments-for-{node_b}"),
                    },
                )
            })
            .collect();

        // Mirror in_flight_request bookkeeping that apply_event would do.
        state.in_flight_request = Some(request_b.clone());
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Sound(SoundResponse {
                    request_id: request_b.id,
                    cycle: 9,
                    status: ResponseStatus::Ok,
                    lane_updates: unanimous_sound_lanes(
                        &request_b.verify_lanes,
                        &node_b,
                        SoundStatus::Fail,
                    ),
                    reviewer_evidence: evidence_b,
                }),
            },
        )
        .expect("apply second sound response in drain");

        // Drain finished — kernel hands to the reviewer.
        let review_request = first_issued_request(&outcome.commands);
        assert_eq!(review_request.kind, RequestKind::Review);
        // BOTH nodes' evidence must be present in the reviewer's request.
        let sound_ev = &review_request.review_verifier_evidence.sound;
        assert!(
            sound_ev.contains_key(&node_a),
            "evidence for first drained node {node_a} missing; got keys {:?}",
            sound_ev.keys().collect::<Vec<_>>()
        );
        assert!(
            sound_ev.contains_key(&node_b),
            "evidence for second drained node {node_b} missing; got keys {:?}",
            sound_ev.keys().collect::<Vec<_>>()
        );
        // Both nodes carry one entry per verifier lane drained from the
        // sound responses. Each lane evidence's `node` field must match
        // the outer node key (this is exactly the soundness invariant
        // the storage shape now enforces).
        // Each sound response was generated for the lanes in its
        // request — capture that for the cross-check.
        let lane_count = request_b.verify_lanes.len();
        for (node, lanes) in sound_ev {
            assert_eq!(
                lanes.len(),
                lane_count,
                "node {node} should have one evidence entry per verifier lane"
            );
            for lane_evidence in lanes.values() {
                assert_eq!(&lane_evidence.node, node);
            }
        }
        // `latest_sound_review_nodes` authorizes override on both — this
        // is exactly the soundness-risk surface the fix protects.
        assert!(outcome.state.latest_sound_review_nodes.contains(&node_a));
        assert!(outcome.state.latest_sound_review_nodes.contains(&node_b));
    }

    #[test]
    fn legacy_sound_reviewer_evidence_shape_deserializes_to_empty() {
        // Audit Finding 3 migration: state files written before the
        // type change carried `latest_sound_reviewer_evidence` and
        // `previous_sound_lane_findings` keyed by `LaneId` directly.
        // We accept those without erroring — the field silently drops
        // to empty on load — because reviewer evidence is ephemeral and
        // is rebuilt the next time the verifier reports back.
        let legacy = serde_json::json!({
            "latest_sound_reviewer_evidence": {
                "v1": {
                    "node": "a",
                    "soundness": {"decision": "STRUCTURAL", "explanation": "old"},
                    "overall": "REJECT",
                    "summary": "old-summary",
                    "comments": "old-comments"
                }
            },
            "previous_sound_lane_findings": {
                "v1": {
                    "node": "a",
                    "soundness": {"decision": "STRUCTURAL", "explanation": "prev"},
                    "overall": "REJECT",
                    "summary": "prev-summary",
                    "comments": "prev-comments"
                }
            }
        });
        let parsed: ProtocolState =
            serde_json::from_value(legacy).expect("legacy shape must deserialize");
        assert!(
            parsed.latest_sound_reviewer_evidence.is_empty(),
            "legacy by-lane shape must drop to empty per-node map"
        );
        assert!(
            parsed.previous_sound_lane_findings.is_empty(),
            "legacy by-lane shape must drop to empty per-node map"
        );

        // New shape round-trips correctly.
        let new_shape = serde_json::json!({
            "latest_sound_reviewer_evidence": {
                "node_a": {
                    "v1": {
                        "node": "node_a",
                        "soundness": {"decision": "STRUCTURAL", "explanation": "new"},
                        "overall": "REJECT",
                        "summary": "new-summary",
                        "comments": "new-comments"
                    }
                }
            }
        });
        let parsed: ProtocolState =
            serde_json::from_value(new_shape).expect("new nested shape must deserialize");
        assert_eq!(parsed.latest_sound_reviewer_evidence.len(), 1);
        assert!(parsed.latest_sound_reviewer_evidence.contains_key("node_a"));
        assert_eq!(
            parsed
                .latest_sound_reviewer_evidence
                .get("node_a")
                .and_then(|by_lane| by_lane.get("v1"))
                .map(|ev| ev.summary.as_str()),
            Some("new-summary"),
        );
    }

    #[test]
    fn cycles_since_clean_resets_when_blockers_empty_and_increments_when_not() {
        let mut state = base_state();
        // base_state has all corr/sound/paper at Pass, so global_blockers is empty.
        state.cycles_since_clean = 7;
        assert!(!state.has_ever_been_clean);
        state.commit_live();
        assert_eq!(state.cycles_since_clean, 0);
        assert!(state.has_ever_been_clean);

        // Introduce a NodeCorr blocker by flipping corr_status.
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.commit_live();
        assert_eq!(state.cycles_since_clean, 1);
        state.commit_live();
        assert_eq!(state.cycles_since_clean, 2);
        // has_ever_been_clean stays sticky — one clean sighting is enough.
        assert!(state.has_ever_been_clean);

        // Resolve the blocker — counter zeroes again.
        state.corr_status.insert("b".into(), CorrStatus::Pass);
        state.commit_live();
        assert_eq!(state.cycles_since_clean, 0);
    }

    #[test]
    fn request_allowed_resets_offers_last_clean_only_after_a_clean_checkpoint() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.retry_outcome_kind = RetryOutcomeKind::None;

        // Fresh state, never seen clean: no LastClean even if cycles_since_clean > 0.
        state.cycles_since_clean = 3;
        state.has_ever_been_clean = false;
        let allowed = state.request_allowed_resets(RequestKind::Review);
        assert!(
            !allowed.contains(&ResetChoice::LastClean),
            "without a prior clean checkpoint, LastClean must not be offered (no git tag to rewind to)"
        );

        // After a clean checkpoint + subsequent dirty one → LastClean offered.
        // Drive through commit_live so the last_clean_* mirrors get populated
        // — without that, the migration gate in request_allowed_resets
        // suppresses LastClean.
        state.commit_live();
        state.cycles_since_clean = 1;
        let allowed = state.request_allowed_resets(RequestKind::Review);
        assert!(allowed.contains(&ResetChoice::LastClean));

        // If currently clean (cycles_since_clean == 0), LastClean is not offered
        // (no reason to rewind; LastClean to current state is a no-op that would
        // just trigger unnecessary verifier re-work).
        state.cycles_since_clean = 0;
        let allowed = state.request_allowed_resets(RequestKind::Review);
        assert!(!allowed.contains(&ResetChoice::LastClean));

        // LastClean is independent of retry state.
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.cycles_since_clean = 4;
        let allowed = state.request_allowed_resets(RequestKind::Review);
        assert!(allowed.contains(&ResetChoice::LastClean));
        assert!(allowed.contains(&ResetChoice::LastCommit));
    }

    #[test]
    fn apply_last_clean_reset_restores_lane_statuses_and_fingerprints_from_mirror() {
        // (#56-extension) Pre-extension `apply_last_clean_reset` cleared
        // corr/sound/paper statuses, producing phantom Unknown blockers
        // in proof/cleanup phases (whose start_cycle routes to Worker,
        // not verifier — so statuses never re-establish, leaving the
        // run stuck with unadjudicable blockers). Now we restore the
        // mirrors: at a clean checkpoint global_blockers().is_empty(),
        // so every relevant status was Pass; restoring keeps post-reset
        // state consistent with the rewound disk.
        let mut state = base_state();
        // base_state has all-Pass statuses, so global_blockers is empty.
        // commit_live populates the last_clean_* mirrors.
        state.commit_live();
        let mirror_corr_status = state.last_clean_corr_status.clone();
        let mirror_paper_status = state.last_clean_paper_status.clone();
        let mirror_sound_status = state.last_clean_sound_status.clone();
        // Now mutate statuses simulating mid-cycle work that drove some
        // nodes to Unknown / Fail (and so global_blockers became
        // non-empty). The rewind should restore the snapshotted Pass.
        state.corr_status.insert("a".into(), CorrStatus::Unknown);
        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.paper_status.insert("t".into(), CorrStatus::Fail);
        state.cycles_since_clean = 5;
        let mirror_corr = state.last_clean_live.corr_current_fingerprints.clone();
        let mirror_sound = state.last_clean_live.sound_current_fingerprints.clone();
        let mirror_paper = state.last_clean_live.paper_current_fingerprints.clone();
        assert!(
            !mirror_corr.is_empty(),
            "test setup: mirror should have corr fingerprints"
        );
        assert!(
            !mirror_paper.is_empty(),
            "test setup: mirror should have paper fingerprints"
        );
        let approved_corr_before = state.corr_approved_fingerprints.clone();
        let approved_sound_before = state.sound_approved_fingerprints.clone();
        let approved_paper_before = state.paper_approved_fingerprints.clone();

        // Patch C-N item 2: apply_last_clean_reset now returns
        // Result<bool, String>. Ok(true) means the full rewind ran;
        // commit_live above seeded the mirrors so that's the expected
        // outcome here.
        assert_eq!(state.apply_last_clean_reset(), Ok(true));

        // Lane statuses RESTORED from mirror, not wiped.
        assert_eq!(state.corr_status, mirror_corr_status);
        assert_eq!(state.paper_status, mirror_paper_status);
        assert_eq!(state.sound_status, mirror_sound_status);
        // current_fingerprints RESTORED from mirror as part of live.
        assert_eq!(state.live.corr_current_fingerprints, mirror_corr);
        assert_eq!(state.live.sound_current_fingerprints, mirror_sound);
        assert_eq!(state.live.paper_current_fingerprints, mirror_paper);
        assert_eq!(state.committed.corr_current_fingerprints, mirror_corr);
        assert_eq!(state.committed.sound_current_fingerprints, mirror_sound);
        assert_eq!(state.committed.paper_current_fingerprints, mirror_paper);
        assert_eq!(state.cycles_since_clean, 0);

        // Crucial: post-reset, global_blockers must be empty. Otherwise
        // proof/cleanup phases get stuck with unadjudicable blockers.
        assert!(
            state.global_blockers().is_empty(),
            "post-LastClean, global_blockers must be empty; got {:?}",
            state.global_blockers(),
        );

        // Approved-fp maps are restored from the
        // last_clean_<lane>_approved_fingerprints mirror (audit
        // follow-up — see test
        // apply_last_clean_reset_restores_approved_fingerprints_from_mirror
        // for the bug this prevents). In this test the test setup
        // doesn't mutate approved_fp between commit_live and the reset,
        // so the mirror values equal the pre-mutation values; the
        // restoration is a no-op here and the equality holds.
        assert_eq!(state.corr_approved_fingerprints, approved_corr_before);
        assert_eq!(state.sound_approved_fingerprints, approved_sound_before);
        assert_eq!(state.paper_approved_fingerprints, approved_paper_before);
    }

    #[test]
    fn apply_last_clean_reset_restores_approved_fingerprints_from_mirror() {
        // (audit follow-up) Pre-fix, approved-fp maps were left at
        // their latest worker-updated values across LastClean while
        // status + current_fp were restored from the clean checkpoint.
        // current_<lane>_state requires status=Pass AND
        // current_fp == approved_fp, so the mismatch flipped the lane
        // to Unknown immediately on the supposedly-clean reset:
        // phantom blocker on a "clean" reset.
        //
        // The fix snapshots approved_fp into
        // last_clean_<lane>_approved_fingerprints mirrors at clean
        // commit_live, and restores them in apply_last_clean_reset
        // alongside status + current_fp. After restore, the three
        // align with the rewound disk → no phantom blockers.
        let mut state = base_state();
        // Capture clean-checkpoint approved_fp.
        state.commit_live();
        let mirror_corr_approved = state.last_clean_corr_approved_fingerprints.clone();
        let mirror_paper_approved = state.last_clean_paper_approved_fingerprints.clone();
        let mirror_sound_approved = state.last_clean_sound_approved_fingerprints.clone();
        assert!(
            !mirror_corr_approved.is_empty(),
            "test setup: mirror should have corr approved fingerprints from base_state",
        );

        // Simulate the bug-trigger scenario: post-clean a worker
        // changed node "a", verifier re-passed it, approved_fp moved
        // from "ca" → "ca-v2". Same for paper target "t" and sound on
        // node "a". Some other blocker is open so the reviewer can
        // pick LastClean.
        state
            .corr_approved_fingerprints
            .insert("a".into(), "ca-v2".into());
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca-v2".into());
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta-v2".into());
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta-v2".into());
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa-v2".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa-v2".into());
        // Other blocker so the system is dirty (reviewer would offer LastClean).
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.cycles_since_clean = 3;

        // Patch C-N item 2: Ok(true) — commit_live seeded both
        // verifier and closure mirrors, so the rewind ran.
        assert_eq!(state.apply_last_clean_reset(), Ok(true));

        // Approved-fp restored to clean-checkpoint values.
        assert_eq!(state.corr_approved_fingerprints, mirror_corr_approved);
        assert_eq!(state.paper_approved_fingerprints, mirror_paper_approved);
        assert_eq!(state.sound_approved_fingerprints, mirror_sound_approved);

        // The crux: post-restore, status + current_fp + approved_fp
        // align (all from the same clean snapshot), so no phantom
        // Unknown is produced. global_blockers stays empty.
        assert!(
            state.global_blockers().is_empty(),
            "post-LastClean, approved_fp + current_fp + status must all align \
             from the clean snapshot — no phantom Unknown blockers; got {:?}",
            state.global_blockers(),
        );
    }

    #[test]
    fn last_clean_mirrors_populated_gates_on_explicit_flag_not_structural_emptiness() {
        // (audit migration follow-up) Pre-fix, last_clean_mirrors_populated()
        // checked structural emptiness — so a state file persisted
        // before the status mirrors (#56-extension) or the
        // approved-fp mirrors (this commit) existed could pass the
        // gate with populated structural mirrors but empty status /
        // approved-fp mirrors. Applying LastClean from such a state
        // restored empty status / approved-fp maps → phantom
        // Unknown blockers.
        //
        // The fix: gate on an explicit
        // last_clean_verifier_mirror_ready bool that commit_live
        // sets only when populating ALL mirrors atomically. State
        // files persisted before any mirror existed deserialize the
        // flag as false, suppressing LastClean until the next clean
        // commit_live writes a complete mirror set.
        let mut state = base_state();
        // Simulate a pre-mirror-set state file: structural mirrors
        // populated by hand, but the readiness flag is false (as
        // serde_default would give).
        state.last_clean_live = state.live.clone();
        state.last_clean_node_kinds = state.node_kinds.clone();
        state.last_clean_proof_nodes = state.proof_nodes.clone();
        state.last_clean_deps = state.deps.clone();
        state.last_clean_target_claims = state.target_claims.clone();
        state.has_ever_been_clean = true;
        // Status + approved-fp mirrors stay default (empty).
        // Readiness flag stays false.
        assert!(
            !state.last_clean_mirrors_populated(),
            "structural mirrors populated but readiness flag is false → \
             gate must reject (otherwise pre-mirror state files leak)",
        );

        // After a real commit_live captures everything atomically,
        // the gate opens.
        state.commit_live();
        assert!(
            state.last_clean_mirrors_populated(),
            "after a clean commit_live with all mirrors written, gate must open",
        );
    }

    #[test]
    fn apply_last_clean_reset_no_ops_when_mirrors_empty() {
        // (#56 migration safety) Pre-#56 state files deserialize with
        // empty last_clean_* mirrors but possibly has_ever_been_clean=true.
        // The reset must NOT restore from empty mirrors, which would
        // violate validate_invariants (paper_current_fingerprints must
        // cover configured_targets).
        //
        // (#56-extension: in this branch we ALSO leave statuses
        // unchanged. Pre-extension we cleared them, but if the only
        // change we make is clearing statuses while leaving structural
        // state alone, we may produce phantom Unknown blockers without
        // any compensating mirror to fix them. Better to leave the
        // pre-call state intact entirely; the migration is only safe
        // for as long as it takes to hit the next clean commit_live.)
        let mut state = base_state();
        // Don't call commit_live — mirrors stay empty.
        state.has_ever_been_clean = true;
        state.cycles_since_clean = 3;
        let live_before = state.live.clone();
        let committed_before = state.committed.clone();
        let corr_before = state.corr_status.clone();
        state
            .corr_status
            .insert("phantom".into(), CorrStatus::Unknown);

        // Patch C-N item 2: empty mirrors → Ok(false). The function
        // still zeros cycles_since_clean unconditionally.
        assert_eq!(state.apply_last_clean_reset(), Ok(false));

        assert_eq!(state.cycles_since_clean, 0);
        // Structural state UNCHANGED (no restore from empty mirrors).
        assert_eq!(state.live, live_before);
        assert_eq!(state.committed, committed_before);
        // Statuses also unchanged: the reset is best-effort only.
        // Pre-call seeded "phantom" stays; pre-existing "a"/"b"
        // entries from base_state stay too.
        assert!(state.corr_status.contains_key("phantom"));
        for (k, v) in &corr_before {
            assert_eq!(state.corr_status.get(k), Some(v));
        }
    }

    #[test]
    fn engine_does_not_emit_restore_worktree_when_apply_last_clean_reset_returns_false() {
        // Patch C-N item 2: the half-migrated state where verifier
        // mirrors are populated (so request_allowed_resets might still
        // offer LastClean on a stale code path) but the closure-mirror
        // readiness flag is false — `apply_last_clean_reset` refuses
        // and returns Ok(false). The engine helper
        // `apply_last_clean_reset_and_emit` must NOT push
        // `RestoreWorktreeToLastClean` in that case: doing so would
        // git-reset disk to the supervisor2/clean tag while kernel
        // state still reflects the post-clean burst (state/disk
        // divergence — the residual hole C-I closed only at the menu
        // level).
        let mut state = base_state();
        // Drive a clean commit_live to populate ALL mirrors (verifier
        // + closure flag, structural snapshots).
        state.commit_live();
        assert!(state.last_clean_verifier_mirror_ready);
        assert!(state.last_clean_local_closure_mirror_ready);
        // Force the migration shape: closure flag false, verifier flag
        // true. This is the exact pre-Patch-C-A persistent-state shape
        // C-I's Option A addressed at the model layer; item 2 closes
        // the gap on any engine call path that ever bypasses the
        // request_allowed_resets menu.
        state.last_clean_local_closure_mirror_ready = false;
        let live_before = state.live.clone();
        let committed_before = state.committed.clone();
        let records_before = state.local_closure_records.clone();

        let mut commands: Vec<ProtocolCommand> = Vec::new();
        apply_last_clean_reset_and_emit(&mut state, &mut commands)
            .expect("helper must not produce Err for Ok(false) inner result");

        // No RestoreWorktreeToLastClean command emitted — disk stays in
        // sync with the (untouched) kernel state.
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, ProtocolCommand::RestoreWorktreeToLastClean)),
            "RestoreWorktreeToLastClean must be suppressed when the reset refuses; got: {:?}",
            commands,
        );
        // State untouched (structural live/committed and closure
        // records all unchanged) — matches the model-layer refusal.
        assert_eq!(state.live, live_before);
        assert_eq!(state.committed, committed_before);
        assert_eq!(state.local_closure_records, records_before);
    }

    #[test]
    fn last_clean_with_next_active_vanished_post_restore_is_illegal() {
        // Updated (#62 follow-up): the post-reset legality probe was
        // dropped in favor of the simpler "Continue+LastClean+Some(_)
        // is always illegal" rule. The engine now applies the reset
        // and re-issues a Review request from the post-reset state, so
        // the reviewer picks next_active fresh on the next turn —
        // there's no pre-reset next_active to validate against the
        // post-reset state in the first place. This test stays as
        // documentation of the rule (Continue+LastClean+Some is illegal)
        // and as a regression guard against a future relaxation that
        // would let the reviewer specify next_active alongside LastClean.
        let mut state = base_state();
        // Seed mirrors via a clean commit_live (base_state: a, b).
        state.commit_live();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.active_node = Some("a".into());
        // Worker created helper "h" after the clean checkpoint.
        state.live.present_nodes.insert("h".into());
        state.committed.present_nodes.insert("h".into());
        state.node_kinds.insert("h".into(), NodeKind::Proof);
        state
            .committed_node_kinds
            .insert("h".into(), NodeKind::Proof);
        state.proof_nodes.insert("h".into());
        state.committed_proof_nodes.insert("h".into());
        // Reviewer is currently happy with current; its
        // kernel_hinted_next_active_nodes set will include "h" because it's
        // a present_nodes member.
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            request.kernel_hinted_next_active_nodes.contains("h"),
            "test setup: reviewer's allowed_next_active should include h pre-reset",
        );
        assert!(request.allowed_resets.contains(&ResetChoice::LastClean));
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            next_active: Some("h".into()),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&response),
            "Continue+LastClean+next_active=h must be illegal: h doesn't exist post-restore",
        );
    }

    #[test]
    fn proof_continue_last_clean_applies_reset_and_dispatches_audit() {
        // End-to-end: Continue+LastClean+None → engine applies the reset
        // (state restored from mirrors; RestoreWorktreeToLastClean emitted).
        // Post `force_stuck_math_audit_after_rewind`: the first dispatch is
        // a StuckMathAudit (the rewound state earns a fresh adversarial
        // look before any Reviewer touches it), not a Review. The
        // reviewer's response fields (next_active, next_mode, etc.) are
        // discarded; the audit-then-reviewer chain re-decides scope.
        let mut state = base_state();
        state.commit_live();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.active_node = Some("a".into());
        // Mid-cycle dirty: pretend the worker added a node "z" that wouldn't
        // exist in the clean checkpoint.
        state.live.present_nodes.insert("z".into());
        state.live.open_nodes.insert("z".into());
        state.proof_nodes.insert("z".into());
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    reset: ResetChoice::LastClean,
                    next_active: None,
                    next_mode: TaskMode::Local,
                    next_worker_context_mode: WorkerContextMode::Resume,
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();

        // State restored from clean mirror — phantom "z" is gone.
        assert!(!outcome.state.live.present_nodes.contains("z"));
        assert_eq!(outcome.state.stage, Stage::StuckMathAudit);
        // Force flag consumed by the dispatch.
        assert!(!outcome.state.force_stuck_math_audit_after_rewind);
        // Latch auto-activated by the forced dispatch.
        assert!(outcome.state.stuck_math_audit.active);
        // Commands: worktree restore + re-issued StuckMathAudit request.
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToLastClean,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::StuckMathAudit
        ));
    }

    #[test]
    fn proof_review_protected_scope_requires_second_confirmation() {
        let mut state = clean_proof_review_state_with_protected_closure();
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let protected_nodes = set(&["b"]);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::CoarseRestructure,
            protected_semantic_change_nodes: protected_nodes.clone(),
            authorized_nodes: set(&["a", "b"]),
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&response));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert!(outcome.state.pending_task.is_none());
        assert_eq!(
            outcome
                .state
                .pending_protected_semantic_scope_confirmation
                .as_ref()
                .map(|confirmation| confirmation.nodes.clone()),
            Some(protected_nodes)
        );
        let issued = first_issued_request(&outcome.commands);
        assert_eq!(issued.kind, RequestKind::Review);
        assert!(issued
            .protected_semantic_change_confirmation
            .as_ref()
            .is_some_and(|confirmation| confirmation.nodes == set(&["b"])));
    }

    #[test]
    fn stuck_math_product_not_recorded_on_protected_scope_confirmation_reissue() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.sound_status.insert("a".into(), SoundStatus::Fail);
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test".into(),
            ..StuckMathAuditState::default()
        };
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let product = serde_json::json!({"kind": "probe", "result": "needs invariant"});
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::CoarseRestructure,
            task_blockers: request.blockers.clone(),
            protected_semantic_change_nodes: set(&["b"]),
            authorized_nodes: set(&["a", "b"]),
            paper_focus_ranges,
            paper_grounding,
            stuck_math_audit: Some(StuckMathAuditReviewReport {
                notes: "diagnostic pending confirmation".into(),
                reviewer_lean_product: Some(product),
            }),
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&response));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(
            outcome.state.stuck_math_audit.last_reviewer_lean_product, None,
            "unconfirmed protected-scope reviews must not latch worker handoff products"
        );
    }

    #[test]
    fn proof_review_protected_scope_confirmation_dispatches_scoped_task() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.pending_protected_semantic_scope_confirmation =
            Some(ProtectedSemanticChangeConfirmation {
                nodes: set(&["b"]),
                next_active: Some("a".into()),
                next_mode: TaskMode::CoarseRestructure,
                allow_new_obligations: true,
                must_close_active: false,
            });
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    next_mode: TaskMode::CoarseRestructure,
                    protected_semantic_change_nodes: set(&["b"]),
                    confirm_protected_semantic_change_scope: true,
                    authorized_nodes: set(&["a", "b"]),
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Start);
        assert_eq!(
            outcome
                .state
                .pending_task
                .as_ref()
                .map(|task| task.protected_semantic_change_nodes.clone()),
            Some(set(&["b"]))
        );
        assert!(outcome
            .state
            .pending_protected_semantic_scope_confirmation
            .is_none());
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::CommitCheckpoint]
        ));
    }

    #[test]
    fn stuck_math_product_records_after_protected_scope_confirmation() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.sound_status.insert("a".into(), SoundStatus::Fail);
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test".into(),
            ..StuckMathAuditState::default()
        };
        state.pending_protected_semantic_scope_confirmation =
            Some(ProtectedSemanticChangeConfirmation {
                nodes: set(&["b"]),
                next_active: Some("a".into()),
                next_mode: TaskMode::CoarseRestructure,
                allow_new_obligations: true,
                must_close_active: false,
            });
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let product = serde_json::json!({"kind": "probe", "result": "needs invariant"});
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    next_mode: TaskMode::CoarseRestructure,
                    task_blockers: request.blockers.clone(),
                    protected_semantic_change_nodes: set(&["b"]),
                    confirm_protected_semantic_change_scope: true,
                    authorized_nodes: set(&["a", "b"]),
                    paper_focus_ranges,
                    paper_grounding,
                    stuck_math_audit: Some(StuckMathAuditReviewReport {
                        notes: "confirmed diagnostic".into(),
                        reviewer_lean_product: Some(product.clone()),
                    }),
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(
            outcome.state.stuck_math_audit.last_reviewer_lean_product,
            Some(product)
        );
    }

    #[test]
    fn protected_reapproval_routes_to_human_gate_after_verifier_drain() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::VerifySound;
        state.deps.insert("a".into(), set(&["b"]));
        state.pending_protected_reapproval_nodes = set(&["b"]);
        assert!(state.global_blockers().is_empty());
        assert!(!state.orphan_cleanup_needed());

        let commands = route_after_progress(&mut state);

        assert_eq!(state.stage, Stage::HumanGate);
        assert_eq!(state.gate_kind, GateKind::ProtectedReapproval);
        assert!(matches!(
            commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }]
                if request.kind == RequestKind::HumanGate
                    && request.protected_reapproval_nodes == set(&["b"])
        ));
    }

    #[test]
    fn protected_reapproval_feedback_repair_survives_start_cycle() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::HumanGate;
        state.gate_kind = GateKind::ProtectedReapproval;
        state.deps.insert("a".into(), set(&["b"]));
        state.pending_protected_reapproval_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Feedback,
                }),
            },
        )
        .unwrap();
        let review_request = first_issued_request(&outcome.commands);
        assert_eq!(review_request.kind, RequestKind::Review);
        let review_request_id = review_request.id;
        assert!(outcome.state.human_input_outstanding);

        let outcome = apply_event(
            outcome.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: review_request_id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    next_mode: TaskMode::Local,
                    next_worker_context_mode: WorkerContextMode::Resume,
                    clear_human_input: false,
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Start);
        assert!(outcome.state.human_input_outstanding);
        assert!(outcome.state.pending_task.is_some());
        assert!(outcome.commands.is_empty());

        let outcome = apply_event(outcome.state, ProtocolEvent::StartCycle).unwrap();

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert!(outcome.state.human_input_outstanding);
        assert!(outcome.state.pending_task.is_some());
        assert_eq!(outcome.state.gate_kind, GateKind::None);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Worker
        ));
    }

    #[test]
    fn protected_reapproval_feedback_cleared_reissues_human_gate() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::HumanGate;
        state.gate_kind = GateKind::ProtectedReapproval;
        state.deps.insert("a".into(), set(&["b"]));
        state.pending_protected_reapproval_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Feedback,
                }),
            },
        )
        .unwrap();
        let review_request = first_issued_request(&outcome.commands);
        let review_request_id = review_request.id;

        let outcome = apply_event(
            outcome.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: review_request_id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    next_mode: TaskMode::Local,
                    next_worker_context_mode: WorkerContextMode::Resume,
                    clear_human_input: true,
                    ..ReviewResponse::default()
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.stage, Stage::Start);
        assert!(!outcome.state.human_input_outstanding);
        assert!(outcome.state.pending_task.is_some());

        let outcome = apply_event(outcome.state, ProtocolEvent::StartCycle).unwrap();

        assert_eq!(outcome.state.stage, Stage::HumanGate);
        assert_eq!(outcome.state.gate_kind, GateKind::ProtectedReapproval);
        assert!(outcome.state.pending_task.is_none());
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }]
                if request.kind == RequestKind::HumanGate
                    && request.protected_reapproval_nodes == set(&["b"])
        ));
    }

    #[test]
    fn protected_reapproval_waits_for_retry_review_to_finish() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::VerifySound;
        state.deps.insert("a".into(), set(&["b"]));
        state.pending_protected_reapproval_nodes = set(&["b"]);
        state.retry_outcome_kind = RetryOutcomeKind::Stuck;
        assert!(state.global_blockers().is_empty());
        assert!(!state.orphan_cleanup_needed());

        let commands = route_after_progress(&mut state);

        assert_eq!(state.stage, Stage::Reviewer);
        assert_eq!(state.gate_kind, GateKind::None);
        assert!(matches!(
            commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn protected_reapproval_schedules_orphan_cleanup_first() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::VerifySound;
        state.pending_protected_reapproval_nodes = set(&["b"]);
        assert!(state.global_blockers().is_empty());
        assert!(state.orphan_cleanup_needed());

        let commands = route_after_progress(&mut state);

        assert_eq!(state.stage, Stage::Worker);
        assert_eq!(state.gate_kind, GateKind::None);
        assert!(state.orphan_cleanup_active());
        assert!(matches!(
            commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Worker
        ));
    }

    #[test]
    fn protected_reapproval_preempts_proof_start_cycle() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::Start;
        state.cycle = 11;
        state.deps.insert("a".into(), set(&["b"]));
        state.pending_protected_reapproval_nodes = set(&["b"]);
        assert!(state.global_blockers().is_empty());
        assert!(!state.orphan_cleanup_needed());

        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();

        assert_eq!(outcome.state.cycle, 12);
        assert_eq!(outcome.state.stage, Stage::HumanGate);
        assert_eq!(outcome.state.gate_kind, GateKind::ProtectedReapproval);
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }]
                if request.kind == RequestKind::HumanGate
                    && request.protected_reapproval_nodes == set(&["b"])
        ));
    }

    #[test]
    fn review_request_surfaces_pending_protected_reapproval_nodes() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.stage = Stage::Reviewer;
        state.pending_protected_reapproval_nodes = set(&["b"]);

        let request = state.expected_request(23, RequestKind::Review);

        assert_eq!(request.protected_reapproval_nodes, set(&["b"]));
        assert_eq!(
            request.review_contract["request_summary"]["protected_reapproval_nodes"],
            serde_json::json!(set::<NodeId>(&["b"]))
        );
        assert_eq!(
            request.review_contract["request_summary"]["protected_reapproval_status"],
            serde_json::json!(
                "pending human reapproval after normal verifier blockers drain; do not treat this as a blocker-action item"
            )
        );
    }

    #[test]
    fn pending_protected_reapproval_is_not_a_clean_checkpoint() {
        let mut state = clean_proof_review_state_with_protected_closure();
        state.pending_protected_reapproval_nodes = set(&["b"]);
        assert!(state.global_blockers().is_empty());

        state.commit_live();

        assert!(!state.last_clean_mirrors_populated());
        assert_eq!(state.cycles_since_clean, 1);
    }

    #[test]
    fn protected_reapproval_human_approval_refreshes_protected_snapshot() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::HumanGate;
        state.gate_kind = GateKind::ProtectedReapproval;
        state.cycle = 9;
        state.pending_protected_reapproval_nodes = set(&["b"]);
        state
            .live
            .protected_closure_nodes_per_target
            .insert("t".into(), set(&["a", "b", "Preamble"]));
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "paper-after-reapproval".into());
        let request = issue_request_for_test(&mut state, RequestKind::HumanGate);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(HumanGateResponse {
                    request_id: request.id,
                    cycle: 9,
                    status: ResponseStatus::Ok,
                    choice: HumanChoice::Approve,
                }),
            },
        )
        .unwrap();

        assert_eq!(outcome.state.phase, Phase::ProofFormalization);
        assert_eq!(outcome.state.stage, Stage::Start);
        assert_eq!(outcome.state.gate_kind, GateKind::None);
        assert!(outcome.state.pending_protected_reapproval_nodes.is_empty());
        assert_eq!(
            outcome.state.approved_targets.protected_closure_nodes,
            set(&["b"])
        );
        assert_eq!(
            outcome.state.paper_approved_fingerprints.get("t"),
            Some(&"paper-after-reapproval".to_string())
        );
        assert_eq!(
            outcome
                .state
                .last_clean_paper_approved_fingerprints
                .get("t"),
            Some(&"paper-after-reapproval".to_string())
        );
    }

    #[test]
    fn continue_last_clean_with_none_next_active_is_legal() {
        // The new "LastClean is a pure rewind, re-issue Review" contract:
        // Continue+LastClean is legal only with next_active=None. The
        // reviewer's routing (next_active, mode, blocker adjudications)
        // is decided on the next Review turn against the post-reset state.
        let mut state = base_state();
        state.commit_live();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            next_active: None,
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "Continue+LastClean+None is legal: kernel re-issues Review after reset",
        );
    }

    #[test]
    fn last_clean_with_next_active_legal_pre_reset_but_no_open_work_post_restore_is_illegal() {
        // (#56-extension follow-up: tightened legality check.)
        // Setup: clean checkpoint contains node "h" present as a closed,
        // non-proof Definition with no blockers. Pre-reset the worker has
        // promoted "h" to a Proof node with an open sorry, so it's a
        // legal proof-phase active. Reviewer picks Continue + LastClean +
        // next_active="h". The pre-reset legality check would pass
        // (request.kernel_hinted_next_active_nodes is computed against current
        // live, where "h" is a legal proof active). But post-restore,
        // "h" is back to: not in proof_nodes, not in open_nodes, no
        // blocker against it. ProofFormalization's active_node_legal
        // requires open_nodes ∋ node OR proof_node_repairs_blocker(node).
        // Both are false post-restore, so the tightened legality check
        // catches this.
        //
        // Earlier framing ("post-restore h reverts to Definition kind")
        // was incorrect — active_node_legal does not consult node_kinds.
        // The real reason for rejection is the absence of any work
        // (open sorry / live blocker) for "h" in the restored state.
        let mut state = base_state();
        // Add "h" as a present-but-closed Definition with seeded
        // corr Pass (so global_blockers stays empty and commit_live
        // populates the last_clean_* mirrors). needs_sound("h") is
        // false (not in proof_nodes), so no sound seed required.
        state.live.present_nodes.insert("h".into());
        state.committed.present_nodes.insert("h".into());
        state.node_kinds.insert("h".into(), NodeKind::Definition);
        state
            .committed_node_kinds
            .insert("h".into(), NodeKind::Definition);
        state
            .live
            .corr_current_fingerprints
            .insert("h".into(), "ch".into());
        state.corr_status.insert("h".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("h".into(), "ch".into());
        mark_substantiveness_pass(&mut state, "h", "sub-h");
        // (No sound seed needed: at the clean checkpoint "h" is a
        // Definition, not in proof_nodes, so needs_sound("h") is false
        // and corr Pass alone keeps global_blockers empty. The earlier
        // sound seed was a workaround for a scoping bug in
        // `proof_node_repairs_blocker` — that helper read self.live /
        // self.sound_status (the *current*, pre-reset state) instead
        // of the post-reset snapshot, so it returned true on Unknown
        // current sound and silently accepted "h" as legal. Fixed by
        // running the post-reset legality check against a clone with
        // `apply_last_clean_reset` applied (option (A) from the audit),
        // so the predicate now sees the restored mirror values where
        // "h" is a closed Definition with no work.)
        // Capture clean mirrors. Asserted below.
        state.commit_live();
        assert!(
            state.last_clean_mirrors_populated(),
            "test setup precondition: commit_live must have populated the \
             last_clean_* mirrors so this test exercises post-restore \
             active_node_legal rather than the migration gate",
        );

        // Now move into proof phase mid-cycle; promote "h" to a Proof
        // node with an open sorry (legal pre-reset proof active).
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.active_node = Some("a".into());
        state.node_kinds.insert("h".into(), NodeKind::Proof);
        state.proof_nodes.insert("h".into());
        state.live.open_nodes.insert("h".into());

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            request.allowed_resets.contains(&ResetChoice::LastClean),
            "test setup precondition: LastClean must be a legal reset \
             choice (mirrors populated, dirty checkpoints since clean) \
             — otherwise the legality test below is vacuous against \
             allowed_resets rather than active_node_legal",
        );
        assert!(
            request.kernel_hinted_next_active_nodes.contains("h"),
            "test setup precondition: pre-reset h is a legal proof active",
        );
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            next_active: Some("h".into()),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&response),
            "Continue+LastClean+next_active=h must be illegal: post-restore \
             h is closed (not in open_nodes), not a proof_node (so \
             proof_node_repairs_blocker is false), with no live blocker \
             — flunks ProofFormalization's active_node_legal predicate",
        );
    }

    #[test]
    fn last_clean_with_next_active_repairs_blocker_in_pre_reset_only_is_illegal() {
        // (Audit follow-up: post-reset legality must be evaluated
        // against post-reset state, not current state.)
        //
        // Canonical failing scenario for the
        // `proof_node_repairs_blocker` scoping bug (option (A) fix):
        //
        //   * Clean checkpoint: "h" present as a closed Definition with
        //     no blockers — corr Pass, paper Pass, no open sorry, not
        //     in proof_nodes. So post-restore neither clause of
        //     ProofFormalization's `active_node_legal` holds for "h":
        //     `open_nodes` doesn't contain it, and
        //     `proof_node_repairs_blocker` returns false (no current
        //     corr/sound/paper blocker).
        //
        //   * Pre-reset (current state): "h" still present as a
        //     Definition, but its corr fingerprints have been bumped
        //     (current != approved → CurrentCheckState::Unknown), so
        //     `current_corr_pass("h")` is false on the pre-reset read
        //     and `proof_node_repairs_blocker("h")` returns true.
        //
        // Before the fix, `review_response_legal` called
        // `active_node_legal(.., &self.last_clean_live)` but the inner
        // `proof_node_repairs_blocker` ignored the snapshot and read
        // `self.live` / `self.corr_status` etc. — so the corr-Unknown
        // current state made the predicate accept "h" as legal even
        // though the post-restore state has no work for it. After
        // the fix (clone + apply_last_clean_reset, then check on
        // clone), the predicate sees the restored corr Pass and
        // correctly rejects.
        let mut state = base_state();
        // Seed the clean checkpoint: "h" present, Definition, corr Pass.
        state.live.present_nodes.insert("h".into());
        state.committed.present_nodes.insert("h".into());
        state.node_kinds.insert("h".into(), NodeKind::Definition);
        state
            .committed_node_kinds
            .insert("h".into(), NodeKind::Definition);
        state
            .live
            .corr_current_fingerprints
            .insert("h".into(), "ch".into());
        state.corr_status.insert("h".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("h".into(), "ch".into());
        mark_substantiveness_pass(&mut state, "h", "sub-h");
        state.commit_live();
        assert!(
            state.last_clean_mirrors_populated(),
            "test setup precondition: clean checkpoint must populate mirrors",
        );

        // Move into proof phase mid-cycle. Bump the *current* corr
        // fingerprint for "h" so current != approved → corr lane
        // becomes Unknown on the pre-reset read. That makes
        // `proof_node_repairs_blocker("h")` return true on
        // `self.live` / `self.corr_status` (the buggy scoping path).
        // The clean mirrors still hold the matching pre-bump
        // fingerprint, so post-reset corr re-passes.
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.active_node = Some("a".into());
        state
            .live
            .corr_current_fingerprints
            .insert("h".into(), "ch-bumped".into());

        // Sanity: the buggy scoping path would see this and accept "h".
        assert!(
            state.proof_node_repairs_blocker(&"h".into()),
            "test setup precondition: pre-reset proof_node_repairs_blocker \
             must return true on h (current corr Unknown), exercising the \
             buggy scoping path",
        );
        // Sanity: the clean snapshot itself doesn't carry "h" in open_nodes.
        assert!(
            !state.last_clean_live.open_nodes.contains("h"),
            "test setup precondition: clean snapshot has h closed",
        );

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            request.allowed_resets.contains(&ResetChoice::LastClean),
            "test setup precondition: LastClean must be a legal reset choice",
        );
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            next_active: Some("h".into()),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&response),
            "Continue+LastClean+next_active=h must be illegal: the only \
             reason a buggy scoping read would accept it is the pre-reset \
             corr-Unknown blocker against h, but post-reset that blocker \
             is gone (clean checkpoint corr Pass) and h has no other \
             work — must reject",
        );
    }

    #[test]
    fn review_response_with_reset_and_nonempty_blocker_actions_is_illegal() {
        // Reset is a pure rollback: the reviewer inherits blockers from the
        // rewound state and must not simultaneously adjudicate the current
        // state's blockers via task/reset blocker-action buckets.
        // Reject such responses at legal-check time.
        //
        // Option C (2026-06-04): override_blocker_ids is retired; the
        // test now exercises the task→Fail and reset paths only.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.cycles_since_clean = 4;
        state.has_ever_been_clean = true;
        // Seed a corr blocker so request.blockers is non-empty.
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        state.latest_corr_review_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_b = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "b"
                )
            })
            .cloned()
            .expect("missing corr blocker");

        // reset=LastClean + task_blockers non-empty → illegal.
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastClean,
            task_blockers: BTreeSet::from([blocker_b.clone()]),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&response));

        // reset=LastCommit + task_blockers non-empty → also illegal.
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.attempt = 2;
        state.invalid_attempt = true;
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::LastCommit,
            task_blockers: BTreeSet::from([blocker_b.clone()]),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&response));

        // reset=None + blocker routed through the task bucket on a
        // restructure mode → legal (the normal case). next_active=b
        // anchors the cone on the blocker-bearing node so the
        // authorized_nodes=[b] envelope check passes.
        state.retry_outcome_kind = RetryOutcomeKind::None;
        state.invalid_attempt = false;
        state.attempt = 1;
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::None,
            task_blockers: BTreeSet::from([blocker_b.clone()]),
            authorized_nodes: set(&["b"]),
            next_active: Some("b".into()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&response));
    }

    #[test]
    fn review_response_with_local_mode_and_task_blockers_is_illegal() {
        // Option C (2026-06-04): override_blocker_ids retired; the
        // `legal_no_tasks` branch below now uses no blocker action.
        // task_blocker_ids tells the worker "fix this blocker." But Local
        // mode authorizes the worker to edit only the active node's proof
        // body — not other nodes, not .tex files, not signatures. So a
        // non-empty task_blockers under Local mode hands the worker a
        // job it cannot legally do; the deterministic checker rejects
        // every cross-node or .tex edit. Reject the reviewer decision so
        // the kernel reissues and the reviewer can pick Restructure or
        // CoarseRestructure (modes that actually authorize the edits a
        // NodeCorr / Soundness task_blocker would need).
        //
        // Failure mode this guards: the reviewer sets next_mode=Local +
        // task_blockers=[NodeCorr on some other node]; the worker tries to
        // edit the .tex of that other node, hits the out-of-scope
        // rejection, and ends up reverting that work and returning
        // misleading comments claiming both tasks were done.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.cycles_since_clean = 4;
        state.has_ever_been_clean = true;
        state.deps.insert("a".into(), set(&["b"]));
        // Seed a corr blocker on a DIFFERENT node so request.blockers is
        // non-empty and the legality check has something to test against.
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        state.latest_corr_review_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_b = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "b"
                )
            })
            .cloned()
            .expect("missing corr blocker");

        // Local mode + task_blockers nonempty → illegal.
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let illegal = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_b.clone()]),
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&illegal));

        // Same response but next_mode=Restructure → legal (Restructure
        // authorizes the cross-node edits the task_blocker calls for).
        // The reviewer must also authorize the blocker-bearing node `b`
        // explicitly so the worker has edit permission on it.
        let legal_restructure = ReviewResponse {
            next_mode: TaskMode::Restructure,
            authorized_nodes: set(&["b"]),
            ..illegal.clone()
        };
        assert!(state.review_response_legal(&legal_restructure));

        // Same response but next_mode=CoarseRestructure → legal.
        let legal_coarse = ReviewResponse {
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: set(&["b"]),
            ..illegal.clone()
        };
        assert!(state.review_response_legal(&legal_coarse));

        // Empty task_blockers + Local mode → legal (Local without tasks
        // is fine — the worker just works on the active node).
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let _ = blocker_b; // unused after override→Pass retirement
        let legal_no_tasks = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&legal_no_tasks));
    }

    #[test]
    fn review_local_mode_with_soundness_task_blocker_is_legal() {
        // Local mode + task_blockers carve-out: Soundness is special.
        //
        // `needs_sound(node)` requires `live.open_nodes.contains(node)` —
        // so a sorry-free node has `needs_sound = false`, which makes
        // `current_sound_state` auto-Pass, which suppresses any Soundness
        // blocker from `global_blockers`. The cleanest response to a
        // Soundness blocker on the active node is therefore a
        // Lean-closure burst: close the proof, the blocker evaporates.
        // That edit (just the active node's `.lean` proof body) is
        // exactly Local mode's authorized scope.
        //
        // This test asserts `next_mode=Local + task_blockers=[soundness]`
        // is LEGAL. The companion strict-rule test
        // (`review_response_with_local_mode_and_task_blockers_is_illegal`)
        // verifies that NodeCorr / PaperFaithfulness / Substantiveness
        // task_blockers under Local remain illegal — those genuinely
        // require cross-file edits Local can't authorize.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.cycles_since_clean = 4;
        state.has_ever_been_clean = true;
        // Flip soundness on `a` from Pass to Fail. The fingerprints in
        // base_state already match (current == approved == "sa"), so the
        // Fail status is "current" — `global_blockers` will emit a
        // Soundness blocker on `a`.
        state.sound_status.insert("a".into(), SoundStatus::Fail);
        state.latest_sound_review_nodes = set(&["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let soundness_blocker_a = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::Soundness) if node == "a"
                )
            })
            .cloned()
            .expect("missing soundness blocker on a");

        // The carve-out: Local + Soundness task_blocker → LEGAL.
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let legal = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([soundness_blocker_a.clone()]),
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            must_close_active: true,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&legal),
            "Local + Soundness task_blocker should be legal (close-in-Lean auto-clears the blocker)"
        );

        // Sanity: same shape but with a NodeCorr blocker mixed in →
        // illegal. The carve-out applies ONLY to Soundness.
        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state.latest_corr_review_nodes = set(&["b"]);
        let request2 = issue_request_for_test(&mut state, RequestKind::Review);
        let corr_blocker_b = request2
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "b"
                )
            })
            .cloned()
            .expect("missing corr blocker on b");
        let soundness_blocker_a2 = request2
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::Soundness) if node == "a"
                )
            })
            .cloned()
            .expect("missing soundness blocker on a (round 2)");
        let illegal_mixed = ReviewResponse {
            request_id: request2.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([soundness_blocker_a2, corr_blocker_b]),
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            must_close_active: true,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&illegal_mixed),
            "Local + mixed (Soundness+NodeCorr) task_blockers should be illegal: NodeCorr needs cross-file edits Local can't authorize"
        );
    }

    #[test]
    fn review_task_blocker_must_be_in_proposed_worker_impact_region() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.live.present_nodes.insert("c".into());
        state.live.open_nodes.insert("c".into());
        state.committed.present_nodes.insert("c".into());
        state.committed.open_nodes.insert("c".into());
        state.deps.insert("a".into(), set(&["b"]));
        state.corr_status.insert("c".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("c");
        state
            .live
            .corr_current_fingerprints
            .insert("c".into(), "cc".into());
        mark_substantiveness_pass(&mut state, "c", "sub-c");

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_c = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "c"
                )
            })
            .cloned()
            .expect("missing corr blocker");

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let outside_scope = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_c.clone()]),
            next_active: Some("a".into()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            authorized_nodes: set(&["c"]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&outside_scope));

        let in_scope = ReviewResponse {
            next_active: Some("c".into()),
            ..outside_scope
        };
        assert!(state.review_response_legal(&in_scope));
    }

    /// Build a proof-formalization Reviewer state where P imports
    /// helpers B and R; both helpers carry NodeCorr blockers (the
    /// aggregate-focus shape that keeps P legal as `next_active`).
    /// Returns the state, B's NodeCorr blocker, and R's NodeCorr
    /// blocker — tests typically task one and override the other.
    fn proof_review_state_with_corr_blocker_on_helper() -> (ProtocolState, Blocker, Blocker) {
        let state = aggregate_sibling_state(BlockerKind::NodeCorr);
        let request = state.expected_request(0, RequestKind::Review);
        let blocker_b = request
            .blockers
            .iter()
            .find(|b| {
                matches!(
                    (&b.object, b.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "B"
                )
            })
            .cloned()
            .expect("missing corr blocker on B");
        let blocker_r = request
            .blockers
            .iter()
            .find(|b| {
                matches!(
                    (&b.object, b.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "R"
                )
            })
            .cloned()
            .expect("missing corr blocker on R");
        (state, blocker_b, blocker_r)
    }

    #[test]
    fn coarse_restructure_requires_explicit_authorized_nodes_inside_scope() {
        let (mut state, blocker_b, blocker_r) = proof_review_state_with_corr_blocker_on_helper();
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        // Empty authorized_nodes for CoarseRestructure → illegal.
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let empty = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_b.clone(), blocker_r.clone()]),
            next_active: Some("P".into()),
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: BTreeSet::new(),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&empty));

        // Authorized_nodes={B,R} (in envelope, covers both blockers) → legal.
        let in_scope = ReviewResponse {
            authorized_nodes: set(&["B", "R"]),
            ..empty.clone()
        };
        assert!(state.review_response_legal(&in_scope));

        // Authorized_nodes={Outside} (not in present_nodes) → illegal.
        let outside_present = ReviewResponse {
            authorized_nodes: BTreeSet::from([NodeId::from("Outside")]),
            ..empty.clone()
        };
        assert!(!state.review_response_legal(&outside_present));
    }

    #[test]
    fn task_blocker_must_be_in_explicit_authorized_nodes() {
        let (mut state, blocker_b, blocker_r) = proof_review_state_with_corr_blocker_on_helper();
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        // task_blockers on B and R but authorized_nodes={R} (does not
        // cover B) → illegal.
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let mismatched = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_b.clone(), blocker_r.clone()]),
            next_active: Some("P".into()),
            next_mode: TaskMode::Restructure,
            authorized_nodes: set(&["R"]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&mismatched));

        // Same but authorized_nodes covers both → legal.
        let aligned = ReviewResponse {
            authorized_nodes: set(&["B", "R"]),
            ..mismatched
        };
        assert!(state.review_response_legal(&aligned));
    }

    #[test]
    fn active_node_explicit_authorization_distinct_from_scope_anchor() {
        // The reviewer may pick next_active=P (anchor) without including
        // P in authorized_nodes — the worker sees authorized_nodes={B,R}
        // and may not edit P.
        let (mut state, blocker_b, blocker_r) = proof_review_state_with_corr_blocker_on_helper();
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_b, blocker_r]),
            next_active: Some("P".into()),
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: set(&["B", "R"]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&response));
        // Apply and check that pending_task captures the explicit list.
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply review response");
        let pending = outcome
            .state
            .pending_task
            .as_ref()
            .expect("pending task expected");
        assert_eq!(pending.authorized_nodes, set(&["B", "R"]));
        assert_eq!(
            outcome.state.current_worker_authorized_nodes(),
            set(&["B", "R"]),
            "worker authorization is the explicit list, not the envelope"
        );
    }

    #[test]
    fn local_mode_rejects_non_empty_authorized_nodes() {
        // Local mode must not be combined with cross-node existing-node
        // authorization — Local is by-design active-node-only.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let illegal = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            authorized_nodes: set(&["a"]),
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&illegal));
        let legal_empty = ReviewResponse {
            authorized_nodes: BTreeSet::new(),
            ..illegal
        };
        assert!(state.review_response_legal(&legal_empty));
    }

    #[test]
    fn coarse_scope_authorized_nodes_may_be_a_proper_subset() {
        // Single-helper-blocker shape: only B carries a blocker.
        // impact_region(B) ⊇ {B, P} via reverse-deps; authorized_nodes={B}
        // is a legitimate proper subset of the envelope.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.live.present_nodes.insert("c".into());
        state.live.open_nodes.insert("c".into());
        state.committed.present_nodes.insert("c".into());
        state.committed.open_nodes.insert("c".into());
        state.deps.insert("a".into(), set(&["b"]));
        state.corr_status.insert("c".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("c");
        state
            .live
            .corr_current_fingerprints
            .insert("c".into(), "cc".into());
        mark_substantiveness_pass(&mut state, "c", "sub-c");

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_c = request
            .blockers
            .iter()
            .find(|b| {
                matches!(
                    (&b.object, b.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "c"
                )
            })
            .cloned()
            .expect("missing corr blocker");
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_c]),
            next_active: Some("c".into()),
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: set(&["c"]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "authorized_nodes={{c}} (a proper subset of impact_region(c)) must be legal"
        );
    }

    /// Build a proof-formalization Reviewer state with an extra
    /// closed parent `P` importing two open helper proof nodes `B` and
    /// `R` (`deps[P] = {B, R}`), and seed `B` and `R` with a node-bound
    /// blocker of the chosen kind. Sibling helpers carry the only live
    /// blockers; `P` itself is clean. This is the cycle-380 shape the
    /// aggregate-focus rule was added to fix.
    ///
    /// The helper sets `node_difficulty` / `easy_attempts` for the new
    /// nodes so `state.validate()` passes after the review is applied
    /// (the `apply_event` exit gate runs `validate`).
    fn aggregate_sibling_state(blocker_kind: BlockerKind) -> ProtocolState {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;

        for n in ["P", "B", "R"] {
            state.live.present_nodes.insert(n.into());
            state.committed.present_nodes.insert(n.into());
            state.proof_nodes.insert(n.into());
            state.committed_proof_nodes.insert(n.into());
            state.node_rank.insert(n.into(), 10);
            state.node_difficulty.insert(n.into(), NodeDifficulty::Hard);
            state.easy_attempts.insert(n.into(), 0);
        }
        // P imports B and R; B and R have no inner deps.
        state.deps.insert("P".into(), set(&["B", "R"]));
        state.committed_deps.insert("P".into(), set(&["B", "R"]));

        // P closed; B and R open (so `needs_sound` fires for the
        // soundness variant — and so the reviewer might naturally want
        // to repair both helpers' lanes in one Restructure cycle).
        state.live.open_nodes.insert("B".into());
        state.live.open_nodes.insert("R".into());
        state.committed.open_nodes.insert("B".into());
        state.committed.open_nodes.insert("R".into());

        // All three nodes start with Pass on every node-bound lane.
        // The override below seeds the chosen lane to Unknown on B/R
        // only — leaving P clean (no own blocker). This forces the
        // reviewer's only path to a parent-focused task to go through
        // the new aggregate clause, not the existing
        // `proof_node_repairs_blocker` clause.
        for n in ["P", "B", "R"] {
            state.corr_status.insert(n.into(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert(n.into(), format!("c{}", n).into());
            state
                .live
                .corr_current_fingerprints
                .insert(n.into(), format!("c{}", n).into());
            mark_substantiveness_pass(&mut state, n, &format!("sub-{}", n));
            state.sound_status.insert(n.into(), SoundStatus::Pass);
            state
                .sound_approved_fingerprints
                .insert(n.into(), format!("s{}", n).into());
            state
                .live
                .sound_current_fingerprints
                .insert(n.into(), format!("s{}", n).into());
        }

        match blocker_kind {
            BlockerKind::NodeCorr => {
                for n in ["B", "R"] {
                    state.corr_status.insert(n.into(), CorrStatus::Unknown);
                    state
                        .corr_approved_fingerprints
                        .remove(&NodeId::from(n.to_string()));
                    state.latest_corr_review_nodes.insert(n.into());
                }
            }
            BlockerKind::Soundness => {
                for n in ["B", "R"] {
                    state.sound_status.insert(n.into(), SoundStatus::Unknown);
                    state
                        .sound_approved_fingerprints
                        .remove(&NodeId::from(n.to_string()));
                    state.latest_sound_review_nodes.insert(n.into());
                }
            }
            BlockerKind::Substantiveness => {
                for n in ["B", "R"] {
                    state
                        .substantiveness_status
                        .insert(n.into(), CorrStatus::Unknown);
                    state
                        .substantiveness_approved_fingerprints
                        .remove(&NodeId::from(n.to_string()));
                    state.latest_substantiveness_review_nodes.insert(n.into());
                }
            }
            other => panic!(
                "unsupported blocker kind for aggregate sibling test: {:?}",
                other
            ),
        }

        state
    }

    fn run_aggregate_sibling_focus_test(blocker_kind: BlockerKind) {
        let mut state = aggregate_sibling_state(blocker_kind);

        let request = issue_request_for_test(&mut state, RequestKind::Review);

        // The aggregate clause exposes the closed parent `P`; the
        // direct blocker-bearing helpers `B` and `R` remain legal via
        // the existing `proof_node_repairs_blocker` clause (and via
        // `open_nodes` membership — they're open helpers).
        let p = NodeId::from("P".to_string());
        let b = NodeId::from("B".to_string());
        let r = NodeId::from("R".to_string());
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&p),
            "P (closed common importer) must be in kernel_hinted_next_active_nodes for {:?}",
            blocker_kind,
        );
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&b),
            "B (own blocker) must remain legal for {:?}",
            blocker_kind,
        );
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&r),
            "R (own blocker) must remain legal for {:?}",
            blocker_kind,
        );

        // Continue + next_active=P + Restructure with all blockers
        // task'd. Worker scope = impact_region(P) = {P, B, R}, so
        // both sibling node-bound blockers fall inside scope. Other
        // base_state nodes have no live blockers, so request.blockers
        // is exactly {B's blocker, R's blocker}.
        let task_blockers: BTreeSet<Blocker> = request.blockers.clone();
        assert_eq!(
            task_blockers.len(),
            2,
            "expected exactly two sibling blockers for {:?}, got {:?}",
            blocker_kind,
            task_blockers,
        );
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: task_blockers.clone(),
            next_active: Some(p.clone()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            authorized_nodes: BTreeSet::from([b.clone(), r.clone()]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "Continue+next_active=P+Restructure must be legal for {:?}",
            blocker_kind,
        );

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply review response");

        // Relegalization must keep the aggregate parent as the active
        // focus (the audit-driven amendment): the new
        // `active_node_legal` clause makes `P` legal in
        // `relegalize_active_fields`, so the reviewer's choice
        // survives the post-application sweep.
        assert_eq!(
            outcome.state.active_node,
            Some(p.clone()),
            "active_node must remain P after relegalization for {:?}",
            blocker_kind,
        );
        let pending = outcome
            .state
            .pending_task
            .as_ref()
            .expect("pending_task set after Continue");
        assert_eq!(
            pending.node,
            Some(p),
            "pending_task.node for {:?}",
            blocker_kind,
        );
        assert_eq!(
            pending.mode,
            TaskMode::Restructure,
            "pending_task.mode for {:?}",
            blocker_kind,
        );
        assert_eq!(
            pending.task_blockers, task_blockers,
            "pending_task.task_blockers must carry both sibling blockers for {:?}",
            blocker_kind,
        );
    }

    #[test]
    fn aggregate_focus_repairs_node_corr_siblings() {
        run_aggregate_sibling_focus_test(BlockerKind::NodeCorr);
    }

    #[test]
    fn aggregate_focus_repairs_soundness_siblings() {
        run_aggregate_sibling_focus_test(BlockerKind::Soundness);
    }

    #[test]
    fn aggregate_focus_repairs_substantiveness_siblings() {
        run_aggregate_sibling_focus_test(BlockerKind::Substantiveness);
    }

    fn add_reviewer_4596_node(state: &mut ProtocolState, name: &str, open: bool) {
        let node = NodeId::from(name);
        state.live.present_nodes.insert(node.clone());
        state.committed.present_nodes.insert(node.clone());
        if open {
            state.live.open_nodes.insert(node.clone());
            state.committed.open_nodes.insert(node.clone());
        }
        state.proof_nodes.insert(node.clone());
        state.committed_proof_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), NodeKind::Proof);
        state
            .committed_node_kinds
            .insert(node.clone(), NodeKind::Proof);
        state.node_rank.insert(node.clone(), 10);
        state
            .node_difficulty
            .insert(node.clone(), NodeDifficulty::Hard);
        let corr_fp = format!("corr-{name}");
        let sound_fp = format!("sound-{name}");
        let subst_fp = format!("subst-{name}");
        state.corr_status.insert(node.clone(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(node.clone(), corr_fp.clone().into());
        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), corr_fp.into());
        state.sound_status.insert(node.clone(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert(node.clone(), sound_fp.clone().into());
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), sound_fp.into());
        mark_substantiveness_pass(state, name, &subst_fp);
    }

    fn reviewer_4596_scope_state() -> ProtocolState {
        let mut state = base_state();
        state.configured_targets.clear();
        state.target_claims.clear();
        state.committed_target_claims.clear();
        state.deps.clear();
        state.committed_deps.clear();
        state.live.coverage.clear();
        state.live.present_nodes.clear();
        state.live.open_nodes.clear();
        state.live.target_fingerprints.clear();
        state.live.paper_current_fingerprints.clear();
        state.live.corr_current_fingerprints.clear();
        state.live.sound_current_fingerprints.clear();
        state.live.substantiveness_current_fingerprints.clear();
        state.committed = state.live.clone();
        state.proof_nodes.clear();
        state.committed_proof_nodes.clear();
        state.node_kinds.clear();
        state.committed_node_kinds.clear();
        state.node_rank.clear();
        state.node_difficulty.clear();
        state.corr_status.clear();
        state.corr_approved_fingerprints.clear();
        state.sound_status.clear();
        state.sound_approved_fingerprints.clear();
        state.substantiveness_status.clear();
        state.substantiveness_approved_fingerprints.clear();
        state.paper_status.clear();
        state.paper_approved_fingerprints.clear();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 966;
        state.retry_outcome_kind = RetryOutcomeKind::NeedsRestructure;
        state.attempt = 1;
        state.human_input_outstanding = true;
        state.active_node = Some("FixedSetHistoryCellRedBranchTranscriptCells".into());
        for (name, open) in [
            ("FixedSetHistoryCellFixedCountBlueWeightSum", true),
            ("FixedSetHistoryCellRedBranchTranscriptCells", true),
            ("FixedSetConditionalExposureCellBound", false),
            ("FixedSetExposureCellProductLaw", false),
            ("FixedSetExposureHistoryCylinder", false),
            ("FixedSetTerminalSupportClassification", false),
            ("TwoBiteTerminalCoordinateUniverse", false),
        ] {
            add_reviewer_4596_node(&mut state, name, open);
        }
        state.deps.insert(
            "FixedSetConditionalExposureCellBound".into(),
            set(&["FixedSetExposureCellProductLaw"]),
        );
        state.deps.insert(
            "FixedSetExposureCellProductLaw".into(),
            set(&["FixedSetExposureHistoryCylinder"]),
        );
        state.deps.insert(
            "FixedSetExposureHistoryCylinder".into(),
            set(&["FixedSetTerminalSupportClassification"]),
        );
        state.deps.insert(
            "FixedSetTerminalSupportClassification".into(),
            set(&["TwoBiteTerminalCoordinateUniverse"]),
        );
        state.deps.insert(
            "FixedSetHistoryCellRedBranchTranscriptCells".into(),
            set(&["TwoBiteTerminalCoordinateUniverse"]),
        );
        state.committed_deps = state.deps.clone();
        state
    }

    #[test]
    fn reviewer_4596_order_source_scope_can_use_unhinted_anchor() {
        let mut state = reviewer_4596_scope_state();
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let anchor = NodeId::from("FixedSetConditionalExposureCellBound");
        assert!(
            !request.kernel_hinted_next_active_nodes.contains(&anchor),
            "4596 regression setup: order-source anchor must be outside kernel hints"
        );
        assert_eq!(
            request.kernel_hinted_next_active_nodes,
            set(&[
                "FixedSetHistoryCellFixedCountBlueWeightSum",
                "FixedSetHistoryCellRedBranchTranscriptCells"
            ])
        );

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let authorized_nodes = set(&[
            "FixedSetConditionalExposureCellBound",
            "FixedSetExposureCellProductLaw",
            "FixedSetExposureHistoryCylinder",
            "FixedSetTerminalSupportClassification",
        ]);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 966,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some(anchor.clone()),
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: authorized_nodes.clone(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };

        assert!(
            state.review_response_legal(&response),
            "reviewer 4596's order-source repair should be routable once proof next_active hints are advisory"
        );
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply reviewer 4596 order-source route");
        assert_eq!(outcome.state.active_node, Some(anchor));
        let pending = outcome.state.pending_task.as_ref().expect("pending task");
        assert_eq!(pending.mode, TaskMode::CoarseRestructure);
        assert_eq!(pending.authorized_nodes, authorized_nodes);
    }

    #[test]
    fn reviewer_4596_bad_scope_gets_minimal_anchor_diagnostic() {
        let mut state = reviewer_4596_scope_state();
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 966,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("FixedSetHistoryCellRedBranchTranscriptCells".into()),
            next_mode: TaskMode::CoarseRestructure,
            authorized_nodes: set(&[
                "FixedSetConditionalExposureCellBound",
                "FixedSetExposureCellProductLaw",
                "FixedSetExposureHistoryCylinder",
                "FixedSetTerminalSupportClassification",
            ]),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };

        assert!(!state.review_response_legal(&response));
        let reasons = state.review_response_rejection_reasons(&response);
        let joined = reasons.join("\n");
        assert!(
            joined.contains("Minimal present next_active example"),
            "expected actionable scope-anchor diagnostic, got {joined}"
        );
        assert!(
            joined.contains("FixedSetConditionalExposureCellBound"),
            "expected diagnostic to name the 4596 order-source anchor, got {joined}"
        );
    }

    #[test]
    fn reviewer_4596_need_input_response_still_valid() {
        let mut state = reviewer_4596_scope_state();
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 966,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::NeedInput,
            next_active: None,
            next_mode: TaskMode::Local,
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "the actual 4596 NeedInput escape remains legal"
        );
    }

    #[test]
    fn proof_continue_requires_next_active_coarse_when_anchor_none() {
        // Phase entry seeds `active_coarse_node`, but legacy state, a
        // stale-anchor recovery in `start_cycle`, or a manual reset can
        // leave it None. In any of those cases a ProofFormalization
        // Continue response that doesn't set `next_active_coarse` would
        // sail past the cone-narrowing in
        // `request_kernel_hinted_next_active_nodes` (no anchor = no
        // narrowing) and let the reviewer roam the full open-set
        // instead of being locked to the active coarse cone. Reject so
        // the reviewer has to commit to a coarse anchor.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 7;
        state.active_node = Some("a".into());
        state.approved_targets.configured_targets = state.configured_targets.clone();
        state.approved_targets.coverage = state.live.coverage.clone();
        state.coarse_dag_nodes = state.live.present_nodes.clone();
        state.active_coarse_node = None;
        state.proof_edit_mode = ProofEditMode::Local;

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            !request.kernel_hinted_next_active_coarse_nodes.is_empty(),
            "anchor=None must surface candidate coarse anchors"
        );

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let bad_response = ReviewResponse {
            request_id: request.id,
            cycle: 7,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::Local,
            next_active_coarse: None,
            next_worker_context_mode: WorkerContextMode::Resume,
            allow_new_obligations: false,
            must_close_active: true,
            paper_focus_ranges: paper_focus_ranges.clone(),
            paper_grounding: paper_grounding.clone(),
            ..ReviewResponse::default()
        };
        let bad_reasons = state.review_response_rejection_reasons(&bad_response);
        let bad_joined = bad_reasons.join("\n");
        assert!(
            bad_reasons
                .iter()
                .any(|r| r.contains("next_active_coarse when active_coarse_node is None")),
            "rejection should name the anchor-required rule, got: {bad_joined}"
        );

        let good_response = ReviewResponse {
            next_active_coarse: Some("a".into()),
            ..bad_response.clone()
        };
        let good_reasons = state.review_response_rejection_reasons(&good_response);
        assert!(
            good_reasons.is_empty(),
            "Continue with next_active_coarse set to a hinted candidate must be legal, got reasons: {good_reasons:?}"
        );
    }

    #[test]
    fn proof_continue_requires_anchor_advance_on_clean_unlock() {
        // When the active coarse anchor reaches shallow-coarse-closure
        // (its only open deps are themselves coarse-DAG nodes, which the
        // shallow-closure check skips) with no global blockers, the lock
        // is open on a clean unlock. The reviewer must NOT preserve the
        // closed anchor by leaving `next_active_coarse = None` and
        // piggybacking on a next-coarse-node in the old cone — that just
        // relabels work that belongs to the next anchor. Force the
        // reviewer to advance.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 11;
        state.approved_targets.configured_targets = state.configured_targets.clone();
        state.approved_targets.coverage = state.live.coverage.clone();
        // base_state has present_nodes = {a, b}, proof_nodes = {a},
        // open_nodes = {a, b}. Make both proof nodes; close anchor a;
        // leave b open. Then a's only dep is b (also coarse) so
        // shallow-coarse-closure(a) holds and the lock opens.
        state.proof_nodes = set(&["a", "b"]);
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.live.open_nodes = set(&["b"]);
        state.committed.open_nodes = state.live.open_nodes.clone();
        state.coarse_dag_nodes = set(&["a", "b"]);
        state.active_coarse_node = Some("a".into());
        state.deps.insert("a".into(), set(&["b"]));
        state.committed_deps.insert("a".into(), set(&["b"]));
        state.active_node = None;
        state.proof_edit_mode = ProofEditMode::Local;
        // b stays open while sound has already passed during
        // theorem-stating phase — this is the regime in which
        // `active_coarse_change_allowed` returns true while the next
        // anchor candidate still has work to do.
        state.sound_status.insert("b".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("b".into(), "sb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("b".into(), "sb".into());

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            request
                .kernel_hinted_next_active_coarse_nodes
                .contains(&NodeId::from("b".to_string())),
            "clean unlock should surface b as anchor candidate"
        );
        assert!(
            !request.coarse_anchor_starvation_unlocked,
            "this is a clean unlock, not starvation"
        );

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let piggybacked = ReviewResponse {
            request_id: request.id,
            cycle: 11,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("b".into()),
            next_mode: TaskMode::Local,
            next_active_coarse: None,
            next_worker_context_mode: WorkerContextMode::Resume,
            allow_new_obligations: false,
            must_close_active: true,
            paper_focus_ranges: paper_focus_ranges.clone(),
            paper_grounding: paper_grounding.clone(),
            ..ReviewResponse::default()
        };
        let bad = state.review_response_rejection_reasons(&piggybacked);
        let joined = bad.join("\n");
        assert!(
            bad.iter()
                .any(|r| r.contains("shallow-coarse-closed (clean unlock)")),
            "rejection should cite the clean-unlock rule, got: {joined}"
        );

        let advanced = ReviewResponse {
            next_active_coarse: Some("b".into()),
            ..piggybacked.clone()
        };
        let advanced_reasons = state.review_response_rejection_reasons(&advanced);
        assert!(
            advanced_reasons.is_empty(),
            "advancing the anchor to a hinted candidate must be legal, got: {advanced_reasons:?}"
        );
    }

    #[test]
    fn direct_importer_of_substantiveness_blocker_is_legal_focus() {
        // Option C (2026-06-04): override_blocker_ids retired. The
        // `mixed_paper_override` sub-assertion below previously checked
        // that a substantiveness task could coexist with an adjudicated
        // paper override; with override→Pass removed, the assertion is
        // now flipped — any non-empty override_blockers is rejected by
        // the always-empty `allowed_override_blockers` gate.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;

        for n in ["G", "P", "B"] {
            state.live.present_nodes.insert(n.into());
            state.committed.present_nodes.insert(n.into());
            state.proof_nodes.insert(n.into());
            state.committed_proof_nodes.insert(n.into());
            state.node_rank.insert(n.into(), 10);
            state.node_difficulty.insert(n.into(), NodeDifficulty::Hard);
            state.easy_attempts.insert(n.into(), 0);
            state.corr_status.insert(n.into(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert(n.into(), format!("c{}", n).into());
            state
                .live
                .corr_current_fingerprints
                .insert(n.into(), format!("c{}", n).into());
            mark_substantiveness_pass(&mut state, n, &format!("sub-{}", n));
            state.sound_status.insert(n.into(), SoundStatus::Pass);
            state
                .sound_approved_fingerprints
                .insert(n.into(), format!("s{}", n).into());
            state
                .live
                .sound_current_fingerprints
                .insert(n.into(), format!("s{}", n).into());
        }
        state.deps.insert("P".into(), set(&["B"]));
        state.deps.insert("G".into(), set(&["P"]));
        state.committed_deps.insert("P".into(), set(&["B"]));
        state.committed_deps.insert("G".into(), set(&["P"]));

        state
            .substantiveness_status
            .insert("B".into(), CorrStatus::Unknown);
        state
            .substantiveness_approved_fingerprints
            .remove(&NodeId::from("B".to_string()));
        state.latest_substantiveness_review_nodes.insert("B".into());

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let p = NodeId::from("P".to_string());
        let g = NodeId::from("G".to_string());
        let b = NodeId::from("B".to_string());
        assert!(state.active_node_legal(Some(&p), &state.live));
        assert!(!state.active_node_legal(Some(&g), &state.live));
        assert!(state.active_node_legal(Some(&b), &state.live));
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&p),
            "direct importer P must be exposed for substantiveness repair"
        );
        assert!(
            !request.kernel_hinted_next_active_nodes.contains(&g),
            "transitive-only importer G must not be newly exposed"
        );
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&b),
            "blocked node B remains legal via its own blocker"
        );

        let task_blockers: BTreeSet<Blocker> = request.blockers.clone();
        assert_eq!(task_blockers.len(), 1);
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: task_blockers.clone(),
            next_active: Some(p.clone()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            authorized_nodes: BTreeSet::from([b.clone()]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "direct importer P should scope the B substantiveness task"
        );
        let coarse_response = ReviewResponse {
            next_mode: TaskMode::CoarseRestructure,
            ..response.clone()
        };
        assert!(
            state.review_response_legal(&coarse_response),
            "direct importer P should also be legal for coarse restructure"
        );
        let local_response = ReviewResponse {
            next_mode: TaskMode::Local,
            authorized_nodes: BTreeSet::new(),
            ..response.clone()
        };
        assert!(
            !state.review_response_legal(&local_response),
            "local mode must not carry task blockers"
        );
        let no_active_response = ReviewResponse {
            next_active: None,
            ..response.clone()
        };
        assert!(
            !state.review_response_legal(&no_active_response),
            "restructure with task blockers must nominate next_active"
        );

        let mut mixed = state.clone();
        mixed.paper_status.insert("t".into(), CorrStatus::Unknown);
        mixed
            .paper_approved_fingerprints
            .remove(&TargetId::from("t".to_string()));
        mixed.latest_paper_review_targets.insert("t".into());
        let mixed_request = issue_request_for_test(&mut mixed, RequestKind::Review);
        assert!(mixed_request.kernel_hinted_next_active_nodes.contains(&p));
        let subst_blocker = mixed_request
            .blockers
            .iter()
            .find(|blocker| {
                blocker.kind == BlockerKind::Substantiveness
                    && matches!(&blocker.object, BlockerObject::Node { node } if node == &b)
            })
            .expect("substantiveness blocker")
            .clone();
        let paper_blocker = mixed_request
            .blockers
            .iter()
            .find(|blocker| blocker.kind == BlockerKind::PaperFaithfulness)
            .expect("paper blocker")
            .clone();
        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let mixed_both_task = ReviewResponse {
            request_id: mixed_request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([subst_blocker.clone(), paper_blocker.clone()]),
            next_active: Some(p.clone()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            authorized_nodes: BTreeSet::from([b.clone()]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            !mixed.review_response_legal(&mixed_both_task),
            "target-bound paper blocker outside P's impact region must not be taskable"
        );
        // Option C (2026-06-04): override_blocker_ids retired. The
        // previous sub-assertion (substantiveness task coexists with
        // a paper override) is gone — a non-empty override_blockers
        // is now always illegal.
        let mixed_paper_override = ReviewResponse {
            task_blockers: BTreeSet::from([subst_blocker]),
            override_blockers: BTreeSet::from([paper_blocker]),
            ..mixed_both_task
        };
        assert!(
            !mixed.review_response_legal(&mixed_paper_override),
            "any non-empty override_blockers is now illegal under Option C (override→Pass retired)"
        );

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply review response");
        assert_eq!(outcome.state.active_node, Some(p.clone()));
        let pending = outcome.state.pending_task.as_ref().expect("pending task");
        assert_eq!(pending.node, Some(p));
        assert_eq!(pending.task_blockers, task_blockers);
    }

    #[test]
    fn aggregate_focus_skipped_when_target_blocker_present() {
        // Mixed-blocker negative: even when two sibling node-bound
        // blockers (B, R) would otherwise expose their common parent
        // P, the presence of any target-bound blocker (a live
        // PaperFaithfulness on `t`) collapses the aggregate candidate
        // set to empty. Without this guard, the worker could be
        // tasked with cross-lane work whose downstream scope rules
        // (`task_blockers_outside_review_worker_scope`'s target-cone
        // disjunction route) might not authorize the necessary edits.
        let mut state = aggregate_sibling_state(BlockerKind::Soundness);
        // Seed an unrelated PaperFaithfulness blocker by flipping
        // paper status for the configured target. base_state's node
        // "a" already covers "t", so the paper blocker has well-formed
        // coverage (not the empty-coverage edge case).
        state.paper_status.insert("t".into(), CorrStatus::Unknown);
        state
            .paper_approved_fingerprints
            .remove(&TargetId::from("t".to_string()));

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let p = NodeId::from("P".to_string());
        assert!(
            !request.kernel_hinted_next_active_nodes.contains(&p),
            "P must NOT be in kernel_hinted_next_active_nodes when a target-bound \
             blocker is live: aggregate-focus rule deliberately collapses to \
             empty in mixed-blocker states. allowed = {:?}",
            request.kernel_hinted_next_active_nodes,
        );
        // Direct sibling helpers are still legal via the open clause /
        // their own soundness blocker — the aggregate guard only
        // suppresses the parent affordance.
        assert!(
            request
                .kernel_hinted_next_active_nodes
                .contains(&NodeId::from("B".to_string())),
            "B must remain legal as next_active even with a target-bound blocker present",
        );
    }

    #[test]
    fn aggregate_focus_returns_minimal_common_importer() {
        // Top imports P imports {B, R}. Both Top and P cover the
        // node-bound blocker set, but P's dep-closure is a strict
        // subset of Top's, so the minimal-cover filter retains P and
        // drops Top.
        let mut state = aggregate_sibling_state(BlockerKind::Soundness);
        state.live.present_nodes.insert("Top".into());
        state.committed.present_nodes.insert("Top".into());
        state.proof_nodes.insert("Top".into());
        state.committed_proof_nodes.insert("Top".into());
        state.node_rank.insert("Top".into(), 11);
        state
            .node_difficulty
            .insert("Top".into(), NodeDifficulty::Hard);
        state.easy_attempts.insert("Top".into(), 0);
        // Top is closed (no sorry). All node-bound lanes Pass so Top
        // carries no own blocker.
        state.corr_status.insert("Top".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("Top".into(), "cTop".into());
        state
            .live
            .corr_current_fingerprints
            .insert("Top".into(), "cTop".into());
        mark_substantiveness_pass(&mut state, "Top", "sub-Top");
        state.sound_status.insert("Top".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("Top".into(), "sTop".into());
        state
            .live
            .sound_current_fingerprints
            .insert("Top".into(), "sTop".into());
        // Top imports P → dep_closure({Top}) = {Top, P, B, R} ⊃ {P, B, R}.
        state.deps.insert("Top".into(), set(&["P"]));
        state.committed_deps.insert("Top".into(), set(&["P"]));

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let p = NodeId::from("P".to_string());
        let top = NodeId::from("Top".to_string());
        assert!(
            request.kernel_hinted_next_active_nodes.contains(&p),
            "P (minimal common importer) must be in kernel_hinted_next_active_nodes",
        );
        assert!(
            !request.kernel_hinted_next_active_nodes.contains(&top),
            "Top (broader importer) must NOT be in kernel_hinted_next_active_nodes: \
             P's cone is a strict subset, so Top is dropped by the \
             minimal-cover filter. allowed = {:?}",
            request.kernel_hinted_next_active_nodes,
        );
    }

    #[test]
    fn review_paper_task_blocker_must_intersect_proposed_worker_scope() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("b".into());
        state.paper_status.insert("t".into(), CorrStatus::Unknown);
        state.paper_approved_fingerprints.remove("t");

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_t = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Target { target }, BlockerKind::PaperFaithfulness) if target == "t"
                )
            })
            .cloned()
            .expect("missing paper blocker");

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let outside_scope = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_t.clone()]),
            next_active: Some("b".into()),
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            authorized_nodes: set(&["b"]),
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&outside_scope));

        let in_scope = ReviewResponse {
            next_active: Some("a".into()),
            authorized_nodes: set(&["a"]),
            ..outside_scope
        };
        assert!(state.review_response_legal(&in_scope));
    }

    #[test]
    fn theorem_global_allows_empty_coverage_paper_task_blocker() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = None;
        state.target_claims.clear();
        state.live.coverage.insert("t".into(), BTreeSet::new());
        state.paper_status.insert("t".into(), CorrStatus::Pass);

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_t = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Target { target }, BlockerKind::PaperFaithfulness) if target == "t"
                )
            })
            .cloned()
            .expect("missing empty-coverage paper blocker");

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_t]),
            next_active: None,
            next_mode: TaskMode::Global,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&response));
    }

    #[test]
    fn proof_continue_no_next_active_with_restructure_mode_is_illegal() {
        // (Audit follow-up.) The reviewer cannot send Continue + next_active=None
        // + Restructure (or CoarseRestructure) mode. The engine's silent-downgrade
        // branch (`engine.rs:1500-1508`) maps `proof_edit_mode` to Local whenever
        // `state.active_node.is_none()`, regardless of the response's `next_mode`.
        // If the reviewer requested Restructure but omitted next_active, the
        // worker would receive a Local-mode task (carrying the reviewer's
        // task_blockers) it cannot legally address. The legality gate now rejects
        // the response so the kernel reissues and the reviewer must either pick a
        // focus or downgrade explicitly.
        //
        // Option C (2026-06-04): the previous "uses override_blockers" sub-case
        // is now redundant (any override_blockers is illegal under the
        // always-empty `allowed_override_blockers` gate). The last
        // sub-assertion is rewritten to use `reset_blockers` instead.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.has_ever_been_clean = true;
        // active_node is None — the orphan/retry shape this rule guards.
        state.active_node = None;
        // Seed a corr blocker on a different node so request.blockers is
        // non-empty (and the reviewer might plausibly want Restructure).
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        state.latest_corr_review_nodes = set(&["b"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_b = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "b"
                )
            })
            .cloned()
            .expect("missing corr blocker");

        // Continue + next_active=None + Restructure + task_blockers → illegal.
        let illegal_restructure = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::from([blocker_b.clone()]),
            next_active: None,
            next_mode: TaskMode::Restructure,
            next_worker_context_mode: WorkerContextMode::Resume,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&illegal_restructure),
            "Continue+next_active=None+Restructure must be illegal: engine would silently downgrade to Local",
        );

        // Same but CoarseRestructure → also illegal for the same reason.
        let illegal_coarse = ReviewResponse {
            next_mode: TaskMode::CoarseRestructure,
            ..illegal_restructure.clone()
        };
        assert!(
            !state.review_response_legal(&illegal_coarse),
            "Continue+next_active=None+CoarseRestructure must be illegal: engine would silently downgrade to Local",
        );

        // Even with no task_blockers / no other blocker actions,
        // Restructure-without-focus is incoherent. (Option C: pre-retirement
        // this sub-case used an override_blocker to keep the per-blocker
        // routing exercised; that path is gone.)
        let _ = blocker_b; // unused after override→Pass retirement
        let illegal_restructure_no_blockers = ReviewResponse {
            task_blockers: BTreeSet::new(),
            next_mode: TaskMode::Restructure,
            ..illegal_restructure.clone()
        };
        assert!(
            !state.review_response_legal(&illegal_restructure_no_blockers),
            "Continue+next_active=None+Restructure must be illegal regardless of task_blockers",
        );
    }

    #[test]
    fn proof_continue_no_next_active_local_mode_no_blockers_is_legal() {
        // Sanity counterpart to the rule above: the genuine orphan-cleanup
        // retry shape — Continue+next_active=None+Local+no task_blockers —
        // remains legal. This is the path engine.rs:1526+ takes when the
        // reviewer is letting the kernel run orphan cleanup or simply
        // continuing without a focus, with no blocker work to do.
        //
        // Option C (2026-06-04): pre-retirement the legal response routed
        // the Unknown corr blocker through the override bucket so
        // task_blockers could stay empty; with override→Pass retired the
        // test now seeds NO blockers and asserts the same shape is legal.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.has_ever_been_clean = true;
        state.active_node = None;
        let request = issue_request_for_test(&mut state, RequestKind::Review);

        let (paper_focus_ranges, paper_grounding) = test_paper_grounding();
        let legal = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            task_blockers: BTreeSet::new(),
            next_active: None,
            next_mode: TaskMode::Local,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges,
            paper_grounding,
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&legal),
            "Continue+next_active=None+Local+no task_blockers must remain legal (orphan/retry path)",
        );
    }

    #[test]
    fn validate_rejects_proof_pending_task_with_blockers_but_no_node() {
        // Defense-in-depth invariant (audit follow-up): even if some future
        // engine path bypassed the legality gate, a `ProtocolState::validate()`
        // sweep must reject a proof-phase pending task with non-empty
        // task_blockers and no focus node, or with a Local mode (which can't
        // address cross-node / signature blockers).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.has_ever_been_clean = true;
        state.active_node = None;
        // base_state doesn't populate node_difficulty / easy_attempts; validate()
        // demands an entry per present_node. Seed both nodes with default
        // difficulty so unrelated invariants don't fire before our check.
        for n in ["a", "b"] {
            state.node_difficulty.insert(n.into(), NodeDifficulty::Hard);
            state.easy_attempts.insert(n.into(), 0);
        }
        // Seed a corr blocker on "b" so global_blockers is non-empty.
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        let blocker = state
            .global_blockers()
            .into_iter()
            .find(|b| {
                matches!(
                    (&b.object, b.kind),
                    (BlockerObject::Node { node }, BlockerKind::NodeCorr) if node == "b"
                )
            })
            .expect("expected corr blocker on b");

        // Pending task with task_blockers but no focus node — must reject.
        // proof_edit_mode = Restructure so the upstream "mode must match
        // current mode" check passes and our new invariant fires.
        state.proof_edit_mode = ProofEditMode::Restructure;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::from([blocker.clone()]),
            node: None,
            mode: TaskMode::Restructure,
            ..PendingTask::default()
        });
        let err = state.validate().expect_err(
            "validate must reject proof-phase pending task with task_blockers and no focus node",
        );
        assert!(
            err.contains("focus node"),
            "unexpected validate error: {err}",
        );

        // Pending task with task_blockers + focus node + Local mode → also reject.
        state.active_node = Some("a".into());
        state.proof_edit_mode = ProofEditMode::Local;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::from([blocker]),
            node: Some("a".into()),
            mode: TaskMode::Local,
            ..PendingTask::default()
        });
        let err = state.validate().expect_err(
            "validate must reject proof-phase pending task with task_blockers under Local mode",
        );
        assert!(
            err.contains("Restructure"),
            "unexpected validate error: {err}",
        );
    }

    #[test]
    fn validate_accepts_proof_pending_task_local_with_soundness_only_task_blockers() {
        // Companion to `validate_rejects_proof_pending_task_with_blockers_but_no_node`
        // covering the Soundness carve-out introduced in 1263d80. The
        // legality gate (`WrapperRequest::review_response_legal`) and the
        // reviewer prompt (`05_after_failed_soundness.md`) agree that
        // `next_mode=Local + task_blockers=[Soundness on active_node]` is
        // legitimate (close-in-Lean clears the blocker). `validate()`
        // must agree too, otherwise an accepted reviewer response trips
        // InvariantViolation in `apply_event` (engine.rs:120).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.has_ever_been_clean = true;
        state.active_node = Some("a".into());
        for n in ["a", "b"] {
            state.node_difficulty.insert(n.into(), NodeDifficulty::Hard);
            state.easy_attempts.insert(n.into(), 0);
        }
        state.sound_status.insert("a".into(), SoundStatus::Fail);
        let blocker = state
            .global_blockers()
            .into_iter()
            .find(|b| {
                matches!(
                    (&b.object, b.kind),
                    (BlockerObject::Node { node }, BlockerKind::Soundness) if node == "a"
                )
            })
            .expect("expected soundness blocker on a");
        state.proof_edit_mode = ProofEditMode::Local;
        state.pending_task = Some(PendingTask {
            task_blockers: BTreeSet::from([blocker]),
            node: Some("a".into()),
            mode: TaskMode::Local,
            must_close_active: true,
            ..PendingTask::default()
        });
        state
            .validate()
            .expect("Local + Soundness-only task_blockers must pass validate (carve-out)");
    }

    #[test]
    fn need_input_requires_neutral_routing_and_no_task_or_override_blockers() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.latest_sound_review_nodes = set(&["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let blocker_a = request
            .blockers
            .iter()
            .find(|blocker| {
                matches!(
                    (&blocker.object, blocker.kind),
                    (BlockerObject::Node { node }, BlockerKind::Soundness) if node == "a"
                )
            })
            .cloned()
            .expect("missing soundness blocker");

        let legal = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::NeedInput,
            next_mode: TaskMode::Local,
            ..ReviewResponse::default()
        };
        assert!(state.review_response_legal(&legal));

        let with_task = ReviewResponse {
            task_blockers: BTreeSet::from([blocker_a.clone()]),
            ..legal.clone()
        };
        assert!(!state.review_response_legal(&with_task));

        let with_override = ReviewResponse {
            override_blockers: BTreeSet::from([blocker_a]),
            ..legal.clone()
        };
        assert!(!state.review_response_legal(&with_override));

        let with_next_active = ReviewResponse {
            next_active: Some("a".into()),
            ..legal
        };
        assert!(!state.review_response_legal(&with_next_active));
    }

    fn route_need_input_review_for_test_with_retry(
        retry_outcome_kind: RetryOutcomeKind,
    ) -> (ProtocolState, WrapperRequest) {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.latest_sound_review_nodes = set(&["a"]);
        state.retry_outcome_kind = retry_outcome_kind;
        if retry_outcome_kind != RetryOutcomeKind::None {
            state.attempt = 1;
        }
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: request.cycle,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::NeedInput,
                    reason: "the reviewer thinks the paper statement is impossible".into(),
                    comments: "suspected contradiction in the target bound".into(),
                    next_mode: TaskMode::Local,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("NeedInput review should route to auditor");
        let audit_request = first_issued_request(&outcome.commands).clone();
        (outcome.state, audit_request)
    }

    fn route_need_input_review_for_test() -> (ProtocolState, WrapperRequest) {
        route_need_input_review_for_test_with_retry(RetryOutcomeKind::None)
    }

    #[test]
    fn validate_enforces_need_input_audit_state_invariants() {
        let mut valid_context = base_state();
        valid_context.phase = Phase::ProofFormalization;
        valid_context.stage = Stage::StuckMathAudit;
        valid_context.stuck_math_audit.active = true;
        valid_context.stuck_math_audit.need_input_audit = Some(NeedInputAuditContext {
            phase: Phase::ProofFormalization,
            active_node: Some("a".into()),
            mode: TaskMode::Local,
            reviewer_reason: "reason".into(),
            review_request_id: 1,
            review_cycle: valid_context.cycle,
            ..NeedInputAuditContext::default()
        });
        valid_context.ensure_node_metadata();
        valid_context
            .validate()
            .expect("active StuckMathAudit context should validate");

        let mut inactive_context = valid_context.clone();
        inactive_context.stuck_math_audit.active = false;
        let err = inactive_context
            .validate()
            .expect_err("need_input_audit context must require active latch");
        assert!(
            err.contains("requires active stuck_math_audit"),
            "unexpected validate error: {err}"
        );

        let mut wrong_stage_context = valid_context.clone();
        wrong_stage_context.stage = Stage::Reviewer;
        let err = wrong_stage_context
            .validate()
            .expect_err("need_input_audit context must be in StuckMathAudit stage");
        assert!(
            err.contains("StuckMathAudit stage"),
            "unexpected validate error: {err}"
        );

        let mut valid_plan = base_state();
        valid_plan.phase = Phase::Cleanup;
        valid_plan.stage = Stage::Reviewer;
        valid_plan.stuck_math_audit.active = true;
        valid_plan.audit_plan = Some(AuditPlan {
            need_input_audit: true,
            ..AuditPlan::default()
        });
        valid_plan.ensure_node_metadata();
        valid_plan
            .validate()
            .expect("need_input_audit plan may persist outside StuckMathAudit while active");

        let mut inactive_plan = valid_plan;
        inactive_plan.stuck_math_audit.active = false;
        let err = inactive_plan
            .validate()
            .expect_err("need_input_audit plan must require active latch");
        assert!(
            err.contains("plan requires active stuck_math_audit"),
            "unexpected validate error: {err}"
        );
    }

    #[test]
    fn reviewer_need_input_routes_to_need_input_auditor_first() {
        let (state, audit_request) = route_need_input_review_for_test();

        assert_eq!(state.stage, Stage::StuckMathAudit);
        assert_eq!(audit_request.kind, RequestKind::StuckMathAudit);
        assert_eq!(
            audit_request
                .stuck_math_audit
                .need_input_audit
                .as_ref()
                .map(|ctx| {
                    (
                        ctx.reviewer_reason.as_str(),
                        ctx.reviewer_comments.as_str(),
                        ctx.phase,
                    )
                }),
            Some((
                "the reviewer thinks the paper statement is impossible",
                "suspected contradiction in the target bound",
                Phase::ProofFormalization,
            ))
        );
        assert_eq!(
            audit_request.stuck_math_audit_contract["request_summary"]["scenario"],
            serde_json::json!("need_input_auditor")
        );
        assert_ne!(audit_request.kind, RequestKind::HumanGate);
    }

    #[test]
    fn retry_review_need_input_marks_invalid_attempt_context() {
        let (state, audit_request) =
            route_need_input_review_for_test_with_retry(RetryOutcomeKind::Invalid);

        assert_eq!(state.stage, Stage::StuckMathAudit);
        assert!(audit_request
            .stuck_math_audit
            .need_input_audit
            .as_ref()
            .is_some_and(|ctx| ctx.gate_from_invalid_attempt));
    }

    #[test]
    fn need_input_auditor_recovery_plan_routes_back_to_review() {
        let (state, audit_request) = route_need_input_review_for_test();
        let response = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Ok,
            confirm_need_input: false,
            report: "## Claim being audited\n".to_string()
                + &"the issue is repairable by weakening the local route, not by human input. "
                    .repeat(8),
            tasks: vec![AuditTask {
                id: "task-1".into(),
                title: "Repair paper-faithful route".into(),
                body: "Compare the cited paper paragraph against node a, then send a worker with the corrected local statement.".into(),
                ..AuditTask::default()
            }],
            ..StuckMathAuditResponse::default()
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("repairing NeedInputAuditor response should be accepted");

        assert_eq!(outcome.state.stage, Stage::Reviewer);
        let plan = outcome.state.audit_plan.as_ref().expect("audit plan");
        assert!(plan.need_input_audit);
        assert_eq!(plan.tasks.len(), 1);
        match outcome.commands.as_slice() {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(request.kind, RequestKind::Review);
                assert!(request.audit_plan.as_ref().is_some_and(|plan| {
                    plan.need_input_audit && plan.tasks.iter().any(|task| task.id == "task-1")
                }));
            }
            other => panic!("expected Review request, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_stuck_math_audit_rejects_confirm_need_input() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::StuckMathAudit;
        state.cycle = 5;
        state.stuck_math_audit.active = true;
        state.stuck_math_audit.trigger = "ordinary stuck audit".into();
        state.corr_status.insert("b".into(), CorrStatus::Unknown);
        state.corr_approved_fingerprints.remove("b");
        let request = issue_request_for_test(&mut state, RequestKind::StuckMathAudit);
        let response = StuckMathAuditResponse {
            request_id: request.id,
            cycle: request.cycle,
            status: ResponseStatus::Ok,
            confirm_need_input: true,
            report: "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
            ..StuckMathAuditResponse::default()
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("invalid ordinary stuck math audit response should be rejected and retried");

        assert!(outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("confirm_need_input is only legal"));
        assert_eq!(
            first_issued_request(&outcome.commands).kind,
            RequestKind::StuckMathAudit
        );
    }

    #[test]
    fn need_input_auditor_rejects_recovery_without_tasks() {
        let (state, audit_request) = route_need_input_review_for_test();
        let response = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Ok,
            confirm_need_input: false,
            report: "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
            ..StuckMathAuditResponse::default()
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("taskless NeedInputAuditor recovery should be rejected and retried");

        assert!(outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("at least one recovery task"));
        assert_eq!(
            first_issued_request(&outcome.commands).kind,
            RequestKind::StuckMathAudit
        );
    }

    #[test]
    fn need_input_auditor_rejects_confirm_with_cone_clean_node() {
        let (state, audit_request) = route_need_input_review_for_test();
        let response = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Ok,
            confirm_need_input: true,
            report: "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
            cone_clean_node: Some("a".into()),
            ..StuckMathAuditResponse::default()
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("NeedInputAuditor confirm+cone-clean should be rejected and retried");

        assert!(outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("must not request cone_clean_node"));
        assert_eq!(
            first_issued_request(&outcome.commands).kind,
            RequestKind::StuckMathAudit
        );
    }

    #[test]
    fn need_input_auditor_malformed_twice_falls_back_to_human_gate() {
        let (state, audit_request) = route_need_input_review_for_test();
        let first = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Malformed,
            ..StuckMathAuditResponse::default()
        };
        let first_outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(first),
            },
        )
        .expect("first malformed NeedInputAuditor response should be retried");

        assert_eq!(first_outcome.state.stage, Stage::StuckMathAudit);
        assert_eq!(first_outcome.state.stuck_math_audit_burst_retry_count, 1);
        let retry_request = first_issued_request(&first_outcome.commands).clone();
        assert_eq!(retry_request.kind, RequestKind::StuckMathAudit);

        let second = StuckMathAuditResponse {
            request_id: retry_request.id,
            cycle: retry_request.cycle,
            status: ResponseStatus::Malformed,
            ..StuckMathAuditResponse::default()
        };
        let second_outcome = apply_event(
            first_outcome.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(second),
            },
        )
        .expect("second malformed NeedInputAuditor response should fall back to HumanGate");

        assert_eq!(second_outcome.state.stage, Stage::HumanGate);
        assert_eq!(second_outcome.state.gate_kind, GateKind::NeedInput);
        assert!(second_outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("NeedInputAuditor failed twice"));
        assert_eq!(
            first_issued_request(&second_outcome.commands).kind,
            RequestKind::HumanGate
        );
    }

    /// Set up a state pre-positioned at a Step A dispatch: Reviewer
    /// emitted a `global_repair_request` for node `b`, kernel routed to
    /// StuckMathAudit, `pending_global_repair_request` is `Some`.
    /// Mirrors the post-`route_global_repair_request_to_auditor`
    /// configuration the engine reaches in production.
    fn route_global_repair_request_review_for_test() -> (ProtocolState, WrapperRequest) {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.latest_sound_review_nodes = set(&["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let response = ReviewResponse {
            request_id: request.id,
            cycle: request.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active_coarse: state.active_coarse_node.clone(),
            global_repair_request: Some(GlobalRepairRequest {
                proposed_extension_nodes: set(&["b"]),
                reason: "cone blocks the repair I need".to_string(),
            }),
            ..ReviewResponse::default()
        };
        assert!(
            state.review_response_legal(&response),
            "Step A response should be legal in fixture"
        );
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("global_repair_request review should route to auditor");
        let audit_request = first_issued_request(&outcome.commands).clone();
        (outcome.state, audit_request)
    }

    /// Regression: a `global_repair_request` review that hits the
    /// retry-exhaust path (two malformed StuckMathAudit responses in
    /// a row) must auto-decline `pending_global_repair_request`,
    /// surface `latest_global_repair_audit_decline_reason`, route to
    /// Reviewer with the latch cleared, and leave the state
    /// `validate()`-clean (mutex invariant: `need_input_audit` +
    /// `pending_global_repair_request` cannot both be Some).
    #[test]
    fn global_repair_auditor_malformed_twice_auto_declines_and_returns_to_reviewer() {
        let (state, audit_request) = route_global_repair_request_review_for_test();
        assert!(
            state.pending_global_repair_request.is_some(),
            "Step A dispatch should set pending_global_repair_request"
        );
        assert_eq!(state.stage, Stage::StuckMathAudit);

        let first = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Malformed,
            ..StuckMathAuditResponse::default()
        };
        let first_outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(first),
            },
        )
        .expect("first malformed GlobalRepairAuditor response should be retried");

        assert_eq!(first_outcome.state.stage, Stage::StuckMathAudit);
        assert_eq!(first_outcome.state.stuck_math_audit_burst_retry_count, 1);
        // Mid-retry: the pending request must still be Some so the
        // auditor's next burst sees the same context.
        assert!(first_outcome
            .state
            .pending_global_repair_request
            .is_some());
        let retry_request = first_issued_request(&first_outcome.commands).clone();
        assert_eq!(retry_request.kind, RequestKind::StuckMathAudit);

        let second = StuckMathAuditResponse {
            request_id: retry_request.id,
            cycle: retry_request.cycle,
            status: ResponseStatus::Malformed,
            ..StuckMathAuditResponse::default()
        };
        let second_outcome = apply_event(
            first_outcome.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(second),
            },
        )
        .expect(
            "second malformed GlobalRepairAuditor response should auto-decline and \
             route to Reviewer (validate() must accept the resulting state)",
        );

        assert_eq!(second_outcome.state.stage, Stage::Reviewer);
        assert!(
            second_outcome.state.pending_global_repair_request.is_none(),
            "retry-exhaust must clear pending_global_repair_request"
        );
        assert!(
            second_outcome.state.pending_global_repair_grant.is_none(),
            "retry-exhaust must not leave a grant"
        );
        assert!(
            second_outcome.state.stuck_math_audit.need_input_audit.is_none(),
            "no NeedInput context was ever set on this dispatch"
        );
        assert!(
            second_outcome
                .state
                .latest_global_repair_audit_decline_reason
                .contains("auto-declining global_repair_request"),
            "auto-decline reason must be surfaced; got {:?}",
            second_outcome.state.latest_global_repair_audit_decline_reason,
        );
        assert_eq!(
            second_outcome.state.latest_global_repair_audit_decline_cycle,
            Some(second_outcome.state.cycle),
            "auto-decline cycle must be set"
        );
        assert!(second_outcome
            .state
            .latest_stuck_math_audit_rejection_reason
            .contains("stuck math audit failed twice"));
        assert_eq!(
            first_issued_request(&second_outcome.commands).kind,
            RequestKind::Review
        );
        // Final invariant: `validate()` accepts the resulting state.
        // (`apply_event` itself already validated, but assert explicitly
        // so a future regression that bypasses `apply_event` cannot
        // accidentally produce the InvariantViolation halt this test
        // guards against.)
        second_outcome
            .state
            .validate()
            .expect("post-auto-decline state must satisfy validate() invariants");
    }

    /// Mutex invariant: a hand-constructed state with both
    /// `need_input_audit` and `pending_global_repair_request` set must
    /// be rejected by `validate()`. Guards the invariant added in
    /// `ProtocolState::validate` against any future code path that
    /// reaches this configuration.
    #[test]
    fn validate_rejects_simultaneous_need_input_audit_and_pending_global_repair_request() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::StuckMathAudit;
        state.cycle = 5;
        state.stuck_math_audit.active = true;
        state.stuck_math_audit.need_input_audit = Some(NeedInputAuditContext {
            phase: Phase::ProofFormalization,
            active_node: Some("a".into()),
            mode: TaskMode::Local,
            reviewer_reason: "reason".into(),
            review_request_id: 1,
            review_cycle: 5,
            ..NeedInputAuditContext::default()
        });
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: set(&["b"]),
            reviewer_reason: "request".into(),
            review_request_id: 1,
            review_cycle: 5,
            dispatched_at_cycle: 5,
        });
        state.ensure_node_metadata();
        let err = state
            .validate()
            .expect_err("simultaneous need_input_audit + pending_global_repair_request must be rejected");
        assert!(
            err.contains("mutually exclusive"),
            "unexpected validate error: {err}"
        );
    }

    /// Proactive mutex: when `route_need_input_to_auditor` runs with a
    /// stale `pending_global_repair_request` already on the state, the
    /// pending request is taken/cleared and surfaced as an
    /// auto-decline. Without this clear the kernel would hit the
    /// mutex invariant added in `validate()` and halt.
    #[test]
    fn route_need_input_clears_in_flight_global_repair_request() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.sound_status.insert("a".into(), SoundStatus::Unknown);
        state.latest_sound_review_nodes = set(&["a"]);
        // Inject the pathological residue: a pending GR request that
        // somehow survived onto a Reviewer-stage state.
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: set(&["b"]),
            reviewer_reason: "stale request".into(),
            review_request_id: 99,
            review_cycle: 4,
            dispatched_at_cycle: 4,
        });
        let request = issue_request_for_test(&mut state, RequestKind::Review);
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: request.cycle,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::NeedInput,
                    reason: "reviewer escalation".into(),
                    comments: "needs human review".into(),
                    next_mode: TaskMode::Local,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect(
            "NeedInput review must route to auditor even when a stale \
             pending_global_repair_request sits on the state",
        );

        assert_eq!(outcome.state.stage, Stage::StuckMathAudit);
        assert!(
            outcome.state.stuck_math_audit.need_input_audit.is_some(),
            "NeedInput audit context must be set after routing"
        );
        assert!(
            outcome.state.pending_global_repair_request.is_none(),
            "stale pending_global_repair_request must be cleared by the proactive mutex"
        );
        assert!(
            outcome
                .state
                .latest_global_repair_audit_decline_reason
                .contains("NeedInput escalation pre-empted"),
            "auto-decline reason must surface the pre-emption; got {:?}",
            outcome.state.latest_global_repair_audit_decline_reason,
        );
        outcome
            .state
            .validate()
            .expect("post-routing state must satisfy validate() (mutex invariant)");
    }

    #[test]
    fn need_input_auditor_confirmation_routes_to_human_gate() {
        let (state, audit_request) = route_need_input_review_for_test();
        let response = StuckMathAuditResponse {
            request_id: audit_request.id,
            cycle: audit_request.cycle,
            status: ResponseStatus::Ok,
            confirm_need_input: true,
            report: "## Claim being audited\n".to_string()
                + &"the reference paper appears to require human adjudication at this exact claim. "
                    .repeat(8),
            ..StuckMathAuditResponse::default()
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("confirmed NeedInputAuditor response should be accepted");

        assert_eq!(outcome.state.stage, Stage::HumanGate);
        assert_eq!(outcome.state.gate_kind, GateKind::NeedInput);
        match outcome.commands.as_slice() {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(request.kind, RequestKind::HumanGate);
                assert!(request
                    .audit_plan
                    .as_ref()
                    .is_some_and(|plan| { plan.need_input_audit && plan.tasks.is_empty() }));
            }
            other => panic!("expected HumanGate request, got {other:?}"),
        }
    }

    #[test]
    fn review_response_with_last_clean_reset_routes_through_apply_last_clean_reset() {
        // (#56-extension) LastClean restores statuses + structural state from
        // the clean-checkpoint mirror. This test verifies the routing
        // (LastClean → apply_last_clean_reset → commit_live's commands)
        // and the post-reset invariant: global_blockers().is_empty()
        // (so the reviewer doesn't get stuck on phantom blockers).
        let mut state = base_state();
        // Seed the mirrors via a clean commit_live (base_state is all-Pass).
        state.commit_live();
        // Now move into ProofFormalization mid-cycle and dirty the state.
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 4;
        state.corr_status.insert("b".into(), CorrStatus::Unknown);

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        // Sanity: the request must offer LastClean — the reviewer-response
        // legality gate (`allowed_resets.contains`) would otherwise treat the
        // response as illegal and re-issue it without applying the reset.
        assert!(request.allowed_resets.contains(&ResetChoice::LastClean));
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    reset: ResetChoice::LastClean,
                    next_mode: TaskMode::Local,
                    next_worker_context_mode: WorkerContextMode::Resume,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("LastClean review response should apply cleanly");

        // The mid-cycle Unknown was wiped — restored from mirror back to
        // its clean-checkpoint value (Pass per base_state).
        assert_eq!(
            outcome.state.corr_status.get("b"),
            Some(&CorrStatus::Pass),
            "LastClean must restore corr_status from mirror, not leave Unknown",
        );
        // Crucial invariant: post-LastClean, global_blockers must be empty.
        // Pre-fix this was non-empty (Unknown statuses from clearing) and
        // proof/cleanup phases got stuck.
        assert!(
            outcome.state.global_blockers().is_empty(),
            "post-LastClean global_blockers must be empty; got {:?}",
            outcome.state.global_blockers(),
        );
    }

    // Done+LastClean (Cleanup) is now structurally unreachable under the
    // cleanup invariant — `request_allowed_resets` for Phase::Cleanup
    // returns {None}, so the outer `allowed_resets` gate rejects the
    // combo before reaching any inner check. The redundant explicit
    // Done+LastClean rejection in `expected_request().review_response_legal`
    // was deleted in the same commit. No unit or synthetic test needed
    // for that combo (testing dead code adds maintenance with zero
    // signal value — if a future regression makes it reachable again,
    // the cleanup invariant tests catch it first).
    //
    // AdvancePhase+LastClean (TheoremStating) IS reachable, just only
    // via a specific theorem-phase trace where the reviewer drove to a
    // clean checkpoint via Continue (not AdvancePhase) before the
    // AdvancePhase opportunity. The unit test below verifies the
    // legality rejection directly. The synthetic precursor to drive
    // this trace end-to-end (~150 LOC of synthetic-side scripting:
    // per-cycle reviewer overrides + clean-then-dirty cycle insertion)
    // wasn't worth landing — the kernel-side legality check is what we
    // care about, and stage 36 already covers "kernel rejects bad
    // reviewer response → reissues Review" end-to-end.

    #[test]
    fn theorem_advance_phase_with_last_clean_is_illegal() {
        // Audit-driven coverage replacing retired bigrun stage 44b. An
        // LLM reviewer occasionally produces semantically incoherent
        // combinations like "advance phase, but first rewind to clean."
        // The kernel must reject at legality (model.rs:856) so the
        // kernel can reissue without entering an apply path that would
        // either silently drop one of the two intents or apply both
        // and produce protocol-state divergence.
        let mut state = base_state();
        // Establish has_ever_been_clean=true via a clean commit_live.
        // base_state has all-Pass statuses → global_blockers empty →
        // commit_live populates mirrors and flips has_ever_been_clean.
        state.commit_live();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        // cycles_since_clean >= 1 so LastClean enters allowed_resets.
        state.cycles_since_clean = 2;
        // No retry context so allowed_decisions includes AdvancePhase.
        state.retry_outcome_kind = RetryOutcomeKind::None;

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        // Test setup precondition: both AdvancePhase and LastClean must
        // be in their respective allowed sets — otherwise the rejection
        // would come from the allowed_decisions / allowed_resets gates,
        // not the AdvancePhase+LastClean coherence check we're targeting.
        assert!(
            request
                .allowed_decisions
                .contains(&ReviewDecisionKind::AdvancePhase),
            "test setup: AdvancePhase must be in allowed_decisions",
        );
        assert!(
            request.allowed_resets.contains(&ResetChoice::LastClean),
            "test setup: LastClean must be in allowed_resets",
        );

        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::AdvancePhase,
            reset: ResetChoice::LastClean,
            next_active: None,
            next_mode: TaskMode::Global,
            ..ReviewResponse::default()
        };
        assert!(
            !state.review_response_legal(&response),
            "AdvancePhase+LastClean must be rejected at legality \
             (model.rs:856): the combination is semantically incoherent — \
             phase advance says 'leave this state behind' while LastClean \
             says 'rewind'.",
        );
    }

    #[test]
    fn theorem_advance_phase_apply_site_rejects_deferred_global_blockers() {
        // Defense-in-depth: AdvancePhase legality (model.rs:2771) gates on
        // the dispatch-eligible-filtered blocker set in the WrapperRequest
        // (`self.blockers.is_empty()`). The apply site
        // (`apply_theorem_review_response` AdvancePhase branch) layers a
        // second check on `state.global_blockers().is_empty()`. The
        // edge case is unreachable under DAG-acyclicity (deferred blockers
        // require non-Pass corr on a Lean-relevant dependency, which would
        // itself surface a dispatch-eligible blocker on the dependency in
        // a healthy DAG), but state load from disk, recovery paths, or
        // future code changes could surface a state where the legality
        // filter is empty while `global_blockers()` is not — without the
        // apply-time assertion the engine would silently advance phase.
        //
        // This test constructs a corrupted state with a Lean-relevant
        // dependency cycle a→b→a where both nodes have corr Fail. Both
        // NodeCorr blockers are deferred (each one's dep is itself
        // non-Pass), so `request_blockers(Review)` returns empty while
        // `global_blockers()` returns the two deferred blockers. The
        // legality check passes; the apply-time check fires.
        let mut state = base_state();
        state.commit_live();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.retry_outcome_kind = RetryOutcomeKind::None;

        // Construct the a→b, b→a Lean-relevant dependency cycle via the
        // corr_current_fingerprint JSON payload, with both nodes pinned
        // Fail (status=Fail, current==approved). Both nodes' corr blockers
        // become deferred because each is waiting on the other's corr.
        let fp_a = r#"{"lean_relevant_dependencies":["b"]}"#.to_string();
        let fp_b = r#"{"lean_relevant_dependencies":["a"]}"#.to_string();
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), fp_a.clone());
        state
            .live
            .corr_current_fingerprints
            .insert("b".into(), fp_b.clone());
        state
            .corr_approved_fingerprints
            .insert("a".into(), fp_a.clone());
        state
            .corr_approved_fingerprints
            .insert("b".into(), fp_b.clone());
        state.corr_status.insert("a".into(), CorrStatus::Fail);
        state.corr_status.insert("b".into(), CorrStatus::Fail);

        // Sanity checks: the corrupted DAG produces deferred-only blockers.
        assert!(
            !state.is_corr_dispatch_eligible(&"a".into()),
            "test setup: a's corr must be deferred (dep b non-Pass)",
        );
        assert!(
            !state.is_corr_dispatch_eligible(&"b".into()),
            "test setup: b's corr must be deferred (dep a non-Pass)",
        );
        let global = state.global_blockers();
        assert!(
            !global.is_empty(),
            "test setup: global_blockers must be non-empty; got {:?}",
            global,
        );
        assert!(
            global.iter().all(|b| b.deferred),
            "test setup: every global blocker must be deferred; got {:?}",
            global,
        );

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        // Filtered (dispatch-eligible) blockers must be empty so the
        // AdvancePhase legality check passes — leaving the apply-time
        // defense-in-depth check as the only remaining gate.
        assert!(
            request.blockers.is_empty(),
            "test setup: filtered blockers must be empty so legality passes; got {:?}",
            request.blockers,
        );
        assert!(
            request
                .allowed_decisions
                .contains(&ReviewDecisionKind::AdvancePhase),
            "test setup: AdvancePhase must be in allowed_decisions",
        );

        let response = ReviewResponse {
            request_id: request.id,
            cycle: 5,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::AdvancePhase,
            reset: ResetChoice::None,
            next_active: None,
            next_mode: TaskMode::Global,
            ..ReviewResponse::default()
        };
        // Confirm legality passes — otherwise the kernel would re-issue
        // Review at apply_review_response (engine.rs:3487) and never reach
        // the defense-in-depth check we're pinning.
        assert!(
            state.review_response_legal(&response),
            "test setup: legality must pass so the apply-time check is the failing gate",
        );

        let result = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        );
        assert!(
            matches!(result, Err(TransitionError::IllegalReviewerDecision)),
            "apply-time defense-in-depth must reject AdvancePhase when \
             global_blockers() is non-empty (even if dispatch-eligible-filtered \
             set is empty); got {:?}",
            result,
        );
    }

    #[test]
    fn non_retry_theorem_continue_review_applies_last_clean_reset() {
        // Regression test. Before the non-retry theorem review branches got
        // explicit reset handlers, `reset == LastClean` on a non-retry theorem
        // Continue was silently dropped at the kernel level while the runtime
        // still git-reset the worktree — state/repo divergence. This test
        // exercises the non-retry branch specifically to ensure the reset
        // applies AND restores the state from the clean-checkpoint mirror.
        let mut state = base_state();
        // Seed mirrors via a clean commit_live (base_state is all-Pass).
        state.commit_live();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cycles_since_clean = 3;
        state.retry_outcome_kind = RetryOutcomeKind::None;
        // Now dirty: blockers across all three lanes.
        state.corr_status.insert("b".into(), CorrStatus::Fail);
        state
            .sound_status
            .insert("a".into(), crate::model::SoundStatus::Fail);
        state.paper_status.insert("t".into(), CorrStatus::Fail);

        let request = issue_request_for_test(&mut state, RequestKind::Review);
        assert!(
            request.allowed_resets.contains(&ResetChoice::LastClean),
            "non-retry theorem review with has_ever_been_clean + cycles_since_clean >= 1 \
             must offer LastClean"
        );

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    reset: ResetChoice::LastClean,
                    next_mode: TaskMode::Global,
                    next_worker_context_mode: WorkerContextMode::Resume,
                    ..ReviewResponse::default()
                }),
            },
        )
        .expect("non-retry theorem Continue with LastClean should apply cleanly");

        // The reset actually applied — Fail statuses replaced with the
        // clean-checkpoint Pass values from the mirror.
        assert_eq!(outcome.state.corr_status.get("b"), Some(&CorrStatus::Pass));
        assert_eq!(
            outcome.state.sound_status.get("a"),
            Some(&crate::model::SoundStatus::Pass),
        );
        assert_eq!(outcome.state.paper_status.get("t"), Some(&CorrStatus::Pass));
        // Crucial invariant: no phantom blockers post-restore.
        assert!(
            outcome.state.global_blockers().is_empty(),
            "post-LastClean global_blockers must be empty; got {:?}",
            outcome.state.global_blockers(),
        );
    }

    // ----------------------------------------------------------------------
    // Bug X principled fix: transport-failure retry tests.
    // ----------------------------------------------------------------------

    #[test]
    fn transport_failure_bumps_transport_attempt_not_attempt() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        state.transport_attempt = 0;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Simulate a bridge-side transport failure: status=Malformed,
        // transport_failure=true, snapshot left at the request baseline.
        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: true,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("transport failure should retry");

        // Transport budget consumed (now at attempt-2 after the first
        // failed try, mirroring how `attempt` jumps from 1 to 2 after the
        // first Invalid response in the existing path);
        // work-quality budget untouched.
        assert_eq!(outcome.state.transport_attempt, 2);
        assert_eq!(
            outcome.state.attempt, 1,
            "attempt must NOT bump on transport failure"
        );
        assert!(!outcome.state.invalid_attempt, "transport is not invalid");
        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::Transport
        );
        assert_eq!(outcome.state.stage, Stage::Worker);

        // Reissues a worker request with retry_outcome_kind=Transport.
        let req = first_issued_request(&outcome.commands);
        assert_eq!(req.kind, RequestKind::Worker);
        assert_eq!(req.retry_outcome_kind, RetryOutcomeKind::Transport);
        assert_eq!(req.retry_attempt, 2);
        assert!(!req.invalid_attempt);
    }

    #[test]
    fn transport_failure_threshold_escalates_to_reviewer() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        // Set threshold low so we can reach it in one step.
        state.transport_invalid_review_threshold = 1;
        state.transport_attempt = 1; // already at the threshold
        state.retry_outcome_kind = RetryOutcomeKind::Transport;
        state.active_node = Some("a".into());
        // External-audit Finding 2: retry-to-review now routes through
        // `route_after_progress`. Pin substantiveness Pass on present
        // nodes so the test focuses on the threshold-escalation contract,
        // not the K-1 preemption (proof-phase substantiveness lane is
        // active and would otherwise create non-adjudicable Unknowns).
        state
            .substantiveness_status
            .insert("a".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("a".into(), "sub-a".into());
        state
            .substantiveness_status
            .insert("b".into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert("b".into(), "sub-b".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert("b".into(), "sub-b".into());
        state.committed.substantiveness_current_fingerprints =
            state.live.substantiveness_current_fingerprints.clone();
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: true,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("transport-threshold reached should escalate to reviewer");

        // Threshold reached: stage moves to reviewer, kind stays Transport.
        assert_eq!(outcome.state.stage, Stage::Reviewer);
        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::Transport
        );
        let req = first_issued_request(&outcome.commands);
        assert_eq!(req.kind, RequestKind::Review);
        assert_eq!(req.retry_outcome_kind, RetryOutcomeKind::Transport);
    }

    #[test]
    fn malformed_without_transport_flag_still_bumps_invalid_attempt() {
        // Sanity check: a Malformed response WITHOUT transport_failure=true
        // continues to consume the regular invalid-attempt budget. This
        // protects pre-existing behavior for callers that haven't been
        // updated to set transport_failure.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: false,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("plain malformed should retry as Invalid");

        assert_eq!(outcome.state.attempt, 2, "regular Malformed bumps attempt");
        assert_eq!(
            outcome.state.transport_attempt, 0,
            "transport budget untouched"
        );
        assert!(outcome.state.invalid_attempt);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::Invalid);
    }

    #[test]
    fn valid_worker_response_after_transport_clears_transport_attempt() {
        // After a few transport failures, a successful worker burst should
        // reset the transport budget (clear_retry_context is called from
        // every Valid worker path).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.attempt = 1;
        state.transport_attempt = 3;
        state.retry_outcome_kind = RetryOutcomeKind::Transport;
        state.active_node = Some("a".into());
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: state.live.clone(),
                    transport_failure: false,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid worker should clear retry context");

        // Transport budget reset; retry kind cleared.
        assert_eq!(outcome.state.transport_attempt, 0);
        assert_eq!(outcome.state.retry_outcome_kind, RetryOutcomeKind::None);
    }

    // ----------------------------------------------------------------------
    // Circuit-breaker tests (2026-05-12): five consecutive transport
    // failures on the same node emit `WriteHaltSentinel` so the
    // supervisor halts at the next checkpoint instead of looping.
    // ----------------------------------------------------------------------

    #[test]
    fn circuit_breaker_emits_halt_sentinel_after_five_consecutive_transport_failures() {
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.consecutive_transport_failure_halt_threshold = 5;
        // Tee up four prior transport failures already on `a`.
        state.consecutive_transport_failure_node = Some("a".into());
        state.consecutive_transport_failure_count = 4;
        // Generous retry budgets so escalation/retry path doesn't
        // interfere with breaker-bookkeeping under test.
        state.transport_invalid_review_threshold = 99;
        state.transport_attempt = 0;
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: true,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("transport failure should retry and emit halt sentinel");

        assert_eq!(
            outcome.state.consecutive_transport_failure_count, 5,
            "5th consecutive failure brings counter to threshold"
        );
        assert_eq!(
            outcome.state.consecutive_transport_failure_node,
            Some("a".into())
        );
        // Halt sentinel emitted alongside normal retry routing.
        let halt_count = outcome
            .commands
            .iter()
            .filter(|c| matches!(c, ProtocolCommand::WriteHaltSentinel { .. }))
            .count();
        assert_eq!(
            halt_count, 1,
            "exactly one WriteHaltSentinel command expected at threshold"
        );
    }

    #[test]
    fn circuit_breaker_resets_on_different_node() {
        // A transport failure on a NEW node resets the counter to 1; the
        // prior streak on the old node doesn't carry over.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = Some("b".into());
        state.consecutive_transport_failure_halt_threshold = 5;
        state.consecutive_transport_failure_node = Some("a".into());
        state.consecutive_transport_failure_count = 4;
        state.transport_invalid_review_threshold = 99;
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: true,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("transport failure on new node should reset counter");

        assert_eq!(
            outcome.state.consecutive_transport_failure_count, 1,
            "different node resets to 1"
        );
        assert_eq!(
            outcome.state.consecutive_transport_failure_node,
            Some("b".into())
        );
        let halt_count = outcome
            .commands
            .iter()
            .filter(|c| matches!(c, ProtocolCommand::WriteHaltSentinel { .. }))
            .count();
        assert_eq!(halt_count, 0, "counter reset to 1 should NOT trip halt");
    }

    #[test]
    fn circuit_breaker_clears_on_non_transport_response() {
        // A non-transport-failure worker outcome (Invalid without
        // transport_failure, or Valid) clears the counter even on the
        // same node.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.consecutive_transport_failure_halt_threshold = 5;
        state.consecutive_transport_failure_node = Some("a".into());
        state.consecutive_transport_failure_count = 4;
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let outcome = apply_event(
            state.clone(),
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Malformed,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: state.live.clone(),
                    transport_failure: false, // real worker output, just bad
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("plain malformed should retry");

        assert_eq!(
            outcome.state.consecutive_transport_failure_count, 0,
            "non-transport response clears the streak"
        );
        assert_eq!(outcome.state.consecutive_transport_failure_node, None);
        let halt_count = outcome
            .commands
            .iter()
            .filter(|c| matches!(c, ProtocolCommand::WriteHaltSentinel { .. }))
            .count();
        assert_eq!(halt_count, 0);
    }

    // ---- Substantiveness-lane drain-order test (audit Finding 4.3) ----
    //
    // This test uses the post-K-8 NodeId / TargetId newtype API directly
    // (`NodeId::from`) to avoid the broken `set` / `node_blockers` test
    // helpers above (pre-existing K-8 migration breakage).

    fn nid(s: &str) -> NodeId {
        NodeId::from(s)
    }

    fn tid(s: &str) -> TargetId {
        TargetId::from(s)
    }

    #[test]
    fn apply_theorem_paper_accept_drains_target_then_substantiveness_then_corr() {
        // Audit Finding §2.1: `apply_theorem_paper_accept` is the choke
        // point that enforces the cycle ordering target -> substantiveness
        // -> corr within a VerifyPaper drain. The function must route to
        // Stage::VerifyPaper while paper-target Unknowns exist, then stay
        // on Stage::VerifyPaper for the substantiveness frontier, then
        // transition to Stage::VerifyCorr.
        //
        // X is the covering node for target T and is corr-Pass from the
        // start (otherwise topological dispatch would defer paper). Y is a
        // separate corr-Unknown node used to exercise the Step-3 corr
        // transition.

        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X"), nid("Y")]);
        state.proof_nodes = BTreeSet::from([nid("X"), nid("Y")]);

        // Configure target T covered by node X.
        state.configured_targets = BTreeSet::from([tid("T")]);
        state
            .target_claims
            .insert(nid("X"), BTreeSet::from([tid("T")]));
        state
            .live
            .coverage
            .insert(tid("T"), BTreeSet::from([nid("X")]));
        state
            .live
            .paper_current_fingerprints
            .insert(tid("T"), "tfp".to_string());
        // No paper_status entry for T -> current_paper_unknown -> on the
        // paper_verify_targets() frontier.

        // X: substantiveness Unknown initially; corr=Pass so it does NOT
        // gate paper-target dispatch under topological dispatch.
        state
            .live
            .substantiveness_current_fingerprints
            .insert(nid("X"), "sfp".to_string());
        state
            .live
            .corr_current_fingerprints
            .insert(nid("X"), "cfp-X".to_string());
        state.corr_status.insert(nid("X"), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(nid("X"), "cfp-X".to_string());

        // Y: substantiveness Pass + corr Unknown so corr_verify_nodes is
        // non-empty for the Step-3 transition.
        state
            .live
            .substantiveness_current_fingerprints
            .insert(nid("Y"), "sfp-Y".to_string());
        state
            .substantiveness_status
            .insert(nid("Y"), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(nid("Y"), "sfp-Y".to_string());
        state
            .live
            .corr_current_fingerprints
            .insert(nid("Y"), "cfp-Y".to_string());

        // Sanity: target frontier is non-empty initially (X is corr-Pass,
        // T is paper-Unknown), substantiveness has X, corr has Y.
        assert!(
            !state.paper_verify_targets().is_empty(),
            "test prerequisite: paper_verify_targets must be non-empty initially"
        );
        assert!(
            !state.substantiveness_verify_nodes().is_empty(),
            "test prerequisite: substantiveness_verify_nodes must be non-empty initially"
        );

        // Step 1: drain target frontier first. (Bypass the verifier
        // round-trip; we're testing apply_theorem_paper_accept's branching
        // directly. Simulate the kernel's own pre-call setup.)
        state.stage = Stage::VerifyPaper;
        let commands = apply_theorem_paper_accept(&mut state)
            .expect("apply_theorem_paper_accept ok with target Unknown");
        assert_eq!(
            state.stage,
            Stage::VerifyPaper,
            "with target Unknown, drain-loop must stay on VerifyPaper",
        );
        let req = first_issued_request(&commands);
        assert_eq!(req.kind, RequestKind::Paper);
        assert!(
            !req.paper_verify_targets.is_empty(),
            "first frontier must be the paper-target frontier; got substantiveness_verify_nodes={:?}",
            req.substantiveness_verify_nodes
        );

        // Step 2: target clears -> substantiveness frontier should fire.
        state.paper_status.insert(tid("T"), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert(tid("T"), "tfp".to_string());
        assert!(
            state.paper_verify_targets().is_empty(),
            "after target clears, paper_verify_targets must be empty"
        );
        let commands = apply_theorem_paper_accept(&mut state)
            .expect("apply_theorem_paper_accept ok with substantiveness Unknown");
        assert_eq!(
            state.stage,
            Stage::VerifyPaper,
            "with target clear and substantiveness Unknown, drain-loop must stay on VerifyPaper",
        );
        let req = first_issued_request(&commands);
        assert_eq!(req.kind, RequestKind::Paper);
        assert!(
            req.paper_verify_targets.is_empty(),
            "after target clears, second Paper request must have empty paper_verify_targets"
        );
        assert!(
            !req.substantiveness_verify_nodes.is_empty(),
            "after target clears, second Paper request must carry substantiveness_verify_nodes"
        );

        // Step 3: substantiveness clears -> corr should fire.
        state
            .substantiveness_status
            .insert(nid("X"), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(nid("X"), "sfp".to_string());
        assert!(
            state.substantiveness_verify_nodes().is_empty(),
            "after substantiveness clears, substantiveness_verify_nodes must be empty"
        );
        let commands = apply_theorem_paper_accept(&mut state)
            .expect("apply_theorem_paper_accept ok after both paper frontiers drain");
        assert_eq!(
            state.stage,
            Stage::VerifyCorr,
            "after BOTH paper frontiers drain, drain-loop must transition to VerifyCorr",
        );
        let req = first_issued_request(&commands);
        assert_eq!(req.kind, RequestKind::Corr);
    }

    #[test]
    fn proof_paper_accept_clears_closed_active_after_substantiveness_pass() {
        // Regression guard: a closed active proof node can be legal only
        // because its substantiveness fingerprint is Unknown. When the
        // Paper/substantiveness verifier passes that node but leaves
        // another node Unknown, the kernel would try to issue the next
        // Paper request with the stale active node still set and trip the
        // active_node_legal invariant.
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::VerifyPaper;
        state.cycle = 7;
        state.active_node = Some(nid("a"));
        state.configured_targets.clear();
        state.target_claims.clear();
        state.committed_target_claims.clear();
        state.live.coverage.clear();
        state.committed.coverage.clear();
        state.proof_nodes = BTreeSet::from([nid("a"), nid("c")]);
        state.live.present_nodes = BTreeSet::from([nid("a"), nid("c")]);
        state.live.open_nodes = BTreeSet::from([nid("c")]);
        state.live.corr_current_fingerprints = BTreeMap::from([
            (nid("a"), "corr-a".to_string()),
            (nid("c"), "corr-c".to_string()),
        ]);
        state.corr_status =
            BTreeMap::from([(nid("a"), CorrStatus::Pass), (nid("c"), CorrStatus::Pass)]);
        state.corr_approved_fingerprints = BTreeMap::from([
            (nid("a"), "corr-a".to_string()),
            (nid("c"), "corr-c".to_string()),
        ]);
        state.live.substantiveness_current_fingerprints = BTreeMap::from([
            (nid("a"), "sub-a".to_string()),
            (nid("c"), "sub-c".to_string()),
        ]);
        state.substantiveness_status.remove(&nid("a"));
        state
            .substantiveness_approved_fingerprints
            .remove(&nid("a"));
        state.committed = state.live.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        assert!(
            state.active_node_legal(state.active_node.as_ref(), &state.live),
            "active a starts legal because substantiveness(a) is Unknown",
        );

        let request = issue_request_for_test(&mut state, RequestKind::Paper);
        assert!(request.substantiveness_verify_nodes.contains(&nid("a")));
        assert!(request.substantiveness_verify_nodes.contains(&nid("c")));

        let mut node_lane_updates: SubstantivenessLaneUpdates = BTreeMap::new();
        for lane in &request.verify_lanes {
            node_lane_updates.insert(
                lane.clone(),
                BTreeMap::from([(nid("a"), Update::Set(SubstantivenessStatus::Pass))]),
            );
        }
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Paper(PaperResponse {
                    request_id: request.id,
                    cycle: 7,
                    status: ResponseStatus::Ok,
                    target_lane_updates: empty_corr_target_lanes(&request.verify_lanes),
                    node_lane_updates,
                    ..PaperResponse::default()
                }),
            },
        )
        .expect("substantiveness pass on the stale active node should not violate invariants");

        assert_eq!(outcome.state.active_node, None);
        assert_eq!(outcome.state.stage, Stage::VerifyPaper);
        let req = first_issued_request(&outcome.commands);
        assert_eq!(req.kind, RequestKind::Paper);
        assert!(!req.substantiveness_verify_nodes.contains(&nid("a")));
        assert!(req.substantiveness_verify_nodes.contains(&nid("c")));
    }

    // ---- Patch C-B local-closure acceptance bookkeeping tests ------------

    fn closure_record_for_test(
        node_name: &str,
        boundary: &[&str],
        strict_thm: &[&str],
        strict_def: &[&str],
    ) -> LocalClosureRecord {
        let mut record = LocalClosureRecord::default();
        record.node = NodeId::from(node_name);
        record.closure_version = "v1".to_string();
        record.boundary_theorems = boundary
            .iter()
            .map(|h| (NodeId::from(*h), "stmt-hash".to_string()))
            .collect();
        record.strict_theorem_deps = strict_thm
            .iter()
            .map(|t| (NodeId::from(*t), "val-hash".to_string()))
            .collect();
        record.strict_definition_deps = strict_def
            .iter()
            .map(|d| (NodeId::from(*d), "sem-hash".to_string()))
            .collect();
        record
    }

    fn proof_burst_state(proof_nodes: &[&str], present: &[&str], open: &[&str]) -> ProtocolState {
        // Minimal ProofFormalization state shaped just enough for
        // apply_proof_worker_response to accept a Valid no-op delta.
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.proof_nodes = proof_nodes.iter().map(|n| NodeId::from(*n)).collect();
        state.live.present_nodes = present.iter().map(|n| NodeId::from(*n)).collect();
        state.live.open_nodes = open.iter().map(|n| NodeId::from(*n)).collect();
        state.committed = state.live.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state.active_node = proof_nodes.first().map(|n| NodeId::from(*n));
        state
    }

    #[test]
    fn apply_revalidation_batch_installs_refreshed_records_and_clears_membership() {
        // Patch C-B — apply_revalidation_batch must install each
        // refreshed record, remove the node from
        // local_closure_unverified_nodes and local_closure_failures, and
        // recompute reverse indices so the new record's helper / dep
        // keys surface immediately.
        //
        // Audit Fix HIGH 6 (post-update): the batch installer now filters
        // entries against `live.present_nodes`, `proof_nodes`, and
        // `!live.open_nodes.contains(node)`. Test state must reflect a
        // live present sorry-free proof node for the entry to survive
        // the filter — populated below.
        //
        // Audit C-1 (post-update): the batch installer also runs the
        // canonical consistency predicate against the post-burst state.
        // Test state must therefore populate the dep nodes
        // (HelperH/ThmT/DefD) into `live.present_nodes` so the record's
        // referenced-deps clause passes.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Foo"));
        state.live.present_nodes.insert(NodeId::from("HelperH"));
        state.live.present_nodes.insert(NodeId::from("ThmT"));
        state.live.present_nodes.insert(NodeId::from("DefD"));
        state.proof_nodes.insert(NodeId::from("Foo"));
        state
            .local_closure_unverified_nodes
            .insert(NodeId::from("Foo"));
        let mut existing_failure = ErrorSummary::default();
        existing_failure.status = "axiom_violation".to_string();
        state
            .local_closure_failures
            .insert(NodeId::from("Foo"), existing_failure);

        let record = closure_record_for_test("Foo", &["HelperH"], &["ThmT"], &["DefD"]);
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Foo"), record.clone())],
            still_unverified: Vec::new(),
        };

        apply_revalidation_batch(&mut state, batch);

        assert_eq!(
            state.local_closure_records.get(&NodeId::from("Foo")),
            Some(&record),
            "refreshed record must land in records map"
        );
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Foo")),
            "refreshed node must leave unverified set"
        );
        assert!(
            !state
                .local_closure_failures
                .contains_key(&NodeId::from("Foo")),
            "refreshed node must leave failures map"
        );
        assert_eq!(
            state
                .boundary_statement_consumers
                .get(&NodeId::from("HelperH")),
            Some(&BTreeSet::from([NodeId::from("Foo")])),
            "reverse index must reflect refreshed record's boundary helper"
        );
        assert_eq!(
            state.strict_dep_consumers.get(&NodeId::from("ThmT")),
            Some(&BTreeSet::from([NodeId::from("Foo")])),
            "reverse index must reflect refreshed record's strict theorem dep"
        );
    }

    #[test]
    fn apply_revalidation_batch_installs_failures_and_adds_to_unverified_set() {
        // Patch C-B — still_unverified entries must land in the
        // failures map AND the unverified set in lockstep, regardless
        // of prior membership.
        //
        // Audit Fix HIGH 6: still-unverified entries are also filtered
        // through the present + proof + !open gate; populate the live
        // tier so the entry survives.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Bar"));
        state.proof_nodes.insert(NodeId::from("Bar"));
        let mut summary = ErrorSummary::default();
        summary.status = "axiom_violation".to_string();
        summary.captured_at_cycle = 42;
        let batch = RevalidationBatch {
            refreshed: Vec::new(),
            still_unverified: vec![(NodeId::from("Bar"), summary.clone())],
        };

        apply_revalidation_batch(&mut state, batch);

        assert_eq!(
            state.local_closure_failures.get(&NodeId::from("Bar")),
            Some(&summary),
            "still-unverified entry must populate failures map"
        );
        assert!(
            state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Bar")),
            "still-unverified entry must surface in unverified set"
        );
    }

    #[test]
    fn proof_worker_accept_creates_record_on_sorryd_to_sorry_free_transition() {
        // Patch C-B §7.0 — sorryd → sorry-free transition must create
        // a LocalClosureRecord from the probe payload. Probe `status==
        // "ok"` with empty errors qualifies for record creation.
        let mut state = proof_burst_state(&["a"], &["a", "b"], &["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker delta closes a's sorry: post-delta open_nodes is empty.
        let mut new_live = state.live.clone();
        new_live.open_nodes.clear();

        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        probe
            .boundary_theorems
            .insert(NodeId::from("HelperH"), "stmt-h".to_string());
        probe
            .strict_theorem_deps
            .insert(NodeId::from("ThmT"), "val-t".to_string());
        probe.kernel_axioms.insert("propext".to_string());

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("a"), probe);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        let record = outcome
            .state
            .local_closure_records
            .get(&NodeId::from("a"))
            .expect("record must be installed for sorryd→sorry-free transition");
        assert_eq!(record.node, NodeId::from("a"));
        assert!(
            record.kernel_axioms.contains("propext"),
            "record must carry kernel_axioms from probe"
        );
        assert_eq!(
            record.boundary_theorems.get(&NodeId::from("HelperH")),
            Some(&"stmt-h".to_string()),
            "record must carry boundary_theorems from probe"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "node with fresh record must not be in unverified set"
        );
        assert_eq!(
            outcome
                .state
                .boundary_statement_consumers
                .get(&NodeId::from("HelperH")),
            Some(&BTreeSet::from([NodeId::from("a")])),
            "reverse index must surface the new record's boundary helper"
        );
    }

    #[test]
    fn proof_worker_accept_writes_failure_summary_on_probe_failure() {
        // Patch C-B §7.0 — sorryd → sorry-free transition with probe
        // `status != "ok"` must write an ErrorSummary in failures and
        // mark the node unverified, NOT install a record.
        let mut state = proof_burst_state(&["a"], &["a", "b"], &["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut new_live = state.live.clone();
        new_live.open_nodes.clear();

        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "axiom_violation".to_string();
        probe.kernel_axioms.insert("Lean.ofReduceBool".to_string());
        probe
            .errors
            .push("uses unapproved kernel axiom".to_string());
        probe.returncode = 0;
        probe.raw_stderr = "[axiom] Lean.ofReduceBool".to_string();

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("a"), probe);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply even when probe fails");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "no record must be installed for failed probe"
        );
        let summary = outcome
            .state
            .local_closure_failures
            .get(&NodeId::from("a"))
            .expect("failure summary must be written on probe failure");
        assert_eq!(summary.status, "axiom_violation");
        assert!(summary
            .axiom_violations
            .contains(&"Lean.ofReduceBool".to_string()));
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "failed-probe node must surface in unverified set"
        );
    }

    #[test]
    fn proof_worker_accept_creates_record_for_newly_added_sorry_free_node() {
        // Patch C-R §7.0 — a worker burst that adds a new sorry-free
        // proof_node (a fresh helper birth, e.g. worker 2707's
        // `FixedEmbeddingGraphEventProbability`) must install a
        // `LocalClosureRecord` for the new node when the probe payload
        // says "ok". Before C-R, the runtime CLI only probed the MCA-
        // gated active node and never produced a result for new
        // helpers; the engine's step (e) loop therefore saw no payload
        // for the helper and left it with no record, no failure, no
        // unverified entry — violating the §527 sorry-free-only
        // invariant ("sorry-free + no fresh record ⇒ in unverified
        // set"). The CLI side now probes new helpers too; this engine
        // test verifies that, given the resulting probe payload, the
        // engine creates a record on the same code path that handles
        // sorryd→sorry-free transitions.
        //
        // Setup: pre-delta has just node `a` (sorry-free). Worker adds
        // new helper `helperH` to present_nodes + proof_nodes, with no
        // sorry in `helperH`. Probe result is attached for `helperH`.
        let mut state = proof_burst_state(&["a"], &["a"], &[]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Post-delta snapshot: present_nodes gains helperH; open_nodes
        // is empty (both a and helperH are sorry-free).
        let mut new_live = state.live.clone();
        new_live.present_nodes.insert(NodeId::from("helperH"));

        // Probe payload for helperH — `ok` status, clean axioms, no
        // errors. Records eligibility: helperH must be in present_nodes
        // (set above), in proof_nodes (set via proof_node_updates
        // below), and out of open_nodes (already empty).
        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        probe.kernel_axioms.insert("propext".to_string());
        probe
            .boundary_theorems
            .insert(NodeId::from("BoundaryB"), "stmt-b".to_string());

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("helperH"), probe);

        // The new helper enters proof_nodes via proof_node_updates.
        let mut proof_node_updates: NodeBoolUpdates = BTreeMap::new();
        proof_node_updates.insert(NodeId::from("helperH"), Update::Set(true));
        let mut node_kind_updates: NodeKindUpdates = BTreeMap::new();
        node_kind_updates.insert(NodeId::from("helperH"), Update::Set(NodeKind::Proof));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    proof_node_updates,
                    node_kind_updates,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        // The record must be installed under helperH's id.
        let record = outcome
            .state
            .local_closure_records
            .get(&NodeId::from("helperH"))
            .expect(
                "record must be installed for newly-added sorry-free helper \
                 — Patch C-R closes the §527 invariant gap",
            );
        assert_eq!(record.node, NodeId::from("helperH"));
        assert!(
            record.kernel_axioms.contains("propext"),
            "record must carry kernel_axioms from probe payload"
        );
        // The §527 invariant requires: sorry-free + record present ⇔
        // NOT in unverified set.
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("helperH")),
            "newly recorded helper must not be in unverified set"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("helperH")),
            "newly recorded helper must not be in failures map"
        );
        // Reverse-index entry for the boundary helper should reflect
        // the new record (boundary_statement_consumers[BoundaryB] ∋
        // helperH).
        assert_eq!(
            outcome
                .state
                .boundary_statement_consumers
                .get(&NodeId::from("BoundaryB")),
            Some(&BTreeSet::from([NodeId::from("helperH")])),
            "reverse index must surface the newly-added helper as a boundary consumer"
        );
    }

    #[test]
    fn proof_worker_accept_writes_failure_for_newly_added_helper_failed_probe() {
        // Patch C-R §7.0 — Patch C-R contract on the engine side: when
        // the runtime CLI rejects a burst at the helper-probe gate
        // (axiom violation / status≠ok / errors non-empty), the engine
        // never sees the response in the Valid path. But to ensure
        // engine plumbing also handles the "probe payload says
        // failure" case for a new helper (defense in depth, parallel
        // to `proof_worker_accept_writes_failure_summary_on_probe_failure`
        // for the active node), we verify that a synthesized failure
        // payload for a newly-added helper produces a failure summary
        // and unverified entry, NOT a record.
        let mut state = proof_burst_state(&["a"], &["a"], &[]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut new_live = state.live.clone();
        new_live.present_nodes.insert(NodeId::from("helperH"));

        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "axiom_violation".to_string();
        probe.kernel_axioms.insert("Lean.ofReduceBool".to_string());
        probe
            .errors
            .push("uses unapproved kernel axiom".to_string());
        probe.returncode = 0;

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("helperH"), probe);

        let mut proof_node_updates: NodeBoolUpdates = BTreeMap::new();
        proof_node_updates.insert(NodeId::from("helperH"), Update::Set(true));
        let mut node_kind_updates: NodeKindUpdates = BTreeMap::new();
        node_kind_updates.insert(NodeId::from("helperH"), Update::Set(NodeKind::Proof));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    proof_node_updates,
                    node_kind_updates,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply even when helper probe fails");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("helperH")),
            "no record must be installed when helper probe reports a failure"
        );
        let summary = outcome
            .state
            .local_closure_failures
            .get(&NodeId::from("helperH"))
            .expect("failure summary must be written for failed helper probe");
        assert_eq!(summary.status, "axiom_violation");
        assert!(
            summary
                .axiom_violations
                .contains(&"Lean.ofReduceBool".to_string()),
            "failure summary must carry the axiom violation list"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("helperH")),
            "failed-probe helper must surface in unverified set"
        );
    }

    #[test]
    fn proof_worker_accept_handles_active_and_helper_in_same_burst() {
        // Patch C-R §7.0 — a single burst can simultaneously close the
        // active node (MCA gate fires) AND add a new sorry-free
        // helper. Both probe payloads should land in
        // `WorkerResponse.local_closure_results`, and the engine should
        // produce one record per node. This is the worst-case Patch C-R
        // pattern: restructure-mode burst that authorises edits to
        // both A and a newly-created helper H.
        //
        // Setup: pre-delta A is sorryd (MCA target), no helperH yet.
        let mut state = proof_burst_state(&["a"], &["a"], &["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Post-delta: helperH added, both a and helperH are sorry-free.
        let mut new_live = state.live.clone();
        new_live.open_nodes.clear();
        new_live.present_nodes.insert(NodeId::from("helperH"));

        let mut active_probe = LocalClosureProbeOutput::default();
        active_probe.status = "ok".to_string();
        active_probe
            .strict_theorem_deps
            .insert(NodeId::from("ThmT"), "val-t".to_string());

        let mut helper_probe = LocalClosureProbeOutput::default();
        helper_probe.status = "ok".to_string();
        helper_probe.kernel_axioms.insert("propext".to_string());

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("a"), active_probe);
        local_closure_results.insert(NodeId::from("helperH"), helper_probe);

        let mut proof_node_updates: NodeBoolUpdates = BTreeMap::new();
        proof_node_updates.insert(NodeId::from("helperH"), Update::Set(true));
        let mut node_kind_updates: NodeKindUpdates = BTreeMap::new();
        node_kind_updates.insert(NodeId::from("helperH"), Update::Set(NodeKind::Proof));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    proof_node_updates,
                    node_kind_updates,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply with two probe payloads");

        // Both records installed.
        assert!(
            outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "active-node record must be installed (MCA / Patch C-B path)"
        );
        assert!(
            outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("helperH")),
            "newly-added helper record must be installed (Patch C-R path)"
        );
        // Neither in unverified set.
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "active-node must not be in unverified set after fresh record install"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("helperH")),
            "helperH must not be in unverified set after fresh record install"
        );
    }

    #[test]
    fn proof_worker_accept_deletes_record_on_sorry_free_to_sorryd_transition() {
        // Patch C-B §7.0 — sorry-free → sorryd transition (worker
        // reintroduces a sorry to a previously-recorded node) must
        // delete records, failures, and unverified-set membership for
        // that node.
        let mut state = proof_burst_state(&["a"], &["a", "b"], &[]);
        // a starts sorry-free with a record installed.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["HelperH"], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        assert!(state
            .boundary_statement_consumers
            .contains_key(&NodeId::from("HelperH")));

        // committed must mirror live for restore_committed safety —
        // base_state is built that way, but proof_burst_state needs an
        // explicit refresh after we mutate the closure tier.
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker reintroduces sorry: post-delta open_nodes contains a.
        let mut new_live = state.live.clone();
        new_live.open_nodes.insert(NodeId::from("a"));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "record must be deleted on sorry-free → sorryd transition"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("a")),
            "failures must be deleted on sorry-free → sorryd transition"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "unverified-set membership must be cleared on sorry-free → sorryd transition"
        );
        assert!(
            outcome.state.live.open_nodes.contains(&NodeId::from("a")),
            "post-delta open_nodes must reflect the new sorryd state"
        );
        assert!(
            !outcome
                .state
                .boundary_statement_consumers
                .contains_key(&NodeId::from("HelperH")),
            "reverse index entry must be pruned with the deleted record"
        );
    }

    #[test]
    fn apply_local_closure_acceptance_bookkeeping_emits_delete_command_for_invalidated_record() {
        // Patch C-O HIGH 1 (c) — every node whose in-memory closure
        // record is invalidated during bookkeeping must produce a
        // `DeleteLocalClosureRecord` engine command so the runtime CLI
        // removes the persisted JSON file at
        // `<runtime_root>/checker-state/local-closure-records/<node>.json`.
        // Otherwise the stale disk record would resurrect on the next
        // supervisor restart.
        //
        // Scenario: sorry-free → sorryd transition (step (b) of the
        // bookkeeping pass).
        let mut state = proof_burst_state(&["a"], &["a", "b"], &[]);
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &[], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);
        let mut new_live = state.live.clone();
        new_live.open_nodes.insert(NodeId::from("a"));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        let delete_for_a = outcome.commands.iter().any(|cmd| {
            matches!(
                cmd,
                ProtocolCommand::DeleteLocalClosureRecord { node } if node == &NodeId::from("a")
            )
        });
        assert!(
            delete_for_a,
            "invalidated record for `a` must produce a DeleteLocalClosureRecord command; got commands: {:?}",
            outcome.commands
        );
    }

    #[test]
    fn proof_worker_accept_prunes_closure_state_on_node_deletion() {
        // Patch C-B §7.0 — when a node leaves live.present_nodes
        // (orphan cleanup), all closure state for that node must be
        // pruned: records, failures, unverified-set membership, and
        // every reverse-index entry where the node appears as KEY or
        // value-set ELEMENT.
        let mut state = proof_burst_state(&["a"], &["a", "b", "c"], &[]);
        // a has a record consuming b as a boundary helper, c as strict
        // theorem dep. b has a record consuming nothing. The reverse
        // indices put a in b's and c's value sets.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["b"], &["c"], &[]),
        );
        state.local_closure_records.insert(
            NodeId::from("b"),
            closure_record_for_test("b", &[], &[], &[]),
        );
        let mut a_failure = ErrorSummary::default();
        a_failure.status = "axiom_violation".to_string();
        // Use b as the failure-bearing node so we can verify deletion
        // touches the failures map too.
        state
            .local_closure_failures
            .insert(NodeId::from("b"), a_failure);
        state
            .local_closure_unverified_nodes
            .insert(NodeId::from("b"));
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();
        state.committed_local_closure_failures = state.local_closure_failures.clone();
        state.committed_local_closure_unverified_nodes =
            state.local_closure_unverified_nodes.clone();

        // Sanity: pre-delta reverse indices include b as a key
        // (because a consumes b as a boundary helper) AND b as a value-
        // set member (the b record consumes nothing → no value-set
        // membership). We use c instead.
        assert_eq!(
            state.boundary_statement_consumers.get(&NodeId::from("b")),
            Some(&BTreeSet::from([NodeId::from("a")])),
            "pre-delta: a must surface as b's boundary consumer"
        );

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker burst removes b from present_nodes (orphan cleanup).
        let mut new_live = state.live.clone();
        new_live.present_nodes.remove(&NodeId::from("b"));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("b")),
            "deleted node's record must be pruned"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("b")),
            "deleted node's failures must be pruned"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("b")),
            "deleted node must leave unverified set"
        );
        assert!(
            !outcome
                .state
                .boundary_statement_consumers
                .contains_key(&NodeId::from("b")),
            "deleted node must vanish from reverse-index keys"
        );
        // a's record was conservatively invalidated when b's structural
        // delta fired (b appears in present_nodes symmetric difference),
        // so a now has no record either; its reverse-index entries
        // (consuming b, c) must also be cleaned.
        assert!(
            !outcome
                .state
                .strict_dep_consumers
                .get(&NodeId::from("c"))
                .is_some_and(|v| v.contains(&NodeId::from("a"))),
            "consumer-side: a's strict-dep on c must be removed when a's record is invalidated"
        );
    }

    #[test]
    fn proof_worker_accept_invalidates_consumers_on_dep_change() {
        // Patch C-B §7.3 (conservative C-B form) — a structural delta
        // that potentially mutates a producer's .lean content marks the
        // producer's consumers stale. Consumers lose their record and
        // (when sorry-free) enter the unverified set.
        let mut state = proof_burst_state(&["a"], &["a", "h"], &[]);
        // a consumes h as a boundary helper. Both are sorry-free.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["h"], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker delta touches h via a node_kind_update — conservative
        // marker for "h's content might have changed."
        let mut node_kind_updates = BTreeMap::new();
        node_kind_updates.insert(NodeId::from("h"), Update::Set(NodeKind::Proof));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: WorkingSnapshot {
                        present_nodes: BTreeSet::from([NodeId::from("a"), NodeId::from("h")]),
                        open_nodes: BTreeSet::new(),
                        ..Default::default()
                    },
                    node_kind_updates,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "consumer a's record must be invalidated when h structurally changes"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "invalidated consumer must enter unverified set"
        );
        // No failure summary should be written for stale-by-invalidation
        // (it's "needs re-probe", not "probed and failed").
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("a")),
            "stale-by-invalidation must NOT write a failure summary"
        );
    }

    #[test]
    fn proof_worker_accept_consumes_revalidation_batch_on_response() {
        // Patch C-B §7.5 — WorkerResponse.local_closure_revalidation,
        // when present, must be applied during the accept path:
        // refreshed records install, still_unverified entries populate
        // failures and the unverified set. Patch C-O HIGH 3 routes this
        // through `apply_revalidation_batch`, so the batch entries must
        // reference proof + present + not-open nodes to survive the
        // filter.
        let mut state = proof_burst_state(&["a", "b"], &["a", "b"], &[]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let refreshed_record = closure_record_for_test("a", &[], &[], &[]);
        let mut still_summary = ErrorSummary::default();
        still_summary.status = "axiom_violation".to_string();
        still_summary.captured_at_cycle = 5;
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("a"), refreshed_record.clone())],
            still_unverified: vec![(NodeId::from("b"), still_summary.clone())],
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: WorkingSnapshot {
                        present_nodes: BTreeSet::from([NodeId::from("a"), NodeId::from("b")]),
                        open_nodes: BTreeSet::new(),
                        ..Default::default()
                    },
                    local_closure_revalidation: Some(batch),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert_eq!(
            outcome.state.local_closure_records.get(&NodeId::from("a")),
            Some(&refreshed_record),
            "refreshed record from batch must land in records map"
        );
        assert_eq!(
            outcome.state.local_closure_failures.get(&NodeId::from("b")),
            Some(&still_summary),
            "still-unverified entry from batch must populate failures"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("b")),
            "still-unverified entry must surface in unverified set"
        );
    }

    #[test]
    fn worker_response_revalidation_entry_for_simultaneously_opened_node_is_dropped() {
        // Patch C-O HIGH 3 — the cleanup adapter builds the
        // revalidation batch from a snapshot of state BEFORE the worker
        // response is applied. If the same response simultaneously
        // opens a node (sorry-free → sorryd) and surfaces a refreshed
        // record / still-unverified entry for that node in the batch,
        // the direct-insertion path used to install stale state.
        // Routing the batch through `apply_revalidation_batch` filters
        // the entry out because `state.live.open_nodes` (post-delta)
        // contains the node.
        let mut state = proof_burst_state(&["a", "b"], &["a", "b"], &[]);
        // a starts sorry-free with a record (so the simultaneous open
        // is a real sorry-free → sorryd transition).
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &[], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker opens a (post-delta sorryd).
        let mut new_live = state.live.clone();
        new_live.open_nodes.insert(NodeId::from("a"));

        // Batch carries a stale "refreshed" for a (built from pre-delta
        // snapshot where a was sorry-free).
        let stale_refresh = closure_record_for_test("a", &[], &[], &[]);
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("a"), stale_refresh)],
            still_unverified: vec![],
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_revalidation: Some(batch),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        // Post-state: a is sorryd, so it must NOT carry any closure
        // state (the §7.2 mutual-exclusion invariant) regardless of the
        // batch entry.
        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "simultaneously-opened node must NOT receive a record from the batch"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "simultaneously-opened node must NOT enter unverified set from the batch (mutex invariant)"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("a")),
            "simultaneously-opened node must NOT receive a failure summary from the batch"
        );
    }

    #[test]
    fn worker_response_revalidation_entry_for_simultaneously_deleted_node_is_dropped() {
        // Patch C-O HIGH 3 — the cleanup adapter builds the
        // revalidation batch from a snapshot of state BEFORE the worker
        // response is applied. If the same response simultaneously
        // deletes a node (orphan cleanup, present_nodes shrink) and
        // surfaces a still-unverified failure for that node in the
        // batch, the direct-insertion path used to install stale state.
        // Routing the batch through `apply_revalidation_batch` filters
        // the entry out because `state.live.present_nodes` (post-delta)
        // does not contain the node.
        let mut state = proof_burst_state(&["a", "b"], &["a", "b"], &[]);
        state.local_closure_records.insert(
            NodeId::from("b"),
            closure_record_for_test("b", &[], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker deletes b from present_nodes (orphan cleanup).
        let mut new_live = state.live.clone();
        new_live.present_nodes.remove(&NodeId::from("b"));

        let mut stale_summary = ErrorSummary::default();
        stale_summary.status = "axiom_violation".to_string();
        let batch = RevalidationBatch {
            refreshed: vec![],
            still_unverified: vec![(NodeId::from("b"), stale_summary)],
        };

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_revalidation: Some(batch),
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        // Post-state: b is gone from present_nodes; no closure state
        // should reference it.
        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("b")),
            "simultaneously-deleted node must NOT receive a record from the batch"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("b")),
            "simultaneously-deleted node must NOT receive a failure summary from the batch"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("b")),
            "simultaneously-deleted node must NOT be added to unverified set from the batch"
        );
    }

    #[test]
    fn proof_worker_accept_preserves_mutual_exclusion_invariant() {
        // Patch C-B §7.0 — `live.open_nodes ∩
        // local_closure_unverified_nodes` must be empty after every
        // accepted delta. The debug-build assertion in
        // `apply_local_closure_acceptance_bookkeeping` would have
        // panicked in this test if the helper accidentally inserted a
        // sorryd node into the unverified set; reaching the asserts
        // below confirms the invariant held.
        let mut state = proof_burst_state(&["a", "b"], &["a", "b"], &["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Construct a delta that simultaneously closes a (sorryd → sorry-
        // free) and structurally touches b (consumer-of-something
        // invalidation candidate). The bookkeeping must pick the right
        // bucket for each (record for a, no record for b but b is
        // sorry-free so it could enter unverified — but b has no
        // pre-existing record / consumer relationship so it stays out).
        let mut new_live = state.live.clone();
        new_live.open_nodes.clear();

        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("a"), probe);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply without violating mutual-exclusion invariant");

        let intersection: BTreeSet<NodeId> = outcome
            .state
            .live
            .open_nodes
            .intersection(&outcome.state.local_closure_unverified_nodes)
            .cloned()
            .collect();
        assert!(
            intersection.is_empty(),
            "live.open_nodes ∩ local_closure_unverified_nodes must be empty post-accept; got {:?}",
            intersection
        );
    }

    #[test]
    fn proof_worker_invalid_rejection_rolls_back_closure_speculation() {
        // Patch C-B + C-A — when a worker burst is rejected, any
        // closure-state speculation accepted during the burst must be
        // rolled back via restore_committed (extended in C-A). This
        // test ensures the C-B accept-time bookkeeping doesn't leak
        // into the post-rejection state.
        let mut state = proof_burst_state(&["a"], &["a", "b"], &["a"]);
        // Pre-burst: a is sorryd, no records. committed mirrors that.
        state.committed_local_closure_records = state.local_closure_records.clone();
        state.committed_local_closure_failures = state.local_closure_failures.clone();
        state.committed_local_closure_unverified_nodes =
            state.local_closure_unverified_nodes.clone();
        state.proof_invalid_review_threshold = 5;

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker emits an Invalid outcome → reject path fires
        // restore_committed.
        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: WorkingSnapshot::default(),
                    deterministic_rejection_reasons: vec!["bogus".to_string()],
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("invalid worker rejection must apply cleanly");

        // Post-reject: closure tier matches committed (still empty).
        assert!(
            outcome.state.local_closure_records.is_empty(),
            "rejection rollback must leave records map empty (matching committed)"
        );
        assert!(
            outcome.state.local_closure_failures.is_empty(),
            "rejection rollback must leave failures map empty (matching committed)"
        );
        assert!(
            outcome.state.local_closure_unverified_nodes.is_empty(),
            "rejection rollback must leave unverified set empty (matching committed)"
        );
    }

    // ---- Patch C-C synthetic pending_task tests --------------------------

    /// Build a minimal proof-phase state with a single sorry-free
    /// proof_node and an unverified-set entry pointing at it (so the
    /// auto-scheduler picks it). The `transport_only` flag controls
    /// whether the failure is transport_error (skip path) or not.
    fn proof_phase_unverified_only_state(
        name: &str,
        transport_only: bool,
        cycle: u32,
    ) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state.cycle = cycle;
        state.proof_nodes = set(&[name]);
        state.live.present_nodes = set(&[name]);
        state.live.open_nodes = BTreeSet::new();
        state.configured_targets = set(&["t"]);
        state.target_claims.insert(name.into(), set(&["t"]));
        state.approved_targets.configured_targets.insert("t".into());
        state
            .approved_targets
            .coverage
            .insert("t".into(), set(&[name]));
        state.live.coverage.insert("t".into(), set(&[name]));
        // Per-node verifier statuses Pass with matching fingerprints
        // so global_blockers is empty (paper-fingerprints invariant
        // requires current_paper_fingerprints to cover configured
        // targets — invariant check would otherwise reject the state).
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), format!("{name}=fp"));
        state.paper_status.insert("t".into(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert("t".into(), format!("{name}=fp"));
        state
            .live
            .corr_current_fingerprints
            .insert(name.into(), "corr".into());
        state.corr_status.insert(name.into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(name.into(), "corr".into());
        mark_substantiveness_pass(&mut state, name, "sub");
        state.committed = state.live.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_target_claims = state.target_claims.clone();
        // Unverified-only entry — no record installed.
        state.local_closure_unverified_nodes.insert(name.into());
        let summary = if transport_only {
            ErrorSummary {
                status: "transport_error".into(),
                returncode: -1,
                timed_out: false,
                stderr_excerpt: "checker socket unreachable".into(),
                axiom_violations: vec![],
                strict_errors: vec![],
                captured_at_cycle: cycle as u64,
                retry_count: 1,
                last_attempt_cycle: cycle as u64,
                next_retry_cycle: (cycle + 2) as u64,
                retry_exhausted: false,
            }
        } else {
            ErrorSummary {
                status: "axiom_violation".into(),
                returncode: 0,
                timed_out: false,
                stderr_excerpt: "uses sorryAx".into(),
                axiom_violations: vec!["sorryAx".into()],
                strict_errors: vec![],
                captured_at_cycle: cycle as u64,
                retry_count: 0,
                last_attempt_cycle: 0,
                next_retry_cycle: 0,
                retry_exhausted: false,
            }
        };
        state.local_closure_failures.insert(name.into(), summary);
        state
    }

    #[test]
    fn auto_scheduled_unverified_node_synthesizes_must_close_active_pending_task() {
        // Patch C-C plan §7.4.1 — when start_cycle's
        // proof_start_request_kind path picks an unverified-only
        // sorry-free proof_node via select_initial_proof_active_node,
        // a synthetic `pending_task` must be installed with
        // `must_close_active=true`, `allow_new_obligations=false`,
        // mode `Local`, and a non-empty diagnostic in
        // `reviewer_comments`.
        let state = proof_phase_unverified_only_state("Foo", false, 5);
        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        // Active node was set to Foo via select_initial_proof_active_node.
        assert_eq!(outcome.state.active_node.as_deref(), Some("Foo"));
        // Request was a Worker (no verifiers pending in this state).
        let request = first_issued_request(&outcome.commands);
        assert_eq!(request.kind, RequestKind::Worker);
        // Pending task carries the strict closure-revalidation defaults.
        let task = outcome
            .state
            .pending_task
            .as_ref()
            .expect("synthetic pending_task must be installed");
        assert!(
            task.must_close_active,
            "synthetic pending_task must set must_close_active=true"
        );
        assert!(
            !task.allow_new_obligations,
            "synthetic pending_task must set allow_new_obligations=false"
        );
        assert_eq!(task.mode, TaskMode::Local);
        assert!(
            task.task_blockers.is_empty(),
            "closure-revalidation task is not blocker-driven"
        );
        // Diagnostic in reviewer_comments names the node and a category.
        assert!(
            outcome.state.reviewer_comments.contains("Foo"),
            "diagnostic must name the failing node; got {:?}",
            outcome.state.reviewer_comments
        );
        assert!(
            outcome.state.reviewer_comments.contains("[axiom]"),
            "diagnostic must surface the failure category; got {:?}",
            outcome.state.reviewer_comments
        );
        // The request payload mirrors the pending_task into worker_context.
        assert!(
            request.worker_context.must_close_active,
            "Worker request must propagate must_close_active=true"
        );
        assert!(
            !request.worker_context.allow_new_obligations,
            "Worker request must propagate allow_new_obligations=false"
        );
    }

    #[test]
    fn auto_scheduler_does_not_pick_transport_error_only_unverified_node() {
        // Patch C-C plan §7.4.1 — transport-error-only failures must
        // NOT route through the worker burst path; they retry via the
        // deterministic-revalidation pass. With no other work on the
        // tablet, no node is auto-scheduled and no synthetic
        // pending_task is installed.
        let state = proof_phase_unverified_only_state("Foo", true, 5);
        let outcome = apply_event(state, ProtocolEvent::StartCycle).unwrap();
        assert_eq!(
            outcome.state.active_node, None,
            "transport-error-only node must not be selected as active"
        );
        // No synthetic pending_task installed (no node to pin).
        if let Some(task) = outcome.state.pending_task.as_ref() {
            assert_ne!(
                task.node.as_deref(),
                Some("Foo"),
                "transport-error-only node must not appear in synthetic pending_task; got {:?}",
                task
            );
        }
    }

    // ---- Patch C-E gap-fill tests --------------------------------------

    #[test]
    fn apply_revalidation_batch_is_idempotent_for_refreshed_records() {
        // Plan §7.5 — applying the same batch twice must converge to the
        // same state (no double-insertion, no flicker between mirrors).
        // The pure-state API must be safe to retry on transient errors.
        //
        // Audit Fix HIGH 6: populate live.present_nodes + proof_nodes so
        // the entry survives the post-update filter gate.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Foo"));
        state.proof_nodes.insert(NodeId::from("Foo"));
        state
            .local_closure_unverified_nodes
            .insert(NodeId::from("Foo"));
        let mut summary = ErrorSummary::default();
        summary.status = "axiom_violation".into();
        state
            .local_closure_failures
            .insert(NodeId::from("Foo"), summary);

        let record = closure_record_for_test("Foo", &["HelperH"], &["ThmT"], &["DefD"]);
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Foo"), record.clone())],
            still_unverified: Vec::new(),
        };
        apply_revalidation_batch(&mut state, batch.clone());
        let after_first = (
            state.local_closure_records.clone(),
            state.local_closure_unverified_nodes.clone(),
            state.local_closure_failures.clone(),
            state.boundary_statement_consumers.clone(),
            state.strict_dep_consumers.clone(),
        );

        // Second apply of the same batch — state must not drift.
        apply_revalidation_batch(&mut state, batch);
        assert_eq!(state.local_closure_records, after_first.0);
        assert_eq!(state.local_closure_unverified_nodes, after_first.1);
        assert_eq!(state.local_closure_failures, after_first.2);
        assert_eq!(state.boundary_statement_consumers, after_first.3);
        assert_eq!(state.strict_dep_consumers, after_first.4);
    }

    #[test]
    fn classify_record_eligibility_returns_not_present_when_node_absent_from_present_nodes() {
        // Patch C-Q Q10 — first arm of the triplet check. A node not in
        // `live.present_nodes` is classified `NotPresent`, regardless
        // of `proof_nodes` / `open_nodes` membership.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(NodeId::from("Ghost"));
        // Deliberately omit Ghost from present_nodes.
        assert_eq!(
            classify_record_eligibility(&state, &NodeId::from("Ghost")),
            RecordEligibility::NotPresent,
        );
    }

    #[test]
    fn classify_record_eligibility_returns_not_proof_for_definition_kind_node() {
        // Patch C-Q Q10 — second arm. A node present but not in
        // `proof_nodes` (e.g. a definition) is classified `NotProof`.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("MyDef"));
        // Deliberately omit MyDef from proof_nodes.
        assert_eq!(
            classify_record_eligibility(&state, &NodeId::from("MyDef")),
            RecordEligibility::NotProof,
        );
    }

    #[test]
    fn classify_record_eligibility_returns_open_for_sorryd_proof_node() {
        // Patch C-Q Q10 — third arm. A node in both present_nodes and
        // proof_nodes but ALSO in `live.open_nodes` is sorryd, so
        // closure records are forbidden by §7.2. Classify `Open`.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Sorryd"));
        state.proof_nodes.insert(NodeId::from("Sorryd"));
        state.live.open_nodes.insert(NodeId::from("Sorryd"));
        assert_eq!(
            classify_record_eligibility(&state, &NodeId::from("Sorryd")),
            RecordEligibility::Open,
        );
    }

    #[test]
    fn classify_record_eligibility_returns_eligible_for_sorry_free_present_proof_node() {
        // Patch C-Q Q10 — fourth arm. Sorry-free + present +
        // proof-bearing → `Eligible`. The only state in which a closure
        // record / unverified entry may be installed.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Closed"));
        state.proof_nodes.insert(NodeId::from("Closed"));
        // `open_nodes` empty by default → sorry-free.
        assert_eq!(
            classify_record_eligibility(&state, &NodeId::from("Closed")),
            RecordEligibility::Eligible,
        );
    }

    #[test]
    fn apply_revalidation_batch_preserves_mutual_exclusion_when_batch_only_has_sorry_free() {
        // Plan §7.0 — when the batch is built from `local_closure_unverified_nodes`
        // (sorry-free-only by invariant) the post-apply mutex invariant
        // `live.open_nodes ∩ local_closure_unverified_nodes = ∅` holds.
        // This mirrors the contract surface: the runtime CLI always
        // pulls revalidation candidates from the unverified set, never
        // from `live.open_nodes` directly.
        //
        // Audit Fix HIGH 6: populate A as a live present sorry-free proof
        // node so the batch's still_unverified entry passes the filter.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("A"));
        state.live.present_nodes.insert(NodeId::from("B"));
        state.proof_nodes.insert(NodeId::from("A"));
        state.proof_nodes.insert(NodeId::from("B"));
        state.live.open_nodes.insert(NodeId::from("B")); // sorryd; not in batch
        let mut summary = ErrorSummary::default();
        summary.status = "axiom_violation".into();
        let batch = RevalidationBatch {
            refreshed: Vec::new(),
            still_unverified: vec![(NodeId::from("A"), summary)],
        };
        apply_revalidation_batch(&mut state, batch);
        assert!(state
            .local_closure_unverified_nodes
            .contains(&NodeId::from("A")));
        let intersection: BTreeSet<NodeId> = state
            .live
            .open_nodes
            .intersection(&state.local_closure_unverified_nodes)
            .cloned()
            .collect();
        assert!(
            intersection.is_empty(),
            "mutex invariant must hold after apply_revalidation_batch on sorry-free-only batch; got {:?}",
            intersection
        );
    }

    #[test]
    fn apply_revalidation_batch_recomputes_reverse_indices_after_refresh_and_failure() {
        // Plan §7.5 + §7.2 — after `apply_revalidation_batch`, reverse
        // indices reflect the post-apply records map exactly (no stale
        // entries from earlier records that were displaced or removed
        // by the batch).
        //
        // Audit Fix HIGH 6: populate live.present_nodes + proof_nodes for
        // A so the refresh entry survives the filter.
        //
        // Audit C-1 (post-update): also populate the dep nodes
        // (HelperNew) into `live.present_nodes` so the canonical
        // predicate's dep-presence clause passes.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("A"));
        state.live.present_nodes.insert(NodeId::from("HelperOld"));
        state.live.present_nodes.insert(NodeId::from("HelperNew"));
        state.proof_nodes.insert(NodeId::from("A"));
        // Pre-existing record for A that consumes HelperOld.
        state.local_closure_records.insert(
            NodeId::from("A"),
            closure_record_for_test("A", &["HelperOld"], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        assert!(state
            .boundary_statement_consumers
            .contains_key(&NodeId::from("HelperOld")));

        // Batch refreshes A's record so it now consumes HelperNew, not Old.
        let refreshed_a = closure_record_for_test("A", &["HelperNew"], &[], &[]);
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("A"), refreshed_a)],
            still_unverified: Vec::new(),
        };
        apply_revalidation_batch(&mut state, batch);

        // Old consumer entry must be gone; new one must be present.
        assert!(
            !state
                .boundary_statement_consumers
                .contains_key(&NodeId::from("HelperOld")),
            "HelperOld must drop out of reverse index after A's record replaces it"
        );
        assert_eq!(
            state
                .boundary_statement_consumers
                .get(&NodeId::from("HelperNew")),
            Some(&BTreeSet::from([NodeId::from("A")])),
            "HelperNew must appear in reverse index after A's record refresh"
        );
    }

    #[test]
    fn restore_committed_then_recompute_reverse_indices_matches_committed_records() {
        // Plan §7.2 / §7.7 — after `restore_committed` rolls back the
        // live tier from the committed mirrors, the reverse indices must
        // be derivable from the restored records via the same recompute
        // helper used at startup (the rebuilt indices must match what a
        // cold-start would produce).
        let mut state = ProtocolState::default();
        let record_a = closure_record_for_test("A", &["H1"], &["T1"], &["D1"]);
        state
            .committed_local_closure_records
            .insert(NodeId::from("A"), record_a.clone());
        // Live tier corrupted/divergent — restore_committed must wipe it.
        state.local_closure_records.insert(
            NodeId::from("Z"),
            closure_record_for_test("Z", &["Hz"], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        assert!(state
            .boundary_statement_consumers
            .contains_key(&NodeId::from("Hz")));

        state.restore_committed();
        // After restore, indices must reflect ONLY the committed records.
        assert!(
            !state
                .boundary_statement_consumers
                .contains_key(&NodeId::from("Hz")),
            "restore_committed must drop stale live-only index entries"
        );
        assert_eq!(
            state.boundary_statement_consumers.get(&NodeId::from("H1")),
            Some(&BTreeSet::from([NodeId::from("A")])),
            "restored record's boundary index must be repopulated"
        );
        // Cross-check: a fresh recompute against the same restored
        // records produces identical indices (idempotency).
        let pre = state.boundary_statement_consumers.clone();
        recompute_local_closure_reverse_indices(&mut state);
        assert_eq!(state.boundary_statement_consumers, pre);
    }

    #[test]
    fn proof_worker_accept_does_not_invalidate_consumer_on_pure_proof_body_change() {
        // Plan §7.12 test 11 — a boundary helper's PROOF (value) change
        // does NOT invalidate a consumer that only consumed its statement.
        // This test exercises the conservative C-B accept path: when a
        // worker burst touches a node that is NOT in a consumer's record's
        // boundary/strict deps, the consumer's record must survive.
        let mut state = proof_burst_state(&["a"], &["a", "h", "z"], &[]);
        // a consumes h as boundary, NOT z.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["h"], &[], &[]),
        );
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Worker delta touches an UNRELATED node z (not in a's record).
        let mut node_kind_updates = BTreeMap::new();
        node_kind_updates.insert(NodeId::from("z"), Update::Set(NodeKind::Proof));

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: WorkingSnapshot {
                        present_nodes: BTreeSet::from([
                            NodeId::from("a"),
                            NodeId::from("h"),
                            NodeId::from("z"),
                        ]),
                        open_nodes: BTreeSet::new(),
                        ..Default::default()
                    },
                    node_kind_updates,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        // a's record survives — z is not in a's consumer set.
        assert!(
            outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "consumer a's record must NOT be invalidated by an unrelated structural delta"
        );
        assert!(
            !outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "a must not enter unverified set when only unrelated nodes mutate"
        );
    }

    // ===========================================================
    // Audit Fix HIGH 2 — accept-time content-change invalidation
    // ===========================================================
    //
    // The accept-time bookkeeping previously built `potentially_changed`
    // only from STRUCTURAL deltas (proof_node_updates / node_kind_updates
    // / dep_updates / target_claim_updates / present-node symdiff). It
    // missed CONTENT-only edits: a worker that changes the Lean proof
    // body or statement text without touching structural metadata still
    // produces fingerprint deltas in the response `WorkingSnapshot`. The
    // fix folds per-node fingerprint deltas into `potentially_changed`,
    // and additionally invalidates the PRODUCER's own record when its
    // own fingerprint changed (the structural walk previously only
    // invalidated CONSUMERS via the reverse indices).
    #[test]
    fn worker_response_changing_only_proof_body_invalidates_active_record() {
        // Audit Fix HIGH 2 (active-record half) — a worker burst that
        // changes only the proof body of a sorry-free active node must
        // invalidate that node's own local-closure record. The burst
        // carries no structural deltas (node_kind / dep / target_claim
        // / proof_node maps are all empty); the only signal is the
        // `corr_current_fingerprint` change for `a` in the response
        // snapshot. Pre-fix, the producer's own record survived; post-
        // fix, the record drops and `a` lands in the unverified set.
        let mut state = proof_burst_state(&["a"], &["a", "h"], &[]);
        // Active node a has a sorry-free record consuming h.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["h"], &[], &[]),
        );
        // Pre-delta fingerprint for a.
        state
            .live
            .corr_current_fingerprints
            .insert(NodeId::from("a"), "corr-a-v1".into());
        recompute_local_closure_reverse_indices(&mut state);
        state.committed_local_closure_records = state.local_closure_records.clone();
        state.committed = state.live.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Post-delta snapshot: SAME present_nodes / open_nodes / coverage
        // but corr-current fingerprint for a flips to v2 (representing a
        // recomputed semantic fingerprint after the proof body edit).
        let mut new_live = state.live.clone();
        new_live
            .corr_current_fingerprints
            .insert(NodeId::from("a"), "corr-a-v2".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    // No structural updates of any kind.
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "active node a's own record must be invalidated when only its proof body fingerprint changes"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "invalidated active node must enter unverified set"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("a")),
            "stale-by-invalidation must NOT write a failure summary (Patch C-D revalidation refreshes or fails)"
        );
    }

    #[test]
    fn boundary_statement_change_invalidates_consumer_records() {
        // Audit Fix HIGH 2 (consumer half) — a worker burst that changes
        // ONLY a helper's statement (no dep-set change, no structural
        // metadata change) must invalidate every consumer that recorded
        // that helper's statement hash. The reverse index
        // `boundary_statement_consumers` is the right structure to
        // query: pre-delta indices identify consumers; post-delta
        // fingerprint deltas identify which producers' content changed.
        let mut state = proof_burst_state(&["a", "h"], &["a", "h"], &[]);
        // Consumer a's record references h as a boundary helper. h has
        // its own (empty) record.
        state.local_closure_records.insert(
            NodeId::from("a"),
            closure_record_for_test("a", &["h"], &[], &[]),
        );
        state.local_closure_records.insert(
            NodeId::from("h"),
            closure_record_for_test("h", &[], &[], &[]),
        );
        // Pre-delta target_fingerprint for h (the proxy for h's
        // statement hash); a's fingerprint is fixed.
        state
            .live
            .target_fingerprints
            .insert(NodeId::from("h"), "stmt-h-v1".into());
        state
            .live
            .target_fingerprints
            .insert(NodeId::from("a"), "stmt-a-v1".into());
        recompute_local_closure_reverse_indices(&mut state);
        // Sanity: the reverse index surfaces a as a consumer of h.
        assert_eq!(
            state.boundary_statement_consumers.get(&NodeId::from("h")),
            Some(&BTreeSet::from([NodeId::from("a")])),
            "test setup: reverse index must list a as a consumer of h"
        );
        state.committed_local_closure_records = state.local_closure_records.clone();
        state.committed = state.live.clone();

        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        // Post-delta: ONLY h's target_fingerprint changes (statement
        // edit). No structural deltas at all. a's fingerprint is
        // unchanged.
        let mut new_live = state.live.clone();
        new_live
            .target_fingerprints
            .insert(NodeId::from("h"), "stmt-h-v2".into());

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    // No structural updates.
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "consumer a's record must be invalidated via reverse index when h's statement fingerprint changes"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "invalidated consumer a must enter unverified set"
        );
        assert!(
            !outcome
                .state
                .local_closure_failures
                .contains_key(&NodeId::from("a")),
            "consumer-stale-by-invalidation must NOT write a failure summary"
        );
        // h is the producer; its own fingerprint changed, so it loses
        // its own record too (the producer-own-record invalidation
        // arm).
        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("h")),
            "producer h's own record must be invalidated when its statement fingerprint changes"
        );
    }

    // ===========================================================
    // Audit Fix HIGH 6 — apply_revalidation_batch filtering
    // ===========================================================
    //
    // The pure-state batch installer previously inserted every entry
    // blindly into closure state. A stale or pre-response batch could
    // therefore reinsert a node into `local_closure_unverified_nodes`
    // or `local_closure_records` even after the node was opened,
    // deleted, or flipped to non-proof. The fix filters every entry
    // against `live.present_nodes ∧ proof_nodes ∧ ¬live.open_nodes`.
    #[test]
    fn apply_revalidation_batch_drops_entries_for_open_nodes() {
        // A node currently in `live.open_nodes` (sorryd) must NOT
        // receive a record install or unverified-set membership from
        // the batch — those would violate the sorry-free-only invariant
        // of plan §7.2.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Opened"));
        state.proof_nodes.insert(NodeId::from("Opened"));
        state.live.open_nodes.insert(NodeId::from("Opened"));

        let record = closure_record_for_test("Opened", &[], &[], &[]);
        let mut summary = ErrorSummary::default();
        summary.status = "axiom_violation".to_string();
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Opened"), record.clone())],
            still_unverified: vec![(NodeId::from("Opened"), summary)],
        };

        apply_revalidation_batch(&mut state, batch);

        assert!(
            !state
                .local_closure_records
                .contains_key(&NodeId::from("Opened")),
            "open node must NOT receive a record from the batch"
        );
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Opened")),
            "open node must NOT enter the unverified set from the batch (mutex invariant of §7.2)"
        );
        assert!(
            !state
                .local_closure_failures
                .contains_key(&NodeId::from("Opened")),
            "open node must NOT receive a failure summary from the batch"
        );
        // Mutex invariant: live.open_nodes ∩ unverified must be empty.
        let intersection: BTreeSet<NodeId> = state
            .live
            .open_nodes
            .intersection(&state.local_closure_unverified_nodes)
            .cloned()
            .collect();
        assert!(
            intersection.is_empty(),
            "mutex invariant must hold after apply_revalidation_batch on open-node batch; got {:?}",
            intersection
        );
    }

    #[test]
    fn apply_revalidation_batch_drops_entries_for_absent_nodes() {
        // A node not in `live.present_nodes` (deleted / never-existed)
        // must NOT receive any closure state from the batch.
        let mut state = ProtocolState::default();
        // Note: "Ghost" is intentionally absent from live.present_nodes.

        let record = closure_record_for_test("Ghost", &["Helper"], &[], &[]);
        let mut summary = ErrorSummary::default();
        summary.status = "transport_error".to_string();
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Ghost"), record)],
            still_unverified: vec![(NodeId::from("Ghost"), summary)],
        };

        apply_revalidation_batch(&mut state, batch);

        assert!(
            !state
                .local_closure_records
                .contains_key(&NodeId::from("Ghost")),
            "absent node must NOT receive a record from the batch"
        );
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Ghost")),
            "absent node must NOT enter the unverified set from the batch"
        );
        assert!(
            !state
                .local_closure_failures
                .contains_key(&NodeId::from("Ghost")),
            "absent node must NOT receive a failure summary from the batch"
        );
        // Reverse indices must not surface Helper (Ghost's would-be
        // boundary): no record installed means no consumer of Helper.
        assert!(
            !state
                .boundary_statement_consumers
                .contains_key(&NodeId::from("Helper")),
            "filtered-out batch entry must not leak into reverse index"
        );
    }

    #[test]
    fn apply_revalidation_batch_drops_entries_for_non_proof_nodes() {
        // A node present in `live.present_nodes` but NOT in
        // `proof_nodes` (e.g. a definition or axiom) must not enter the
        // closure-records lifecycle — those are proof-bearing only by
        // plan §7.0.
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("MyDef"));
        // Intentionally NOT inserted into proof_nodes.

        let record = closure_record_for_test("MyDef", &[], &[], &[]);
        let mut summary = ErrorSummary::default();
        summary.status = "strict_error".to_string();
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("MyDef"), record)],
            still_unverified: vec![(NodeId::from("MyDef"), summary)],
        };

        apply_revalidation_batch(&mut state, batch);

        assert!(
            !state
                .local_closure_records
                .contains_key(&NodeId::from("MyDef")),
            "non-proof node must NOT receive a record from the batch"
        );
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("MyDef")),
            "non-proof node must NOT enter the unverified set from the batch"
        );
        assert!(
            !state
                .local_closure_failures
                .contains_key(&NodeId::from("MyDef")),
            "non-proof node must NOT receive a failure summary from the batch"
        );
    }

    // ===========================================================
    // Audit C-1 — apply_revalidation_batch rejects stale records
    // whose kernel_semantic_hashes disagree with current state.
    // ===========================================================

    #[test]
    fn apply_revalidation_batch_rejects_record_with_stale_kernel_semantic_hash() {
        // Audit C-1 scenario: cleanup batch's record carries pre-burst
        // `kernel_semantic_hashes[Helper] = "F0"`, but the post-burst
        // state has `corr_current_fingerprints[Helper] = "F1"`. The
        // batch installer must drop the record AND mark the consumer
        // unverified (so the next probe pass refreshes it).
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Consumer"));
        state.live.present_nodes.insert(NodeId::from("Helper"));
        state.proof_nodes.insert(NodeId::from("Consumer"));
        state
            .live
            .corr_current_fingerprints
            .insert(NodeId::from("Helper"), "F1".to_string());

        // Build a record with kernel_semantic_hashes[Helper] = "F0"
        // (stale relative to live).
        let mut record = closure_record_for_test("Consumer", &["Helper"], &[], &[]);
        record.toolchain_hash = "live".to_string();
        record.lake_manifest_hash = "live".to_string();
        record.preamble_hash = "live".to_string();
        record.approved_axioms_hash = "live".to_string();
        record.active_decl_hash = "live".to_string();
        record.active_statement_hash = "live".to_string();
        record
            .kernel_semantic_hashes
            .insert(NodeId::from("Helper"), "F0".to_string());

        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Consumer"), record)],
            still_unverified: Vec::new(),
        };

        apply_revalidation_batch(&mut state, batch);

        assert!(
            !state
                .local_closure_records
                .contains_key(&NodeId::from("Consumer")),
            "stale-hash batch record must NOT be installed"
        );
        assert!(
            state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Consumer")),
            "stale-hash batch must force consumer into unverified for re-probe"
        );
    }

    // ===========================================================
    // Audit H-3 — apply_revalidation_batch_with_exclusions: cleanup
    // batch cannot overwrite a same-burst probe-derived record.
    // ===========================================================

    #[test]
    fn apply_revalidation_batch_with_exclusions_skips_already_installed_node() {
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(NodeId::from("Foo"));
        state.live.present_nodes.insert(NodeId::from("HelperH"));
        state.proof_nodes.insert(NodeId::from("Foo"));

        // Pre-installed record from the same burst's probe.
        let mut probe_record = closure_record_for_test("Foo", &["HelperH"], &[], &[]);
        probe_record.toolchain_hash = "live".to_string();
        probe_record.lake_manifest_hash = "live".to_string();
        probe_record.preamble_hash = "live".to_string();
        probe_record.approved_axioms_hash = "live".to_string();
        probe_record.active_decl_hash = "live".to_string();
        probe_record.active_statement_hash = "live".to_string();
        probe_record.accepted_at_snapshot_id = "fresh-probe".to_string();
        state
            .local_closure_records
            .insert(NodeId::from("Foo"), probe_record.clone());

        // Batch attempts to overwrite Foo with a staler record.
        let mut batch_record = closure_record_for_test("Foo", &["HelperH"], &[], &[]);
        batch_record.toolchain_hash = "live".to_string();
        batch_record.lake_manifest_hash = "live".to_string();
        batch_record.preamble_hash = "live".to_string();
        batch_record.approved_axioms_hash = "live".to_string();
        batch_record.active_decl_hash = "live".to_string();
        batch_record.active_statement_hash = "live".to_string();
        batch_record.accepted_at_snapshot_id = "stale-batch".to_string();
        let batch = RevalidationBatch {
            refreshed: vec![(NodeId::from("Foo"), batch_record)],
            still_unverified: Vec::new(),
        };

        let mut exclusions: BTreeSet<NodeId> = BTreeSet::new();
        exclusions.insert(NodeId::from("Foo"));
        apply_revalidation_batch_with_exclusions(&mut state, batch, &exclusions);

        let kept = state
            .local_closure_records
            .get(&NodeId::from("Foo"))
            .expect("Foo's record must survive");
        assert_eq!(
            kept.accepted_at_snapshot_id, "fresh-probe",
            "exclusion list must preserve same-burst probe record over stale batch entry"
        );
    }

    // ===========================================================
    // Audit Fix MEDIUM — defensive accept-time record install
    // ===========================================================
    //
    // Even with `must_close_active=true` enforcing a per-node approved
    // axiom subset check via the runtime CLI's `load_approved_axioms`,
    // the engine accept path itself trusted any probe with `status ==
    // "ok"` and empty `errors` to produce a record. A malformed or
    // replay-injected probe with a non-canonical axiom could therefore
    // be blessed at install time. The fix tightens the install gate to
    // also require `kernel_axioms ⊆ ENGINE_CANONICAL_APPROVED_AXIOMS`;
    // probes that escape the canonical four are routed to the failure
    // path even if the MCA gate accepted them.
    #[test]
    fn accept_time_record_install_rejects_record_with_unapproved_axiom() {
        // Audit Fix MEDIUM — a sorryd→sorry-free probe that reports
        // `status=="ok"` with empty `errors` but carries a non-canonical
        // axiom in `kernel_axioms` must NOT produce a record. Instead,
        // the engine writes an `axiom_violation` failure summary and
        // adds the node to `local_closure_unverified_nodes`.
        let mut state = proof_burst_state(&["a"], &["a", "b"], &["a"]);
        let request = issue_request_for_test(&mut state, RequestKind::Worker);

        let mut new_live = state.live.clone();
        new_live.open_nodes.clear();

        // Probe payload mimicking a malformed/replay-injected accept:
        // `status=="ok"`, `errors` empty, but `kernel_axioms` carries
        // an axiom NOT in the canonical four. Pre-fix, this would
        // install a record carrying the unapproved axiom; post-fix,
        // the engine refuses install.
        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        probe.kernel_axioms.insert("propext".to_string()); // canonical — allowed
        probe.kernel_axioms.insert("Lean.ofReduceBool".to_string()); // NON-canonical — disallowed

        let mut local_closure_results = BTreeMap::new();
        local_closure_results.insert(NodeId::from("a"), probe);

        let outcome = apply_event(
            state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(WorkerResponse {
                    request_id: request.id,
                    cycle: 5,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Valid,
                    snapshot: new_live,
                    local_closure_results,
                    ..WorkerResponse::default()
                }),
            },
        )
        .expect("valid delta should apply");

        assert!(
            !outcome
                .state
                .local_closure_records
                .contains_key(&NodeId::from("a")),
            "defensive gate must refuse to install a record carrying a non-canonical axiom"
        );
        let summary = outcome
            .state
            .local_closure_failures
            .get(&NodeId::from("a"))
            .expect("defensive gate must write a failure summary on canonical-axiom violation");
        assert_eq!(
            summary.status, "axiom_violation",
            "defensive gate must classify as axiom_violation"
        );
        assert!(
            summary
                .axiom_violations
                .contains(&"Lean.ofReduceBool".to_string()),
            "failure summary must record the non-canonical axiom that triggered the rejection"
        );
        assert!(
            !summary.axiom_violations.contains(&"propext".to_string()),
            "canonical axioms must NOT appear in the violation list"
        );
        assert!(
            outcome
                .state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("a")),
            "node rejected by defensive gate must surface in unverified set"
        );
    }

    // ---- Cleanup-v2 engine tests (Steps 18-20, 2026-05-14) ---------------

    /// Build a minimal Phase::Cleanup state with the audit sub-phase
    /// dispatched (stage = CleanupAudit, in-flight Audit request).
    /// Helper used by apply_audit_response tests.
    fn cleanup_audit_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::CleanupAudit;
        state.cycle = 1;
        state.cleanup_audit_round = 1;
        state.cleanup_audit_burst_count = 0;
        state.live.present_nodes = set(&["A", "B", "C"]);
        // Simulate an in-flight Audit request (the engine's
        // expect_stage check + cycle check rely on it).
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Audit,
            cycle: 1,
            ..WrapperRequest::default()
        });
        state
    }

    fn audit_response(
        outcome: AuditOutcome,
        new_tasks: Vec<NewCleanupAuditTask>,
        task_mods: Vec<CleanupAuditTaskModification>,
        scratchpad: String,
        status: ResponseStatus,
    ) -> AuditResponse {
        AuditResponse {
            request_id: 1,
            cycle: 1,
            status,
            new_tasks,
            task_modifications: task_mods,
            scratchpad_replace: scratchpad,
            outcome,
        }
    }

    #[test]
    fn apply_audit_response_valid_append_appends_and_increments() {
        let mut state = cleanup_audit_state();
        let response = audit_response(
            AuditOutcome::AuditDone,
            vec![NewCleanupAuditTask {
                target_node: NodeId::from("A"),
                rationale: "wrapper of Nat.add_comm".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::Substitution {
                    replacement: CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                },
            }],
            Vec::new(),
            "scratchpad note".into(),
            ResponseStatus::Ok,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply_audit_response");
        assert_eq!(state.cleanup_audit_tasks.len(), 1);
        assert_eq!(state.cleanup_audit_tasks[0].target_node, NodeId::from("A"));
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Pending
        ));
        assert_eq!(state.cleanup_audit_tasks[0].audit_origin_round, 1);
        assert_eq!(state.cleanup_audit_burst_count, 1);
        assert_eq!(state.cleanup_audit_scratchpad, "scratchpad note");
        // AuditDone routes to Reviewer.
        assert_eq!(state.stage, Stage::Reviewer);
        // Single IssueRequest (Reviewer) emitted.
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn apply_audit_response_need_to_continue_re_issues_audit() {
        let mut state = cleanup_audit_state();
        let response = audit_response(
            AuditOutcome::NeedToContinue,
            vec![NewCleanupAuditTask {
                target_node: NodeId::from("A"),
                rationale: "first burst".into(),
                confidence: CleanupTaskConfidence::Medium,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "unused var".into(),
                },
            }],
            Vec::new(),
            "".into(),
            ResponseStatus::Ok,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        assert_eq!(state.cleanup_audit_tasks.len(), 1);
        assert_eq!(state.cleanup_audit_burst_count, 1);
        // Stage stays CleanupAudit; next request is Audit.
        assert_eq!(state.stage, Stage::CleanupAudit);
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Audit
        ));
    }

    #[test]
    fn apply_audit_response_need_to_continue_force_done_at_cap() {
        let mut state = cleanup_audit_state();
        // Pre-fill burst count to one below the cap.
        state.cleanup_audit_burst_count = CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND - 1;
        let response = audit_response(
            AuditOutcome::NeedToContinue,
            Vec::new(),
            Vec::new(),
            "".into(),
            ResponseStatus::Ok,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        // burst_count == cap after increment; NeedToContinue with cap-hit
        // routes to Reviewer (forced Done).
        assert_eq!(
            state.cleanup_audit_burst_count,
            CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND
        );
        assert_eq!(state.stage, Stage::Reviewer);
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn apply_audit_response_validation_failure_first_retry_reissues_audit() {
        let mut state = cleanup_audit_state();
        // Propose an illegal task: target not in present_nodes.
        let response = audit_response(
            AuditOutcome::NeedToContinue,
            vec![NewCleanupAuditTask {
                target_node: NodeId::from("NotPresent"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Low,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "x".into(),
                },
            }],
            Vec::new(),
            "".into(),
            ResponseStatus::Ok,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        // Validation failed; task list NOT mutated; retry counter bumped.
        assert!(state.cleanup_audit_tasks.is_empty());
        assert_eq!(state.audit_burst_retry_count, 1);
        assert!(!state.latest_audit_rejection_reason.is_empty());
        // Stage stays CleanupAudit; next request is another Audit.
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Audit
        ));
    }

    #[test]
    fn apply_audit_response_validation_failure_second_consecutive_forces_audit_done() {
        let mut state = cleanup_audit_state();
        state.audit_burst_retry_count = 1; // pre-set: one retry already used
        let initial_burst_count = state.cleanup_audit_burst_count;
        let response = audit_response(
            AuditOutcome::NeedToContinue,
            vec![NewCleanupAuditTask {
                target_node: NodeId::from("NotPresent"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Low,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "x".into(),
                },
            }],
            Vec::new(),
            "".into(),
            ResponseStatus::Ok,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        // Second consecutive validation failure → force AuditDone.
        assert_eq!(state.audit_burst_retry_count, 0);
        assert_eq!(state.stage, Stage::Reviewer);
        // Cleanup-v2 (audit Finding 3): forced-AuditDone (validation
        // exhausted) must also bump `cleanup_audit_burst_count` for the
        // same reason as the malformed-exhausted path — the reviewer
        // Continue branch uses `cleanup_audit_burst_count > 0` to route
        // through cleanup-v2 (vs the legacy lint-only fallback).
        assert_eq!(
            state.cleanup_audit_burst_count,
            initial_burst_count + 1,
            "forced-AuditDone (validation exhausted) must increment cleanup_audit_burst_count"
        );
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn apply_audit_response_malformed_first_retries_audit() {
        let mut state = cleanup_audit_state();
        let response = audit_response(
            AuditOutcome::AuditDone, // ignored for Malformed
            Vec::new(),
            Vec::new(),
            "".into(),
            ResponseStatus::Malformed,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        assert_eq!(state.audit_burst_retry_count, 1);
        assert!(state.cleanup_audit_tasks.is_empty());
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Audit
        ));
    }

    #[test]
    fn apply_audit_response_malformed_second_consecutive_forces_audit_done() {
        let mut state = cleanup_audit_state();
        state.audit_burst_retry_count = 1;
        let initial_burst_count = state.cleanup_audit_burst_count;
        let response = audit_response(
            AuditOutcome::AuditDone,
            Vec::new(),
            Vec::new(),
            "".into(),
            ResponseStatus::Malformed,
        );
        let commands = apply_audit_response(&mut state, response).expect("apply");
        // Second malformed forces AuditDone → Reviewer.
        assert_eq!(state.audit_burst_retry_count, 0);
        assert_eq!(state.stage, Stage::Reviewer);
        // Cleanup-v2 (audit Finding 3): the forced-AuditDone path must
        // bump `cleanup_audit_burst_count` so the subsequent reviewer
        // Continue routes through cleanup-v2 (sub-cases A/B) rather than
        // the legacy lint-only fallback (sub-case C), which keys off
        // `cleanup_audit_burst_count > 0`.
        assert_eq!(
            state.cleanup_audit_burst_count,
            initial_burst_count + 1,
            "forced-AuditDone (malformed exhausted) must increment cleanup_audit_burst_count"
        );
        assert!(matches!(
            commands.first(),
            Some(ProtocolCommand::IssueRequest { request })
                if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn apply_audit_response_task_modification_dismisses_pending_current_round_task() {
        let mut state = cleanup_audit_state();
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "first burst".into(),
            confidence: CleanupTaskConfidence::Medium,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        let response = audit_response(
            AuditOutcome::AuditDone,
            Vec::new(),
            vec![CleanupAuditTaskModification {
                task_index: 0,
                reason: "second-look: not actually unused".into(),
            }],
            "".into(),
            ResponseStatus::Ok,
        );
        apply_audit_response(&mut state, response).expect("apply");
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Dismissed { .. }
        ));
    }

    /// Cleanup-v2 (audit Finding 4): round-2 audit IS permitted to revise
    /// round-1 leftover Pending tasks. Pending-status is the only gate;
    /// origin-round no longer matters for task_modifications. This test
    /// supersedes the prior `apply_audit_response_task_modification_rejects_out_of_round_index`
    /// test (which asserted the prior — too strict — rejection that
    /// contradicted `audit/05_loop_semantics.md`'s rendered semantics).
    #[test]
    fn apply_audit_response_task_modification_can_dismiss_round_1_pending_in_round_2() {
        let mut state = cleanup_audit_state();
        // Simulate the round-2 entry: leftover round-1 Pending task,
        // current round is 2.
        state.cleanup_audit_round = 2;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "round-1 proposal".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        let response = audit_response(
            AuditOutcome::AuditDone,
            Vec::new(),
            vec![CleanupAuditTaskModification {
                task_index: 0,
                reason: "round-2 re-examination: not actually a wrapper".into(),
            }],
            "".into(),
            ResponseStatus::Ok,
        );
        apply_audit_response(&mut state, response).expect("apply");
        assert_eq!(state.audit_burst_retry_count, 0);
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Dismissed { .. }
        ));
    }

    /// Cleanup-v2 (audit Finding 4): terminal-status round-1 tasks
    /// (Completed/Failed/Dismissed) remain immutable to round-2 audit
    /// task_modifications. Only Pending tasks are revisable.
    #[test]
    fn apply_audit_response_task_modification_rejects_terminal_round_1_task_in_round_2() {
        let mut state = cleanup_audit_state();
        state.cleanup_audit_round = 2;
        // Leftover round-1 Completed task — must not be touchable.
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "round-1 proposal".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Completed,
            audit_origin_round: 1,
        });
        let response = audit_response(
            AuditOutcome::AuditDone,
            Vec::new(),
            vec![CleanupAuditTaskModification {
                task_index: 0,
                reason: "trying to dismiss a Completed task".into(),
            }],
            "".into(),
            ResponseStatus::Ok,
        );
        apply_audit_response(&mut state, response).expect("apply");
        // Validation rejected the modification → retry counter bumped,
        // task status unchanged.
        assert_eq!(state.audit_burst_retry_count, 1);
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Completed
        ));
    }

    #[test]
    fn mark_cleanup_task_failed_increments_counter_and_latches_force_done_at_threshold() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        for _ in 0..(CLEANUP_CONSECUTIVE_INVALID_THRESHOLD - 1) {
            mark_cleanup_task_failed(&mut state, "reason".into());
            state.cleanup_active_task = Some(0); // re-pin for the next iteration
        }
        assert!(!state.cleanup_force_done);
        mark_cleanup_task_failed(&mut state, "third failure".into());
        assert!(state.cleanup_force_done);
        assert_eq!(
            state.cleanup_consecutive_invalid_workers,
            CLEANUP_CONSECUTIVE_INVALID_THRESHOLD
        );
    }

    #[test]
    fn mark_cleanup_task_completed_resets_counter_and_clears_active() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        state.cleanup_consecutive_invalid_workers = 2;
        mark_cleanup_task_completed(&mut state);
        assert_eq!(state.cleanup_consecutive_invalid_workers, 0);
        assert!(state.cleanup_active_task.is_none());
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Completed
        ));
    }

    #[test]
    fn re_audit_round_2_preserves_terminal_status_tasks() {
        // Regression test: when the reviewer requests a re-audit on Done
        // (round 1 → round 2), terminal-status tasks survive verbatim;
        // round 2's audit may revise only Pending current-round tasks
        // (which become round-2 tasks for revision purposes via the
        // audit_origin_round bump on new_tasks — old Pending tasks
        // STAY round-1 and become immutable for the round-2 audit).
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cleanup_audit_round = 1;
        state.cleanup_audit_burst_count = 3;
        state.cleanup_audit_scratchpad = "round 1 scratchpad".into();
        state.live.present_nodes = set(&["A", "B", "C", "D"]);
        // Populate one of each terminal status + one Pending.
        let tasks = vec![
            CleanupAuditTask {
                target_node: NodeId::from("A"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "x".into(),
                },
                status: CleanupTaskStatus::Completed,
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("B"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Medium,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "y".into(),
                },
                status: CleanupTaskStatus::Failed {
                    reason: "worker invalid".into(),
                },
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("C"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Low,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "z".into(),
                },
                status: CleanupTaskStatus::Dismissed {
                    reason: "reviewer-dismissed".into(),
                },
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("D"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Medium,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "w".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
        ];
        state.cleanup_audit_tasks = tasks.clone();
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Done;
        response.cycle = 5;
        response.cleanup_request_reaudit = true;

        let _ = apply_cleanup_review_response(&mut state, response).expect("review");

        // Re-audit fired: round bumped, scratchpad cleared, burst count reset.
        assert_eq!(state.cleanup_audit_round, 2);
        assert_eq!(state.cleanup_audit_burst_count, 0);
        assert!(state.cleanup_audit_scratchpad.is_empty());
        // Phase stayed Cleanup (no flip to Complete).
        assert_eq!(state.phase, Phase::Cleanup);
        // Stage::Start so start_cycle dispatches the round-2 Audit.
        assert_eq!(state.stage, Stage::Start);
        // All tasks preserved.
        assert_eq!(state.cleanup_audit_tasks.len(), 4);
        for (i, expected) in tasks.iter().enumerate() {
            assert_eq!(
                state.cleanup_audit_tasks[i].target_node,
                expected.target_node
            );
            assert_eq!(state.cleanup_audit_tasks[i].status, expected.status);
            assert_eq!(
                state.cleanup_audit_tasks[i].audit_origin_round,
                expected.audit_origin_round
            );
        }
    }

    #[test]
    fn re_audit_request_ignored_at_max_rounds() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cleanup_audit_round = CLEANUP_AUDIT_MAX_ROUNDS;
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Done;
        response.cycle = 5;
        response.cleanup_request_reaudit = true;
        let _ = apply_cleanup_review_response(&mut state, response).expect("review");
        // Done finalized; phase flipped to Complete despite reaudit request.
        assert_eq!(state.phase, Phase::Complete);
        assert_eq!(state.stage, Stage::Complete);
    }

    #[test]
    fn re_audit_request_ignored_when_force_done_latched() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cleanup_audit_round = 1; // round 1 of 2, would normally allow reaudit
        state.cleanup_force_done = true; // but force_done latched
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Done;
        response.cycle = 5;
        response.cleanup_request_reaudit = true;
        let _ = apply_cleanup_review_response(&mut state, response).expect("review");
        assert_eq!(state.phase, Phase::Complete);
    }

    #[test]
    fn cleanup_review_continue_bulk_dismiss_marks_pending_tasks_dismissed() {
        // Step 18 (test plan): Continue with dismissals only; no
        // dispatch; all listed Pending tasks transition to Dismissed.
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cleanup_audit_tasks = vec![
            CleanupAuditTask {
                target_node: NodeId::from("A"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "x".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("B"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Medium,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "y".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("C"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::Low,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "z".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
        ];
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Continue;
        response.cycle = 5;
        response.reset = ResetChoice::None;
        response.cleanup_dismiss_tasks =
            vec![(0, "not worth it".into()), (2, "out of scope".into())];
        response.cleanup_next_task = None;
        let _ = apply_cleanup_review_response(&mut state, response).expect("review");

        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Dismissed { .. }
        ));
        assert!(matches!(
            state.cleanup_audit_tasks[1].status,
            CleanupTaskStatus::Pending
        )); // not in dismiss list — preserved
        assert!(matches!(
            state.cleanup_audit_tasks[2].status,
            CleanupTaskStatus::Dismissed { .. }
        ));
        assert!(state.cleanup_active_task.is_none());
    }

    #[test]
    fn cleanup_review_continue_dispatches_pending_task_with_authorized_nodes() {
        // Step 18: Continue + cleanup_next_task → cleanup_active_task set;
        // PendingTask built with authorized_nodes from the reviewer.
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        state.cleanup_audit_tasks = vec![CleanupAuditTask {
            target_node: NodeId::from("Target"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "X".into(),
                },
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        }];
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let authorized = set::<NodeId>(&["Importer1", "Importer2"]);
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Continue;
        response.cycle = 5;
        response.reset = ResetChoice::None;
        response.cleanup_next_task = Some(0);
        response.authorized_nodes = authorized.clone();
        let _ = apply_cleanup_review_response(&mut state, response).expect("review");

        assert_eq!(state.cleanup_active_task, Some(0));
        let pending = state.pending_task.expect("pending_task should be set");
        assert_eq!(pending.authorized_nodes, authorized);
        assert_eq!(pending.mode, TaskMode::Cleanup);
        // Cleanup-v2 single-source-of-truth: pending_task.node and
        // state.active_node are derived from the dispatched task's
        // target_node, not from response.next_active.
        assert_eq!(pending.node, Some(NodeId::from("Target")));
        assert_eq!(state.active_node, Some(NodeId::from("Target")));
    }

    /// Cleanup-v2 (audit Finding 4): dismiss-only Continue (cleanup_next_task
    /// = None, cleanup_dismiss_tasks non-empty) with Pending tasks remaining
    /// should NOT set a pending_task (no legacy Worker dispatch) and should
    /// re-issue Review for the next cycle.
    #[test]
    fn cleanup_review_continue_dismiss_only_reissues_review_not_worker() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 5;
        // Audit has already run: burst_count > 0 indicates we're in
        // cleanup-v2 loop, not legacy lint-only fallback.
        state.cleanup_audit_burst_count = 1;
        state.cleanup_audit_tasks = vec![
            CleanupAuditTask {
                target_node: NodeId::from("A"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "x".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
            CleanupAuditTask {
                target_node: NodeId::from("B"),
                rationale: "".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::LintFix {
                    warning_text: "y".into(),
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            },
        ];
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 5,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Continue;
        response.cycle = 5;
        response.reset = ResetChoice::None;
        response.cleanup_dismiss_tasks = vec![(0, "discarded".into())];
        response.cleanup_next_task = None;
        let commands = apply_cleanup_review_response(&mut state, response).expect("review");

        // Audit Finding 4: dismiss-only Continue must NOT set pending_task.
        assert!(
            state.pending_task.is_none(),
            "pending_task must be empty on dismiss-only Continue"
        );
        assert!(state.cleanup_active_task.is_none());
        // Stage::Reviewer to drive the next cycle to a Review request, not
        // a Worker request.
        assert_eq!(state.stage, Stage::Reviewer);
        // Should have re-issued a Review request.
        assert!(commands.iter().any(|c| matches!(
            c,
            ProtocolCommand::IssueRequest { request } if request.kind == RequestKind::Review
        )));
        // First dismissal applied; the other task remains Pending.
        assert!(matches!(
            state.cleanup_audit_tasks[0].status,
            CleanupTaskStatus::Dismissed { .. }
        ));
        assert!(matches!(
            state.cleanup_audit_tasks[1].status,
            CleanupTaskStatus::Pending
        ));
    }

    /// Cleanup-v2 (audit Finding 4): when all Pending tasks are dismissed
    /// in one Continue, auto-Done triggers (Phase::Complete) since no work
    /// remains.
    #[test]
    fn cleanup_review_continue_dismiss_all_pending_triggers_auto_done() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Reviewer;
        state.cycle = 7;
        state.cleanup_audit_burst_count = 2;
        state.cleanup_audit_tasks = vec![CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        }];
        state.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            cycle: 7,
            ..WrapperRequest::default()
        });
        let mut response = ReviewResponse::default();
        response.decision = ReviewDecisionKind::Continue;
        response.cycle = 7;
        response.reset = ResetChoice::None;
        response.cleanup_dismiss_tasks = vec![(0, "stale".into())];
        response.cleanup_next_task = None;
        let commands = apply_cleanup_review_response(&mut state, response).expect("review");
        // Audit Finding 4: no Pending tasks remain → auto-Done.
        assert_eq!(state.phase, Phase::Complete);
        assert_eq!(state.stage, Stage::Complete);
        // Should emit a CommitCheckpoint, not an IssueRequest.
        assert!(commands
            .iter()
            .any(|c| matches!(c, ProtocolCommand::CommitCheckpoint)));
    }

    /// Cleanup-v2 (audit Finding 6): when a cleanup worker burst is
    /// rejected (Malformed/Invalid), the task is marked Failed BEFORE
    /// the next Review request is issued — so the in-flight Review
    /// reflects the new status, not stale Pending.
    #[test]
    fn apply_cleanup_worker_response_invalid_marks_task_failed_before_review_issued() {
        use crate::model::WorkerOutcome;
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Worker;
        state.cycle = 9;
        state.cleanup_audit_burst_count = 1;
        state.cleanup_audit_tasks = vec![CleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        }];
        state.cleanup_active_task = Some(0);
        state.pending_task = Some(PendingTask {
            mode: TaskMode::Cleanup,
            ..PendingTask::default()
        });
        let request = WrapperRequest {
            id: 1,
            kind: RequestKind::Worker,
            phase: Phase::Cleanup,
            cycle: 9,
            ..WrapperRequest::default()
        };
        state.in_flight_request = Some(request.clone());
        let response = WorkerResponse {
            request_id: 1,
            cycle: 9,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Invalid,
            ..WorkerResponse::default()
        };
        let _ = apply_cleanup_worker_response(&mut state, &request, response).expect("worker");
        // Audit Finding 6: the task must be marked Failed at the point the
        // Review is re-issued, not after. Confirm via the in_flight_request
        // — its cleanup_audit_tasks_view should now contain Failed.
        let in_flight = state
            .in_flight_request
            .as_ref()
            .expect("in-flight Review request");
        assert_eq!(in_flight.kind, RequestKind::Review);
        assert!(
            !in_flight.cleanup_audit_tasks_view.is_empty(),
            "in-flight Review should surface the cleanup_audit_tasks view"
        );
        assert!(
            matches!(
                in_flight.cleanup_audit_tasks_view[0].status,
                CleanupTaskStatus::Failed { .. }
            ),
            "task status in the in-flight Review must be Failed, not stale Pending"
        );
    }

    /// Cleanup-v2 (audit Finding 9): two identical (target_node, kind)
    /// entries within the SAME audit response are rejected as duplicates.
    #[test]
    fn apply_audit_response_rejects_intra_burst_duplicate_tasks() {
        let mut state = cleanup_audit_state();
        let task = NewCleanupAuditTask {
            target_node: NodeId::from("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "Nat.add_comm".into(),
                },
            },
        };
        let response = audit_response(
            AuditOutcome::AuditDone,
            vec![task.clone(), task.clone()],
            Vec::new(),
            "".into(),
            ResponseStatus::Ok,
        );
        let _ = apply_audit_response(&mut state, response).expect("retry path");
        // Validation should have failed: latest_audit_rejection_reason set.
        assert!(
            state.latest_audit_rejection_reason.contains("duplicate"),
            "intra-burst duplicate should produce duplicate rejection: {}",
            state.latest_audit_rejection_reason
        );
        // No tasks should have been appended.
        assert!(state.cleanup_audit_tasks.is_empty());
        // Retry counter bumped.
        assert_eq!(state.audit_burst_retry_count, 1);
    }

    /// Build a ProofFormalization state whose `refresh_stuck_math_audit_latch`
    /// keeps the latch active (shallow-coarse no-progress trigger), with a
    /// caller-supplied `audit_plan` and `last_stuck_math_audit_dispatched_cycle`.
    fn audit_dispatch_state(
        audit_plan: Option<AuditPlan>,
        last_dispatched: u32,
        cycle: u32,
    ) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.cycle = cycle;
        // Activate the shallow-coarse no-progress trigger so refresh
        // doesn't clear the latch underneath us.
        state.coarse_dag_nodes = set(&["Foo"]);
        state.live.present_nodes = set(&["Foo"]);
        state.live.open_nodes = set(&["Foo"]);
        state.shallow_coarse_closed_count = 0;
        state.cycles_since_shallow_coarse_closed_count_increase =
            STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT;
        state.audit_plan = audit_plan;
        state.last_stuck_math_audit_dispatched_cycle = Some(last_dispatched);
        state
    }

    fn make_test_audit_plan() -> AuditPlan {
        AuditPlan {
            report: "test report".to_string(),
            tasks: Vec::new(),
            probe_paths: Vec::new(),
            need_input_audit: false,
            cone_clean_node: None,
            written_at_cycle: 1,
            written_by_request: 1,
            trigger_at_write: "test".to_string(),
        }
    }

    #[test]
    fn audit_redispatches_periodically_even_when_plan_present() {
        let interval = stuck_math_audit_reaudit_interval_cycles();
        // Just below interval: should NOT dispatch.
        let mut s = audit_dispatch_state(Some(make_test_audit_plan()), 10, 10 + interval - 1);
        assert!(
            !should_dispatch_stuck_math_audit(&mut s),
            "must not re-audit before {interval} cycles elapsed"
        );
        // At interval boundary: should dispatch.
        let mut s = audit_dispatch_state(Some(make_test_audit_plan()), 10, 10 + interval);
        assert!(
            should_dispatch_stuck_math_audit(&mut s),
            "must re-audit at {interval}-cycle boundary even though audit_plan is set"
        );
    }

    #[test]
    fn last_clean_rewind_forces_audit_even_when_reaudit_interval_unmet() {
        let interval = stuck_math_audit_reaudit_interval_cycles();
        // Below the interval AND with audit_plan present — normally would
        // return Review. With the force flag set (as apply_last_clean_reset
        // does), the audit must dispatch instead.
        let mut s = audit_dispatch_state(Some(make_test_audit_plan()), 100, 100 + interval - 1);
        s.force_stuck_math_audit_after_rewind = true;
        assert!(
            should_dispatch_stuck_math_audit(&mut s),
            "force_stuck_math_audit_after_rewind must override the reaudit-interval gate"
        );
        assert!(
            !s.force_stuck_math_audit_after_rewind,
            "force flag must be consumed (set false) on dispatch decision"
        );
    }

    #[test]
    fn last_clean_rewind_forces_audit_even_when_latch_inactive() {
        let mut s = ProtocolState::default();
        s.phase = Phase::ProofFormalization;
        s.stage = Stage::Reviewer;
        s.cycle = 100;
        // No latch trigger configured — refresh would normally leave the
        // latch off and dispatch return false. The force flag activates
        // the latch and dispatches anyway.
        assert!(!s.stuck_math_audit.active, "test premise: latch inactive");
        s.force_stuck_math_audit_after_rewind = true;
        assert!(
            should_dispatch_stuck_math_audit(&mut s),
            "force flag must trigger dispatch even when the usual triggers don't fit"
        );
        assert!(
            s.stuck_math_audit.active,
            "force flag must auto-activate the latch"
        );
        assert!(s.stuck_math_audit.trigger.contains("reset/rewind"));
    }

    #[test]
    fn audit_with_no_plan_still_uses_cooldown_not_reaudit_interval() {
        let cooldown = stuck_math_audit_dispatch_cooldown_cycles();
        let interval = stuck_math_audit_reaudit_interval_cycles();
        // Pick a cycle gap satisfying cooldown but below reaudit interval.
        // The no-plan branch must use the (smaller) cooldown, not the
        // re-audit interval, so this should dispatch.
        assert!(
            cooldown < interval,
            "test premise: cooldown ({cooldown}) < interval ({interval})"
        );
        let mut s = audit_dispatch_state(None, 10, 10 + cooldown);
        assert!(
            should_dispatch_stuck_math_audit(&mut s),
            "no-plan branch must dispatch at cooldown boundary, not wait for reaudit interval"
        );
    }

    /// Build a `Phase::TheoremStating` state whose `commit_live` will
    /// produce a non-empty Sound-blocker NODE set carried by `node`.
    /// The configured target and held_target are arranged so the
    /// node is in scope for sound verification (`needs_sound` true, and
    /// `select_theorem_held_target` returns the node).
    fn theorem_stating_sound_blocker_state(node: &str) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Start;
        state.cycle = 1;
        let target = "t";
        state.configured_targets = set(&[target]);
        // `held_target` is by-node, not by-target. The select helper
        // pins it to the node that claims the target.
        state.proof_nodes = set(&[node]);
        state.target_claims.insert(node.into(), set(&[target]));
        state.live.present_nodes = set(&[node]);
        state.live.open_nodes = set(&[node]);
        state.live.coverage.insert(target.into(), set(&[node]));
        // Paper + Corr pinned Pass so the only blocker carrier is Sound.
        state
            .live
            .paper_current_fingerprints
            .insert(target.into(), format!("{node}=t"));
        state
            .live
            .target_fingerprints
            .insert(node.into(), "t".into());
        state
            .live
            .corr_current_fingerprints
            .insert(node.into(), "c".into());
        state.corr_status.insert(node.into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(node.into(), "c".into());
        state.paper_status.insert(target.into(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert(target.into(), format!("{node}=t"));
        mark_substantiveness_pass(&mut state, node, "sub");
        // Sound is Unknown (no fingerprint pinned approved=current → Unknown,
        // not Pass), which produces a Soundness blocker for this node
        // when `needs_sound` is true.
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state
    }

    /// Mark `node` as Sound-Pass with matching approved/current
    /// fingerprints (so the node leaves `current_sound_blocker_node_set`).
    fn mark_sound_pass(state: &mut ProtocolState, node: &str, fp: &str) {
        let n = NodeId::from(node);
        state
            .live
            .sound_current_fingerprints
            .insert(n.clone(), fp.into());
        state
            .sound_approved_fingerprints
            .insert(n.clone(), fp.into());
        state.sound_status.insert(n, SoundStatus::Pass);
    }

    /// Mark `node` as Sound-Unknown by clearing its assessed status and
    /// approved fingerprint (so the node re-enters
    /// `current_sound_blocker_node_set`). Mirrors how a Sound
    /// regression / re-verification on a drifted dep would manifest in
    /// live state for the purposes of progress accounting.
    fn mark_sound_unknown(state: &mut ProtocolState, node: &str) {
        let n = NodeId::from(node);
        state.sound_status.remove(&n);
        state.sound_approved_fingerprints.remove(&n);
        state.live.sound_current_fingerprints.remove(&n);
        state.sound_assessments.remove(&n);
    }

    /// Build a two-node TheoremStating state with both nodes initially
    /// Sound-Unknown (so both are Sound-blocker carriers). Mirrors
    /// `theorem_stating_sound_blocker_state` but with a configurable
    /// node set.
    fn theorem_stating_multi_node_state(nodes: &[&str]) -> ProtocolState {
        let target = TargetId::from("t");
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::Start;
        state.cycle = 1;
        state.configured_targets = BTreeSet::from([target.clone()]);
        let node_ids: BTreeSet<NodeId> = nodes.iter().map(|n| NodeId::from(*n)).collect();
        state.proof_nodes = node_ids.clone();
        // First node carries the target so coverage is non-empty.
        let head = NodeId::from(nodes[0]);
        state
            .target_claims
            .insert(head.clone(), BTreeSet::from([target.clone()]));
        state.live.present_nodes = node_ids.clone();
        state.live.open_nodes = node_ids.clone();
        state
            .live
            .coverage
            .insert(target.clone(), BTreeSet::from([head.clone()]));
        state
            .live
            .paper_current_fingerprints
            .insert(target.clone(), format!("{}=t", nodes[0]));
        state
            .live
            .target_fingerprints
            .insert(head.clone(), "t".into());
        state.paper_status.insert(target.clone(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert(target.clone(), format!("{}=t", nodes[0]));
        for n in nodes {
            let nid = NodeId::from(*n);
            state
                .live
                .corr_current_fingerprints
                .insert(nid.clone(), format!("c{n}"));
            state.corr_status.insert(nid.clone(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert(nid.clone(), format!("c{n}"));
            mark_substantiveness_pass(&mut state, n, &format!("s{n}"));
        }
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state
    }

    /// Add a new node `n` to the live + committed structural view
    /// mid-stream. Inherits paper/corr Pass / substantiveness Pass so
    /// the only blocker carrier remains Sound. Used for the
    /// "new-node-mid-window" scenario.
    fn add_sound_unknown_node(state: &mut ProtocolState, n: &str) {
        let nid = NodeId::from(n);
        state.proof_nodes.insert(nid.clone());
        state.committed_proof_nodes.insert(nid.clone());
        state.live.present_nodes.insert(nid.clone());
        state.live.open_nodes.insert(nid.clone());
        state.committed.present_nodes.insert(nid.clone());
        state.committed.open_nodes.insert(nid.clone());
        state
            .live
            .corr_current_fingerprints
            .insert(nid.clone(), format!("c{n}"));
        state
            .committed
            .corr_current_fingerprints
            .insert(nid.clone(), format!("c{n}"));
        state.corr_status.insert(nid.clone(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(nid.clone(), format!("c{n}"));
        mark_substantiveness_pass(state, n, &format!("s{n}"));
    }

    /// Remove `n` from the live + committed structural view. Used for
    /// the "deletion-mid-window" scenario.
    fn remove_node(state: &mut ProtocolState, n: &str) {
        let nid = NodeId::from(n);
        state.proof_nodes.remove(&nid);
        state.committed_proof_nodes.remove(&nid);
        state.live.present_nodes.remove(&nid);
        state.live.open_nodes.remove(&nid);
        state.committed.present_nodes.remove(&nid);
        state.committed.open_nodes.remove(&nid);
        state.live.corr_current_fingerprints.remove(&nid);
        state.committed.corr_current_fingerprints.remove(&nid);
        state.corr_status.remove(&nid);
        state.corr_approved_fingerprints.remove(&nid);
        state.substantiveness_status.remove(&nid);
        state.substantiveness_approved_fingerprints.remove(&nid);
        state.live.substantiveness_current_fingerprints.remove(&nid);
        state
            .committed
            .substantiveness_current_fingerprints
            .remove(&nid);
    }

    /// Scenario 1 (oscillation): k=4, two nodes alternate which is
    /// sound. At the latest checkpoint both are unsound; an origin
    /// with both unsound has neither surviving node going unprog→prog,
    /// so the gate fires.
    #[test]
    fn no_sound_progress_gate_fires_on_oscillation() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_multi_node_state(&["a", "b"]);
        // Cycle 0 (origin): both unsound.
        state.commit_live();
        // Cycles 1..=k: alternate; latest cycle has both unsound to
        // ensure no surviving node ever ends up progressed at latest.
        for i in 1..=k {
            if i % 2 == 1 {
                mark_sound_pass(&mut state, "a", "sa-pass");
                mark_sound_unknown(&mut state, "b");
            } else {
                mark_sound_pass(&mut state, "b", "sb-pass");
                mark_sound_unknown(&mut state, "a");
            }
            state.commit_live();
        }
        // One more cycle with both unsound so latest.progressed = {}.
        mark_sound_unknown(&mut state, "a");
        mark_sound_unknown(&mut state, "b");
        state.commit_live();
        assert!(
            state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "k={k} cycles of oscillation must fire the no-Sound-progress gate"
        );
        state.refresh_stuck_math_audit_latch();
        assert!(state.stuck_math_audit.active);
        assert!(
            state
                .stuck_math_audit
                .trigger
                .starts_with("sound-stagnation-window:"),
            "trigger reason must use the canonical sound-stagnation-window prefix, got {:?}",
            state.stuck_math_audit.trigger
        );
        assert!(state.stuck_math_audit.trigger.contains("theorem-stating"));
    }

    /// Scenario 2 (monotone progress): k=4, each cycle one
    /// previously-unsound node becomes sound; for every eligible
    /// origin some surviving node progressed → gate stays off.
    #[test]
    fn no_sound_progress_gate_stays_off_under_monotone_progress() {
        let k = stuck_math_audit_no_sound_progress_window();
        // (k+2) nodes so every cycle can close one and still leave at
        // least one carrier open (so `current_sound_blocker_node_set`
        // stays non-empty, the guard for the trigger).
        let names: Vec<String> = (0..(k + 2)).map(|i| format!("n{i}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut state = theorem_stating_multi_node_state(&refs);
        // Cycle 0: all unsound.
        state.commit_live();
        // Cycles 1..=k+1: close one new node each cycle.
        for i in 0..=k {
            mark_sound_pass(&mut state, &names[i as usize], &format!("p{i}"));
            state.commit_live();
        }
        assert!(
            !state.current_sound_blocker_node_set().is_empty(),
            "test setup: at least one node must remain a Sound blocker"
        );
        assert!(
            !state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "gate must stay off when every origin saw some surviving node make unprog->prog progress"
        );
    }

    /// Scenario 3 (new node mid-window): a node added mid-stream that
    /// becomes sound does NOT count as progress for the original
    /// stagnation window — it was not present at the origin and so is
    /// outside the surviving intersection.
    #[test]
    fn no_sound_progress_gate_ignores_progress_on_nodes_added_mid_window() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_multi_node_state(&["a", "b"]);
        // Cycle 0 (origin): {a,b} present, both unsound.
        state.commit_live();
        // Cycles 1..=k: a,b stagnant; halfway through add `c` and
        // close it. The latest progressed set contains c but
        // `present @ origin ∩ present @ latest = {a, b}` excludes c.
        for i in 1..=k {
            if i == k / 2 {
                add_sound_unknown_node(&mut state, "c");
            }
            if i == (k / 2) + 1 {
                mark_sound_pass(&mut state, "c", "sc-pass");
            }
            state.commit_live();
        }
        // One more for k+1 snapshots total.
        state.commit_live();
        assert!(
            state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "progress on a mid-window-added node must NOT count for the original surviving intersection; gate must fire"
        );
    }

    /// Scenario 4 (deletion mid-window): deleting a node mid-stream
    /// drops it from the surviving intersection; the remaining
    /// surviving nodes' stagnation still drives the gate. Deletion
    /// does not whitewash the streak.
    #[test]
    fn no_sound_progress_gate_fires_when_only_deleted_node_made_progress() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_multi_node_state(&["a", "b"]);
        // Cycle 0: {a,b}, both unsound.
        state.commit_live();
        // Cycles 1..=k: delete `a` halfway through. `b` stays
        // unsound throughout.
        for i in 1..=k {
            if i == k / 2 {
                remove_node(&mut state, "a");
            }
            state.commit_live();
        }
        state.commit_live();
        // Surviving intersection at any origin includes b only;
        // b never progressed → gate fires.
        assert!(
            state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "gate must fire when surviving nodes (after deletion) stayed unprogressed across the window"
        );
    }

    /// Scenario 5 (pure regression): a node sound at origin becomes
    /// unsound at latest. That is unprog@latest while prog@origin —
    /// regression, NOT progress. Gate fires.
    #[test]
    fn no_sound_progress_gate_fires_on_pure_regression() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_multi_node_state(&["a", "b"]);
        // Cycle 0 (origin): `a` is sound, `b` is unsound. Latest will
        // have both unsound. Origin: progressed={a}, present={a,b}.
        mark_sound_pass(&mut state, "a", "sa-orig");
        state.commit_live();
        // Cycles 1..=k: `a` regresses to unsound, `b` stays unsound.
        mark_sound_unknown(&mut state, "a");
        for _ in 1..=k {
            state.commit_live();
        }
        state.commit_live();
        // At origin a was prog, at latest a is unprog. b unprog both.
        // No surviving node went unprog->prog. Gate fires.
        assert!(
            state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "a sound->unsound regression does not count as progress; gate must fire"
        );
    }

    /// Scenario 6 (cold start): a fresh `ProgressHistory` with < k+1
    /// snapshots cannot satisfy the depth requirement; gate stays off.
    #[test]
    fn no_sound_progress_gate_stays_off_until_buffer_fills() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_multi_node_state(&["a"]);
        // Run exactly k commit_lives. Buffer indices: 0..=k-1.
        // Latest=k-1, origin=0 has depth k-1 < k → not eligible.
        for _ in 0..k {
            state.commit_live();
        }
        assert!(
            !state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "gate must stay off until the buffer accumulates >= k+1 snapshots"
        );
        // One more: now origin=0 has depth k >= k → eligible.
        state.commit_live();
        assert!(
            state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "gate must fire once the buffer reaches k+1 snapshots of stagnation"
        );
    }

    /// End-to-end: after the gate fires and no verifier frontier
    /// remains, `start_cycle` preempts the would-be Worker dispatch
    /// with `StuckMathAudit`. The progress history is marked dispatched
    /// so the same streak does not re-fire on subsequent cycles.
    #[test]
    fn theorem_stating_start_cycle_preempts_worker_with_stuck_math_audit() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_sound_blocker_state("a");
        // Pin Sound-Fail (current==approved) so the Sound blocker
        // remains in `global_blockers` but `current_sound_unknown` is
        // false → `sound_verify_nodes()` is empty and the would-be
        // dispatch is Worker (not Sound verifier).
        state
            .live
            .sound_current_fingerprints
            .insert(NodeId::from("a"), "sa-fail".into());
        state
            .sound_approved_fingerprints
            .insert(NodeId::from("a"), "sa-fail".into());
        state
            .sound_status
            .insert(NodeId::from("a"), SoundStatus::Fail);
        state.held_target = None;
        // Confirm setup: no verifier frontier in TheoremStating.
        assert!(state.paper_verify_targets().is_empty());
        assert!(state.substantiveness_verify_nodes().is_empty());
        assert!(state.corr_verify_nodes().is_empty());
        assert!(
            state.sound_verify_nodes().is_empty(),
            "Sound-Fail on a fingerprint-pinned node must not appear in sound_verify_nodes"
        );
        assert!(
            !state.current_sound_blocker_node_set().is_empty(),
            "Sound-Fail must still produce a Sound blocker"
        );
        // Roll history to k+1 same-set snapshots to satisfy the gate.
        for _ in 0..=k {
            state.commit_live();
        }
        assert!(state.stuck_math_audit_theorem_stating_no_sound_progress_trigger());
        state.stage = Stage::Start;
        state.in_flight_request = None;
        let dispatched_before = state.progress_history.last_dispatched_index;
        let commands = start_cycle(&mut state).expect("start_cycle must succeed");
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            ProtocolCommand::IssueRequest { request } => {
                assert_eq!(
                    request.kind,
                    RequestKind::StuckMathAudit,
                    "TheoremStating Worker dispatch must be preempted by StuckMathAudit"
                );
            }
            other => panic!("expected IssueRequest, got {other:?}"),
        }
        assert_eq!(state.stage, Stage::StuckMathAudit);
        // Dispatch must advance the dispatch marker so the same
        // stagnation streak does not re-fire on subsequent cycles.
        assert_ne!(
            state.progress_history.last_dispatched_index, dispatched_before,
            "dispatch must mark progress_history so the streak fires once"
        );
        assert_eq!(
            state.last_stuck_math_audit_dispatched_cycle,
            Some(state.cycle),
            "dispatch must record the cycle so the cooldown gates subsequent dispatches"
        );
        // Trigger is now ineligible because every buffered snapshot
        // sits at or before the dispatch marker.
        assert!(
            !state.stuck_math_audit_theorem_stating_no_sound_progress_trigger(),
            "post-dispatch debounce must drop the trigger until new snapshots accumulate"
        );
    }

    /// Regression: after the TheoremStating StuckMathAudit dispatches
    /// and `apply_stuck_math_audit_response` writes `state.audit_plan =
    /// Some(_)` (no cone_clean), the subsequent Review request emitted
    /// in the same transition must carry the audit plan. This guards
    /// against `request_audit_plan` accidentally regressing to a
    /// ProofFormalization-only phase gate (audit-2 finding B-1).
    #[test]
    fn theorem_stating_stuck_math_audit_response_delivers_audit_plan_to_reviewer() {
        let node = NodeId::from("a");
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = theorem_stating_sound_blocker_state("a");
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), "sa-fail".into());
        state
            .sound_approved_fingerprints
            .insert(node.clone(), "sa-fail".into());
        state.sound_status.insert(node.clone(), SoundStatus::Fail);
        state.held_target = None;
        // Roll history to k+1 snapshots so the no-Sound-progress gate
        // fires.
        for _ in 0..=k {
            state.commit_live();
        }
        state.stage = Stage::Start;
        state.in_flight_request = None;
        let dispatch = apply_event(state, ProtocolEvent::StartCycle)
            .expect("start_cycle must dispatch StuckMathAudit");
        let request = match &dispatch.commands[..] {
            [ProtocolCommand::IssueRequest { request }] => {
                assert_eq!(request.kind, RequestKind::StuckMathAudit);
                request.clone()
            }
            other => panic!("expected StuckMathAudit IssueRequest, got {other:?}"),
        };
        assert_eq!(dispatch.state.stage, Stage::StuckMathAudit);

        let report = "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS);
        let response = StuckMathAuditResponse {
            request_id: request.id,
            cycle: request.cycle,
            status: ResponseStatus::Ok,
            report: report.clone(),
            ..StuckMathAuditResponse::default()
        };
        let outcome = apply_event(
            dispatch.state,
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("StuckMathAuditResponse must transition to Reviewer with audit plan");

        let plan = outcome
            .state
            .audit_plan
            .as_ref()
            .expect("audit_plan must be written by apply_stuck_math_audit_response");
        assert_eq!(plan.report, report.trim());
        assert!(plan.cone_clean_node.is_none());
        assert!(!plan.need_input_audit);

        match outcome.commands.as_slice() {
            [ProtocolCommand::IssueRequest { request: review }] => {
                assert_eq!(review.kind, RequestKind::Review);
                assert_eq!(review.phase, Phase::TheoremStating);
                let plan_on_request = review.audit_plan.as_ref().expect(
                    "Review WrapperRequest must carry the just-written audit_plan in TheoremStating",
                );
                assert_eq!(plan_on_request, plan);
            }
            other => panic!("expected Review IssueRequest after audit response, got {other:?}"),
        }
        assert_eq!(outcome.state.stage, Stage::Reviewer);
    }

    /// Build a ProofFormalization state with two Sound carriers
    /// initially Unknown (one Sound-blocker each). All other lanes
    /// pinned Pass so the only blockers are Sound.
    fn proof_formalization_sound_blocker_state(nodes: &[&str]) -> ProtocolState {
        let mut state = theorem_stating_multi_node_state(nodes);
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Start;
        state
    }

    /// Companion test for the ProofFormalization variant: Sound
    /// carriers exist and are not all closed, and there has been no
    /// Sound progress over k snapshots → gate fires under
    /// `Phase::ProofFormalization` too, with a distinct reason string.
    #[test]
    fn no_sound_progress_gate_fires_in_proof_formalization() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = proof_formalization_sound_blocker_state(&["a", "b"]);
        // Cycle 0: both unsound.
        state.commit_live();
        // Cycles 1..=k: stagnate.
        for _ in 1..=k {
            state.commit_live();
        }
        assert!(
            !state.current_sound_blocker_node_set().is_empty(),
            "test setup: Sound carriers must still be open"
        );
        assert!(
            state.stuck_math_audit_proof_formalization_no_sound_progress_trigger(),
            "ProofFormalization variant must fire after >= k+1 stagnant snapshots with open Sound carriers"
        );
        state.refresh_stuck_math_audit_latch();
        assert!(
            state.stuck_math_audit.active,
            "latch must activate from the ProofFormalization no-Sound-progress trigger"
        );
        assert!(
            state
                .stuck_math_audit
                .trigger
                .starts_with("sound-stagnation-window:"),
            "trigger reason must use the sound-stagnation-window prefix, got {:?}",
            state.stuck_math_audit.trigger
        );
        assert!(
            state
                .stuck_math_audit
                .trigger
                .contains("proof-formalization"),
            "ProofFormalization variant must distinguish itself in the trigger reason: got {:?}",
            state.stuck_math_audit.trigger
        );
    }

    /// Companion check: when every Sound carrier IS closed in
    /// ProofFormalization, the variant trigger does not fire even if
    /// the buffer contains a long stagnation streak (the
    /// non-empty-blocker-set guard plays the role of "Sound nodes are
    /// not all closed").
    #[test]
    fn no_sound_progress_gate_off_when_all_sound_carriers_closed_proof_formalization() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = proof_formalization_sound_blocker_state(&["a", "b"]);
        // Stagnate first so the buffer fills, then close everything.
        for _ in 0..=k {
            state.commit_live();
        }
        mark_sound_pass(&mut state, "a", "sa-pass");
        mark_sound_pass(&mut state, "b", "sb-pass");
        state.commit_live();
        assert!(
            state.current_sound_blocker_node_set().is_empty(),
            "test setup: no Sound carrier may remain after both nodes are sound-pass"
        );
        assert!(
            !state.stuck_math_audit_proof_formalization_no_sound_progress_trigger(),
            "with all Sound carriers closed, the variant trigger must stay off"
        );
    }

    /// Regression: a reviewer-NeedInput escalation writes a sticky
    /// "reviewer requested NeedInput: ..." trigger string and attaches a
    /// `NeedInputAuditContext`. A subsequent `refresh_stuck_math_audit_latch`
    /// must NOT clobber that operator-facing reason just because the
    /// no-Sound-progress predicate happens to fire in the background —
    /// the reviewer's escalation is the salient trigger for the duration
    /// of the NeedInput context.
    #[test]
    fn refresh_latch_preserves_reviewer_need_input_trigger() {
        let k = stuck_math_audit_no_sound_progress_window();
        let mut state = proof_formalization_sound_blocker_state(&["a", "b"]);
        // Accumulate enough stagnant snapshots that the
        // no-Sound-progress predicate would otherwise fire on refresh.
        for _ in 0..=k {
            state.commit_live();
        }
        assert!(
            state.stuck_math_audit_proof_formalization_no_sound_progress_trigger(),
            "test premise: the background no-Sound-progress gate must be eligible"
        );

        // Simulate a reviewer-NeedInput escalation having landed first
        // (mirrors `route_need_input_to_auditor` in engine.rs).
        let reviewer_reason = "structural disagreement on lemma scope".to_string();
        let trigger = format!("reviewer requested NeedInput: {reviewer_reason}");
        state.stuck_math_audit.active = true;
        state.stuck_math_audit.trigger = trigger.clone();
        state.stuck_math_audit.active_since_cycle = state.cycle;
        state.stuck_math_audit.need_input_audit = Some(NeedInputAuditContext {
            phase: state.phase,
            reviewer_reason,
            ..NeedInputAuditContext::default()
        });

        state.refresh_stuck_math_audit_latch();

        assert!(
            state.stuck_math_audit.active,
            "latch must remain active across refresh"
        );
        assert!(
            state.stuck_math_audit.need_input_audit.is_some(),
            "NeedInput context must survive refresh"
        );
        assert!(
            state
                .stuck_math_audit
                .trigger
                .starts_with("reviewer requested NeedInput"),
            "reviewer-NeedInput trigger must remain sticky while need_input_audit is Some, got {:?}",
            state.stuck_math_audit.trigger
        );
        assert!(
            !state
                .stuck_math_audit
                .trigger
                .starts_with("sound-stagnation-window"),
            "background no-Sound-progress reason must not clobber the reviewer NeedInput trigger, got {:?}",
            state.stuck_math_audit.trigger
        );
    }

    // ---- Paper-target umbrella sync at PF→Cleanup (2026-05-29) ----------

    /// Verify `enter_cleanup_phase` emits a
    /// `SyncTabletRootForPaperTargets` carrying the resolved umbrella
    /// node set (paper-target covering nodes ∪ {Preamble}). The four
    /// production PF→Cleanup callers funnel through this helper, so
    /// covering the helper covers all four transition paths.
    #[test]
    fn enter_cleanup_phase_emits_paper_target_umbrella_sync() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.approved_targets.configured_targets =
            set::<TargetId>(&["t1", "t2"]);
        state.approved_targets.coverage.insert(
            TargetId::from("t1"),
            set::<NodeId>(&["Foo"]),
        );
        state.approved_targets.coverage.insert(
            TargetId::from("t2"),
            set::<NodeId>(&["Bar"]),
        );
        let commands = enter_cleanup_phase(&mut state);
        assert_eq!(state.phase, Phase::Cleanup);
        assert_eq!(state.stage, Stage::Start);
        assert_eq!(commands.len(), 1, "expected exactly one command");
        match &commands[0] {
            ProtocolCommand::SyncTabletRootForPaperTargets { node_names } => {
                assert_eq!(
                    *node_names,
                    set::<NodeId>(&["Bar", "Foo", "Preamble"]),
                    "umbrella must be {{Bar, Foo, Preamble}} (sorted)"
                );
            }
            other => panic!(
                "expected SyncTabletRootForPaperTargets, got {:?}",
                other
            ),
        }
    }

    /// Defensive baseline — if `approved_targets.coverage` is empty
    /// (shouldn't happen post-TheoremStating advance, but in defense)
    /// the helper still emits a sync command whose umbrella is
    /// `{Preamble}` only. The generated file content from this set is
    /// the legacy "-- Auto-generated …\nimport Tablet.Preamble\n",
    /// which matches the pre-change content — a no-op write rather
    /// than an error.
    #[test]
    fn enter_cleanup_phase_with_empty_coverage_emits_preamble_only_sync() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        // No approved_targets seeded.
        let commands = enter_cleanup_phase(&mut state);
        assert_eq!(state.phase, Phase::Cleanup);
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            ProtocolCommand::SyncTabletRootForPaperTargets { node_names } => {
                assert_eq!(*node_names, set::<NodeId>(&["Preamble"]));
            }
            other => panic!(
                "expected SyncTabletRootForPaperTargets, got {:?}",
                other
            ),
        }
    }

    /// Documents that `protected_closure_nodes` is NOT in the
    /// umbrella (the helper only walks `coverage`). The protection
    /// surface stays accessible via transitive Lean imports from the
    /// covering nodes; including it at umbrella level would
    /// reintroduce the bloat the change is designed to avoid.
    #[test]
    fn enter_cleanup_phase_umbrella_excludes_protected_closure_nodes() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Reviewer;
        state.approved_targets.configured_targets = set::<TargetId>(&["t1"]);
        state
            .approved_targets
            .coverage
            .insert(TargetId::from("t1"), set::<NodeId>(&["Cover"]));
        state.approved_targets.protected_closure_nodes =
            set::<NodeId>(&["ProtectedA", "ProtectedB"]);
        let commands = enter_cleanup_phase(&mut state);
        match &commands[0] {
            ProtocolCommand::SyncTabletRootForPaperTargets { node_names } => {
                assert_eq!(*node_names, set::<NodeId>(&["Cover", "Preamble"]));
                assert!(!node_names.contains(&NodeId::from("ProtectedA")));
                assert!(!node_names.contains(&NodeId::from("ProtectedB")));
            }
            other => panic!(
                "expected SyncTabletRootForPaperTargets, got {:?}",
                other
            ),
        }
    }
}
