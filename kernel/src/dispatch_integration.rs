//! Reconstruct a reserved task on current state and use ordinary acceptance.
//! No state diff from an old worker checkout is ever merged into canonical state.
use crate::dispatch::{DependencyGraph, EditScope, TaskId};
use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionOutcome};
use crate::model::*;
use std::collections::BTreeMap;

/// Small reviewer/retry context; large source snapshots stay in attempt
/// evidence, rather than being copied into every later scheduler event.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttemptReport {
    pub candidate_sha256: String,
    pub payload_sha256: String,
    pub summary: String,
    pub comments: String,
    pub rejection_reasons: Vec<String>,
    pub suggested_nodes: std::collections::BTreeSet<NodeId>,
    pub under_model_disproof: String,
    pub under_model_route_opinion: String,
    pub under_model_reasoning: String,
    pub audit_request: Option<PendingAuditRequest>,
}

impl AttemptReport {
    fn capture(state: &ProtocolState, payload: &WorkerPublication) -> Result<Self, String> {
        Ok(Self {
            candidate_sha256: payload.candidate_sha256.clone(), payload_sha256: payload.digest()?,
            summary: state.latest_worker_summary.clone(), comments: state.latest_worker_comments.clone(),
            rejection_reasons: state.deterministic_worker_rejection_reasons.clone(),
            suggested_nodes: state.latest_worker_needs_restructure_suggested_nodes.clone(),
            under_model_disproof: state.latest_worker_under_model_disproof.clone(),
            under_model_route_opinion: state.latest_worker_under_model_route_opinion.clone(),
            under_model_reasoning: state.latest_worker_under_model_reasoning.clone(),
            audit_request: state.pending_worker_audit_request.clone(),
        })
    }
    pub fn install(&self, state: &mut ProtocolState) {
        state.latest_worker_summary = self.summary.clone();
        state.latest_worker_comments = self.comments.clone();
        state.deterministic_worker_rejection_reasons = self.rejection_reasons.clone();
        state.latest_worker_needs_restructure_suggested_nodes = self.suggested_nodes.clone();
        state.latest_worker_under_model_disproof = self.under_model_disproof.clone();
        state.latest_worker_under_model_route_opinion = self.under_model_route_opinion.clone();
        state.latest_worker_under_model_reasoning = self.under_model_reasoning.clone();
        state.pending_worker_audit_request = self.audit_request.clone();
    }
}

/// Host-authenticated completed staged finalization on exact current inputs.
/// Never a worker carrier or a prefix-equivalence assertion.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshCheckedInputs {
    pub assignment_id: String,
    pub assignment_inputs_sha256: String,
    pub inputs_sha256: String,
    pub completion_sha256: String,
}

/// Runtime-authored checked input, bound to the exact canonical state used
/// for acceptance. It carries no arbitrary replacement ProtocolState.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPublication {
    /// Host-selected delivery policy. Absent preserves historical retry replay.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closure_stuck_to_review: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_checked_inputs: Option<FreshCheckedInputs>,
    /// Raw submitted outcome, retained separately from the host's verdict.
    /// A worker-declared terminal failure must not become a correction loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_outcome: Option<WorkerOutcome>,
    pub task_id: TaskId,
    pub candidate_sha256: String,
    pub base_state_sha256: String,
    /// Fixed at the durable publication decision, not recomputed on recovery.
    pub decided_at_ms: u64,
    pub inputs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proof_body_rebase_inputs: BTreeMap<String, String>,
    pub actual_scope: EditScope,
    pub graph_before: DependencyGraph,
    pub graph_after: DependencyGraph,
    pub response: WorkerResponse,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReceipt {
    pub candidate_sha256: String,
    pub payload_sha256: String,
    pub event_id: u64,
}

pub fn state_digest(state: &ProtocolState) -> Result<String, String> {
    // Runtime prompt enrichment depends on files and is refreshed on load.
    // Bind the complete semantic state and request identity, using the same
    // pure request derivation on either side of the source-write boundary.
    let mut normalized = state.clone();
    normalized.ensure_node_metadata();
    normalized.in_flight_request = state.in_flight_request.as_ref()
        .map(|r| Box::new(normalized.expected_request(r.id, r.kind)));
    Ok(crate::trusted_artifact_rebind::digest(&serde_json::to_value(normalized).map_err(|e| e.to_string())?))
}

impl WorkerPublication {
    pub fn digest(&self) -> Result<String, String> {
        Ok(crate::trusted_artifact_rebind::digest(&serde_json::to_value(self).map_err(|e| e.to_string())?))
    }

    pub fn committed(&self, state: &ProtocolState) -> Result<Option<PublicationReceipt>, String> {
        let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
        match ledger.publications.get(&self.task_id) {
            Some(receipt) if receipt.candidate_sha256 == self.candidate_sha256
                && receipt.payload_sha256 == self.digest()? => Ok(Some(receipt.clone())),
            Some(_) => Err("task already committed a different publication".into()),
            None => Ok(None),
        }
    }
}

/// Closure work has a private focus, which must not become the canonical
/// reviewer's focus when the isolated task returns or fails.
fn restore_closure_reviewer_focus(canonical: &ProtocolState, state: &mut ProtocolState, task_id: TaskId) {
    if !(canonical.coarse_goals.is_some() && canonical.phase == Phase::ProofFormalization) && canonical.dispatch.as_ref().and_then(|ledger| ledger.tasks.get(&task_id))
        .is_none_or(|task| task.assignment.task_kind != crate::dispatch::WorkerTaskKind::LeanClosure)
    { return; }
    state.active_node = canonical.active_node.clone();
    state.coarse_goals = canonical.coarse_goals.clone();
    state.coarse_task_goal = None;
    state.active_coarse_node = canonical.active_coarse_node.clone();
    state.audit_plan = canonical.audit_plan.clone();
    state.superseded_audit_plan = canonical.superseded_audit_plan.clone();
    state.stuck_math_audit = canonical.stuck_math_audit.clone();
    state.held_target = canonical.held_target.clone();
    state.target_edit_mode = canonical.target_edit_mode;
    state.proof_edit_mode = canonical.proof_edit_mode;
    state.cycles_in_coarse_repair_mode = canonical.cycles_in_coarse_repair_mode;
    // A successful closure may have closed the old active node. Apply the
    // ordinary current-source legality rules after removing the exemption.
    state.relegalize_active_fields();
}

/// Worker acceptance reinstalls its task-local reviewer comments. Carry the
/// matching original assignment's advisory provenance with that text, rather
/// than leaving a later review's origin attached to an older task's comments.
/// This changes no comments, permissions, or accepted evidence.
fn restore_worker_handoff(state: &mut ProtocolState, task_id: TaskId) {
    let ledger = state.dispatch.as_mut().unwrap();
    ledger.worker_handoff = ledger.tasks.get(&task_id).and_then(|task| {
        task.assignment.review.as_ref().filter(|review|
            review.request_id == task.review_id
                && review.comments.trim() == state.reviewer_comments)
            .map(|review| crate::dispatch::ReviewerHandoff::new(review.request_id, &state.reviewer_comments))
    }).or_else(|| ledger.worker_handoff.clone().filter(|origin| origin.matches(&state.reviewer_comments)));
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    use crate::dispatch::{CompilerCapability, DispatchConfig, DispatchLedger, PreparedAssignment,
        ReviewerHandoff, WorkerTaskKind};

    #[test]
    fn worker_handoff_requires_exact_original_assignment_and_preserves_other_state() {
        let comments = "Read .trellis/stuck-math-audit/original/λ.lean";
        let graph = DependencyGraph { edges: [("a".into(), Default::default())].into() };
        let mut ledger = DispatchLedger::new(DispatchConfig::default(), Phase::ProofFormalization).unwrap();
        let assignment = PreparedAssignment {
            assignment_id: "original".into(), profile: "sol-xhigh".into(), timeout_seconds: None,
            compiler: CompilerCapability::Managed, task_kind: WorkerTaskKind::Ordinary,
            scope: EditScope { nodes: ["a".into()].into(), ..Default::default() },
            base_snapshot: "base".into(), input_hashes: [("Tablet/a.lean".into(), "1".repeat(64))].into(),
            proof_body_rebase_inputs: Default::default(),
            request: std::sync::Arc::new(WrapperRequest { phase: Phase::ProofFormalization,
                kind: RequestKind::Worker, ..Default::default() }).into(), authorized_worker: None,
            review: Some(Box::new(ReviewResponse { request_id: 7,
                comments: format!("  {comments}\n"), ..Default::default() })),
        };
        let id = ledger.reserve(7, assignment, &graph, 1).unwrap();
        ledger.continuations.push_back(Box::new(crate::dispatch_routing::Continuation::Review {
            review_id: 9, phase: Phase::ProofFormalization,
            response: Box::new(ReviewResponse { request_id: 9, comments: "queued later guidance".into(), ..Default::default() }),
        }));
        let initial = ProtocolState { dispatch: Some(ledger), reviewer_comments: comments.into(), ..Default::default() };
        for case in ["original", "retry", "no review", "wrong id", "wrong comments", "missing task"] {
            for matching_prior in [false, true] {
                let mut state = initial.clone();
                let ledger = state.dispatch.as_mut().unwrap();
                ledger.worker_handoff = Some(ReviewerHandoff::new(99,
                    if matching_prior { comments } else { "unrelated current review" }));
                let task = ledger.tasks.get_mut(&id).unwrap();
                match case {
                    "retry" => task.retry_of = Some(999),
                    "no review" => task.assignment.review = None,
                    "wrong id" => task.assignment.review.as_mut().unwrap().request_id = 8,
                    "wrong comments" => task.assignment.review.as_mut().unwrap().comments.push('!'),
                    _ => (),
                }
                let expected_origin = if matches!(case, "original" | "retry") {
                    Some(ReviewerHandoff::new(7, comments))
                } else if matching_prior { Some(ReviewerHandoff::new(99, comments)) } else { None };
                let mut expected = state.clone();
                expected.dispatch.as_mut().unwrap().worker_handoff = expected_origin;
                restore_worker_handoff(&mut state, if case == "missing task" { 999 } else { id });
                assert_eq!(state, expected, "{case}, matching_prior={matching_prior}");
            }
        }
    }
}

/// Reuse ordinary acceptance on a fresh canonical context, then preserve the
/// independent scheduling state. Failed candidates never enter this path and
/// cannot restore or replace the shared worktree.
pub fn publish(canonical: &ProtocolState, payload: &WorkerPublication) -> Result<TransitionOutcome, String> {
    if payload.closure_stuck_to_review {
        return Err("closure Stuck disposition cannot publish proof progress".into());
    }
    if payload.committed(canonical)?.is_some() {
        return Ok(TransitionOutcome { state: canonical.clone(), commands: vec![] });
    }
    if payload.base_state_sha256 != state_digest(canonical)? {
        return Err("canonical state changed after candidate acceptance; rebase and check again".into());
    }
    if payload.response.status != ResponseStatus::Ok
        || payload.response.outcome != WorkerOutcome::Valid
        || !payload.response.acceptance_check_passed
    {
        return Err("source publication requires an authoritative accepted Valid result".into());
    }
    let mut result = check_transition_with_fresh_inputs(canonical, payload.task_id, &payload.candidate_sha256,
        payload.decided_at_ms, &payload.inputs, &payload.proof_body_rebase_inputs, &payload.actual_scope, &payload.graph_before,
        &payload.graph_after, payload.response.clone(), payload.fresh_checked_inputs.as_ref())?;
    if result.state.stage != Stage::Reviewer || result.state.phase != canonical.phase
        || result.commands.iter().any(|c| !matches!(c, ProtocolCommand::DeleteLocalClosureRecord { .. }))
    {
        return Err("parallel worker acceptance attempted an unscheduled global operation".into());
    }
    let mut ledger = canonical.dispatch.clone().ok_or("parallel dispatch is not enabled")?;
    let receipt = PublicationReceipt {
        candidate_sha256: payload.candidate_sha256.clone(), payload_sha256: payload.digest()?,
        event_id: ledger.event_seq.checked_add(1).ok_or("scheduler event counter exhausted")?,
    };
    ledger.stop(payload.task_id, crate::dispatch::AttemptOutcome::Accepted { event_id: receipt.event_id })?;
    ledger.tasks.get_mut(&payload.task_id).unwrap().report = Some(AttemptReport::capture(&result.state, payload)?);
    ledger.event_seq = receipt.event_id;
    ledger.publications.insert(payload.task_id, receipt);
    use crate::dispatch_routing::Continuation;
    let cleanup = matches!(ledger.tasks[&payload.task_id].assignment.request.inline()?.worker_context.validation_kind,
        WorkerValidationKind::Cleanup | WorkerValidationKind::FinalCleanup);
    // Ordinary progress covers the current combined snapshot. Coalesce worker
    // completions, but never discard a separately pending reviewer action.
    let has_delta = payload.response.snapshot != canonical.live
        || ledger.continuations.iter().any(|c| matches!(c.as_ref(), Continuation::Worker { has_delta: true, .. }));
    ledger.continuations.retain(|c| matches!(c.as_ref(), Continuation::Review { .. } | Continuation::Request { .. }));
    ledger.continuations.push_front(Box::new(if cleanup { Continuation::CleanupWorker } else {
        Continuation::Worker { response: Box::new(payload.response.clone()), has_delta }
    }));
    ledger.routing_pending = true;
    result.state.dispatch = Some(ledger);
    result.state.worker_execution = None;
    result.state.worker_task_kind = crate::dispatch::WorkerTaskKind::Ordinary;
    restore_closure_reviewer_focus(canonical, &mut result.state, payload.task_id);
    restore_worker_handoff(&mut result.state, payload.task_id);
    result.state.record_coarse_attempt(canonical, payload.task_id)?;
    // The reviewer is still reasoning against its immutable snapshot. Keep
    // its identity, with current derived fields, for assignment revalidation.
    result.state.request_seq = result.state.request_seq.max(canonical.request_seq);
    result.state.in_flight_request = None;
    result.state.commit_live();
    result.state.in_flight_request = canonical.in_flight_request.as_ref()
        .map(|request| Box::new(result.state.expected_request(request.id, request.kind)));
    result.state.validate()?;
    result.commands.push(ProtocolCommand::CommitCheckpoint);
    Ok(result)
}

/// Apply the ordinary rejection/retry policy to a fresh private context.
/// Only that derived transition is installed, never the failed source tree.
pub fn fail(canonical: &ProtocolState, payload: &WorkerPublication) -> Result<TransitionOutcome, String> {
    if payload.fresh_checked_inputs.is_some() { return Err("fresh checked inputs require positive staged publication".into()); }
    use crate::dispatch::{AttemptOutcome, RetryContext, TaskStatus};
    let ledger = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    let task = ledger.tasks.get(&payload.task_id).ok_or("unknown worker task")?;
    if task.worker_attempt.as_ref().is_some_and(|attempt| matches!(&attempt.phase,
        crate::worker_attempt::Phase::Ready { feedback_sha256: Some(hash) } if Some(hash) == payload.digest().ok().as_ref())) {
        return Ok(TransitionOutcome { state: canonical.clone(), commands: vec![] });
    }
    if matches!(task.status, TaskStatus::Stopping { .. } | TaskStatus::Resolved { .. }) {
        return if task.report.as_ref().is_some_and(|r| r.payload_sha256 == payload.digest().unwrap_or_default()) {
            Ok(TransitionOutcome { state: canonical.clone(), commands: vec![] })
        } else { Err("attempt already has a different terminal outcome".into()) };
    }
    if payload.base_state_sha256 != state_digest(canonical)? {
        return Err("canonical state changed before recording worker failure".into());
    }
    if payload.response.status == ResponseStatus::Ok && payload.response.outcome == WorkerOutcome::Valid {
        return Err("a Valid worker must use authoritative source publication".into());
    }
    if payload.closure_stuck_to_review
        && (task.assignment.task_kind != crate::dispatch::WorkerTaskKind::LeanClosure
            || payload.response.status != ResponseStatus::Ok || payload.response.outcome != WorkerOutcome::Stuck
            || payload.response.transport_failure || payload.response.preflight_error.is_some()
            || payload.response.acceptance_check_passed
            || payload.submitted_outcome != Some(WorkerOutcome::Stuck)) {
        return Err("direct reviewer delivery requires an honest closure Stuck report".into());
    }
    let mut result = check_transition(canonical, payload.task_id, &payload.candidate_sha256,
        payload.decided_at_ms, &payload.inputs, &payload.proof_body_rebase_inputs, &task.assignment.scope,
        &payload.graph_before, &payload.graph_before, payload.response.clone())?;
    for (name, changed) in [
        ("live source snapshot", result.state.live != canonical.live),
        ("dependencies", result.state.deps != canonical.deps),
        ("proof nodes", result.state.proof_nodes != canonical.proof_nodes),
        ("node kinds", result.state.node_kinds != canonical.node_kinds),
        ("target claims", result.state.target_claims != canonical.target_claims),
        ("local closure records", result.state.local_closure_records != canonical.local_closure_records),
    ] {
        if changed { return Err(format!("failed worker attempted to change shared {name}")); }
    }
    if result.state.phase != canonical.phase
        || !matches!(result.state.stage, Stage::Worker | Stage::Reviewer)
        || result.commands.iter().any(|c| match c {
            ProtocolCommand::RestoreWorktreeToActiveWorkerBase | ProtocolCommand::WriteHaltSentinel { .. } => false,
            ProtocolCommand::IssueRequest { request } => request.kind != RequestKind::Worker,
            _ => true,
        })
    { return Err("failed worker attempted to change shared source state or global routing".into()); }
    // A checked delivery/validation failure can return to the same conversation.
    // Do not commit the speculative ordinary failure transition: it would spend
    // another attempt, restore source, and release the original reservation.
    // Explicit Stuck/NeedsRestructure and infrastructure failures retain their
    // ordinary terminal dispositions. A halt always wins over correction.
    if task.worker_attempt.as_ref().is_some_and(|attempt| attempt.can_correct())
        && !payload.response.transport_failure && payload.response.preflight_error.is_none()
        && (payload.response.outcome == WorkerOutcome::Invalid || payload.response.status == ResponseStatus::Malformed)
        && (payload.submitted_outcome == Some(WorkerOutcome::Valid)
            || payload.submitted_outcome.is_none() && payload.response.status == ResponseStatus::Malformed)
        && !result.commands.iter().any(|c| matches!(c, ProtocolCommand::WriteHaltSentinel { .. }))
    {
        let mut state = canonical.clone();
        let ledger = state.dispatch.as_mut().unwrap();
        ledger.correct(payload.task_id, payload.candidate_sha256.clone(), payload.digest()?)?;
        ledger.event_seq = ledger.event_seq.checked_add(1).ok_or("scheduler event counter exhausted")?;
        ledger.validate()?;
        return Ok(TransitionOutcome { state, commands: vec![] });
    }
    let retry = (result.state.stage == Stage::Worker && !payload.closure_stuck_to_review)
        .then(|| RetryContext::capture(&result.state));
    let report = AttemptReport::capture(&result.state, payload)?;
    let reason = if report.rejection_reasons.is_empty() { report.comments.clone() }
        else { report.rejection_reasons.join("; ") };
    let outcome = match payload.response.outcome {
        WorkerOutcome::Stuck => AttemptOutcome::Stuck { reason },
        WorkerOutcome::NeedsRestructure => AttemptOutcome::NeedsRestructure { reason },
        WorkerOutcome::TargetFalseUnderModel => AttemptOutcome::TargetFalseUnderModel { reason },
        _ => AttemptOutcome::Rejected { reason },
    };
    let mut ledger = ledger.clone();
    ledger.stop(payload.task_id, outcome)?;
    let task = ledger.tasks.get_mut(&payload.task_id).unwrap();
    task.pending_retry = retry;
    task.report = Some(report);
    ledger.routing_pending = true;
    if ledger.continuations.is_empty() {
        ledger.continuations.push_back(Box::new(crate::dispatch_routing::Continuation::Progress));
    }
    result.state.dispatch = Some(ledger);
    result.state.worker_execution = None;
    result.state.worker_task_kind = crate::dispatch::WorkerTaskKind::Ordinary;
    restore_closure_reviewer_focus(canonical, &mut result.state, payload.task_id);
    restore_worker_handoff(&mut result.state, payload.task_id);
    result.state.record_coarse_attempt(canonical, payload.task_id)?;
    result.state.clear_pending_task();
    result.state.stage = Stage::Reviewer;
    result.state.in_flight_request = canonical.in_flight_request.as_ref()
        .map(|r| Box::new(result.state.expected_request(r.id, r.kind)));
    if result.state.dispatch.as_ref().unwrap().tasks[&payload.task_id].pending_retry.is_some()
        && result.state.dispatch.as_ref().unwrap().exclusive_workers.contains_key(&payload.task_id)
    {
        let mut seed = result.state.clone();
        seed.dispatch = None;
        seed.stage = Stage::Worker;
        let task = &result.state.dispatch.as_ref().unwrap().tasks[&payload.task_id];
        seed.pending_task = canonical.dispatch.as_ref().unwrap().exclusive_workers[&payload.task_id].state.pending_task.clone();
        task.pending_retry.as_ref().unwrap().install(&mut seed);
        seed.worker_execution = task.assignment.request.inline()?.task_execution.clone();
        seed.in_flight_request = Some(Box::new(seed.expected_request(seed.request_seq, RequestKind::Worker)));
        seed.validate()?;
        let canonical_sha256 = crate::dispatch_routing::exclusive_digest(&result.state)?;
        result.state.dispatch.as_mut().unwrap().exclusive_workers.insert(payload.task_id,
            crate::dispatch_routing::ExclusiveWorker { state: Box::new(seed), canonical_sha256 });
    }
    result.state.validate()?;
    result.commands.retain(|c| matches!(c, ProtocolCommand::WriteHaltSentinel { .. }));
    Ok(result)
}

/// Fresh private input for the ordinary authoritative worker checker. The
/// caller has compared exact source inputs and prepared an isolated rebase.
/// This function rechecks current routing and permissions as well: source
/// freshness alone cannot preserve a task invalidated by a process decision.
pub fn acceptance_state(
    canonical: &ProtocolState, task_id: TaskId, graph: &DependencyGraph,
) -> Result<ProtocolState, String> {
    let ledger = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    let task = ledger.tasks.get(&task_id).ok_or("unknown worker task")?;
    if let Some(seed) = ledger.exclusive_workers.get(&task_id) {
        if seed.canonical_sha256 != crate::dispatch_routing::exclusive_digest(canonical)? {
            return Err("exclusive worker's canonical process state changed; retain its candidate for review".into());
        }
        let mut private = *seed.state.clone();
        task.retry.install(&mut private);
        if let Some(report) = &task.report { report.install(&mut private); }
        private.worker_execution = task.assignment.request.inline()?.task_execution.clone();
        let request = private.in_flight_request.as_ref().ok_or("exclusive worker has no request")?;
        private.in_flight_request = Some(Box::new(private.expected_request(request.id, RequestKind::Worker)));
        private.validate()?;
        return Ok(private);
    }
    if canonical.phase != ledger.phase || canonical.stage != Stage::Reviewer {
        return Err("task acceptance requires its original phase and the parallel reviewer boundary".into());
    }
    let assignment = &task.assignment;
    assignment.scope.validate(graph)?;
    if !assignment.scope.nodes.is_subset(&canonical.live.present_nodes) {
        return Err("authorized worker scope no longer exists".into());
    }
    let mut private = canonical.clone_without_dispatch();
    private.in_flight_request = None;
    assignment.authorized_worker.as_ref().ok_or("task lacks its ordinary authorization context")?
        .install(&mut private)?;
    // The held theorem target is reviewer scheduling context, not this
    // worker's edit scope. Independent publications can reopen statement
    // verification while a reserved worker is running. Suspend its old held
    // target on the current surface, just as relegalize_active_fields does;
    // retain the active node, pending task and all contract comparisons below.
    if private.phase.is_theorem_stating_like() && private.corr_blockers_exist() {
        private.held_target = None;
    }
    private.stage = Stage::Worker;
    private.cycle = private.cycle.checked_add(1).ok_or("cycle counter exhausted")?;
    private.request_seq = private.request_seq.checked_add(1).ok_or("request counter exhausted")?;
    private.attempt = 1;
    private.gate_kind = GateKind::None;
    private.gate_from_invalid_attempt = false;
    private.worker_task_kind = assignment.task_kind;
    if canonical.coarse_goals.is_some() && assignment.task_kind.is_ordinary()
        && canonical.phase == Phase::ProofFormalization {
        let goal = assignment.request.inline()?.active_coarse_node.clone()
            .ok_or("ordinary task has no coarse-goal binding")?;
        if !canonical.coarse_goals.as_ref().unwrap().active().contains(&goal) {
            return Err("task's coarse goal is no longer active".into());
        }
        private.project_coarse_goal(&goal)?;
    }
    task.retry.install(&mut private);
    if let Some(report) = &task.report { report.install(&mut private); }
    private.worker_execution = assignment.request.inline()?.task_execution.clone();
    let request = private.expected_request(private.request_seq, RequestKind::Worker);
    if request.worker_context != assignment.request.inline()?.worker_context
        || request.active_node != assignment.request.inline()?.active_node
        || request.work_kind != assignment.request.inline()?.work_kind
        || request.assumption_authoring != assignment.request.inline()?.assumption_authoring
    {
        return Err("current process state changed this task's authorized contract; retain its candidate for review".into());
    }
    private.in_flight_request = Some(Box::new(request));
    private.validate()?;
    Ok(private)
}

/// Pure replay of the same ordinary worker transition used by serial runs.
/// The output remains private until the runtime has reconciled source-backed
/// closure records and committed a recoverable publication. In particular,
/// this function does not treat a scheduler outcome as mathematical evidence.
pub fn check_transition(
    canonical: &ProtocolState, task_id: TaskId, candidate: &str, now_ms: u64,
    inputs: &BTreeMap<String, String>, proof_body_rebase_inputs: &BTreeMap<String, String>, actual_scope: &EditScope,
    graph_before: &DependencyGraph, graph_after: &DependencyGraph,
    response: WorkerResponse,
) -> Result<TransitionOutcome, String> {
    check_transition_with_fresh_inputs(canonical, task_id, candidate, now_ms, inputs,
        proof_body_rebase_inputs, actual_scope, graph_before, graph_after, response, None)
}

fn check_transition_with_fresh_inputs(
    canonical: &ProtocolState, task_id: TaskId, candidate: &str, now_ms: u64,
    inputs: &BTreeMap<String, String>, proof_body_rebase_inputs: &BTreeMap<String, String>, actual_scope: &EditScope,
    graph_before: &DependencyGraph, graph_after: &DependencyGraph, response: WorkerResponse,
    fresh: Option<&FreshCheckedInputs>,
) -> Result<TransitionOutcome, String> {
    let ledger = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    ledger.require_live_candidate(task_id, candidate, now_ms)?;
    let fresh_readonly = if let Some(basis) = fresh {
        let task = ledger.tasks.get(&task_id).ok_or("unknown task")?;
        let digest = |value: &BTreeMap<String, String>| crate::trusted_artifact_rebind::digest(
            &serde_json::to_value(value).expect("string map"));
        let request = task.assignment.request.inline()?;
        if canonical.phase != Phase::ProofFormalization || request.is_pv
            || task.assignment.compiler != crate::dispatch::CompilerCapability::Managed
            || task.assignment.scope.exclusive || !task.assignment.scope.shared_writes.is_empty()
            || actual_scope.exclusive || !actual_scope.shared_writes.is_empty()
            || !matches!(request.worker_acceptance.validation_execution_plan.as_slice(),
                [WorkerValidationExecutionPlanStep::ProofWorkerDelta { .. }])
            || response.status != ResponseStatus::Ok || response.outcome != WorkerOutcome::Valid
            || !response.acceptance_check_passed || response.transport_failure || response.preflight_error.is_some()
            || basis.assignment_id != task.assignment.assignment_id
            || basis.assignment_inputs_sha256 != digest(&task.assignment.input_hashes)
            || basis.inputs_sha256 != digest(inputs)
            || basis.completion_sha256.len() != 64
            || !basis.completion_sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("fresh checked input basis does not match this accepted task".into());
        }
        true
    } else { false };
    ledger.check_candidate_inputs_with_fresh_readonly(task_id, inputs, proof_body_rebase_inputs,
        actual_scope, graph_after, fresh_readonly)?;
    let context = acceptance_state(canonical, task_id, graph_before)?;
    let checker_accepted = response.acceptance_check_passed && response.status == ResponseStatus::Ok
        && response.outcome == WorkerOutcome::Valid;
    let result = apply_event(context, ProtocolEvent::WrapperResponse { response: WrapperResponse::Worker(response) })
        .map_err(|e| format!("ordinary worker transition refused candidate: {e:?}"))?;
    if checker_accepted && result.commands.iter().any(|c| matches!(c, ProtocolCommand::RestoreWorktreeToActiveWorkerBase)) {
        return Err(format!("authoritative checker/engine disagreement: {}",
            result.state.deterministic_worker_rejection_reasons.join("; ")));
    }
    Ok(result)
}
