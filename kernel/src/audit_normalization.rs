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
use std::collections::BTreeSet;

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
    /// `extract_shared` only: every declared parent other than the
    /// canonical target. Normalization combines this with `target_node`,
    /// sorts the full set, and rewrites the least parent as the target.
    pub co_parents: Option<Vec<String>>,
    /// `extract_helper` only: which extraction in the parent's ladder
    /// this is (>= 1).
    pub ordinal: Option<u32>,
    /// Advisory guidance naming the block for `extract_helper` /
    /// `extract_shared`, or likely-dead regions for `dead_code_elim`.
    /// May be empty.
    pub hint: Option<String>,
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
        let mut task = parse_new_task(i, raw)?;
        // Kernel-attached region stat: for ExtractShared, look the
        // declared parent set up in the dedup scan the kernel embedded
        // in THIS request's audit contract (`shared_proof_blocks`) and
        // carry the block length onto the task, so the reviewer can
        // rank without a region view of its own. The value never comes
        // from the audit's artifact (the allowlist strips it), and no
        // scan re-runs here.
        if let CleanupTaskKind::ExtractShared { co_parents, .. } = &task.kind {
            let mut declared = co_parents.clone();
            declared.insert(task.target_node.clone());
            task.region_block_lines = region_block_lines_for(&request.audit_contract, &declared);
        }
        new_tasks.push(task);
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

/// Look a declared ExtractShared parent set up in the request's
/// kernel-authored `audit_contract.shared_proof_blocks.regions` view.
/// A region (representative, or one of its `nested_alternatives`) whose
/// member node set contains every declared parent means those parents
/// share that block; among such candidates the longest block is the
/// honest stat for what the helper could extract. Returns `None` when
/// nothing matches — a narrowed set outside the scanned view, or a
/// region past the render cap — rather than inventing a value.
fn region_block_lines_for(
    audit_contract: &serde_json::Value,
    declared_parents: &BTreeSet<NodeId>,
) -> Option<u32> {
    fn member_nodes(members: &serde_json::Value) -> BTreeSet<&str> {
        members
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|member| {
                        // Representative members are objects with a
                        // `node` key; nested-alternative members are
                        // bare node strings.
                        member
                            .get("node")
                            .and_then(|node| node.as_str())
                            .or_else(|| member.as_str())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    let declared: BTreeSet<&str> = declared_parents.iter().map(|node| node.as_str()).collect();
    let regions = audit_contract
        .get("shared_proof_blocks")
        .and_then(|blocks| blocks.get("regions"))
        .and_then(|regions| regions.as_array())?;
    let mut best: Option<u32> = None;
    for region in regions {
        let mut candidates: Vec<&serde_json::Value> = vec![region];
        if let Some(alternatives) = region
            .get("nested_alternatives")
            .and_then(|alts| alts.as_array())
        {
            candidates.extend(alternatives.iter());
        }
        for candidate in candidates {
            let members = candidate
                .get("members")
                .map(member_nodes)
                .unwrap_or_default();
            if !declared.is_subset(&members) {
                continue;
            }
            let Some(block_lines) = candidate
                .get("block_lines")
                .and_then(|lines| lines.as_u64())
                .and_then(|lines| u32::try_from(lines).ok())
            else {
                continue;
            };
            if best.is_none_or(|current| block_lines > current) {
                best = Some(block_lines);
            }
        }
    }
    best
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
    let mut kind = parse_task_kind(i, &raw.kind)?;
    let mut target_node = NodeId::from(target_node);
    if let CleanupTaskKind::ExtractShared { co_parents, .. } = &mut kind {
        let mut full_parents = co_parents.clone();
        full_parents.insert(target_node.clone());
        let Some(canonical_target) = full_parents.iter().next().cloned() else {
            return Err(format!(
                "new_tasks[{i}].kind=extract_shared requires at least two declared parents"
            ));
        };
        full_parents.remove(&canonical_target);
        target_node = canonical_target;
        *co_parents = full_parents;
    }
    Ok(NewCleanupAuditTask {
        target_node,
        rationale: raw.rationale.clone(),
        confidence,
        kind,
        // Attached by the caller from the request's own dedup scan;
        // never parsed from the raw artifact.
        region_block_lines: None,
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
        "dead_code_elim" | "deadcodeelim" => Ok(CleanupTaskKind::DeadCodeElim {
            hint: raw.hint.clone().unwrap_or_default().trim().to_string(),
        }),
        "extract_helper" | "extracthelper" => {
            let ordinal = raw.ordinal.unwrap_or(0);
            if ordinal < 1 {
                return Err(format!(
                    "new_tasks[{i}].ordinal must be an integer >= 1 for extract_helper; ordinals \
                     number the successive extractions planned for one parent"
                ));
            }
            Ok(CleanupTaskKind::ExtractHelper {
                ordinal,
                hint: raw.hint.clone().unwrap_or_default().trim().to_string(),
            })
        }
        "extract_shared" | "extractshared" => {
            let ordinal = raw.ordinal.unwrap_or(0);
            if ordinal < 1 {
                return Err(format!(
                    "new_tasks[{i}].ordinal must be an integer >= 1 for extract_shared"
                ));
            }
            let Some(raw_parents) = raw.co_parents.as_ref() else {
                return Err(format!(
                    "new_tasks[{i}].co_parents must be an array for extract_shared"
                ));
            };
            let mut co_parents = BTreeSet::new();
            for (parent_index, parent) in raw_parents.iter().enumerate() {
                let parent = parent.trim();
                if parent.is_empty() {
                    return Err(format!(
                        "new_tasks[{i}].co_parents[{parent_index}] must be a non-empty string"
                    ));
                }
                co_parents.insert(NodeId::from(parent));
            }
            Ok(CleanupTaskKind::ExtractShared {
                co_parents,
                ordinal,
                hint: raw.hint.clone().unwrap_or_default().trim().to_string(),
            })
        }
        other => Err(format!(
            "new_tasks[{i}].kind must be one of ['substitution', 'lint_fix', 'dead_code_elim', \
             'extract_helper', 'extract_shared']; got {other:?}"
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
    fn normalize_audit_response_extract_helper_carries_ordinal_and_hint() {
        // Sibling of the `validate_audit_task_kind` allowlist: the raw
        // payload struct is a second place a new field can be dropped
        // without any test noticing.
        let request = build_request();
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "BigParent".into(),
                    rationale: "7295 lines".into(),
                    confidence: "high".into(),
                    kind: RawCleanupTaskKind {
                        kind: "extract_helper".into(),
                        replacement: None,
                        warning_text: None,
                        co_parents: None,
                        ordinal: Some(3),
                        hint: Some("  the fourth case split  ".into()),
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        match &out.response.new_tasks[0].kind {
            CleanupTaskKind::ExtractHelper { ordinal, hint } => {
                assert_eq!(*ordinal, 3);
                assert_eq!(hint, "the fourth case split");
            }
            other => panic!("expected ExtractHelper, got {other:?}"),
        }
    }

    #[test]
    fn normalize_audit_response_dead_code_elim_carries_hint() {
        let input = AuditNormalizationInput {
            request: build_request(),
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "LongProof".into(),
                    rationale: "unused preliminary facts".into(),
                    confidence: "medium".into(),
                    kind: RawCleanupTaskKind {
                        kind: "dead_code_elim".into(),
                        hint: Some("  first have-chain  ".into()),
                        ..RawCleanupTaskKind::default()
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert!(matches!(
            &out.response.new_tasks[0].kind,
            CleanupTaskKind::DeadCodeElim { hint } if hint == "first have-chain"
        ));
    }

    #[test]
    fn normalize_audit_response_extract_shared_canonicalizes_full_parent_set() {
        let input = AuditNormalizationInput {
            request: build_request(),
            raw_payload: RawAuditPayload {
                new_tasks: vec![RawNewCleanupAuditTask {
                    target_node: "ZebraParent".into(),
                    rationale: "shared region".into(),
                    confidence: "high".into(),
                    kind: RawCleanupTaskKind {
                        kind: "extract_shared".into(),
                        co_parents: Some(vec!["BetaParent".into(), "AlphaParent".into()]),
                        ordinal: Some(2),
                        hint: Some("  region-abc  ".into()),
                        ..RawCleanupTaskKind::default()
                    },
                }],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        let task = &out.response.new_tasks[0];
        assert_eq!(task.target_node, NodeId::from("AlphaParent"));
        assert!(matches!(
            &task.kind,
            CleanupTaskKind::ExtractShared {
                co_parents,
                ordinal: 2,
                hint,
            } if co_parents == &BTreeSet::from([
                NodeId::from("BetaParent"),
                NodeId::from("ZebraParent"),
            ]) && hint == "region-abc"
        ));
    }

    /// Kernel-attached region stat: normalization stamps an
    /// ExtractShared proposal with the block length of the matching
    /// region from the request's own `shared_proof_blocks` scan —
    /// preferring the longest candidate (representative or nested
    /// alternative) whose members contain the declared parent set —
    /// and leaves every other kind, and an unmatched set, unstamped.
    #[test]
    fn normalize_audit_response_attaches_region_block_lines_from_request_scan() {
        let mut request = build_request();
        request.audit_contract = serde_json::json!({
            "shared_proof_blocks": {
                "regions": [
                    {
                        "block_lines": 12,
                        "members": [
                            {"node": "ParentA", "body_span": [0, 12]},
                            {"node": "ParentB", "body_span": [4, 16]},
                            {"node": "ParentC", "body_span": [9, 21]},
                        ],
                        "nested_alternatives": [
                            {"block_lines": 20, "members": ["ParentA", "ParentB"]},
                        ],
                    },
                ],
            },
        });
        let extract_shared = |target: &str, co_parents: Vec<String>, ordinal: u32| {
            RawNewCleanupAuditTask {
                target_node: target.into(),
                rationale: "shared region".into(),
                confidence: "medium".into(),
                kind: RawCleanupTaskKind {
                    kind: "extract_shared".into(),
                    co_parents: Some(co_parents),
                    ordinal: Some(ordinal),
                    ..RawCleanupTaskKind::default()
                },
            }
        };
        let input = AuditNormalizationInput {
            request,
            raw_payload: RawAuditPayload {
                new_tasks: vec![
                    // {A, B}: contained by both the 12-line representative
                    // and the 20-line nested alternative — longest wins.
                    extract_shared("ParentB", vec!["ParentA".into()], 1),
                    // {A, B, C}: contained by the representative only.
                    extract_shared("ParentB", vec!["ParentA".into(), "ParentC".into()], 1),
                    // {A, D}: no scanned region contains it — no value.
                    extract_shared("ParentD", vec!["ParentA".into()], 1),
                    // Non-ExtractShared kinds are never stamped.
                    RawNewCleanupAuditTask {
                        target_node: "ParentA".into(),
                        rationale: "unused variable".into(),
                        confidence: "high".into(),
                        kind: RawCleanupTaskKind {
                            kind: "lint_fix".into(),
                            warning_text: Some("unused variable 'x'".into()),
                            ..RawCleanupTaskKind::default()
                        },
                    },
                ],
                outcome: "audit_done".into(),
                ..RawAuditPayload::default()
            },
        };
        let out = normalize_audit_response(&input).expect("normalize");
        assert_eq!(out.response.new_tasks[0].region_block_lines, Some(20));
        assert_eq!(out.response.new_tasks[1].region_block_lines, Some(12));
        assert_eq!(out.response.new_tasks[2].region_block_lines, None);
        assert_eq!(out.response.new_tasks[3].region_block_lines, None);
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
                        co_parents: None,
                        ordinal: None,
                        hint: None,
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
                        co_parents: None,
                        ordinal: None,
                        hint: None,
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
                        co_parents: None,
                        ordinal: None,
                        hint: None,
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
                        co_parents: None,
                        ordinal: None,
                        hint: None,
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
                        co_parents: None,
                        ordinal: None,
                        hint: None,
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
            swept_parents: BTreeSet::new(),
            region_block_lines: None,
        };
        let _: BTreeSet<NodeId> = BTreeSet::from([dummy_task.target_node]);
    }
}
