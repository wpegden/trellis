use crate::model::{
    Blocker, BlockerKind, BlockerObject, NodeId, Phase, RequestKind, RetryOutcomeKind, TargetId,
    TaskMode, WorkerProfile, WorkerValidationKind, WrapperRequest,
};
use crate::{blocker_choice_ids, blocker_choices, extract_tex_statement_items};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

pub fn default_contract_value() -> Value {
    json!({})
}

pub fn prompt_contract_version() -> u32 {
    // Bumped 33 -> 34 for deviation protocol prompts and reviewer
    // evidence separation.
    //
    // Bumped 32 -> 33 for NeedInputAuditor: reviewer `need_input`
    // requests now first dispatch a dedicated auditor scenario on the
    // existing stuck_math_audit lane, and the audit artifact gains
    // `confirm_need_input`.
    //
    // Bumped 31 -> 32 for the active-coarse-anchor workflow layer
    // (proposal v32, 2026-05-20). ProofFormalization adds a locked
    // coarse-DAG focus on top of the existing per-cycle active_node:
    // the reviewer picks an `active_coarse_node` from
    // `kernel_hinted_next_active_coarse_nodes`, then `active_node`
    // legality is narrowed to the down-cone of that anchor (widened
    // to blocker-repair cones when `coarse_repair_mode` is true).
    // The anchor stays locked against change until shallow-coarse
    // closure + empty global blockers, OR a starvation threshold
    // (`stuck_coarse_repair_threshold`) is hit. Reviewer response
    // gains `next_active_coarse: Option<NodeId>`; review request
    // surfaces `active_coarse_node`, `kernel_hinted_next_active_coarse_nodes`,
    // `coarse_repair_mode`, `cycles_in_coarse_repair_mode`. New
    // prompt fragments: `08_coarse_anchor_locked.md`,
    // `08_coarse_anchor_open.md`, `09_coarse_repair_mode.md`.
    // Mechanism dormant when `coarse_dag_nodes` is empty.
    //
    // Bumped 30 -> 31 to rename proof next-active routing from hard
    // allowed nodes to kernel hints and surface reviewer rejection reasons.
    //
    // Bumped 29 -> 30 to surface shallow-coarse progress counters and
    // make StuckMathAudit activation use a configurable no-progress
    // threshold instead of worker Stuck/NeedsRestructure outcomes.
    //
    // Bumped 28 -> 29 for a dedicated StuckMathAudit reference-paper
    // fragment that exposes the configured paper source path and makes
    // paper-grounding explicit for the read-only audit role.
    //
    // Bumped 27 -> 28 for StuckMathAudit as an independent read-only
    // audit role with its own prompt/artifact contract and durable
    // audit_plan handoff to reviewers/workers.
    //
    // Bumped 26 -> 27 for StuckMathAudit reviewer Lean product handoff.
    // Repeated proof-formalization math blockage can now activate a
    // reviewer-side Lean scratch mode and forward a neutral
    // `reviewer_lean_product` to the next worker.
    //
    // Bumped 25 -> 26 for explicit proof-obligation scope controls
    // (2026-05-04). Reviewers now choose allow_new_obligations and
    // must_close_active; easy/hard difficulty is advisory only.
    //
    // Bumped 24 → 25 for pending protected-reapproval visibility
    // (2026-05-03). Review requests now surface the pending protected
    // reapproval node set when ordinary verifier blockers still need to
    // drain before the HumanGate reapproval.
    //
    // Bumped 23 → 24 for protected semantic change scoping (2026-05-03).
    // Reviewer contracts can now surface protected_semantic_change_node_ids
    // plus a confirmation flag; worker contracts surface the approved
    // protected scope when such a change is explicitly authorized.
    //
    // Bumped 22 → 23 for the substantiveness lane (2026-04-29).
    // Kernel now emits Paper requests with `substantiveness_verify_nodes`
    // populated in the per-node scenario; verifier prompt picks between
    // target-package and per-node-frontier rubrics; PaperResponse carries
    // a new `node_lane_updates` field with `SubstantivenessStatus` (admits
    // `NotDoneYet` for verifier triage). Spec must match.
    34
}

fn scheme_fragment_path(use_full: bool) -> &'static str {
    if use_full {
        "common/TRELLIS_FORMALIZATION_SCHEME.md"
    } else {
        "common/00_trellis_scheme_brief.md"
    }
}

fn verifier_scheme_fragment_path() -> &'static str {
    // B6: verifiers always get the trim "verifier reference" version that
    // omits reviewer-only mode-machinery (TheoremStating Global/Targeted,
    // ProofFormalization Easy/Hard, end-to-end reviewer step).
    "common/TRELLIS_FORMALIZATION_SCHEME_verifier.md"
}

fn request_uses_full_scheme(request: &WrapperRequest) -> bool {
    match request.kind {
        RequestKind::Paper | RequestKind::Corr | RequestKind::Sound => true,
        RequestKind::Worker | RequestKind::Review => request.fresh_context,
        RequestKind::HumanGate => false,
        // Cleanup-v2 audit gets its own scheme treatment in the audit
        // contract payload (added later); for now, mirror the verifier
        // policy (trim scheme reference) since the audit is a one-shot
        // structured-output role rather than a stateful worker/reviewer
        // continuation.
        RequestKind::Audit | RequestKind::StuckMathAudit => true,
    }
}

/// Verifier housekeeping fields scrubbed from the inline prompt contract
/// JSON. Mirrors `bridge_prompts._VERIFIER_HOUSEKEEPING_FIELDS`. These
/// fields either are kernel render-machinery (prompt_fragments,
/// artifact_prompt_view), are rendered separately via dedicated
/// placeholders (request_summary, previous_own_findings_*), or are
/// kernel-only flags the prompt text already explains
/// (issue_reporting_policy, fixed_item_reporting_policy).
const PROMPT_FACING_VERIFIER_HOUSEKEEPING_DROP: &[&str] = &[
    "prompt_fragments",
    "request_summary",
    "artifact_prompt_view",
    "issue_reporting_policy",
    "fixed_item_reporting_policy",
    "previous_own_findings_by_lane",
    "previous_own_findings",
    "previous_own_findings_for_lane",
];

/// Build a paper-contract prompt-facing view. Drops verifier housekeeping
/// fields + paper-specific duplicates (`target_covering_nodes`,
/// `node_paper_basis_inputs`) that are rendered separately via dedicated
/// placeholders. Drops null Option<> fields. Mirrors
/// `bridge_prompts._prompt_facing_paper_contract` exactly.
///
/// Note: `target_issue_scope` and `node_issue_scope` are NOT emitted by
/// `paper_contract_payload` (strategy (a): don't construct what we'd just
/// scrub), so they need no entry here.
fn paper_prompt_facing_view(payload: &Value) -> Value {
    let mut view = payload.clone();
    if let Some(map) = view.as_object_mut() {
        for field in PROMPT_FACING_VERIFIER_HOUSEKEEPING_DROP {
            map.remove(*field);
        }
        for field in [
            "target_covering_nodes",
            "node_paper_basis_inputs",
        ] {
            map.remove(field);
        }
    }
    drop_null_keys(view)
}

/// Recursively drop `null` map entries from a JSON value, mirroring
/// `bridge_prompts._drop_null_keys`. Used by `prompt_facing_view` builders
/// to keep the inline prompt contract JSON free of `"key": null` noise
/// without affecting the on-disk structured request.
fn drop_null_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(k, v)| {
                    if v.is_null() {
                        None
                    } else {
                        Some((k, drop_null_keys(v)))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(drop_null_keys).collect()),
        other => other,
    }
}

fn artifact_prompt_view_payload() -> Value {
    json!({
        "raw_output_format": "json_only",
        "escape_json_backslashes": true,
        "done_marker_contract": "write_done_after_json_check_passes",
        "checker_authority": "exact_command_is_authoritative",
        "json_check_command_template": [],
        "acceptance_check_command_template": [],
        "failure_recovery": "json_check_required_acceptance_check_best_effort",
        "stdout_policy": "do_not_print_json_to_stdout",
    })
}

fn verifier_common_prompt_fragments(_request: &WrapperRequest) -> Vec<&'static str> {
    // B6: route verifiers to the trim verifier-only scheme reference.
    vec![
        verifier_scheme_fragment_path(),
        "verifier/common/00_intro.md",
    ]
}

fn verifier_shared_prompt_fragments() -> Vec<&'static str> {
    vec![
        "shared/10_repository_root.md",
        "verifier/common/10_lane_id.md",
        "verifier/common/15_previous_findings.md",
        "shared/20_read_files.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
    ]
}

/// Always-last prompt fragment that points the agent at the on-disk
/// structured request file the bridge writes alongside the raw output
/// path. Every kernel-authored fragment list ends with this entry;
/// keeping it kernel-side eliminates the matching bridge-side append
/// (`_augment_fragments_with_request_pointer`).
const STRUCTURED_REQUEST_POINTER_FRAGMENT: &str = "shared/91_structured_request_pointer.md";

fn has_blocker_kind(request: &WrapperRequest, kind: BlockerKind) -> bool {
    request.blockers.iter().any(|blocker| blocker.kind == kind)
}

fn task_mode_snake(mode: &TaskMode) -> &'static str {
    match mode {
        TaskMode::Global => "global",
        TaskMode::Targeted => "targeted",
        TaskMode::Local => "local",
        TaskMode::Restructure => "restructure",
        TaskMode::CoarseRestructure => "coarse_restructure",
        TaskMode::Cleanup => "cleanup",
    }
}

fn reset_choice_snake(reset: &crate::ResetChoice) -> &'static str {
    use crate::ResetChoice::*;
    match reset {
        None => "none",
        LastCommit => "last_commit",
        LastClean => "last_clean",
        TheoremStatingNode => "theorem_stating_node",
    }
}

fn review_decision_snake(decision: &crate::model::ReviewDecisionKind) -> &'static str {
    use crate::model::ReviewDecisionKind::*;
    match decision {
        Continue => "continue",
        AdvancePhase => "advance_phase",
        NeedInput => "need_input",
        Done => "done",
    }
}

/// Mirrors `WrapperRequest::review_response_audit_plan_rejection_reason`
/// in model.rs:3087-3115. Reviewers may dismiss audit-plan tasks only when
/// (a) an audit plan exists, (b) StuckMathAudit is active, and
/// (c) the phase admits dismissal (ProofFormalization / TheoremStating /
/// any need_input_audit plan).
fn review_audit_dismissal_legal(request: &WrapperRequest) -> bool {
    let Some(plan) = request.audit_plan.as_ref() else {
        return false;
    };
    if !request.stuck_math_audit.active {
        return false;
    }
    matches!(
        request.phase,
        Phase::ProofFormalization | Phase::TheoremStating
    ) || plan.need_input_audit
}

fn nonempty_decision_set<'a, I>(values: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    values
        .into_iter()
        .filter_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_ascii_uppercase())
        })
        .collect()
}

fn paper_review_is_split(request: &WrapperRequest) -> bool {
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .paper
            .values()
            .map(|lane| lane.paper_faithfulness.decision.as_str()),
    )
    .len()
        > 1
}

fn corr_review_is_split(request: &WrapperRequest) -> bool {
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .corr
            .values()
            .map(|lane| lane.correspondence.decision.as_str()),
    )
    .len()
        > 1
}

fn sound_review_is_split(request: &WrapperRequest) -> bool {
    // Audit Finding 3: sound evidence is now nested by node then lane;
    // split detection still asks "did any pair of lane verdicts disagree
    // anywhere in this cycle's accumulated evidence?".
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .sound
            .values()
            .flat_map(|by_lane| by_lane.values())
            .map(|lane| lane.soundness.decision.as_str()),
    )
    .len()
        > 1
}

fn has_deterministic_worker_rejection_reasons(request: &WrapperRequest) -> bool {
    !request.deterministic_worker_rejection_reasons.is_empty()
}

fn has_review_verifier_evidence(request: &WrapperRequest) -> bool {
    !request.review_verifier_evidence.paper.is_empty()
        || !request.review_verifier_evidence.deviation.is_empty()
        || !request.review_verifier_evidence.substantiveness.is_empty()
        || !request.review_verifier_evidence.corr.is_empty()
        || !request.review_verifier_evidence.sound.is_empty()
}

fn paper_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if request.deviation_verify_id.is_some() {
        return vec!["verifier/deviation/05_single_file.md"];
    }
    // Per-node scenario: `substantiveness_verify_nodes` non-empty AND
    // `paper_verify_targets` empty (kernel cycle scheduler enforces
    // exactly one frontier active per request). Fall through to the
    // target-package scenario otherwise.
    if !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty() {
        let mut fragments = if request.previous_substantiveness_lane_findings.is_empty() {
            vec!["verifier/substantiveness/05_fresh_node_frontier.md"]
        } else {
            vec!["verifier/substantiveness/05_revisit_node_frontier.md"]
        };
        if request
            .substantiveness_verify_nodes
            .iter()
            .any(|n| n.as_str() == "Preamble")
        {
            fragments.push("verifier/substantiveness/06_with_preamble.md");
        }
        fragments.push("verifier/substantiveness/15_deviations.md");
        return fragments;
    }
    if request.previous_paper_lane_findings.is_empty() {
        vec!["verifier/paper_faithfulness/05_fresh_target_package.md"]
    } else {
        vec!["verifier/paper_faithfulness/05_revisit_target_package.md"]
    }
}

fn corr_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = if request.previous_corr_lane_findings.is_empty() {
        vec!["verifier/correspondence/05_frontier.md"]
    } else {
        vec!["verifier/correspondence/05_revisit_frontier.md"]
    };
    if request.corr_verify_nodes.contains("Preamble") || request.verify_nodes.contains("Preamble") {
        fragments.push("verifier/correspondence/06_with_preamble.md");
    }
    fragments.push("verifier/correspondence/07_scratchpad.md");
    fragments
}

fn sound_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![match request.phase {
        Phase::TheoremStating => "verifier/soundness/05_theorem_target.md",
        Phase::ProofFormalization | Phase::Cleanup | Phase::Complete => {
            "verifier/soundness/05_proof_node.md"
        }
    }];
    if !request.previous_sound_lane_findings.is_empty() {
        fragments.push("verifier/soundness/06_revisit_target.md");
    }
    fragments
}

fn paper_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request);
    fragments.extend(paper_scenario_prompt_fragments(request));
    fragments.extend(verifier_shared_prompt_fragments());
    let is_per_node_scenario =
        !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty();
    let is_deviation_scenario = request.deviation_verify_id.is_some();
    if is_deviation_scenario {
        fragments.extend([
            "verifier/deviation/20_file.md",
            "verifier/deviation/30_contract.md",
            "shared/90_artifact_delivery.md",
            "canonical/DEVIATIONS.md",
        ]);
    } else if is_per_node_scenario {
        fragments.extend([
            "verifier/substantiveness/20_node_frontier.md",
            "verifier/substantiveness/30_contract.md",
            "verifier/substantiveness/40_rubric.md",
            "verifier/substantiveness/50_authority.md",
            "shared/90_artifact_delivery.md",
            "canonical/SUBSTANTIVENESS.md",
        ]);
    } else {
        fragments.extend([
            "verifier/paper_faithfulness/20_targets.md",
            "verifier/paper_faithfulness/30_contract.md",
            "verifier/paper_faithfulness/40_rubric.md",
            "verifier/paper_faithfulness/50_authority.md",
            "shared/90_artifact_delivery.md",
            "canonical/FAITHFULNESS.md",
        ]);
    }
    fragments.push(STRUCTURED_REQUEST_POINTER_FRAGMENT);
    fragments
}

fn paper_target_covering_nodes(request: &WrapperRequest) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    request
        .paper_verify_targets
        .iter()
        .cloned()
        .map(|target| {
            let covering_nodes = request
                .current_target_claims
                .iter()
                .filter(|(node, claims)| {
                    request.current_present_nodes.contains(*node) && claims.contains(&target)
                })
                .map(|(node, _)| node.clone())
                .collect();
            (target, covering_nodes)
        })
        .collect()
}

fn correspondence_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request);
    fragments.extend(corr_scenario_prompt_fragments(request));
    fragments.extend(verifier_shared_prompt_fragments());
    fragments.extend([
        "verifier/correspondence/20_frontier.md",
        "verifier/correspondence/30_contract.md",
        "verifier/correspondence/40_rubric.md",
        "verifier/correspondence/50_authority.md",
        "shared/90_artifact_delivery.md",
        "canonical/CORRESPONDENCE.md",
    ]);
    fragments.push(STRUCTURED_REQUEST_POINTER_FRAGMENT);
    fragments
}

fn soundness_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request);
    fragments.extend(sound_scenario_prompt_fragments(request));
    fragments.extend(verifier_shared_prompt_fragments());
    // Re-verification context fragment: included only when the kernel
    // has computed per-target dep-drift / own-tex-drift context for
    // this Sound request (i.e. the target was previously approved and
    // is now being re-verified because of a fingerprint change). The
    // bridge provides the `reverification_context_json` placeholder
    // only when this fragment is present, so unconditional inclusion
    // would break prompt rendering for fresh-Unknown targets.
    if request.sound_reverification_context.is_some() {
        fragments.push("verifier/common/15a_reverification_context.md");
    }
    fragments.extend([
        "verifier/soundness/20_target.md",
        "verifier/soundness/30_contract.md",
        "verifier/soundness/40_rubric.md",
        "verifier/soundness/50_authority.md",
        "shared/90_artifact_delivery.md",
        "canonical/SOUNDNESS.md",
    ]);
    fragments.push(STRUCTURED_REQUEST_POINTER_FRAGMENT);
    fragments
}

fn worker_intro_fragment(request: &WrapperRequest) -> &'static str {
    match request.worker_context.worker_profile {
        crate::model::WorkerProfile::Theorem => "worker/theorem_stating/00_intro.md",
        crate::model::WorkerProfile::ProofEasy | crate::model::WorkerProfile::ProofHard => {
            "worker/proof_formalization/00_intro.md"
        }
        crate::model::WorkerProfile::Cleanup => "worker/cleanup/00_intro.md",
        crate::model::WorkerProfile::FinalCleanup => "worker/final_cleanup/00_intro.md",
        crate::model::WorkerProfile::None => "worker/generic/00_intro.md",
    }
}

fn theorem_worker_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if has_blocker_kind(request, BlockerKind::PaperFaithfulness) {
        vec!["worker/theorem_stating/05_after_paper_faithfulness_review.md"]
    } else if has_blocker_kind(request, BlockerKind::Substantiveness) {
        vec!["worker/theorem_stating/05_after_substantiveness_review.md"]
    } else if has_blocker_kind(request, BlockerKind::NodeCorr) {
        vec!["worker/theorem_stating/05_after_correspondence_review.md"]
    } else if has_blocker_kind(request, BlockerKind::Soundness) {
        vec!["worker/theorem_stating/05_after_soundness_review.md"]
    } else {
        vec!["worker/theorem_stating/05_frontier_work.md"]
    }
}

fn theorem_worker_first_request_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if request.id == 1 {
        vec!["worker/theorem_stating/12_first_request_dag_decomposition.md"]
    } else {
        Vec::new()
    }
}

fn proof_worker_scope_prompt_fragment(request: &WrapperRequest) -> &'static str {
    match request.worker_context.validation_kind {
        WorkerValidationKind::ProofRestructure => {
            "worker/proof_formalization/05_scope_restructure.md"
        }
        WorkerValidationKind::ProofCoarseRestructure => {
            "worker/proof_formalization/05_scope_coarse_restructure.md"
        }
        WorkerValidationKind::ProofEasy
        | WorkerValidationKind::ProofLocal
        | WorkerValidationKind::None
        | WorkerValidationKind::TheoremGlobal
        | WorkerValidationKind::TheoremTargeted
        | WorkerValidationKind::Cleanup
        | WorkerValidationKind::FinalCleanup => "worker/proof_formalization/05_scope_local.md",
    }
}

fn proof_worker_gate_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![
        if request.worker_context.allow_new_obligations {
            "worker/proof_formalization/06_gate_allow_new_obligations.md"
        } else {
            "worker/proof_formalization/06_gate_no_new_obligations.md"
        },
        if request.worker_context.must_close_active {
            "worker/proof_formalization/07_gate_must_close_active.md"
        } else {
            "worker/proof_formalization/07_gate_active_may_remain_open.md"
        },
    ];
    // Proposal v32: surface the active-coarse-anchor framing whenever
    // an anchor is set (which itself implies `coarse_dag_nodes` is
    // non-empty since the kernel only seeds the field then). The
    // fragment is identity-only when `coarse_repair_mode` is false;
    // it switches its framing in the repair-mode branch via the
    // prompt-side request fields.
    if request.active_coarse_node.is_some() {
        fragments.push("worker/proof_formalization/08_coarse_anchor.md");
    }
    fragments
}

fn proof_worker_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    // Substantiveness blockers in proof-formalization arise from
    // helper nodes added during proof-formalization. Include the same
    // substantiveness-after-review fragment as theorem-stating; the
    // rubric and remediation are phase-neutral.
    //
    // NodeCorr blockers in proof-formalization arise from helper nodes
    // whose Lean signature drifted from the TeX statement during helper
    // authoring. The proof-formalization-specific fragment frames repair
    // as helper-node alignment (rather than principal-statement repair,
    // which is the theorem-stating wording).
    //
    // Soundness blockers in proof-formalization are NL-soundness repair
    // tasks selected by the reviewer/kernel. Triggered on
    // `BlockerKind::Soundness` in `request.blockers` rather than on
    // editable scope, because authorized-nodes is broader than assigned
    // task intent. Substantiveness, correspondence, and soundness
    // fragments compose additively when multiple blockers are present.
    let mut fragments = Vec::new();
    if has_blocker_kind(request, BlockerKind::Substantiveness) {
        fragments.push("worker/theorem_stating/05_after_substantiveness_review.md");
    }
    if has_blocker_kind(request, BlockerKind::NodeCorr) {
        fragments.push("worker/proof_formalization/05_after_correspondence_review.md");
    }
    if has_blocker_kind(request, BlockerKind::Soundness) {
        fragments.push("worker/proof_formalization/05_after_soundness_review.md");
    }
    fragments.push(proof_worker_scope_prompt_fragment(request));
    fragments.extend(proof_worker_gate_prompt_fragments(request));
    fragments
}

fn worker_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    match request.worker_context.worker_profile {
        WorkerProfile::Theorem => theorem_worker_scenario_prompt_fragments(request),
        WorkerProfile::ProofEasy | WorkerProfile::ProofHard => {
            proof_worker_scenario_prompt_fragments(request)
        }
        WorkerProfile::Cleanup => vec!["worker/cleanup/05_orphan_cleanup_task.md"],
        // Cleanup-v2 Step 16: branch FinalCleanup task fragment on the
        // active task's kind. Substitution tasks see the substitution-
        // specific fragment (deletion + \noderef sweep + importer
        // rewrites); LintFix tasks see the lintfix fragment
        // (single-node scope). Legacy lint-only mode (no active task)
        // keeps the generic 05_task fragment.
        WorkerProfile::FinalCleanup => {
            match request
                .worker_context
                .cleanup_active_task_kind_view
                .as_ref()
            {
                Some(crate::model::CleanupTaskKind::Substitution { .. }) => {
                    vec![
                        "worker/final_cleanup/05_task.md",
                        "worker/final_cleanup/06_substitution_task.md",
                    ]
                }
                Some(crate::model::CleanupTaskKind::LintFix { .. }) => {
                    vec![
                        "worker/final_cleanup/05_task.md",
                        "worker/final_cleanup/06_lintfix_task.md",
                    ]
                }
                None => vec!["worker/final_cleanup/05_task.md"],
            }
        }
        WorkerProfile::None => vec!["worker/generic/05_task.md"],
    }
}

fn worker_profile_guidance_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    match request.worker_context.worker_profile {
        WorkerProfile::Theorem => {
            let mut fragments = vec!["worker/theorem_stating/10_mode_guidance.md"];
            fragments.extend(theorem_worker_first_request_prompt_fragments(request));
            fragments.extend([
                "worker/theorem_stating/15_initial_dag_size.md",
                "worker/theorem_stating/17_helper_policy.md",
                "worker/theorem_stating/20_common_failure_modes.md",
            ]);
            fragments
        }
        WorkerProfile::ProofEasy | WorkerProfile::ProofHard => vec![
            "worker/proof_formalization/10_operational_guidance.md",
            "worker/proof_formalization/15_failure_triage.md",
            "worker/proof_formalization/20_helper_decomposition.md",
        ],
        WorkerProfile::Cleanup | WorkerProfile::FinalCleanup | WorkerProfile::None => Vec::new(),
    }
}

fn cleanup_like_worker(request: &WrapperRequest) -> bool {
    matches!(
        request.worker_context.worker_profile,
        WorkerProfile::Cleanup | WorkerProfile::FinalCleanup
    )
}

fn worker_field_guidance_fragment(request: &WrapperRequest) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/37_field_guidance.md"
    } else {
        "worker/common/37_field_guidance.md"
    }
}

fn worker_reviewer_comments_fragment(request: &WrapperRequest) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/35_reviewer_comments.md"
    } else {
        "worker/common/35_reviewer_comments.md"
    }
}

fn worker_outcomes_fragment(request: &WrapperRequest) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/45_outcomes.md"
    } else {
        "worker/common/45_outcomes.md"
    }
}

fn worker_new_nodes_allowed(request: &WrapperRequest) -> bool {
    matches!(
        request.worker_context.validation_kind,
        WorkerValidationKind::TheoremGlobal
            | WorkerValidationKind::TheoremTargeted
            | WorkerValidationKind::ProofEasy
            | WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure
    )
}

fn worker_has_last_invalid_snapshot(request: &WrapperRequest) -> bool {
    // Mirror runtime::worker_response_should_preserve_attempt: include any
    // retry whose previous attempt left a last_invalid sidecar. After the
    // 2026-04-25 broadening, all four non-Valid outcomes (Invalid,
    // Malformed, Stuck, NeedsRestructure) have both a Tablet snapshot AND
    // metadata.json — the kernel rolls the worktree back unconditionally
    // and preserves the worker's WIP at the sidecar location.
    matches!(
        request.retry_outcome_kind,
        RetryOutcomeKind::Invalid | RetryOutcomeKind::Stuck | RetryOutcomeKind::NeedsRestructure
    )
}

/// Per the canonical-def inlining matrix
/// (memory: project_canonical_def_inlining_plan.md): inline the lane
/// def files that are relevant to what the worker can author under
/// its current validation_kind. Cleanup phases don't reopen tablet
/// structure and get an empty list. Helper-allowing
/// proof-formalization kinds get substantiveness + correspondence +
/// soundness (no faithfulness — paper-target coverage is locked once
/// theorem-stating clears). Difficulty is advisory; proof scope and
/// helper-obligation gates come from explicit reviewer controls.
/// TheoremGlobal/Targeted authors all four kinds of content, so all
/// four defs.
fn canonical_def_fragments_for_worker(request: &WrapperRequest) -> Vec<&'static str> {
    use crate::model::WorkerValidationKind::*;
    match request.worker_context.validation_kind {
        TheoremGlobal | TheoremTargeted => vec![
            "canonical/DEVIATIONS.md",
            "canonical/FAITHFULNESS.md",
            "canonical/SUBSTANTIVENESS.md",
            "canonical/CORRESPONDENCE.md",
            "canonical/SOUNDNESS.md",
        ],
        ProofEasy | ProofLocal | ProofRestructure | ProofCoarseRestructure => vec![
            "canonical/DEVIATIONS.md",
            "canonical/SUBSTANTIVENESS.md",
            "canonical/CORRESPONDENCE.md",
            "canonical/SOUNDNESS.md",
        ],
        Cleanup | FinalCleanup | None => vec![],
    }
}

/// Per the inlining matrix: reviewer scope follows phase. TheoremStating
/// reviewer adjudicates all four lanes; ProofFormalization reviewer
/// adjudicates substantiveness (helper nodes from Hard mode),
/// correspondence, and soundness. Cleanup is dormant.
fn canonical_def_fragments_for_reviewer(request: &WrapperRequest) -> Vec<&'static str> {
    match request.phase {
        Phase::TheoremStating => vec![
            "canonical/DEVIATIONS.md",
            "canonical/FAITHFULNESS.md",
            "canonical/SUBSTANTIVENESS.md",
            "canonical/CORRESPONDENCE.md",
            "canonical/SOUNDNESS.md",
        ],
        Phase::ProofFormalization => vec![
            "canonical/DEVIATIONS.md",
            "canonical/SUBSTANTIVENESS.md",
            "canonical/CORRESPONDENCE.md",
            "canonical/SOUNDNESS.md",
        ],
        Phase::Cleanup | Phase::Complete => vec![],
    }
}

fn worker_has_meaningful_routing_hints(request: &WrapperRequest) -> bool {
    // B7: render the routing-hints fragment only when at least one hint
    // is set to a non-default value. Defaults are: next_context_mode =
    // Resume, paper_focus_ranges = [], work_style_hint = None.
    !request.worker_context.paper_focus_ranges.is_empty()
        || request.worker_context.next_context_mode != crate::model::WorkerContextMode::Resume
        || request.worker_context.work_style_hint != crate::model::WorkerWorkStyleHint::None
}

fn prompt_json_value_meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn worker_has_stuck_math_reviewer_lean_product(request: &WrapperRequest) -> bool {
    request
        .stuck_math_audit
        .last_reviewer_lean_product
        .as_ref()
        .is_some_and(prompt_json_value_meaningful)
}

fn request_has_audit_plan(request: &WrapperRequest) -> bool {
    request.audit_plan.is_some()
}

fn request_has_need_input_audit_plan(request: &WrapperRequest) -> bool {
    request
        .audit_plan
        .as_ref()
        .is_some_and(|plan| plan.need_input_audit)
}

/// Option A: true when the historical-audit-plan-snapshot surface is
/// populated AND no live `audit_plan` is presented. Drives the
/// `29c_last_audit_plan.md` / `34d_last_audit_plan.md` prompt fragments
/// so the reviewer/worker reads the snapshot as advisory-only context.
fn request_has_only_snapshot_audit_plan(request: &WrapperRequest) -> bool {
    request.audit_plan.is_none() && request.previous_audit_plan_snapshot.is_some()
}

fn worker_post_initial_sketch_policy_applies(request: &WrapperRequest) -> bool {
    request.cycle > 1
        && matches!(
            request.worker_context.worker_profile,
            WorkerProfile::Theorem | WorkerProfile::ProofEasy | WorkerProfile::ProofHard
        )
}

fn worker_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![
        scheme_fragment_path(request_uses_full_scheme(request)),
        worker_intro_fragment(request),
    ];
    fragments.extend(worker_scenario_prompt_fragments(request));
    fragments.extend([
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        "worker/common/15_loogle.md",
        "worker/common/17_mathlib.md",
        "worker/common/18_reference_paper.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "worker/common/20_authority.md",
        "worker/common/21_deviations.md",
        "worker/common/30_request.md",
    ]);
    if worker_has_meaningful_routing_hints(request) {
        fragments.push("worker/common/33_routing_hints.md");
    }
    fragments.extend([
        worker_reviewer_comments_fragment(request),
        "worker/common/36_recent_burst_history.md",
        worker_field_guidance_fragment(request),
    ]);
    if worker_post_initial_sketch_policy_applies(request) {
        fragments.push("worker/common/39_post_initial_sketch_policy.md");
    }
    fragments.extend([
        "worker/common/40_contract.md",
        worker_outcomes_fragment(request),
        "worker/common/50_acceptance.md",
        "shared/90_artifact_delivery.md",
        "worker/common/95_gate_authority.md",
    ]);
    fragments.splice(12..12, worker_profile_guidance_prompt_fragments(request));
    if worker_new_nodes_allowed(request) {
        fragments.insert(12, "worker/common/38_new_node_difficulty.md");
    }
    if has_review_verifier_evidence(request) {
        fragments.insert(12, "worker/common/34_verifier_evidence.md");
    }
    if worker_has_stuck_math_reviewer_lean_product(request) {
        fragments.insert(12, "worker/common/34b_stuck_math_reviewer_lean_product.md");
    }
    if request_has_audit_plan(request) {
        let fragment = if request_has_need_input_audit_plan(request) {
            "worker/common/34c_need_input_audit_plan.md"
        } else {
            "worker/common/34c_audit_plan.md"
        };
        fragments.insert(12, fragment);
    }
    if request_has_only_snapshot_audit_plan(request) {
        fragments.insert(12, "worker/common/34d_last_audit_plan.md");
    }
    if has_deterministic_worker_rejection_reasons(request) {
        fragments.insert(12, "worker/common/32_deterministic_worker_rejection.md");
    }
    if worker_has_last_invalid_snapshot(request) {
        fragments.insert(12, "worker/common/31_last_invalid.md");
    }
    fragments.insert(12, "worker/common/31_scratchpad.md");
    fragments.extend(canonical_def_fragments_for_worker(request));
    // Paper-grounding fragment: appended last so its presence does
    // not shift the hardcoded position-12 splice/insert targets above.
    // The fragment self-collapses to empty when the reviewer attached
    // no `paper_focus_ranges` (the bridge's `_paper_focus_fragments_block`
    // returns "" in that case), so unconditional inclusion is safe.
    fragments.push("worker/common/19_paper_focus_fragments.md");
    fragments.push(STRUCTURED_REQUEST_POINTER_FRAGMENT);
    fragments
}

fn review_primary_scenario_prompt_fragment(request: &WrapperRequest) -> &'static str {
    if request.post_advance_routing {
        return "review/common/05_post_advance_routing.md";
    }
    match request.retry_outcome_kind {
        RetryOutcomeKind::Invalid => "review/common/05_after_worker_invalid.md",
        RetryOutcomeKind::Stuck => "review/common/05_after_worker_stuck.md",
        RetryOutcomeKind::NeedsRestructure => "review/common/05_after_worker_needs_restructure.md",
        // Bug X principled fix: a transport-failure escalation reaches the
        // reviewer when the bridge could not get any meaningful output from
        // the worker after `transport_invalid_review_threshold` retries.
        // Reuse the after-invalid fragment for now — the reviewer's
        // adjudication options (continue, give up, advance phase) are the
        // same as for an invalid worker; the comments will explain it was a
        // transport failure rather than bad work.
        RetryOutcomeKind::Transport => "review/common/05_after_worker_invalid.md",
        RetryOutcomeKind::None => {
            if has_blocker_kind(request, BlockerKind::PaperFaithfulness) {
                if paper_review_is_split(request) {
                    "review/common/05_after_split_paper_faithfulness.md"
                } else {
                    "review/common/05_after_failed_paper_faithfulness.md"
                }
            } else if has_blocker_kind(request, BlockerKind::Deviation) {
                "review/common/05_after_failed_deviation.md"
            } else if has_blocker_kind(request, BlockerKind::Substantiveness) {
                "review/common/05_after_failed_substantiveness.md"
            } else if has_blocker_kind(request, BlockerKind::NodeCorr) {
                if corr_review_is_split(request) {
                    "review/common/05_after_split_correspondence.md"
                } else {
                    "review/common/05_after_failed_correspondence.md"
                }
            } else if has_blocker_kind(request, BlockerKind::Soundness) {
                if sound_review_is_split(request) {
                    "review/common/05_after_split_soundness.md"
                } else {
                    "review/common/05_after_failed_soundness.md"
                }
            } else {
                "review/common/05_after_clean_verification.md"
            }
        }
    }
}

fn review_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![review_primary_scenario_prompt_fragment(request)];
    if request.human_input_outstanding {
        fragments.push("review/common/06_with_outstanding_human_input.md");
    }
    fragments
}

fn reviewer_source_recourse_available() -> bool {
    #[cfg(test)]
    if let Some(value) = *SOURCE_RECOURSE_AVAILABLE_OVERRIDE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
    {
        return value;
    }
    // Defense-in-depth: the reviewer can consult a read-only snapshot of
    // the trellis source tree when process semantics seem to block
    // progress. The snapshot is materialized by `scripts/trellis.sh` at
    // run startup; if both env vars are set, we add the
    // `05_source_recourse.md` fragment (and the Python bridge populates
    // matching context keys). If either is unset (no snapshot this run),
    // we silently omit the fragment — no broken-template artifact.
    let snapshot_set = std::env::var("TRELLIS_REVIEWER_SOURCE_SNAPSHOT")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let sha_set = std::env::var("TRELLIS_REVIEWER_SOURCE_SHA")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    snapshot_set && sha_set
}

#[cfg(test)]
static SOURCE_RECOURSE_AVAILABLE_OVERRIDE: std::sync::Mutex<Option<bool>> =
    std::sync::Mutex::new(None);

fn review_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    // B1: target-orientation fragment is phase-conditional. TheoremStating
    // gets the Global-authorizes-everything variant; ProofFormalization
    // gets the Restructure/CoarseRestructure variant. Cleanup/Complete
    // omit the fragment (no relevant levers to discuss).
    let target_orientation_fragment = match request.phase {
        Phase::TheoremStating => Some("review/common/33b_theorem_target_orientation.md"),
        Phase::ProofFormalization => Some("review/common/33b_proof_target_orientation.md"),
        Phase::Cleanup | Phase::Complete => None,
    };
    let mut fragments = vec![
        scheme_fragment_path(request_uses_full_scheme(request)),
        "review/common/00_intro.md",
    ];
    fragments.extend(review_scenario_prompt_fragments(request));
    fragments.extend([
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "review/common/10_request.md",
        "review/common/12_deterministic_worker_rejection.md",
        "review/common/20_blocker_choices.md",
        "review/common/25_verifier_reasoning.md",
        "review/common/26_recent_burst_history.md",
        "review/common/27_reference_paper.md",
        "review/common/28_scratchpad.md",
        "review/common/30_contract.md",
        "review/common/30a_blocker_actions.md",
        "review/common/31_need_input.md",
        "review/common/32_revert.md",
    ]);
    // `32a_revert_last_clean.md` carves out the `reset = last_clean`
    // threshold mandate from the always-shown `32_revert.md`. The kernel's
    // `allowed_resets` gate (`has_ever_been_clean && last_clean_mirrors_populated()`
    // in `model.rs`) never includes `last_clean` during TheoremStating, so
    // showing the mandate language there contradicts the contract every
    // burst. Gate by phase.
    if request.phase != Phase::TheoremStating {
        fragments.push("review/common/32a_revert_last_clean.md");
    }
    fragments.extend(["review/common/33_routing_hints.md"]);
    if !request.latest_review_rejection_reasons.is_empty() {
        fragments.insert(10, "review/common/13_review_response_rejection.md");
    }
    if let Some(fragment) = target_orientation_fragment {
        fragments.push(fragment);
    }
    if request.phase == Phase::TheoremStating {
        fragments.push("review/common/33c_theorem_helper_policy.md");
    }
    fragments.extend([
        "review/common/34_worker_context_strategy.md",
        "review/common/35_comments.md",
    ]);
    if request.phase == Phase::ProofFormalization {
        fragments.push("review/common/36_authorized_nodes.md");
    }
    // B5: include the early-build-out 15-50 proof-bearing-nodes guidance
    // only during theorem-stating with no held target. Once a target is
    // held, the DAG size advice is not the right framing.
    if request.phase == Phase::TheoremStating && request.held_target.is_none() {
        fragments.push("review/common/35b_initial_dag_size_comment.md");
    }
    fragments.extend([
        "review/common/38_paper_focus_strategy.md",
        "review/common/39_revert_strategy.md",
        "review/common/40_authority.md",
        "shared/90_artifact_delivery.md",
    ]);
    if request.phase == Phase::ProofFormalization {
        fragments.insert(19, "review/common/37_restructure_strategy.md");
    }
    // Proposal v32: surface the active-coarse-anchor framing during
    // ProofFormalization Review, both when an anchor is locked (the
    // common case) and when the lock is open (kernel hints non-empty).
    // The fragment text covers both branches via the surfaced request
    // fields. Skipped when the mechanism is dormant (coarse DAG empty),
    // since the fragment would mislead the reviewer about state that
    // doesn't exist.
    if request.phase == Phase::ProofFormalization && !request.coarse_dag_nodes.is_empty() {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30a_blocker_actions.md")
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/30b_coarse_anchor.md");
    }
    if request.phase == Phase::Cleanup {
        // Mirror of the worker's `final_cleanup/05_task.md` aimed at the
        // reviewer, plus the explicit "declare done when no more
        // meaningful cleanup is happening" rule.
        fragments.insert(2, "review/common/05_cleanup_phase.md");
    }
    if reviewer_source_recourse_available() {
        // Recourse fragment lives near the other `05_*` after-context
        // fragments. We append at the end of the leading scenario block
        // (after any `05_cleanup_phase.md` insertion above) so its
        // ordering stays stable regardless of phase.
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "shared/10_repository_root.md")
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/05_source_recourse.md");
    }
    if request.stuck_math_audit.active {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        let fragment = if request_has_need_input_audit_plan(request) {
            "review/common/29_need_input_auditor.md"
        } else {
            "review/common/29_stuck_math_audit.md"
        };
        fragments.insert(insert_at, fragment);
    }
    if request_has_audit_plan(request) {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        let fragment = if request_has_need_input_audit_plan(request) {
            "review/common/29b_need_input_audit_plan.md"
        } else {
            "review/common/29b_audit_plan.md"
        };
        fragments.insert(insert_at, fragment);
    }
    if request_has_only_snapshot_audit_plan(request) {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/29c_last_audit_plan.md");
    }
    fragments.extend(canonical_def_fragments_for_reviewer(request));
    fragments.push(STRUCTURED_REQUEST_POINTER_FRAGMENT);
    fragments
}

fn checker_command_template(parts: &[&str]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|part| Value::String((*part).to_owned()))
            .collect(),
    )
}

fn artifact_prompt_view_with_commands(json_parts: &[&str], acceptance_parts: &[&str]) -> Value {
    let mut value = artifact_prompt_view_payload();
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "json_check_command_template".to_owned(),
            checker_command_template(json_parts),
        );
        // Trim 13: emit `null` for the acceptance-check command when no
        // parts are supplied (corr/paper/sound contracts call this with
        // an empty slice). The bridge-side null-drop helper then strips
        // the `"acceptance_check_command_template": null` line so the
        // verifier prompt does not include an empty-array placeholder.
        let acceptance_value = if acceptance_parts.is_empty() {
            Value::Null
        } else {
            checker_command_template(acceptance_parts)
        };
        map.insert(
            "acceptance_check_command_template".to_owned(),
            acceptance_value,
        );
    }
    value
}

fn preamble_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if !request.verify_nodes.contains("Preamble") {
        return json!({
            "mode": "none",
            "item_ids": [],
            "empty_items_vacuously_supported": true,
        });
    }
    let items = repo_path
        .map(|repo| repo.join("Tablet").join("Preamble.tex"))
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|content| extract_tex_statement_items(&content, true))
        .unwrap_or_default();
    let item_ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    json!({
        "mode": "one_way_support",
        "item_ids": item_ids,
        "empty_items_vacuously_supported": true,
    })
}

pub fn project_invariants_payload() -> Value {
    json!({
        "node_pair_contract": "every_present_node_has_lean_and_nl_statement",
        "proof_bearing_contract": "proof_nodes_need_closed_lean_or_rigorous_nl",
        "node_file_contract": "tablet_node_files_must_follow_filespec",
        "filespec_reference": "FILESPEC.md",
        "progress_modes": [
            "close_proof",
            "paper_faithful_dag_improvement",
        ],
        "role_authority": {
            "worker": "writes_repository_content_only",
            "reviewer": "chooses_next_step_and_guidance",
            "verifier": "checks_invariants_without_choosing_work",
        }
    })
}

fn no_paper_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "targets": [],
            "blocked_targets": [],
        },
        "target_covering_nodes": {},
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "none",
        "fixed_item_reporting_policy": "none",
        "target_issue_scope": [],
        "rubric": {
            "paper_statement_authority": "none",
            "covering_set_authority": "none",
            "definition_dependency_authority": "none",
            "faithfulness_standard": "none",
        },
        "artifact_contract": {
            "result_type": "paper_faithfulness_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "paper_faithfulness": {"decision": "PASS or FAIL", "issues": []},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
            "phase_blocks": {
                "paper_faithfulness": {
                    "decision_values": [],
                    "issue_subject_kind": "none",
                },
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_corr_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "nodes": [],
            "blocked_targets": [],
        },
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "none",
        "fixed_item_reporting_policy": "none",
        "node_issue_scope": [],
        "rubric": {
            "statement_alignment_checks": [],
            "project_definition_policy": "none",
            "definition_hygiene": [],
            "duplicate_mathlib_definition_policy": "none",
            "preamble_item_issue_policy": "none",
        },
        "artifact_contract": {
            "result_type": "correspondence_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "correspondence": {"decision": "PASS or FAIL", "verdicts": []},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
            "phase_blocks": {
                "correspondence": {
                    "decision_values": [],
                    "verdict_values": [],
                    "comment_required_on_fail": true,
                },
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
        "preamble_contract": {
            "mode": "none",
            "item_ids": [],
            "empty_items_vacuously_supported": true,
        },
    })
}

fn no_sound_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "node": "",
            "active_node": "",
            "held_target": "",
        },
        "previous_own_findings": {},
        "target_nodes": [],
        "evaluation_basis": "none",
        "detail_floor": "none",
        "rubric": {
            "proof_standard": "none",
            "reject_sketches": false,
            "detail_floor": "none",
            "lean_code_relevance": "none",
        },
        "artifact_contract": {
            "result_type": "soundness_result_v1",
            "decision_values": [],
            "overall_rule": "approve_iff_sound",
            "prompt_schema_example": {
                "node": "",
                "soundness": {"decision": "", "explanation": ""},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_worker_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "mode": "",
            "active_node": "",
            "held_target": "",
            "worker_context": {
                "enabled": false,
                "active_difficulty": "hard",
                "active_easy_attempts": 0,
                "worker_profile": "none",
                "validation_kind": "none",
                "authorized_nodes": [],
                "allow_new_obligations": true,
                "must_close_active": false,
            },
            "blockers": [],
            "current_present_nodes": [],
            "current_definition_nodes": [],
            "current_preamble_nodes": [],
            "current_deps_scoped": {},
            "current_deps_scope_nodes": [],
            "current_target_claims_nonempty": {},
        },
        "reviewer_comments": "",
        "result_type": "worker_result_v1",
        "kernel_derives_structural_snapshot": true,
        "allowed_outcomes": [],
        "reported_delta_fields": [],
        "prompt_schema_example": {
            "outcome": "",
            "summary": "",
            "comments": "",
            "target_claim_updates": {"node_id": []},
            "difficulty_updates": {"node_id": ""},
        },
        "scope_contract": {
            "existing_node_scope_mode": "none",
            "authorized_existing_nodes": [],
            "configured_targets": [],
            "pending_targets": [],
            "pending_targets_meaning": "none",
            "new_nodes_allowed": false,
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        "stuck_contract": {
            "allowed": false,
            "forbid_tablet_changes_when_stuck": false,
            "meaning": "none",
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_review_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "mode": "",
            "active_node": "",
            "held_target": "",
            "invalid_attempt": false,
            "human_input_outstanding": false,
            "blocked_targets": [],
            "protected_nodes": [],
            "latest_worker_rationale": {
                "summary": "",
                "comments": "",
            },
        },
        "artifact_contract": {
            "result_type": "review_result_v1",
            "required_fields": [],
            "optional_fields": [],
            "prompt_schema_example": {
                "decision": [],
                "reason": "",
                "comments": "",
                "task_blocker_ids": [],
                "reset_blocker_ids": [],
                "request_sound_verifier_node_ids": [],
                "next_active": "",
                "next_mode": [],
                "reset": [],
                "difficulty_updates": {"node_id": ""},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": "",
            },
        },
        "verifier_evidence": {
            "paper": {},
            "corr": {},
            "sound": {},
        },
        "blocker_actions": {
            "required": false,
            "action_fields": [],
            "choices": [],
            "allowed_reset_ids": [],
            "sound_verifier_requestable_nodes": [],
            "reset_semantics": "none",
        },
        "blocker_partition": {
            "required": false,
            "action_fields": [],
            "choices": [],
            "allowed_reset_ids": [],
            "sound_verifier_requestable_nodes": [],
            "reset_semantics": "none",
        },
        "need_input_contract": {
            "meaning": "none",
            "blocker_partition_required": false,
            "task_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "",
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        "next_active_contract": {
            "kernel_hinted_nodes": [],
            "targeted_allowed_nodes": [],
            "allow_targeted_without_next_active": false,
        },
        "difficulty_update_contract": {
            "allowed_nodes": [],
        },
        "clear_human_input_contract": {
            "allowed_when_outstanding": false,
            "omit_when_not_allowed": true,
        },
        "comments_contract": {
            "field": "comments",
            "semantics": "non_authoritative_guidance_forwarded_to_future_workers",
            "empty_string_means_no_comments": true,
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

pub fn correspondence_contract_payload(
    request: &WrapperRequest,
    repo_path: Option<&Path>,
) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Corr) {
        return no_corr_contract_payload();
    }
    // A9: drop kernel housekeeping fields (prompt_fragments,
    // artifact_prompt_view, issue/fixed_item_reporting_policy) from the
    // verifier-rendered contract via `_prompt_facing_corr_contract` on the
    // Python side, but they remain on the kernel-side contract for the
    // bridge's lane-scoping helpers. Below we still emit the kernel
    // structure; trimming for verifier prompts happens in bridge_prompts.
    // A10: drop duplicate `node_issue_scope` (redundant with
    // request_summary.nodes) — still emit at kernel level for legacy
    // consumers but the prompt-facing contract drops it.
    // A11: omit `preamble_contract` when Preamble is not in verify_nodes.
    let mut contract = serde_json::Map::new();
    contract.insert(
        "prompt_fragments".to_owned(),
        json!(correspondence_prompt_fragments(request)),
    );
    contract.insert(
        "request_summary".to_owned(),
        json!({
            "phase": request.phase,
            "nodes": request.verify_nodes,
            "blocked_targets": request.blocked_targets,
        }),
    );
    contract.insert(
        "previous_own_findings_by_lane".to_owned(),
        json!(request.previous_corr_lane_findings),
    );
    contract.insert(
        "issue_reporting_policy".to_owned(),
        json!("explicit_per_node_verdicts"),
    );
    contract.insert(
        "fixed_item_reporting_policy".to_owned(),
        json!("summary_only"),
    );
    contract.insert("node_issue_scope".to_owned(), json!(request.verify_nodes));
    contract.insert(
        "rubric".to_owned(),
        json!({
            "statement_alignment_checks": [
                "quantifier_scope",
                "type_constraints",
                "implicit_assumptions",
                "domain_context",
            ],
            "project_definition_policy": "expand_project_definitions_but_trust_mathlib",
            "definition_hygiene": [
                "reject_opaque",
                "reject_axiom",
                "reject_constant",
                "reject_sorry_in_definition",
            ],
            "duplicate_mathlib_definition_policy": "reject_project_duplicates",
            "preamble_item_issue_policy": "use_exact_item_id",
        }),
    );
    contract.insert(
        "artifact_contract".to_owned(),
        json!({
            "result_type": "correspondence_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "correspondence": {
                    "decision": "PASS or FAIL",
                    "verdicts": [
                        {"node": "node_id", "verdict": "Pass"},
                        {"node": "node_id", "verdict": "Fail",
                         "comment": "concrete recommendation for the worker"},
                    ],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "correspondence": {
                    "decision_values": ["PASS", "FAIL"],
                    "verdict_values": ["Pass", "Fail"],
                    "comment_required_on_fail": true,
                }
            }
        }),
    );
    contract.insert(
        "artifact_prompt_view".to_owned(),
        artifact_prompt_view_with_commands(
            &[
                "python3",
                "{{check_script_path}}",
                "correspondence-result",
                "{{raw_output_path}}",
            ],
            &[],
        ),
    );
    if request.verify_nodes.contains("Preamble") {
        contract.insert(
            "preamble_contract".to_owned(),
            preamble_contract_payload(request, repo_path),
        );
    }
    Value::Object(contract)
}

pub fn paper_contract_payload(request: &WrapperRequest) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Paper) {
        return no_paper_contract_payload();
    }
    let is_per_node_scenario =
        !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty();
    if let Some(deviation_id) = request.deviation_verify_id.as_ref() {
        let mut payload = json!({
            "prompt_fragments": paper_prompt_fragments(request),
            "request_summary": {
                "phase": request.phase,
                "scenario": "deviation_authorization",
                "deviation_id": deviation_id,
                "deviation_path": request.deviation_verify_path,
            },
            // Always emit (paper_verify_targets is empty in this scenario,
            // so the map is empty); kernel-side guarantee lets the bridge
            // consume the field unconditionally without a fallback shim.
            "target_covering_nodes": paper_target_covering_nodes(request),
            "deviation": {
                "id": deviation_id,
                "path": request.deviation_verify_path,
            },
            "rubric": {
                "rubric_reference": "DEVIATIONS.md",
                "verdict": "pass_iff_deviation_is_tex_only_explicit_and_has_a_rigorous_return_to_paper_faithful_steps",
            },
            "artifact_contract": {
                "result_type": "deviation_authorization_result_v1",
                "overall_rule": "approve_iff_pass",
                "prompt_schema_example": {
                    "deviation_authorization": {
                        "id": deviation_id,
                        "decision": "PASS or FAIL",
                        "comment": "required on FAIL",
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note",
                },
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(&[
                "python3",
                "{{check_script_path}}",
                "deviation-authorization-result",
                "{{raw_output_path}}",
            ], &[]),
        });
        let view = paper_prompt_facing_view(&payload);
        if let Some(map) = payload.as_object_mut() {
            map.insert("prompt_facing_view".to_string(), view);
        }
        return payload;
    }
    if is_per_node_scenario {
        // Substantiveness scenario. The verifier sees the
        // outstanding Unknown set and triages — Pass / Fail / NotDoneYet
        // per node, with each verdict carried explicitly in
        // `verdicts[]`. The kernel collects per-node evidence and
        // re-issues another Paper request for any NotDoneYet residual,
        // subject to a safety bound.
        let mut payload = json!({
            "prompt_fragments": paper_prompt_fragments(request),
            "request_summary": {
                "phase": request.phase,
                "scenario": "substantiveness",
                "nodes": request.substantiveness_verify_nodes,
                "blocked_targets": request.blocked_targets,
            },
            // Always emit (paper_verify_targets is empty in this scenario,
            // so the map is empty); kernel-side guarantee lets the bridge
            // consume the field unconditionally without a fallback shim.
            "target_covering_nodes": paper_target_covering_nodes(request),
            "node_paper_basis_inputs": substantiveness_basis_inputs(request),
            "authorized_deviations": request.authorized_deviations,
            "node_deviation_claims": request.node_deviation_claims,
            "previous_own_findings": request.previous_substantiveness_lane_findings,
            "issue_reporting_policy": "explicit_per_node_verdicts",
            "fixed_item_reporting_policy": "summary_only",
            "rubric": {
                "verdict": "pass_iff_valid_AND_meaningful_decomposition",
                "rubric_reference": "SUBSTANTIVENESS.md",
                "strengthening_allowed": true,
                "missing_node_default": "NotDoneYet",
                "triage_signal": "verdict: 'NotDoneYet' marks the node as not-yet-evaluated; missing nodes default to NotDoneYet",
            },
            "artifact_contract": {
                "result_type": "substantiveness_result_v1",
                "overall_rule": "approve_iff_pass",
                "prompt_schema_example": {
                    "substantiveness": {
                        "decision": "PASS or FAIL",
                        "verdicts": [
                            {"node": "node_id", "verdict": "Pass"},
                            {"node": "node_id", "verdict": "Fail", "comment": "concrete recommendation: strengthen / merge / remove / etc."},
                            {"node": "node_id", "verdict": "NotDoneYet"},
                            {"node": "node_id", "verdict": "NotDoneYet", "comment": "ran out of time on the case analysis"},
                        ],
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note",
                },
                "phase_blocks": {
                    "substantiveness": {
                        "decision_values": ["PASS", "FAIL"],
                        "verdict_values": ["Pass", "Fail", "NotDoneYet"],
                        "comment_required_on_fail": true,
                    }
                }
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(&[
                "python3",
                "{{check_script_path}}",
                "substantiveness-result",
                "{{raw_output_path}}",
            ], &[]),
        });
        let view = paper_prompt_facing_view(&payload);
        if let Some(map) = payload.as_object_mut() {
            map.insert("prompt_facing_view".to_string(), view);
        }
        return payload;
    }
    let mut payload = json!({
        "prompt_fragments": paper_prompt_fragments(request),
        "request_summary": {
            "phase": request.phase,
            "scenario": "target_package",
            "targets": request.paper_verify_targets,
            "blocked_targets": request.blocked_targets,
        },
        "target_covering_nodes": paper_target_covering_nodes(request),
        "previous_own_findings_by_lane": request.previous_paper_lane_findings,
        "issue_reporting_policy": "current_failures_only",
        "fixed_item_reporting_policy": "summary_only",
        "rubric": {
            "paper_statement_authority": "configured_target_ids_label_first",
            "covering_set_authority": "covering_nodes_for_target_claim",
            "definition_dependency_authority": "statement_level_definition_hashes_only",
            "faithfulness_standard": "covering_tex_statements_collectively_capture_target_statement",
        },
        "artifact_contract": {
            "result_type": "paper_faithfulness_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "paper_faithfulness": {
                    "decision": "PASS or FAIL",
                    "issues": [{"node": "target_id", "description": "..."}],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "paper_faithfulness": {
                    "decision_values": ["PASS", "FAIL"],
                    "issue_subject_kind": "target",
                }
            }
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "paper-faithfulness-result",
            "{{raw_output_path}}",
        ], &[]),
    });
    let view = paper_prompt_facing_view(&payload);
    if let Some(map) = payload.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), view);
    }
    payload
}

/// Per-node paper-basis inputs surfaced in the contract for the per-node
/// scenario. For each node on the frontier we provide:
///   - `tex_path`: path to the node's `.tex` file under `Tablet/`.
///   - `lean_path`: path to the node's `.lean` file under `Tablet/`.
///   - `imported_by`: nodes that import this one (target reverse-deps),
///     so the verifier can judge how downstream usability is affected by
///     a weakened statement.
///   - `node_kind`: preamble / definition / proof.
fn substantiveness_basis_inputs(request: &WrapperRequest) -> Value {
    let mut by_node = serde_json::Map::new();
    for node in &request.substantiveness_verify_nodes {
        let imported_by: BTreeSet<NodeId> = request
            .current_deps
            .iter()
            .filter_map(|(parent, children)| {
                if children.contains(node) {
                    Some(parent.clone())
                } else {
                    None
                }
            })
            .collect();
        let node_kind = request
            .current_node_kinds
            .get(node)
            .copied()
            .unwrap_or_default();
        by_node.insert(
            node.as_str().to_string(),
            json!({
                "tex_path": format!("Tablet/{}.tex", node.as_str()),
                "lean_path": format!("Tablet/{}.lean", node.as_str()),
                "imported_by": imported_by,
                "node_kind": node_kind,
            }),
        );
    }
    Value::Object(by_node)
}

pub fn soundness_contract_payload(request: &WrapperRequest) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Sound) {
        return no_sound_contract_payload();
    }
    let reverification_context = request
        .sound_reverification_context
        .as_ref()
        .map(|ctx| {
            json!({
                "target": ctx.target,
                "prior_status": ctx.prior_status,
                "current_status": ctx.current_status,
                "own_tex_changed": ctx.own_tex_changed,
                "deps_changed": ctx.deps_changed,
                "prior_lane_evidence": ctx.prior_lane_evidence,
                "git_access_hint": "The repository (including .git) is mounted read-only inside this sandbox. You may inspect prior content with `git -C <repo_path> show <cycle-tag>:Tablet/<Dep>.tex` or `git -C <repo_path> log -- Tablet/<Dep>.tex`. Tags of the form `cycle-N` exist for every committed cycle.",
            })
        })
        .unwrap_or(Value::Null);
    json!({
        "prompt_fragments": soundness_prompt_fragments(request),
        "request_summary": {
            "phase": request.phase,
            "node": request.sound_verify_node,
            "active_node": request.active_node,
            "held_target": request.held_target,
        },
        "previous_own_findings": request.previous_sound_lane_findings,
        "reverification_context": reverification_context,
        "target_nodes": request.sound_verify_nodes,
        "evaluation_basis": "nl_only",
        "detail_floor": "paper_floor",
        "rubric": {
            "proof_standard": "line_by_line_rigorous",
            "reject_sketches": true,
            "detail_floor": "paper_floor",
            "lean_code_relevance": "ignore_lean_check_nl_only",
            "dependency_citation_rule": "every_cross_node_nl_dependency_must_use_noderef",
            "dependency_citation_syntax": "\\noderef{NodeName}",
        },
        "artifact_contract": {
            "result_type": "soundness_result_v1",
            "decision_values": ["SOUND", "UNSOUND", "STRUCTURAL"],
            "overall_rule": "approve_iff_sound",
            "prompt_schema_example": {
                "node": "target_node",
                "soundness": {
                    "decision": "SOUND, UNSOUND, or STRUCTURAL",
                    "explanation": "brief explanation",
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
        }
        ,
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "soundness-result",
            "{{raw_output_path}}",
            "--node",
            "{{node_name}}",
        ], &[
        ]),
    })
}

/// Worker-prompt blocker-status block, rendered by the kernel and spliced
/// into the worker prompt by the bridge.
///
/// `md` is the Markdown body. When the live blocker count overflows the
/// inline limit, the body contains the literal placeholder `{sidecar_path}`
/// (three-char prefix + suffix) that the bridge substitutes with the
/// concrete sidecar path on disk before splicing. `sidecar_payload` carries
/// the structured payload the bridge writes to that sidecar; `None` means
/// inline-only and no sidecar I/O is needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerBlockerStatusBlock {
    pub md: String,
    pub sidecar_payload: Option<Value>,
}

/// Default inline limit for the worker blocker-status table.
///
/// Mirrors the bridge's `BLOCKER_INLINE_LIMIT_DEFAULT`. When the
/// `TRELLIS_BLOCKER_INLINE_LIMIT` env var is set (and parseable), it
/// overrides this default.
pub const WORKER_BLOCKER_INLINE_LIMIT_DEFAULT: usize = 8;
/// Fallback K when the actionable filter returns nothing; first K blockers
/// by stable alphabetical label order are shown.
pub const WORKER_BLOCKER_ACTIONABLE_FALLBACK_K: usize = 5;
/// Env var that overrides the inline limit at runtime.
pub const WORKER_BLOCKER_INLINE_LIMIT_ENV: &str = "TRELLIS_BLOCKER_INLINE_LIMIT";
/// Literal placeholder inside `WorkerBlockerStatusBlock::md` that the
/// bridge replaces with the concrete on-disk sidecar path before splicing.
pub const WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER: &str = "{sidecar_path}";

fn worker_blocker_inline_limit() -> usize {
    if let Ok(raw) = std::env::var(WORKER_BLOCKER_INLINE_LIMIT_ENV) {
        if let Ok(value) = raw.trim().parse::<usize>() {
            return value;
        }
    }
    WORKER_BLOCKER_INLINE_LIMIT_DEFAULT
}

/// Snake-case JSON tag for a `BlockerKind` variant (matches Serde's default
/// derive output: variant name as-is, e.g. "PaperFaithfulness").
fn blocker_kind_str(kind: BlockerKind) -> &'static str {
    match kind {
        BlockerKind::PaperFaithfulness => "PaperFaithfulness",
        BlockerKind::Deviation => "Deviation",
        BlockerKind::NodeCorr => "NodeCorr",
        BlockerKind::Soundness => "Soundness",
        BlockerKind::Substantiveness => "Substantiveness",
    }
}

/// "otype:body" label used in worker-facing blocker rows.
fn worker_blocker_object_label(blocker: &Blocker) -> String {
    match &blocker.object {
        BlockerObject::Node { node } => format!("node:{}", node.as_str()),
        BlockerObject::Target { target } => format!("target:{}", target.as_str()),
        BlockerObject::Deviation { deviation } => format!("deviation:{}", deviation.as_str()),
    }
}

/// Worker-row format `"%5d | %-16s | %s"`, mirroring the bridge formatter.
fn worker_blocker_format_row(index: usize, blocker: &Blocker) -> String {
    let kind = blocker_kind_str(blocker.kind);
    let label = worker_blocker_object_label(blocker);
    format!("{:5} | {:16} | {}", index, kind, label)
}

/// "k1=n1, k2=n2, ..." counts-by-kind line, sorted by kind label.
fn worker_blocker_kind_counts_line(blockers: &[&Blocker]) -> String {
    if blockers.is_empty() {
        return "(none)".to_owned();
    }
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for b in blockers {
        *counts.entry(blocker_kind_str(b.kind)).or_insert(0) += 1;
    }
    let mut parts = Vec::with_capacity(counts.len());
    for (k, v) in counts.iter() {
        parts.push(format!("{}={}", k, v));
    }
    parts.join(", ")
}

/// Select actionable blocker indices using the same heuristic as the
/// bridge's `_select_actionable_blocker_indices`. Returns
/// `(indices, reason_phrase)` where the reason phrase is rendered inline.
///
/// A blocker is actionable when its node referent lives in
/// `{active_node} ∪ deps_neighborhood`, or its target referent equals
/// `held_target`. On empty actionable set we fall back to first K by
/// stable alphabetical label order (matching the Python's tuple sort).
fn worker_blocker_select_actionable(
    blockers: &[&Blocker],
    active_node: Option<&str>,
    held_target: Option<&str>,
    deps_neighborhood: &BTreeSet<String>,
) -> (Vec<usize>, String) {
    let mut neighborhood: BTreeSet<&str> = BTreeSet::new();
    if let Some(n) = active_node {
        neighborhood.insert(n);
    }
    for n in deps_neighborhood {
        neighborhood.insert(n.as_str());
    }
    let target_focus = held_target;

    let mut matched: Vec<usize> = Vec::new();
    for (index, blocker) in blockers.iter().enumerate() {
        match &blocker.object {
            BlockerObject::Node { node } => {
                if neighborhood.contains(node.as_str()) {
                    matched.push(index);
                }
            }
            BlockerObject::Target { target } => {
                if let Some(t) = target_focus {
                    if target.as_str() == t {
                        matched.push(index);
                    }
                }
            }
            BlockerObject::Deviation { .. } => {}
        }
    }

    if !matched.is_empty() {
        return (matched, "active node + direct-dep neighborhood".to_owned());
    }

    let fallback_count = std::cmp::min(WORKER_BLOCKER_ACTIONABLE_FALLBACK_K, blockers.len());
    if fallback_count == 0 {
        return (Vec::new(), "no live blockers".to_owned());
    }
    let mut sortable: Vec<(String, usize)> = blockers
        .iter()
        .enumerate()
        .map(|(i, b)| (worker_blocker_object_label(b), i))
        .collect();
    sortable.sort();
    let indices: Vec<usize> = sortable
        .into_iter()
        .take(fallback_count)
        .map(|(_, i)| i)
        .collect();
    let note = format!(
        "fallback: no blockers touch active_node/held_target; showing \
first {} of {} by label order",
        fallback_count,
        blockers.len()
    );
    (indices, note)
}

/// Format the actionable-subset table (worker-facing, no `id` column).
fn worker_blocker_format_actionable_table(
    indices: &[usize],
    blockers: &[&Blocker],
    note: &str,
) -> String {
    if indices.is_empty() {
        return format!("(none) -- {}", note);
    }
    let rows: Vec<String> = indices
        .iter()
        .filter_map(|&i| blockers.get(i).map(|b| worker_blocker_format_row(i, b)))
        .collect();
    let header_lines = [
        "Index | Kind             | Object".to_owned(),
        "------|------------------|".to_owned() + &"-".repeat(32),
    ];
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Actionable subset ({} of {}): {}",
        rows.len(),
        blockers.len(),
        note
    );
    out.push('\n');
    for line in header_lines.iter() {
        out.push_str(line);
        out.push('\n');
    }
    for (i, row) in rows.iter().enumerate() {
        out.push_str(row);
        if i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Compute the bridge's `deps_neighborhood`: direct out-edges of
/// `active_node` plus reverse-edges (consumers of `active_node`),
/// projected through the worker-prompt DAG scope. Matches the bridge's
/// algorithm in `bridge_prompts.py`.
fn worker_blocker_deps_neighborhood(request: &WrapperRequest) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let active = match request.active_node.as_ref() {
        Some(n) => n,
        None => return out,
    };
    let dag_scope = request.worker_prompt_dag_scope();
    // Direct out-edges of active_node (only when active_node itself is in
    // the scoped view).
    if dag_scope.contains(active) {
        if let Some(direct) = request.current_deps.get(active) {
            for n in direct {
                out.insert(n.as_str().to_owned());
            }
        }
    }
    // Reverse-edges: any node in scope listing active_node in its deps.
    for (node, deps) in request.current_deps.iter() {
        if !dag_scope.contains(node) {
            continue;
        }
        if deps.contains(active) {
            out.insert(node.as_str().to_owned());
        }
    }
    out
}

/// Render the worker-facing blocker-status block.
///
/// Byte-equivalent to the bridge's `_worker_blocker_status_block`. In the
/// overflow case, `md` contains the literal `{sidecar_path}` placeholder
/// (see `WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER`) that the bridge
/// substitutes with the concrete sidecar path before splicing.
pub fn worker_blocker_status_block(request: &WrapperRequest) -> WorkerBlockerStatusBlock {
    let blockers: Vec<&Blocker> = request.blockers.iter().collect();
    if blockers.is_empty() {
        return WorkerBlockerStatusBlock {
            md: "No live blockers.".to_owned(),
            sidecar_payload: None,
        };
    }
    let total = blockers.len();
    let counts_line = worker_blocker_kind_counts_line(&blockers);
    let deps_neighborhood = worker_blocker_deps_neighborhood(request);
    let active_node = request.active_node.as_ref().map(|n| n.as_str());
    let held_target = request.held_target.as_ref().map(|n| n.as_str());
    let (indices, note) =
        worker_blocker_select_actionable(&blockers, active_node, held_target, &deps_neighborhood);
    let actionable_table = worker_blocker_format_actionable_table(&indices, &blockers, &note);
    let header = format!(
        "{} live blocker(s). Counts by kind: {}. Reviewer comments above \
describe what to repair; this list shows the live verifier blockers for \
situational awareness.",
        total, counts_line
    );

    let inline_limit = worker_blocker_inline_limit();
    if total <= inline_limit {
        let rows: Vec<String> = blockers
            .iter()
            .enumerate()
            .map(|(i, b)| worker_blocker_format_row(i, b))
            .collect();
        let mut md = String::new();
        md.push_str(&header);
        md.push('\n');
        md.push('\n');
        md.push_str("Index | Kind             | Object");
        md.push('\n');
        md.push_str("------|------------------|");
        md.push_str(&"-".repeat(32));
        md.push('\n');
        for r in rows.iter() {
            md.push_str(r);
            md.push('\n');
        }
        md.push('\n');
        md.push_str(&actionable_table);
        return WorkerBlockerStatusBlock {
            md,
            sidecar_payload: None,
        };
    }

    // Overflow: emit the actionable subset inline with a `{sidecar_path}`
    // placeholder, and the structured payload the bridge writes to disk.
    let synth_choices: Vec<Value> = blockers
        .iter()
        .map(|b| {
            json!({
                "id": "(worker view: id only emitted in review contract)",
                "blocker": *b,
            })
        })
        .collect();
    let mut md = String::new();
    md.push_str(&header);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Live blocker count ({}) exceeds the inline limit ({}); full \
structured blocker list is on disk so the actionable subset stays visible \
inline.",
        total, inline_limit
    );
    md.push('\n');
    md.push('\n');
    md.push_str(&actionable_table);
    md.push('\n');
    md.push('\n');
    let _ = write!(md, "Full blocker list sidecar: `{}`", WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER);
    md.push('\n');
    md.push('\n');
    md.push_str(
        "Sidecar shape: `{\"blocker_choices\": [{\"id\": ..., \"blocker\": ...}, ...]}`. \
Workers do not echo blocker `id`s back; this file exists so you can inspect any \
blocker beyond the inline actionable subset if the reviewer's comments reference it.",
    );
    WorkerBlockerStatusBlock {
        md,
        sidecar_payload: Some(json!({"blocker_choices": synth_choices})),
    }
}

// ----------------------------------------------------------------------------
// Reviewer-side blocker-choices block (Phase 2 of the 2026-06-04
// bridge-to-kernel migration). Mirrors the worker-side `_worker_blocker_status_block`
// shape (kernel emits `{md, sidecar_payload}`; the bridge writes the sidecar
// JSON to disk and substitutes `{sidecar_path}` plus `{context_json_path}`
// before splicing). Byte-equivalent to the bridge's
// `_format_blocker_choices_summary` for representative fixtures; see the
// snapshot tests under `kernel/tests/runtime_cli_snapshots.rs`.
// ----------------------------------------------------------------------------

/// Reviewer-side blocker choices block.
///
/// Same envelope as `WorkerBlockerStatusBlock` (the bridge writes the sidecar
/// JSON and substitutes placeholders before splicing). The reviewer-side md
/// additionally contains the `{context_json_path}` placeholder that the
/// bridge replaces with the kernel-emitted `<request_id>.context.json` path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewBlockerChoicesBlock {
    pub md: String,
    pub sidecar_payload: Option<Value>,
}

/// Literal placeholder inside `ReviewBlockerChoicesBlock::md` for the on-disk
/// `<request>.context.json` path. The bridge substitutes the real path before
/// splicing.
pub const REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER: &str = "{context_json_path}";

/// Format a reviewer-facing 4-column row (includes the fingerprint-encoded
/// blocker `id`). Mirrors the bridge's `_format_blocker_row(include_id=True)`.
fn review_blocker_format_row_with_id(index: usize, choice: &Value) -> String {
    let blocker = choice.get("blocker");
    let kind = blocker
        .and_then(|b| b.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("?");
    let label = blocker
        .map(|b| {
            let obj = b.get("object");
            let otype = obj
                .and_then(|o| o.get("otype"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            let body = obj
                .and_then(|o| {
                    o.get("node")
                        .or_else(|| o.get("target"))
                        .or_else(|| o.get("id"))
                        .or_else(|| o.get("name"))
                })
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("{otype}:{body}")
        })
        .unwrap_or_else(|| "?:?".to_owned());
    let bid = choice
        .get("id")
        .and_then(|s| s.as_str())
        .unwrap_or("?");
    format!("{:5} | {:16} | {:32} | id={}", index, kind, label, bid)
}

/// Format a reviewer-facing 3-column row (no `id` column).
/// Mirrors the bridge's `_format_blocker_row(include_id=False)`.
fn review_blocker_format_row_no_id(index: usize, choice: &Value) -> String {
    let blocker = choice.get("blocker");
    let kind = blocker
        .and_then(|b| b.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("?");
    let label = blocker
        .map(|b| {
            let obj = b.get("object");
            let otype = obj
                .and_then(|o| o.get("otype"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            let body = obj
                .and_then(|o| {
                    o.get("node")
                        .or_else(|| o.get("target"))
                        .or_else(|| o.get("id"))
                        .or_else(|| o.get("name"))
                })
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("{otype}:{body}")
        })
        .unwrap_or_else(|| "?:?".to_owned());
    format!("{:5} | {:16} | {}", index, kind, label)
}

/// Counts-by-kind line for the reviewer; mirrors the bridge's
/// `_kind_counts_line` (which operates on the `blocker_choices` list and
/// drills through to `choice["blocker"]["kind"]`).
fn review_blocker_kind_counts_line(choices: &[Value]) -> String {
    if choices.is_empty() {
        return "(none)".to_owned();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for choice in choices {
        let kind = choice
            .get("blocker")
            .and_then(|b| b.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("?")
            .to_owned();
        *counts.entry(kind).or_insert(0) += 1;
    }
    let parts: Vec<String> = counts.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    parts.join(", ")
}

/// Reviewer-side actionable selection — parallel to
/// `worker_blocker_select_actionable` but operating on the `blocker_choices`
/// list (the bridge passes `deps_neighborhood=None` at the reviewer call
/// site, so we accept the same `Option`-shaped input here for parity).
fn review_blocker_select_actionable(
    choices: &[Value],
    active_node: Option<&str>,
    held_target: Option<&str>,
) -> (Vec<usize>, String) {
    let mut neighborhood: BTreeSet<&str> = BTreeSet::new();
    if let Some(n) = active_node {
        neighborhood.insert(n);
    }
    let target_focus = held_target;

    let mut matched: Vec<usize> = Vec::new();
    for (index, choice) in choices.iter().enumerate() {
        let blocker = match choice.get("blocker") {
            Some(b) => b,
            None => continue,
        };
        let obj = match blocker.get("object") {
            Some(o) => o,
            None => continue,
        };
        let otype = obj.get("otype").and_then(|s| s.as_str()).unwrap_or("");
        match otype {
            "node" => {
                if let Some(node) = obj.get("node").and_then(|s| s.as_str()) {
                    if neighborhood.contains(node) {
                        matched.push(index);
                    }
                }
            }
            "target" => {
                if let (Some(target), Some(focus)) =
                    (obj.get("target").and_then(|s| s.as_str()), target_focus)
                {
                    if target == focus {
                        matched.push(index);
                    }
                }
            }
            _ => {}
        }
    }

    if !matched.is_empty() {
        return (matched, "active node + direct-dep neighborhood".to_owned());
    }

    let fallback_count = std::cmp::min(WORKER_BLOCKER_ACTIONABLE_FALLBACK_K, choices.len());
    if fallback_count == 0 {
        return (Vec::new(), "no live blockers".to_owned());
    }
    let mut sortable: Vec<(String, usize)> = choices
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = c
                .get("blocker")
                .map(|b| {
                    let obj = b.get("object");
                    let otype = obj
                        .and_then(|o| o.get("otype"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("?");
                    let body = obj
                        .and_then(|o| {
                            o.get("node")
                                .or_else(|| o.get("target"))
                                .or_else(|| o.get("id"))
                                .or_else(|| o.get("name"))
                        })
                        .and_then(|s| s.as_str())
                        .unwrap_or("?");
                    format!("{otype}:{body}")
                })
                .unwrap_or_else(|| "?:?".to_owned());
            (label, i)
        })
        .collect();
    sortable.sort();
    let indices: Vec<usize> = sortable
        .into_iter()
        .take(fallback_count)
        .map(|(_, i)| i)
        .collect();
    let note = format!(
        "fallback: no blockers touch active_node/held_target; showing \
first {} of {} by label order",
        fallback_count,
        choices.len()
    );
    (indices, note)
}

/// Render the reviewer-facing actionable-subset table (4-column with `id`).
fn review_blocker_format_actionable_table(
    indices: &[usize],
    choices: &[Value],
    note: &str,
) -> String {
    if indices.is_empty() {
        return format!("(none) -- {}", note);
    }
    let rows: Vec<String> = indices
        .iter()
        .filter_map(|&i| choices.get(i).map(|c| review_blocker_format_row_with_id(i, c)))
        .collect();
    let header_lines = [
        "Index | Kind             | otype:body                       | id".to_owned(),
        "------|------------------|----------------------------------|".to_owned() + &"-".repeat(40),
    ];
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Actionable subset ({} of {}): {}",
        rows.len(),
        choices.len(),
        note
    );
    out.push('\n');
    for line in header_lines.iter() {
        out.push_str(line);
        out.push('\n');
    }
    for (i, row) in rows.iter().enumerate() {
        out.push_str(row);
        if i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Render the reviewer-facing blocker-choices block.
///
/// Byte-equivalent to the bridge's `_format_blocker_choices_summary` for
/// representative inputs. In the overflow case, `md` contains
/// `{sidecar_path}` (sidecar location) and in both cases it contains
/// `{context_json_path}` (the kernel-written context.json). The bridge
/// substitutes both placeholders with concrete on-disk paths before
/// splicing.
pub fn review_blocker_choices_block(request: &WrapperRequest) -> ReviewBlockerChoicesBlock {
    // Compute the choices list the same way `review_contract_payload` does
    // (so we work off the same `id`s the reviewer sees in the contract).
    let raw_choices = blocker_choices(&request.blockers);
    let choices: Vec<Value> = raw_choices
        .iter()
        .map(|c| json!(c))
        .collect();
    let total = choices.len();
    let counts_line = review_blocker_kind_counts_line(&choices);
    let active_node = request.active_node.as_ref().map(|n| n.as_str());
    let held_target = request.held_target.as_ref().map(|n| n.as_str());
    let (indices, note) =
        review_blocker_select_actionable(&choices, active_node, held_target);
    let actionable_table = review_blocker_format_actionable_table(&indices, &choices, &note);
    let header = format!(
        "{} blocker choices total. Counts by kind: {}",
        total, counts_line
    );

    let inline_limit = worker_blocker_inline_limit();
    if total <= inline_limit {
        // Small enough -- inline everything (no sidecar needed).
        let rows: Vec<String> = choices
            .iter()
            .enumerate()
            .map(|(i, c)| review_blocker_format_row_no_id(i, c))
            .collect();
        let mut md = String::new();
        md.push_str(&header);
        md.push('\n');
        md.push('\n');
        md.push_str("Index | Kind             | Object");
        md.push('\n');
        md.push_str("------|------------------|");
        md.push_str(&"-".repeat(32));
        md.push('\n');
        for r in rows.iter() {
            md.push_str(r);
            md.push('\n');
        }
        if total > 0 {
            md.push('\n');
            md.push_str(&actionable_table);
            md.push('\n');
        }
        md.push('\n');
        md.push_str("Full structured blocker data (with the fingerprint-encoded `id`");
        md.push('\n');
        md.push_str("field that you must echo back verbatim if you select a blocker)");
        md.push('\n');
        let _ = write!(
            md,
            "lives at: {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        md.push('\n');
        md.push('\n');
        md.push_str("List every blocker `id`:");
        md.push('\n');
        md.push('\n');
        let _ = write!(
            md,
            "  jq -r '.review_blocker_choices[].id' {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        md.push('\n');
        md.push('\n');
        md.push_str("Read one full blocker by index:");
        md.push('\n');
        md.push('\n');
        let _ = write!(
            md,
            "  jq '.review_blocker_choices[N]' {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        return ReviewBlockerChoicesBlock {
            md,
            sidecar_payload: None,
        };
    }

    // Overflow path — emit actionable subset + sidecar pointer + context
    // pointer. The bridge writes the sidecar JSON and substitutes both
    // placeholders.
    let mut md = String::new();
    md.push_str(&header);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Live blocker count ({}) exceeds the inline limit ({}); full \
structured blocker list is moved to a sidecar so the actionable subset \
stays visible inline.",
        total, inline_limit
    );
    md.push('\n');
    md.push('\n');
    md.push_str(&actionable_table);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Full blocker list sidecar: `{}`",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    md.push_str(
        "The sidecar JSON has the shape \
`{\"blocker_choices\": [{\"id\": ..., \"blocker\": ...}, ...]}`. \
Use blocker `id`s for task/override/reset lists; use node ids for \
`request_sound_verifier_node_ids`.",
    );
    md.push('\n');
    md.push('\n');
    md.push_str("List every blocker `id` from the sidecar:");
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "  jq -r '.blocker_choices[].id' {}",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    md.push_str("Read one full blocker by index:");
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "  jq '.blocker_choices[N]' {}",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "The original kernel context.json also has the same data under \
`.review_blocker_choices`, mirrored at {}.",
        REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
    );
    ReviewBlockerChoicesBlock {
        md,
        sidecar_payload: Some(json!({"blocker_choices": choices})),
    }
}

pub fn worker_contract_payload(request: &WrapperRequest) -> Value {
    if request.kind != crate::model::RequestKind::Worker {
        return no_worker_contract_payload();
    }
    let validation_kind = request.worker_acceptance.validation_kind;
    let existing_node_scope_mode = match validation_kind {
        WorkerValidationKind::TheoremGlobal | WorkerValidationKind::Cleanup => "all_present",
        // Cleanup-v2 (audit Finding 5): FinalCleanup's worker-visible
        // scope is `pending_task.authorized_nodes ∪ {target_node}` for
        // Substitution and `{target_node}` for LintFix (see
        // `current_worker_authorized_nodes` at `model.rs:5898`). Both are
        // exactly-matching whitelists, not all_present. Map to
        // `authorized_existing_nodes` so the rendered scope contract
        // matches what the runtime validator enforces. Legacy lint-only
        // mode (no active cleanup task) still falls through the same
        // mode label — its scope is the active node only in practice,
        // which is a degenerate single-element whitelist.
        WorkerValidationKind::FinalCleanup => "authorized_existing_nodes",
        WorkerValidationKind::TheoremTargeted
        | WorkerValidationKind::ProofRestructure
        | WorkerValidationKind::ProofCoarseRestructure => "authorized_existing_nodes",
        WorkerValidationKind::ProofEasy | WorkerValidationKind::ProofLocal => "active_node_only",
        WorkerValidationKind::None => "none",
    };
    let new_nodes_allowed = matches!(
        validation_kind,
        WorkerValidationKind::TheoremGlobal
            | WorkerValidationKind::TheoremTargeted
            | WorkerValidationKind::ProofEasy
            | WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure
    );
    let cleanup_worker = cleanup_like_worker(request);
    // A1/A2/A6/A7/A8: build worker_context_payload conditionally — drop
    // proof-formalization-only knobs (active_difficulty, active_easy_attempts)
    // during TheoremStating and drop the reviewer-to-runner directive
    // next_context_mode from the worker payload entirely (the worker can't
    // act on it).
    let worker_context_payload = {
        let mut map = serde_json::Map::new();
        map.insert("enabled".to_owned(), json!(request.worker_context.enabled));
        if request.phase != Phase::TheoremStating {
            map.insert(
                "active_difficulty".to_owned(),
                json!(request.worker_context.active_difficulty),
            );
            map.insert(
                "active_easy_attempts".to_owned(),
                json!(request.worker_context.active_easy_attempts),
            );
        }
        map.insert(
            "worker_profile".to_owned(),
            json!(request.worker_context.worker_profile),
        );
        map.insert(
            "validation_kind".to_owned(),
            json!(request.worker_context.validation_kind),
        );
        map.insert(
            "authorized_nodes".to_owned(),
            json!(request.worker_context.authorized_nodes),
        );
        map.insert(
            "allow_new_obligations".to_owned(),
            json!(request.worker_context.allow_new_obligations),
        );
        map.insert(
            "must_close_active".to_owned(),
            json!(request.worker_context.must_close_active),
        );
        if !request
            .worker_context
            .protected_semantic_change_nodes
            .is_empty()
        {
            map.insert(
                "protected_semantic_change_nodes".to_owned(),
                json!(request.worker_context.protected_semantic_change_nodes),
            );
        }
        // Trim 10: omit paper_focus_ranges / work_style_hint when they
        // hold their default values. Mirrors the existing
        // `worker_has_meaningful_routing_hints` predicate that already
        // gates the `worker/common/33_routing_hints.md` fragment — when
        // that fragment is absent, these two fields have no rendered
        // counterpart and shouldn't show up as default-valued JSON noise.
        // Inserted only when set to non-default values.
        if !request.worker_context.paper_focus_ranges.is_empty() {
            map.insert(
                "paper_focus_ranges".to_owned(),
                json!(request.worker_context.paper_focus_ranges),
            );
        }
        if request.worker_context.work_style_hint != crate::model::WorkerWorkStyleHint::None {
            map.insert(
                "work_style_hint".to_owned(),
                json!(request.worker_context.work_style_hint),
            );
        }
        // Cleanup-v2 (audit Finding 5): surface the active cleanup task's
        // view fields so the substitution / lintfix worker prompts can
        // render `target_node`, the task kind (with its embedded
        // replacement / warning_text payload), and the audit rationale.
        // Pre-fix these fields lived on `WorkerContext` but were never
        // serialized into the worker JSON, so the prompt fragments
        // referenced fields the worker couldn't see. Inserted only when
        // populated (None on non-cleanup-v2 / legacy lint-only workers).
        if let Some(kind) = &request.worker_context.cleanup_active_task_kind_view {
            map.insert("cleanup_active_task_kind".to_owned(), json!(kind));
        }
        if let Some(target) = &request.worker_context.cleanup_active_target_node_view {
            map.insert("cleanup_active_target_node".to_owned(), json!(target));
        }
        if !request
            .worker_context
            .cleanup_active_rationale_view
            .is_empty()
        {
            map.insert(
                "cleanup_active_rationale".to_owned(),
                json!(request.worker_context.cleanup_active_rationale_view),
            );
        }
        Value::Object(map)
    };
    let mut scope_contract = serde_json::Map::new();
    scope_contract.insert(
        "existing_node_scope_mode".to_owned(),
        json!(existing_node_scope_mode),
    );
    scope_contract.insert(
        "authorized_existing_nodes".to_owned(),
        json!(request.worker_acceptance.authorized_nodes),
    );
    scope_contract.insert(
        "configured_targets".to_owned(),
        json!(request.configured_targets),
    );
    if !request.blocked_targets.is_empty() {
        scope_contract.insert("pending_targets".to_owned(), json!(request.blocked_targets));
        scope_contract.insert(
            "pending_targets_meaning".to_owned(),
            json!("targets_lacking_current_approved_support"),
        );
    }
    scope_contract.insert("new_nodes_allowed".to_owned(), json!(new_nodes_allowed));
    scope_contract.insert(
        "allow_new_obligations".to_owned(),
        json!(request.worker_context.allow_new_obligations),
    );
    scope_contract.insert(
        "must_close_active".to_owned(),
        json!(request.worker_context.must_close_active),
    );
    scope_contract.insert(
        "proof_obligation_controls_meaning".to_owned(),
        json!("allow_new_obligations=false requires every new helper node to be Lean-closed; must_close_active=true requires the active node to be Lean-closed"),
    );
    if !request
        .worker_acceptance
        .protected_semantic_change_nodes
        .is_empty()
    {
        scope_contract.insert(
            "protected_semantic_change_nodes".to_owned(),
            json!(request.worker_acceptance.protected_semantic_change_nodes),
        );
        scope_contract.insert(
            "protected_semantic_change_nodes_meaning".to_owned(),
            json!("only_these_approved_target_or_protected_closure_nodes_may_have_correspondence_reopened"),
        );
    }
    if request.phase != Phase::TheoremStating {
        // Nodes present at the end of theorem-stating. Changing any of
        // their declaration signatures (hypotheses / return type) requires
        // `coarse_restructure` mode; plain `restructure` only unlocks
        // signatures of nodes added later in proof-formalization. Empty
        // on legacy runs — the checker then treats every node as coarse
        // to preserve prior behaviour. Omitted entirely during
        // theorem-stating: the coarse set is conceptually nonexistent
        // until the theorem-stating → proof-formalization transition
        // computes it.
        scope_contract.insert(
            "coarse_dag_nodes".to_owned(),
            json!(request.coarse_dag_nodes),
        );
        scope_contract.insert(
            "coarse_dag_nodes_meaning".to_owned(),
            json!("signature_edits_require_coarse_restructure"),
        );
    }
    let mut prompt_schema_example = serde_json::Map::new();
    prompt_schema_example.insert(
        "outcome".to_owned(),
        if cleanup_worker {
            json!("valid / invalid")
        } else {
            json!("valid / invalid / stuck / needs_restructure")
        },
    );
    prompt_schema_example.insert("summary".to_owned(), json!("brief summary"));
    prompt_schema_example.insert("comments".to_owned(), json!("optional short note"));
    if !cleanup_worker {
        prompt_schema_example.insert(
            "target_claim_updates".to_owned(),
            json!({"node_id": ["target_id"]}),
        );
        prompt_schema_example.insert(
            "difficulty_updates".to_owned(),
            json!({"node_id": "easy or hard"}),
        );
        prompt_schema_example.insert(
            "deviation_requests".to_owned(),
            json!({"deviation_id": {"path": "reference/path.tex", "summary": "departure and return argument", "affected_nodes": ["node_id"]}}),
        );
        prompt_schema_example.insert(
            "node_deviation_claims".to_owned(),
            json!({"node_id": ["authorized_deviation_id"]}),
        );
        prompt_schema_example.insert(
            "deviation_deletions".to_owned(),
            json!(["deviation_id_to_retire"]),
        );
        prompt_schema_example.insert(
            "needs_restructure_suggested_nodes".to_owned(),
            json!([
                "REQUIRED (non-empty) when outcome=needs_restructure: names of existing Tablet nodes the reviewer should consider authorizing on the next dispatch — i.e. the nodes you needed to edit but couldn't under the current scope. Empty/absent for other outcomes."
            ]),
        );
    }
    // Trims for prompt-token economy. Worker prompts repeatedly emitted
    // ~75KB of structural JSON dominated by mostly-empty / out-of-scope
    // entries; these three filters typically remove ~50KB without
    // dropping any context the worker actually needs.
    //
    // 1) `current_target_claims_nonempty`: only nodes that cover at
    //    least one configured paper target. The full map's empty
    //    entries are noise.
    let target_claims_nonempty: BTreeMap<&NodeId, &BTreeSet<crate::model::TargetId>> = request
        .current_target_claims
        .iter()
        .filter(|(_, targets)| !targets.is_empty())
        .collect();
    // 2) `current_deps_scoped` is a partial view of `current_deps`,
    //    keyed by `WrapperRequest::worker_prompt_dag_scope` (the
    //    bidirectional closure of {active_node, authorized_nodes,
    //    blocker-referenced nodes}). The worker sees deps for nodes
    //    in their authorized region; whole-tablet validation kinds
    //    fall back to the full DAG via the helper. The companion
    //    field `current_deps_scope_nodes` lists exactly which nodes'
    //    deps were emitted, so the worker can tell at a glance
    //    whether a node they're considering is in the visible
    //    portion. NOTE: this is intentionally NOT named
    //    `current_deps` — that name implied the full DAG; readers
    //    should use the suffix to know the view is partial.
    let dag_scope = request.worker_prompt_dag_scope();
    let scoped_deps: BTreeMap<&NodeId, &BTreeSet<NodeId>> = request
        .current_deps
        .iter()
        .filter(|(node, _)| dag_scope.contains(*node))
        .collect();
    // 3) `current_definition_nodes` instead of `current_proof_nodes`:
    //    definitions are the minority kind in most projects; absent
    //    entries default to "proof". Preamble is named separately
    //    via `current_preamble_node` (typically a single name) so the
    //    worker can derive proof_nodes = present − definition − preamble.
    let definition_nodes: BTreeSet<&NodeId> = request
        .current_node_kinds
        .iter()
        .filter(|(_, kind)| **kind == crate::model::NodeKind::Definition)
        .map(|(node, _)| node)
        .collect();
    let preamble_nodes: BTreeSet<&NodeId> = request
        .current_node_kinds
        .iter()
        .filter(|(_, kind)| **kind == crate::model::NodeKind::Preamble)
        .map(|(node, _)| node)
        .collect();
    let mut payload = json!({
        "prompt_fragments": worker_prompt_fragments(request),
        "reviewer_lean_product": request.stuck_math_audit.last_reviewer_lean_product.clone(),
        "audit_plan": request.audit_plan.clone(),
        // Option A widening: historical audit-plan snapshot for the
        // worker. Present iff there is no live `audit_plan` and the
        // kernel has a stash (either an inactive `state.audit_plan` or
        // a `superseded_audit_plan`). Advisory-only; the worker has no
        // dismiss affordance. The `34d_last_audit_plan.md` prompt
        // fragment frames the snapshot as historical context, not as
        // an actionable plan.
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        // Worker-prompt blocker-status block (process issue 4, 2026-05-22).
        // Kernel-rendered Markdown spliced by the bridge into the worker
        // prompt's `{{blocker_status_block}}` context variable. The bridge
        // also writes `sidecar_payload` (when Some) to
        // `<raw_output_path>.blockers.json` and substitutes the
        // `{sidecar_path}` placeholder in `md` with the actual path.
        "blocker_status": worker_blocker_status_block(request),
        "request_summary": {
            "phase": request.phase,
            "mode": request.mode,
            "active_node": request.active_node,
            "held_target": request.held_target,
            "fresh_context": request.fresh_context,
            "worker_context": worker_context_payload,
            "blockers": request.blockers,
            "shallow_coarse_closed_count": request.shallow_coarse_closed_count,
            "cycles_since_shallow_coarse_closed_count_increase": request.cycles_since_shallow_coarse_closed_count_increase,
            "current_present_nodes": request.current_present_nodes,
            "current_definition_nodes": definition_nodes,
            "current_preamble_nodes": preamble_nodes,
            "current_deps_scoped": scoped_deps,
            "current_deps_scope_nodes": dag_scope,
            "current_target_claims_nonempty": target_claims_nonempty,
            "authorized_deviations": request.authorized_deviations,
            "current_deviation_files": request.current_deviation_files,
            "node_deviation_claims": request.node_deviation_claims,
            // Mirrors review_contract_payload's request_summary so the bridge
            // prompt's `{{deterministic_worker_rejection_reasons_json}}`
            // placeholder (filled from `request_summary.get(...)`) actually
            // surfaces the rejection text on retry. Without this field the
            // 32_deterministic_worker_rejection.md fragment renders to
            // `[]` and the retrying worker gets the pointer to last_invalid
            // but no content — the failure mode being a retrying worker
            // forced to read metadata.json off disk to learn why its prior
            // attempt was rejected, which it generally won't do.
            "deterministic_worker_rejection_reasons": request.deterministic_worker_rejection_reasons,
        },
        "reviewer_comments": request.reviewer_comments,
        "result_type": "worker_result_v1",
        "kernel_derives_structural_snapshot": true,
        "allowed_outcomes": if cleanup_worker {
            json!(["valid", "invalid"])
        } else {
            json!(["valid", "invalid", "stuck", "needs_restructure"])
        },
        "reported_delta_fields": if cleanup_worker {
            json!([])
        } else {
            json!(["target_claim_updates", "difficulty_updates", "deviation_requests", "node_deviation_claims", "deviation_deletions"])
        },
        // A6: forbidden_legacy_fields removed (2-year-old migration relic).
        // A7: next_context_mode removed from worker payload — it's a
        //     reviewer-to-runner directive the worker can't act on.
        "prompt_schema_example": Value::Object(prompt_schema_example),
        "scope_contract": Value::Object(scope_contract),
        "stuck_contract": {
            "allowed": !cleanup_worker,
            "forbid_tablet_changes_when_stuck": request.worker_acceptance.forbid_tablet_changes_when_stuck,
            "meaning": if cleanup_worker {
                json!("none")
            } else {
                json!("cannot_make_progress_on_pending_work_under_current_scope")
            },
        },
        "needs_restructure_contract": {
            "allowed": !cleanup_worker,
            // Post Stuck/NR rule removal: the kernel honours
            // NeedsRestructure regardless of tablet deltas — the
            // engine's restore_committed + RestoreWorktreeToActiveWorkerBase
            // path rolls any in-progress work back. The worker may still
            // bundle structural changes when reporting NR, but those
            // changes are discarded; the verdict signal is what matters.
            "forbid_tablet_changes_when_needs_restructure": false,
            "meaning": if cleanup_worker {
                json!("none")
            } else {
                json!("worker_can_name_broader_restructure_needed_but_current_scope_does_not_authorize_it")
            },
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-worker-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{acceptance_context_path}}",
            "--raw-only",
        ], &[
            "python3",
            "{{check_script_path}}",
            "trellis-worker-result",
            "{{raw_output_path}}",
            "--repo",
            "{{repo_path}}",
            "--context-json",
            "{{acceptance_context_path}}",
        ]),
    });
    // Proposal v32 audit-2 followup #2 (post-fix): only surface the
    // active-coarse-anchor framing to the worker when the explanatory
    // fragment `worker/proof_formalization/08_coarse_anchor.md` is also
    // assembled. The fragment chooser at `proof_worker_gate_prompt_fragments`
    // gates on `request.active_coarse_node.is_some()`; mirror that here
    // so the JSON and the prompt text agree. Without alignment, a
    // ProofFormalization request with `active_coarse_node = None` (boot,
    // post-cone-clean, post-engine-fallback) would still emit
    // `"active_coarse_node": null, "coarse_repair_mode": false` literals
    // in `request_summary` with no fragment to explain them. The
    // bridge's `_drop_null_keys` doesn't run on `request_summary`
    // (rendered separately via `_json_fence`), and even if it did the
    // bool `false` would survive.
    if request.active_coarse_node.is_some() {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(|v| v.as_object_mut())
        {
            summary.insert(
                "active_coarse_node".to_owned(),
                json!(request.active_coarse_node),
            );
            summary.insert(
                "coarse_repair_mode".to_owned(),
                json!(request.coarse_repair_mode),
            );
        }
    }
    payload
}

pub fn review_contract_payload(request: &WrapperRequest) -> Value {
    if request.kind != crate::model::RequestKind::Review {
        return no_review_contract_payload();
    }
    // A3: omit `coarse_dag_nodes` from request_summary during TheoremStating —
    // the coarse-DAG concept does not exist until the
    // theorem-stating → proof-formalization transition computes it.
    let mut request_summary = serde_json::Map::new();
    request_summary.insert("phase".to_owned(), json!(request.phase));
    request_summary.insert("mode".to_owned(), json!(request.mode));
    request_summary.insert("active_node".to_owned(), json!(request.active_node));
    request_summary.insert("held_target".to_owned(), json!(request.held_target));
    // Proposal v32 audit-2 followup #2: surface every active-coarse-anchor
    // field referenced by the reviewer prompt fragment
    // `review/common/30b_coarse_anchor.md`. Without these, the fragment
    // tells the reviewer to inspect fields that the rendered payload
    // didn't expose. Gated on ProofFormalization + non-empty
    // `coarse_dag_nodes` (post-fix): outside that regime every field is
    // at its inert default (None / false / 0 / empty set) and emitting
    // them is just prompt noise.
    if request.phase == Phase::ProofFormalization && !request.coarse_dag_nodes.is_empty() {
        request_summary.insert(
            "active_coarse_node".to_owned(),
            json!(request.active_coarse_node),
        );
        request_summary.insert(
            "kernel_hinted_next_active_coarse_nodes".to_owned(),
            json!(request.kernel_hinted_next_active_coarse_nodes),
        );
        request_summary.insert(
            "coarse_repair_mode".to_owned(),
            json!(request.coarse_repair_mode),
        );
        request_summary.insert(
            "cycles_in_coarse_repair_mode".to_owned(),
            json!(request.cycles_in_coarse_repair_mode),
        );
        request_summary.insert(
            "coarse_anchor_starvation_unlocked".to_owned(),
            json!(request.coarse_anchor_starvation_unlocked),
        );
    }
    request_summary.insert("invalid_attempt".to_owned(), json!(request.invalid_attempt));
    request_summary.insert(
        "retry_outcome_kind".to_owned(),
        json!(request.retry_outcome_kind),
    );
    request_summary.insert("retry_attempt".to_owned(), json!(request.retry_attempt));
    request_summary.insert(
        "human_input_outstanding".to_owned(),
        json!(request.human_input_outstanding),
    );
    request_summary.insert("blocked_targets".to_owned(), json!(request.blocked_targets));
    request_summary.insert(
        "cycles_since_clean".to_owned(),
        json!(request.cycles_since_clean),
    );
    request_summary.insert(
        "no_sound_progress_window_cycles".to_owned(),
        json!(request.no_sound_progress_window_cycles),
    );
    request_summary.insert(
        "shallow_coarse_closed_count".to_owned(),
        json!(request.shallow_coarse_closed_count),
    );
    request_summary.insert(
        "cycles_since_shallow_coarse_closed_count_increase".to_owned(),
        json!(request.cycles_since_shallow_coarse_closed_count_increase),
    );
    request_summary.insert(
        "last_clean_rewind_count".to_owned(),
        json!(request.last_clean_rewind_count),
    );
    if request.stuck_math_audit.active {
        request_summary.insert(
            "stuck_math_audit".to_owned(),
            json!(request.stuck_math_audit.clone()),
        );
    }
    // Option A: surface the live audit_plan in request_summary iff
    // it is dismissable. Otherwise emit the historical snapshot under
    // a distinct key so the reviewer prompt can render it as
    // advisory-only context.
    if review_audit_dismissal_legal(request) {
        if let Some(plan) = request.audit_plan.as_ref() {
            request_summary.insert("audit_plan".to_owned(), json!(plan));
        }
    } else if let Some(snapshot) = request.previous_audit_plan_snapshot.as_ref() {
        request_summary.insert(
            "previous_audit_plan_snapshot".to_owned(),
            json!(snapshot),
        );
    }
    // Surface the effective mandatory-LastClean threshold so the
    // reviewer prompt fragment `review/common/32_revert.md` can render
    // it without being separately edited when the operator overrides
    // the default via `TRELLIS_CSC_LAST_CLEAN_THRESHOLD`. The value
    // is computed by the kernel via `csc_last_clean_threshold()` and
    // matches the threshold actually enforced in `request_allowed_resets`.
    request_summary.insert(
        "csc_last_clean_threshold".to_owned(),
        json!(crate::model::csc_last_clean_threshold()),
    );
    // Number of rewinds to the current clean checkpoint that waives
    // the mandatory-LastClean rule. Matches the constant
    // `CSC_REWIND_WAIVER_COUNT` enforced in `request_allowed_resets`;
    // surfaced so the prompt fragment renders the effective number
    // without drift.
    request_summary.insert(
        "csc_rewind_waiver_count".to_owned(),
        json!(crate::model::CSC_REWIND_WAIVER_COUNT),
    );
    request_summary.insert(
        "latest_worker_rationale".to_owned(),
        json!({
            "summary": request.latest_worker_summary,
            "comments": request.latest_worker_comments,
            "needs_restructure_suggested_nodes": request.latest_worker_needs_restructure_suggested_nodes,
        }),
    );
    request_summary.insert(
        "deterministic_worker_rejection_reasons".to_owned(),
        json!(request.deterministic_worker_rejection_reasons),
    );
    request_summary.insert(
        "latest_review_rejection_reasons".to_owned(),
        json!(request.latest_review_rejection_reasons),
    );
    // Cross-cycle history: the reviewer's prompt previously only included
    // the immediately-previous worker (via `latest_worker_rationale`). The
    // append-only burst-history ledger at the path below carries one row
    // per WrapperResponse across the entire run — workers, reviewers, and
    // verifiers — so an agent grepping by active_node can see prior
    // reviewer decisions / worker attempts on the same node. Surface the
    // path here so the prompt fragment can point at it without hardcoding.
    request_summary.insert(
        "recent_burst_history_path".to_owned(),
        json!(".trellis/logs/burst-history.jsonl"),
    );
    if request.phase != Phase::TheoremStating {
        // Nodes present at the end of theorem-stating. When granting a
        // restructure mode to repair an active node's signature, check
        // whether the active node is in this set: if yes, the repair
        // needs `coarse_restructure`; if no, plain `restructure` is
        // sufficient. Empty for legacy runs that predate this field.
        request_summary.insert(
            "coarse_dag_nodes".to_owned(),
            json!(request.coarse_dag_nodes),
        );
    }
    if request.phase == Phase::ProofFormalization
        && !request.resettable_theorem_stating_nodes.is_empty()
    {
        request_summary.insert(
            "resettable_theorem_stating_nodes".to_owned(),
            json!(request.resettable_theorem_stating_nodes),
        );
    }
    if request.phase == Phase::ProofFormalization && !request.approved_target_nodes.is_empty() {
        request_summary.insert(
            "approved_target_nodes".to_owned(),
            json!(request.approved_target_nodes),
        );
    }
    if let Some(confirmation) = request.protected_semantic_change_confirmation.as_ref() {
        request_summary.insert(
            "protected_semantic_change_confirmation".to_owned(),
            json!(confirmation),
        );
    }
    if !request.protected_reapproval_nodes.is_empty() {
        request_summary.insert(
            "protected_reapproval_nodes".to_owned(),
            json!(request.protected_reapproval_nodes),
        );
        request_summary.insert(
            "protected_reapproval_status".to_owned(),
            json!(
                "pending human reapproval after normal verifier blockers drain; do not treat this as a blocker-action item"
            ),
        );
    }

    // Blocker action fields are independent choices, not a complete
    // partition. Reviewers only name obligations they are acting on in this
    // transition; omitted blockers remain live and will resurface.
    // Option C (2026-06-04): override_blocker_ids retired; the reviewer's
    // blocker actions collapse to {task, reset, request_sound_verifier}.
    let allowed_reset_ids = blocker_choice_ids(&request.allowed_reset_blockers);
    let mut action_fields: Vec<&'static str> = vec!["task_blocker_ids"];
    if !allowed_reset_ids.is_empty() {
        action_fields.push("reset_blocker_ids");
    }
    action_fields.push("request_sound_verifier_node_ids");

    // Build the prompt_schema_example, omitting fields not in the
    // current action_fields list.
    let mut prompt_schema_example = serde_json::Map::new();
    // Mirror the snake_case fix for `next_mode`/`reset` (commit 9f88125):
    // the artifact validator's `parse_decision` does `to_ascii_lowercase()`
    // against snake_case constants, so PascalCase (`AdvancePhase`,
    // `NeedInput`) lowercases to `advancephase`/`needinput` and fails to
    // match `advance_phase`/`need_input`. No hard FAIL this run because
    // every chat that emitted `decision` happened to pick the single-token
    // `Continue`/`Done` forms; surface the underscored vocabulary in the
    // example so the latent footgun goes away.
    prompt_schema_example.insert(
        "decision".to_owned(),
        json!(request
            .allowed_decisions
            .iter()
            .map(review_decision_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reason".to_owned(),
        json!("brief rationale for the decision"),
    );
    prompt_schema_example.insert(
        "comments".to_owned(),
        json!("optional non-authoritative comments"),
    );
    prompt_schema_example.insert(
        "task_blocker_ids".to_owned(),
        json!(["subset of listed ids assigned to the next worker; omit blockers you are not assigning now"]),
    );
    if !allowed_reset_ids.is_empty() {
        prompt_schema_example.insert(
            "reset_blocker_ids".to_owned(),
            json!(["subset of allowed reset ids"]),
        );
    }
    prompt_schema_example.insert(
        "request_sound_verifier_node_ids".to_owned(),
        json!(["node id from blocker_actions.sound_verifier_requestable_nodes"]),
    );
    prompt_schema_example.insert(
        "next_active".to_owned(),
        match request.phase {
            Phase::TheoremStating => json!("node id or empty string"),
            Phase::ProofFormalization => {
                json!("node id (required; empty string is rejected)")
            }
            Phase::Cleanup => {
                json!("empty string (cleanup dispatch is task-driven via cleanup_next_task; the worker's active node is resolved from the task's target_node)")
            }
            Phase::Complete => json!("node id or empty string"),
        },
    );
    // Proposal v32 audit-2 followup #3: surface `next_active_coarse` in
    // the schema example so reviewers don't have to grep source to learn
    // about it. The string `""` is the "preserve current anchor"
    // sentinel (legal everywhere); a non-empty node id is only legal in
    // ProofFormalization Continue (non-retry) and must be a member of
    // `next_active_coarse_contract.kernel_hinted_coarse_nodes`. See
    // `review_next_active_coarse_legal_for_response` in model.rs:2394.
    prompt_schema_example.insert(
        "next_active_coarse".to_owned(),
        if request.phase == Phase::ProofFormalization
            && matches!(request.retry_outcome_kind, RetryOutcomeKind::None)
            && !request.kernel_hinted_next_active_coarse_nodes.is_empty()
        {
            json!("empty string to preserve current anchor; or a node id from kernel_hinted_next_active_coarse_nodes to switch coarse anchor this cycle")
        } else {
            json!("")
        },
    );
    // Surface `next_mode` and `reset` as the snake_case strings the
    // artifact validator (`artifact_validation.rs`) accepts, not the
    // PascalCase produced by serde's default serialization. The
    // validator's check is `to_ascii_lowercase()` against snake_case
    // constants — so the PascalCase form (e.g. `CoarseRestructure`)
    // lowercases to `coarserestructure` and fails to match
    // `coarse_restructure`. Reviewers who copied from the schema example
    // hit this every time they tried a Restructure / CoarseRestructure
    // response and had to retry with the underscore form.
    prompt_schema_example.insert(
        "next_mode".to_owned(),
        json!(request
            .allowed_next_modes
            .iter()
            .map(task_mode_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reset".to_owned(),
        json!(request
            .allowed_resets
            .iter()
            .map(reset_choice_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reset_node".to_owned(),
        if request
            .allowed_resets
            .contains(&crate::model::ResetChoice::TheoremStatingNode)
        {
            json!("node id from resettable_theorem_stating_nodes when reset=theorem_stating_node; otherwise empty string")
        } else {
            json!("")
        },
    );
    prompt_schema_example.insert(
        "difficulty_updates".to_owned(),
        json!({"node_id from allowed_difficulty_update_nodes": "easy or hard"}),
    );
    prompt_schema_example.insert(
        "allow_new_obligations".to_owned(),
        if request.phase == Phase::ProofFormalization {
            json!("true/false; false requires every new helper to be Lean-closed")
        } else {
            json!(true)
        },
    );
    prompt_schema_example.insert(
        "must_close_active".to_owned(),
        if request.phase == Phase::ProofFormalization {
            json!("true/false; true requires the active node to be Lean-closed")
        } else {
            json!(false)
        },
    );
    // Trim 4: gate the four Continue-only example fields on whether
    // Continue is in the allowed decision set. When Continue is NOT
    // allowed (e.g. terminal-state Done / AdvancePhase / NeedInput
    // states), the worker is not going to run again, so these
    // routing-hint and human-input-clearing examples have no effect on
    // the outcome and should not appear in the schema example.
    let continue_allowed = request
        .allowed_decisions
        .contains(&crate::model::ReviewDecisionKind::Continue);
    let protected_scope_available =
        request.phase == Phase::ProofFormalization && !request.approved_target_nodes.is_empty();
    if continue_allowed {
        prompt_schema_example.insert(
            "clear_human_input".to_owned(),
            if request.human_input_outstanding {
                json!(true)
            } else {
                json!("omit unless clearing human input")
            },
        );
        prompt_schema_example.insert(
            "next_worker_context_mode".to_owned(),
            json!("resume or fresh"),
        );
        prompt_schema_example.insert(
            "paper_focus_ranges".to_owned(),
            json!([{"start_line": 1, "end_line": 5, "reason": "optional source-paper focus"}]),
        );
        prompt_schema_example.insert(
            "paper_grounding".to_owned(),
            json!({
                "consulted_cited_ranges": "true after directly reading every range in paper_focus_ranges; required for continue in friction reviews and whenever paper_focus_ranges is nonempty",
                "basis_summary": "short reviewer-authored note of what the cited paper text says and why it matters",
            }),
        );
        if request.stuck_math_audit.active {
            prompt_schema_example.insert(
                "stuck_math_audit".to_owned(),
                json!({
                    "notes": "brief reviewer note from the StuckMathAudit pass",
                    "reviewer_lean_product": format!(
                        "optional schema-light diagnostic product to forward to the next worker; max {} serialized JSON characters",
                        crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
                    ),
                }),
            );
        }
        if review_audit_dismissal_legal(request) {
            prompt_schema_example.insert(
                "dismiss_audit_plan".to_owned(),
                json!("optional true to dismiss the whole current audit_plan"),
            );
            prompt_schema_example.insert(
                "dismissed_tasks".to_owned(),
                json!([{"id": "audit task id", "reason": "why this task is stale or wrong"}]),
            );
        }
        prompt_schema_example.insert("work_style_hint".to_owned(), json!("none or restructure"));
        if protected_scope_available {
            prompt_schema_example.insert(
                "protected_semantic_change_node_ids".to_owned(),
                json!("empty list unless exceptional; subset of protected_semantic_change_contract.allowed_nodes"),
            );
            prompt_schema_example.insert(
                "confirm_protected_semantic_change_scope".to_owned(),
                json!(request.protected_semantic_change_confirmation.is_some()),
            );
        }
        if request.phase == Phase::ProofFormalization {
            prompt_schema_example.insert(
                "authorized_node_ids".to_owned(),
                json!("narrow list of existing nodes the worker may edit; required (non-empty) for restructure / coarse_restructure; required empty for local mode; must be a subset of next_active+next_mode's scope envelope; next_active is a scope anchor, include it here only if the worker may edit that node"),
            );
            if request.global_repair_mode_enabled {
                prompt_schema_example.insert(
                    "global_repair_request".to_owned(),
                    json!("optional Step A: omit to skip, or {proposed_extension_node_ids: [..], reason: \"..\"} to request an audit-gated cone extension. Non-acting: do not set authorized_node_ids / next_active / task_blocker_ids alongside."),
                );
                prompt_schema_example.insert(
                    "consume_global_repair_grant".to_owned(),
                    json!("optional Step C: set true to consume the pending audit grant; authorized_node_ids and next_active may include nodes from pending_global_repair_grant.approved_extension_nodes."),
                );
            }
        }
    }

    // A4: the difficulty_update_contract is a proof-formalization mechanism;
    // emit `null` during TheoremStating so the prompt does not dump 33 node
    // names with `easy/hard` slots that have no functional effect.
    let difficulty_update_contract = if request.phase == Phase::TheoremStating {
        Value::Null
    } else {
        json!({
            "allowed_nodes": request.allowed_difficulty_update_nodes,
        })
    };

    let sound_obligations: Vec<Value> = request
        .blockers
        .iter()
        .filter_map(|blocker| match (&blocker.object, blocker.kind) {
            (BlockerObject::Node { node }, BlockerKind::Soundness) => Some(json!({
                "node": node,
                "blocker_id": crate::blocker_choice_id(blocker),
                "status": request.sound_assessment_statuses.get(node),
                "worker_repair_ready": request.sound_repair_ready_nodes.contains(node),
                "verifier_requestable": request.sound_verifier_requestable_nodes.contains(node),
            })),
            _ => None,
        })
        .collect();
    let mut blocker_actions = serde_json::Map::new();
    blocker_actions.insert("required".to_owned(), json!(false));
    blocker_actions.insert("action_fields".to_owned(), json!(action_fields));
    blocker_actions.insert(
        "meaning".to_owned(),
        json!("Each list is optional action for this transition. Omitted blockers remain live; there is no complete-partition requirement."),
    );
    blocker_actions.insert(
        "choices".to_owned(),
        json!(blocker_choices(&request.blockers)),
    );
    blocker_actions.insert("allowed_reset_ids".to_owned(), json!(allowed_reset_ids));
    // Option C (2026-06-04): `allowed_override_ids` retired; the
    // reviewer's blocker actions collapse to {task, reset,
    // request_sound_verifier}. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    blocker_actions.insert("sound_obligations".to_owned(), json!(sound_obligations));
    blocker_actions.insert(
        "sound_verifier_requestable_nodes".to_owned(),
        json!(request.sound_verifier_requestable_nodes),
    );
    blocker_actions.insert(
        "sound_repair_ready_nodes".to_owned(),
        json!(request.sound_repair_ready_nodes),
    );
    blocker_actions.insert(
        "reset_semantics".to_owned(),
        json!("clear_current_fail_to_unknown"),
    );
    let mut optional_fields = vec![
        "clear_human_input",
        "next_worker_context_mode",
        "paper_focus_ranges",
        "paper_grounding",
        "work_style_hint",
        "next_active_coarse",
    ];
    if request.stuck_math_audit.active {
        optional_fields.push("stuck_math_audit");
    }
    if review_audit_dismissal_legal(request) {
        optional_fields.push("dismiss_audit_plan");
        optional_fields.push("dismissed_tasks");
    }
    if continue_allowed && protected_scope_available {
        optional_fields.push("protected_semantic_change_node_ids");
        optional_fields.push("confirm_protected_semantic_change_scope");
    }
    // global_repair_mode: surface Step A and Step C optional fields
    // only when the feature is enabled AND we're in ProofFormalization.
    if request.global_repair_mode_enabled
        && request.phase == Phase::ProofFormalization
        && continue_allowed
    {
        optional_fields.push("global_repair_request");
        optional_fields.push("consume_global_repair_grant");
    }
    // `authorized_node_ids` is required when the reviewer can hand
    // proof Continue+Restructure/CoarseRestructure work to a worker;
    // otherwise it's optional (and must be empty in non-cross-node
    // modes — see review/common/36_authorized_nodes.md).
    let proof_restructure_modes_allowed = request.phase == Phase::ProofFormalization
        && continue_allowed
        && (request.allowed_next_modes.contains(&TaskMode::Restructure)
            || request
                .allowed_next_modes
                .contains(&TaskMode::CoarseRestructure));
    // `request_sound_verifier_node_ids` is intentionally NOT in required_fields:
    // the validator (artifact_validation.rs `expect_string_list`) treats it
    // as optional with default `[]`, matching the semantic that not every
    // reviewer response asks for a Sound verifier dispatch. The other entries
    // here ARE strictly required (decision, reason, task_blocker_ids, etc.).
    // Option C (2026-06-04): `override_blocker_ids` removed from the
    // required field list; the reviewer no longer needs to emit it.
    // The raw payload field is still tolerated for back-compat (see
    // RawReviewPayload) and silently dropped during normalization.
    let mut required_fields: Vec<&'static str> = vec![
        "decision",
        "reason",
        "comments",
        "task_blocker_ids",
        "reset_blocker_ids",
        "next_active",
        "next_mode",
        "reset",
        "reset_node",
        "difficulty_updates",
        "allow_new_obligations",
        "must_close_active",
    ];
    if proof_restructure_modes_allowed {
        required_fields.push("authorized_node_ids");
    } else if request.phase == Phase::ProofFormalization && continue_allowed {
        // Local-only proof state: the field is still emitted in the
        // schema example for completeness, but is required to be
        // empty.
        optional_fields.push("authorized_node_ids");
    }
    let protected_semantic_change_contract = if protected_scope_available {
        json!({
            "allowed_nodes": request.approved_target_nodes,
            "default": [],
            "requires": {
                "decision": "continue",
                "next_mode": "coarse_restructure",
                "next_active": "non_empty",
                "reset": "none",
            },
            "confirmation_required": request.protected_semantic_change_confirmation.is_some(),
            "pending_confirmation": request.protected_semantic_change_confirmation,
            "warning": if request.protected_semantic_change_confirmation.is_some() {
                json!("Confirming this scope allows a worker to reopen protected semantic meaning; any actual reopen must pass verifier lanes and then triggers human reapproval.")
            } else {
                json!("Exceptional only. Leave empty unless preserving the protected semantic node is genuinely impossible.")
            },
        })
    } else {
        Value::Null
    };

    // Cleanup-v2 Step 17: when reviewing in Phase::Cleanup, surface the
    // task list + per-status counts + allowed-next-task indices +
    // re-audit legality so the prompt can render the task table and
    // the reviewer's allowed inputs are explicit. On other phases,
    // these fields are omitted (the surface only matters in cleanup).
    let cleanup_contract = if request.phase == Phase::Cleanup
        && request.kind == crate::model::RequestKind::Review
    {
        let tasks_view: Vec<Value> = request
            .cleanup_audit_tasks_view
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "task_index": i,
                    "target_node": t.target_node,
                    "rationale": t.rationale,
                    "confidence": t.confidence,
                    "kind": t.kind,
                    "status": t.status,
                    "audit_origin_round": t.audit_origin_round,
                })
            })
            .collect();
        let pending_indices: Vec<u32> = request
            .cleanup_audit_tasks_view
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(t.status, crate::model::CleanupTaskStatus::Pending))
            .map(|(i, _)| i as u32)
            .collect();
        let pending_count = pending_indices.len();
        let completed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Completed))
            .count();
        let failed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Failed { .. }))
            .count();
        let dismissed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Dismissed { .. }))
            .count();
        let request_reaudit_legal =
            request.cleanup_audit_round_view < crate::model::CLEANUP_AUDIT_MAX_ROUNDS;
        json!({
            "tasks": tasks_view,
            "pending_count": pending_count,
            "completed_count": completed_count,
            "failed_count": failed_count,
            "dismissed_count": dismissed_count,
            "pending_indices": pending_indices,
            "cleanup_audit_round": request.cleanup_audit_round_view,
            "max_rounds": crate::model::CLEANUP_AUDIT_MAX_ROUNDS,
            "request_reaudit_legal": request_reaudit_legal,
            "protected_statement_node_set": request.cleanup_protected_statement_node_set_view,
            "dispatch_semantics": {
                "cleanup_dismiss_tasks": "bulk-dismiss any subset of Pending tasks; each entry (task_index, reason)",
                "cleanup_next_task": "Optional<task_index>; dispatch exactly one Pending task to a worker burst this cycle",
                "cleanup_request_reaudit": "Only legal on Done; only effective when cleanup_audit_round < max_rounds",
                "authorized_nodes": "Worker edit scope. For Substitution, include all importers; the target is implicit and deletable. For LintFix, single-node.",
            }
        })
    } else {
        Value::Null
    };
    let stuck_math_audit_contract = if request.stuck_math_audit.active {
        json!({
            "active": true,
            "response_field": "stuck_math_audit",
            "required_when": "decision=continue and reset=none",
            "shape": {
                "notes": "string; non-empty notes are sufficient when no product is useful",
                "reviewer_lean_product": format!(
                    "optional schema-light JSON value forwarded to the next worker when present; must serialize to at most {} JSON characters",
                    crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
                ),
            },
            "current_state": request.stuck_math_audit.clone(),
        })
    } else {
        Value::Null
    };

    json!({
        "prompt_fragments": review_prompt_fragments(request),
        "request_summary": Value::Object(request_summary),
        "cleanup_contract": cleanup_contract,
        "stuck_math_audit_contract": stuck_math_audit_contract,
        // Option A: visibility ⇔ dismissability. The live audit plan
        // and its contract block are surfaced to the reviewer iff
        // dismissal is legal. When the plan is non-dismissable
        // (latch off, wrong phase, etc.), the live surfaces are
        // suppressed and `previous_audit_plan_snapshot` (below)
        // carries a clearly-tagged historical reference instead.
        "audit_plan": if review_audit_dismissal_legal(request) {
            json!(request.audit_plan.clone())
        } else {
            Value::Null
        },
        "audit_plan_contract": if review_audit_dismissal_legal(request) {
            let mut ctx = serde_json::Map::new();
            ctx.insert("visible".to_owned(), json!(true));
            ctx.insert("dismissal_legal".to_owned(), json!(true));
            ctx.insert(
                "dismiss_audit_plan_field".to_owned(),
                json!("dismiss_audit_plan"),
            );
            ctx.insert(
                "dismissed_tasks_field".to_owned(),
                json!("dismissed_tasks"),
            );
            ctx.insert(
                "dismissed_tasks_shape".to_owned(),
                json!([{"id": "task id from audit_plan.tasks", "reason": "non-empty reason"}]),
            );
            ctx.insert(
                "semantics".to_owned(),
                json!("Audit tasks are suggestions, not blocker authority. Stale tasks may only be dismissed explicitly while StuckMathAudit is active (proof_formalization / theorem_stating, or a NeedInputAuditor plan); otherwise route useful tasks through ordinary reviewer decisions."),
            );
            Value::Object(ctx)
        } else {
            Value::Null
        },
        // Option A snapshot surface: a clearly-tagged historical
        // reference, present iff the live plan is not surfaced and
        // some snapshot (live `audit_plan` or `superseded_audit_plan`)
        // exists. Distinct from `audit_plan` so the reviewer cannot
        // confuse a retired plan with an actionable one. The
        // `29c_last_audit_plan.md` prompt fragment explains the
        // advisory-only semantics.
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        "artifact_contract": {
            "result_type": "review_result_v1",
            "required_fields": required_fields,
            "optional_fields": optional_fields,
            "prompt_schema_example": Value::Object(prompt_schema_example),
        },
        "verifier_evidence": request.review_verifier_evidence,
        "blocker_actions": Value::Object(blocker_actions.clone()),
        "blocker_partition": Value::Object(blocker_actions),
        // Phase 2 of the bridge-to-kernel migration (2026-06-04): kernel-
        // rendered Markdown body + structured sidecar payload for the
        // reviewer-facing blocker-choices block. The bridge consumes this
        // via `_resolve_review_blocker_choices_block` and falls back to the
        // legacy in-bridge `_format_blocker_choices_summary` if the field is
        // absent (old-kernel compat). Direct parallel to
        // `worker_contract.blocker_status` (worker-side migration).
        "blocker_choices_block": review_blocker_choices_block(request),
        "need_input_contract": {
            "meaning": "escalate_to_human_before_blocker_adjudication",
            "blocker_partition_required": false,
            "task_blocker_ids": [],
            "reset_blocker_ids": [],
            "request_sound_verifier_node_ids": [],
            "next_active": "",
            "next_mode": request.mode,
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        "next_active_contract": {
            "kernel_hinted_nodes": request.kernel_hinted_next_active_nodes,
            "targeted_allowed_nodes": request.targeted_next_active_nodes,
            "allow_targeted_without_next_active": request.allow_targeted_without_next_active,
            "proof_restructure_semantics": "For proof_formalization Continue (including next_mode=restructure/coarse_restructure), next_active must be in kernel_hinted_nodes; that set is already filtered to the active coarse-anchor cone (widened in coarse_repair_mode). To anchor outside the current cone, set next_active_coarse in the same response.",
            "theorem_stating_semantics": "next_mode=Global requires next_active in kernel_hinted_nodes; next_mode=Targeted requires next_active in targeted_allowed_nodes.",
        },
        // A4: emit `null` during TheoremStating — the difficulty-update
        // mechanism is a proof-formalization knob (no easy/hard distinction
        // applies to nodes during theorem-stating).
        "difficulty_update_contract": difficulty_update_contract,
        "proof_obligation_scope_contract": {
            "applies_when": "phase=proof_formalization and decision=continue",
            "default_outside_applies_when": {
                "allow_new_obligations": true,
                "must_close_active": false,
            },
            "allow_new_obligations": {
                "false": "new helper nodes must be Lean-closed with no sorry, in addition to all normal scope and verifier requirements",
                "true": "new helper nodes may remain open with sorry/NL proof when otherwise legal"
            },
            "must_close_active": {
                "false": "the active node may remain open if the current scope otherwise accepts the burst",
                "true": "the active node must be Lean-closed with no sorry for the burst to be valid"
            },
            "difficulty_note": "easy/hard remains an advisory difficulty hint only; it does not change scope or closure gates"
        },
        "clear_human_input_contract": {
            "allowed_when_outstanding": request.human_input_outstanding,
            "omit_when_not_allowed": true,
        },
        "comments_contract": {
            "field": "comments",
            "semantics": "non_authoritative_guidance_forwarded_to_future_workers",
            "empty_string_means_no_comments": true,
        },
        "routing_hints_contract": {
            "next_worker_context_mode_values": ["resume", "fresh"],
            "paper_focus_ranges_shape": {"start_line": ">= 1", "end_line": ">= start_line", "reason": "optional short reason"},
            "work_style_hint_values": ["none", "restructure"],
            "continue_only": true,
            "advisory_only": true,
            "semantics": "non_authoritative_hints_forwarded_to_future_workers_without_expanding_kernel_authority",
        },
        "paper_grounding_contract": {
            "required_for_continue_reset_none_in_friction": request.review_requires_paper_grounding(),
            "required_when_paper_focus_ranges_nonempty": true,
            "friction_definition": "any blockers present, or retry_outcome_kind in {stuck, needs_restructure}",
            "attestation_field": "paper_grounding.consulted_cited_ranges",
            "attestation_semantics": "true iff the reviewer directly consulted the original paper text for every range in paper_focus_ranges before submitting this response",
            "basis_summary_field": "paper_grounding.basis_summary",
            "basis_summary_required_when_attesting": true,
            "cited_ranges_field": "paper_focus_ranges",
            "non_continue_must_be_default": true,
            "non_friction_continue_without_ranges_must_be_default": true,
        },
        "protected_semantic_change_contract": protected_semantic_change_contract,
        "reset_contract": {
            "allowed_resets": request.allowed_resets,
            "last_commit_semantics": "discard_unaccepted_live_changes_and_resume_from_last_accepted_checkpoint",
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-reviewer-result",
            "{{raw_output_path}}",
        ], &[
            "python3",
            "{{check_script_path}}",
            "trellis-reviewer-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ]),
    })
}

fn no_audit_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "scenario": "audit_dormant",
            "audit_round": 0,
            "audit_burst_index": 0,
            "max_bursts_per_round": 0,
        },
        "artifact_contract": {},
    })
}

fn no_stuck_math_audit_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "scenario": "stuck_math_audit_dormant",
        },
        "artifact_contract": {},
    })
}

/// Cleanup-v2 Step 12 (2026-05-14): audit-burst prompt contract.
/// Populated for `RequestKind::Audit` only; otherwise returns a
/// dormant payload. Surfaces:
///   - Phase + audit round + burst index + max bursts per round
///   - The live DAG view (present_nodes, deps, target_claims)
///   - Protected-statement node set (statements + protected closure)
///   - Current `cleanup_audit_tasks` (rendered with status, kind,
///     confidence, rationale, audit_origin_round)
///   - Current `cleanup_audit_scratchpad`
///   - Latest audit rejection reason (when re-issuing after a
///     validation-fail or malformed response)
///   - Artifact contract for the `AuditResponse` JSON shape
pub fn audit_contract_payload(request: &WrapperRequest) -> Value {
    if request.kind != crate::model::RequestKind::Audit {
        return no_audit_contract_payload();
    }
    let tasks_view: Vec<Value> = request
        .cleanup_audit_tasks_view
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "task_index": i,
                "target_node": t.target_node,
                "rationale": t.rationale,
                "confidence": t.confidence,
                "kind": t.kind,
                "status": t.status,
                "audit_origin_round": t.audit_origin_round,
            })
        })
        .collect();
    let mut payload = json!({
        "prompt_fragments": [
            "audit/00_intro.md",
            "audit/05_loop_semantics.md",
            "audit/10_target_constraints.md",
            "audit/20_artifact_contract.md",
            STRUCTURED_REQUEST_POINTER_FRAGMENT,
        ],
        "request_summary": {
            "phase": request.phase,
            "scenario": "cleanup_audit",
            "audit_round": request.cleanup_audit_round_view,
            "audit_burst_index": request.cleanup_audit_burst_count_view,
            "max_bursts_per_round": crate::model::CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND,
            "max_rounds": crate::model::CLEANUP_AUDIT_MAX_ROUNDS,
        },
        "dag_view": {
            "present_nodes": request.current_present_nodes,
            "deps": request.current_deps,
            "target_claims": request.current_target_claims,
            "configured_targets": request.configured_targets,
        },
        "protected_statement_node_set": request.cleanup_protected_statement_node_set_view,
        "cleanup_audit_tasks": tasks_view,
        "cleanup_audit_scratchpad": request.cleanup_audit_scratchpad_view,
        "latest_audit_rejection_reason": request.latest_audit_rejection_reason_view,
        "artifact_contract": {
            "result_type": "cleanup_audit_result_v1",
            "prompt_schema_example": {
                "new_tasks": [
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "substitution",
                            "replacement": {
                                "kind": "mathlib",
                                "citation": "Nat.add_comm"
                            }
                        }
                    },
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "substitution",
                            "replacement": {
                                "kind": "tablet_wrapper",
                                "node": "ReplacementNodeId"
                            }
                        }
                    },
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "lint_fix",
                            "warning_text": "verbatim lake build warning"
                        }
                    }
                ],
                "task_modifications": [
                    {"task_index": 0, "reason": "second-look: not actually a wrapper"}
                ],
                "scratchpad_replace": "scratchpad text to carry across bursts",
                "outcome": "audit_done | need_to_continue"
            }
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-audit-result",
            "{{raw_output_path}}",
        ], &[]),
    });
    // Cleanup-v2 migration (2026-06-04): pre-compute the trimmed inline
    // view the bridge renders in the prompt. Mirrors what
    // `bridge_prompts._prompt_facing_audit_contract` used to do: keep
    // only `request_summary` and `artifact_contract.result_type`. The
    // dropped fields (`dag_view`, `protected_statement_node_set`,
    // `cleanup_audit_tasks`, `cleanup_audit_scratchpad`,
    // `latest_audit_rejection_reason`, `prompt_fragments`,
    // `artifact_prompt_view`) are all rendered via dedicated
    // placeholders elsewhere in the prompt; the full payload still
    // ships via `structured_request_path`.
    let mut view = serde_json::Map::new();
    if let Some(rs) = payload.get("request_summary").cloned() {
        view.insert("request_summary".to_string(), rs);
    }
    if let Some(result_type) = payload
        .get("artifact_contract")
        .and_then(|ac| ac.get("result_type"))
        .cloned()
    {
        view.insert(
            "artifact_contract".to_string(),
            json!({"result_type": result_type}),
        );
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), Value::Object(view));
    }
    payload
}

pub fn stuck_math_audit_contract_payload(request: &WrapperRequest) -> Value {
    if request.kind != crate::model::RequestKind::StuckMathAudit {
        return no_stuck_math_audit_contract_payload();
    }
    let is_need_input_auditor = request.stuck_math_audit.need_input_audit.is_some();
    // Suppress the cone_clean fragment + contract + schema-example
    // field when there are no allowed nodes to clean.
    // `resettable_theorem_stating_nodes` is populated only in
    // `Phase::ProofFormalization` (see
    // `ProtocolState::resettable_theorem_stating_nodes`); in
    // TheoremStating it is always empty, so cone_clean is suppressed
    // there without hardcoding phase. Mirrors other phase-conditional
    // fragment gating that keys off populated request data, not
    // `request.phase`.
    let cone_clean_available =
        !is_need_input_auditor && !request.resettable_theorem_stating_nodes.is_empty();
    let prompt_schema_example = if is_need_input_auditor {
        json!({
            "confirm_need_input": false,
            "report": "substantive audit report with concrete current evidence",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific recovery task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    } else if cone_clean_available {
        json!({
            "report": "substantive audit report with concrete current evidence",
            "cone_clean_node": "optional node id from cone_clean_contract.allowed_nodes, or empty string",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    } else {
        json!({
            "report": "substantive audit report with concrete current evidence",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    };
    let mut prompt_schema_example = prompt_schema_example;
    if request.pending_global_repair_request.is_some() {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert("global_repair_approve".to_owned(), json!(true));
        obj.insert(
            "global_repair_approved_extension_node_ids".to_owned(),
            json!(["minimal subset of pending_global_repair_request.proposed_extension_node_ids"]),
        );
        obj.insert(
            "global_repair_auditor_reason".to_owned(),
            json!("brief decline reason; required iff global_repair_approve is false"),
        );
    }
    let role_fragment = if request.pending_global_repair_request.is_some() {
        "stuck_math_audit/common/01_global_repair_auditor_role.md"
    } else if is_need_input_auditor {
        "stuck_math_audit/common/01_need_input_auditor_role.md"
    } else {
        "stuck_math_audit/common/01_role.md"
    };
    let output_fragment = if is_need_input_auditor {
        "stuck_math_audit/common/05_need_input_output_contract.md"
    } else {
        "stuck_math_audit/common/05_output_contract.md"
    };
    let mut prompt_fragments = vec![
        role_fragment,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        "stuck_math_audit/common/02_reference_paper.md",
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/02_request_context.md",
        "stuck_math_audit/common/02b_trigger_reason.md",
        "stuck_math_audit/common/03_history_access.md",
        "stuck_math_audit/common/04_scratchpad.md",
    ];
    if !is_need_input_auditor && request.phase == Phase::TheoremStating {
        prompt_fragments.insert(1, "stuck_math_audit/common/01b_theorem_stating_framing.md");
        // Same helper-node policy the worker and reviewer see in
        // TheoremStating, so audit prescriptions don't reflexively call
        // for new helper nodes as the default soundness-repair move.
        prompt_fragments.push("review/common/33c_theorem_helper_policy.md");
    }
    if cone_clean_available {
        prompt_fragments.push("stuck_math_audit/common/04b_cone_clean.md");
    }
    prompt_fragments.extend([
        output_fragment,
        "shared/90_artifact_delivery.md",
        STRUCTURED_REQUEST_POINTER_FRAGMENT,
    ]);
    let cone_clean_contract = if cone_clean_available {
        json!({
            "allowed_nodes": request.resettable_theorem_stating_nodes,
            "response_field": "cone_clean_node",
            "optional": true,
            "semantics": "Optional coarse-node cone clean. Runtime restores the selected node to theorem-stating files, prunes orphaned helpers, and sends the audit plan to Review.",
        })
    } else {
        Value::Null
    };
    let confirm_need_input_contract = if is_need_input_auditor {
        json!({
            "response_field": "confirm_need_input",
            "true_semantics": "Confirm a real fundamental paper problem or paper/tablet impossibility requiring human input.",
            "false_semantics": "Reject the escalation and provide recovery tasks when the issue is fixable within the protocol.",
            "false_requires_tasks": true,
        })
    } else {
        Value::Null
    };
    let mut contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": if is_need_input_auditor { "need_input_auditor" } else { "stuck_math_audit" },
        "request_summary": {
            "phase": request.phase,
            "scenario": if is_need_input_auditor { "need_input_auditor" } else { "stuck_math_audit" },
            "cycle": request.cycle,
            "request_id": request.id,
            "active_node": request.active_node,
            "mode": request.mode,
            "cycles_since_clean": request.cycles_since_clean,
            "no_sound_progress_window_cycles": request.no_sound_progress_window_cycles,
            "shallow_coarse_closed_count": request.shallow_coarse_closed_count,
            "cycles_since_shallow_coarse_closed_count_increase": request.cycles_since_shallow_coarse_closed_count_increase,
            "last_clean_rewind_count": request.last_clean_rewind_count,
            "retry_outcome_kind": request.retry_outcome_kind,
            "retry_attempt": request.retry_attempt,
            "blockers": request.blockers,
            "current_present_nodes": request.current_present_nodes,
            "current_proof_nodes": request.current_proof_nodes,
            "current_deps": request.current_deps,
            "current_target_claims": request.current_target_claims,
            "resettable_theorem_stating_nodes": request.resettable_theorem_stating_nodes,
            "latest_worker_rationale": {
                "summary": request.latest_worker_summary,
                "comments": request.latest_worker_comments,
                "needs_restructure_suggested_nodes": request.latest_worker_needs_restructure_suggested_nodes,
            },
            "reviewer_comments": request.reviewer_comments,
            "deterministic_worker_rejection_reasons": request.deterministic_worker_rejection_reasons,
        },
        "audit_latch": request.stuck_math_audit.clone(),
        "stuck_math_audit": request.stuck_math_audit.clone(),
        "need_input_audit": request.stuck_math_audit.need_input_audit.clone(),
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        "latest_stuck_math_audit_rejection_reason": request.latest_stuck_math_audit_rejection_reason,
        "cone_clean_contract": cone_clean_contract,
        "confirm_need_input_contract": confirm_need_input_contract,
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "task_title_max_chars": crate::model::AUDIT_TASK_TITLE_MAX_CHARS,
            "task_body_max_chars": crate::model::AUDIT_TASK_BODY_MAX_CHARS,
            "plan_max_json_chars": crate::model::AUDIT_PLAN_MAX_JSON_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    // Cleanup-v2 migration (2026-06-04): pre-compute the trimmed view the
    // bridge renders inline in the prompt. Mirrors what
    // `bridge_prompts._prompt_facing_stuck_math_audit_contract` used to do
    // (drop `prompt_fragments` + `artifact_prompt_view` housekeeping, then
    // drop any null Option<> fields). Bridge reads this verbatim; the
    // full contract still ships via `structured_request_path`.
    let mut view = contract.clone();
    if let Some(view_map) = view.as_object_mut() {
        view_map.remove("prompt_fragments");
        view_map.remove("artifact_prompt_view");
    }
    if let Some(map) = contract.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), drop_null_keys(view));
    }
    contract
}

pub fn populate_request_prompt_contracts(request: &mut WrapperRequest, repo_path: Option<&Path>) {
    request.prompt_contract_version = prompt_contract_version();
    request.project_invariants = project_invariants_payload();
    request.paper_contract = paper_contract_payload(request);
    request.corr_contract = correspondence_contract_payload(request, repo_path);
    request.sound_contract = soundness_contract_payload(request);
    request.worker_contract = worker_contract_payload(request);
    request.review_contract = review_contract_payload(request);
    request.audit_contract = audit_contract_payload(request);
    request.stuck_math_audit_contract = stuck_math_audit_contract_payload(request);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn stuck_math_audit_contract_emits_dedicated_fragments() {
        let mut request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 1,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test trigger".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            // Non-empty so the cone_clean fragment + contract are
            // emitted (gated on the set being non-empty rather than
            // hardcoding phase).
            resettable_theorem_stating_nodes: BTreeSet::from([NodeId::from("n")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments");
        let fragment_names: Vec<_> = fragments
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(fragment_names.contains(&"stuck_math_audit/common/01_role.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/02_reference_paper.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/04b_cone_clean.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/05_output_contract.md"));
        assert!(
            !fragment_names.contains(&"stuck_math_audit/common/05_need_input_output_contract.md")
        );
        let reference_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/02_reference_paper.md")
            .expect("reference paper fragment");
        let context_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/02_request_context.md")
            .expect("request context fragment");
        let cone_clean_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/04b_cone_clean.md")
            .expect("cone clean fragment");
        let output_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/05_output_contract.md")
            .expect("output contract fragment");
        assert!(reference_idx < context_idx);
        assert!(cone_clean_idx < output_idx);
        assert_eq!(
            request.stuck_math_audit_contract["artifact_contract"]["result_type"],
            json!("stuck_math_audit_result_v1")
        );
        assert_eq!(
            request.stuck_math_audit_contract["burst_role"],
            json!("stuck_math_audit")
        );
        assert!(request.stuck_math_audit_contract["confirm_need_input_contract"].is_null());
        assert!(
            request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("schema object")
                .get("confirm_need_input")
                .is_none()
        );
    }

    #[test]
    fn need_input_auditor_contract_uses_dedicated_role_on_same_lane() {
        let mut request = WrapperRequest {
            id: 3,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 8,
            phase: Phase::TheoremStating,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "reviewer requested NeedInput".into(),
                need_input_audit: Some(crate::model::NeedInputAuditContext {
                    phase: Phase::TheoremStating,
                    reviewer_reason: "suspected paper gap".into(),
                    reviewer_comments: "reviewer escalation".into(),
                    review_request_id: 2,
                    review_cycle: 8,
                    ..crate::model::NeedInputAuditContext::default()
                }),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments");
        let fragment_names: Vec<_> = fragments
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert_eq!(
            fragment_names[0],
            "stuck_math_audit/common/01_need_input_auditor_role.md"
        );
        assert!(
            fragment_names.contains(&"stuck_math_audit/common/05_need_input_output_contract.md")
        );
        assert!(!fragment_names.contains(&"stuck_math_audit/common/04b_cone_clean.md"));
        assert!(!fragment_names.contains(&"stuck_math_audit/common/05_output_contract.md"));
        assert_eq!(
            request.stuck_math_audit_contract["burst_role"],
            json!("need_input_auditor")
        );
        assert_eq!(
            request.stuck_math_audit_contract["request_summary"]["scenario"],
            json!("need_input_auditor")
        );
        assert_eq!(
            request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
                ["confirm_need_input"],
            json!(false)
        );
        assert_eq!(
            request.stuck_math_audit_contract["confirm_need_input_contract"]
                ["false_requires_tasks"],
            json!(true)
        );
        assert!(request.stuck_math_audit_contract["cone_clean_contract"].is_null());
    }

    #[test]
    fn cleanup_worker_contract_restricts_outcomes() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        assert_eq!(
            request.worker_contract["allowed_outcomes"],
            json!(["valid", "invalid"])
        );
        assert_eq!(
            request.worker_contract["stuck_contract"]["allowed"],
            json!(false)
        );
        assert_eq!(
            request.worker_contract["needs_restructure_contract"]["allowed"],
            json!(false)
        );
        assert!(request.worker_contract["prompt_fragments"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|value| value == "worker/final_cleanup/05_task.md"))));
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/37_field_guidance.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/35_reviewer_comments.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/45_outcomes.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/35_reviewer_comments.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/37_field_guidance.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/45_outcomes.md"));
    }

    #[test]
    fn paper_contract_includes_covering_nodes_for_each_target() {
        let request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            paper_verify_targets: BTreeSet::from([
                TargetId::from("target_a"),
                TargetId::from("target_b"),
            ]),
            current_present_nodes: BTreeSet::from([
                NodeId::from("CoverA"),
                NodeId::from("CoverB"),
                NodeId::from("Hidden"),
            ]),
            current_target_claims: BTreeMap::from([
                (
                    NodeId::from("CoverA"),
                    BTreeSet::from([TargetId::from("target_a")]),
                ),
                (
                    NodeId::from("CoverB"),
                    BTreeSet::from([TargetId::from("target_a"), TargetId::from("target_b")]),
                ),
                (
                    NodeId::from("Missing"),
                    BTreeSet::from([TargetId::from("target_b")]),
                ),
            ]),
            ..WrapperRequest::default()
        };

        let contract = paper_contract_payload(&request);
        assert_eq!(
            contract["target_covering_nodes"],
            json!({
                "target_a": ["CoverA", "CoverB"],
                "target_b": ["CoverB"],
            })
        );
    }

    #[test]
    fn worker_prompt_uses_brief_scheme_when_context_is_resumed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.worker_contract["prompt_fragments"][0],
            json!("common/00_trellis_scheme_brief.md")
        );
    }

    #[test]
    fn worker_prompt_uses_full_scheme_when_context_is_fresh() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: true,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.worker_contract["prompt_fragments"][0],
            json!("common/TRELLIS_FORMALIZATION_SCHEME.md")
        );
    }

    #[test]
    fn review_prompt_uses_brief_scheme_when_context_is_resumed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            fresh_context: false,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["prompt_fragments"][0],
            json!("common/00_trellis_scheme_brief.md")
        );
    }

    #[test]
    fn verifier_prompts_use_verifier_scheme() {
        // B6: verifiers always get the trim verifier-only scheme that
        // omits reviewer-only mode-machinery sections.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            fresh_context: false,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.corr_contract["prompt_fragments"][0],
            json!("common/TRELLIS_FORMALIZATION_SCHEME_verifier.md")
        );
    }

    #[test]
    fn worker_prompt_omits_verifier_evidence_fragment_when_none_exists() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/34_verifier_evidence.md"));
    }

    #[test]
    fn worker_prompt_includes_verifier_evidence_fragment_when_present() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            review_verifier_evidence: crate::model::ReviewVerifierEvidence {
                corr: std::collections::BTreeMap::from([(
                    "lane_1".to_string(),
                    crate::model::CorrReviewerLaneEvidence {
                        correspondence: crate::model::CorrReviewerPhaseEvidence {
                            decision: "FAIL".to_string(),
                            issues: vec![],
                        },
                        overall: "REJECT".to_string(),
                        summary: "mismatch".to_string(),
                        comments: "fix it".to_string(),
                    },
                )]),
                ..crate::model::ReviewVerifierEvidence::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/34_verifier_evidence.md"));
    }

    #[test]
    fn worker_prompt_omits_deterministic_rejection_fragment_when_none_exists() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/32_deterministic_worker_rejection.md"));
    }

    #[test]
    fn worker_prompt_includes_deterministic_rejection_fragment_when_present() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            deterministic_worker_rejection_reasons: vec![
                "Tablet/SubcriticalExpectation.lean has an application type mismatch".to_string(),
            ],
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/32_deterministic_worker_rejection.md"));
    }

    #[test]
    fn worker_prompt_always_includes_scratchpad_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_scratchpad.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_invalid_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            invalid_attempt: true,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_stuck_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Stuck,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_needs_restructure_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::NeedsRestructure,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_omits_last_invalid_fragment_when_no_retry_context() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::None,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn theorem_worker_prompt_includes_first_request_dag_fragment_for_request_one() {
        let mut request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments.iter().any(|item| {
            item == "worker/theorem_stating/12_first_request_dag_decomposition.md"
        }));
    }

    #[test]
    fn proof_worker_prompt_includes_soundness_review_fragment_when_soundness_blocker_present() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "proof worker prompt with Soundness blocker must include the \
             after-soundness-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_omits_soundness_review_fragment_when_no_soundness_blocker() {
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            !fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "proof worker prompt without a Soundness blocker must omit the \
             after-soundness-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_both_substantiveness_and_soundness_fragments_when_both_blockers_present(
    ) {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let substantiveness_blocker = crate::model::Blocker {
            kind: BlockerKind::Substantiveness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("B"),
            },
            fingerprint: "fp-b".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker, substantiveness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments.iter().any(|item| {
                item == "worker/theorem_stating/05_after_substantiveness_review.md"
            }),
            "expected substantiveness fragment present; got: {:?}",
            fragments
        );
        assert!(
            fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "expected soundness fragment present; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_corr_review_fragment_when_nodecorr_blocker_present() {
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments.iter().any(|item| {
                item == "worker/proof_formalization/05_after_correspondence_review.md"
            }),
            "proof worker prompt with NodeCorr blocker must include the \
             after-correspondence-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_omits_corr_review_fragment_when_no_nodecorr_blocker() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            !fragments.iter().any(|item| {
                item == "worker/proof_formalization/05_after_correspondence_review.md"
            }),
            "proof worker prompt without a NodeCorr blocker must omit the \
             after-correspondence-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_all_three_scenario_fragments_when_all_blockers_present() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let substantiveness_blocker = crate::model::Blocker {
            kind: BlockerKind::Substantiveness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("B"),
            },
            fingerprint: "fp-b".to_string(),
            deferred: false,
        };
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("C"),
            },
            fingerprint: "fp-c".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker, substantiveness_blocker, corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        for expected in [
            "worker/theorem_stating/05_after_substantiveness_review.md",
            "worker/proof_formalization/05_after_correspondence_review.md",
            "worker/proof_formalization/05_after_soundness_review.md",
        ] {
            assert!(
                fragments.iter().any(|item| item == expected),
                "expected fragment {} present; got: {:?}",
                expected,
                fragments
            );
        }
    }

    #[test]
    fn theorem_worker_prompt_omits_first_request_dag_fragment_after_request_one() {
        let mut request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments.iter().any(|item| {
            item == "worker/theorem_stating/12_first_request_dag_decomposition.md"
        }));
    }

    #[test]
    fn worker_prompt_includes_post_initial_sketch_policy_after_cycle_one() {
        let mut request = WrapperRequest {
            id: 2,
            cycle: 2,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| { item == "worker/common/39_post_initial_sketch_policy.md" }));
    }

    #[test]
    fn worker_prompt_omits_post_initial_sketch_policy_on_cycle_one() {
        let mut request = WrapperRequest {
            id: 1,
            cycle: 1,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| { item == "worker/common/39_post_initial_sketch_policy.md" }));
    }

    #[test]
    fn theorem_review_prompt_omits_proof_restructure_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn review_contract_includes_latest_worker_rationale_in_request_summary() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            latest_worker_summary: "worker built a broad first DAG".to_string(),
            latest_worker_comments: "critical window branch still feels shaky".to_string(),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["request_summary"]["latest_worker_rationale"]["summary"],
            serde_json::json!("worker built a broad first DAG")
        );
        assert_eq!(
            request.review_contract["request_summary"]["latest_worker_rationale"]["comments"],
            serde_json::json!("critical window branch still feels shaky")
        );
    }

    // Option C (2026-06-04): `review_contract_surfaces_allowed_override_ids`
    // removed — `allowed_override_ids` and `override_blocker_ids` are no
    // longer emitted into the reviewer contract. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

    #[test]
    fn review_contract_surfaces_need_input_contract() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: crate::TaskMode::Local,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/31_need_input.md"));
        assert_eq!(
            request.review_contract["need_input_contract"]["task_blocker_ids"],
            serde_json::json!([])
        );
        // Option C (2026-06-04): `override_blocker_ids` no longer emitted
        // into the need_input_contract.
        assert!(
            request.review_contract["need_input_contract"]
                .get("override_blocker_ids")
                .is_none(),
            "override_blocker_ids should not be emitted into need_input_contract"
        );
        assert_eq!(
            request.review_contract["need_input_contract"]["next_active"],
            serde_json::json!("")
        );
        assert_eq!(
            request.review_contract["need_input_contract"]["next_mode"],
            serde_json::json!(crate::TaskMode::Local)
        );
    }

    /// Structural invariant: the `need_input_contract` sub-block emitted by
    /// `review_contract_payload` MUST advertise empty arrays for every
    /// blocker-id / verifier-node list field. The kernel's own legality
    /// predicate `review_response_rejection_reasons` rejects ANY non-empty
    /// `reset_blockers` on NeedInput; the other three list fields are
    /// likewise NeedInput-illegal. Publishing placeholder strings here
    /// causes reviewers to paraphrase the example literally and get
    /// bounced. Guards against future schema-example drift.
    #[test]
    fn review_need_input_contract_lists_are_empty_arrays() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: crate::TaskMode::Local,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let need_input = &request.review_contract["need_input_contract"];
        // Option C (2026-06-04): `override_blocker_ids` removed from the
        // need_input_contract sub-block; the field is no longer emitted.
        for field in [
            "task_blocker_ids",
            "reset_blocker_ids",
            "request_sound_verifier_node_ids",
        ] {
            assert_eq!(
                need_input[field],
                Value::Array(vec![]),
                "need_input_contract.{} must be an empty array (NeedInput legality predicate rejects any non-empty entries)",
                field
            );
        }
    }

    #[test]
    fn proof_review_prompt_includes_proof_restructure_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn proof_review_contract_surfaces_protected_semantic_confirmation() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([crate::model::ReviewDecisionKind::Continue]),
            approved_target_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("B")]),
            protected_semantic_change_confirmation: Some(
                crate::model::ProtectedSemanticChangeConfirmation {
                    nodes: BTreeSet::from([NodeId::from("B")]),
                    next_active: Some(NodeId::from("A")),
                    next_mode: crate::model::TaskMode::CoarseRestructure,
                    allow_new_obligations: true,
                    must_close_active: false,
                },
            ),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        assert_eq!(
            schema["confirm_protected_semantic_change_scope"],
            serde_json::json!(true)
        );
        assert_eq!(
            request.review_contract["protected_semantic_change_contract"]["confirmation_required"],
            serde_json::json!(true)
        );
        assert_eq!(
            request.review_contract["protected_semantic_change_contract"]["allowed_nodes"],
            serde_json::json!(BTreeSet::from([NodeId::from("A"), NodeId::from("B")]))
        );
    }

    #[test]
    fn cleanup_review_prompt_omits_proof_difficulty_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn theorem_review_prompt_omits_revert_last_clean_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        // Always-shown umbrella stays
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/32_revert.md"));
        // last_clean carve-out is withheld; the kernel's allowed_resets
        // never contains `last_clean` in TheoremStating, so the
        // threshold-mandate language would contradict the contract.
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/32a_revert_last_clean.md"));
    }

    #[test]
    fn proof_review_prompt_includes_revert_last_clean_fragment_after_revert() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let revert_idx = fragments
            .iter()
            .position(|item| item == "review/common/32_revert.md")
            .expect("32_revert.md present");
        let last_clean_idx = fragments
            .iter()
            .position(|item| item == "review/common/32a_revert_last_clean.md")
            .expect("32a_revert_last_clean.md present in ProofFormalization");
        // `32a` must land immediately after `32_revert.md` — same
        // ordering position as the prior Python-side splice.
        assert_eq!(last_clean_idx, revert_idx + 1);
    }

    #[test]
    fn cleanup_review_prompt_includes_revert_last_clean_fragment_after_revert() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let revert_idx = fragments
            .iter()
            .position(|item| item == "review/common/32_revert.md")
            .expect("32_revert.md present");
        let last_clean_idx = fragments
            .iter()
            .position(|item| item == "review/common/32a_revert_last_clean.md")
            .expect("32a_revert_last_clean.md present in Cleanup");
        assert_eq!(last_clean_idx, revert_idx + 1);
    }

    #[test]
    fn worker_prompt_includes_new_node_difficulty_guidance_when_new_nodes_are_allowed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    #[test]
    fn worker_prompt_includes_new_node_difficulty_guidance_for_proof_easy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofEasy,
                validation_kind: WorkerValidationKind::ProofEasy,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    #[test]
    fn worker_prompt_omits_new_node_difficulty_guidance_when_new_nodes_are_forbidden() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Cleanup,
                validation_kind: WorkerValidationKind::Cleanup,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    /// Both reviewer-source-recourse tests share a process-wide mutex
    /// because the availability switch is a test-only global override.
    static SOURCE_RECOURSE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct SourceRecourseOverrideGuard {
        prior: Option<bool>,
    }

    impl SourceRecourseOverrideGuard {
        fn install(value: bool) -> Self {
            let mut slot = super::SOURCE_RECOURSE_AVAILABLE_OVERRIDE
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = *slot;
            *slot = Some(value);
            Self { prior }
        }
    }

    impl Drop for SourceRecourseOverrideGuard {
        fn drop(&mut self) {
            let mut slot = super::SOURCE_RECOURSE_AVAILABLE_OVERRIDE
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            *slot = self.prior;
        }
    }

    fn with_source_recourse_env<F: FnOnce()>(snapshot: Option<&str>, sha: Option<&str>, body: F) {
        let _guard = SOURCE_RECOURSE_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let available = snapshot
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
            && sha.map(|value| !value.trim().is_empty()).unwrap_or(false);
        let _override_guard = SourceRecourseOverrideGuard::install(available);
        body();
    }

    #[test]
    fn review_prompt_includes_source_recourse_when_env_vars_set() {
        with_source_recourse_env(
            Some("/tmp/trellis-source-snapshot/abc"),
            Some("abc"),
            || {
                let mut request = WrapperRequest {
                    kind: crate::model::RequestKind::Review,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                populate_request_prompt_contracts(&mut request, None);
                let fragments = request.review_contract["prompt_fragments"]
                    .as_array()
                    .expect("review prompt fragments");
                assert!(fragments
                    .iter()
                    .any(|item| item == "review/common/05_source_recourse.md"));
            },
        );
    }

    #[test]
    fn review_prompt_omits_source_recourse_when_env_vars_unset() {
        with_source_recourse_env(None, None, || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
    }

    #[test]
    fn review_prompt_omits_source_recourse_when_only_one_env_var_set() {
        // Defense-in-depth: both env vars must be set together. If the
        // operator sets only the SHA (or only the snapshot path), we
        // omit the fragment rather than render with a placeholder.
        with_source_recourse_env(Some("/tmp/trellis-source-snapshot/abc"), None, || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
        with_source_recourse_env(None, Some("abc"), || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
    }

    // -- Trim 3: forbidden_legacy_fields stub-only relic ----------------

    #[test]
    fn no_worker_contract_payload_no_longer_emits_forbidden_legacy_fields() {
        // Trim 3 absent-when-inert: the no_worker_contract_payload stub
        // (returned when kind != Worker) used to emit a vestigial
        // `forbidden_legacy_fields: []`. The actual worker contract
        // already omits it (audit A6); the stub now matches.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert!(
            !request
                .worker_contract
                .as_object()
                .unwrap()
                .contains_key("forbidden_legacy_fields"),
            "no_worker_contract_payload stub must not emit forbidden_legacy_fields"
        );
    }

    #[test]
    fn worker_contract_request_summary_includes_rejection_reasons_on_retry() {
        // Regression guard for a request-summary omission bug:
        // `deterministic_worker_rejection_reasons`
        // was populated at the request top level but absent from
        // `worker_contract.request_summary`, causing
        // `bridge_prompts.py`'s `request_summary.get(...)` lookup to render
        // `[]` and the 32_deterministic_worker_rejection.md fragment to surface
        // an empty array — leaving the retrying worker without the rejection
        // text. The fragment `last_invalid` pointer was rendered fine; only
        // the inline JSON was empty. This test pins the fix.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            invalid_attempt: true,
            deterministic_worker_rejection_reasons: vec![
                "Declaration name is \"C0_identity\", expected \"FixedSetProjectionMiddleRegimeExponent\"".to_string(),
                ".lean shape errors: [\"single principal top-level declaration\"]".to_string(),
            ],
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let request_summary = &request.worker_contract["request_summary"];
        let reasons = request_summary
            .get("deterministic_worker_rejection_reasons")
            .expect("worker_contract.request_summary must surface deterministic_worker_rejection_reasons");
        let reasons_arr = reasons
            .as_array()
            .expect("rejection_reasons must be an array");
        assert_eq!(reasons_arr.len(), 2);
        assert!(
            reasons_arr[0]
                .as_str()
                .expect("first reason must be a string")
                .contains("C0_identity"),
            "rejection reason text must propagate to the worker prompt's request_summary"
        );
    }

    #[test]
    fn worker_contract_request_summary_emits_empty_rejection_reasons_when_none() {
        // Symmetry guard: when there's nothing to report, the field is still
        // present (and empty) — matches the reviewer payload's pattern at
        // line 1729-1732 and lets the bridge render an empty array without
        // a missing-key fallback.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let request_summary = &request.worker_contract["request_summary"];
        let reasons = request_summary
            .get("deterministic_worker_rejection_reasons")
            .expect("field must be present even when empty");
        assert!(
            reasons.as_array().expect("must be an array").is_empty(),
            "no-reject case should emit []"
        );
    }

    #[test]
    fn worker_contract_payload_no_longer_emits_forbidden_legacy_fields() {
        // Trim 3 present-when-relevant baseline: the actual
        // `worker_contract_payload` continues to omit the field
        // (audit A6 already removed it). The pair documents both halves
        // of the contract.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert!(
            !request
                .worker_contract
                .as_object()
                .unwrap()
                .contains_key("forbidden_legacy_fields"),
            "worker_contract_payload must not emit forbidden_legacy_fields"
        );
    }

    // -- Trim 10: paper_focus_ranges + work_style_hint default-omit -----

    #[test]
    fn worker_context_omits_default_routing_hint_fields() {
        // Trim 10 absent-when-inert: when paper_focus_ranges is empty and
        // work_style_hint is None (defaults), the worker_context payload
        // should omit both fields.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            !map.contains_key("paper_focus_ranges"),
            "default empty paper_focus_ranges should be omitted"
        );
        assert!(
            !map.contains_key("work_style_hint"),
            "default work_style_hint=none should be omitted"
        );
    }

    #[test]
    fn worker_context_includes_routing_hint_fields_when_set() {
        // Trim 10 present-when-relevant: when the kernel forwards
        // non-default routing hints, the fields must appear so the
        // worker can act on them.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                paper_focus_ranges: vec![crate::model::PaperFocusRange {
                    start_line: 1,
                    end_line: 10,
                    reason: "hint".to_string(),
                }],
                work_style_hint: crate::model::WorkerWorkStyleHint::Restructure,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            map.contains_key("paper_focus_ranges"),
            "non-default paper_focus_ranges must be present"
        );
        assert!(
            map.contains_key("work_style_hint"),
            "non-default work_style_hint must be present"
        );
    }

    /// Cleanup-v2 (audit Finding 5a): when a Substitution cleanup task is
    /// in flight, the worker's JSON must include the active task view
    /// fields (`cleanup_active_task_kind`, `cleanup_active_target_node`,
    /// `cleanup_active_rationale`). The substitution worker prompt
    /// fragment references these by name; without them the prompt
    /// rendered `target_node` against no actual value.
    #[test]
    fn worker_context_payload_surfaces_cleanup_substitution_view_fields() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                cleanup_active_task_kind_view: Some(crate::model::CleanupTaskKind::Substitution {
                    replacement: crate::model::CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                }),
                cleanup_active_target_node_view: Some(crate::model::NodeId::from("Wrapper")),
                cleanup_active_rationale_view: "Inlines Nat.add_comm 1-for-1".to_string(),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            map.contains_key("cleanup_active_task_kind"),
            "Substitution task_kind view field must be surfaced to the worker"
        );
        assert!(
            map.contains_key("cleanup_active_target_node"),
            "target_node view field must be surfaced to the worker"
        );
        assert_eq!(
            map.get("cleanup_active_target_node")
                .and_then(|v| v.as_str()),
            Some("Wrapper")
        );
        assert!(
            map.contains_key("cleanup_active_rationale"),
            "rationale view field must be surfaced to the worker"
        );
    }

    /// Cleanup-v2 (audit Finding 5a): legacy lint-only / non-cleanup
    /// worker requests (no active cleanup task) must NOT surface the
    /// view fields — they would be confusing noise (null payloads in
    /// non-cleanup contexts).
    #[test]
    fn worker_context_payload_omits_cleanup_view_fields_when_no_active_task() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                cleanup_active_task_kind_view: None,
                cleanup_active_target_node_view: None,
                cleanup_active_rationale_view: String::new(),
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(!map.contains_key("cleanup_active_task_kind"));
        assert!(!map.contains_key("cleanup_active_target_node"));
        assert!(!map.contains_key("cleanup_active_rationale"));
    }

    /// Cleanup-v2 (audit Finding 5b): `existing_node_scope_mode` for
    /// FinalCleanup must NOT be "all_present". The runtime validator
    /// restricts edits to `authorized_nodes ∪ {target_node}` for
    /// Substitution and `{target_node}` for LintFix — both are
    /// whitelist-shaped, not all_present.
    #[test]
    fn worker_contract_scope_mode_for_final_cleanup_is_authorized_existing_nodes() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let mode = request.worker_contract["scope_contract"]["existing_node_scope_mode"].clone();
        assert_eq!(mode, json!("authorized_existing_nodes"));
    }

    // -- Trim 13: acceptance_check_command_template empty -> Null -------

    #[test]
    fn corr_contract_acceptance_check_command_template_is_null() {
        // Trim 13 absent-when-inert: corr verifier has no acceptance
        // checker (the `&[]` slice is passed). The kernel emits Null so
        // the bridge null-drop helper strips the line entirely.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let view = &request.corr_contract["artifact_prompt_view"];
        assert_eq!(
            view["acceptance_check_command_template"],
            Value::Null,
            "corr verifier has no acceptance checker; field must be null"
        );
    }

    #[test]
    fn worker_contract_acceptance_check_command_template_is_array() {
        // Trim 13 present-when-relevant: the worker contract DOES supply
        // an acceptance check template (the runtime-snapshot context-aware
        // command). The field stays an array.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let view = &request.worker_contract["artifact_prompt_view"];
        let raw = &view["acceptance_check_command_template"];
        assert!(
            raw.is_array() && !raw.as_array().unwrap().is_empty(),
            "worker has an acceptance check template; field must be a non-empty array"
        );
    }

    // -- Trim 4: Continue-only schema example fields --------------------

    #[test]
    fn review_schema_example_omits_continue_only_fields_when_continue_disallowed() {
        // Trim 4 absent-when-inert: when allowed_decisions excludes
        // Continue (e.g. terminal-state Done-only or NeedInput-only),
        // the four routing-hint / human-input clear example fields are
        // unreachable and must be absent from the schema example.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        for field in [
            "clear_human_input",
            "next_worker_context_mode",
            "paper_focus_ranges",
            "work_style_hint",
        ] {
            assert!(
                !map.contains_key(field),
                "{field} should be absent when Continue not in allowed_decisions"
            );
        }
    }

    #[test]
    fn review_schema_example_includes_continue_only_fields_when_continue_allowed() {
        // Trim 4 present-when-relevant: when Continue is allowed, the
        // four fields must appear so the reviewer knows their schema.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        for field in [
            "clear_human_input",
            "next_worker_context_mode",
            "paper_focus_ranges",
            "work_style_hint",
        ] {
            assert!(
                map.contains_key(field),
                "{field} must be present when Continue is in allowed_decisions"
            );
        }
    }

    #[test]
    fn review_schema_example_decision_uses_snake_case() {
        // Section F regression: previously the schema example emitted
        // `allowed_decisions` directly, producing PascalCase
        // (`AdvancePhase`, `NeedInput`). `parse_decision` lowercases the
        // input against snake_case constants, so a reviewer copy-paste of
        // `AdvancePhase` lowercases to `advancephase` and fails to match
        // `advance_phase`. The schema example must surface the snake_case
        // form. Sort order is BTreeSet iteration over the
        // ReviewDecisionKind enum (Continue, AdvancePhase, NeedInput, Done
        // — i.e. the enum-declared order, mapped to snake_case).
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::AdvancePhase);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        let decision = map.get("decision").expect("decision key present");
        let decision_arr = decision.as_array().expect("decision is a JSON array");
        let values: Vec<&str> = decision_arr
            .iter()
            .map(|v| v.as_str().expect("decision array element is a string"))
            .collect();
        assert_eq!(
            values,
            vec!["continue", "advance_phase", "need_input", "done"],
            "schema example decision array must be snake_case in enum-declared order"
        );
        assert!(
            !values.contains(&"AdvancePhase"),
            "schema example must not surface PascalCase AdvancePhase"
        );
        assert!(
            !values.contains(&"NeedInput"),
            "schema example must not surface PascalCase NeedInput"
        );
    }

    #[test]
    fn review_schema_example_decision_array_round_trips_through_validator() {
        // Section F regression: each rendered decision value must be
        // accepted by the artifact validator's decision check (the same
        // path `parse_decision` covers — both lowercase the raw string
        // against the snake_case constant set). Build a minimal valid
        // reviewer payload and substitute each schema-example decision
        // value; the validator must not emit the
        // "decision must be one of [...]" rejection for any of them.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::AdvancePhase);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let decision_arr = schema["decision"]
            .as_array()
            .expect("decision is a JSON array");
        for value in decision_arr {
            let value_str = value.as_str().expect("decision array element is a string");
            let payload = json!({
                "decision": value_str,
                "reason": "round-trip test",
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
            });
            let result = crate::artifact_validation::validate_trellis_reviewer_result_data(&payload);
            assert!(
                !result.errors.iter().any(|e| e.contains("decision must be one of")),
                "schema-example decision value {value_str:?} should round-trip through the validator without a 'decision must be one of' error; got errors: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn review_schema_example_includes_next_active_coarse_with_sentinel() {
        // Section B prompt-half: in ProofFormalization with a non-empty
        // `kernel_hinted_next_active_coarse_nodes` set and no retry
        // (RetryOutcomeKind::None), the schema example surfaces
        // `next_active_coarse` with the descriptive sentinel string so
        // reviewers don't have to grep source to learn about the field.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut hinted = BTreeSet::new();
        hinted.insert(NodeId::from("CoarseNodeB"));
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            kernel_hinted_next_active_coarse_nodes: hinted,
            retry_outcome_kind: RetryOutcomeKind::None,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        let value = map
            .get("next_active_coarse")
            .expect("next_active_coarse key present");
        let text = value
            .as_str()
            .expect("next_active_coarse schema-example value is a string");
        assert!(
            text.contains("kernel_hinted_next_active_coarse_nodes"),
            "descriptive sentinel must reference the hinted-coarse-nodes set; got {text:?}"
        );
        assert!(
            text.contains("preserve current anchor"),
            "descriptive sentinel must describe the empty-string preserve case; got {text:?}"
        );
        // `next_active_coarse` is documented as an optional reviewer
        // field via the `optional_fields` vec surfaced on the artifact
        // contract (one level above `prompt_schema_example`).
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields surfaced on artifact_contract");
        assert!(
            optional
                .iter()
                .any(|v| v.as_str() == Some("next_active_coarse")),
            "next_active_coarse must be listed in optional_fields"
        );
    }

    #[test]
    fn review_schema_example_next_active_coarse_is_empty_when_locked() {
        // Section B prompt-half: outside the ProofFormalization Continue
        // (non-retry) window — or with an empty hinted-coarse-nodes set —
        // the schema example surfaces an empty-string sentinel for
        // `next_active_coarse`, signalling "anchor locked / no switch
        // legal this cycle." The key still appears so reviewers see
        // the field; the value just tells them not to populate it.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        // Case 1: TheoremStating phase (never legal to switch the coarse
        // anchor) — empty sentinel.
        let mut request_theorem = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed.clone(),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request_theorem, None);
        let theorem_schema =
            &request_theorem.review_contract["artifact_contract"]["prompt_schema_example"];
        let theorem_map = theorem_schema
            .as_object()
            .expect("theorem prompt_schema_example object");
        assert_eq!(
            theorem_map.get("next_active_coarse"),
            Some(&json!("")),
            "TheoremStating must surface next_active_coarse with the empty sentinel"
        );
        // Case 2: ProofFormalization but empty hinted set — still empty
        // sentinel (no anchor switch is currently legal).
        let mut request_proof = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            kernel_hinted_next_active_coarse_nodes: BTreeSet::new(),
            retry_outcome_kind: RetryOutcomeKind::None,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request_proof, None);
        let proof_schema =
            &request_proof.review_contract["artifact_contract"]["prompt_schema_example"];
        let proof_map = proof_schema
            .as_object()
            .expect("proof prompt_schema_example object");
        assert_eq!(
            proof_map.get("next_active_coarse"),
            Some(&json!("")),
            "ProofFormalization with empty hinted set must surface next_active_coarse with the empty sentinel"
        );
    }

    #[test]
    fn review_schema_example_omits_dismiss_fields_without_stuck_math_audit() {
        // Section A: with an `audit_plan` present but
        // `stuck_math_audit.active=false`, the kernel-side gate
        // (model.rs:3087-3115) rejects any dismissal attempt. Mirror that
        // in the schema-example renderer: the `dismiss_audit_plan` /
        // `dismissed_tasks` fields must NOT be surfaced so reviewers
        // don't see an affordance that the kernel will hard-reject.
        //
        // Option A extension: visibility ⇔ dismissability. Beyond
        // suppressing the dismiss field names, the entire
        // `audit_plan_contract` block and the `review_contract.audit_plan`
        // surface go to Null when dismissal is illegal — preventing the
        // Review 315 muddle where the reviewer reads the plan as
        // authoritative but the kernel rejects every dismissal attempt.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: false,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            !map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be absent when StuckMathAudit is inactive"
        );
        assert!(
            !map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be absent when StuckMathAudit is inactive"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> =
            optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must not be listed in optional_fields when StuckMathAudit is inactive"
        );
        assert!(
            !optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must not be listed in optional_fields when StuckMathAudit is inactive"
        );
        // Option A: dismissal illegal ⇒ live audit_plan surface zeros
        // out entirely (the reviewer sees a clearly-tagged historical
        // snapshot via `previous_audit_plan_snapshot` instead, not the
        // live plan).
        assert!(
            review_audit_dismissal_legal(&request) == false,
            "test precondition: dismissal must be illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan"],
            Value::Null,
            "review_contract.audit_plan must be null when dismissal is illegal (visibility ⇔ dismissability)"
        );
        assert_eq!(
            request.review_contract["audit_plan_contract"],
            Value::Null,
            "audit_plan_contract must be null when dismissal is illegal"
        );
        // The block-form of the A-followup field-name gating is now
        // collapsed into the whole-block null gating: the dismiss
        // field-name keys remain absent (vacuously, since the block
        // is null and not an object).
    }

    #[test]
    fn review_schema_example_omits_dismiss_fields_when_phase_disallows() {
        // Section A: with `stuck_math_audit.active=true` but the phase
        // outside the legal set (Cleanup here) AND the audit plan not
        // flagged `need_input_audit`, the kernel-side gate rejects
        // dismissals. The schema example must omit the dismiss-fields so
        // the affordance isn't shown when illegal.
        //
        // Option A extension: visibility ⇔ dismissability. The whole
        // `audit_plan_contract` block and the `review_contract.audit_plan`
        // surface go to Null when dismissal is illegal.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: false,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            !map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be absent in Cleanup phase with non-need-input plan"
        );
        assert!(
            !map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be absent in Cleanup phase with non-need-input plan"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> =
            optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must not be listed in optional_fields outside the legal window"
        );
        assert!(
            !optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must not be listed in optional_fields outside the legal window"
        );
        // Option A: dismissal illegal ⇒ live audit_plan surface zeros
        // out entirely.
        assert!(
            review_audit_dismissal_legal(&request) == false,
            "test precondition: dismissal must be illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan"],
            Value::Null,
            "review_contract.audit_plan must be null when dismissal is illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan_contract"],
            Value::Null,
            "audit_plan_contract must be null when dismissal is illegal"
        );
    }

    #[test]
    fn review_schema_example_includes_dismiss_fields_in_legal_window() {
        // Section A: `stuck_math_audit.active=true` + phase
        // ProofFormalization + audit_plan present = legal window per
        // model.rs:3087-3115. Schema example must surface
        // dismiss_audit_plan / dismissed_tasks so the reviewer sees the
        // affordance.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: false,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be present in the legal window (ProofFormalization + active StuckMathAudit)"
        );
        assert!(
            map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be present in the legal window (ProofFormalization + active StuckMathAudit)"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> =
            optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must be listed in optional_fields in the legal window"
        );
        assert!(
            optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must be listed in optional_fields in the legal window"
        );
        // A-followup: audit_plan_contract MUST surface dismiss field names
        // in the legal window so the reviewer knows the affordance exists.
        let apc = &request.review_contract["audit_plan_contract"];
        let apc_map = apc.as_object().expect("audit_plan_contract object");
        assert!(apc_map.contains_key("dismiss_audit_plan_field"));
        assert!(apc_map.contains_key("dismissed_tasks_field"));
        assert!(apc_map.contains_key("dismissed_tasks_shape"));
        assert_eq!(
            apc_map.get("dismissal_legal").and_then(|v| v.as_bool()),
            Some(true)
        );
        // Option A: dismissal legal ⇒ live audit_plan surface present
        // (visibility ⇔ dismissability).
        assert!(
            review_audit_dismissal_legal(&request),
            "test precondition: dismissal must be legal"
        );
        assert!(
            !request.review_contract["audit_plan"].is_null(),
            "review_contract.audit_plan must be non-null in the legal window"
        );
    }

    #[test]
    fn review_schema_example_includes_dismiss_fields_on_need_input_audit_plan() {
        // Section A: `audit_plan.need_input_audit=true` opens the
        // dismissal window even outside ProofFormalization /
        // TheoremStating (matches the model.rs:3107-3115 `|| plan.need_input_audit`
        // branch). Use Cleanup phase to confirm the phase check is
        // bypassed by the need_input_audit flag.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be present when audit_plan.need_input_audit=true"
        );
        assert!(
            map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be present when audit_plan.need_input_audit=true"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> =
            optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must be listed in optional_fields with a need_input_audit plan"
        );
        assert!(
            optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must be listed in optional_fields with a need_input_audit plan"
        );
        // A-followup: audit_plan_contract gating under the need-input branch.
        let apc = &request.review_contract["audit_plan_contract"];
        let apc_map = apc.as_object().expect("audit_plan_contract object");
        assert!(apc_map.contains_key("dismiss_audit_plan_field"));
        assert!(apc_map.contains_key("dismissed_tasks_field"));
        assert!(apc_map.contains_key("dismissed_tasks_shape"));
        assert_eq!(
            apc_map.get("dismissal_legal").and_then(|v| v.as_bool()),
            Some(true)
        );
        // Option A: dismissal legal ⇒ live audit_plan surface present
        // (visibility ⇔ dismissability).
        assert!(
            review_audit_dismissal_legal(&request),
            "test precondition: dismissal must be legal"
        );
        assert!(
            !request.review_contract["audit_plan"].is_null(),
            "review_contract.audit_plan must be non-null on a need_input_audit plan"
        );
    }

    #[test]
    fn review_prompt_includes_recent_burst_history_fragment() {
        // Pin the fragment id so the assembly list never silently drops
        // the cross-cycle history pointer for the reviewer. The fragment
        // must appear AFTER the verifier reasoning fragment and BEFORE
        // the contract fragment so the reviewer reads history while
        // forming its decision shape.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let idx_history = fragments
            .iter()
            .position(|item| item == "review/common/26_recent_burst_history.md")
            .expect("review/common/26_recent_burst_history.md must be included");
        let idx_verifier = fragments
            .iter()
            .position(|item| item == "review/common/25_verifier_reasoning.md")
            .expect("verifier reasoning fragment must be included");
        let idx_contract = fragments
            .iter()
            .position(|item| item == "review/common/30_contract.md")
            .expect("contract fragment must be included");
        assert!(
            idx_verifier < idx_history,
            "recent_burst_history should sit after verifier reasoning"
        );
        assert!(
            idx_history < idx_contract,
            "recent_burst_history should sit before the contract fragment"
        );
    }

    #[test]
    fn worker_prompt_includes_recent_burst_history_fragment() {
        // Pin the fragment id so the assembly list never silently drops
        // the cross-cycle history pointer for the worker. Order check
        // is light: history must appear after the request and before
        // the contract.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            active_node: Some(NodeId::from("Foo")),
            ..WrapperRequest::default()
        };
        request.worker_acceptance.validation_kind = crate::model::WorkerValidationKind::ProofLocal;
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        let idx_history = fragments
            .iter()
            .position(|item| item == "worker/common/36_recent_burst_history.md")
            .expect("worker/common/36_recent_burst_history.md must be included");
        let idx_request = fragments
            .iter()
            .position(|item| item == "worker/common/30_request.md")
            .expect("request fragment must be included");
        let idx_contract = fragments
            .iter()
            .position(|item| item == "worker/common/40_contract.md")
            .expect("contract fragment must be included");
        assert!(
            idx_request < idx_history,
            "recent_burst_history should sit after the request fragment"
        );
        assert!(
            idx_history < idx_contract,
            "recent_burst_history should sit before the contract fragment"
        );
    }

    #[test]
    fn review_prompt_includes_stuck_math_audit_fragment_when_active() {
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| item == "review/common/29_stuck_math_audit.md"),
            "active StuckMathAudit reviews must include the StuckMathAudit prompt fragment"
        );
        assert_eq!(
            request.review_contract["stuck_math_audit_contract"]["response_field"],
            json!("stuck_math_audit")
        );
        assert!(
            request.review_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("schema object")
                .contains_key("stuck_math_audit"),
            "active StuckMathAudit reviews must render the response field shape"
        );
    }

    #[test]
    fn review_prompt_splits_need_input_auditor_plan_fragments() {
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "need input recovery".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/29_need_input_auditor.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/29b_need_input_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/29_stuck_math_audit.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/29b_audit_plan.md"));
    }

    #[test]
    fn worker_prompt_includes_neutral_reviewer_lean_product_handoff() {
        let product = json!({"kind": "sufficient_statement", "statement": "add invariant H"});
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                last_reviewer_lean_product: Some(product.clone()),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| item == "worker/common/34b_stuck_math_reviewer_lean_product.md"),
            "worker prompts must include the neutral StuckMathAudit handoff fragment when a reviewer Lean product exists"
        );
        assert_eq!(request.worker_contract["reviewer_lean_product"], product);
    }

    #[test]
    fn worker_prompt_splits_need_input_auditor_plan_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/34c_need_input_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/34c_audit_plan.md"));
    }

    #[test]
    fn review_request_summary_includes_recent_burst_history_path() {
        // The kernel-authored review request_summary surfaces the
        // ledger path so the prompt fragment has a single discovery
        // point and operator-side tooling can reference the same
        // canonical location.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["request_summary"]["recent_burst_history_path"],
            serde_json::json!(".trellis/logs/burst-history.jsonl"),
        );
    }

    #[test]
    fn theorem_worker_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.worker_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "worker/theorem_stating/17_helper_policy.md"),
            "worker TheoremStating prompt is missing 17_helper_policy.md; got: {frag_strs:?}"
        );
    }

    #[test]
    fn theorem_review_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.review_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "review TheoremStating prompt is missing 33c_theorem_helper_policy.md; got: {frag_strs:?}"
        );
    }

    #[test]
    fn theorem_stuck_math_audit_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::TheoremStating,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "TheoremStating audit prompt is missing helper policy; got: {frag_strs:?}"
        );
    }

    #[test]
    fn proof_formalization_stuck_math_audit_omits_theorem_helper_policy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "ProofFormalization audit should NOT include the TheoremStating helper policy"
        );
    }

    #[test]
    fn proof_formalization_review_omits_theorem_helper_policy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.review_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "ProofFormalization review should NOT include theorem helper policy; got: {frag_strs:?}"
        );
    }

    #[test]
    fn paper_contract_renders_deviation_authorization_scenario() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            deviation_verify_id: Some(crate::model::DeviationId::from("const_loss")),
            deviation_verify_path: "reference/deviations/const_loss.tex".into(),
            verify_lanes: BTreeSet::from(["paper".to_string()]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.paper_contract["request_summary"]["scenario"],
            "deviation_authorization"
        );
        let frags = request.paper_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<&str> = frags.iter().filter_map(|v| v.as_str()).collect();
        assert!(frag_strs.contains(&"verifier/deviation/05_single_file.md"));
        assert!(frag_strs.contains(&"canonical/DEVIATIONS.md"));
        assert_eq!(
            request.paper_contract["artifact_contract"]["result_type"],
            "deviation_authorization_result_v1"
        );
    }

    #[test]
    fn post_advance_routing_review_emits_dedicated_primary_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 1,
            phase: Phase::ProofFormalization,
            post_advance_routing: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_post_advance_routing.md",
            "post_advance_routing=true must select the routing primary fragment, \
             overriding the retry_outcome_kind / blocker chain"
        );
    }

    #[test]
    fn non_routing_review_still_uses_legacy_primary_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 1,
            phase: Phase::ProofFormalization,
            post_advance_routing: false,
            ..WrapperRequest::default()
        };
        // Default RetryOutcomeKind::None + no blockers → clean fragment.
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_clean_verification.md",
        );
    }

    #[test]
    fn soundness_contract_emits_reverification_fragment_when_context_present() {
        let target = NodeId::from("FlatteningWeightedProcessSupport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Sound,
            cycle: 178,
            phase: Phase::ProofFormalization,
            sound_verify_node: Some(target.clone()),
            sound_verify_nodes: BTreeSet::from([target.clone()]),
            sound_reverification_context: Some(crate::model::SoundReverificationContext {
                target: target.clone(),
                prior_status: crate::model::SoundAssessmentStatus::VerifierPass,
                current_status:
                    crate::model::SoundAssessmentStatus::DepEditOnlyStalePassDeferred,
                own_tex_changed: false,
                deps_changed: vec![crate::model::SoundDepHashDriftEntry {
                    dep: NodeId::from("FlatteningPaperN0ProcessAbsorptions"),
                    prior_hash: "abc123def456".to_string(),
                    current_hash: "fedcba654321".to_string(),
                }],
                prior_lane_evidence: BTreeMap::new(),
            }),
            ..WrapperRequest::default()
        };
        let payload = soundness_contract_payload(&request);
        let fragments: Vec<&str> = payload["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(
            fragments.contains(&"verifier/common/15a_reverification_context.md"),
            "expected reverification fragment to be emitted; got {:?}",
            fragments,
        );
        // The reverification_context block must be present and carry
        // the dep-drift entry verbatim (truncated hashes flow through
        // the request struct, not this layer).
        let reverif = &payload["reverification_context"];
        assert_eq!(reverif["target"], json!("FlatteningWeightedProcessSupport"));
        assert_eq!(reverif["own_tex_changed"], json!(false));
        assert_eq!(
            reverif["current_status"],
            json!("DepEditOnlyStalePassDeferred"),
        );
        assert_eq!(reverif["prior_status"], json!("VerifierPass"));
        let deps = reverif["deps_changed"].as_array().expect("deps_changed array");
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0]["dep"],
            json!("FlatteningPaperN0ProcessAbsorptions"),
        );
        assert_eq!(deps[0]["prior_hash"], json!("abc123def456"));
        assert_eq!(deps[0]["current_hash"], json!("fedcba654321"));
        assert!(
            reverif["git_access_hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("git -C")),
            "git_access_hint should mention git invocation",
        );
    }

    #[test]
    fn soundness_contract_omits_reverification_fragment_when_context_absent() {
        let target = NodeId::from("FreshUnknownNode");
        let request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::Sound,
            cycle: 5,
            phase: Phase::ProofFormalization,
            sound_verify_node: Some(target.clone()),
            sound_verify_nodes: BTreeSet::from([target.clone()]),
            // sound_reverification_context omitted -> None (default)
            ..WrapperRequest::default()
        };
        let payload = soundness_contract_payload(&request);
        let fragments: Vec<&str> = payload["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(
            !fragments.contains(&"verifier/common/15a_reverification_context.md"),
            "reverification fragment must NOT be emitted for fresh-Unknown target; got {:?}",
            fragments,
        );
        assert!(
            payload["reverification_context"].is_null(),
            "reverification_context payload must be null when no context exists",
        );
    }

    #[test]
    fn dep_statement_hash_diff_helper_handles_added_removed_and_changed() {
        use crate::model::{dep_statement_hash_diff, truncate_fingerprint_for_display};
        let stored = BTreeMap::from([
            (NodeId::from("A"), "aaaaaaaaaaaa1111".to_string()),
            (NodeId::from("B"), "bbbbbbbbbbbb2222".to_string()),
            // C is absent (will be added)
            (NodeId::from("D"), "dddddddddddd4444".to_string()), // unchanged
        ]);
        let current = BTreeMap::from([
            (NodeId::from("A"), "aaaaaaaaaaaa1111".to_string()), // unchanged
            // B removed
            (NodeId::from("C"), "cccccccccccc3333".to_string()), // added
            (NodeId::from("D"), "dddddddddddd4444".to_string()), // unchanged
        ]);
        let diff = dep_statement_hash_diff(&stored, &current);
        // Sorted by NodeId: B (removed), C (added).
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].dep, NodeId::from("B"));
        assert_eq!(diff[0].prior_hash, "bbbbbbbbbbbb\u{2026}");
        assert_eq!(diff[0].current_hash, "(absent)");
        assert_eq!(diff[1].dep, NodeId::from("C"));
        assert_eq!(diff[1].prior_hash, "(absent)");
        assert_eq!(diff[1].current_hash, "cccccccccccc\u{2026}");
        // Bounded display: full hashes are truncated to 12 chars + ellipsis.
        assert_eq!(
            truncate_fingerprint_for_display(&"0123456789abcdef".to_string()),
            "0123456789ab\u{2026}",
        );
        // Short hashes pass through unchanged.
        assert_eq!(
            truncate_fingerprint_for_display(&"short".to_string()),
            "short",
        );
        assert_eq!(
            truncate_fingerprint_for_display(&"".to_string()),
            "(absent)",
        );
    }
}
