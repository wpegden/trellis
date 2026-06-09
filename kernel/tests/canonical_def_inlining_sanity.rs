//! Production-like prompt-fragment sanity check: assemble contract payloads
//! for representative role/validation_kind/phase scenarios and assert that
//! the right `canonical/<NAME>.md` fragments are inlined per the matrix in
//! project_canonical_def_inlining_plan.md.

use trellis_kernel::model::{
    Phase, RequestKind, WorkerContext, WorkerContextMode, WorkerProfile, WorkerValidationKind,
    WorkerWorkStyleHint, WrapperRequest,
};
use trellis_kernel::request_contracts::{
    correspondence_contract_payload, paper_contract_payload, review_contract_payload,
    soundness_contract_payload, worker_contract_payload,
};

const FAITH: &str = "canonical/FAITHFULNESS.md";
const SUBST: &str = "canonical/SUBSTANTIVENESS.md";
const CORR: &str = "canonical/CORRESPONDENCE.md";
const SOUND: &str = "canonical/SOUNDNESS.md";

fn fragments_of(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("prompt_fragments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_contains(fragments: &[String], expected: &str, role: &str) {
    assert!(
        fragments.iter().any(|f| f == expected),
        "{} prompt is missing {}; got fragments: {:?}",
        role,
        expected,
        fragments
    );
}

fn assert_not_contains(fragments: &[String], unexpected: &str, role: &str) {
    assert!(
        fragments.iter().all(|f| f != unexpected),
        "{} prompt has unexpected {}; got fragments: {:?}",
        role,
        unexpected,
        fragments
    );
}

fn assert_no_duplicates(fragments: &[String], role: &str) {
    let mut seen = std::collections::BTreeSet::new();
    for f in fragments {
        if !seen.insert(f.clone()) {
            panic!(
                "{} prompt has duplicate fragment {}; got fragments: {:?}",
                role, f, fragments
            );
        }
    }
}

fn worker_request(
    profile: WorkerProfile,
    validation: WorkerValidationKind,
    phase: Phase,
) -> WrapperRequest {
    WrapperRequest {
        kind: RequestKind::Worker,
        phase,
        worker_context: WorkerContext {
            worker_profile: profile,
            validation_kind: validation,
            next_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::None,
            ..WorkerContext::default()
        },
        ..WrapperRequest::default()
    }
}

#[test]
fn worker_theorem_global_sees_all_four_canonical_defs() {
    let req = worker_request(
        WorkerProfile::Theorem,
        WorkerValidationKind::TheoremGlobal,
        Phase::TheoremStating,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_contains(&fragments, FAITH, "worker/TheoremGlobal");
    assert_contains(&fragments, SUBST, "worker/TheoremGlobal");
    assert_contains(&fragments, CORR, "worker/TheoremGlobal");
    assert_contains(&fragments, SOUND, "worker/TheoremGlobal");
    assert_no_duplicates(&fragments, "worker/TheoremGlobal");
}

#[test]
fn worker_theorem_targeted_sees_all_four_canonical_defs() {
    let req = worker_request(
        WorkerProfile::Theorem,
        WorkerValidationKind::TheoremTargeted,
        Phase::TheoremStating,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_contains(&fragments, FAITH, "worker/TheoremTargeted");
    assert_contains(&fragments, SUBST, "worker/TheoremTargeted");
    assert_contains(&fragments, CORR, "worker/TheoremTargeted");
    assert_contains(&fragments, SOUND, "worker/TheoremTargeted");
}

#[test]
fn worker_proof_easy_sees_proof_lane_canonical_defs() {
    let req = worker_request(
        WorkerProfile::ProofEasy,
        WorkerValidationKind::ProofEasy,
        Phase::ProofFormalization,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/ProofEasy");
    assert_contains(&fragments, SUBST, "worker/ProofEasy");
    assert_contains(&fragments, CORR, "worker/ProofEasy");
    assert_contains(&fragments, SOUND, "worker/ProofEasy");
}

#[test]
fn worker_proof_local_sees_substantiveness_corr_soundness() {
    let req = worker_request(
        WorkerProfile::ProofHard,
        WorkerValidationKind::ProofLocal,
        Phase::ProofFormalization,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/ProofLocal");
    assert_contains(&fragments, SUBST, "worker/ProofLocal");
    assert_contains(&fragments, CORR, "worker/ProofLocal");
    assert_contains(&fragments, SOUND, "worker/ProofLocal");
}

#[test]
fn worker_proof_restructure_sees_substantiveness_corr_soundness() {
    let req = worker_request(
        WorkerProfile::ProofHard,
        WorkerValidationKind::ProofRestructure,
        Phase::ProofFormalization,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/ProofRestructure");
    assert_contains(&fragments, SUBST, "worker/ProofRestructure");
    assert_contains(&fragments, CORR, "worker/ProofRestructure");
    assert_contains(&fragments, SOUND, "worker/ProofRestructure");
}

#[test]
fn worker_proof_coarse_restructure_sees_substantiveness_corr_soundness() {
    let req = worker_request(
        WorkerProfile::ProofHard,
        WorkerValidationKind::ProofCoarseRestructure,
        Phase::ProofFormalization,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/ProofCoarseRestructure");
    assert_contains(&fragments, SUBST, "worker/ProofCoarseRestructure");
    assert_contains(&fragments, CORR, "worker/ProofCoarseRestructure");
    assert_contains(&fragments, SOUND, "worker/ProofCoarseRestructure");
}

#[test]
fn worker_cleanup_sees_no_canonical_defs() {
    let req = worker_request(
        WorkerProfile::Cleanup,
        WorkerValidationKind::Cleanup,
        Phase::Cleanup,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/Cleanup");
    assert_not_contains(&fragments, SUBST, "worker/Cleanup");
    assert_not_contains(&fragments, CORR, "worker/Cleanup");
    assert_not_contains(&fragments, SOUND, "worker/Cleanup");
}

#[test]
fn worker_final_cleanup_sees_no_canonical_defs() {
    let req = worker_request(
        WorkerProfile::FinalCleanup,
        WorkerValidationKind::FinalCleanup,
        Phase::Cleanup,
    );
    let fragments = fragments_of(&worker_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "worker/FinalCleanup");
    assert_not_contains(&fragments, SUBST, "worker/FinalCleanup");
    assert_not_contains(&fragments, CORR, "worker/FinalCleanup");
    assert_not_contains(&fragments, SOUND, "worker/FinalCleanup");
}

fn review_request(phase: Phase) -> WrapperRequest {
    WrapperRequest {
        kind: RequestKind::Review,
        phase,
        ..WrapperRequest::default()
    }
}

#[test]
fn reviewer_theorem_stating_sees_all_four_canonical_defs() {
    let req = review_request(Phase::TheoremStating);
    let fragments = fragments_of(&review_contract_payload(&req));
    assert_contains(&fragments, FAITH, "reviewer/TheoremStating");
    assert_contains(&fragments, SUBST, "reviewer/TheoremStating");
    assert_contains(&fragments, CORR, "reviewer/TheoremStating");
    assert_contains(&fragments, SOUND, "reviewer/TheoremStating");
}

#[test]
fn reviewer_proof_formalization_sees_substantiveness_corr_soundness() {
    let req = review_request(Phase::ProofFormalization);
    let fragments = fragments_of(&review_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "reviewer/ProofFormalization");
    assert_contains(&fragments, SUBST, "reviewer/ProofFormalization");
    assert_contains(&fragments, CORR, "reviewer/ProofFormalization");
    assert_contains(&fragments, SOUND, "reviewer/ProofFormalization");
}

#[test]
fn reviewer_cleanup_sees_no_canonical_defs() {
    let req = review_request(Phase::Cleanup);
    let fragments = fragments_of(&review_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "reviewer/Cleanup");
    assert_not_contains(&fragments, SUBST, "reviewer/Cleanup");
    assert_not_contains(&fragments, CORR, "reviewer/Cleanup");
    assert_not_contains(&fragments, SOUND, "reviewer/Cleanup");
}

#[test]
fn verifier_paper_target_scenario_sees_only_faithfulness() {
    use trellis_kernel::model::TargetId;
    let mut req = WrapperRequest {
        kind: RequestKind::Paper,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    req.paper_verify_targets = std::iter::once(TargetId::from("T1")).collect();
    let fragments = fragments_of(&paper_contract_payload(&req));
    assert_contains(&fragments, FAITH, "verifier/paper-target");
    assert_not_contains(&fragments, SUBST, "verifier/paper-target");
    assert_not_contains(&fragments, CORR, "verifier/paper-target");
    assert_not_contains(&fragments, SOUND, "verifier/paper-target");
}

#[test]
fn verifier_substantiveness_per_node_scenario_sees_only_substantiveness() {
    use trellis_kernel::model::NodeId;
    let mut req = WrapperRequest {
        kind: RequestKind::Paper,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    req.substantiveness_verify_nodes = std::iter::once(NodeId::from("N1")).collect();
    let fragments = fragments_of(&paper_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "verifier/substantiveness");
    assert_contains(&fragments, SUBST, "verifier/substantiveness");
    assert_not_contains(&fragments, CORR, "verifier/substantiveness");
    assert_not_contains(&fragments, SOUND, "verifier/substantiveness");
}

#[test]
fn verifier_correspondence_sees_only_correspondence() {
    use trellis_kernel::model::NodeId;
    let mut req = WrapperRequest {
        kind: RequestKind::Corr,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    req.verify_nodes = std::iter::once(NodeId::from("N1")).collect();
    let fragments = fragments_of(&correspondence_contract_payload(&req, None));
    assert_not_contains(&fragments, FAITH, "verifier/correspondence");
    assert_not_contains(&fragments, SUBST, "verifier/correspondence");
    assert_contains(&fragments, CORR, "verifier/correspondence");
    assert_not_contains(&fragments, SOUND, "verifier/correspondence");
}

#[test]
fn verifier_soundness_sees_only_soundness() {
    use trellis_kernel::model::NodeId;
    let mut req = WrapperRequest {
        kind: RequestKind::Sound,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    let node = NodeId::from("N1");
    req.sound_verify_node = Some(node.clone());
    req.sound_verify_nodes = std::iter::once(node).collect();
    let fragments = fragments_of(&soundness_contract_payload(&req));
    assert_not_contains(&fragments, FAITH, "verifier/soundness");
    assert_not_contains(&fragments, SUBST, "verifier/soundness");
    assert_not_contains(&fragments, CORR, "verifier/soundness");
    assert_contains(&fragments, SOUND, "verifier/soundness");
}

/// Sweep every realistic scenario and assert no fragment path
/// appears more than once in the assembled `prompt_fragments` list.
/// The bridge renders fragments by reading each file in order; a
/// duplicate would cause the same content to appear twice in the
/// prompt the agent sees, wasting tokens and creating internal
/// inconsistency if the fragment is ever templated differently.
#[test]
fn no_duplicate_fragments_in_any_assembled_prompt() {
    use trellis_kernel::model::{NodeId, TargetId};
    let scenarios: Vec<(&str, Box<dyn Fn() -> Vec<String>>)> = vec![
        (
            "worker/TheoremGlobal",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::Theorem,
                    WorkerValidationKind::TheoremGlobal,
                    Phase::TheoremStating,
                )))
            }),
        ),
        (
            "worker/TheoremTargeted",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::Theorem,
                    WorkerValidationKind::TheoremTargeted,
                    Phase::TheoremStating,
                )))
            }),
        ),
        (
            "worker/ProofEasy",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::ProofEasy,
                    WorkerValidationKind::ProofEasy,
                    Phase::ProofFormalization,
                )))
            }),
        ),
        (
            "worker/ProofLocal",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::ProofHard,
                    WorkerValidationKind::ProofLocal,
                    Phase::ProofFormalization,
                )))
            }),
        ),
        (
            "worker/ProofRestructure",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::ProofHard,
                    WorkerValidationKind::ProofRestructure,
                    Phase::ProofFormalization,
                )))
            }),
        ),
        (
            "worker/ProofCoarseRestructure",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::ProofHard,
                    WorkerValidationKind::ProofCoarseRestructure,
                    Phase::ProofFormalization,
                )))
            }),
        ),
        (
            "worker/Cleanup",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::Cleanup,
                    WorkerValidationKind::Cleanup,
                    Phase::Cleanup,
                )))
            }),
        ),
        (
            "worker/FinalCleanup",
            Box::new(|| {
                fragments_of(&worker_contract_payload(&worker_request(
                    WorkerProfile::FinalCleanup,
                    WorkerValidationKind::FinalCleanup,
                    Phase::Cleanup,
                )))
            }),
        ),
        (
            "reviewer/TheoremStating",
            Box::new(|| {
                fragments_of(&review_contract_payload(&review_request(
                    Phase::TheoremStating,
                )))
            }),
        ),
        (
            "reviewer/ProofFormalization",
            Box::new(|| {
                fragments_of(&review_contract_payload(&review_request(
                    Phase::ProofFormalization,
                )))
            }),
        ),
        (
            "reviewer/Cleanup",
            Box::new(|| fragments_of(&review_contract_payload(&review_request(Phase::Cleanup)))),
        ),
        (
            "verifier/paper-target",
            Box::new(|| {
                let mut req = WrapperRequest {
                    kind: RequestKind::Paper,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                req.paper_verify_targets = std::iter::once(TargetId::from("T1")).collect();
                fragments_of(&paper_contract_payload(&req))
            }),
        ),
        (
            "verifier/substantiveness",
            Box::new(|| {
                let mut req = WrapperRequest {
                    kind: RequestKind::Paper,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                req.substantiveness_verify_nodes = std::iter::once(NodeId::from("N1")).collect();
                fragments_of(&paper_contract_payload(&req))
            }),
        ),
        (
            "verifier/correspondence",
            Box::new(|| {
                let mut req = WrapperRequest {
                    kind: RequestKind::Corr,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                req.verify_nodes = std::iter::once(NodeId::from("N1")).collect();
                fragments_of(&correspondence_contract_payload(&req, None))
            }),
        ),
        (
            "verifier/soundness",
            Box::new(|| {
                let mut req = WrapperRequest {
                    kind: RequestKind::Sound,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                let node = NodeId::from("N1");
                req.sound_verify_node = Some(node.clone());
                req.sound_verify_nodes = std::iter::once(node).collect();
                fragments_of(&soundness_contract_payload(&req))
            }),
        ),
    ];
    for (role, builder) in scenarios {
        let fragments = builder();
        assert_no_duplicates(&fragments, role);
    }
}
