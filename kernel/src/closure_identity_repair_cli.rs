// Explicit offline planning and candidate repair; never use the mutating
// SupervisorRuntime loader for diagnosis or before successful evidence staging.

fn repair_input_generation(repo: &Path, owners: &BTreeSet<NodeId>) -> Result<String, String> {
    let mut paths: BTreeSet<PathBuf> = [
        "lean-toolchain",
        "lake-manifest.json",
        "trellis.config.json",
        "APPROVED_AXIOMS.json",
        "PROPOSED_ASSUMPTIONS.json",
        "tcb_manifest.json",
        "isabelle/ROOT",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    for node in owners {
        for ext in ["lean", "thy", "tex"] {
            paths.insert(PathBuf::from(format!("Tablet/{node}.{ext}")));
        }
    }
    let mut values = BTreeMap::new();
    for path in paths {
        let value = match fs::read(repo.join(&path)) {
            Ok(bytes) => Some(hash_bytes(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "repair input {}: {error}",
                    repo.join(&path).display()
                ))
            }
        };
        values.insert(path, value);
    }
    Ok(hash_bytes(
        &serde_json::to_vec(&values).map_err(|e| e.to_string())?,
    ))
}

fn plan_closure_identity_file(
    state_path: &Path,
    repo: Option<&Path>,
    identity: Option<Identity>,
) -> Result<serde_json::Value, String> {
    let state: ProtocolState =
        serde_json::from_slice(&fs::read(state_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let root = state_path.parent().ok_or("state file has no parent")?;
    if let Some(identity) = identity {
        if !identity.unavailable(true).is_empty() {
            return Err("IdentityUnavailable: incomplete supplied audit identity".into());
        }
        if identity.checker_script_hash != hash_text(trellis_kernel::closure_identity::COLLECTOR)
            || identity.closure_version != CLOSURE_VERSION
        {
            return Err(
                "audit identity must select this binary's embedded collector and closure version"
                    .into(),
            );
        }
        let mut owners = Vec::new();
        let mut counts = BTreeMap::new();
        for (tier, snapshot, kinds, proofs, records) in [
            (
                "live",
                &state.live,
                &state.node_kinds,
                &state.proof_nodes,
                &state.local_closure_records,
            ),
            (
                "committed",
                &state.committed,
                &state.committed_node_kinds,
                &state.committed_proof_nodes,
                &state.committed_local_closure_records,
            ),
            (
                "last_clean",
                &state.last_clean_live,
                &state.last_clean_node_kinds,
                &state.last_clean_proof_nodes,
                &state.last_clean_local_closure_records,
            ),
        ] {
            if tier == "last_clean" && !state.last_clean_local_closure_mirror_ready {
                continue;
            }
            let mut eligible = 0;
            for node in &snapshot.present_nodes {
                if state.trust_base.seed_support_definitions.contains_key(node)
                    || matches!(
                        state.node_role.get(node),
                        Some(trellis_kernel::PvRole::UnderModelAssumptions)
                    )
                    || !(proofs.contains(node)
                        || matches!(
                            kinds.get(node),
                            Some(NodeKind::Proof | NodeKind::Definition | NodeKind::Preamble)
                        ))
                {
                    continue;
                }
                eligible += 1;
                let mut r = OwnerCurrency {
                    tier: tier.into(),
                    node: node.clone(),
                    findings: vec![],
                    source_checked: false,
                    artifacts_checked: false,
                };
                match records.get(node) {
                    None => r.add(
                        Currency::Missing,
                        "record",
                        "absent",
                        "eligible owner record",
                    ),
                    Some(record) => {
                        r.findings.extend(identity.compare(record, true));
                        if let Some(cert) = &record.node_certificate {
                            if !cert.roots_are_current()
                                || cert.source_sha256.to_string() != record.active_decl_hash
                            {
                                r.add(
                                    Currency::CorruptEvidence,
                                    "node_certificate",
                                    "invalid retained binding",
                                    "self-consistent certificate",
                                );
                            }
                        }
                    }
                }
                if !r.current() {
                    owners.push(r);
                }
            }
            counts.insert(tier, eligible);
        }
        let root_consistency = state.validate_local_closure_root_consistency();
        return Ok(
            serde_json::json!({ "status": "closure_identity_plan", "plan": repair_report(owners, &state, root),
            "eligible": counts, "identity": identity, "source_checked": false, "policy_checked": false,
            "artifacts_checked": false, "root_consistency": root_consistency }),
        );
    }
    let repo = repo.ok_or("plan requires repo_path or a captured identity (offline audit only)")?;
    Ok(
        serde_json::json!({ "status": "closure_identity_plan", "plan": plan_runtime_identity(&state, repo, root), "artifacts_checked": false }),
    )
}

fn migrate_local_closure_identity_with_probe<F, S>(
    root: PathBuf,
    confirm_runtime_stopped: bool,
    parallelism: Option<usize>,
    probe: F,
    observe: S,
) -> Result<RuntimeCliResponse, String>
where
    F: Fn(&Path, &str, Duration) -> Result<LocalClosureProbeOutput, String> + Sync,
    S: Fn(&Path, &ProtocolState, &BTreeSet<NodeId>) -> Result<BTreeMap<NodeId, String>, String>,
{
    if !confirm_runtime_stopped {
        return Err("identity repair requires stopped runtime and source writers".into());
    }
    let paths = RuntimePaths::new(&root);
    if root
        .join("closure-identity-publication/commit.json")
        .exists()
    {
        return Err("unfinished identity publication: resume/load once to recover before planning another repair".into());
    }
    let original = fs::read(&paths.state_path).map_err(|e| e.to_string())?;
    let state: ProtocolState = serde_json::from_slice(&original).map_err(|e| e.to_string())?;
    let metadata_bytes = fs::read(&paths.metadata_path).map_err(|e| e.to_string())?;
    let metadata: RuntimeMetadata =
        serde_json::from_slice(&metadata_bytes).map_err(|e| e.to_string())?;
    let repo = metadata
        .repo_path
        .as_deref()
        .ok_or("runtime metadata has no repository path")?;
    let context = ClosureContext::capture(repo);
    let plan = plan_runtime_identity(&state, repo, &root);
    // Always print the complete direct/expanded plan before any mutation.
    eprintln!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"status": "closure_identity_plan", "plan": plan})
        )
        .unwrap()
    );
    context.require_available()?;
    let eligible: BTreeSet<_> = state
        .live
        .present_nodes
        .iter()
        .filter(|n| state.local_closure_owner_eligible(n))
        .cloned()
        .collect();
    if plan.direct_stale.is_empty() {
        return Ok(RuntimeCliResponse::MigrateLocalClosureRecordsOk {
            total_eligible: eligible.len(),
            already_current: eligible.len(),
            minted: 0,
        });
    }
    let _ownership = trellis_kernel::closure_identity_transaction::RepairOwnership::acquire(&root)?;
    if fs::read(&paths.state_path).map_err(|e| e.to_string())? != original {
        return Err("state changed between planning and acquiring repair ownership".into());
    }
    let input_generation = repair_input_generation(repo, &state.live.present_nodes)?;
    // A historical tier requiring a different owner source needs its own
    // evidence transaction. Do not adopt dirty Worker bytes as a new baseline.
    for node in &eligible {
        let r = classify_closure_record(
            state.local_closure_records.get(node),
            node,
            repo,
            &state,
            &context,
            false,
        );
        if r.findings.iter().any(|f| {
            matches!(
                f.category,
                Currency::SourceStale | Currency::CorruptEvidence
            )
        }) {
            return Err(format!(
                "identity-only repair blocked by source/evidence pairing: {}",
                serde_json::to_string(&r).unwrap()
            ));
        }
    }
    let scheduled: BTreeSet<_> = plan
        .expanded_stale
        .intersection(&eligible)
        .cloned()
        .collect();
    if scheduled != plan.expanded_stale {
        return Err("identity repair needs a historical source view; no state written".into());
    }
    // Re-observe the semantic projection before issuing under a new identity.
    // A changed projection needs the ordinary correspondence transition.
    let semantic = observe(repo, &state, &scheduled)?;
    for node in &scheduled {
        let old = state
            .live
            .corr_current_fingerprints
            .get(node)
            .map(String::as_str)
            .unwrap_or("");
        let new = semantic
            .get(node)
            .ok_or_else(|| format!("semantic re-observation missing for {node}"))?;
        if corr_fingerprint_mismatched(old, new) {
            return Err(format!(
                "identity-only repair requires semantic revalidation for {node}; no state written"
            ));
        }
    }
    let parallelism = offline_migration_parallelism(parallelism)?;
    let batch = parallel_revalidate_exact_set_with_probe(
        &state,
        repo,
        &scheduled,
        state.cycle as u64,
        parallelism,
        &probe,
    )?;
    let refreshed: BTreeSet<_> = batch.refreshed.iter().map(|(n, _)| n.clone()).collect();
    if refreshed != scheduled {
        return Err("identity repair probe set incomplete; original records retained".into());
    }
    let mut candidate = state.clone();
    trellis_kernel::engine::apply_revalidation_batch(&mut candidate, batch);
    // New evidence may expose additional edges. Do not publish an unplanned
    // cascade: the existing union-graph walk tells the operator what is left.
    let additional =
        ScheduledLocalClosureIssuer::reverse_root_delta_consumers(&state, &candidate, &refreshed);
    if !additional.is_empty() {
        return Err(format!(
            "identity repair expansion changed: {additional:?}; re-plan required, no state written"
        ));
    }
    ScheduledLocalClosureIssuer::mirror_current_records_into_matching_snapshots(
        &mut candidate,
        &refreshed,
    );
    trellis_kernel::model::recompute_local_closure_reverse_indices(&mut candidate);
    let after = ClosureContext::capture(repo);
    if after.identity != context.identity || !after.unavailable.is_empty() {
        return Err("identity changed during repair; original records retained".into());
    }
    for node in &scheduled {
        let record = &candidate.local_closure_records[node];
        if !classify_closure_record(Some(record), node, repo, &candidate, &after, true).current() {
            return Err(format!(
                "candidate admission failed for {node}; original records retained"
            ));
        }
        // This operation does not update artifact epochs. Existing bundles
        // must remain exactly paired with both old and newly issued records.
        if let Some(old) = state
            .local_closure_records
            .get(node)
            .and_then(|r| r.node_certificate.as_ref())
        {
            if record.node_certificate.as_ref().map(|c| &c.artifact_bundle)
                != Some(&old.artifact_bundle)
            {
                return Err(format!(
                    "artifact epoch changed for {node}; identity-only repair cannot publish it"
                ));
            }
        }
    }
    let remaining = plan_runtime_identity(&candidate, repo, &root);
    if !remaining.direct_stale.is_empty() {
        return Err(RuntimeError::ClosureIdentityRepairRequired(remaining).to_string());
    }
    candidate.validate_local_closure_root_consistency()?;
    if repair_input_generation(repo, &state.live.present_nodes)? != input_generation
        || fs::read(&paths.metadata_path).map_err(|e| e.to_string())? != metadata_bytes
    {
        return Err(
            "source/policy/metadata generation changed during repair; original records retained"
                .into(),
        );
    }
    let records = refreshed
        .iter()
        .map(|n| candidate.local_closure_records[n].clone())
        .collect();
    trellis_kernel::closure_identity_transaction::publish(&root, &original, &candidate, records)?;
    Ok(RuntimeCliResponse::MigrateLocalClosureRecordsOk {
        total_eligible: eligible.len(),
        already_current: eligible.len() - scheduled.len(),
        minted: scheduled.len(),
    })
}

fn migrate_local_closure_identity(
    root: PathBuf,
    stopped: bool,
    parallelism: Option<usize>,
) -> Result<RuntimeCliResponse, String> {
    // This CLI action runs before any request threads are started. The flag
    // follows every child request and makes the checker refuse materialization.
    std::env::set_var("TRELLIS_CLOSURE_IDENTITY_ONLY", "1");
    migrate_local_closure_identity_with_probe(
        root,
        stopped,
        parallelism,
        run_local_closure_axioms_with_timeout,
        |repo, state, nodes| {
            observe_correspondence_fingerprints_with_under_model_assumptions(
                repo,
                nodes,
                &runtime_cli_observations::under_model_assumption_nodes_from_state(state),
            )
        },
    )
}
