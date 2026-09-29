//! Recoverable publication of a complete identity repair. The durable journal
//! is the commit decision. Readers finish it before loading protocol state.
use crate::{LocalClosureRecord, ProtocolState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::{fs, io::Write, path::Path};

const DIR: &str = "closure-identity-publication";

/// Held for the complete explicit repair, including probes and publication.
pub struct RepairOwnership(fs::File);
impl RepairOwnership {
    pub fn acquire(root: &Path) -> Result<Self, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("closure-identity-repair.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("another closure identity repair owns this runtime".into());
        }
        Ok(Self(file))
    }
}
impl Drop for RepairOwnership {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Journal {
    old_state_sha256: String,
    new_state_sha256: String,
    records: Vec<LocalClosureRecord>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn durable_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("missing parent")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("identity-tmp");
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

pub fn publish(
    root: &Path,
    original: &[u8],
    state: &ProtocolState,
    records: Vec<LocalClosureRecord>,
) -> Result<(), String> {
    if fs::read(root.join("protocol_state.json")).map_err(|e| e.to_string())? != original {
        return Err("identity repair input generation changed; nothing published".into());
    }
    state.validate_local_closure_root_consistency()?;
    for record in &records {
        validate_mirror_node(record)?;
        if state.local_closure_records.get(&record.node) != Some(record) {
            return Err("identity publication mirror does not belong to candidate state".into());
        }
    }
    let next = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let dir = root.join(DIR);
    if dir.join("commit.json").exists() {
        return Err("pending identity publication must be recovered first".into());
    }
    durable_write(&dir.join("state.json"), &next)?;
    let journal = Journal {
        old_state_sha256: digest(original),
        new_state_sha256: digest(&next),
        records,
    };
    durable_write(
        &dir.join("commit.json"),
        &serde_json::to_vec(&journal).map_err(|e| e.to_string())?,
    )?;
    // Make the journal directory entry durable before publishing any carrier.
    fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    recover_with_hook(root, |_| Ok(()))
}

pub fn recover(root: &Path) -> Result<(), String> {
    if !root.join(DIR).join("commit.json").exists() {
        return Ok(());
    }
    let _ownership = RepairOwnership::acquire(root)?;
    recover_with_hook(root, |_| Ok(()))
}

fn validate_mirror_node(record: &LocalClosureRecord) -> Result<(), String> {
    let name = record.node.as_str();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("invalid identity mirror node".into());
    }
    Ok(())
}

fn recover_with_hook(
    root: &Path,
    mut after: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let dir = root.join(DIR);
    let journal_path = dir.join("commit.json");
    if !journal_path.exists() {
        return Ok(());
    }
    let journal: Journal =
        serde_json::from_slice(&fs::read(&journal_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let next = fs::read(dir.join("state.json")).map_err(|e| e.to_string())?;
    if digest(&next) != journal.new_state_sha256 {
        return Err("corrupt identity publication candidate".into());
    }
    let candidate: ProtocolState = serde_json::from_slice(&next).map_err(|e| e.to_string())?;
    candidate.validate_local_closure_root_consistency()?;
    for record in &journal.records {
        if candidate.local_closure_records.get(&record.node) != Some(record) {
            return Err("identity publication mirror does not belong to candidate state".into());
        }
    }
    let state_path = root.join("protocol_state.json");
    let current = digest(&fs::read(&state_path).map_err(|e| e.to_string())?);
    if current != journal.old_state_sha256 && current != journal.new_state_sha256 {
        return Err("identity publication conflicts with another state writer".into());
    }
    fs::create_dir_all(root.join("checker-state/local-closure-records"))
        .map_err(|e| e.to_string())?;
    for directory in [root.join("checker-state"), root.to_path_buf()] {
        fs::File::open(directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    for record in &journal.records {
        validate_mirror_node(record)?;
        durable_write(
            &root
                .join("checker-state/local-closure-records")
                .join(format!("{}.json", record.node)),
            &serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?,
        )?;
        after("mirror")?;
    }
    let marker = crate::runtime::local_closure_replay_snapshot_pending_path(root);
    durable_write(
        &marker,
        b"next event must carry complete offline local-closure migration coverage\n",
    )?;
    after("replay_marker")?;
    durable_write(&state_path, &next)?;
    after("state")?;
    fs::remove_file(&journal_path).map_err(|e| e.to_string())?;
    fs::File::open(&dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interruption_at_each_publication_boundary_recovers_complete_generation() {
        for boundary in ["mirror", "replay_marker", "state"] {
            let dir = tempfile::tempdir().unwrap();
            let old = b"old generation";
            fs::write(dir.path().join("protocol_state.json"), old).unwrap();
            let mut state = ProtocolState::default();
            state.cycle = 17;
            let mut record = LocalClosureRecord::default();
            record.node = "Preamble".into();
            state
                .local_closure_records
                .insert(record.node.clone(), record.clone());
            let next = serde_json::to_vec_pretty(&state).unwrap();
            let txn = dir.path().join(DIR);
            fs::create_dir(&txn).unwrap();
            fs::write(txn.join("state.json"), &next).unwrap();
            let j = Journal {
                old_state_sha256: digest(old),
                new_state_sha256: digest(&next),
                records: vec![record.clone()],
            };
            fs::write(txn.join("commit.json"), serde_json::to_vec(&j).unwrap()).unwrap();
            assert!(recover_with_hook(dir.path(), |at| if at == boundary {
                Err("interrupted".into())
            } else {
                Ok(())
            })
            .is_err());
            recover(dir.path()).unwrap();
            assert_eq!(
                fs::read(dir.path().join("protocol_state.json")).unwrap(),
                next
            );
            let installed: LocalClosureRecord = serde_json::from_slice(
                &fs::read(
                    dir.path()
                        .join("checker-state/local-closure-records/Preamble.json"),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(installed, record);
            assert!(
                crate::runtime::local_closure_replay_snapshot_pending_path(dir.path()).is_file()
            );
            recover(dir.path()).unwrap();
        }
    }
}
