//! Verify the host's recoverable source publication at the native commit seam.
//! A journal is evidence of a durable decision, never a closure certificate.
use crate::dispatch_integration::{PublicationReceipt, WorkerPublication};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path};

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredArtifacts {
    format: String,
    journal_sha256: String,
    receipt: PublicationReceipt,
    complete: bool,
}

/// A retired cache can only acknowledge an already committed, resolved task.
/// It can never authorize a source write, artifact install, or new publication.
/// The host may have stopped after any unlink; `complete` is diagnostic only.
pub fn retired_receipt(runtime: &Path, repo: &Path, payload: &WorkerPublication,
    state: &crate::model::ProtocolState) -> Result<Option<PublicationReceipt>, String> {
    use std::os::unix::fs::MetadataExt;
    let root = runtime.join("dispatch/publication");
    match std::fs::symlink_metadata(root.join("pending.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(_) => (),
    }
    let pending: Pending = serde_json::from_slice(&regular(&root, "pending.json")?).map_err(|e| e.to_string())?;
    if !sha(&pending.identity) { return Err("invalid publication journal identity".into()); }
    let relative = format!("{}/artifacts-retired.json", pending.identity);
    let metadata = match std::fs::symlink_metadata(root.join(&relative)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(metadata) => metadata,
    };
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > 4096 {
        return Err("invalid publication cache retirement marker".into());
    }
    let marker: RetiredArtifacts = serde_json::from_slice(&regular(&root, &relative)?).map_err(|e| e.to_string())?;
    let _completion_is_not_authority = marker.complete;
    if marker.format != "trellis-publication-artifacts-retired/1" || marker.journal_sha256 != pending.identity {
        return Err("publication cache retirement belongs to another journal".into());
    }
    let bytes = regular(&root, &format!("{}/record.json", pending.identity))?;
    if hash(&bytes) != pending.identity { return Err("retired publication journal changed".into()); }
    let record: Journal = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if record.version != 2 || record.artifacts.is_none()
        || Path::new(&record.repo) != repo.canonicalize().map_err(|e| e.to_string())?
        || record.task_id != payload.task_id || record.candidate_sha256 != payload.candidate_sha256
        || record.kernel_event != *payload {
        return Err("retired publication differs from the exact committed event".into());
    }
    let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    let receipt = ledger.publications.get(&payload.task_id)
        .ok_or("retired publication has no native receipt")?.clone();
    // Hash the original JSON event, not a typed reserialization that can fill
    // or omit fields after schema evolution. The typed equality above still
    // binds the caller to precisely this historical event's semantics.
    let original: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if receipt.candidate_sha256 != record.candidate_sha256
        || receipt.payload_sha256 != crate::trusted_artifact_rebind::digest(&original["kernel_event"]) {
        return Err("retired publication differs from the recorded native payload".into());
    }
    let task = ledger.tasks.get(&payload.task_id).ok_or("retired publication task is absent")?;
    if receipt != marker.receipt || task.pending_retry.is_some()
        || ledger.tasks.values().any(|other| other.retry_of == Some(payload.task_id))
        || !matches!(&task.status, crate::dispatch::TaskStatus::Resolved {
            outcome: crate::dispatch::AttemptOutcome::Accepted { event_id }, ..
        } if *event_id == receipt.event_id) {
        return Err("retired publication no longer has its terminal native receipt".into());
    }
    Ok(Some(receipt))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending { identity: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change { before: Option<String>, after: Option<String> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    task_id: u64,
    candidate_sha256: String,
    repo: String,
    before: BTreeMap<String, String>,
    after: BTreeMap<String, String>,
    changes: BTreeMap<String, Change>,
    kernel_event: WorkerPublication,
    #[serde(default)]
    artifacts: Option<ArtifactHandoff>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactHandoff {
    version: u32,
    nodes: BTreeMap<String, ArtifactNode>,
    #[serde(default)]
    portable_environments: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactNode {
    source_closure_sha256: String,
    artifact_bundle: Vec<crate::node_certificate::ArtifactBundlePart>,
    files: BTreeMap<String, Option<String>>,
}

fn artifact_paths(node: &str) -> Result<Vec<String>, String> {
    if node.is_empty() || !node.bytes().enumerate().all(|(i, b)| {
        b.is_ascii_alphabetic() || b == b'_' || (i > 0 && (b.is_ascii_digit() || b == b'\''))
    }) { return Err("invalid publication artifact owner".into()); }
    let mut paths: Vec<_> = [".olean", ".olean.server", ".olean.private", ".olean.srcclosure",
        ".olean.hash", ".olean.server.hash", ".olean.private.hash", ".ilean", ".ilean.hash", ".ir", ".ir.hash", ".trace"]
        .into_iter().map(|suffix| format!("lib/lean/Tablet/{node}{suffix}")).collect();
    paths.extend([format!("ir/Tablet/{node}.c"), format!("ir/Tablet/{node}.c.hash")]);
    Ok(paths)
}

fn verify_artifacts(root: &Path, handoff: &ArtifactHandoff, payload: &WorkerPublication) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if handoff.version != 1 { return Err("unsupported publication artifact handoff".into()); }
    if handoff.portable_environments.len() > 128 {
        return Err("portable publication inventory exceeds limit".into());
    }
    let mut total = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    for identity in &handoff.portable_environments {
        if !sha(identity) || !seen.insert(identity) {
            return Err("invalid portable publication inventory".into());
        }
        let relative = format!(".trellis/kernel-replay-environments/{identity}.json");
        let metadata = std::fs::symlink_metadata(root.join(&relative)).map_err(|e| e.to_string())?;
        total = total.saturating_add(metadata.len());
        if metadata.len() > 32 << 20 || total > 64 << 20 {
            return Err("portable publication inventory exceeds limit".into());
        }
        if metadata.nlink() != 1 || hash(&regular(root, &relative)?) != *identity {
            return Err("portable publication evidence changed".into());
        }
    }
    for (node, entry) in &handoff.nodes {
        let expected: std::collections::BTreeSet<_> = artifact_paths(node)?.into_iter().collect();
        if !sha(&entry.source_closure_sha256) || expected != entry.files.keys().cloned().collect() {
            return Err("publication artifact inventory is incomplete".into());
        }
        let certificate = payload.response.local_closure_revalidation.as_ref()
            .and_then(|batch| batch.refreshed.iter().find(|(name, _)| name.as_str() == node))
            .and_then(|(_, record)| record.node_certificate.as_ref())
            .ok_or("publication artifact lacks its checked response certificate")?;
        if entry.artifact_bundle.is_empty() || entry.artifact_bundle != certificate.artifact_bundle {
            return Err("publication artifact bundle differs from checked certificate".into());
        }
        for (path, digest) in &entry.files {
            if let Some(digest) = digest {
                let relative = format!("artifacts/{path}");
                let bytes = regular(root, &relative)?;
                if !sha(digest) || hash(&bytes) != *digest || std::fs::metadata(root.join(relative)).map_err(|e| e.to_string())?.nlink() != 1 {
                    return Err(format!("publication artifact evidence changed: {path}"));
                }
            }
        }
        for (level, suffix) in [("exported", ".olean"), ("server", ".olean.server"), ("private", ".olean.private")] {
            let path = format!("lib/lean/Tablet/{node}{suffix}");
            match entry.artifact_bundle.iter().find(|part| part.level == level) {
                Some(part) => {
                    if entry.files[&path].as_deref() != Some(part.sha256.to_string().as_str())
                        || regular(root, &format!("artifacts/{path}"))?.len() as u64 != part.size_bytes {
                        return Err("publication artifact visibility bundle mismatch".into());
                    }
                }
                None if entry.files[&path].is_some() => return Err("unexpected publication artifact visibility part".into()),
                None => (),
            }
        }
    }
    Ok(())
}

fn regular(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let path = Path::new(relative);
    if relative.is_empty() || relative.contains('\\') || relative.contains('\0')
        || relative.split('/').any(|p| matches!(p, "" | "." | ".."))
        || path.components().any(|c| !matches!(c, Component::Normal(_)))
        || path.to_string_lossy() != relative
    { return Err(format!("unsafe publication path: {relative}")); }
    let mut current = root.to_path_buf();
    for component in path.components() {
        current.push(component);
        if std::fs::symlink_metadata(&current).map_err(|e| format!("{}: {e}", current.display()))?.file_type().is_symlink() {
            return Err(format!("publication path traverses a symlink: {relative}"));
        }
    }
    if !current.is_file() { return Err(format!("publication input is not a regular file: {relative}")); }
    std::fs::read(&current).map_err(|e| format!("{}: {e}", current.display()))
}

/// Match the complete worker source surfaces used by the Python publisher.
/// Exact set equality also detects unrelated additions and deletions.
pub fn source_manifest(repo: &Path) -> Result<BTreeMap<String, String>, String> {
    fn walk(repo: &Path, path: &Path, out: &mut BTreeMap<String, String>) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
                walk(repo, &entry.map_err(|e| e.to_string())?.path(), out)?;
            }
        } else if metadata.is_file() {
            let relative = path.strip_prefix(repo).map_err(|e| e.to_string())?.to_str()
                .ok_or("publication path is not UTF-8")?.to_owned();
            out.insert(relative.clone(), hash(&regular(repo, &relative)?));
        } else {
            return Err(format!("non-regular publication source: {}", path.display()));
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    for surface in ["Tablet", "reference"] {
        let path = repo.join(surface);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
            Ok(meta) if !meta.is_dir() => return Err(format!("publication surface is not a directory: {surface}")),
            _ => walk(repo, &path, &mut out)?,
        }
    }
    Ok(out)
}

pub fn verify_journal(runtime: &Path, repo: &Path, payload: &WorkerPublication) -> Result<(), String> {
    let root = runtime.join("dispatch/publication");
    let pending: Pending = serde_json::from_slice(&regular(&root, "pending.json")?).map_err(|e| e.to_string())?;
    if !sha(&pending.identity) { return Err("invalid publication journal identity".into()); }
    let bytes = regular(&root, &format!("{}/record.json", pending.identity))?;
    if hash(&bytes) != pending.identity { return Err("publication journal digest mismatch".into()); }
    let record: Journal = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !matches!(record.version, 1 | 2) || Path::new(&record.repo) != repo.canonicalize().map_err(|e| e.to_string())?
        || record.task_id != payload.task_id || record.candidate_sha256 != payload.candidate_sha256
        || record.kernel_event != *payload
    { return Err("publication journal differs from the checked task, repository or event".into()); }
    if source_manifest(repo)? != record.after { return Err("canonical sources differ from the checked publication".into()); }
    let changed: std::collections::BTreeSet<_> = record.before.keys().chain(record.after.keys())
        .filter(|p| record.before.get(*p) != record.after.get(*p)).cloned().collect();
    if changed != record.changes.keys().cloned().collect() {
        return Err("publication change inventory is incomplete".into());
    }
    for (path, change) in &record.changes {
        if change.before.as_ref() != record.before.get(path) || change.after.as_ref() != record.after.get(path) {
            return Err(format!("publication change disagrees with its manifests: {path}"));
        }
        if let Some(expected) = &change.after {
            let bytes = regular(&root, &format!("{}/files/{path}", pending.identity))?;
            if hash(&bytes) != *expected { return Err(format!("publication source evidence changed: {path}")); }
        }
    }
    match (record.version, record.artifacts.as_ref()) {
        (1, None) => (),
        (2, Some(handoff)) => verify_artifacts(&root.join(&pending.identity), handoff, payload)?,
        _ => return Err("publication version/artifact manifest mismatch".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload() -> WorkerPublication {
        WorkerPublication { closure_stuck_to_review: false, fresh_checked_inputs: None, submitted_outcome: None, task_id: 1, candidate_sha256: "a".repeat(64), base_state_sha256: "b".repeat(64),
            decided_at_ms: 40, inputs: BTreeMap::new(), proof_body_rebase_inputs: BTreeMap::new(), actual_scope: Default::default(),
            graph_before: crate::dispatch::DependencyGraph { edges: BTreeMap::new() },
            graph_after: crate::dispatch::DependencyGraph { edges: BTreeMap::new() }, response: Default::default() }
    }
    fn write_journal(runtime: &Path, record: &serde_json::Value, files: &[(&str, &str)]) -> String {
        let bytes = serde_json::to_vec(record).unwrap(); let id = hash(&bytes);
        let root = runtime.join("dispatch/publication");
        std::fs::create_dir_all(root.join(&id)).unwrap();
        std::fs::write(root.join(&id).join("record.json"), bytes).unwrap();
        for (path, content) in files {
            let dest = root.join(&id).join("files").join(path);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, content).unwrap();
        }
        std::fs::write(root.join("pending.json"), serde_json::to_vec(&serde_json::json!({"identity": id})).unwrap()).unwrap();
        id
    }
    fn retired_state(event: &WorkerPublication) -> crate::model::ProtocolState {
        use crate::dispatch::*;
        use crate::model::{Phase, WrapperRequest};
        let mut state = crate::model::ProtocolState::default();
        let mut ledger = DispatchLedger::new(DispatchConfig::default(), Phase::ProofFormalization).unwrap();
        let assignment = PreparedAssignment {
            assignment_id: "one".into(), profile: "sol-xhigh".into(), timeout_seconds: None,
            compiler: CompilerCapability::Managed, task_kind: WorkerTaskKind::Ordinary,
            scope: EditScope::default(), base_snapshot: "captured".into(),
            input_hashes: BTreeMap::new(), proof_body_rebase_inputs: BTreeMap::new(),
            request: std::sync::Arc::new(WrapperRequest::default()).into(), review: None, authorized_worker: None,
        };
        ledger.tasks.insert(1, WorkerTask {
            id: 1, review_id: 2, assignment, binding: ledger.config.profiles["sol-xhigh"].clone(),
            status: TaskStatus::Resolved { outcome: AttemptOutcome::Accepted { event_id: 41 }, resolved_at_ms: 50 },
            retry_of: None, retry: RetryContext::default(), reserved_at_ms: 1, launched_at_ms: Some(2),
            process: None, checker_wait_ms: 0, report: None, pending_retry: None, retry_refusal: None,
            worker_attempt: None, execution_capacity_released: false,
        });
        ledger.publications.insert(1, PublicationReceipt {
            candidate_sha256: event.candidate_sha256.clone(), payload_sha256: event.digest().unwrap(), event_id: 41,
        });
        state.dispatch = Some(ledger);
        state
    }

    #[test]
    fn no_pending_publication_preserves_uncommitted_receipt_query_and_commit_refusal() {
        let tmp = tempfile::tempdir().unwrap(); let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap(); let runtime = tmp.path().join("runtime");
        let event = payload(); let mut state = retired_state(&event);
        state.dispatch.as_mut().unwrap().publications.clear();
        assert!(retired_receipt(&runtime,&repo,&event,&state).unwrap().is_none());
        assert!(event.committed(&state).unwrap().is_none());
        assert!(verify_journal(&runtime,&repo,&event).is_err());
    }

    #[test]
    fn retired_artifacts_replay_exact_terminal_receipt_without_cache_or_source_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("runtime"); let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let event = payload(); let mut state = retired_state(&event);
        let record = serde_json::json!({"version":2,"task_id":1,"candidate_sha256":event.candidate_sha256,
            "repo":repo,"before":{},"after":{},"changes":{},"kernel_event":event,
            "artifacts":{"version":1,"nodes":{},"portable_environments":["c".repeat(64)]}});
        let id = write_journal(&runtime,&record,&[]);
        assert!(retired_receipt(&runtime,&repo,&event,&state).unwrap().is_none());
        let receipt = event.committed(&state).unwrap().unwrap();
        let marker = runtime.join("dispatch/publication").join(&id).join("artifacts-retired.json");
        for complete in [false,true] {
            std::fs::write(&marker,serde_json::to_vec(&serde_json::json!({
                "format":"trellis-publication-artifacts-retired/1","journal_sha256":id,
                "receipt":receipt,"complete":complete})).unwrap()).unwrap();
            assert_eq!(retired_receipt(&runtime,&repo,&event,&state).unwrap(),Some(receipt.clone()));
        }
        // The ordinary validator still needs missing cache evidence. Only the
        // exact already-committed terminal branch may avoid that work.
        assert!(verify_journal(&runtime,&repo,&event).is_err());
        let mut wrong = event.clone(); wrong.decided_at_ms += 1;
        assert!(retired_receipt(&runtime,&repo,&wrong,&state).is_err());
        state.dispatch.as_mut().unwrap().tasks.get_mut(&1).unwrap().status = crate::dispatch::TaskStatus::Reserved;
        assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
        state = retired_state(&event);
        let mut retry = state.dispatch.as_ref().unwrap().tasks[&1].clone(); retry.id = 2; retry.retry_of = Some(1);
        state.dispatch.as_mut().unwrap().tasks.insert(2,retry);
        assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
        state = retired_state(&event);
        state.dispatch.as_mut().unwrap().publications.clear();
        assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
    }

    #[test]
    fn retired_replay_preserves_original_digest_across_optional_field_serialization_changes() {
        let tmp = tempfile::tempdir().unwrap(); let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap(); let runtime = tmp.path().join("runtime");
        let event = payload(); let mut state = retired_state(&event);
        let mut original = serde_json::to_value(&event).unwrap();
        original["proof_body_rebase_inputs"] = serde_json::json!({});
        original.as_object_mut().unwrap().remove("closure_stuck_to_review");
        let receipt = state.dispatch.as_mut().unwrap().publications.get_mut(&1).unwrap();
        receipt.payload_sha256 = crate::trusted_artifact_rebind::digest(&original);
        let receipt = receipt.clone();
        assert!(event.committed(&state).is_err());
        let record = serde_json::json!({"version":2,"task_id":1,"candidate_sha256":event.candidate_sha256,
            "repo":repo,"before":{},"after":{},"changes":{},"kernel_event":original,
            "artifacts":{"version":1,"nodes":{}}});
        let id = write_journal(&runtime,&record,&[]);
        let marker = runtime.join("dispatch/publication").join(&id).join("artifacts-retired.json");
        std::fs::write(marker,serde_json::to_vec(&serde_json::json!({
            "format":"trellis-publication-artifacts-retired/1","journal_sha256":id,
            "receipt":receipt,"complete":false})).unwrap()).unwrap();
        assert_eq!(retired_receipt(&runtime,&repo,&event,&state).unwrap(),Some(receipt));
    }

    #[test]
    fn retired_artifact_marker_and_journal_cannot_be_substituted() {
        let tmp = tempfile::tempdir().unwrap(); let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap(); let runtime = tmp.path().join("runtime");
        let event = payload(); let state = retired_state(&event);
        let record = serde_json::json!({"version":2,"task_id":1,"candidate_sha256":event.candidate_sha256,
            "repo":repo,"before":{},"after":{},"changes":{},"kernel_event":event,
            "artifacts":{"version":1,"nodes":{}}});
        let id = write_journal(&runtime,&record,&[]);
        let folder = runtime.join("dispatch/publication").join(&id);
        let marker = folder.join("artifacts-retired.json");
        let valid = serde_json::json!({"format":"trellis-publication-artifacts-retired/1",
            "journal_sha256":id,"receipt":event.committed(&state).unwrap().unwrap(),"complete":true});
        for mutation in ["identity","receipt","unknown","malformed"] {
            let mut value = valid.clone();
            match mutation {
                "identity" => value["journal_sha256"] = serde_json::json!("f".repeat(64)),
                "receipt" => value["receipt"]["event_id"] = serde_json::json!(42),
                "unknown" => value["future"] = serde_json::json!(true),
                _ => value["complete"] = serde_json::json!(1),
            }
            std::fs::write(&marker,serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
        }
        std::fs::write(&marker,serde_json::to_vec(&valid).unwrap()).unwrap();
        std::fs::hard_link(&marker,folder.join("alias")).unwrap();
        assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
        std::fs::remove_file(folder.join("alias")).unwrap();
        std::fs::write(folder.join("record.json"),b"{}").unwrap();
        assert!(retired_receipt(&runtime,&repo,&event,&state).is_err());
    }

    #[test]
    fn publication_journal_binds_sources_event_and_complete_inventory() {
        let tmp = tempfile::tempdir().unwrap(); let runtime = tmp.path().join("runtime");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        std::fs::write(repo.join("Tablet/A.lean"), "new").unwrap();
        std::fs::write(repo.join("Tablet/Peer.lean"), "accepted peer").unwrap();
        let after = source_manifest(&repo).unwrap(); let mut before = after.clone();
        before.insert("Tablet/A.lean".into(), hash(b"old"));
        let event = payload();
        let record = serde_json::json!({"version": 1, "task_id": 1, "candidate_sha256": event.candidate_sha256,
            "repo": repo, "before": before, "after": after,
            "changes": {"Tablet/A.lean": {"before": hash(b"old"), "after": hash(b"new")}}, "kernel_event": event});
        let id = write_journal(&runtime, &record, &[("Tablet/A.lean", "new")]);
        verify_journal(&runtime, &repo, &event).unwrap();
        let mut wrong = event.clone(); wrong.decided_at_ms += 1;
        assert!(verify_journal(&runtime, &repo, &wrong).unwrap_err().contains("differs"));
        std::fs::write(repo.join("Tablet/Peer.lean"), "unrelated writer").unwrap();
        assert!(verify_journal(&runtime, &repo, &event).unwrap_err().contains("canonical sources"));
        std::fs::write(repo.join("Tablet/Peer.lean"), "accepted peer").unwrap();
        std::fs::write(runtime.join("dispatch/publication").join(&id).join("files/Tablet/A.lean"), "tampered").unwrap();
        assert!(verify_journal(&runtime, &repo, &event).unwrap_err().contains("evidence changed"));
        let mut incomplete = record.clone(); incomplete["changes"] = serde_json::json!({});
        write_journal(&runtime, &incomplete, &[]);
        assert!(verify_journal(&runtime, &repo, &event).unwrap_err().contains("inventory"));
    }
    #[test]
    fn v2_artifacts_bind_complete_visibility_and_independent_journal_bytes() {
        use crate::node_certificate::{ArtifactBundlePart, NodeCertificate};
        let tmp = tempfile::tempdir().unwrap(); let root = tmp.path();
        let bytes = b"exact host checked exported artifact";
        let path = "lib/lean/Tablet/A.olean";
        let part = ArtifactBundlePart { level: "exported".into(),
            sha256: crate::trust_base::raw_sha256(bytes), size_bytes: bytes.len() as u64 };
        let mut event = payload();
        event.response.local_closure_revalidation = Some(crate::RevalidationBatch {
            refreshed: vec![("A".into(), crate::LocalClosureRecord {
                node_certificate: Some(NodeCertificate { artifact_bundle: vec![part.clone()], ..Default::default() }),
                ..Default::default()
            })], still_unverified: vec![],
        });
        let mut files: BTreeMap<_, _> = artifact_paths("A").unwrap().into_iter().map(|name| (name, None)).collect();
        files.insert(path.into(), Some(hash(bytes)));
        let mut handoff = ArtifactHandoff { version: 1, nodes: BTreeMap::from([("A".into(), ArtifactNode {
            source_closure_sha256: "c".repeat(64), artifact_bundle: vec![part], files,
        })]), portable_environments: vec![] };
        let artifact = root.join("artifacts").join(path);
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, bytes).unwrap();
        verify_artifacts(root, &handoff, &event).unwrap();
        std::fs::write(&artifact, b"wrong root artifact").unwrap();
        assert!(verify_artifacts(root, &handoff, &event).unwrap_err().contains("evidence changed"));
        std::fs::write(&artifact, bytes).unwrap();
        std::fs::hard_link(&artifact, root.join("shared-writable")).unwrap();
        assert!(verify_artifacts(root, &handoff, &event).unwrap_err().contains("evidence changed"));
        std::fs::remove_file(root.join("shared-writable")).unwrap();
        handoff.nodes.get_mut("A").unwrap().files.remove("lib/lean/Tablet/A.olean.private");
        assert!(verify_artifacts(root, &handoff, &event).unwrap_err().contains("inventory"));
    }

    #[test]
    fn portable_publication_inventory_is_optional_bounded_and_content_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let legacy: ArtifactHandoff = serde_json::from_str(r#"{"version":1,"nodes":{}}"#).unwrap();
        verify_artifacts(root, &legacy, &payload()).unwrap();
        let bytes = br#"{"version":1,"roots":["A"]}"#;
        let identity = hash(bytes);
        let path = root.join(format!(".trellis/kernel-replay-environments/{identity}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mut handoff: ArtifactHandoff = serde_json::from_value(serde_json::json!({
            "version": 1, "nodes": {}, "portable_environments": [identity]
        })).unwrap();
        verify_artifacts(root, &handoff, &payload()).unwrap();
        std::fs::write(&path, b"changed environment").unwrap();
        assert!(verify_artifacts(root, &handoff, &payload()).unwrap_err().contains("evidence changed"));
        handoff.portable_environments = vec!["../escape".into()];
        assert!(verify_artifacts(root, &handoff, &payload()).unwrap_err().contains("invalid portable"));
        handoff.portable_environments = vec!["a".repeat(64); 129];
        assert!(verify_artifacts(root, &handoff, &payload()).unwrap_err().contains("exceeds limit"));
    }

    #[test]
    fn publication_paths_reject_aliases_and_escape() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["", "/tmp/file", "a/../b", "a/./b", "a//b", "a/", "a\\b"] {
            assert!(regular(tmp.path(), name).unwrap_err().contains("unsafe"), "{name}");
        }
    }
    #[test]
    fn publication_manifest_includes_all_surfaces_and_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap(); let repo = tmp.path();
        std::fs::create_dir(repo.join("Tablet")).unwrap();
        std::fs::create_dir(repo.join("reference")).unwrap();
        std::fs::write(repo.join("Tablet/A.lean"), "a").unwrap();
        std::fs::write(repo.join("reference/paper.tex"), "paper").unwrap();
        let manifest = source_manifest(repo).unwrap();
        assert_eq!(manifest.len(), 2);
        assert_eq!(manifest["Tablet/A.lean"], hash(b"a"));
        std::os::unix::fs::symlink("A.lean", repo.join("Tablet/Alias.lean")).unwrap();
        assert!(source_manifest(repo).unwrap_err().contains("non-regular"));
    }
}
