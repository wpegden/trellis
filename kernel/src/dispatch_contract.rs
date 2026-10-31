//! Build task contracts through the existing reviewer/worker transition rules.
//! This module does not invent an alternate acceptance or phase-advance gate.
use crate::dispatch::{CompilerCapability, DependencyGraph, EditScope, PreparedAssignment, WorkerTaskKind};
use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Permit added ordinary import lines without granting edits to the rest of
/// the theorem's prefix. The ordinary FILESPEC, semantic and dependency checks
/// still validate the resulting module; this is only a cheap write-scope guard.
pub fn closure_prefix_allows_imports(before: &str, after: &str) -> bool {
    fn parts(prefix: &str) -> (String, BTreeSet<String>) {
        let mut fixed = String::new();
        let mut imports = BTreeSet::new();
        for line in prefix.split_inclusive('\n') {
            let code = line.split_once("--").map_or(line, |(code, _)| code).trim();
            let mut words = code.split_whitespace();
            let modules: Vec<_> = if words.next() == Some("import") { words.collect() } else { Vec::new() };
            let module_name = |module: &&str| module.split('.').all(|part| {
                let mut chars = part.chars();
                chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                    && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '\'')
            });
            if !modules.is_empty() && modules.iter().all(module_name) {
                imports.extend(modules.into_iter().map(str::to_owned));
            } else {
                fixed.push_str(line);
            }
        }
        (fixed, imports)
    }
    let (before_fixed, before_imports) = parts(before);
    let (after_fixed, after_imports) = parts(after);
    before_fixed == after_fixed && before_imports.is_subset(&after_imports)
}

/// Import additions can affect name resolution even when signature text is
/// unchanged. Reuse the ordinary typed semantic fingerprint, not import text.
pub fn lean_closure_statement_error(before: Option<&String>, after: Option<&String>) -> Option<String> {
    let semantic = |value: Option<&String>| -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(value?).ok()?;
        value.get("lean_semantic_closure")?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
    };
    match (semantic(before), semantic(after)) {
        (Some(before), Some(after)) if before == after => None,
        (Some(_), Some(_)) => Some("Lean-closure work changed the elaborated theorem statement; keep its meaning unchanged when adding imports".into()),
        _ => Some("Lean-closure statement preservation could not be checked because baseline or current semantic evidence is missing; report this checker failure in system_feedback".into()),
    }
}

/// Inspect raw fields before ordinary normalization can discard unknown or
/// inapplicable metadata. Reporting failure does not grant planning authority.
pub fn lean_closure_payload_errors(raw: &serde_json::Value) -> Vec<String> {
    let Some(obj) = raw.as_object() else { return vec!["Lean-closure result must be an object".into()]; };
    let mut errors = Vec::new();
    if !matches!(obj.get("outcome").and_then(|v| v.as_str()), Some("valid" | "invalid" | "stuck")) {
        errors.push("Lean-closure results must be valid, invalid or stuck; report scope limitations as stuck".into());
    }
    for (key, value) in obj {
        let empty = value.is_null() || value.as_str() == Some("") || value.as_bool() == Some(false)
            || value.as_array().is_some_and(|v| v.is_empty())
            || value.as_object().is_some_and(|v| v.is_empty());
        if !matches!(key.as_str(), "outcome" | "summary" | "comments" | "system_feedback" | "invalid_kind") && !empty {
            errors.push(format!("Lean-closure results cannot include {key}; remove it and report any need for broader scope as stuck in summary"));
        }
    }
    errors
}

/// An inexpensive prefix/write-set check before authoritative compilation.
/// Ordinary proof-delta, closure and phase-specific validation still follow.
pub fn lean_closure_source_errors(
    repo: &std::path::Path, before: &BTreeMap<String, String>,
    contents: &BTreeMap<String, String>, node: &NodeId, require_closed: bool,
) -> Vec<String> {
    let filename = format!("{node}.lean");
    let after = crate::snapshot_tablet_dir(repo);
    let changed: Vec<_> = before.keys().chain(after.keys()).collect::<BTreeSet<_>>()
        .into_iter().filter(|name| before.get(*name) != after.get(*name)).collect();
    // A checker attempt can refresh these derived documents before a later
    // validation fails. On retry, recognize only existing files containing
    // the exact current render; this grants no authored documentation scope.
    let rendered = if changed.iter().any(|name| {
        crate::is_kernel_regenerated_tablet_support_filename(name)
            && before.contains_key(*name) && after.contains_key(*name)
    }) {
        crate::build_tablet_support_snapshot_from_repo(repo).ok()
            .map(|snapshot| crate::build_tablet_support_render_output(repo, &snapshot))
    } else {
        None
    };
    let mut errors = Vec::new();
    for name in changed {
        let expected = rendered.as_ref().and_then(|render| match name.as_str() {
            "INDEX.md" => Some(render.index_md_content.as_bytes()),
            "README.md" => Some(render.readme_md_content.as_bytes()),
            _ => None,
        });
        let path = repo.join("Tablet").join(name);
        let regenerated = before.contains_key(name) && after.contains_key(name)
            && expected.is_some_and(|expected| {
                std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file())
                    && std::fs::read(&path).is_ok_and(|bytes| bytes.as_slice() == expected)
            });
        if name != &filename && !regenerated {
            errors.push(format!("Lean-closure work may change only {filename}; unauthorized edit: {name}"));
        }
    }
    let Some(base) = contents.get(&filename) else {
        errors.push(format!("Lean-closure task has no original statement bytes for {node}"));
        return errors;
    };
    let check = || -> Result<(), String> {
        let current = std::fs::read_to_string(repo.join("Tablet").join(&filename))
            .map_err(|e| format!("cannot read assigned Lean-closure theorem: {e}"))?;
        let original = crate::filespec_split::split(base, node.as_str())?;
        let proposed = crate::filespec_split::split(&current, node.as_str())?;
        if !closure_prefix_allows_imports(&base[..original.body_marker_end_byte], &current[..proposed.body_marker_end_byte]) {
            return Err("Lean-closure work may add imports, but cannot remove existing imports or change preamble options, namespace commands or the theorem statement; a term-local heartbeat option wholly below -- BODY does not change that prefix".into());
        }
        if require_closed {
            if let Some(token) = crate::sidecar::sidecar_body_ban_scan(&current[proposed.body_marker_end_byte..]) {
                return Err(format!("Lean-closure proof body contains banned token: {token}"));
            }
        }
        Ok(())
    };
    if let Err(error) = check() { errors.push(error); }
    errors
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PreparedWorker {
    pub assignment: PreparedAssignment,
    /// Persist in the attempt's private runtime. It has no dispatch ledger and
    /// therefore cannot launch sibling tasks or modify canonical recovery state.
    pub private_state: ProtocolState,
}

/// The task-local context actually authorized by the ordinary engine. It is
/// reconstructed on current canonical mathematics at acceptance; starting a
/// new reviewer/cycle here would spuriously revoke independent in-flight work.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthorizedWorker {
    pending_task: Option<PendingTask>,
    active_node: Option<NodeId>,
    held_target: Option<NodeId>,
    target_edit_mode: TargetEditMode,
    proof_edit_mode: ProofEditMode,
    active_coarse_node: Option<NodeId>,
    cycles_in_coarse_repair_mode: u32,
    reviewer_comments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assumption_authoring: Option<AssumptionAuthoringContext>,
}

impl AuthorizedWorker {
    pub fn capture(state: &ProtocolState) -> Self {
        Self { pending_task: state.pending_task.clone(), active_node: state.active_node.clone(),
            held_target: state.held_target.clone(), target_edit_mode: state.target_edit_mode,
            proof_edit_mode: state.proof_edit_mode, active_coarse_node: state.active_coarse_node.clone(),
            cycles_in_coarse_repair_mode: state.cycles_in_coarse_repair_mode,
            reviewer_comments: state.reviewer_comments.clone(),
            assumption_authoring: state.assumption_authoring.clone() }
    }
    pub(crate) fn coarse_goal(&self) -> Option<&NodeId> { self.active_coarse_node.as_ref() }
    pub fn install(&self, state: &mut ProtocolState) -> Result<(), String> {
        if let Some(authoring) = &self.assumption_authoring {
            // Initial admission consumes the auditor's current candidate;
            // retries retain the same already-enacted native context.
            if state.assumption_authoring.as_ref() != Some(authoring)
                && state.pending_authoring_candidate.trim() != authoring.candidate_invariant
            {
                return Err("assumption-authoring candidate changed after reservation".into());
            }
            state.assumption_authoring = Some(authoring.clone());
            state.pending_authoring_candidate.clear();
        }
        state.pending_task = self.pending_task.clone();
        let current = state.global_blockers();
        if let Some(task) = state.pending_task.as_mut() {
            task.task_blockers.retain(|b| current.contains(b));
        }
        state.active_node = self.active_node.clone();
        state.held_target = self.held_target.clone();
        state.target_edit_mode = self.target_edit_mode;
        state.proof_edit_mode = self.proof_edit_mode;
        state.active_coarse_node = self.active_coarse_node.clone();
        state.cycles_in_coarse_repair_mode = self.cycles_in_coarse_repair_mode;
        state.reviewer_comments = self.reviewer_comments.clone();
        Ok(())
    }
}

impl PreparedWorker {
    pub fn bind_reserved_task(mut self, task: &crate::dispatch::WorkerTask) -> Result<Self, String> {
        let context = task.assignment.request.inline()?.task_execution.clone()
            .ok_or("reserved task has no host execution binding")?;
        // Compare forward from the prepared request. Attaching execution
        // narrows closure/NL-only contracts, but must retain every ordinary
        // feature overlay and must not hide a changed reserved contract.
        let mut expected = self.assignment.clone();
        let request = expected.request.make_mut()?;
        request.task_execution = Some(context.clone());
        crate::populate_request_prompt_contracts(request, None);
        if task.assignment != expected {
            return Err("reserved worker assignment differs from the prepared task".into());
        }
        self.private_state.worker_execution = Some(context);
        self.private_state.in_flight_request = Some(Box::new(task.assignment.request.inline()?.clone()));
        self.assignment = task.assignment.clone();
        self.private_state.validate()?;
        Ok(self)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentOptions {
    pub assignment_id: String,
    pub profile: String,
    pub timeout_seconds: Option<u64>,
    pub compiler: CompilerCapability,
    pub task_kind: WorkerTaskKind,
    pub helper_prefix: String,
    pub base_snapshot: String,
    pub input_hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub evidence_paths: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proof_body_rebase_inputs: BTreeMap<String, String>,
}

fn validate_evidence_options(canonical: &ProtocolState, options: &AssignmentOptions) -> Result<(), String> {
    if options.evidence_paths.is_empty() { return Ok(()); }
    if canonical.phase != Phase::Modeling || !canonical.model_preservation_enabled
        || options.task_kind != WorkerTaskKind::Ordinary || options.compiler != CompilerCapability::Managed
    {
        return Err("evidence_paths requires ordinary Modeling with model preservation and compiler=managed".into());
    }
    if options.evidence_paths.len() > 64 { return Err("evidence_paths permits at most 64 exact files".into()); }
    for path in &options.evidence_paths {
        crate::model_preservation::validate_evidence_path(path)?;
        if options.input_hashes.get(path).is_none_or(|hash| hash != "absent"
            && (hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())))
        {
            return Err(format!("missing exact evidence input identity: {path}"));
        }
    }
    Ok(())
}

/// The caller supplies observed Lean/definition edges, including explicit empty
/// sets for leaf nodes. The kernel adds every recorded semantic dependency.
pub fn dependency_graph(
    state: &ProtocolState,
    observed: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> Result<DependencyGraph, String> {
    let mut edges = BTreeMap::new();
    for node in &state.live.present_nodes {
        let mut deps = observed.get(node).cloned()
            .ok_or_else(|| format!("missing Lean/definition dependency observation for {node}"))?;
        deps.extend(state.deps.get(node).into_iter().flatten().cloned());
        if !deps.is_subset(&state.live.present_nodes) {
            return Err(format!("dependency observation for {node} names an absent node"));
        }
        edges.insert(node.clone(), deps);
    }
    Ok(DependencyGraph { edges })
}

pub fn prepare_worker(
    canonical: &ProtocolState,
    review: ReviewResponse,
    graph: &DependencyGraph,
    options: AssignmentOptions,
) -> Result<PreparedWorker, String> {
    prepare_worker_context(canonical, review, graph, options)
}

/// Assignment-only restrictions shared by the reviewer self-check and native
/// preparation. Ordinary reviewer actions remain legal outside assignments.
pub fn validate_assignment_review(review: &ReviewResponse) -> Result<(), String> {
    if review.decision != ReviewDecisionKind::Continue || review.reset != ResetChoice::None {
        return Err("assignment review requires decision=continue and reset=none; submit phase changes and resets in action for the exclusive drain path".into());
    }
    let fields: Vec<_> = [
        ("reset_blocker_ids", !review.reset_blockers.is_empty()),
        ("request_sound_verifier_node_ids", !review.request_sound_verifier_nodes.is_empty()),
        ("memory_challenges", !review.memory_challenges.is_empty()),
        ("sidecar_queue_add", !review.sidecar_queue_add.is_empty()),
        ("sidecar_queue_remove", !review.sidecar_queue_remove.is_empty()),
        ("audit_request", review.audit_request.is_some()),
        ("global_repair_request", review.global_repair_request.is_some()),
        ("modeling_request", review.modeling_request.is_some()),
        ("dismiss_audit_plan", review.dismiss_audit_plan),
        ("dismissed_tasks", !review.dismissed_tasks.is_empty()),
        ("cleanup_dismiss_tasks", !review.cleanup_dismiss_tasks.is_empty()),
        ("clear_human_input", review.clear_human_input),
    ].into_iter().filter_map(|(field, present)| present.then_some(field)).collect();
    if !fields.is_empty() {
        return Err(format!("assignment review cannot include {}; submit reviewer-wide actions separately in action", fields.join(", ")));
    }
    Ok(())
}

/// Project only the isolated closure assignment's routing contract. The
/// canonical reviewer retains its coarse focus and ordinary authoring limits.
pub fn project_closure_review_request(request: &mut WrapperRequest) {
    request.kernel_hinted_next_active_nodes = request.lean_closure_candidates.clone();
    request.kernel_hinted_next_active_coarse_nodes.clear();
    if request.phase.is_theorem_stating_like() {
        request.targeted_next_active_nodes = request.lean_closure_candidates.clone();
        request.allow_targeted_without_next_active = false;
        request.allowed_next_modes.insert(TaskMode::Targeted);
    }
}

pub fn validate_closure_review(review: &ReviewResponse) -> Result<(), String> {
    validate_assignment_review(review)?;
    if review.next_active.is_none() || review.next_active_coarse.is_some()
        || review.allow_new_obligations || !review.must_close_active || !review.authorized_nodes.is_empty()
        || !review.protected_semantic_change_nodes.is_empty() || !review.difficulty_updates.is_empty()
        || review.consume_global_repair_grant || review.assumption_authoring_request
        || review.dispatch_node_retirement || !review.node_retirement_decline_reason.is_empty()
        || !matches!(review.next_mode, TaskMode::Local | TaskMode::Targeted)
    {
        return Err("Lean-closure assignments may only close one existing statement: no decomposition, new obligations, semantic edits, difficulty changes or coarse-focus changes".into());
    }
    Ok(())
}

pub(crate) fn prepare_worker_context(
    canonical: &ProtocolState,
    mut review: ReviewResponse,
    graph: &DependencyGraph,
    options: AssignmentOptions,
) -> Result<PreparedWorker, String> {
    let frozen_review = review.clone();
    let ledger = canonical.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    for node in &canonical.live.present_nodes {
        let observed = graph.edges.get(node)
            .ok_or_else(|| format!("missing dependency observation for {node}"))?;
        if !observed.is_subset(&canonical.live.present_nodes)
            || canonical.deps.get(node).is_some_and(|semantic| !semantic.is_subset(observed))
        {
            return Err(format!("dependency observation for {node} disagrees with current semantic dependencies"));
        }
    }
    let binding = ledger.config.bind(&options.profile, options.timeout_seconds)?;
    if !binding.allowed_tasks.contains(&options.task_kind) {
        return Err(format!("profile {:?} is restricted to {:?}; this assignment requires {:?}",
                           options.profile, binding.allowed_tasks, options.task_kind));
    }
    validate_evidence_options(canonical, &options)?;
    let closure_only = options.task_kind == WorkerTaskKind::LeanClosure;
    if closure_only {
        validate_closure_review(&review)?;
        if options.compiler != CompilerCapability::Managed {
            return Err("Lean-closure assignments require compiler=managed".into());
        }
        if review.next_active.as_ref().is_none_or(|node| !canonical.lean_closure_candidates().contains(node)) {
            return Err("Lean-closure assignments require one eligible, existing, Lean-open proof node".into());
        }
    }
    if closure_only && canonical.tablet_target != crate::backend::BackendId::Lean {
        return Err("Lean-closure tasks require a Lean tablet".into());
    }
    if closure_only && !matches!(canonical.phase,
        Phase::TheoremStating | Phase::RevisionStating | Phase::ProofFormalization)
    {
        return Err("Lean-closure tasks require theorem-stating, revision-stating or proof-formalization".into());
    }
    if canonical.stage != Stage::Reviewer
        || canonical.in_flight_request.as_ref().is_none_or(|r| r.kind != RequestKind::Review)
    {
        return Err("worker assignments require an active reviewer request".into());
    }
    validate_assignment_review(&review)?;
    if review.assumption_authoring_request && options.compiler != CompilerCapability::Managed {
        return Err("assumption-authoring assignments require compiler=managed".into());
    }
    // The legacy theorem-stating router requires its general authoring flags.
    // Use that envelope only for routing; the isolated task below adds the
    // stricter closure acceptance gate and removes authoring permissions.
    // This does not alter the canonical review or any phase-advance predicate.
    if closure_only && canonical.phase != Phase::ProofFormalization {
        let advance = ReviewResponse {
            request_id: review.request_id, cycle: review.cycle,
            decision: ReviewDecisionKind::AdvancePhase, next_mode: canonical.current_mode(),
            ..ReviewResponse::default()
        };
        if canonical.review_response_legal(&advance) {
            return Err("phase advancement is ready; drain and advance instead of dispatching speculative closure work".into());
        }
        review.allow_new_obligations = true;
        review.must_close_active = false;
    }
    let mut private = canonical.clone_without_dispatch();
    if canonical.coarse_goals.is_some() && !closure_only
        && canonical.phase == Phase::ProofFormalization {
        private.coarse_task_goal = review.next_active_coarse.clone().or_else(|| canonical.active_coarse_node.clone());
        let goal = private.coarse_task_goal.clone().ok_or("select a coarse goal for ordinary proof work")?;
        private.select_coarse_goal(&goal)?;
        private.project_coarse_goal(&goal)?;
        private.in_flight_request = private.in_flight_request.as_ref()
            .map(|r| Box::new(private.expected_request(r.id, RequestKind::Review)));
    }
    if closure_only {
        private.worker_task_kind = WorkerTaskKind::LeanClosure;
        let id = private.in_flight_request.as_ref().unwrap().id;
        private.in_flight_request = Some(Box::new(private.expected_request(id, RequestKind::Review)));
    }
    if !private.review_response_legal(&review) {
        return Err(private.review_response_rejection_reasons(&review).join("; "));
    }
    let transitioned = apply_event(private, ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Review(review),
    }).map_err(|e| format!("ordinary reviewer transition refused assignment: {e:?}"))?;
    let mut private = transitioned.state;
    let mut commands = transitioned.commands;
    if private.stage == Stage::Start && private.in_flight_request.is_none() {
        let next = apply_event(private, ProtocolEvent::StartCycle)
            .map_err(|e| format!("ordinary worker routing refused assignment: {e:?}"))?;
        private = next.state;
        commands.extend(next.commands);
    }
    if private.phase != canonical.phase || private.stage != Stage::Worker {
        return Err(format!("ordinary process requires {:?} before this worker may run", private.stage));
    }
    if commands.iter().any(|c| !matches!(c, ProtocolCommand::IssueRequest { .. } | ProtocolCommand::CommitCheckpoint)) {
        return Err("assignment requires a shared runtime operation; drain before dispatching it".into());
    }
    let request_id = private.in_flight_request.as_ref()
        .filter(|r| r.kind == RequestKind::Worker).ok_or("ordinary routing produced no worker request")?.clone();
    private.worker_task_kind = options.task_kind;
    if closure_only {
        let pending = private.pending_task.as_mut().ok_or("closure assignment has no ordinary task contract")?;
        pending.allow_new_obligations = false;
        pending.must_close_active = true;
    }
    let request = Arc::new(private.expected_request(request_id.id, RequestKind::Worker));
    private.in_flight_request = Some(Box::new(request.as_ref().clone()));
    let nodes = if closure_only || matches!(private.current_worker_validation_kind(),
        WorkerValidationKind::ProofEasy | WorkerValidationKind::ProofLocal)
    {
        private.active_node.iter().cloned().collect()
    } else {
        private.current_worker_authorized_nodes()
    };
    if canonical.coarse_goals.is_some() && canonical.phase == Phase::ProofFormalization && !closure_only {
        let repairs = private.pending_task.as_ref().map(|t| t.task_blockers.clone()).unwrap_or_default();
        let pending = canonical.coarse_prerequisite_failures_for(&graph.closure(&nodes)?, &nodes, &repairs);
        if !pending.is_empty() {
            return Err(format!("this assignment's semantic or Lean dependencies still need verification: {}; wait for these verifier results",
                pending.iter().take(8).cloned().collect::<Vec<_>>().join(", ")));
        }
    }
    let witness_path = if closure_only { None } else { witness_write_path(&request, options.compiler) };
    let ordinary_exclusive = witness_path.is_some() || matches!(private.current_worker_validation_kind(),
        WorkerValidationKind::TheoremGlobal | WorkerValidationKind::Cleanup)
        || private.assumption_authoring.is_some()
        || private.live != canonical.live;
    // Modeling writers are serial even after the draft is certified. This is
    // only a scheduling restriction: ordinary_exclusive below still owns the
    // existing shared-write grants, and all ordinary validation is retained.
    let exclusive = ordinary_exclusive || canonical.phase == Phase::Modeling
        || !options.evidence_paths.is_empty();
    if canonical.live.modeling_draft && !exclusive {
        return Err("an uncertified Modeling draft requires an exclusive worker".into());
    }
    if closure_only && (exclusive || nodes.len() != 1 || !nodes.is_subset(&canonical.proof_nodes)
        || !nodes.is_subset(&canonical.live.open_nodes))
    {
        return Err("Lean-closure assignments require one existing, Lean-open proof node".into());
    }
    let mut scope = EditScope {
        nodes, helper_prefixes: if closure_only { BTreeSet::new() } else { BTreeSet::from([options.helper_prefix]) },
        shared_writes: if ordinary_exclusive { options.input_hashes.keys().filter(|p|
            p.starts_with("reference/") || p.starts_with("Tablet/")
                && !p.ends_with(".lean") && !p.ends_with(".tex")).cloned().collect() }
            else { BTreeSet::new() }, exclusive,
    };
    scope.shared_writes.extend(witness_path);
    // Evidence-induced exclusivity grants only the explicitly named files.
    // Existing ordinary exclusive contracts retain their original scope.
    scope.shared_writes.extend(options.evidence_paths);
    scope.validate(graph)?;
    let inputs = graph.closure(&scope.nodes)?;
    // The runtime observes exact source bytes, not just mtimes or declaration
    // names. Missing hashes fail closed before any worker slot is reserved.
    for node in &inputs {
        for extension in ["lean", "tex"] {
            let path = format!("Tablet/{node}.{extension}");
            // Extraction-model dependencies need not have a TeX file. Its
            // absence is still an input: creating one makes the task stale.
            if options.input_hashes.get(&path).is_none_or(|h|
                !(extension == "tex" && h == "absent")
                && (h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit())))
            {
                return Err(format!("missing exact dependency input identity: {path}"));
            }
        }
    }
    // Keep exact dependency inputs, plus shared inputs. Binding every peer's
    // Tablet files here would make independent publications spuriously stale.
    let input_hashes = options.input_hashes.into_iter().filter(|(path, _)| {
        // Framework-rendered indexes are regenerated from current state before
        // acceptance. They are not authored dependencies of a reserved task;
        // binding them would stale every sibling after any publication.
        if matches!(path.as_str(), "Tablet/INDEX.md" | "Tablet/README.md" | "Tablet/header.tex") { return false; }
        if exclusive { return true; }
        let Some(file) = path.strip_prefix("Tablet/") else { return true; };
        let node = file.strip_suffix(".lean").or_else(|| file.strip_suffix(".tex"));
        node.is_none_or(|node| !canonical.live.present_nodes.contains(&NodeId::from(node))
            || inputs.contains(&NodeId::from(node)))
    }).collect();
    // Only readonly proof dependencies may drift in their proof bodies. Own
    // writes, definitions, TeX and shared inputs retain exact source identities.
    let proof_body_rebase_inputs = if exclusive { BTreeMap::new() } else {
        options.proof_body_rebase_inputs.into_iter().filter(|(path, _)| {
            path.strip_prefix("Tablet/").and_then(|p| p.strip_suffix(".lean"))
                .is_some_and(|node| {
                    let node = NodeId::from(node);
                    inputs.contains(&node) && canonical.proof_nodes.contains(&node)
                        && !scope.nodes.contains(&node)
                })
        }).collect()
    };
    Ok(PreparedWorker {
        assignment: PreparedAssignment {
            assignment_id: options.assignment_id, profile: options.profile,
            timeout_seconds: options.timeout_seconds, compiler: options.compiler,
            task_kind: options.task_kind,
            scope, base_snapshot: options.base_snapshot, input_hashes, proof_body_rebase_inputs,
            request: request.into(),
            authorized_worker: Some(AuthorizedWorker::capture(&private)),
            review: Some(Box::new(frozen_review)),
        },
        private_state: private,
    })
}

/// Optional executable witnesses are shared artifacts: reserve their exact
/// kernel-derived path (including creation) and drain before authoring them.
pub(crate) fn witness_write_path(request: &WrapperRequest, compiler: CompilerCapability) -> Option<String> {
    if compiler != CompilerCapability::Managed
        || !request.trust_base_required_v1 || request.phase != Phase::ProofFormalization
        || request.work_kind != WorkerWorkKind::Standard { return None; }
    crate::request_contracts::active_disprove_primary(request)
        .map(crate::trust_base::rust_witness_relative_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence_options() -> AssignmentOptions {
        AssignmentOptions {
            assignment_id: "evidence".into(), profile: "sol-xhigh".into(), timeout_seconds: None,
            compiler: CompilerCapability::Managed, task_kind: WorkerTaskKind::Ordinary,
            helper_prefix: "P_".into(), base_snapshot: "base".into(),
            input_hashes: BTreeMap::from([("reference/model-evidence/new.rs".into(), "absent".into())]),
            evidence_paths: BTreeSet::from(["reference/model-evidence/new.rs".into()]),
            proof_body_rebase_inputs: BTreeMap::new(),
        }
    }

    #[test]
    fn evidence_options_require_exact_files_and_managed_modeling_preservation() {
        let state = ProtocolState { phase: Phase::Modeling, model_preservation_enabled: true, ..Default::default() };
        let options = evidence_options();
        assert!(validate_evidence_options(&state, &options).is_ok());
        for drift in ["phase", "feature", "compiler", "kind", "hash", "missing", "path", "limit"] {
            let mut current = state.clone();
            let mut changed = options.clone();
            match drift {
                "phase" => current.phase = Phase::ProofFormalization,
                "feature" => current.model_preservation_enabled = false,
                "compiler" => changed.compiler = CompilerCapability::None,
                "kind" => changed.task_kind = WorkerTaskKind::LeanClosure,
                "hash" => { changed.input_hashes.insert("reference/model-evidence/new.rs".into(), "unknown".into()); },
                "missing" => changed.input_hashes.clear(),
                "path" => changed.evidence_paths = BTreeSet::from(["reference/model-evidence/../APPROVED_AXIOMS.json".into()]),
                "limit" => changed.evidence_paths = (0..65).map(|i| format!("reference/model-evidence/{i}")).collect(),
                _ => unreachable!(),
            }
            assert!(validate_evidence_options(&current, &changed).is_err(), "{drift}");
        }
        let mut old = options;
        old.evidence_paths.clear();
        let wire = serde_json::to_value(&old).unwrap();
        assert!(wire.get("evidence_paths").is_none());
        let restored: AssignmentOptions = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), wire, "old event wire stays unchanged");
    }

    #[test]
    fn assignment_review_check_names_actions_and_requires_continue_without_reset() {
        let mut review = ReviewResponse::default();
        assert!(validate_assignment_review(&review).is_ok());
        review.clear_human_input = true;
        review.dismiss_audit_plan = true;
        assert_eq!(validate_assignment_review(&review).unwrap_err(),
            "assignment review cannot include dismiss_audit_plan, clear_human_input; submit reviewer-wide actions separately in action");
        review = ReviewResponse::default();
        review.decision = ReviewDecisionKind::AdvancePhase;
        assert!(validate_assignment_review(&review).unwrap_err().contains("decision=continue"));
        review.decision = ReviewDecisionKind::Continue;
        review.reset = ResetChoice::LastCommit;
        assert!(validate_assignment_review(&review).unwrap_err().contains("reset=none"));
    }

    #[test]
    fn closure_payload_refuses_planning_metadata_and_specialized_outcomes() {
        assert!(lean_closure_payload_errors(&serde_json::json!({
            "outcome": "stuck", "summary": "Could not close the node.", "comments": "", "difficulty_updates": {},
        })).is_empty());
        for key in ["semantic_dep_updates", "difficulty_updates", "audit_request", "conditional_theorem_proposal"] {
            let mut raw = serde_json::json!({"outcome": "valid", "summary": "done"});
            raw[key] = serde_json::json!({"a": "change"});
            assert!(lean_closure_payload_errors(&raw).iter().any(|e| e.contains(key)));
        }
        assert!(!lean_closure_payload_errors(&serde_json::json!({"outcome": "target_false_under_model"})).is_empty());
    }

    #[test]
    fn closure_sources_freeze_prefix_tex_and_other_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let tablet = dir.path().join("Tablet");
        std::fs::create_dir(&tablet).unwrap();
        let base = "-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\nby sorry\n";
        std::fs::write(tablet.join("A.lean"), base).unwrap();
        std::fs::write(tablet.join("A.tex"), "SKETCH").unwrap();
        let before = crate::snapshot_tablet_dir(dir.path());
        let contents = crate::snapshot_tablet_file_contents(dir.path());
        let check = || lean_closure_source_errors(dir.path(), &before, &contents, &"A".into(), true);
        assert!(lean_closure_source_errors(dir.path(), &before, &contents, &"A".into(), false).is_empty());
        assert!(check().iter().any(|e| e.contains("sorry")));
        std::fs::write(tablet.join("A.lean"), base.replace("by sorry", "by trivial")).unwrap();
        assert!(check().is_empty(), "{:?}", check());
        std::fs::write(tablet.join("A.lean"), base.replace("by sorry", "set_option maxHeartbeats 400000 in by trivial")).unwrap();
        assert!(check().is_empty(), "body-local heartbeat option stays in scope: {:?}", check());
        std::fs::write(tablet.join("A.lean"), format!("set_option maxHeartbeats 400000\n{}", base.replace("by sorry", "by trivial"))).unwrap();
        assert!(check().iter().any(|e| e.contains("preamble options")));
        std::fs::write(tablet.join("A.lean"), format!("import Mathlib\n{}", base.replace("by sorry", "by trivial"))).unwrap();
        assert!(check().is_empty(), "importing existing support is permitted: {:?}", check());
        std::fs::write(tablet.join("A.tex"), "new decomposition").unwrap();
        assert!(check().iter().any(|e| e.contains("unauthorized edit: A.tex")));
        std::fs::write(tablet.join("A.tex"), "SKETCH").unwrap();
        std::fs::write(tablet.join("A.lean"), base.replace("True", "False")).unwrap();
        assert!(check().iter().any(|e| e.contains("theorem statement")));
        std::fs::write(tablet.join("Helper.lean"), "theorem Helper : True := by trivial").unwrap();
        assert!(check().iter().any(|e| e.contains("unauthorized edit: Helper.lean")));
    }

    #[test]
    fn closure_import_additions_preserve_all_non_import_commands() {
        let base = "import Tablet.Preamble\nopen Nat\nset_option autoImplicit false\nnamespace N\n-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\n";
        for extra in ["import Tablet.Helper\n", "import Mathlib.Data.Nat.Basic -- existing lemma\n", "import Mathlib Tablet.Helper\n", "import Mathlib.Αλφα\n"] {
            assert!(closure_prefix_allows_imports(base, &format!("{extra}{base}")));
        }
        assert!(closure_prefix_allows_imports(base, &base.replace("import Tablet.Preamble", "import Tablet.Preamble Tablet.Helper")));
        for changed in [base.replace("import Tablet.Preamble\n", ""),
            base.replace("open Nat", "open Int"), base.replace("false", "true"),
            base.replace("namespace N", "namespace M"), base.replace("True", "False"),
            format!("import Mathlib; axiom hidden : False\n{base}"),
            format!("import Mathlib\nattribute [simp] Nat.add_comm\n{base}")]
        {
            assert!(!closure_prefix_allows_imports(base, &changed), "{changed}");
        }
    }

    #[test]
    fn closure_imports_cannot_change_elaborated_statement_or_lose_evidence() {
        let before = serde_json::json!({"lean_semantic_closure": "typed-statement", "own_tex": "tex"}).to_string();
        let same = serde_json::json!({"lean_semantic_closure": "typed-statement", "own_tex": "tex", "unrelated": "import metadata"}).to_string();
        assert!(lean_closure_statement_error(Some(&before), Some(&same)).is_none());
        let changed = serde_json::json!({"lean_semantic_closure": "shadowed-statement"}).to_string();
        assert!(lean_closure_statement_error(Some(&before), Some(&changed)).unwrap().contains("elaborated theorem statement"));
        for absent in [None, Some(&String::new()), Some(&"{}".to_string())] {
            assert!(lean_closure_statement_error(absent, Some(&same)).is_some());
            assert!(lean_closure_statement_error(Some(&before), absent).is_some());
        }
    }

    fn render_closure_test_docs(repo: &std::path::Path) {
        let snapshot = crate::build_tablet_support_snapshot_from_repo(repo).unwrap();
        let render = crate::build_tablet_support_render_output(repo, &snapshot);
        std::fs::write(repo.join("Tablet/INDEX.md"), render.index_md_content).unwrap();
        std::fs::write(repo.join("Tablet/README.md"), render.readme_md_content).unwrap();
    }

    fn closure_docs_fixture(repo: &std::path::Path) {
        std::fs::create_dir(repo.join("Tablet")).unwrap();
        std::fs::write(repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\ntheorem A : True :=\n-- BODY\nby sorry\n").unwrap();
        std::fs::write(repo.join("Tablet/A.tex"),
            "\\begin{lemma}True.\\end{lemma}\n\\begin{proof}Trivial.\\end{proof}\n").unwrap();
        std::fs::write(repo.join("Tablet/header.tex"), "% original header\n").unwrap();
        render_closure_test_docs(repo);
    }

    fn close_fixture_proof(repo: &std::path::Path) {
        let path = repo.join("Tablet/A.lean");
        let source = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, source.replace("by sorry", "by trivial")).unwrap();
    }

    #[test]
    fn closure_scope_retry_accepts_exact_regenerated_docs_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        closure_docs_fixture(repo);
        let before = crate::snapshot_tablet_dir(repo);
        let contents = crate::snapshot_tablet_file_contents(repo);
        close_fixture_proof(repo);
        render_closure_test_docs(repo);
        let after = crate::snapshot_tablet_dir(repo);
        for name in ["INDEX.md", "README.md"] {
            assert_ne!(before[name], after[name]);
        }
        for require_closed in [true, false, true] {
            let errors = lean_closure_source_errors(repo, &before, &contents, &"A".into(), require_closed);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(crate::snapshot_tablet_dir(repo), after, "scope checking must not alter proof or docs");
        }
    }

    #[test]
    fn closure_scope_retry_rejects_authored_doc_changes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        closure_docs_fixture(repo);
        let before = crate::snapshot_tablet_dir(repo);
        let contents = crate::snapshot_tablet_file_contents(repo);
        close_fixture_proof(repo);
        for name in ["INDEX.md", "README.md"] {
            render_closure_test_docs(repo);
            let path = repo.join("Tablet").join(name);
            let rendered = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, format!("{rendered}\nworker-authored text\n")).unwrap();
            let errors = lean_closure_source_errors(repo, &before, &contents, &"A".into(), true);
            assert!(errors.iter().any(|error| error.contains(&format!("unauthorized edit: {name}"))), "{errors:?}");
        }
    }

    #[test]
    fn closure_scope_retry_rejects_doc_creation_and_deletion() {
        for name in ["INDEX.md", "README.md"] {
            for created in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let repo = dir.path();
                closure_docs_fixture(repo);
                let path = repo.join("Tablet").join(name);
                if created { std::fs::remove_file(&path).unwrap(); }
                let before = crate::snapshot_tablet_dir(repo);
                let contents = crate::snapshot_tablet_file_contents(repo);
                close_fixture_proof(repo);
                render_closure_test_docs(repo);
                if !created { std::fs::remove_file(&path).unwrap(); }
                let errors = lean_closure_source_errors(repo, &before, &contents, &"A".into(), true);
                assert!(errors.iter().any(|error| error.contains(&format!("unauthorized edit: {name}"))), "{errors:?}");
            }
        }
    }

    #[test]
    fn closure_scope_retry_keeps_other_sources_and_prefix_protected() {
        for name in ["header.tex", "Preamble.tex", "A.tex", "Helper.lean", "A.lean"] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            closure_docs_fixture(repo);
            std::fs::write(repo.join("Tablet/Preamble.tex"), "% original preamble\n").unwrap();
            std::fs::write(repo.join("Tablet/Helper.lean"), "theorem Helper : True := by trivial\n").unwrap();
            render_closure_test_docs(repo);
            let before = crate::snapshot_tablet_dir(repo);
            let contents = crate::snapshot_tablet_file_contents(repo);
            close_fixture_proof(repo);
            let path = repo.join("Tablet").join(name);
            let original = std::fs::read_to_string(&path).unwrap();
            let changed = if name == "A.lean" { original.replace("True", "False") }
                else { format!("{original}\nunauthorized source change\n") };
            std::fs::write(path, changed).unwrap();
            render_closure_test_docs(repo);
            let errors = lean_closure_source_errors(repo, &before, &contents, &"A".into(), true);
            let expected = if name == "A.lean" { "theorem statement".to_string() }
                else { format!("unauthorized edit: {name}") };
            assert!(errors.iter().any(|error| error.contains(&expected)), "{name}: {errors:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn closure_scope_retry_rejects_rendered_doc_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        closure_docs_fixture(repo);
        let before = crate::snapshot_tablet_dir(repo);
        let contents = crate::snapshot_tablet_file_contents(repo);
        close_fixture_proof(repo);
        render_closure_test_docs(repo);
        let path = repo.join("Tablet/INDEX.md");
        let outside = repo.join("rendered-index.md");
        std::fs::rename(&path, &outside).unwrap();
        std::os::unix::fs::symlink(outside, path).unwrap();
        let errors = lean_closure_source_errors(repo, &before, &contents, &"A".into(), true);
        assert!(errors.iter().any(|error| error.contains("unauthorized edit: INDEX.md")), "{errors:?}");
    }

    #[test]
    fn graph_requires_complete_observations_and_unions_semantic_edges() {
        let mut state = ProtocolState::default();
        state.live.present_nodes = BTreeSet::from(["A".into(), "B".into(), "C".into()]);
        state.deps.insert("A".into(), BTreeSet::from(["C".into()]));
        let observed = BTreeMap::from([
            ("A".into(), BTreeSet::from(["B".into()])),
            ("B".into(), BTreeSet::new()), ("C".into(), BTreeSet::new()),
        ]);
        let graph = dependency_graph(&state, &observed).unwrap();
        assert_eq!(graph.edges[&NodeId::from("A")], BTreeSet::from(["B".into(), "C".into()]));
        let mut missing = observed.clone();
        missing.remove(&NodeId::from("C"));
        assert!(dependency_graph(&state, &missing).unwrap_err().contains("missing"));
        let mut outside = observed;
        outside.get_mut(&NodeId::from("B")).unwrap().insert("D".into());
        assert!(dependency_graph(&state, &outside).unwrap_err().contains("absent"));
    }
}
