use crate::model::{
    Blocker, BlockerKind, BlockerObject, GlobalRepairRequest, NodeDifficulty, NodeId,
    PaperFocusRange, PaperGrounding, Phase, RequestKind, ResetChoice, ResponseStatus,
    ReviewDecisionKind, ReviewResponse, StuckMathAuditReviewReport, TaskDismissal, TaskMode,
    Update, WorkerContextMode, WorkerWorkStyleHint, WrapperRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBlockerChoice {
    pub id: String,
    pub blocker: Blocker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawReviewPayload {
    pub decision: String,
    pub reason: String,
    pub comments: String,
    pub task_blocker_ids: Vec<String>,
    /// Option C (2026-06-04): retired. Field retained on the raw
    /// payload for serde back-compat with legacy reviewer JSON; the
    /// normalizer silently drops the value. See
    /// REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    pub override_blocker_ids: Vec<String>,
    pub reset_blocker_ids: Vec<String>,
    #[serde(
        default,
        alias = "request_sound_verifier_nodes",
        alias = "requested_sound_verifier_node_ids"
    )]
    pub request_sound_verifier_node_ids: Vec<String>,
    pub next_active: String,
    /// Proposal v32: reviewer-chosen next active coarse anchor.
    /// Empty string => `None` (preserve current anchor). Validated
    /// downstream via `review_next_active_coarse_legal_for_response`.
    #[serde(default)]
    pub next_active_coarse: String,
    pub reset: String,
    #[serde(default, alias = "reset_node_id")]
    pub reset_node: String,
    pub next_mode: String,
    pub difficulty_updates: BTreeMap<String, String>,
    pub allow_new_obligations: Option<bool>,
    pub must_close_active: Option<bool>,
    pub clear_human_input: bool,
    pub next_worker_context_mode: String,
    pub paper_focus_ranges: Vec<PaperFocusRange>,
    pub work_style_hint: String,
    #[serde(default, alias = "protected_semantic_change_nodes")]
    pub protected_semantic_change_node_ids: Vec<String>,
    pub confirm_protected_semantic_change_scope: bool,
    /// global_repair_mode Step A raw shape: top-level object with
    /// `proposed_extension_node_ids` (list of strings) and `reason`
    /// (string). `None` (absent) means the reviewer is NOT requesting
    /// an audit-gated cone extension this cycle.
    #[serde(default)]
    pub global_repair_request: Option<RawGlobalRepairRequest>,
    /// global_repair_mode Step C raw flag.
    #[serde(default)]
    pub consume_global_repair_grant: bool,
    /// Existing nodes the worker may edit. Required (non-empty) for
    /// proof Continue+Restructure/CoarseRestructure; required empty
    /// for proof Continue+Local. `Option` distinguishes "field omitted"
    /// from "present but empty," matching the existing pattern for
    /// `allow_new_obligations` / `must_close_active`.
    #[serde(default, alias = "authorized_nodes")]
    pub authorized_node_ids: Option<Vec<String>>,
    /// Cleanup-v2 (audit Finding 2): bulk-dismiss any number of pending
    /// cleanup_audit_tasks this cycle. Each entry is `[task_index,
    /// reason]`. Legal only in Phase::Cleanup + Continue. Defaults to
    /// empty for legacy state files / non-cleanup cycles.
    #[serde(default)]
    pub cleanup_dismiss_tasks: Vec<CleanupDismissTaskRaw>,
    /// Cleanup-v2 (audit Finding 2): optional index of the single pending
    /// task to dispatch a worker against this cycle. Legal only in
    /// Phase::Cleanup + Continue with the index pointing at a Pending
    /// task.
    #[serde(default)]
    pub cleanup_next_task: Option<u32>,
    /// Cleanup-v2 (audit Finding 2): when set true on a Done decision,
    /// requests another audit round. Ignored if
    /// `cleanup_audit_round >= CLEANUP_AUDIT_MAX_ROUNDS` or if the kernel
    /// has latched a force-Done due to consecutive-invalid threshold.
    #[serde(default)]
    pub cleanup_request_reaudit: bool,
    /// Reviewer-side attestation that `paper_focus_ranges` were directly
    /// consulted. Required for Continue+reset=None in friction reviews
    /// and whenever `paper_focus_ranges` is nonempty. Enforced by
    /// `WrapperRequest::review_response_paper_grounding_legal`. Defaults
    /// to (false, "") for backward compatibility with old state files
    /// and raw artifacts.
    #[serde(default)]
    pub paper_grounding: PaperGrounding,
    /// StuckMathAudit report emitted by reviewers only when the
    /// request's StuckMathAudit view is active.
    #[serde(default)]
    pub stuck_math_audit: Option<StuckMathAuditReviewReport>,
    #[serde(default)]
    pub dismiss_audit_plan: bool,
    #[serde(default)]
    pub dismissed_tasks: Vec<TaskDismissal>,
}

/// Cleanup-v2 (audit Finding 2): raw `(task_index, reason)` entry for the
/// reviewer's `cleanup_dismiss_tasks` field. Accepts either a JSON object
/// `{"task_index": <u32>, "reason": "<str>"}` (the canonical shape) or a
/// `[index, reason]` tuple via serde's untagged variant. The kernel
/// converts these to the `(u32, String)` tuple used by `ReviewResponse`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CleanupDismissTaskRaw {
    pub task_index: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RawGlobalRepairRequest {
    #[serde(alias = "proposed_extension_nodes")]
    pub proposed_extension_node_ids: Vec<String>,
    pub reason: String,
}

impl Default for RawReviewPayload {
    fn default() -> Self {
        Self {
            decision: String::new(),
            reason: String::new(),
            comments: String::new(),
            task_blocker_ids: Vec::new(),
            override_blocker_ids: Vec::new(),
            reset_blocker_ids: Vec::new(),
            request_sound_verifier_node_ids: Vec::new(),
            next_active: String::new(),
            next_active_coarse: String::new(),
            reset: String::new(),
            reset_node: String::new(),
            next_mode: String::new(),
            difficulty_updates: BTreeMap::new(),
            allow_new_obligations: None,
            must_close_active: None,
            clear_human_input: false,
            next_worker_context_mode: String::new(),
            paper_focus_ranges: Vec::new(),
            work_style_hint: String::new(),
            protected_semantic_change_node_ids: Vec::new(),
            confirm_protected_semantic_change_scope: false,
            global_repair_request: None,
            consume_global_repair_grant: false,
            authorized_node_ids: None,
            cleanup_dismiss_tasks: Vec::new(),
            cleanup_next_task: None,
            cleanup_request_reaudit: false,
            paper_grounding: PaperGrounding::default(),
            stuck_math_audit: None,
            dismiss_audit_plan: false,
            dismissed_tasks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewNormalizationInput {
    pub request: WrapperRequest,
    pub raw_payload: RawReviewPayload,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewNormalizationOutput {
    pub response: ReviewResponse,
}

pub fn blocker_choice_id(blocker: &Blocker) -> String {
    let kind = match blocker.kind {
        BlockerKind::PaperFaithfulness => "paperfaithfulness",
        BlockerKind::Deviation => "deviation",
        BlockerKind::Substantiveness => "substantiveness",
        BlockerKind::NodeCorr => "nodecorr",
        BlockerKind::Soundness => "soundness",
    };
    let (otype, target) = match &blocker.object {
        BlockerObject::Node { node } => ("node", node.as_str()),
        BlockerObject::Target { target } => ("target", target.as_str()),
        BlockerObject::Deviation { deviation } => ("deviation", deviation.as_str()),
    };
    format!("{kind}:{otype}:{target}:{}", blocker.fingerprint)
}

pub fn blocker_choices(blockers: &BTreeSet<Blocker>) -> Vec<ReviewBlockerChoice> {
    blockers
        .iter()
        .cloned()
        .map(|blocker| ReviewBlockerChoice {
            id: blocker_choice_id(&blocker),
            blocker,
        })
        .collect()
}

pub fn blocker_choice_ids(blockers: &BTreeSet<Blocker>) -> Vec<String> {
    blockers.iter().map(blocker_choice_id).collect()
}

pub fn normalize_review_response(
    input: &ReviewNormalizationInput,
) -> Result<ReviewNormalizationOutput, String> {
    let request = &input.request;
    if request.kind != RequestKind::Review {
        return Err("review normalization requires a review request".into());
    }
    let blocker_catalog: BTreeMap<_, _> = blocker_choices(&request.blockers)
        .into_iter()
        .map(|choice| (choice.id, choice.blocker))
        .collect();

    let decision = parse_decision(&input.raw_payload.decision, &request.allowed_decisions)?;
    let reason = input.raw_payload.reason.trim().to_string();
    let comments = input.raw_payload.comments.trim().to_string();
    let next_mode = parse_next_mode(&input.raw_payload.next_mode, &request.allowed_next_modes)?;
    let reset = parse_reset(&input.raw_payload.reset, &request.allowed_resets)?;
    let reset_node = parse_reset_node(&input.raw_payload.reset_node, request, reset)?;
    let next_active = normalize_optional_node(&input.raw_payload.next_active);
    let next_active_coarse = normalize_optional_node(&input.raw_payload.next_active_coarse);
    validate_next_active(
        request,
        decision,
        reset,
        next_mode,
        next_active.as_ref(),
        next_active_coarse.as_ref(),
    )?;
    let task_blockers = resolve_blockers(
        &input.raw_payload.task_blocker_ids,
        &blocker_catalog,
        "task_blocker_ids",
    )?;
    // Option C (2026-06-04): reviewer Pass-override is retired. Any
    // `override_blocker_ids` payload field is tolerated for back-compat
    // with legacy reviewer responses and silently dropped; the engine
    // never consumes the resulting set. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    let override_blockers: BTreeSet<Blocker> = BTreeSet::new();
    let reset_blockers = resolve_blockers(
        &input.raw_payload.reset_blocker_ids,
        &blocker_catalog,
        "reset_blocker_ids",
    )?;
    let request_sound_verifier_nodes =
        parse_sound_verifier_node_ids(&input.raw_payload.request_sound_verifier_node_ids, request)?;
    let difficulty_updates = parse_difficulty_updates(
        &input.raw_payload.difficulty_updates,
        &request.allowed_difficulty_update_nodes,
    )?;
    let next_worker_context_mode =
        parse_next_worker_context_mode(&input.raw_payload.next_worker_context_mode)?;
    let paper_focus_ranges = parse_paper_focus_ranges(&input.raw_payload.paper_focus_ranges)?;
    let work_style_hint = parse_work_style_hint(&input.raw_payload.work_style_hint)?;
    let allow_new_obligations = input
        .raw_payload
        .allow_new_obligations
        .ok_or_else(|| "reviewer result must explicitly set allow_new_obligations".to_string())?;
    let must_close_active = input
        .raw_payload
        .must_close_active
        .ok_or_else(|| "reviewer result must explicitly set must_close_active".to_string())?;
    let protected_semantic_change_nodes = parse_protected_semantic_change_nodes(
        &input.raw_payload.protected_semantic_change_node_ids,
        &request.approved_target_nodes,
    )?;
    let authorized_nodes = parse_authorized_node_ids(
        &input.raw_payload.authorized_node_ids,
        request,
        decision,
        reset,
        next_mode,
    )?;
    let global_repair_request = match input.raw_payload.global_repair_request.as_ref() {
        None => None,
        Some(raw) => {
            let nodes: BTreeSet<NodeId> = raw
                .proposed_extension_node_ids
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(NodeId::from)
                .collect();
            if nodes.is_empty() {
                return Err(
                    "global_repair_request.proposed_extension_node_ids must be non-empty".into(),
                );
            }
            Some(GlobalRepairRequest {
                proposed_extension_nodes: nodes,
                reason: raw.reason.trim().to_string(),
            })
        }
    };
    let consume_global_repair_grant = input.raw_payload.consume_global_repair_grant;
    if global_repair_request.is_some() && consume_global_repair_grant {
        return Err(
            "global_repair_request and consume_global_repair_grant are mutually exclusive".into(),
        );
    }

    // Cleanup-v2 (audit Finding 2): plumb through reviewer cleanup
    // controls so the kernel can deserialize them. Legality (Phase
    // gating, index-in-range, status==Pending, max-rounds) is enforced
    // by `review_response_legal` at request-acceptance time (see audit
    // Finding 3 fixes).
    let cleanup_dismiss_tasks: Vec<(u32, String)> = input
        .raw_payload
        .cleanup_dismiss_tasks
        .iter()
        .map(|entry| (entry.task_index, entry.reason.clone()))
        .collect();
    let cleanup_next_task = input.raw_payload.cleanup_next_task;
    let cleanup_request_reaudit = input.raw_payload.cleanup_request_reaudit;
    let paper_grounding = PaperGrounding {
        consulted_cited_ranges: input.raw_payload.paper_grounding.consulted_cited_ranges,
        basis_summary: input
            .raw_payload
            .paper_grounding
            .basis_summary
            .trim()
            .to_string(),
    };
    let stuck_math_audit =
        input
            .raw_payload
            .stuck_math_audit
            .as_ref()
            .map(|report| StuckMathAuditReviewReport {
                notes: report.notes.trim().to_string(),
                reviewer_lean_product: report.reviewer_lean_product.clone(),
            });

    let response = ReviewResponse {
        request_id: request.id,
        cycle: request.cycle,
        status: ResponseStatus::Ok,
        decision,
        reason,
        comments,
        task_blockers,
        override_blockers,
        reset_blockers,
        request_sound_verifier_nodes,
        next_active,
        next_active_coarse,
        reset,
        reset_node,
        next_mode,
        difficulty_updates,
        allow_new_obligations,
        must_close_active,
        clear_human_input: input.raw_payload.clear_human_input,
        next_worker_context_mode,
        paper_focus_ranges,
        work_style_hint,
        protected_semantic_change_nodes,
        confirm_protected_semantic_change_scope: input
            .raw_payload
            .confirm_protected_semantic_change_scope,
        global_repair_request,
        consume_global_repair_grant,
        authorized_nodes,
        cleanup_dismiss_tasks,
        cleanup_next_task,
        cleanup_request_reaudit,
        paper_grounding,
        stuck_math_audit,
        dismiss_audit_plan: input.raw_payload.dismiss_audit_plan,
        dismissed_tasks: input.raw_payload.dismissed_tasks.clone(),
    };
    // NeedInput is stop-and-escalate-to-auditor — no state-mutating action
    // lists. The kernel's `review_response_legal` gate at model.rs already
    // rejects NeedInput with non-empty task_blockers / override_blockers /
    // request_sound_verifier_nodes / next_active / next_mode change, but it
    // historically omits the same check for `reset_blockers`. The engine's
    // NeedInput path then calls `apply_review_blocker_resets` on the
    // response, silently mutating blocker state before routing to the
    // auditor — contradicting the "no state mutation on NeedInput" contract
    // and the reviewer prompt at
    // `review/common/30a_blocker_actions.md:12`. Reject here so a
    // NeedInput response carrying reset_blocker_ids cannot reach the
    // engine.
    if response.decision == ReviewDecisionKind::NeedInput && !response.reset_blockers.is_empty() {
        return Err(format!(
            "review response is not legal for the request: decision=need_input requires empty task_blocker_ids, override_blocker_ids, reset_blocker_ids, and request_sound_verifier_node_ids, no next_active, and next_mode equal to the request's current mode"
        ));
    }
    if !request.review_response_legal(&response) {
        // The kernel's `review_response_legal` is a single bool gate. The
        // bare "not legal" message is too thin for the LLM to debug from
        // — it has historically misdiagnosed the failure (e.g. assumed it
        // was a vocabulary-case issue and re-emitted the same
        // blocker-action mistake). Ask the request for the same per-rule
        // diagnostics the kernel will surface on a reissued Review; fall
        // back to the older first-clause diagnostic only if that list is
        // unexpectedly empty.
        let reasons = request.review_response_rejection_reasons(&response);
        let detail = if reasons.is_empty() {
            diagnose_review_illegality(request, &response)
        } else {
            reasons.join("; ")
        };
        return Err(format!(
            "review response is not legal for the request: {detail}"
        ));
    }
    Ok(ReviewNormalizationOutput { response })
}

/// Build a human-actionable rejection reason for a review response that
/// failed `review_response_legal`. Mirrors the rule order of
/// `WrapperRequest::review_response_legal` (kernel/src/model.rs:842) and
/// returns a string that names the first violated clause it finds. Falls
/// back to a generic "see kernel/src/model.rs::review_response_legal for
/// the full rule set" when no specific clause matches — that lets us
/// extend coverage without breaking when a future rule is added.
fn diagnose_review_illegality(request: &WrapperRequest, review: &ReviewResponse) -> String {
    if request.kind != RequestKind::Review {
        return "request kind is not Review (kernel state went wrong somewhere)".into();
    }
    // Subset checks against catalog
    let extras: BTreeSet<_> = review
        .task_blockers
        .difference(&request.blockers)
        .cloned()
        .collect();
    if !extras.is_empty() {
        return format!(
            "task_blocker_ids contains blockers not present in the request: {}",
            format_blocker_ids(&extras)
        );
    }
    let extras: BTreeSet<_> = review
        .override_blockers
        .difference(&request.allowed_override_blockers)
        .cloned()
        .collect();
    if !extras.is_empty() {
        return format!(
            "override_blocker_ids contains blockers not in allowed_override_blockers: {}",
            format_blocker_ids(&extras)
        );
    }
    let extras: BTreeSet<_> = review
        .reset_blockers
        .difference(&request.allowed_reset_blockers)
        .cloned()
        .collect();
    if !extras.is_empty() {
        return format!(
            "reset_blocker_ids contains blockers not in allowed_reset_blockers: {}",
            format_blocker_ids(&extras)
        );
    }
    // Disjointness
    let dup_task_override: BTreeSet<_> = review
        .task_blockers
        .intersection(&review.override_blockers)
        .cloned()
        .collect();
    if !dup_task_override.is_empty() {
        return format!(
            "the same blocker appears in both task_blocker_ids and override_blocker_ids: {}",
            format_blocker_ids(&dup_task_override)
        );
    }
    let dup_task_reset: BTreeSet<_> = review
        .task_blockers
        .intersection(&review.reset_blockers)
        .cloned()
        .collect();
    if !dup_task_reset.is_empty() {
        return format!(
            "the same blocker appears in both task_blocker_ids and reset_blocker_ids: {}",
            format_blocker_ids(&dup_task_reset)
        );
    }
    let dup_override_reset: BTreeSet<_> = review
        .override_blockers
        .intersection(&review.reset_blockers)
        .cloned()
        .collect();
    if !dup_override_reset.is_empty() {
        return format!(
            "the same blocker appears in both override_blocker_ids and reset_blocker_ids: {}",
            format_blocker_ids(&dup_override_reset)
        );
    }
    let bad_sound_requests: BTreeSet<_> = review
        .request_sound_verifier_nodes
        .difference(&request.sound_verifier_requestable_nodes)
        .cloned()
        .collect();
    if !bad_sound_requests.is_empty() {
        return format!(
            "request_sound_verifier_node_ids contains nodes that are not legal Sound verifier targets: {:?}. Legal targets: {:?}",
            bad_sound_requests, request.sound_verifier_requestable_nodes
        );
    }
    let conflicting_sound_requests =
        request.sound_verifier_requests_conflicting_with_blocker_actions(review);
    if !conflicting_sound_requests.is_empty() {
        return format!(
            "request_sound_verifier_node_ids conflicts with task/override/reset blocker actions on the same Sound nodes: {:?}. Pick either worker/override/reset action or verifier request, not both.",
            conflicting_sound_requests
        );
    }
    // Local mode + non-empty task_blockers — with the commit 1263d80
    // Soundness carve-out: a Local + Soundness-only task_blockers shape
    // IS legal (close-in-Lean clears the active node's soundness blocker
    // without any cross-file edit; see
    // `WrapperRequest::review_response_legal` at model.rs:1762ff. and
    // the close-in-Lean recipe in
    // `review/common/05_after_failed_soundness.md`). Reject only when at
    // least one task_blocker requires a cross-file / signature repair.
    if review.next_mode == TaskMode::Local && !review.task_blockers.is_empty() {
        let non_soundness: Vec<_> = review
            .task_blockers
            .iter()
            .filter(|b| b.kind != BlockerKind::Soundness)
            .cloned()
            .collect();
        if !non_soundness.is_empty() {
            let non_soundness_set: BTreeSet<Blocker> = non_soundness.into_iter().collect();
            return format!(
                "next_mode=local with non-Soundness task_blocker_ids is illegal: local authorizes only the active node's proof body, so it cannot address blockers on other nodes or .tex/signature edits. Use restructure or coarse_restructure for these blockers, or restrict task_blocker_ids to a single Soundness blocker on the active node and close it with must_close_active. Blockers needing a wider mode: {}",
                format_blocker_ids(&non_soundness_set)
            );
        }
    }
    // authorized_nodes outside the scope envelope. The parse layer
    // catches "missing for Restructure/CoarseRestructure" and "not a
    // present node"; this clause covers the legality-only case where
    // the listed nodes are present but lie outside the impact region
    // of next_active+next_mode (and not in protected_semantic_change_nodes).
    if !review.authorized_nodes.is_empty() {
        let mut allowed = request.review_scope_envelope(review);
        allowed.extend(review.protected_semantic_change_nodes.iter().cloned());
        let outside: BTreeSet<_> = review
            .authorized_nodes
            .difference(&allowed)
            .cloned()
            .collect();
        if !outside.is_empty() {
            return format!(
                "authorized_node_ids contains nodes outside the scope envelope of next_active={} and next_mode={:?}: {:?}. Pick a wider next_active or next_mode, drop these nodes from the list, or list them in protected_semantic_change_node_ids if their semantic shape needs to change.",
                review
                    .next_active
                    .as_ref()
                    .map(|node| node.as_str())
                    .unwrap_or("<none>"),
                review.next_mode,
                outside,
            );
        }
    }
    let outside_scope = request.task_blockers_outside_review_worker_scope(review);
    if !outside_scope.is_empty() {
        return format!(
            "task_blocker_ids contains blockers outside the worker scope implied by next_active={} and next_mode={:?}: {}. Pick a next_active whose authorized impact region covers those blockers, choose a wider legal mode, or move those blockers to override/reset if appropriate.",
            review
                .next_active
                .as_ref()
                .map(|node| node.as_str())
                .unwrap_or("<none>"),
            review.next_mode,
            format_blocker_ids(&outside_scope),
        );
    }
    let sound_not_ready = request.sound_task_blockers_not_repair_ready(review);
    if !sound_not_ready.is_empty() {
        return format!(
            "task_blocker_ids contains Soundness blockers that are not sound-repair-ready: {}. A Soundness repair task is legal only after the node and its direct noderef dependencies have current Substantiveness and Correspondence Pass.",
            format_blocker_ids(&sound_not_ready)
        );
    }
    // Reset + non-empty action/request lists
    let reset_requested = review.reset != ResetChoice::None;
    if reset_requested
        && (!review.task_blockers.is_empty()
            || !review.override_blockers.is_empty()
            || !review.reset_blockers.is_empty()
            || !review.request_sound_verifier_nodes.is_empty())
    {
        return format!(
            "reset={:?} with non-empty blocker/verifier action lists is illegal: a reset inherits the post-reset blocker state and does not adjudicate individual blockers. Empty task_blocker_ids, override_blocker_ids, reset_blocker_ids, and request_sound_verifier_node_ids when requesting reset.",
            review.reset
        );
    }
    // Protected semantic change scope.
    if review.protected_semantic_change_nodes.is_empty()
        && review.confirm_protected_semantic_change_scope
    {
        return "confirm_protected_semantic_change_scope=true is only legal when protected_semantic_change_node_ids is non-empty".into();
    }
    if !review.protected_semantic_change_nodes.is_empty() {
        if request.phase != Phase::ProofFormalization {
            return "protected_semantic_change_node_ids is only legal during ProofFormalization"
                .into();
        }
        if review.decision != ReviewDecisionKind::Continue {
            return "protected_semantic_change_node_ids is only legal with decision=continue"
                .into();
        }
        if review.reset != ResetChoice::None {
            return "protected_semantic_change_node_ids is not legal with a reset; reset first, then re-review scope".into();
        }
        if review.next_mode != TaskMode::CoarseRestructure {
            return "protected_semantic_change_node_ids requires next_mode=coarse_restructure"
                .into();
        }
        if review.next_active.is_none() {
            return "protected_semantic_change_node_ids requires a concrete next_active node"
                .into();
        }
        let extras: BTreeSet<_> = review
            .protected_semantic_change_nodes
            .difference(&request.approved_target_nodes)
            .cloned()
            .collect();
        if !extras.is_empty() {
            return format!(
                "protected_semantic_change_node_ids contains nodes outside approved_target_nodes: {:?}",
                extras
            );
        }
    }
    // Difficulty updates
    if let Some(node) = review
        .difficulty_updates
        .keys()
        .find(|node| !request.allowed_difficulty_update_nodes.contains(*node))
    {
        return format!(
            "difficulty_updates references node '{}' which is not in allowed_difficulty_update_nodes",
            node.as_str()
        );
    }
    // clear_human_input gating
    if review.clear_human_input && !request.human_input_outstanding {
        return "clear_human_input=true is only allowed when the request signals human_input_outstanding".into();
    }
    if !(request.phase == Phase::ProofFormalization
        && review.decision == ReviewDecisionKind::Continue)
        && (!review.allow_new_obligations || review.must_close_active)
    {
        return "allow_new_obligations/must_close_active are proof-formalization Continue-only controls; outside that state use allow_new_obligations=true and must_close_active=false".into();
    }
    // need_input requires empty blockers / no next_active / unchanged mode
    let need_input = review.decision == ReviewDecisionKind::NeedInput;
    if need_input
        && (!review.task_blockers.is_empty()
            || !review.override_blockers.is_empty()
            || !review.reset_blockers.is_empty()
            || !review.request_sound_verifier_nodes.is_empty()
            || review.next_active.is_some()
            || review.next_mode != request.mode)
    {
        return "decision=need_input requires empty task_blocker_ids, override_blocker_ids, reset_blocker_ids, and request_sound_verifier_node_ids, no next_active, and next_mode equal to the request's current mode".into();
    }
    // allowed_decisions / allowed_resets / allowed_next_modes / kernel_hinted_next_active_nodes
    if !request.allowed_decisions.contains(&review.decision) {
        return format!(
            "decision={:?} is not in allowed_decisions={:?}",
            review.decision, request.allowed_decisions
        );
    }
    if !request.allowed_resets.contains(&review.reset) {
        return format!(
            "reset={:?} is not in allowed_resets={:?}",
            review.reset, request.allowed_resets
        );
    }
    match review.reset {
        ResetChoice::TheoremStatingNode => match review.reset_node.as_ref() {
            Some(node) if request.resettable_theorem_stating_nodes.contains(node) => {}
            Some(node) => {
                return format!(
                    "reset_node='{}' is not in resettable_theorem_stating_nodes={:?}",
                    node.as_str(),
                    request.resettable_theorem_stating_nodes
                );
            }
            None => {
                return "reset=theorem_stating_node requires reset_node to name a resettable coarse node"
                    .into();
            }
        },
        ResetChoice::None | ResetChoice::LastCommit | ResetChoice::LastClean => {
            if review.reset_node.is_some() {
                return "reset_node is only legal with reset=theorem_stating_node".into();
            }
        }
    }
    if !request.allowed_next_modes.contains(&review.next_mode) {
        return format!(
            "next_mode={:?} is not in allowed_next_modes={:?}",
            review.next_mode, request.allowed_next_modes
        );
    }
    if let Some(node) = review.next_active.as_ref() {
        let proof_restructure_anchor = request.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue
            && review.reset == ResetChoice::None
            && matches!(
                review.next_mode,
                TaskMode::Restructure | TaskMode::CoarseRestructure
            );
        if proof_restructure_anchor {
            if !request.current_present_nodes.contains(node) {
                return format!("next_active='{}' is not a present node", node.as_str());
            }
        } else if !request.kernel_hinted_next_active_nodes.contains(node) {
            return format!(
                "next_active='{}' is not in kernel_hinted_next_active_nodes",
                node.as_str()
            );
        }
    }
    // TheoremStating retry-outcome (Invalid/Transport) edge case
    if request.phase == Phase::TheoremStating
        && matches!(
            request.retry_outcome_kind,
            crate::model::RetryOutcomeKind::Invalid | crate::model::RetryOutcomeKind::Transport
        )
        && review.next_active.is_some()
    {
        return format!(
            "next_active='{}' is not allowed when the request is a retry of an Invalid/Transport outcome (phase=theorem_stating); leave next_active empty",
            review.next_active.as_ref().map(|n| n.as_str()).unwrap_or("")
        );
    }
    // TheoremStating AdvancePhase invariants (model.rs:963-976)
    if request.phase == Phase::TheoremStating && review.decision == ReviewDecisionKind::AdvancePhase
    {
        if !request.blockers.is_empty() {
            return format!(
                "decision=advance_phase requires blockers to be empty; the request currently has {} blocker(s). You must address them with continue first, or pick reset.",
                request.blockers.len()
            );
        }
        if review.reset == ResetChoice::LastClean {
            return "decision=advance_phase with reset=last_clean is semantically incoherent (advance says leave this state, last_clean says rewind). Pick one or the other.".into();
        }
        if request.human_input_outstanding && !review.clear_human_input {
            return "decision=advance_phase requires clear_human_input=true when the request signals human_input_outstanding; otherwise the human-input flag would survive into the next phase".into();
        }
    }
    // ProofFormalization: Continue + next_active=None edge case.
    // Cleanup-v2 (audit Finding 1): scope this rule to ProofFormalization
    // only. Phase::Cleanup's only legal next_mode is TaskMode::Cleanup, so
    // applying the same rule there would reject every cleanup Continue
    // unconditionally. In Cleanup the active node is implicit per-task
    // (resolved from `cleanup_audit_tasks[cleanup_next_task].target_node`);
    // legality is enforced by `cleanup_v2_review_fields_legal`.
    if request.phase == Phase::ProofFormalization
        && review.decision == ReviewDecisionKind::Continue
        && review.next_active.is_none()
        && !matches!(
            review.reset,
            ResetChoice::LastClean | ResetChoice::TheoremStatingNode
        )
        && (request.active_node.is_some()
            || !review.task_blockers.is_empty()
            || review.next_mode != TaskMode::Local)
    {
        return "Continue with empty next_active is only legal when the active_node is already None AND task_blocker_ids is empty AND next_mode=local. Either nominate a next_active node or downgrade to local with no blocker tasks.".into();
    }
    // Cleanup Done invariant
    if request.phase == Phase::Cleanup
        && review.decision == ReviewDecisionKind::Done
        && (!request.blockers.is_empty()
            || !review.task_blockers.is_empty()
            || !review.override_blockers.is_empty()
            || !review.reset_blockers.is_empty()
            || !review.request_sound_verifier_nodes.is_empty())
    {
        return "decision=done in Cleanup phase requires the request's blocker set to be empty and all blocker/verifier action lists to be empty".into();
    }
    // Cleanup-v2 (audit Finding 2): consecutive-invalid-worker latch
    if request.phase == Phase::Cleanup && request.cleanup_force_done_view {
        if review.decision != ReviewDecisionKind::Done {
            return "the cleanup consecutive-invalid-worker threshold has fired (cleanup_force_done is set on this request); decision must be done — Continue would just queue another worker burst that is also expected to fail".into();
        }
        if review.cleanup_request_reaudit {
            return "the cleanup consecutive-invalid-worker threshold has fired (cleanup_force_done is set on this request); cleanup_request_reaudit is ignored under the latch and must not be set on the response".into();
        }
    }
    // Non-Continue decisions cap context-mode/focus/work-style fields
    if review.decision != ReviewDecisionKind::Continue {
        if review.next_worker_context_mode != WorkerContextMode::Resume {
            return format!(
                "next_worker_context_mode={:?} is only allowed when decision=continue (decision is {:?}); use 'resume' (default) for other decisions",
                review.next_worker_context_mode, review.decision
            );
        }
        if !review.paper_focus_ranges.is_empty() {
            return format!(
                "paper_focus_ranges is only allowed when decision=continue (decision is {:?}); leave empty for other decisions",
                review.decision
            );
        }
        if review.work_style_hint != WorkerWorkStyleHint::None {
            return format!(
                "work_style_hint={:?} is only allowed when decision=continue (decision is {:?}); use 'none' (default) for other decisions",
                review.work_style_hint, review.decision
            );
        }
        if review.paper_grounding.consulted_cited_ranges
            || !review.paper_grounding.basis_summary.trim().is_empty()
        {
            return format!(
                "paper_grounding is only allowed when decision=continue (decision is {:?}); leave it default (consulted_cited_ranges=false, basis_summary=\"\") for other decisions",
                review.decision
            );
        }
        if review
            .stuck_math_audit
            .as_ref()
            .is_some_and(StuckMathAuditReviewReport::has_content)
        {
            return format!(
                "stuck_math_audit is only allowed when decision=continue and reset=none in active StuckMathAudit mode (decision is {:?}); leave it absent for other decisions",
                review.decision
            );
        }
    }
    // Paper-grounding gate for Continue. Friction = any blockers OR
    // retry_outcome_kind ∈ {Stuck, NeedsRestructure} (per
    // `review_requires_paper_grounding`). Citing ranges always requires
    // the attestation, friction or not.
    if review.decision == ReviewDecisionKind::Continue {
        let friction =
            request.review_requires_paper_grounding() && review.reset == ResetChoice::None;
        if friction && review.paper_focus_ranges.is_empty() {
            return "paper grounding is required for this friction review (blockers present and/or retry_outcome ∈ {stuck, needs_restructure}): include at least one paper_focus_ranges entry, set paper_grounding.consulted_cited_ranges=true, and provide paper_grounding.basis_summary".into();
        }
        if (friction || !review.paper_focus_ranges.is_empty())
            && !review.paper_grounding.consulted_cited_ranges
        {
            return "paper_focus_ranges were cited (or paper grounding is required for this friction review); paper_grounding.consulted_cited_ranges must be true after directly consulting those source-paper ranges".into();
        }
        if (friction || !review.paper_focus_ranges.is_empty())
            && review.paper_grounding.basis_summary.trim().is_empty()
        {
            return "paper_focus_ranges were cited (or paper grounding is required for this friction review); paper_grounding.basis_summary must briefly state what the cited paper text says and why it matters".into();
        }
        if !friction
            && review.paper_focus_ranges.is_empty()
            && (review.paper_grounding.consulted_cited_ranges
                || !review.paper_grounding.basis_summary.trim().is_empty())
        {
            return "paper_grounding is only allowed when paper_focus_ranges is nonempty or the review is in a friction state; clear paper_grounding fields if you have nothing to attest".into();
        }
    }
    if request.stuck_math_audit.active {
        let has_stuck_math_content = review
            .stuck_math_audit
            .as_ref()
            .is_some_and(StuckMathAuditReviewReport::has_content);
        if review.decision == ReviewDecisionKind::Continue && review.reset == ResetChoice::None {
            if !has_stuck_math_content {
                return "StuckMathAudit is active: include a stuck_math_audit object with either non-empty notes or a reviewer_lean_product for continue/reset=none".into();
            }
            if review
                .stuck_math_audit
                .as_ref()
                .is_some_and(|report| !report.reviewer_lean_product_within_limit())
            {
                return "stuck_math_audit.reviewer_lean_product is too large; put larger artifacts on disk and include a compact summary/path".into();
            }
        } else if has_stuck_math_content {
            return "stuck_math_audit content is only legal for continue/reset=none while StuckMathAudit is active; omit it for reset, need_input, advance_phase, or done".into();
        }
    } else if review
        .stuck_math_audit
        .as_ref()
        .is_some_and(StuckMathAuditReviewReport::has_content)
    {
        return "stuck_math_audit content was provided, but this request is not in StuckMathAudit mode".into();
    }
    // Fallback when no specific clause matched
    "no specific clause identified by the diagnostic; see kernel/src/model.rs::review_response_legal for the full rule set".into()
}

fn format_blocker_ids(blockers: &BTreeSet<Blocker>) -> String {
    let ids: Vec<_> = blockers.iter().map(blocker_choice_id).collect();
    format!("[{}]", ids.join(", "))
}

fn normalize_optional_node(raw: &str) -> Option<NodeId> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(NodeId::from(trimmed))
    }
}

fn parse_decision(
    raw: &str,
    allowed: &BTreeSet<ReviewDecisionKind>,
) -> Result<ReviewDecisionKind, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let decision = match normalized.as_str() {
        "continue" => ReviewDecisionKind::Continue,
        "advance_phase" => ReviewDecisionKind::AdvancePhase,
        "need_input" => ReviewDecisionKind::NeedInput,
        "done" => ReviewDecisionKind::Done,
        _ => {
            return Err(format!(
                "reviewer decision must be one of {}",
                format_decisions(allowed)
            ))
        }
    };
    if !allowed.contains(&decision) {
        return Err(format!(
            "reviewer decision must be one of {}",
            format_decisions(allowed)
        ));
    }
    Ok(decision)
}

fn parse_next_mode(raw: &str, allowed: &BTreeSet<TaskMode>) -> Result<TaskMode, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let mode = match normalized.as_str() {
        "global" => TaskMode::Global,
        "targeted" => TaskMode::Targeted,
        "local" => TaskMode::Local,
        "restructure" => TaskMode::Restructure,
        "coarse_restructure" => TaskMode::CoarseRestructure,
        "cleanup" => TaskMode::Cleanup,
        _ => {
            return Err(format!(
                "reviewer next_mode must be one of {}",
                format_modes(allowed)
            ))
        }
    };
    if !allowed.contains(&mode) {
        return Err(format!(
            "reviewer next_mode must be one of {}",
            format_modes(allowed)
        ));
    }
    Ok(mode)
}

fn parse_reset(raw: &str, allowed: &BTreeSet<ResetChoice>) -> Result<ResetChoice, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    let reset = match normalized.as_str() {
        "none" => ResetChoice::None,
        "last_commit" => ResetChoice::LastCommit,
        "last_clean" => ResetChoice::LastClean,
        "theorem_stating_node" => ResetChoice::TheoremStatingNode,
        _ => {
            return Err(format!(
                "reviewer reset must be one of {}",
                format_resets(allowed)
            ))
        }
    };
    if !allowed.contains(&reset) {
        return Err(format!(
            "reviewer reset must be one of {}",
            format_resets(allowed)
        ));
    }
    Ok(reset)
}

fn parse_reset_node(
    raw: &str,
    request: &WrapperRequest,
    reset: ResetChoice,
) -> Result<Option<NodeId>, String> {
    let node = normalize_optional_node(raw);
    if reset == ResetChoice::TheoremStatingNode {
        let Some(node) = node else {
            return Err(
                "reset=theorem_stating_node requires reset_node to name a resettable coarse node"
                    .into(),
            );
        };
        if !request.resettable_theorem_stating_nodes.contains(&node) {
            return Err(format!(
                "reset_node must be one of resettable_theorem_stating_nodes: {:?}",
                request.resettable_theorem_stating_nodes
            ));
        }
        Ok(Some(node))
    } else {
        if node.is_some() {
            return Err("reset_node is only legal with reset=theorem_stating_node".into());
        }
        Ok(None)
    }
}

fn parse_next_worker_context_mode(raw: &str) -> Result<WorkerContextMode, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "resume" => Ok(WorkerContextMode::Resume),
        "fresh" => Ok(WorkerContextMode::Fresh),
        _ => Err("reviewer next_worker_context_mode must be one of ['resume', 'fresh']".into()),
    }
}

fn parse_paper_focus_ranges(raw: &[PaperFocusRange]) -> Result<Vec<PaperFocusRange>, String> {
    if raw
        .iter()
        .any(|range| range.start_line == 0 || range.end_line < range.start_line)
    {
        return Err(
            "paper_focus_ranges must be a list of {start_line >= 1, end_line >= start_line, reason}"
                .into(),
        );
    }
    Ok(raw.to_vec())
}

fn parse_authorized_node_ids(
    raw_field: &Option<Vec<String>>,
    request: &WrapperRequest,
    decision: ReviewDecisionKind,
    reset: ResetChoice,
    next_mode: TaskMode,
) -> Result<BTreeSet<NodeId>, String> {
    let assigns_proof_worker = request.phase == Phase::ProofFormalization
        && decision == ReviewDecisionKind::Continue
        && reset == ResetChoice::None;
    let needs_explicit_list = assigns_proof_worker
        && matches!(
            next_mode,
            TaskMode::Restructure | TaskMode::CoarseRestructure
        );
    let must_be_empty = assigns_proof_worker && matches!(next_mode, TaskMode::Local);

    let normalized: BTreeSet<NodeId> = match raw_field {
        None => BTreeSet::new(),
        Some(ids) => ids
            .iter()
            .map(|raw| raw.trim())
            .filter(|raw| !raw.is_empty())
            .map(NodeId::from)
            .collect(),
    };

    if needs_explicit_list && normalized.is_empty() {
        return Err(
            "authorized_node_ids must be set (non-empty) for Continue responses that assign worker work in Restructure or CoarseRestructure mode"
                .into(),
        );
    }
    if must_be_empty && !normalized.is_empty() {
        return Err(
            "authorized_node_ids must be empty for Local mode; Local does not authorize cross-node existing-node edits"
                .into(),
        );
    }

    let unknown: BTreeSet<_> = normalized
        .difference(&request.current_present_nodes)
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "authorized_node_ids contains nodes that are not present existing nodes: {unknown:?}"
        ));
    }

    Ok(normalized)
}

fn parse_protected_semantic_change_nodes(
    raw_nodes: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
) -> Result<BTreeSet<NodeId>, String> {
    let nodes: BTreeSet<NodeId> = raw_nodes
        .iter()
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
        .map(NodeId::from)
        .collect();
    let extras: BTreeSet<_> = nodes.difference(allowed_nodes).cloned().collect();
    if !extras.is_empty() {
        return Err(format!(
            "protected_semantic_change_node_ids contains nodes outside approved_target_nodes: {:?}",
            extras
        ));
    }
    Ok(nodes)
}

fn parse_work_style_hint(raw: &str) -> Result<WorkerWorkStyleHint, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "none" => Ok(WorkerWorkStyleHint::None),
        "restructure" => Ok(WorkerWorkStyleHint::Restructure),
        _ => Err("reviewer work_style_hint must be one of ['none', 'restructure']".into()),
    }
}

fn resolve_blockers(
    ids: &[String],
    blocker_catalog: &BTreeMap<String, Blocker>,
    field_name: &str,
) -> Result<BTreeSet<Blocker>, String> {
    let normalized_ids = normalized_strings(ids);
    let unknown: Vec<_> = normalized_ids
        .iter()
        .filter(|id| !blocker_catalog.contains_key(*id))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "reviewer referenced unknown blocker ids in {field_name}: {unknown:?}"
        ));
    }
    Ok(normalized_ids
        .into_iter()
        .filter_map(|id| blocker_catalog.get(&id).cloned())
        .collect())
}

fn parse_sound_verifier_node_ids(
    ids: &[String],
    request: &WrapperRequest,
) -> Result<BTreeSet<NodeId>, String> {
    let mut out = BTreeSet::new();
    for raw in normalized_strings(ids) {
        let node = NodeId::from(raw.clone());
        if !request.sound_verifier_requestable_nodes.contains(&node) {
            return Err(format!(
                "request_sound_verifier_node_ids contains '{raw}', but legal Sound verifier targets are {:?}",
                request.sound_verifier_requestable_nodes
            ));
        }
        out.insert(node);
    }
    Ok(out)
}

fn parse_difficulty_updates(
    raw_updates: &BTreeMap<String, String>,
    allowed_nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<NodeId, Update<NodeDifficulty>>, String> {
    let mut normalized = BTreeMap::new();
    for (raw_name, raw_value) in raw_updates {
        let name = raw_name.trim();
        let value = raw_value.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err("reviewer difficulty_updates entries must have non-empty node ids".into());
        }
        if !allowed_nodes.contains(name) {
            return Err(format!(
                "reviewer difficulty_updates nodes must be within {:?}",
                allowed_nodes
            ));
        }
        let difficulty = match value.as_str() {
            "easy" => NodeDifficulty::Easy,
            "hard" => NodeDifficulty::Hard,
            _ => return Err("reviewer difficulty_updates entries must be easy/hard".into()),
        };
        normalized.insert(NodeId::from(name), Update::Set(difficulty));
    }
    Ok(normalized)
}

fn validate_next_active(
    request: &WrapperRequest,
    decision: ReviewDecisionKind,
    reset: ResetChoice,
    next_mode: TaskMode,
    next_active: Option<&NodeId>,
    next_active_coarse: Option<&NodeId>,
) -> Result<(), String> {
    if let Some(node) = next_active {
        let proof_restructure_anchor = request.phase == Phase::ProofFormalization
            && decision == ReviewDecisionKind::Continue
            && reset == ResetChoice::None
            && matches!(
                next_mode,
                TaskMode::Restructure | TaskMode::CoarseRestructure
            );
        // When the reviewer is simultaneously switching the coarse
        // anchor (`next_active_coarse=Some(B)` differing from
        // `request.active_coarse_node`), the request's
        // `kernel_hinted_next_active_nodes` set was projected against
        // the OLD anchor's cone and is typically empty at the moment
        // of a clean cone closure. The kernel's own
        // `review_response_legal` recomputes legality against the new
        // anchor's cone (model.rs:2375), so this CLI pre-check would
        // spuriously reject a legal one-cycle anchor + active switch.
        // Skip the hinted-set check here when an anchor switch is
        // pending; still enforce the present-node floor and let the
        // kernel do the authoritative cone-membership check.
        let anchor_changing =
            next_active_coarse.is_some() && next_active_coarse != request.active_coarse_node.as_ref();
        if proof_restructure_anchor {
            if !request.current_present_nodes.contains(node) {
                return Err("reviewer next_active must be a present node".into());
            }
        } else if anchor_changing {
            if !request.current_present_nodes.contains(node) {
                return Err("reviewer next_active must be a present node".into());
            }
        } else if !request.kernel_hinted_next_active_nodes.contains(node) {
            return Err(format!(
                "reviewer next_active must be one of {:?}",
                request.kernel_hinted_next_active_nodes
            ));
        }
    }
    if next_mode == TaskMode::Targeted {
        // AdvancePhase is a phase transition; the next phase's request
        // re-derives next_active from scratch, so the field is inert
        // for this decision. Without this waiver, a reviewer who picks
        // AdvancePhase from a Targeted-mode state must invent a
        // routing target that the engine then discards — and gets
        // rejected if the targeted_next_active_nodes set is empty.
        let advance_phase_waiver = decision == ReviewDecisionKind::AdvancePhase;
        if request.allow_targeted_without_next_active {
            if next_active.is_some() && !advance_phase_waiver {
                return Err("reviewer next_active must be empty when next_mode is targeted".into());
            }
        } else if !advance_phase_waiver {
            let Some(node) = next_active else {
                return Err("reviewer next_active is required when next_mode is targeted".into());
            };
            if !request.targeted_next_active_nodes.contains(node) {
                return Err(format!(
                    "reviewer targeted next_active must be one of {:?}",
                    request.targeted_next_active_nodes
                ));
            }
        }
    }
    Ok(())
}

fn normalized_strings(values: &[String]) -> Vec<String> {
    let mut out: Vec<_> = values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn format_decisions(allowed: &BTreeSet<ReviewDecisionKind>) -> String {
    format!(
        "{:?}",
        allowed.iter().map(decision_name).collect::<Vec<_>>()
    )
}

fn format_modes(allowed: &BTreeSet<TaskMode>) -> String {
    format!(
        "{:?}",
        allowed.iter().map(task_mode_name).collect::<Vec<_>>()
    )
}

fn format_resets(allowed: &BTreeSet<ResetChoice>) -> String {
    format!("{:?}", allowed.iter().map(reset_name).collect::<Vec<_>>())
}

fn decision_name(decision: &ReviewDecisionKind) -> &'static str {
    match decision {
        ReviewDecisionKind::Continue => "continue",
        ReviewDecisionKind::AdvancePhase => "advance_phase",
        ReviewDecisionKind::NeedInput => "need_input",
        ReviewDecisionKind::Done => "done",
    }
}

fn task_mode_name(mode: &TaskMode) -> &'static str {
    match mode {
        TaskMode::Global => "global",
        TaskMode::Targeted => "targeted",
        TaskMode::Local => "local",
        TaskMode::Restructure => "restructure",
        TaskMode::CoarseRestructure => "coarse_restructure",
        TaskMode::Cleanup => "cleanup",
    }
}

fn reset_name(reset: &ResetChoice) -> &'static str {
    match reset {
        ResetChoice::None => "none",
        ResetChoice::LastCommit => "last_commit",
        ResetChoice::LastClean => "last_clean",
        ResetChoice::TheoremStatingNode => "theorem_stating_node",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlockerKind, BlockerObject, Phase, TaskMode, WrapperRequest};

    #[test]
    fn normalize_review_response_enforces_request_affordances() {
        let blocker = Blocker {
            kind: BlockerKind::PaperFaithfulness,
            object: BlockerObject::Target {
                target: "main_result".into(),
            },
            fingerprint: "fp-main".into(),
            deferred: false,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 9,
            cycle: 5,
            phase: Phase::TheoremStating,
            blockers: BTreeSet::from([blocker.clone()]),
            allowed_decisions: BTreeSet::from([
                ReviewDecisionKind::Continue,
                ReviewDecisionKind::AdvancePhase,
                ReviewDecisionKind::NeedInput,
            ]),
            allowed_next_modes: BTreeSet::from([TaskMode::Global, TaskMode::Targeted]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            targeted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            allowed_override_blockers: BTreeSet::from([blocker.clone()]),
            allowed_difficulty_update_nodes: BTreeSet::from(["main_node".into()]),
            human_input_outstanding: true,
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reason: "paper-grounded route".into(),
                comments: "focus the current blocker".into(),
                task_blocker_ids: vec![],
                override_blocker_ids: vec![blocker_choice_id(&blocker)],
                reset_blocker_ids: vec![],
                next_active: "main_node".into(),
                reset: "none".into(),
                next_mode: "targeted".into(),
                difficulty_updates: BTreeMap::from([("main_node".into(), "hard".into())]),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                clear_human_input: true,
                paper_focus_ranges: vec![PaperFocusRange {
                    start_line: 1,
                    end_line: 1,
                    reason: "test".to_string(),
                }],
                paper_grounding: PaperGrounding {
                    consulted_cited_ranges: true,
                    basis_summary: "test basis".to_string(),
                },
                ..RawReviewPayload::default()
            },
        };

        let normalized = normalize_review_response(&input).expect("normalize review");
        assert_eq!(normalized.response.decision, ReviewDecisionKind::Continue);
        assert_eq!(normalized.response.reason, "paper-grounded route");
        assert_eq!(normalized.response.next_mode, TaskMode::Targeted);
        // Option C (2026-06-04): override_blockers is now always empty
        // even when the raw payload carries `override_blocker_ids` —
        // the field is silently dropped at normalization time.
        let _ = blocker; // unused after override→Pass retirement
        assert!(
            normalized.response.override_blockers.is_empty(),
            "override_blockers must be empty under Option C (override→Pass retired)"
        );
        assert_eq!(
            normalized.response.difficulty_updates.get("main_node"),
            Some(&Update::Set(NodeDifficulty::Hard))
        );
    }

    #[test]
    fn normalize_review_response_allows_partial_blocker_actions() {
        let paper = Blocker {
            kind: BlockerKind::PaperFaithfulness,
            object: BlockerObject::Target {
                target: "main_result".into(),
            },
            fingerprint: "fp-main".into(),
            deferred: false,
        };
        let subst = Blocker {
            kind: BlockerKind::Substantiveness,
            object: BlockerObject::Node { node: "a".into() },
            fingerprint: "sub-a".into(),
            deferred: false,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 11,
            cycle: 7,
            phase: Phase::TheoremStating,
            blockers: BTreeSet::from([paper.clone(), subst]),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Global]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reason: "route one concrete blocker".into(),
                comments: "leave the unrelated blocker live".into(),
                task_blocker_ids: vec![blocker_choice_id(&paper)],
                reset: "none".into(),
                next_mode: "global".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                paper_focus_ranges: vec![PaperFocusRange {
                    start_line: 1,
                    end_line: 1,
                    reason: "test".into(),
                }],
                paper_grounding: PaperGrounding {
                    consulted_cited_ranges: true,
                    basis_summary: "test basis".into(),
                },
                ..RawReviewPayload::default()
            },
        };

        let normalized = normalize_review_response(&input).expect("normalize partial action list");
        assert_eq!(normalized.response.task_blockers, BTreeSet::from([paper]));
    }

    #[test]
    fn normalize_review_response_accepts_reviewer_requested_sound_verifier() {
        let blocker = Blocker {
            kind: BlockerKind::Soundness,
            object: BlockerObject::Node { node: "a".into() },
            fingerprint: "sa".into(),
            deferred: false,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 12,
            cycle: 8,
            phase: Phase::ProofFormalization,
            mode: TaskMode::Local,
            blockers: BTreeSet::from([blocker]),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            sound_verifier_requestable_nodes: BTreeSet::from(["a".into()]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reason: "need real Sound evidence".into(),
                comments: "request re-verification".into(),
                request_sound_verifier_node_ids: vec!["a".into()],
                reset: "none".into(),
                next_mode: "local".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                paper_focus_ranges: vec![PaperFocusRange {
                    start_line: 1,
                    end_line: 1,
                    reason: "test".into(),
                }],
                paper_grounding: PaperGrounding {
                    consulted_cited_ranges: true,
                    basis_summary: "test basis".into(),
                },
                ..RawReviewPayload::default()
            },
        };

        let normalized = normalize_review_response(&input).expect("normalize verifier request");
        assert_eq!(
            normalized.response.request_sound_verifier_nodes,
            BTreeSet::from(["a".into()])
        );
        assert!(normalized.response.task_blockers.is_empty());
    }

    #[test]
    fn normalize_need_input_requires_empty_task_and_override_blockers() {
        let blocker = Blocker {
            kind: BlockerKind::Soundness,
            object: BlockerObject::Node {
                node: "main_node".into(),
            },
            fingerprint: "fp-main".into(),
            deferred: false,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 10,
            cycle: 6,
            phase: Phase::ProofFormalization,
            mode: TaskMode::Local,
            blockers: BTreeSet::from([blocker.clone()]),
            allowed_decisions: BTreeSet::from([
                ReviewDecisionKind::Continue,
                ReviewDecisionKind::NeedInput,
            ]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local, TaskMode::Restructure]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            allowed_difficulty_update_nodes: BTreeSet::from(["main_node".into()]),
            ..WrapperRequest::default()
        };
        let ok = ReviewNormalizationInput {
            request: request.clone(),
            raw_payload: RawReviewPayload {
                decision: "need_input".into(),
                reset: "none".into(),
                next_mode: "local".into(),
                difficulty_updates: BTreeMap::from([("main_node".into(), "hard".into())]),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                ..RawReviewPayload::default()
            },
        };
        let normalized = normalize_review_response(&ok).expect("normalize need_input");
        assert_eq!(normalized.response.decision, ReviewDecisionKind::NeedInput);
        assert!(normalized.response.task_blockers.is_empty());
        assert!(normalized.response.override_blockers.is_empty());

        let illegal = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "need_input".into(),
                reset: "none".into(),
                next_mode: "local".into(),
                task_blocker_ids: vec![blocker_choice_id(&blocker)],
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                ..RawReviewPayload::default()
            },
        };
        let err =
            normalize_review_response(&illegal).expect_err("need_input task blockers illegal");
        assert!(err.contains("not legal"));
    }

    #[test]
    fn normalize_need_input_requires_empty_reset_blockers() {
        let blocker = Blocker {
            kind: BlockerKind::Soundness,
            object: BlockerObject::Node {
                node: "main_node".into(),
            },
            fingerprint: "fp-main".into(),
            deferred: false,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 11,
            cycle: 7,
            phase: Phase::ProofFormalization,
            mode: TaskMode::Local,
            blockers: BTreeSet::from([blocker.clone()]),
            allowed_reset_blockers: BTreeSet::from([blocker.clone()]),
            allowed_decisions: BTreeSet::from([
                ReviewDecisionKind::Continue,
                ReviewDecisionKind::NeedInput,
            ]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local, TaskMode::Restructure]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            allowed_difficulty_update_nodes: BTreeSet::from(["main_node".into()]),
            ..WrapperRequest::default()
        };
        let illegal = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "need_input".into(),
                reset: "none".into(),
                next_mode: "local".into(),
                reset_blocker_ids: vec![blocker_choice_id(&blocker)],
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                ..RawReviewPayload::default()
            },
        };
        let err =
            normalize_review_response(&illegal).expect_err("need_input reset blockers illegal");
        assert!(err.contains("not legal"));
        assert!(err.contains("reset_blocker_ids"));
    }

    #[test]
    fn normalize_review_response_requires_explicit_gate_fields() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 12,
            cycle: 8,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "local".into(),
                next_active: "main_node".into(),
                ..RawReviewPayload::default()
            },
        };

        let err = normalize_review_response(&input).expect_err("missing gate fields rejected");
        assert!(err.contains("allow_new_obligations"));
    }

    #[test]
    fn normalize_review_response_rejects_missing_authorized_nodes_for_restructure() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 5,
            phase: Phase::ProofFormalization,
            active_node: Some("A".into()),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([
                TaskMode::Local,
                TaskMode::Restructure,
                TaskMode::CoarseRestructure,
            ]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["A".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            current_present_nodes: BTreeSet::from(["A".into()]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "restructure".into(),
                next_active: "A".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                authorized_node_ids: None,
                ..RawReviewPayload::default()
            },
        };
        let err = normalize_review_response(&input).expect_err("must reject missing field");
        assert!(
            err.contains("authorized_node_ids"),
            "diagnostic should name the field; got: {err}"
        );
    }

    #[test]
    fn normalize_review_response_rejects_unknown_node_in_authorized_node_ids() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 5,
            phase: Phase::ProofFormalization,
            active_node: Some("A".into()),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Restructure]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["A".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            current_present_nodes: BTreeSet::from(["A".into()]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "restructure".into(),
                next_active: "A".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                authorized_node_ids: Some(vec!["A".into(), "Ghost".into()]),
                ..RawReviewPayload::default()
            },
        };
        let err = normalize_review_response(&input).expect_err("must reject unknown node");
        assert!(
            err.contains("not present existing nodes") && err.contains("Ghost"),
            "diagnostic should name the unknown node; got: {err}"
        );
    }

    #[test]
    fn normalize_review_response_rejects_authorized_node_ids_under_local_mode() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 5,
            phase: Phase::ProofFormalization,
            active_node: Some("A".into()),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["A".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            current_present_nodes: BTreeSet::from(["A".into()]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "local".into(),
                next_active: "A".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                authorized_node_ids: Some(vec!["A".into()]),
                ..RawReviewPayload::default()
            },
        };
        let err = normalize_review_response(&input).expect_err("Local must reject non-empty list");
        assert!(
            err.contains("Local") || err.contains("local"),
            "diagnostic should mention Local mode; got: {err}"
        );
    }

    #[test]
    fn normalize_review_response_parses_protected_semantic_scope() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 11,
            cycle: 7,
            phase: Phase::ProofFormalization,
            active_node: Some("A".into()),
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local, TaskMode::CoarseRestructure]),
            kernel_hinted_next_active_nodes: BTreeSet::from(["A".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            approved_target_nodes: BTreeSet::from(["A".into(), "B".into()]),
            current_present_nodes: BTreeSet::from(["A".into(), "B".into()]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request: request.clone(),
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "coarse_restructure".into(),
                next_active: "A".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                protected_semantic_change_node_ids: vec!["B".into()],
                confirm_protected_semantic_change_scope: true,
                authorized_node_ids: Some(vec!["A".into(), "B".into()]),
                ..RawReviewPayload::default()
            },
        };

        let normalized = normalize_review_response(&input).expect("normalize protected scope");
        assert_eq!(
            normalized.response.protected_semantic_change_nodes,
            BTreeSet::from(["B".into()])
        );
        assert!(normalized.response.confirm_protected_semantic_change_scope);

        let illegal = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "continue".into(),
                reset: "none".into(),
                next_mode: "coarse_restructure".into(),
                next_active: "A".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                protected_semantic_change_node_ids: vec!["C".into()],
                authorized_node_ids: Some(vec!["A".into(), "B".into()]),
                ..RawReviewPayload::default()
            },
        };
        let err = normalize_review_response(&illegal)
            .expect_err("outside protected scope should be rejected");
        assert!(err.contains("outside approved_target_nodes"));
    }

    /// Cleanup-v2 (audit Finding 2): the raw payload must be able to
    /// deserialize cleanup_dismiss_tasks, cleanup_next_task, and
    /// cleanup_request_reaudit. Pre-fix these fields were absent from
    /// `RawReviewPayload` so even a well-formed reviewer JSON was lost
    /// at the kernel boundary.
    #[test]
    fn raw_review_payload_deserializes_cleanup_v2_controls() {
        let raw: RawReviewPayload = serde_json::from_str(
            r#"{
                "decision": "continue",
                "reset": "none",
                "next_mode": "cleanup",
                "allow_new_obligations": true,
                "must_close_active": false,
                "cleanup_dismiss_tasks": [
                    {"task_index": 0, "reason": "redundant after burst 1"},
                    {"task_index": 2, "reason": "second-look: wrapper doesn't actually generalize"}
                ],
                "cleanup_next_task": 1,
                "cleanup_request_reaudit": false
            }"#,
        )
        .expect("deserialize");
        assert_eq!(raw.cleanup_dismiss_tasks.len(), 2);
        assert_eq!(raw.cleanup_dismiss_tasks[0].task_index, 0);
        assert_eq!(
            raw.cleanup_dismiss_tasks[0].reason,
            "redundant after burst 1"
        );
        assert_eq!(raw.cleanup_next_task, Some(1));
        assert!(!raw.cleanup_request_reaudit);
    }

    /// Cleanup-v2 (audit Finding 2): a Done response that requests
    /// re-audit must be plumbed through normalization. Pre-fix
    /// `cleanup_request_reaudit` was hard-coded to `false` in the
    /// normalizer, ignoring whatever the reviewer emitted.
    #[test]
    fn normalize_review_response_carries_cleanup_request_reaudit_on_done() {
        // Set up a Phase::Cleanup Done with no blockers and round 1
        // (below CLEANUP_AUDIT_MAX_ROUNDS = 2).
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            cleanup_audit_round_view: 1,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Done]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let input = ReviewNormalizationInput {
            request,
            raw_payload: RawReviewPayload {
                decision: "done".into(),
                reset: "none".into(),
                next_mode: "cleanup".into(),
                allow_new_obligations: Some(true),
                must_close_active: Some(false),
                cleanup_request_reaudit: true,
                ..RawReviewPayload::default()
            },
        };
        let out = normalize_review_response(&input).expect("normalize");
        assert!(out.response.cleanup_request_reaudit);
    }

    /// Cleanup-v2 (audit Finding 3): a Continue response in
    /// Phase::Cleanup with `cleanup_next_task` referencing a non-Pending
    /// task is rejected by `review_response_legal`.
    #[test]
    fn review_response_legal_rejects_cleanup_next_task_for_non_pending() {
        use crate::model::{
            CleanupAuditTask, CleanupReplacement, CleanupTaskConfidence, CleanupTaskKind,
            CleanupTaskStatus,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            cleanup_audit_tasks_view: vec![CleanupAuditTask {
                target_node: "A".into(),
                rationale: "wraps".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::Substitution {
                    replacement: CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                },
                // Already terminal — Continue should not dispatch.
                status: CleanupTaskStatus::Completed,
                audit_origin_round: 1,
            }],
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Cleanup,
            cleanup_next_task: Some(0),
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "review_response_legal must reject cleanup_next_task pointing at a non-Pending task"
        );
    }

    /// Cleanup-v2 (audit Finding 3): `cleanup_dismiss_tasks` index out of
    /// range is rejected.
    #[test]
    fn review_response_legal_rejects_cleanup_dismiss_tasks_out_of_range() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            cleanup_audit_tasks_view: Vec::new(),
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Cleanup,
            cleanup_dismiss_tasks: vec![(5, "stale".into())],
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "cleanup_dismiss_tasks index out of range must be rejected"
        );
    }

    /// Cleanup-v2 (audit Finding 3): `cleanup_request_reaudit` is only
    /// legal on Done in Cleanup phase with round < max.
    #[test]
    fn review_response_legal_rejects_reaudit_at_max_round() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            cleanup_audit_round_view: crate::model::CLEANUP_AUDIT_MAX_ROUNDS,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Done]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Done,
            next_mode: TaskMode::Cleanup,
            cleanup_request_reaudit: true,
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "cleanup_request_reaudit at max round must be rejected"
        );
    }

    /// Cleanup-v2 (audit Finding 3): cleanup_dismiss_tasks outside
    /// Phase::Cleanup is rejected.
    #[test]
    fn review_response_legal_rejects_cleanup_dismiss_tasks_outside_cleanup_phase() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            cleanup_dismiss_tasks: vec![(0, "spurious".into())],
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "cleanup_dismiss_tasks outside Phase::Cleanup must be rejected"
        );
    }

    /// Cleanup-v2 (audit Finding 1): a Cleanup Continue with
    /// `next_active=None`, `cleanup_next_task=Some(idx)`, and
    /// `next_mode=Cleanup` must be LEGAL. Pre-fix the proof/cleanup
    /// `next_active=None` rejection at `model.rs:1880-1888` fired
    /// unconditionally because Phase::Cleanup's only legal next_mode
    /// is `TaskMode::Cleanup` (not `Local`), so the
    /// `next_mode != Local` arm always evaluated to true.
    #[test]
    fn review_response_legal_cleanup_continue_with_next_task_and_no_next_active_is_legal() {
        use crate::model::{
            CleanupAuditTask, CleanupReplacement, CleanupTaskConfidence, CleanupTaskKind,
            CleanupTaskStatus,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            cleanup_audit_tasks_view: vec![CleanupAuditTask {
                target_node: "A".into(),
                rationale: "wraps".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::Substitution {
                    replacement: CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            }],
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Cleanup,
            next_active: None,
            cleanup_next_task: Some(0),
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            request.review_response_legal(&response),
            "Cleanup Continue with next_active=None, cleanup_next_task=Some(0), next_mode=Cleanup must be legal (the cleanup dispatch resolves the active node per-task)"
        );
    }

    /// Cleanup-v2 (audit Finding 1): ProofFormalization Continue with
    /// `next_active=None`, `next_mode=Restructure`, no task_blockers
    /// must still be rejected. This guards the existing proof-phase
    /// invariant under the scoped condition.
    #[test]
    fn review_response_legal_proof_continue_without_next_active_with_non_local_mode_is_still_rejected(
    ) {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::ProofFormalization,
            active_node: None,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([
                TaskMode::Local,
                TaskMode::Restructure,
                TaskMode::CoarseRestructure,
            ]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Restructure,
            next_active: None,
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "ProofFormalization Continue with empty next_active under a non-Local mode must still be rejected"
        );
    }

    /// Cleanup-v2 (audit Finding 2): when `cleanup_force_done_view` is
    /// true on the Review request, the reviewer's only legal decision
    /// is Done — Continue (with or without dispatch) is rejected.
    #[test]
    fn review_response_legal_rejects_continue_when_cleanup_force_done_is_set() {
        use crate::model::{
            CleanupAuditTask, CleanupReplacement, CleanupTaskConfidence, CleanupTaskKind,
            CleanupTaskStatus,
        };
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Done]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            cleanup_force_done_view: true,
            cleanup_audit_tasks_view: vec![CleanupAuditTask {
                target_node: "A".into(),
                rationale: "wraps".into(),
                confidence: CleanupTaskConfidence::High,
                kind: CleanupTaskKind::Substitution {
                    replacement: CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                },
                status: CleanupTaskStatus::Pending,
                audit_origin_round: 1,
            }],
            ..WrapperRequest::default()
        };
        let response = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Cleanup,
            cleanup_next_task: Some(0),
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response),
            "Continue must be rejected when cleanup_force_done_view is true"
        );
    }

    /// Cleanup-v2 (audit Finding 2): when `cleanup_force_done_view` is
    /// true, Done is legal but Done+request_reaudit is NOT (the latch
    /// overrides re-audit; accepting the response would silently drop
    /// the request).
    #[test]
    fn review_response_legal_rejects_done_with_reaudit_when_cleanup_force_done_is_set() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::Cleanup,
            cleanup_audit_round_view: 1,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Done]),
            allowed_next_modes: BTreeSet::from([TaskMode::Cleanup]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            cleanup_force_done_view: true,
            ..WrapperRequest::default()
        };
        let response_done_only = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Done,
            next_mode: TaskMode::Cleanup,
            cleanup_request_reaudit: false,
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            request.review_response_legal(&response_done_only),
            "Done alone must be legal under cleanup_force_done_view"
        );
        let response_done_with_reaudit = crate::model::ReviewResponse {
            decision: ReviewDecisionKind::Done,
            next_mode: TaskMode::Cleanup,
            cleanup_request_reaudit: true,
            ..crate::model::ReviewResponse::default()
        };
        assert!(
            !request.review_response_legal(&response_done_with_reaudit),
            "Done+cleanup_request_reaudit must be rejected when cleanup_force_done_view is true"
        );
    }

    /// Finding D: when the reviewer picks AdvancePhase from a Targeted-mode
    /// state, the validator must accept `next_active=None` even though
    /// the regular Targeted-mode rule requires a routing target. The
    /// next phase's request rederives next_active from scratch, so the
    /// field is inert on this code path.
    #[test]
    fn validate_next_active_advance_phase_targeted_no_next_active_passes() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::TheoremStating,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::AdvancePhase]),
            allowed_next_modes: BTreeSet::from([TaskMode::Targeted]),
            targeted_next_active_nodes: BTreeSet::new(),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let result = validate_next_active(
            &request,
            ReviewDecisionKind::AdvancePhase,
            ResetChoice::None,
            TaskMode::Targeted,
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "AdvancePhase+Targeted+next_active=None must pass (got {:?})",
            result
        );
    }

    /// Finding D regression guard: the Continue path must still reject
    /// Targeted-mode requests without a `next_active` — the waiver is
    /// narrowly scoped to AdvancePhase.
    #[test]
    fn validate_next_active_continue_targeted_no_next_active_still_rejects() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            id: 1,
            cycle: 1,
            phase: Phase::TheoremStating,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Targeted]),
            targeted_next_active_nodes: BTreeSet::from(["main_node".into()]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let result = validate_next_active(
            &request,
            ReviewDecisionKind::Continue,
            ResetChoice::None,
            TaskMode::Targeted,
            None,
            None,
        );
        assert!(
            result.is_err(),
            "Continue+Targeted+next_active=None must still be rejected (waiver is AdvancePhase-only)"
        );
    }
}
