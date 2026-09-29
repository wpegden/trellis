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
    apply_conditional_proposal_carrier(state, Some(proposal), &BTreeSet::new(), snapshots)
        .expect("proposal is stamped");
    let target = ChallengeTargetId::from("goal:generic");
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
