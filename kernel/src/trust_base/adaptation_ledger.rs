//! Canonical Phase-0 adaptation ledger and byte-complete replay.

use super::campaign_plan::{AdaptationCitation, AdaptationLedgerEntry, AdaptationSeamClass, CampaignTrustPlan};
use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash,
    DomainTag, Sha256Digest, TrustError,
};
use super::source_tree::{read_source_tree, source_tree_manifest_from_files, validate_relative_path};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const PHASE0_ADAPTATION_LEDGER_SCHEMA: &str = "trellis-phase0-adaptation-ledger/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AdaptationLedger {
    pub schema: String,
    pub generation: u64,
    pub unadapted_tree_sha256: Sha256Digest,
    pub adapted_tree_sha256: Sha256Digest,
    pub goal_sha256: Sha256Digest,
    pub entries: Vec<Phase0AdaptationEntry>,
    pub ledger_entries_root: Sha256Digest,
    pub ledger_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AdaptationEntry {
    pub id: String,
    pub file: String,
    pub operation: Phase0FileOperation,
    pub before_span: ByteSpan,
    pub after_span: ByteSpan,
    pub before_sha256: Option<Sha256Digest>,
    pub after_sha256: Option<Sha256Digest>,
    pub patch: ExactBytePatch,
    pub diff_sha256: Sha256Digest,
    pub quoted_error: QuotedCheckerError,
    pub discovery_failure_receipt_sha256: Sha256Digest,
    pub ablation_failure_receipt_sha256: Sha256Digest,
    pub behavior_preservation_claim: String,
    pub seam_class: AdaptationSeamClass,
    pub citation: AdaptationCitation,
    pub affected_targets: Vec<String>,
    pub meaning_change: bool,
    pub status: Phase0LedgerStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0FileOperation {
    Add,
    Modify,
    Delete,
}

impl Phase0FileOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Modify => "modify",
            Self::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0LedgerStatus {
    Draft,
    Audited,
    Sealed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteSpan {
    pub start: u64,
    pub end: u64,
}

impl ByteSpan {
    fn as_range(self, length: usize, label: &str) -> Result<std::ops::Range<usize>, TrustError> {
        let start = usize::try_from(self.start).map_err(|_| ledger_error(format!(
            "{label} start does not fit this platform"
        )))?;
        let end = usize::try_from(self.end).map_err(|_| ledger_error(format!(
            "{label} end does not fit this platform"
        )))?;
        if start > end || end > length {
            return Err(ledger_error(format!("{label} is outside its file")));
        }
        Ok(start..end)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactBytePatch {
    pub removed_base64: String,
    pub inserted_base64: String,
    pub removed_sha256: Sha256Digest,
    pub inserted_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotedCheckerError {
    pub tool: String,
    pub receipt_sha256: Sha256Digest,
    pub stage_index: u64,
    pub stream: CheckerStream,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub quote: String,
    pub quote_sha256: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckerStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatibleAdaptationLedger {
    Phase0(Phase0AdaptationLedger),
    Legacy(Vec<AdaptationLedgerEntry>),
}

impl Phase0AdaptationEntry {
    pub fn seal_bindings(&mut self, generation: u64) -> Result<(), TrustError> {
        let removed = decode_patch_member(&self.patch.removed_base64, "removed_base64")?;
        let inserted = decode_patch_member(&self.patch.inserted_base64, "inserted_base64")?;
        self.patch.removed_sha256 = raw_sha256(&removed);
        self.patch.inserted_sha256 = raw_sha256(&inserted);
        self.diff_sha256 = adaptation_diff_digest(self)?;
        self.id = phase0_entry_id(generation, self.diff_sha256);
        Ok(())
    }
}

impl Phase0AdaptationLedger {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        for entry in &mut self.entries {
            entry.seal_bindings(self.generation)?;
        }
        self.entries.sort_by(entry_order);
        self.ledger_entries_root = entries_root(&self.entries)?;
        self.ledger_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self)
            .map_err(|error| ledger_error(error.to_string()))?;
        self.ledger_sha256 = self_digest(DomainTag::RawArtifact, &value, "ledger_sha256")?;
        Ok(self)
    }

    pub fn parse_and_validate(
        bytes: &[u8],
        goal_targets: &BTreeSet<String>,
    ) -> Result<Self, TrustError> {
        let value = parse_json_strict(bytes)?;
        if canonical_json_value(&value)? != bytes {
            return Err(ledger_error("Phase-0 ledger must be exact canonical JSON"));
        }
        let ledger: Self = serde_json::from_value(value)
            .map_err(|error| ledger_error(error.to_string()))?;
        ledger.validate(goal_targets)?;
        Ok(ledger)
    }

    pub fn validate(&self, goal_targets: &BTreeSet<String>) -> Result<(), TrustError> {
        if self.schema != PHASE0_ADAPTATION_LEDGER_SCHEMA {
            return Err(ledger_error("Phase-0 adaptation ledger has the wrong schema"));
        }
        if goal_targets.is_empty() {
            return Err(ledger_error("Phase-0 ledger requires the immutable GOAL target set"));
        }
        for digest in [
            self.unadapted_tree_sha256,
            self.adapted_tree_sha256,
            self.goal_sha256,
            self.ledger_entries_root,
            self.ledger_sha256,
        ] {
            if digest == Sha256Digest::ZERO {
                return Err(ledger_error("Phase-0 ledger contains a zero digest"));
            }
        }
        if self
            .entries
            .windows(2)
            .any(|pair| entry_order_compare(&pair[0], &pair[1]).is_ge())
        {
            return Err(ledger_error("ledger entries are not in canonical order"));
        }
        if entries_root(&self.entries)? != self.ledger_entries_root {
            return Err(ledger_error("ledger_entries_root does not match the canonical entries"));
        }
        let value = serde_json::to_value(self)
            .map_err(|error| ledger_error(error.to_string()))?;
        if self_digest(DomainTag::RawArtifact, &value, "ledger_sha256")?
            != self.ledger_sha256
        {
            return Err(ledger_error("ledger_sha256 is not the ledger self digest"));
        }
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            validate_entry(entry, self.generation, goal_targets)?;
            if !ids.insert(entry.id.as_str()) {
                return Err(ledger_error("ledger repeats an entry id"));
            }
        }
        Ok(())
    }
}

pub fn parse_compatible_adaptation_ledger(
    bytes: &[u8],
    goal_targets: &BTreeSet<String>,
) -> Result<CompatibleAdaptationLedger, TrustError> {
    let value = parse_json_strict(bytes)?;
    if value.get("schema").and_then(Value::as_str) == Some(PHASE0_ADAPTATION_LEDGER_SCHEMA) {
        return Phase0AdaptationLedger::parse_and_validate(bytes, goal_targets)
            .map(CompatibleAdaptationLedger::Phase0);
    }
    if value.is_array() {
        let rows: Vec<AdaptationLedgerEntry> = serde_json::from_value(value)
            .map_err(|error| ledger_error(format!("legacy ledger is invalid: {error}")))?;
        return Ok(CompatibleAdaptationLedger::Legacy(rows));
    }
    if value.get("schema").and_then(Value::as_str) == Some(super::campaign_plan::CAMPAIGN_TRUST_PLAN_SCHEMA_V4) {
        let plan = CampaignTrustPlan::parse_and_validate(bytes)?;
        return Ok(CompatibleAdaptationLedger::Legacy(plan.adaptation_ledger));
    }
    Err(ledger_error("adaptation ledger has no recognized compatible schema"))
}

pub fn adaptation_diff_digest(entry: &Phase0AdaptationEntry) -> Result<Sha256Digest, TrustError> {
    // An entry's identity is the change it makes to the uploaded file: the
    // file, the operation, the span of the ORIGINAL file it replaces, and the
    // patch bytes. The whole-file digests and the after-span are consequences
    // of the whole ledger (a later seam in the same file moves them) and are
    // verified by replay, not part of the identity; otherwise every earlier
    // entry of a file would look new each time the file changes again.
    let value = serde_json::json!({
        "before_span": entry.before_span,
        "file": entry.file,
        "operation": entry.operation,
        "patch": entry.patch,
    });
    Ok(tagged_hash(
        DomainTag::AdaptationLedgerRow,
        &canonical_json_value(&value)?,
    ))
}

pub fn phase0_entry_id(generation: u64, digest: Sha256Digest) -> String {
    format!("phase0-{generation}-{digest}")
}

fn entries_root(entries: &[Phase0AdaptationEntry]) -> Result<Sha256Digest, TrustError> {
    // This is the generation's stable checker binding.  Ablation receipts are
    // produced by checks which themselves bind this root, so hashing those
    // receipts here would create an impossible digest cycle.  The ledger self
    // digest below still binds the completed receipt fields and lifecycle
    // status.  Audit requests bind both digests.
    let value = Value::Array(
        entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "affected_targets": entry.affected_targets,
                    "after_sha256": entry.after_sha256,
                    "after_span": entry.after_span,
                    "before_sha256": entry.before_sha256,
                    "before_span": entry.before_span,
                    "behavior_preservation_claim": entry.behavior_preservation_claim,
                    "citation": entry.citation,
                    "diff_sha256": entry.diff_sha256,
                    "discovery_failure_receipt_sha256": entry.discovery_failure_receipt_sha256,
                    "file": entry.file,
                    "id": entry.id,
                    "meaning_change": entry.meaning_change,
                    "operation": entry.operation,
                    "patch": entry.patch,
                    "quoted_error": entry.quoted_error,
                    "seam_class": entry.seam_class,
                })
            })
            .collect(),
    );
    Ok(tagged_hash(
        DomainTag::AdaptationLedgerRow,
        &canonical_json_value(&value)?,
    ))
}

fn validate_entry(
    entry: &Phase0AdaptationEntry,
    generation: u64,
    goal_targets: &BTreeSet<String>,
) -> Result<(), TrustError> {
    validate_relative_path(&entry.file)?;
    if adaptation_diff_digest(entry)? != entry.diff_sha256
        || phase0_entry_id(generation, entry.diff_sha256) != entry.id
    {
        return Err(ledger_error(format!("entry {} has forged delta bindings", entry.id)));
    }
    let removed = decode_patch_member(&entry.patch.removed_base64, "removed_base64")?;
    let inserted = decode_patch_member(&entry.patch.inserted_base64, "inserted_base64")?;
    if raw_sha256(&removed) != entry.patch.removed_sha256
        || raw_sha256(&inserted) != entry.patch.inserted_sha256
    {
        return Err(ledger_error(format!("entry {} patch digest mismatch", entry.id)));
    }
    if entry.operation == Phase0FileOperation::Modify && removed == inserted {
        return Err(ledger_error(format!("entry {} is a zero-op", entry.id)));
    }
    match entry.operation {
        Phase0FileOperation::Add => {
            if entry.before_sha256.is_some()
                || entry.after_sha256.is_none()
                || entry.before_span != (ByteSpan { start: 0, end: 0 })
                || entry.after_span != (ByteSpan { start: 0, end: inserted.len() as u64 })
                || !removed.is_empty()
            {
                return Err(ledger_error("add entries must insert one complete new file"));
            }
        }
        Phase0FileOperation::Modify => {
            if entry.before_sha256.is_none() || entry.after_sha256.is_none() {
                return Err(ledger_error("modify entries require before and after digests"));
            }
        }
        Phase0FileOperation::Delete => {
            if entry.before_sha256.is_none()
                || entry.after_sha256.is_some()
                || entry.before_span != (ByteSpan { start: 0, end: removed.len() as u64 })
                || entry.after_span != (ByteSpan { start: 0, end: 0 })
                || !inserted.is_empty()
            {
                return Err(ledger_error("delete entries must remove one complete file"));
            }
        }
    }
    if entry.discovery_failure_receipt_sha256 == Sha256Digest::ZERO
        || entry.quoted_error.receipt_sha256 == Sha256Digest::ZERO
        || entry.quoted_error.quote_sha256 == Sha256Digest::ZERO
        || entry.quoted_error.receipt_sha256 != entry.discovery_failure_receipt_sha256
    {
        return Err(ledger_error("entry has invalid checker receipt bindings"));
    }
    if entry.status != Phase0LedgerStatus::Draft
        && entry.ablation_failure_receipt_sha256 == Sha256Digest::ZERO
    {
        return Err(ledger_error(
            "an audited or sealed entry requires its ablation failure receipt",
        ));
    }
    validate_text(&entry.quoted_error.tool, "quoted error tool")?;
    validate_text(&entry.quoted_error.quote, "quoted error")?;
    if entry.quoted_error.byte_length != entry.quoted_error.quote.as_bytes().len() as u64
        || raw_sha256(entry.quoted_error.quote.as_bytes()) != entry.quoted_error.quote_sha256
        || entry
            .quoted_error
            .byte_offset
            .checked_add(entry.quoted_error.byte_length)
            .is_none()
    {
        return Err(ledger_error("quoted error byte binding is invalid"));
    }
    validate_text(
        &entry.behavior_preservation_claim,
        "behavior-preservation claim",
    )?;
    if entry.seam_class == AdaptationSeamClass::Seed {
        return Err(ledger_error("new Phase-0 entries may not use the legacy seed seam"));
    }
    match &entry.citation {
        AdaptationCitation::LanguageGuarantee { statement } => {
            validate_text(statement, "language guarantee")?
        }
        AdaptationCitation::UpstreamReference { reference } => {
            validate_text(reference, "upstream reference")?
        }
    }
    if entry.affected_targets.is_empty() {
        return Err(ledger_error("entry needs at least one affected GOAL target"));
    }
    let mut previous_target: Option<&str> = None;
    for target in &entry.affected_targets {
        if !target.starts_with("goal:") || !goal_targets.contains(target) {
            return Err(ledger_error(format!("entry names unknown GOAL target {target}")));
        }
        if previous_target.is_some_and(|prior| prior.as_bytes() >= target.as_bytes()) {
            return Err(ledger_error("affected_targets are not strictly sorted"));
        }
        previous_target = Some(target);
    }
    Ok(())
}

fn validate_text(value: &str, label: &str) -> Result<(), TrustError> {
    if value.trim().is_empty() || value.as_bytes().contains(&0) {
        return Err(ledger_error(format!("entry has invalid {label}")));
    }
    Ok(())
}

fn decode_patch_member(value: &str, label: &str) -> Result<Vec<u8>, TrustError> {
    let bytes = BASE64_STANDARD
        .decode(value)
        .map_err(|error| ledger_error(format!("{label} is invalid base64: {error}")))?;
    if BASE64_STANDARD.encode(&bytes) != value {
        return Err(ledger_error(format!("{label} is not canonical base64")));
    }
    Ok(bytes)
}

fn entry_order(left: &Phase0AdaptationEntry, right: &Phase0AdaptationEntry) -> std::cmp::Ordering {
    entry_order_compare(left, right)
}

fn entry_order_compare(
    left: &Phase0AdaptationEntry,
    right: &Phase0AdaptationEntry,
) -> std::cmp::Ordering {
    left.file
        .as_bytes()
        .cmp(right.file.as_bytes())
        .then_with(|| left.before_span.start.cmp(&right.before_span.start))
        .then_with(|| left.after_span.start.cmp(&right.after_span.start))
        .then_with(|| left.operation.as_str().cmp(right.operation.as_str()))
        .then_with(|| left.id.as_bytes().cmp(right.id.as_bytes()))
}

/// Reconstruct the adapted source in a fresh destination and require exact
/// path, byte, manifest, and root equality with the supplied candidate.
pub fn replay_phase0_adaptation(
    unadapted_root: &Path,
    ledger: &Phase0AdaptationLedger,
    goal_targets: &BTreeSet<String>,
    candidate_adapted_root: &Path,
    output_root: &Path,
) -> Result<Sha256Digest, TrustError> {
    let root = verify_phase0_adaptation(
        unadapted_root,
        ledger,
        goal_targets,
        candidate_adapted_root,
    )?;
    let candidate = read_source_tree(candidate_adapted_root)?;
    let candidate_manifest = source_tree_manifest_from_files(&candidate)?;
    write_replay_tree(output_root, &candidate)?;
    let written = match read_source_tree(output_root) {
        Ok(written) => written,
        Err(error) => {
            let _ = fs::remove_dir_all(output_root);
            return Err(error);
        }
    };
    let written_manifest = source_tree_manifest_from_files(&written)?;
    if written != candidate || written_manifest != candidate_manifest {
        let _ = fs::remove_dir_all(output_root);
        return Err(replay_error("materialized replay changed a path, byte, or root"));
    }
    Ok(root)
}

/// Verify the complete raw + ledger = candidate relation without materializing
/// an output tree.  Phase-0 state validation calls this on every load and
/// transition, where a write-only scratch destination would weaken the
/// otherwise read-only validation path.
pub fn verify_phase0_adaptation(
    unadapted_root: &Path,
    ledger: &Phase0AdaptationLedger,
    goal_targets: &BTreeSet<String>,
    candidate_adapted_root: &Path,
) -> Result<Sha256Digest, TrustError> {
    ledger.validate(goal_targets)?;
    let unadapted = read_source_tree(unadapted_root)?;
    let candidate = read_source_tree(candidate_adapted_root)?;
    let unadapted_manifest = source_tree_manifest_from_files(&unadapted)?;
    let candidate_manifest = source_tree_manifest_from_files(&candidate)?;
    if unadapted_manifest.source_tree_sha256 != ledger.unadapted_tree_sha256
        || candidate_manifest.source_tree_sha256 != ledger.adapted_tree_sha256
    {
        return Err(replay_error("ledger source-tree roots do not match the supplied trees"));
    }

    let actual_changed: BTreeSet<String> = unadapted
        .keys()
        .chain(candidate.keys())
        .filter(|path| unadapted.get(*path) != candidate.get(*path))
        .cloned()
        .collect();
    let declared: BTreeSet<String> = ledger.entries.iter().map(|entry| entry.file.clone()).collect();
    if actual_changed != declared {
        let unlogged: Vec<_> = actual_changed.difference(&declared).cloned().collect();
        let spurious: Vec<_> = declared.difference(&actual_changed).cloned().collect();
        return Err(replay_error(format!(
            "ledger path set is incomplete (unlogged={unlogged:?}, unchanged={spurious:?})"
        )));
    }

    let mut grouped: BTreeMap<&str, Vec<&Phase0AdaptationEntry>> = BTreeMap::new();
    for entry in &ledger.entries {
        grouped.entry(&entry.file).or_default().push(entry);
    }
    let mut replayed = unadapted.clone();
    for (path, entries) in grouped {
        let operation = entries[0].operation;
        if entries.iter().any(|entry| entry.operation != operation) {
            return Err(replay_error(format!("{path} mixes operation kinds")));
        }
        match operation {
            Phase0FileOperation::Add => replay_add(path, &entries, &candidate, &mut replayed)?,
            Phase0FileOperation::Delete => replay_delete(path, &entries, &unadapted, &mut replayed)?,
            Phase0FileOperation::Modify => {
                replay_modify(path, &entries, &unadapted, &candidate, &mut replayed)?
            }
        }
    }
    if replayed != candidate {
        return Err(replay_error("replayed files differ from the adapted candidate"));
    }
    Ok(candidate_manifest.source_tree_sha256)
}

/// Compute the exact tree root obtained from the frozen raw tree after
/// applying every ledger row except `omitted_entry_id`.  This is the kernel
/// fact bound to an entry-ablation checker receipt.
pub fn phase0_ablated_tree_digest(
    unadapted_root: &Path,
    ledger: &Phase0AdaptationLedger,
    goal_targets: &BTreeSet<String>,
    omitted_entry_id: &str,
) -> Result<Sha256Digest, TrustError> {
    ledger.validate(goal_targets)?;
    if !ledger.entries.iter().any(|entry| entry.id == omitted_entry_id) {
        return Err(replay_error("ablation names an unknown ledger entry"));
    }
    let mut files = read_source_tree(unadapted_root)?;
    let mut grouped: BTreeMap<&str, Vec<&Phase0AdaptationEntry>> = BTreeMap::new();
    for entry in &ledger.entries {
        if entry.id != omitted_entry_id {
            grouped.entry(&entry.file).or_default().push(entry);
        }
    }
    let changed_paths: BTreeSet<&str> = ledger.entries.iter().map(|entry| entry.file.as_str()).collect();
    for path in changed_paths {
        let original_entries: Vec<_> = ledger
            .entries
            .iter()
            .filter(|entry| entry.file == path)
            .collect();
        let operation = original_entries[0].operation;
        let entries = grouped.get(path).cloned().unwrap_or_default();
        match operation {
            Phase0FileOperation::Add => {
                if entries.len() > 1 {
                    return Err(replay_error(format!("add for {path} is not atomic")));
                }
                if let Some(entry) = entries.first() {
                    files.insert(
                        path.to_owned(),
                        decode_patch_member(&entry.patch.inserted_base64, "inserted_base64")?,
                    );
                }
            }
            Phase0FileOperation::Delete => {
                if entries.len() > 1 {
                    return Err(replay_error(format!("delete for {path} is not atomic")));
                }
                if !entries.is_empty() {
                    files.remove(path);
                }
            }
            Phase0FileOperation::Modify => {
                let before = files
                    .get(path)
                    .cloned()
                    .ok_or_else(|| replay_error(format!("modified source file {path} is missing")))?;
                let mut ordered = entries;
                ordered.sort_by_key(|entry| entry.before_span.start);
                let mut cursor = 0usize;
                let mut output = Vec::new();
                for entry in ordered {
                    let range = entry.before_span.as_range(before.len(), "before_span")?;
                    if range.start < cursor {
                        return Err(replay_error(format!("modify spans overlap in original file {path}")));
                    }
                    output.extend_from_slice(&before[cursor..range.start]);
                    let removed = decode_patch_member(&entry.patch.removed_base64, "removed_base64")?;
                    if before[range.clone()] != removed {
                        return Err(replay_error(format!("modify for {path} removed-byte mismatch")));
                    }
                    output.extend_from_slice(&decode_patch_member(
                        &entry.patch.inserted_base64,
                        "inserted_base64",
                    )?);
                    cursor = range.end;
                }
                output.extend_from_slice(&before[cursor..]);
                files.insert(path.to_owned(), output);
            }
        }
    }
    Ok(source_tree_manifest_from_files(&files)?.source_tree_sha256)
}

fn replay_add(
    path: &str,
    entries: &[&Phase0AdaptationEntry],
    candidate: &BTreeMap<String, Vec<u8>>,
    replayed: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), TrustError> {
    if entries.len() != 1 || replayed.contains_key(path) {
        return Err(replay_error(format!("add for {path} is not one new file")));
    }
    let entry = entries[0];
    let inserted = decode_patch_member(&entry.patch.inserted_base64, "inserted_base64")?;
    let after = candidate
        .get(path)
        .ok_or_else(|| replay_error(format!("added candidate file {path} is missing")))?;
    if &inserted != after || entry.after_sha256 != Some(raw_sha256(after)) {
        return Err(replay_error(format!("add for {path} does not match candidate bytes")));
    }
    replayed.insert(path.to_owned(), inserted);
    Ok(())
}

fn replay_delete(
    path: &str,
    entries: &[&Phase0AdaptationEntry],
    unadapted: &BTreeMap<String, Vec<u8>>,
    replayed: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), TrustError> {
    if entries.len() != 1 {
        return Err(replay_error(format!("delete for {path} is not one full-file entry")));
    }
    let entry = entries[0];
    let before = unadapted
        .get(path)
        .ok_or_else(|| replay_error(format!("deleted source file {path} is missing")))?;
    let removed = decode_patch_member(&entry.patch.removed_base64, "removed_base64")?;
    if &removed != before || entry.before_sha256 != Some(raw_sha256(before)) {
        return Err(replay_error(format!("delete for {path} does not match source bytes")));
    }
    replayed.remove(path);
    Ok(())
}

fn replay_modify(
    path: &str,
    entries: &[&Phase0AdaptationEntry],
    unadapted: &BTreeMap<String, Vec<u8>>,
    candidate: &BTreeMap<String, Vec<u8>>,
    replayed: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), TrustError> {
    let before = unadapted
        .get(path)
        .ok_or_else(|| replay_error(format!("modified source file {path} is missing")))?;
    let after = candidate
        .get(path)
        .ok_or_else(|| replay_error(format!("modified candidate file {path} is missing")))?;
    let before_digest = raw_sha256(before);
    let after_digest = raw_sha256(after);
    let mut ordered = entries.to_vec();
    ordered.sort_by_key(|entry| (entry.before_span.start, entry.after_span.start));
    let mut cursor = 0usize;
    let mut final_cursor = 0usize;
    let mut output = Vec::with_capacity(after.len());
    for entry in ordered {
        if entry.before_sha256 != Some(before_digest) || entry.after_sha256 != Some(after_digest) {
            return Err(replay_error(format!("modify for {path} has before/after digest mismatch")));
        }
        let before_range = entry.before_span.as_range(before.len(), "before_span")?;
        if before_range.start < cursor {
            return Err(replay_error(format!("modify spans overlap in original file {path}")));
        }
        output.extend_from_slice(&before[cursor..before_range.start]);
        final_cursor += before_range.start - cursor;
        let removed = decode_patch_member(&entry.patch.removed_base64, "removed_base64")?;
        let inserted = decode_patch_member(&entry.patch.inserted_base64, "inserted_base64")?;
        if before[before_range.clone()] != removed {
            return Err(replay_error(format!("modify for {path} removed-byte mismatch")));
        }
        let expected_after = ByteSpan {
            start: final_cursor as u64,
            end: (final_cursor + inserted.len()) as u64,
        };
        if entry.after_span != expected_after {
            return Err(replay_error(format!("modify for {path} has rebased after-span mismatch")));
        }
        let after_range = entry.after_span.as_range(after.len(), "after_span")?;
        if after[after_range] != inserted {
            return Err(replay_error(format!("modify for {path} inserted-byte mismatch")));
        }
        output.extend_from_slice(&inserted);
        final_cursor += inserted.len();
        cursor = before_range.end;
    }
    output.extend_from_slice(&before[cursor..]);
    if output != *after {
        return Err(replay_error(format!("modified bytes for {path} do not produce candidate")));
    }
    replayed.insert(path.to_owned(), output);
    Ok(())
}

fn write_replay_tree(
    output_root: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), TrustError> {
    if fs::symlink_metadata(output_root).is_ok() {
        return Err(replay_error(format!(
            "replay output already exists: {}",
            output_root.display()
        )));
    }
    fs::create_dir(output_root)
        .map_err(|error| replay_error(format!("create replay root: {error}")))?;
    #[cfg(unix)]
    fs::set_permissions(output_root, fs::Permissions::from_mode(0o700))
        .map_err(|error| replay_error(format!("protect replay root: {error}")))?;
    let result = (|| {
        for (relative, bytes) in files {
            validate_relative_path(relative)?;
            let path = output_root.join(relative);
            let parent = path
                .parent()
                .ok_or_else(|| replay_error("replay member has no parent"))?;
            fs::create_dir_all(parent)
                .map_err(|error| replay_error(format!("create replay directory: {error}")))?;
            #[cfg(unix)]
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|error| replay_error(format!("protect replay directory: {error}")))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| replay_error(format!("create replay file {relative}: {error}")))?;
            file.write_all(bytes)
                .map_err(|error| replay_error(format!("write replay file {relative}: {error}")))?;
            file.sync_all()
                .map_err(|error| replay_error(format!("sync replay file {relative}: {error}")))?;
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|error| replay_error(format!("protect replay file {relative}: {error}")))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(output_root);
    }
    result
}

fn ledger_error(detail: impl Into<String>) -> TrustError {
    TrustError::new("phase0_adaptation_ledger_invalid", detail)
}

fn replay_error(detail: impl Into<String>) -> TrustError {
    TrustError::new("phase0_adaptation_replay_failed", detail)
}
