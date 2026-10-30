//! Paper-backed implementation protocol. This carries provenance and review
//! subjects, never a new assumption authority or an agent-supplied approval.
use std::collections::{BTreeMap, BTreeSet};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::{GateKind, NodeId, Phase, ProtocolState, PvRole, Stage, TargetId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperImplementationRoute { RustAeneas, LeanNative }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperModelingChoice {
    pub node: NodeId,
    pub source_locator: String,
    pub rationale: String,
    pub evidence_path: String,
    pub consequential: bool,
    /// Optional locator only. Authority is the actual native route decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_decision: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperProofLibrary { Mathlib }

/// Derived from admitted frozen proof-project inputs, never a capability flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperProofSupport {
    pub kind: PaperProofLibrary,
    pub mathlib_rev: String,
    pub dependency_inventory_sha256: String,
    pub lake_manifest_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperImplementation {
    pub version: u32,
    pub route: PaperImplementationRoute,
    pub manifest_path: String,
    pub subject_sha256: String,
    pub configuration_sha256: String,
    pub paper_path: String,
    pub paper_sha256: String,
    pub implementation_roots: BTreeMap<TargetId, BTreeSet<NodeId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_support: Option<PaperProofSupport>,
    #[serde(default)]
    pub modeling_choices: Vec<PaperModelingChoice>,
    #[serde(default)]
    pub excluded_complexity_clauses: BTreeMap<TargetId, Vec<String>>,
}

pub fn approval_basis(subject: &PaperImplementation) -> String {
    crate::cache_key::hash_bytes(&serde_json::to_vec(subject).expect("serializable paper subject"))
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub fn validate_subject(subject: &PaperImplementation) -> Result<(), String> {
    if subject.version != 1 || !digest(&subject.subject_sha256) || !digest(&subject.paper_sha256) || !digest(&subject.configuration_sha256) {
        return Err("paper implementation requires v1 and exact SHA-256 subject/paper identities".into());
    }
    for path in [&subject.manifest_path, &subject.paper_path] {
        if path.trim().is_empty() || std::path::Path::new(path).is_absolute()
            || std::path::Path::new(path).components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err("paper implementation evidence paths must be nonempty repo-relative paths".into());
        }
    }
    if let Some(proof) = &subject.proof_support {
        if subject.route != PaperImplementationRoute::LeanNative
            || proof.mathlib_rev.len() != 40 || !proof.mathlib_rev.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            || !digest(&proof.dependency_inventory_sha256) || !digest(&proof.lake_manifest_sha256) {
            return Err("Mathlib proof support requires a Lean-native subject with exact admitted package identities".into());
        }
    }
    if subject.implementation_roots.is_empty() || subject.implementation_roots.iter().any(|(t, roots)|
        t.as_str().trim().is_empty() || roots.is_empty() || roots.iter().any(|n| n.as_str().trim().is_empty())) {
        return Err("paper implementation requires original paper targets with explicit implementation roots".into());
    }
    if subject.excluded_complexity_clauses.keys().any(|t| !subject.implementation_roots.contains_key(t)) {
        return Err("excluded complexity clauses must belong to selected original paper targets".into());
    }
    let mut nodes = BTreeSet::new();
    for choice in &subject.modeling_choices {
        if !nodes.insert(&choice.node) || choice.node.as_str().trim().is_empty()
            || choice.source_locator.trim().is_empty() || choice.rationale.trim().is_empty()
            || choice.evidence_path.trim().is_empty() {
            return Err("manual modeling choices need unique nodes, source evidence and rationale".into());
        }
    }
    if subject.route == PaperImplementationRoute::LeanNative && !subject.modeling_choices.is_empty() {
        return Err("Lean-native cannot inherit Rust manual-model exceptions".into());
    }
    Ok(())
}

/// Structural admission/resume guard. Missing proofs and route reviews are
/// ordinary blockers, not malformed intake. No new assumption lifecycle.
pub fn validate_state(state: &ProtocolState) -> Result<(), String> {
    let Some(subject) = &state.paper_implementation else { return Ok(()); };
    validate_subject(subject)?;
    if subject.implementation_roots.keys().cloned().collect::<BTreeSet<_>>() != state.configured_targets {
        return Err("paper implementation selected targets changed; prepare a new generation".into());
    }
    if state.pv_goal_prose_sha256.is_some() || !state.pv_verification_target_prefixes.is_empty()
        || state.authored_goals_decidable || state.pv_revision_enabled {
        return Err("paper implementation uses paper targets, not GOAL/auto-seeded/Decide or PV revision targets".into());
    }
    if state.pending_under_model_assumptions != 0 || state.effective_pending_assumption_draft().is_some()
        || !state.staged_assumption_lanes.is_empty()
        || state.node_role.values().any(|r| *r == PvRole::UnderModelAssumptions)
        || state.stuck_math_audit.assumptions_lane.is_some()
        || !state.trust_base.conditional_candidates.is_empty()
        || state.trust_base.active_revision_lane_id.is_some() {
        return Err("paper implementation v1 rejects additional assumption, conditional and trust revision admission".into());
    }
    match subject.route {
        PaperImplementationRoute::RustAeneas => {
            if !state.is_pv() || !state.model_preservation_enabled {
                return Err("Rust paper implementation requires existing extraction and preservation handling".into());
            }
        }
        PaperImplementationRoute::LeanNative => {
            if state.is_pv() || state.node_role.values().any(|r| *r == PvRole::ExtractionModel) {
                return Err("Lean-native uses ordinary checked definitions, never an extraction-role exemption".into());
            }
        }
    }
    if state.phase == Phase::RevisionStating {
        return Err("paper implementation v1 does not revise the selected paper generation in place".into());
    }
    if state.paper_implementation_approved_sha256.is_none()
        && matches!(state.phase, Phase::ProofFormalization | Phase::Cleanup) {
        return Err("paper implementation proof formalization requires its sole human approval".into());
    }
    if let Some(approved) = &state.paper_implementation_approved_sha256 {
        if approved != &approval_basis(subject) {
            return Err("paper implementation source/evidence changed after the sole human gate".into());
        }
        if matches!(state.phase, Phase::Modeling | Phase::TheoremStating | Phase::RevisionStating)
            || state.stage == Stage::HumanGate {
            return Err("paper implementation v1 cannot reopen an approved generation or add another human gate".into());
        }
    }
    if state.stage == Stage::HumanGate && (state.phase != Phase::TheoremStating || state.gate_kind != GateKind::Advance) {
        return Err("paper implementation has one Advance gate after theorem stating".into());
    }
    if state.modeling_human_approved {
        return Err("paper implementation cannot fabricate or reuse an earlier Modeling human approval".into());
    }
    Ok(())
}

/// A route's agent decision authorizes the listed consequential choices only
/// on the exact source/paper generation it reviewed. Body checking and source
/// preservation still follow; this does not establish semantic equivalence.
pub fn choice_has_route(state: &ProtocolState, node: &NodeId) -> bool {
    let Some(subject) = &state.paper_implementation else { return false; };
    state.modeling_route_decisions.iter().any(|decision| {
        decision["paper_implementation_basis"].as_str() == Some(approval_basis(subject).as_str())
            && decision["resolution"]["action"].as_str() == Some("approve")
            && decision["attempts"].as_array().is_some_and(|a| a.len() == 2)
            && decision["paper_modeling_choices"].as_array().is_some_and(|choices|
                choices.iter().any(|choice| choice.as_str() == Some(node.as_str())))
    })
}

pub fn route_task_id(node: &NodeId) -> String { format!("paper-model:{node}") }

pub fn approved_route_choices(subject: &PaperImplementation, resolution: &crate::ModelingResolution) -> Result<BTreeSet<NodeId>, String> {
    let declared = subject.modeling_choices.iter().filter(|choice| choice.consequential)
        .map(|choice| choice.node.clone()).collect();
    approved_route_choice_ids(&declared, resolution)
}

/// Shared preparation/native scope check. Declared IDs and proposal markers
/// describe what was reviewed; only the existing route audit can approve it.
pub fn approved_route_choice_ids(declared: &BTreeSet<NodeId>, resolution: &crate::ModelingResolution) -> Result<BTreeSet<NodeId>, String> {
    let crate::ModelingResolution::Approve { tasks, .. } = resolution else { return Ok(BTreeSet::new()); };
    if declared.iter().any(|node| node.as_str().trim().is_empty()) {
        return Err("declared consequential paper-model choice IDs must be nonblank".into());
    }
    let declared: BTreeMap<_, _> = declared.iter().map(|node| (route_task_id(node), node.clone())).collect();
    let mut selected = BTreeSet::new();
    for task in tasks {
        if !task.id.starts_with("paper-model:") { continue; }
        let node = declared.get(&task.id).ok_or_else(|| format!("modeling route task {} does not name a declared consequential paper-model choice", task.id))?;
        selected.insert(node.clone());
    }
    Ok(selected)
}

/// Keep the final authorization subset within the choices explicitly put to
/// both existing planner/critic rounds. This is identity bookkeeping; whether
/// the cited arguments justify the choice remains the lanes' semantic task.
pub fn route_choices_reviewed(review: &crate::ModelingRouteReview, selected: &BTreeSet<NodeId>) -> bool {
    selected.iter().all(|node| {
        let marker = route_task_id(node);
        review.attempts.len() == 2 && review.attempts.iter().all(|attempt| {
            attempt.route_tex.match_indices(&marker).any(|(start, _)| {
                let boundary = |c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '\'' | '-'));
                attempt.route_tex[..start].chars().next_back().is_none_or(boundary)
                    && attempt.route_tex[start + marker.len()..].chars().next().is_none_or(boundary)
            })
        })
    })
}

pub fn manual_authoring_errors(state: &ProtocolState, changed_nodes: &BTreeSet<NodeId>) -> Vec<String> {
    state.paper_implementation.iter().flat_map(|subject| subject.modeling_choices.iter())
        .filter(|choice| choice.consequential && changed_nodes.contains(&choice.node)
            && !choice_has_route(state, &choice.node))
        .map(|choice| format!("manual model {} requires the existing native modeling-route agent approval before authoring", choice.node))
        .collect()
}

/// Necessary structural binding, not sufficient semantic correspondence.
/// Paper/Corr must judge nonvacuous use and the implementation/refinement claim.
pub fn target_has_implementation_roots(state: &ProtocolState, target: &TargetId) -> bool {
    let Some(subject) = &state.paper_implementation else { return true; };
    let Some(roots) = subject.implementation_roots.get(target) else { return false; };
    if !roots.iter().all(|r| {
        if !state.live.present_nodes.contains(r)
            || state.node_kinds.get(r) != Some(&crate::NodeKind::Definition) { return false; }
        if subject.route == PaperImplementationRoute::LeanNative {
            return state.node_role.get(r) != Some(&PvRole::ExtractionModel);
        }
        state.node_role.get(r) == Some(&PvRole::ExtractionModel)
            && state.challenge_claims.get(r).is_some_and(|claims| claims.iter().any(|id| {
                state.configured_challenge_targets.get(id).is_some_and(|spec|
                    spec.kind == crate::ChallengeTargetKind::Def && !spec.lean.trim().is_empty()
                        && digest(&spec.provenance.source_sha256)
                        && digest(&spec.provenance.extractor_toolchain_sha256)
                        && state.approved_extraction_provenance.get(id) == Some(&spec.provenance))
            }))
    }) { return false; }
    let mut pending: Vec<_> = state.live.coverage.get(target).into_iter().flatten().cloned().collect();
    let mut reachable = BTreeSet::new();
    while let Some(node) = pending.pop() {
        if !reachable.insert(node.clone()) { continue; }
        pending.extend(state.deps.get(&node).into_iter().flatten().cloned());
        pending.extend(state.lean_relevant_dependencies_of(&node));
    }
    roots.is_subset(&reachable)
}

pub fn modeling_choices_ready(state: &ProtocolState) -> bool {
    let Some(subject) = &state.paper_implementation else { return true; };
    subject.modeling_choices.iter().all(|choice| {
        state.preservation_owner(&choice.node)
            && state.current_corr_pass(&choice.node)
            && state.current_sound_pass(&choice.node)
            && (!choice.consequential || choice_has_route(state, &choice.node))
    })
}

pub fn review_evidence(state: &ProtocolState) -> Value {
    let Some(subject) = &state.paper_implementation else { return Value::Null; };
    json!({
        "subject": subject,
        "one_human_gate": "Advance after Modeling/support and TheoremStating arguments, before ProofFormalization",
        "human_review_topics": ["algorithm fidelity", "all Rust manual modeling exceptions", "original paper/formal target correspondence, including computational evidence dependencies"],
        "target_covering_nodes": state.live.coverage,
        "implementation_roots": subject.implementation_roots,
        "proof_support": subject.proof_support,
        "proof_library_boundary": "Optional Mathlib is trusted proof support from the existing host-prewarmed checker boundary; executable assembly remains core-only with identical implementation source files. Existing axiom policy is unchanged.",
        "modeling_route_decisions": state.modeling_route_decisions,
        "modeling_choices_ready": modeling_choices_ready(state),
        "consequential_choice_task_ids": subject.modeling_choices.iter().filter(|choice| choice.consequential)
            .map(|choice| (choice.node.clone(), route_task_id(&choice.node))).collect::<BTreeMap<_,_>>(),
        "approved_subject_sha256": state.paper_implementation_approved_sha256,
        "frozen_semantic_owners": state.paper_reviewed_semantic_nodes(),
        "proof_status": "Completion requires checked transitive closure",
        "scope": "Selected semantic conclusions; excluded complexity clauses are recorded in the subject",
        "trust": "Existing trust basis; compiler/runtime correctness remains trusted"
    })
}

/// Only new-mode runs opt into disk subject observation; historical math/PV
/// fingerprints remain byte-identical, including config-free unit fixtures.
pub fn observed_subject_basis(repo: &std::path::Path) -> Result<Option<String>, String> {
    let config_path = repo.join("trellis.config.json");
    let configured = std::fs::read(&config_path).ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|config| config.get("paper_implementation").is_some());
    if !configured { return Ok(None); }
    crate::paper_implementation_io::load_subject(repo, &config_path)
        .map(|subject| subject.as_ref().map(approval_basis))
}

#[cfg(test)]
pub(crate) fn test_subject(route: PaperImplementationRoute) -> PaperImplementation {
    PaperImplementation {
        version: 1, route, manifest_path: "paper-implementation/manifest.json".into(),
        subject_sha256: "a".repeat(64), configuration_sha256: "c".repeat(64),
        paper_path: "paper/main.tex".into(), paper_sha256: "b".repeat(64),
        implementation_roots: BTreeMap::from([("t".into(), BTreeSet::from(["Impl".into()]))]),
        modeling_choices: vec![], excluded_complexity_clauses: BTreeMap::new(), proof_support: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.paper_implementation = Some(test_subject(PaperImplementationRoute::LeanNative));
        state.configured_targets.insert("t".into());
        state
    }

    #[test]
    fn paper_implementation_subject_has_no_cost_or_assumption_configuration() {
        let mut value = serde_json::to_value(test_subject(PaperImplementationRoute::LeanNative)).unwrap();
        value["cost_verification"] = json!(true);
        assert!(serde_json::from_value::<PaperImplementation>(value).is_err());
        let mut value = serde_json::to_value(test_subject(PaperImplementationRoute::LeanNative)).unwrap();
        value["approved_axioms"] = json!(["assume_correct"]);
        assert!(serde_json::from_value::<PaperImplementation>(value).is_err());
    }

    #[test]
    fn paper_implementation_rejects_pending_assumptions_and_extraction_role_on_lean() {
        let baseline = state();
        assert!(validate_state(&baseline).is_ok());
        let mut changed = baseline.clone(); changed.pending_under_model_assumptions = 1;
        assert!(validate_state(&changed).unwrap_err().contains("assumption"));
        let mut changed = baseline.clone(); changed.node_role.insert("Impl".into(), PvRole::ExtractionModel);
        assert!(validate_state(&changed).unwrap_err().contains("ordinary"));
        let mut changed = baseline; changed.pv_goal_prose_sha256 = Some("d".repeat(64));
        assert!(validate_state(&changed).unwrap_err().contains("GOAL"));
    }

    #[test]
    fn paper_implementation_freeze_binds_configuration_and_forbids_second_gate() {
        let mut state = state();
        state.paper_implementation_approved_sha256 = Some(approval_basis(state.paper_implementation.as_ref().unwrap()));
        state.phase = Phase::ProofFormalization;
        assert!(validate_state(&state).is_ok());
        let mut changed = state.clone(); changed.paper_implementation.as_mut().unwrap().configuration_sha256 = "d".repeat(64);
        assert!(validate_state(&changed).unwrap_err().contains("changed"));
        let mut changed = state; changed.stage = Stage::HumanGate; changed.gate_kind = GateKind::ProtectedReapproval;
        assert!(validate_state(&changed).unwrap_err().contains("another human gate"));
    }

    #[test]
    fn paper_implementation_ideal_theorem_without_implementation_bridge_fails_coverage() {
        let mut state = state();
        state.live.present_nodes = BTreeSet::from(["Impl".into(), "Ideal".into(), "Target".into()]);
        state.node_kinds.insert("Impl".into(), crate::NodeKind::Definition);
        state.live.coverage.insert("t".into(), BTreeSet::from(["Target".into()]));
        state.deps.insert("Target".into(), BTreeSet::from(["Ideal".into()]));
        assert!(!target_has_implementation_roots(&state, &"t".into()));
        state.deps.insert("Ideal".into(), BTreeSet::from(["Impl".into()]));
        assert!(target_has_implementation_roots(&state, &"t".into()));
        state.node_kinds.insert("Impl".into(), crate::NodeKind::Proof);
        assert!(!target_has_implementation_roots(&state, &"t".into()));
    }

    #[test]
    fn paper_implementation_empty_coverage_is_a_repairable_failure() {
        for route in [PaperImplementationRoute::LeanNative, PaperImplementationRoute::RustAeneas] {
            for fingerprint in [None, Some("")] {
                let mut state = state();
                state.paper_implementation.as_mut().unwrap().route = route;
                if let Some(fp) = fingerprint {
                    state.live.paper_current_fingerprints.insert("t".into(), fp.into());
                }
                for status in [crate::CorrStatus::Unknown, crate::CorrStatus::Fail, crate::CorrStatus::Pass] {
                    state.paper_status.insert("t".into(), status);
                    state.paper_approved_fingerprints.insert("t".into(), String::new());
                    assert_eq!(state.current_paper_state(&"t".into()), crate::CurrentCheckState::Fail);
                    assert!(state.paper_verify_targets().is_empty());
                    assert!(state.current_failed_blockers().iter().any(|b|
                        b.kind == crate::BlockerKind::PaperFaithfulness));
                    assert!(!state.modeling_human_approved);
                }
            }
        }
    }

    #[test]
    fn paper_implementation_rooted_coverage_requires_a_current_fingerprint() {
        let mut state = state();
        state.live.present_nodes = BTreeSet::from(["Impl".into(), "Target".into()]);
        state.node_kinds.insert("Impl".into(), crate::NodeKind::Definition);
        state.live.coverage.insert("t".into(), BTreeSet::from(["Target".into()]));
        state.deps.insert("Target".into(), BTreeSet::from(["Impl".into()]));
        for fingerprint in [None, Some("")] {
            state.live.paper_current_fingerprints.clear();
            if let Some(fp) = fingerprint {
                state.live.paper_current_fingerprints.insert("t".into(), fp.into());
            }
            for status in [crate::CorrStatus::Pass, crate::CorrStatus::Fail] {
                state.paper_status.insert("t".into(), status);
                state.paper_approved_fingerprints.insert("t".into(), String::new());
                assert_eq!(state.current_paper_state(&"t".into()), crate::CurrentCheckState::Unknown);
            }
        }
        state.live.paper_current_fingerprints.insert("t".into(), "new-source".into());
        assert_eq!(state.current_paper_state(&"t".into()), crate::CurrentCheckState::Unknown);
        state.paper_status.insert("t".into(), crate::CorrStatus::Pass);
        state.paper_approved_fingerprints.insert("t".into(), "new-source".into());
        assert_eq!(state.current_paper_state(&"t".into()), crate::CurrentCheckState::Pass);
        state.deps.clear();
        assert_eq!(state.current_paper_state(&"t".into()), crate::CurrentCheckState::Fail);
    }

    #[test]
    fn paper_implementation_rust_root_cannot_be_an_authored_idealization() {
        let mut state = state();
        state.paper_implementation.as_mut().unwrap().route = PaperImplementationRoute::RustAeneas;
        state.live.present_nodes = BTreeSet::from(["Impl".into(), "ExtractedOther".into(), "Target".into()]);
        state.node_kinds.insert("Impl".into(), crate::NodeKind::Definition);
        state.node_kinds.insert("ExtractedOther".into(), crate::NodeKind::Definition);
        state.node_role.insert("ExtractedOther".into(), PvRole::ExtractionModel);
        state.live.coverage.insert("t".into(), BTreeSet::from(["Target".into()]));
        state.deps.insert("Target".into(), BTreeSet::from(["Impl".into()]));
        assert!(!target_has_implementation_roots(&state, &"t".into()));
        state.node_role.insert("Impl".into(), PvRole::ExtractionModel);
        assert!(!target_has_implementation_roots(&state, &"t".into()), "a role without registered extraction identity is insufficient");
    }

    #[test]
    fn paper_implementation_route_tasks_authorize_only_explicit_choices() {
        let mut subject = test_subject(PaperImplementationRoute::RustAeneas);
        for name in ["A", "B"] {
            subject.modeling_choices.push(PaperModelingChoice { node: name.into(), source_locator: "crate/lib.rs:3".into(),
                rationale: "Missing operation binding".into(), evidence_path: "evidence/model.txt".into(), consequential: true, route_decision: None });
        }
        let resolution = |id: &str| crate::ModelingResolution::Approve { scope: "Exactly the named workaround".into(), limitations: String::new(),
            tasks: vec![crate::AuditTask { id: id.into(), title: "Implement named choice".into(), body: "Preserve its exact source operation".into(), ..Default::default() }],
            declined_targets: BTreeSet::new(), reopen_setup: false };
        assert!(approved_route_choices(&subject, &resolution("unrelated-task")).unwrap().is_empty());
        assert_eq!(approved_route_choices(&subject, &resolution("paper-model:A")).unwrap(), BTreeSet::from(["A".into()]));
        assert!(approved_route_choices(&subject, &resolution("paper-model:Unknown")).is_err());
        let declared = BTreeSet::from(["A".into(), "B".into()]);
        for id in ["unrelated-task", "paper-model:A", "paper-model:Unknown"] {
            assert_eq!(approved_route_choices(&subject, &resolution(id)), approved_route_choice_ids(&declared, &resolution(id)),
                "native and preparation must apply identical task-to-choice identity checks");
        }
        subject.modeling_choices.push(PaperModelingChoice { node: "OrdinaryHelper".into(), source_locator: "paper:12".into(),
            rationale: "Ordinary proof support".into(), evidence_path: "paper/main.tex".into(), consequential: false, route_decision: None });
        assert!(approved_route_choices(&subject, &resolution("paper-model:OrdinaryHelper")).is_err(),
            "ordinary proof helpers are not consequential exception inventory");
        let mut review = crate::ModelingRouteReview::default();
        review.attempts = vec![crate::ModelingRouteAttempt { route_tex: "First route paper-model:A".into(), critic_decision: "accept".into(), critic_report: "Checked A".into(), critic_feedback: String::new() },
            crate::ModelingRouteAttempt { route_tex: "Second route paper-model:AB".into(), critic_decision: "accept".into(), critic_report: "Checked AB".into(), critic_feedback: String::new() }];
        let selected = BTreeSet::from(["A".into()]);
        assert!(!route_choices_reviewed(&review, &selected), "prefixes cannot impersonate exact choice IDs");
        review.attempts[1].route_tex = "Second route `paper-model:A`".into();
        assert!(route_choices_reviewed(&review, &selected));
        let mut state = state(); state.paper_implementation = Some(subject);
        state.modeling_route_decisions.push(json!({"resolution":{"action":"approve"}, "attempts":[{},{}],
            "paper_implementation_basis": approval_basis(state.paper_implementation.as_ref().unwrap()), "paper_modeling_choices":["A"]}));
        assert!(choice_has_route(&state, &"A".into()));
        assert!(!choice_has_route(&state, &"B".into()));
    }

    #[test]
    fn paper_implementation_manual_authoring_requires_actual_current_route() {
        let mut state = state();
        let subject = state.paper_implementation.as_mut().unwrap();
        subject.route = PaperImplementationRoute::RustAeneas;
        subject.modeling_choices.push(PaperModelingChoice { node: "Model".into(), source_locator: "crate/lib.rs:3".into(),
            rationale: "Missing operation binding".into(), evidence_path: "evidence/model.txt".into(), consequential: true, route_decision: Some(0) });
        let changed = BTreeSet::from(["Model".into()]);
        assert_eq!(manual_authoring_errors(&state, &changed).len(), 1);
        state.modeling_route_decisions.push(json!({"resolution":{"action":"approve"}, "attempts":[{},{}],
            "paper_implementation_basis": approval_basis(state.paper_implementation.as_ref().unwrap()),
            "paper_modeling_choices":["Model"]}));
        assert!(manual_authoring_errors(&state, &changed).is_empty());
        state.paper_implementation.as_mut().unwrap().configuration_sha256 = "d".repeat(64);
        assert_eq!(manual_authoring_errors(&state, &changed).len(), 1);
    }
}
