//! TLA replay fixture regenerator.
//!
//! Driven by `scripts/regenerate_tla_replay_fixtures.sh`. Reads the
//! current `tests/fixtures/tla_replay.json`, runs every step's event
//! through `apply_abstract_event`, and writes a NEW fixture with the
//! engine-derived `expected` blocks. The `action` and `event` fields
//! are preserved from the original fixture; only `expected` and
//! `expected_commands` are rewritten.
//!
//! This test is opt-in via the `TRELLIS_TLA_REPLAY_REGEN` env var.
//! Without that var, it's a no-op. With it set to a path, the
//! rewritten fixture is written there (and the script copies it back
//! over the original).
//!
//! The action/event matching reuses the same logic the main
//! `tla_replay.rs` test uses — we vendor a minimal copy of
//! `action_matches_event` here so the regen path stays self-contained.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};
use trellis_kernel::{
    apply_abstract_event, AbstractEvent, AbstractState, AbstractTransitionOutcome, CorrStatus,
    Fingerprint, HumanChoice, NodeId, NodeKind, ReviewDecisionKind, SpecAction, TargetId,
    TraceCase, TraceFixture, WorkerOutcome, WrapperResponse,
};

// ---- helpers vendored from tla_replay.rs ------------------------------------

fn invert_coverage(
    coverage: &BTreeMap<TargetId, BTreeSet<NodeId>>,
) -> BTreeMap<NodeId, BTreeSet<TargetId>> {
    let mut claims = BTreeMap::new();
    for (target, nodes) in coverage {
        for node in nodes {
            claims
                .entry(node.clone())
                .or_insert_with(BTreeSet::new)
                .insert(target.clone());
        }
    }
    claims
}

fn derive_paper_fingerprints(
    configured_targets: &BTreeSet<TargetId>,
    coverage: &BTreeMap<TargetId, BTreeSet<NodeId>>,
    target_fingerprints: &BTreeMap<NodeId, String>,
) -> BTreeMap<TargetId, String> {
    configured_targets
        .iter()
        .map(|target| {
            let fingerprint = coverage
                .get(target)
                .into_iter()
                .flat_map(|nodes| nodes.iter())
                .map(|node| {
                    format!(
                        "{}={}",
                        node,
                        target_fingerprints.get(node).cloned().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("|");
            (target.clone(), fingerprint)
        })
        .collect()
}

fn backfill_node_kinds(
    node_kinds: &mut BTreeMap<NodeId, NodeKind>,
    present_nodes: &BTreeSet<NodeId>,
    proof_nodes: &BTreeSet<NodeId>,
) {
    node_kinds.retain(|node, _| present_nodes.contains(node));
    for node in present_nodes {
        node_kinds.entry(node.clone()).or_insert_with(|| {
            if node.as_str() == "Preamble" {
                NodeKind::Preamble
            } else if proof_nodes.contains(node) {
                NodeKind::Proof
            } else {
                NodeKind::Definition
            }
        });
    }
}

fn normalize_fixture_state(mut state: AbstractState) -> AbstractState {
    let missing_live_claims = state.target_claims.is_empty();
    let missing_committed_claims = state.committed_target_claims.is_empty();
    if state.configured_targets.is_empty() {
        state
            .configured_targets
            .extend(state.live.coverage.keys().cloned());
        state
            .configured_targets
            .extend(state.committed.coverage.keys().cloned());
    }
    if state.committed_target_claims.is_empty() {
        state.committed_target_claims = invert_coverage(&state.committed.coverage);
    }
    if missing_live_claims
        && missing_committed_claims
        && state.live.coverage == state.committed.coverage
    {
        state.target_claims = invert_coverage(&state.live.coverage);
    }
    if state.committed_proof_nodes.is_empty() {
        state.committed_proof_nodes = state.proof_nodes.clone();
    }
    backfill_node_kinds(
        &mut state.node_kinds,
        &state.live.present_nodes,
        &state.proof_nodes,
    );
    backfill_node_kinds(
        &mut state.committed_node_kinds,
        &state.committed.present_nodes,
        &state.committed_proof_nodes,
    );
    if state.paper_status.is_empty() {
        for target in &state.configured_targets {
            state.paper_status.insert(target.clone(), CorrStatus::Pass);
        }
    }
    if state.substantiveness_status.is_empty() {
        for node in state
            .live
            .present_nodes
            .iter()
            .chain(state.committed.present_nodes.iter())
        {
            if node.as_str() == "Preamble" {
                continue;
            }
            state
                .substantiveness_status
                .entry(node.clone())
                .or_insert(CorrStatus::Pass);
            let fp = Fingerprint::from(format!("sub_{}", node.as_str()));
            state
                .live
                .substantiveness_current_fingerprints
                .entry(node.clone())
                .or_insert_with(|| fp.clone());
            state
                .committed
                .substantiveness_current_fingerprints
                .entry(node.clone())
                .or_insert_with(|| fp.clone());
            state
                .substantiveness_approved_fingerprints
                .entry(node.clone())
                .or_insert(fp);
        }
    }
    state.live.paper_current_fingerprints = derive_paper_fingerprints(
        &state.configured_targets,
        &state.live.coverage,
        &state.live.target_fingerprints,
    );
    state.committed.paper_current_fingerprints = derive_paper_fingerprints(
        &state.configured_targets,
        &state.committed.coverage,
        &state.committed.target_fingerprints,
    );
    for (target, status) in state.paper_status.clone() {
        if status == CorrStatus::Pass {
            let approved_fp = state
                .committed
                .paper_current_fingerprints
                .get(&target)
                .or_else(|| state.live.paper_current_fingerprints.get(&target));
            if let Some(current) = approved_fp {
                state
                    .paper_approved_fingerprints
                    .insert(target, current.clone());
            }
        }
    }
    for (target, fp) in state.live.paper_current_fingerprints.clone() {
        state
            .paper_approved_fingerprints
            .entry(target)
            .or_insert(fp);
    }
    // Backfill the in_flight_request with full WrapperRequest derived
    // from current state (validation_kind, blockers, etc.). The
    // fixture stores only {id, kind, cycle}; without this backfill,
    // worker responses get rejected with "no worker validation kind"
    // because the default `worker_context.validation_kind` is None.
    state.ensure_node_metadata();
    if let Some(request) = state.in_flight_request.clone() {
        state.in_flight_request = Some(state.expected_request(request.id, request.kind));
    }
    state
}

fn action_matches_event(action: &SpecAction, state: &AbstractState, event: &AbstractEvent) -> bool {
    match (action, event) {
        (SpecAction::StartCycle, AbstractEvent::StartCycle) => true,
        (
            SpecAction::AcceptValidWorker,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Worker(worker),
            },
        ) => worker.outcome == WorkerOutcome::Valid,
        (
            SpecAction::AcceptInvalidWorker,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Worker(worker),
            },
        ) => {
            worker.outcome == WorkerOutcome::Invalid
                || worker.status == trellis_kernel::ResponseStatus::Malformed
        }
        (
            SpecAction::AcceptStuckWorker,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Worker(worker),
            },
        ) => worker.outcome == WorkerOutcome::Stuck,
        (
            SpecAction::AcceptPaperArtifact,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Paper(_),
            },
        ) => true,
        (
            SpecAction::AcceptCorrArtifact,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Corr(_),
            },
        ) => true,
        (
            SpecAction::AcceptSoundArtifact,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Sound(_),
            },
        ) => true,
        (
            SpecAction::ReviewContinueAfterInvalid,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => state.invalid_attempt && review.decision == ReviewDecisionKind::Continue,
        (
            SpecAction::ReviewNeedInputAfterInvalid,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => state.invalid_attempt && review.decision == ReviewDecisionKind::NeedInput,
        (
            SpecAction::ReviewContinueAfterValid,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            !state.invalid_attempt
                && state.phase == trellis_kernel::Phase::TheoremStating
                && review.decision == ReviewDecisionKind::Continue
        }
        (
            SpecAction::ReviewNeedInputAfterValid,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            !state.invalid_attempt
                && state.phase == trellis_kernel::Phase::TheoremStating
                && review.decision == ReviewDecisionKind::NeedInput
        }
        (
            SpecAction::ReviewAdvancePhase,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => !state.invalid_attempt && review.decision == ReviewDecisionKind::AdvancePhase,
        (
            SpecAction::ReviewContinueProof,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            state.phase == trellis_kernel::Phase::ProofFormalization
                && review.decision == ReviewDecisionKind::Continue
        }
        (
            SpecAction::ReviewNeedInputProof,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            state.phase == trellis_kernel::Phase::ProofFormalization
                && review.decision == ReviewDecisionKind::NeedInput
        }
        (
            SpecAction::ReviewContinueCleanup,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            state.phase == trellis_kernel::Phase::Cleanup
                && review.decision == ReviewDecisionKind::Continue
        }
        (
            SpecAction::ReviewNeedInputCleanup,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            state.phase == trellis_kernel::Phase::Cleanup
                && review.decision == ReviewDecisionKind::NeedInput
        }
        (
            SpecAction::ReviewDoneCleanup,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::Review(review),
            },
        ) => {
            state.phase == trellis_kernel::Phase::Cleanup
                && review.decision == ReviewDecisionKind::Done
        }
        (
            SpecAction::HumanApproveAdvance,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(human),
            },
        ) => {
            state.gate_kind == trellis_kernel::GateKind::Advance
                && human.choice == HumanChoice::Approve
        }
        (
            SpecAction::HumanFeedbackAfterAdvance,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(human),
            },
        ) => {
            state.gate_kind == trellis_kernel::GateKind::Advance
                && human.choice == HumanChoice::Feedback
        }
        (
            SpecAction::HumanResolveNeedInput,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::HumanGate(_),
            },
        ) => state.gate_kind == trellis_kernel::GateKind::NeedInput,
        (
            SpecAction::AcceptStuckMathAuditDispatchHumanGate,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(audit),
            },
        ) => {
            audit.status == trellis_kernel::ResponseStatus::Ok
                && audit.confirm_need_input
                && state.stuck_math_audit.need_input_audit.is_some()
        }
        (
            SpecAction::AcceptStuckMathAuditBackToReviewer,
            AbstractEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(audit),
            },
        ) => {
            audit.status == trellis_kernel::ResponseStatus::Ok
                && (
                    (!audit.confirm_need_input
                        && state.stuck_math_audit.need_input_audit.is_some())
                    || (state.stuck_math_audit.need_input_audit.is_none()
                        && audit.cone_clean_node.is_none())
                )
        }
        _ => false,
    }
}

fn backfill_event_substantiveness_fps(state: &AbstractState, event: &mut AbstractEvent) {
    let snapshot = match event {
        AbstractEvent::WrapperResponse { response } => match response {
            WrapperResponse::Worker(w) => &mut w.snapshot,
            _ => return,
        },
        _ => return,
    };
    if !snapshot.substantiveness_current_fingerprints.is_empty() {
        return;
    }
    for node in &snapshot.present_nodes {
        if node.as_str() == "Preamble" {
            continue;
        }
        if let Some(fp) = state.live.substantiveness_current_fingerprints.get(node) {
            snapshot
                .substantiveness_current_fingerprints
                .insert(node.clone(), fp.clone());
        }
    }
}

// ---- partial-emit pruning --------------------------------------------------
//
// The engine serializes `AbstractState` (= `ProtocolState`) with every field
// explicit, producing ~1500 lines per step's `expected` block. The replay
// test only cares that the deserialized `AbstractState` round-trips through
// `apply_abstract_event` byte-for-byte — and `ProtocolState` carries
// `#[serde(default)]`, so missing fields fill from `Default::default()`.
//
// The helpers below convert the engine's full-state JSON to a partial that
// elides any field equal to `ProtocolState::default()` at the same path.
// The pruned form deserializes back to the same `AbstractState`. Result: each
// step's `expected` block shrinks from ~1500 lines to a few dozen — only the
// fields that the engine actually changed for this step are visible.
//
// Special cases (load-bearing):
//
//  - `in_flight_request` (full `WrapperRequest`): the replay's
//    `normalize_expected_state` re-derives this via
//    `state.expected_request(id, kind)`, so only `{id, kind, cycle}` need
//    survive in the fixture.
//
//  - `IssueRequest.request` inside `expected_commands`: same — replay's
//    `normalize_expected_commands` re-derives it via the same path.

/// Recursively prune `value` against `default`, returning `None` if the
/// pruned result is equal to `default` and therefore contributes nothing
/// to the partial. Object keys missing from `default` are kept verbatim
/// (a field present on the value side but not the default side is a
/// schema delta and must round-trip; serde-default fills the parent's
/// missing children at deserialize time).
fn prune_against_default(value: &Value, default: &Value) -> Option<Value> {
    if value == default {
        return None;
    }
    match (value, default) {
        (Value::Object(value_map), Value::Object(default_map)) => {
            let mut out: Map<String, Value> = Map::new();
            for (key, child_value) in value_map {
                let child_default = default_map
                    .get(key)
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Some(pruned) = prune_against_default(child_value, &child_default) {
                    out.insert(key.clone(), pruned);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        // Arrays / strings / numbers / null: emit verbatim if non-default.
        _ => Some(value.clone()),
    }
}

/// Build the `{id, kind, cycle}` triple that the replay test's
/// `normalize_expected_state` / `normalize_expected_commands` keys off
/// when re-deriving the full `WrapperRequest`. Strip all other fields so
/// future-schema drift in `WrapperRequest` doesn't bloat the fixture.
fn slim_wrapper_request(request_value: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = request_value.as_object() {
        for key in ["id", "kind", "cycle"] {
            if let Some(v) = obj.get(key) {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Convert the engine's full `AbstractState` JSON into a partial that
/// elides default-valued fields and slims `in_flight_request` to
/// `{id, kind, cycle}`. The replay test's normalize hooks rebuild
/// everything else from the surviving fields plus defaults.
fn slim_expected_state(state: &AbstractState) -> Value {
    let default_state = AbstractState::default();
    let state_value = serde_json::to_value(state).expect("serialize state");
    let default_value = serde_json::to_value(&default_state).expect("serialize default");
    let mut pruned =
        prune_against_default(&state_value, &default_value).unwrap_or(Value::Object(Map::new()));
    if let Value::Object(map) = &mut pruned {
        if let Some(req) = map.get("in_flight_request").cloned() {
            if !req.is_null() {
                map.insert("in_flight_request".to_string(), slim_wrapper_request(&req));
            }
        }
    }
    pruned
}

/// Convert the engine's emitted commands list into a JSON array, slimming
/// any `IssueRequest.request` payload to `{id, kind, cycle}`.
fn slim_expected_commands(
    commands: &[trellis_kernel::AbstractCommand],
) -> Value {
    let raw = serde_json::to_value(commands).expect("serialize commands");
    if let Value::Array(items) = raw {
        let slimmed: Vec<Value> = items
            .into_iter()
            .map(|item| match item {
                Value::Object(mut obj) => {
                    let is_issue_request = obj
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|s| s == "issue_request")
                        .unwrap_or(false);
                    if is_issue_request {
                        if let Some(req) = obj.get("request").cloned() {
                            obj.insert("request".to_string(), slim_wrapper_request(&req));
                        }
                    }
                    Value::Object(obj)
                }
                other => other,
            })
            .collect();
        Value::Array(slimmed)
    } else {
        raw
    }
}

/// Slim `initial` blocks the same way — they share serde-default semantics.
fn slim_initial_state(state: &AbstractState) -> Value {
    slim_expected_state(state)
}

/// Slim the fixture's `event` payload against the default for its
/// variant. `ProtocolEvent::StartCycle` collapses to the bare tag
/// `{"event": "start_cycle"}`. `WrapperResponse` events keep the
/// dispatch tags (`event` / `kind`) and prune the rest against a
/// default-constructed payload of the same kind.
fn slim_event(event: &AbstractEvent) -> Value {
    let event_value = serde_json::to_value(event).expect("serialize event");
    let default_event: AbstractEvent = match event {
        AbstractEvent::StartCycle => AbstractEvent::StartCycle,
        AbstractEvent::WrapperResponse { response } => AbstractEvent::WrapperResponse {
            response: match response {
                WrapperResponse::Worker(_) => {
                    WrapperResponse::Worker(trellis_kernel::WorkerResponse::default())
                }
                WrapperResponse::Paper(_) => {
                    WrapperResponse::Paper(trellis_kernel::PaperResponse::default())
                }
                WrapperResponse::Corr(_) => {
                    WrapperResponse::Corr(trellis_kernel::CorrResponse::default())
                }
                WrapperResponse::Sound(_) => {
                    WrapperResponse::Sound(trellis_kernel::SoundResponse::default())
                }
                WrapperResponse::Review(_) => {
                    WrapperResponse::Review(trellis_kernel::ReviewResponse::default())
                }
                WrapperResponse::HumanGate(_) => {
                    WrapperResponse::HumanGate(trellis_kernel::HumanGateResponse::default())
                }
                WrapperResponse::Audit(_) => {
                    WrapperResponse::Audit(trellis_kernel::AuditResponse::default())
                }
                WrapperResponse::StuckMathAudit(_) => WrapperResponse::StuckMathAudit(
                    trellis_kernel::StuckMathAuditResponse::default(),
                ),
            },
        },
    };
    let default_value = serde_json::to_value(&default_event).expect("serialize default event");
    let mut pruned = prune_against_default(&event_value, &default_value)
        .unwrap_or(Value::Object(Map::new()));
    // The `event` tag is the dispatch discriminant — restore it if
    // pruning elided it (StartCycle's payload is just the tag).
    if let Value::Object(map) = &mut pruned {
        if !map.contains_key("event") {
            if let Some(tag) = event_value.as_object().and_then(|m| m.get("event")).cloned() {
                map.insert("event".to_string(), tag);
            }
        }
        // For WrapperResponse events: restore the inner `kind` discriminant
        // on the nested `response` object if it got elided.
        if let Some(Value::Object(response_map)) = map.get_mut("response") {
            if !response_map.contains_key("kind") {
                if let Some(orig_kind) = event_value
                    .as_object()
                    .and_then(|m| m.get("response"))
                    .and_then(|r| r.as_object())
                    .and_then(|r| r.get("kind"))
                    .cloned()
                {
                    response_map.insert("kind".to_string(), orig_kind);
                }
            }
        }
    }
    pruned
}

// ---- regeneration ----------------------------------------------------------

/// Engine-driven regen of a single case, returning the case's `initial`
/// + a JSON array of slimmed-on-the-fly steps. Each step's `expected`
/// and `expected_commands` are pruned against `ProtocolState::default()`
/// / the slim-WrapperRequest rule (see `slim_expected_state` /
/// `slim_expected_commands`) so the resulting fixture stays roughly the
/// size of the pre-regen hand-curated baseline.
fn regenerate_one(case: TraceCase) -> Result<(String, Value, Vec<Value>), String> {
    let mut state = normalize_fixture_state(case.initial.clone());
    let mut slim_steps: Vec<Value> = Vec::with_capacity(case.steps.len());
    for (index, step) in case.steps.into_iter().enumerate() {
        if !action_matches_event(&step.action, &state, &step.event) {
            return Err(format!(
                "case={} step={} action/event mismatch: action={:?} event={:?}",
                case.name, index, step.action, step.event
            ));
        }
        let mut event = step.event.clone();
        backfill_event_substantiveness_fps(&state, &mut event);
        let outcome: AbstractTransitionOutcome =
            apply_abstract_event(state, event).map_err(|err| {
                format!(
                    "case={} step={} action={:?} apply failed: {:?}",
                    case.name, index, step.action, err
                )
            })?;
        // The regenerated step keeps the original `action` and `event`
        // (those are inputs); replaces `expected` and `expected_commands`
        // with the engine's actual outputs — slimmed to a partial form.
        let mut step_obj = Map::new();
        step_obj.insert(
            "action".to_string(),
            serde_json::to_value(&step.action).expect("serialize action"),
        );
        step_obj.insert("event".to_string(), slim_event(&step.event));
        step_obj.insert("expected".to_string(), slim_expected_state(&outcome.state));
        step_obj.insert(
            "expected_commands".to_string(),
            slim_expected_commands(&outcome.commands),
        );
        slim_steps.push(Value::Object(step_obj));
        state = outcome.state;
    }
    Ok((case.name, slim_initial_state(&case.initial), slim_steps))
}

#[test]
fn regenerate_tla_replay_fixture_when_env_set() {
    let target = match std::env::var("TRELLIS_TLA_REPLAY_REGEN") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            // The script does not opt-in to regen — skip silently. This
            // means `cargo test --test tla_replay_regen` is a no-op
            // unless explicitly driven. That's intentional.
            return;
        }
    };

    let fixture: TraceFixture =
        serde_json::from_str(include_str!("fixtures/tla_replay.json")).expect("valid fixture");

    let case_count = fixture.cases.len();
    let mut slim_cases: Vec<Value> = Vec::with_capacity(case_count);
    for case in fixture.cases {
        let (name, initial, steps) = regenerate_one(case).expect("regenerate case");
        let mut case_obj = Map::new();
        case_obj.insert("name".to_string(), Value::String(name));
        case_obj.insert("initial".to_string(), initial);
        case_obj.insert("steps".to_string(), Value::Array(steps));
        slim_cases.push(Value::Object(case_obj));
    }

    let mut root = Map::new();
    root.insert("cases".to_string(), Value::Array(slim_cases));
    let new_fixture = Value::Object(root);

    let target_path = Path::new(&target);
    let mut out = fs::File::create(target_path).expect("create regen output");
    // Pretty-print to match the existing fixture's format.
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    new_fixture.serialize(&mut ser).expect("serialize");
    out.write_all(&buffer).expect("write regen output");
    out.write_all(b"\n").expect("trailing newline");
    eprintln!(
        "regenerated {} cases → {} bytes at {}",
        case_count,
        buffer.len() + 1,
        target_path.display()
    );
}
