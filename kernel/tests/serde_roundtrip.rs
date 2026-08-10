//! Property tests for the Serde round-trip on wire-format types.
//!
//! ## Why this file
//!
//! The wire format is spelled three times across `request_contracts.rs`,
//! `runtime_cli_observations.rs`, and `bin/runtime_cli.rs`. Any
//! refactor that consolidates them needs to preserve the property:
//!
//!   for every value `v` of the type:
//!     v == from_json(to_json(v))
//!     normalize(to_json(from_json(s))) == normalize(s)
//!
//! ## How
//!
//! For each Serializable + Deserializable type in scope, we build a
//! `proptest::Strategy` that generates representative values (small
//! enough to be fast; varied enough to catch field-dropping), then
//! assert `from_str(to_string(v)) == v`. Catches:
//!
//!   * Field accidentally renamed in struct but not in `#[serde(rename)]`.
//!   * Field accidentally dropped from struct.
//!   * Enum variant tag changed during refactor.
//!   * Default value drift on `#[serde(default)]` fields.
//!   * Collection ordering becoming non-canonical (BTreeMap vs HashMap).
//!
//! ## Scope
//!
//! This file targets the wire-format primitives — the data shapes the
//! refactor will reshuffle most. The big top-level envelopes
//! (`ProtocolState`, `WrapperRequest`, `WrapperResponse`) are exercised
//! via the snapshot suite in `runtime_cli_snapshots.rs` (representative
//! inputs) and the TLA replay test (whole-state-vector equality), so
//! the proptest layer here covers their building blocks.
//!
//! Strategy depth is intentionally bounded: deeply nested random
//! structures become CPU-expensive without buying coverage. Per-type
//! cases: 64 by default. Override with `PROPTEST_CASES=N`.

use proptest::collection::{btree_set, vec as pvec};
use proptest::option;
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use trellis_kernel::{
    AuditNormalizationOutput, AuditResponse, AuditTask, Blocker, BlockerKind, BlockerObject,
    CorrLaneMemberInput, CorrNormalizationInput, CorrNormalizationOutput, CorrResponse,
    CorrStatus, Fingerprint, GateKind, HumanChoice, HumanGateResponse, LaneId,
    NodeDifficulty, NodeId, NodeKind, PaperFocusRange, PaperGrounding, PaperLaneMemberInput,
    PaperNormalizationInput, PaperNormalizationOutput, PaperResponse, Phase, PvRole,
    RawAuditPayload,
    RawCorrPayload, RawCorrPhasePayload, RawCorrVerdict, RawDeviationPhasePayload,
    RawPaperLanePayload, RawPaperPayload, RawReviewPayload, RawSoundPayload,
    RawSoundnessPayload, RawSubstantivenessPayload, RawSubstantivenessPhasePayload,
    RawSubstantivenessVerdict, RequestKind, ResetChoice, ResponseStatus, ReviewDecisionKind,
    ReviewNormalizationOutput, ReviewResponse, SoundLaneMemberInput, SoundNormalizationInput,
    SoundNormalizationOutput, SoundResponse, SoundStatus, Stage, StuckMathAuditResponse,
    SubstantivenessStatus, TargetId, TaskMode, Update, WorkerNormalizationOutput, WorkerOutcome,
    WorkerProfile, WorkerResponse, WorkerValidationKind, WrapperRequest, WrapperResponse,
};

// ============================================================================
// Property-test helpers: round-trip property + JSON-normalization property
// ============================================================================

/// Assert `from_json(to_json(value)) == value`. Panics on inequality with
/// a side-by-side diff for debugging.
fn assert_value_roundtrip<T>(value: &T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let parsed: T = serde_json::from_str(&json)
        .unwrap_or_else(|err| panic!("deserialize failed: {err}\njson:\n{}", json));
    assert_eq!(
        &parsed, value,
        "round-trip mismatch\nleft (parsed):  {:#?}\nright (value):  {:#?}\nintermediate json: {}",
        parsed, value, json
    );
}

/// Pretty-print a JSON value with stable key ordering.
fn normalize_json(v: serde_json::Value) -> String {
    fn sort_keys(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut sorted: BTreeMap<String, serde_json::Value> = BTreeMap::new();
                for (k, v) in map {
                    sorted.insert(k, sort_keys(v));
                }
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(sort_keys).collect())
            }
            other => other,
        }
    }
    serde_json::to_string(&sort_keys(v)).expect("serialize")
}

/// Stronger property: `to_json(from_json(s)).normalized() ==
/// s.normalized()`. Requires that `s` only carries known fields (no
/// unknown-field cruft for which the type discards info on parse).
fn assert_json_roundtrip<T>(value: &T)
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
{
    let json1 = serde_json::to_value(value).expect("serialize to value");
    let parsed: T = serde_json::from_value(json1.clone()).expect("deserialize");
    let json2 = serde_json::to_value(&parsed).expect("re-serialize");
    assert_eq!(
        normalize_json(json1.clone()),
        normalize_json(json2.clone()),
        "json round-trip mismatch\njson1:  {}\njson2:  {}",
        json1,
        json2,
    );
}

// ============================================================================
// Leaf strategies: enums, primitives, small bags
// ============================================================================

fn nonempty_ident() -> impl Strategy<Value = String> {
    // Use a narrow charset so the generator stays fast and the values
    // are still distinct enough to catch field-mixing bugs.
    "[a-zA-Z][a-zA-Z0-9_]{0,8}".prop_map(String::from)
}

fn small_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ._-]{0,20}".prop_map(String::from)
}

fn node_id_strategy() -> impl Strategy<Value = NodeId> {
    nonempty_ident().prop_map(NodeId::from)
}

fn target_id_strategy() -> impl Strategy<Value = TargetId> {
    nonempty_ident().prop_map(TargetId::from)
}

fn lane_id_strategy() -> impl Strategy<Value = LaneId> {
    nonempty_ident().prop_map(LaneId::from)
}

fn fingerprint_strategy() -> impl Strategy<Value = Fingerprint> {
    "[a-f0-9]{0,16}".prop_map(|s| Fingerprint::from(s.to_string()))
}

fn corr_status_strategy() -> impl Strategy<Value = CorrStatus> {
    prop_oneof![
        Just(CorrStatus::Pass),
        Just(CorrStatus::Fail),
        Just(CorrStatus::Unknown),
    ]
}

fn sound_status_strategy() -> impl Strategy<Value = SoundStatus> {
    prop_oneof![
        Just(SoundStatus::Pass),
        Just(SoundStatus::Fail),
        Just(SoundStatus::Unknown),
        Just(SoundStatus::Structural),
    ]
}

fn substantiveness_status_strategy() -> impl Strategy<Value = SubstantivenessStatus> {
    prop_oneof![
        Just(SubstantivenessStatus::Pass),
        Just(SubstantivenessStatus::Fail),
        Just(SubstantivenessStatus::Unknown),
    ]
}

fn phase_strategy() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::TheoremStating),
        Just(Phase::ProofFormalization),
        Just(Phase::Cleanup),
        Just(Phase::Complete),
    ]
}

fn stage_strategy() -> impl Strategy<Value = Stage> {
    prop_oneof![
        Just(Stage::Start),
        Just(Stage::Worker),
        Just(Stage::Reviewer),
        Just(Stage::HumanGate),
        Just(Stage::VerifyPaper),
        Just(Stage::VerifyCorr),
        Just(Stage::VerifySound),
        Just(Stage::Complete),
        Just(Stage::StuckMathAudit),
    ]
}

fn worker_outcome_strategy() -> impl Strategy<Value = WorkerOutcome> {
    prop_oneof![
        Just(WorkerOutcome::Valid),
        Just(WorkerOutcome::Invalid),
        Just(WorkerOutcome::Stuck),
        Just(WorkerOutcome::NeedsRestructure),
    ]
}

fn response_status_strategy() -> impl Strategy<Value = ResponseStatus> {
    prop_oneof![Just(ResponseStatus::Ok), Just(ResponseStatus::Malformed),]
}

fn review_decision_strategy() -> impl Strategy<Value = ReviewDecisionKind> {
    prop_oneof![
        Just(ReviewDecisionKind::Continue),
        Just(ReviewDecisionKind::NeedInput),
        Just(ReviewDecisionKind::AdvancePhase),
        Just(ReviewDecisionKind::Done),
    ]
}

fn request_kind_strategy() -> impl Strategy<Value = RequestKind> {
    prop_oneof![
        Just(RequestKind::Worker),
        Just(RequestKind::Review),
        Just(RequestKind::Corr),
        Just(RequestKind::Sound),
        Just(RequestKind::Paper),
        Just(RequestKind::HumanGate),
        Just(RequestKind::Audit),
        Just(RequestKind::StuckMathAudit),
    ]
}

fn task_mode_strategy() -> impl Strategy<Value = TaskMode> {
    prop_oneof![
        Just(TaskMode::Global),
        Just(TaskMode::Targeted),
        Just(TaskMode::Local),
        Just(TaskMode::Restructure),
        Just(TaskMode::CoarseRestructure),
        Just(TaskMode::Cleanup),
    ]
}

fn node_kind_strategy() -> impl Strategy<Value = NodeKind> {
    prop_oneof![
        Just(NodeKind::Preamble),
        Just(NodeKind::Definition),
        Just(NodeKind::Proof),
    ]
}

fn pv_role_strategy() -> impl Strategy<Value = PvRole> {
    prop_oneof![
        Just(PvRole::Spec),
        Just(PvRole::ExtractionModel),
        Just(PvRole::ExternalModel),
        Just(PvRole::Invariant),
        Just(PvRole::Safety),
        Just(PvRole::Correctness),
        Just(PvRole::LibraryLemma),
    ]
}

fn node_difficulty_strategy() -> impl Strategy<Value = NodeDifficulty> {
    prop_oneof![Just(NodeDifficulty::Easy), Just(NodeDifficulty::Hard),]
}

fn worker_profile_strategy() -> impl Strategy<Value = WorkerProfile> {
    prop_oneof![
        Just(WorkerProfile::None),
        Just(WorkerProfile::Theorem),
        Just(WorkerProfile::ProofEasy),
        Just(WorkerProfile::ProofHard),
        Just(WorkerProfile::Cleanup),
        Just(WorkerProfile::FinalCleanup),
    ]
}

fn worker_validation_kind_strategy() -> impl Strategy<Value = WorkerValidationKind> {
    prop_oneof![
        Just(WorkerValidationKind::None),
        Just(WorkerValidationKind::TheoremGlobal),
        Just(WorkerValidationKind::TheoremTargeted),
        Just(WorkerValidationKind::ProofEasy),
        Just(WorkerValidationKind::ProofLocal),
        Just(WorkerValidationKind::ProofRestructure),
        Just(WorkerValidationKind::ProofCoarseRestructure),
        Just(WorkerValidationKind::Cleanup),
        Just(WorkerValidationKind::FinalCleanup),
    ]
}

fn gate_kind_strategy() -> impl Strategy<Value = GateKind> {
    prop_oneof![
        Just(GateKind::None),
        Just(GateKind::Advance),
        Just(GateKind::NeedInput),
        Just(GateKind::ProtectedReapproval),
    ]
}

fn reset_choice_strategy() -> impl Strategy<Value = ResetChoice> {
    prop_oneof![
        Just(ResetChoice::None),
        Just(ResetChoice::LastCommit),
        Just(ResetChoice::LastClean),
        Just(ResetChoice::TheoremStatingNode),
    ]
}

fn human_choice_strategy() -> impl Strategy<Value = HumanChoice> {
    prop_oneof![Just(HumanChoice::Approve), Just(HumanChoice::Feedback),]
}

// ============================================================================
// Update<T> generic strategy (Same / Set update primitives)
// ============================================================================

fn update_strategy<T: Clone + std::fmt::Debug + 'static>(
    inner: impl Strategy<Value = T> + 'static,
) -> impl Strategy<Value = Update<T>>
where
    T: PartialEq,
{
    prop_oneof![inner.prop_map(Update::Set), Just(Update::Same),]
}

// ============================================================================
// Composite strategies: payloads and lane member inputs
// ============================================================================

fn raw_corr_verdict_strategy() -> impl Strategy<Value = RawCorrVerdict> {
    (small_string(), small_string(), small_string()).prop_map(|(node, verdict, comment)| {
        RawCorrVerdict {
            node,
            verdict,
            comment,
        }
    })
}

fn raw_corr_phase_strategy() -> impl Strategy<Value = RawCorrPhasePayload> {
    (
        small_string(),
        pvec(raw_corr_verdict_strategy(), 0..3),
        pvec((nonempty_ident(), small_string()), 0..3).prop_map(|tuples| {
            tuples
                .into_iter()
                .map(|(node, description)| trellis_kernel::VerifierIssue {
                    node: NodeId::from(node),
                    description,
                })
                .collect::<Vec<_>>()
        }),
    )
        .prop_map(|(decision, verdicts, issues)| RawCorrPhasePayload {
            decision,
            verdicts,
            issues,
        })
}

fn raw_corr_payload_strategy() -> impl Strategy<Value = RawCorrPayload> {
    (
        raw_corr_phase_strategy(),
        small_string(),
        small_string(),
        small_string(),
    )
        .prop_map(
            |(correspondence, overall, summary, comments)| RawCorrPayload {
                correspondence,
                overall,
                summary,
                comments,
            },
        )
}

fn raw_paper_payload_strategy() -> impl Strategy<Value = RawPaperPayload> {
    (
        raw_corr_phase_strategy(),
        small_string(),
        small_string(),
        small_string(),
    )
        .prop_map(|(pf, overall, summary, comments)| RawPaperPayload {
            paper_faithfulness: pf,
            overall,
            summary,
            comments,
        })
}

fn raw_substantiveness_verdict_strategy() -> impl Strategy<Value = RawSubstantivenessVerdict> {
    (small_string(), small_string(), small_string()).prop_map(|(node, verdict, comment)| {
        RawSubstantivenessVerdict {
            node,
            verdict,
            comment,
        }
    })
}

fn raw_substantiveness_phase_strategy() -> impl Strategy<Value = RawSubstantivenessPhasePayload> {
    (
        small_string(),
        pvec(raw_substantiveness_verdict_strategy(), 0..3),
    )
        .prop_map(|(decision, verdicts)| RawSubstantivenessPhasePayload {
            decision,
            verdicts,
        })
}

fn raw_substantiveness_payload_strategy() -> impl Strategy<Value = RawSubstantivenessPayload> {
    (
        raw_substantiveness_phase_strategy(),
        small_string(),
        small_string(),
        small_string(),
    )
        .prop_map(|(s, overall, summary, comments)| RawSubstantivenessPayload {
            substantiveness: s,
            overall,
            summary,
            comments,
        })
}

fn raw_deviation_phase_strategy() -> impl Strategy<Value = RawDeviationPhasePayload> {
    (small_string(), small_string(), small_string()).prop_map(|(id, decision, comment)| {
        RawDeviationPhasePayload {
            id,
            decision,
            comment,
        }
    })
}

fn raw_paper_lane_payload_strategy() -> impl Strategy<Value = RawPaperLanePayload> {
    (
        raw_corr_phase_strategy(),
        raw_substantiveness_phase_strategy(),
        raw_deviation_phase_strategy(),
        small_string(),
        small_string(),
        small_string(),
    )
        .prop_map(|(pf, s, d, overall, summary, comments)| RawPaperLanePayload {
            paper_faithfulness: pf,
            substantiveness: s,
            deviation_authorization: d,
            overall,
            summary,
            comments,
        })
}

fn raw_soundness_payload_strategy() -> impl Strategy<Value = RawSoundnessPayload> {
    (small_string(), small_string()).prop_map(|(decision, explanation)| RawSoundnessPayload {
        decision,
        explanation,
    })
}

fn raw_sound_payload_strategy() -> impl Strategy<Value = RawSoundPayload> {
    (
        small_string(),
        raw_soundness_payload_strategy(),
        small_string(),
        small_string(),
        small_string(),
    )
        .prop_map(
            |(node, soundness, overall, summary, comments)| RawSoundPayload {
                node,
                soundness,
                overall,
                summary,
                comments,
            },
        )
}

fn raw_review_payload_strategy() -> impl Strategy<Value = RawReviewPayload> {
    (
        small_string(),
        small_string(),
        small_string(),
        pvec(small_string(), 0..3),
        pvec(small_string(), 0..3),
        pvec(small_string(), 0..3),
        small_string(),
        small_string(),
        small_string(),
        option::of(any::<bool>()),
        option::of(any::<bool>()),
        any::<bool>(),
    )
        .prop_map(
            |(
                decision,
                reason,
                comments,
                tb,
                ob,
                rb,
                next_active,
                reset,
                next_mode,
                allow_new_obligations,
                must_close_active,
                clear_human_input,
            )| {
                RawReviewPayload {
                    decision,
                    reason,
                    comments,
                    task_blocker_ids: tb,
                    override_blocker_ids: ob,
                    reset_blocker_ids: rb,
                    next_active,
                    reset,
                    next_mode,
                    allow_new_obligations,
                    must_close_active,
                    clear_human_input,
                    ..RawReviewPayload::default()
                }
            },
        )
}

fn raw_audit_payload_strategy() -> impl Strategy<Value = RawAuditPayload> {
    // RawAuditPayload's `new_tasks` items embed `RawCleanupTaskKind` which
    // is a structured type — using the explicit Default for each item
    // keeps the strategy fast and the schema honest.
    (
        small_string(),
        small_string(),
    )
        .prop_map(|(scratchpad_replace, outcome)| RawAuditPayload {
            new_tasks: Vec::new(),
            task_modifications: Vec::new(),
            scratchpad_replace,
            outcome,
        })
}

fn corr_lane_member_input_strategy() -> impl Strategy<Value = CorrLaneMemberInput> {
    (
        lane_id_strategy(),
        any::<bool>(),
        option::of(raw_corr_payload_strategy()),
        small_string(),
    )
        .prop_map(|(lane_id, ok, payload, error)| CorrLaneMemberInput {
            lane_id,
            ok,
            payload,
            error,
        })
}

fn paper_lane_member_input_strategy() -> impl Strategy<Value = PaperLaneMemberInput> {
    (
        lane_id_strategy(),
        any::<bool>(),
        option::of(raw_paper_lane_payload_strategy()),
        small_string(),
    )
        .prop_map(|(lane_id, ok, payload, error)| PaperLaneMemberInput {
            lane_id,
            ok,
            payload,
            error,
        })
}

fn sound_lane_member_input_strategy() -> impl Strategy<Value = SoundLaneMemberInput> {
    (
        lane_id_strategy(),
        any::<bool>(),
        option::of(raw_sound_payload_strategy()),
        small_string(),
    )
        .prop_map(|(lane_id, ok, payload, error)| SoundLaneMemberInput {
            lane_id,
            ok,
            payload,
            error,
        })
}

fn corr_norm_input_strategy() -> impl Strategy<Value = CorrNormalizationInput> {
    (
        any::<u32>(),
        any::<u32>(),
        btree_set(lane_id_strategy(), 0..2),
        btree_set(node_id_strategy(), 0..3),
        btree_set(target_id_strategy(), 0..2),
        pvec(corr_lane_member_input_strategy(), 0..2),
    )
        .prop_map(
            |(request_id, cycle, verify_lanes, verify_nodes, verify_targets, members)| {
                CorrNormalizationInput {
                    request_id,
                    cycle,
                    verify_lanes,
                    verify_nodes,
                    verify_targets,
                    members,
                }
            },
        )
}

fn paper_norm_input_strategy() -> impl Strategy<Value = PaperNormalizationInput> {
    (
        any::<u32>(),
        any::<u32>(),
        btree_set(lane_id_strategy(), 0..2),
        btree_set(target_id_strategy(), 0..2),
        btree_set(node_id_strategy(), 0..3),
        pvec(paper_lane_member_input_strategy(), 0..2),
    )
        .prop_map(
            |(request_id, cycle, verify_lanes, verify_targets, verify_nodes, members)| {
                PaperNormalizationInput {
                    request_id,
                    cycle,
                    verify_lanes,
                    verify_targets,
                    verify_nodes,
                    members,
                    ..PaperNormalizationInput::default()
                }
            },
        )
}

fn sound_norm_input_strategy() -> impl Strategy<Value = SoundNormalizationInput> {
    (
        any::<u32>(),
        any::<u32>(),
        btree_set(lane_id_strategy(), 0..2),
        btree_set(node_id_strategy(), 0..2),
        pvec(sound_lane_member_input_strategy(), 0..2),
    )
        .prop_map(
            |(request_id, cycle, verify_lanes, verify_nodes, members)| SoundNormalizationInput {
                request_id,
                cycle,
                verify_lanes,
                verify_nodes,
                members,
            },
        )
}

fn paper_focus_range_strategy() -> impl Strategy<Value = PaperFocusRange> {
    (
        any::<u32>(),
        any::<u32>(),
        small_string(),
        option::of(small_string()),
    )
        .prop_map(|(start_line, end_line, reason, doc)| PaperFocusRange {
            start_line,
            end_line,
            reason,
            doc: doc.map(trellis_kernel::RefPaperId::from),
        })
}

fn paper_grounding_strategy() -> impl Strategy<Value = PaperGrounding> {
    (any::<bool>(), small_string()).prop_map(|(consulted, basis)| PaperGrounding {
        consulted_cited_ranges: consulted,
        basis_summary: basis,
    })
}

fn audit_task_strategy() -> impl Strategy<Value = AuditTask> {
    (
        small_string(),
        small_string(),
        small_string(),
        any::<bool>(),
        small_string(),
        option::of(any::<u32>()),
    )
        .prop_map(
            |(id, title, body, dismissed, dismissed_reason, dismissed_at_cycle)| AuditTask {
                id,
                title,
                body,
                dismissed,
                dismissed_reason,
                dismissed_at_cycle,
            },
        )
}

fn blocker_object_strategy() -> impl Strategy<Value = BlockerObject> {
    prop_oneof![
        node_id_strategy().prop_map(|node| BlockerObject::Node { node }),
        target_id_strategy().prop_map(|target| BlockerObject::Target { target }),
        nonempty_ident().prop_map(|deviation| BlockerObject::Deviation {
            deviation: trellis_kernel::DeviationId::from(deviation)
        }),
    ]
}

fn blocker_kind_strategy() -> impl Strategy<Value = BlockerKind> {
    prop_oneof![
        Just(BlockerKind::PaperFaithfulness),
        Just(BlockerKind::Substantiveness),
        Just(BlockerKind::NodeCorr),
        Just(BlockerKind::Soundness),
        Just(BlockerKind::Deviation),
    ]
}

fn blocker_strategy() -> impl Strategy<Value = Blocker> {
    (
        blocker_kind_strategy(),
        blocker_object_strategy(),
        fingerprint_strategy(),
        any::<bool>(),
    )
        .prop_map(|(kind, object, fingerprint, deferred)| Blocker {
            kind,
            object,
            fingerprint,
            deferred,
        })
}

// ============================================================================
// Property tests
// ============================================================================
//
// We split into per-type proptest blocks so a failure in one block
// names the type clearly. `proptest!` blocks default to 256 cases;
// we narrow to 64 to keep total CPU under the host budget. Override
// with `PROPTEST_CASES=N` if you want more for a focused investigation.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // -----------------------------------------------------------------
    // Enum tags — catches the "renamed a variant during the refactor"
    // class of regression.
    // -----------------------------------------------------------------
    #[test]
    fn corr_status_roundtrips(v in corr_status_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn sound_status_roundtrips(v in sound_status_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn substantiveness_status_roundtrips(v in substantiveness_status_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn phase_roundtrips(v in phase_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn stage_roundtrips(v in stage_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn worker_outcome_roundtrips(v in worker_outcome_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn response_status_roundtrips(v in response_status_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn review_decision_roundtrips(v in review_decision_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn request_kind_roundtrips(v in request_kind_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn task_mode_roundtrips(v in task_mode_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn node_kind_roundtrips(v in node_kind_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn pv_role_roundtrips(v in pv_role_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn node_difficulty_roundtrips(v in node_difficulty_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn gate_kind_roundtrips(v in gate_kind_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn reset_choice_roundtrips(v in reset_choice_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn human_choice_roundtrips(v in human_choice_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn worker_profile_roundtrips(v in worker_profile_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn worker_validation_kind_roundtrips(v in worker_validation_kind_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    // -----------------------------------------------------------------
    // Newtypes around String — catches accidental String→OsString or
    // Vec<u8> migration.
    // -----------------------------------------------------------------
    #[test]
    fn node_id_roundtrips(v in node_id_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn target_id_roundtrips(v in target_id_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn lane_id_roundtrips(v in lane_id_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn fingerprint_roundtrips(v in fingerprint_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    // -----------------------------------------------------------------
    // Update<T> — refactor-fragile generic wire format.
    // -----------------------------------------------------------------
    #[test]
    fn update_node_difficulty_roundtrips(v in update_strategy(node_difficulty_strategy())) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn update_node_kind_roundtrips(v in update_strategy(node_kind_strategy())) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn update_btreeset_node_id_roundtrips(
        v in update_strategy(btree_set(node_id_strategy(), 0..3))
    ) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    // -----------------------------------------------------------------
    // Composite payloads / lane member inputs — the workhorses of the
    // five parallel normalization pipelines.
    // -----------------------------------------------------------------
    #[test]
    fn raw_corr_verdict_roundtrips(v in raw_corr_verdict_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_corr_phase_roundtrips(v in raw_corr_phase_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_corr_payload_roundtrips(v in raw_corr_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_paper_payload_roundtrips(v in raw_paper_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_substantiveness_verdict_roundtrips(v in raw_substantiveness_verdict_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_substantiveness_phase_roundtrips(v in raw_substantiveness_phase_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_substantiveness_payload_roundtrips(v in raw_substantiveness_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_deviation_phase_roundtrips(v in raw_deviation_phase_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_paper_lane_payload_roundtrips(v in raw_paper_lane_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_soundness_payload_roundtrips(v in raw_soundness_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_sound_payload_roundtrips(v in raw_sound_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_review_payload_roundtrips(v in raw_review_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn raw_audit_payload_roundtrips(v in raw_audit_payload_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn corr_lane_member_input_roundtrips(v in corr_lane_member_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn paper_lane_member_input_roundtrips(v in paper_lane_member_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn sound_lane_member_input_roundtrips(v in sound_lane_member_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    // -----------------------------------------------------------------
    // Normalization inputs — the actual `RuntimeCliRequest` argument
    // shapes for normalize_corr / normalize_paper / normalize_sound.
    // -----------------------------------------------------------------
    #[test]
    fn corr_norm_input_roundtrips(v in corr_norm_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn paper_norm_input_roundtrips(v in paper_norm_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn sound_norm_input_roundtrips(v in sound_norm_input_strategy()) {
        assert_value_roundtrip(&v);
    }

    // -----------------------------------------------------------------
    // Model-level building blocks — Blocker, AuditTask,
    // PaperFocusRange, PaperGrounding.
    // -----------------------------------------------------------------
    #[test]
    fn blocker_object_roundtrips(v in blocker_object_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn blocker_kind_roundtrips(v in blocker_kind_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn blocker_roundtrips(v in blocker_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn audit_task_roundtrips(v in audit_task_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn paper_focus_range_roundtrips(v in paper_focus_range_strategy()) {
        assert_value_roundtrip(&v);
    }

    #[test]
    fn paper_grounding_roundtrips(v in paper_grounding_strategy()) {
        assert_value_roundtrip(&v);
    }
}

// ============================================================================
// Sanity check: every default value of the major types round-trips. This
// is the "if proptest's strategies miss a corner, at least defaults work"
// floor.
// ============================================================================

#[test]
fn defaults_roundtrip() {
    // Enums that carry a Default impl.
    assert_value_roundtrip(&Phase::default());
    assert_value_roundtrip(&Stage::default());
    assert_value_roundtrip(&CorrStatus::default());
    assert_value_roundtrip(&SoundStatus::default());
    assert_value_roundtrip(&SubstantivenessStatus::default());
    assert_value_roundtrip(&WorkerOutcome::default());
    assert_value_roundtrip(&ResponseStatus::default());
    assert_value_roundtrip(&ReviewDecisionKind::default());
    assert_value_roundtrip(&RequestKind::default());
    assert_value_roundtrip(&TaskMode::default());
    assert_value_roundtrip(&GateKind::default());
    assert_value_roundtrip(&ResetChoice::default());

    // Payload structs.
    assert_value_roundtrip(&RawCorrPayload::default());
    assert_value_roundtrip(&RawPaperPayload::default());
    assert_value_roundtrip(&RawPaperLanePayload::default());
    assert_value_roundtrip(&RawSubstantivenessPayload::default());
    assert_value_roundtrip(&RawSoundPayload::default());
    assert_value_roundtrip(&RawReviewPayload::default());
    assert_value_roundtrip(&RawAuditPayload::default());

    // Inputs.
    assert_value_roundtrip(&CorrNormalizationInput::default());
    assert_value_roundtrip(&PaperNormalizationInput::default());
    assert_value_roundtrip(&SoundNormalizationInput::default());

    // Outputs.
    assert_value_roundtrip(&CorrNormalizationOutput::default());
    assert_value_roundtrip(&PaperNormalizationOutput::default());
    assert_value_roundtrip(&SoundNormalizationOutput::default());
    assert_value_roundtrip(&ReviewNormalizationOutput::default());
    assert_value_roundtrip(&AuditNormalizationOutput::default());
    assert_value_roundtrip(&WorkerNormalizationOutput::default());

    // Top-level envelopes.
    assert_value_roundtrip(&WrapperRequest::default());
    assert_value_roundtrip(&WrapperResponse::Worker(WorkerResponse::default()));
    assert_value_roundtrip(&WrapperResponse::Review(ReviewResponse::default()));
    assert_value_roundtrip(&WrapperResponse::Paper(PaperResponse::default()));
    assert_value_roundtrip(&WrapperResponse::Corr(CorrResponse::default()));
    assert_value_roundtrip(&WrapperResponse::Sound(SoundResponse::default()));
    assert_value_roundtrip(&WrapperResponse::Audit(AuditResponse::default()));
    assert_value_roundtrip(&WrapperResponse::HumanGate(HumanGateResponse::default()));
    assert_value_roundtrip(&WrapperResponse::StuckMathAudit(
        StuckMathAuditResponse::default(),
    ));
}

// ============================================================================
// Sidecar queue redesign: ReviewResponse queue fields — populated roundtrip
// plus the A7 wire pin (both fields skip-when-empty so queue-free responses
// serialize byte-identically to the pre-feature shape).
// ============================================================================

#[test]
fn review_response_sidecar_queue_fields_roundtrip_and_skip_when_empty() {
    let default_json = serde_json::to_value(ReviewResponse::default()).expect("serialize");
    let obj = default_json.as_object().expect("object");
    assert!(
        !obj.contains_key("sidecar_queue_add") && !obj.contains_key("sidecar_queue_remove"),
        "empty queue fields must stay off the wire (A7)"
    );

    let populated = ReviewResponse {
        sidecar_queue_add: vec![NodeId::from("Rung"), NodeId::from("Lift")],
        sidecar_queue_remove: vec![NodeId::from("Old")],
        ..ReviewResponse::default()
    };
    assert_value_roundtrip(&populated);
    let round: ReviewResponse =
        serde_json::from_value(serde_json::to_value(&populated).unwrap()).unwrap();
    assert_eq!(round.sidecar_queue_add, populated.sidecar_queue_add);
    assert_eq!(round.sidecar_queue_remove, populated.sidecar_queue_remove);
}

// ============================================================================
// Phase IV step 11: checkpoint back-compat for `ProtocolState.tablet_target`.
// Existing on-disk checkpoints predate the field; the live-run resume
// guarantee is that they load as `BackendId::Lean` (struct-level + field-level
// `#[serde(default)]`). Pin it: a `ProtocolState` JSON OMITTING the key must
// deserialize with `tablet_target == Lean`.
// ============================================================================

#[test]
fn protocol_state_missing_tablet_target_defaults_to_lean() {
    use trellis_kernel::backend::BackendId;
    use trellis_kernel::ProtocolState;

    // Serialize a default state, then strip the `tablet_target` key to
    // simulate a pre-field checkpoint, and confirm it round-trips to `Lean`.
    let mut value =
        serde_json::to_value(ProtocolState::default()).expect("serialize ProtocolState");
    let obj = value.as_object_mut().expect("ProtocolState serializes to a JSON object");
    assert!(
        obj.remove("tablet_target").is_some(),
        "tablet_target should be present in a freshly-serialized ProtocolState"
    );
    let parsed: ProtocolState =
        serde_json::from_value(value).expect("deserialize legacy (no tablet_target) ProtocolState");
    assert_eq!(parsed.tablet_target, BackendId::Lean);

    // Also exercise the literal empty-object path: a `{}` checkpoint loads as
    // a full default, including `tablet_target == Lean`.
    let from_empty: ProtocolState =
        serde_json::from_str("{}").expect("deserialize empty-object ProtocolState");
    assert_eq!(from_empty.tablet_target, BackendId::Lean);
}

// ============================================================================
// Phase IV step 12: checkpoint back-compat for `ProtocolState.node_target`
// (the per-node backend-override map). Unlike `tablet_target`, this field is
// `skip_serializing_if = "BTreeMap::is_empty"`, so the all-Lean default is the
// ABSENCE of the key — a freshly-serialized default state must NOT emit it.
// The live-run resume guarantee is that an old checkpoint with no key loads as
// the empty map, i.e. all-Lean: `effective_node_target` falls back to
// `tablet_target == Lean` for every node.
// ============================================================================

#[test]
fn protocol_state_missing_node_target_defaults_to_empty_all_lean() {
    use trellis_kernel::backend::BackendId;
    use trellis_kernel::ProtocolState;

    // A default (all-Lean) state is empty ⇒ `skip_serializing_if` omits the
    // key entirely. This is what makes node_target invisible to the contract
    // wire / regen-pruner in the all-Lean case.
    let value = serde_json::to_value(ProtocolState::default()).expect("serialize ProtocolState");
    let obj = value.as_object().expect("ProtocolState serializes to a JSON object");
    assert!(
        !obj.contains_key("node_target"),
        "an empty node_target must be skipped from serialized JSON (skip_serializing_if)"
    );

    // A legacy checkpoint with the key explicitly absent deserializes to the
    // empty map ⇒ every node resolves to the tablet default `Lean`.
    let parsed: ProtocolState =
        serde_json::from_value(value).expect("deserialize ProtocolState without node_target");
    assert!(parsed.node_target.is_empty());
    assert_eq!(
        parsed.effective_node_target(&"any_node".into()),
        BackendId::Lean
    );

    // The literal empty-object path: a `{}` checkpoint loads as a full default,
    // including an empty node_target (all-Lean).
    let from_empty: ProtocolState =
        serde_json::from_str("{}").expect("deserialize empty-object ProtocolState");
    assert!(from_empty.node_target.is_empty());
    assert_eq!(
        from_empty.effective_node_target(&"any_node".into()),
        BackendId::Lean
    );
}

// ============================================================================
// Fresh-run initial planner: checkpoint back-compat for the new serde-default
// fields. Pre-feature checkpoints predate `StuckMathAuditState.initial_planning`,
// `AuditPlan.initial_plan`, and `RuntimeMetadata.initial_planning_seeded`; the
// resume guarantee is that they load as None / false. Also pin that a state
// carrying a live `initial_planning` carrier round-trips byte-stable.
// ============================================================================

#[test]
fn initial_planning_fields_are_checkpoint_back_compatible() {
    use trellis_kernel::{
        AuditPlan, ChallengeTargetId, InitialPlanningContext, InitialPlanningSource,
        StuckMathAuditState,
    };

    // 1. Pre-feature StuckMathAuditState (no `initial_planning` key) loads as
    //    None; an empty object loads as a full default.
    let mut sma = serde_json::to_value(StuckMathAuditState::default())
        .expect("serialize StuckMathAuditState");
    sma.as_object_mut()
        .expect("object")
        .remove("initial_planning");
    let parsed: StuckMathAuditState =
        serde_json::from_value(sma).expect("deserialize legacy StuckMathAuditState");
    assert!(parsed.initial_planning.is_none());
    let from_empty: StuckMathAuditState =
        serde_json::from_str("{}").expect("empty-object StuckMathAuditState");
    assert!(from_empty.initial_planning.is_none());

    // 2. Pre-feature AuditPlan (no `initial_plan` key) loads as false.
    let mut plan = serde_json::to_value(AuditPlan::default()).expect("serialize AuditPlan");
    plan.as_object_mut().expect("object").remove("initial_plan");
    let parsed_plan: AuditPlan =
        serde_json::from_value(plan).expect("deserialize legacy AuditPlan");
    assert!(!parsed_plan.initial_plan);

    // 3. A live carrier round-trips for every source kind.
    for source in [
        InitialPlanningSource::PaperManuscript {
            paper_tex_path: "paper/main.tex".into(),
        },
        InitialPlanningSource::PvGoalProse {
            goal_path: "GOAL.md".into(),
        },
        InitialPlanningSource::PvChallengeSpecs {
            specs: BTreeMap::from([(
                ChallengeTargetId::from("goal:g"),
                "theorem g : True := by".to_string(),
            )]),
        },
    ] {
        let ctx = InitialPlanningContext {
            source,
            configured_target_ids: std::collections::BTreeSet::from(["t".to_string()]),
            ..Default::default()
        };
        let json = serde_json::to_string(&ctx).expect("serialize context");
        let back: InitialPlanningContext =
            serde_json::from_str(&json).expect("deserialize context");
        assert_eq!(ctx, back);
    }

    let live = StuckMathAuditState {
        active: true,
        initial_planning: Some(InitialPlanningContext {
            source: InitialPlanningSource::PaperManuscript {
                paper_tex_path: "paper/main.tex".into(),
            },
            configured_target_ids: std::collections::BTreeSet::from(["t".to_string()]),
            ..Default::default()
        }),
        ..StuckMathAuditState::default()
    };
    let json = serde_json::to_string(&live).expect("serialize live latch");
    let back: StuckMathAuditState = serde_json::from_str(&json).expect("deserialize live latch");
    assert_eq!(live, back);
}

// ============================================================================
// Coverage re-planning fields: additive, default-skipped. Pre-feature JSON
// (no new keys) loads with defaults; default-valued fields stay OFF the wire
// so feature-off states serialize byte-identically to pre-feature states.
// ============================================================================

#[test]
fn coverage_replanning_fields_are_checkpoint_back_compatible() {
    use trellis_kernel::{InitialPlanningContext, InitialPlanningSource, ProtocolState};

    // 1. Feature-off ProtocolState keeps `coverage_replanning_source` off the
    //    wire (skip-if-None), and pre-feature JSON loads as None.
    let state_json =
        serde_json::to_value(ProtocolState::default()).expect("serialize ProtocolState");
    assert!(
        state_json.get("coverage_replanning_source").is_none(),
        "feature-off state must not carry the source key"
    );
    let parsed: ProtocolState =
        serde_json::from_value(state_json).expect("deserialize pre-feature-shaped state");
    assert!(parsed.coverage_replanning_source.is_none());

    // 2. An armed source round-trips.
    let mut armed = ProtocolState::default();
    armed.coverage_replanning_source = Some(InitialPlanningSource::PaperManuscript {
        paper_tex_path: "paper/main.tex".into(),
    });
    let json = serde_json::to_string(&armed).expect("serialize armed state");
    assert!(json.contains("coverage_replanning_source"));
    let back: ProtocolState = serde_json::from_str(&json).expect("deserialize armed state");
    assert_eq!(back.coverage_replanning_source, armed.coverage_replanning_source);

    // 3. Cycle-1-shaped context (coverage fields default) serializes without
    //    the new keys — initial-planner-era carriers stay byte-identical.
    let cycle_one = InitialPlanningContext {
        source: InitialPlanningSource::PaperManuscript {
            paper_tex_path: "paper/main.tex".into(),
        },
        configured_target_ids: std::collections::BTreeSet::from(["t".to_string()]),
        ..Default::default()
    };
    let ctx_json = serde_json::to_value(&cycle_one).expect("serialize context");
    for key in ["coverage_replanning", "uncovered_target_ids", "covered_targets"] {
        assert!(
            ctx_json.get(key).is_none(),
            "default coverage field {key} must stay off the wire"
        );
    }
    // Pre-feature context JSON (exactly these keys) loads with defaults.
    let parsed: InitialPlanningContext =
        serde_json::from_value(ctx_json).expect("deserialize pre-feature context");
    assert!(!parsed.coverage_replanning);
    assert!(parsed.uncovered_target_ids.is_empty());
    assert!(parsed.covered_targets.is_empty());

    // 4. A populated coverage carrier round-trips.
    let coverage = InitialPlanningContext {
        coverage_replanning: true,
        uncovered_target_ids: std::collections::BTreeSet::from(["u".to_string()]),
        covered_targets: std::collections::BTreeMap::from([(
            "t".to_string(),
            std::collections::BTreeSet::from(["a".to_string()]),
        )]),
        ..cycle_one
    };
    let json = serde_json::to_string(&coverage).expect("serialize coverage context");
    let back: InitialPlanningContext =
        serde_json::from_str(&json).expect("deserialize coverage context");
    assert_eq!(back, coverage);
}

#[test]
fn runtime_metadata_missing_coverage_replanning_seeded_defaults_false() {
    use trellis_kernel::runtime::RuntimeMetadata;

    let mut value =
        serde_json::to_value(RuntimeMetadata::default()).expect("serialize RuntimeMetadata");
    value
        .as_object_mut()
        .expect("object")
        .remove("coverage_replanning_seeded");
    let parsed: RuntimeMetadata =
        serde_json::from_value(value).expect("deserialize legacy RuntimeMetadata");
    assert!(
        !parsed.coverage_replanning_seeded,
        "pre-feature metadata must replay with the coverage seed gate off"
    );
    let from_empty: RuntimeMetadata =
        serde_json::from_str("{}").expect("empty-object RuntimeMetadata");
    assert!(!from_empty.coverage_replanning_seeded);
}

#[test]
fn runtime_metadata_missing_initial_planning_seeded_defaults_false() {
    use trellis_kernel::runtime::RuntimeMetadata;

    let mut value =
        serde_json::to_value(RuntimeMetadata::default()).expect("serialize RuntimeMetadata");
    value
        .as_object_mut()
        .expect("object")
        .remove("initial_planning_seeded");
    let parsed: RuntimeMetadata =
        serde_json::from_value(value).expect("deserialize legacy RuntimeMetadata");
    assert!(
        !parsed.initial_planning_seeded,
        "pre-feature metadata must replay with the seed gate off"
    );
    let from_empty: RuntimeMetadata =
        serde_json::from_str("{}").expect("empty-object RuntimeMetadata");
    assert!(!from_empty.initial_planning_seeded);
}

// ============================================================================
// Decide live-polarity surfacing: request-wire back-compat for
// `WrapperRequest.pv_live_polarity` (the per-pair polarity map the
// StuckMathAudit contract renders as `decide_live_polarity`). The live-run
// resume guarantee is that a pre-field request JSON (no key) loads as the
// empty map — i.e. every Decide pair reads as the `Prove` default — and that
// a default request keeps the key OFF the wire (byte-identical baseline for
// non-Decide / unflipped runs). A populated map must round-trip value-stable.
// ============================================================================

#[test]
fn wrapper_request_pv_live_polarity_is_wire_back_compatible() {
    use trellis_kernel::{ChallengePolarity, ChallengeTargetId};

    // A default request omits the key entirely (skip_serializing_if).
    let value =
        serde_json::to_value(WrapperRequest::default()).expect("serialize WrapperRequest");
    let obj = value
        .as_object()
        .expect("WrapperRequest serializes to a JSON object");
    assert!(
        !obj.contains_key("pv_live_polarity"),
        "an empty pv_live_polarity must be skipped from serialized JSON"
    );

    // A pre-field request JSON (key absent) deserializes to the empty map.
    let parsed: WrapperRequest =
        serde_json::from_value(value).expect("deserialize WrapperRequest without pv_live_polarity");
    assert!(parsed.pv_live_polarity.is_empty());

    // The literal empty-object path loads as a full default too.
    let from_empty: WrapperRequest =
        serde_json::from_str("{}").expect("empty-object WrapperRequest");
    assert!(from_empty.pv_live_polarity.is_empty());

    // A populated map (one flipped pair, one absent-⇒-Prove pair) round-trips.
    let live = WrapperRequest {
        pv_live_polarity: BTreeMap::from([(
            ChallengeTargetId::from("goal:flipped"),
            ChallengePolarity::Disprove,
        )]),
        ..WrapperRequest::default()
    };
    assert_value_roundtrip(&live);
    let json = serde_json::to_value(&live).expect("serialize populated request");
    assert_eq!(
        json["pv_live_polarity"],
        serde_json::json!({"goal:flipped": "disprove"}),
        "the map serializes as primary-id -> snake_case polarity"
    );
}

// ============================================================================
// Parallel-closure sidecar wire shapes
// ============================================================================

fn sidecar_closure_meta_strategy(
) -> impl Strategy<Value = trellis_kernel::SidecarClosureMeta> {
    (small_string(), small_string(), small_string(), any::<u64>()).prop_map(
        |(attempt_id, provider, model, wall_ms)| trellis_kernel::SidecarClosureMeta {
            attempt_id,
            provider,
            model,
            wall_ms,
        },
    )
}

fn node_closure_provenance_strategy(
) -> impl Strategy<Value = trellis_kernel::NodeClosureProvenance> {
    prop_oneof![
        any::<u32>().prop_map(|cycle| trellis_kernel::NodeClosureProvenance {
            closed_by: trellis_kernel::ClosedBy::Worker,
            cycle,
            sidecar: None,
        }),
        (any::<u32>(), sidecar_closure_meta_strategy()).prop_map(|(cycle, meta)| {
            trellis_kernel::NodeClosureProvenance {
                closed_by: trellis_kernel::ClosedBy::Sidecar,
                cycle,
                sidecar: Some(meta),
            }
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sidecar_closure_meta_roundtrips(v in sidecar_closure_meta_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }

    #[test]
    fn node_closure_provenance_roundtrips(v in node_closure_provenance_strategy()) {
        assert_value_roundtrip(&v);
        assert_json_roundtrip(&v);
    }
}
