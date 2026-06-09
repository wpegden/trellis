use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path};

use crate::model::{
    AUDIT_PLAN_MAX_JSON_CHARS, AUDIT_REPORT_TEXT_MAX_CHARS, AUDIT_REPORT_TEXT_MIN_CHARS,
    AUDIT_TASK_BODY_MAX_CHARS, AUDIT_TASK_REASON_MAX_CHARS, AUDIT_TASK_TITLE_MAX_CHARS,
};

const WORKER_OUTCOMES: &[&str] = &["valid", "invalid", "stuck", "needs_restructure"];
const REVIEWER_DECISIONS: &[&str] = &["continue", "advance_phase", "need_input", "done"];
const REVIEWER_NEXT_MODES: &[&str] = &[
    "global",
    "targeted",
    "local",
    "restructure",
    "coarse_restructure",
    "cleanup",
];
const REVIEWER_RESETS: &[&str] = &["none", "last_commit", "last_clean", "theorem_stating_node"];
const REVIEWER_CONTEXT_MODES: &[&str] = &["resume", "fresh"];
const REVIEWER_WORK_STYLE_HINTS: &[&str] = &["none", "restructure"];
const PHASE_DECISIONS: &[&str] = &["PASS", "FAIL"];
const SOUNDNESS_DECISIONS: &[&str] = &["SOUND", "UNSOUND", "STRUCTURAL"];
const OVERALL_DECISIONS: &[&str] = &["APPROVE", "REJECT"];

fn validate_deviation_request_path(id: &str, path: &str, errors: &mut Vec<String>) {
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        errors.push(format!("deviation_requests.{id}.path must be relative"));
    }
    if !path.ends_with(".tex") {
        errors.push(format!("deviation_requests.{id}.path must end with .tex"));
    }
    let components: Vec<Component<'_>> = path_obj.components().collect();
    if components
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        errors.push(format!(
            "deviation_requests.{id}.path must not contain '.', '..', root, or prefix components"
        ));
    }
    let under_reference = components.first().and_then(|component| match component {
        Component::Normal(name) => name.to_str(),
        _ => None,
    }) == Some("reference");
    if !under_reference {
        errors.push(format!(
            "deviation_requests.{id}.path must be under reference/"
        ));
    }
}
const SUBSTANTIVENESS_VERDICTS: &[&str] = &["Pass", "Fail", "NotDoneYet"];
// The corr-node lane has no third state — silence is treated as Fail at the
// kernel normalizer (see `verification_normalization::normalize_corr_response`),
// so the verifier must vote explicitly Pass or Fail for every node it was
// given. NotDoneYet is rejected here at validation time so a stale shape from
// the substantiveness lane doesn't sneak through.
const CORR_NODE_VERDICTS: &[&str] = &["Pass", "Fail"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactValidationOutput {
    pub ok: bool,
    pub errors: Vec<String>,
    pub data: Option<Value>,
}

impl ArtifactValidationOutput {
    fn success(data: Value) -> Self {
        Self {
            ok: true,
            errors: Vec::new(),
            data: Some(data),
        }
    }

    fn failure(errors: Vec<String>) -> Self {
        Self {
            ok: false,
            errors,
            data: None,
        }
    }
}

fn validate_trellis_worker_result_data_inner(
    data: &Value,
    allowed_outcomes: &[String],
) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };

    let mut errors = Vec::new();
    let summary = expect_string(obj.get("summary"), "summary", false, &mut errors);
    let outcome_raw = expect_string(obj.get("outcome"), "outcome", false, &mut errors);
    let comments = expect_comments(obj, &mut errors);
    let outcome = outcome_raw.to_ascii_lowercase();
    let normalized_allowed_outcomes: Vec<String> = allowed_outcomes
        .iter()
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect();
    if !outcome.is_empty()
        && !normalized_allowed_outcomes
            .iter()
            .any(|item| item == &outcome)
    {
        let joined = normalized_allowed_outcomes
            .iter()
            .map(|item| format!("'{item}'"))
            .collect::<Vec<_>>()
            .join(", ");
        errors.push(format!("outcome must be one of [{joined}]"));
    }

    let semantic_dep_updates = normalize_node_string_list_updates(
        obj.get("semantic_dep_updates"),
        "semantic_dep_updates",
        &mut errors,
    );
    let target_claim_updates = normalize_node_string_list_updates(
        obj.get("target_claim_updates"),
        "target_claim_updates",
        &mut errors,
    );
    let node_deviation_claims = normalize_node_string_list_updates(
        obj.get("node_deviation_claims"),
        "node_deviation_claims",
        &mut errors,
    );
    let mut deviation_requests = serde_json::Map::new();
    if let Some(value) = obj.get("deviation_requests") {
        match value {
            Value::Object(map) => {
                for (id, raw) in map {
                    let Some(req) = raw.as_object() else {
                        errors.push(format!("deviation_requests.{id} must be an object"));
                        continue;
                    };
                    let path = expect_string(
                        req.get("path"),
                        "deviation_requests.path",
                        false,
                        &mut errors,
                    );
                    let summary = expect_string(
                        req.get("summary"),
                        "deviation_requests.summary",
                        false,
                        &mut errors,
                    );
                    let affected = expect_string_list(
                        req.get("affected_nodes"),
                        "deviation_requests.affected_nodes",
                        &mut errors,
                    );
                    validate_deviation_request_path(id, &path, &mut errors);
                    deviation_requests.insert(
                        id.clone(),
                        json!({
                            "path": path,
                            "summary": summary,
                            "affected_nodes": affected,
                        }),
                    );
                }
            }
            Value::Null => {}
            _ => errors.push("deviation_requests must be a JSON object".to_string()),
        }
    }
    let difficulty_updates = normalize_string_dict(
        obj.get("difficulty_updates"),
        "difficulty_updates",
        Some(&["easy", "hard"]),
        &mut errors,
    );
    let mut deviation_deletions: Vec<String> = Vec::new();
    if let Some(value) = obj.get("deviation_deletions") {
        match value {
            Value::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(s) => {
                            let trimmed = s.trim();
                            if trimmed.is_empty() {
                                errors.push(format!(
                                    "deviation_deletions[{idx}] must be a non-empty string"
                                ));
                            } else {
                                deviation_deletions.push(trimmed.to_string());
                            }
                        }
                        None => errors.push(format!(
                            "deviation_deletions[{idx}] must be a string"
                        )),
                    }
                }
            }
            Value::Null => {}
            _ => errors.push(
                "deviation_deletions must be a JSON array of deviation ids".to_string(),
            ),
        }
    }
    deviation_deletions.sort();
    deviation_deletions.dedup();
    // Worker must name suggested-broader-scope nodes when reporting
    // needs_restructure, so the next reviewer can authorize the actual
    // structural surface instead of guessing.
    let suggested_nodes_raw = obj.get("needs_restructure_suggested_nodes");
    let mut suggested_nodes: Vec<String> = Vec::new();
    if let Some(value) = suggested_nodes_raw {
        match value {
            Value::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(s) => {
                            let trimmed = s.trim();
                            if trimmed.is_empty() {
                                errors.push(format!(
                                    "needs_restructure_suggested_nodes[{idx}] must be a non-empty string"
                                ));
                            } else {
                                suggested_nodes.push(trimmed.to_string());
                            }
                        }
                        None => errors.push(format!(
                            "needs_restructure_suggested_nodes[{idx}] must be a string"
                        )),
                    }
                }
            }
            Value::Null => {}
            _ => errors.push(
                "needs_restructure_suggested_nodes must be a JSON array of strings".to_string(),
            ),
        }
    }
    if outcome == "needs_restructure" && suggested_nodes.is_empty() {
        errors.push(
            "needs_restructure_suggested_nodes must be a non-empty array of node names when outcome=needs_restructure".to_string(),
        );
    }
    if outcome != "needs_restructure" && !suggested_nodes.is_empty() {
        errors.push(
            "needs_restructure_suggested_nodes must be empty when outcome is not needs_restructure"
                .to_string(),
        );
    }
    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        "summary": summary,
        "outcome": outcome,
        "comments": comments,
        "semantic_dep_updates": semantic_dep_updates,
        "target_claim_updates": target_claim_updates,
        "deviation_requests": deviation_requests,
        "node_deviation_claims": node_deviation_claims,
        "deviation_deletions": deviation_deletions,
        "difficulty_updates": difficulty_updates,
        "needs_restructure_suggested_nodes": suggested_nodes,
    }))
}

pub fn validate_trellis_worker_result_data(data: &Value) -> ArtifactValidationOutput {
    validate_trellis_worker_result_data_with_allowed_outcomes(
        data,
        &WORKER_OUTCOMES
            .iter()
            .map(|item| (*item).to_string())
            .collect::<Vec<_>>(),
    )
}

pub fn validate_trellis_worker_result_data_with_allowed_outcomes(
    data: &Value,
    allowed_outcomes: &[String],
) -> ArtifactValidationOutput {
    validate_trellis_worker_result_data_inner(data, allowed_outcomes)
}

pub fn validate_trellis_reviewer_result_data(data: &Value) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };

    let mut errors = Vec::new();
    let decision_raw = expect_string(obj.get("decision"), "decision", false, &mut errors);
    let reason = expect_string(obj.get("reason"), "reason", false, &mut errors);
    let comments = expect_string(obj.get("comments"), "comments", true, &mut errors);
    let next_active = expect_string(obj.get("next_active"), "next_active", true, &mut errors);
    // Proposal v32: reviewer-chosen coarse anchor for ProofFormalization.
    // Missing field is a valid "preserve current anchor" signal — pass
    // through as an empty string, which the downstream
    // `normalize_optional_node` maps to `None`. Was previously stripped
    // by this allowlist re-emit, silently defaulting reviewer anchor
    // choices to `None` in the live bridge path. See
    // [[feedback_allowlist_validator]].
    let next_active_coarse = expect_string(
        obj.get("next_active_coarse"),
        "next_active_coarse",
        true,
        &mut errors,
    );
    let next_mode_raw = expect_string(obj.get("next_mode"), "next_mode", false, &mut errors);
    let reset_raw = match obj.get("reset") {
        Some(value) => expect_string(Some(value), "reset", false, &mut errors),
        None => "none".to_string(),
    };
    let reset_node = expect_string(
        obj.get("reset_node").or_else(|| obj.get("reset_node_id")),
        "reset_node",
        true,
        &mut errors,
    );
    let task_blocker_ids =
        expect_string_list(obj.get("task_blocker_ids"), "task_blocker_ids", &mut errors);
    // Option C (2026-06-04): `override_blocker_ids` retired. The field
    // is no longer in the reviewer prompt schema; if a legacy or
    // bypassing client still emits it, the value is silently dropped
    // here (validator strips unknown fields). The normalizer also
    // drops the value on its side as defense-in-depth. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    let reset_blocker_ids = expect_string_list(
        obj.get("reset_blocker_ids"),
        "reset_blocker_ids",
        &mut errors,
    );
    // New-soundness contract field. RawReviewPayload accepts it via serde
    // alias, but the allowlist validator re-emits a slimmer JSON, so the
    // field needs an explicit pass-through here or it gets stripped before
    // normalize_review_response ever sees it. [[feedback_allowlist_validator]]
    let request_sound_verifier_node_ids = expect_string_list(
        obj.get("request_sound_verifier_node_ids")
            .or_else(|| obj.get("request_sound_verifier_nodes")),
        "request_sound_verifier_node_ids",
        &mut errors,
    );
    let difficulty_updates = normalize_string_dict(
        obj.get("difficulty_updates"),
        "difficulty_updates",
        Some(&["easy", "hard"]),
        &mut errors,
    );
    let allow_new_obligations = expect_required_bool(
        obj.get("allow_new_obligations"),
        "allow_new_obligations",
        &mut errors,
    );
    let must_close_active = expect_required_bool(
        obj.get("must_close_active"),
        "must_close_active",
        &mut errors,
    );
    let clear_human_input = expect_bool(
        obj.get("clear_human_input"),
        "clear_human_input",
        &mut errors,
    );
    let next_worker_context_mode_raw = match obj.get("next_worker_context_mode") {
        Some(value) => expect_string(Some(value), "next_worker_context_mode", false, &mut errors),
        None => "resume".to_string(),
    };
    let paper_focus_ranges =
        normalize_paper_focus_ranges(obj.get("paper_focus_ranges"), &mut errors);
    let paper_grounding = normalize_paper_grounding(obj.get("paper_grounding"), &mut errors);
    let stuck_math_audit = normalize_stuck_math_audit(obj.get("stuck_math_audit"), &mut errors);
    let work_style_hint_raw = match obj.get("work_style_hint") {
        Some(value) => expect_string(Some(value), "work_style_hint", false, &mut errors),
        None => "none".to_string(),
    };
    let protected_semantic_change_node_ids = expect_string_list(
        obj.get("protected_semantic_change_node_ids")
            .or_else(|| obj.get("protected_semantic_change_nodes")),
        "protected_semantic_change_node_ids",
        &mut errors,
    );
    let confirm_protected_semantic_change_scope = expect_bool(
        obj.get("confirm_protected_semantic_change_scope"),
        "confirm_protected_semantic_change_scope",
        &mut errors,
    );
    // `authorized_node_ids` is Option-shaped: absent ≠ empty list. We
    // emit `null` when the field is missing so the downstream
    // RawReviewPayload deserializer sees `Option::None`; otherwise a
    // validated string list.
    let authorized_node_ids_field = obj
        .get("authorized_node_ids")
        .or_else(|| obj.get("authorized_nodes"));
    let authorized_node_ids: Option<Vec<String>> = if authorized_node_ids_field.is_some() {
        Some(expect_string_list(
            authorized_node_ids_field,
            "authorized_node_ids",
            &mut errors,
        ))
    } else {
        None
    };
    // Cleanup-v2 (audit Finding 2): cleanup-phase reviewer controls.
    // - `cleanup_dismiss_tasks`: array of {task_index: u32, reason: str}
    // - `cleanup_next_task`: Option<u32>
    // - `cleanup_request_reaudit`: bool
    // Missing fields default to empty / None / false so legacy reviewer
    // emissions (no cleanup-v2 controls) remain valid.
    let cleanup_dismiss_tasks =
        normalize_cleanup_dismiss_tasks(obj.get("cleanup_dismiss_tasks"), &mut errors);
    let cleanup_next_task = normalize_optional_u32(
        obj.get("cleanup_next_task"),
        "cleanup_next_task",
        &mut errors,
    );
    let cleanup_request_reaudit = expect_bool(
        obj.get("cleanup_request_reaudit"),
        "cleanup_request_reaudit",
        &mut errors,
    );
    let dismiss_audit_plan = expect_bool(
        obj.get("dismiss_audit_plan"),
        "dismiss_audit_plan",
        &mut errors,
    );
    let dismissed_tasks = normalize_audit_task_dismissals(obj.get("dismissed_tasks"), &mut errors);
    // global_repair_mode Step A: optional sub-object pass-through. Without
    // listing here the allowlist re-emit silently strips the field before
    // RawReviewPayload normalizes it (feedback_allowlist_validator).
    let global_repair_request = match obj.get("global_repair_request") {
        None => Value::Null,
        Some(v) if v.is_null() => Value::Null,
        Some(v) => match v.as_object() {
            None => {
                errors.push("global_repair_request must be an object".to_string());
                Value::Null
            }
            Some(gr_obj) => {
                let proposed = expect_string_list(
                    gr_obj
                        .get("proposed_extension_node_ids")
                        .or_else(|| gr_obj.get("proposed_extension_nodes")),
                    "global_repair_request.proposed_extension_node_ids",
                    &mut errors,
                );
                let reason = expect_string(
                    gr_obj.get("reason"),
                    "global_repair_request.reason",
                    true,
                    &mut errors,
                );
                json!({
                    "proposed_extension_node_ids": proposed,
                    "reason": reason,
                })
            }
        },
    };
    let consume_global_repair_grant = expect_bool(
        obj.get("consume_global_repair_grant"),
        "consume_global_repair_grant",
        &mut errors,
    );

    let decision = decision_raw.to_ascii_lowercase();
    let next_mode = next_mode_raw.to_ascii_lowercase();
    let reset = reset_raw.to_ascii_lowercase();
    let next_worker_context_mode = next_worker_context_mode_raw.to_ascii_lowercase();
    let work_style_hint = work_style_hint_raw.to_ascii_lowercase();

    if !decision.is_empty() && !REVIEWER_DECISIONS.contains(&decision.as_str()) {
        errors.push(
            "decision must be one of ['continue', 'advance_phase', 'need_input', 'done']"
                .to_string(),
        );
    }
    if !next_mode.is_empty() && !REVIEWER_NEXT_MODES.contains(&next_mode.as_str()) {
        errors.push(
            "next_mode must be one of ['global', 'targeted', 'local', 'restructure', 'coarse_restructure', 'cleanup']"
                .to_string(),
        );
    }
    if !reset.is_empty() && !REVIEWER_RESETS.contains(&reset.as_str()) {
        errors.push(
            "reset must be one of ['none', 'last_commit', 'last_clean', 'theorem_stating_node']"
                .to_string(),
        );
    }
    if !next_worker_context_mode.is_empty()
        && !REVIEWER_CONTEXT_MODES.contains(&next_worker_context_mode.as_str())
    {
        errors.push("next_worker_context_mode must be one of ['resume', 'fresh']".to_string());
    }
    if !work_style_hint.is_empty() && !REVIEWER_WORK_STYLE_HINTS.contains(&work_style_hint.as_str())
    {
        errors.push("work_style_hint must be one of ['none', 'restructure']".to_string());
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        "decision": decision,
        "reason": reason,
        "comments": comments,
        "task_blocker_ids": task_blocker_ids,
        "reset_blocker_ids": reset_blocker_ids,
        "request_sound_verifier_node_ids": request_sound_verifier_node_ids,
        "next_active": next_active,
        "next_active_coarse": next_active_coarse,
        "next_mode": next_mode,
        "reset": reset,
        "reset_node": reset_node,
        "difficulty_updates": difficulty_updates,
        "allow_new_obligations": allow_new_obligations,
        "must_close_active": must_close_active,
        "clear_human_input": clear_human_input,
        "next_worker_context_mode": next_worker_context_mode,
        "paper_focus_ranges": paper_focus_ranges,
        "paper_grounding": paper_grounding,
        "stuck_math_audit": stuck_math_audit,
        "work_style_hint": work_style_hint,
        "protected_semantic_change_node_ids": protected_semantic_change_node_ids,
        "confirm_protected_semantic_change_scope": confirm_protected_semantic_change_scope,
        "authorized_node_ids": authorized_node_ids,
        "cleanup_dismiss_tasks": cleanup_dismiss_tasks,
        "cleanup_next_task": cleanup_next_task,
        "cleanup_request_reaudit": cleanup_request_reaudit,
        "dismiss_audit_plan": dismiss_audit_plan,
        "dismissed_tasks": dismissed_tasks,
        "global_repair_request": global_repair_request,
        "consume_global_repair_grant": consume_global_repair_grant,
    }))
}

fn normalize_audit_task_dismissals(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    let Some(arr) = field.as_array() else {
        errors.push("dismissed_tasks must be an array".to_string());
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    let mut seen = HashSet::new();
    for (i, entry) in arr.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            errors.push(format!("dismissed_tasks[{i}] must be an object"));
            continue;
        };
        let id = expect_string(
            obj.get("id"),
            &format!("dismissed_tasks[{i}].id"),
            false,
            errors,
        );
        if !id.is_empty() && !seen.insert(id.clone()) {
            errors.push(format!(
                "dismissed_tasks[{i}].id duplicates an earlier dismissal"
            ));
        }
        let reason = expect_string(
            obj.get("reason"),
            &format!("dismissed_tasks[{i}].reason"),
            false,
            errors,
        );
        if reason.chars().count() > AUDIT_TASK_REASON_MAX_CHARS {
            errors.push(format!(
                "dismissed_tasks[{i}].reason must be at most {AUDIT_TASK_REASON_MAX_CHARS} characters"
            ));
        }
        out.push(json!({
            "id": id,
            "reason": reason,
        }));
    }
    out
}

/// Cleanup-v2 (audit Finding 2): validate and normalize the reviewer's
/// `cleanup_dismiss_tasks` field. Accepts an array of objects of shape
/// `{"task_index": <non-negative int>, "reason": "<str>"}`. Missing
/// fields default to empty / 0 / empty string; non-object entries are
/// flagged as errors.
fn normalize_cleanup_dismiss_tasks(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    let Some(arr) = field.as_array() else {
        errors.push("cleanup_dismiss_tasks must be an array".to_string());
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            errors.push(format!(
                "cleanup_dismiss_tasks[{i}] must be an object with task_index and reason"
            ));
            continue;
        };
        let task_index = match entry_obj.get("task_index").and_then(|v| v.as_u64()) {
            Some(idx) => idx as u32,
            None => {
                errors.push(format!(
                    "cleanup_dismiss_tasks[{i}].task_index must be a non-negative integer"
                ));
                0
            }
        };
        let reason = entry_obj
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(json!({"task_index": task_index, "reason": reason}));
    }
    out
}

/// Cleanup-v2 (audit Finding 2): parse Option<u32> from a JSON value.
/// Accepts a missing field, JSON null, or a non-negative integer. Other
/// shapes produce an error.
fn normalize_optional_u32(
    value: Option<&Value>,
    field_name: &str,
    errors: &mut Vec<String>,
) -> Option<u32> {
    match value {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n as u32),
            None => {
                errors.push(format!(
                    "{field_name} must be null or a non-negative integer"
                ));
                None
            }
        },
    }
}

/// Shared "single-decision-block" validator used by correspondence,
/// paper-faithfulness, and substantiveness result data. Each caller
/// supplies the block key (matches both the JSON field and the success
/// JSON key) and a per-lane block validator. The PASS-vs-other → APPROVE
/// expectation is uniform; `reason_phrase` is interpolated into the
/// mismatch error so reject-lane prompts stay byte-identical.
fn validate_single_decision_result_data(
    data: &Value,
    block_key: &str,
    block_validator: impl FnOnce(Option<&Value>, &str) -> (Option<Value>, Vec<String>),
    reason_phrase: &str,
) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };

    let mut errors = Vec::new();
    let (block, block_errors) = block_validator(obj.get(block_key), block_key);
    let summary = expect_string(obj.get("summary"), "summary", false, &mut errors);
    let overall = expect_string(obj.get("overall"), "overall", false, &mut errors);
    let comments = expect_comments(obj, &mut errors);

    errors.extend(block_errors);
    if !overall.is_empty() && !OVERALL_DECISIONS.contains(&overall.as_str()) {
        errors.push("overall must be one of ['APPROVE', 'REJECT']".to_string());
    }
    let expected_overall = if block
        .as_ref()
        .and_then(|phase| phase.get("decision"))
        .and_then(Value::as_str)
        .is_some_and(|decision| decision == "PASS")
    {
        "APPROVE"
    } else {
        "REJECT"
    };
    if errors.is_empty() && !overall.is_empty() && overall != expected_overall {
        errors.push(format!(
            "overall must be {expected_overall} for the supplied {reason_phrase}"
        ));
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        block_key: block.expect("validated block"),
        "overall": overall,
        "summary": summary,
        "comments": comments,
    }))
}

pub fn validate_correspondence_result_data(data: &Value) -> ArtifactValidationOutput {
    validate_single_decision_result_data(
        data,
        "correspondence",
        validate_corr_node_block,
        "phase decisions",
    )
}

pub fn validate_paper_faithfulness_result_data(data: &Value) -> ArtifactValidationOutput {
    validate_single_decision_result_data(
        data,
        "paper_faithfulness",
        validate_phase_block,
        "phase decisions",
    )
}

pub fn validate_deviation_authorization_result_data(data: &Value) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };

    let mut errors = Vec::new();
    let Some(dev) = obj
        .get("deviation_authorization")
        .and_then(Value::as_object)
    else {
        return ArtifactValidationOutput::failure(vec![
            "deviation_authorization must be an object".to_string(),
        ]);
    };
    let id = expect_string(
        dev.get("id"),
        "deviation_authorization.id",
        false,
        &mut errors,
    );
    let decision = expect_string(
        dev.get("decision"),
        "deviation_authorization.decision",
        false,
        &mut errors,
    );
    let comment = expect_string(
        dev.get("comment"),
        "deviation_authorization.comment",
        true,
        &mut errors,
    );
    let summary = expect_string(obj.get("summary"), "summary", false, &mut errors);
    let overall = expect_string(obj.get("overall"), "overall", false, &mut errors);
    let comments = expect_comments(obj, &mut errors);

    if !decision.is_empty() && !["PASS", "FAIL"].contains(&decision.as_str()) {
        errors.push("deviation_authorization.decision must be one of ['PASS', 'FAIL']".to_string());
    }
    if decision == "FAIL" && comment.trim().is_empty() {
        errors.push("deviation_authorization.comment is required when decision=FAIL".to_string());
    }
    if !overall.is_empty() && !OVERALL_DECISIONS.contains(&overall.as_str()) {
        errors.push("overall must be one of ['APPROVE', 'REJECT']".to_string());
    }
    let expected_overall = if decision == "PASS" {
        "APPROVE"
    } else {
        "REJECT"
    };
    if errors.is_empty() && !overall.is_empty() && overall != expected_overall {
        errors.push(format!(
            "overall must be {expected_overall} for the supplied deviation decision"
        ));
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        "deviation_authorization": {
            "id": id,
            "decision": decision,
            "comment": comment,
        },
        "overall": overall,
        "summary": summary,
        "comments": comments,
    }))
}

/// Cleanup-v2 (audit Finding 1): validate the audit-burst JSON artifact
/// shape. Mirrors the `cleanup_audit_result_v1` schema documented in
/// `request_contracts.rs::audit_contract_payload`:
///   - `new_tasks`: array of {target_node, rationale, confidence, kind}
///   - `task_modifications`: array of {task_index, reason}
///   - `scratchpad_replace`: string
///   - `outcome`: "audit_done" | "need_to_continue"
///
/// This is a thin shape-only validator. Domain legality (target_node ∈
/// present, target ∉ protected, replacement validity, etc.) is enforced
/// by the kernel `apply_audit_response` handler via `legal_cleanup_task`
/// against the live ProtocolState — not by this artifact validator,
/// which only sees the raw JSON.
pub fn validate_trellis_audit_result_data(data: &Value) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };
    let mut errors = Vec::new();
    let outcome_raw = expect_string(obj.get("outcome"), "outcome", false, &mut errors);
    let scratchpad_replace = expect_string(
        obj.get("scratchpad_replace"),
        "scratchpad_replace",
        true,
        &mut errors,
    );
    let new_tasks = validate_audit_new_tasks(obj.get("new_tasks"), &mut errors);
    let task_modifications =
        validate_audit_task_modifications(obj.get("task_modifications"), &mut errors);

    let outcome = outcome_raw.trim().to_ascii_lowercase();
    if !outcome.is_empty()
        && outcome.as_str() != "audit_done"
        && outcome.as_str() != "need_to_continue"
        && outcome.as_str() != "done"
        && outcome.as_str() != "continue"
    {
        errors.push("outcome must be one of ['audit_done', 'need_to_continue']".to_string());
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        "new_tasks": new_tasks,
        "task_modifications": task_modifications,
        "scratchpad_replace": scratchpad_replace,
        "outcome": outcome,
    }))
}

pub fn validate_trellis_stuck_math_audit_result_data(data: &Value) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };
    let mut errors = Vec::new();
    let confirm_need_input = expect_bool(
        obj.get("confirm_need_input"),
        "confirm_need_input",
        &mut errors,
    );
    let report = expect_string(obj.get("report"), "report", false, &mut errors);
    let report_len = report.chars().count();
    if report_len > 0 && report_len < AUDIT_REPORT_TEXT_MIN_CHARS {
        errors.push(format!(
            "report must contain at least {AUDIT_REPORT_TEXT_MIN_CHARS} characters"
        ));
    }
    if report_len > AUDIT_REPORT_TEXT_MAX_CHARS {
        errors.push(format!(
            "report must contain at most {AUDIT_REPORT_TEXT_MAX_CHARS} characters"
        ));
    }
    let tasks = validate_stuck_math_audit_tasks(obj.get("tasks"), &mut errors);
    let probe_paths = validate_stuck_math_audit_probe_paths(obj.get("probe_paths"), &mut errors);
    let cone_clean_node = expect_string(
        obj.get("cone_clean_node")
            .or_else(|| obj.get("recommended_cone_clean_node")),
        "cone_clean_node",
        true,
        &mut errors,
    );
    // global_repair_mode Step B pass-through. Allowlist re-emit would
    // strip these without explicit listing (feedback_allowlist_validator).
    let global_repair_approve = expect_bool(
        obj.get("global_repair_approve"),
        "global_repair_approve",
        &mut errors,
    );
    let global_repair_approved_extension_node_ids = expect_string_list(
        obj.get("global_repair_approved_extension_node_ids")
            .or_else(|| obj.get("global_repair_approved_extension_nodes")),
        "global_repair_approved_extension_node_ids",
        &mut errors,
    );
    let global_repair_auditor_reason = expect_string(
        obj.get("global_repair_auditor_reason"),
        "global_repair_auditor_reason",
        true,
        &mut errors,
    );
    if probe_paths.is_empty()
        && !report.contains("```")
        && !report.contains("## Claim being audited")
    {
        errors.push(
            "report must include a concrete signal: probe_paths, a fenced code block, or a '## Claim being audited' heading"
                .to_string(),
        );
    }
    let plan_view = json!({
        "confirm_need_input": confirm_need_input,
        "report": report,
        "tasks": tasks,
        "probe_paths": probe_paths,
        "cone_clean_node": cone_clean_node,
        "global_repair_approve": global_repair_approve,
        "global_repair_approved_extension_node_ids": global_repair_approved_extension_node_ids,
        "global_repair_auditor_reason": global_repair_auditor_reason,
    });
    if let Ok(text) = serde_json::to_string(&plan_view) {
        if text.chars().count() > AUDIT_PLAN_MAX_JSON_CHARS {
            errors.push(format!(
                "stuck math audit plan must serialize to at most {AUDIT_PLAN_MAX_JSON_CHARS} JSON characters"
            ));
        }
    }
    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }
    ArtifactValidationOutput::success(plan_view)
}

fn validate_stuck_math_audit_tasks(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    let Some(arr) = field.as_array() else {
        errors.push("tasks must be an array".to_string());
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    let mut seen = HashSet::new();
    for (i, entry) in arr.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            errors.push(format!("tasks[{i}] must be an object"));
            continue;
        };
        let id = expect_string(
            entry_obj.get("id"),
            &format!("tasks[{i}].id"),
            false,
            errors,
        );
        if !id.is_empty() && !seen.insert(id.clone()) {
            errors.push(format!("tasks[{i}].id duplicates an earlier task id"));
        }
        let title = expect_string(
            entry_obj.get("title"),
            &format!("tasks[{i}].title"),
            false,
            errors,
        );
        if title.chars().count() > AUDIT_TASK_TITLE_MAX_CHARS {
            errors.push(format!(
                "tasks[{i}].title must be at most {AUDIT_TASK_TITLE_MAX_CHARS} characters"
            ));
        }
        let body = expect_string(
            entry_obj.get("body"),
            &format!("tasks[{i}].body"),
            false,
            errors,
        );
        if body.chars().count() > AUDIT_TASK_BODY_MAX_CHARS {
            errors.push(format!(
                "tasks[{i}].body must be at most {AUDIT_TASK_BODY_MAX_CHARS} characters"
            ));
        }
        if entry_obj
            .get("dismissed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || entry_obj
                .get("dismissed_reason")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
            || entry_obj.get("dismissed_at_cycle").is_some()
        {
            errors.push(format!(
                "tasks[{i}] must not pre-populate reviewer dismissal fields"
            ));
        }
        out.push(json!({
            "id": id,
            "title": title,
            "body": body,
            "dismissed": false,
            "dismissed_reason": "",
            "dismissed_at_cycle": null,
        }));
    }
    out
}

fn validate_stuck_math_audit_probe_paths(
    value: Option<&Value>,
    errors: &mut Vec<String>,
) -> Vec<String> {
    let paths = expect_string_list(value, "probe_paths", errors);
    let mut out = Vec::with_capacity(paths.len());
    for (i, path) in paths.into_iter().enumerate() {
        if path.starts_with('/')
            || path.contains("..")
            || !path.starts_with(".trellis/stuck-math-audit/")
        {
            errors.push(format!(
                "probe_paths[{i}] must be relative under .trellis/stuck-math-audit/"
            ));
            continue;
        }
        out.push(path);
    }
    out
}

fn validate_audit_new_tasks(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    let Some(arr) = field.as_array() else {
        errors.push("new_tasks must be an array".to_string());
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            errors.push(format!("new_tasks[{i}] must be an object"));
            continue;
        };
        let target_node = entry_obj
            .get("target_node")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if target_node.is_empty() {
            errors.push(format!(
                "new_tasks[{i}].target_node must be a non-empty string"
            ));
        }
        let rationale = entry_obj
            .get("rationale")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let confidence = entry_obj
            .get("confidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !confidence.is_empty()
            && confidence != "high"
            && confidence != "medium"
            && confidence != "low"
        {
            errors.push(format!(
                "new_tasks[{i}].confidence must be one of ['high', 'medium', 'low']"
            ));
        }
        let kind_value = entry_obj.get("kind");
        let kind = validate_audit_task_kind(kind_value, &format!("new_tasks[{i}].kind"), errors);
        out.push(json!({
            "target_node": target_node,
            "rationale": rationale,
            "confidence": confidence,
            "kind": kind,
        }));
    }
    out
}

fn validate_audit_task_kind(
    value: Option<&Value>,
    prefix: &str,
    errors: &mut Vec<String>,
) -> Value {
    let Some(field) = value else {
        errors.push(format!("{prefix} is required"));
        return json!(null);
    };
    let Some(obj) = field.as_object() else {
        errors.push(format!("{prefix} must be an object"));
        return json!(null);
    };
    let kind_tag = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match kind_tag.as_str() {
        "substitution" => {
            let replacement = obj.get("replacement");
            let replacement_value = match replacement {
                Some(Value::Object(rep_obj)) => {
                    let rep_kind = rep_obj
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    match rep_kind.as_str() {
                        "mathlib" => {
                            let citation = rep_obj
                                .get("citation")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            if citation.is_empty() {
                                errors.push(format!(
                                    "{prefix}.replacement.citation must be a non-empty string"
                                ));
                            }
                            json!({"kind": "mathlib", "citation": citation})
                        }
                        "tablet_wrapper" => {
                            let node = rep_obj
                                .get("node")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            if node.is_empty() {
                                errors.push(format!(
                                    "{prefix}.replacement.node must be a non-empty string"
                                ));
                            }
                            json!({"kind": "tablet_wrapper", "node": node})
                        }
                        _ => {
                            errors.push(format!(
                                "{prefix}.replacement.kind must be one of ['mathlib', 'tablet_wrapper']"
                            ));
                            json!(null)
                        }
                    }
                }
                _ => {
                    errors.push(format!("{prefix}.replacement must be an object"));
                    json!(null)
                }
            };
            json!({"kind": "substitution", "replacement": replacement_value})
        }
        "lint_fix" | "lintfix" => {
            let warning_text = obj
                .get("warning_text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if warning_text.trim().is_empty() {
                errors.push(format!("{prefix}.warning_text must be a non-empty string"));
            }
            json!({"kind": "lint_fix", "warning_text": warning_text})
        }
        _ => {
            errors.push(format!(
                "{prefix}.kind must be one of ['substitution', 'lint_fix']"
            ));
            json!(null)
        }
    }
}

fn validate_audit_task_modifications(
    value: Option<&Value>,
    errors: &mut Vec<String>,
) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    let Some(arr) = field.as_array() else {
        errors.push("task_modifications must be an array".to_string());
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            errors.push(format!("task_modifications[{i}] must be an object"));
            continue;
        };
        let task_index = match entry_obj.get("task_index").and_then(|v| v.as_u64()) {
            Some(n) => n as u32,
            None => {
                errors.push(format!(
                    "task_modifications[{i}].task_index must be a non-negative integer"
                ));
                0
            }
        };
        let reason = entry_obj
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(json!({"task_index": task_index, "reason": reason}));
    }
    out
}

pub fn validate_substantiveness_result_data(data: &Value) -> ArtifactValidationOutput {
    validate_single_decision_result_data(
        data,
        "substantiveness",
        validate_substantiveness_block,
        "substantiveness decision",
    )
}

pub fn validate_soundness_result_data(data: &Value, node_name: &str) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };

    let mut errors = Vec::new();
    let node = expect_string(obj.get("node"), "node", false, &mut errors);
    let summary = expect_string(obj.get("summary"), "summary", false, &mut errors);
    let overall = expect_string(obj.get("overall"), "overall", false, &mut errors);
    let comments = expect_comments(obj, &mut errors);
    let (soundness, soundness_errors) = validate_soundness_block(obj.get("soundness"));
    errors.extend(soundness_errors);

    if !node.is_empty() && node != node_name {
        errors.push(format!("node must equal {node_name}"));
    }
    if !overall.is_empty() && !OVERALL_DECISIONS.contains(&overall.as_str()) {
        errors.push("overall must be one of ['APPROVE', 'REJECT']".to_string());
    }
    let expected_overall = if soundness
        .as_ref()
        .and_then(|block| block.get("decision"))
        .and_then(Value::as_str)
        .is_some_and(|decision| decision == "SOUND")
    {
        "APPROVE"
    } else {
        "REJECT"
    };
    if errors.is_empty() && !overall.is_empty() && overall != expected_overall {
        errors.push(format!(
            "overall must be {expected_overall} when soundness.decision is {}",
            soundness
                .as_ref()
                .and_then(|block| block.get("decision"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ));
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    ArtifactValidationOutput::success(json!({
        "node": node,
        "soundness": soundness.expect("validated soundness block"),
        "overall": overall,
        "summary": summary,
        "comments": comments,
    }))
}

fn expect_comments(obj: &serde_json::Map<String, Value>, errors: &mut Vec<String>) -> String {
    if obj.contains_key("comments") {
        return expect_string(obj.get("comments"), "comments", true, errors);
    }
    if obj.contains_key("feedback") {
        return expect_string(obj.get("feedback"), "feedback", true, errors);
    }
    String::new()
}

fn normalize_paper_focus_ranges(value: Option<&Value>, errors: &mut Vec<String>) -> Value {
    match value {
        None => json!([]),
        Some(Value::Array(items)) => {
            let mut normalized = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    errors.push(
                        "paper_focus_ranges must be a list of {start_line, end_line, reason}"
                            .to_string(),
                    );
                    return json!([]);
                };
                let mut local_errors = Vec::new();
                let start_line = expect_u64(obj.get("start_line"), "start_line", &mut local_errors);
                let end_line = expect_u64(obj.get("end_line"), "end_line", &mut local_errors);
                let reason = expect_string(obj.get("reason"), "reason", true, &mut local_errors);
                if !local_errors.is_empty() {
                    errors.push(
                        "paper_focus_ranges must be a list of {start_line, end_line, reason}"
                            .to_string(),
                    );
                    return json!([]);
                }
                if start_line == 0 || end_line < start_line {
                    errors.push(
                        "paper_focus_ranges must be a list of {start_line >= 1, end_line >= start_line, reason}"
                            .to_string(),
                    );
                    return json!([]);
                }
                normalized.push(json!({
                    "start_line": start_line,
                    "end_line": end_line,
                    "reason": reason,
                }));
            }
            Value::Array(normalized)
        }
        Some(_) => {
            errors.push(
                "paper_focus_ranges must be a list of {start_line, end_line, reason}".to_string(),
            );
            json!([])
        }
    }
}

/// Shape-only normalization for the reviewer's `paper_grounding`
/// attestation. The request-aware rule about *when* attestation is
/// required is enforced by
/// `WrapperRequest::review_response_paper_grounding_legal`; this just
/// makes sure the field is either absent (→ default false/empty) or
/// a well-shaped object with the two expected keys.
fn normalize_paper_grounding(value: Option<&Value>, errors: &mut Vec<String>) -> Value {
    let default = json!({"consulted_cited_ranges": false, "basis_summary": ""});
    match value {
        None => default,
        Some(Value::Object(obj)) => {
            let consulted = expect_bool(
                obj.get("consulted_cited_ranges"),
                "paper_grounding.consulted_cited_ranges",
                errors,
            );
            let summary = expect_string(
                obj.get("basis_summary"),
                "paper_grounding.basis_summary",
                true,
                errors,
            );
            json!({
                "consulted_cited_ranges": consulted,
                "basis_summary": summary,
            })
        }
        Some(_) => {
            errors.push("paper_grounding must be an object".to_string());
            default
        }
    }
}

/// Shape-only normalization for the reviewer's optional StuckMathAudit
/// report. The request-aware rule about when this field is required is
/// enforced by `WrapperRequest::review_response_legal`; this function only
/// normalizes the schema-light object that the Rust deserializer consumes.
fn normalize_stuck_math_audit(value: Option<&Value>, errors: &mut Vec<String>) -> Value {
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Object(obj)) => {
            let notes = expect_string(obj.get("notes"), "stuck_math_audit.notes", true, errors);
            let reviewer_lean_product = obj
                .get("reviewer_lean_product")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or(Value::Null);
            if !reviewer_lean_product.is_null()
                && !crate::model::stuck_math_reviewer_lean_product_within_limit(
                    &reviewer_lean_product,
                )
            {
                errors.push(format!(
                    "stuck_math_audit.reviewer_lean_product must serialize to at most {} JSON characters; put larger artifacts on disk and include a compact summary/path",
                    crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
                ));
            }
            json!({
                "notes": notes,
                "reviewer_lean_product": reviewer_lean_product,
            })
        }
        Some(_) => {
            errors.push("stuck_math_audit must be an object".to_string());
            Value::Null
        }
    }
}

fn validate_phase_block(value: Option<&Value>, field: &str) -> (Option<Value>, Vec<String>) {
    let Some(value) = value else {
        return (None, vec![format!("{field} must be an object")]);
    };
    let Some(obj) = value.as_object() else {
        return (None, vec![format!("{field} must be an object")]);
    };

    let mut errors = Vec::new();
    let decision = expect_string(
        obj.get("decision"),
        &format!("{field}.decision"),
        false,
        &mut errors,
    );
    let issues = validate_issue_list(obj.get("issues"), &format!("{field}.issues"), &mut errors);
    if !decision.is_empty() && !PHASE_DECISIONS.contains(&decision.as_str()) {
        errors.push(format!("{field}.decision must be one of ['PASS', 'FAIL']"));
    }
    if decision == "PASS" && !issues.is_empty() {
        errors.push(format!(
            "{field}.issues must be [] when {field}.decision is PASS"
        ));
    }
    if decision == "FAIL" && issues.is_empty() {
        errors.push(format!(
            "{field}.issues must be non-empty when {field}.decision is FAIL"
        ));
    }

    if !errors.is_empty() {
        return (None, errors);
    }

    (
        Some(json!({
            "decision": decision,
            "issues": issues,
        })),
        Vec::new(),
    )
}

/// Validate a correspondence per-node phase block. Mirrors
/// `validate_substantiveness_block` but with two corr-specific rules:
///   - allowed verdict values are `["Pass", "Fail"]` only (NotDoneYet is
///     rejected — corr has no third state, silence defaults to Fail at the
///     normalizer)
///   - `decision == FAIL` requires at least one `Fail` verdict (parallel to
///     substantiveness; symmetric `decision == PASS` rejects any Fail verdict)
fn validate_corr_node_block(value: Option<&Value>, field: &str) -> (Option<Value>, Vec<String>) {
    let Some(value) = value else {
        return (None, vec![format!("{field} must be an object")]);
    };
    let Some(obj) = value.as_object() else {
        return (None, vec![format!("{field} must be an object")]);
    };

    let mut errors = Vec::new();
    let decision = expect_string(
        obj.get("decision"),
        &format!("{field}.decision"),
        false,
        &mut errors,
    );
    let verdicts = validate_verdict_list_with_allowed(
        obj.get("verdicts"),
        &format!("{field}.verdicts"),
        &mut errors,
        CORR_NODE_VERDICTS,
    );
    if !decision.is_empty() && !PHASE_DECISIONS.contains(&decision.as_str()) {
        errors.push(format!("{field}.decision must be one of ['PASS', 'FAIL']"));
    }

    // Lane-decision consistency mirrors substantiveness: PASS iff no Fail
    // verdict, FAIL iff at least one Fail verdict.
    let any_fail = verdicts.iter().any(|item| {
        item.get("verdict")
            .and_then(Value::as_str)
            .is_some_and(|v| v == "Fail")
    });
    if decision == "PASS" && any_fail {
        errors.push(format!(
            "{field}.decision must be FAIL when any verdict is Fail"
        ));
    }
    if decision == "FAIL" && !any_fail {
        errors.push(format!(
            "{field}.decision must be PASS when no verdict is Fail"
        ));
    }

    if !errors.is_empty() {
        return (None, errors);
    }

    (
        Some(json!({
            "decision": decision,
            "verdicts": verdicts,
        })),
        Vec::new(),
    )
}

fn validate_substantiveness_block(
    value: Option<&Value>,
    field: &str,
) -> (Option<Value>, Vec<String>) {
    let Some(value) = value else {
        return (None, vec![format!("{field} must be an object")]);
    };
    let Some(obj) = value.as_object() else {
        return (None, vec![format!("{field} must be an object")]);
    };

    let mut errors = Vec::new();
    let decision = expect_string(
        obj.get("decision"),
        &format!("{field}.decision"),
        false,
        &mut errors,
    );
    let verdicts = validate_verdict_list(
        obj.get("verdicts"),
        &format!("{field}.verdicts"),
        &mut errors,
    );
    if !decision.is_empty() && !PHASE_DECISIONS.contains(&decision.as_str()) {
        errors.push(format!("{field}.decision must be one of ['PASS', 'FAIL']"));
    }

    // Lane-decision consistency:
    //   - PASS iff no verdict is `Fail`.
    //   - FAIL iff at least one verdict is `Fail`.
    let any_fail = verdicts.iter().any(|item| {
        item.get("verdict")
            .and_then(Value::as_str)
            .is_some_and(|v| v == "Fail")
    });
    if decision == "PASS" && any_fail {
        errors.push(format!(
            "{field}.decision must be FAIL when any verdict is Fail"
        ));
    }
    if decision == "FAIL" && !any_fail {
        errors.push(format!(
            "{field}.decision must be PASS when no verdict is Fail (NotDoneYet alone does not Fail the lane)"
        ));
    }

    if !errors.is_empty() {
        return (None, errors);
    }

    (
        Some(json!({
            "decision": decision,
            "verdicts": verdicts,
        })),
        Vec::new(),
    )
}

fn validate_verdict_list(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<String>,
) -> Vec<Value> {
    validate_verdict_list_with_allowed(value, field, errors, SUBSTANTIVENESS_VERDICTS)
}

fn validate_verdict_list_with_allowed(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<String>,
    allowed: &[&str],
) -> Vec<Value> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(format!("{field} must be a list"));
        return Vec::new();
    };

    let mut normalized = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let allowed_label = format!(
        "[{}]",
        allowed
            .iter()
            .map(|v| format!("'{v}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (idx, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            errors.push(format!("{field}[{idx}] must be an object"));
            continue;
        };
        let node = expect_string(
            obj.get("node"),
            &format!("{field}[{idx}].node"),
            false,
            errors,
        );
        let verdict = expect_string(
            obj.get("verdict"),
            &format!("{field}[{idx}].verdict"),
            false,
            errors,
        );
        // The `[NotDoneYet]` suffix hack is retired — the verdict goes in
        // its own field now.
        if node.ends_with("[NotDoneYet]") {
            errors.push(format!(
                "{field}[{idx}].node must not carry the '[NotDoneYet]' suffix (use verdict: 'NotDoneYet' instead)"
            ));
        }
        if !verdict.is_empty() && !allowed.contains(&verdict.as_str()) {
            errors.push(format!(
                "{field}[{idx}].verdict must be one of {allowed_label}"
            ));
        }
        let comment = match obj.get("comment") {
            Some(value) => expect_string(
                Some(value),
                &format!("{field}[{idx}].comment"),
                true,
                errors,
            ),
            None => String::new(),
        };
        if verdict == "Fail" && comment.is_empty() {
            errors.push(format!(
                "{field}[{idx}].comment must be non-empty when verdict is Fail"
            ));
        }
        if node.is_empty() || verdict.is_empty() {
            continue;
        }
        if !seen.insert(node.clone()) {
            errors.push(format!(
                "{field} contains duplicate verdict for node {node:?}"
            ));
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert("node".to_string(), Value::String(node));
        entry.insert("verdict".to_string(), Value::String(verdict));
        if !comment.is_empty() {
            entry.insert("comment".to_string(), Value::String(comment));
        }
        normalized.push(Value::Object(entry));
    }
    normalized
}

fn validate_soundness_block(value: Option<&Value>) -> (Option<Value>, Vec<String>) {
    let Some(value) = value else {
        return (None, vec!["soundness must be an object".to_string()]);
    };
    let Some(obj) = value.as_object() else {
        return (None, vec!["soundness must be an object".to_string()]);
    };

    let mut errors = Vec::new();
    let decision = expect_string(
        obj.get("decision"),
        "soundness.decision",
        false,
        &mut errors,
    );
    let explanation = expect_string(
        obj.get("explanation"),
        "soundness.explanation",
        false,
        &mut errors,
    );
    if !decision.is_empty() && !SOUNDNESS_DECISIONS.contains(&decision.as_str()) {
        errors.push(
            "soundness.decision must be one of ['SOUND', 'UNSOUND', 'STRUCTURAL']".to_string(),
        );
    }

    if !errors.is_empty() {
        return (None, errors);
    }

    (
        Some(json!({
            "decision": decision,
            "explanation": explanation,
        })),
        Vec::new(),
    )
}

fn validate_issue_list(value: Option<&Value>, field: &str, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(format!("{field} must be a list"));
        return Vec::new();
    };

    let mut normalized = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            errors.push(format!("{field}[{idx}] must be an object"));
            continue;
        };
        let node = expect_string(
            obj.get("node"),
            &format!("{field}[{idx}].node"),
            false,
            errors,
        );
        let description = expect_string(
            obj.get("description"),
            &format!("{field}[{idx}].description"),
            false,
            errors,
        );
        if node.is_empty() || description.is_empty() {
            continue;
        }
        normalized.push(json!({
            "node": node,
            "description": description,
        }));
    }
    normalized
}

fn expect_string(
    value: Option<&Value>,
    field: &str,
    allow_empty: bool,
    errors: &mut Vec<String>,
) -> String {
    // JSON `null` is treated the same as a missing field for the
    // `allow_empty=true` path. Reviewers commonly write `"foo": null`
    // when they mean "no value"; previously this raised "must be a
    // string" even when omitting the key was legal. The
    // `allow_empty=false` path still rejects null (a required field
    // must be a concrete string).
    let Some(value) = value else {
        if allow_empty {
            return String::new();
        }
        errors.push(format!("{field} must be non-empty"));
        return String::new();
    };
    if value.is_null() {
        if allow_empty {
            return String::new();
        }
        errors.push(format!("{field} must be non-empty"));
        return String::new();
    }
    let Some(text) = value.as_str() else {
        errors.push(format!("{field} must be a string"));
        return String::new();
    };
    let text = text.trim().to_string();
    if !allow_empty && text.is_empty() {
        errors.push(format!("{field} must be non-empty"));
    }
    text
}

fn expect_bool(value: Option<&Value>, field: &str, errors: &mut Vec<String>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Some(flag) = value.as_bool() else {
        errors.push(format!("{field} must be a boolean"));
        return false;
    };
    flag
}

fn expect_required_bool(value: Option<&Value>, field: &str, errors: &mut Vec<String>) -> bool {
    let Some(value) = value else {
        errors.push(format!("{field} must be a boolean"));
        return false;
    };
    expect_bool(Some(value), field, errors)
}

fn expect_u64(value: Option<&Value>, field: &str, errors: &mut Vec<String>) -> u64 {
    match value.and_then(Value::as_u64) {
        Some(parsed) => parsed,
        None => {
            errors.push(format!("{field} must be an integer"));
            0
        }
    }
}

fn expect_string_list(value: Option<&Value>, field: &str, errors: &mut Vec<String>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(format!("{field} must be a list"));
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let Some(text) = item.as_str() else {
            errors.push(format!("{field}[{idx}] must be a string"));
            continue;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            errors.push(format!("{field}[{idx}] must be non-empty"));
            continue;
        }
        if seen.insert(text.clone()) {
            normalized.push(text);
        }
    }
    normalized
}

fn normalize_node_string_list_updates(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<String>,
) -> BTreeMap<String, Vec<String>> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Some(obj) = value.as_object() else {
        errors.push(format!("{field} must be an object"));
        return BTreeMap::new();
    };

    let mut normalized = BTreeMap::new();
    for (raw_key, raw_value) in obj {
        let key = raw_key.trim();
        if key.is_empty() {
            errors.push(format!("{field} keys must be non-empty strings"));
            continue;
        }
        let Some(items) = raw_value.as_array() else {
            errors.push(format!("{field}.{key} must be a list"));
            continue;
        };
        let mut seen = HashSet::new();
        let mut normalized_items = Vec::new();
        let mut item_errors = false;
        for (idx, item) in items.iter().enumerate() {
            let Some(text) = item.as_str() else {
                errors.push(format!("{field}.{key}[{idx}] must be a string"));
                item_errors = true;
                continue;
            };
            let text = text.trim().to_string();
            if text.is_empty() {
                errors.push(format!("{field}.{key}[{idx}] must be non-empty"));
                item_errors = true;
                continue;
            }
            if seen.insert(text.clone()) {
                normalized_items.push(text);
            }
        }
        if !item_errors {
            normalized.insert(key.to_string(), normalized_items);
        }
    }
    normalized
}

fn normalize_string_dict(
    value: Option<&Value>,
    field: &str,
    allowed_values: Option<&[&str]>,
    errors: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let Some(obj) = value.as_object() else {
        errors.push(format!("{field} must be an object"));
        return BTreeMap::new();
    };
    let mut normalized = BTreeMap::new();
    for (raw_key, raw_value) in obj {
        let key = raw_key.trim();
        if key.is_empty() {
            errors.push(format!("{field} has an invalid key"));
            continue;
        }
        let Some(text) = raw_value.as_str() else {
            errors.push(format!("{field}.{key} must be a string"));
            continue;
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            errors.push(format!("{field}.{key} must be non-empty"));
            continue;
        }
        if let Some(allowed_values) = allowed_values {
            if !allowed_values.contains(&text.as_str()) {
                errors.push(format!(
                    "{field}.{key} must be one of {}",
                    format_allowed_values(allowed_values)
                ));
                continue;
            }
        }
        normalized.insert(key.to_string(), text);
    }
    normalized
}

fn format_allowed_values(values: &[&str]) -> String {
    let rendered = values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{rendered}]")
}

#[cfg(test)]
mod tests {
    use super::{
        validate_substantiveness_result_data, validate_trellis_reviewer_result_data,
        validate_trellis_stuck_math_audit_result_data, validate_trellis_worker_result_data,
    };
    use serde_json::json;

    #[test]
    fn stuck_math_audit_validator_accepts_plan_shape() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "tasks": [
                {
                    "id": "task-1",
                    "title": "Check obstruction",
                    "body": "Read the cited scratch probe and decide whether the active statement needs a strengthened hypothesis."
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-1-request-2/probe.lean"
            ]
        }));

        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["confirm_need_input"], json!(false));
        assert_eq!(data["tasks"][0]["dismissed"], json!(false));
    }

    #[test]
    fn worker_validator_rejects_deviation_paths_outside_reference() {
        let result = validate_trellis_worker_result_data(&json!({
            "outcome": "valid",
            "summary": "bad deviation path",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
            "deviation_requests": {
                "dev:a": {
                    "path": "../outside.tex",
                    "summary": "departure",
                    "affected_nodes": ["N"]
                }
            },
            "node_deviation_claims": {},
            "needs_restructure_suggested_nodes": []
        }));

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err.contains("reference/")));
    }

    #[test]
    fn stuck_math_audit_validator_rejects_probe_outside_scratch() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "tasks": [],
            "probe_paths": ["Tablet/Main.lean"]
        }));

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains(".trellis/stuck-math-audit")));
    }

    #[test]
    fn reviewer_validator_rejects_stuck_decision() {
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "stuck",
            "reason": "bad decision",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "global",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
        }));

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err.contains(
            "decision must be one of ['continue', 'advance_phase', 'need_input', 'done']"
        )));
    }

    #[test]
    fn reviewer_validator_passes_through_next_active_coarse() {
        // Proposal v32 audit-2 followup #1 regression test. Pre-fix the
        // allowlist re-emit dropped `next_active_coarse` (added to
        // RawReviewPayload but missing from this validator's extract/emit
        // block), so reviewer anchor choices never reached the engine via
        // the live JSON path — the in-process Rust tests masked the bug
        // by constructing ReviewResponse directly. See
        // [[feedback_allowlist_validator]] in claude memory.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "switch coarse anchor",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "HelperB",
            "next_active_coarse": "B",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": false,
            "must_close_active": true,
            "clear_human_input": false,
        }));

        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(
            data["next_active_coarse"],
            json!("B"),
            "next_active_coarse must survive the allowlist re-emit"
        );

        // Round-trip: deserialize into RawReviewPayload and confirm the
        // field arrives non-empty (matching what the normalizer reads).
        let raw: crate::review_normalization::RawReviewPayload =
            serde_json::from_value(data).expect("round-trip into RawReviewPayload");
        assert_eq!(raw.next_active_coarse, "B");

        // Missing field path — defaults to "" which downstream normalizes
        // to None. This is the legitimate "preserve current anchor" signal.
        let omitted = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "keep current anchor",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": false,
            "must_close_active": false,
            "clear_human_input": false,
        }));
        assert!(omitted.ok, "errors={:?}", omitted.errors);
        let omitted_data = omitted.data.expect("normalized payload");
        assert_eq!(omitted_data["next_active_coarse"], json!(""));
    }

    #[test]
    fn reviewer_validator_treats_null_next_active_coarse_as_absent() {
        // Audit finding B (validator-half): JSON `null` for an optional
        // string field (`allow_empty=true`) was previously rejected with
        // "must be a string", forcing reviewers to either omit the key
        // or send "" — neither documented. The validator now treats
        // null the same as a missing field on the allow_empty path.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "preserve current anchor via null",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_active_coarse": null,
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": false,
            "must_close_active": false,
            "clear_human_input": false,
        }));

        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(
            data["next_active_coarse"],
            json!(""),
            "null next_active_coarse must normalize to empty string"
        );

        // Round-trip: deserialize into RawReviewPayload and confirm the
        // field arrives empty (the "preserve current anchor" signal).
        let raw: crate::review_normalization::RawReviewPayload =
            serde_json::from_value(data).expect("round-trip into RawReviewPayload");
        assert_eq!(raw.next_active_coarse, "");
    }

    #[test]
    fn reviewer_validator_still_rejects_null_for_required_string_field() {
        // Audit finding B regression guard: the null-as-absent
        // short-circuit must be gated on `allow_empty=true`. A required
        // string field (`decision`) receiving null must still produce
        // the "must be non-empty" error (not "must be a string", and
        // not silently accepted). Confirms the new branch does not
        // relax required-field validation.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": null,
            "reason": "null decision",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": false,
            "must_close_active": false,
            "clear_human_input": false,
        }));

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|err| err == "decision must be non-empty"),
            "expected 'decision must be non-empty' error for null required field, got: {:?}",
            result.errors
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err == "decision must be a string"),
            "null on required field should produce 'must be non-empty', not 'must be a string': {:?}",
            result.errors
        );
    }

    #[test]
    fn reviewer_validator_passes_through_request_sound_verifier_node_ids() {
        // New-soundness regression test. Pre-fix the allowlist re-emit
        // dropped `request_sound_verifier_node_ids` (added to
        // RawReviewPayload by commit 86d39e8 but missing from this
        // validator's extract/emit block), so reviewer Sound dispatch
        // requests silently became empty in the live JSON path and the
        // kernel routed to a Worker on the active node instead. The
        // general hazard: an allowlist re-emit validator that fails to
        // pass through a newly added payload field will silently drop it,
        // even though the Rust struct round-trips it correctly.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "run Sound verifier on the node",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "request_sound_verifier_node_ids": ["LocalDecoderLemma"],
            "next_active": "LocalDecoderLemma",
            "next_mode": "global",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
        }));
        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(
            data["request_sound_verifier_node_ids"],
            json!(["LocalDecoderLemma"]),
            "request_sound_verifier_node_ids must survive the allowlist re-emit"
        );

        // Round-trip: deserialize into RawReviewPayload and confirm the
        // normalizer will see the non-empty list.
        let raw: crate::review_normalization::RawReviewPayload =
            serde_json::from_value(data).expect("round-trip into RawReviewPayload");
        assert_eq!(
            raw.request_sound_verifier_node_ids,
            vec!["LocalDecoderLemma".to_string()]
        );

        // Legacy alias: agents that emit the older `request_sound_verifier_nodes`
        // name must also pass through under the canonical output key.
        let aliased = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "alias path",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "request_sound_verifier_nodes": ["AbsorberLemma"],
            "next_active": "AbsorberLemma",
            "next_mode": "global",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
        }));
        assert!(aliased.ok, "errors={:?}", aliased.errors);
        assert_eq!(
            aliased.data.expect("payload")["request_sound_verifier_node_ids"],
            json!(["AbsorberLemma"]),
            "alias `request_sound_verifier_nodes` must also map through"
        );

        // Missing field — defaults to empty list (the typical case).
        let omitted = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "no sound request",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": false,
            "must_close_active": false,
            "clear_human_input": false,
        }));
        assert!(omitted.ok, "errors={:?}", omitted.errors);
        assert_eq!(
            omitted.data.expect("payload")["request_sound_verifier_node_ids"],
            json!([])
        );
    }

    #[test]
    fn reviewer_validator_requires_explicit_proof_gate_fields() {
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "keep going",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "A",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "clear_human_input": false,
        }));

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err == "allow_new_obligations must be a boolean"));
        assert!(result
            .errors
            .iter()
            .any(|err| err == "must_close_active must be a boolean"));
    }

    #[test]
    fn reviewer_validator_normalizes_stuck_math_audit_product() {
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "diagnosed",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
            "stuck_math_audit": {
                "notes": "needs invariant H",
                "reviewer_lean_product": {
                    "kind": "sufficient_statement",
                    "statement": "H is enough"
                }
            }
        }));

        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(
            data["stuck_math_audit"]["reviewer_lean_product"]["kind"],
            json!("sufficient_statement")
        );
        assert_eq!(
            data["stuck_math_audit"]["notes"],
            json!("needs invariant H")
        );
    }

    #[test]
    fn reviewer_validator_rejects_oversized_stuck_math_product() {
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "diagnosed",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "local",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
            "stuck_math_audit": {
                "notes": "large product",
                "reviewer_lean_product": {
                    "kind": "oversized",
                    "payload": "x".repeat(crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS)
                }
            }
        }));

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("stuck_math_audit.reviewer_lean_product")));
    }

    #[test]
    fn substantiveness_admits_pass_with_optional_comment() {
        // Pass verdict + non-empty comment is OK (comment is optional on Pass).
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Pass"},
                    {"node": "BarThm", "verdict": "Pass", "comment": "looks good"},
                ],
            },
            "overall": "APPROVE",
            "summary": "two passes",
            "comments": "",
        }));
        assert!(result.ok, "errors={:?}", result.errors);
    }

    #[test]
    fn substantiveness_requires_comment_on_fail() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "FAIL",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Fail"},
                ],
            },
            "overall": "REJECT",
            "summary": "fail without comment",
            "comments": "",
        }));
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("comment must be non-empty when verdict is Fail")),
            "expected comment-required error; got {:?}",
            result.errors
        );

        let ok = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "FAIL",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Fail", "comment": "merge with Bar"},
                ],
            },
            "overall": "REJECT",
            "summary": "one fail",
            "comments": "",
        }));
        assert!(ok.ok, "errors={:?}", ok.errors);
    }

    #[test]
    fn substantiveness_admits_not_done_yet_with_optional_comment() {
        let no_comment = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "NotDoneYet"},
                ],
            },
            "overall": "APPROVE",
            "summary": "ran out of time on Foo",
            "comments": "",
        }));
        assert!(no_comment.ok, "errors={:?}", no_comment.errors);

        let with_comment = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "NotDoneYet", "comment": "ran out of time on case analysis"},
                ],
            },
            "overall": "APPROVE",
            "summary": "ran out of time on Foo",
            "comments": "",
        }));
        assert!(with_comment.ok, "errors={:?}", with_comment.errors);
    }

    #[test]
    fn substantiveness_rejects_pass_decision_with_fail_verdict() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Pass"},
                    {"node": "BarThm", "verdict": "Fail", "comment": "merge"},
                ],
            },
            "overall": "APPROVE",
            "summary": "inconsistent",
            "comments": "",
        }));
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("decision must be FAIL when any verdict is Fail")),
            "errors={:?}",
            result.errors
        );
    }

    #[test]
    fn substantiveness_rejects_fail_decision_when_no_fail_verdicts() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "FAIL",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Pass"},
                    {"node": "BarThm", "verdict": "NotDoneYet"},
                ],
            },
            "overall": "REJECT",
            "summary": "wrong",
            "comments": "",
        }));
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("decision must be PASS when no verdict is Fail")),
            "errors={:?}",
            result.errors
        );
    }

    #[test]
    fn substantiveness_rejects_duplicate_node_verdicts() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma", "verdict": "Pass"},
                    {"node": "FooLemma", "verdict": "NotDoneYet"},
                ],
            },
            "overall": "APPROVE",
            "summary": "duplicate",
            "comments": "",
        }));
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("duplicate verdict for node \"FooLemma\"")),
            "errors={:?}",
            result.errors
        );
    }

    #[test]
    fn substantiveness_rejects_not_done_yet_suffix_on_node() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "FooLemma[NotDoneYet]", "verdict": "NotDoneYet"},
                ],
            },
            "overall": "APPROVE",
            "summary": "wrong shape",
            "comments": "",
        }));
        assert!(!result.ok);
        assert!(
            result.errors.iter().any(|e| e.contains("[NotDoneYet]")),
            "errors={:?}",
            result.errors
        );
    }

    #[test]
    fn substantiveness_admits_empty_verdicts_list() {
        // Empty verdicts is allowed by the validator itself (kernel
        // normalizer flips missing nodes to NotDoneYet). Useful when the
        // request had an empty frontier.
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [],
            },
            "overall": "APPROVE",
            "summary": "empty",
            "comments": "",
        }));
        assert!(result.ok, "errors={:?}", result.errors);
    }

    /// Case R — Allowlist validator round-trip preserves Step A fields.
    #[test]
    fn validate_trellis_reviewer_result_preserves_global_repair_fields() {
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "needs widening",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "restructure",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "clear_human_input": false,
            "global_repair_request": {
                "proposed_extension_node_ids": ["X", "Y"],
                "reason": "out of cone",
            },
            "consume_global_repair_grant": false,
        }));
        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("data");
        assert_eq!(
            data["global_repair_request"]["proposed_extension_node_ids"],
            json!(["X", "Y"])
        );
        assert_eq!(data["global_repair_request"]["reason"], "out of cone");
        assert_eq!(data["consume_global_repair_grant"], false);
    }

    /// Case S — Allowlist validator preserves audit Step B fields.
    #[test]
    fn validate_trellis_stuck_math_audit_result_preserves_global_repair_fields() {
        let report_padding = "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS);
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "confirm_need_input": false,
            "report": format!("## Claim being audited\nwidening {report_padding}"),
            "tasks": [],
            "probe_paths": [],
            "global_repair_approve": true,
            "global_repair_approved_extension_node_ids": ["X"],
            "global_repair_auditor_reason": "in scope",
        }));
        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("data");
        assert_eq!(data["global_repair_approve"], true);
        assert_eq!(data["global_repair_approved_extension_node_ids"], json!(["X"]));
        assert_eq!(data["global_repair_auditor_reason"], "in scope");
    }
}
