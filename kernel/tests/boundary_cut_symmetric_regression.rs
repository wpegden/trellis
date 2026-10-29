//! Audit H-6 — Rust-side safety-net regression tests for the
//! dual-collector boundary-cut symmetric regression. The Lean-side
//! coverage lives in `kernel/tests/local_closure_smoke.rs` and is
//! `#[ignore]`d because it requires an operator-built `.olean` tree;
//! the build/run script for that path is at
//! `scripts/run_local_closure_smoke_tests.sh`.
//!
//! This file pins the Rust-side safety net: even if the Lean script
//! regresses AND the dual collector also regresses (both walk through
//! a boundary helper's value, agree, and produce a probe envelope with
//! `axcheck.agreed = true` carrying a non-canonical kernel axiom like
//! `sorryAx`), the engine's accept-time defensive gate MUST refuse to
//! install the record. This is the load-bearing safety property that
//! prevents the symmetric-regression bug class from silently shipping.
//!
//! Background: `apply_local_closure_acceptance_bookkeeping` in
//! engine.rs computes `axiom_violations = probe.kernel_axioms -
//! ENGINE_CANONICAL_APPROVED_AXIOMS` and refuses install when the
//! violation list is non-empty, even on `status="ok" && errors=[]`.
//! This test exercises that path with an axcheck-agreed shape that the
//! Lean smoke test would otherwise be the only place to catch.

use std::collections::{BTreeMap, BTreeSet};

use trellis_kernel::backend::lean_descriptor;
use trellis_kernel::engine::{apply_event, ProtocolEvent};
use trellis_kernel::model::{
    AxiomizationCheckOutput, LocalClosureProbeOutput, LocalClosureRecord, NodeId, NodeKind, Phase,
    ProtocolState, RequestKind, ResponseStatus, RevalidationBatch, Stage, WorkerOutcome,
    WorkerResponse, WrapperResponse, WORKER_RESULT_LOCAL_CLOSURE_ISSUANCE_PREFIX,
};

fn build_proof_burst_state() -> ProtocolState {
    // Minimal ProofFormalization state with `a` as a sorryd proof node
    // about to flip sorry-free. Mirrors `engine::tests::proof_burst_state`
    // structure but reconstructed via public API.
    let mut state = ProtocolState::default();
    state.phase = Phase::ProofFormalization;
    state.stage = Stage::Worker;
    state.cycle = 5;
    state.proof_nodes = BTreeSet::from([NodeId::from("a")]);
    state.node_kinds = BTreeMap::from([
        (NodeId::from("a"), NodeKind::Proof),
        (NodeId::from("b"), NodeKind::Definition),
    ]);
    state.live.present_nodes = BTreeSet::from([NodeId::from("a"), NodeId::from("b")]);
    state.live.open_nodes = BTreeSet::from([NodeId::from("a")]);
    state.committed = state.live.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_node_kinds = state.node_kinds.clone();
    state.active_node = Some(NodeId::from("a"));
    // Issue the worker request via the public API so the in-flight
    // shape matches the kernel's normalization.
    let _ = state.issue_request(RequestKind::Worker);
    state
}

fn same_burst_record_for_engine_test(kernel_axioms: &BTreeSet<String>) -> RevalidationBatch {
    RevalidationBatch {
        refreshed: vec![(
            NodeId::from("a"),
            LocalClosureRecord {
                node: NodeId::from("a"),
                closure_version: "integration-test-complete-record".to_string(),
                kernel_axioms: kernel_axioms.clone(),
                accepted_at_snapshot_id: format!(
                    "{WORKER_RESULT_LOCAL_CLOSURE_ISSUANCE_PREFIX}5"
                ),
                ..LocalClosureRecord::default()
            },
        )],
        still_unverified: Vec::new(),
    }
}

#[test]
fn boundary_cut_symmetric_regression_engine_refuses_record_install() {
    // Audit H-6 Rust-side safety net. Constructed scenario:
    //   * Worker burst flips `a` from sorryd → sorry-free.
    //   * Probe reports `status="ok"`, `errors=[]` — no parser-side
    //     flip; the LEAN script believes the boundary cut worked.
    //   * Dual-collector axcheck reports `agreed=true, skipped=false`
    //     — BOTH collectors agree. This is the symmetric-regression
    //     shape: a buggy Lean script that walks through a boundary
    //     helper's value will surface non-canonical axioms in
    //     `kernel_axioms`, and a similarly-buggy axcheck collector
    //     will agree.
    //   * `kernel_axioms` contains `sorryAx` (the regression's
    //     fingerprint).
    //
    // The engine MUST refuse to install a record carrying sorryAx
    // even with axcheck agreement. The Lean-side
    // `local_closure_smoke_uses_helper_records_boundary` fixture
    // catches this at the Lean elaboration level; this Rust test
    // catches it at the engine accept-time level, which is the
    // last-mile defense against ALL upstream regressions.
    let state = build_proof_burst_state();
    let request_id = state.in_flight_request.as_ref().unwrap().id;

    // Snapshot moves `a` to sorry-free.
    let mut new_live = state.live.clone();
    new_live.open_nodes.clear();

    // Construct the regression-shaped probe envelope.
    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.errors.clear();
    probe.kernel_axioms.insert("propext".to_string()); // canonical
    probe.kernel_axioms.insert("sorryAx".to_string()); // the regression: non-canonical
    probe.axiomization_check = Some(AxiomizationCheckOutput {
        kernel_axioms: probe.kernel_axioms.clone(), // both collectors agreed (the regression)
        boundary_theorems: BTreeSet::new(),
        agreed: true,
        skipped: false,
        primary_only_axioms: Vec::new(),
        axcheck_only_axioms: Vec::new(),
        primary_only_boundaries: Vec::new(),
        axcheck_only_boundaries: Vec::new(),
        error: None,
    });

    let mut local_closure_results = BTreeMap::new();
    local_closure_results.insert(NodeId::from("a"), probe);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(WorkerResponse {
                request_id,
                cycle: 5,
                status: ResponseStatus::Ok,
                outcome: WorkerOutcome::Valid,
                snapshot: new_live,
                local_closure_results,
                ..WorkerResponse::default()
            }),
        },
    )
    .expect("worker delta should apply");

    // Load-bearing assertion: NO record was installed for `a`,
    // despite probe reporting status="ok" with agreed axcheck. The
    // engine's accept-time defensive ceiling
    // (`apply_local_closure_acceptance_bookkeeping`'s `axiom_violations`
    // computation against `ENGINE_CANONICAL_APPROVED_AXIOMS`) is the
    // safety net.
    assert!(
        !outcome
            .state
            .local_closure_records
            .contains_key(&NodeId::from("a")),
        "H-6 safety net: engine must refuse to install a record with `sorryAx` in kernel_axioms \
         even when axcheck agreed (boundary-cut symmetric regression). \
         If this test ever fails, the canonical accept-time ceiling has been removed or weakened — \
         the dual-collector regression class can now silently leak sorryAx through Lean → engine"
    );
    // Defensive: failure summary should classify the rejection as
    // `axiom_violation` so an operator sees the structured reason.
    let summary = outcome
        .state
        .local_closure_failures
        .get(&NodeId::from("a"))
        .expect("H-6 safety net must write a failure summary classifying the rejection");
    assert_eq!(
        summary.status, "axiom_violation",
        "H-6 safety net must classify as axiom_violation"
    );
    assert!(
        summary
            .axiom_violations
            .contains(&"sorryAx".to_string()),
        "failure summary must record sorryAx as the offending non-canonical axiom; got: {:?}",
        summary.axiom_violations
    );
    assert!(
        outcome
            .state
            .local_closure_unverified_nodes
            .contains(&NodeId::from("a")),
        "H-6 safety net: rejected node must surface in unverified set so the next \
         deterministic-revalidation pass re-probes it"
    );
}

#[test]
fn boundary_cut_passes_when_only_canonical_axioms_present() {
    // Symmetric positive control: a probe envelope whose
    // `kernel_axioms` are entirely within the canonical four AND
    // axcheck agrees, accompanied by the complete same-burst issuance record,
    // MUST install the record. Raw probe evidence alone is deliberately not a
    // record. This confirms the safety net isn't over-rejecting clean probes.
    let state = build_proof_burst_state();
    let request_id = state.in_flight_request.as_ref().unwrap().id;
    let mut new_live = state.live.clone();
    new_live.open_nodes.clear();

    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.kernel_axioms.insert("propext".to_string());
    probe.kernel_axioms.insert("Classical.choice".to_string());
    probe.axiomization_check = Some(AxiomizationCheckOutput {
        kernel_axioms: probe.kernel_axioms.clone(),
        boundary_theorems: BTreeSet::new(),
        agreed: true,
        skipped: false,
        primary_only_axioms: Vec::new(),
        axcheck_only_axioms: Vec::new(),
        primary_only_boundaries: Vec::new(),
        axcheck_only_boundaries: Vec::new(),
        error: None,
    });

    let issuance = same_burst_record_for_engine_test(&probe.kernel_axioms);
    let mut local_closure_results = BTreeMap::new();
    local_closure_results.insert(NodeId::from("a"), probe);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(WorkerResponse {
                request_id,
                cycle: 5,
                status: ResponseStatus::Ok,
                outcome: WorkerOutcome::Valid,
                snapshot: new_live,
                local_closure_results,
                local_closure_revalidation: Some(issuance),
                ..WorkerResponse::default()
            }),
        },
    )
    .expect("worker delta should apply");

    assert!(
        outcome
            .state
            .local_closure_records
            .contains_key(&NodeId::from("a")),
        "H-6 positive control: canonical probe evidence plus its complete same-burst issuance \
         MUST install a record (the safety net is not over-rejecting)"
    );
    assert!(
        !outcome
            .state
            .local_closure_unverified_nodes
            .contains(&NodeId::from("a")),
        "successful install must remove the node from unverified"
    );
}

#[test]
fn boundary_cut_engine_refuses_record_even_when_axcheck_skipped() {
    // Defensive variant: when axcheck is `skipped` (operator turned
    // it off via env var / CLI flag), the engine's
    // canonical-axiom ceiling MUST still apply. Without the axcheck
    // layer the runtime has fewer signals; the engine accept-time
    // gate is the only defense, and it must hold.
    let state = build_proof_burst_state();
    let request_id = state.in_flight_request.as_ref().unwrap().id;
    let mut new_live = state.live.clone();
    new_live.open_nodes.clear();

    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.kernel_axioms.insert("Lean.ofReduceBool".to_string()); // non-canonical
    probe.axiomization_check = Some(AxiomizationCheckOutput {
        kernel_axioms: BTreeSet::new(),
        boundary_theorems: BTreeSet::new(),
        agreed: true, // vacuously true when skipped
        skipped: true,
        primary_only_axioms: Vec::new(),
        axcheck_only_axioms: Vec::new(),
        primary_only_boundaries: Vec::new(),
        axcheck_only_boundaries: Vec::new(),
        error: None,
    });

    let mut local_closure_results = BTreeMap::new();
    local_closure_results.insert(NodeId::from("a"), probe);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(WorkerResponse {
                request_id,
                cycle: 5,
                status: ResponseStatus::Ok,
                outcome: WorkerOutcome::Valid,
                snapshot: new_live,
                local_closure_results,
                ..WorkerResponse::default()
            }),
        },
    )
    .expect("worker delta should apply");

    assert!(
        !outcome
            .state
            .local_closure_records
            .contains_key(&NodeId::from("a")),
        "H-6 safety net: engine must refuse non-canonical axioms even when axcheck is skipped \
         (operator turned off the dual collector — the engine ceiling is the only defense left)"
    );
    let summary = outcome
        .state
        .local_closure_failures
        .get(&NodeId::from("a"))
        .expect("failure summary must be written");
    assert_eq!(summary.status, "axiom_violation");
}

// ----- Phase II step 6: gate reads the approved-axiom floor from the
// active backend descriptor -----

#[test]
fn gate_rejects_sorry_ax_reading_from_descriptor() {
    // Step-6 pin: the engine accept-time gate now reads its approved-axiom
    // floor from `lean_descriptor().approved_axioms_floor`. This pins both
    // (a) the floor's membership read from the descriptor — it must contain
    // the canonical four and must NOT contain `sorryAx`; and (b) the gate's
    // load-bearing refusal — a probe carrying `sorryAx` installs no record
    // and is classified `axiom_violation`. Together these prove the
    // descriptor-sourced floor still rejects the regression's fingerprint.
    let floor = lean_descriptor().approved_axioms_floor;
    assert!(
        !floor.contains(&"sorryAx"),
        "the descriptor floor must NOT bless sorryAx; got {:?}",
        floor,
    );
    for canonical in ["propext", "funext", "Classical.choice", "Quot.sound"] {
        assert!(
            floor.contains(&canonical),
            "the descriptor floor must contain canonical axiom {canonical}; got {:?}",
            floor,
        );
    }

    let state = build_proof_burst_state();
    let request_id = state.in_flight_request.as_ref().unwrap().id;
    let mut new_live = state.live.clone();
    new_live.open_nodes.clear();

    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.errors.clear();
    probe.kernel_axioms.insert("propext".to_string()); // canonical
    probe.kernel_axioms.insert("sorryAx".to_string()); // not on the descriptor floor

    let mut local_closure_results = BTreeMap::new();
    local_closure_results.insert(NodeId::from("a"), probe);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(WorkerResponse {
                request_id,
                cycle: 5,
                status: ResponseStatus::Ok,
                outcome: WorkerOutcome::Valid,
                snapshot: new_live,
                local_closure_results,
                ..WorkerResponse::default()
            }),
        },
    )
    .expect("worker delta should apply");

    assert!(
        !outcome
            .state
            .local_closure_records
            .contains_key(&NodeId::from("a")),
        "step-6: the descriptor-sourced floor must still refuse to install a record \
         carrying sorryAx",
    );
    let summary = outcome
        .state
        .local_closure_failures
        .get(&NodeId::from("a"))
        .expect("step-6: a failure summary must classify the rejection");
    assert_eq!(
        summary.status, "axiom_violation",
        "step-6: rejection must be classified axiom_violation",
    );
    assert!(
        summary.axiom_violations.contains(&"sorryAx".to_string()),
        "step-6: the failure summary must name sorryAx; got {:?}",
        summary.axiom_violations,
    );
}

#[test]
fn gate_accepts_record_using_only_canonical_four() {
    // Step-6 positive companion (NEW): a probe whose `kernel_axioms` are
    // exactly the four axioms on the descriptor floor MUST install a record.
    // This is the affirmative side of the descriptor-floor rewrite —
    // confirming the floor read from `lean_descriptor()` still admits the
    // full canonical set (the existing positive control only exercises two
    // of the four). Guards against an accidental narrowing of the floor.
    let floor = lean_descriptor().approved_axioms_floor;

    let state = build_proof_burst_state();
    let request_id = state.in_flight_request.as_ref().unwrap().id;
    let mut new_live = state.live.clone();
    new_live.open_nodes.clear();

    let mut probe = LocalClosureProbeOutput::default();
    probe.status = "ok".to_string();
    probe.errors.clear();
    for axiom in ["propext", "funext", "Classical.choice", "Quot.sound"] {
        probe.kernel_axioms.insert(axiom.to_string());
    }
    // Sanity: every axiom we install is on the descriptor floor.
    for axiom in &probe.kernel_axioms {
        assert!(
            floor.contains(&axiom.as_str()),
            "test precondition: {axiom} must be on the descriptor floor",
        );
    }

    let issuance = same_burst_record_for_engine_test(&probe.kernel_axioms);
    let mut local_closure_results = BTreeMap::new();
    local_closure_results.insert(NodeId::from("a"), probe);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(WorkerResponse {
                request_id,
                cycle: 5,
                status: ResponseStatus::Ok,
                outcome: WorkerOutcome::Valid,
                snapshot: new_live,
                local_closure_results,
                local_closure_revalidation: Some(issuance),
                ..WorkerResponse::default()
            }),
        },
    )
    .expect("worker delta should apply");

    assert!(
        outcome
            .state
            .local_closure_records
            .contains_key(&NodeId::from("a")),
        "step-6 positive: canonical-four probe evidence plus its complete same-burst \
         issuance MUST install a record",
    );
    assert!(
        !outcome
            .state
            .local_closure_failures
            .contains_key(&NodeId::from("a")),
        "step-6 positive: a clean canonical-four probe must write no failure summary",
    );
    assert!(
        !outcome
            .state
            .local_closure_unverified_nodes
            .contains(&NodeId::from("a")),
        "step-6 positive: successful install must remove the node from unverified",
    );
}
