//! Recoverable publication of the state, maintenance event and checkpoint cache.
//! Immutable artifact epochs are retained before the commit decision. Their
//! record-set keys become reachable only with the matching published state.
use crate::closure_identity_transaction::{durable_write, RepairOwnership};
use crate::{
    runtime::*, trusted_artifact_rebind::TrustedArtifactRebind, ProtocolEvent, ProtocolState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

const DIR: &str = "trusted-artifact-rebind-publication";
#[derive(Serialize, Deserialize)]
struct Entry {
    destination: PathBuf,
    before: Option<String>,
    after: String,
    staged: String,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    input_generation: String,
    entries: Vec<Entry>,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_hash(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(hash(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn completed(root: &Path) -> bool {
    root.join(DIR).join("complete.json").is_file()
}

pub fn publish(
    root: &Path,
    original: &[u8],
    after: &ProtocolState,
    payload: TrustedArtifactRebind,
) -> Result<(), String> {
    let paths = RuntimePaths::new(root);
    if fs::read(&paths.state_path).map_err(|e| e.to_string())? != original {
        return Err("trusted rebind state input changed; nothing published".into());
    }
    let mut before: ProtocolState = serde_json::from_slice(original).map_err(|e| e.to_string())?;
    crate::model::recompute_local_closure_reverse_indices(&mut before);
    if payload.apply(&before)? != *after {
        return Err("maintenance replay disagrees with candidate".into());
    }
    let metadata: RuntimeMetadata =
        serde_json::from_slice(&fs::read(&paths.metadata_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let log_dir = event_log_dir_for(root, &metadata);
    let files = event_log_cycle_files(&log_dir).map_err(|e| e.to_string())?;
    let mut count = 0;
    let mut last_cycle = 0;
    let mut prefix = Sha256::new();
    for path in files {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err("event prefix has an incomplete final line".into());
        }
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            let record: EventLogRecord = serde_json::from_slice(line).map_err(|e| e.to_string())?;
            if record.index != count {
                return Err(format!("event prefix is not dense at index {count}"));
            }
            last_cycle = record.cycle;
            count += 1;
        }
        prefix.update(&bytes);
    }
    if last_cycle > after.cycle {
        return Err("maintenance event would reorder historical cycle files".into());
    }
    let repo = metadata.repo_path.as_ref().ok_or("trusted rebind needs a checkpoint repository")?;
    let boundary = crate::trusted_checkpoint_boundary::TrustedCheckpointBoundary::prepare(repo, &before, count, &format!("{:x}", prefix.finalize()))?;
    // The old unmarked checkpoint count is the omitted response's index.
    // Reserve that slot for explicit snapshot adoption; never reuse it for
    // artifact migration or manufacture a provider response.
    let boundary_record = boundary.map(|payload| EventLogRecord {
        index: count, event: ProtocolEvent::TrustedCheckpointBoundary { payload },
        commands: vec![], phase: before.phase, stage: before.stage, cycle: before.cycle,
        ts_ms: 0, trust_record: None, additional_trust_records: vec![],
    });
    if boundary_record.is_some() { count += 1; }
    let record = EventLogRecord {
        index: count,
        event: ProtocolEvent::TrustedArtifactRebind {
            payload: payload.clone(),
        },
        commands: vec![],
        phase: after.phase,
        stage: after.stage,
        cycle: after.cycle,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    let log_path = event_log_cycle_file(&log_dir, after.cycle);
    let mut log_bytes = if log_path.exists() {
        fs::read(&log_path).map_err(|e| e.to_string())?
    } else {
        vec![]
    };
    if let Some(record) = boundary_record {
        log_bytes.extend(serde_json::to_vec(&record).map_err(|e| e.to_string())?);
        log_bytes.push(b'\n');
    }
    log_bytes.extend(serde_json::to_vec(&record).map_err(|e| e.to_string())?);
    log_bytes.push(b'\n');
    let previous: RuntimeCheckpoint =
        serde_json::from_slice(&fs::read(&paths.checkpoint_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let checkpoint = RuntimeCheckpoint {
        cycle: after.cycle,
        phase: after.phase,
        gate_kind: after.gate_kind,
        active_node: after.active_node.clone(),
        committed: after.committed.clone(),
        cleanup_unreachable_deletion: previous.cleanup_unreachable_deletion.clone(),
    };
    let mut outputs = vec![
        (
            paths.state_path,
            serde_json::to_vec_pretty(after).map_err(|e| e.to_string())?,
        ),
        (
            paths.checkpoint_path,
            serde_json::to_vec_pretty(&checkpoint).map_err(|e| e.to_string())?,
        ),
        (log_path, log_bytes),
    ];
    if let Some(repo) = metadata.repo_path.as_ref() {
        // Plain and shared canonical snapshots have the same decoded schema.
        // The next ordinary Python checkpoint may compact this document again.
        let document = serde_json::json!({"event_count": count + 1, "event_count_convention": "record_count", "state": after,
            "checkpoint": checkpoint, "metadata": metadata, "commands": []});
        outputs.push((
            repo.join(".trellis-history/supervisor_state.json"),
            serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())?,
        ));
        let seed = serde_json::json!({"event_count": count, "event_count_convention": "record_count", "state": before,
            "checkpoint": previous, "metadata": metadata, "commands": []});
        outputs.push((
            root.join("trusted-rebind-seed.json"),
            serde_json::to_vec_pretty(&seed).map_err(|e| e.to_string())?,
        ));
    }
    for (node, record) in &after.local_closure_records {
        if node.as_str().is_empty()
            || !node
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err("invalid mirror owner".into());
        }
        outputs.push((
            root.join("checker-state/local-closure-records")
                .join(format!("{node}.json")),
            serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?,
        ));
    }
    let dir = root.join(DIR);
    if dir.join("commit.json").exists() {
        return Err("recover pending trusted rebind before publishing".into());
    }
    let mut journal = Journal {
        version: 1,
        input_generation: payload.input_generation,
        entries: vec![],
    };
    for (index, (destination, bytes)) in outputs.into_iter().enumerate() {
        let staged = format!("{index}.json");
        durable_write(&dir.join(&staged), &bytes)?;
        journal.entries.push(Entry {
            before: file_hash(&destination)?,
            destination,
            after: hash(&bytes),
            staged,
        });
    }
    durable_write(
        &dir.join("commit.json"),
        &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
    )?;
    fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    recover_with_hook(root, |_| Ok(()))
}

pub fn recover(root: &Path) -> Result<(), String> {
    if !root.join(DIR).join("commit.json").exists() {
        return Ok(());
    }
    let _lock = RepairOwnership::acquire(root)?;
    recover_with_hook(root, |_| Ok(()))
}

fn recover_with_hook(
    root: &Path,
    mut after_write: impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    let dir = root.join(DIR);
    let journal_bytes = fs::read(dir.join("commit.json")).map_err(|e| e.to_string())?;
    let journal: Journal = serde_json::from_slice(&journal_bytes).map_err(|e| e.to_string())?;
    if journal.version != 1 {
        return Err("unsupported trusted rebind publication version".into());
    }
    // Check the *whole* committed generation before finishing any carrier.
    for entry in &journal.entries {
        let current = file_hash(&entry.destination)?;
        if current != entry.before && current.as_ref() != Some(&entry.after) {
            return Err(format!(
                "trusted rebind publication conflicts with writer: {}",
                entry.destination.display()
            ));
        }
        if hash(&fs::read(dir.join(&entry.staged)).map_err(|e| e.to_string())?) != entry.after {
            return Err("corrupt trusted rebind publication candidate".into());
        }
    }
    for (index, entry) in journal.entries.iter().enumerate() {
        durable_write(
            &entry.destination,
            &fs::read(dir.join(&entry.staged)).map_err(|e| e.to_string())?,
        )?;
        after_write(index)?;
    }
    durable_write(&dir.join("complete.json"), &journal_bytes)?;
    fs::remove_file(dir.join("commit.json")).map_err(|e| e.to_string())?;
    fs::File::open(&dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_appends_replay_and_publishes_checkpoint_without_advancing_counters() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let mut state = ProtocolState::default();
        state.corr_fingerprint_schema_version = 4;
        state.cycle = 3;
        let original = serde_json::to_vec_pretty(&state).unwrap();
        let metadata = RuntimeMetadata {
            repo_path: Some(repo.clone()),
            ..Default::default()
        };
        let checkpoint = RuntimeCheckpoint {
            cycle: state.cycle,
            phase: state.phase,
            gate_kind: state.gate_kind,
            active_node: None,
            committed: state.committed.clone(),
            cleanup_unreachable_deletion: None,
        };
        fs::write(root.join("protocol_state.json"), &original).unwrap();
        fs::write(
            root.join("runtime_metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("checkpoint.json"),
            serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();
        let log = event_log_cycle_file(&event_log_dir_for(&root, &metadata), 3);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        let old_event = EventLogRecord {
            index: 0,
            event: ProtocolEvent::StartCycle,
            commands: vec![],
            cycle: 3,
            phase: state.phase,
            stage: state.stage,
            ts_ms: 99,
            trust_record: None,
            additional_trust_records: vec![],
        };
        let prefix = format!("  {}\n", serde_json::to_string(&old_event).unwrap());
        fs::write(&log, &prefix).unwrap();
        fs::write(repo.join(".trellis-history/supervisor_state.json"),
            serde_json::to_vec(&serde_json::json!({"event_count":1,"event_count_convention":"record_count","state":state})).unwrap()).unwrap();
        let event = TrustedArtifactRebind::between(&state, &state, "a".repeat(64)).unwrap();
        publish(&root, &original, &state, event).unwrap();
        let bytes = fs::read(&log).unwrap();
        assert!(bytes.starts_with(prefix.as_bytes()));
        let appended: EventLogRecord = serde_json::from_slice(&bytes[prefix.len()..]).unwrap();
        assert_eq!(appended.index, 1);
        assert!(appended.commands.is_empty());
        assert_eq!(
            crate::apply_event(state.clone(), appended.event)
                .unwrap()
                .state,
            state
        );
        let canonical: serde_json::Value = serde_json::from_slice(
            &fs::read(repo.join(".trellis-history/supervisor_state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(canonical["event_count"], 2);
        assert_eq!(canonical["state"], serde_json::to_value(&state).unwrap());
        assert!(completed(&root));
    }
    #[test]
    fn interruption_at_every_carrier_recovers_whole_generation() {
        for boundary in 0..4 {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let dir = root.join(DIR);
            fs::create_dir(&dir).unwrap();
            let mut journal = Journal {
                version: 1,
                input_generation: "a".repeat(64),
                entries: vec![],
            };
            for index in 0..4 {
                let destination = root.join(format!("carrier-{index}"));
                fs::write(&destination, b"old\nprefix\n").unwrap();
                let bytes = b"old\nprefix\nnew event\n";
                let staged = format!("{index}.json");
                fs::write(dir.join(&staged), bytes).unwrap();
                journal.entries.push(Entry {
                    destination,
                    before: Some(hash(b"old\nprefix\n")),
                    after: hash(bytes),
                    staged,
                });
            }
            fs::write(
                dir.join("commit.json"),
                serde_json::to_vec(&journal).unwrap(),
            )
            .unwrap();
            assert!(recover_with_hook(root, |index| if index == boundary {
                Err("interruption".into())
            } else {
                Ok(())
            })
            .is_err());
            recover(root).unwrap();
            for entry in &journal.entries {
                assert_eq!(
                    fs::read(&entry.destination).unwrap(),
                    b"old\nprefix\nnew event\n"
                );
            }
            assert!(completed(root));
            recover(root).unwrap();
        }
    }
}
