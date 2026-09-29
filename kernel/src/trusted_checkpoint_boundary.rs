//! Explicit adoption of a legacy Git checkpoint whose final event was not
//! included in its published log. This is a trusted snapshot boundary, never
//! a reconstructed or fabricated WrapperResponse.
use crate::{model::recompute_local_closure_reverse_indices, ProtocolState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

const HISTORY: &str = ".trellis-history/supervisor_state.json";
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("cannot verify trusted checkpoint Git evidence".into());
    }
    Ok(output.stdout)
}

/// Use the exact existing codec, embedded with the kernel just like its
/// deterministic observers. No installed Python package or source path is
/// needed; replay itself uses the Rust decoder and never starts Python.
pub fn compact_document(document: &Value) -> Result<Value, String> {
    let code = concat!(include_str!("../../trellis/history_artifacts.py"),
        "\nimport sys\njson.dump(encode_shared_state(json.load(sys.stdin)), sys.stdout, separators=(',', ':'))\n");
    let mut child = Command::new("python3")
        .args(["-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let write_result =
        serde_json::to_writer(child.stdin.take().ok_or("missing codec input")?, document);
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    write_result.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "checkpoint compaction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedCheckpointBoundary {
    pub version: u32,
    pub source_commit: String,
    pub checkpoint_raw_sha256: String,
    pub checkpoint_document_sha256: String,
    pub event_prefix_sha256: String,
    pub legacy_event_index: u64,
    /// Only the shared container is carried; no second decoded state copy.
    pub checkpoint: Value,
}

impl TrustedCheckpointBoundary {
    pub fn prepare(
        repo: &Path,
        before: &ProtocolState,
        count: u64,
        prefix_sha256: &str,
    ) -> Result<Option<Self>, String> {
        let raw = fs::read(repo.join(HISTORY)).map_err(|e| e.to_string())?;
        let encoded: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        let document =
            crate::shared_state_codec::decode_shared_state(encoded).map_err(|e| e.to_string())?;
        if document["event_count"].as_u64() != Some(count) {
            return Err("checkpoint/prefix count mismatch".into());
        }
        let mut selected: ProtocolState =
            serde_json::from_value(document["state"].clone()).map_err(|e| e.to_string())?;
        recompute_local_closure_reverse_indices(&mut selected);
        if selected != *before {
            return Err("canonical checkpoint differs from selected runtime state".into());
        }
        match document
            .get("event_count_convention")
            .and_then(Value::as_str)
        {
            Some("record_count") => return Ok(None),
            None if document.get("event_count_convention").is_none() => {}
            _ => return Err("unsupported checkpoint event-count convention".into()),
        }
        if document["event_count"].as_u64() != Some(count) {
            return Err("legacy checkpoint/prefix count mismatch".into());
        }
        let source_commit = String::from_utf8(git(repo, &["rev-parse", "HEAD"])?)
            .map_err(|e| e.to_string())?
            .trim()
            .to_owned();
        let actual = git(repo, &["show", &format!("{source_commit}:{HISTORY}")])?;
        if actual != raw {
            return Err("selected canonical checkpoint differs from its pinned Git blob".into());
        }
        let carrier = Self {
            version: 1,
            source_commit,
            checkpoint_raw_sha256: hash(&raw),
            checkpoint_document_sha256: crate::trusted_artifact_rebind::digest(&document),
            event_prefix_sha256: prefix_sha256.to_owned(),
            legacy_event_index: count,
            checkpoint: compact_document(&document)?,
        };
        if carrier.apply()? != *before {
            return Err("legacy boundary differs from exact selected state".into());
        }
        carrier.validate_context_digest(repo, count, prefix_sha256)?;
        Ok(Some(carrier))
    }

    pub fn apply(&self) -> Result<ProtocolState, String> {
        if self.version != 1
            || !(valid_hash(&self.source_commit, 40) || valid_hash(&self.source_commit, 64))
            || !valid_hash(&self.checkpoint_raw_sha256, 64)
            || !valid_hash(&self.event_prefix_sha256, 64)
            || !crate::shared_state_codec::is_shared_state(&self.checkpoint)
        {
            return Err("unsupported or malformed trusted checkpoint boundary".into());
        }
        let document = crate::shared_state_codec::decode_shared_state(self.checkpoint.clone())
            .map_err(|e| e.to_string())?;
        if document.get("event_count_convention").is_some()
            || document["event_count"].as_u64() != Some(self.legacy_event_index)
            || crate::trusted_artifact_rebind::digest(&document) != self.checkpoint_document_sha256
        {
            return Err("trusted checkpoint boundary document binding mismatch".into());
        }
        let mut state: ProtocolState =
            serde_json::from_value(document["state"].clone()).map_err(|e| e.to_string())?;
        recompute_local_closure_reverse_indices(&mut state);
        crate::trusted_artifact_rebind::check_scope(&state)?;
        state.validate()?;
        Ok(state)
    }

    /// Called by publication and replay even when a selected seed skips this
    /// record. Pure apply_event cannot inspect the immutable Git/log context.
    pub fn validate_context(&self, repo: &Path, index: u64, prefix: &[u8]) -> Result<(), String> {
        self.validate_context_digest(repo, index, &hash(prefix))
    }

    fn validate_context_digest(
        &self,
        repo: &Path,
        index: u64,
        prefix_sha256: &str,
    ) -> Result<(), String> {
        if index != self.legacy_event_index || prefix_sha256 != self.event_prefix_sha256 {
            return Err("trusted checkpoint boundary event prefix/index mismatch".into());
        }
        let raw = git(
            repo,
            &["show", &format!("{}:{HISTORY}", self.source_commit)],
        )?;
        if hash(&raw) != self.checkpoint_raw_sha256 {
            return Err("trusted checkpoint raw Git blob mismatch".into());
        }
        let document = crate::shared_state_codec::decode_shared_state(
            serde_json::from_slice(&raw).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if crate::trusted_artifact_rebind::digest(&document) != self.checkpoint_document_sha256 {
            return Err("trusted checkpoint compact/Git document mismatch".into());
        }
        // The Git source must contain exactly the bound prefix, not merely a
        // snapshot with a matching count.
        let names = git(
            repo,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &self.source_commit,
                "--",
                ".trellis-history/event-log",
            ],
        )?;
        let mut source_prefix = Sha256::new();
        for name in String::from_utf8(names).map_err(|e| e.to_string())?.lines() {
            if name
                .rsplit('/')
                .next()
                .is_some_and(|s| s.starts_with("cycle-") && s.ends_with(".jsonl"))
            {
                source_prefix.update(git(
                    repo,
                    &["show", &format!("{}:{name}", self.source_commit)],
                )?);
            }
        }
        if format!("{:x}", source_prefix.finalize()) != prefix_sha256 {
            return Err("trusted checkpoint Git event prefix differs".into());
        }
        self.apply()?;
        Ok(())
    }
}

/// Validate exact JSONL bytes, including whitespace, before replay hydrates a
/// seed. Every anchor's provenance remains checked even if replay skips it.
pub fn validate_log_context(
    repo: &Path,
    files: &[std::path::PathBuf],
    keep: u64,
) -> Result<(), String> {
    #[derive(Deserialize)]
    struct EventKind {
        event: String,
    }
    #[derive(Deserialize)]
    struct Index {
        index: u64,
        event: EventKind,
    }
    let mut prefix = Sha256::new();
    let mut index = 0u64;
    for file in files {
        let bytes = fs::read(file).map_err(|e| e.to_string())?;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            // Repair replay may intentionally discard malformed future data.
            // Provenance is required for the retained prefix, including anchors
            // skipped by the seed, but not for excluded future records.
            if index == keep {
                return Ok(());
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                prefix.update(line);
                continue;
            }
            let header: Index = serde_json::from_slice(line).map_err(|e| e.to_string())?;
            if header.index != index {
                return Err("replay event prefix is not dense".into());
            }
            if header.event.event == "trusted_checkpoint_boundary" {
                let record: crate::EventLogRecord =
                    serde_json::from_slice(line).map_err(|e| e.to_string())?;
                if let crate::ProtocolEvent::TrustedCheckpointBoundary { payload } = record.event {
                    payload.validate_context_digest(
                        repo,
                        index,
                        &format!("{:x}", prefix.clone().finalize()),
                    )?;
                }
            }
            prefix.update(line);
            index += 1;
        }
    }
    Ok(())
}
