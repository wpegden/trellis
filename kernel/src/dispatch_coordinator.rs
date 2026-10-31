//! Host preparation and replayable dispatch receipts. Uses the ordinary task
//! router for every assignment; failed preparations do not discard good peers.
use crate::dispatch::{DependencyGraph, DispatchEvent, DispatchReceipt, PreparedItem, TaskStatus};
use crate::dispatch_contract::{prepare_worker, AssignmentOptions, PreparedWorker};
use crate::model::{ProtocolState, ReviewResponse};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub review: ReviewResponse,
    pub options: AssignmentOptions,
    #[serde(default)]
    pub preparation_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedBatch {
    pub event: DispatchEvent,
    pub receipt: DispatchReceipt,
    pub workers: BTreeMap<u64, PreparedWorker>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedRetry {
    pub event: DispatchEvent,
    pub worker: Option<PreparedWorker>,
    pub task_id: Option<u64>,
}

pub fn prepare_retry(state: &ProtocolState, predecessor: u64, graph: DependencyGraph,
    inputs: BTreeMap<String, String>, proof_body_rebase_inputs: BTreeMap<String, String>, base_snapshot: String, now_ms: u64,
) -> Result<Option<PreparedRetry>, String> {
    let mut ledger = state.dispatch.clone().ok_or("parallel dispatch is not enabled")?;
    let prior = ledger.tasks.get(&predecessor).ok_or("unknown retry predecessor")?;
    let retry = prior.pending_retry.clone().ok_or("predecessor has no automatic retry")?;
    if !matches!(prior.status, crate::dispatch::TaskStatus::Resolved { .. }) {
        return Err("automatic retry must wait for predecessor cleanup".into());
    }
    let budget = ledger.config.bind(&prior.assignment.profile, prior.assignment.timeout_seconds)
        .and_then(|binding| ledger.check_lightweight_budget(&prior.assignment, &binding, None));
    if budget.is_ok() && ledger.draining.is_none() && (ledger.available_slots() == 0
        || !ledger.profile_capacity_available(&ledger.config.bind(&prior.assignment.profile, prior.assignment.timeout_seconds)?))
    {
        return Ok(None); // Capacity is temporary; retain the pending retry.
    }
    let prepared = (|| {
        if let Some(reason) = &ledger.draining { return Err(format!("run is draining: {reason}")); }
        budget?;
        for (path, expected) in &prior.assignment.input_hashes {
            if !prior.assignment.input_matches(path, expected, &inputs, &proof_body_rebase_inputs) { return Err(format!("stale retry input: {path}")); }
        }
        let mut private = crate::dispatch_integration::acceptance_state(state, predecessor, &graph)?;
        retry.install(&mut private);
        // bind_reserved_task installs the new attempt identity after reserve.
        private.worker_execution = None;
        let id = private.in_flight_request.as_ref().ok_or("retry has no worker request")?.id;
        let mut assignment = prior.assignment.clone();
        assignment.request = Arc::new(private.expected_request(id, crate::model::RequestKind::Worker)).into();
        assignment.base_snapshot = base_snapshot;
        private.in_flight_request = Some(Box::new(assignment.request.inline()?.clone()));
        Ok(PreparedWorker { assignment, private_state: private })
    })();
    let (worker, assignment, refusal) = match prepared {
        Ok(worker) => (Some(worker.clone()), Some(worker.assignment), None),
        Err(reason) => (None, None, Some(reason)),
    };
    let event = DispatchEvent::RetryPrepared { predecessor, assignment, refusal, graph, now_ms };
    ledger.apply(event.clone())?;
    let task = ledger.tasks.values().find(|t| t.retry_of == Some(predecessor));
    let task_id = task.map(|t| t.id);
    let worker = match (worker, task) {
        (Some(worker), Some(task)) => Some(worker.bind_reserved_task(task)?),
        (None, None) => None,
        _ => return Err("retry reservation disagrees with its prepared worker".into()),
    };
    Ok(Some(PreparedRetry { event, worker, task_id }))
}

pub fn prepare_batch(
    state: &ProtocolState, assignments: Vec<Assignment>, graph: DependencyGraph,
    review_id: u32, response_sha256: String, now_ms: u64,
) -> Result<PreparedBatch, String> {
    let mut ledger = state.dispatch.clone().ok_or("parallel dispatch is not enabled")?;
    let mut items = Vec::new();
    let mut prepared = Vec::new();
    for mut assignment in assignments {
        let id = assignment.options.assignment_id.clone();
        // The reviewer reasoned over an immutable earlier snapshot. Rebase
        // scheduling coordinates; ordinary legality checks all substantive
        // choices against current canonical state below.
        assignment.review.cycle = state.cycle;
        assignment.review.request_id = state.in_flight_request.as_ref()
            .filter(|r| r.kind == crate::model::RequestKind::Review)
            .map(|r| r.id).unwrap_or(review_id);
        let result = match assignment.preparation_error {
            Some(reason) => Err(reason),
            None => prepare_worker(state, assignment.review, &graph, assignment.options),
        };
        match result {
            Ok(worker) => {
                items.push(PreparedItem::Ready { assignment: worker.assignment.clone() });
                prepared.push(Some(worker));
            }
            Err(reason) => {
                items.push(PreparedItem::Refused { assignment_id: id, reason });
                prepared.push(None);
            }
        }
    }
    let event = state.coarse_filter_dispatch(DispatchEvent::BatchPrepared { review_id, response_sha256, items, graph, now_ms })?;
    ledger.apply(event.clone())?;
    let receipt = ledger.receipts.get(&review_id).ok_or("batch has no durable receipt")?.clone();
    let mut workers = BTreeMap::new();
    for (decision, worker) in receipt.decisions.iter().zip(prepared) {
        if let Some(id) = decision.task_id {
            let worker = worker.ok_or("accepted assignment has no prepared worker")?;
            workers.insert(id, worker.bind_reserved_task(&ledger.tasks[&id])?);
        }
    }
    Ok(PreparedBatch { event, receipt, workers })
}

pub fn status(state: &ProtocolState) -> Result<serde_json::Value, String> {
    let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    // Status is a scheduling view, not an acceptance or recovery snapshot.
    // Construct it directly: serializing the complete ledger first copies all
    // historical private states and requests on every coordinator poll.
    let tasks: BTreeMap<_, _> = ledger.tasks.iter().map(|(id, task)| {
        let a = &task.assignment;
        (*id, serde_json::json!({
            "id": task.id, "review_id": task.review_id,
            "assignment": {
                "assignment_id": a.assignment_id, "profile": a.profile,
                "timeout_seconds": a.timeout_seconds, "compiler": a.compiler,
                "task_kind": a.task_kind, "scope": a.scope,
                "base_snapshot": a.base_snapshot, "input_hashes": a.input_hashes,
                "proof_body_rebase_inputs": a.proof_body_rebase_inputs,
            },
            "binding": task.binding, "status": task.status, "retry_of": task.retry_of,
            "retry": task.retry, "reserved_at_ms": task.reserved_at_ms,
            "launched_at_ms": task.launched_at_ms, "process": task.process,
            "checker_wait_ms": task.checker_wait_ms, "report": task.report,
            "worker_attempt": task.worker_attempt,
            "execution_capacity_managed": ledger.config.max_in_flight_tasks.is_some(),
            "execution_runnable": ledger.worker_execution_runnable(task.id),
            "execution_capacity_released": task.execution_capacity_released,
            "pending_retry": task.pending_retry, "retry_refusal": task.retry_refusal,
            "deadline_ms": if task.status == TaskStatus::Running { task.deadline_ms() } else { None },
            "request_archivable": !state.pv_revision_ready() && state.phase != crate::Phase::Complete
                && state.stage != crate::Stage::Complete && matches!(task.status, crate::dispatch::TaskStatus::Resolved { .. })
                && task.pending_retry.is_none() && matches!(task.assignment.request, crate::dispatch_history::FrozenRequest::Inline(_)),
        }))
    }).collect();
    let jobs: BTreeMap<_, _> = ledger.verifiers.jobs.iter().map(|(id, job)| {
        // Generic verifier scope covers correspondence, sound and paper
        // targets; per-node substantiveness uses its own selected frontier.
        let summary = job.request.as_deref().map(crate::verifier_history::Summary::from_request)
            .or_else(|| job.retired_request.as_ref().map(|r| r.summary.clone())).unwrap_or_default();
        (*id, serde_json::json!({
            "id": job.id, "identity": job.identity, "required": job.required,
            "reviewer_requested": job.reviewer_requested, "status": job.status,
            "nodes": summary.nodes, "targets": summary.targets,
            "audit_goals": summary.audit_goals,
        }))
    }).collect();
    let mut value = serde_json::json!({
        "config": ledger.config, "phase": ledger.phase, "tasks": tasks,
        "lightweight_attempts_per_node": ledger.lightweight_limit(),
        "lightweight_attempts_by_node": ledger.lightweight_attempts_by_node(),
        "pv_revision_ready": state.pv_revision_ready(),
        "resolved_verifier_requests_retirable": crate::verifier_history::eligible(state),
        "coarse_goals": state.coarse_goals_view(),
        "coarse_audits_pending": crate::coarse_audits::needs_routing(state),
        "receipts": ledger.receipts, "next_task_id": ledger.next_task_id,
        "event_seq": ledger.event_seq, "reviewed_event": ledger.reviewed_event,
        "reviewer": ledger.reviewer, "draining": ledger.draining,
        "verifiers": {"next_id": ledger.verifiers.next_id, "jobs": jobs},
        "routing_pending": ledger.routing_pending, "publications": ledger.publications,
    });
    value["available_slots"] = ledger.available_slots().into();
    value["available_sol_slots"] = ledger.available_sol_slots().into();
    value["available_heavy_slots"] = ledger.available_heavy_slots().into();
    value["occupied_heavy_slots"] = ledger.occupied_heavy_slots().into();
    value["reviewer_runnable"] = (ledger.reviewer_runnable() && state.stage == crate::model::Stage::Reviewer
        && !ledger.routing_pending && !ledger.tasks.values().any(|t| t.assignment.scope.exclusive && t.status.occupies_slot())).into();
    value["stage"] = serde_json::to_value(state.stage).map_err(|e| e.to_string())?;
    value["request"] = serde_json::to_value(&state.in_flight_request).map_err(|e| e.to_string())?;
    value["verifiers_runnable"] = (!state.pv_revision_ready() && (ledger.verifiers.jobs.values().any(|j| matches!(j.status,
        crate::dispatch_verifiers::VerifierStatus::Running { .. } | crate::dispatch_verifiers::VerifierStatus::Checking { .. }))
        || (!crate::dispatch_routing::exclusive_work_pending(ledger)
            && ledger.verifiers.next_queued(ledger.config.verifier_slots).is_some())
        || !matches!(state.stage, crate::model::Stage::Reviewer | crate::model::Stage::Worker
            | crate::model::Stage::HumanGate | crate::model::Stage::Complete))).into();
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_pv_revision_is_not_an_exclusive_protocol_step() {
        let mut state = ProtocolState::default();
        crate::runtime::test_support::add_resolved_dispatch_task(&mut state);
        state.stage = crate::Stage::Start;
        assert_eq!(status(&state).unwrap()["verifiers_runnable"], true);
        state.pv_revision_handoff = Some(crate::model::PvRevisionHandoff {
            schema_version: 1, cycle: state.cycle, refuted_goals: vec![],
        });
        let projection = status(&state).unwrap();
        assert_eq!(projection["pv_revision_ready"], true);
        assert_eq!(projection["verifiers_runnable"], false);
        assert_eq!(projection["reviewer_runnable"], false);
        assert_eq!(projection["tasks"]["1"]["request_archivable"], false);
    }
}
