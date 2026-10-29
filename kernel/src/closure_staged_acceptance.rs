//! Staged proof observations: expensive owner work is independent of a verdict.
//!
//! Only the host may donate an authenticated facts object. Every finalization
//! reruns the actual proof-delta consumer with its fresh inputs, including the
//! correspondence guard, scope checks and axiom policy. This is deliberately
//! not a cache of an earlier WorkerResponse.
use super::*;
#[path = "closure_staged_modeling.rs"]
mod modeling;
use std::cell::RefCell;
use runtime_cli_observations::{LiveProofDeltaObservations, NodeObservation,
    ProofDeltaGuard, ProofDeltaObservations};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerObservations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tablet: Option<runtime_cli_observations::TabletObservation>,
    pub nodes: BTreeMap<NodeId, NodeObservation>,
    pub probes: BTreeMap<NodeId, LocalClosureProbeOutput>,
    pub policies: BTreeMap<NodeId, LocalClosureProbeOutput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnedProofFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modeling: Option<modeling::ModelingFacts>,
    pub schema_version: u8,
    pub acceptance_logic_identity: String,
    pub payload_sha256: String,
    pub owner_ok: bool,
    pub failed_step_results: Vec<WorkerValidationStepResult>,
    pub validation_execution_plan_sha256: String,
    pub owner_nodes: BTreeSet<NodeId>,
    pub deleted_nodes: BTreeSet<NodeId>,
    pub read_nodes: BTreeSet<NodeId>,
    pub source_inputs: BTreeMap<String, String>,
    pub source_inputs_sha256: String,
    pub observation_environment: Option<RefreshEnvironment>,
    pub observations: OwnerObservations,
    pub candidate_graph: trellis_kernel::dispatch::DependencyGraph,
}

#[derive(Default)]
struct Recorder(RefCell<OwnerObservations>);
impl ProofDeltaObservations for Recorder {
    fn tablet(&self, repo: &Path, nodes: &BTreeSet<NodeId>, roles: &BTreeMap<NodeId, trellis_kernel::PvRole>)
        -> Result<runtime_cli_observations::TabletObservation, String> {
        let result = LiveProofDeltaObservations.tablet(repo, nodes, roles)?;
        self.0.borrow_mut().tablet = Some(result.clone());
        Ok(result)
    }
    fn node(&self, repo: &Path, node: &str) -> Result<NodeObservation, String> {
        let result = LiveProofDeltaObservations.node(repo, node)?;
        self.0.borrow_mut().nodes.insert(node.into(), result.clone());
        Ok(result)
    }
    fn closure(&self, repo: &Path, node: &str, premises: &BTreeSet<NodeId>)
        -> Result<LocalClosureProbeOutput, String> {
        let result = LiveProofDeltaObservations.closure(repo, node, premises)?;
        self.0.borrow_mut().probes.insert(node.into(), result.clone());
        Ok(result)
    }
    fn policy(&self, repo: &Path, node: &str) -> Result<LocalClosureProbeOutput, String> {
        let result = LiveProofDeltaObservations.policy(repo, node)?;
        self.0.borrow_mut().policies.insert(node.into(), result.clone());
        Ok(result)
    }
}

/// Construction is private: exact current source/environment binding must
/// succeed before any recorded expensive observation reaches the validator.
struct Replay<'a>(&'a OwnerObservations, std::cell::Cell<bool>);
impl Replay<'_> {
    fn binding_error(&self, message: String) -> String {
        self.1.set(true); message
    }
    /// Certificate finalization consumes the same authoring scan as phase2.
    /// Reuse that raw fact only on a Replay constructed after source/import
    /// environment validation. A tex-only owner may legitimately have no
    /// phase2 policy scan, in which case the ordinary scanner remains live.
    fn certificate_policy<F>(&self, repo: &Path, node: &str, fallback: F) -> Result<LocalClosureProbeOutput, String>
    where F: FnOnce(&Path, &str) -> Result<LocalClosureProbeOutput, String> {
        if self.0.policies.contains_key(&NodeId::from(node)) { self.policy(repo, node) }
        else { fallback(repo, node) }
    }
}

fn observation_source_matches(path: &Path, exists: bool, content: &str) -> bool {
    if exists { fs::read_to_string(path).ok().as_deref() == Some(content) }
    else { matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound) }
}

impl ProofDeltaObservations for Replay<'_> {
    fn tablet(&self, repo: &Path, nodes: &BTreeSet<NodeId>, _roles: &BTreeMap<NodeId, trellis_kernel::PvRole>)
        -> Result<runtime_cli_observations::TabletObservation, String> {
        let mut result = self.0.tablet.clone().ok_or_else(|| self.binding_error("staged facts lack tablet observation".into()))?;
        let expected: BTreeSet<String> = nodes.iter().map(ToString::to_string)
            .filter(|node| !matches!(node.as_str(), "Preamble" | "Axioms" | "header")).collect();
        if result.node_names.iter().cloned().collect::<BTreeSet<_>>() != expected
            || result.tablet_exists != repo.join("Tablet").is_dir() {
            return Err(self.binding_error("staged tablet observation scope changed".into()));
        }
        for (name, observation) in &mut result.nodes {
            let lean = repo.join("Tablet").join(format!("{name}.lean"));
            let tex = repo.join("Tablet").join(format!("{name}.tex"));
            if observation.node != *name
                || !observation_source_matches(&lean, observation.lean_exists, &observation.lean_content)
                || !observation_source_matches(&tex, observation.tex_exists, &observation.tex_content) {
                return Err(self.binding_error(format!("staged tablet source changed: {name}")));
            }
            observation.lean_path = lean.display().to_string();
            observation.tex_path = tex.display().to_string();
        }
        if !observation_source_matches(&repo.join("Tablet/Preamble.lean"), result.preamble.lean_exists, &result.preamble.lean_content)
            || !observation_source_matches(&repo.join("Tablet/Preamble.tex"), result.preamble.tex_exists, &result.preamble.tex_content) {
            return Err(self.binding_error("staged preamble observation source changed".into()));
        }
        result.global_errors = runtime_cli_observations::observe_native_basis_errors(repo)?;
        Ok(result)
    }
    fn binding_failed(&self) -> bool { self.1.get() }
    fn node(&self, repo: &Path, node: &str) -> Result<NodeObservation, String> {
        let mut result = self.0.nodes.get(&NodeId::from(node)).cloned()
            .ok_or_else(|| self.binding_error(format!("staged facts lack node observation: {node}")))?;
        let lean = repo.join("Tablet").join(format!("{node}.lean"));
        let tex = repo.join("Tablet").join(format!("{node}.tex"));
        // Relocate only the two explicit diagnostic path carriers; never
        // rewrite proof terms, source text or nested observation payloads.
        if result.node != node || !result.lean_exists
            || fs::read_to_string(&lean).ok().as_ref() != Some(&result.lean_content)
            || (result.tex_exists && fs::read_to_string(&tex).ok().as_ref() != Some(&result.tex_content))
            || (!result.tex_exists && !matches!(fs::symlink_metadata(&tex),
                Err(error) if error.kind() == io::ErrorKind::NotFound)) {
            return Err(self.binding_error(format!("staged node observation source changed: {node}")));
        }
        result.lean_path = lean.display().to_string();
        result.tex_path = tex.display().to_string();
        Ok(result)
    }
    fn closure(&self, _repo: &Path, node: &str, premises: &BTreeSet<NodeId>)
        -> Result<LocalClosureProbeOutput, String> {
        let result = self.0.probes.get(&NodeId::from(node)).cloned()
            .ok_or_else(|| self.binding_error(format!("staged facts lack closure probe: {node}")))?;
        if &result.premise_owners != premises {
            return Err(self.binding_error(format!("staged probe premise owners changed: {node}")));
        }
        Ok(result)
    }
    fn policy(&self, _repo: &Path, node: &str) -> Result<LocalClosureProbeOutput, String> {
        self.0.policies.get(&NodeId::from(node)).cloned()
            .ok_or_else(|| self.binding_error(format!("staged facts lack authoring-policy observation: {node}")))
    }
}

fn execute_owned(input: &ExecuteWorkerValidationPlanInput,
    observations: &dyn ProofDeltaObservations, guard: ProofDeltaGuard,
) -> Result<ExecutedWorkerValidationPlanOutput, String> {
    if modeling::shape(&input.validation_execution_plan) { return modeling::execute(input, observations); }
    let [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
        active_node, mode, authorized_nodes, protected_semantic_change_nodes,
        allow_new_obligations, must_close_active,
    }] = input.validation_execution_plan.as_slice() else {
        return Err("staged proof requires one proof_worker_delta step".into());
    };
    if !matches!(mode, WorkerProofDeltaMode::Easy | WorkerProofDeltaMode::Local | WorkerProofDeltaMode::Restructure)
        || !protected_semantic_change_nodes.is_empty()
        || input.under_model_assumption_nodes != runtime_cli_observations::under_model_assumption_nodes_from_roles(true, &input.node_role)
        || input.assumption_authoring_node.is_some() || input.modeling_draft
        || !input.reserved_non_node_stems.is_empty() || !input.seed_support_definition_nodes.is_empty()
        || trellis_kernel::tablet_target_for_repo(&input.repo_path) != trellis_kernel::backend::BackendId::Lean {
        return Err("unsupported staged proof policy or backend".into());
    }
    let active = active_node.as_ref().or(input.active_node.as_ref()).ok_or("missing staged active node")?;
    let authorized = if authorized_nodes.is_empty() { &input.authorized_nodes } else { authorized_nodes };
    let mut protected = BTreeSet::new();
    let result = runtime_cli_observations::proof_worker_delta_with_observations(
        &input.repo_path, active.as_str(), &input.before_snapshot, &input.before_tablet_contents,
        &input.current_present_nodes, &input.declared_deleted_nodes, &input.current_node_kinds,
        &input.current_open_nodes, &input.expected_active_hash, *mode, &input.approved_target_nodes,
        &input.approved_corr_fingerprints, &input.coarse_dag_nodes, authorized,
        protected_semantic_change_nodes, &mut protected, &input.under_model_assumption_nodes,
        input.assumption_authoring_node.as_ref(), &input.seed_support_definition_nodes,
        &input.node_role, *allow_new_obligations, *must_close_active, observations, guard);
    let result = observed_step_result(result, observations, authorized)?;
    Ok(ExecutedWorkerValidationPlanOutput { step_results: vec![result], protected_semantic_change_nodes: protected })
}

fn observed_step_result(result: Result<WorkerValidationStepResult, String>,
    observations: &dyn ProofDeltaObservations, authorized: &BTreeSet<NodeId>,
) -> Result<WorkerValidationStepResult, String> {
    match result {
        Ok(step) => Ok(step),
        Err(error) if observations.binding_failed() => Err(error),
        // Same classification as execute_worker_validation_plan. In
        // particular, a current protected consumer's real compiler failure
        // rejects the candidate; it must not endlessly repeat owned proof work.
        Err(error) => Ok(WorkerValidationStepResult { kind: "proof_worker_delta".into(), ok: false,
            detail: error.clone(), errors: vec![error], allowed_nodes: authorized.clone(), ..Default::default() }),
    }
}

fn owned_scope(input: &ExecuteWorkerValidationPlanInput)
    -> Result<runtime_cli_observations::ProofWorkerDeltaReceiptScope, String> {
    if modeling::shape(&input.validation_execution_plan) {
        return modeling::scope(&input.repo_path, &input.before_snapshot, &input.current_present_nodes,
            input.active_node.as_ref(), &input.declared_deleted_nodes, &input.reserved_non_node_stems,
            &input.validation_execution_plan);
    }
    let [WorkerValidationExecutionPlanStep::ProofWorkerDelta { active_node, authorized_nodes,
        must_close_active, mode, .. }] = input.validation_execution_plan.as_slice() else {
        return Err("unsupported staged validation plan".into());
    };
    let active = active_node.as_ref().or(input.active_node.as_ref()).ok_or("missing staged active node")?;
    let authorized = if authorized_nodes.is_empty() { &input.authorized_nodes } else { authorized_nodes };
    runtime_cli_observations::proof_worker_delta_receipt_scope_for_mode(&input.repo_path, &input.before_snapshot,
        &input.current_present_nodes, active, authorized, &input.declared_deleted_nodes, *must_close_active, *mode)
        .ok_or_else(|| "unsupported staged source delta".into())
}

/// All globals stay exact. Only unrelated registered Tablet source pairs and
/// native-rendered indexes are outside an owner's source read projection.
fn source_projection(inputs: &BTreeMap<String, String>, present: &BTreeSet<NodeId>, reads: &BTreeSet<NodeId>, deleted: &BTreeSet<NodeId>)
    -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    for (path, hash) in inputs {
        if matches!(path.as_str(), "Tablet/INDEX.md" | "Tablet/README.md") { continue; }
        let node = path.strip_prefix("Tablet/").and_then(|p|
            p.strip_suffix(".lean").or_else(|| p.strip_suffix(".tex"))).map(NodeId::from);
        if node.is_some_and(|node| present.contains(&node) && !reads.contains(&node)) { continue; }
        result.insert(path.clone(), hash.clone());
    }
    for node in reads {
        let lean = format!("Tablet/{node}.lean");
        if deleted.contains(node) {
            if inputs.contains_key(&lean) || inputs.contains_key(&format!("Tablet/{node}.tex")) {
                return Err(format!("deleted staged source reappeared: {node}"));
            }
            result.insert(lean, "absent".into());
        } else if !result.contains_key(&lean) { return Err(format!("missing staged source input: {lean}")); }
        result.entry(format!("Tablet/{node}.tex")).or_insert_with(|| "absent".into());
    }
    Ok(result)
}

fn candidate_graph(input: &ExecuteWorkerValidationPlanInput, observations: &OwnerObservations,
    graph: &trellis_kernel::dispatch::DependencyGraph, owners: &BTreeSet<NodeId>, present: &BTreeSet<NodeId>,
) -> Result<trellis_kernel::dispatch::DependencyGraph, String> {
    let mut graph = graph.clone();
    let direct = direct_deps_from_repo(&input.repo_path, present);
    for node in owners.iter().filter(|node| present.contains(*node)) {
        let mut edges = direct.get(node).cloned().unwrap_or_default();
        if let Some(probe) = observations.probes.get(node) {
            let imports = probe.direct_imports.clone().unwrap_or_default();
            edges.extend(imports.iter().filter_map(|name| name.strip_prefix("Tablet.")).map(NodeId::from));
            edges.extend(probe.boundary_theorems.keys().cloned());
            edges.extend(probe.strict_theorem_deps.keys().cloned());
            edges.extend(probe.strict_definition_deps.keys().cloned());
        }
        edges.remove(node);
        if !edges.is_subset(present) { return Err(format!("unregistered staged dependency for {node}")); }
        graph.edges.insert(node.clone(), edges);
    }
    Ok(graph)
}

/// A successful owner stage is still NOT acceptance. Only the finalizer may
/// construct a WorkerResponse after current shared observations and legality.
pub(crate) fn check_owned(input: &ExecuteWorkerValidationPlanInput, raw_payload: &Value,
    graph: &trellis_kernel::dispatch::DependencyGraph, config: Option<&PreparedConfigBinding>,
) -> Result<Value, String> {
    let scope = owned_scope(input)?;
    if scope.observation_nodes.is_empty() { return Err("staged proof has no owner observations".into()); }
    if config.is_some_and(|config| !config.current_carriers_match(&input.repo_path)) {
        return Err("staged host config carrier mismatch".into());
    }
    // Optional eager preparation only establishes successful observation timing.
    // A failed build continues through the existing validator for its ordinary
    // compile-error classification; do not label a type error infrastructure.
    let prepared = prepare_receipt_observation_sources(&input.repo_path, &scope.observation_nodes);
    let source_before = trellis_kernel::dispatch_observation::verifier_inputs(&input.repo_path)?;
    let before = prepared.map_err(|reason| ("preparation", CaptureFailure { reason, retryable:true, diagnostic:None })).and_then(|()|
        capture_publication_environment_checked(&input.repo_path, &scope.observation_nodes)
            .map_err(|reason| ("before_observations", reason)));
    let recorder = Recorder::default();
    let result = execute_owned(input, &recorder, ProofDeltaGuard::Deferred)?;
    let owner_ok = result.step_results.iter().all(|step| step.ok);
    let after = if owner_ok && before.is_ok() {
        capture_publication_environment_checked(&input.repo_path, &scope.observation_nodes)
            .map(Some).map_err(|reason| ("after_observations", reason))
    } else { Ok(None) };
    if source_before != trellis_kernel::dispatch_observation::verifier_inputs(&input.repo_path)? {
        return Err("staged owner source changed during observation".into());
    }
    let environment = match owned_observation_environment(owner_ok, before, after, &scope.observation_nodes)? {
        Ok(environment) => environment,
        Err(cold) => return Ok(cold),  // No facts exist without both observation-time bindings.
    };
    let observations = recorder.0.into_inner();
    let mut graph = candidate_graph(input, &observations, graph, &scope.observation_nodes, &scope.final_present_nodes)?;
    let read_roots = scope.observation_nodes.union(&scope.deleted_nodes).cloned().collect();
    let reads = if modeling::shape(&input.validation_execution_plan) {
        modeling::reserve_imports(&input.repo_path, &mut graph, &read_roots, &scope.deleted_nodes)?
    } else { graph.closure(&read_roots)? };
    let sources = source_projection(&source_before, &scope.final_present_nodes, &reads, &scope.deleted_nodes)?;
    let digest = validation_fact_receipt::source_inputs_digest(&sources, config)
        .ok_or("staged source binding unavailable")?;
    let facts = OwnedProofFacts { modeling: if modeling::shape(&input.validation_execution_plan) { Some(modeling::facts(input, raw_payload)?) } else { None }, schema_version: 1,
        acceptance_logic_identity: trellis_kernel::model::acceptance_logic_identity(),
        payload_sha256: validation_fact_receipt::complete_digest("staged_payload", raw_payload)
            .ok_or("staged payload exceeds bound")?,
        owner_ok, failed_step_results: if owner_ok { Vec::new() } else { result.step_results.clone() },
        validation_execution_plan_sha256: validation_fact_receipt::complete_digest("staged_plan", &input.validation_execution_plan)
            .ok_or("staged plan exceeds bound")?,
        owner_nodes: scope.observation_nodes, deleted_nodes: scope.deleted_nodes, read_nodes: reads, source_inputs: sources,
        source_inputs_sha256: digest, observation_environment: environment, observations, candidate_graph: graph };
    Ok(json!({"status":if owner_ok { "facts" } else { "rejected" }, "facts":facts,
        "validation_step_results":result.step_results,
        "errors":result.step_results.iter().flat_map(|step| step.errors.clone()).collect::<Vec<_>>() }))
}

/// Only unavailable capture is retryable. Source drift is checked by the caller;
/// differing successful inventories remain semantic staleness, not infrastructure.
fn owned_observation_environment(owner_ok: bool,
    before: Result<RefreshEnvironment, (&str, CaptureFailure)>,
    after: Result<Option<RefreshEnvironment>, (&str, CaptureFailure)>, roots: &BTreeSet<NodeId>,
) -> Result<Result<Option<RefreshEnvironment>, Value>, String> {
    if !owner_ok { return Ok(Ok(None)); }  // Keep real validator errors as rejection.
    let unavailable = |(phase, failure): (&str, CaptureFailure)| {
        let reason: String = failure.reason.chars().take(480).collect();
        let mut cold = json!({"status":"cold", "capture_phase":phase,
            "reason":format!("staged owner environment unavailable ({phase}): {reason}")});
        if failure.retryable { cold["retry_class"] = json!("owned_environment_capture"); }
        if let Some(diagnostic) = failure.diagnostic { cold["environment_capture"] = diagnostic; }
        cold
    };
    let before = match before { Ok(value) => value, Err(error) => return Ok(Err(unavailable(error))) };
    let after = match after { Ok(Some(value)) => value, Err(error) => return Ok(Err(unavailable(error))),
        Ok(None) => return Err("staged owner post-observation capture missing".into()) };
    if !after.agrees(&before, roots) || before.artifact_files != after.artifact_files {
        return Err("staged owner environment changed during observation".into());
    }
    Ok(Ok(Some(after)))
}

fn compatible_owned_producer(facts: &OwnedProofFacts) -> bool {
    if facts.acceptance_logic_identity == trellis_kernel::model::acceptance_logic_identity() {
        return true;
    }
    // Reviewed b7 facts retain their original identity. They are raw positive
    // math observations, not a verdict; all current consumers below still run.
    let policy: Value = serde_json::from_str(include_str!(
        "../../trellis/dispatch/compatible_staged_producers.json")).expect("staged producer policy");
    facts.owner_ok && policy["version"] == json!(1)
        && policy["positive_math_owned"].as_array().is_some_and(|identities|
            identities.iter().any(|identity| identity.as_str() == Some(facts.acceptance_logic_identity.as_str())))
}

fn replay_current<'a>(input: &ExecuteWorkerValidationPlanInput, raw: &Value, facts: &'a OwnedProofFacts,
    environment: Option<&RefreshEnvironment>, config: Option<&PreparedConfigBinding>,
) -> Result<Replay<'a>, String> {
    let scope = owned_scope(input)?;
    let modeling_facts = if modeling::shape(&input.validation_execution_plan) { Some(modeling::facts(input, raw)?) } else { None };
    if facts.modeling != modeling_facts
        || facts.schema_version != 1 || !compatible_owned_producer(facts)
        || (facts.acceptance_logic_identity != trellis_kernel::model::acceptance_logic_identity()
            && (!input.node_role.is_empty() || !input.under_model_assumption_nodes.is_empty()
                || input.modeling_draft || input.assumption_authoring_node.is_some()))
        || validation_fact_receipt::complete_digest("staged_payload", raw).as_ref() != Some(&facts.payload_sha256)
        || scope.observation_nodes != facts.owner_nodes || scope.deleted_nodes != facts.deleted_nodes
        || config.is_some_and(|config| !config.current_carriers_match(&input.repo_path))
        || (facts.owner_ok && !environment.zip(facts.observation_environment.as_ref()).is_some_and(|(current, old)|
            current.agrees(old, &facts.owner_nodes) && current.current(&facts.owner_nodes))) {
        return Err("staged owner identity or imported environment changed".into());
    }
    let current = trellis_kernel::dispatch_observation::verifier_inputs(&input.repo_path)?;
    let current = source_projection(&current, &scope.final_present_nodes, &facts.read_nodes, &scope.deleted_nodes)?;
    if validation_fact_receipt::source_inputs_digest(&current, config).as_ref() != Some(&facts.source_inputs_sha256) {
        return Err("staged owner source reads changed".into());
    }
    Ok(Replay(&facts.observations, std::cell::Cell::new(false)))
}

pub(crate) fn finalize_owned_step(input: &ExecuteWorkerValidationPlanInput, raw: &Value,
    facts: &OwnedProofFacts, environment: Option<&RefreshEnvironment>, config: Option<&PreparedConfigBinding>,
) -> Result<ExecutedWorkerValidationPlanOutput, String> {
    let replay = replay_current(input, raw, facts, environment, config)?;
    if !facts.owner_ok {
        if facts.failed_step_results.is_empty() || facts.failed_step_results.iter().all(|step| step.ok)
            || validation_fact_receipt::complete_digest("staged_plan", &input.validation_execution_plan).as_ref()
                != Some(&facts.validation_execution_plan_sha256) {
            return Err("staged failure no longer matches current validation plan".into());
        }
        // An authenticated failed observation cannot adopt sources or facts.
        // Reapply the same failed step to fresh native failure normalization;
        // there is no successful artifact whose absence should cause a retry.
        return Ok(ExecutedWorkerValidationPlanOutput { step_results: facts.failed_step_results.clone(),
            protected_semantic_change_nodes: BTreeSet::new() });
    }
    // The guard is real/current. Expensive owned leaves are replayed; the
    // unchanged core reruns current policy, scope, kinds, signatures and cuts.
    let result = execute_owned(input, &replay, ProofDeltaGuard::Observe)?;
    replay_current(input, raw, facts, environment, config)?;
    Ok(result)
}

/// Pure admission hint. Replay-attested current imports are conservatively
/// unioned with the candidate header imports; the eventual owner observation
/// still establishes the actual compiled graph and exact source/environment.
pub(crate) fn plan(repo: &Path, private: &ProtocolState,
    graph: &trellis_kernel::dispatch::DependencyGraph, before: &BTreeMap<String, String>,
    edit: &trellis_kernel::dispatch::EditScope, raw: &Value,
) -> Result<Value, String> {
    let request = private.in_flight_request.as_deref().ok_or("missing staged request")?;
    supported_request(repo, request)?;
    if edit.exclusive || !edit.shared_writes.is_empty() { return Err("exclusive source scope".into()); }
    if raw.get("outcome").and_then(Value::as_str) != Some("valid") {
        return Err("staged proof lane requires a candidate requesting Valid".into());
    }
    let declared_deleted: BTreeSet<NodeId> = serde_json::from_value(raw.get("deleted_nodes").cloned().unwrap_or_else(|| json!([])))
        .map_err(|e| e.to_string())?;
    let modeling = request.phase == trellis_kernel::Phase::Modeling;
    if (!modeling && declared_deleted.contains(request.active_node.as_ref().ok_or("missing staged active")?))
        || !declared_deleted.is_subset(&edit.nodes) {
        return Err("staged deletion outside owned scope".into());
    }
    // Admit only source shapes the owned producer can represent. This uses
    // the same mode/fallback authorization as owned_scope, so worker scope
    // mistakes take the ordinary gate before starting staged compilation.
    let mut proof_scope = None;
    let modeling_scope = if modeling {
        if request.worker_acceptance.authorized_nodes != edit.nodes {
            return Err("Modeling authorization differs from reserved source scope".into());
        }
        Some(modeling::scope(repo, before, &request.current_present_nodes, request.active_node.as_ref(),
            &declared_deleted, &BTreeSet::new(), &request.worker_acceptance.validation_execution_plan)?)
    } else {
    let [WorkerValidationExecutionPlanStep::ProofWorkerDelta { active_node, mode,
        authorized_nodes, must_close_active, .. }] = request.worker_acceptance.validation_execution_plan.as_slice()
        else { return Err("unsupported staged validation plan".into()); };
    let scope_active = active_node.as_ref().or(request.active_node.as_ref()).ok_or("missing staged active")?;
    let authorized = if authorized_nodes.is_empty() { &request.worker_acceptance.authorized_nodes } else { authorized_nodes };
    proof_scope = Some(runtime_cli_observations::proof_worker_delta_receipt_scope_for_mode(repo, before,
        &request.current_present_nodes, scope_active, authorized, &declared_deleted, *must_close_active, *mode)
        .ok_or("unsupported staged source delta")?);
        None
    };
    let after = snapshot_tablet_dir(repo);
    let mut writes = BTreeSet::new();
    let mut present = private.live.present_nodes.clone();
    for path in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        if before.get(path) == after.get(path) { continue; }
        if matches!(path.as_str(), "INDEX.md" | "README.md") { continue; }
        let stem = path.strip_suffix(".lean").or_else(|| path.strip_suffix(".tex"))
            .ok_or_else(|| format!("staged work changes shared source: {path}"))?;
        if matches!(stem, "Preamble" | "Axioms" | "Assumptions" | "header") {
            return Err("staged shared source is unsupported".into());
        }
        let node = NodeId::from(stem);
        if !after.contains_key(path) {
            if !declared_deleted.contains(&node) || ["lean", "tex"].iter().any(|ext|
                !matches!(fs::symlink_metadata(repo.join("Tablet").join(format!("{node}.{ext}"))),
                    Err(error) if error.kind() == io::ErrorKind::NotFound)) {
                return Err("staged deletion must remove an authorized complete source pair".into());
            }
            writes.insert(node); continue;
        }
        if private.live.present_nodes.contains(&node) {
            if !edit.nodes.contains(&node) { return Err(format!("staged write outside owned scope: {node}")); }
        } else {
            if !edit.helper_prefixes.iter().any(|prefix| stem.starts_with(prefix))
                || !after.contains_key(&format!("{stem}.lean")) || !after.contains_key(&format!("{stem}.tex")) {
                return Err(format!("staged helper lacks authorized paired ownership: {node}"));
            }
            present.insert(node.clone());
        }
        writes.insert(node);
    }
    let active = request.active_node.as_ref().ok_or("missing staged active")?;
    // Unchanged active work still observes the assigned owner when must-close
    // is requested. Reserving it also prevents same-owner publication races.
    writes.insert(active.clone());
    let mut candidate = graph.clone();
    let direct = direct_deps_from_repo(repo, &present);
    for owner in &writes {
        let edges = candidate.edges.entry(owner.clone()).or_default();
        edges.extend(direct.get(owner).into_iter().flatten().cloned());
        if !edges.is_subset(&present) { return Err(format!("unregistered staged import in {owner}")); }
    }
    if !declared_deleted.is_subset(&writes) { return Err("declared staged deletion absent from actual delta".into()); }
    let new_nodes = writes.difference(&private.live.present_nodes).cloned().collect();
    let closure = request.task_execution.as_ref().is_some_and(|execution|
        execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::LeanClosure);
    let validated = validate_worker_task_payload(raw, &["valid".into()], closure);
    if !validated.ok { return Err(validated.errors.join("\n")); }
    let payload: CheckedWorkerPayload = serde_json::from_value(validated.data.ok_or("staged payload absent")?)
        .map_err(|error| error.to_string())?;
    let metadata_nodes = edit.nodes.union(&new_nodes).cloned().collect();
    let local_helpers = proof_scope.as_ref().and_then(|scope|
        local_helper_difficulty_nodes(request, &new_nodes, &scope.observation_nodes));
    supported_payload_for_scope_with_local_helpers(&payload, &new_nodes, &metadata_nodes,
        modeling.then_some(&request.configured_targets),
        ordinary_restructure_metadata(request).then(|| &proof_scope.as_ref().expect("proof scope").observation_nodes),
        local_helpers.as_ref())?;
    // Metadata writes retain the same owner reservation as source writes.
    // The ordinary extension only admits owners already observed by phase 2.
    if ordinary_restructure_metadata(request) {
        writes.extend(payload.difficulty_updates.keys().map(|node| NodeId::from(node.as_str())));
    }
    if modeling {
        writes.extend(payload.target_claim_updates.keys().map(|node| NodeId::from(node.as_str())));
        writes.extend(payload.difficulty_updates.keys().map(|node| NodeId::from(node.as_str())));
        if writes.iter().any(|node| matches!(request.node_role.get(node),
            Some(trellis_kernel::PvRole::UnderModelAssumptions | trellis_kernel::PvRole::ExtractionModel))) {
            return Err("Modeling trust/model source edits require ordinary exclusive checking".into());
        }
    }
    let surviving_writes = writes.difference(&declared_deleted).cloned().collect();
    unchanged_import_normalization(repo, &surviving_writes)?;
    if closure && (!declared_deleted.is_empty() || !new_nodes.is_empty()
        || writes != BTreeSet::from([active.clone()])) {
        return Err("closure-only staged scope changed membership or another owner".into());
    }
    let mut roots = writes.clone();
    if let Some(scope) = &modeling_scope {
        // The producer checks every actual scoped observation, including
        // unchanged prerequisites. Decline before admission if any would
        // need source-changing normalization; ordinary checking owns it.
        let ordinary_nodes = scope.observation_nodes.iter()
            .filter(|node| node.as_str() != "Preamble").cloned().collect();
        unchanged_import_normalization(repo, &ordinary_nodes)?;
        roots.extend(scope.observation_nodes.iter().cloned());
        roots.extend(request.worker_acceptance.authorized_nodes.iter().cloned());
    }
    let reads = if modeling { modeling::reserve_imports(repo, &mut candidate, &roots, &declared_deleted)? }
        else { candidate.closure(&roots)? };
    Ok(json!({"status":"planned", "write_nodes":writes, "owner_nodes":writes,
        "read_nodes":reads, "candidate_graph":candidate}))
}

fn supported_request(repo: &Path, request: &WrapperRequest) -> Result<(), String> {
    use trellis_kernel::dispatch::CompilerCapability;
    let modeling = request.phase == trellis_kernel::Phase::Modeling;
    if request.kind != RequestKind::Worker || !matches!(request.phase,
        trellis_kernel::Phase::ProofFormalization | trellis_kernel::Phase::Modeling)
        || request.assumption_authoring.is_some() || request.modeling_draft
        || (!request.is_pv && (!request.node_role.is_empty() || !request.configured_challenge_targets.is_empty()))
        || request.trust_base_required_v1
        || request.task_execution.as_ref().is_none_or(|execution| execution.compiler != CompilerCapability::Managed)
        || trellis_kernel::tablet_target_for_repo(repo) != trellis_kernel::backend::BackendId::Lean {
        return Err("unsupported staged worker phase/backend/authority".into());
    }
    if modeling {
        let observations = &request.worker_acceptance.observation_plan;
        if request.task_execution.as_ref().is_none_or(|execution|
                execution.task_kind != trellis_kernel::dispatch::WorkerTaskKind::Ordinary)
            || request.worker_acceptance.validation_kind != WorkerValidationKind::TheoremRestructure
            || request.trust_seam_repair_context.is_some()
            || !modeling::shape(&request.worker_acceptance.validation_execution_plan)
            || !observations.capture_scoped_tablet_baseline_errors
            || observations.scoped_tablet_baseline_scope != trellis_kernel::WorkerBaselineScope::AuthorizedNodes
            || observations.capture_baseline_correspondence_hashes {
            return Err("unsupported staged Modeling scope or baseline".into());
        }
        return Ok(());
    }
    if request.is_pv {
        // Existing pinned model theorem proof work is supported; authoring a
        // trust boundary or a new compiled statement transform is a distinct
        // lane. A populated immutable role/challenge registry is not itself an
        // unsupported operation (normal PV repositories always carry one).
        if request.trust_seam_repair_context.is_some()
            || request.worker_acceptance.observation_plan.capture_scoped_tablet_baseline_errors
            || request.worker_acceptance.observation_plan.capture_baseline_correspondence_hashes
            || request.active_node.as_ref().is_none_or(|node|
                request.current_node_kinds.get(node) != Some(&NodeKind::Proof)
                    || request.node_role.get(node) == Some(&trellis_kernel::PvRole::UnderModelAssumptions))
            || request.configured_challenge_targets.iter().any(|(id, spec)|
                id.as_str().starts_with("authored:")
                    || spec.statement_provenance == trellis_kernel::model::StatementProvenance::WorkerAuthored)
        { return Err("unsupported staged PV authoring or statement-transform lane".into()); }
    }
    match request.worker_acceptance.validation_execution_plan.as_slice() {
        [WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode, protected_semantic_change_nodes, .. }]
            if matches!(mode, WorkerProofDeltaMode::Easy | WorkerProofDeltaMode::Local | WorkerProofDeltaMode::Restructure)
                && (!request.is_pv || matches!(mode, WorkerProofDeltaMode::Easy | WorkerProofDeltaMode::Local))
                && protected_semantic_change_nodes.is_empty() => Ok(()),
        _ => Err("unsupported staged worker execution plan".into()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationScope {
    pub nodes: BTreeSet<NodeId>,
    pub targets: BTreeSet<TargetId>,
    pub guard_nodes: BTreeSet<NodeId>,
    pub roots: BTreeSet<NodeId>,
}

/// The full current universe stays in the consumer input. This selects only
/// which rows must be re-observed after applying the owned candidate delta.
fn final_scope(private: &ProtocolState, graph: &trellis_kernel::dispatch::DependencyGraph,
    facts: &OwnedProofFacts,
) -> Result<ObservationScope, String> {
    let request = private.in_flight_request.as_deref().ok_or("missing current finalization request")?;
    let mut edges = graph.edges.clone();
    for (node, record) in &private.local_closure_records {
        edges.entry(node.clone()).or_default().extend(record.authoritative_dependency_owners(private));
    }
    let writes = facts.modeling.as_ref().map(|modeling| &modeling.write_nodes).unwrap_or(&facts.owner_nodes);
    for node in writes.difference(&facts.deleted_nodes) {
        // Preserve removed edges for reverse invalidation, and include added
        // helper/import edges attested by the original owner observations.
        edges.entry(node.clone()).or_default().extend(facts.candidate_graph.edges.get(node)
            .ok_or("staged owner graph row missing")?.iter().cloned());
    }
    let mut nodes: BTreeSet<_> = writes.union(&facts.deleted_nodes).cloned().collect();
    loop {
        let affected: BTreeSet<_> = edges.iter().filter(|(_, deps)| !deps.is_disjoint(&nodes))
            .map(|(node, _)| node.clone()).collect();
        let previous = nodes.len(); nodes.extend(affected);
        if previous == nodes.len() { break; }
    }
    let mut targets: BTreeSet<_> = private.live.coverage.iter()
        .chain(private.live.protected_closure_nodes_per_target.iter())
        .filter(|(_, owners)| !owners.is_disjoint(&nodes)).map(|(target, _)| target.clone()).collect();
    if let Some(modeling) = &facts.modeling { targets.extend(modeling.effect_targets.iter().cloned()); }
    let approved = runtime_cli_observations::paper_target_corr_reopen_guard_nodes(
        &request.approved_target_nodes, &request.approved_corr_fingerprints);
    // An unrelated protected row may already be stale/reopened. Observe it
    // rather than silently changing the full guard's existing rejection rule.
    let guard_nodes: BTreeSet<_> = approved.into_iter().filter(|node| nodes.contains(node)
        || private.live.corr_current_fingerprints.get(node) != request.approved_corr_fingerprints.get(node)).collect();
    let mut roots = nodes.union(&guard_nodes).cloned().collect::<BTreeSet<_>>();
    for target in &targets {
        roots.extend(private.live.coverage.get(target).into_iter().flatten().cloned());
        roots.extend(private.live.protected_closure_nodes_per_target.get(target).into_iter().flatten().cloned());
    }
    // Deleted owners cause reverse invalidation but have no current compiled
    // observation. Their disappearance is separately source/graph-bound.
    nodes.retain(|node| !facts.deleted_nodes.contains(node));
    roots.retain(|node| !facts.deleted_nodes.contains(node));
    if !guard_nodes.is_disjoint(&facts.deleted_nodes) {
        return Err("cannot delete an actually protected guard owner".into());
    }
    Ok(ObservationScope { nodes, targets, guard_nodes, roots })
}

fn replace_rows<K: Ord + Clone, V: Clone>(destination: &mut BTreeMap<K, V>, keys: &BTreeSet<K>, observed: &BTreeMap<K, V>) {
    destination.retain(|key, _| !keys.contains(key));
    destination.extend(observed.iter().filter(|(key, _)| keys.contains(*key)).map(|(key, value)| (key.clone(), value.clone())));
}
fn replace_members(destination: &mut BTreeSet<NodeId>, keys: &BTreeSet<NodeId>, observed: BTreeSet<NodeId>) {
    destination.retain(|key| !keys.contains(key));
    destination.extend(observed.intersection(keys).cloned());
}

fn paper_observations(repo: &Path, input: &FingerprintInput, targets: &BTreeSet<TargetId>)
    -> Result<(BTreeMap<TargetId, String>, BTreeMap<TargetId, BTreeSet<NodeId>>), String> {
    let coverage = coverage_from_target_claims(&input.configured_targets, &input.target_claims, &input.present_nodes);
    let selected: BTreeMap<_, _> = coverage.into_iter().filter(|(target, _)| targets.contains(target))
        .map(|(target, nodes)| (target, nodes.difference(&input.under_model_assumption_nodes).cloned().collect::<BTreeSet<_>>())).collect();
    let covering = selected.values().flatten().cloned().collect();
    let descendants = runtime_cli_observations::observe_lean_relevant_definition_descendants_per_node(repo, &covering)?;
    let paper = observe_paper_faithfulness_fingerprints(repo, targets, &input.target_claims, &input.present_nodes,
        &input.approved_paper_fingerprints, &descendants, input.goal_prose_path.as_deref());
    let protected = runtime_cli_observations::observe_protected_closure_nodes(repo, &selected, &input.present_nodes)?;
    Ok((paper, protected))
}

/// Same observers as populate_response_fingerprints_from_input. Membership,
/// kinds and claims remain complete; only observation roots and applied rows
/// are selected. In particular, never pass a truncated `present_nodes` set.
pub(crate) fn observe_scoped(repo: &Path, input: &FingerprintInput, scope: &ObservationScope, response: &mut WorkerResponse)
    -> Result<(), String> {
    if !scope.nodes.is_subset(&input.present_nodes) || !scope.targets.is_subset(&input.configured_targets)
        || input.soundness_mode != format!("{:?}", runtime_cli_observations::soundness_fingerprint_mode()) {
        return Err("unsupported current scoped fingerprint operands".into());
    }
    let nodes = &scope.nodes;
    let corr = observe_correspondence_fingerprints_with_under_model_assumptions(repo, nodes, &input.under_model_assumption_nodes)?;
    let sound = observe_soundness_fingerprints(repo, nodes, &input.node_kinds, &input.node_role)?;
    let parts = observe_soundness_fingerprint_parts(repo, nodes, &input.node_kinds, &input.node_role)?;
    let deviations = observe_deviation_fingerprints(repo, &input.deviation_files)?;
    let substantive = runtime_cli_observations::observe_substantiveness_fingerprints(repo, nodes,
        input.paper_source_path.as_deref(), &input.node_kinds, &input.node_deviation_claims, &deviations,
        &input.configured_reference_papers, &input.node_reference_grounds)?;
    let sketch = runtime_cli_observations::observe_incomplete_argument_nodes(repo, nodes,
        &input.node_kinds, &input.node_role, &input.configured_targets);
    let placeholder = runtime_cli_observations::observe_placeholder_definition_nodes(repo, nodes, &input.node_kinds);
    let (paper, protected) = paper_observations(repo, input, &scope.targets)?;
    let snapshot = &mut response.snapshot;
    macro_rules! retain_nodes { ($($field:ident),+) => { $(snapshot.$field.retain(|node, _| input.present_nodes.contains(node));)+ }; }
    retain_nodes!(target_fingerprints, corr_current_fingerprints, sound_current_fingerprints,
        sound_current_fingerprint_parts, substantiveness_current_fingerprints);
    snapshot.paper_current_fingerprints.retain(|target, _| input.configured_targets.contains(target));
    snapshot.protected_closure_nodes_per_target.retain(|target, _| input.configured_targets.contains(target));
    snapshot.sketch_proof_nodes.retain(|node| input.present_nodes.contains(node));
    snapshot.placeholder_definition_nodes.retain(|node| input.present_nodes.contains(node));
    replace_rows(&mut snapshot.target_fingerprints, nodes, &corr);
    replace_rows(&mut snapshot.corr_current_fingerprints, nodes, &corr);
    replace_rows(&mut snapshot.sound_current_fingerprints, nodes, &sound);
    replace_rows(&mut snapshot.sound_current_fingerprint_parts, nodes, &parts);
    replace_rows(&mut snapshot.substantiveness_current_fingerprints, nodes, &substantive);
    replace_rows(&mut snapshot.paper_current_fingerprints, &scope.targets, &paper);
    replace_rows(&mut snapshot.protected_closure_nodes_per_target, &scope.targets, &protected);
    replace_members(&mut snapshot.sketch_proof_nodes, nodes, sketch);
    replace_members(&mut snapshot.placeholder_definition_nodes, nodes, placeholder);
    snapshot.deviation_current_fingerprints = deviations;
    snapshot.coverage = coverage_from_target_claims(&input.configured_targets, &input.target_claims, &input.present_nodes);
    Ok(())
}

fn staged_goal_path(repo: &Path, is_pv: bool, goal: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if !is_pv { return Ok(None); }
    goal.map(|goal| {
        if goal.is_absolute() { Ok(goal.to_path_buf()) }
        else { Ok(repo.canonicalize().map_err(|error| error.to_string())?.join(goal)) }
    }).transpose()
}

pub(crate) fn prepare_context(repo: &Path, request: &WrapperRequest, paper: Option<&Path>, goal: Option<&Path>,
    private: Option<&ProtocolState>, graph: Option<&trellis_kernel::dispatch::DependencyGraph>,
    owner_facts: Option<&OwnedProofFacts>, finalization: bool,
) -> Result<Value, String> {
    supported_request(repo, request)?;
    if owner_facts.is_some_and(|facts|
        facts.acceptance_logic_identity != trellis_kernel::model::acceptance_logic_identity())
        && (request.is_pv || request.phase != trellis_kernel::Phase::ProofFormalization) {
        return Err("reviewed staged owner facts require math proof formalization".into());
    }
    trellis_kernel::validate_native_extraction_registration(repo, &request.node_role)?;
    // Resolve the real PV referent once. Both the fingerprint observer (which
    // reads the supplied path directly) and its exact binding must see this
    // same absolute file; math continues to ignore the default GOAL.md carrier.
    let goal = staged_goal_path(repo, request.is_pv, goal)?;
    let input = worker_baseline_input(request, true, goal.as_deref());
    let modeling = request.phase == trellis_kernel::Phase::Modeling;
    if (!modeling && input.observation_plan.capture_scoped_tablet_baseline_errors)
        || input.observation_plan.capture_baseline_correspondence_hashes {
        return Err("staged baseline requires additional compiled consumers".into());
    }
    sync_tablet_render_support_from_repo(repo)?;
    let baseline_errors = if modeling {
        // Same authorized-node baseline as the ordinary gate, before overlay.
        // Do not donate or rewrite path-bearing baseline diagnostic strings.
        let mut roots = input.authorized_nodes.clone(); roots.insert("Preamble".into());
        let source_before = trellis_kernel::dispatch_observation::verifier_inputs(repo)?;
        // Ordinary Modeling preparation requires an observable build result,
        // not a successful build: genuine baseline type errors must reach the
        // same evaluator and conservative ordinary-lane fallback below.
        prepare_worker_checker_for_phase(repo, &roots, true)?;
        runtime_cli_observations::finish_observation_source_render(repo, &source_before,
            "staged Modeling baseline preparation changed source")?;
        let observed = runtime_cli_observations::observe_tablet_nodes_with_roles(repo,
            &input.authorized_nodes, &input.node_role)?;
        let evaluated = evaluate_tablet_observation_with_roles(repo, &observed,
            &input.node_role, input.assumption_authoring_node.as_ref());
        let errors = relevant_new_errors(&evaluated, &[], &input.authorized_nodes);
        if !finalization && !errors.is_empty() {
            return Ok(json!({"status":"unsupported", "fallback_class":"ordinary_modeling_baseline",
                "reason":"scoped Modeling baseline has existing errors; ordinary checking required"}));
        }
        errors
    } else { Vec::new() };
    let observations = trellis_kernel::prepare_worker_gate_observations(&WorkerGateObservationInput {
        repo_path: repo.to_path_buf(), current_present_nodes: input.current_present_nodes.clone(),
        active_node: input.active_node.clone(), under_model_assumption_nodes: input.under_model_assumption_nodes.clone(),
        assumption_authoring_node: input.assumption_authoring_node.clone(), observation_plan: input.observation_plan.clone(), collect_observations: true,
    })?;
    let baseline_corr = if input.lean_closure {
        let roots = BTreeSet::from([input.active_node.clone().ok_or("staged closure active missing")?]);
        prepare_receipt_observation_sources(repo, &roots)?;
        observe_correspondence_fingerprints_with_under_model_assumptions(repo, &roots, &input.under_model_assumption_nodes)?
    } else { BTreeMap::new() };
    let request_payload = bridge_request_payload(request, None, Some(repo))?;
    let facts = BaselineFacts { observations, baseline_errors, baseline_correspondence_hashes: baseline_corr,
        baseline_lint_diagnostics: BTreeMap::new(), current_paper_current_fingerprints: BTreeMap::new(),
        current_open_nodes: trellis_kernel::open_nodes_from_repo(repo, &input.current_present_nodes),
        current_sketch_proof_nodes: input.is_pv.then(|| runtime_cli_observations::observe_incomplete_argument_nodes(
            repo, &input.current_present_nodes, &input.current_node_kinds, &input.node_role, &input.configured_targets)),
        before_closure_source_digest: None };
    let mut context = assemble_worker_gate_output(repo, request, request_payload, paper, goal.as_deref(), &input, facts);
    let scope = if finalization {
        let private = private.ok_or("current private state required for staged finalization")?;
        let current = private.in_flight_request.as_deref().ok_or("missing current request")?;
        if current.id != request.id || current.cycle != request.cycle || private.live.present_nodes != request.current_present_nodes {
            return Err("staged finalization request/state mismatch".into());
        }
        let scope = final_scope(private, graph.ok_or("missing current source graph")?, owner_facts.ok_or("missing owned facts")?)?;
        let mut response = WorkerResponse { snapshot: private.live.clone(), ..Default::default() };
        let fp_input = fingerprint_input_for_context(repo, &context, &response);
        // Baseline observes only current, pre-overlay covering roots for the
        // affected targets. New helper roots do not exist on this side yet.
        let mut baseline_roots = BTreeSet::new();
        for target in &scope.targets { baseline_roots.extend(private.live.coverage.get(target).into_iter().flatten().cloned()); }
        if !baseline_roots.is_empty() { prepare_receipt_observation_sources(repo, &baseline_roots)?; }
        let (paper, _) = paper_observations(repo, &fp_input, &scope.targets)?;
        replace_rows(&mut response.snapshot.paper_current_fingerprints, &scope.targets, &paper);
        context.current_paper_current_fingerprints = response.snapshot.paper_current_fingerprints;
        Some(scope)
    } else { None };
    sync_tablet_render_support_from_repo(repo)?;
    let current_observations = trellis_kernel::prepare_worker_gate_observations(&WorkerGateObservationInput {
        repo_path: repo.to_path_buf(), current_present_nodes: input.current_present_nodes.clone(),
        active_node: input.active_node.clone(), under_model_assumption_nodes: input.under_model_assumption_nodes.clone(),
        assumption_authoring_node: input.assumption_authoring_node.clone(), observation_plan: input.observation_plan.clone(), collect_observations: true,
    })?;
    context.before_snapshot = current_observations.before_snapshot;
    context.before_tablet_contents = current_observations.before_tablet_contents;
    context.imports_before = current_observations.imports_before;
    context.expected_active_hash = current_observations.expected_active_hash;
    context.baseline_declaration_hashes = current_observations.baseline_declaration_hashes;
    Ok(json!({"status":"prepared", "context":context, "observation_scope":scope}))
}

fn fingerprint_input_for_context(repo: &Path, context: &PreparedWorkerGateOutput, response: &WorkerResponse) -> FingerprintInput {
    response_fingerprint_input(repo, &context.configured_targets, &context.current_target_claims,
        &context.current_deviation_files, &context.current_node_deviation_claims, &context.configured_reference_papers,
        &context.current_node_reference_grounds, &context.current_paper_approved_fingerprints,
        context.paper_source_path.as_deref(), context.goal_prose_path.as_deref(), &context.current_node_kinds,
        &context.node_role, &context.under_model_assumption_nodes, response)
}

fn unchanged_import_normalization(repo: &Path, nodes: &BTreeSet<NodeId>) -> Result<(), String> {
    let mut prerequisites = BTreeSet::new();
    trellis_kernel::cache_key::recursive_imports(repo, "Preamble", &mut prerequisites);
    for node in nodes {
        let content = fs::read_to_string(repo.join("Tablet").join(format!("{node}.lean"))).map_err(|e| e.to_string())?;
        if !prerequisites.contains(node.as_str()) && trellis_kernel::filespec::ensure_preamble_import_for_orphan(&content, node.as_str()).is_some() {
            return Err("staged candidate needs source-changing fallback import normalization".into());
        }
    }
    Ok(())
}

fn supported_payload(payload: &CheckedWorkerPayload, new_nodes: &BTreeSet<NodeId>) -> Result<(), String> {
    supported_payload_for_scope(payload, new_nodes, new_nodes, None, None)
}

// Difficulty labels are pure normalizer inputs, not compiler observations.
// Keep the extension restricted to the ordinary restructure contract; fresh
// finalization still runs the normalizer and the raw payload hash binds labels.
fn ordinary_restructure_metadata(request: &WrapperRequest) -> bool {
    !request.is_pv && request.phase == trellis_kernel::Phase::ProofFormalization
        && request.task_execution.as_ref().is_some_and(|execution|
            execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::Ordinary)
        && matches!(request.worker_acceptance.validation_execution_plan.as_slice(),
            [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                mode: WorkerProofDeltaMode::Restructure, .. }])
}

// Local work may introduce hard helper obligations. Only actual new helpers
// observed by this owned proof stage gain that metadata allowance; existing
// owners and unrelated helpers retain the original policy. Both admission and
// fresh finalization derive this set from their current source scope.
fn local_helper_difficulty_nodes(request: &WrapperRequest, new_nodes: &BTreeSet<NodeId>,
    observed_nodes: &BTreeSet<NodeId>,
) -> Option<BTreeSet<NodeId>> {
    (!request.is_pv && request.phase == trellis_kernel::Phase::ProofFormalization
        && request.task_execution.as_ref().is_some_and(|execution|
            execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::Ordinary)
        && matches!(request.worker_acceptance.validation_execution_plan.as_slice(),
            [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                mode: WorkerProofDeltaMode::Local, allow_new_obligations: true, .. }]))
        .then(|| new_nodes.intersection(observed_nodes).cloned().collect())
}

fn supported_payload_for_scope(payload: &CheckedWorkerPayload, new_nodes: &BTreeSet<NodeId>,
    metadata_nodes: &BTreeSet<NodeId>, targets: Option<&BTreeSet<TargetId>>,
    difficulty_nodes: Option<&BTreeSet<NodeId>>,
) -> Result<(), String> {
    supported_payload_for_scope_with_local_helpers(payload, new_nodes, metadata_nodes,
        targets, difficulty_nodes, None)
}

fn supported_payload_for_scope_with_local_helpers(payload: &CheckedWorkerPayload,
    new_nodes: &BTreeSet<NodeId>, metadata_nodes: &BTreeSet<NodeId>,
    targets: Option<&BTreeSet<TargetId>>, difficulty_nodes: Option<&BTreeSet<NodeId>>,
    local_helpers: Option<&BTreeSet<NodeId>>,
) -> Result<(), String> {
    if !payload.semantic_dep_updates.is_empty()
        || !payload.challenge_claim_updates.keys().all(|node| new_nodes.contains(&NodeId::from(node.as_str())))
        || payload.challenge_claim_updates.values().any(|claims| !claims.is_empty())
        || !payload.deviation_requests.is_empty() || !payload.node_deviation_claims.is_empty()
        || !payload.node_reference_grounds.is_empty() || !payload.deviation_deletions.is_empty()
        || payload.conditional_theorem_proposal.is_some()
        || !payload.conditional_theorem_withdrawals.is_empty() || payload.rust_witness_artifact.is_some()
        || !payload.authored_assumption_id.is_empty() || !payload.authored_axiom_name.is_empty()
        || !payload.authored_citation_locator.is_empty() || !payload.authored_rust_justification.is_empty()
        || !payload.authored_claim_class.is_empty()
        || !payload.target_claim_updates.keys().all(|node| metadata_nodes.contains(&NodeId::from(node.as_str())))
        || payload.target_claim_updates.values().any(|claims| match targets {
            Some(targets) => claims.iter().any(|claim| !targets.contains(claim.as_str())),
            None => !claims.is_empty(),
        })
        || payload.difficulty_updates.iter().any(|(node, difficulty)|
            !metadata_nodes.contains(&NodeId::from(node.as_str())) || match difficulty_nodes {
                Some(nodes) => !nodes.contains(&NodeId::from(node.as_str()))
                    || !matches!(difficulty.as_str(), "easy" | "hard"),
                None => targets.is_none() && difficulty != "easy"
                    && !(difficulty == "hard" && new_nodes.contains(node.as_str())
                        && local_helpers.is_some_and(|nodes| nodes.contains(node.as_str()))),
            }) {
        return Err("unsupported staged structural or trust payload".into());
    }
    Ok(())
}

fn checked_payload(repo: &Path, context: &PreparedWorkerGateOutput, raw: &Value)
    -> Result<(CheckedWorkerPayload, Value, ExecuteWorkerValidationPlanInput), String> {
    let request: WrapperRequest = serde_json::from_value(context.request.clone()).map_err(|error| error.to_string())?;
    supported_request(repo, &request)?;
    let closure = request.task_execution.as_ref().is_some_and(|execution|
        execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::LeanClosure);
    let allowed = worker_allowed_outcomes_for_validation(&serde_json::to_value(context).map_err(|e| e.to_string())?)?;
    let validated = validate_worker_task_payload(raw, &allowed, closure);
    if !validated.ok { return Err(validated.errors.join("\n")); }
    let data = validated.data.ok_or("staged payload validation returned no data")?;
    let payload: CheckedWorkerPayload = serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
    if payload.outcome != "valid" { return Err("staged proof requires requested Valid".into()); }
    let input = refresh_validation_input(repo, context, &payload)?;
    let scope = owned_scope(&input)?;
    if request.phase == trellis_kernel::Phase::Modeling {
        let writes = modeling::facts(&input, raw)?.write_nodes;
        if writes.iter().any(|node| matches!(request.node_role.get(node),
            Some(trellis_kernel::PvRole::UnderModelAssumptions | trellis_kernel::PvRole::ExtractionModel))) {
            return Err("Modeling trust/model source edits require ordinary exclusive checking".into());
        }
    }
    let ordinary_nodes = scope.observation_nodes.iter().filter(|node| node.as_str() != "Preamble").cloned().collect();
    unchanged_import_normalization(repo, &ordinary_nodes)?;
    let metadata_nodes = request.worker_acceptance.authorized_nodes.union(&scope.new_nodes).cloned().collect();
    let local_helpers = local_helper_difficulty_nodes(&request, &scope.new_nodes, &scope.observation_nodes);
    supported_payload_for_scope_with_local_helpers(&payload, &scope.new_nodes, &metadata_nodes,
        (request.phase == trellis_kernel::Phase::Modeling).then_some(&request.configured_targets),
        ordinary_restructure_metadata(&request).then_some(&scope.observation_nodes),
        local_helpers.as_ref())?;
    Ok((payload, data, input))
}

pub(crate) fn check_owned_request(repo: &Path, context: PreparedWorkerGateOutput, raw: Value,
    graph: trellis_kernel::dispatch::DependencyGraph, config: Option<&PreparedConfigBinding>,
) -> Result<Value, String> {
    let (payload, _, input) = checked_payload(repo, &context, &raw)?;
    let mut errors = worker_filespec_errors(repo, &compute_changed_node_stems_for_autofix(&context.before_snapshot, repo));
    let request: WrapperRequest = serde_json::from_value(context.request.clone()).map_err(|e| e.to_string())?;
    if request.task_execution.as_ref().is_some_and(|execution| execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::LeanClosure) {
        errors.extend(trellis_kernel::dispatch_contract::lean_closure_source_errors(repo,
            &context.before_snapshot, &context.before_tablet_contents,
            request.active_node.as_ref().ok_or("missing closure active")?, true));
    }
    if request.phase != trellis_kernel::Phase::Modeling {
        let preflight = worker_acceptance_input_from_checked_payload(repo, &context, &payload,
            input.validation_execution_plan.clone(), skipped_validation_results(&input.validation_execution_plan), BTreeSet::new())?;
        errors.extend(accept_worker_response(&preflight)?.contract_errors);
    } // Modeling global goal/challenge transforms run only in current finalization.
    if !errors.is_empty() {
        return negative_facts(&input, &raw, &graph, config, errors);
    }
    check_owned(&input, &raw, &graph, config)
}

fn negative_facts(input: &ExecuteWorkerValidationPlanInput, raw: &Value,
    graph: &trellis_kernel::dispatch::DependencyGraph, config: Option<&PreparedConfigBinding>, errors: Vec<String>,
) -> Result<Value, String> {
    let scope = owned_scope(input)?;
    let observations = OwnerObservations::default();
    let mut graph = candidate_graph(input, &observations, graph, &scope.observation_nodes, &scope.final_present_nodes)?;
    let read_roots = scope.observation_nodes.union(&scope.deleted_nodes).cloned().collect();
    let reads = if modeling::shape(&input.validation_execution_plan) {
        modeling::reserve_imports(&input.repo_path, &mut graph, &read_roots, &scope.deleted_nodes)?
    } else { graph.closure(&read_roots)? };
    let sources = source_projection(&trellis_kernel::dispatch_observation::verifier_inputs(&input.repo_path)?,
        &scope.final_present_nodes, &reads, &scope.deleted_nodes)?;
    let mut steps = if modeling::shape(&input.validation_execution_plan) {
        skipped_validation_results(&input.validation_execution_plan)
    } else { vec![] };
    let failure = WorkerValidationStepResult { kind: if steps.is_empty() { "proof_worker_delta" } else { "scoped_tablet" }.into(), ok: false,
        detail: errors.first().cloned().unwrap_or_default(), errors: errors.clone(),
        ..Default::default() };
    if let Some(last) = steps.last_mut() { *last = failure; } else { steps.push(failure); }
    let facts = OwnedProofFacts { modeling: if modeling::shape(&input.validation_execution_plan) { Some(modeling::facts(input, raw)?) } else { None }, schema_version: 1, acceptance_logic_identity: trellis_kernel::model::acceptance_logic_identity(),
        payload_sha256: validation_fact_receipt::complete_digest("staged_payload", raw).ok_or("staged payload bound")?,
        owner_ok: false, failed_step_results: steps, validation_execution_plan_sha256:
            validation_fact_receipt::complete_digest("staged_plan", &input.validation_execution_plan).ok_or("staged plan bound")?,
        owner_nodes: scope.observation_nodes, deleted_nodes: scope.deleted_nodes, read_nodes: reads,
        source_inputs_sha256: validation_fact_receipt::source_inputs_digest(&sources, config).ok_or("staged source binding")?,
        source_inputs: sources, observation_environment: None, observations, candidate_graph: graph };
    Ok(json!({"status":"rejected", "facts":facts, "errors":errors}))
}

fn record_final_observation_failure(output: &mut WorkerAcceptanceOutput, error: String) {
    output.validation_errors.push(error.clone()); output.errors.push(error);
    output.final_outcome = WorkerOutcome::Invalid; output.ok = false;
    output.response.outcome = WorkerOutcome::Invalid;
}

fn result_output(mut output: WorkerAcceptanceOutput, data: Value, steps: Vec<WorkerValidationStepResult>)
    -> Result<CheckedTrellisWorkerResultOutput, String> {
    output.response.elaboration_costs = runtime_cli_observations::take_elaboration_costs();
    output.response.acceptance_check_passed = output.ok && output.final_outcome == WorkerOutcome::Valid;
    output.response.deterministic_rejection_reasons = if output.final_outcome == WorkerOutcome::Invalid {
        output.errors.clone() } else { Vec::new() };
    let mut response = serde_json::to_value(output.response).map_err(|error| error.to_string())?;
    response.as_object_mut().ok_or("staged response is not an object")?.insert("kind".into(), json!("worker"));
    Ok(CheckedTrellisWorkerResultOutput { acceptance_read_trace: None, validation_fact_receipt: None,
        validation_fact_reuse: None, acceptance_context_input_sha256: None, publication_observations: None,
        final_environment: None, ok: output.ok, errors: output.errors, data: Some(data), response: Some(response),
        validation_step_results: steps, contract_errors: output.contract_errors, validation_errors: output.validation_errors,
        final_outcome: format!("{:?}", output.final_outcome).to_ascii_lowercase() })
}

/// Failed cold materialization can still have copied its pre-overlay source
/// projections. Finish the same native render as successful preparation before
/// checking exact source equality. Rendering failure is Cold; the original
/// compiler error remains available for honest native Invalid classification.
pub(super) fn finish_failed_final_preparation(repo: &Path, preparation: &Result<(), String>) -> Result<(), String> {
    if preparation.is_err() {
        sync_tablet_render_support_from_repo(repo)?;
    }
    Ok(())
}

/// A cold correspondence observation may restore the supervisor's old support
/// projections. Render again, then compare every source input; authored edits
/// and source-changing normalization still invalidate the saved observations.
pub(super) fn finish_staged_normalization(repo: &Path, sources: &BTreeMap<String, String>) -> Result<(), String> {
    runtime_cli_observations::finish_observation_source_render(repo, sources, "staged normalization changed source; owned facts remain retained")
}

pub(crate) fn finalize(repo: &Path, private: &ProtocolState, graph: &trellis_kernel::dispatch::DependencyGraph,
    context: PreparedWorkerGateOutput, raw: Value, facts: &OwnedProofFacts,
    environment: Option<&RefreshEnvironment>, config: Option<&PreparedConfigBinding>,
) -> Result<Value, String> {
    let request = private.in_flight_request.as_deref().ok_or("missing current staged request")?;
    supported_request(repo, request)?;
    if facts.acceptance_logic_identity != trellis_kernel::model::acceptance_logic_identity()
        && (request.is_pv || request.phase != trellis_kernel::Phase::ProofFormalization) {
        return Err("reviewed staged owner facts require math proof formalization".into());
    }
    if request_id_from_value(&context.request) != request.id || cycle_from_value(&context.request) != request.cycle
        || context.current_present_nodes != private.live.present_nodes
        || context.current_deps != private.deps || context.current_node_kinds != private.node_kinds
        || context.current_target_claims != private.target_claims {
        return Err("staged finalization context is not current".into());
    }
    let (payload, data, mut input) = checked_payload(repo, &context, &raw)?;
    let scope = final_scope(private, graph, facts)?;
    sync_tablet_render_support_from_repo(repo)?;
    let sources = trellis_kernel::dispatch_observation::verifier_inputs(repo)?;
    let mut source_errors = worker_filespec_errors(repo, &compute_changed_node_stems_for_autofix(&context.before_snapshot, repo));
    if request.task_execution.as_ref().is_some_and(|execution| execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::LeanClosure) {
        source_errors.extend(trellis_kernel::dispatch_contract::lean_closure_source_errors(repo,
            &context.before_snapshot, &context.before_tablet_contents,
            request.active_node.as_ref().ok_or("missing closure active")?, true));
    }
    if facts.owner_ok && !source_errors.is_empty() { return Err("staged sources no longer satisfy FILESPEC".into()); }
    // Correspondence protection is observed on the actual affected roots.
    // Unaffected roots with uncertain/mismatched canonical fingerprints were
    // also included by final_scope, preserving the full guard's refusal rule.
    input.approved_target_nodes = scope.guard_nodes.clone();
    input.approved_corr_fingerprints.retain(|node, _| scope.guard_nodes.contains(node));
    let executed = finalize_owned_step(&input, &raw, facts, environment, config)?;
    let acceptance = worker_acceptance_input_from_checked_payload(repo, &context, &payload,
        input.validation_execution_plan.clone(), executed.step_results.clone(), executed.protected_semantic_change_nodes)?;
    let mut output = accept_worker_response(&acceptance)?;
    output.response.summary = payload.summary.trim().into();
    output.response.comments = payload.comments.trim().into();
    // Preserve the normal PV response's advisory feedback. These fields do
    // not supply proof authority, but dropping them changes reviewer context.
    output.response.under_model_disproof = payload.under_model_disproof.trim().into();
    output.response.under_model_route_opinion = payload.under_model_route_opinion.trim().into();
    output.response.under_model_reasoning = payload.under_model_reasoning.trim().into();
    if !facts.owner_ok && output.final_outcome == WorkerOutcome::Valid {
        return Err("failed owner facts cannot become acceptance".into());
    }
    if !output.ok || output.final_outcome != WorkerOutcome::Valid {
        return Ok(json!({"status":"checked", "output":result_output(output, data, executed.step_results)?}));
    }
    // Source-changing normalization/autofix is deliberately outside this
    // donor lane. Do not accept a probe for pre-edit bytes after mutation.
    finish_staged_normalization(repo, &sources)?;
    if output.response.snapshot.present_nodes != owned_scope(&input)?.final_present_nodes {
        return Err("staged normalized membership disagrees with actual owned delta".into());
    }
    let expected_membership = output.response.snapshot.present_nodes.clone();
    // Preserve current sibling facts; pure normalization owns full source
    // membership/open/kind/dependency fields. Scoped observers overlay only
    // rows whose actual sources/consumers may have changed.
    FingerprintFacts::capture(&private.live).apply(&mut output.response.snapshot);
    let fp_input = fingerprint_input_for_context(repo, &context, &output.response);
    let referents = fp_input.referents(repo).ok_or("staged fingerprint referents unavailable")?;
    // A changed secondary definition may compile in its owner while making
    // a current downstream consumer fail. Preserve ordinary native Invalid
    // classification; an observation error is not an invitation to repeat
    // successful owner proof work. Source/environment drift remains Cold.
    // Empty materialization means ALL present in the checker. No final
    // compiled consumer is selected after deleting an unused entire scope.
    let preparation = if scope.roots.is_empty() { Ok(()) }
        else { prepare_receipt_observation_sources(repo, &scope.roots) };
    finish_failed_final_preparation(repo, &preparation)?;
    replay_current(&input, &raw, facts, environment, config)?;
    match preparation {
        Err(error) => record_final_observation_failure(&mut output, error),
        Ok(()) => if let Err(error) = observe_scoped(repo, &fp_input, &scope, &mut output.response) {
            record_final_observation_failure(&mut output, error);
        },
    }
    if output.response.snapshot.present_nodes != expected_membership { return Err("scoped observer changed membership".into()); }
    if output.ok {
        if let Some(error) = proof_protected_package_legality_error(&context.validation_kind, &context, &output.response) {
            output.ok = false; output.final_outcome = WorkerOutcome::Invalid;
            output.response.outcome = WorkerOutcome::Invalid; output.errors.push(error.clone()); output.validation_errors.push(error);
        }
        if request.task_execution.as_ref().is_some_and(|execution| execution.task_kind == trellis_kernel::dispatch::WorkerTaskKind::LeanClosure) {
            let node = request.active_node.as_ref().ok_or("missing final closure active")?;
            if let Some(error) = trellis_kernel::dispatch_contract::lean_closure_statement_error(
                context.baseline_correspondence_hashes.get(node), output.response.snapshot.corr_current_fingerprints.get(node)) {
                output.ok = false; output.final_outcome = WorkerOutcome::Invalid;
                output.response.outcome = WorkerOutcome::Invalid; output.errors.push(error.clone()); output.validation_errors.push(error);
            }
        }
    }
    if output.ok {
        // Same native source-backed issuer, against the CURRENT private base
        // and merged candidate. Only authenticated owned probes are supplied;
        // changed downstream consumers are observed and certified here.
        let mut trace = AcceptanceReadTrace::default();
        let replay = replay_current(&input, &raw, facts, environment, config)?;
        if let Err(error) = certify_final_worker_artifacts_with_policy(&context, &mut output.response, repo, &mut trace, None, environment,
            |repo, node| replay.certificate_policy(repo, node, runtime_cli_observations::run_local_closure_authoring_policy)) {
            record_final_observation_failure(&mut output, error);
        }
    }
    if sources != trellis_kernel::dispatch_observation::verifier_inputs(repo)?
        || fp_input.referents(repo).as_ref() != Some(&referents) {
        return Err("staged finalization sources or external referents changed".into());
    }
    replay_current(&input, &raw, facts, environment, config)?;
    Ok(json!({"status":"checked", "output":result_output(output, data, executed.step_results)?,
        "observation_scope":scope}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trellis_kernel::dispatch::{CompilerCapability, DependencyGraph, DispatchConfig, EditScope, TaskExecutionContext, WorkerTaskKind};
    fn ns(names: &[&str]) -> BTreeSet<NodeId> { names.iter().map(|name| NodeId::from(*name)).collect() }
    fn graph() -> DependencyGraph {
        DependencyGraph { edges: BTreeMap::from([
            ("H".into(), ns(&[])), ("A".into(), ns(&["H"])), ("B".into(), ns(&["H"])),
            ("Join".into(), ns(&["A", "B"])), ("C".into(), ns(&[])),
        ]) }
    }
    fn private() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.live.present_nodes = ns(&["H", "A", "B", "Join", "C"]);
        let mut request = WrapperRequest::default();
        request.kind = RequestKind::Worker;
        request.phase = trellis_kernel::Phase::ProofFormalization;
        request.active_node = Some("A".into());
        request.current_present_nodes = state.live.present_nodes.clone();
        request.task_execution = Some(TaskExecutionContext { task_id: 1, profile: "sol-xhigh".into(),
            binding: DispatchConfig::default().profiles["sol-xhigh"].clone(),
            compiler: CompilerCapability::Managed, task_kind: WorkerTaskKind::Ordinary });
        request.worker_acceptance.validation_execution_plan = vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some("A".into()), mode: WorkerProofDeltaMode::Restructure,
            authorized_nodes: ns(&["A"]), protected_semantic_change_nodes: ns(&[]),
            allow_new_obligations: true, must_close_active: true }];
        state.in_flight_request = Some(Box::new(request));
        state
    }
    fn facts() -> OwnedProofFacts {
        OwnedProofFacts { modeling: None, schema_version: 1, acceptance_logic_identity: "test".into(), payload_sha256: "test".into(),
            owner_ok: true, failed_step_results: vec![], validation_execution_plan_sha256: "test".into(),
            owner_nodes: ns(&["A"]), deleted_nodes: ns(&[]), read_nodes: ns(&["A", "H"]),
            source_inputs: BTreeMap::new(), source_inputs_sha256: "test".into(), observation_environment: None,
            observations: OwnerObservations::default(), candidate_graph: graph() }
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("Tablet")).unwrap();
        for (node, imports) in [("H", "import Mathlib\n"), ("A", "import Tablet.H\n"), ("B", "import Tablet.H\n"),
            ("Join", "import Tablet.A\nimport Tablet.B\n"), ("C", "import Mathlib\n")] {
            fs::write(dir.path().join(format!("Tablet/{node}.lean")), format!("{imports}theorem {node} : True :=\n-- BODY\nby trivial\n")).unwrap();
            fs::write(dir.path().join(format!("Tablet/{node}.tex")), "statement").unwrap();
        }
        dir
    }
    fn raw() -> Value { json!({"outcome":"valid", "summary":"proof", "comments":"", "semantic_dep_updates":{},
        "target_claim_updates":{}, "difficulty_updates":{}, "deleted_nodes":[]}) }

    fn pv_fixture() -> (tempfile::TempDir, WrapperRequest) {
        let dir = repo();
        let source = "import Tablet.H\n\n-- [TABLET NODE: A]\ntheorem A : True := by\n-- BODY\n  trivial\n";
        fs::write(dir.path().join("Tablet/A.lean"), source).unwrap();
        fs::write(dir.path().join("Tablet/Assumptions.lean"), "axiom assumed : True\n").unwrap();
        fs::write(dir.path().join("Tablet/Assumptions.tex"), "assumed premise").unwrap();
        fs::write(dir.path().join("GOAL.md"), "Supplied program goal v1\n").unwrap();
        // Only the existing source renderer is allowed during ordinary staged
        // baseline preparation; accidental checker/Lean work fails the test.
        fs::create_dir_all(dir.path().join(".trellis/scripts")).unwrap();
        fs::write(dir.path().join(".trellis/scripts/check.py"), r#"
import json, pathlib, sys
assert sys.argv[1] == 'sync-tablet-support', sys.argv[1]
payload = json.load(sys.stdin)
updated = []
for key in ['header_tex', 'index_md', 'readme_md']:
    content = payload.get(key + '_content')
    if content is not None:
        pathlib.Path(payload[key + '_path']).write_text(content)
        updated.append(payload[key + '_path'])
print(json.dumps(dict(updated_paths=updated,
    **{key + '_path': payload[key + '_path'] for key in ['header_tex', 'index_md', 'readme_md']})))
"#).unwrap();
        let mut state = private();
        state.phase = Phase::ProofFormalization; state.stage = trellis_kernel::Stage::Worker;
        state.active_node = Some("A".into()); state.proof_edit_mode = trellis_kernel::ProofEditMode::Local;
        state.worker_execution = state.in_flight_request.as_deref().unwrap().task_execution.clone();
        state.in_flight_request = None; state.pv_tablet_configured = true;
        state.lean_statement_transforms_enabled = true;
        state.live.present_nodes.insert("Assumptions".into());
        state.proof_nodes = ns(&["A", "B", "H", "Join", "C"]);
        state.node_kinds = state.proof_nodes.iter().cloned().map(|node| (node, NodeKind::Proof)).collect();
        state.node_kinds.insert("Assumptions".into(), NodeKind::Definition);
        state.node_role = BTreeMap::from([("A".into(), trellis_kernel::PvRole::ExtractionModel),
            ("Assumptions".into(), trellis_kernel::PvRole::UnderModelAssumptions)]);
        let mut spec = trellis_kernel::ChallengeTargetSpec { name: "A".into(),
            kind: trellis_kernel::ChallengeTargetKind::Theorem,
            lean: trellis_kernel::filespec_split::prescribed_region(source, false).unwrap(), ..Default::default() };
        spec.provenance.source_sha256 = "source-pin".into();
        spec.provenance.extractor_toolchain_sha256 = "extractor-pin".into();
        state.configured_challenge_targets.insert("model:A".into(), spec);
        state.challenge_claims.insert("A".into(), BTreeSet::from(["model:A".into()]));
        state.pending_task = Some(trellis_kernel::PendingTask { node: Some("A".into()),
            mode: trellis_kernel::TaskMode::Local, allow_new_obligations: false,
            must_close_active: true, ..Default::default() });
        // Generate the actual native PF contract, including observation flags
        // and lowercase bridge serialization; do not hand-write a plan.
        let request = state.expected_request(17, RequestKind::Worker);
        (dir, request)
    }

    fn modeling_fixture(authorized: &[&str]) -> (tempfile::TempDir, ProtocolState, DependencyGraph, EditScope) {
        let (dir, pv) = pv_fixture();
        fs::write(dir.path().join("Tablet/Preamble.lean"), "import Tablet.H\n").unwrap();
        fs::write(dir.path().join("Tablet/Preamble.tex"), "Preamble").unwrap();
        for node in ["A", "B", "Join", "C"] {
            let path = dir.path().join(format!("Tablet/{node}.lean"));
            fs::write(&path, format!("import Tablet.Preamble\n{}", fs::read_to_string(&path).unwrap())).unwrap();
        }
        let mut state = private();
        state.phase = Phase::Modeling; state.stage = trellis_kernel::Stage::Worker;
        state.pv_tablet_configured = true; state.target_edit_mode = trellis_kernel::TargetEditMode::Restructure;
        state.live.present_nodes.insert("Preamble".into());
        state.active_node = Some("A".into());
        state.worker_execution = state.in_flight_request.as_deref().unwrap().task_execution.clone();
        state.node_role = pv.node_role; state.node_role.remove(&NodeId::from("A"));
        state.node_role.insert("H".into(), trellis_kernel::PvRole::ExtractionModel);
        state.configured_challenge_targets = pv.configured_challenge_targets;
        state.pending_task = Some(trellis_kernel::PendingTask { node: Some("A".into()),
            mode: trellis_kernel::TaskMode::Restructure, authorized_nodes: ns(authorized), ..Default::default() });
        state.configured_targets = BTreeSet::from(["t".into(), "new_target".into()]);
        state.in_flight_request = Some(Box::new(state.expected_request(17, RequestKind::Worker)));
        let mut graph = graph();
        graph.edges.insert("Preamble".into(), ns(&["H"]));
        for node in ["A", "B", "Join", "C"] { graph.edges.get_mut(node).unwrap().insert("Preamble".into()); }
        let edit = EditScope { nodes: ns(authorized), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
        (dir, state, graph, edit)
    }

    #[test]
    fn staged_modeling_generated_scope_keeps_real_reads_and_new_claim_targets() {
        let (dir, private, graph, edit) = modeling_fixture(&["A"]);
        let request = private.in_flight_request.as_deref().unwrap();
        assert_eq!(request.worker_acceptance.validation_kind, WorkerValidationKind::TheoremRestructure);
        assert!(modeling::shape(&request.worker_acceptance.validation_execution_plan));
        assert!(!request.configured_challenge_targets.is_empty());
        supported_request(dir.path(), request).unwrap();
        let before = snapshot_tablet_dir(dir.path());
        let path = dir.path().join("Tablet/A.lean");
        fs::write(&path, fs::read_to_string(&path).unwrap().replace("  trivial", "  exact True.intro")).unwrap();
        let mut payload = raw(); payload["target_claim_updates"] = json!({"A":["new_target"]});
        let planned = plan(dir.path(), &private, &graph, &before, &edit, &payload).unwrap();
        let writes: BTreeSet<NodeId> = serde_json::from_value(planned["write_nodes"].clone()).unwrap();
        let reads: BTreeSet<NodeId> = serde_json::from_value(planned["read_nodes"].clone()).unwrap();
        assert_eq!(writes, ns(&["A"]));
        assert!(ns(&["A", "H", "Preamble", "Join", "B"]).is_subset(&reads));
        assert!(!writes.contains("Preamble"));
        let mut facts = facts();
        facts.owner_nodes = reads;
        facts.modeling = Some(modeling::ModelingFacts { write_nodes: writes,
            effect_targets: BTreeSet::from(["new_target".into()]) });
        let mut private = private;
        private.live.coverage.insert("new_target".into(), ns(&["C"]));
        let scope = final_scope(&private, &graph, &facts).unwrap();
        assert!(scope.targets.contains("new_target"));
        assert!(scope.roots.contains("C"), "new claims need the old target's covering roots");
        assert!(!scope.nodes.contains("Preamble"), "shared reads must not seed the reverse effect cone");
        let content = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("import Tablet.C\n{content}")).unwrap();
        let changed = plan(dir.path(), &private, &graph, &before, &edit, &payload).unwrap();
        assert!(changed["read_nodes"].as_array().unwrap().contains(&json!("C")), "new imports create admission conflicts");
        let mut bad = payload; bad["target_claim_updates"] = json!({"C":["new_target"]});
        assert!(plan(dir.path(), &private, &graph, &before, &edit, &bad).is_err());
    }

    #[test]
    fn staged_modeling_plan_declines_unchanged_observed_import_normalization() {
        let (dir, private, graph, edit) = modeling_fixture(&["A", "B"]);
        let b = dir.path().join("Tablet/B.lean");
        let canonical = fs::read(&b).unwrap();
        fs::write(&b, "theorem B : True :=\n-- BODY\nby trivial\n").unwrap();
        let before = snapshot_tablet_dir(dir.path());
        let a = dir.path().join("Tablet/A.lean");
        fs::write(&a, fs::read_to_string(&a).unwrap().replace("  trivial", "  exact True.intro")).unwrap();
        let candidate = snapshot_tablet_dir(dir.path());
        assert_eq!(before.get("B.lean"), candidate.get("B.lean"));
        assert_ne!(before.get("A.lean"), candidate.get("A.lean"));
        assert!(plan(dir.path(), &private, &graph, &before, &edit, &raw()).unwrap_err()
            .contains("source-changing fallback import normalization"));
        assert_eq!(snapshot_tablet_dir(dir.path()), candidate, "preflight cannot repair sources");
        // With an already-normalized prerequisite the same actual A-only
        // candidate remains eligible; B does not become a source writer.
        fs::write(&b, canonical).unwrap();
        let mut normalized_before = before;
        normalized_before.insert("B.lean".into(), snapshot_tablet_dir(dir.path())["B.lean"].clone());
        let planned = plan(dir.path(), &private, &graph, &normalized_before, &edit, &raw()).unwrap();
        assert_eq!(planned["write_nodes"], json!(["A"]));
    }

    #[test]
    fn staged_modeling_all_deleted_uses_only_preamble_artifacts_and_no_final_roots() {
        let (dir, mut private, graph, edit) = modeling_fixture(&["A"]);
        // A genuinely unused owner has no current or candidate reverse users.
        fs::write(dir.path().join("Tablet/Join.lean"), "import Tablet.B\ntheorem Join : True := by trivial\n").unwrap();
        let mut graph = graph; graph.edges.insert("Join".into(), ns(&["B"]));
        let before = snapshot_tablet_dir(dir.path());
        fs::remove_file(dir.path().join("Tablet/A.lean")).unwrap();
        fs::remove_file(dir.path().join("Tablet/A.tex")).unwrap();
        let request = private.in_flight_request.as_deref().unwrap();
        let scope = modeling::scope(dir.path(), &before, &private.live.present_nodes,
            request.active_node.as_ref(), &ns(&["A"]), &ns(&[]),
            &request.worker_acceptance.validation_execution_plan).unwrap();
        assert_eq!(scope.observation_nodes, ns(&["Preamble"]));
        let mut payload = raw(); payload["deleted_nodes"] = json!(["A"]);
        let planned = plan(dir.path(), &private, &graph, &before, &edit, &payload).unwrap();
        assert!(planned["read_nodes"].as_array().unwrap().contains(&json!("A")), "deleted names remain reserved");
        let mut facts = facts(); facts.owner_nodes = scope.observation_nodes;
        facts.deleted_nodes = ns(&["A"]);
        facts.modeling = Some(modeling::ModelingFacts { write_nodes: ns(&["A"]), effect_targets: BTreeSet::new() });
        private.live.coverage.clear(); private.live.protected_closure_nodes_per_target.clear();
        let final_scope = final_scope(&private, &graph, &facts).unwrap();
        assert!(final_scope.nodes.is_empty() && final_scope.roots.is_empty());
        assert_eq!(facts.owner_nodes, ns(&["Preamble"]));
    }

    #[test]
    fn staged_modeling_rejects_trust_writes_and_unscoped_plans() {
        let (dir, private, graph, edit) = modeling_fixture(&["A"]);
        let before = snapshot_tablet_dir(dir.path());
        for change in ["global", "assumptions", "corr_baseline", "wrong_baseline", "modeling_draft"] {
            let mut request = private.in_flight_request.as_deref().unwrap().clone();
            match change {
                "global" => request.worker_acceptance.validation_kind = WorkerValidationKind::TheoremGlobal,
                "assumptions" => request.trust_base_required_v1 = true,
                "corr_baseline" => request.worker_acceptance.observation_plan.capture_baseline_correspondence_hashes = true,
                "wrong_baseline" => request.worker_acceptance.observation_plan.scoped_tablet_baseline_scope = trellis_kernel::WorkerBaselineScope::AllPresent,
                "modeling_draft" => request.modeling_draft = true,
                _ => unreachable!(),
            }
            assert!(supported_request(dir.path(), &request).is_err(), "{change}");
        }
        let mut private = private;
        private.in_flight_request.as_mut().unwrap().node_role.insert("A".into(), trellis_kernel::PvRole::ExtractionModel);
        let mut payload = raw(); payload["target_claim_updates"] = json!({"A":["t"]});
        assert!(plan(dir.path(), &private, &graph, &before, &edit, &payload).unwrap_err().contains("trust/model"));
    }

    #[test]
    fn staged_modeling_canonical_header_rejects_hidden_direct_and_transitive_reads() {
        let parse = trellis_kernel::cache_key::canonical_header_tablet_imports;
        assert_eq!(parse("/- import Tablet.Hidden /- nested -/ -/\n  import Tablet.A -- comment\nimport Mathlib\n-- marker\ntheorem t : True := by trivial"),
            Some(BTreeSet::from(["A".into()])));
        for source in ["import Mathlib Tablet.B\ntheorem t := 1", "import Mathlib\n Tablet.B\ntheorem t := 1",
            "import\n Tablet.B\ntheorem t := 1", "public import Tablet.B", "import Tablet.«B»",
            "import Tablet.A\n/-comment-/ Tablet.B\ntheorem t := 1"] {
            assert!(parse(source).is_none(), "{source}");
        }
        let (dir, private, graph, edit) = modeling_fixture(&["A"]);
        let before = snapshot_tablet_dir(dir.path());
        let path = dir.path().join("Tablet/H.lean");
        // The owned source is conventional, but its existing dependency can
        // contain the hidden read. This must be caught before any owned check.
        fs::write(path, "import Mathlib Tablet.C\ntheorem H : True := by trivial").unwrap();
        let current_before = snapshot_tablet_dir(dir.path());
        let result = plan(dir.path(), &private, &graph, &current_before, &edit, &raw());
        assert!(result.unwrap_err().contains("canonical single-module imports: H"));
        assert_ne!(before, current_before);
    }

    #[test]
    fn staged_modeling_replayed_tablet_rechecks_sources_and_current_native_basis() {
        let (dir, private, _, _) = modeling_fixture(&["A"]);
        let repo = dir.path();
        let command: runtime_cli_observations::ExternalCommandObservation = serde_json::from_value(json!({
            "returncode":0,"stdout":"","stderr":"","timed_out":false,"spawn_error":""})).unwrap();
        let node = NodeObservation { node:"A".into(), lean_path:"/old/Tablet/A.lean".into(), tex_path:"/old/Tablet/A.tex".into(),
            lean_exists:true, tex_exists:true, lean_content:fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            tex_content:fs::read_to_string(repo.join("Tablet/A.tex")).unwrap(), compile:command.clone(), print_axioms:command.clone() };
        let tablet = runtime_cli_observations::TabletObservation { tablet_exists:true, global_errors:vec!["old external basis error".into()],
            preamble:runtime_cli_observations::PreambleObservation { lean_exists:true, tex_exists:true,
                lean_content:fs::read_to_string(repo.join("Tablet/Preamble.lean")).unwrap(),
                tex_content:fs::read_to_string(repo.join("Tablet/Preamble.tex")).unwrap(), compile:Some(command.clone()) },
            node_names:vec!["A".into()], invalid_node_names:vec![], missing_tex_for_lean:vec![], orphan_tex_nodes:vec![],
            nodes:BTreeMap::from([("A".into(), node)]), build:command };
        let observations = OwnerObservations { tablet:Some(tablet), ..Default::default() };
        let replay = Replay(&observations, std::cell::Cell::new(false));
        let current = replay.tablet(repo, &ns(&["A"]), &private.node_role).unwrap();
        assert!(current.global_errors.is_empty(), "do not reuse external native-basis observations");
        assert_eq!(current.nodes["A"].lean_path, repo.join("Tablet/A.lean").display().to_string());
        fs::create_dir(repo.join("modeling-input")).unwrap();
        fs::write(repo.join("modeling-input/manifest.json"), r#"{"provenance_schema":"changed"}"#).unwrap();
        assert_eq!(replay.tablet(repo, &ns(&["A"]), &private.node_role).unwrap().global_errors,
            vec!["Unrecognized native preparation provenance schema"]);
        fs::write(repo.join("Tablet/A.lean"), "changed secondary definition, same principal theorem").unwrap();
        assert!(replay.tablet(repo, &ns(&["A"]), &private.node_role).unwrap_err().contains("source changed"));
        assert!(replay.binding_failed());
    }

    fn capture_fixture() -> RefreshEnvironment {
        RefreshEnvironment { version:1, environment_sha256s:BTreeMap::from([("A".into(), "a".repeat(64))]),
            artifact_files:BTreeMap::from([(PathBuf::from("/exact/A.olean"), Some("b".repeat(64)))]) }
    }

    #[test]
    fn staged_owned_capture_failure_is_typed_bounded_and_never_facts() {
        for phase in ["preparation", "before_observations", "after_observations"] {
            let error = Err((phase, CaptureFailure { reason:"missing import inventory; ".repeat(80), retryable:true, diagnostic:None }));
            let result = if phase == "after_observations" {
                owned_observation_environment(true, Ok(capture_fixture()), error.map(Some), &ns(&["A"]))
            } else {
                owned_observation_environment(true, error, Ok(None), &ns(&["A"]))
            }.unwrap().unwrap_err();
            assert_eq!(result["retry_class"], "owned_environment_capture");
            assert_eq!(result["capture_phase"], phase);
            assert!(result["reason"].as_str().unwrap().len() < 560);
            assert!(result.get("facts").is_none());
            assert!(result.get("validation_step_results").is_none());
        }
    }

    #[test]
    fn staged_capture_keeps_bounded_diagnostics_without_promoting_binding_failures() {
        for (class, retryable) in [("environment_capture", true), ("environment_binding", false)] {
            let wire = json!({"environment":null,
                "cold_reason":"import resolution changed before initial content binding",
                "failure_class":class,
                "failure_detail":{"path":"/exact/External.olean", "changed_fields":["st_ctime_ns", "st_nlink"]},
                "measurement":{"files_hashed":5934, "bytes_hashed":1321248373u64}});
            let failure = parse_publication_environment(&wire, &ns(&["A"])).unwrap_err();
            let cold = owned_observation_environment(true, Ok(capture_fixture()),
                Err(("after_observations", failure)), &ns(&["A"])).unwrap().unwrap_err();
            assert_eq!(cold.get("retry_class").is_some(), retryable);
            assert_eq!(cold["environment_capture"]["failure_detail"], wire["failure_detail"]);
            assert_eq!(cold["environment_capture"]["measurement"], wire["measurement"]);
            assert!(cold.get("facts").is_none());
            assert!(cold.get("validation_step_results").is_none());
        }
        let large = json!({"failure_detail":{"path":"x".repeat(20000)},
            "measurement":{"stderr":"y".repeat(20000)}, "environment":{"never":"copy"}});
        let diagnostic = capture_failure_diagnostic(&large).unwrap();
        assert!(diagnostic.to_string().len() < 300);
        assert!(diagnostic["failure_detail"]["omitted"].is_string());
        assert!(diagnostic.get("environment").is_none());
    }

    #[test]
    fn staged_owned_capture_preserves_rejection_and_rejects_changed_success() {
        assert!(owned_observation_environment(false, Err(("preparation", CaptureFailure { reason:"type error".into(), retryable:true, diagnostic:None })),
            Ok(None), &ns(&["A"])).unwrap().unwrap().is_none());
        let before = capture_fixture();
        assert!(owned_observation_environment(true, Ok(before.clone()), Ok(Some(before.clone())),
            &ns(&["A"])).unwrap().unwrap().is_some());
        let mut after = before.clone(); after.artifact_files.values_mut().next().map(|x| *x = None);
        assert!(owned_observation_environment(true, Ok(before.clone()), Ok(Some(after)),
            &ns(&["A"])).unwrap_err().contains("changed during observation"));
        let mut after = before.clone(); after.environment_sha256s.insert("A".into(), "c".repeat(64));
        assert!(owned_observation_environment(true, Ok(before), Ok(Some(after)), &ns(&["A"])).is_err());
    }

    #[test]
    fn staged_capture_diagnostics_distinguish_transport_payload_and_coverage() {
        let roots = ns(&["A"]);
        assert_eq!(parse_publication_environment(&json!({"environment":null,"cold_reason":"import artifact changed during content capture"}),
            &roots).unwrap_err().reason, "import artifact changed during content capture");
        assert!(parse_publication_environment(&json!({"environment":{}}), &roots).unwrap_err().reason.contains("invalid environment inventory"));
        assert!(parse_publication_environment(&json!({"environment":capture_fixture()}), &ns(&["Missing"]))
            .unwrap_err().reason.contains("does not cover"));
        assert!(parse_publication_environment(&json!({"environment":capture_fixture()}), &roots).is_ok());
        for reason in ["import artifact changed during content capture", "import resolution changed during content capture", "import alias is not portable"] {
            let error = parse_publication_environment(&json!({"environment":null,"failure_class":"environment_binding","cold_reason":reason}), &roots).unwrap_err();
            assert!(!error.retryable);
            let output = owned_observation_environment(true, Err(("before_observations", error)), Ok(None), &roots).unwrap().unwrap_err();
            assert!(output.get("retry_class").is_none()); assert!(output.get("facts").is_none());
        }
        let unavailable = parse_publication_environment(&json!({"environment":null,"failure_class":"environment_capture","cold_reason":"import inventory unavailable or oversized"}), &roots).unwrap_err();
        assert!(unavailable.retryable);
        let dir = tempfile::tempdir().unwrap();
        assert!(capture_publication_environment_checked(dir.path(), &roots).unwrap_err().reason.contains("environment capture command"));
    }

    #[test]
    fn staged_pv_generated_request_keeps_roles_premises_goal_and_sketch_baseline() {
        let (dir, request) = pv_fixture();
        assert!(request.is_pv); assert!(!request.node_role.is_empty());
        assert!(!request.configured_challenge_targets.is_empty());
        assert_eq!(request.worker_acceptance.validation_kind, WorkerValidationKind::ProofLocal);
        supported_request(dir.path(), &request).unwrap();
        let prepared = prepare_context(dir.path(), &request, None, Some(Path::new("GOAL.md")), None, None, None, false).unwrap();
        let context: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        assert_eq!(context.node_role, request.node_role);
        assert_eq!(context.under_model_assumption_nodes, ns(&["Assumptions"]));
        assert_eq!(context.configured_challenge_targets, request.configured_challenge_targets);
        assert_eq!(context.current_challenge_claims, request.current_challenge_claims);
        assert_eq!(context.extraction_model_nodes, ns(&["A"]));
        assert_eq!(context.goal_prose_path, Some(dir.path().join("GOAL.md")));
        assert_eq!(context.current_sketch_proof_nodes, Some(runtime_cli_observations::observe_incomplete_argument_nodes(
            dir.path(), &request.current_present_nodes, &request.current_node_kinds, &request.node_role, &request.configured_targets)));
        let payload = CheckedWorkerPayload { outcome: "valid".into(), ..Default::default() };
        let input = refresh_validation_input(dir.path(), &context, &payload).unwrap();
        assert_eq!(input.node_role, request.node_role);
        assert_eq!(input.under_model_assumption_nodes, ns(&["Assumptions"]));
        let acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context, &payload,
            input.validation_execution_plan.clone(), skipped_validation_results(&input.validation_execution_plan), ns(&[])).unwrap();
        assert_eq!(acceptance.normalization.current_sketch_proof_nodes, context.current_sketch_proof_nodes);
        assert_eq!(acceptance.normalization.configured_challenge_targets, request.configured_challenge_targets);
        assert!(acceptance.lean_statement_transforms_enabled);
        let fp = fingerprint_input_for_context(dir.path(), &context, &WorkerResponse::default());
        let before = fp.referents(dir.path()).unwrap();
        assert!(before.contains_key(&dir.path().join("GOAL.md")));
        fs::write(dir.path().join("GOAL.md"), "changed current goal").unwrap();
        assert_ne!(before, fp.referents(dir.path()).unwrap());
        assert_eq!(staged_goal_path(dir.path(), false, Some(Path::new("GOAL.md"))).unwrap(), None);
    }

    #[test]
    fn staged_pv_keeps_current_challenge_pins_and_extraction_provenance() {
        let (dir, request) = pv_fixture();
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let context: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        let payload = CheckedWorkerPayload { outcome: "valid".into(), ..Default::default() };
        let acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context, &payload,
            request.worker_acceptance.validation_execution_plan.clone(), vec![], ns(&[])).unwrap();
        let mut normalization = acceptance.normalization;
        let errors = |input: &WorkerNormalizationInput| trellis_kernel::normalize_worker_response(input).unwrap().contract_errors;
        assert!(!errors(&normalization).iter().any(|error| error.contains("challenge") || error.contains("extraction-chain")));
        let source = fs::read_to_string(dir.path().join("Tablet/A.lean")).unwrap();
        fs::write(dir.path().join("Tablet/A.lean"), source.replace("A : True", "A : False")).unwrap();
        assert!(errors(&normalization).iter().any(|error| error.contains("challenge byte-conformance")));
        fs::write(dir.path().join("Tablet/A.lean"), source).unwrap();
        normalization.configured_challenge_targets.get_mut(&trellis_kernel::ChallengeTargetId::from("model:A")).unwrap().namespace_context = "namespace Native".into();
        assert!(errors(&normalization).iter().any(|error| error.contains("namespace-context")));
        let spec = normalization.configured_challenge_targets.get_mut(&trellis_kernel::ChallengeTargetId::from("model:A")).unwrap();
        spec.namespace_context.clear(); spec.provenance.extractor_toolchain_sha256.clear();
        assert!(errors(&normalization).iter().any(|error| error.contains("extraction-chain")));
    }

    #[test]
    fn staged_pv_helper_empty_challenge_claims_satisfy_actual_normalizer() {
        let (dir, request) = pv_fixture();
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let context: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.lean"),
            "import Tablet.H\n-- [TABLET NODE: P_Aux]\ntheorem P_Aux : True := by\n-- BODY\n  trivial\n").unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n").unwrap();
        let active = fs::read_to_string(dir.path().join("Tablet/A.lean")).unwrap();
        fs::write(dir.path().join("Tablet/A.lean"), active.replace("import Tablet.H", "import Tablet.H\nimport Tablet.P_Aux")).unwrap();
        let mut payload = CheckedWorkerPayload { outcome: "valid".into(), ..Default::default() };
        payload.target_claim_updates.insert("P_Aux".into(), BTreeSet::new());
        payload.challenge_claim_updates.insert("P_Aux".into(), BTreeSet::new());
        let scope = owned_scope(&refresh_validation_input(dir.path(), &context, &payload).unwrap()).unwrap();
        assert_eq!(scope.new_nodes, ns(&["P_Aux"]));
        supported_payload(&payload, &scope.new_nodes).unwrap();
        let acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context, &payload,
            request.worker_acceptance.validation_execution_plan.clone(), vec![], ns(&[])).unwrap();
        let mut normalization = acceptance.normalization;
        let normalized = trellis_kernel::normalize_worker_response(&normalization).unwrap();
        assert!(normalized.contract_errors.is_empty(), "{:?}", normalized.contract_errors);
        assert_eq!(normalized.challenge_claim_updates.get(&NodeId::from("P_Aux")), Some(&Update::Set(BTreeSet::new())));
        normalization.challenge_claim_updates.clear();
        assert!(trellis_kernel::normalize_worker_response(&normalization).unwrap().contract_errors.iter()
            .any(|error| error.contains("explicitly report challenge_claim_updates")));
        // Empty entries for existing owners can revoke claims, and nonempty
        // new entries can reassign them. Neither enters this staged subset.
        payload.challenge_claim_updates.insert("A".into(), BTreeSet::new());
        assert!(supported_payload(&payload, &scope.new_nodes).is_err());
        payload.challenge_claim_updates.remove("A");
        payload.challenge_claim_updates.insert("P_Aux".into(), BTreeSet::from(["model:A".into()]));
        assert!(supported_payload(&payload, &scope.new_nodes).is_err());
    }

    #[test]
    fn staged_pv_rechecks_frozen_native_registration_before_preparing() {
        let (dir, request) = pv_fixture();
        fs::create_dir_all(dir.path().join("modeling-input")).unwrap();
        fs::write(dir.path().join("modeling-input/manifest.json"),
            r#"{"model_support_schema":"trellis-model-support/v1"}"#).unwrap();
        fs::write(dir.path().join("Tablet/A.lean"), "theorem Wrong : True := by trivial\n").unwrap();
        let error = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap_err();
        assert!(error.contains("Frozen native extraction cannot be registered"), "{error}");
    }

    #[test]
    fn staged_pv_excludes_authoring_trust_and_uncovered_baseline_lanes() {
        let (dir, request) = pv_fixture();
        for change in ["modeling", "trust", "authored", "restructure", "baseline", "assumptions"] {
            let mut changed = request.clone();
            match change {
                "modeling" => { changed.phase = Phase::Modeling; changed.modeling_draft = true; },
                "trust" => changed.trust_base_required_v1 = true,
                "authored" => changed.configured_challenge_targets.get_mut(&trellis_kernel::ChallengeTargetId::from("model:A")).unwrap().statement_provenance =
                    trellis_kernel::model::StatementProvenance::WorkerAuthored,
                "restructure" => if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode, .. } =
                    &mut changed.worker_acceptance.validation_execution_plan[0] { *mode = WorkerProofDeltaMode::Restructure; },
                "baseline" => changed.worker_acceptance.observation_plan.capture_scoped_tablet_baseline_errors = true,
                "assumptions" => changed.active_node = Some("Assumptions".into()),
                _ => unreachable!(),
            }
            assert!(supported_request(dir.path(), &changed).is_err(), "{change}");
        }
        let mut authored = CheckedWorkerPayload::default(); authored.authored_axiom_name = "hidden_authoring".into();
        assert!(supported_payload(&authored, &ns(&[])).is_err());
        let mut claims = CheckedWorkerPayload::default(); claims.challenge_claim_updates.insert("A".into(), BTreeSet::from(["model:A".into()]));
        assert!(supported_payload(&claims, &ns(&[])).is_err());
    }

    #[test]
    fn staged_pv_owned_executor_retains_real_negative_classification_and_premise_gate() {
        struct Broken;
        impl ProofDeltaObservations for Broken {
            fn node(&self, _: &Path, _: &str) -> Result<NodeObservation, String> { Err("real proof compilation failure".into()) }
            fn closure(&self, _: &Path, _: &str, _: &BTreeSet<NodeId>) -> Result<LocalClosureProbeOutput, String> { panic!("failed node must short circuit") }
            fn policy(&self, _: &Path, _: &str) -> Result<LocalClosureProbeOutput, String> { panic!("failed node must short circuit") }
        }
        let (dir, request) = pv_fixture();
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let context: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        let mut input = refresh_validation_input(dir.path(), &context, &CheckedWorkerPayload::default()).unwrap();
        let result = execute_owned(&input, &Broken, ProofDeltaGuard::Deferred).unwrap();
        assert!(!result.step_results[0].ok);
        assert_eq!(result.step_results[0].errors, ["real proof compilation failure"]);
        input.under_model_assumption_nodes.clear();
        assert!(execute_owned(&input, &Broken, ProofDeltaGuard::Deferred).is_err());
    }

    fn mode_scope_input(repo: &Path, mode: WorkerProofDeltaMode) -> ExecuteWorkerValidationPlanInput {
        ExecuteWorkerValidationPlanInput {
            repo_path: repo.to_path_buf(), before_snapshot: snapshot_tablet_dir(repo),
            current_present_nodes: ns(&["H", "A", "B", "Join", "C"]), active_node: Some("A".into()),
            validation_execution_plan: vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                active_node: Some("A".into()), mode, authorized_nodes: ns(&[]),
                protected_semantic_change_nodes: ns(&[]), allow_new_obligations: false,
                must_close_active: true }], ..Default::default()
        }
    }

    #[test]
    fn staged_easy_local_implicit_active_scope_matches_plan_and_collects_helpers() {
        for mode in [WorkerProofDeltaMode::Easy, WorkerProofDeltaMode::Local] {
            let dir = repo(); let input = mode_scope_input(dir.path(), mode);
            fs::write(dir.path().join("Tablet/A.lean"),
                "import Tablet.H\ntheorem A : True :=\n-- BODY\nby exact True.intro\n").unwrap();
            // cf11: a legitimate ordinary Local payload with no explicit
            // authorized_nodes, changing only the active proof body.
            assert_eq!(owned_scope(&input).unwrap().observation_nodes, ns(&["A"]));
            fs::write(dir.path().join("Tablet/A.lean"),
                "import Tablet.H\nimport Tablet.P_Aux\ntheorem A : True :=\n-- BODY\nby exact P_Aux\n").unwrap();
            fs::write(dir.path().join("Tablet/P_Aux.lean"),
                "import Tablet.H\ntheorem P_Aux : True :=\n-- BODY\nby trivial\n").unwrap();
            fs::write(dir.path().join("Tablet/P_Aux.tex"), "helper").unwrap();
            let mut state = private();
            state.in_flight_request.as_deref_mut().unwrap().worker_acceptance.validation_execution_plan = input.validation_execution_plan.clone();
            let edit = EditScope { nodes: ns(&["A"]), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
            let planned = plan(dir.path(), &state, &graph(), &input.before_snapshot, &edit, &raw()).unwrap();
            assert_eq!(planned["write_nodes"], json!(["A", "P_Aux"]));
            let scope = owned_scope(&input).unwrap();
            assert_eq!(scope.observation_nodes, ns(&["A", "P_Aux"]));
            assert_eq!(scope.new_nodes, ns(&["P_Aux"]));
            assert!(scope.final_present_nodes.contains("P_Aux"));
            // The pre-existing ordinary-refresh wrapper remains Restructure-only.
            assert!(runtime_cli_observations::proof_worker_delta_receipt_scope(dir.path(),
                &input.before_snapshot, &input.current_present_nodes, &"A".into(), &ns(&[]), &ns(&[]), true).is_none());
        }
    }

    #[test]
    fn staged_easy_local_scope_never_borrows_restructure_existing_authorization() {
        for mode in [WorkerProofDeltaMode::Easy, WorkerProofDeltaMode::Local] {
            let dir = repo(); let mut input = mode_scope_input(dir.path(), mode);
            input.authorized_nodes = ns(&["A", "B"]);
            fs::write(dir.path().join("Tablet/B.tex"), "changed existing statement").unwrap();
            assert!(owned_scope(&input).is_err());
            fs::write(dir.path().join("Tablet/B.tex"), "statement").unwrap();
            fs::remove_file(dir.path().join("Tablet/B.lean")).unwrap();
            fs::remove_file(dir.path().join("Tablet/B.tex")).unwrap();
            input.declared_deleted_nodes = ns(&["B"]);
            assert!(owned_scope(&input).is_err());
        }
        let dir = repo(); let mut input = mode_scope_input(dir.path(), WorkerProofDeltaMode::Restructure);
        fs::write(dir.path().join("Tablet/A.lean"), "import Tablet.H\ntheorem A : True := by trivial\n").unwrap();
        assert!(owned_scope(&input).is_err());
        input.authorized_nodes = ns(&["A"]);
        assert_eq!(owned_scope(&input).unwrap().observation_nodes, ns(&["A"]));
    }

    #[test]
    fn staged_source_projection_binds_secondary_definitions_imports_and_globals() {
        let input: BTreeMap<String, String> = [
            ("Tablet/A.lean", "principal plus secondary definition v1"), ("Tablet/A.tex", "a"),
            ("Tablet/H.lean", "dependency-v1"), ("Tablet/B.lean", "sibling-v1"),
            ("Tablet/INDEX.md", "generated-v1"), ("trellis.config.json", "policy-v1"),
            ("reference/paper.tex", "paper-v1"),
        ].into_iter().map(|(k,v)| (k.into(),v.into())).collect();
        let projected = source_projection(&input, &ns(&["A","B","H"]), &ns(&["A","H"]), &ns(&[])).unwrap();
        for path in ["Tablet/A.lean", "Tablet/H.lean", "trellis.config.json", "reference/paper.tex"] {
            let mut changed = input.clone(); changed.insert(path.into(), "changed".into());
            assert_ne!(projected, source_projection(&changed, &ns(&["A","B","H"]), &ns(&["A","H"]), &ns(&[])).unwrap());
        }
        let mut sibling = input.clone(); sibling.insert("Tablet/B.lean".into(), "v2".into()); sibling.insert("Tablet/INDEX.md".into(), "v2".into());
        assert_eq!(projected, source_projection(&sibling, &ns(&["A","B","H"]), &ns(&["A","H"]), &ns(&[])).unwrap());
        assert_eq!(projected["Tablet/H.tex"], "absent");
    }

    #[test]
    fn staged_deleted_source_absence_is_explicit_and_reappearance_rejects() {
        let input = BTreeMap::from([("Tablet/A.lean".into(), "a".into())]);
        let projection = source_projection(&input, &ns(&["A"]), &ns(&["A","D"]), &ns(&["D"])).unwrap();
        assert_eq!(projection["Tablet/D.lean"], "absent"); assert_eq!(projection["Tablet/D.tex"], "absent");
        let mut changed = input; changed.insert("Tablet/D.tex".into(), "returns".into());
        assert!(source_projection(&changed, &ns(&["A"]), &ns(&["A","D"]), &ns(&["D"])).is_err());
    }

    #[test]
    fn staged_final_scope_reconciles_shared_consumer_preserves_unrelated_rows() {
        let mut state = private(); state.live.coverage.insert("t".into(), ns(&["Join"]));
        let scope = final_scope(&state, &graph(), &facts()).unwrap();
        assert_eq!(scope.nodes, ns(&["A","Join"])); assert_eq!(scope.targets, BTreeSet::from([TargetId::from("t")]));
        assert!(!scope.nodes.contains(&NodeId::from("B"))); assert!(!scope.nodes.contains(&NodeId::from("C")));
        let mut current = BTreeMap::from([("A".into(), "old-a"), ("B".into(), "newly-published-b"), ("Join".into(), "old-join")]);
        replace_rows(&mut current, &scope.nodes, &BTreeMap::from([("A".into(), "new-a"), ("Join".into(), "current-a-and-b")]));
        assert_eq!(current[&NodeId::from("B")], "newly-published-b"); assert_eq!(current[&NodeId::from("Join")], "current-a-and-b");
    }

    #[test]
    fn staged_final_scope_preserves_uncertain_unaffected_guard() {
        let mut state = private();
        let request = state.in_flight_request.as_deref_mut().unwrap();
        request.approved_target_nodes = ns(&["C"]); request.approved_corr_fingerprints.insert("C".into(), "approved".into());
        assert_eq!(final_scope(&state, &graph(), &facts()).unwrap().guard_nodes, ns(&["C"]));
        state.live.corr_current_fingerprints.insert("C".into(), "approved".into());
        assert!(final_scope(&state, &graph(), &facts()).unwrap().guard_nodes.is_empty());
    }

    #[test]
    fn staged_plan_reserves_shared_dependencies_without_reverse_consumers() {
        let dir = repo(); let before = snapshot_tablet_dir(dir.path());
        let state = private(); let edit = EditScope { nodes: ns(&["A"]), ..Default::default() };
        let planned = plan(dir.path(), &state, &graph(), &before, &edit, &raw()).unwrap();
        assert_eq!(planned["read_nodes"], json!(["A","H"])); assert_eq!(planned["write_nodes"], json!(["A"]));
    }

    #[test]
    fn staged_plan_includes_new_helper_imports_and_rejects_unsupported_metadata_early() {
        let dir = repo(); let before = snapshot_tablet_dir(dir.path());
        fs::write(dir.path().join("Tablet/P_Aux.lean"), "import Tablet.B\ntheorem P_Aux : True :=\n-- BODY\nby trivial\n").unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.tex"), "helper").unwrap();
        let mut raw = raw(); raw["target_claim_updates"] = json!({"P_Aux":[]}); raw["difficulty_updates"] = json!({"P_Aux":"easy"});
        let edit = EditScope { nodes: ns(&["A"]), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
        let planned = plan(dir.path(), &private(), &graph(), &before, &edit, &raw).unwrap();
        assert_eq!(planned["read_nodes"], json!(["A","B","H","P_Aux"]));
        raw["target_claim_updates"] = json!({"P_Aux":["t"]});
        assert!(plan(dir.path(), &private(), &graph(), &before, &edit, &raw).is_err());
    }

    #[test]
    fn staged_ordinary_restructure_difficulty_reserves_observed_owners_only() {
        let dir = repo(); let before = snapshot_tablet_dir(dir.path());
        fs::write(dir.path().join("Tablet/P_Aux.lean"), "import Tablet.B\ntheorem P_Aux : True :=\n-- BODY\nby trivial\n").unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.tex"), "helper").unwrap();
        let mut state = private();
        let request = state.in_flight_request.as_deref_mut().unwrap();
        request.worker_acceptance.authorized_nodes = ns(&["A", "C"]);
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { authorized_nodes, .. } =
            &mut request.worker_acceptance.validation_execution_plan[0] { *authorized_nodes = ns(&["A", "C"]); }
        let edit = EditScope { nodes: ns(&["A", "C"]), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
        let mut payload = raw(); payload["target_claim_updates"] = json!({"P_Aux":[]});
        payload["difficulty_updates"] = json!({"A":"easy", "P_Aux":"hard"});
        let planned = plan(dir.path(), &state, &graph(), &before, &edit, &payload).unwrap();
        assert_eq!(planned["write_nodes"], json!(["A", "P_Aux"]));
        assert_eq!(planned["read_nodes"], json!(["A", "B", "H", "P_Aux"]));
        for value in ["easy", "hard"] {
            payload["difficulty_updates"]["C"] = json!(value);
            assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err(),
                "merely authorized unchanged sibling must remain outside staged metadata scope");
        }
        payload["difficulty_updates"].as_object_mut().unwrap().remove("C");
        payload["difficulty_updates"]["P_Aux"] = json!("unknown");
        assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err());
        payload["difficulty_updates"]["P_Aux"] = json!("hard");
        payload["target_claim_updates"]["P_Aux"] = json!(["t"]);
        assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err());
    }

    #[test]
    fn staged_hard_difficulty_extension_does_not_expand_pv_or_other_modes() {
        let mut request = private().in_flight_request.unwrap();
        assert!(ordinary_restructure_metadata(&request));
        request.is_pv = true; assert!(!ordinary_restructure_metadata(&request));
        request.is_pv = false;
        request.task_execution.as_mut().unwrap().task_kind = WorkerTaskKind::LeanClosure;
        assert!(!ordinary_restructure_metadata(&request));
        request.task_execution.as_mut().unwrap().task_kind = WorkerTaskKind::Ordinary;
        for mode in [WorkerProofDeltaMode::Easy, WorkerProofDeltaMode::Local] {
            if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode: current, .. } =
                &mut request.worker_acceptance.validation_execution_plan[0] { *current = mode; }
            assert!(!ordinary_restructure_metadata(&request));
        }
        let mut payload = CheckedWorkerPayload::default();
        payload.difficulty_updates.insert("P_Aux".into(), "hard".into());
        assert!(supported_payload(&payload, &ns(&["P_Aux"])).is_err());
        // Modeling keeps its existing independently checked metadata policy.
        assert!(supported_payload_for_scope(&payload, &ns(&["P_Aux"]), &ns(&["P_Aux"]),
            Some(&BTreeSet::new()), None).is_ok());
    }

    #[test]
    fn staged_local_hard_helpers_reserve_only_new_observed_sources() {
        let dir = repo(); let before = snapshot_tablet_dir(dir.path());
        fs::write(dir.path().join("Tablet/P_Aux.lean"), "import Tablet.B\ntheorem P_Aux : True :=\n-- BODY\nby trivial\n").unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.tex"), "helper").unwrap();
        let mut state = private();
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode, .. } =
            &mut state.in_flight_request.as_deref_mut().unwrap().worker_acceptance.validation_execution_plan[0] {
            *mode = WorkerProofDeltaMode::Local;
        }
        let edit = EditScope { nodes: ns(&["A"]), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
        let mut payload = raw(); payload["target_claim_updates"] = json!({"P_Aux":[]});
        payload["difficulty_updates"] = json!({"P_Aux":"hard"});
        let planned = plan(dir.path(), &state, &graph(), &before, &edit, &payload).unwrap();
        assert_eq!(planned["write_nodes"], json!(["A", "P_Aux"]));
        assert_eq!(planned["read_nodes"], json!(["A", "B", "H", "P_Aux"]));
        for node in ["A", "B", "Missing"] {
            payload["difficulty_updates"][node] = json!("hard");
            assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err());
            payload["difficulty_updates"].as_object_mut().unwrap().remove(node);
        }
        payload["difficulty_updates"]["P_Aux"] = json!("unknown");
        assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err());
        payload["difficulty_updates"]["P_Aux"] = json!("hard");
        payload["node_deviation_claims"] = json!({"P_Aux":["claim"]});
        assert!(plan(dir.path(), &state, &graph(), &before, &edit, &payload).is_err());
    }

    #[test]
    fn staged_local_hard_helper_allowance_keeps_mode_and_observation_guards() {
        let mut request = private().in_flight_request.unwrap();
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode, .. } =
            &mut request.worker_acceptance.validation_execution_plan[0] { *mode = WorkerProofDeltaMode::Local; }
        let helpers = local_helper_difficulty_nodes(&request, &ns(&["P_Aux", "P_Unobserved"]), &ns(&["A", "P_Aux"])).unwrap();
        assert_eq!(helpers, ns(&["P_Aux"]));
        let mut payload = CheckedWorkerPayload::default();
        payload.difficulty_updates.insert("P_Unobserved".into(), "hard".into());
        assert!(supported_payload_for_scope_with_local_helpers(&payload, &ns(&["P_Aux", "P_Unobserved"]),
            &ns(&["A", "P_Aux", "P_Unobserved"]), None, None, Some(&helpers)).is_err());
        payload.difficulty_updates.clear(); payload.difficulty_updates.insert("A".into(), "easy".into());
        assert!(supported_payload_for_scope_with_local_helpers(&payload, &ns(&["P_Aux"]),
            &ns(&["A", "P_Aux"]), None, None, Some(&helpers)).is_ok(), "existing easy policy remains unchanged");
        request.is_pv = true;
        assert!(local_helper_difficulty_nodes(&request, &ns(&["P_Aux"]), &ns(&["P_Aux"])).is_none());
        request.is_pv = false; request.task_execution.as_mut().unwrap().task_kind = WorkerTaskKind::LeanClosure;
        assert!(local_helper_difficulty_nodes(&request, &ns(&["P_Aux"]), &ns(&["P_Aux"])).is_none());
        request.task_execution.as_mut().unwrap().task_kind = WorkerTaskKind::Ordinary;
        for mode in [WorkerProofDeltaMode::Easy, WorkerProofDeltaMode::Restructure] {
            if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode: current, .. } =
                &mut request.worker_acceptance.validation_execution_plan[0] { *current = mode; }
            assert!(local_helper_difficulty_nodes(&request, &ns(&["P_Aux"]), &ns(&["P_Aux"])).is_none());
        }
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { mode, allow_new_obligations, .. } =
            &mut request.worker_acceptance.validation_execution_plan[0] {
            *mode = WorkerProofDeltaMode::Local; *allow_new_obligations = false;
        }
        assert!(local_helper_difficulty_nodes(&request, &ns(&["P_Aux"]), &ns(&["P_Aux"])).is_none());
    }

    #[test]
    fn staged_difficulty_checked_payload_preserves_fresh_normalizer_and_sibling_metadata() {
        let (dir, mut request) = pv_fixture();
        request.is_pv = false; request.node_role.clear(); request.configured_challenge_targets.clear();
        request.current_challenge_claims.clear();
        request.worker_acceptance.authorized_nodes = ns(&["A"]);
        request.worker_acceptance.validation_execution_plan = vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some("A".into()), mode: WorkerProofDeltaMode::Restructure,
            authorized_nodes: ns(&["A"]), protected_semantic_change_nodes: ns(&[]),
            allow_new_obligations: true, must_close_active: true }];
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let context: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        let mut raw = raw(); raw["difficulty_updates"] = json!({"A":"hard"});
        let (payload, _, input) = checked_payload(dir.path(), &context, &raw).unwrap();
        let acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context, &payload,
            input.validation_execution_plan.clone(), skipped_validation_results(&input.validation_execution_plan), ns(&[])).unwrap();
        let output = accept_worker_response(&acceptance).unwrap();
        assert_eq!(output.response.difficulty_updates.get(&NodeId::from("A")), Some(&Update::Set(NodeDifficulty::Hard)));
        // Publication applies the returned delta to current metadata, rather
        // than restoring the preparation snapshot and overwriting a sibling.
        let mut fresh = private(); fresh.proof_nodes = fresh.live.present_nodes.clone();
        fresh.node_difficulty.insert("B".into(), NodeDifficulty::Easy);
        fresh.easy_attempts.insert("B".into(), 7);
        fresh.apply_difficulty_updates(&output.response.difficulty_updates);
        assert_eq!(fresh.node_difficulty[&NodeId::from("A")], NodeDifficulty::Hard);
        assert_eq!(fresh.easy_attempts[&NodeId::from("A")], 0);
        assert_eq!(fresh.node_difficulty[&NodeId::from("B")], NodeDifficulty::Easy);
        assert_eq!(fresh.easy_attempts[&NodeId::from("B")], 7);
        raw["difficulty_updates"]["B"] = json!("hard");
        assert!(checked_payload(dir.path(), &context, &raw).is_err());
    }

    fn local_metadata_fixture(mode: WorkerProofDeltaMode)
        -> (tempfile::TempDir, ProtocolState, PreparedWorkerGateOutput) {
        let (dir, _) = pv_fixture();
        let mut state = private();
        state.phase = Phase::ProofFormalization;
        state.stage = trellis_kernel::Stage::Worker;
        state.active_node = Some("A".into());
        state.proof_edit_mode = match mode {
            WorkerProofDeltaMode::Local => trellis_kernel::ProofEditMode::Local,
            WorkerProofDeltaMode::Restructure => trellis_kernel::ProofEditMode::Restructure,
            _ => unreachable!(),
        };
        state.configured_targets = BTreeSet::from([trellis_kernel::TargetId::from("t.a")]);
        state.target_claims = BTreeMap::from([("A".into(), state.configured_targets.clone())]);
        state.deps = graph().edges;
        state.live.present_nodes.insert("Assumptions".into());
        state.proof_nodes = ns(&["A", "B", "H", "Join", "C"]);
        state.node_kinds = state.proof_nodes.iter().cloned().map(|node| (node, NodeKind::Proof)).collect();
        state.node_kinds.insert("Assumptions".into(), NodeKind::Definition);
        state.worker_execution = state.in_flight_request.as_deref().unwrap().task_execution.clone();
        state.in_flight_request = None;
        state.pending_task = Some(trellis_kernel::PendingTask {
            node: Some("A".into()),
            mode: match mode {
                WorkerProofDeltaMode::Local => trellis_kernel::TaskMode::Local,
                WorkerProofDeltaMode::Restructure => trellis_kernel::TaskMode::Restructure,
                _ => unreachable!(),
            },
            authorized_nodes: ns(&["A"]), allow_new_obligations: true,
            must_close_active: true, ..Default::default()
        });
        state.node_difficulty.insert("B".into(), NodeDifficulty::Easy);
        state.easy_attempts.insert("B".into(), 7);
        // Build a valid ordinary baseline, including the committed rollback
        // tier, before deriving its request. Do not convert a PV request by
        // changing selected fields independently of its owning state.
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        state.commit_live();
        let request = state.expected_request(17, RequestKind::Worker);
        state.in_flight_request = Some(Box::new(request.clone()));
        state.validate().expect("valid local metadata fixture baseline");
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let context = serde_json::from_value(prepared["context"].clone()).unwrap();
        (dir, state, context)
    }

    fn add_local_metadata_helper(repo: &Path) -> Value {
        fs::write(repo.join("Tablet/P_Aux.lean"),
            "import Tablet.H\n\n-- [TABLET NODE: P_Aux]\ntheorem P_Aux : True := by\n-- BODY\n  trivial\n").unwrap();
        fs::write(repo.join("Tablet/P_Aux.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n").unwrap();
        let owner = repo.join("Tablet/A.lean");
        let old = fs::read_to_string(&owner).unwrap();
        fs::write(owner, format!("import Tablet.P_Aux\n{old}")).unwrap();
        let mut payload = raw();
        payload["target_claim_updates"] = json!({"P_Aux":[]});
        payload["difficulty_updates"] = json!({"P_Aux":"hard"});
        payload
    }

    #[test]
    fn staged_local_metadata_complete_legacy_response_and_transition_parity() {
        for mode in [WorkerProofDeltaMode::Local, WorkerProofDeltaMode::Restructure] {
            let (dir, state, context) = local_metadata_fixture(mode);
            let raw = add_local_metadata_helper(dir.path());
            let original_state = state.clone();
            let context_bytes = serde_json::to_vec(&context).unwrap();
            let source_before = snapshot_tablet_dir(dir.path());
            // The legacy payload gate and the staged gate see the exact same
            // request, source snapshot and explicit validation evidence. This
            // tests eligibility/normalization/transition, not proof replay.
            let allowed = worker_allowed_outcomes_for_validation(&serde_json::to_value(&context).unwrap()).unwrap();
            let legacy = validate_worker_task_payload(&raw, &allowed, false);
            assert!(legacy.ok, "{:?}", legacy.errors);
            let legacy_data = legacy.data.unwrap();
            let legacy_payload: CheckedWorkerPayload = serde_json::from_value(legacy_data.clone()).unwrap();
            let legacy_input = refresh_validation_input(dir.path(), &context, &legacy_payload).unwrap();
            let (staged_payload, staged_data, staged_input) = checked_payload(dir.path(), &context, &raw).unwrap();
            assert_eq!(staged_data, legacy_data);
            assert_eq!(serde_json::to_value(&staged_input).unwrap(), serde_json::to_value(&legacy_input).unwrap());
            let evidence = skipped_validation_results(&legacy_input.validation_execution_plan);
            let legacy_acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context,
                &legacy_payload, legacy_input.validation_execution_plan.clone(), evidence.clone(), ns(&[])).unwrap();
            let staged_acceptance = worker_acceptance_input_from_checked_payload(dir.path(), &context,
                &staged_payload, staged_input.validation_execution_plan.clone(), evidence, ns(&[])).unwrap();
            assert_eq!(legacy_acceptance, staged_acceptance);
            let legacy_output = accept_worker_response(&legacy_acceptance).unwrap();
            let staged_output = accept_worker_response(&staged_acceptance).unwrap();
            assert_eq!(legacy_output, staged_output);
            assert!(legacy_output.ok, "{:?}", legacy_output.errors);
            assert_eq!(legacy_output.final_outcome, WorkerOutcome::Valid);
            let legacy_transition = trellis_kernel::apply_event(state.clone(), trellis_kernel::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(legacy_output.response) }).unwrap();
            let staged_transition = trellis_kernel::apply_event(state.clone(), trellis_kernel::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(staged_output.response) }).unwrap();
            assert_eq!(legacy_transition, staged_transition);
            assert!(staged_transition.state.live.present_nodes.contains(&NodeId::from("P_Aux")));
            assert_eq!(staged_transition.state.node_difficulty[&NodeId::from("P_Aux")], NodeDifficulty::Hard);
            assert_eq!(staged_transition.state.node_difficulty[&NodeId::from("B")], NodeDifficulty::Easy);
            assert_eq!(staged_transition.state.easy_attempts[&NodeId::from("B")], 7);
            assert_eq!(state, original_state);
            assert_eq!(serde_json::to_vec(&context).unwrap(), context_bytes);
            assert_eq!(snapshot_tablet_dir(dir.path()), source_before);
        }
    }

    #[test]
    fn staged_local_metadata_fresh_gate_refuses_removed_or_existing_helper_without_mutation() {
        let (dir, state, context) = local_metadata_fixture(WorkerProofDeltaMode::Local);
        let raw = add_local_metadata_helper(dir.path());
        let (_, _, input) = checked_payload(dir.path(), &context, &raw).unwrap();
        let edit = EditScope { nodes: ns(&["A"]), helper_prefixes: BTreeSet::from(["P_".into()]), ..Default::default() };
        let reservation = plan(dir.path(), &state, &graph(), &input.before_snapshot, &edit, &raw).unwrap();
        assert_eq!(reservation["write_nodes"], json!(["A", "P_Aux"]));
        let saved_state = state.clone(); let saved_context = serde_json::to_vec(&context).unwrap();
        for change in ["existing-hard", "unknown", "deviation"] {
            let mut changed = raw.clone();
            match change {
                "existing-hard" => changed["difficulty_updates"]["A"] = json!("hard"),
                "unknown" => changed["difficulty_updates"]["P_Aux"] = json!("unknown"),
                _ => changed["node_deviation_claims"] = json!({"P_Aux":["dev:x"]}),
            }
            assert!(checked_payload(dir.path(), &context, &changed).is_err(), "{change}");
            assert_eq!(state, saved_state); assert_eq!(serde_json::to_vec(&context).unwrap(), saved_context);
        }
        fs::remove_file(dir.path().join("Tablet/P_Aux.lean")).unwrap();
        fs::remove_file(dir.path().join("Tablet/P_Aux.tex")).unwrap();
        assert!(checked_payload(dir.path(), &context, &raw).is_err());
        assert_eq!(state, saved_state); assert_eq!(serde_json::to_vec(&context).unwrap(), saved_context);
        assert_eq!(reservation["write_nodes"], json!(["A", "P_Aux"]));

        // The same helper prefix is insufficient: it must be absent in the
        // captured baseline and present in the fresh observation set.
        let (dir, _, context) = local_metadata_fixture(WorkerProofDeltaMode::Local);
        let raw = add_local_metadata_helper(dir.path());
        let mut request: WrapperRequest = serde_json::from_value(context.request.clone()).unwrap();
        request.current_present_nodes.insert("P_Aux".into());
        let prepared = prepare_context(dir.path(), &request, None, None, None, None, None, false).unwrap();
        let existing: PreparedWorkerGateOutput = serde_json::from_value(prepared["context"].clone()).unwrap();
        fs::write(dir.path().join("Tablet/P_Aux.lean"),
            "import Tablet.H\ntheorem P_Aux : True :=\n-- BODY\nby exact True.intro\n").unwrap();
        let saved = serde_json::to_vec(&existing).unwrap();
        assert!(checked_payload(dir.path(), &existing, &raw).is_err());
        assert_eq!(serde_json::to_vec(&existing).unwrap(), saved);
    }

    #[test]
    fn staged_local_metadata_facts_bind_payload_and_both_helper_and_read_sources() {
        for changed_path in ["P_Aux", "H"] {
            let (dir, _, context) = local_metadata_fixture(WorkerProofDeltaMode::Local);
            let raw = add_local_metadata_helper(dir.path());
            let (_, _, input) = checked_payload(dir.path(), &context, &raw).unwrap();
            let output = negative_facts(&input, &raw, &graph(), None, vec!["explicit invalid proof evidence".into()]).unwrap();
            let facts: OwnedProofFacts = serde_json::from_value(output["facts"].clone()).unwrap();
            let saved = serde_json::to_vec(&facts).unwrap();
            assert!(replay_current(&input, &raw, &facts, None, None).is_ok());
            assert!(!finalize_owned_step(&input, &raw, &facts, None, None).unwrap().step_results[0].ok);
            let mut changed = raw.clone(); changed["difficulty_updates"]["P_Aux"] = json!("easy");
            assert!(replay_current(&input, &changed, &facts, None, None).is_err());
            fs::write(dir.path().join(format!("Tablet/{changed_path}.lean")), "changed after owned proof observation\n").unwrap();
            assert!(replay_current(&input, &raw, &facts, None, None).is_err());
            assert_eq!(serde_json::to_vec(&facts).unwrap(), saved);
        }
    }

    #[test]
    fn staged_difficulty_restart_facts_bind_exact_labels_and_reject_changed_sources() {
        let dir = repo(); let mut input = mode_scope_input(dir.path(), WorkerProofDeltaMode::Restructure);
        input.authorized_nodes = ns(&["A"]);
        let mut payload = raw(); payload["difficulty_updates"] = json!({"A":"hard"});
        let output = negative_facts(&input, &payload, &graph(), None, vec!["real invalid owner".into()]).unwrap();
        let facts: OwnedProofFacts = serde_json::from_slice(&serde_json::to_vec(&output["facts"]).unwrap()).unwrap();
        assert!(replay_current(&input, &payload, &facts, None, None).is_ok());
        let mut changed = payload.clone(); changed["difficulty_updates"]["A"] = json!("easy");
        assert!(replay_current(&input, &changed, &facts, None, None).is_err());
        // A failed observation remains Invalid after restart, never promoted
        // by an otherwise supported scheduling-metadata update.
        assert!(!finalize_owned_step(&input, &payload, &facts, None, None).unwrap().step_results[0].ok);
        fs::write(dir.path().join("Tablet/H.lean"), "changed imported source").unwrap();
        assert!(replay_current(&input, &payload, &facts, None, None).is_err());
    }

    #[test]
    fn staged_plan_deleted_owner_keeps_old_read_reservation_and_requires_pair() {
        let dir = repo(); let before = snapshot_tablet_dir(dir.path());
        let mut state = private();
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { authorized_nodes, .. } =
            &mut state.in_flight_request.as_deref_mut().unwrap().worker_acceptance.validation_execution_plan[0] {
            *authorized_nodes = ns(&["A", "B"]);
        }
        let edit = EditScope { nodes: ns(&["A","B"]), ..Default::default() };
        fs::remove_file(dir.path().join("Tablet/B.lean")).unwrap();
        let mut raw = raw(); raw["deleted_nodes"] = json!(["B"]);
        assert!(plan(dir.path(), &state, &graph(), &before, &edit, &raw).is_err());
        fs::remove_file(dir.path().join("Tablet/B.tex")).unwrap();
        let planned = plan(dir.path(), &state, &graph(), &before, &edit, &raw).unwrap();
        assert_eq!(planned["write_nodes"], json!(["A","B"])); assert_eq!(planned["read_nodes"], json!(["A","B","H"]));
        // Final pure legality still rejects the unchanged Join's dangling B
        // import. Admission is not acceptance and cannot invent that verdict.
    }

    #[test]
    fn staged_plan_rejects_mode_scope_errors_before_owned_work() {
        for mode in [WorkerProofDeltaMode::Easy, WorkerProofDeltaMode::Local] {
            let dir = repo(); let input = mode_scope_input(dir.path(), mode);
            let mut state = private();
            let request = state.in_flight_request.as_deref_mut().unwrap();
            request.worker_acceptance.validation_execution_plan = input.validation_execution_plan.clone();
            request.worker_acceptance.authorized_nodes = ns(&["A", "B"]);
            let edit = EditScope { nodes: ns(&["A", "B"]), ..Default::default() };
            fs::write(dir.path().join("Tablet/B.tex"), "out-of-mode statement edit").unwrap();
            assert!(plan(dir.path(), &state, &graph(), &input.before_snapshot, &edit, &raw()).is_err());
            fs::write(dir.path().join("Tablet/B.tex"), "statement").unwrap();
            fs::remove_file(dir.path().join("Tablet/B.lean")).unwrap();
            fs::remove_file(dir.path().join("Tablet/B.tex")).unwrap();
            let mut deleted = raw(); deleted["deleted_nodes"] = json!(["B"]);
            assert!(plan(dir.path(), &state, &graph(), &input.before_snapshot, &edit, &deleted).is_err());
        }
        // Allowed shape with a broken proof is still admitted: native owned
        // execution must produce honest negative facts, not preflight Cold.
        let dir = repo(); let input = mode_scope_input(dir.path(), WorkerProofDeltaMode::Local);
        let mut state = private();
        state.in_flight_request.as_deref_mut().unwrap().worker_acceptance.validation_execution_plan = input.validation_execution_plan.clone();
        fs::write(dir.path().join("Tablet/A.lean"), "import Tablet.H\ntheorem A : True :=\n-- BODY\nby exact missingProof\n").unwrap();
        let edit = EditScope { nodes: ns(&["A"]), ..Default::default() };
        assert!(plan(dir.path(), &state, &graph(), &input.before_snapshot, &edit, &raw()).is_ok());
    }

    #[test]
    fn staged_metadata_and_fallback_import_guards_are_pure() {
        let dir = repo(); let source = dir.path().join("Tablet/A.lean");
        fs::write(&source, "theorem A : True :=\n-- BODY\nby trivial\n").unwrap();
        let before = fs::read(&source).unwrap();
        assert!(unchanged_import_normalization(dir.path(), &ns(&["A"])).is_err());
        assert_eq!(fs::read(&source).unwrap(), before);
        let mut payload = CheckedWorkerPayload::default(); payload.target_claim_updates.insert("P_Aux".into(), BTreeSet::new());
        payload.difficulty_updates.insert("P_Aux".into(), "easy".into());
        assert!(supported_payload(&payload, &ns(&["P_Aux"])).is_ok());
        assert!(supported_payload(&CheckedWorkerPayload::default(), &ns(&["P_Aux"])).is_ok());
        payload.semantic_dep_updates.insert("A".into(), BTreeSet::from(["B".into()]));
        assert!(supported_payload(&payload, &ns(&["P_Aux"])).is_err());
    }

    #[test]
    fn staged_replay_source_binds_full_module_not_just_principal_statement() {
        let dir = repo(); let lean = fs::read_to_string(dir.path().join("Tablet/A.lean")).unwrap();
        let command = || runtime_cli_observations::ExternalCommandObservation { returncode: Some(0), stdout: String::new(),
            stderr: String::new(), timed_out: false, spawn_error: String::new(), cost: None };
        let mut observations = OwnerObservations::default();
        observations.nodes.insert("A".into(), NodeObservation { node:"A".into(), lean_path:"/old/A.lean".into(), tex_path:"/old/A.tex".into(),
            lean_exists:true, tex_exists:true, lean_content:lean.clone(), tex_content:"statement".into(), compile:command(), print_axioms:command() });
        let replay = Replay(&observations, std::cell::Cell::new(false));
        assert_eq!(replay.node(dir.path(), "A").unwrap().lean_path, dir.path().join("Tablet/A.lean").display().to_string());
        fs::write(dir.path().join("Tablet/A.lean"), format!("{lean}\ndef secondary : Nat := 7\n")).unwrap();
        assert!(replay.node(dir.path(), "A").is_err());
    }

    #[test]
    fn staged_replay_does_not_relax_current_premise_owners() {
        let mut observations = OwnerObservations::default();
        observations.probes.insert("A".into(), LocalClosureProbeOutput::default());
        assert!(Replay(&observations, std::cell::Cell::new(false)).closure(Path::new("/unused"), "A", &ns(&[])).is_ok());
        assert!(Replay(&observations, std::cell::Cell::new(false)).closure(Path::new("/unused"), "A", &ns(&["Assumptions"])).is_err());
    }
    #[test]
    fn staged_negative_facts_need_no_successful_artifact_and_remain_failed() {
        let dir = repo();
        let input = ExecuteWorkerValidationPlanInput {
            repo_path: dir.path().to_path_buf(), before_snapshot: snapshot_tablet_dir(dir.path()),
            current_present_nodes: ns(&["H", "A", "B", "Join", "C"]), active_node: Some("A".into()),
            authorized_nodes: ns(&["A"]),
            validation_execution_plan: private().in_flight_request.as_deref().unwrap().worker_acceptance.validation_execution_plan.clone(),
            ..Default::default()
        };
        let produced = negative_facts(&input, &raw(), &graph(), None, vec!["genuine compile/type error".into()]).unwrap();
        let mut facts: OwnedProofFacts = serde_json::from_value(produced["facts"].clone()).unwrap();
        assert!(!facts.owner_ok); assert!(facts.observation_environment.is_none());
        let result = finalize_owned_step(&input, &raw(), &facts, None, None).unwrap();
        assert!(!result.step_results[0].ok); assert_eq!(result.step_results[0].errors, ["genuine compile/type error"]);
        let mut changed_plan = input.validation_execution_plan.clone();
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { must_close_active, .. } = &mut changed_plan[0] {
            *must_close_active = false;
        }
        let mut encoded_input = serde_json::to_value(&input).unwrap();
        // Repository location is intentionally omitted from typed semantic
        // hashes but remains required for executable input deserialization.
        encoded_input["repo_path"] = json!(dir.path());
        let mut changed_input: ExecuteWorkerValidationPlanInput = serde_json::from_value(encoded_input).unwrap();
        changed_input.validation_execution_plan = changed_plan;
        assert!(finalize_owned_step(&changed_input, &raw(), &facts, None, None).is_err());
        facts.owner_ok = true;
        assert!(finalize_owned_step(&input, &raw(), &facts, None, None).is_err());
    }

    #[test]
    fn staged_environment_binds_imported_artifact_and_resolution_absence() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap(); let imported = dir.path().join("H.olean");
        let shadow = dir.path().join("earlier-H.olean"); fs::write(&imported, "secondary def value v1").unwrap();
        let environment = RefreshEnvironment { version:1, environment_sha256s: BTreeMap::from([("A".into(), "a".repeat(64))]),
            artifact_files: BTreeMap::from([(imported.clone(), Some(format!("{:x}", Sha256::digest(fs::read(&imported).unwrap())))), (shadow.clone(), None)]) };
        assert!(environment.current(&ns(&["A"])));
        fs::write(&imported, "secondary def value v2").unwrap(); assert!(!environment.current(&ns(&["A"])));
        fs::write(&imported, "secondary def value v1").unwrap(); fs::write(&shadow, "new earlier resolver candidate").unwrap();
        assert!(!environment.current(&ns(&["A"])));
    }

    #[test]
    fn reviewed_b7_raw_facts_still_require_current_source_and_import_environment() {
        use sha2::{Digest, Sha256};
        let dir = repo();
        let mut input = mode_scope_input(dir.path(), WorkerProofDeltaMode::Local);
        fs::write(dir.path().join("Tablet/A.lean"),
            "import Tablet.H\ntheorem A : True :=\n-- BODY\nby trivial\n").unwrap();
        // This unit exercises only the binding gate, not proof acceptance.
        let output = negative_facts(&input, &raw(), &graph(), None, vec!["fixture".into()]).unwrap();
        let mut facts: OwnedProofFacts = serde_json::from_value(output["facts"].clone()).unwrap();
        let policy: Value = serde_json::from_str(include_str!(
            "../../trellis/dispatch/compatible_staged_producers.json")).unwrap();
        facts.acceptance_logic_identity = policy["positive_math_owned"][0].as_str().unwrap().into();
        assert!(!compatible_owned_producer(&facts));
        facts.owner_ok = true;
        assert!(compatible_owned_producer(&facts));
        let artifact = dir.path().join(".lake/build/fixture.olean");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "exact imported artifact").unwrap();
        let environment = RefreshEnvironment { version: 1,
            environment_sha256s: BTreeMap::from([("A".into(), "a".repeat(64))]),
            artifact_files: BTreeMap::from([(artifact.clone(), Some(format!("{:x}",
                Sha256::digest(fs::read(&artifact).unwrap()))))]) };
        facts.observation_environment = Some(environment.clone());
        assert!(replay_current(&input, &raw(), &facts, Some(&environment), None).is_ok());
        assert!(replay_current(&input, &raw(), &facts, None, None).is_err());
        input.node_role.insert("H".into(), trellis_kernel::PvRole::ExtractionModel);
        assert!(replay_current(&input, &raw(), &facts, Some(&environment), None).is_err());
        input.node_role.clear();
        fs::write(&artifact, "changed imported artifact").unwrap();
        assert!(replay_current(&input, &raw(), &facts, Some(&environment), None).is_err());
        fs::write(&artifact, "exact imported artifact").unwrap();
        fs::write(dir.path().join("Tablet/H.lean"), "changed source").unwrap();
        assert!(replay_current(&input, &raw(), &facts, Some(&environment), None).is_err());
        facts.acceptance_logic_identity = "unreviewed".into();
        assert!(!compatible_owned_producer(&facts));
    }

    #[test]
    fn staged_downstream_compile_failure_retains_honest_native_invalid() {
        let mut output = WorkerAcceptanceOutput { response: WorkerResponse { outcome: WorkerOutcome::Valid,
                ..Default::default() }, contract_errors: vec![], validation_errors: vec![],
            final_outcome: WorkerOutcome::Valid, ok: true, errors: vec![] };
        record_final_observation_failure(&mut output, "Join: changed imported secondary definition does not typecheck".into());
        let checked = result_output(output, raw(), vec![WorkerValidationStepResult { ok: true, ..Default::default() }]).unwrap();
        assert!(!checked.ok); assert_eq!(checked.final_outcome, "invalid");
        let response: WorkerResponse = serde_json::from_value(checked.response.as_ref().unwrap().clone()).unwrap();
        assert!(!response.acceptance_check_passed);
        assert!(checked.validation_errors[0].contains("secondary definition"));
        assert_eq!(checked.response.as_ref().unwrap()["deterministic_rejection_reasons"], json!(checked.validation_errors));
    }

    #[test]
    fn staged_certificate_policy_reuses_recorded_scan_without_checker_call() {
        let mut observations = OwnerObservations::default();
        let original = LocalClosureProbeOutput { status: "ok".into(), ..Default::default() };
        observations.policies.insert("A".into(), original.clone());
        let replay = Replay(&observations, std::cell::Cell::new(false));
        let reused = replay.certificate_policy(Path::new("/unused"), "A", |_, _| panic!("recorded scan must not invoke checker")).unwrap();
        assert_eq!(reused, original);
        let calls = std::cell::Cell::new(0);
        let missing = replay.certificate_policy(Path::new("/unused"), "TexOnly", |_, node| {
            assert_eq!(node, "TexOnly"); calls.set(calls.get()+1); Ok(original.clone())
        }).unwrap();
        assert_eq!(calls.get(),1); assert_eq!(missing,original);
    }

    #[test]
    fn staged_real_guard_failure_is_rejection_but_missing_donor_fact_is_cold() {
        let observations = OwnerObservations::default();
        let replay = Replay(&observations, std::cell::Cell::new(false));
        let step = observed_step_result(Err("current protected Root failed elaboration".into()), &replay, &ns(&["A"])).unwrap();
        assert!(!step.ok); assert!(step.errors[0].contains("Root"));
        let missing = replay.closure(Path::new("/unused"), "A", &ns(&[]));
        assert!(missing.is_err()); assert!(replay.binding_failed());
        assert!(observed_step_result(Err(missing.unwrap_err()), &replay, &ns(&["A"])).is_err());
    }

}
