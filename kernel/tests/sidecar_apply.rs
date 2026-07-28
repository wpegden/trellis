//! Parallel-closure sidecar — engine apply/reject unit tests
//! (SIDECAR plan commit 3, test plan §6.1 "apply-gate rejections" at
//! the engine tier).
//!
//! Covers `ProtocolEvent::SidecarClosure`:
//!   * success apply mutates EXACTLY {`live.open_nodes`,
//!     `local_closure_records`, unverified/failure maps,
//!     `closure_provenance`} and emits exactly `CommitCheckpoint`;
//!   * every eligibility violation is a fail-loud `TransitionError`
//!     with zero state delta (apply_event returns the error before any
//!     mutation is committed to the caller);
//!   * the defensive canonical-axiom ceiling routes escapes through the
//!     failure path (mirror of the primary accept path);
//!   * the event's serde wire tag is `sidecar_closure` and round-trips.

use std::collections::BTreeMap;

use trellis_kernel::{
    apply_event, CorrStatus, LocalClosureRecord, NodeDifficulty, NodeId, NodeKind, Phase,
    ProtocolCommand, ProtocolEvent, ProtocolState, SidecarClosurePayload, Stage, TargetId,
    TransitionError,
};

fn node(id: &str) -> NodeId {
    NodeId::from(id)
}

/// A ProofFormalization state (stage Start, no in-flight request) with
/// one fully sidecar-eligible open proof node `Rung`. `node_difficulty`
/// / `easy_attempts` are pre-seeded so `ensure_node_metadata` is a
/// no-op and "mutates exactly" comparisons stay byte-tight.
fn eligible_state() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.phase = Phase::ProofFormalization;
    state.stage = Stage::Start;
    let n = node("Rung");
    state.live.present_nodes.insert(n.clone());
    state.live.open_nodes.insert(n.clone());
    state.node_kinds.insert(n.clone(), NodeKind::Proof);
    state.proof_nodes.insert(n.clone());
    state.corr_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .corr_current_fingerprints
        .insert(n.clone(), "c1".to_string());
    state
        .corr_approved_fingerprints
        .insert(n.clone(), "c1".to_string());
    state.substantiveness_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(n.clone(), "s1".to_string());
    state
        .substantiveness_approved_fingerprints
        .insert(n.clone(), "s1".to_string());
    state
        .node_difficulty
        .insert(n.clone(), NodeDifficulty::Hard);
    state.easy_attempts.insert(n.clone(), 0);
    // Queue redesign: the apply requires (and consumes) queue
    // membership; the baseline state queues Rung.
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: n.clone(),
        entry_seq: 7,
        queued_at_cycle: 0,
    });
    state.sidecar_queue_seq = 7;
    state
}

/// A complete, sentinel-free record for `Rung` with no dep references
/// and the canonical axiom set — consistent with `eligible_state()`
/// after the close.
fn record_for(nodename: &str) -> LocalClosureRecord {
    LocalClosureRecord {
        node: node(nodename),
        closure_version: "closure-v1".to_string(),
        toolchain_hash: "th".to_string(),
        lean_executable_hash: "lean-exe".to_string(),
        lake_executable_hash: "lake-exe".to_string(),
        checker_script_hash: "checker".to_string(),
        lake_manifest_hash: "lm".to_string(),
        preamble_hash: "ph".to_string(),
        approved_axioms_hash: "ah".to_string(),
        active_decl_hash: "dh".to_string(),
        active_statement_hash: "sh".to_string(),
        kernel_axioms: ["propext", "Classical.choice", "Quot.sound"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        boundary_theorems: BTreeMap::new(),
        strict_theorem_deps: BTreeMap::new(),
        strict_definition_deps: BTreeMap::new(),
        // Trust-v1 seed-support surface: this fixture has no support
        // carriers, so all three stay empty (the non-trust-v1 shape).
        seed_support_definition_deps: BTreeMap::new(),
        seed_support_evidence_root: None,
        seed_support_file_hashes: BTreeMap::new(),
        kernel_semantic_hashes: BTreeMap::new(),
        accepted_at_snapshot_id: "cycle-7".to_string(),
        axcheck_status: Default::default(),
    }
}

fn payload_for(nodename: &str) -> SidecarClosurePayload {
    SidecarClosurePayload {
        node: node(nodename),
        attempt_id: "sc-20260722-213301-Rung".to_string(),
        provider: "mistral".to_string(),
        model: "labs-leanstral-1-5".to_string(),
        wall_ms: 811_400,
        iterations: 7,
        declaration_hash_strict: "decl-strict".to_string(),
        node_file_sha256: "post-splice-sha".to_string(),
        record: record_for(nodename),
    }
}

fn sidecar_event(nodename: &str) -> ProtocolEvent {
    ProtocolEvent::SidecarClosure {
        payload: payload_for(nodename),
    }
}

#[test]
fn apply_success_mutates_exactly_the_closure_surface() {
    let state = eligible_state();
    let outcome =
        apply_event(state.clone(), sidecar_event("Rung")).expect("sidecar apply must succeed");

    assert_eq!(outcome.commands, vec![ProtocolCommand::CommitCheckpoint]);

    // Expected state = base with EXACTLY the documented mutations
    // (queue redesign: the queue entry is CONSUMED in the same
    // transition).
    let mut expected = state;
    let n = node("Rung");
    expected.sidecar_queue.clear();
    expected.live.open_nodes.remove(&n);
    expected
        .local_closure_records
        .insert(n.clone(), record_for("Rung"));
    // The same closure lands in the `committed` mirror: this transition
    // emits `CommitCheckpoint`, so the grunt's body edit is in git, and
    // `committed` is what `restore_committed` rolls a rejected burst back
    // to. Leaving it behind made the first rejected burst after a grunt
    // closure resurrect the node as open and hand its closure to the
    // next worker.
    expected.committed.open_nodes.remove(&n);
    expected
        .committed_local_closure_records
        .insert(n.clone(), record_for("Rung"));
    let cycle = expected.cycle;
    expected.closure_provenance.insert(
        n.clone(),
        trellis_kernel::NodeClosureProvenance {
            closed_by: trellis_kernel::ClosedBy::Sidecar,
            cycle,
            sidecar: Some(trellis_kernel::SidecarClosureMeta {
                attempt_id: "sc-20260722-213301-Rung".to_string(),
                provider: "mistral".to_string(),
                model: "labs-leanstral-1-5".to_string(),
                wall_ms: 811_400,
            }),
        },
    );
    assert_eq!(outcome.state, expected, "exactly the closure surface moves");
}

#[test]
fn apply_purges_unverified_and_failure_entries() {
    let mut state = eligible_state();
    let n = node("Rung");
    // A stale unverified/failure pair from an earlier probe failure
    // must be purged in the same transition (C-3 tier coverage).
    state.local_closure_unverified_nodes.insert(n.clone());
    state
        .local_closure_failures
        .insert(n.clone(), Default::default());
    let outcome = apply_event(state, sidecar_event("Rung")).expect("apply must succeed");
    assert!(!outcome.state.local_closure_unverified_nodes.contains(&n));
    assert!(!outcome.state.local_closure_failures.contains_key(&n));
    assert!(outcome.state.local_closure_records.contains_key(&n));
}

#[test]
fn axiom_escape_routes_through_failure_path() {
    let state = eligible_state();
    let mut payload = payload_for("Rung");
    payload
        .record
        .kernel_axioms
        .insert("Extra.axiom".to_string());
    let outcome = apply_event(state, ProtocolEvent::SidecarClosure { payload })
        .expect("axiom escape is failure-routed, not a TransitionError");
    let n = node("Rung");
    // Node still closes (disk truth), but the record is withheld:
    // unverified + failure entry preserve the C-3 tier invariant and
    // hand the node to the deterministic-revalidation pass.
    assert!(!outcome.state.live.open_nodes.contains(&n));
    assert!(!outcome.state.local_closure_records.contains_key(&n));
    assert!(outcome.state.local_closure_unverified_nodes.contains(&n));
    let summary = outcome
        .state
        .local_closure_failures
        .get(&n)
        .expect("failure summary installed");
    assert_eq!(summary.status, "axiom_violation");
    assert!(summary
        .axiom_violations
        .contains(&"Extra.axiom".to_string()));
    // Provenance still recorded — the closure DID apply.
    assert_eq!(
        outcome.state.closure_provenance[&n].closed_by,
        trellis_kernel::ClosedBy::Sidecar
    );
}

/// Regression (perfect run, cycles 310/311) — a grunt closure whose
/// record names ANY dep desynchronized the derived reverse indices
/// (`boundary_statement_consumers` / `strict_dep_consumers`), so
/// `apply_event`'s trailing `validate()` refused EVERY such closure with
/// `InvariantViolation("closure invariant: ... out of sync with
/// records")`. The two live rejections were shaped exactly like the two
/// helpers below: `K33MinusEdgeBipartite` carried strict definition deps
/// and no boundary theorems (so the boundary index matched and the
/// STRICT check fired), `PerfectInduce` carried boundary theorems (so
/// the BOUNDARY check fired first).
///
/// Adds a pre-existing, dep-carrying record for an unrelated closed node
/// so the indices are non-empty and IN SYNC before the event — the
/// mid-run shape, not a from-empty special case.
fn state_with_dep_bearing_neighbour() -> ProtocolState {
    let mut state = eligible_state();
    let other = node("AlreadyClosed");
    state.live.present_nodes.insert(other.clone());
    state.node_kinds.insert(other.clone(), NodeKind::Proof);
    state.proof_nodes.insert(other.clone());
    state
        .node_difficulty
        .insert(other.clone(), NodeDifficulty::Hard);
    state.easy_attempts.insert(other.clone(), 0);
    let mut neighbour = record_for("AlreadyClosed");
    neighbour
        .boundary_theorems
        .insert(node("SharedHelper"), "bh".to_string());
    neighbour
        .strict_definition_deps
        .insert(node("SharedDef"), "sd".to_string());
    state.local_closure_records.insert(other, neighbour);
    trellis_kernel::model::recompute_local_closure_reverse_indices(&mut state);
    assert_eq!(
        state.validate(),
        Ok(()),
        "baseline state must be invariant-clean before the sidecar event"
    );
    state
}

/// `K33MinusEdgeBipartite` shape: strict definition deps, no boundary
/// theorems. Pre-fix this failed with
/// `strict_dep_consumers out of sync with records`.
#[test]
fn apply_keeps_strict_dep_consumers_in_sync() {
    let state = state_with_dep_bearing_neighbour();
    let mut payload = payload_for("Rung");
    payload
        .record
        .strict_definition_deps
        .insert(node("K33Graph"), "g1".to_string());
    payload
        .record
        .strict_theorem_deps
        .insert(node("SharedHelper"), "t1".to_string());
    let outcome = apply_event(state, ProtocolEvent::SidecarClosure { payload })
        .expect("a closure whose record names strict deps must apply");
    let n = node("Rung");
    assert!(outcome.state.local_closure_records.contains_key(&n));
    assert_eq!(
        outcome.state.strict_dep_consumers.get(&node("K33Graph")),
        Some(&[n.clone()].into_iter().collect()),
        "the closed node must be indexed as a consumer of its strict deps"
    );
    assert_eq!(
        outcome.state.strict_dep_consumers.get(&node("SharedDef")),
        Some(&[node("AlreadyClosed")].into_iter().collect()),
        "the pre-existing neighbour's index entries survive"
    );
    assert_eq!(
        outcome
            .state
            .strict_dep_consumers
            .get(&node("SharedHelper")),
        Some(&[n].into_iter().collect()),
        "a dep shared with a boundary-theorem name lands in the strict index"
    );
}

/// `PerfectInduce` shape: boundary theorems (`ChromaticNumberIso` /
/// `CliqueNumIso`). Pre-fix this failed with
/// `boundary_statement_consumers out of sync with records`.
#[test]
fn apply_keeps_boundary_statement_consumers_in_sync() {
    let state = state_with_dep_bearing_neighbour();
    let mut payload = payload_for("Rung");
    payload
        .record
        .boundary_theorems
        .insert(node("ChromaticNumberIso"), "b1".to_string());
    payload
        .record
        .boundary_theorems
        .insert(node("SharedHelper"), "b2".to_string());
    let outcome = apply_event(state, ProtocolEvent::SidecarClosure { payload })
        .expect("a closure whose record names boundary theorems must apply");
    let n = node("Rung");
    assert_eq!(
        outcome
            .state
            .boundary_statement_consumers
            .get(&node("ChromaticNumberIso")),
        Some(&[n.clone()].into_iter().collect()),
    );
    assert_eq!(
        outcome
            .state
            .boundary_statement_consumers
            .get(&node("SharedHelper")),
        Some(&[node("AlreadyClosed"), n].into_iter().collect()),
        "the closed node JOINS the existing consumer set for a shared helper"
    );
}

/// The rebuild must equal the canonical recomputation for both indices,
/// which is exactly what `validate()` asserts — belt-and-braces against
/// a future hand-rolled incremental update drifting.
#[test]
fn apply_reverse_indices_equal_canonical_recomputation() {
    let state = state_with_dep_bearing_neighbour();
    let mut payload = payload_for("Rung");
    payload
        .record
        .boundary_theorems
        .insert(node("Helper"), "b".to_string());
    payload
        .record
        .strict_theorem_deps
        .insert(node("ThmDep"), "t".to_string());
    payload
        .record
        .strict_definition_deps
        .insert(node("DefDep"), "d".to_string());
    let outcome =
        apply_event(state, ProtocolEvent::SidecarClosure { payload }).expect("apply must succeed");
    let mut recomputed = outcome.state.clone();
    trellis_kernel::model::recompute_local_closure_reverse_indices(&mut recomputed);
    assert_eq!(
        outcome.state.boundary_statement_consumers,
        recomputed.boundary_statement_consumers
    );
    assert_eq!(
        outcome.state.strict_dep_consumers,
        recomputed.strict_dep_consumers
    );
}

fn assert_rejected(state: ProtocolState, event: ProtocolEvent, label: &str) {
    match apply_event(state, event) {
        Err(
            TransitionError::IllegalResponse(_)
            | TransitionError::InvariantViolation(_)
            | TransitionError::InvalidStage { .. },
        ) => {}
        other => panic!("{label}: expected fail-loud TransitionError, got {other:?}"),
    }
}

#[test]
fn apply_rejects_every_eligibility_violation() {
    let n = node("Rung");

    // Wrong stage.
    let mut state = eligible_state();
    state.stage = Stage::Worker;
    assert_rejected(state, sidecar_event("Rung"), "stage != Start");

    // In-flight request. `issue_request` mutates stage too, so set the
    // field directly via a Worker-stage issue then reset stage.
    let mut state = eligible_state();
    let request = state.issue_request(trellis_kernel::RequestKind::Worker);
    state.in_flight_request = Some(request);
    state.stage = Stage::Start;
    assert_rejected(state, sidecar_event("Rung"), "in-flight request");

    // Closed node.
    let mut state = eligible_state();
    state.live.open_nodes.remove(&n);
    assert_rejected(state, sidecar_event("Rung"), "already closed");

    // Absent node.
    let mut state = eligible_state();
    state.live.present_nodes.remove(&n);
    state.live.open_nodes.remove(&n);
    assert_rejected(state, sidecar_event("Rung"), "absent node");

    // Sketch node.
    let mut state = eligible_state();
    state.live.sketch_proof_nodes.insert(n.clone());
    assert_rejected(state, sidecar_event("Rung"), "sketch node");

    // Active node.
    let mut state = eligible_state();
    state.active_node = Some(n.clone());
    assert_rejected(state, sidecar_event("Rung"), "active node");

    // Wrong phase (window shut).
    let mut state = eligible_state();
    state.phase = Phase::Cleanup;
    state.live.open_nodes.remove(&n); // cleanup implies no open nodes
    assert_rejected(state, sidecar_event("Rung"), "cleanup phase");

    // Stating phase with the orphan-construction window OPEN.
    let mut state = eligible_state();
    state.phase = Phase::TheoremStating;
    state.configured_targets.insert(TargetId::from("t"));
    assert_rejected(state, sidecar_event("Rung"), "orphan window open");

    // Record/payload node mismatch.
    let state = eligible_state();
    let mut payload = payload_for("Rung");
    payload.record.node = node("Other");
    assert_rejected(
        state,
        ProtocolEvent::SidecarClosure { payload },
        "record node mismatch",
    );

    // Queue redesign: eligible but UNQUEUED — the membership
    // re-assertion fails loud (forged/replayed payloads for unqueued
    // nodes cannot apply).
    let mut state = eligible_state();
    state.sidecar_queue.clear();
    state.sidecar_queue_seq = 0;
    match apply_event(state, sidecar_event("Rung")) {
        Err(TransitionError::IllegalResponse(msg)) => {
            assert!(msg.contains("unqueued"), "reason names the queue gate: {msg}")
        }
        other => panic!("unqueued node: expected IllegalResponse, got {other:?}"),
    }
}

#[test]
fn stating_phase_with_covered_targets_applies() {
    let n = node("Rung");
    let mut state = eligible_state();
    state.phase = Phase::TheoremStating;
    state.configured_targets.insert(TargetId::from("t"));
    // Coverage must be derived from target claims (validate()).
    state
        .target_claims
        .insert(n.clone(), [TargetId::from("t")].into_iter().collect());
    state
        .live
        .coverage
        .insert(TargetId::from("t"), [n.clone()].into_iter().collect());
    state
        .live
        .paper_current_fingerprints
        .insert(TargetId::from("t"), "pf".to_string());
    state
        .live
        .target_fingerprints
        .insert(n.clone(), "tf".to_string());
    state.committed = state.live.clone();
    state.committed_target_claims = state.target_claims.clone();
    let outcome = apply_event(state, sidecar_event("Rung"))
        .expect("TS-phase apply with covered targets must succeed");
    assert!(!outcome.state.live.open_nodes.contains(&n));
}

#[test]
fn event_wire_tag_is_sidecar_closure_and_roundtrips() {
    let event = sidecar_event("Rung");
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], serde_json::json!("sidecar_closure"));
    assert_eq!(json["payload"]["node"], serde_json::json!("Rung"));
    let parsed: ProtocolEvent = serde_json::from_value(json).expect("deserialize");
    assert_eq!(parsed, event);

    // Old two-variant logs are unaffected: the additive variant does
    // not change the existing tags.
    let start: ProtocolEvent = serde_json::from_str(r#"{"event":"start_cycle"}"#).unwrap();
    assert_eq!(start, ProtocolEvent::StartCycle);
}

// ====================================================================
// `SidecarAttemptOutcomes` — auto-prune of SPENT queue generations
// ====================================================================

fn outcome(nodename: &str, entry_seq: u64, status: &str) -> trellis_kernel::SidecarAttemptOutcome {
    trellis_kernel::SidecarAttemptOutcome {
        node: node(nodename),
        entry_seq,
        attempt_id: format!("sc-{nodename}-{entry_seq}"),
        status: status.to_string(),
        detail: "unsolved goals".to_string(),
        source: trellis_kernel::SidecarAttemptOutcomeSource::Daemon,
    }
}

fn outcomes_event(outcomes: Vec<trellis_kernel::SidecarAttemptOutcome>) -> ProtocolEvent {
    ProtocolEvent::SidecarAttemptOutcomes {
        payload: trellis_kernel::SidecarAttemptOutcomesPayload { outcomes },
    }
}

/// The whole point of the event, and its whole blast radius: the queue
/// entry leaves, a prune-log line explains why, and NOTHING else in the
/// state moves — no close, no record, no provenance, no checkpoint.
/// This test is the stand-in for the deferred SupervisorProtocol.tla
/// parity work (deviations §30).
#[test]
fn outcome_touches_nothing_but_the_queue() {
    let state = eligible_state();
    let outcome = apply_event(state.clone(), outcomes_event(vec![outcome("Rung", 7, "failed")]))
        .expect("spent-generation expiry must apply");

    assert!(
        outcome.commands.is_empty(),
        "nothing changed on disk, so no CommitCheckpoint"
    );

    let mut expected = state;
    expected.sidecar_queue.clear();
    expected
        .sidecar_queue_prune_log
        .push(trellis_kernel::SidecarQueuePrune {
            node: node("Rung"),
            entry_seq: 7,
            cycle: expected.cycle,
            reason: "attempt_spent:failed".to_string(),
        });
    assert_eq!(
        outcome.state, expected,
        "exactly the queue entry (and its prune-log line) moves"
    );
}

/// The generation gate is the SAME one `preflight_claimed_attempt`
/// uses. A reviewer remove + re-add mints a strictly greater
/// `entry_seq`, so an outcome from the superseded generation must not
/// touch the fresh entry — and must not fail the boundary either.
#[test]
fn outcome_with_stale_generation_is_a_noop() {
    let mut state = eligible_state();
    // Remove + re-add: the fresh entry is generation 8.
    state.sidecar_queue.clear();
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: node("Rung"),
        entry_seq: 8,
        queued_at_cycle: 1,
    });
    state.sidecar_queue_seq = 8;
    let outcome = apply_event(state.clone(), outcomes_event(vec![outcome("Rung", 7, "failed")]))
        .expect("a stale generation is a no-op, never a refusal");
    assert_eq!(outcome.state, state, "the fresh generation survives intact");
    assert!(outcome.state.sidecar_queue_prune_log.is_empty());
}

/// A node that is not queued at all (the reviewer already removed it,
/// or a closure already consumed the entry) is the same no-op.
#[test]
fn outcome_for_unqueued_node_is_a_noop() {
    let mut state = eligible_state();
    state.sidecar_queue.clear();
    let outcome = apply_event(state.clone(), outcomes_event(vec![outcome("Rung", 7, "failed")]))
        .expect("an unqueued node is a no-op");
    assert_eq!(outcome.state, state);
}

/// No-op-on-mismatch is what buys idempotence: re-applying the same
/// batch (the boundary re-swept a claim after a crash) reaches the same
/// state, with no second prune-log line.
#[test]
fn outcome_is_idempotent() {
    let state = eligible_state();
    let once = apply_event(state, outcomes_event(vec![outcome("Rung", 7, "failed")]))
        .expect("first apply");
    let twice = apply_event(
        once.state.clone(),
        outcomes_event(vec![outcome("Rung", 7, "failed")]),
    )
    .expect("second apply is a no-op, not a refusal");
    assert_eq!(twice.state, once.state);
    assert_eq!(twice.state.sidecar_queue_prune_log.len(), 1);
}

/// Kernel-side rejections of a SUCCESSFUL attempt ride the same event
/// with `source: kernel_reject` and a `rejected:<gate>` status; the
/// prune reason carries the gate through to the reviewer's table.
#[test]
fn kernel_rejection_outcome_expires_with_its_gate_named() {
    let state = eligible_state();
    let mut rejection = outcome("Rung", 7, "rejected:closure_probe");
    rejection.source = trellis_kernel::SidecarAttemptOutcomeSource::KernelReject;
    let applied = apply_event(state, outcomes_event(vec![rejection])).expect("apply");
    assert!(applied.state.sidecar_queue.is_empty());
    assert_eq!(
        applied.state.sidecar_queue_prune_log[0].reason,
        "attempt_spent:rejected:closure_probe"
    );
}

/// A batch expires several generations in one transition, in payload
/// order, leaving unrelated entries alone.
#[test]
fn outcome_batch_expires_each_matching_generation() {
    let mut state = eligible_state();
    for (name, seq) in [("Other", 8u64), ("Third", 9)] {
        state.live.present_nodes.insert(node(name));
        state.live.open_nodes.insert(node(name));
        state.node_kinds.insert(node(name), NodeKind::Proof);
        state.proof_nodes.insert(node(name));
        // The statement lanes must read Pass, or the deterministic tail
        // prune drops the entry as `lane_drift` before we can see it.
        state.corr_status.insert(node(name), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(node(name), "c1".to_string());
        state
            .corr_approved_fingerprints
            .insert(node(name), "c1".to_string());
        state
            .substantiveness_status
            .insert(node(name), CorrStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node(name), "s1".to_string());
        state
            .substantiveness_approved_fingerprints
            .insert(node(name), "s1".to_string());
        state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
            node: node(name),
            entry_seq: seq,
            queued_at_cycle: 0,
        });
    }
    state.sidecar_queue_seq = 9;
    let applied = apply_event(
        state,
        outcomes_event(vec![
            outcome("Rung", 7, "failed"),
            outcome("Third", 9, "budget_exhausted"),
        ]),
    )
    .expect("batch apply");
    let remaining: Vec<&str> = applied
        .state
        .sidecar_queue
        .iter()
        .map(|entry| entry.node.as_str())
        .collect();
    assert_eq!(remaining, vec!["Other"]);
    let reasons: Vec<&str> = applied
        .state
        .sidecar_queue_prune_log
        .iter()
        .map(|prune| prune.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        vec!["attempt_spent:failed", "attempt_spent:budget_exhausted"]
    );
}

/// The prune log stays bounded and newest-last across the event, the
/// same discipline the deterministic tail prune follows (both go
/// through `record_sidecar_queue_prunes`).
#[test]
fn outcome_prune_log_stays_bounded() {
    let mut state = eligible_state();
    for index in 0..trellis_kernel::SIDECAR_QUEUE_PRUNE_LOG_MAX {
        state
            .sidecar_queue_prune_log
            .push(trellis_kernel::SidecarQueuePrune {
                node: node(&format!("Old{index}")),
                entry_seq: index as u64,
                cycle: 1,
                reason: "closed".to_string(),
            });
    }
    let applied = apply_event(state, outcomes_event(vec![outcome("Rung", 7, "failed")]))
        .expect("apply");
    assert_eq!(
        applied.state.sidecar_queue_prune_log.len(),
        trellis_kernel::SIDECAR_QUEUE_PRUNE_LOG_MAX
    );
    assert_eq!(
        applied
            .state
            .sidecar_queue_prune_log
            .last()
            .expect("newest")
            .reason,
        "attempt_spent:failed"
    );
    assert_eq!(
        applied.state.sidecar_queue_prune_log[0].node.as_str(),
        "Old1",
        "the oldest line is the one dropped"
    );
}

/// Malformed payloads fail loud: the shape is the runtime's
/// responsibility, and an empty / self-contradictory batch means the
/// producer is broken rather than that a generation is stale.
#[test]
fn malformed_outcome_payloads_fail_loud() {
    let state = eligible_state();
    match apply_event(state.clone(), outcomes_event(vec![])) {
        Err(TransitionError::IllegalResponse(msg)) => {
            assert!(msg.contains("no outcomes"), "reason names the shape: {msg}")
        }
        other => panic!("empty batch: expected IllegalResponse, got {other:?}"),
    }
    match apply_event(
        state.clone(),
        outcomes_event(vec![outcome("Rung", 7, "failed"), outcome("Rung", 7, "error")]),
    ) {
        Err(TransitionError::IllegalResponse(msg)) => {
            assert!(msg.contains("repeats generation"), "reason: {msg}")
        }
        other => panic!("duplicate generation: expected IllegalResponse, got {other:?}"),
    }
    let mut empty_node = outcome("Rung", 7, "failed");
    empty_node.node = NodeId::from("");
    match apply_event(state, outcomes_event(vec![empty_node])) {
        Err(TransitionError::IllegalResponse(msg)) => {
            assert!(msg.contains("empty node"), "reason: {msg}")
        }
        other => panic!("empty node: expected IllegalResponse, got {other:?}"),
    }
}

/// Boundary-only, exactly like the closure apply: mid-cycle the
/// reviewer may be holding a prompt that lists the entry, so an expiry
/// must never land between the prompt and the response.
#[test]
fn outcome_requires_the_quiescent_boundary() {
    let mut state = eligible_state();
    state.stage = Stage::Reviewer;
    match apply_event(state, outcomes_event(vec![outcome("Rung", 7, "failed")])) {
        Err(TransitionError::InvalidStage { .. }) => {}
        other => panic!("non-Start stage: expected InvalidStage, got {other:?}"),
    }

    let mut state = eligible_state();
    let request = state.issue_request(trellis_kernel::RequestKind::Worker);
    state.in_flight_request = Some(request);
    state.stage = Stage::Start;
    match apply_event(state, outcomes_event(vec![outcome("Rung", 7, "failed")])) {
        Err(TransitionError::InvariantViolation(msg)) => {
            assert!(msg.contains("in-flight"), "reason: {msg}")
        }
        other => panic!("in-flight request: expected InvariantViolation, got {other:?}"),
    }
}

/// Replay byte-identity: the event carries EVERYTHING the apply
/// consumes, so a round-trip through the event-log wire format
/// reproduces the identical state, byte for byte.
#[test]
fn outcome_event_replays_byte_identically() {
    let state = eligible_state();
    let event = outcomes_event(vec![
        outcome("Rung", 7, "failed"),
        {
            let mut rejection = outcome("Ghost", 3, "rejected:corr_drift");
            rejection.source = trellis_kernel::SidecarAttemptOutcomeSource::KernelReject;
            rejection
        },
    ]);
    let wire = serde_json::to_string(&event).expect("serialize");
    assert!(wire.contains(r#""event":"sidecar_attempt_outcomes""#));
    assert!(wire.contains(r#""source":"kernel_reject""#));
    let replayed: ProtocolEvent = serde_json::from_str(&wire).expect("deserialize");
    assert_eq!(replayed, event);

    let direct = apply_event(state.clone(), event).expect("direct apply");
    let from_log = apply_event(state, replayed).expect("replayed apply");
    assert_eq!(
        serde_json::to_string(&direct.state).expect("serialize direct"),
        serde_json::to_string(&from_log.state).expect("serialize replayed"),
    );
    assert_eq!(direct.commands, from_log.commands);
}

/// Pre-feature logs are unaffected by the additive variant, and a
/// payload written by an older producer (no `source`, no `detail`)
/// still deserializes — `#[serde(default)]` throughout.
#[test]
fn outcome_payload_is_serde_lenient() {
    let event: ProtocolEvent = serde_json::from_str(
        r#"{"event":"sidecar_attempt_outcomes","payload":{"outcomes":[
             {"node":"Rung","entry_seq":7,"status":"failed"}]}}"#,
    )
    .expect("lenient deserialize");
    let ProtocolEvent::SidecarAttemptOutcomes { payload } = &event else {
        panic!("wrong variant");
    };
    assert_eq!(payload.outcomes[0].detail, "");
    assert_eq!(
        payload.outcomes[0].source,
        trellis_kernel::SidecarAttemptOutcomeSource::Daemon
    );
    let applied = apply_event(eligible_state(), event).expect("apply");
    assert!(applied.state.sidecar_queue.is_empty());
}
