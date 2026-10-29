//! Storage-only prepared batch adapter. No worker authority or protocol delta
//! is reconstructed from current state: objects preserve the prepared values.
use crate::dispatch_coordinator::PreparedBatch;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::{Read, Write}, path::{Path, PathBuf}};

pub const FORMAT: &str = "trellis-prepared-batch/1";
pub const WORKER_FORMAT: &str = "trellis-prepared-worker/1";
pub const MAX_WORKER_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub repo_path: PathBuf,
    pub object_root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReference {
    pub format: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Serialize)]
pub struct StoredBatch {
    pub format: &'static str,
    pub event: crate::dispatch::DispatchEvent,
    pub receipt: crate::dispatch::DispatchReceipt,
    pub workers: BTreeMap<u64, WorkerReference>,
}

fn directory(root: &Path) -> Result<(), String> {
    if !root.is_absolute() { return Err("batch object root must be absolute".into()); }
    let mut current = PathBuf::new();
    for part in root.components() {
        match part {
            std::path::Component::RootDir | std::path::Component::Normal(_) => current.push(part),
            _ => return Err("invalid batch object root".into()),
        }
        if !current.exists() {
            fs::create_dir(&current).map_err(|e| e.to_string())?;
            fs::File::open(current.parent().unwrap()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        }
        let metadata = fs::symlink_metadata(&current).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("batch object root is not a regular directory".into());
        }
    }
    Ok(())
}

struct HashedWriter<W> { writer: W, hash: Sha256, bytes: u64 }
impl<W: Write> Write for HashedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.saturating_add(bytes.len() as u64) > MAX_WORKER_BYTES {
            return Err(std::io::Error::other("prepared worker exceeds storage bound"));
        }
        let count = self.writer.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> { self.writer.flush() }
}

fn verify(root: &Path, reference: &WorkerReference) -> Result<(), String> {
    if reference.format != WORKER_FORMAT || reference.bytes == 0 || reference.bytes > MAX_WORKER_BYTES
        || reference.sha256.len() != 64 || !reference.sha256.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return Err("invalid prepared worker reference".into());
    }
    let path = root.join(format!("{}.zip", reference.sha256));
    let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() { return Err("prepared worker object is not a regular file".into()); }
    let mut archive = zip::ZipArchive::new(fs::File::open(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    if archive.len() != 1 { return Err("prepared worker archive must have one entry".into()); }
    let mut entry = archive.by_index(0).map_err(|e| e.to_string())?;
    if entry.name() != "worker.json" || entry.is_dir() || entry.size() != reference.bytes {
        return Err("prepared worker archive has wrong entry or length".into());
    }
    let mut hash = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = entry.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 { break; }
        length += count as u64;
        if length > reference.bytes { return Err("prepared worker exceeds declared length".into()); }
        hash.update(&buffer[..count]);
    }
    if length != reference.bytes || format!("{:x}", hash.finalize()) != reference.sha256 {
        return Err("prepared worker object digest or length changed".into());
    }
    Ok(())
}

fn publish(root: &Path, worker: &crate::dispatch_contract::PreparedWorker) -> Result<WorkerReference, String> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let temp = root.join(format!(".{:032x}.tmp", u128::from_be_bytes(nonce)));
    let result = (|| {
        let file = fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipWriter::new(file);
        archive.start_file("worker.json", zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated).compression_level(Some(1)))
            .map_err(|e| e.to_string())?;
        let mut writer = HashedWriter { writer: &mut archive, hash: Sha256::new(), bytes: 0 };
        // No batch/worker Value or uncompressed serialization buffer. The
        // host verifies these exact bytes, then uses its usual canonical JSON
        // for private runtime files and preparation/receipt identities.
        {
            // JSON emits many tiny punctuation/key writes. Batch those before
            // hashing/deflation without buffering an entire worker.
            let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, &mut writer);
            serde_json::to_writer(&mut buffered, worker).map_err(|e| e.to_string())?;
            buffered.flush().map_err(|e| e.to_string())?;
        }
        let reference = WorkerReference { format: WORKER_FORMAT.into(),
            sha256: format!("{:x}", writer.hash.finalize()), bytes: writer.bytes };
        archive.finish().and_then(|f| f.sync_all().map_err(zip::result::ZipError::Io)).map_err(|e| e.to_string())?;
        let path = root.join(format!("{}.zip", reference.sha256));
        match fs::hard_link(&temp, &path) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => verify(root, &reference)?,
            Err(e) => return Err(e.to_string()),
        }
        fs::File::open(root).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        Ok(reference)
    })();
    let _ = fs::remove_file(temp);
    result
}

pub fn store(batch: PreparedBatch, storage: &Storage) -> Result<StoredBatch, String> {
    directory(&storage.object_root)?;
    let mut event = crate::engine::ProtocolEvent::Dispatch { payload: batch.event };
    // These objects already belong to native checkpoint/history recovery.
    // Pre-reservation event requests must not be replaced by bound workers.
    crate::dispatch_history::externalize_event(&storage.repo_path, &mut event)?;
    let crate::engine::ProtocolEvent::Dispatch { payload: event } = event else { unreachable!() };
    let mut workers = BTreeMap::new();
    for (id, worker) in batch.workers {
        workers.insert(id, publish(&storage.object_root, &worker)?);
    }
    Ok(StoredBatch { format: FORMAT, event, receipt: batch.receipt, workers })
}

#[cfg(test)]
pub(crate) fn read_for_test(root: &Path, reference: &WorkerReference) -> serde_json::Value {
    verify(root, reference).unwrap();
    let file = fs::File::open(root.join(format!("{}.zip", reference.sha256))).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    serde_json::from_reader(zip.by_index(0).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> crate::dispatch_contract::PreparedWorker {
        let mut state = crate::model::ProtocolState::default();
        let id = crate::runtime::test_support::add_resolved_dispatch_task(&mut state);
        let assignment = state.dispatch.as_ref().unwrap().tasks[&id].assignment.clone();
        state.worker_execution = assignment.request.inline().unwrap().task_execution.clone();
        state.in_flight_request = Some(Box::new(assignment.request.inline().unwrap().clone()));
        state.dispatch = None;
        crate::dispatch_contract::PreparedWorker { assignment, private_state: state }
    }

    #[test]
    fn prepared_batch_storage_streams_exact_worker_and_reuses_verified_object() {
        let root = tempfile::tempdir().unwrap();
        let worker = fixture();
        let reference = publish(root.path(), &worker).unwrap();
        assert_eq!(read_for_test(root.path(), &reference), serde_json::to_value(&worker).unwrap());
        assert_eq!(publish(root.path(), &worker).unwrap().sha256, reference.sha256);
        let compressed = fs::metadata(root.path().join(format!("{}.zip", reference.sha256))).unwrap().len();
        assert!(compressed < reference.bytes);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1, "temporary link removed");
        fs::write(root.path().join(format!("{}.zip", reference.sha256)), b"corrupt").unwrap();
        assert!(publish(root.path(), &worker).is_err(), "existing object must verify, never overwrite corruption");
    }

    #[test]
    fn prepared_batch_storage_rejects_aliases_lengths_and_wrong_kinds() {
        let root = tempfile::tempdir().unwrap();
        let reference = publish(root.path(), &fixture()).unwrap();
        let mut bad = reference.clone();
        bad.bytes += 1;
        assert!(verify(root.path(), &bad).is_err());
        bad = reference.clone(); bad.format = crate::dispatch_history::FORMAT.into();
        assert!(verify(root.path(), &bad).is_err());
        bad = reference.clone(); bad.bytes = MAX_WORKER_BYTES + 1;
        assert!(verify(root.path(), &bad).is_err());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        assert!(directory(&alias).is_err());
        let path = root.path().join(format!("{}.zip", reference.sha256));
        let retained = root.path().join("retained.zip");
        fs::rename(&path, &retained).unwrap();
        std::os::unix::fs::symlink(&retained, &path).unwrap();
        assert!(publish(root.path(), &fixture()).is_err());
        assert!(retained.is_file());
    }

    #[test]
    #[ignore = "bounded synthetic transport benchmark; needs explicit output directory"]
    fn prepared_batch_storage_benchmark_export() {
        let root = PathBuf::from(std::env::var("TRELLIS_BATCH_BENCH_ROOT").unwrap());
        let mode = std::env::var("TRELLIS_BATCH_BENCH_MODE").unwrap();
        let mut template = fixture();
        let fingerprints: BTreeMap<crate::NodeId, String> = (0..12000).map(|i| (
            format!("node_{i:05}").into(), format!("{:x}", Sha256::digest(format!("synthetic-record-{i}").as_bytes()))
        )).collect();
        template.private_state.live.corr_current_fingerprints = fingerprints.clone();
        template.private_state.committed = template.private_state.live.clone();
        template.private_state.last_clean_live = template.private_state.live.clone();
        let mut workers = BTreeMap::new();
        let mut items = Vec::new();
        for id in 1..=8 {
            let mut worker = template.clone();
            let request = worker.assignment.request.make_mut().unwrap();
            request.current_paper_approved_fingerprints = fingerprints.iter()
                .map(|(node, hash)| (node.to_string().into(), hash.clone())).collect();
            request.task_execution.as_mut().unwrap().task_id = id;
            worker.private_state.worker_execution = request.task_execution.clone();
            worker.private_state.in_flight_request = Some(Box::new(request.clone()));
            items.push(crate::dispatch::PreparedItem::Ready { assignment: worker.assignment.clone() });
            workers.insert(id, worker);
        }
        let batch = PreparedBatch {
            event: crate::dispatch::DispatchEvent::BatchPrepared { review_id: 1,
                response_sha256: "a".repeat(64), items,
                graph: crate::dispatch::DependencyGraph { edges: BTreeMap::new() }, now_ms: 1 },
            receipt: crate::dispatch::DispatchReceipt { review_id: 1, response_sha256: "a".repeat(64), decisions: vec![] },
            workers,
        };
        let output = fs::File::create(root.join("output.json")).unwrap();
        match mode.as_str() {
            "inline" => serde_json::to_writer(output, &serde_json::to_value(batch).unwrap()).unwrap(),
            "stored" => {
                let stored = store(batch, &Storage { repo_path: root.clone(), object_root: root.join("workers") }).unwrap();
                serde_json::to_writer(output, &serde_json::to_value(stored).unwrap()).unwrap();
            },
            _ => panic!("unknown benchmark mode"),
        }
    }

}
