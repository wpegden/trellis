//! Exact frozen dispatch requests, shared by terminal state and durable history.
//! References are never worker authority: callers must resolve and verify the
//! complete object before reading a historical request or restoring an active one.
use crate::dispatch::{TaskExecutionContext, TaskStatus};
use crate::model::{ProtocolState, WrapperRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const FORMAT: &str = "trellis-dispatch-request/1";
pub const DIRECTORY: &str = ".trellis-history/dispatch-requests";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestReference {
    #[serde(rename = "$dispatch_request")]
    pub format: String,
    pub sha256: String,
    pub bytes: u64,
    pub task_execution: Option<TaskExecutionContext>,
}

impl RequestReference {
    pub fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT || self.bytes == 0 || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("invalid frozen dispatch request reference".into());
        }
        Ok(())
    }

    pub fn from_request(request: &WrapperRequest) -> Result<(Self, Vec<u8>), String> {
        // Sorting JSON object keys gives the existing serde Value encoding a
        // single stable representation, independent of struct field order.
        let value = serde_json::to_value(request).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        Ok((Self { format: FORMAT.into(), sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64, task_execution: request.task_execution.clone() }, bytes))
    }

    pub fn verify(&self, bytes: &[u8]) -> Result<Arc<WrapperRequest>, String> {
        self.validate()?;
        if bytes.len() as u64 != self.bytes || format!("{:x}", Sha256::digest(bytes)) != self.sha256 {
            return Err("frozen dispatch request object has wrong size or digest".into());
        }
        // The producer binds the exact bytes. Never reserialize to verify:
        // later defaulted fields must not rewrite or invalidate old evidence.
        let request: WrapperRequest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if request.task_execution != self.task_execution {
            return Err("frozen dispatch request object has wrong execution binding".into());
        }
        Ok(Arc::new(request))
    }
}

/// An ephemeral proof that exact stored bytes decoded to this complete request.
/// Private fields prevent an external JSON caller from asserting verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRequest {
    reference: RequestReference,
    request: Arc<WrapperRequest>,
}
impl VerifiedRequest {
    pub(crate) fn matches(&self, reference: &RequestReference, request: &WrapperRequest) -> bool {
        &self.reference == reference && self.request.as_ref() == request
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrozenRequest {
    Inline(Arc<WrapperRequest>),
    Archived(RequestReference),
    /// Storage-only indirection; must expand before any protocol transition.
    Stored(RequestReference),
}

impl From<Arc<WrapperRequest>> for FrozenRequest {
    fn from(value: Arc<WrapperRequest>) -> Self { Self::Inline(value) }
}
impl From<Box<WrapperRequest>> for FrozenRequest {
    fn from(value: Box<WrapperRequest>) -> Self { Self::Inline(value.into()) }
}
impl FrozenRequest {
    pub fn inline(&self) -> Result<&WrapperRequest, String> {
        match self { Self::Inline(request) => Ok(request), Self::Archived(_) | Self::Stored(_) =>
            Err("historical dispatch request requires verified object resolution".into()) }
    }
    pub fn inline_arc(&self) -> Result<&Arc<WrapperRequest>, String> {
        match self { Self::Inline(request) => Ok(request), Self::Archived(_) | Self::Stored(_) =>
            Err("historical dispatch request requires verified object resolution".into()) }
    }
    pub fn make_mut(&mut self) -> Result<&mut WrapperRequest, String> {
        match self { Self::Inline(request) => Ok(Arc::make_mut(request)), Self::Archived(_) | Self::Stored(_) =>
            Err("cannot bind an unresolved historical dispatch request".into()) }
    }
    pub fn execution(&self) -> Option<&TaskExecutionContext> {
        match self { Self::Inline(request) => request.task_execution.as_ref(),
            Self::Archived(reference) | Self::Stored(reference) => reference.task_execution.as_ref() }
    }
}
impl Serialize for FrozenRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self { Self::Inline(value) => value.serialize(serializer),
            Self::Archived(value) => value.serialize(serializer),
            Self::Stored(value) => serde_json::json!({"$stored_dispatch_request": value}).serialize(serializer) }
    }
}
impl<'de> Deserialize<'de> for FrozenRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(stored) = value.get("$stored_dispatch_request") {
            if value.as_object().is_none_or(|m| m.len() != 1) { return Err(serde::de::Error::custom("malformed stored dispatch request")); }
            let reference: RequestReference = serde_json::from_value(stored.clone()).map_err(serde::de::Error::custom)?;
            reference.validate().map_err(serde::de::Error::custom)?;
            Ok(Self::Stored(reference))
        } else if value.get("$dispatch_request").is_some() {
            let reference: RequestReference = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            reference.validate().map_err(serde::de::Error::custom)?;
            Ok(Self::Archived(reference))
        } else {
            serde_json::from_value(value).map(|r| Self::Inline(Arc::new(r))).map_err(serde::de::Error::custom)
        }
    }
}

fn directory(repo: &Path, create: bool) -> Result<PathBuf, String> {
    let mut path = repo.to_path_buf();
    for part in [".trellis-history", "dispatch-requests"] {
        path.push(part);
        if create {
            match std::fs::create_dir(&path) {
                Ok(()) => { std::fs::File::open(path.parent().unwrap()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?; },
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {},
                Err(e) => return Err(e.to_string()),
            }
        }
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("dispatch history {}: {e}", path.display()))?;
        if !meta.is_dir() || meta.file_type().is_symlink() { return Err("dispatch history directory is not a regular directory".into()); }
    }
    Ok(path)
}

pub fn read(repo: &Path, reference: &RequestReference) -> Result<Arc<WrapperRequest>, String> {
    reference.validate()?;
    let path = directory(repo, false)?.join(format!("{}.zip", reference.sha256));
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("dispatch history {}: {e}", path.display()))?;
    if !meta.is_file() || meta.file_type().is_symlink() { return Err("dispatch request object is not a regular file".into()); }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    decode_archive(file, reference).map_err(|e| format!("dispatch request {}: {e}", reference.sha256))
}

fn decode_archive(reader: impl std::io::Read + std::io::Seek, reference: &RequestReference) -> Result<Arc<WrapperRequest>, String> {
    reference.verify(&archive_payload(reader, reference)?)
}

fn archive_payload(reader: impl std::io::Read + std::io::Seek, reference: &RequestReference) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
    if archive.len() != 1 { return Err("dispatch request archive must have exactly one entry".into()); }
    let mut entry = archive.by_index(0).map_err(|e| e.to_string())?;
    if entry.name() != "request.json" || entry.is_dir() || entry.size() != reference.bytes {
        return Err("dispatch request archive has wrong entry name or length".into());
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    (&mut entry).take(reference.bytes.checked_add(1).ok_or("dispatch request length overflow")?)
        .read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

pub fn publish(repo: &Path, request: &WrapperRequest) -> Result<RequestReference, String> {
    let (reference, bytes) = RequestReference::from_request(request)?;
    let dir = directory(repo, true)?;
    let path = dir.join(format!("{}.zip", reference.sha256));
    if path.symlink_metadata().is_ok() { read(repo, &reference)?; return Ok(reference); }
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer.start_file("request.json", zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated).compression_level(Some(6)))
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    writer.write_all(&bytes).map_err(|e| e.to_string())?;
    let archive = writer.finish().map_err(|e| e.to_string())?.into_inner();
    write_archive(repo, &reference, &archive)?;
    Ok(reference)
}

/// Rewind retains compressed bytes, never another fully expanded task ledger.
pub fn retain_archive(repo: &Path, reference: &RequestReference) -> Result<Vec<u8>, String> {
    reference.validate()?;
    let path = directory(repo, false)?.join(format!("{}.zip", reference.sha256));
    let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() { return Err("dispatch request object is not a regular file".into()); }
    let archive = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_archive(std::io::Cursor::new(&archive), reference).map_err(|e| format!("dispatch request {}: {e}", reference.sha256))?;
    Ok(archive)
}

pub fn publish_archive(repo: &Path, reference: &RequestReference, archive: &[u8]) -> Result<(), String> {
    reference.validate()?;
    decode_archive(std::io::Cursor::new(archive), reference)?;
    write_archive(repo, reference, archive)
}

fn write_archive(repo: &Path, reference: &RequestReference, archive: &[u8]) -> Result<(), String> {
    let dir = directory(repo, true)?;
    let path = dir.join(format!("{}.zip", reference.sha256));
    if path.symlink_metadata().is_ok() { read(repo, reference)?; return Ok(()); }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let temp = dir.join(format!(".{}-{:032x}.tmp", reference.sha256, u128::from_be_bytes(nonce)));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e| e.to_string())?;
    let result = (|| {
        file.write_all(archive).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
        // A hard link publishes atomically without replacing an existing object.
        match std::fs::hard_link(&temp, &path) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => { read(repo, reference)?; },
            Err(e) => return Err(e.to_string()),
        }
        std::fs::File::open(&dir).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temp);
    result
}

/// Checkpoint files may reference active requests to avoid repeating the same
/// bytes at every checkpoint. Hydration precedes ordinary ledger validation.
pub fn hydrate_active(repo: &Path, state: &mut ProtocolState) -> Result<(), String> {
    if let Some(ledger) = &mut state.dispatch {
        for task in ledger.tasks.values_mut() {
            if matches!(task.assignment.request, FrozenRequest::Stored(_)) {
                hydrate(repo, &mut task.assignment.request)?;
            }
        }
    }
    Ok(())
}

pub fn hydrate(repo: &Path, request: &mut FrozenRequest) -> Result<(), String> {
    if let FrozenRequest::Archived(reference) | FrozenRequest::Stored(reference) = request {
        *request = FrozenRequest::Inline(read(repo, reference)?);
    }
    Ok(())
}

/// Only dispatch assignment carriers are externalized. Mathematical response
/// bytes and human trust-decision records retain their existing encodings.
pub fn event_requests(event: &mut crate::engine::ProtocolEvent,
    mut visit: impl FnMut(&mut FrozenRequest) -> Result<(), String>) -> Result<(), String> {
    use crate::dispatch::{DispatchEvent, PreparedItem};
    if let crate::engine::ProtocolEvent::Dispatch { payload } = event {
        match payload {
            DispatchEvent::AssignmentsDispatched { assignments, .. } => {
                for assignment in assignments { visit(&mut assignment.request)?; }
            },
            DispatchEvent::BatchPrepared { items, .. } => {
                for item in items { if let PreparedItem::Ready { assignment } = item { visit(&mut assignment.request)?; } }
            },
            DispatchEvent::RetryPrepared { assignment: Some(assignment), .. } => visit(&mut assignment.request)?,
            _ => {},
        }
    }
    Ok(())
}

pub fn externalize_event(repo: &Path, event: &mut crate::engine::ProtocolEvent) -> Result<(), String> {
    event_requests(event, |request| {
        match request {
            FrozenRequest::Inline(inline) => *request = FrozenRequest::Stored(publish(repo, inline)?),
            FrozenRequest::Archived(reference) | FrozenRequest::Stored(reference) => { read(repo, reference)?; },
        }
        Ok(())
    })
}

pub fn hydrate_event(repo: &Path, event: &mut crate::engine::ProtocolEvent) -> Result<(), String> {
    event_requests(event, |request| hydrate(repo, request))
}

pub fn verify_state(repo: &Path, state: &ProtocolState) -> Result<(), String> {
    crate::coarse_audits::verify_history_with(state, |r| crate::coarse_audits::read(repo, r))?;
    if let Some(ledger) = &state.dispatch {
        ledger.validate()?;
        for task in ledger.tasks.values() {
            if let FrozenRequest::Archived(reference) = &task.assignment.request { read(repo, reference)?; }
        }
    }
    Ok(())
}

pub fn archive_event(repo: &Path, state: &ProtocolState) -> Result<Option<crate::dispatch::DispatchEvent>, String> {
    if state.pv_revision_ready() || state.phase == crate::Phase::Complete || state.stage == crate::Stage::Complete {
        return Ok(None);
    }
    let mut requests = std::collections::BTreeMap::new();
    if let Some(ledger) = &state.dispatch {
        for (id, task) in &ledger.tasks {
            if matches!(task.status, TaskStatus::Resolved { .. }) && task.pending_retry.is_none() {
                if let FrozenRequest::Inline(request) = &task.assignment.request {
                    requests.insert(*id, publish(repo, request)?);
                }
            }
        }
    }
    Ok((!requests.is_empty()).then_some(crate::dispatch::DispatchEvent::RequestsArchived { requests, verified: Default::default() }))
}

/// Recovery reads the committed object tree, never an uncommitted worktree copy.
fn git_archive(repo: &Path, revision: &str, reference: &RequestReference) -> Result<Vec<u8>, String> {
    reference.validate()?;
    let object = format!("{revision}:{DIRECTORY}/{}.zip", reference.sha256);
    let result = std::process::Command::new("git").arg("-C").arg(repo)
        .args(["cat-file", "blob", &object]).output().map_err(|e| format!("cannot read dispatch request {object}: {e}"))?;
    if !result.status.success() {
        return Err(format!("cannot read committed dispatch request {object}: exit {:?}: {}",
            result.status.code(), String::from_utf8_lossy(&result.stderr).chars().take(500).collect::<String>()));
    }
    Ok(result.stdout)
}

pub fn read_git(repo: &Path, revision: &str, reference: &RequestReference) -> Result<Arc<WrapperRequest>, String> {
    decode_archive(std::io::Cursor::new(git_archive(repo, revision, reference)?), reference)
        .map_err(|e| format!("dispatch request {} at {revision}: {e}", reference.sha256))
}

pub fn read_git_value(repo: &Path, revision: &str, reference: &RequestReference) -> Result<serde_json::Value, String> {
    let raw = archive_payload(std::io::Cursor::new(git_archive(repo, revision, reference)?), reference)
        .map_err(|e| format!("dispatch request {} at {revision}: {e}", reference.sha256))?;
    reference.verify(&raw).map_err(|e| format!("dispatch request {} at {revision}: {e}", reference.sha256))?;
    serde_json::from_slice(&raw).map_err(|e| e.to_string())
}

pub fn hydrate_checkpoint_with(state: &mut ProtocolState,
    mut resolve: impl FnMut(&RequestReference) -> Result<Arc<WrapperRequest>, String>) -> Result<(), String> {
    if let Some(ledger) = &mut state.dispatch {
        for task in ledger.tasks.values_mut() {
            match &task.assignment.request {
                FrozenRequest::Stored(reference) => task.assignment.request = FrozenRequest::Inline(resolve(reference)?),
                FrozenRequest::Archived(reference) => { resolve(reference)?; },
                FrozenRequest::Inline(_) => {},
            }
        }
        ledger.validate()?;
    }
    Ok(())
}

/// Find the object tree accompanying a portable state/checkpoint file. A bare
/// state is exported alongside `.trellis-history`, never with absolute paths.
pub fn root_for_state(path: &Path) -> Result<PathBuf, String> {
    let parent = path.parent().ok_or("state file has no parent directory")?;
    if parent.file_name().is_some_and(|n| n == ".trellis-history") {
        return parent.parent().map(Path::to_path_buf).ok_or("history file has no repository parent".into());
    }
    // Campaign exports share a single pool across generation directories.
    // The bundle manifest is the explicit root marker; do not search arbitrary
    // ancestors for unrelated ambient request stores.
    for ancestor in parent.ancestors() {
        if ancestor.join("campaign.json").is_file() && ancestor.join(DIRECTORY).is_dir() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(parent.to_path_buf())
}

pub fn export_state_objects(source: &Path, destination: &Path, state: &ProtocolState) -> Result<(), String> {
    crate::coarse_audits::export_history(source, destination, state)?;
    if let Some(ledger) = &state.dispatch {
        for task in ledger.tasks.values() {
            if let FrozenRequest::Archived(reference) = &task.assignment.request {
                let archive = retain_archive(source, reference)?;
                publish_archive(destination, reference, &archive)?;
            }
        }
    }
    Ok(())
}

/// Expand storage indirection without rewriting any other historical field.
/// Protocol archive references remain references and are fully verified here.
pub fn hydrate_document(document: &mut serde_json::Value,
    mut resolve: impl FnMut(&RequestReference) -> Result<serde_json::Value, String>) -> Result<(), String> {
    let state = if document.get("state").is_some() { &mut document["state"] } else { document };
    if let Some(tasks) = state.pointer_mut("/dispatch/tasks").and_then(serde_json::Value::as_object_mut) {
        for task in tasks.values_mut() {
            let request = &mut task["assignment"]["request"];
            if request.get("$stored_dispatch_request").is_some() || request.get("$dispatch_request").is_some() {
                match serde_json::from_value::<FrozenRequest>(request.clone()).map_err(|e| e.to_string())? {
                    FrozenRequest::Stored(reference) => *request = resolve(&reference)?,
                    FrozenRequest::Archived(reference) => { resolve(&reference)?; },
                    FrozenRequest::Inline(_) => unreachable!(),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> WrapperRequest {
        let mut request = WrapperRequest::default();
        request.reviewer_comments = "exact frozen evidence λ\n".repeat(500);
        request.worker_contract = serde_json::json!({"small_float": 1e-7, "unicode": "𝒜", "negative": -7});
        request
    }

    #[test]
    fn request_archive_exact_roundtrip_dedup_and_python_reader() {
        let repo = tempfile::tempdir().unwrap();
        let request = request();
        let reference = publish(repo.path(), &request).unwrap();
        let path = repo.path().join(DIRECTORY).join(format!("{}.zip", reference.sha256));
        let original = std::fs::read(&path).unwrap();
        assert!(original.len() < reference.bytes as usize / 2);
        assert_eq!(*read(repo.path(), &reference).unwrap(), request);
        assert_eq!(publish(repo.path(), &request).unwrap(), reference);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let script = "import json,sys; from pathlib import Path; from trellis.dispatch.history import resolve; r=json.loads(sys.argv[2]); q=resolve(Path(sys.argv[1]),r); assert q['worker_contract']['small_float']==1e-7";
        assert!(std::process::Command::new("python3").current_dir(root)
            .args(["-c", script]).arg(repo.path()).arg(serde_json::to_string(&reference).unwrap())
            .status().unwrap().success());
    }

    #[test]
    fn request_archive_rejects_corruption_wrong_binding_shape_and_symlinks() {
        let repo = tempfile::tempdir().unwrap();
        let request = request();
        let reference = publish(repo.path(), &request).unwrap();
        let path = repo.path().join(DIRECTORY).join(format!("{}.zip", reference.sha256));
        let original = std::fs::read(&path).unwrap();
        let mut wrong = reference.clone();
        wrong.bytes += 1;
        assert!(read(repo.path(), &wrong).is_err());
        wrong = reference.clone();
        wrong.sha256 = "../outside".into();
        assert!(read(repo.path(), &wrong).is_err());
        let (_, raw) = RequestReference::from_request(&request).unwrap();
        let mut changed = request.clone();
        changed.reviewer_comments.push('!');
        assert!(reference.verify(&RequestReference::from_request(&changed).unwrap().1).is_err());
        std::fs::write(&path, b"corrupt ZIP").unwrap();
        assert!(read(repo.path(), &reference).is_err());
        assert!(publish(repo.path(), &request).is_err(), "never overwrite corrupt immutable objects");
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("other.json", zip::write::SimpleFileOptions::default()).unwrap();
        use std::io::Write;
        zip.write_all(&raw).unwrap();
        std::fs::write(&path, zip.finish().unwrap().into_inner()).unwrap();
        assert!(read(repo.path(), &reference).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(read(repo.path(), &reference).is_err());
        let outside = repo.path().join("outside.zip");
        std::fs::write(&outside, original).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(read(repo.path(), &reference).is_err());
        assert!(publish(repo.path(), &request).is_err());
    }

    #[test]
    fn request_archive_storage_reference_is_not_protocol_authority() {
        let repo = tempfile::tempdir().unwrap();
        let request = request();
        let reference = publish(repo.path(), &request).unwrap();
        let inline = FrozenRequest::Inline(Arc::new(request.clone()));
        assert_eq!(serde_json::to_vec(&inline).unwrap(), serde_json::to_vec(&request).unwrap());
        for value in [FrozenRequest::Stored(reference.clone()), FrozenRequest::Archived(reference)] {
            let mut restored: FrozenRequest = serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(restored, value);
            assert!(restored.inline().is_err());
            assert!(restored.make_mut().is_err());
            hydrate(repo.path(), &mut restored).unwrap();
            assert_eq!(restored.inline().unwrap(), &request);
        }
        assert!(serde_json::from_value::<FrozenRequest>(serde_json::json!({"$dispatch_request":"unknown"})).is_err());
        assert!(serde_json::from_value::<FrozenRequest>(serde_json::json!({"$stored_dispatch_request":null,"extra":true})).is_err());
    }

    #[test]
    fn legacy_raw_request_bytes_replay_export_and_boundary_digest_survive_added_defaults() {
        let repo = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut state = ProtocolState::default();
        let id = crate::runtime::test_support::add_resolved_dispatch_task(&mut state);
        let request = state.dispatch.as_ref().unwrap().tasks[&id].assignment.request.inline().unwrap();
        let mut legacy = serde_json::to_value(request).unwrap();
        // This defaulted field did not exist in the historical schema.
        legacy.as_object_mut().unwrap().remove("active_node");
        let raw = serde_json::to_vec(&legacy).unwrap();
        let reference = RequestReference { format: FORMAT.into(), sha256: format!("{:x}", Sha256::digest(&raw)),
            bytes: raw.len() as u64, task_execution: request.task_execution.clone() };
        assert_eq!(reference.verify(&raw).unwrap().as_ref(), request);
        assert_ne!(RequestReference::from_request(request).unwrap().0, reference);
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("request.json", zip::write::SimpleFileOptions::default()).unwrap();
        use std::io::Write;
        zip.write_all(&raw).unwrap();
        let archive = zip.finish().unwrap().into_inner();
        publish_archive(repo.path(), &reference, &archive).unwrap();
        let mut event = crate::ProtocolEvent::Dispatch { payload: crate::dispatch::DispatchEvent::RequestsArchived {
            requests: std::collections::BTreeMap::from([(id, reference.clone())]), verified: Default::default(),
        }};
        // A JSON caller cannot forge the private verified carrier.
        let mut forged = serde_json::to_value(&event).unwrap();
        forged["payload"]["verified"] = serde_json::json!({"1": {"reference": reference, "request": legacy}});
        let forged: crate::ProtocolEvent = serde_json::from_value(forged).unwrap();
        assert!(crate::apply_event(state.clone(), forged).is_err());
        verify_archive_event(&state, &mut event, |_| Ok(archive.clone())).unwrap();
        let after = crate::apply_event(state, event).unwrap().state;
        export_state_objects(repo.path(), destination.path(), &after).unwrap();
        assert_eq!(retain_archive(destination.path(), &reference).unwrap(), archive);
        let mut document = serde_json::json!({"state": {"dispatch": {"tasks": {
            "1": {"assignment": {"request": FrozenRequest::Stored(reference.clone())}}
        }}}});
        let expected = serde_json::json!({"state": {"dispatch": {"tasks": {
            "1": {"assignment": {"request": legacy}}
        }}}});
        // The trusted-boundary hash sees the original absent fields, not the
        // current typed request's expanded defaults.
        hydrate_document(&mut document, |r| { r.verify(&raw)?; Ok(serde_json::from_slice(&raw).unwrap()) }).unwrap();
        assert_eq!(serde_json::to_vec(&document).unwrap(), serde_json::to_vec(&expected).unwrap());
    }

    #[test]
    fn archive_maintenance_refuses_frozen_terminal_and_pv_boundaries_before_writes() {
        for frozen in 0..3 {
            let repo = tempfile::tempdir().unwrap();
            let mut state = ProtocolState::default();
            crate::runtime::test_support::add_resolved_dispatch_task(&mut state);
            let event = archive_event(repo.path(), &state).unwrap().unwrap();
            std::fs::remove_dir_all(repo.path().join(".trellis-history")).unwrap();
            match frozen {
                0 => state.phase = crate::Phase::Complete,
                1 => state.stage = crate::Stage::Complete,
                _ => state.pv_revision_handoff = Some(crate::model::PvRevisionHandoff {
                    schema_version: 1, cycle: 1, refuted_goals: vec![],
                }),
            }
            assert!(archive_event(repo.path(), &state).unwrap().is_none());
            assert!(!repo.path().join(DIRECTORY).exists());
            assert!(crate::apply_event(state, crate::ProtocolEvent::Dispatch { payload: event }).is_err());
        }
    }

    #[test]
    fn request_archive_interrupted_publication_leaves_only_unreferenced_evidence() {
        let repo = tempfile::tempdir().unwrap();
        let request = request();
        // A stop after durable object publication, before the archive event,
        // must neither erase the inline contract nor prevent an exact retry.
        let before = FrozenRequest::Inline(Arc::new(request.clone()));
        let reference = publish(repo.path(), before.inline().unwrap()).unwrap();
        assert_eq!(before.inline().unwrap(), &request);
        assert_eq!(publish(repo.path(), before.inline().unwrap()).unwrap(), reference);
        // Orphan temporary files from earlier interrupted writes cannot block
        // future publication, and are never treated as an object reference.
        std::fs::write(repo.path().join(DIRECTORY).join(".orphan.tmp"), b"partial").unwrap();
        let mut next = request;
        next.id += 1;
        let next_reference = publish(repo.path(), &next).unwrap();
        assert_eq!(*read(repo.path(), &next_reference).unwrap(), next);
    }
}

/// The pure transition needs the exact historical request even if a newer
/// WrapperRequest schema adds defaults. Verify raw bytes once at the boundary,
/// then share the matching existing inline Arc rather than duplicating it.
pub fn verify_archive_event(state: &ProtocolState, event: &mut crate::ProtocolEvent,
    mut object: impl FnMut(&RequestReference) -> Result<Vec<u8>, String>) -> Result<(), String> {
    if let crate::ProtocolEvent::Dispatch { payload: crate::dispatch::DispatchEvent::RequestsArchived { requests, verified } } = event {
        verified.clear();
        for (id, reference) in requests {
            reference.validate()?;
            let archive = object(reference)?;
            let decoded = decode_archive(std::io::Cursor::new(archive), reference)
                .map_err(|e| format!("dispatch request {}: {e}", reference.sha256))?;
            let task = state.dispatch.as_ref().and_then(|l| l.tasks.get(id)).ok_or("unknown archived task")?;
            if let FrozenRequest::Inline(request) = &task.assignment.request {
                if request.as_ref() != decoded.as_ref() { return Err("stored object differs from the complete frozen request".into()); }
                verified.insert(*id, VerifiedRequest { reference: reference.clone(), request: request.clone() });
            }
        }
    }
    Ok(())
}
