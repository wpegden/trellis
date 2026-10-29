//! Generic conditional theorem protocol: immutable proposal generations,
//! correspondence authorization, ratification, activation, and completion.

use std::collections::BTreeSet;

use trellis_kernel::engine::{
    apply_conditional_activation, apply_conditional_proposal_carrier,
    bind_conditional_ratification, invalidate_stale_closed_conditionals,
    refresh_conditional_closures,
};
use trellis_kernel::trust_base::{
    build_conditional_correspondence_request, conditional_activation_payload,
    conditional_ratification_packet_digest_for_approved, raw_sha256,
    seal_conditional_theorem, stamp_conditional_proposal, terminal_outcome,
    validate_conditional_protocol_state, ConditionalAssumptionSnapshot,
    ConditionalCandidateGeneration, ConditionalCorrespondenceVerdict, ConditionalDisposition,
    ConditionalEvidenceReferences, ConditionalObligation, ConditionalStage,
    ConditionalTheoremProposal,
    ConditionalTriggerClassification, Sha256Digest, TerminalOutcome,
};
use trellis_kernel::{
    apply_event, refutation_target_id, ChallengeResolution, ChallengeTargetId,
    ChallengeTargetKind, ChallengeTargetSpec, GateKind, HumanChoice, HumanGateResponse,
    LocalClosureRecord, NodeId, NodeKind, Phase, ProtocolEvent, ProtocolState, RequestKind,
    ResponseStatus, Stage, StatementProvenance, TrustBaseMode, WrapperResponse,
};

const GATE: &str = "conditional-test-gate";

fn spec(name: &str, lean: &str, resolution: ChallengeResolution) -> ChallengeTargetSpec {
    ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem,
        name: name.into(),
        lean: lean.into(),
        resolution,
        statement_provenance: StatementProvenance::KernelDerived,
        ..ChallengeTargetSpec::default()
    }
}

fn state_with_pair(lean: &str) -> (ProtocolState, ChallengeTargetId) {
    let mut state = ProtocolState::default();
    state.trust_base.mode = TrustBaseMode::RequiredV1;
    state.trust_base.format_version = 2;
    state.trust_base.advance_gate_episode_id = Some(GATE.into());
    let target = ChallengeTargetId::from("goal:generic");
    let twin = refutation_target_id(&target);
    state.configured_challenge_targets.insert(
        target.clone(),
        spec("Original", lean, ChallengeResolution::Decide),
    );
    state.configured_challenge_targets.insert(
        twin,
        spec(
            "Original__Refutation",
            "theorem Original__Refutation : False := by",
            ChallengeResolution::Prove,
        ),
    );
    for name in ["Original", "Original__Refutation"] {
        let node = NodeId::from(name);
        state.live.present_nodes.insert(node.clone());
        state.live.open_nodes.insert(node.clone());
        state.committed.present_nodes.insert(node.clone());
        state.committed.open_nodes.insert(node.clone());
        state.proof_nodes.insert(node.clone());
        state.committed_proof_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), NodeKind::Proof);
        state.committed_node_kinds.insert(node, NodeKind::Proof);
    }
    (state, target)
}

fn proposal(
    target: &ChallengeTargetId,
    condition: &str,
    arguments: Option<Vec<&str>>,
) -> ConditionalTheoremProposal {
    ConditionalTheoremProposal {
        target_id: target.clone(),
        condition_lean: condition.into(),
        condition_informal: "the declared input lies in the supported domain".into(),
        rationale: "the unrestricted statement is not established".into(),
        trigger: ConditionalTriggerClassification::UnconditionalNotEstablished,
        evidence: ConditionalEvidenceReferences::default(),
        concrete_counterexample_arguments: arguments.map(|items| {
            items.into_iter().map(str::to_owned).collect()
        }),
        ..ConditionalTheoremProposal::default()
    }
}

fn pass_correspondence(
    state: &mut ProtocolState,
    proposal: ConditionalTheoremProposal,
    snapshots: Vec<ConditionalAssumptionSnapshot>,
) -> ConditionalCandidateGeneration {
    let target = proposal.target_id.clone();
    apply_conditional_proposal_carrier(state, Some(proposal), &BTreeSet::new(), snapshots)
        .expect("proposal is stamped");
    let current = state.trust_base.conditional_candidates[&target].clone();
    assert_eq!(current.stage, ConditionalStage::Proposed);
    let request = current.correspondence_request.clone().unwrap();
    let verdict = ConditionalCorrespondenceVerdict {
        request_sha256: request.request_sha256,
        proposal_sha256: current.stamped.proposal_sha256,
        boundary_expression_correct: true,
        condition_relevant: true,
        realizable_non_vacuous: true,
        obligation_preserved_on_domain: true,
        rust_axioms_backed_by_approved_assumptions: true,
        findings: "the exact condition is a relevant, inhabited restriction".into(),
    };
    let (approval, sealed) = seal_conditional_theorem(
        state,
        &current.stamped,
        &request,
        &verdict,
    )
    .expect("correspondence pass seals");
    let candidate = state.trust_base.conditional_candidates.get_mut(&target).unwrap();
    candidate.correspondence = Some(approval);
    candidate.sealed = Some(sealed);
    candidate.ratification_gate_episode_id = Some(GATE.into());
    candidate.stage = ConditionalStage::CorrespondencePass;
    candidate.clone()
}

fn ratify_and_activate(state: &mut ProtocolState, target: &ChallengeTargetId) {
    let approval = raw_sha256(b"conditional human ratification");
    state.trust_base.current_human_approval_event_hash = Some(approval);
    assert!(bind_conditional_ratification(state, GATE, approval).unwrap());
    assert_eq!(
        state.trust_base.conditional_candidates[target].stage,
        ConditionalStage::HumanApproved
    );
    let persisted = serde_json::to_vec(state).expect("serialize ratified state");
    *state = serde_json::from_slice(&persisted).expect("restart from ratified state");
    assert!(validate_conditional_protocol_state(state).is_ok());
    let payload = conditional_activation_payload(&state.trust_base.conditional_candidates[target])
        .expect("ratified packet activates");
    let encoded = serde_json::to_vec(&ProtocolEvent::ConditionalActivation {
        payload: payload.clone(),
    })
    .unwrap();
    let replayed: ProtocolEvent = serde_json::from_slice(&encoded).unwrap();
    let ProtocolEvent::ConditionalActivation { payload: replayed } = replayed else {
        panic!("event changed variant during replay")
    };
    assert_eq!(payload, replayed);
    apply_conditional_activation(state, &replayed).expect("activation registers the seal");
    assert_eq!(
        state.trust_base.conditional_candidates[target].stage,
        ConditionalStage::Open
    );
    assert!(apply_conditional_activation(state, &replayed).is_err());
}

fn close_all_conditional_obligations(state: &mut ProtocolState, target: &ChallengeTargetId) {
    let sealed = state.trust_base.conditional_candidates[target]
        .sealed
        .clone()
        .unwrap();
    let mut obligations = vec![sealed.proof, sealed.inhabited];
    obligations.extend(sealed.counterexample_excluded);
    for obligation in obligations {
        install_test_closure(state, &obligation, BTreeSet::new());
    }
    refresh_conditional_closures(state);
}

fn install_test_closure(
    state: &mut ProtocolState,
    obligation: &ConditionalObligation,
    kernel_axioms: BTreeSet<String>,
) {
    state.live.open_nodes.remove(&obligation.node);
    state.committed.open_nodes.remove(&obligation.node);
    state.local_closure_records.insert(
        obligation.node.clone(),
        LocalClosureRecord {
            node: obligation.node.clone(),
            closure_version: "conditional-integration-v1".into(),
            toolchain_hash: "toolchain".into(),
            lean_executable_hash: "lean".into(),
            lake_executable_hash: "lake".into(),
            checker_script_hash: "checker".into(),
            lake_manifest_hash: "manifest".into(),
            preamble_hash: "preamble".into(),
            approved_axioms_hash: "approved".into(),
            active_decl_hash: "declaration".into(),
            active_statement_hash: "statement".into(),
            kernel_axioms,
            ..LocalClosureRecord::default()
        },
    );
}

#[test]
fn arbitrary_and_dependent_binders_seal_exact_parenthesized_shapes() {
    let shapes = [
        (
            "theorem Original : True ↔ True := by",
            "1 < 2",
            "theorem TrellisConditional_",
            " : (1 < 2) → (True ↔ True) := by",
            " : (1 < 2) := by",
        ),
        (
            "theorem Original (n : Nat) : n = n := by",
            "n ≤ 8",
            "(n : Nat) : (n ≤ 8) → (n = n) := by",
            "",
            "∃ (n : Nat), (n ≤ 8)",
        ),
        (
            "theorem Original {α : Type} (x : α) (f : α → α) : f x = f x := by",
            "x = x ∧ f x = f x",
            "{α : Type} (x : α) (f : α → α) : (x = x ∧ f x = f x) → (f x = f x) := by",
            "",
            "∃ (α : Type), ∃ (x : α), ∃ (f : α → α), (x = x ∧ f x = f x)",
        ),
        (
            "theorem Original ⦃n : Nat⦄ [inst : Inhabited Nat] (v : Fin (n + 1)) : v = v := by",
            "n < 10 ∧ v = v",
            "⦃n : Nat⦄ [inst : Inhabited Nat] (v : Fin (n + 1)) : (n < 10 ∧ v = v) → (v = v) := by",
            "",
            "∃ (n : Nat), ∃ (inst : Inhabited Nat), letI := inst; ∃ (v : Fin (n + 1)), (n < 10 ∧ v = v)",
        ),
        (
            "theorem Original : ∀ (n : Nat), ∀ {{v : Fin (n + 1)}}, v = v := by",
            "n < 4 ∧ v = v",
            "(n : Nat) {{v : Fin (n + 1)}} : (n < 4 ∧ v = v) → (v = v) := by",
            "",
            "∃ (n : Nat), ∃ (v : Fin (n + 1)), (n < 4 ∧ v = v)",
        ),
    ];
    for (lean, condition, proof_contains, proof_tail, inhabited_contains) in shapes {
        let (state, target) = state_with_pair(lean);
        let stamped = stamp_conditional_proposal(
            &state,
            proposal(&target, condition, None),
            1,
            Vec::new(),
        )
        .unwrap();
        let request = build_conditional_correspondence_request(
            &state,
            &stamped,
            serde_json::json!({"source": "fixture"}),
            serde_json::Value::Null,
        )
        .unwrap();
        let verdict = ConditionalCorrespondenceVerdict {
            request_sha256: request.request_sha256,
            proposal_sha256: stamped.proposal_sha256,
            boundary_expression_correct: true,
            condition_relevant: true,
            realizable_non_vacuous: true,
            obligation_preserved_on_domain: true,
            rust_axioms_backed_by_approved_assumptions: true,
            findings: "exact generic shape".into(),
        };
        let (_, sealed) = seal_conditional_theorem(&state, &stamped, &request, &verdict).unwrap();
        assert!(sealed.proof.statement_lean.contains(proof_contains));
        assert!(sealed.proof.statement_lean.contains(proof_tail));
        assert!(sealed.inhabited.statement_lean.contains(inhabited_contains));
        assert!(!sealed.proof.statement_lean.contains("Original"));
        assert!(!sealed.proof.statement_lean.contains("import Tablet.Original"));
    }
}

#[test]
fn agent_proposal_and_human_ratification_have_disjoint_bound_shapes() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    let carrier = proposal(&target, "n < 8", None);
    let proposal_value = serde_json::to_value(&carrier).unwrap();
    let proposal_keys: BTreeSet<&str> = proposal_value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        proposal_keys,
        BTreeSet::from([
            "condition_informal",
            "condition_lean",
            "evidence",
            "rationale",
            "target_id",
            "trigger",
        ])
    );
    for forbidden in [
        "binders",
        "comparison",
        "witness_path",
        "bound",
        "theorem_name",
        "target_suffix",
        "sealed_statement",
    ] {
        assert!(proposal_value.get(forbidden).is_none());
    }

    pass_correspondence(&mut state, carrier, Vec::new());
    let approval = raw_sha256(b"shape ratification");
    state.trust_base.current_human_approval_event_hash = Some(approval);
    assert!(bind_conditional_ratification(&mut state, GATE, approval).unwrap());
    let ratification = state.trust_base.conditional_candidates[&target]
        .ratification
        .as_ref()
        .unwrap();
    let ratification_value = serde_json::to_value(ratification).unwrap();
    let keys: BTreeSet<&str> = ratification_value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "approval_record_sha256",
            "condition_sha256",
            "correspondence_sha256",
            "evidence_set_sha256",
            "gate_episode_id",
            "generation",
            "proposal_sha256",
            "seal_sha256",
            "target_id",
            "target_statement_sha256",
        ])
    );
}

#[test]
fn concrete_counterexample_seals_checked_substitution_including_binder_free() {
    let (mut state, target) = state_with_pair(
        "theorem Original (n : Nat) (v : Fin (n + 1)) : v = v := by",
    );
    let candidate = pass_correspondence(
        &mut state,
        proposal(&target, "n ≠ 0 ∧ v = v", Some(vec!["0", "0"])),
        Vec::new(),
    );
    let exclusion = candidate.sealed.unwrap().counterexample_excluded.unwrap();
    assert!(exclusion.statement_lean.contains(
        "fun (n : Nat) (v : Fin (n + 1)) => (n ≠ 0 ∧ v = v); @predicate 0 0"
    ));

    let (state, target) = state_with_pair("theorem Original : True := by");
    let stamped = stamp_conditional_proposal(
        &state,
        proposal(&target, "Nat.succ 0 = 1", Some(Vec::new())),
        1,
        Vec::new(),
    )
    .unwrap();
    let request = build_conditional_correspondence_request(
        &state,
        &stamped,
        serde_json::Value::Null,
        serde_json::Value::Null,
    )
    .unwrap();
    let verdict = ConditionalCorrespondenceVerdict {
        request_sha256: request.request_sha256,
        proposal_sha256: stamped.proposal_sha256,
        boundary_expression_correct: true,
        condition_relevant: true,
        realizable_non_vacuous: true,
        obligation_preserved_on_domain: true,
        rust_axioms_backed_by_approved_assumptions: true,
        findings: "binder-free exclusion".into(),
    };
    let (_, sealed) = seal_conditional_theorem(&state, &stamped, &request, &verdict).unwrap();
    assert!(sealed
        .counterexample_excluded
        .unwrap()
        .statement_lean
        .contains(": ¬ ((Nat.succ 0 = 1)) := by"));
}

#[test]
fn stale_target_proposal_lane_gate_and_activation_hashes_refuse() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 8", None)),
        &BTreeSet::new(),
        Vec::new(),
    )
    .unwrap();
    let current = state.trust_base.conditional_candidates[&target].clone();
    let request = current.correspondence_request.clone().unwrap();
    let good = ConditionalCorrespondenceVerdict {
        request_sha256: request.request_sha256,
        proposal_sha256: current.stamped.proposal_sha256,
        boundary_expression_correct: true,
        condition_relevant: true,
        realizable_non_vacuous: true,
        obligation_preserved_on_domain: true,
        rust_axioms_backed_by_approved_assumptions: true,
        findings: "bound".into(),
    };
    let mut changed_target = state.clone();
    changed_target
        .configured_challenge_targets
        .get_mut(&target)
        .unwrap()
        .lean = "theorem Original (n : Nat) : n + 0 = n := by".into();
    assert!(seal_conditional_theorem(
        &changed_target,
        &current.stamped,
        &request,
        &good
    )
    .is_err());
    let mut changed_proposal = request.clone();
    changed_proposal.proposal.proposal.rationale.push_str(" stale");
    assert!(seal_conditional_theorem(&state, &current.stamped, &changed_proposal, &good).is_err());
    let mut changed_lane = good.clone();
    changed_lane.request_sha256 = raw_sha256(b"other request");
    assert!(seal_conditional_theorem(&state, &current.stamped, &request, &changed_lane).is_err());
    let mut failing_verdict = good.clone();
    failing_verdict.obligation_preserved_on_domain = false;
    assert!(
        seal_conditional_theorem(&state, &current.stamped, &request, &failing_verdict)
            .unwrap_err()
            .contains("did not pass")
    );

    pass_correspondence(
        &mut state,
        proposal(&target, "n < 8", None),
        Vec::new(),
    );
    assert!(!bind_conditional_ratification(
        &mut state,
        "different-gate",
        raw_sha256(b"approval")
    )
    .unwrap());
    let approval = raw_sha256(b"approval");
    state.trust_base.current_human_approval_event_hash = Some(approval);
    bind_conditional_ratification(&mut state, GATE, approval).unwrap();
    let mut activation =
        conditional_activation_payload(&state.trust_base.conditional_candidates[&target]).unwrap();
    let mut stale_gate = activation.clone();
    stale_gate.approval_record_sha256 = raw_sha256(b"stale approval");
    assert!(apply_conditional_activation(&mut state, &stale_gate).is_err());
    activation.seal_sha256 = Sha256Digest::ZERO;
    assert!(apply_conditional_activation(&mut state, &activation).is_err());
}

#[test]
fn stages_are_monotone_within_generation_and_revision_retires_authority() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    let mut observed = vec![ConditionalStage::None];
    let candidate = pass_correspondence(
        &mut state,
        proposal(&target, "n < 8", None),
        Vec::new(),
    );
    observed.extend([ConditionalStage::Proposed, candidate.stage]);
    ratify_and_activate(&mut state, &target);
    observed.extend([ConditionalStage::HumanApproved, ConditionalStage::Open]);
    state.live.open_nodes.remove(&NodeId::from("Original"));
    close_all_conditional_obligations(&mut state, &target);
    assert_eq!(
        state.trust_base.conditional_candidates[&target].stage,
        ConditionalStage::Open,
        "closing an unconditional side cannot complete the conditional"
    );
    state.live.open_nodes.insert(NodeId::from("Original"));
    refresh_conditional_closures(&mut state);
    observed.push(state.trust_base.conditional_candidates[&target].stage);
    assert!(observed.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(validate_conditional_protocol_state(&state).is_ok());
    assert!(matches!(
        terminal_outcome(&state, &target).unwrap(),
        TerminalOutcome::ConditionalTheorem { .. }
    ));
    assert!(state.live.open_nodes.contains(&NodeId::from("Original")));
    assert!(state
        .live
        .open_nodes
        .contains(&NodeId::from("Original__Refutation")));

    let old_seal = state.trust_base.conditional_candidates[&target]
        .sealed
        .as_ref()
        .unwrap();
    let mut old_obligations = vec![old_seal.proof.node.clone(), old_seal.inhabited.node.clone()];
    old_obligations.extend(
        old_seal
            .counterexample_excluded
            .as_ref()
            .map(|obligation| obligation.node.clone()),
    );
    let closed_supersede = apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 4", None)),
        &BTreeSet::new(),
        Vec::new(),
    );
    assert!(format!("{:?}", closed_supersede.unwrap_err()).contains("Closed conditional"));
    assert_eq!(state.trust_base.conditional_candidates[&target].stage, ConditionalStage::Closed);
    for node in &old_obligations {
        assert!(state.live.present_nodes.contains(node));
    }
    apply_conditional_proposal_carrier(
        &mut state,
        None,
        &BTreeSet::from([target.clone()]),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        state.trust_base.conditional_candidates[&target].disposition,
        Some(ConditionalDisposition::Withdrawn)
    );
    apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 2", None)),
        &BTreeSet::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        state.trust_base.retired_conditional_candidates[0].disposition,
        Some(ConditionalDisposition::Withdrawn),
        "retirement must preserve an irreversible generation disposition"
    );
    assert_eq!(
        state.trust_base.conditional_candidates[&target]
            .stamped
            .generation,
        2
    );
}

#[test]
fn syntactically_vacuous_lean_conditions_are_rejected_at_proposal() {
    let (state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    for condition in ["True", "False", "(True)", "n = n", "n ↔ n"] {
        let error = stamp_conditional_proposal(
            &state, proposal(&target, condition, None), 1, Vec::new(),
        ).unwrap_err();
        assert!(error.contains("invalid or undocumented condition"), "{condition}: {error}");
    }
}

#[test]
fn condition_inhabited_closure_is_independently_required() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    pass_correspondence(
        &mut state,
        proposal(&target, "n < 8", None),
        Vec::new(),
    );
    ratify_and_activate(&mut state, &target);
    let sealed = state.trust_base.conditional_candidates[&target]
        .sealed
        .clone()
        .unwrap();
    install_test_closure(&mut state, &sealed.proof, BTreeSet::new());
    refresh_conditional_closures(&mut state);
    assert_eq!(
        state.trust_base.conditional_candidates[&target].stage,
        ConditionalStage::Open
    );
    install_test_closure(&mut state, &sealed.inhabited, BTreeSet::new());
    refresh_conditional_closures(&mut state);
    assert_eq!(
        state.trust_base.conditional_candidates[&target].stage,
        ConditionalStage::Closed
    );
}

#[test]
fn inhabitedness_and_counterexample_closure_are_required_and_revocation_invalidates() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    let assumption = ConditionalAssumptionSnapshot {
        id: "rust-domain".into(),
        axiom_name: "RustModel.domain_fact".into(),
        status: "approved".into(),
        record: serde_json::json!({
            "classification": "rust-language-assumption",
            "id": "rust-domain",
            "name": "RustModel.domain_fact",
        }),
        record_sha256: Sha256Digest::ZERO,
    };
    let assumption = ConditionalAssumptionSnapshot {
        record_sha256: raw_sha256(
            &trellis_kernel::trust_base::canonical_json(&assumption.record).unwrap(),
        ),
        ..assumption
    };
    let mut item = proposal(&target, "n ≠ 0", Some(vec!["0"]));
    item.evidence.assumption_ids.insert(assumption.id.clone());
    let authorized = pass_correspondence(&mut state, item, vec![assumption.clone()]);
    assert_eq!(
        authorized
            .correspondence_request
            .as_ref()
            .unwrap()
            .proposal
            .assumption_snapshots[0]
            .record,
        assumption.record
    );
    ratify_and_activate(&mut state, &target);
    let sealed = state.trust_base.conditional_candidates[&target]
        .sealed
        .clone()
        .unwrap();

    install_test_closure(
        &mut state,
        &sealed.proof,
        BTreeSet::from([assumption.axiom_name.clone()]),
    );
    install_test_closure(&mut state, &sealed.inhabited, BTreeSet::new());
    refresh_conditional_closures(&mut state);
    assert_eq!(state.trust_base.conditional_candidates[&target].stage, ConditionalStage::Open);
    let excluded = sealed.counterexample_excluded.as_ref().unwrap();
    install_test_closure(&mut state, excluded, BTreeSet::new());
    refresh_conditional_closures(&mut state);
    assert_eq!(state.trust_base.conditional_candidates[&target].stage, ConditionalStage::Closed);

    state.live.open_nodes.insert(sealed.proof.node.clone());
    state.local_closure_records.remove(&sealed.proof.node);
    invalidate_stale_closed_conditionals(&mut state);
    assert_eq!(
        state.trust_base.conditional_candidates[&target].disposition,
        Some(ConditionalDisposition::Rejected)
    );
    assert!(terminal_outcome(&state, &target).is_err());
}

/// A Decide pair keeps one side dormant. The dormant twin is not present on
/// the tablet, and nothing about it is established, so it counts as open: a
/// conditional theorem may be proposed beside it. A side that is present and
/// closed still refuses the proposal.
#[test]
fn dormant_refutation_twin_counts_as_open_for_a_proposal() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    let twin = NodeId::from("Original__Refutation");
    state.live.present_nodes.remove(&twin);
    state.live.open_nodes.remove(&twin);
    state.committed.present_nodes.remove(&twin);
    state.committed.open_nodes.remove(&twin);
    apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 8", Some(vec!["9"]))),
        &BTreeSet::new(),
        Vec::new(),
    )
    .expect("a dormant twin leaves the unconditional side open");
    assert_eq!(
        state.trust_base.conditional_candidates[&target].stage,
        ConditionalStage::Proposed
    );

    let (mut closed, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    closed.live.open_nodes.remove(&NodeId::from("Original"));
    let err = apply_conditional_proposal_carrier(
        &mut closed,
        Some(proposal(&target, "n < 8", None)),
        &BTreeSet::new(),
        Vec::new(),
    )
    .expect_err("a present, closed side refuses the proposal");
    assert!(format!("{err:?}").contains("both unconditional sides"), "{err:?}");
}

/// A namespaced declaration `NS.Original` lives as the node `NS_Original`; the
/// sides-open check resolves it the way the flip mutator does, so a present,
/// closed namespaced side refuses the proposal and a dormant one admits it.
#[test]
fn namespaced_sides_resolve_to_their_sanitized_nodes() {
    let mut state = ProtocolState::default();
    state.trust_base.mode = TrustBaseMode::RequiredV1;
    state.trust_base.format_version = 2;
    state.trust_base.advance_gate_episode_id = Some(GATE.into());
    let target = ChallengeTargetId::from("goal:namespaced");
    let twin = refutation_target_id(&target);
    state.configured_challenge_targets.insert(
        target.clone(),
        spec("NS.Original", "theorem NS.Original (n : Nat) : n = n := by", ChallengeResolution::Decide),
    );
    state.configured_challenge_targets.insert(
        twin,
        spec("NS.Original__Refutation", "theorem NS.Original__Refutation : False := by", ChallengeResolution::Prove),
    );
    for name in ["NS_Original", "NS_Original__Refutation"] {
        let node = NodeId::from(name);
        state.live.present_nodes.insert(node.clone());
        state.committed.present_nodes.insert(node.clone());
        state.proof_nodes.insert(node.clone());
        state.committed_proof_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), NodeKind::Proof);
        state.committed_node_kinds.insert(node, NodeKind::Proof);
    }
    // The primary is open; the refutation twin is present and CLOSED.
    state.live.open_nodes.insert(NodeId::from("NS_Original"));
    state.committed.open_nodes.insert(NodeId::from("NS_Original"));
    let err = apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 8", None)),
        &BTreeSet::new(),
        Vec::new(),
    )
    .expect_err("a closed namespaced twin refuses the proposal");
    assert!(format!("{err:?}").contains("both unconditional sides"), "{err:?}");

    // Dormant twin instead: admitted.
    let twin_node = NodeId::from("NS_Original__Refutation");
    state.live.present_nodes.remove(&twin_node);
    state.committed.present_nodes.remove(&twin_node);
    apply_conditional_proposal_carrier(
        &mut state,
        Some(proposal(&target, "n < 8", None)),
        &BTreeSet::new(),
        Vec::new(),
    )
    .expect("a dormant namespaced twin leaves the side open");
}

/// A PV Decide run without a trust seed: the human Advance approval is the
/// ratification, bound to the sealed packet's digest, and activation checks
/// exactly that binding.
fn seedless_state_with_pair(lean: &str) -> (ProtocolState, ChallengeTargetId) {
    let (mut state, target) = state_with_pair(lean);
    state.trust_base.mode = TrustBaseMode::Disabled;
    state.trust_base.format_version = 0;
    state.trust_base.advance_gate_episode_id = None;
    state.pv_tablet_configured = true;
    state.phase = Phase::TheoremStating;
    // `apply_event` validates the derived coverage mirrors, which the bare
    // pair fixture leaves empty; register the claims the seed would.
    let twin = refutation_target_id(&target);
    state
        .challenge_claims
        .insert(NodeId::from("Original"), BTreeSet::from([target.clone()]));
    state
        .challenge_claims
        .insert(NodeId::from("Original__Refutation"), BTreeSet::from([twin]));
    for name in ["Original", "Original__Refutation"] {
        state.deps.insert(NodeId::from(name), BTreeSet::new());
    }
    state.normalize_all_structural_state();
    state.ensure_node_metadata();
    assert!(state.conditional_theorem_lane_available());
    (state, target)
}

fn human_gate(state: &mut ProtocolState, choice: HumanChoice) -> ProtocolState {
    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::Advance;
    let request = state.issue_request(RequestKind::HumanGate);
    let outcome = apply_event(
        state.clone(),
        ProtocolEvent::WrapperResponse {
            response: WrapperResponse::HumanGate(HumanGateResponse {
                request_id: request.id,
                cycle: state.cycle,
                status: ResponseStatus::Ok,
                choice,
            }),
        },
    )
    .expect("the seedless advance gate accepts the decision");
    outcome.state
}

#[test]
fn seedless_advance_approval_ratifies_the_presented_packet_and_activates() {
    let (mut state, target) =
        seedless_state_with_pair("theorem Original (n : Nat) : n = n := by");
    let candidate = pass_correspondence(
        &mut state,
        proposal(&target, "n ≤ 8", Some(vec!["(9 : Nat)"])),
        vec![],
    );
    let expected = conditional_ratification_packet_digest_for_approved(&candidate).unwrap();

    let mut approved = human_gate(&mut state, HumanChoice::Approve);
    assert_eq!(approved.phase, Phase::ProofFormalization);
    let ratified = &approved.trust_base.conditional_candidates[&target];
    assert_eq!(ratified.stage, ConditionalStage::HumanApproved);
    let ratification = ratified.ratification.clone().expect("approval binds the packet");
    assert_eq!(ratification.gate_episode_id, GATE);
    assert_eq!(ratification.approval_record_sha256, Some(expected));
    assert!(approved.trust_base.current_human_approval_event_hash.is_none());
    validate_conditional_protocol_state(&approved).expect("ratified seedless state validates");

    // A binding to anything but the presented packet does not activate.
    let mut tampered = approved.clone();
    tampered
        .trust_base
        .conditional_candidates
        .get_mut(&target)
        .unwrap()
        .ratification
        .as_mut()
        .unwrap()
        .approval_record_sha256 = Some(raw_sha256(b"some other decision"));
    let payload = conditional_activation_payload(&tampered.trust_base.conditional_candidates[&target])
        .unwrap();
    assert!(apply_conditional_activation(&mut tampered, &payload).is_err());

    let payload =
        conditional_activation_payload(&approved.trust_base.conditional_candidates[&target]).unwrap();
    apply_conditional_activation(&mut approved, &payload).expect("the bound packet activates");
    assert_eq!(
        approved.trust_base.conditional_candidates[&target].stage,
        ConditionalStage::Open
    );
    validate_conditional_protocol_state(&approved).expect("open seedless conditional validates");
    approved
        .trust_base
        .conditional_candidates
        .get_mut(&target)
        .unwrap()
        .ratification
        .as_mut()
        .unwrap()
        .approval_record_sha256 = Some(raw_sha256(b"rewritten after activation"));
    assert!(
        validate_conditional_protocol_state(&approved).is_err(),
        "a persisted seedless binding that names another digest must not validate"
    );
}

#[test]
fn seedless_advance_feedback_rejects_the_pending_packet() {
    let (mut state, target) =
        seedless_state_with_pair("theorem Original (n : Nat) : n = n := by");
    pass_correspondence(&mut state, proposal(&target, "n ≤ 8", Some(vec!["(9 : Nat)"])), vec![]);
    let declined = human_gate(&mut state, HumanChoice::Feedback);
    assert_eq!(declined.phase, Phase::TheoremStating);
    let candidate = &declined.trust_base.conditional_candidates[&target];
    assert_eq!(candidate.disposition, Some(ConditionalDisposition::Rejected));
    assert!(candidate.ratification.is_none());
}


fn add_second_pair(state: &mut ProtocolState) -> ChallengeTargetId {
    let target = ChallengeTargetId::from("goal:second");
    for (id, name, lean, resolution) in [
        (target.clone(), "Second", "theorem Second (n : Nat) : n < 16 := by", ChallengeResolution::Decide),
        (refutation_target_id(&target), "Second__Refutation", "theorem Second__Refutation : False := by", ChallengeResolution::Prove),
    ] {
        state.configured_challenge_targets.insert(id.clone(), spec(name, lean, resolution));
        let node = NodeId::from(name);
        state.live.present_nodes.insert(node.clone());
        state.live.open_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), NodeKind::Proof);
        state.proof_nodes.insert(node.clone());
        state.challenge_claims.insert(node.clone(), BTreeSet::from([id]));
        state.deps.insert(node, BTreeSet::new());
    }
    state.normalize_all_structural_state();
    state.commit_live();
    target
}

fn record_false_as_stated(state: &mut ProtocolState, target: &ChallengeTargetId) {
    let spec = state.configured_challenge_targets.get_mut(target).unwrap();
    spec.statement_provenance = StatementProvenance::WorkerAuthored;
    let node = trellis_kernel::challenge_node_id(spec);
    state.substantiveness_status.insert(node.clone(), trellis_kernel::SubstantivenessStatus::Fail);
    state.live.substantiveness_current_fingerprints.insert(node.clone(), "false-as-stated".into());
    state.substantiveness_approved_fingerprints.insert(node, "false-as-stated".into());
}

fn advance_through_reviewer(state: &mut ProtocolState) -> ProtocolState {
    state.stage = Stage::Reviewer;
    state.active_node = None;
    state.clear_pending_task();
    let request = state.issue_request(RequestKind::Review);
    let response = trellis_kernel::ReviewResponse {
        request_id: request.id,
        cycle: state.cycle,
        status: ResponseStatus::Ok,
        decision: trellis_kernel::ReviewDecisionKind::AdvancePhase,
        ..trellis_kernel::ReviewResponse::default()
    };
    assert!(state.review_response_legal(&response), "{:?}", state.review_response_rejection_reasons(&response));
    let result = apply_event(state.clone(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Review(response),
    }).expect("the ordinary reviewer path must reach phase approval");
    assert_eq!(result.state.stage, Stage::HumanGate);
    assert_eq!(result.state.gate_kind, GateKind::Advance);
    result.state
}

#[test]
fn false_as_stated_conditional_reaches_ordinary_gate_without_becoming_a_result() {
    let (mut state, target) = seedless_state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    record_false_as_stated(&mut state, &target);
    assert!(!state.global_blockers().is_empty());
    pass_correspondence(&mut state, proposal(&target, "n < 4", None), vec![]);
    let node = NodeId::from("Original");
    assert!(state.global_blockers().is_empty(), "{:?}", state.global_blockers());
    assert!(!state.sound_dispatch_surface_blocked());
    assert!(!state.sound_verifier_eligible(&node));
    assert!(state.current_substantiveness_fail(&node), "routing is not a fabricated Pass");
    assert!(state.live.open_nodes.contains(&node));
    assert!(!state.formalization_complete());
    let candidate = &state.trust_base.conditional_candidates[&target];
    assert!(candidate.ratification.is_none());
    assert!(conditional_activation_payload(candidate).is_err());
    let mut at_gate = advance_through_reviewer(&mut state);
    assert_eq!(at_gate.in_flight_request.as_ref().unwrap().conditional_ratification_packets.len(), 1);
    let approved = human_gate(&mut at_gate, HumanChoice::Approve);
    let mut approved = approved;
    let payload = conditional_activation_payload(&approved.trust_base.conditional_candidates[&target]).unwrap();
    apply_conditional_activation(&mut approved, &payload).unwrap();
    assert_eq!(approved.trust_base.conditional_candidates[&target].stage, ConditionalStage::Open);
    assert!(!approved.formalization_complete(), "approval/activation is not Lean closure");
    assert!(approved.current_substantiveness_fail(&node));
    assert!(approved.live.open_nodes.contains(&node));
    assert!(payload.obligations.iter().all(|obligation| approved.live.open_nodes.contains(&obligation.node)));
}

#[test]
fn one_advance_gate_ratifies_every_packet_and_feedback_restores_original_blockers() {
    let (mut state, first) = seedless_state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    let second = add_second_pair(&mut state);
    for (target, condition) in [(&first, "n < 4"), (&second, "n < 12")] {
        record_false_as_stated(&mut state, target);
        pass_correspondence(&mut state, proposal(target, condition, None), vec![]);
        let candidate = state.trust_base.conditional_candidates.get_mut(target).unwrap();
        candidate.ratification_gate_episode_id = Some(format!("conditional-advance:{}", candidate.stamped.proposal_sha256.to_hex()));
    }
    assert!(state.global_blockers().is_empty());
    let mut gate = advance_through_reviewer(&mut state);
    let packets = gate.pending_conditional_ratification_packets().unwrap();
    assert_eq!(packets.len(), 2);
    assert_eq!(gate.in_flight_request.as_ref().unwrap().conditional_ratification_packets, packets);
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("Tablet")).unwrap();
    std::fs::write(repo.path().join("GOAL.md"), "Decide both bounded numerical claims.").unwrap();
    std::fs::write(repo.path().join("tcb_manifest.json"), r#"{"extraction_provenance": {}}"#).unwrap();
    for spec in gate.configured_challenge_targets.values() {
        std::fs::write(repo.path().join(format!("Tablet/{}.lean", spec.name)), &spec.lean).unwrap();
        std::fs::write(repo.path().join(format!("Tablet/{}.tex", spec.name)), "The registered numerical claim.").unwrap();
    }
    let rendered = trellis_kernel::runtime::trust_advance_gate_presentation_bytes_for_repo(&gate, repo.path()).unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains("goal:generic") && rendered.contains("goal:second"));
    let approved = human_gate(&mut gate.clone(), HumanChoice::Approve);
    assert_eq!(approved.phase, Phase::ProofFormalization);
    for target in [&first, &second] {
        let candidate = &approved.trust_base.conditional_candidates[target];
        assert_eq!(candidate.stage, ConditionalStage::HumanApproved);
        assert_eq!(candidate.ratification.as_ref().unwrap().approval_record_sha256,
            Some(conditional_ratification_packet_digest_for_approved(candidate).unwrap()));
    }
    let rejected = human_gate(&mut gate, HumanChoice::Feedback);
    assert_eq!(rejected.phase, Phase::TheoremStating);
    assert!(!rejected.global_blockers().is_empty());
    assert!(rejected.trust_base.conditional_candidates.values().all(|candidate|
        candidate.disposition == Some(ConditionalDisposition::Rejected) && candidate.ratification.is_none()));
}

#[test]
fn conditional_route_does_not_hide_other_nodes_and_withdrawal_restores_work() {
    let (mut state, target) = seedless_state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    record_false_as_stated(&mut state, &target);
    pass_correspondence(&mut state, proposal(&target, "n < 4", None), vec![]);
    state.live.present_nodes.insert("Helper".into());
    state.node_kinds.insert("Helper".into(), NodeKind::Proof);
    state.proof_nodes.insert("Helper".into());
    state.live.open_nodes.insert("Helper".into());
    state.deps.insert("Helper".into(), BTreeSet::from([NodeId::from("Original")]));
    assert!(!state.global_blockers().is_empty());
    assert!(state.sound_dispatch_surface_blocked());
    // Clear the consumer's own review lanes. Its failed premise must still
    // prevent Sound dispatch even though that premise is routed conditionally.
    state.corr_status.insert("Helper".into(), trellis_kernel::CorrStatus::Pass);
    state.live.corr_current_fingerprints.insert("Helper".into(), "helper".into());
    state.corr_approved_fingerprints.insert("Helper".into(), "helper".into());
    state.substantiveness_status.insert("Helper".into(), trellis_kernel::SubstantivenessStatus::Pass);
    state.live.substantiveness_current_fingerprints.insert("Helper".into(), "helper".into());
    state.substantiveness_approved_fingerprints.insert("Helper".into(), "helper".into());
    assert!(!state.sound_dispatch_surface_blocked());
    assert!(!state.sound_verifier_eligible(&"Helper".into()), "a failed original cannot become a verified premise");
    apply_conditional_proposal_carrier(&mut state, None, &BTreeSet::from([target]), vec![]).unwrap();
    assert!(!state.conditional_routes_unconditional_node(&"Original".into()));
    assert!(state.global_blockers().iter().any(|blocker|
        blocker.object == trellis_kernel::BlockerObject::Node { node: "Original".into() }));
}

#[test]
fn trust_record_ratifies_multiple_packets_atomically_and_replays_exactly() {
    let (mut state, first) = state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    let second = add_second_pair(&mut state);
    pass_correspondence(&mut state, proposal(&first, "n < 4", None), vec![]);
    pass_correspondence(&mut state, proposal(&second, "n < 12", None), vec![]);
    let digest = raw_sha256(b"complete gate decision");
    assert!(bind_conditional_ratification(&mut state, GATE, digest).unwrap());
    assert!(!bind_conditional_ratification(&mut state, GATE, digest).unwrap());
    assert!(state.trust_base.conditional_candidates.values().all(|candidate|
        candidate.ratification.as_ref().unwrap().approval_record_sha256 == Some(digest)));
    let before = state.clone();
    assert!(bind_conditional_ratification(&mut state, GATE, raw_sha256(b"other decision")).is_err());
    assert_eq!(state, before);
}

#[test]
fn gate_cannot_approve_a_packet_removed_after_presentation() {
    let (mut state, target) = seedless_state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    pass_correspondence(&mut state, proposal(&target, "n < 4", None), vec![]);
    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::Advance;
    let request = state.issue_request(RequestKind::HumanGate);
    apply_conditional_proposal_carrier(&mut state, None, &BTreeSet::from([target.clone()]), vec![]).unwrap();
    let result = apply_event(state.clone(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::HumanGate(HumanGateResponse {
            request_id: request.id, cycle: state.cycle, status: ResponseStatus::Ok,
            choice: HumanChoice::Approve,
        }),
    });
    assert!(format!("{:?}", result.unwrap_err()).contains("differs from the issued gate"));
    assert!(state.trust_base.conditional_candidates[&target].ratification.is_none());
}

#[test]
fn ordinary_correspondence_response_routes_a_false_as_stated_original() {
    let (mut state, target) = seedless_state_with_pair("theorem Original : ∀ n : Nat, n < 8 := by");
    state.conditional_binding_syntax_enabled = true;
    record_false_as_stated(&mut state, &target);
    apply_conditional_proposal_carrier(&mut state, Some(proposal(&target, "n < 4", None)),
        &BTreeSet::new(), vec![]).unwrap();
    state.stage = Stage::VerifyCorr;
    let request = state.issue_request(RequestKind::Corr);
    let packet = request.conditional_theorem_correspondence.as_ref().unwrap();
    let verdict = ConditionalCorrespondenceVerdict {
        request_sha256: packet.request_sha256,
        proposal_sha256: packet.proposal.proposal_sha256,
        boundary_expression_correct: true, condition_relevant: true,
        realizable_non_vacuous: true, obligation_preserved_on_domain: true,
        rust_axioms_backed_by_approved_assumptions: true,
        findings: "The restricted numerical theorem has an inhabited domain.".into(),
    };
    let outcome = apply_event(state, ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Corr(trellis_kernel::CorrResponse {
            request_id: request.id, cycle: request.cycle, status: ResponseStatus::Ok,
            conditional_theorem_correspondence: Some(verdict),
            ..Default::default()
        }),
    }).unwrap();
    let mut state = outcome.state;
    assert_eq!(state.trust_base.conditional_candidates[&target].stage, ConditionalStage::CorrespondencePass);
    assert!(state.global_blockers().is_empty());
    assert!(!state.sound_dispatch_surface_blocked());
    assert!(state.sound_dispatch_surface_blockers().is_empty());
    assert!(state.select_theorem_held_target().is_none());
    assert!(state.current_substantiveness_fail(&"Original".into()));
    assert!(!state.formalization_complete());
    let gate = advance_through_reviewer(&mut state);
    assert_eq!(gate.in_flight_request.unwrap().conditional_ratification_packets.len(), 1);
}

#[test]
fn conditional_binding_precheck_rejects_unsupported_shape_and_wrong_witness_arity() {
    for (lean, arguments, reason) in [
        ("theorem Original : ∀ n, n = n := by", None, "type annotation"),
        ("theorem Original : ∀ n : Nat, n = n := by", Some(vec!["0", "1"]), "binder arity"),
        ("theorem Original : ∀ n : Nat, n = n := by", Some(vec!["bad -- comment"]), "invalid Lean argument"),
    ] {
        let (mut state, target) = state_with_pair(lean);
        state.conditional_binding_syntax_enabled = true;
        let before = state.clone();
        let error = apply_conditional_proposal_carrier(&mut state,
            Some(proposal(&target, "n < 4", arguments)), &BTreeSet::new(), vec![]).unwrap_err();
        assert!(format!("{error:?}").contains(reason), "{error:?}");
        assert_eq!(state, before, "bad syntax must not install an unsealable candidate");
    }
}

#[test]
fn conditional_binding_migration_preserves_legacy_seals_and_request_identity() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    let candidate = pass_correspondence(&mut state,
        proposal(&target, "n ≠ 0", Some(vec!["0"])), vec![]);
    let legacy = candidate.sealed.as_ref().unwrap();
    assert!(legacy.counterexample_excluded.as_ref().unwrap().statement_lean.contains("@predicate 0)"));
    let before = serde_json::to_value(&state).unwrap();
    assert!(before.get("conditional_binding_syntax_enabled").is_none());
    let meta = trellis_kernel::runtime::RuntimeMetadata {
        conditional_binding_syntax_activation_event_count: Some(271), ..Default::default()
    };
    meta.apply_conditional_binding_boundary(&mut state, 270);
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    meta.apply_conditional_binding_boundary(&mut state, 271);
    assert!(state.conditional_binding_syntax_enabled);
    assert_eq!(state.trust_base.conditional_candidates[&target], candidate);
    validate_conditional_protocol_state(&state).unwrap();
    let request = candidate.correspondence_request.as_ref().unwrap();
    let verdict = &candidate.correspondence.as_ref().unwrap().verdict;
    let (_, current) = seal_conditional_theorem(&state, &candidate.stamped, request, verdict).unwrap();
    assert_eq!(current.proposal_sha256, legacy.proposal_sha256);
    assert_eq!(current.correspondence_sha256, legacy.correspondence_sha256);
    assert!(current.counterexample_excluded.unwrap().statement_lean.contains("@predicate (0))"));
    let empty = serde_json::to_value(trellis_kernel::runtime::RuntimeMetadata::default()).unwrap();
    assert!(empty.get("conditional_binding_syntax_seeded").is_none());
    assert!(empty.get("conditional_binding_syntax_activation_event_count").is_none());
}

#[test]
fn legacy_single_packet_request_approves_only_its_frozen_packet() {
    let (mut state, target) = seedless_state_with_pair("theorem Original (n : Nat) : n < 8 := by");
    pass_correspondence(&mut state, proposal(&target, "n < 4", None), vec![]);
    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::Advance;
    let request = state.issue_request(RequestKind::HumanGate);
    state.in_flight_request.as_mut().unwrap().conditional_ratification_packets.clear();
    let approved = apply_event(state, ProtocolEvent::WrapperResponse {
        response: WrapperResponse::HumanGate(HumanGateResponse {
            request_id: request.id, cycle: request.cycle, status: ResponseStatus::Ok,
            choice: HumanChoice::Approve,
        }),
    }).unwrap().state;
    let candidate = &approved.trust_base.conditional_candidates[&target];
    assert_eq!(candidate.stage, ConditionalStage::HumanApproved);
    assert_eq!(candidate.ratification.as_ref().unwrap().approval_record_sha256,
        Some(conditional_ratification_packet_digest_for_approved(candidate).unwrap()));
}

#[test]
fn audit_routing_boundary_exposes_existing_packets_without_changing_the_seal() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    state.phase = Phase::TheoremStating;
    let candidate = pass_correspondence(&mut state, proposal(&target, "n < 4", None), vec![]);
    state.stage = Stage::StuckMathAudit;
    state.issue_request(RequestKind::StuckMathAudit);
    assert!(state.in_flight_request.as_ref().unwrap().conditional_ratification_packets.is_empty());
    let legacy = state.clone();
    let meta = trellis_kernel::runtime::RuntimeMetadata {
        audit_routing_consistency_activation_event_count: Some(307), ..Default::default()
    };
    meta.apply_audit_routing_boundary(&mut state, 306);
    assert_eq!(state, legacy);
    meta.apply_audit_routing_boundary(&mut state, 307);
    assert!(state.audit_routing_consistency_enabled);
    assert_eq!(state.trust_base.conditional_candidates[&target], candidate);
    let request = state.in_flight_request.as_ref().unwrap();
    assert_eq!(request.id, legacy.in_flight_request.as_ref().unwrap().id);
    assert_eq!(request.conditional_ratification_packets.len(), 1);
    assert_eq!(request.conditional_ratification_packets, state.expected_request(request.id, RequestKind::Review).conditional_ratification_packets);
    let contract = trellis_kernel::request_contracts::stuck_math_audit_contract_payload(request);
    assert_eq!(contract["request_summary"]["conditional_ratification_packets"], serde_json::json!(request.conditional_ratification_packets));
    let upgraded = state.clone();
    meta.apply_audit_routing_boundary(&mut state, 308);
    assert_eq!(state, upgraded);
    let empty = serde_json::to_value(trellis_kernel::runtime::RuntimeMetadata::default()).unwrap();
    assert!(empty.get("audit_routing_consistency_seeded").is_none());
    assert!(empty.get("audit_routing_consistency_activation_event_count").is_none());
}

#[test]
fn audit_lifecycle_boundary_preserves_history_and_exposes_pinned_targets() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    state.trust_base.conditional_candidates.insert(target.clone(),
        trellis_kernel::trust_base::ConditionalCandidateGeneration {
            stage: ConditionalStage::Closed, ..Default::default()
        });
    state.stage = Stage::StuckMathAudit;
    state.issue_request(RequestKind::StuckMathAudit);
    let legacy = state.clone();
    let meta = trellis_kernel::runtime::RuntimeMetadata {
        audit_lifecycle_retention_activation_event_count: Some(17), ..Default::default()
    };
    meta.apply_audit_routing_boundary(&mut state, 16);
    assert_eq!(state, legacy);
    meta.apply_audit_routing_boundary(&mut state, 17);
    assert!(state.audit_lifecycle_retention_enabled);
    assert_eq!(state.trust_base, legacy.trust_base);
    assert_eq!(state.pv_live_polarity, legacy.pv_live_polarity);
    assert_eq!(state.request_seq, legacy.request_seq);
    for kind in [RequestKind::Review, RequestKind::StuckMathAudit] {
        let request = state.expected_request(state.request_seq, kind);
        assert_eq!(request.closed_conditional_polarity_targets,
            std::collections::BTreeSet::from([target.clone()]));
        let contract = if kind == RequestKind::Review {
            trellis_kernel::request_contracts::review_contract_payload(&request, None)
        } else { trellis_kernel::request_contracts::stuck_math_audit_contract_payload(&request) };
        assert_eq!(contract["request_summary"]["closed_conditional_polarity_targets"],
            serde_json::json!([target]));
    }
    assert!(state.expected_request(state.request_seq, RequestKind::Worker)
        .closed_conditional_polarity_targets.is_empty());
    let upgraded = state.clone();
    meta.apply_audit_routing_boundary(&mut state, 18);
    assert_eq!(state, upgraded);
    let empty = serde_json::to_value(trellis_kernel::runtime::RuntimeMetadata::default()).unwrap();
    assert!(empty.get("audit_lifecycle_retention_seeded").is_none());
    assert!(empty.get("audit_lifecycle_retention_activation_event_count").is_none());
}

#[test]
fn independent_original_work_preserves_conditional_fallback_and_exact_closures() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    state.conditional_coexistence_enabled = true;
    pass_correspondence(&mut state, proposal(&target, "n < 8", None), Vec::new());
    ratify_and_activate(&mut state, &target);
    close_all_conditional_obligations(&mut state, &target);
    let candidate = state.trust_base.conditional_candidates[&target].clone();
    assert_eq!(candidate.stage, ConditionalStage::Closed);
    assert!(state.conditional_disclosed_unconditional_node(&"Original".into()));
    assert!(matches!(terminal_outcome(&state, &target).unwrap(), TerminalOutcome::ConditionalTheorem { .. }));
    state.unrestricted_work_selections.insert(target.clone());
    assert!(!state.conditional_routes_unconditional_node(&"Original".into()));
    assert!(!state.conditional_disclosed_unconditional_node(&"Original".into()));
    assert!(terminal_outcome(&state, &target).unwrap_err().contains("selected unrestricted work"));
    let mut record = state.local_closure_records[&candidate.sealed.as_ref().unwrap().proof.node].clone();
    record.node = "Original__Refutation".into();
    state.live.open_nodes.remove(&record.node);
    state.local_closure_records.insert(record.node.clone(), record.clone());
    assert!(matches!(terminal_outcome(&state, &target).unwrap(), TerminalOutcome::Disproved { .. }));
    assert!(validate_conditional_protocol_state(&state).is_ok());
    assert_eq!(state.trust_base.conditional_candidates[&target], candidate);
    let dossiers = trellis_kernel::trust_base::claim::refutation_dossier_rows(&state);
    assert_eq!(dossiers[0]["conditional_companion"]["status"], "checked");
    assert_eq!(dossiers[0]["conditional_companion"]["result"]["conditional_target_id"], serde_json::json!(candidate.sealed.as_ref().unwrap().proof.target_id));
    state.trust_base.conditional_candidates.get_mut(&target).unwrap().stage = ConditionalStage::Open;
    assert_eq!(trellis_kernel::trust_base::claim::refutation_dossier_rows(&state)[0]["conditional_companion"]["status"], "unresolved");
    assert!(matches!(terminal_outcome(&state, &target).unwrap(), TerminalOutcome::Disproved { .. }));
    state.trust_base.conditional_candidates.get_mut(&target).unwrap().stage = ConditionalStage::Closed;
    state.live.open_nodes.insert(candidate.sealed.as_ref().unwrap().proof.node.clone());
    assert!(matches!(terminal_outcome(&state, &target).unwrap(), TerminalOutcome::Disproved { .. }));
    assert!(validate_conditional_protocol_state(&state).is_err());
    record.node = "Original".into();
    state.live.open_nodes.remove(&record.node);
    state.local_closure_records.insert(record.node.clone(), record);
    assert!(terminal_outcome(&state, &target).unwrap_err().contains("NotBothSidesClosed"));
}

#[test]
fn explicit_disproof_survives_later_conditional_and_exports_both_results() {
    let (mut state, target) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    state.conditional_coexistence_enabled = true;
    state.pv_live_polarity.insert(target.clone(), trellis_kernel::ChallengePolarity::Disprove);
    pass_correspondence(&mut state, proposal(&target, "n < 8", None), Vec::new());
    ratify_and_activate(&mut state, &target);
    close_all_conditional_obligations(&mut state, &target);
    assert!(state.unrestricted_work_selected(&target));
    assert!(!state.conditional_disclosed_unconditional_node(&"Original__Refutation".into()));
    state.approved_targets.configured_targets.insert("goal".into());
    state.approved_targets.coverage.insert("goal".into(), BTreeSet::from(["Original".into()]));
    let approved = state.approved_targets.clone();
    let nodes = trellis_kernel::tablet_root::result_umbrella_nodes(&state);
    assert!(!nodes.contains(&NodeId::from("Original")));
    assert!(nodes.contains(&NodeId::from("Original__Refutation")));
    assert!(nodes.contains(&state.trust_base.conditional_candidates[&target].sealed.as_ref().unwrap().proof.node));
    assert_eq!(state.approved_targets, approved);
}

#[test]
fn coexistence_boundary_preserves_approval_and_refreshes_existing_request_once() {
    let (mut state, _) = state_with_pair("theorem Original (n : Nat) : n = n := by");
    state.stage = Stage::StuckMathAudit;
    state.issue_request(RequestKind::StuckMathAudit);
    let legacy = state.clone();
    let meta = trellis_kernel::RuntimeMetadata {
        conditional_coexistence_activation_event_count: Some(12), ..Default::default()
    };
    meta.apply_audit_routing_boundary(&mut state, 11);
    assert_eq!(state, legacy);
    meta.apply_audit_routing_boundary(&mut state, 12);
    assert!(state.conditional_coexistence_enabled);
    assert!(state.in_flight_request.as_ref().unwrap().conditional_coexistence_enabled);
    assert_eq!(state.trust_base, legacy.trust_base);
    assert_eq!(state.approved_targets, legacy.approved_targets);
    assert_eq!(state.request_seq, legacy.request_seq);
    let upgraded = state.clone();
    meta.apply_audit_routing_boundary(&mut state, 13);
    assert_eq!(state, upgraded);
}

#[test]
fn modeling_approval_activation_preserves_history_and_existing_approvals() {
    let mut state = ProtocolState::default();
    let before = state.approved_targets.clone();
    let metadata = trellis_kernel::RuntimeMetadata {
        modeling_approval_protection_activation_event_count: Some(12), ..Default::default()
    };
    metadata.apply_audit_routing_boundary(&mut state, 11);
    assert!(!state.modeling_approval_protection_enabled);
    metadata.apply_audit_routing_boundary(&mut state, 12);
    assert!(state.modeling_approval_protection_enabled);
    assert_eq!(state.approved_targets, before);
    assert!(!state.modeling_human_approved);
}
