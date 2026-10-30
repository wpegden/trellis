//! Ordinary target-scoped Modeling observations. These are raw checker facts,
//! never a WorkerResponse; current global goal/trust validation stays final.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelingFacts {
    pub write_nodes: BTreeSet<NodeId>,
    pub effect_targets: BTreeSet<TargetId>,
}

pub(super) fn shape(plan: &[WorkerValidationExecutionPlanStep]) -> bool {
    matches!(plan, [WorkerValidationExecutionPlanStep::TheoremTargetEditScope { .. },
        WorkerValidationExecutionPlanStep::ScopedTablet {
            allowed_nodes_mode: trellis_kernel::ScopedTabletAllowedNodesMode::PreviousOrExplicit, .. }])
}

fn selection(repo: &Path, before: &BTreeMap<String, String>, active: Option<&NodeId>,
    deleted: &BTreeSet<NodeId>, reserved: &BTreeSet<NodeId>, plan: &[WorkerValidationExecutionPlanStep],
) -> Result<(WorkerValidationStepResult, BTreeSet<NodeId>), String> {
    let [WorkerValidationExecutionPlanStep::TheoremTargetEditScope { target, initial_scope },
        WorkerValidationExecutionPlanStep::ScopedTablet {
            allowed_nodes_mode: trellis_kernel::ScopedTabletAllowedNodesMode::PreviousOrExplicit,
            explicit_nodes }] = plan else { return Err("unsupported staged Modeling plan".into()); };
    let target = target.as_ref().or(active).ok_or("Modeling scope anchor missing")?;
    let mut first = runtime_cli_observations::theorem_target_edit_scope_step_result_with_assumption_authoring(
        repo, target.as_str(), before, initial_scope, None, deleted);
    first.allowed_nodes.retain(|node| !reserved.contains(node));
    let allowed = if first.allowed_nodes.is_empty() { explicit_nodes } else { &first.allowed_nodes };
    Ok((first.clone(), allowed.difference(reserved).cloned().collect()))
}

pub(super) fn scope(repo: &Path, before: &BTreeMap<String, String>, present: &BTreeSet<NodeId>,
    active: Option<&NodeId>, deleted: &BTreeSet<NodeId>, reserved: &BTreeSet<NodeId>,
    plan: &[WorkerValidationExecutionPlanStep],
) -> Result<runtime_cli_observations::ProofWorkerDeltaReceiptScope, String> {
    let (_, allowed) = selection(repo, before, active, deleted, reserved, plan)?;
    let after = snapshot_tablet_dir(repo);
    let names: BTreeSet<NodeId> = after.keys().filter_map(|name| name.strip_suffix(".lean"))
        .filter(|name| !matches!(*name, "Preamble" | "Axioms" | "Assumptions" | "header"))
        .map(NodeId::from).filter(|node| !reserved.contains(node)).collect();
    let new_nodes: BTreeSet<NodeId> = names.difference(present).cloned().collect();
    let mut final_present_nodes = present.clone();
    final_present_nodes.extend(new_nodes.iter().cloned());
    final_present_nodes.retain(|node| !deleted.contains(node));
    let mut observation_nodes: BTreeSet<_> = allowed.difference(deleted).cloned().collect();
    // ScopedTablet always observes Preamble, even after deleting every owner.
    // Empty materialization means all-present in the checker, so never pass it.
    observation_nodes.insert("Preamble".into());
    Ok(runtime_cli_observations::ProofWorkerDeltaReceiptScope { new_nodes,
        deleted_nodes: deleted.clone(), observation_nodes, final_present_nodes })
}

pub(super) fn execute(input: &ExecuteWorkerValidationPlanInput, observations: &dyn ProofDeltaObservations)
    -> Result<ExecutedWorkerValidationPlanOutput, String> {
    let (first, allowed) = selection(&input.repo_path, &input.before_snapshot, input.active_node.as_ref(),
        &input.declared_deleted_nodes, &input.reserved_non_node_stems, &input.validation_execution_plan)?;
    let effective = allowed.difference(&input.declared_deleted_nodes).cloned().collect();
    let observed = observations.tablet(&input.repo_path, &effective, &input.node_role);
    let result = observed.and_then(|observation|
        runtime_cli_observations::scoped_tablet_step_result_from_observation(&input.repo_path,
            &input.baseline_errors, &allowed, &input.before_snapshot, &input.node_role,
            input.assumption_authoring_node.as_ref(), &input.declared_deleted_nodes, &observation));
    let second = match result {
        Ok(step) => step,
        Err(error) if observations.binding_failed() => return Err(error),
        Err(error) => WorkerValidationStepResult { kind: "scoped_tablet".into(), ok: false,
            detail: error.clone(), errors: vec![error], allowed_nodes: allowed, ..Default::default() },
    };
    Ok(ExecutedWorkerValidationPlanOutput { step_results: vec![first, second],
        protected_semantic_change_nodes: BTreeSet::new() })
}

pub(super) fn changed_nodes(repo: &Path, before: &BTreeMap<String, String>) -> Result<BTreeSet<NodeId>, String> {
    let after = snapshot_tablet_dir(repo);
    let mut nodes = BTreeSet::new();
    for name in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        if before.get(name) == after.get(name) || matches!(name.as_str(), "INDEX.md" | "README.md") { continue; }
        let stem = name.strip_suffix(".lean").or_else(|| name.strip_suffix(".tex"))
            .ok_or("Modeling changes shared source")?;
        if matches!(stem, "Preamble" | "Axioms" | "Assumptions" | "header") { return Err("Modeling changes shared source".into()); }
        nodes.insert(stem.into());
    }
    Ok(nodes)
}

pub(super) fn facts(input: &ExecuteWorkerValidationPlanInput, raw: &Value) -> Result<ModelingFacts, String> {
    let mut write_nodes = changed_nodes(&input.repo_path, &input.before_snapshot)?;
    let claims: BTreeMap<NodeId, BTreeSet<TargetId>> = serde_json::from_value(
        raw.get("target_claim_updates").cloned().unwrap_or_else(|| json!({}))).map_err(|error| error.to_string())?;
    write_nodes.extend(claims.keys().cloned());
    let difficulty: BTreeMap<NodeId, String> = serde_json::from_value(
        raw.get("difficulty_updates").cloned().unwrap_or_else(|| json!({}))).map_err(|error| error.to_string())?;
    write_nodes.extend(difficulty.keys().cloned());
    Ok(ModelingFacts { write_nodes, effect_targets: claims.values().flatten().cloned().collect() })
}

/// Reserve complete source import reads before compilation. Existing attested
/// edges remain conservative reads; canonical new header edges are added.
pub(super) fn reserve_imports(repo: &Path, graph: &mut trellis_kernel::dispatch::DependencyGraph,
    roots: &BTreeSet<NodeId>, deleted: &BTreeSet<NodeId>,
) -> Result<BTreeSet<NodeId>, String> {
    let mut reads = BTreeSet::new();
    let mut pending: Vec<_> = roots.iter().cloned().collect();
    while let Some(node) = pending.pop() {
        if !reads.insert(node.clone()) { continue; }
        if !deleted.contains(&node) {
            let source = fs::read_to_string(repo.join("Tablet").join(format!("{node}.lean")))
                .map_err(|error| format!("staged Modeling read source {node}: {error}"))?;
            let imports = trellis_kernel::cache_key::canonical_header_tablet_imports(&source)
                .ok_or_else(|| format!("staged Modeling requires canonical single-module imports: {node}"))?;
            let imports: BTreeSet<NodeId> = imports.into_iter().map(NodeId::from).collect();
            if imports.iter().any(|dep| !graph.edges.contains_key(dep)) {
                return Err(format!("unregistered staged Modeling import in {node}"));
            }
            graph.edges.entry(node.clone()).or_default().extend(imports);
        }
        pending.extend(graph.edges.get(&node).ok_or("staged Modeling graph row missing")?.iter().cloned());
    }
    Ok(reads)
}
