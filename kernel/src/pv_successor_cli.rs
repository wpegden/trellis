// Included by runtime_cli. The operator's frozen predecessor is evidence,
// never an agent-supplied status map. Ordinary resume imports no judgments.

fn successor_read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn successor_same_file(before: &Path, after: &Path) -> bool {
    match (fs::read(before), fs::read(after)) {
        (Ok(a), Ok(b)) => a == b,
        (Err(a), Err(b)) => a.kind() == io::ErrorKind::NotFound && b.kind() == io::ErrorKind::NotFound,
        _ => false,
    }
}

fn successor_dependency_closed(
    mut nodes: BTreeSet<NodeId>, deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    loop {
        let previous = nodes.clone();
        nodes.retain(|node| deps.get(node).is_none_or(|ds| ds.is_subset(&previous)));
        if previous == nodes { return nodes; }
    }
}

fn successor_aligned<'a>(current: &'a BTreeMap<NodeId, String>,
    approved: &'a BTreeMap<NodeId, String>, node: &NodeId) -> Option<&'a String> {
    current.get(node).filter(|fp| !fp.is_empty() && approved.get(node) == Some(*fp))
}

// An approval is a judgment about the fingerprint that was actually reviewed,
// not a certificate for whatever the successor happens to contain. Retaining
// that baseline is safe before a checker exists: without a current fingerprint
// the ordinary current_corr_state predicate still returns Unknown. Only a later
// authoritative observation with the SAME fingerprint can revive the judgment.
fn successor_correspondence_baseline(
    old: &ProtocolState, node: &NodeId, invalidated: &BTreeSet<NodeId>,
) -> Result<String, String> {
    if old.corr_fingerprint_schema_version
        != runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION {
        return Err("predecessor correspondence fingerprint schema is not current".into());
    }
    if invalidated.contains(node) {
        return Err("explicit revision correspondence audit invalidation".into());
    }
    if old.corr_status.get(node) != Some(&trellis_kernel::CorrStatus::Pass) {
        return Err("predecessor correspondence verdict was not Pass".into());
    }
    let fp = successor_aligned(&old.live.corr_current_fingerprints,
        &old.corr_approved_fingerprints, node)
        .ok_or("predecessor correspondence approval was empty or not current")?;
    let meaning = runtime_cli_observations::CorrespondenceFingerprint::from_storage_string(fp)
        .filter(|meaning| !meaning.own_tex.is_empty() && !meaning.lean_semantic_closure.is_empty())
        .ok_or("predecessor correspondence semantic fingerprint is unavailable")?;
    // This is the authenticated predecessor observer's TYPE surface for a
    // theorem and BODY surface for a definition, not the textual import graph.
    // An audit can revoke a judgment even when executable text is unchanged.
    let invalid_meanings: BTreeSet<_> = meaning.lean_relevant_dependencies.iter()
        .chain(meaning.lean_relevant_definition_descendants.keys())
        .map(|name| NodeId::from(name.as_str()))
        .filter(|dependency| invalidated.contains(dependency)).collect();
    if !invalid_meanings.is_empty() {
        return Err(format!("explicit revision correspondence audit invalidated meaning dependencies: {invalid_meanings:?}"));
    }
    Ok(fp.clone())
}

fn successor_install_correspondence_baseline(
    state: &mut ProtocolState, old: &ProtocolState, node: &NodeId,
    invalidated: &BTreeSet<NodeId>, exact_input_closure: bool,
) -> Result<(), String> {
    if state.corr_fingerprint_schema_version != 0
        && state.corr_fingerprint_schema_version
            != runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION {
        return Err("successor correspondence fingerprint schema is incompatible".into());
    }
    let fp = successor_correspondence_baseline(old, node, invalidated)?;
    state.corr_status.insert(node.clone(), trellis_kernel::CorrStatus::Pass);
    state.corr_approved_fingerprints.insert(node.clone(), fp.clone());
    if exact_input_closure {
        state.live.corr_current_fingerprints.insert(node.clone(), fp);
    } else {
        // Do not manufacture a current observation from the old approval, even
        // when static comparison suggests only generated comments changed.
        state.live.corr_current_fingerprints.remove(node);
    }
    // These are current-schema baselines, not legacy observations to re-pin
    // during a later schema migration. Never rebaseline against fresh meaning.
    state.corr_fingerprint_schema_version = runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION;
    Ok(())
}

fn successor_copy_artifacts(previous: &Path, repo: &Path, record: &LocalClosureRecord) -> Result<(), String> {
    let evidence = record.node_certificate_evidence.as_ref().ok_or("missing replay evidence")?;
    let supervisor = previous.join(".trellis/supervisor/repo");
    let sources = [&supervisor, previous];
    let destination = repo.join(".lake/build/lib/lean/Tablet");
    let mut parts = Vec::new();
    for part in &evidence.artifact_bundle {
        let suffix = match part.level.as_str() {
            "exported" => ".olean", "server" => ".olean.server", "private" => ".olean.private",
            _ => return Err("unsupported retained artifact level".into()),
        };
        let name = format!("{}{suffix}", record.node);
        let bytes = sources.iter().find_map(|base| {
            let bytes = fs::read(base.join(".lake/build/lib/lean/Tablet").join(&name)).ok()?;
            (bytes.len() as u64 == part.size_bytes
                && trellis_kernel::trust_base::raw_sha256(&bytes) == part.sha256).then_some(bytes)
        }).ok_or_else(|| format!("retained replay artifact unavailable: {name}"))?;
        parts.push((name, bytes));
    }
    fs::create_dir_all(&destination).map_err(|e| e.to_string())?;
    for (name, bytes) in parts {
        fs::write(destination.join(name), bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

struct SuccessorPreservationContext {
    old: ProtocolState,
    previous: PathBuf,
    frozen: PathBuf,
    selected: BTreeSet<NodeId>,
    package: String,
    predecessor_state_sha256: String,
    invalidated: BTreeMap<String, BTreeSet<NodeId>>,
    review: Value,
}

fn successor_preservation_context(repo: &Path) -> Result<SuccessorPreservationContext, String> {
    let plan_path = repo.join("modeling-input/preserved-dag.json");
    let plan = successor_read_json(&plan_path)?;
    let marker = successor_read_json(&repo.join("modeling-input/revision-from.json"))?;
    if plan["schema"] != "trellis-preserved-dag/v1"
        || marker["schema"] != "trellis-pv-revision-successor/v1"
        || plan["package_digest"] != marker["package_digest"] {
        return Err("PV successor preservation receipt mismatch".into());
    }
    let previous = PathBuf::from(marker["predecessor"]["repo"].as_str().ok_or("missing predecessor repo")?);
    let revision = PathBuf::from(marker["revision_root"].as_str().ok_or("missing revision root")?);
    let frozen = revision.join("predecessor");
    let bytes = fs::read(frozen.join("protocol_state.json")).map_err(|e| e.to_string())?;
    if marker["predecessor"]["protocol_state_sha256"].as_str() != Some(hash_bytes(&bytes).as_str()) {
        return Err("frozen predecessor state does not match revision receipt".into());
    }
    let old: ProtocolState = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let manifest = marker["predecessor"]["tablet_manifest"]["files"].as_array().ok_or("missing frozen Tablet manifest")?;
    // Validate the whole frozen source inventory, including inputs of results
    // whose own files survived. No status can be vouched for by a modified old
    // dependency. Paths are relative to the frozen Tablet directory.
    for entry in manifest {
        let name = entry["relative_path"].as_str().ok_or("invalid frozen Tablet path")?;
        let expected = &entry["sha256_of_raw_bytes"];
        let path = Path::new(name);
        if path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err("invalid frozen Tablet path".into());
        }
        let bytes = fs::read(frozen.join("Tablet").join(path)).map_err(|e| e.to_string())?;
        if expected.as_str() != Some(hash_bytes(&bytes).as_str()) {
            return Err(format!("frozen predecessor source changed: {name}"));
        }
    }
    let selected: BTreeSet<NodeId> = serde_json::from_value(plan["authored_nodes"].clone()).map_err(|e| e.to_string())?;
    for node in &selected {
        if !old.live.present_nodes.contains(node)
            || node.as_str().contains(['/', '\\']) || matches!(node.as_str(), "." | "..") {
            return Err(format!("invalid preserved authored node: {node}"));
        }
    }
    let package = marker["package_digest"].as_str().ok_or("missing package digest")?;
    if package.len() != 64 || !package.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid package digest".into());
    }
    let review = successor_read_json(&revision.join("reviews").join(package).join("review.json"))?;
    if review["digest"] != marker["package_digest"] { return Err("review package digest mismatch".into()); }
    let mut invalidated: BTreeMap<String, BTreeSet<NodeId>> = BTreeMap::new();
    for row in review["audit_invalidation"].as_array().ok_or("missing audit invalidations")? {
        let id = row["node"].as_str().ok_or("invalid audit subject")?;
        let mut affected = BTreeSet::from([NodeId::from(id)]);
        affected.extend(old.target_claims.iter().filter(|(_, goals)| goals.iter().any(|g| g.as_str() == id)).map(|(n, _)| n.clone()));
        for lane in row["lanes"].as_array().ok_or("invalid audit lanes")? {
            invalidated.entry(lane.as_str().ok_or("invalid audit lane")?.to_string()).or_default().extend(affected.clone());
        }
    }
    Ok(SuccessorPreservationContext { old, previous, frozen, selected,
        package: package.to_string(), predecessor_state_sha256: hash_bytes(&bytes), invalidated, review })
}

/// Preserve ownership of an unchanged positive requirement, not its proof or
/// review approval. Read identities from the authenticated revision package;
/// the Python source-selection report is never authority for target claims.
fn successor_target_claims(old: &ProtocolState, state: &ProtocolState, node: &NodeId,
    review: &Value, current_goals: &Value) -> Result<BTreeSet<trellis_kernel::TargetId>, String>
{
    let claims = old.target_claims.get(node).cloned().unwrap_or_default();
    if claims.is_empty() { return Ok(claims); }
    if let Some(primary) = old.authored_decide_primary_for_node(node) {
        if old.live_polarity(&primary) != trellis_kernel::ChallengePolarity::Prove
            || old.configured_challenge_targets.get(&primary).is_none_or(|s| s.name != node.as_str()) {
            return Err("predecessor carrier does not state the positive requirement".into());
        }
    } else if old.authored_goals_decidable || state.authored_goals_decidable {
        return Err("predecessor principal statement registration is missing".into());
    }
    let before = review.get("original_collection").cloned()
        .unwrap_or_else(|| serde_json::json!({"context":"", "goals":review["original_goals"]}));
    let after = review.get("proposed_collection").cloned()
        .unwrap_or_else(|| serde_json::json!({"context":"", "goals":review["proposed_goals"]}));
    let context = |v: &Value| v.get("context").and_then(Value::as_str).unwrap_or("").to_string();
    if context(&before) != context(&after) || context(&after) != context(current_goals) {
        return Err("requirement shared context changed".into());
    }
    for goal in &claims {
        if !old.configured_targets.contains(goal) || !state.configured_targets.contains(goal) {
            return Err(format!("requirement {goal} is not configured in both generations"));
        }
        let find = |v: &Value| v["goals"].as_array().and_then(|gs| gs.iter()
            .find(|g| g["id"].as_str() == Some(goal.as_str()))).cloned();
        let original = find(&before);
        if original.is_none() || original != find(&after) || original != find(current_goals) {
            return Err(format!("requirement {goal} changed or was removed"));
        }
        if review["plan"]["dispositions"].as_array().is_some_and(|rows| rows.iter().any(|row|
            row["goal_id"].as_str() == Some(goal.as_str())
                && !matches!(row["remedy"].as_str(), Some("repair" | "model_mismatch")))) {
            return Err(format!("requirement {goal} has a replacement or retirement disposition"));
        }
    }
    Ok(claims)
}

fn successor_environment_equal(old: &ProtocolState, state: &ProtocolState, previous: &Path, repo: &Path) -> bool {
    old.tablet_target == state.tablet_target
        && ((!old.trust_base.required() && !state.trust_base.required())
            || old.trust_base == state.trust_base)
        && ["lean-toolchain", "lake-manifest.json", "lakefile.lean", "lakefile.toml"]
            .iter().all(|p| successor_same_file(&previous.join(p), &repo.join(p)))
}

#[derive(Debug, Serialize)]
struct SuccessorCorrespondenceRestoration {
    package_digest: String,
    predecessor_state_sha256: String,
    baselines: BTreeMap<NodeId, String>,
    exclusions: BTreeMap<NodeId, String>,
}

// Read-only derivation for an explicit, logged native maintenance command.
// The caller must establish a quiescent transaction boundary and derive the
// touched set from authoritative post-genesis history, not merely today's
// absence of a verdict. A reset followed by deletion must remain a revocation.
// This helper neither edits state nor launches the checker.
fn derive_pv_successor_correspondence_baselines(
    state: &ProtocolState, repo: &Path, correspondence_touched_since_genesis: &BTreeSet<NodeId>,
) -> Result<SuccessorCorrespondenceRestoration, String> {
    let context = successor_preservation_context(repo)?;
    let mut result = SuccessorCorrespondenceRestoration {
        package_digest: context.package.clone(),
        predecessor_state_sha256: context.predecessor_state_sha256.clone(),
        baselines: BTreeMap::new(), exclusions: BTreeMap::new(),
    };
    let environment_equal = successor_environment_equal(&context.old, state, &context.previous, repo);
    let invalidated = context.invalidated.get("correspondence").cloned().unwrap_or_default();
    for node in &context.selected {
        let reason = if !environment_equal {
            Some("backend, trust base or library environment changed".to_string())
        } else if state.corr_fingerprint_schema_version != 0
            && state.corr_fingerprint_schema_version != runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION {
            Some("successor correspondence fingerprint schema is incompatible".to_string())
        } else if correspondence_touched_since_genesis.contains(node) {
            Some("post-genesis correspondence judgment or node identity was touched; preserve newer history".to_string())
        } else if state.corr_status.contains_key(node) || state.corr_approved_fingerprints.contains_key(node) {
            Some("successor already has a correspondence judgment or approved baseline".to_string())
        } else if !state.live.present_nodes.contains(node)
            || state.node_kinds.get(node) != context.old.node_kinds.get(node) {
            Some("successor node is absent or changed kind".to_string())
        } else if !["lean", "tex"].iter().all(|ext| {
            let relative = format!("Tablet/{node}.{ext}");
            repo.join(&relative).is_file()
                && successor_same_file(&context.frozen.join(&relative), &repo.join(&relative))
        }) {
            Some("successor authored Lean or prose differs from the frozen predecessor".to_string())
        } else { None };
        if let Some(reason) = reason {
            result.exclusions.insert(node.clone(), reason);
        } else {
            match successor_correspondence_baseline(&context.old, node, &invalidated) {
                Ok(fp) => { result.baselines.insert(node.clone(), fp); }
                Err(reason) => { result.exclusions.insert(node.clone(), reason); }
            }
        }
    }
    Ok(result)
}

fn preserve_pv_successor_state(state: &mut ProtocolState, repo: &Path, root: &Path) -> Result<(), String> {
    if !repo.join("modeling-input/preserved-dag.json").exists() { return Ok(()); }
    if state.cycle != 0 || state.phase != trellis_kernel::Phase::Modeling || !state.live.modeling_draft {
        return Err("PV predecessor preservation requires fresh Modeling genesis".into());
    }
    let SuccessorPreservationContext { old, previous, frozen, selected, package,
        predecessor_state_sha256, invalidated, review } = successor_preservation_context(repo)?;
    let current_goals = successor_read_json(&repo.join("modeling-input/goals.json"))?;
    let mut preserved_claims = BTreeMap::new();
    let mut claims_not_reused = BTreeMap::new();
    for node in &selected {
        if state.live.present_nodes.contains(node) {
            return Err(format!("preserved authored node already exists: {node}"));
        }
        for ext in ["lean", "tex"] {
            let relative = format!("Tablet/{node}.{ext}");
            if !repo.join(&relative).is_file() || !successor_same_file(&frozen.join(&relative), &repo.join(&relative)) {
                return Err(format!("preserved authored source differs: {relative}"));
            }
        }
        let kind = *old.node_kinds.get(node).ok_or("missing predecessor node kind")?;
        state.live.present_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), kind);
        state.deps.insert(node.clone(), old.deps.get(node).cloned().unwrap_or_default());
        let claims = match successor_target_claims(&old, state, node, &review, &current_goals) {
            Ok(claims) => claims,
            Err(reason) => { claims_not_reused.insert(node.clone(), reason); BTreeSet::new() }
        };
        if !claims.is_empty() { preserved_claims.insert(node.clone(), claims.clone()); }
        state.target_claims.insert(node.clone(), claims);
    }
    // Fresh bootstrap normally seeds Preamble before support imports exist.
    // Preserve the recorded graph for every byte-identical Lean source, not
    // just authored additions, so missing bootstrap edges cannot authorize reuse.
    for node in state.live.present_nodes.intersection(&old.live.present_nodes) {
        let relative = format!("Tablet/{node}.lean");
        if successor_same_file(&frozen.join(&relative), &repo.join(&relative)) {
            if let Some(deps) = old.deps.get(node) {
                state.deps.insert(node.clone(), deps.clone());
            }
        }
    }
    for node in &selected {
        if !state.deps[node].is_subset(&state.live.present_nodes) {
            return Err(format!("preserved node {node} has missing dependencies"));
        }
    }
    state.live.open_nodes = trellis_kernel::open_nodes_from_repo(repo, &state.live.present_nodes);
    state.normalize_all_structural_state();
    state.ensure_node_metadata();

    // Reuse only exact Lean/prose input closures. Comment-only extracted source
    // changes can reuse authoring, but do not silently rewrite certificate hashes.
    let source_equal: BTreeSet<_> = state.live.present_nodes.intersection(&old.live.present_nodes)
        .filter(|node| ["lean", "tex"].iter().all(|ext| {
            let relative = format!("Tablet/{node}.{ext}");
            successor_same_file(&frozen.join(&relative), &repo.join(&relative))
        })).cloned().collect();
    let equal = successor_dependency_closed(source_equal, &state.deps);
    let context = ClosureContext::capture(repo);
    let mut reused_closures = BTreeSet::new();
    let mut skipped: BTreeMap<NodeId, String> = BTreeMap::new();
    // Correspondence observations can be reused without launching the semantic
    // collector only when their entire input closure, backend and libraries are
    // identical. The ordinary closure classifier still checks all certificate,
    // policy and tool identities before a closure status is admitted.
    let environment_equal = successor_environment_equal(&old, state, &previous, repo);
    let mut eligible = if environment_equal { equal } else { BTreeSet::new() };
    // The full trust_base contains transient gate bookkeeping; disabled trust
    // has no seed authority. Its policy is checked by ClosureContext below.
    if !environment_equal {
        for node in &selected { skipped.insert(node.clone(), "backend, trust base or library environment changed".into()); }
    }
    for lane in ["closure", "correspondence"] {
        if let Some(nodes) = invalidated.get(lane) { eligible.retain(|n| !nodes.contains(n)); }
    }
    eligible = successor_dependency_closed(eligible, &state.deps);
    for node in &eligible {
        if let Some(fp) = old.live.corr_current_fingerprints.get(node) {
            state.live.corr_current_fingerprints.insert(node.clone(), fp.clone());
        }
        if let Some(fp) = old.live.target_fingerprints.get(node) {
            state.live.target_fingerprints.insert(node.clone(), fp.clone());
        }
    }
    let mut pending = eligible.clone();
    for node in state.live.present_nodes.intersection(&old.live.present_nodes) {
        if !eligible.contains(node) {
            skipped.entry(node.clone()).or_insert_with(||
                "source, prose, dependency inputs or explicit audit invalidation require checking again".into());
        }
    }
    loop {
        let mut progress = false;
        for node in pending.clone() {
            let Some(record) = old.local_closure_records.get(&node) else { continue; };
            if old.local_closure_unverified_nodes.contains(&node) || old.local_closure_failures.contains_key(&node) {
                skipped.insert(node.clone(), "predecessor closure was not current".into()); continue;
            }
            if let Err(error) = successor_copy_artifacts(&previous, repo, record) {
                skipped.insert(node.clone(), error); continue;
            }
            let currency = classify_closure_record(Some(record), &node, repo, state, &context, true);
            if currency.current() {
                state.local_closure_records.insert(node.clone(), record.clone());
                reused_closures.insert(node.clone()); pending.remove(&node); skipped.remove(&node); progress = true;
            } else {
                let mut reason = serde_json::to_string(&currency.findings).map_err(|e| e.to_string())?;
                let mut diagnostic = record.clone();
                if let Err(error) = issue_node_certificate_for_record(&mut diagnostic, state, repo) {
                    reason.push_str(&format!("; {error}"));
                }
                skipped.insert(node, reason);
            }
        }
        if !progress { break; }
    }
    let mut reused_corr = BTreeSet::new();
    let mut pending_corr_baselines = BTreeSet::new();
    let mut corr_not_reused = BTreeMap::new();
    let corr_invalidated = invalidated.get("correspondence").cloned().unwrap_or_default();
    for node in selected.union(&eligible) {
        let outcome = if environment_equal {
            successor_install_correspondence_baseline(state, &old, node,
                &corr_invalidated, eligible.contains(node))
        } else {
            Err("backend, trust base or library environment changed".into())
        };
        match outcome {
            Ok(()) if eligible.contains(node) => { reused_corr.insert(node.clone()); }
            Ok(()) => { pending_corr_baselines.insert(node.clone()); }
            Err(reason) => { corr_not_reused.insert(node.clone(), reason); }
        }
    }
    // NL lane fingerprints are cheap native reads. Preserve a current old Pass
    // or Fail only on exact fingerprint equality; a changed goal/source basis
    // remains Unknown. This does not rebaseline judgments against a new paper.
    let parts = runtime_cli_observations::observe_soundness_fingerprint_parts(repo, &selected, &state.node_kinds, &state.node_role)?;
    let mut reused_sound = BTreeSet::new();
    let mut sound_not_reused = BTreeMap::new();
    for (node, part) in parts {
        if invalidated.get("soundness").is_some_and(|ns| ns.contains(&node)) {
            sound_not_reused.insert(node, "explicit revision audit invalidation"); continue;
        }
        if environment_equal && successor_aligned(&old.live.sound_current_fingerprints, &old.sound_approved_fingerprints, &node) == Some(&part.combined_sound_fp) {
            if let Some(status) = old.sound_status.get(&node) {
                state.sound_status.insert(node.clone(), *status);
                state.sound_approved_fingerprints.insert(node.clone(), part.combined_sound_fp.clone());
                state.live.sound_current_fingerprints.insert(node.clone(), part.combined_sound_fp.clone());
                state.live.sound_current_fingerprint_parts.insert(node.clone(), part); reused_sound.insert(node);
            }
        } else if old.sound_status.contains_key(&node) {
            sound_not_reused.insert(node, "source/goal/evidence fingerprint changed or prior verdict not current");
        }
    }
    let substantive = runtime_cli_observations::observe_substantiveness_fingerprints(repo, &selected,
        Some(Path::new("GOAL.md")), &state.node_kinds, &state.node_deviation_claims,
        &state.live.deviation_current_fingerprints, &state.configured_reference_papers, &state.node_reference_grounds)?;
    let mut reused_substantive = BTreeSet::new();
    for (node, fp) in substantive {
        if invalidated.get("substantiveness").is_some_and(|ns| ns.contains(&node)) { continue; }
        if environment_equal && successor_aligned(&old.live.substantiveness_current_fingerprints, &old.substantiveness_approved_fingerprints, &node) == Some(&fp) {
            if let Some(status) = old.substantiveness_status.get(&node) {
                state.substantiveness_status.insert(node.clone(), *status);
                state.substantiveness_approved_fingerprints.insert(node.clone(), fp.clone());
                state.live.substantiveness_current_fingerprints.insert(node.clone(), fp); reused_substantive.insert(node);
            }
        }
    }
    state.committed = state.live.clone();
    state.committed_node_kinds = state.node_kinds.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_deps = state.deps.clone();
    state.committed_target_claims = state.target_claims.clone();
    state.committed_local_closure_records = state.local_closure_records.clone();
    state.normalize_all_structural_state();
    trellis_kernel::model::recompute_local_closure_reverse_indices(state);
    state.validate()?;
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    fs::write(root.join("pv-successor-preservation.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "schema": "trellis-pv-successor-verification/v1", "predecessor_state_sha256": predecessor_state_sha256,
        "package_digest": package, "authored_nodes": selected, "closures": reused_closures,
        "target_claims": preserved_claims, "target_claims_not_reused": claims_not_reused,
        "target_claim_policy": "Authenticated unchanged positive requirement ownership with retained paired principal sources only; current closure, correspondence, soundness and human approval are checked separately. Decide registration remains at the ordinary human-approved Modeling boundary.",
        "correspondence": reused_corr, "soundness": reused_sound, "substantiveness": reused_substantive,
        "correspondence_baselines_pending_observation": pending_corr_baselines,
        "correspondence_not_reused": corr_not_reused,
        "correspondence_policy": "An old current Pass may retain its exact approved baseline without a proof certificate. A non-identical input closure has no current fingerprint and remains Unknown until an ordinary authoritative observation exactly matches. Audit-invalidated nodes and nodes whose native meaning dependencies include them are excluded; no semantic rebaseline.",
        "closure_not_reused": skipped,
        "soundness_not_reused": sound_not_reused,
        "substantiveness_policy": "Reuse requires the same node statement, kind, claimed evidence and complete goal prose fingerprint.",
        "policy": "Exact current inputs and native certificate admission; no new human approvals. Retained goal ownership is authenticated separately."
    })).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod pv_successor_tests {
    use super::*;

    #[test]
    fn unchanged_positive_claims_survive_audit_revalidation_without_approval() {
        let node = NodeId::from("Principal");
        let goal = trellis_kernel::TargetId::from("goal");
        let mut old = ProtocolState::default();
        old.configured_targets.insert(goal.clone());
        old.target_claims.insert(node.clone(), BTreeSet::from([goal.clone()]));
        let mut new = ProtocolState::default();
        new.configured_targets.insert(goal.clone());
        let collection = serde_json::json!({"context":"shared", "goals":[{"id":"goal","text":"unchanged"}]});
        let review = serde_json::json!({"original_collection":collection,"proposed_collection":collection,
            "audit_invalidation":[{"node":"goal","lanes":["correspondence"]}],
            "plan":{"dispositions":[{"goal_id":"goal","remedy":"repair"}]}});
        assert_eq!(successor_target_claims(&old, &new, &node, &review, &collection).unwrap(), BTreeSet::from([goal]));
        assert!(new.corr_status.is_empty());
        assert!(new.local_closure_records.is_empty());
        assert!(!new.modeling_human_approved);
        let mut changed = collection.clone();
        changed["goals"][0]["text"] = "changed current goal".into();
        assert!(successor_target_claims(&old, &new, &node, &review, &changed).is_err());
        changed = collection.clone();
        changed["context"] = "changed context".into();
        assert!(successor_target_claims(&old, &new, &node, &review, &changed).is_err());
        let mut replaced = review.clone();
        replaced["plan"]["dispositions"][0]["remedy"] = "replace".into();
        assert!(successor_target_claims(&old, &new, &node, &replaced, &collection).is_err());
        replaced["proposed_collection"]["goals"] = serde_json::json!([]);
        assert!(successor_target_claims(&old, &new, &node, &replaced, &collection).is_err());
    }

    #[test]
    fn predecessor_negative_or_unregistered_principal_cannot_claim_fresh_positive_goal() {
        let node = NodeId::from("Principal");
        let goal = trellis_kernel::TargetId::from("goal");
        let primary = trellis_kernel::ChallengeTargetId::from("authored:Principal");
        let mut old = ProtocolState::default();
        old.authored_goals_decidable = true;
        old.configured_targets.insert(goal.clone());
        old.target_claims.insert(node.clone(), BTreeSet::from([goal.clone()]));
        let mut new = ProtocolState::default();
        new.configured_targets.insert(goal.clone());
        let collection = serde_json::json!({"goals":[{"id":"goal","text":"unchanged"}]});
        let review = serde_json::json!({"original_collection":collection,"proposed_collection":collection});
        assert!(successor_target_claims(&old, &new, &node, &review, &collection).is_err());
        old.authored_decide_goals.insert(primary.clone(), goal);
        old.configured_challenge_targets.insert(primary.clone(), trellis_kernel::ChallengeTargetSpec {
            name: node.as_str().into(), ..Default::default()
        });
        assert!(successor_target_claims(&old, &new, &node, &review, &collection).is_ok());
        old.pv_live_polarity.insert(primary, trellis_kernel::ChallengePolarity::Disprove);
        assert!(successor_target_claims(&old, &new, &node, &review, &collection).is_err());
    }

    fn reviewed_correspondence(node: &NodeId, meaning_dependencies: &[&str]) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.corr_fingerprint_schema_version = runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION;
        state.live.present_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), trellis_kernel::NodeKind::Proof);
        let fp = runtime_cli_observations::CorrespondenceFingerprint {
            own_tex: "reviewed prose".into(),
            lean_semantic_closure: "reviewed TYPE/BODY closure".into(),
            lean_relevant_dependencies: meaning_dependencies.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }.to_storage_string();
        state.corr_status.insert(node.clone(), trellis_kernel::CorrStatus::Pass);
        state.live.corr_current_fingerprints.insert(node.clone(), fp.clone());
        state.corr_approved_fingerprints.insert(node.clone(), fp);
        state
    }

    fn successor_receipt_fixture() -> (tempfile::TempDir, PathBuf, ProtocolState) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("successor");
        let previous = root.path().join("previous");
        let revision = root.path().join("revision");
        let frozen = revision.join("predecessor");
        let package = "a".repeat(64);
        for dir in [repo.join("Tablet"), repo.join("modeling-input"), previous.clone(),
            frozen.join("Tablet"), revision.join("reviews").join(&package)] {
            fs::create_dir_all(dir).unwrap();
        }
        let node = NodeId::from("proof");
        let old = reviewed_correspondence(&node, &[]);
        let state_bytes = serde_json::to_vec(&old).unwrap();
        fs::write(frozen.join("protocol_state.json"), &state_bytes).unwrap();
        let mut files = Vec::new();
        for (name, bytes) in [("proof.lean", "theorem proof : True := by trivial\n"),
            ("proof.tex", "\\begin{theorem}True.\\end{theorem}\n")] {
            fs::write(frozen.join("Tablet").join(name), bytes).unwrap();
            fs::write(repo.join("Tablet").join(name), bytes).unwrap();
            files.push(serde_json::json!({ "relative_path": name,
                "sha256_of_raw_bytes": hash_bytes(bytes.as_bytes()) }));
        }
        fs::write(repo.join("modeling-input/preserved-dag.json"), serde_json::to_vec(&serde_json::json!({
            "schema": "trellis-preserved-dag/v1", "package_digest": package,
            "authored_nodes": ["proof"]
        })).unwrap()).unwrap();
        fs::write(repo.join("modeling-input/revision-from.json"), serde_json::to_vec(&serde_json::json!({
            "schema": "trellis-pv-revision-successor/v1", "package_digest": package,
            "revision_root": revision, "predecessor": { "repo": previous,
                "protocol_state_sha256": hash_bytes(&state_bytes), "tablet_manifest": { "files": files } }
        })).unwrap()).unwrap();
        fs::write(revision.join("reviews").join(&package).join("review.json"),
            serde_json::to_vec(&serde_json::json!({ "digest": package, "audit_invalidation": [] })).unwrap()).unwrap();
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(node.clone());
        state.node_kinds.insert(node, trellis_kernel::NodeKind::Proof);
        (root, repo, state)
    }

    #[test]
    fn maintenance_derivation_preserves_current_observations_and_newer_judgments() {
        let (_root, repo, mut state) = successor_receipt_fixture();
        let node = NodeId::from("proof");
        state.live.corr_current_fingerprints.insert(node.clone(), "fresh observation".into());
        let before = serde_json::to_value(&state).unwrap();
        let result = derive_pv_successor_correspondence_baselines(&state, &repo, &BTreeSet::new()).unwrap();
        assert_eq!(result.baselines.len(), 1);
        assert_eq!(serde_json::to_value(&state).unwrap(), before);

        let touched = derive_pv_successor_correspondence_baselines(&state, &repo,
            &BTreeSet::from([node.clone()])).unwrap();
        assert!(touched.baselines.is_empty());
        assert!(touched.exclusions[&node].contains("post-genesis"));
        state.corr_status.insert(node.clone(), trellis_kernel::CorrStatus::Unknown);
        let reset = derive_pv_successor_correspondence_baselines(&state, &repo, &BTreeSet::new()).unwrap();
        assert!(reset.baselines.is_empty());
        assert!(reset.exclusions[&node].contains("already has"));
    }

    #[test]
    fn maintenance_derivation_authenticates_frozen_source_and_checks_current_authoring() {
        let (root, repo, state) = successor_receipt_fixture();
        fs::write(repo.join("Tablet/proof.lean"), "changed successor authoring").unwrap();
        let changed = derive_pv_successor_correspondence_baselines(&state, &repo, &BTreeSet::new()).unwrap();
        assert!(changed.baselines.is_empty());
        assert!(changed.exclusions[&NodeId::from("proof")].contains("differs"));
        fs::write(root.path().join("revision/predecessor/Tablet/proof.tex"), "tampered frozen prose").unwrap();
        assert!(derive_pv_successor_correspondence_baselines(&state, &repo, &BTreeSet::new())
            .unwrap_err().contains("frozen predecessor source changed"));
    }

    #[test]
    fn maintenance_derivation_rejects_modified_predecessor_state_receipt() {
        let (root, repo, state) = successor_receipt_fixture();
        fs::write(root.path().join("revision/predecessor/protocol_state.json"), "{}").unwrap();
        assert!(derive_pv_successor_correspondence_baselines(&state, &repo, &BTreeSet::new())
            .unwrap_err().contains("does not match revision receipt"));
    }

    #[test]
    fn baseline_without_artifacts_waits_for_exact_authoritative_observation() {
        let node = NodeId::from("proof");
        let old = reviewed_correspondence(&node, &["definition"]);
        let mut new = ProtocolState::default();
        new.live.present_nodes.insert(node.clone());
        new.node_kinds.insert(node.clone(), trellis_kernel::NodeKind::Proof);
        assert!(old.local_closure_records.is_empty());
        successor_install_correspondence_baseline(&mut new, &old, &node, &BTreeSet::new(), false).unwrap();
        assert!(!new.live.corr_current_fingerprints.contains_key(&node));
        assert!(new.current_corr_unknown(&node));
        assert_eq!(new.corr_approved_fingerprints[&node], old.corr_approved_fingerprints[&node]);
        assert!(new.local_closure_records.is_empty());

        // Failed/empty or changed semantic observations must not revive Pass.
        new.live.corr_current_fingerprints.insert(node.clone(), String::new());
        assert!(new.current_corr_unknown(&node));
        let mut changed = runtime_cli_observations::CorrespondenceFingerprint::from_storage_string(
            &old.corr_approved_fingerprints[&node]).unwrap();
        changed.lean_semantic_closure = "different TYPE/BODY closure".into();
        new.live.corr_current_fingerprints.insert(node.clone(), changed.to_storage_string());
        assert!(new.current_corr_unknown(&node));
        let repo = tempfile::tempdir().unwrap();
        runtime_cli_observations::migrate_corr_fingerprint_schema(&mut new, repo.path(), None).unwrap();
        assert_eq!(new.corr_approved_fingerprints[&node], old.corr_approved_fingerprints[&node]);
        assert!(new.current_corr_unknown(&node));

        new.live.corr_current_fingerprints.insert(node.clone(), old.corr_approved_fingerprints[&node].clone());
        assert!(new.current_corr_pass(&node));
    }

    #[test]
    fn explicit_audit_blocks_baseline_for_subject_and_native_meaning_dependencies() {
        let node = NodeId::from("proof");
        let old = reviewed_correspondence(&node, &["meaning"]);
        assert!(successor_correspondence_baseline(&old, &node,
            &BTreeSet::from([node.clone()])).is_err());
        assert!(successor_correspondence_baseline(&old, &node,
            &BTreeSet::from([NodeId::from("meaning")])).is_err());
        // An irrelevant textual import is not a meaning dependency. Retaining
        // this baseline still grants no current Pass before fresh observation.
        assert!(successor_correspondence_baseline(&old, &node,
            &BTreeSet::from([NodeId::from("proof_only_import")])).is_ok());
        let mut old = old;
        let mut fp = runtime_cli_observations::CorrespondenceFingerprint::from_storage_string(
            &old.corr_approved_fingerprints[&node]).unwrap();
        fp.lean_relevant_definition_descendants.insert("definition".into(), "tex".into());
        old.live.corr_current_fingerprints.insert(node.clone(), fp.to_storage_string());
        old.corr_approved_fingerprints.insert(node.clone(), fp.to_storage_string());
        assert!(successor_correspondence_baseline(&old, &node,
            &BTreeSet::from([NodeId::from("definition")])).is_err());
    }

    #[test]
    fn stale_failed_and_incompatible_correspondence_never_seed_approval() {
        let node = NodeId::from("proof");
        let mut old = reviewed_correspondence(&node, &[]);
        for status in [trellis_kernel::CorrStatus::Unknown, trellis_kernel::CorrStatus::Fail] {
            old.corr_status.insert(node.clone(), status);
            assert!(successor_correspondence_baseline(&old, &node, &BTreeSet::new()).is_err());
        }
        old.corr_status.insert(node.clone(), trellis_kernel::CorrStatus::Pass);
        old.live.corr_current_fingerprints.insert(node.clone(), "different".into());
        assert!(successor_correspondence_baseline(&old, &node, &BTreeSet::new()).is_err());
        old.live.corr_current_fingerprints.insert(node.clone(), old.corr_approved_fingerprints[&node].clone());
        old.corr_fingerprint_schema_version = 4;
        assert!(successor_correspondence_baseline(&old, &node, &BTreeSet::new()).is_err());
        old.corr_fingerprint_schema_version = runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION;
        let mut new = ProtocolState::default();
        new.corr_fingerprint_schema_version = runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION + 1;
        assert!(successor_install_correspondence_baseline(&mut new, &old, &node, &BTreeSet::new(), false).is_err());
        assert!(new.corr_status.is_empty());
        assert!(new.corr_approved_fingerprints.is_empty());
    }

    #[test]
    fn exact_input_correspondence_is_independent_of_proof_certificate_reuse() {
        let node = NodeId::from("proof");
        let old = reviewed_correspondence(&node, &[]);
        let mut new = ProtocolState::default();
        new.live.present_nodes.insert(node.clone());
        new.node_kinds.insert(node.clone(), trellis_kernel::NodeKind::Proof);
        successor_install_correspondence_baseline(&mut new, &old, &node, &BTreeSet::new(), true).unwrap();
        assert!(new.current_corr_pass(&node));
        assert!(new.local_closure_records.is_empty());
    }

    #[test]
    fn changed_import_invalidates_transitive_reuse_but_keeps_independent_nodes() {
        let deps = BTreeMap::from([
            (NodeId::from("consumer"), BTreeSet::from([NodeId::from("middle")])),
            (NodeId::from("middle"), BTreeSet::from([NodeId::from("changed")])),
            (NodeId::from("independent"), BTreeSet::new()),
        ]);
        let candidates = ["consumer", "middle", "independent"].map(NodeId::from).into_iter().collect();
        assert_eq!(successor_dependency_closed(candidates, &deps), BTreeSet::from([NodeId::from("independent")]));
    }

    #[test]
    fn stale_or_empty_verifier_approval_does_not_transfer() {
        let node = NodeId::from("proof");
        let mut current = BTreeMap::from([(node.clone(), "current".into())]);
        let approved = BTreeMap::from([(node.clone(), "old".into())]);
        assert_eq!(successor_aligned(&current, &approved, &node), None);
        current.insert(node.clone(), "old".into());
        assert_eq!(successor_aligned(&current, &approved, &node), approved.get(&node));
        current.insert(node.clone(), String::new());
        assert_eq!(successor_aligned(&current, &current, &node), None);
    }

    #[test]
    fn retained_artifact_must_match_the_replay_evidence_before_copy() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let path = old.path().join(".lake/build/lib/lean/Tablet");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("proof.olean"), b"modified").unwrap();
        let mut record = LocalClosureRecord { node: "proof".into(), ..Default::default() };
        record.node_certificate_evidence = Some(trellis_kernel::NodeCertificateEvidence {
            artifact_bundle: vec![trellis_kernel::ArtifactBundlePart {
                level: "exported".into(), size_bytes: 8,
                sha256: trellis_kernel::trust_base::raw_sha256(b"original"),
            }], ..Default::default()
        });
        assert!(successor_copy_artifacts(old.path(), new.path(), &record).is_err());
        assert!(!new.path().join(".lake").exists());
        fs::write(path.join("proof.olean"), b"original").unwrap();
        successor_copy_artifacts(old.path(), new.path(), &record).unwrap();
        assert_eq!(fs::read(new.path().join(".lake/build/lib/lean/Tablet/proof.olean")).unwrap(), b"original");
    }
}
