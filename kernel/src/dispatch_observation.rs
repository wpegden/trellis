//! Capture scheduler dependencies from existing replay evidence. Source text
//! alone is insufficient to establish the complete Lean import relation.
use crate::dispatch::DependencyGraph;
use crate::model::{NodeId, Phase, ProtocolState, Update, WorkerResponse};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Complete source/evidence view copied into verifier sandboxes. In particular
/// substantiveness reads consumers, not only the node being judged. Bind all
/// those files until per-lane evidence bundles can enforce a narrower read set.
/// Build products and operational logs are not mathematical panel inputs.
pub fn verifier_inputs(repo: &Path) -> Result<BTreeMap<String, String>, String> {
    fn visit(repo: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        for item in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let path = item.path();
            let name = item.file_name();
            let name = name.to_str().ok_or("non-UTF8 verifier input")?;
            if matches!(name, ".git" | ".lake" | ".trellis-history" | ".pytest_cache" |
                "__pycache__" | "node_modules" | ".viewer" | "target" | "last_invalid" |
                ".trellis-stop-after-checkpoint") { continue; }
            let relative = path.strip_prefix(repo).map_err(|e| e.to_string())?;
            if relative.starts_with(".trellis") && relative != Path::new(".trellis")
                && !relative.starts_with(".trellis/artifacts") && !relative.starts_with(".trellis/scripts") { continue; }
            let metadata = path.symlink_metadata().map_err(|e| e.to_string())?;
            if metadata.is_dir() { visit(repo, &path, out)?; }
            else if metadata.is_file() {
                use sha2::{Digest, Sha256};
                let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                out.insert(relative.to_str().ok_or("non-UTF8 input path")?.into(), format!("{:x}", Sha256::digest(bytes)));
            } else { return Err(format!("verifier input is not regular: {}", relative.display())); }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(repo, repo, &mut out)?;
    Ok(out)
}

#[derive(serde::Serialize)]
pub struct SourceView {
    pub graph: DependencyGraph,
    pub manifest: BTreeMap<String, String>,
    pub input_hashes: BTreeMap<String, String>,
    pub proof_body_rebase_inputs: BTreeMap<String, String>,
}

pub fn observe(state: &ProtocolState, repo: &Path, response: Option<&WorkerResponse>) -> Result<SourceView, String> {
    let manifest = crate::dispatch_publication::source_manifest(repo)?;
    let present = response.map(|r| &r.snapshot.present_nodes).unwrap_or(&state.live.present_nodes);
    // A fresh Modeling draft is deliberately not yet compilable. Its worker
    // owns the whole tablet exclusively, so scheduling needs no inferred Lean
    // import relation. Bind every source byte and retain declared edges; the
    // ordinary activation check must still issue all owner certificates.
    let draft = state.phase == Phase::Modeling && state.live.modeling_draft
        && response.is_none_or(|r| r.snapshot.modeling_draft);
    if draft && state.dispatch.as_ref().is_some_and(|ledger| ledger.tasks.values()
        .any(|task| task.status.occupies_slot() && !task.assignment.scope.exclusive)) {
        return Err("an uncertified Modeling draft requires exclusive worker ownership".into());
    }
    let mut edges = BTreeMap::new();
    let mut inputs = manifest.clone();
    let mut proof_body_rebase_inputs = BTreeMap::new();
    for node in present {
        let path = format!("Tablet/{node}.lean");
        let source = manifest.get(&path).ok_or_else(|| format!("missing task source: {path}"))?;
        if state.proof_nodes.contains(node) {
            let content = std::fs::read_to_string(repo.join(&path)).map_err(|e| e.to_string())?;
            if let Ok(split) = crate::filespec_split::split(&content, node.as_str()) {
                // Bind both identities to the same bytes. Invalid/missing BODY
                // markers simply retain the legacy exact-source requirement.
                if &split.source_hash != source { return Err(format!("task source changed during observation: {path}")); }
                use sha2::{Digest, Sha256};
                proof_body_rebase_inputs.insert(path.clone(), format!("{:x}",
                    Sha256::digest(&content.as_bytes()[..split.body_marker_end_byte])));
            }
        }
        let probe = response.and_then(|r| r.local_closure_results.get(node))
            .filter(|p| &p.source_sha256 == source);
        let record = response.and_then(|r| r.local_closure_revalidation.as_ref())
            .and_then(|batch| batch.refreshed.iter().find(|(id, record)| id == node && &record.active_decl_hash == source))
            .map(|(_, record)| record)
            .or_else(|| state.local_closure_records.get(node))
            .filter(|r| &r.active_decl_hash == source);
        let imports = probe.and_then(|p| p.direct_imports.as_ref())
            .or_else(|| record.and_then(|r| r.node_certificate_evidence.as_ref())
                .filter(|e| &e.source_sha256 == source).and_then(|e| e.direct_imports.as_ref()));
        if imports.is_none() && !draft {
            return Err(format!("parallel dispatch needs current replay-attested imports for {node}; run the ordinary authoritative source check"));
        }
        let mut deps: BTreeSet<NodeId> = imports.into_iter().flatten().filter_map(|name| name.strip_prefix("Tablet."))
            .map(NodeId::from).filter(|dep| dep != node && (dep.as_str() != "Preamble" || present.contains(dep))).collect();
        if let Some(Update::Set(next)) = response.and_then(|r| r.dep_updates.get(node)) {
            deps.extend(next.iter().cloned());
        } else {
            deps.extend(state.deps.get(node).into_iter().flatten().cloned());
        }
        if let Some(record) = record {
            deps.extend(record.boundary_theorems.keys().cloned());
            deps.extend(record.strict_theorem_deps.keys().cloned());
            deps.extend(record.strict_definition_deps.keys().cloned());
        }
        deps.remove(node);
        if !deps.is_subset(present) {
            return Err(format!("{node} depends on an unregistered Tablet module; capture its ownership before parallel dispatch"));
        }
        edges.insert(node.clone(), deps);
        inputs.entry(format!("Tablet/{node}.tex")).or_insert_with(|| "absent".into());
    }
    Ok(SourceView { graph: DependencyGraph { edges }, manifest, input_hashes: inputs, proof_body_rebase_inputs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LocalClosureRecord, LocalClosureProbeOutput};
    use crate::node_certificate::NodeCertificateEvidence;
    #[test]
    fn modeling_draft_observation_binds_sources_but_activation_requires_certificates() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Tablet")).unwrap();
        for node in ["Assumptions", "A"] {
            std::fs::write(tmp.path().join(format!("Tablet/{node}.lean")), "draft not yet Lean").unwrap();
        }
        let mut state = ProtocolState::default();
        state.phase = Phase::Modeling;
        state.live.modeling_draft = true;
        state.live.present_nodes = ["Assumptions".into(), "A".into()].into();
        state.deps.insert("A".into(), ["Assumptions".into()].into());
        let draft = observe(&state, tmp.path(), None).unwrap();
        assert_eq!(draft.graph.edges[&NodeId::from("A")], ["Assumptions".into()].into());
        assert_eq!(draft.input_hashes["Tablet/A.lean"], draft.manifest["Tablet/A.lean"]);
        assert!(state.local_closure_records.is_empty());
        let mut response = WorkerResponse { snapshot: state.live.clone(), ..Default::default() };
        observe(&state, tmp.path(), Some(&response)).unwrap();
        response.snapshot.modeling_draft = false;
        assert!(observe(&state, tmp.path(), Some(&response)).err().unwrap().contains("replay-attested"));
        for node in ["Assumptions", "A"] {
            response.local_closure_results.insert(node.into(), LocalClosureProbeOutput {
                source_sha256: draft.manifest[&format!("Tablet/{node}.lean")].clone(),
                direct_imports: Some(BTreeSet::new()), ..Default::default()
            });
        }
        observe(&state, tmp.path(), Some(&response)).unwrap();
        state.live.modeling_draft = false;
        assert!(observe(&state, tmp.path(), None).err().unwrap().contains("replay-attested"));
        state.live.modeling_draft = true;
        state.phase = Phase::ProofFormalization;
        assert!(observe(&state, tmp.path(), None).err().unwrap().contains("replay-attested"));
    }
    #[test]
    fn proof_prefix_capture_binds_current_exact_bytes_and_preserves_full_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Tablet")).unwrap();
        let path = tmp.path().join("Tablet/A.lean");
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert("A".into()); state.proof_nodes.insert("A".into());
        let observe_text = |text: &str, state: &ProtocolState| {
            std::fs::write(&path, text).unwrap();
            let source = crate::dispatch_publication::source_manifest(tmp.path()).unwrap()["Tablet/A.lean"].clone();
            let response = WorkerResponse { snapshot: state.live.clone(), local_closure_results: BTreeMap::from([
                ("A".into(), LocalClosureProbeOutput { source_sha256: source,
                    direct_imports: Some(BTreeSet::new()), ..Default::default() }),
            ]), ..Default::default() };
            observe(state, tmp.path(), Some(&response)).unwrap()
        };
        let first = observe_text("theorem A : True :=\r\n-- BODY\r\nby sorry\r\n", &state);
        let second = observe_text("theorem A : True :=\r\n-- BODY\r\nby trivial\r\n", &state);
        assert_eq!(first.proof_body_rebase_inputs, second.proof_body_rebase_inputs);
        assert_ne!(first.input_hashes["Tablet/A.lean"], second.input_hashes["Tablet/A.lean"]);
        let statement = observe_text("theorem A : False :=\r\n-- BODY\r\nby sorry\r\n", &state);
        assert_ne!(first.proof_body_rebase_inputs, statement.proof_body_rebase_inputs);
        for malformed in ["theorem A : True := by trivial", "-- BODY\n-- BODY\n"] {
            assert!(observe_text(malformed, &state).proof_body_rebase_inputs.is_empty());
        }
        state.proof_nodes.clear();
        assert!(observe_text("def A :=\n-- BODY\n1\n", &state).proof_body_rebase_inputs.is_empty());
    }

    #[test]
    fn scheduling_uses_replayed_imports_and_binds_missing_tex() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Tablet")).unwrap();
        std::fs::write(tmp.path().join("Tablet/A.lean"), "import\n  Tablet.B\n").unwrap();
        std::fs::write(tmp.path().join("Tablet/B.lean"), "def B := 1").unwrap();
        let hashes = crate::dispatch_publication::source_manifest(tmp.path()).unwrap();
        let mut state = ProtocolState::default();
        state.live.present_nodes = BTreeSet::from(["A".into(), "B".into()]);
        for name in ["A", "B"] {
            let source = hashes[&format!("Tablet/{name}.lean")].clone();
            state.local_closure_records.insert(name.into(), LocalClosureRecord {
                node: name.into(), active_decl_hash: source.clone(),
                node_certificate_evidence: Some(NodeCertificateEvidence {
                    source_sha256: source, direct_imports: Some(if name == "A" {
                        BTreeSet::from(["Tablet.B".into()]) } else { BTreeSet::new() }),
                    ..Default::default()
                }), ..Default::default()
            });
        }
        let view = observe(&state, tmp.path(), None).unwrap();
        assert_eq!(view.graph.edges[&NodeId::from("A")], BTreeSet::from(["B".into()]));
        assert_eq!(view.input_hashes["Tablet/B.tex"], "absent");
        std::fs::write(tmp.path().join("Tablet/A.lean"), "new source").unwrap();
        assert!(observe(&state, tmp.path(), None).err().unwrap().contains("replay-attested"));
        let source = crate::dispatch_publication::source_manifest(tmp.path()).unwrap()["Tablet/A.lean"].clone();
        let response = WorkerResponse { snapshot: state.live.clone(), local_closure_results: BTreeMap::from([
            ("A".into(), LocalClosureProbeOutput { source_sha256: source, direct_imports: Some(BTreeSet::from(["Tablet.B".into()])), ..Default::default() }),
        ]), ..Default::default() };
        observe(&state, tmp.path(), Some(&response)).unwrap();
    }
}
