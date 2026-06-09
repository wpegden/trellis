// Integration tests for the per-node substantiveness path through
// `normalize_paper_response`. Exercises the new `verdicts[]` schema and
// the missing-from-response default flip (silence → NotDoneYet).
//
// Lives as an integration test because the kernel's lib `mod tests`
// modules are blocked by pre-existing K-8 NodeId/TargetId migration
// breakage (out of scope for the verdicts[] overhaul).

use std::collections::BTreeSet;

use trellis_kernel::{
    normalize_paper_response, NodeId, PaperLaneMemberInput, PaperNormalizationInput,
    RawPaperLanePayload, RawSubstantivenessPhasePayload, RawSubstantivenessVerdict,
    SubstantivenessStatus, Update,
};

#[test]
fn per_node_buckets_explicit_pass_fail_not_done_yet_verdicts() {
    let output = normalize_paper_response(&PaperNormalizationInput {
        request_id: 11,
        cycle: 5,
        verify_lanes: BTreeSet::from(["v1".to_string()]),
        verify_targets: BTreeSet::new(),
        verify_nodes: BTreeSet::from([
            NodeId::from("NodeA"),
            NodeId::from("NodeB"),
            NodeId::from("NodeC"),
        ]),
        verify_deviations: BTreeSet::new(),
        members: vec![PaperLaneMemberInput {
            lane_id: "v1".to_string(),
            ok: true,
            payload: Some(RawPaperLanePayload {
                substantiveness: RawSubstantivenessPhasePayload {
                    decision: "FAIL".to_string(),
                    verdicts: vec![
                        RawSubstantivenessVerdict {
                            node: "NodeA".to_string(),
                            verdict: "Fail".to_string(),
                            comment: "no paper basis".to_string(),
                        },
                        RawSubstantivenessVerdict {
                            node: "NodeB".to_string(),
                            verdict: "NotDoneYet".to_string(),
                            comment: "didn't have time".to_string(),
                        },
                        RawSubstantivenessVerdict {
                            node: "NodeC".to_string(),
                            verdict: "Pass".to_string(),
                            comment: String::new(),
                        },
                    ],
                },
                overall: "REJECT".to_string(),
                summary: "v1 finds A failing, triages B, passes C".to_string(),
                comments: String::new(),
                ..RawPaperLanePayload::default()
            }),
            error: String::new(),
        }],
    })
    .expect("normalize ok");

    let v1 = &output.response.node_lane_updates["v1"];
    assert_eq!(
        v1[&NodeId::from("NodeA")],
        Update::Set(SubstantivenessStatus::Fail)
    );
    assert_eq!(
        v1[&NodeId::from("NodeB")],
        Update::Set(SubstantivenessStatus::NotDoneYet)
    );
    assert_eq!(
        v1[&NodeId::from("NodeC")],
        Update::Set(SubstantivenessStatus::Pass)
    );
}

#[test]
fn missing_node_from_response_defaults_to_not_done_yet() {
    // The default flip: a node from `verify_nodes` that the verifier
    // omits entirely is now NotDoneYet, not Pass. This forces the
    // verifier to emit explicit verdicts and prevents accidental
    // Pass-by-omission.
    let output = normalize_paper_response(&PaperNormalizationInput {
        request_id: 12,
        cycle: 6,
        verify_lanes: BTreeSet::from(["v1".to_string()]),
        verify_targets: BTreeSet::new(),
        verify_nodes: BTreeSet::from([
            NodeId::from("NodeA"),
            NodeId::from("NodeB"),
            NodeId::from("NodeC"),
        ]),
        verify_deviations: BTreeSet::new(),
        members: vec![PaperLaneMemberInput {
            lane_id: "v1".to_string(),
            ok: true,
            payload: Some(RawPaperLanePayload {
                substantiveness: RawSubstantivenessPhasePayload {
                    decision: "PASS".to_string(),
                    // Only NodeA is in the response; NodeB and NodeC are
                    // omitted. They should default to NotDoneYet under
                    // the flipped default.
                    verdicts: vec![RawSubstantivenessVerdict {
                        node: "NodeA".to_string(),
                        verdict: "Pass".to_string(),
                        comment: String::new(),
                    }],
                },
                overall: "APPROVE".to_string(),
                summary: "lane covers only A this round".to_string(),
                comments: String::new(),
                ..RawPaperLanePayload::default()
            }),
            error: String::new(),
        }],
    })
    .expect("normalize ok");

    let v1 = &output.response.node_lane_updates["v1"];
    assert_eq!(
        v1[&NodeId::from("NodeA")],
        Update::Set(SubstantivenessStatus::Pass)
    );
    assert_eq!(
        v1[&NodeId::from("NodeB")],
        Update::Set(SubstantivenessStatus::NotDoneYet),
        "missing node should default to NotDoneYet, not Pass"
    );
    assert_eq!(
        v1[&NodeId::from("NodeC")],
        Update::Set(SubstantivenessStatus::NotDoneYet),
        "missing node should default to NotDoneYet, not Pass"
    );
}

#[test]
fn per_node_evidence_excludes_pass_verdicts() {
    // Pass verdicts must NOT contribute per-node reviewer evidence even
    // when the verifier supplies a comment on Pass — lane summary
    // suffices for Pass nodes. Fail and NotDoneYet verdicts with
    // non-empty comments do contribute.
    let output = normalize_paper_response(&PaperNormalizationInput {
        request_id: 13,
        cycle: 7,
        verify_lanes: BTreeSet::from(["v1".to_string()]),
        verify_targets: BTreeSet::new(),
        verify_nodes: BTreeSet::from([
            NodeId::from("NodeA"),
            NodeId::from("NodeB"),
            NodeId::from("NodeC"),
        ]),
        verify_deviations: BTreeSet::new(),
        members: vec![PaperLaneMemberInput {
            lane_id: "v1".to_string(),
            ok: true,
            payload: Some(RawPaperLanePayload {
                substantiveness: RawSubstantivenessPhasePayload {
                    decision: "FAIL".to_string(),
                    verdicts: vec![
                        RawSubstantivenessVerdict {
                            node: "NodeA".to_string(),
                            verdict: "Pass".to_string(),
                            comment: "Pass-with-comment, should not surface in per-node evidence"
                                .to_string(),
                        },
                        RawSubstantivenessVerdict {
                            node: "NodeB".to_string(),
                            verdict: "Fail".to_string(),
                            comment: "merge with NodeC".to_string(),
                        },
                        RawSubstantivenessVerdict {
                            node: "NodeC".to_string(),
                            verdict: "NotDoneYet".to_string(),
                            comment: "ran out of time".to_string(),
                        },
                    ],
                },
                overall: "REJECT".to_string(),
                summary: "mixed".to_string(),
                comments: String::new(),
                ..RawPaperLanePayload::default()
            }),
            error: String::new(),
        }],
    })
    .expect("normalize ok");

    assert!(
        !output
            .response
            .node_reviewer_evidence
            .contains_key(&NodeId::from("NodeA")),
        "Pass with comment should not produce per-node evidence"
    );
    assert!(
        output
            .response
            .node_reviewer_evidence
            .contains_key(&NodeId::from("NodeB")),
        "Fail with comment should produce per-node evidence"
    );
    assert!(
        output
            .response
            .node_reviewer_evidence
            .contains_key(&NodeId::from("NodeC")),
        "NotDoneYet with comment should produce per-node evidence"
    );
}

#[test]
fn target_scenario_unchanged_uses_paper_faithfulness_block() {
    // Target-level scenarios continue to use the existing
    // `paper_faithfulness` issues[] schema unchanged. Only per-node
    // scenarios are affected by the verdicts[] overhaul.
    use trellis_kernel::{CorrStatus, TargetId, VerifierIssue};

    let output = normalize_paper_response(&PaperNormalizationInput {
        request_id: 7,
        cycle: 3,
        verify_lanes: BTreeSet::from(["v1".to_string()]),
        verify_targets: BTreeSet::from([TargetId::from("target_a")]),
        verify_nodes: BTreeSet::new(),
        verify_deviations: BTreeSet::new(),
        members: vec![PaperLaneMemberInput {
            lane_id: "v1".to_string(),
            ok: true,
            payload: Some(RawPaperLanePayload {
                paper_faithfulness: trellis_kernel::RawCorrPhasePayload {
                    decision: "FAIL".to_string(),
                    issues: vec![VerifierIssue {
                        node: NodeId::from("target_a"),
                        description: "coverage mismatch".to_string(),
                    }],
                    ..Default::default()
                },
                overall: "REJECT".to_string(),
                summary: "lane 1 rejects".to_string(),
                comments: String::new(),
                ..RawPaperLanePayload::default()
            }),
            error: String::new(),
        }],
    })
    .expect("normalize ok");

    assert_eq!(
        output.response.target_lane_updates["v1"][&TargetId::from("target_a")],
        Update::Set(CorrStatus::Fail)
    );
}
