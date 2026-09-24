use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path};

use crate::model::{
    AUDIT_PLAN_MAX_JSON_CHARS, AUDIT_REPORT_TEXT_MAX_CHARS, AUDIT_REPORT_TEXT_MIN_CHARS,
    AUDIT_TASK_BODY_MAX_CHARS, AUDIT_TASK_REASON_MAX_CHARS, AUDIT_TASK_TITLE_MAX_CHARS,
};

const WORKER_OUTCOMES: &[&str] = &[
    "valid",
    "invalid",
    "stuck",
    "needs_restructure",
    // PV under-model (Slice 1): the worker concluded a Decide target `T` is
    // false under the Aeneas model. PV/Decide-specific; the cleanup allowlist
    // (`["valid", "invalid"]`) excludes it.
    "target_false_under_model",
];
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
const SUBSTANTIVENESS_VERDICTS: &[&str] =
    &["Pass", "FalseAsStated", "Fail", "NotDoneYet"];
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
    let invalid_kind = expect_string(obj.get("invalid_kind"), "invalid_kind", true, &mut errors);
    const INVALID_KINDS: &[&str] = &[
        "implementation_invalid",
        "task_infeasible",
        "checker_rejected",
        "contract_conflict",
    ];
    if !invalid_kind.is_empty() && !INVALID_KINDS.contains(&invalid_kind.as_str()) {
        errors.push(format!(
            "invalid_kind must be one of [{}]",
            INVALID_KINDS
                .iter()
                .map(|kind| format!("'{kind}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if outcome != "invalid" && !invalid_kind.is_empty() {
        errors.push("invalid_kind is only legal when outcome=invalid".to_string());
    }
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
    // Allowlist entry for the challenge-target claim field: this
    // validator strips unknown fields, so omitting it here would drop
    // `challenge_claim_updates` from live JSON while Rust-only tests
    // still pass.
    let challenge_claim_updates = normalize_node_string_list_updates(
        obj.get("challenge_claim_updates"),
        "challenge_claim_updates",
        &mut errors,
    );
    let node_deviation_claims = normalize_node_string_list_updates(
        obj.get("node_deviation_claims"),
        "node_deviation_claims",
        &mut errors,
    );
    // Reference-paper claims: parse here AND re-emit into `success`
    // below (the `node_deviation_claims` twin) — this validator strips
    // unknown fields, so omitting either half silently drops the claims
    // from live JSON while Rust-only tests still pass (Amendment G3).
    // Re-emitted only when the worker supplied the key, so the validator
    // output for legacy payloads stays byte-identical.
    let node_reference_grounds_present =
        obj.get("node_reference_grounds").is_some_and(|v| !v.is_null());
    let node_reference_grounds = normalize_node_string_list_updates(
        obj.get("node_reference_grounds"),
        "node_reference_grounds",
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
                    let mut entry = json!({
                        "path": path,
                        "summary": summary,
                        "affected_nodes": affected,
                    });
                    // Stage 7 (plan doc 32, N3): the structured seam-repair
                    // declaration passes through the allowlisted rebuild
                    // with exact-shape validation (serde deny_unknown_fields
                    // over the closed enums — identity/schema checks only,
                    // A3).  The runtime checker strips unadvertised strays
                    // on non-trust requests BEFORE this validator runs, so
                    // presence here is the advertised trust surface.
                    match req.get("seam_repair").filter(|value| !value.is_null()) {
                        None => {}
                        Some(raw) => match serde_json::from_value::<
                            crate::model::SeamRepairDeclaration,
                        >(raw.clone())
                        {
                            Ok(declaration) => match serde_json::to_value(&declaration) {
                                Ok(value) => {
                                    entry["seam_repair"] = value;
                                }
                                Err(err) => errors.push(format!(
                                    "deviation_requests.{id}.seam_repair failed to \
                                     re-serialize: {err}"
                                )),
                            },
                            Err(err) => errors.push(format!(
                                "deviation_requests.{id}.seam_repair is malformed: {err}"
                            )),
                        },
                    }
                    deviation_requests.insert(id.clone(), entry);
                }
            }
            Value::Null => {}
            _ => errors.push("deviation_requests must be a JSON object".to_string()),
        }
    }
    let rust_witness_artifact = match obj
        .get("rust_witness_artifact")
        .filter(|value| !value.is_null())
    {
        None => None,
        Some(raw) => match serde_json::from_value::<
            crate::trust_base::RustWitnessArtifactDeclaration,
        >(raw.clone()) {
            Ok(declaration)
                if !declaration.target_id.as_str().trim().is_empty()
                    && !declaration.relative_path.trim().is_empty() =>
            {
                Some(declaration)
            }
            Ok(_) => {
                errors.push(
                    "rust_witness_artifact requires non-empty target_id and relative_path"
                        .to_string(),
                );
                None
            }
            Err(error) => {
                errors.push(format!("rust_witness_artifact is malformed: {error}"));
                None
            }
        },
    };
    let conditional_theorem_proposal = match obj
        .get("conditional_theorem_proposal")
        .filter(|value| !value.is_null())
    {
        None => None,
        Some(raw) => match serde_json::from_value::<crate::trust_base::ConditionalTheoremProposal>(raw.clone()) {
            Ok(proposal) => Some(proposal),
            Err(error) => {
                errors.push(format!("conditional_theorem_proposal is malformed: {error}"));
                None
            }
        },
    };
    let conditional_theorem_withdrawals: BTreeSet<crate::model::ChallengeTargetId> =
        match obj.get("conditional_theorem_withdrawals").filter(|value| !value.is_null()) {
            None => BTreeSet::new(),
            Some(raw) => match serde_json::from_value(raw.clone()) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("conditional_theorem_withdrawals is malformed: {error}"));
                    BTreeSet::new()
                }
            },
        };
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
                        None => errors.push(format!("deviation_deletions[{idx}] must be a string")),
                    }
                }
            }
            Value::Null => {}
            _ => {
                errors.push("deviation_deletions must be a JSON array of deviation ids".to_string())
            }
        }
    }
    deviation_deletions.sort();
    deviation_deletions.dedup();
    let mut deleted_nodes: Vec<String> = Vec::new();
    match obj.get("deleted_nodes") {
        Some(Value::Array(items)) => {
            for (idx, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(s) => {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            errors.push(format!("deleted_nodes[{idx}] must be a non-empty string"));
                        } else {
                            deleted_nodes.push(trimmed.to_string());
                        }
                    }
                    None => errors.push(format!("deleted_nodes[{idx}] must be a string")),
                }
            }
        }
        Some(_) | None => errors.push("deleted_nodes must be a JSON array of node ids".to_string()),
    }
    deleted_nodes.sort();
    deleted_nodes.dedup();
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
    // On-demand audit (advisory): re-emit the optional `audit_request`
    // sub-object so it survives the allowlist strip before
    // worker normalization (feedback_allowlist_validator).
    let audit_request = normalize_audit_request_field(obj.get("audit_request"), &mut errors);

    // Process memory challenges: re-emit through the allowlist
    // (feedback_allowlist_validator); emitted into the success payload
    // only when non-empty so non-challenging worker payloads stay
    // byte-identical.
    let memory_challenges = normalize_memory_challenges_field(obj.get("memory_challenges"), &mut errors);

    // PV under-model (Slice 1): the worker's NL disproof / route opinion /
    // reasoning. REQUIRED (non-empty) iff `outcome == target_false_under_model`;
    // must be absent/empty for every other outcome (so a stale disproof can't
    // ride an ordinary outcome to the auditor). These are re-emitted into the
    // success allowlist ONLY for that outcome — keeping the validator output
    // byte-identical for valid/invalid/stuck/needs_restructure (the math-mode
    // and baseline cases). Same allowlist-strip discipline as
    // `needs_restructure_suggested_nodes` (feedback_allowlist_validator).
    let is_under_model = outcome == "target_false_under_model";
    let under_model_disproof =
        expect_string(obj.get("under_model_disproof"), "under_model_disproof", true, &mut errors);
    let under_model_route_opinion = expect_string(
        obj.get("under_model_route_opinion"),
        "under_model_route_opinion",
        true,
        &mut errors,
    );
    let under_model_reasoning = expect_string(
        obj.get("under_model_reasoning"),
        "under_model_reasoning",
        true,
        &mut errors,
    );
    if is_under_model && under_model_disproof.trim().is_empty() {
        errors.push(
            "under_model_disproof must be a non-empty NL disproof (the falsifying witness and why \
             it breaks the spec) when outcome=target_false_under_model"
                .to_string(),
        );
    }
    if is_under_model && under_model_route_opinion.trim().is_empty() {
        errors.push(
            "under_model_route_opinion must be non-empty (flip vs model-deviation) when \
             outcome=target_false_under_model"
                .to_string(),
        );
    }
    if !is_under_model
        && (!under_model_disproof.trim().is_empty()
            || !under_model_route_opinion.trim().is_empty()
            || !under_model_reasoning.trim().is_empty())
    {
        errors.push(
            "under_model_disproof / under_model_route_opinion / under_model_reasoning must be \
             empty when outcome is not target_false_under_model"
                .to_string(),
        );
    }

    // PV under-model (Slice 2): assumption-AUTHORING metadata. The worker
    // writes Lean/NL into Tablet/Assumptions.{lean,tex}; JSON carries only the
    // block id, axiom name, citation locator, and Rust justification.
    // Re-emitted into the success allowlist only when non-empty, so every
    // non-authoring worker payload — math mode included — stays byte-identical
    // (feedback_allowlist_validator).
    let authored_assumption_id = expect_string(
        obj.get("authored_assumption_id"),
        "authored_assumption_id",
        true,
        &mut errors,
    );
    let authored_axiom_name = expect_string(
        obj.get("authored_axiom_name"),
        "authored_axiom_name",
        true,
        &mut errors,
    );
    let authored_citation_locator = expect_string(
        obj.get("authored_citation_locator"),
        "authored_citation_locator",
        true,
        &mut errors,
    );
    let authored_rust_justification = expect_string(
        obj.get("authored_rust_justification"),
        "authored_rust_justification",
        true,
        &mut errors,
    );
    let authored_claim_class = expect_string(
        obj.get("authored_claim_class"),
        "authored_claim_class",
        true,
        &mut errors,
    );
    if !authored_claim_class.trim().is_empty()
        && authored_claim_class.trim() != "behavior"
        && authored_claim_class.trim() != "domain"
    {
        errors.push(
            "authored_claim_class must be \"\", \"behavior\", or \"domain\"".to_string(),
        );
    }
    let any_authored = !authored_assumption_id.trim().is_empty()
        || !authored_axiom_name.trim().is_empty()
        || !authored_citation_locator.trim().is_empty()
        || !authored_rust_justification.trim().is_empty()
        || !authored_claim_class.trim().is_empty();
    if any_authored
        && (authored_assumption_id.trim().is_empty()
            || authored_axiom_name.trim().is_empty()
            || authored_citation_locator.trim().is_empty()
            || authored_rust_justification.trim().is_empty())
    {
        errors.push(
            "an authored assumption requires authored_assumption_id, authored_axiom_name, \
             authored_citation_locator, and authored_rust_justification; Lean/NL statements \
             must be written to Tablet/Assumptions.{lean,tex}, not JSON"
                .to_string(),
        );
    }

    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }

    let mut success = serde_json::Map::new();
    success.insert("summary".to_owned(), json!(summary));
    success.insert("outcome".to_owned(), json!(outcome));
    success.insert("comments".to_owned(), json!(comments));
    if !invalid_kind.is_empty() {
        success.insert("invalid_kind".to_owned(), json!(invalid_kind));
    }
    success.insert("semantic_dep_updates".to_owned(), json!(semantic_dep_updates));
    success.insert("target_claim_updates".to_owned(), json!(target_claim_updates));
    success.insert("challenge_claim_updates".to_owned(), json!(challenge_claim_updates));
    success.insert("deviation_requests".to_owned(), json!(deviation_requests));
    if let Some(declaration) = rust_witness_artifact {
        success.insert("rust_witness_artifact".to_owned(), json!(declaration));
    }
    if let Some(proposal) = conditional_theorem_proposal {
        success.insert("conditional_theorem_proposal".to_owned(), json!(proposal));
    }
    if !conditional_theorem_withdrawals.is_empty() {
        success.insert("conditional_theorem_withdrawals".to_owned(), json!(conditional_theorem_withdrawals));
    }
    success.insert("node_deviation_claims".to_owned(), json!(node_deviation_claims));
    if node_reference_grounds_present {
        success.insert(
            "node_reference_grounds".to_owned(),
            json!(node_reference_grounds),
        );
    }
    success.insert("deviation_deletions".to_owned(), json!(deviation_deletions));
    success.insert("deleted_nodes".to_owned(), json!(deleted_nodes));
    success.insert("difficulty_updates".to_owned(), json!(difficulty_updates));
    success.insert("needs_restructure_suggested_nodes".to_owned(), json!(suggested_nodes));
    // Emit the under-model carriers ONLY for the under-model outcome — every
    // other outcome's validator output stays byte-identical to the pre-Slice-1
    // shape (so the contract-baseline / runtime-cli fixtures are unaffected).
    if is_under_model {
        success.insert(
            "under_model_disproof".to_owned(),
            json!(under_model_disproof.trim()),
        );
        success.insert(
            "under_model_route_opinion".to_owned(),
            json!(under_model_route_opinion.trim()),
        );
        success.insert(
            "under_model_reasoning".to_owned(),
            json!(under_model_reasoning.trim()),
        );
    }
    // Re-emit the authored-assumption carriers only when the worker authored
    // one (any field non-empty) — baseline-stable otherwise.
    if any_authored {
        success.insert(
            "authored_assumption_id".to_owned(),
            json!(authored_assumption_id.trim()),
        );
        success.insert(
            "authored_axiom_name".to_owned(),
            json!(authored_axiom_name.trim()),
        );
        success.insert(
            "authored_citation_locator".to_owned(),
            json!(authored_citation_locator.trim()),
        );
        success.insert(
            "authored_rust_justification".to_owned(),
            json!(authored_rust_justification.trim()),
        );
        success.insert(
            "authored_claim_class".to_owned(),
            json!(authored_claim_class.trim()),
        );
    }
    success.insert("audit_request".to_owned(), audit_request);
    if !memory_challenges.is_empty() {
        success.insert("memory_challenges".to_owned(), json!(memory_challenges));
    }
    ArtifactValidationOutput::success(Value::Object(success))
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
    // Correspondence-repair dispatch. This literal rebuild is an
    // allowlist: a field not named in the `json!` below is silently
    // dropped, so it passes every Rust test and then arrives empty in the
    // live run. Name it.
    let cleanup_repair_node = match obj.get("cleanup_repair_node") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Value::Null
            } else {
                json!(trimmed)
            }
        }
        Some(_) => {
            errors.push("cleanup_repair_node must be a string node id or null".to_string());
            Value::Null
        }
    };
    // Cleanup-v2 batch dispatch: optional list of Pending LintFix task
    // indices to dispatch to a single worker burst this cycle. This
    // allowlist validator only faithfully carries the field through;
    // batch legality (kind/pending/protected/cap/mutual-exclusion) is
    // enforced downstream by `review_response_legal`.
    let cleanup_batch_tasks = normalize_optional_u32_list(
        obj.get("cleanup_batch_tasks"),
        "cleanup_batch_tasks",
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
    // On-demand audit: optional sub-object pass-through. Without listing
    // here the allowlist re-emit silently strips the field before
    // RawReviewPayload normalizes it (feedback_allowlist_validator).
    let audit_request = normalize_audit_request_field(obj.get("audit_request"), &mut errors);
    // PV under-model (approach-audit route): optional enact flag
    // pass-through — same allowlist-strip hazard as `audit_request`.
    let assumption_authoring_request = expect_bool(
        obj.get("assumption_authoring_request"),
        "assumption_authoring_request",
        &mut errors,
    );
    // Audit-ordered node retirement: dispatch flag + decline reason
    // pass-through — without explicit listing the allowlist re-emit
    // silently strips them (feedback_allowlist_validator). Emitted into
    // the success payload only when set, keeping every other reviewer
    // payload byte-identical.
    let dispatch_node_retirement = expect_bool(
        obj.get("dispatch_node_retirement"),
        "dispatch_node_retirement",
        &mut errors,
    );
    let node_retirement_decline_reason = expect_string(
        obj.get("node_retirement_decline_reason"),
        "node_retirement_decline_reason",
        true,
        &mut errors,
    );
    if dispatch_node_retirement && !node_retirement_decline_reason.trim().is_empty() {
        errors.push(
            "dispatch_node_retirement and node_retirement_decline_reason are mutually exclusive"
                .to_string(),
        );
    }
    // Process memory: challenge list + LastClean carry-forward flag
    // pass-through (feedback_allowlist_validator). Both are emitted into
    // the success payload only when supplied, keeping non-memory reviewer
    // payloads byte-identical.
    let memory_challenges = normalize_memory_challenges_field(obj.get("memory_challenges"), &mut errors);
    // Sidecar grunt queue: string-list pass-throughs — without explicit
    // listing here the allowlist re-emit silently strips both fields
    // before RawReviewPayload normalizes them
    // ([[feedback_allowlist_validator]]). Emitted into the success
    // payload only when non-empty, keeping queue-free reviewer payloads
    // byte-identical.
    // Duplicates are deliberately PRESERVED (unlike `expect_string_list`):
    // the normalizer keeps them so downstream legality
    // (`sidecar_queue_response_violations`) can reject a duplicated add
    // with its NAMED duplicate reason instead of the validator silently
    // repairing the payload (deviation 1).
    let sidecar_queue_add = expect_string_list_keep_duplicates(
        obj.get("sidecar_queue_add"),
        "sidecar_queue_add",
        &mut errors,
    );
    let sidecar_queue_remove = expect_string_list_keep_duplicates(
        obj.get("sidecar_queue_remove"),
        "sidecar_queue_remove",
        &mut errors,
    );
    let preserve_process_memory = match obj.get("preserve_process_memory") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_bool() {
            Some(flag) => Some(flag),
            None => {
                errors.push("preserve_process_memory must be a boolean".to_string());
                None
            }
        },
    };

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

    let mut success = json!({
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
        "cleanup_repair_node": cleanup_repair_node,
        "cleanup_request_reaudit": cleanup_request_reaudit,
        "dismiss_audit_plan": dismiss_audit_plan,
        "dismissed_tasks": dismissed_tasks,
        "global_repair_request": global_repair_request,
        "consume_global_repair_grant": consume_global_repair_grant,
        "audit_request": audit_request,
        "assumption_authoring_request": assumption_authoring_request,
    });
    let success_map = success.as_object_mut().expect("success is an object");
    // Cleanup-v2 batch dispatch: mirror `skip_serializing_if =
    // "Option::is_none"` on `RawReviewPayload::cleanup_batch_tasks` so that
    // absent stays absent (non-batch decisions' validated output is
    // byte-unchanged) and a present list survives the allowlist re-emit.
    if let Some(batch) = cleanup_batch_tasks.as_ref() {
        success_map.insert("cleanup_batch_tasks".to_owned(), json!(batch));
    }
    if !memory_challenges.is_empty() {
        success_map.insert("memory_challenges".to_owned(), json!(memory_challenges));
    }
    if dispatch_node_retirement {
        success_map.insert("dispatch_node_retirement".to_owned(), json!(true));
    }
    if !node_retirement_decline_reason.trim().is_empty() {
        success_map.insert(
            "node_retirement_decline_reason".to_owned(),
            json!(node_retirement_decline_reason.trim()),
        );
    }
    if let Some(flag) = preserve_process_memory {
        success_map.insert("preserve_process_memory".to_owned(), json!(flag));
    }
    if !sidecar_queue_add.is_empty() {
        success_map.insert("sidecar_queue_add".to_owned(), json!(sidecar_queue_add));
    }
    if !sidecar_queue_remove.is_empty() {
        success_map.insert(
            "sidecar_queue_remove".to_owned(),
            json!(sidecar_queue_remove),
        );
    }
    ArtifactValidationOutput::success(success)
}

/// On-demand audit: re-emit the `audit_request` sub-object through the
/// reviewer/worker allowlist validators so it survives to normalization
/// (feedback_allowlist_validator). Shape: `{reason_kind, reason}`. Absent
/// or null → JSON null (no request).
fn normalize_audit_request_field(value: Option<&Value>, errors: &mut Vec<String>) -> Value {
    match value {
        None => Value::Null,
        Some(v) if v.is_null() => Value::Null,
        Some(v) => match v.as_object() {
            None => {
                errors.push("audit_request must be an object".to_string());
                Value::Null
            }
            Some(ar_obj) => {
                let reason_kind = expect_string(
                    ar_obj.get("reason_kind"),
                    "audit_request.reason_kind",
                    false,
                    errors,
                );
                let reason = expect_string(
                    ar_obj.get("reason"),
                    "audit_request.reason",
                    false,
                    errors,
                );
                json!({
                    "reason_kind": reason_kind,
                    "reason": reason,
                })
            }
        },
    }
}

/// Process memory: re-emit the `memory_challenges` list through the
/// worker/reviewer allowlist validators so it survives to normalization
/// (feedback_allowlist_validator). Shape: array of `{entry_id, reason}`
/// objects with non-empty string fields. Absent/null/empty → empty vec
/// (and the caller keeps it OFF the success payload, so every
/// non-challenging response stays byte-identical to the pre-feature
/// shape).
fn normalize_memory_challenges_field(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    if field.is_null() {
        return Vec::new();
    }
    let Some(arr) = field.as_array() else {
        errors.push("memory_challenges must be an array of {entry_id, reason} objects".to_string());
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            errors.push(format!("memory_challenges[{i}] must be an object"));
            continue;
        };
        let entry_id = expect_string(
            obj.get("entry_id"),
            &format!("memory_challenges[{i}].entry_id"),
            false,
            errors,
        );
        let reason = expect_string(
            obj.get("reason"),
            &format!("memory_challenges[{i}].reason"),
            false,
            errors,
        );
        out.push(json!({"entry_id": entry_id, "reason": reason}));
    }
    out
}

/// Process memory: validate + re-emit the stuck-math-audit
/// `memory_operations` list (feedback_allowlist_validator). Shape checks
/// only — this validator has no request context or repo access, so the
/// coarse-node membership and entry-existence/status checks live in the
/// runtime CLI checker (`check_trellis_stuck_math_audit_result_output`)
/// and the engine (`stuck_math_audit_validation_failure`).
fn normalize_memory_operations_field(value: Option<&Value>, errors: &mut Vec<String>) -> Vec<Value> {
    let Some(field) = value else {
        return Vec::new();
    };
    if field.is_null() {
        return Vec::new();
    }
    let ops: Vec<crate::process_memory::MemoryOperation> =
        match serde_json::from_value(field.clone()) {
            Ok(ops) => ops,
            Err(err) => {
                errors.push(format!(
                    "memory_operations must be an array of operation objects: {err}"
                ));
                return Vec::new();
            }
        };
    errors.extend(crate::process_memory::validate_memory_operations_shape(
        &ops, None,
    ));
    ops.iter()
        .map(|op| serde_json::to_value(op).expect("memory operation serializes"))
        .collect()
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

/// Cleanup-v2 batch dispatch: parse `Option<Vec<u32>>` from a JSON value.
/// Accepts a missing field or JSON null (→ None), or a JSON array of
/// non-negative integers (→ Some(Vec<u32>)). A non-array value, or an
/// array containing a non-integer or negative entry, produces an error.
/// This validator faithfully carries the field through the allowlist
/// re-emit; it does NOT re-validate batch legality (kind/pending/
/// protected/cap/mutual-exclusion) — that is `review_response_legal`'s
/// job downstream.
fn normalize_optional_u32_list(
    value: Option<&Value>,
    field_name: &str,
    errors: &mut Vec<String>,
) -> Option<Vec<u32>> {
    match value {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_array() {
            None => {
                errors.push(format!(
                    "{field_name} must be null or an array of non-negative integers"
                ));
                None
            }
            Some(arr) => {
                let mut out: Vec<u32> = Vec::with_capacity(arr.len());
                for (i, entry) in arr.iter().enumerate() {
                    match entry.as_u64() {
                        Some(n) => out.push(n as u32),
                        None => {
                            errors.push(format!(
                                "{field_name}[{i}] must be a non-negative integer"
                            ));
                        }
                    }
                }
                Some(out)
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
    let mut output = validate_single_decision_result_data(
        data,
        "correspondence",
        validate_corr_node_block,
        "phase decisions",
    );
    if !output.ok {
        return output;
    }
    if let Some(raw) = data.get("rust_witness_artifact") {
        let verdict: crate::trust_base::RustWitnessCorrespondenceLaneVerdict =
            match serde_json::from_value::<
                crate::trust_base::RustWitnessCorrespondenceLaneVerdict,
            >(raw.clone()) {
                Ok(verdict) if !verdict.reason.trim().is_empty() => verdict,
                Ok(_) => {
                    return ArtifactValidationOutput::failure(vec![
                        "rust_witness_artifact.reason must be non-empty".into(),
                    ])
                }
                Err(error) => {
                    return ArtifactValidationOutput::failure(vec![format!(
                        "rust_witness_artifact has an invalid closed shape: {error}"
                    )])
                }
            };
        if let Some(object) = output.data.as_mut().and_then(Value::as_object_mut) {
            object.insert(
                "rust_witness_artifact".into(),
                serde_json::to_value(verdict).expect("validated verdict serializes"),
            );
        }
    }
    if let Some(raw) = data.get("conditional_theorem") {
        let verdict: crate::trust_base::ConditionalCorrespondenceVerdict =
            match serde_json::from_value::<
                crate::trust_base::ConditionalCorrespondenceVerdict,
            >(raw.clone()) {
                Ok(verdict) if !verdict.findings.trim().is_empty() => verdict,
                Ok(_) => return ArtifactValidationOutput::failure(vec![
                    "conditional_theorem.findings must be non-empty".into(),
                ]),
                Err(error) => return ArtifactValidationOutput::failure(vec![format!(
                    "conditional_theorem has an invalid closed shape: {error}"
                )]),
            };
        if let Some(object) = output.data.as_mut().and_then(Value::as_object_mut) {
            object.insert("conditional_theorem".into(), json!(verdict));
        }
    }
    output
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

/// GapResearch Critic: validate + normalize the `gap_decision`. Returns the
/// lowercased decision string (or empty when absent). Guardrails:
/// `decision ∈ {accept, reject}`; non-empty `gap_feedback` on reject.
fn validate_gap_decision(
    decision_value: Option<&Value>,
    feedback: &str,
    errors: &mut Vec<String>,
) -> String {
    let decision = expect_string(decision_value, "gap_decision", true, errors).to_ascii_lowercase();
    if !decision.is_empty() && decision != "accept" && decision != "reject" {
        errors.push("gap_decision must be one of ['accept', 'reject']".to_string());
    }
    if decision == "reject" && feedback.trim().is_empty() {
        errors.push("gap_feedback must be non-empty when gap_decision is 'reject'".to_string());
    }
    decision
}

/// Trust protocol v1 refusal for a named under-model candidate. The
/// worker/auditor-staged assumption lane is deliberately closed in trust mode
/// (Required-v1 never mints an axiom from an auditor's diagnosis), so an
/// artifact naming `C` is bounced onto the legal trust-mode menu at the
/// earliest honest point instead of stranding a later reviewer whose contract
/// cannot advertise the enact field.
pub const TRUST_V1_UNDER_MODEL_CANDIDATE_REFUSAL: &str =
    "trust protocol v2: under_model_candidate_invariant must be empty — the \
     assumption-authoring lane is closed. Route the finding through the retained \
     assumption-floor workflow or attempt the Lean refutation";

pub fn validate_trellis_stuck_math_audit_result_data(data: &Value) -> ArtifactValidationOutput {
    validate_trellis_stuck_math_audit_result_data_with_trust(data, false)
}

/// Trust-aware variant: `trust_base_required_v1` is peeked mechanically from
/// the dispatching request (see `check_trellis_stuck_math_audit_result_output`)
/// because the pure validator has no protocol state. In trust mode the
/// under-model candidate field must be empty on every ruling; outside trust the
/// legacy cross-field rules (deviation names `C`, bug/reject forbid it) apply.
pub fn validate_trellis_stuck_math_audit_result_data_with_trust(
    data: &Value,
    trust_base_required_v1: bool,
) -> ArtifactValidationOutput {
    let Some(obj) = data.as_object() else {
        return ArtifactValidationOutput::failure(vec!["result must be a JSON object".to_string()]);
    };
    let mut errors = Vec::new();
    let confirm_need_input = expect_bool(
        obj.get("confirm_need_input"),
        "confirm_need_input",
        &mut errors,
    );
    let report = expect_string(obj.get("report"), "report", true, &mut errors);
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
    // GapResearch Planner / Critic flat payloads. Added to the allowlist
    // explicitly (feedback_allowlist_validator) so the re-emit below does
    // not silently strip them. The Planner emits `route_tex` /
    // `route_needs_human`; the Critic emits `gap_decision` / `gap_feedback`
    // (+ `report` / `tasks` on ACCEPT, validated above).
    let route_tex = expect_string(obj.get("route_tex"), "route_tex", true, &mut errors);
    let route_needs_human = expect_bool(
        obj.get("route_needs_human"),
        "route_needs_human",
        &mut errors,
    );
    let gap_feedback = expect_string(obj.get("gap_feedback"), "gap_feedback", true, &mut errors);
    let gap_decision = validate_gap_decision(obj.get("gap_decision"), &gap_feedback, &mut errors);
    // Revision Planner structured action list (`revision_plan.md` §9). Shape +
    // action-set check here; the name-existence check (targets/nodes must be
    // carried in the revision-planning context) lives in the engine validator
    // where the request context is available. Added to the allowlist explicitly
    // (feedback_allowlist_validator) so the re-emit below does not strip it.
    let revision_actions =
        validate_revision_actions(obj.get("revision_actions"), &mut errors);
    // PV "prove OR disprove": the auditor's polarity decision for the active
    // `Decide` pair — "" (no decision), "prove", or "disprove". The engine
    // (`apply_stuck_math_audit_response`) is the SOLE flipper of
    // `pv_live_polarity` and validates legality (active target is a configured
    // Decide pair) + idempotence + the anti-thrash bound. Added to the
    // allowlist explicitly (feedback_allowlist_validator) so the re-emit below
    // does not silently strip it.
    let set_live_polarity = expect_string(
        obj.get("set_live_polarity"),
        "set_live_polarity",
        true,
        &mut errors,
    );
    if !set_live_polarity.is_empty()
        && set_live_polarity != "prove"
        && set_live_polarity != "disprove"
    {
        errors.push(
            "set_live_polarity must be \"\", \"prove\", or \"disprove\"".to_string(),
        );
    }
    // PV "prove OR disprove": the optional pair NAME for the flip, used when
    // the pair cannot bind from the active/held seat. Cross-field: it directs
    // `set_live_polarity` and is meaningless without one. Resolution against
    // the configured pairs is state-dependent and lives in the engine
    // (`resolve_decide_primary_target`).
    let set_live_polarity_target = expect_string(
        obj.get("set_live_polarity_target"),
        "set_live_polarity_target",
        true,
        &mut errors,
    );
    if !set_live_polarity_target.trim().is_empty() && set_live_polarity.is_empty() {
        errors.push(
            "set_live_polarity_target is only meaningful with a non-empty set_live_polarity"
                .to_string(),
        );
    }
    let conditional_theorem_proposal = match obj
        .get("conditional_theorem_proposal")
        .filter(|value| !value.is_null())
    {
        None => None,
        Some(raw) => match serde_json::from_value::<crate::trust_base::ConditionalTheoremProposal>(raw.clone()) {
            Ok(proposal) => Some(proposal),
            Err(error) => {
                errors.push(format!("conditional_theorem_proposal is malformed: {error}"));
                None
            }
        },
    };
    let conditional_theorem_withdrawals: BTreeSet<crate::model::ChallengeTargetId> =
        match obj.get("conditional_theorem_withdrawals").filter(|value| !value.is_null()) {
            None => BTreeSet::new(),
            Some(raw) => match serde_json::from_value(raw.clone()) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("conditional_theorem_withdrawals is malformed: {error}"));
                    BTreeSet::new()
                }
            },
        };
    // PV under-model (Slice 1): the auditor's bug/deviation ruling on a
    // `TargetFalseUnderModel` claim + the named candidate invariant `C`. Both
    // default to "" for every non-under-model audit (so the validator output
    // stays byte-identical for math-mode / baseline audits — they are
    // re-emitted into `plan_view` only when non-empty below).
    let under_model_ruling = expect_string(
        obj.get("under_model_ruling"),
        "under_model_ruling",
        true,
        &mut errors,
    );
    if !under_model_ruling.is_empty()
        && under_model_ruling != "bug"
        && under_model_ruling != "deviation"
        && under_model_ruling != "reject"
    {
        errors.push(
            "under_model_ruling must be \"\", \"bug\", \"deviation\", or \"reject\"".to_string(),
        );
    }
    let under_model_candidate_invariant = expect_string(
        obj.get("under_model_candidate_invariant"),
        "under_model_candidate_invariant",
        true,
        &mut errors,
    );
    if trust_base_required_v1 {
        // Trust protocol v1: the candidate field carries no authority on ANY
        // ruling — a `deviation` ruling names the violated guarantee in
        // reasoning prose, and the engine treats a candidate as a hint at
        // most. Naming one is refused with the legal trust-mode menu.
        if !under_model_candidate_invariant.trim().is_empty() {
            errors.push(TRUST_V1_UNDER_MODEL_CANDIDATE_REFUSAL.to_string());
        }
    } else {
        if under_model_ruling == "deviation" && under_model_candidate_invariant.trim().is_empty() {
            errors.push(
                "under_model_candidate_invariant must name the candidate Rust language invariant C \
                 when under_model_ruling=deviation"
                    .to_string(),
            );
        }
        // PV under-model (approach-audit route): a PLAN-writing audit (empty
        // ruling) MAY name a candidate `C` to recommend opening the
        // assumption-authoring lane — the kernel records it for the reviewer to
        // enact (`assumption_authoring_request`). A `bug` / `reject` ruling
        // explicitly rules the assumption route out, so a candidate there is
        // contradictory and still rejected.
        if matches!(under_model_ruling.as_str(), "bug" | "reject")
            && !under_model_candidate_invariant.trim().is_empty()
        {
            errors.push(
                "under_model_candidate_invariant must be empty on a bug/reject ruling; name a candidate only with under_model_ruling=deviation or (for a plan-writing audit recommending the assumption-authoring lane) with an empty ruling"
                    .to_string(),
            );
        }
    }
    // PV under-model (Slice 2): the assumptions-lane verdict carriers. Default
    // "" for every non-assumptions-lane audit (re-emitted into `plan_view` only
    // when non-empty, so math-mode / baseline audits stay byte-identical).
    let assumptions_lane_verdict = expect_string(
        obj.get("assumptions_lane_verdict"),
        "assumptions_lane_verdict",
        true,
        &mut errors,
    );
    if !assumptions_lane_verdict.is_empty()
        && assumptions_lane_verdict != "pass"
        && assumptions_lane_verdict != "reject"
    {
        errors.push(
            "assumptions_lane_verdict must be \"\", \"pass\", or \"reject\"".to_string(),
        );
    }
    let assumptions_lane_reason = expect_string(
        obj.get("assumptions_lane_reason"),
        "assumptions_lane_reason",
        true,
        &mut errors,
    );
    if assumptions_lane_verdict == "reject" && assumptions_lane_reason.trim().is_empty() {
        errors.push(
            "assumptions_lane_reason must be non-empty (the eligibility / citation / hunt \
             finding) when assumptions_lane_verdict=reject"
                .to_string(),
        );
    }
    let assumptions_lane_hunt_result = expect_string(
        obj.get("assumptions_lane_hunt_result"),
        "assumptions_lane_hunt_result",
        true,
        &mut errors,
    );
    let assumptions_lane_probe_result = expect_string(
        obj.get("assumptions_lane_probe_result"),
        "assumptions_lane_probe_result",
        true,
        &mut errors,
    );
    let has_assumptions_lane_verdict = !assumptions_lane_verdict.is_empty();
    if !has_assumptions_lane_verdict && report.is_empty() {
        errors.push("report must be non-empty".to_string());
    }
    // A Planner / Critic burst has no obligation to file an audit-style
    // `report` with a probe/code/heading "concrete signal"; its concrete
    // signal IS the route / decision / structured action list. Likewise, the
    // PV assumptions lane is a first-class verdict artifact: a non-empty
    // assumptions_lane_verdict is the concrete signal. Only the
    // diagnosis-/recovery-style responses (no such payload) must carry a
    // legacy concrete signal.
    let has_revision_actions = revision_actions
        .get("targets")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
        || revision_actions
            .get("nodes")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
    let has_gap_payload = !route_tex.is_empty()
        || route_needs_human
        || !gap_decision.is_empty()
        || has_revision_actions
        || has_assumptions_lane_verdict;
    if !has_gap_payload
        && probe_paths.is_empty()
        && !report.contains("```")
        && !report.contains("## Claim being audited")
    {
        errors.push(
            "report must include a concrete signal: probe_paths, a fenced code block, or a '## Claim being audited' heading"
                .to_string(),
        );
    }
    let mut plan_view_map = serde_json::Map::new();
    plan_view_map.insert("confirm_need_input".to_owned(), json!(confirm_need_input));
    plan_view_map.insert("report".to_owned(), json!(report));
    plan_view_map.insert("tasks".to_owned(), json!(tasks));
    plan_view_map.insert("probe_paths".to_owned(), json!(probe_paths));
    plan_view_map.insert("cone_clean_node".to_owned(), json!(cone_clean_node));
    plan_view_map.insert("global_repair_approve".to_owned(), json!(global_repair_approve));
    plan_view_map.insert(
        "global_repair_approved_extension_node_ids".to_owned(),
        json!(global_repair_approved_extension_node_ids),
    );
    plan_view_map.insert(
        "global_repair_auditor_reason".to_owned(),
        json!(global_repair_auditor_reason),
    );
    plan_view_map.insert("route_tex".to_owned(), json!(route_tex));
    plan_view_map.insert("route_needs_human".to_owned(), json!(route_needs_human));
    plan_view_map.insert("gap_decision".to_owned(), json!(gap_decision));
    plan_view_map.insert("gap_feedback".to_owned(), json!(gap_feedback));
    plan_view_map.insert("revision_actions".to_owned(), json!(revision_actions));
    plan_view_map.insert("set_live_polarity".to_owned(), json!(set_live_polarity));
    // Re-emit the flip's pair name ONLY when the auditor actually named one —
    // every target-less audit's validator output is byte-identical to the
    // prior shape (baseline-stable), mirroring under_model_candidate_invariant.
    if !set_live_polarity_target.trim().is_empty() {
        plan_view_map.insert(
            "set_live_polarity_target".to_owned(),
            json!(set_live_polarity_target.trim()),
        );
    }
    if let Some(proposal) = conditional_theorem_proposal {
        plan_view_map.insert("conditional_theorem_proposal".to_owned(), json!(proposal));
    }
    if !conditional_theorem_withdrawals.is_empty() {
        plan_view_map.insert(
            "conditional_theorem_withdrawals".to_owned(),
            json!(conditional_theorem_withdrawals),
        );
    }
    // Re-emit the under-model ruling fields ONLY when the auditor actually
    // ruled (non-empty) — every non-under-model audit's validator output is
    // byte-identical to the pre-Slice-1 shape (baseline-stable).
    if !under_model_ruling.is_empty() {
        plan_view_map.insert("under_model_ruling".to_owned(), json!(under_model_ruling));
        plan_view_map.insert(
            "under_model_candidate_invariant".to_owned(),
            json!(under_model_candidate_invariant.trim()),
        );
    } else if !under_model_candidate_invariant.trim().is_empty() {
        // PV under-model (approach-audit route): a PLAN-writing audit (empty
        // ruling) names its authoring candidate in this same field. Without
        // this re-emit the round-trip SILENTLY STRIPS it — the kernel then
        // records an empty candidate, the reviewer has nothing to enact, and
        // the recommend-without-candidate bounce fires against an auditor
        // that actually complied (verified on the dec2flt example: audits 610
        // and 611 both named candidates in their raw results; the kernel
        // saw ""). Gated on non-empty, so every candidate-less audit output
        // stays byte-identical.
        plan_view_map.insert(
            "under_model_candidate_invariant".to_owned(),
            json!(under_model_candidate_invariant.trim()),
        );
    }
    // PV under-model (Slice 2): re-emit the assumptions-lane carriers ONLY when
    // the lane actually ruled (non-empty verdict) — baseline-stable otherwise.
    if has_assumptions_lane_verdict {
        plan_view_map.insert(
            "assumptions_lane_verdict".to_owned(),
            json!(assumptions_lane_verdict),
        );
        plan_view_map.insert(
            "assumptions_lane_reason".to_owned(),
            json!(assumptions_lane_reason.trim()),
        );
        plan_view_map.insert(
            "assumptions_lane_hunt_result".to_owned(),
            json!(assumptions_lane_hunt_result.trim()),
        );
        plan_view_map.insert(
            "assumptions_lane_probe_result".to_owned(),
            json!(assumptions_lane_probe_result.trim()),
        );
    }
    let plan_view = Value::Object(plan_view_map);
    if let Ok(text) = serde_json::to_string(&plan_view) {
        if text.chars().count() > AUDIT_PLAN_MAX_JSON_CHARS {
            errors.push(format!(
                "stuck math audit plan must serialize to at most {AUDIT_PLAN_MAX_JSON_CHARS} JSON characters"
            ));
        }
    }
    // Process memory: shape-validate + re-emit `memory_operations`
    // (feedback_allowlist_validator). Inserted AFTER the plan-size guard
    // — operations materialize to `process-memory/` files, not into the
    // stored audit plan, so they don't count against the plan budget
    // (bodies have their own per-entry cap). Emitted only when
    // non-empty, so non-memory audit payloads stay byte-identical.
    let memory_operations =
        normalize_memory_operations_field(obj.get("memory_operations"), &mut errors);
    let mut plan_view = plan_view;
    if !memory_operations.is_empty() {
        plan_view
            .as_object_mut()
            .expect("plan_view is an object")
            .insert("memory_operations".to_owned(), json!(memory_operations));
    }
    // Audit-ordered node retirement: shape-validate + re-emit the
    // optional `node_retirement_request` (feedback_allowlist_validator).
    // State-dependent legality (nodes present / non-coarse /
    // non-protected, PF-only) lives in the engine. Emitted only when
    // present, so every other audit payload stays byte-identical.
    let node_retirement_request =
        normalize_node_retirement_request_field(obj.get("node_retirement_request"), &mut errors);
    if let Some(request) = node_retirement_request {
        plan_view
            .as_object_mut()
            .expect("plan_view is an object")
            .insert("node_retirement_request".to_owned(), request);
    }
    if !errors.is_empty() {
        return ArtifactValidationOutput::failure(errors);
    }
    ArtifactValidationOutput::success(plan_view)
}


fn normalize_node_retirement_request_field(
    value: Option<&Value>,
    errors: &mut Vec<String>,
) -> Option<Value> {
    let field = value?;
    if field.is_null() {
        return None;
    }
    let Some(obj) = field.as_object() else {
        errors.push("node_retirement_request must be an object".to_string());
        return None;
    };
    let nodes = expect_string_list(
        obj.get("nodes").or_else(|| obj.get("node_ids")),
        "node_retirement_request.nodes",
        errors,
    );
    if nodes.is_empty() {
        errors.push("node_retirement_request.nodes must be non-empty".to_string());
    }
    let reason = expect_string(
        obj.get("reason"),
        "node_retirement_request.reason",
        false,
        errors,
    );
    Some(json!({
        "nodes": nodes,
        "reason": reason,
    }))
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

/// Revision Planner structured action list (`revision_plan.md` §9). Validates
/// shape (an object with `targets` / `nodes` arrays of the documented entry
/// shapes) and that each `action` is drawn from the allowed verb set. Names
/// are NOT checked here — the validator has no request context; the engine
/// validator (`stuck_math_audit_validation_failure`) checks target/node names
/// against the revision-planning context. Returns the normalized object so the
/// allowlist re-emit carries it through.
fn validate_revision_actions(value: Option<&Value>, errors: &mut Vec<String>) -> Value {
    let empty = json!({ "targets": [], "nodes": [] });
    let Some(field) = value else {
        return empty;
    };
    if field.is_null() {
        return empty;
    }
    let Some(obj) = field.as_object() else {
        errors.push("revision_actions must be an object".to_string());
        return empty;
    };
    let targets = validate_revision_action_entries(
        obj.get("targets"),
        "revision_actions.targets",
        "target",
        "classification",
        &crate::model::REVISION_TARGET_CLASSIFICATIONS,
        &["covering_nodes"],
        errors,
    );
    let nodes = validate_revision_action_entries(
        obj.get("nodes"),
        "revision_actions.nodes",
        "node",
        "action",
        &crate::model::REVISION_NODE_ACTIONS,
        &["reason"],
        errors,
    );
    json!({ "targets": targets, "nodes": nodes })
}

/// Shared shape/action-set check for one `revision_actions` entry array.
/// `subject_key` is the entry's name field (`target` / `node`); `extras` are
/// the entry's extra fields preserved verbatim (`covering_nodes` for targets,
/// `reason` for nodes).
fn validate_revision_action_entries(
    value: Option<&Value>,
    field: &str,
    subject_key: &str,
    verb_key: &str,
    allowed_verbs: &[&str],
    extras: &[&str],
    errors: &mut Vec<String>,
) -> Vec<Value> {
    let Some(field_value) = value else {
        return Vec::new();
    };
    if field_value.is_null() {
        return Vec::new();
    }
    let Some(arr) = field_value.as_array() else {
        errors.push(format!("{field} must be an array"));
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let Some(entry_obj) = entry.as_object() else {
            errors.push(format!("{field}[{i}] must be an object"));
            continue;
        };
        let subject = expect_string(
            entry_obj.get(subject_key),
            &format!("{field}[{i}].{subject_key}"),
            false,
            errors,
        );
        let verb = expect_string(
            entry_obj.get(verb_key),
            &format!("{field}[{i}].{verb_key}"),
            false,
            errors,
        );
        if !verb.is_empty() && !allowed_verbs.contains(&verb.as_str()) {
            errors.push(format!(
                "{field}[{i}].{verb_key} `{verb}` must be one of {allowed_verbs:?}"
            ));
        }
        let mut normalized = serde_json::Map::new();
        normalized.insert(subject_key.to_string(), json!(subject));
        normalized.insert(verb_key.to_string(), json!(verb));
        for extra in extras {
            match *extra {
                "covering_nodes" => {
                    let nodes = expect_string_list(
                        entry_obj.get("covering_nodes"),
                        &format!("{field}[{i}].covering_nodes"),
                        errors,
                    );
                    normalized.insert("covering_nodes".to_string(), json!(nodes));
                }
                "reason" => {
                    let reason = expect_string(
                        entry_obj.get("reason"),
                        &format!("{field}[{i}].reason"),
                        true,
                        errors,
                    );
                    normalized.insert("reason".to_string(), json!(reason));
                }
                _ => {}
            }
        }
        out.push(Value::Object(normalized));
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
        "dead_code_elim" | "deadcodeelim" => {
            // This literal rebuild is an allowlist. Preserve the advisory
            // hint explicitly or it will disappear before normalization.
            let hint = obj
                .get("hint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            json!({"kind": "dead_code_elim", "hint": hint})
        }
        "extract_helper" | "extracthelper" => {
            // Both fields must be named in this literal rebuild. The
            // rebuild is an allowlist: anything not mentioned here is
            // dropped, so an unnamed field passes every Rust test and
            // then arrives empty in the live run.
            let ordinal = obj.get("ordinal").and_then(|v| v.as_u64()).unwrap_or(0);
            if ordinal < 1 {
                errors.push(format!(
                    "{prefix}.ordinal must be an integer >= 1 for extract_helper"
                ));
            }
            let hint = obj
                .get("hint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            json!({"kind": "extract_helper", "ordinal": ordinal, "hint": hint})
        }
        "extract_shared" | "extractshared" => {
            // Literal allowlist rebuild: preserve every new kind field.
            let ordinal = obj.get("ordinal").and_then(|v| v.as_u64()).unwrap_or(0);
            if ordinal < 1 {
                errors.push(format!(
                    "{prefix}.ordinal must be an integer >= 1 for extract_shared"
                ));
            }
            let hint = obj
                .get("hint")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let co_parents = match obj.get("co_parents") {
                Some(Value::Array(parents)) => parents
                    .iter()
                    .enumerate()
                    .filter_map(|(index, parent)| {
                        let value = parent.as_str().unwrap_or("").trim().to_string();
                        if value.is_empty() {
                            errors.push(format!(
                                "{prefix}.co_parents[{index}] must be a non-empty string"
                            ));
                            None
                        } else {
                            Some(value)
                        }
                    })
                    .collect::<Vec<_>>(),
                _ => {
                    errors.push(format!(
                        "{prefix}.co_parents must be an array for extract_shared"
                    ));
                    Vec::new()
                }
            };
            json!({
                "kind": "extract_shared",
                "co_parents": co_parents,
                "ordinal": ordinal,
                "hint": hint,
            })
        }
        _ => {
            errors.push(format!(
                "{prefix}.kind must be one of ['substitution', 'lint_fix', 'dead_code_elim', \
                 'extract_helper', 'extract_shared']"
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
                // Amendment G2: this validator REBUILDS each entry, so
                // `doc` must be carried through explicitly or the wire
                // silently strips it before review normalization ever
                // sees it. Shape-only here (non-empty string when
                // present); the registry-membership check is
                // `parse_paper_focus_ranges`'s (fail-closed).
                let mut entry = serde_json::Map::new();
                entry.insert("start_line".to_owned(), json!(start_line));
                entry.insert("end_line".to_owned(), json!(end_line));
                entry.insert("reason".to_owned(), json!(reason));
                match obj.get("doc") {
                    None | Some(Value::Null) => {}
                    Some(Value::String(doc)) if !doc.trim().is_empty() => {
                        entry.insert("doc".to_owned(), json!(doc.trim()));
                    }
                    Some(_) => {
                        errors.push(
                            "paper_focus_ranges doc must be a non-empty reference-paper id string when present"
                                .to_string(),
                        );
                        return json!([]);
                    }
                }
                normalized.push(Value::Object(entry));
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
        if verdict == "FalseAsStated" && comment.is_empty() {
            errors.push(format!(
                "{field}[{idx}].comment must be non-empty when verdict is FalseAsStated"
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

/// `expect_string_list` minus the dedupe: order preserved, duplicates flow
/// through. Used for the sidecar queue action lists, whose duplicate
/// entries must reach `sidecar_queue_response_violations` so legality can
/// reject them with the named duplicate reason (deviation 1) rather than
/// the validator silently repairing the payload.
fn expect_string_list_keep_duplicates(
    value: Option<&Value>,
    field: &str,
    errors: &mut Vec<String>,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(format!("{field} must be a list"));
        return Vec::new();
    };
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
        normalized.push(text);
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
        validate_substantiveness_result_data, validate_trellis_audit_result_data,
        validate_trellis_reviewer_result_data, validate_trellis_stuck_math_audit_result_data,
        validate_trellis_stuck_math_audit_result_data_with_trust,
        validate_trellis_worker_result_data,
    };
    use serde_json::json;

    #[test]
    fn audit_validator_round_trips_every_extract_helper_field() {
        // These validators rebuild each arm with a literal `json!`, which
        // is an allowlist: an unnamed field is silently dropped, passes
        // every Rust test that only checks `ok`, and then arrives EMPTY in
        // the live run. Assert the values survive, not just that the
        // payload validates.
        let result = validate_trellis_audit_result_data(&json!({
            "new_tasks": [{
                "target_node": "BigParent",
                "rationale": "7295 lines",
                "confidence": "high",
                "kind": {
                    "kind": "extract_helper",
                    "ordinal": 3,
                    "hint": "the block proving X around the fourth case split"
                }
            }],
            "outcome": "audit_done"
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        let kind = &data["new_tasks"][0]["kind"];
        assert_eq!(kind["kind"], json!("extract_helper"));
        assert_eq!(kind["ordinal"], json!(3));
        assert_eq!(
            kind["hint"],
            json!("the block proving X around the fourth case split")
        );

        let zero_ordinal = validate_trellis_audit_result_data(&json!({
            "new_tasks": [{
                "target_node": "BigParent",
                "rationale": "r",
                "confidence": "high",
                "kind": {"kind": "extract_helper", "ordinal": 0, "hint": ""}
            }],
            "outcome": "audit_done"
        }));
        assert!(!zero_ordinal.ok, "ordinals are 1-based");
    }

    #[test]
    fn audit_validator_round_trips_dead_code_elim_hint() {
        let result = validate_trellis_audit_result_data(&json!({
            "new_tasks": [{
                "target_node": "LongProof",
                "rationale": "suspect dead context",
                "confidence": "medium",
                "kind": {"kind": "dead_code_elim", "hint": "first have-chain"}
            }],
            "outcome": "audit_done"
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let kind = &result.data.expect("validated payload")["new_tasks"][0]["kind"];
        assert_eq!(kind, &json!({"kind": "dead_code_elim", "hint": "first have-chain"}));
    }

    #[test]
    fn audit_validator_round_trips_every_extract_shared_field() {
        let result = validate_trellis_audit_result_data(&json!({
            "new_tasks": [{
                "target_node": "AlphaParent",
                "rationale": "three nodes share the region",
                "confidence": "high",
                "kind": {
                    "kind": "extract_shared",
                    "co_parents": ["BetaParent", "GammaParent"],
                    "ordinal": 4,
                    "hint": "region-deadbeef"
                }
            }],
            "outcome": "audit_done"
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        assert_eq!(
            &result.data.expect("validated payload")["new_tasks"][0]["kind"],
            &json!({
                "kind": "extract_shared",
                "co_parents": ["BetaParent", "GammaParent"],
                "ordinal": 4,
                "hint": "region-deadbeef"
            })
        );
    }

    #[test]
    fn reviewer_validator_round_trips_cleanup_repair_node() {
        // Same allowlist hazard on the reviewer side.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "next_mode": "cleanup",
            "allow_new_obligations": true,
            "must_close_active": false,
            "reason": "repair the helper's NL statement",
            "cleanup_repair_node": "Helper"
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["cleanup_repair_node"], json!("Helper"));

        let absent = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "next_mode": "cleanup",
            "allow_new_obligations": true,
            "must_close_active": false,
            "reason": "keep polishing"
        }));
        assert_eq!(
            absent.data.expect("validated payload")["cleanup_repair_node"],
            json!(null)
        );
    }

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
    fn stuck_math_audit_validator_trust_accepts_deviation_with_empty_candidate() {
        // Agreement with the trust adjudication fragment
        // (pv/stuck_audit/08_model_refutation_adjudication_trust_v1.md):
        // "`under_model_candidate_invariant` must be empty." A
        // fragment-obeying deviation ruling must pass the real validator.
        let result = validate_trellis_stuck_math_audit_result_data_with_trust(
            &json!({
                "report": "## Claim being audited\n".to_string()
                    + &"x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
                "confirm_need_input": true,
                "under_model_ruling": "deviation",
                "under_model_candidate_invariant": "",
            }),
            true,
        );
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
    }


    /// Standing constraint: the give-up adjudication critic is PV-only and
    /// NOTHING in math mode may change. This shared validator takes
    /// `trust_base_required_v1`, so the adjudication pair must be off the
    /// math-mode allowlist entirely: a hallucinated `adjudication_decision`
    /// is stripped from the round-trip and cannot serve as the concrete
    /// signal (`has_gap_payload`), so a tasks-free / probes-free result the
    /// checker refused before this lane existed is still refused.
    #[test]
    fn stuck_math_audit_validator_preserves_plan_audit_candidate_without_ruling() {
        // PV under-model (approach-audit route): a PLAN-writing audit (empty
        // ruling) that names an authoring candidate must have the candidate
        // SURVIVE the validator round-trip — the runtime_cli builds the
        // kernel response from `validated_data`, so a strip here silently
        // empties the candidate the reviewer needs to enact (the dec2flt
        // audits 610/611 complied in their raw results; the kernel saw "").
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "## Claim being audited\n".to_string()
                + &"x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "tasks": [
                {
                    "id": "task-1",
                    "title": "Open the assumption-authoring lane",
                    "body": "Admit the slice-boundary contract as a PV under-model assumption."
                }
            ],
            "under_model_candidate_invariant":
                "  core.slice.Slice.first on a valid nonempty byte slice returns its head  "
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(
            data["under_model_candidate_invariant"],
            json!("core.slice.Slice.first on a valid nonempty byte slice returns its head"),
            "the plan-audit candidate must survive the validator round-trip"
        );
        assert!(
            data.get("under_model_ruling").is_none(),
            "no ruling was made; the ruling key stays absent (baseline-stable)"
        );

        // A candidate-less plan audit stays byte-identical to the
        // pre-Slice-1 shape: neither key is emitted.
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "## Claim being audited\n".to_string()
                + &"x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert!(data.get("under_model_candidate_invariant").is_none());
        assert!(data.get("under_model_ruling").is_none());
    }

    #[test]
    fn stuck_math_audit_validator_set_live_polarity_target_cross_field_and_reemit() {
        let report = "## Claim being audited\n".to_string()
            + &"x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS);

        // A target with an EMPTY set_live_polarity is a cross-field error.
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": report,
            "set_live_polarity_target": "goal:correct"
        }));
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("only meaningful with a non-empty set_live_polarity")),
            "unexpected errors: {:?}",
            result.errors
        );

        // Target + polarity: accepted, and the (trimmed) target survives the
        // round-trip into plan_view so the runtime CLI can rebuild it.
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": report,
            "set_live_polarity": "disprove",
            "set_live_polarity_target": "  goal:correct  "
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["set_live_polarity_target"], json!("goal:correct"));

        // A target-less flip audit stays byte-identical to the prior shape:
        // the key is absent from plan_view (baseline-stable).
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": report,
            "set_live_polarity": "disprove"
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert!(data.get("set_live_polarity_target").is_none());
    }

    #[test]
    fn stuck_math_audit_validator_accepts_minimal_assumptions_lane_verdict() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "assumptions_lane_verdict": "pass"
        }));

        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["report"], json!(""));
        assert_eq!(data["tasks"], json!([]));
        assert_eq!(data["probe_paths"], json!([]));
        assert_eq!(data["assumptions_lane_verdict"], json!("pass"));
        assert_eq!(data["assumptions_lane_reason"], json!(""));
        assert_eq!(data["assumptions_lane_hunt_result"], json!(""));
    }

    #[test]
    fn gap_validator_accepts_planner_route_and_survives_allowlist() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "route_tex": "Reduce the bounded tail through the paper's elimination tree; measure = depth."
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        // Allowlist: the route fields must survive the re-emit, not be stripped.
        assert_eq!(
            data["route_tex"],
            json!("Reduce the bounded tail through the paper's elimination tree; measure = depth.")
        );
        assert_eq!(data["route_needs_human"], json!(false));
    }

    #[test]
    fn gap_validator_accepts_planner_route_needs_human() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "route_needs_human": true
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["route_needs_human"], json!(true));
    }

    #[test]
    fn revision_validator_accepts_actions_and_survives_allowlist() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "tasks": [],
            "revision_actions": {
                "targets": [
                    {"target": "thm:main", "classification": "strengthen", "covering_nodes": ["MainTheorem"]}
                ],
                "nodes": [
                    {"node": "MainTheorem", "action": "restate", "reason": "stronger exponent"}
                ]
            }
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        // Allowlist: the structured action list must survive the re-emit.
        assert_eq!(data["revision_actions"]["targets"][0]["target"], json!("thm:main"));
        assert_eq!(data["revision_actions"]["targets"][0]["classification"], json!("strengthen"));
        assert_eq!(data["revision_actions"]["nodes"][0]["node"], json!("MainTheorem"));
        assert_eq!(data["revision_actions"]["nodes"][0]["action"], json!("restate"));
        // A non-empty action list is itself the concrete signal.
    }

    #[test]
    fn revision_validator_rejects_unknown_action_verb() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "revision_actions": {
                "nodes": [{"node": "MainTheorem", "action": "obliterate", "reason": "no"}]
            }
        }));
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("action `obliterate`")));
    }

    #[test]
    fn gap_validator_critic_reject_requires_feedback() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "gap_decision": "reject",
            "gap_feedback": ""
        }));
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("gap_feedback must be non-empty")));
    }

    #[test]
    fn gap_validator_accepts_critic_accept_with_tasks() {
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "report": "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS),
            "gap_decision": "accept",
            "tasks": [{
                "id": "gap-impl",
                "title": "Author the support node",
                "body": "Implement the accepted route and arrive mechanically closed."
            }]
        }));
        assert!(result.ok, "unexpected errors: {:?}", result.errors);
        let data = result.data.expect("validated payload");
        assert_eq!(data["gap_decision"], json!("accept"));
        assert_eq!(data["tasks"].as_array().unwrap().len(), 1);
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
            "deleted_nodes": [],
            "needs_restructure_suggested_nodes": []
        }));

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err.contains("reference/")));
    }

    #[test]
    fn worker_validator_parses_and_reemits_node_reference_grounds() {
        // Amendment G3 regression: this validator strips unknown fields,
        // so node_reference_grounds must be parsed AND re-emitted (the
        // node_deviation_claims twin) — and stay ABSENT when the worker
        // omitted it (byte-stability for legacy payloads).
        let base = |extra: Option<serde_json::Value>| {
            let mut payload = json!({
                "outcome": "valid",
                "summary": "claims a reference",
                "comments": "",
                "semantic_dep_updates": {},
                "target_claim_updates": {},
                "difficulty_updates": {},
                "deleted_nodes": [],
                "needs_restructure_suggested_nodes": []
            });
            if let Some(grounds) = extra {
                payload
                    .as_object_mut()
                    .unwrap()
                    .insert("node_reference_grounds".to_string(), grounds);
            }
            payload
        };

        let with = validate_trellis_worker_result_data(&base(Some(
            json!({"A": ["smith2020"], "B": []}),
        )));
        assert!(with.ok, "errors: {:?}", with.errors);
        let data = with.data.expect("success data");
        assert_eq!(
            data["node_reference_grounds"],
            json!({"A": ["smith2020"], "B": []}),
            "claims (incl. explicit clears) must survive the allowlist re-emit"
        );

        let without = validate_trellis_worker_result_data(&base(None));
        assert!(without.ok, "errors: {:?}", without.errors);
        let data = without.data.expect("success data");
        assert!(
            data.get("node_reference_grounds").is_none(),
            "omitted field stays omitted — legacy validator output is byte-identical"
        );
    }

    #[test]
    fn worker_validator_preserves_under_model_fields_on_target_false_under_model() {
        // PV under-model (Slice 1, regression d): the new outcome's disproof /
        // route opinion / reasoning must SURVIVE the allowlist strip
        // (feedback_allowlist_validator) — they are re-emitted in the success
        // data only for this outcome.
        let result = validate_trellis_worker_result_data(&json!({
            "outcome": "target_false_under_model",
            "summary": "T is false under the model",
            "comments": "",
            "deleted_nodes": [],
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
            "needs_restructure_suggested_nodes": [],
            "deleted_nodes": [],
            "under_model_disproof": "  x0 = over-isize::MAX slice falsifies parse_number  ",
            "under_model_route_opinion": "model-deviation",
            "under_model_reasoning": "the Usize→Isize cast wraps negative"
        }));
        assert!(result.ok, "errors: {:?}", result.errors);
        let data = result.data.expect("success data");
        assert_eq!(data["outcome"], json!("target_false_under_model"));
        assert_eq!(
            data["under_model_disproof"],
            json!("x0 = over-isize::MAX slice falsifies parse_number")
        );
        assert_eq!(data["under_model_route_opinion"], json!("model-deviation"));
        assert_eq!(
            data["under_model_reasoning"],
            json!("the Usize→Isize cast wraps negative")
        );
    }

    #[test]
    fn worker_validator_requires_disproof_for_target_false_under_model() {
        // The disproof + route opinion are REQUIRED for the under-model outcome.
        let result = validate_trellis_worker_result_data(&json!({
            "outcome": "target_false_under_model",
            "summary": "T is false under the model",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
            "needs_restructure_suggested_nodes": []
        }));
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("under_model_disproof must be a non-empty")));
    }

    #[test]
    fn worker_validator_strips_under_model_fields_on_other_outcomes() {
        // For a non-under-model outcome the under-model carriers must be ABSENT
        // from the success data (byte-stability) — and supplying them is an
        // error (so a stale disproof cannot ride an ordinary outcome).
        let clean = validate_trellis_worker_result_data(&json!({
            "outcome": "valid",
            "summary": "ordinary",
            "comments": "",
            "deleted_nodes": [],
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
            "needs_restructure_suggested_nodes": [],
            "deleted_nodes": []
        }));
        assert!(clean.ok);
        let data = clean.data.expect("success data");
        assert!(data.get("under_model_disproof").is_none());
        assert!(data.get("under_model_route_opinion").is_none());
        assert!(data.get("under_model_reasoning").is_none());

        let bad = validate_trellis_worker_result_data(&json!({
            "outcome": "valid",
            "summary": "ordinary",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
            "needs_restructure_suggested_nodes": [],
            "deleted_nodes": [],
            "under_model_disproof": "leaked"
        }));
        assert!(!bad.ok);
        assert!(bad
            .errors
            .iter()
            .any(|err| err.contains("when outcome is not target_false_under_model")));
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
    fn reviewer_validator_rebuild_carries_paper_focus_range_doc() {
        // Amendment G2 regression: `normalize_paper_focus_ranges` REBUILDS
        // each entry, so `doc` must be carried through explicitly or the
        // reviewer wire silently strips it before normalization.
        let payload = |ranges: serde_json::Value| {
            json!({
                "decision": "continue",
                "reason": "cite the reference",
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
                "paper_focus_ranges": ranges,
            })
        };
        let result = validate_trellis_reviewer_result_data(&payload(json!([
            {"start_line": 3, "end_line": 9, "reason": "cited lemma", "doc": "smith2020"},
            {"start_line": 1, "end_line": 2, "reason": "primary passage"},
        ])));
        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(data["paper_focus_ranges"][0]["doc"], json!("smith2020"));
        assert!(
            data["paper_focus_ranges"][1].get("doc").is_none(),
            "doc-free entries stay byte-identical to the legacy shape"
        );
        // Round-trip into the typed payload the normalizer reads.
        let raw: crate::review_normalization::RawReviewPayload =
            serde_json::from_value(data).expect("round-trip into RawReviewPayload");
        assert_eq!(
            raw.paper_focus_ranges[0].doc,
            Some(crate::model::RefPaperId::from("smith2020"))
        );
        assert_eq!(raw.paper_focus_ranges[1].doc, None);

        // Bad shapes are rejected.
        let bad = validate_trellis_reviewer_result_data(&payload(json!([
            {"start_line": 3, "end_line": 9, "reason": "r", "doc": 7},
        ])));
        assert!(!bad.ok);
        assert!(bad
            .errors
            .iter()
            .any(|err| err.contains("doc must be a non-empty reference-paper id string")));
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
    fn reviewer_validator_passes_through_cleanup_batch_tasks() {
        // Regression test for the batch-dispatch strip bug: the allowlist
        // re-emit dropped `cleanup_batch_tasks` (added to RawReviewPayload
        // + the engine apply path, but missing from this validator's
        // extract/emit block), so a reviewer's cleanup batch never reached
        // the engine via the live JSON path — the in-process Rust tests
        // masked the bug by constructing ReviewResponse directly. This
        // mirrors [[feedback_allowlist_validator]] (next_active_coarse,
        // audit_request, global_repair). The real 3534 artifact carried
        // `cleanup_batch_tasks:[9,13,14]` (with no system_feedback) and it
        // was silently dropped. Exercises the FULL JSON path the prior
        // audit skipped: validate → assert survives → deserialize
        // RawReviewPayload → assert survives.
        let result = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "batch three lintfix tasks into one burst",
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
            "cleanup_batch_tasks": [9, 13, 14],
        }));

        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("normalized payload");
        assert_eq!(
            data["cleanup_batch_tasks"],
            json!([9, 13, 14]),
            "cleanup_batch_tasks must survive the allowlist re-emit"
        );

        // Round-trip into RawReviewPayload (exactly what
        // check_trellis_reviewer_result_output deserializes from the
        // stripped validated_data before normalization runs).
        let raw: crate::review_normalization::RawReviewPayload =
            serde_json::from_value(data).expect("round-trip into RawReviewPayload");
        assert_eq!(
            raw.cleanup_batch_tasks,
            Some(vec![9u32, 13, 14]),
            "cleanup_batch_tasks must survive RawReviewPayload deserialization"
        );
    }

    #[test]
    fn reviewer_validator_omits_absent_cleanup_batch_tasks() {
        // Byte-stability guard: a non-batch decision (no cleanup_batch_tasks
        // key) must NOT gain a spurious `cleanup_batch_tasks` key in the
        // validated output — mirrors `skip_serializing_if = "Option::is_none"`
        // so existing contract baselines for non-batch decisions stay
        // byte-unchanged. Also confirms JSON null is treated as absent.
        let omitted = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "no batch this cycle",
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
        assert!(
            omitted_data.get("cleanup_batch_tasks").is_none(),
            "absent cleanup_batch_tasks must stay absent (no spurious key): {omitted_data}"
        );

        let null_batch = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "explicit null batch",
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
            "cleanup_batch_tasks": null,
        }));
        assert!(null_batch.ok, "errors={:?}", null_batch.errors);
        let null_data = null_batch.data.expect("normalized payload");
        assert!(
            null_data.get("cleanup_batch_tasks").is_none(),
            "null cleanup_batch_tasks must be treated as absent: {null_data}"
        );
    }

    #[test]
    fn reviewer_validator_rejects_non_array_cleanup_batch_tasks() {
        // A non-array value, or an array with a non-integer entry, is a
        // clear error (consistent with the validator's existing style).
        let bad_kind = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "bad batch shape",
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
            "cleanup_batch_tasks": "9,13,14",
        }));
        assert!(!bad_kind.ok);
        assert!(
            bad_kind
                .errors
                .iter()
                .any(|e| e.contains("cleanup_batch_tasks")),
            "expected a cleanup_batch_tasks error: {:?}",
            bad_kind.errors
        );

        let bad_entry = validate_trellis_reviewer_result_data(&json!({
            "decision": "continue",
            "reason": "bad batch entry",
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
            "cleanup_batch_tasks": [9, -3, "x"],
        }));
        assert!(!bad_entry.ok);
        assert!(
            bad_entry
                .errors
                .iter()
                .any(|e| e.contains("cleanup_batch_tasks")),
            "expected a cleanup_batch_tasks entry error: {:?}",
            bad_entry.errors
        );
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
    fn substantiveness_admits_false_as_stated_with_required_comment() {
        let result = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {
                        "node": "GoalStmt",
                        "verdict": "FalseAsStated",
                        "comment": "refuted under the pinned extraction model"
                    },
                ],
            },
            "overall": "APPROVE",
            "summary": "the intended obligation is false under the pinned model",
            "comments": "",
        }));
        assert!(result.ok, "errors={:?}", result.errors);

        let missing_comment = validate_substantiveness_result_data(&json!({
            "substantiveness": {
                "decision": "PASS",
                "verdicts": [
                    {"node": "GoalStmt", "verdict": "FalseAsStated"},
                ],
            },
            "overall": "APPROVE",
            "summary": "missing refutation basis",
            "comments": "",
        }));
        assert!(!missing_comment.ok);
        assert!(
            missing_comment
                .errors
                .iter()
                .any(|error| error
                    .contains("comment must be non-empty when verdict is FalseAsStated")),
            "expected comment-required error; got {:?}",
            missing_comment.errors
        );
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
        assert_eq!(
            data["global_repair_approved_extension_node_ids"],
            json!(["X"])
        );
        assert_eq!(data["global_repair_auditor_reason"], "in scope");
    }

    /// feedback_allowlist_validator regression: the audit's
    /// `node_retirement_request` must survive the allowlist re-emit,
    /// stay absent when unsupplied, and reject each bad shape.
    #[test]
    fn validate_trellis_stuck_math_audit_result_node_retirement_request_survives_and_rejects() {
        let report_padding = "x".repeat(crate::model::AUDIT_REPORT_TEXT_MIN_CHARS);
        let report = format!("## Claim being audited\nretire {report_padding}");
        let result = validate_trellis_stuck_math_audit_result_data(&json!({
            "confirm_need_input": false,
            "report": report,
            "tasks": [],
            "probe_paths": [],
            "node_retirement_request": {"nodes": ["H1", "H2"], "reason": "mis-factored"},
        }));
        assert!(result.ok, "errors={:?}", result.errors);
        let data = result.data.expect("data");
        assert_eq!(
            data["node_retirement_request"],
            json!({"nodes": ["H1", "H2"], "reason": "mis-factored"})
        );

        // Absent / null stays off the output (byte-compatible).
        let absent = validate_trellis_stuck_math_audit_result_data(&json!({
            "confirm_need_input": false,
            "report": report,
            "tasks": [],
            "probe_paths": [],
        }));
        assert!(absent.ok, "errors={:?}", absent.errors);
        let absent_data = absent.data.expect("data");
        assert!(absent_data.get("node_retirement_request").is_none());
        let null = validate_trellis_stuck_math_audit_result_data(&json!({
            "confirm_need_input": false,
            "report": report,
            "tasks": [],
            "probe_paths": [],
            "node_retirement_request": null,
        }));
        assert!(null.ok, "errors={:?}", null.errors);
        assert!(null
            .data
            .expect("data")
            .get("node_retirement_request")
            .is_none());

        // Rejections: non-object, empty nodes, empty reason.
        for (payload, expected) in [
            (json!("H1"), "must be an object"),
            (
                json!({"nodes": [], "reason": "r"}),
                "nodes must be non-empty",
            ),
            (
                json!({"nodes": ["H1"], "reason": ""}),
                "reason must be non-empty",
            ),
        ] {
            let bad = validate_trellis_stuck_math_audit_result_data(&json!({
                "confirm_need_input": false,
                "report": report,
                "tasks": [],
                "probe_paths": [],
                "node_retirement_request": payload,
            }));
            assert!(!bad.ok);
            assert!(
                bad.errors.iter().any(|err| err.contains(expected)),
                "expected error containing {expected:?}; got {:?}",
                bad.errors
            );
        }
    }

    /// feedback_allowlist_validator regression: the reviewer's
    /// `dispatch_node_retirement` / `node_retirement_decline_reason`
    /// must survive the allowlist re-emit and stay off the output when
    /// unset.
    #[test]
    fn validate_trellis_reviewer_result_preserves_node_retirement_fields() {
        let base = json!({
            "decision": "continue",
            "reason": "retire per audit order",
            "comments": "",
            "next_mode": "restructure",
            "reset": "none",
            "allow_new_obligations": false,
            "must_close_active": false,
        });
        let mut dispatch = base.clone();
        dispatch
            .as_object_mut()
            .unwrap()
            .insert("dispatch_node_retirement".into(), json!(true));
        let result = validate_trellis_reviewer_result_data(&dispatch);
        assert!(result.ok, "errors={:?}", result.errors);
        assert_eq!(result.data.expect("data")["dispatch_node_retirement"], true);

        let mut decline = base.clone();
        decline.as_object_mut().unwrap().insert(
            "node_retirement_decline_reason".into(),
            json!("  survivors are load-bearing  "),
        );
        let result = validate_trellis_reviewer_result_data(&decline);
        assert!(result.ok, "errors={:?}", result.errors);
        assert_eq!(
            result.data.expect("data")["node_retirement_decline_reason"],
            "survivors are load-bearing"
        );

        // Unset fields stay off the output (byte-compatible).
        let plain = validate_trellis_reviewer_result_data(&base);
        assert!(plain.ok, "errors={:?}", plain.errors);
        let plain_data = plain.data.expect("data");
        assert!(plain_data.get("dispatch_node_retirement").is_none());
        assert!(plain_data.get("node_retirement_decline_reason").is_none());

        // Dispatch + decline are mutually exclusive.
        let mut both = base;
        both.as_object_mut()
            .unwrap()
            .insert("dispatch_node_retirement".into(), json!(true));
        both.as_object_mut().unwrap().insert(
            "node_retirement_decline_reason".into(),
            json!("also declining"),
        );
        let result = validate_trellis_reviewer_result_data(&both);
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("mutually exclusive")));
    }
}
