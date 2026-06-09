//! Cleanup-v2 (audit Finding 1): audit-burst response normalization.
//!
//! Parallel to `review_normalization` / `verification_normalization`. The
//! bridge calls `normalize_audit_response` with a raw audit JSON payload
//! and the originating `WrapperRequest`; the kernel returns an
//! `AuditResponse` envelope ready to be fed to `apply_audit_response`.
//!
//! Domain legality (target_node ∈ present, target ∉ protected,
//! replacement validity, intra-burst duplicates, task_modifications
//! round/status legality) is enforced by `apply_audit_response` against
//! the live ProtocolState. This normalizer enforces shape only.

use crate::model::{
    AuditOutcome, AuditResponse, CleanupAuditTaskModification, CleanupReplacement,
    CleanupTaskConfidence, CleanupTaskKind, NewCleanupAuditTask, NodeId, RequestKind,
    ResponseStatus, WrapperRequest,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawAuditPayload {
    pub new_tasks: Vec<RawNewCleanupAuditTask>,
    pub task_modifications: Vec<RawCleanupAuditTaskModification>,
    pub scratchpad_replace: String,
    pub outcome: String,
}

impl Default for RawAuditPayload {
    fn default() -> Self {
        Self {
            new_tasks: Vec::new(),
            task_modifications: Vec::new(),
            scratchpad_replace: String::new(),
            outcome: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RawNewCleanupAuditTask {
    pub target_node: String,
    pub rationale: String,
    pub confidence: String,
    pub kind: RawCleanupTaskKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RawCleanupTaskKind {
    pub kind: String,
    pub replacement: Option<RawCleanupReplacement>,
    pub warning_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RawCleanupReplacement {
    pub kind: String,
    pub citation: Option<String>,
    pub node: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RawCleanupAuditTaskModification {
    pub task_index: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditNormalizationInput {
    pub request: WrapperRequest,
    pub raw_payload: RawAuditPayload,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditNormalizationOutput {
    pub response: AuditResponse,
}

pub fn normalize_audit_response(
    input: &AuditNormalizationInput,
) -> Result<AuditNormalizationOutput, String> {
    let request = &input.request;
    if request.kind != RequestKind::Audit {
        return Err("audit normalization requires an Audit request".into());
    }
    let mut new_tasks = Vec::with_capacity(input.raw_payload.new_tasks.len());
    for (i, raw) in input.raw_payload.new_tasks.iter().enumerate() {
        new_tasks.push(parse_new_task(i, raw)?);
    }
    let mut task_modifications = Vec::with_capacity(input.raw_payload.task_modifications.len());
    for raw in &input.raw_payload.task_modifications {
        task_modifications.push(CleanupAuditTaskModification {
            task_index: raw.task_index,
            reason: raw.reason.clone(),
        });
    }
    let outcome = parse_outcome(&input.raw_payload.outcome)?;
    let response = AuditResponse {
        request_id: request.id,
        cycle: request.cycle,
        status: ResponseStatus::Ok,
        new_tasks,
        task_modifications,
        scratchpad_replace: input.raw_payload.scratchpad_replace.clone(),
        outcome,
    };
    Ok(AuditNormalizationOutput { response })
}

fn parse_new_task(i: usize, raw: &RawNewCleanupAuditTask) -> Result<NewCleanupAuditTask, String> {
    let target_node = raw.target_node.trim();
    if target_node.is_empty() {
        return Err(format!(
            "new_tasks[{i}].target_node must be a non-empty string"
        ));
    }
    let confidence =
        parse_confidence(&raw.confidence).map_err(|e| format!("new_tasks[{i}].confidence: {e}"))?;
    let kind = parse_task_kind(i, &raw.kind)?;
    Ok(NewCleanupAuditTask {
        target_node: NodeId::from(target_node),
        rationale: raw.rationale.clone(),
        confidence,
        kind,
    })
}

fn parse_confidence(raw: &str) -> Result<CleanupTaskConfidence, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "low" => Ok(CleanupTaskConfidence::Low),
        "medium" => Ok(CleanupTaskConfidence::Medium),
        "high" => Ok(CleanupTaskConfidence::High),
        other => Err(format!(
            "confidence must be one of ['high', 'medium', 'low']; got {other:?}"
        )),
    }
}

fn parse_task_kind(i: usize, raw: &RawCleanupTaskKind) -> Result<CleanupTaskKind, String> {
    match raw.kind.trim().to_ascii_lowercase().as_str() {
        "substitution" => {
            let Some(rep) = raw.replacement.as_ref() else {
                return Err(format!(
                    "new_tasks[{i}].kind=substitution requires a replacement object"
                ));
            };
            let replacement = match rep.kind.trim().to_ascii_lowercase().as_str() {
                "mathlib" => {
                    let citation = rep.citation.as_deref().unwrap_or("").trim().to_string();
                    if citation.is_empty() {
                        return Err(format!(
                            "new_tasks[{i}].replacement.citation must be a non-empty string"
                        ));
                    }
                    CleanupReplacement::Mathlib { citation }
                }
                "tablet_wrapper" => {
                    let node = rep.node.as_deref().unwrap_or("").trim().to_string();
                    if node.is_empty() {
                        return Err(format!(
                            "new_tasks[{i}].replacement.node must be a non-empty string"
                        ));
                    }
                    CleanupReplacement::TabletWrapper {
                        node: NodeId::from(node),
                    }
                }
                other => {
                    return Err(format!(
                        "new_tasks[{i}].replacement.kind must be one of ['mathlib', 'tablet_wrapper']; got {other:?}"
                    ));
                }
            };
            Ok(CleanupTaskKind::Substitution { replacement })
        }
        "lint_fix" | "lintfix" => {
            let warning_text = raw.warning_text.clone().unwrap_or_default();
            if warning_text.trim().is_empty() {
                return Err(format!(
                    "new_tasks[{i}].warning_text must be a non-empty string for lint_fix"
                ));
            }
            Ok(CleanupTaskKind::LintFix { warning_text })
        }
        other => Err(format!(
            "new_tasks[{i}].kind must be one of ['substitution', 'lint_fix']; got {other:?}"
        )),
    }
}

fn parse_outcome(raw: &str) -> Result<AuditOutcome, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "audit_done" | "done" => Ok(AuditOutcome::AuditDone),
        "need_to_continue" | "continue" => Ok(AuditOutcome::NeedToContinue),
        other => Err(format!(
            "outcome must be one of ['audit_done', 'need_to_continue']; got {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CleanupAuditTask;
    use std::collections::BTreeSet;

    fn build_request() -> WrapperRequest {
        WrapperRequest {
            kind: RequestKind::Audit,
            id: 7,
            cycle: 11,
            ..WrapperRequest::default()
        }
    }

    #[test]
    fn normalize_audit_response_minimal_audit_done() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert!(out.response.new_tasks.is_empty());
        assert!(out.response.task_modifications.is_empty());
        assert_eq!(out.response.outcome, AuditOutcome::AuditDone);
        assert_eq!(out.response.status, ResponseStatus::Ok);
        assert_eq!(out.response.request_id, 7);
        assert_eq!(out.response.cycle, 11);
    }

    #[test]
    fn normalize_audit_response_substitution_mathlib() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "MyNode".into(),
                    rationale: "this wraps Nat.add_comm".into(),
                    confidence: "high".into(),
                    kind: RawCleanupTaskKind {
                        kind: "substitution".into(),
                        replacement: Some(RawCleanupReplacement {
                            kind: "mathlib".into(),
                            citation: Some("Nat.add_comm".into()),
                            node: None,
                        }),
                        warning_text: None,
                    },
                }],
                outcome: "need_to_continue".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert_eq!(out.response.new_tasks.len(), 1);
        let t = &out.response.new_tasks[0];
        assert_eq!(t.target_node.as_str(), "MyNode");
        assert_eq!(t.confidence, CleanupTaskConfidence::High);
        match &t.kind {
            CleanupTaskKind::Substitution { replacement } => match replacement {
                CleanupReplacement::Mathlib { citation } => {
                    assert_eq!(citation, "Nat.add_comm")
                }
                _ => panic!("expected Mathlib replacement"),
            },
            _ => panic!("expected Substitution kind"),
        }
        assert_eq!(out.response.outcome, AuditOutcome::NeedToContinue);
    }

    #[test]
    fn normalize_audit_response_lintfix() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "MyNode".into(),
                    rationale: "fixes a warning".into(),
                    confidence: "medium".into(),
                    kind: RawCleanupTaskKind {
                        kind: "lint_fix".into(),
                        replacement: None,
                        warning_text: Some("unused variable `foo`".into()),
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert_eq!(out.response.new_tasks.len(), 1);
        match &out.response.new_tasks[0].kind {
            CleanupTaskKind::LintFix { warning_text } => {
                assert!(warning_text.contains("unused variable"))
            }
            _ => panic!("expected LintFix kind"),
        }
    }

    #[test]
    fn normalize_audit_response_rejects_unknown_outcome() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                outcome: "maybe_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let err = normalize_audit_response(&input).expect_err("unknown outcome rejected");
        assert!(err.contains("outcome"));
    }

    #[test]
    fn normalize_audit_response_rejects_missing_replacement_for_substitution() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "MyNode".into(),
                    rationale: "".into(),
                    confidence: "low".into(),
                    kind: RawCleanupTaskKind {
                        kind: "substitution".into(),
                        replacement: None,
                        warning_text: None,
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let err = normalize_audit_response(&input)
            .expect_err("substitution without replacement rejected");
        assert!(err.contains("replacement"));
    }

    #[test]
    fn normalize_audit_response_rejects_empty_lintfix_warning() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "MyNode".into(),
                    rationale: "".into(),
                    confidence: "low".into(),
                    kind: RawCleanupTaskKind {
                        kind: "lint_fix".into(),
                        replacement: None,
                        warning_text: Some("   ".into()),
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let err =
            normalize_audit_response(&input).expect_err("empty warning rejected for lint_fix");
        assert!(err.contains("warning_text"));
    }

    #[test]
    fn normalize_audit_response_with_task_modifications() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                task_modifications: vec![RawCleanupAuditTaskModification {
                    task_index: 2,
                    reason: "second-look: not actually a wrapper".into(),
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert_eq!(out.response.task_modifications.len(), 1);
        assert_eq!(out.response.task_modifications[0].task_index, 2);
    }

    // Audit Finding 1 round-trip smoke test: confirm that the resulting
    // AuditResponse can be deserialized back from JSON to itself,
    // demonstrating that the normalizer and the engine's
    // apply_audit_response both speak the same `AuditResponse` shape.
    #[test]
    fn normalize_audit_response_round_trip_through_json() {
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "AlphaNode".into(),
                    rationale: "wraps Nat.add_zero".into(),
                    confidence: "high".into(),
                    kind: RawCleanupTaskKind {
                        kind: "substitution".into(),
                        replacement: Some(RawCleanupReplacement {
                            kind: "tablet_wrapper".into(),
                            citation: None,
                            node: Some("BetaNode".into()),
                        }),
                        warning_text: None,
                    },
                }],
                outcome: "need_to_continue".into(),
                scratchpad_replace: "Round 1 notes".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        let serialized = serde_json::to_string(&out.response).expect("serialize");
        let _round_tripped: AuditResponse = serde_json::from_str(&serialized).expect("round-trip");
        // sanity check: the normalized response also fits into the
        // CleanupAuditTask shape used at append time.
        let dummy_task = CleanupAuditTask {
            target_node: out.response.new_tasks[0].target_node.clone(),
            rationale: out.response.new_tasks[0].rationale.clone(),
            confidence: out.response.new_tasks[0].confidence,
            kind: out.response.new_tasks[0].kind.clone(),
            status: crate::model::CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        };
        let _: BTreeSet<NodeId> = BTreeSet::from([dummy_task.target_node]);
    }
}
