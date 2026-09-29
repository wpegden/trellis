// Included by runtime_cli so maintenance shares the production admission and
// dependency-frontier issuers. No ordinary runtime load precedes publication.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedRebindView {
    repo_path: PathBuf,
    runtime_root: PathBuf,
    paper_path: PathBuf,
    checker_socket: PathBuf,
    source_commit: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedRebindViews {
    version: u32,
    views: BTreeMap<String, TrustedRebindView>,
}

fn trusted_rebind_git_head(repo: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("cannot resolve pinned maintenance source view".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn trusted_rebind_inputs(
    root: &Path,
    config: &TrustedRebindViews,
    metadata: &RuntimeMetadata,
) -> Result<String, String> {
    let mut hashes = BTreeMap::new();
    for name in [
        "protocol_state.json",
        "runtime_metadata.json",
        "checkpoint.json",
        "trusted-rebind-views.json",
    ] {
        let path = root.join(name);
        hashes.insert(
            path.clone(),
            hash_bytes(&fs::read(&path).map_err(|e| e.to_string())?),
        );
    }
    for path in trellis_kernel::runtime::event_log_cycle_files(
        &trellis_kernel::runtime::event_log_dir_for(root, metadata),
    )
    .map_err(|e| e.to_string())?
    {
        hashes.insert(
            path.clone(),
            hash_bytes(&fs::read(path).map_err(|e| e.to_string())?),
        );
    }
    for view in config.views.values() {
        let listed = Command::new("git")
            .arg("-C")
            .arg(&view.repo_path)
            .args(["ls-files", "-z"])
            .output()
            .map_err(|e| e.to_string())?;
        if !listed.status.success() {
            return Err("cannot freeze tracked source inputs".into());
        }
        for name in listed.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let name = std::str::from_utf8(name).map_err(|e| e.to_string())?;
            let path = view.repo_path.join(name);
            hashes.insert(
                path.clone(),
                hash_bytes(&fs::read(path).map_err(|e| e.to_string())?),
            );
        }
        hashes.insert(
            view.paper_path.clone(),
            hash_bytes(&fs::read(&view.paper_path).map_err(|e| e.to_string())?),
        );
    }
    Ok(trellis_kernel::trusted_artifact_rebind::digest(
        &serde_json::to_value(hashes).map_err(|e| e.to_string())?,
    ))
}

fn migrate_trusted_artifacts(
    root: PathBuf,
    stopped: bool,
    parallelism: Option<usize>,
) -> Result<RuntimeCliResponse, String> {
    use trellis_kernel::{
        trusted_artifact_rebind as rebind, trusted_rebind_transaction as transaction,
    };
    if !stopped {
        return Err(
            "trusted artifact rebind requires stopped supervisor and source writers".into(),
        );
    }
    transaction::recover(&root)?;
    let _ownership = trellis_kernel::closure_identity_transaction::RepairOwnership::acquire(&root)?;
    if transaction::completed(&root) {
        // A job retry must never rewrite a committed migration or a destination
        // which may already be running. Readiness is a separate job phase.
        return Ok(RuntimeCliResponse::MigrateLocalClosureRecordsOk {
            total_eligible: 0,
            already_current: 0,
            minted: 0,
        });
    }
    let original = fs::read(root.join("protocol_state.json")).map_err(|e| e.to_string())?;
    let mut before: ProtocolState = serde_json::from_slice(&original).map_err(|e| e.to_string())?;
    trellis_kernel::model::recompute_local_closure_reverse_indices(&mut before);
    rebind::check_scope(&before)?;
    let metadata: RuntimeMetadata = serde_json::from_slice(
        &fs::read(root.join("runtime_metadata.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let repo = metadata
        .repo_path
        .as_deref()
        .ok_or("missing destination repository")?;
    let head = trusted_rebind_git_head(repo)?;
    let clean = if before.last_clean_local_closure_mirror_ready {
        Some(
            trellis_kernel::runtime::resolve_last_clean_source(
                repo,
                before.last_clean_commit.as_deref(),
            )
            .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let config: TrustedRebindViews = serde_json::from_slice(
        &fs::read(root.join("trusted-rebind-views.json"))
            .map_err(|e| format!("missing prepared historical source views: {e}"))?,
    )
    .map_err(|e| e.to_string())?;
    if config.version != 1 {
        return Err("unsupported rebind source-view schema".into());
    }
    let frozen_inputs = trusted_rebind_inputs(&root, &config, &metadata)?;
    let mut candidate = before.clone();
    let history: serde_json::Value = serde_json::from_slice(
        &fs::read(repo.join(".trellis-history/supervisor_state.json"))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let history = trellis_kernel::shared_state_codec::decode_shared_state(history)
        .map_err(|e| e.to_string())?;
    for reference in candidate.configured_reference_papers.values_mut() {
        let path = Path::new(&reference.tex_path);
        if path.is_absolute() {
            let old_repo = history["metadata"]["repo_path"]
                .as_str()
                .ok_or("missing source root for reference path rebinding")?;
            reference.tex_path = path
                .strip_prefix(old_repo)
                .map_err(|_| {
                    "reference paper lies outside the source repository; track it before resuming"
                })?
                .to_string_lossy()
                .to_string();
        }
        if Path::new(&reference.tex_path)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("reference paper path must remain inside the source repository".into());
        }
    }
    let mut total = 0;
    let mut minted = 0;
    let parallelism = offline_migration_parallelism(parallelism)?;
    let mut generations = BTreeMap::new();
    for tier in ["live", "committed", "last_clean"] {
        if tier == "last_clean" && clean.is_none() {
            continue;
        }
        let view = config
            .views
            .get(tier)
            .ok_or_else(|| format!("missing source-paired {tier} view"))?;
        let expected = if tier == "last_clean" {
            clean.as_deref().unwrap()
        } else {
            &head
        };
        let expected = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", &format!("{expected}^{{commit}}")])
            .output()
            .map_err(|e| e.to_string())?;
        if !expected.status.success()
            || view.source_commit != String::from_utf8_lossy(&expected.stdout).trim()
            || trusted_rebind_git_head(&view.repo_path)? != view.source_commit
        {
            return Err(format!(
                "{tier} source view is not paired with the selected checkpoint"
            ));
        }
        let mut state = before.clone();
        state.configured_reference_papers = candidate.configured_reference_papers.clone();
        if tier == "committed" {
            state.restore_committed();
        }
        if tier == "last_clean" && !state.apply_last_clean_reset()? {
            return Err("LastClean activation refused".into());
        }
        let source_generation =
            repair_input_generation(&view.repo_path, &state.live.present_nodes)?;
        generations.insert(tier, source_generation.clone());
        for (node, record) in &state.local_closure_records {
            if hash_file_or_empty(&view.repo_path.join(format!("Tablet/{node}.lean")))
                != record.active_decl_hash
            {
                return Err(format!(
                    "{tier} source does not match checkpoint owner {node}"
                ));
            }
        }
        // This CLI is a one-shot maintenance process. Its scoped parallel probe
        // workers have joined before moving to the next checker/source view.
        std::env::set_var("TRELLIS_CHECKER_SOCKET", &view.checker_socket);
        std::env::set_var("TRELLIS_KERNEL_CACHE_ROOT", &view.runtime_root);
        let token = std::env::var(format!("TRELLIS_REBIND_TOKEN_{}", tier.to_uppercase()))
            .map_err(|_| format!("missing maintenance capability for {tier}"))?;
        std::env::set_var("TRELLIS_CHECKER_TOKEN", token);
        std::env::remove_var("TRELLIS_CLOSURE_IDENTITY_ONLY");
        ClosureContext::capture(&view.repo_path).require_available()?;
        let observed = trellis_kernel::runtime::observe_trusted_rebind_view(
            &view.repo_path,
            &state,
            &view.paper_path,
        )
        .map_err(|e| e.to_string())?;
        rebind::rebind_observed_snapshot(&mut state, observed)?;
        let eligible: BTreeSet<_> = state
            .live
            .present_nodes
            .iter()
            .filter(|n| state.local_closure_owner_eligible(n))
            .cloned()
            .collect();
        total += eligible.len();
        let records_dir = root.join("trusted-rebind-progress").join(tier);
        let mut dependency_records = state.local_closure_records.clone();
        state.local_closure_records.clear();
        let mut remaining = eligible.clone();
        while !remaining.is_empty() {
            let frontier = certificate_ready_frontier(
                &remaining,
                &BTreeMap::new(),
                &view.repo_path,
                &dependency_records,
            );
            if frontier.is_empty() {
                return Err(format!(
                    "{tier} certificate graph has no dependency-ready frontier: {remaining:?}"
                ));
            }
            let mut to_probe = Vec::new();
            for node in frontier {
                let path =
                    records_dir.join(trellis_kernel::runtime::persisted_record_file_name(&node));
                if let Ok(record) = load_persisted_record(&path) {
                    if record_hashes_match_current(&record, &view.repo_path, &state) {
                        dependency_records.insert(node.clone(), record.clone());
                        state.local_closure_records.insert(node.clone(), record);
                        remaining.remove(&node);
                        continue;
                    }
                }
                to_probe.push(node);
            }
            let frontier_state = state.clone();
            parallel_probe_frontier_with_probe(
                &frontier_state,
                &view.repo_path,
                to_probe,
                before.cycle as u64,
                Duration::from_secs(24 * 60 * 60),
                parallelism,
                &run_local_closure_axioms_with_timeout,
                |node, outcome| {
                    if outcome.budget_exhausted
                        || !outcome.batch.still_unverified.is_empty()
                        || outcome.batch.refreshed.len() != 1
                    {
                        return Err(format!(
                            "{tier}/{node} deterministic closure probe failed: {:?}",
                            outcome.batch.still_unverified
                        ));
                    }
                    let record = outcome.batch.refreshed[0].1.clone();
                    if record.node != node {
                        return Err("closure probe owner mismatch".into());
                    }
                    state
                        .local_closure_records
                        .insert(node.clone(), record.clone());
                    if !record_hashes_match_current(&record, &view.repo_path, &state) {
                        return Err(format!("{tier}/{node} failed full certificate admission"));
                    }
                    persist_record_to_disk(&records_dir, &record, before.cycle as u64)?;
                    dependency_records.insert(node.clone(), record);
                    remaining.remove(&node);
                    minted += 1;
                    eprintln!(
                        "[trusted rebind] {tier}: {}/{} owners admitted ({node})",
                        eligible.len() - remaining.len(),
                        eligible.len()
                    );
                    Ok(())
                },
            )?;
        }
        trellis_kernel::model::recompute_local_closure_reverse_indices(&mut state);
        ScheduledLocalClosureIssuer::require_current_coverage(
            &state,
            &view.repo_path,
            "trusted_artifact_rebind",
        )
        .map_err(|e| e.to_string())?;
        if repair_input_generation(&view.repo_path, &state.live.present_nodes)? != source_generation
        {
            return Err(format!("{tier} source/policy changed during rebuild"));
        }
        trellis_kernel::runtime::retain_rollback_artifact_epoch(&root, &view.repo_path, &state)
            .map_err(|e| e.to_string())?;
        match tier {
            "live" => {
                candidate.live = state.live;
                candidate.pending_task = state.pending_task;
                candidate.corr_approved_fingerprints = state.corr_approved_fingerprints;
                candidate.local_closure_records = state.local_closure_records;
            }
            "committed" => {
                candidate.committed = state.live;
                candidate.committed_local_closure_records = state.local_closure_records;
            }
            "last_clean" => {
                candidate.last_clean_live = state.live;
                candidate.last_clean_corr_approved_fingerprints = state.corr_approved_fingerprints;
                candidate.last_clean_local_closure_records = state.local_closure_records;
            }
            _ => unreachable!(),
        }
    }
    trellis_kernel::model::recompute_local_closure_reverse_indices(&mut candidate);
    candidate.validate()?;
    let generation = rebind::digest(
        &serde_json::json!({"state": hash_bytes(&original), "sources": generations, "head": head}),
    );
    let payload = rebind::TrustedArtifactRebind::between(&before, &candidate, generation)?.compact()?;
    if trusted_rebind_inputs(&root, &config, &metadata)? != frozen_inputs {
        return Err(
            "trusted rebuild source, paper, metadata or event prefix changed; nothing published"
                .into(),
        );
    }
    transaction::publish(&root, &original, &candidate, payload)?;
    Ok(RuntimeCliResponse::MigrateLocalClosureRecordsOk {
        total_eligible: total,
        already_current: total - minted,
        minted,
    })
}

fn plan_trusted_artifact_rebind(root: &Path) -> Result<serde_json::Value, String> {
    let state: ProtocolState = serde_json::from_slice(
        &fs::read(root.join("protocol_state.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    trellis_kernel::trusted_artifact_rebind::check_scope(&state)?;
    let metadata: RuntimeMetadata = serde_json::from_slice(
        &fs::read(root.join("runtime_metadata.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let repo = metadata
        .repo_path
        .as_deref()
        .ok_or("missing repository path")?;
    let head = trusted_rebind_git_head(repo)?;
    let mut views = BTreeMap::new();
    for tier in ["live", "committed", "last_clean"] {
        if tier == "last_clean" && !state.last_clean_local_closure_mirror_ready {
            continue;
        }
        let mut paired = state.clone();
        if tier == "committed" {
            paired.restore_committed();
        }
        if tier == "last_clean" {
            paired.apply_last_clean_reset()?;
        }
        let source = if tier == "last_clean" {
            let reference = trellis_kernel::runtime::resolve_last_clean_source(
                repo,
                state.last_clean_commit.as_deref(),
            )
            .map_err(|e| e.to_string())?;
            let output = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["rev-parse", &format!("{reference}^{{commit}}")])
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err("LastClean source commit unavailable".into());
            }
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            head.clone()
        };
        let owners: BTreeSet<_> = paired
            .live
            .present_nodes
            .iter()
            .filter(|node| paired.local_closure_owner_eligible(node))
            .cloned()
            .collect();
        views.insert(
            tier,
            serde_json::json!({"source_commit": source, "eligible_owners": owners}),
        );
    }
    Ok(
        serde_json::json!({"version": 1, "views": views, "cycle": state.cycle, "stage": state.stage, "phase": state.phase, "artifacts_checked": false}),
    )
}

fn check_trusted_artifact_rebind(root: &Path) -> Result<serde_json::Value, String> {
    let state: ProtocolState = serde_json::from_slice(
        &fs::read(root.join("protocol_state.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    trellis_kernel::trusted_artifact_rebind::check_scope(&state)?;
    state.validate_local_closure_root_consistency()?;
    let views: TrustedRebindViews = serde_json::from_slice(
        &fs::read(root.join("trusted-rebind-views.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if views.version != 1 {
        return Err("unsupported rebind source-view schema".into());
    }
    let plan = plan_trusted_artifact_rebind(root)?;
    let mut coverage = BTreeMap::new();
    for tier in ["live", "committed", "last_clean"] {
        if tier == "last_clean" && !state.last_clean_local_closure_mirror_ready {
            continue;
        }
        let view = views
            .views
            .get(tier)
            .ok_or_else(|| format!("readiness missing {tier} view"))?;
        if trusted_rebind_git_head(&view.repo_path)? != view.source_commit
            || plan["views"][tier]["source_commit"].as_str() != Some(&view.source_commit)
        {
            return Err(format!("{tier} source view changed after rebuild"));
        }
        let mut paired = state.clone();
        if tier == "committed" {
            paired.restore_committed();
        }
        if tier == "last_clean" {
            paired.apply_last_clean_reset()?;
        }
        std::env::set_var("TRELLIS_CHECKER_SOCKET", &view.checker_socket);
        std::env::set_var("TRELLIS_KERNEL_CACHE_ROOT", &view.runtime_root);
        std::env::set_var(
            "TRELLIS_CHECKER_TOKEN",
            std::env::var(format!("TRELLIS_REBIND_TOKEN_{}", tier.to_uppercase()))
                .map_err(|e| e.to_string())?,
        );
        ClosureContext::capture(&view.repo_path).require_available()?;
        let observed = trellis_kernel::runtime::observe_trusted_rebind_view(
            &view.repo_path,
            &paired,
            &view.paper_path,
        )
        .map_err(|e| e.to_string())?;
        if observed != paired.live {
            return Err(format!(
                "{tier} disk fingerprints diverged after trusted rebind"
            ));
        }
        ScheduledLocalClosureIssuer::require_current_coverage(
            &paired,
            &view.repo_path,
            "remote_resume_readiness",
        )
        .map_err(|e| e.to_string())?;
        trellis_kernel::runtime::validate_rollback_artifact_epoch(root, &paired)
            .map_err(|e| e.to_string())?;
        coverage.insert(
            tier,
            paired
                .live
                .present_nodes
                .iter()
                .filter(|n| paired.local_closure_owner_eligible(n))
                .count(),
        );
    }
    Ok(
        serde_json::json!({"ready":true, "artifacts_checked":true, "coverage": coverage, "provider_requests":0}),
    )
}

#[cfg(test)]
mod trusted_rebind_research_audit {
    use super::*;
    #[test]
    #[ignore = "read-only audit requires an explicitly copied research fixture"]
    fn cold_research_coverage_and_production_artifact_admission() {
        let root = PathBuf::from(
            std::env::var("TRELLIS_RESUME_RESEARCH_COPY").expect("set the copied runtime root"),
        );
        let state: ProtocolState =
            serde_json::from_slice(&fs::read(root.join("protocol_state.json")).unwrap()).unwrap();
        let metadata: RuntimeMetadata =
            serde_json::from_slice(&fs::read(root.join("runtime_metadata.json")).unwrap()).unwrap();
        let repo = metadata.repo_path.unwrap();
        let plan = plan_trusted_artifact_rebind(&root).unwrap();
        let mut result = BTreeMap::new();
        for tier in ["live", "committed", "last_clean"] {
            let mut paired = state.clone();
            if tier == "committed" {
                paired.restore_committed();
            }
            if tier == "last_clean" {
                assert!(paired.apply_last_clean_reset().unwrap());
            }
            assert_eq!(plan["views"][tier]["source_commit"].as_str().unwrap(), trusted_rebind_git_head(&repo).unwrap(), "this audit requires identical source commits; materialize separate views otherwise");
            let owners: Vec<_> = paired
                .live
                .present_nodes
                .iter()
                .filter(|n| paired.local_closure_owner_eligible(n))
                .collect();
            assert!(!owners.is_empty());
            let mut rejected = 0;
            let mut missing_parts = 0;
            for node in &owners {
                let record = paired
                    .local_closure_records
                    .get(*node)
                    .expect("complete recorded owner coverage");
                assert_eq!(
                    active_decl_hash_for_node(&repo, node.as_str()),
                    record.active_decl_hash
                );
                if !node_certificate_matches_current(record, &paired, &repo) {
                    rejected += 1;
                }
                for part in &record.node_certificate.as_ref().unwrap().artifact_bundle {
                    let suffix = match part.level.as_str() {
                        "exported" => "olean",
                        "server" => "olean.server",
                        "private" => "olean.private",
                        _ => panic!("unknown artifact part"),
                    };
                    if !repo
                        .join(format!(".lake/build/lib/lean/Tablet/{node}.{suffix}"))
                        .is_file()
                    {
                        missing_parts += 1;
                    }
                }
            }
            assert_eq!(
                rejected,
                owners.len(),
                "cold records must not pass production artifact admission"
            );
            assert!(missing_parts >= owners.len());
            result.insert(tier, serde_json::json!({"eligible_owners": owners.len(), "source_paired_records": owners.len(), "artifact_admission_rejected": rejected, "missing_artifact_parts": missing_parts}));
        }
        fs::write(
            root.parent().unwrap().join("admission-audit.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        eprintln!("{}", serde_json::to_string(&result).unwrap());
    }
}
