//! Ordinary Trellis routing with isolated verifier lifetimes. Scheduling never
//! supplies a verdict: results pass through the existing wrapper transition.
use crate::dispatch::{CompilerCapability, ProcessIdentity};
use crate::dispatch_verifiers::{VerifierIdentity, VerifierLane, VerifierStatus};
use crate::engine::{apply_event, continue_parallel_dispatch, ProtocolCommand, ProtocolEvent, TransitionOutcome};
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const ROUTING_DRAIN: &str = "ordinary Trellis routing requires an exclusive checkpoint";

/// An ordinary, exclusively routed worker (for example cleanup). Its private
/// seed is produced by the ordinary engine, never supplied by an agent. The
/// canonical semantic state must remain identical until acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusiveWorker {
    pub state: Box<ProtocolState>,
    pub canonical_sha256: String,
}

pub fn exclusive_digest(state: &ProtocolState) -> Result<String, String> {
    let mut value = state.clone();
    value.dispatch = None;
    value.in_flight_request = None;
    crate::dispatch_integration::state_digest(&value)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "continuation", rename_all = "snake_case")]
pub enum Continuation {
    Progress,
    CleanupWorker,
    Worker { response: Box<WorkerResponse>, has_delta: bool },
    Request { kind: RequestKind },
    Review { review_id: u32, phase: Phase, response: Box<ReviewResponse> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutingEvent {
    ExclusiveStep { inputs: BTreeMap<String, String>, expected_inputs: BTreeMap<String, String>, response: Option<Box<WrapperResponse>> },
    SupersedeQueued { id: u64, reason: String },
    AdoptWorker { graph: crate::dispatch::DependencyGraph, inputs: BTreeMap<String, String>, base_snapshot: String, now_ms: u64 },
    ReviewAction { review_id: u32, response_sha256: String, response: Box<ReviewResponse> },
    Route {
        inputs: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "crate::model::is_zero_u32")]
        verifier_context_version: u32,
        /// New routing calls may avoid notifying the reviewer about its own
        /// accepted Sound-only action. Absent on historical journal events.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        suppress_sound_review_self_wakeup: bool,
    },
    VerifierLaunched { id: u64, process: ProcessIdentity, now_ms: u64 },
    VerifierCandidate { id: u64, response: Box<WrapperResponse> },
    /// Host has reaped the complete panel and drained its compiler channels.
    VerifierFinished { id: u64, inputs: BTreeMap<String, String>, response: Box<WrapperResponse> },
}

pub(crate) fn stage_for(kind: RequestKind) -> Result<Stage, String> {
    match kind {
        RequestKind::Review => Ok(Stage::Reviewer),
        RequestKind::Paper => Ok(Stage::VerifyPaper),
        RequestKind::Corr => Ok(Stage::VerifyCorr),
        RequestKind::Sound => Ok(Stage::VerifySound),
        _ => Err("request requires exclusive ordinary routing".into()),
    }
}

fn identity(request: &WrapperRequest, inputs: &BTreeMap<String, String>) -> Result<VerifierIdentity, String> {
    if request.scoped_audit.is_some() { return crate::coarse_audits::identity(request, inputs); }
    if inputs.is_empty() { return Err("verifier requires a complete immutable input inventory".into()); }
    let lane = match request.kind {
        RequestKind::Paper if !request.substantiveness_verify_nodes.is_empty() => VerifierLane::Substantiveness,
        RequestKind::Paper => VerifierLane::Paper,
        RequestKind::Corr => VerifierLane::Correspondence,
        RequestKind::Sound => VerifierLane::Sound,
        _ => return Err("not an independent verifier request".into()),
    };
    // Bind every field the panel receives, including membership, policy,
    // fingerprints, consumer evidence and scenario. Rebase scheduling identity;
    // v4 Sound also separates checker-only build provenance below. This never
    // infers a narrower evidence read set.
    let mut value = serde_json::to_value(request).map_err(|e| e.to_string())?;
    value.as_object_mut().unwrap().remove("id");
    value.as_object_mut().unwrap().remove("cycle");
    if matches!(request.verifier_context_version, 4 | 5) && request.kind == RequestKind::Sound {
        if !request.verifier_logic_identity.starts_with("trellis-sound-verifier-v1:sha256:") {
            return Err("Sound verifier requires its implementation identity".into());
        }
        // The full build identity stays in the immutable request as provenance.
        // The separate Sound identity binds judgment dependencies; excluded
        // checker code still serves launch/cache/cleanup, not unbound successful
        // panel evidence (see model::sound_verifier_logic_identity). Never apply
        // this projection to legacy requests or other verifier lanes.
        value.as_object_mut().unwrap().remove("acceptance_logic_identity");
    }
    let panel_sha256 = crate::trusted_artifact_rebind::digest(&value);
    Ok(VerifierIdentity { lane, panel_sha256, input_hashes: inputs.clone(),
        compiler: if lane.permits_compilation() { CompilerCapability::Managed } else { CompilerCapability::None } })
}

/// A frozen future Sound target retains its admission origin, while current
/// predicates decide whether this evidence is still applicable. Loss of
/// eligibility is normal stale work, distinct from malformed native bindings.
fn sound_target_current(canonical: &ProtocolState, request: &WrapperRequest) -> Result<bool, String> {
    if request.kind != RequestKind::Sound || request.verifier_context_version != 5 { return Ok(true); }
    let binding = request.independent_sound_target.as_ref().ok_or("independent Sound target missing")?;
    if request.sound_verify_node.as_ref() != Some(&binding.node)
        || request.sound_verify_nodes != BTreeSet::from([binding.node.clone()]) {
        return Err("independent Sound target differs from frozen panel".into());
    }
    Ok(canonical.independent_sound_eligible(binding))
}

pub fn verifier_state(canonical: &ProtocolState, id: u64) -> Result<ProtocolState, String> {
    let job = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?
        .verifiers.jobs.get(&id).ok_or("unknown verifier job")?;
    let request = job.request.as_ref().ok_or("verifier job has no ordinary request")?;
    let mut private = canonical.clone_without_dispatch();
    if request.kind == RequestKind::Sound && request.verifier_context_version == 5 {
        if !sound_target_current(canonical, request)? { return Err("independent Sound target is no longer eligible".into()); }
        private.independent_sound_target = request.independent_sound_target.clone();
    }
    if let Some(scope) = &request.scoped_audit {
        let mut current = scope.clone();
        current.nodes.extend(crate::coarse_audits::evidence_nodes(canonical, &scope.goals)?);
        private.scoped_audit = Some(current);
        private.stuck_math_audit = StuckMathAuditState { active: true, trigger: scope.reason.clone(), ..Default::default() };
        private.stage = Stage::StuckMathAudit;
    } else { private.stage = stage_for(request.kind)?; }
    private.in_flight_request = Some(Box::new(private.expected_request_with_verifier_context_version(
        request.id, request.kind, request.verifier_context_version)));
    private.validate()?;
    Ok(private)
}

/// Revalidate queued work with the same complete identity used at completion.
/// A saved workspace is not evidence that its panel policy is still current.
pub fn prepare_verifier(
    canonical: &ProtocolState, id: u64, inputs: &BTreeMap<String, String>,
) -> Result<Option<ProtocolState>, String> {
    let job = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?
        .verifiers.jobs.get(&id).ok_or("unknown verifier job")?;
    if job.status != VerifierStatus::Queued {
        return Err("verifier preparation requires a queued job".into());
    }
    if !sound_target_current(canonical, job.request.as_ref().ok_or("verifier job has no ordinary request")?)? {
        return Ok(None);
    }
    let private = verifier_state(canonical, id)?;
    let current = identity(private.in_flight_request.as_ref().unwrap(), inputs)?;
    Ok((current == job.identity).then_some(private))
}

/// Exclusive workers retain their ordinary process snapshot through cleanup
/// and automatic retry admission. Even an independent verifier may change
/// that snapshot, so leave ordinary routing parked until the work resolves.
pub fn exclusive_work_pending(ledger: &crate::dispatch::DispatchLedger) -> bool {
    ledger.tasks.values().any(|task| task.assignment.scope.exclusive
        && (task.status.occupies_slot() || task.pending_retry.is_some()))
}

/// A global transition cannot race a live reviewer or any attempt's cleanup.
/// The current verifier has already been reaped before Finished is submitted.
fn exclusive_ready(state: &ProtocolState, finishing: Option<u64>) -> bool {
    let ledger = state.dispatch.as_ref().unwrap();
    !ledger.tasks.values().any(|task| task.status.occupies_slot()) && ledger.reviewer.is_none()
        && ledger.verifiers.jobs.iter().all(|(id, j)| Some(*id) == finishing
            || matches!(j.status, VerifierStatus::Queued | VerifierStatus::Resolved { .. }))
}

/// Human input still enters through the existing operator boundary. It is
/// never manufactured by the parallel coordinator or a reviewer assignment.
pub fn human_decision(canonical: &ProtocolState, response: HumanGateResponse) -> Result<TransitionOutcome, String> {
    if canonical.stage != Stage::HumanGate || !exclusive_ready(canonical, None) {
        return Err("human decision requires the drained ordinary HumanGate boundary".into());
    }
    let mut private = canonical.clone();
    private.dispatch = None;
    let mut result = apply_event(private, ProtocolEvent::WrapperResponse { response: WrapperResponse::HumanGate(response) })
        .map_err(|e| format!("ordinary human decision refused: {e:?}"))?;
    let mut ledger = canonical.dispatch.clone().unwrap();
    ledger.phase = result.state.phase;
    ledger.signal()?;
    result.state.dispatch = Some(ledger);
    // Ordinary routing derives presentation with no parallel ledger. Restore
    // the canonical scheduling presentation before strict state validation.
    crate::runtime::normalize_in_flight_request_execution_hints(&mut result.state);
    result.state.validate()?;
    Ok(result)
}

fn independent_outcome(result: &TransitionOutcome, canonical: &ProtocolState) -> bool {
    result.state.phase == canonical.phase
        && matches!(result.state.in_flight_request.as_ref().map(|r| r.kind),
            Some(RequestKind::Review | RequestKind::Paper | RequestKind::Corr | RequestKind::Sound))
        && result.commands.iter().all(|c| matches!(c,
            ProtocolCommand::IssueRequest { .. } | ProtocolCommand::CommitCheckpoint
            | ProtocolCommand::DeleteLocalClosureRecord { .. }))
}

fn sound_only_review(state: &ProtocolState, response: &ReviewResponse) -> bool {
    if state.phase != Phase::TheoremStating || response.request_sound_verifier_nodes.is_empty() {
        return false;
    }
    // Positive allowlist: the older is_verifier_only_continuation predicate
    // also permits effects such as reset_blockers. Advice and grounding are
    // retained by the ordinary transition; every other action must be absent.
    response == &ReviewResponse {
        request_id: response.request_id,
        cycle: response.cycle,
        status: ResponseStatus::Ok,
        decision: ReviewDecisionKind::Continue,
        reason: response.reason.clone(),
        comments: response.comments.clone(),
        request_sound_verifier_nodes: response.request_sound_verifier_nodes.clone(),
        next_mode: state.current_mode(),
        paper_focus_ranges: response.paper_focus_ranges.clone(),
        paper_grounding: response.paper_grounding.clone(),
        ..Default::default()
    }
}

fn park(mut result: TransitionOutcome, canonical: &ProtocolState,
    inputs: &BTreeMap<String, String>, finishing: Option<u64>, consume_continuation: bool, verifier_context_version: u32,
    suppress_sound_review_self_wakeup: bool,
) -> Result<TransitionOutcome, String> {
    let mut ledger = canonical.dispatch.clone().unwrap();
    let kind = result.state.in_flight_request.as_ref().map(|r| r.kind);
    // A legacy Corr/Paper completion can select the ordinary Sound backlog.
    // Its private seed cannot survive parking in Reviewer, and only v5
    // carries the selected node/origin across that boundary. This branch
    // previously failed validation; successful legacy transitions stay intact.
    let verifier_context_version = if kind == Some(RequestKind::Sound)
        && result.state.sound_backlog_dispatch.is_some() && verifier_context_version < 5 {
        5
    } else { verifier_context_version };
    let independent = independent_outcome(&result, canonical);
    if !independent && !exclusive_ready(canonical, finishing) {
        let mut waiting = canonical.clone();
        waiting.dispatch.as_mut().unwrap().drain(ROUTING_DRAIN.into())?;
        return Ok(TransitionOutcome { state: waiting, commands: vec![] });
    }
    let reviewer_requested = consume_continuation
        && ledger.continuations.front().is_some_and(|c| matches!(c.as_ref(), Continuation::Review { .. }));
    let sound_review_self_wakeup = suppress_sound_review_self_wakeup
        && independent && kind == Some(RequestKind::Sound) && verifier_context_version == 5
        && finishing.is_none() && consume_continuation && canonical.pending_task.is_none()
        && ledger.draining.is_none()
        && ledger.continuations.front().is_some_and(|c| match c.as_ref() {
            Continuation::Review { phase, response, .. } => *phase == canonical.phase
                && sound_only_review(canonical, response)
                && result.state.independent_sound_selection().is_some_and(|target|
                    target.origin == IndependentSoundOrigin::ReviewerRequested
                    && response.request_sound_verifier_nodes.contains(&target.node)),
            _ => false,
        });
    if ledger.worker_handoff.as_ref().is_some_and(|h| !h.matches(&result.state.reviewer_comments)) {
        ledger.worker_handoff = None;
    }
    if consume_continuation {
        if let Some(Continuation::Review { review_id, response, .. }) = ledger.continuations.front().map(|c| c.as_ref()) {
            if result.state.reviewer_comments == response.comments.trim() {
                ledger.worker_handoff = Some(crate::dispatch::ReviewerHandoff::new(*review_id, &result.state.reviewer_comments));
            }
        }
        ledger.continuations.pop_front();
    }
    ledger.routing_pending = !ledger.continuations.is_empty();
    if independent {
        // A stale panel can remove the exclusive transition that requested
        // this drain. Preserve other reasons and still-waiting results/actions.
        if ledger.draining.as_deref() == Some(ROUTING_DRAIN)
            && ledger.continuations.is_empty()
            && !ledger.verifiers.jobs.values().any(|j| matches!(j.status, VerifierStatus::Checking { .. })) {
            ledger.draining = None;
        }
        if let Some(kind @ (RequestKind::Paper | RequestKind::Corr | RequestKind::Sound)) = kind {
            let id = result.state.in_flight_request.as_ref().unwrap().id;
            let version = if kind == RequestKind::Sound { verifier_context_version } else { verifier_context_version.min(4) };
            let request = Box::new(result.state.expected_request_with_verifier_context_version(id, kind, version));
            if !sound_target_current(&result.state, &request)? { return Err("new independent Sound target is ineligible".into()); }
            let key = identity(&request, inputs)?;
            // A fresh ordinary request after a completed Unknown/Malformed
            // round must run again; a simultaneously queued/running identical
            // request is shared by automatic and explicit dispatch.
            for job in ledger.verifiers.jobs.values_mut() {
                if job.identity == key && matches!(job.status, VerifierStatus::Resolved { .. }) {
                    if let VerifierStatus::Resolved { stale_reason, .. } = &mut job.status {
                        *stale_reason = Some("ordinary router requested a new verification round".into());
                    }
                }
            }
            let id = ledger.verifiers.enqueue(key, !reviewer_requested, reviewer_requested)?;
            ledger.verifiers.jobs.get_mut(&id).unwrap().request = Some(request);
            ledger.verifiers.jobs.get_mut(&id).unwrap().retired_request = None;
            debug_assert!(stage_for(kind).is_ok());
        }
        result.commands.retain(|c| !matches!(c, ProtocolCommand::IssueRequest { .. }));
        // A malformed independent Sound reissue has already captured its
        // original target/origin above. The private pin cannot enter canonical
        // Reviewer state or become authority for the next ordinary selection.
        result.state.independent_sound_target = None;
        if verifier_context_version == 5 { result.state.sound_backlog_dispatch = None; }
        result.state.stage = Stage::Reviewer;
        result.state.in_flight_request = canonical.in_flight_request.as_ref()
            .map(|r| Box::new(result.state.expected_request(r.id, RequestKind::Review)));
    } else {
        // Queued jobs have no process. Their snapshots cannot cross a global
        // phase/source change; running jobs were excluded above.
        let queued: Vec<_> = ledger.verifiers.jobs.values().filter(|j| j.status == VerifierStatus::Queued).map(|j| j.id).collect();
        for id in queued { ledger.verifiers.supersede_queued(id, "exclusive Trellis transition".into())?; }
        ledger.phase = result.state.phase;
        ledger.draining = None;
    }
    // The action and queue mutation are still journaled. event_seq is the
    // reviewer notification counter: issuing/sharing the Sound panel the
    // reviewer just requested is not another decision. Never acknowledge a
    // later event here; concurrent worker/result signals remain outstanding.
    // VerifierFinished and all other routing paths retain their normal signal.
    if !sound_review_self_wakeup { ledger.signal()?; }
    result.state.dispatch = Some(ledger);
    crate::runtime::normalize_in_flight_request_execution_hints(&mut result.state);
    result.state.validate()?;
    Ok(result)
}

pub fn apply(canonical: &ProtocolState, event: RoutingEvent) -> Result<TransitionOutcome, String> {
    let mut state = canonical.clone();
    let ledger = state.dispatch.as_mut().ok_or("parallel dispatch is not enabled")?;
    match event {
        RoutingEvent::SupersedeQueued { id, reason } => {
            ledger.verifiers.supersede_queued(id, reason)?;
            ledger.routing_pending = true;
            ledger.signal()?;
            crate::coarse_audits::requeue_stale(&mut state, id)?;
        }
        RoutingEvent::ExclusiveStep { inputs, expected_inputs, response } => {
            if !exclusive_ready(&state, None) || inputs != expected_inputs {
                return Err("exclusive operation requires drained attempts and unchanged captured inputs".into());
            }
            if matches!(state.stage, Stage::Reviewer | Stage::Worker | Stage::HumanGate | Stage::Complete) {
                return Err("this boundary requires a reviewer assignment, worker acceptance, or human decision".into());
            }
            let mut private = state.clone();
            private.dispatch = None;
            let event = match response {
                Some(response) => ProtocolEvent::WrapperResponse { response: *response },
                None if private.stage == Stage::Start => ProtocolEvent::StartCycle,
                None => return Err("exclusive request has no response".into()),
            };
            let result = apply_event(private, event).map_err(|e| format!("ordinary exclusive transition refused: {e:?}"))?;
            return park(result, &state, &inputs, None, false, 0, false);
        }
        RoutingEvent::AdoptWorker { graph, inputs, base_snapshot, now_ms } => {
            use crate::dispatch::{EditScope, PreparedAssignment, WorkerTaskKind};
            if state.stage != Stage::Worker || !exclusive_ready(&state, None) {
                return Err("exclusive worker adoption requires a drained ordinary Worker boundary".into());
            }
            let mut private = state.clone();
            private.dispatch = None;
            let request = private.in_flight_request.clone().ok_or("ordinary worker has no request")?;
            let mut ledger = state.dispatch.take().unwrap();
            let profile = ledger.config.profiles.iter().find(|(_, p)| p.allowed_tasks.contains(&WorkerTaskKind::Ordinary))
                .map(|(id, _)| id.clone()).ok_or("exclusive worker requires an ordinary-capable model profile")?;
            let mut shared_writes: BTreeSet<_> = inputs.keys().filter(|p| p.starts_with("reference/") || p.starts_with("Tablet/")).cloned().collect();
            shared_writes.extend(crate::dispatch_contract::witness_write_path(&request, CompilerCapability::Managed));
            let assignment = PreparedAssignment {
            proof_body_rebase_inputs: BTreeMap::new(),
                assignment_id: format!("exclusive-{}", request.id), profile, timeout_seconds: None,
                compiler: CompilerCapability::Managed, task_kind: WorkerTaskKind::Ordinary,
                scope: EditScope { nodes: state.live.present_nodes.clone(),
                    helper_prefixes: [format!("P{}_", ledger.next_task_id)].into(),
                    shared_writes,
                    exclusive: true },
                base_snapshot, input_hashes: inputs, request: request.into(), review: None, authorized_worker: None,
            };
            ledger.draining = None;
            let review_id = ledger.worker_handoff.as_ref()
                .filter(|h| h.matches(&state.reviewer_comments)).map_or(0, |h| h.review_id);
            let id = ledger.reserve(review_id, assignment, &graph, now_ms)?;
            let task = &ledger.tasks[&id];
            private.worker_execution = task.assignment.request.inline()?.task_execution.clone();
            private.in_flight_request = Some(Box::new(task.assignment.request.inline()?.clone()));
            private.validate()?;
            state.stage = Stage::Reviewer;
            state.clear_pending_task();
            state.in_flight_request = None;
            ledger.exclusive_workers.insert(id, ExclusiveWorker { state: Box::new(private), canonical_sha256: exclusive_digest(&state)? });
            state.dispatch = Some(ledger);
        }
        RoutingEvent::ReviewAction { review_id, response_sha256, response } => {
            if ledger.receipts.get(&review_id).is_some_and(|r| r.response_sha256 == response_sha256) {
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            let scoped_action = response.audit_request.as_ref().is_some_and(|a| a.coarse_nodes.is_some())
                || (state.coarse_goals.is_some() && response.next_active_coarse.is_some()
                    && (response.dismiss_audit_plan || !response.dismissed_tasks.is_empty()));
            if scoped_action && (state.stage != Stage::Reviewer || state.phase != Phase::ProofFormalization
                || state.audit_lane_in_flight() || state.dispatch.as_ref().unwrap().draining.is_some()) {
                state.dispatch.as_mut().unwrap().dispatch_prepared(review_id, response_sha256,
                    vec![crate::dispatch::PreparedItem::Refused { assignment_id: "action".into(),
                        reason: "scoped reviewer actions must wait for a proof-formalization reviewer boundary with no global operation in progress".into() }], &Default::default(), 0)?;
                state.in_flight_request = None;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            if let Some(validation) = crate::coarse_goals::plan_action(&state.expected_request(review_id, RequestKind::Review), &response) {
                let result = validation.and_then(|_| {
                    let goal = response.next_active_coarse.as_ref().unwrap();
                    let region = crate::coarse_goals::shallow_region(&state, goal);
                    if state.dispatch.as_ref().unwrap().tasks.values().any(|t| (t.status.occupies_slot() || t.pending_retry.is_some())
                        && (!t.assignment.scope.nodes.is_disjoint(&region)
                            || t.assignment.task_kind.is_ordinary() && t.assignment.request.inline().ok()
                                .and_then(|r| r.active_coarse_node.as_ref()) == Some(goal))) {
                        return Err("wait for this goal's current attempts before dismissing their audit plan".into());
                    }
                    if state.coarse_goals.as_ref().unwrap().pending_audits.iter().any(|a| a.goals.contains(goal))
                        || state.dispatch.as_ref().unwrap().verifiers.jobs.values().any(|j|
                            !matches!(j.status, VerifierStatus::Resolved { .. }) && j.request.as_ref()
                                .and_then(|r| r.scoped_audit.as_ref()).is_some_and(|a| a.goals.contains(goal))) {
                        return Err("wait for this goal's queued or running audit before dismissing its plan".into());
                    }
                    let mut private = state.clone_without_dispatch();
                    private.project_coarse_goal(goal)?;
                    crate::engine::apply_review_audit_plan_actions(&mut private, &response);
                    state.coarse_goals.as_mut().unwrap().goals.get_mut(goal).unwrap().plan = private.audit_plan;
                    crate::engine::record_memory_challenges(&mut state, "reviewer", response.request_id, &response.memory_challenges);
                    Ok(())
                });
                let items = result.err().into_iter().map(|reason| crate::dispatch::PreparedItem::Refused { assignment_id: "action".into(), reason }).collect();
                state.dispatch.as_mut().unwrap().dispatch_prepared(review_id, response_sha256, items, &Default::default(), 0)?;
                state.in_flight_request = None;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            if let Some(scopes) = response.audit_request.as_ref().and_then(|a| a.coarse_nodes.as_ref()) {
                let mut trial = state.clone();
                let mut review = *response.clone();
                review.cycle = state.cycle;
                review.request_id = trial.in_flight_request.as_ref().map_or(review_id, |r| r.id);
                let refusal = if !trial.review_response_legal(&review) {
                    Some(trial.review_response_rejection_reasons(&review).join("; "))
                } else {
                    crate::coarse_audits::submit(&mut trial, crate::coarse_audits::AuditIntent {
                        goals: scopes.clone(), reason: response.audit_request.as_ref().unwrap().reason.clone(), reviewer_requested: true, retry: 0, feedback: String::new(),
                    }).err()
                };
                if refusal.is_none() {
                    state.coarse_goals = trial.coarse_goals;
                    crate::engine::record_memory_challenges(&mut state, "reviewer", response.request_id, &response.memory_challenges);
                    state.pending_worker_audit_request = None;
                }
                let items = refusal.into_iter().map(|reason| crate::dispatch::PreparedItem::Refused { assignment_id: "action".into(), reason }).collect();
                let ledger = state.dispatch.as_mut().unwrap();
                ledger.dispatch_prepared(review_id, response_sha256, items, &Default::default(), 0)?;
                state.in_flight_request = None;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            let retired_sidecar = !response.sidecar_queue_add.is_empty() || !response.sidecar_queue_remove.is_empty();
            let continuation = Continuation::Review { review_id, phase: state.phase, response };
            // Validate now, then again after any required drain. A reviewer
            // cannot smuggle an unreserved worker into this global channel.
            let refusal = if retired_sidecar {
                Some("parallel runs use worker assignments for closure work; the sidecar queue is retired".into())
            } else {
                continue_parallel_dispatch(state.clone(), &continuation).err()
                    .map(|error| format!("reviewer action refused: {error:?}"))
            };
            if let Some(reason) = refusal {
                let ledger = state.dispatch.as_mut().unwrap();
                ledger.dispatch_prepared(review_id, response_sha256,
                    vec![crate::dispatch::PreparedItem::Refused {
                        assignment_id: "action".into(), reason,
                    }], &Default::default(), 0)?;
                state.in_flight_request = None;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            let ledger = state.dispatch.as_mut().unwrap();
            ledger.dispatch_prepared(review_id, response_sha256, vec![], &Default::default(), 0)?;
            ledger.continuations.push_back(Box::new(continuation));
            ledger.routing_pending = true;
            state.in_flight_request = None;
        }
        RoutingEvent::VerifierLaunched { id, process, now_ms } => {
            ledger.verifiers.launch(id, process, now_ms, ledger.config.verifier_slots)?;
        }
        RoutingEvent::VerifierCandidate { id, response } => {
            let result_sha256 = crate::trusted_artifact_rebind::digest(&serde_json::to_value(response).map_err(|e| e.to_string())?);
            ledger.verifiers.candidate(id, result_sha256)?;
        }
        RoutingEvent::Route { inputs, verifier_context_version, suppress_sound_review_self_wakeup } => {
            if verifier_context_version > 5 { return Err("unsupported verifier context version".into()); }
            crate::coarse_audits::enqueue_ready(&mut state, &inputs)?;
            refresh_coarse_request(&mut state);
            let ledger = state.dispatch.as_mut().unwrap();
            // Explicit routing can also recover an automatic drain retained
            // by an older deployment after its last result was consumed.
            let retained_drain = state.stage == Stage::Reviewer
                && ledger.draining.as_deref() == Some(ROUTING_DRAIN);
            if (!ledger.routing_pending && !retained_drain) || exclusive_work_pending(ledger) {
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            let continuation = ledger.continuations.front().cloned().unwrap_or(Box::new(Continuation::Progress));
            let next = continue_parallel_dispatch(state.clone(), &continuation);
            if let Continuation::Review { review_id, phase, .. } = continuation.as_ref() {
                let refusal = if *phase != state.phase || state.stage != Stage::Reviewer {
                    Some("the ordinary router advanced since this reviewer action was prepared".to_string())
                } else { next.as_ref().err().map(|e| format!("reviewer action is no longer applicable: {e:?}")) };
                if let Some(reason) = refusal {
                    let ledger = state.dispatch.as_mut().unwrap();
                    ledger.receipts.get_mut(review_id).ok_or("pending reviewer action has no receipt")?
                        .decisions.push(crate::dispatch::DispatchDecision {
                            assignment_id: "action".into(), task_id: None, rejection: Some(reason),
                        });
                    ledger.continuations.pop_front();
                    ledger.routing_pending = !ledger.continuations.is_empty();
                    if !ledger.routing_pending && ledger.draining.as_deref() == Some(ROUTING_DRAIN) {
                        ledger.draining = None;
                    }
                    ledger.signal()?;
                    state.validate()?;
                    return Ok(TransitionOutcome { state, commands: vec![] });
                }
            }
            let next = next.map_err(|e| format!("ordinary dispatch routing refused: {e:?}"))?;
            return park(next, &state, &inputs, None, true, verifier_context_version, suppress_sound_review_self_wakeup);
        }
        RoutingEvent::VerifierFinished { id, inputs, response } => {
            let job = ledger.verifiers.jobs.get(&id).ok_or("unknown verifier job")?.clone();
            let result_hash = crate::trusted_artifact_rebind::digest(&serde_json::to_value(&response).map_err(|e| e.to_string())?);
            if matches!(job.status, VerifierStatus::Resolved { .. }) {
                if !matches!(&job.status, VerifierStatus::Resolved { result_sha256: Some(hash), .. } if hash == &result_hash) {
                    return Err("verifier already resolved a different result".into());
                }
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            if job.status != (VerifierStatus::Checking { result_sha256: result_hash }) {
                return Err("verifier result differs from its sealed candidate".into());
            }
            if !sound_target_current(&state, job.request.as_ref().ok_or("verifier job has no ordinary request")?)? {
                let ledger = state.dispatch.as_mut().unwrap();
                ledger.verifiers.finish_stale(id, "admitted Sound target is no longer eligible".into())?;
                ledger.routing_pending = true;
                ledger.signal()?;
                state.validate()?;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            let private = verifier_state(&state, id)?;
            let request = private.in_flight_request.as_ref().unwrap();
            let verifier_context_version = request.verifier_context_version;
            let current = identity(request, &inputs)?;
            if current != job.identity {
                let ledger = state.dispatch.as_mut().unwrap();
                ledger.verifiers.finish(id, &current)?;
                ledger.routing_pending = true;
                ledger.signal()?;
                crate::coarse_audits::requeue_stale(&mut state, id)?;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            if request.scoped_audit.is_some() {
                return crate::coarse_audits::finish(state, id, *response, current);
            }
            // Rebind scheduling coordinates only after the complete semantic
            // request and source inventory matched. The panel's verdicts and
            // membership evidence remain untouched.
            let mut value = serde_json::to_value(response).map_err(|e| e.to_string())?;
            value["request_id"] = request.id.into();
            value["cycle"] = private.cycle.into();
            let response = serde_json::from_value(value).map_err(|e| format!("verifier response: {e}"))?;
            let next = apply_event(private, ProtocolEvent::WrapperResponse { response })
                .map_err(|e| format!("ordinary verifier transition refused result: {e:?}"))?;
            // Mark this panel resolved in the input supplied to park, so an
            // immediately repeated ordinary round receives a new queue ID.
            let independent = independent_outcome(&next, &state);
            let mut resolved = state.clone();
            resolved.dispatch.as_mut().unwrap().verifiers.finish(id, &current)?;
            let result = park(next, &resolved, &inputs, Some(id), false, verifier_context_version, false)?;
            if !independent && result.state.dispatch.as_ref().unwrap().draining.is_some()
                && !exclusive_ready(&state, Some(id)) {
                // Keep the original Checking result and its occupied slot
                // until the exclusive transition actually consumes it.
                state.dispatch.as_mut().unwrap().drain(ROUTING_DRAIN.into())?;
                return Ok(TransitionOutcome { state, commands: vec![] });
            }
            return Ok(result);
        }
    }
    refresh_coarse_request(&mut state);
    state.validate()?;
    Ok(TransitionOutcome { state, commands: vec![] })
}

// Scoped queue/progress mutations change the derived reviewer view even when
// no mathematical transition runs. Frozen worker/verifier contracts stay fixed.
fn refresh_coarse_request(state: &mut ProtocolState) {
    if state.coarse_goals.is_some() {
        if let Some((id, kind)) = state.in_flight_request.as_ref().map(|r| (r.id, r.kind)) {
            state.in_flight_request = Some(Box::new(state.expected_request(id, kind)));
        }
    }
}

#[cfg(test)]
mod scoped_sound_context_tests {
    use super::*;

    #[test]
    fn sound_review_self_wakeup_guard_requires_only_allowlisted_advice() {
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        let response = ReviewResponse {
            status: ResponseStatus::Ok, decision: ReviewDecisionKind::Continue,
            next_mode: state.current_mode(), request_sound_verifier_nodes: BTreeSet::from(["a".into()]),
            comments: "Preserve the worker and verify a".into(), ..Default::default()
        };
        assert!(sound_only_review(&state, &response));
        for field in ["clear_human_input", "assumption_authoring_request", "dismiss_audit_plan",
            "cleanup_request_reaudit", "consume_global_repair_grant", "must_close_active",
            "confirm_protected_semantic_change_scope", "dispatch_node_retirement"] {
            let mut value = serde_json::to_value(&response).unwrap();
            value[field] = true.into();
            let changed: ReviewResponse = serde_json::from_value(value).unwrap();
            assert!(!sound_only_review(&state, &changed), "{field} is an action, not advice");
        }
        let mut changed = response.clone();
        changed.next_active = Some("a".into());
        assert!(!sound_only_review(&state, &changed));
        changed = response.clone(); changed.sidecar_queue_add.push("a".into());
        assert!(!sound_only_review(&state, &changed));
        changed = response.clone(); changed.node_retirement_decline_reason = "decline".into();
        assert!(!sound_only_review(&state, &changed));
        changed = response.clone(); changed.request_sound_verifier_nodes.clear();
        assert!(!sound_only_review(&state, &changed));
        changed = response.clone(); changed.status = ResponseStatus::Malformed;
        assert!(!sound_only_review(&state, &changed));
        for phase in [Phase::Modeling, Phase::RevisionStating, Phase::ProofFormalization] {
            state.phase = phase;
            assert!(!sound_only_review(&state, &response));
        }
    }

    #[test]
    fn sound_review_self_wakeup_guard_is_explicit_in_new_route_journal_only() {
        let old = serde_json::json!({"kind":"route", "inputs":{}, "verifier_context_version":5});
        let event: RoutingEvent = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(event, RoutingEvent::Route { suppress_sound_review_self_wakeup: false, .. }));
        assert_eq!(serde_json::to_value(event).unwrap(), old);
        let mut new = old; new["suppress_sound_review_self_wakeup"] = true.into();
        let event: RoutingEvent = serde_json::from_value(new.clone()).unwrap();
        assert!(matches!(event, RoutingEvent::Route { suppress_sound_review_self_wakeup: true, .. }));
        assert_eq!(serde_json::to_value(event).unwrap(), new);
    }

    #[test]
    fn sound_v4_separates_build_provenance_from_judgment_policy_only() {
        let state = ProtocolState::default();
        let inputs = BTreeMap::from([("Tablet/A.tex".into(), "1".repeat(64)),
            ("reference/absent.txt".into(), "absent".into())]);
        for version in 0..=4 {
            for kind in [RequestKind::Sound, RequestKind::Paper, RequestKind::Corr] {
                let request = state.expected_request_with_verifier_context_version(7, kind, version);
                let key = identity(&request, &inputs).unwrap();
                let mut changed = request.clone();
                changed.acceptance_logic_identity = "different complete deployment".into();
                crate::populate_request_prompt_contracts(&mut changed, None);
                let scoped = version == 4 && kind == RequestKind::Sound;
                assert_eq!(identity(&changed, &inputs).unwrap() == key, scoped,
                    "only new Sound requests may survive checker-only changes");
                if !scoped {
                    assert!(request.verifier_logic_identity.is_empty());
                    assert!(serde_json::to_value(&request).unwrap().get("verifier_logic_identity").is_none());
                    continue;
                }
                assert_eq!(key.compiler, CompilerCapability::None);
                let saved = serde_json::to_value(&changed).unwrap();
                assert_eq!(saved["acceptance_logic_identity"], "different complete deployment");
                let restored: WrapperRequest = serde_json::from_value(saved).unwrap();
                assert_eq!(key, identity(&restored, &inputs).unwrap());
                for field in ["policy", "membership", "prompt", "evidence"] {
                    let mut changed = request.clone();
                    match field {
                        "policy" => changed.verifier_logic_identity.push('1'),
                        "membership" => { changed.verify_lanes.insert("another-member".into()); },
                        "prompt" => changed.sound_contract["rubric"] = serde_json::json!("changed rubric"),
                        _ => { changed.previous_sound_lane_findings.insert("A".into(), BTreeMap::new()); },
                    }
                    assert_ne!(key, identity(&changed, &inputs).unwrap(), "{field}");
                }
                changed.verifier_logic_identity.clear();
                assert!(identity(&changed, &inputs).is_err());
                for path in inputs.keys() {
                    let mut changed = inputs.clone();
                    changed.insert(path.clone(), "2".repeat(64));
                    assert_ne!(key, identity(&request, &changed).unwrap(), "{path}");
                }
            }
        }
    }

    #[test]
    fn sound_v3_identity_ignores_only_unrelated_recent_sound_findings() {
        // Production verifier51/52: identical source inventory; only request
        // id/cycle and previous owner's recent Sound evidence changed.
        let mut before = ProtocolState::default();
        before.phase = Phase::ProofFormalization;
        before.cycle = 28;
        before.live.present_nodes = BTreeSet::from(["A".into(), "B".into()]);
        before.sound_backlog_dispatch = Some("B".into());
        before.latest_sound_reviewer_evidence.insert("A".into(), BTreeMap::from([
            ("v1".into(), SoundReviewerLaneEvidence { node: "A".into(), summary: "prior owner finding".into(), ..Default::default() })
        ]));
        before.previous_sound_lane_findings.insert("B".into(), BTreeMap::from([
            ("v1".into(), SoundReviewerLaneEvidence { node: "B".into(), summary: "own historical finding".into(), ..Default::default() })
        ]));
        let mut after = before.clone();
        after.cycle = 29;
        after.latest_sound_reviewer_evidence.clear();
        let inputs = BTreeMap::from([("Tablet/B.tex".into(), "1".repeat(64)),
            ("Tablet/Dependency.tex".into(), "2".repeat(64)), ("reference/missing.txt".into(), "absent".into())]);
        for version in [0, 1, 2, 3] {
            let request = before.expected_request_with_verifier_context_version(153, RequestKind::Sound, version);
            let key = identity(&request, &inputs).unwrap();
            let current = after.expected_request_with_verifier_context_version(157, RequestKind::Sound, version);
            assert_eq!(key == identity(&current, &inputs).unwrap(), version == 3);
            let mut queue = crate::dispatch_verifiers::VerifierQueue::default();
            let id = queue.enqueue(key.clone(), true, false).unwrap();
            queue.launch(id, ProcessIdentity { pid: 10, start_ticks: 1, boot_id: "test".into() }, 1, 1).unwrap();
            queue.candidate(id, "3".repeat(64)).unwrap();
            assert_eq!(queue.finish(id, &identity(&current, &inputs).unwrap()).unwrap(), version == 3);
            let restored: crate::dispatch_verifiers::VerifierQueue = serde_json::from_value(serde_json::to_value(&queue).unwrap()).unwrap();
            assert_eq!(restored, queue);
            if version != 3 { continue; }

            for field in ["recent", "historical", "target"] {
                let mut changed = after.clone();
                match field {
                    "recent" => { changed.latest_sound_reviewer_evidence.insert("B".into(), BTreeMap::from([
                        ("v1".into(), SoundReviewerLaneEvidence { node: "B".into(), summary: "new own finding".into(), ..Default::default() })
                    ])); }
                    "historical" => { changed.previous_sound_lane_findings.clear(); }
                    _ => { changed.sound_backlog_dispatch = Some("A".into()); }
                }
                assert_ne!(key, identity(&changed.expected_request_with_verifier_context_version(157, RequestKind::Sound, version), &inputs).unwrap(), "{field} remains current");
            }
            for path in inputs.keys() {
                let mut changed = inputs.clone();
                changed.insert(path.clone(), "4".repeat(64));
                assert_ne!(key, identity(&current, &changed).unwrap(), "every source and absence remains bound");
            }
            let mut changed = current.clone();
            changed.acceptance_logic_identity = "changed implementation".into();
            assert_ne!(key, identity(&changed, &inputs).unwrap());
            let mut changed = current.clone();
            changed.verify_lanes.insert("another-panel-member".into());
            assert_ne!(key, identity(&changed, &inputs).unwrap());
        }
    }
}
