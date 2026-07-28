//! Deterministic package authorization and detached offline verification.
//!
//! The authorized archive never contains its own authorization event.  The
//! event is committed to the external journal first; only after durable HEAD
//! installation is it signed by the seed-pinned journal authority.  The
//! detached sidecar carries the complete replayable prefix and that receipt.

use super::auth::{
    sign_journal_commit_receipt, verify_journal_commit_receipt, ActorKeyManifest,
    JournalCommitReceiptContext, ManifestAuthorityRoots,
};
use super::archive::{
    read_package_member, verify_package_archive, verify_single_claim_presentation,
};
use super::closure::{verify_seed_definition_bundle, VerifiedSeedDefinitionClosure};
use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash,
    verify_self_digest, DomainTag, Sha256Digest, TrustError,
};
use super::journal::{AppendRequest, JournalActor, TrustJournal};
use super::records::{
    AuthoritativeRecord, EventKind, JournalEvent, JournalEventPayload, JournalHead, Subject,
};
use super::schema::SchemaRegistry;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

const TRUST_BINDING_PATH: &str = "TRELLIS_TRUST_BINDING.json";
const CLAIM_DOCUMENT_PATH: &str = "WHAT_THE_PROOFS_ASSUME.md";
const PACKET_INDEX_PATH: &str = "TRELLIS_PACKET_INDEX.json";
const MAX_TRUST_BINDING_BYTES: u64 = 1024 * 1024;
const MAX_CLAIM_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PACKET_INDEX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PACKET_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PackageAuthorizationRequest<'a> {
    pub transaction_id: &'a str,
    pub generator_and_validator_closure_root: Sha256Digest,
    pub journal_authority_identity: &'a str,
    pub journal_key_id: &'a str,
    pub journal_signing_key: &'a SigningKey,
}

/// Opaque proof that the semantic pipeline replayed the current journal head
/// and found every target terminal and honestly rendered. External callers
/// cannot construct this value.
#[derive(Clone, Debug)]
pub struct PackageReadiness {
    pub(crate) journal_id: String,
    pub(crate) journal_head: JournalHead,
    pub(crate) human_approval_event_hash: Sha256Digest,
    pub(crate) semantic_root: Sha256Digest,
    pub(crate) derived_result_root: Sha256Digest,
    pub(crate) approved_evidence_tool_input_root: Sha256Digest,
    pub(crate) gate_presentation_sha256: Sha256Digest,
    pub(crate) target_count: usize,
    pub(crate) claim_rows_root: Sha256Digest,
    pub(crate) claim_document_bytes: Vec<u8>,
    /// Present only for required-v1 campaigns. Legacy packet builders retain
    /// their historical typed-role checks, while v1 packets must exactly
    /// realize this journal/seed-derived proof inventory.
    pub(crate) proof_requirements: Option<ProofPackageRequirements>,
    pub(crate) expected_proof_tree_root: Option<Sha256Digest>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProofPackageRequirements {
    /// Exact proof receipts already committed by formal-result events, keyed
    /// by a stable kind/target identity.
    pub(crate) journal_receipts: BTreeMap<String, Value>,
    /// Only seed-frozen conditional candidates selected in the current
    /// approval epoch belong to the packet proof inventory.  A decisive
    /// source reproduction therefore leaves this map empty.
    pub(crate) conditional_candidates: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    /// Exact receipts committed inside the current epoch's generated
    /// conditional-statement events.  The package-ready runtime checkpoint
    /// must reproduce these bytes exactly; it cannot replace journal
    /// authority with a mutable receipt directory.
    pub(crate) conditional_receipts: BTreeMap<Sha256Digest, Value>,
    /// True only after the package-ready runtime has recomputed the selected
    /// receipts from its checked protocol state.  Detached verification uses
    /// the journal receipts directly and does not consult this online latch.
    runtime_conditional_receipts_validated: bool,
}

#[derive(Clone, Debug)]
struct VerifiedProofTree {
    root: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct PackageArtifact {
    pub role: String,
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct AuthorizedPackage {
    pub package_presentation_sha256: Sha256Digest,
    pub authorization_head: JournalHead,
    pub sidecar_sha256: Sha256Digest,
    pub sidecar: Value,
    pub sidecar_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPackageAuthorization {
    pub package_presentation_sha256: Sha256Digest,
    pub authorization_head: JournalHead,
    pub human_approval_event_hash: Sha256Digest,
    pub derived_result_closure_root: Sha256Digest,
    pub generator_and_validator_closure_root: Sha256Digest,
    pub proof_tree_root: Option<Sha256Digest>,
    pub journal_authority_identity: String,
    pub journal_key_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarWire {
    schema: String,
    package_presentation_sha256: Sha256Digest,
    package_authorization_bundle: Value,
    authentication_key_manifest: Value,
    journal_commit_receipt: Value,
    canonical_genesis_head: JournalHead,
    journal_chain_bundles: Vec<Value>,
    pre_authorization_journal_head: JournalHead,
    committed_authorization_journal_head: JournalHead,
    sidecar_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustBindingWire {
    schema: String,
    journal_id: String,
    pre_authorization_journal_head: JournalHead,
    human_approval_event_hash: Sha256Digest,
    semantic_root: Sha256Digest,
    derived_result_root: Sha256Digest,
    approved_evidence_tool_input_root: Sha256Digest,
    target_count: usize,
    claim_rows_root: Sha256Digest,
    claim_document_sha256: Sha256Digest,
    gate_presentation_sha256: Sha256Digest,
    packet_index_sha256: Sha256Digest,
    generator_and_validator_tool_root: Sha256Digest,
    #[serde(default)]
    proof_tree_root: Option<Sha256Digest>,
    binding_sha256: Sha256Digest,
}

struct VerifiedPacketIndex {
    raw_sha256: Sha256Digest,
    paths_by_role: BTreeMap<String, Vec<String>>,
}

impl ProofPackageRequirements {
    pub(crate) fn new(
        journal_receipts: BTreeMap<String, Value>,
        conditional_candidates: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    ) -> Result<Self, TrustError> {
        if journal_receipts.is_empty() {
            return Err(TrustError::new(
                "package_proof_receipt_inventory_empty",
                "required-v1 package readiness has no journaled formal-result receipts",
            ));
        }
        for (identity, receipt) in &journal_receipts {
            validate_receipt_self_identity(receipt).map_err(|error| {
                TrustError::new(
                    "package_journal_proof_receipt_invalid",
                    format!("{identity}: {error}"),
                )
            })?;
        }
        let runtime_conditional_receipts_validated = conditional_candidates.is_empty();
        Ok(Self {
            journal_receipts,
            conditional_candidates,
            conditional_receipts: BTreeMap::new(),
            runtime_conditional_receipts_validated,
        })
    }

    pub(crate) fn set_conditional_receipts(
        &mut self,
        receipts: BTreeMap<Sha256Digest, Value>,
    ) -> Result<(), TrustError> {
        if receipts.keys().copied().collect::<BTreeSet<_>>()
            != self
                .conditional_candidates
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
        {
            return Err(TrustError::new(
                "package_conditional_receipt_set_mismatch",
                "package-ready runtime receipts differ from the current journal-selected candidate set",
            ));
        }
        for (candidate_digest, receipt) in &receipts {
            let candidate = self
                .conditional_candidates
                .get(candidate_digest)
                .expect("candidate key set checked above");
            super::pipeline::validate_conditional_candidate_proof_receipt(candidate, receipt)?;
        }
        if receipts != self.conditional_receipts {
            return Err(TrustError::new(
                "package_conditional_receipt_stale",
                "package-ready runtime receipts differ from the receipts committed by current generated-conditional events",
            ));
        }
        self.runtime_conditional_receipts_validated = true;
        Ok(())
    }

    fn bind_journal_conditional_receipts(
        &mut self,
        receipts: BTreeMap<Sha256Digest, Value>,
    ) -> Result<(), TrustError> {
        if receipts.keys().copied().collect::<BTreeSet<_>>()
            != self
                .conditional_candidates
                .keys()
                .copied()
                .collect::<BTreeSet<_>>()
        {
            return Err(TrustError::new(
                "package_conditional_receipt_set_mismatch",
                "current generated-conditional events do not exactly cover the selected candidate set",
            ));
        }
        for (candidate_digest, receipt) in &receipts {
            super::pipeline::validate_conditional_candidate_proof_receipt(
                self.conditional_candidates
                    .get(candidate_digest)
                    .expect("candidate key set checked above"),
                receipt,
            )?;
        }
        self.conditional_receipts = receipts;
        self.runtime_conditional_receipts_validated = self.conditional_candidates.is_empty();
        Ok(())
    }

    pub(crate) fn build_artifacts(
        &self,
        tablet_root: &Path,
    ) -> Result<Vec<PackageArtifact>, TrustError> {
        if self.conditional_receipts.len() != self.conditional_candidates.len() {
            return Err(TrustError::new(
                "package_conditional_receipts_incomplete",
                "build requires one exact journal-bound receipt for every current selected conditional candidate",
            ));
        }
        let mut receipts: Vec<Value> = self.journal_receipts.values().cloned().collect();
        receipts.extend(self.conditional_receipts.values().cloned());
        let mut artifacts = Vec::new();
        let mut sources = BTreeMap::<Sha256Digest, (String, Vec<u8>)>::new();
        for receipt in receipts {
            let digest = validate_receipt_self_identity(&receipt)?;
            artifacts.push(PackageArtifact {
                role: "proof_receipt".into(),
                path: format!("proof/receipts/{digest}.json"),
                bytes: canonical_json_value(&receipt)?,
            });
            for record in receipt_local_closure_records(&receipt)? {
                let node = string_field(record, "node")?;
                validate_flat_lean_node(node)?;
                let declared: Sha256Digest = string_field(record, "active_decl_hash")?.parse()?;
                let path = tablet_root.join(format!("{node}.lean"));
                let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                    TrustError::new(
                        "package_proof_source_unavailable",
                        format!("{}: {error}", path.display()),
                    )
                })?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(TrustError::new(
                        "package_proof_source_type_invalid",
                        format!("{} must be a regular non-symlink file", path.display()),
                    ));
                }
                let bytes = std::fs::read(&path).map_err(|error| {
                    TrustError::new(
                        "package_proof_source_unreadable",
                        format!("{}: {error}", path.display()),
                    )
                })?;
                if raw_sha256(&bytes) != declared {
                    return Err(TrustError::new(
                        "package_proof_source_body_mismatch",
                        format!("{node}.lean differs from its journal/runtime proof receipt"),
                    ));
                }
                match sources.entry(declared) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert((node.to_owned(), bytes));
                    }
                    std::collections::btree_map::Entry::Occupied(entry)
                        if entry.get().1 != bytes =>
                    {
                        return Err(TrustError::new(
                            "package_proof_source_digest_collision",
                            "different proof source bytes claim the same SHA-256 digest",
                        ));
                    }
                    _ => {}
                }
            }
        }
        artifacts.extend(sources.into_iter().map(|(digest, (_node, bytes))| PackageArtifact {
            role: "proof_source".into(),
            path: format!("proof/sources/{digest}.lean"),
            bytes,
        }));
        Ok(artifacts)
    }

    pub(crate) fn current_tree_root(
        &self,
        tablet_root: &Path,
    ) -> Result<Sha256Digest, TrustError> {
        let campaign_root = tablet_root.parent().ok_or_else(|| {
            TrustError::new(
                "package_tablet_root_invalid",
                "Tablet root has no campaign-repository parent",
            )
        })?;
        let mut artifacts = self.build_artifacts(tablet_root)?;
        for (path, source) in [
            ("proof/lean-toolchain", campaign_root.join("lean-toolchain")),
            (
                "proof/lake-manifest.json",
                campaign_root.join("lake-manifest.json"),
            ),
            (
                "proof/Tablet/Preamble.lean",
                tablet_root.join("Preamble.lean"),
            ),
        ] {
            let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
                TrustError::new(
                    "package_proof_toolchain_input_unavailable",
                    format!("{}: {error}", source.display()),
                )
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(TrustError::new(
                    "package_proof_toolchain_input_type_invalid",
                    format!("{} must be a regular non-symlink file", source.display()),
                ));
            }
            artifacts.push(PackageArtifact {
                role: "toolchain_input".into(),
                path: path.into(),
                bytes: std::fs::read(&source).map_err(|error| {
                    TrustError::new(
                        "package_proof_toolchain_input_unreadable",
                        format!("{}: {error}", source.display()),
                    )
                })?,
            });
        }
        Ok(verify_proof_artifact_set(&artifacts, self)?.root)
    }
}

fn validate_flat_lean_node(node: &str) -> Result<(), TrustError> {
    if node.is_empty()
        || node.len() > 256
        || node.contains('/')
        || node.contains('\\')
        || node.chars().any(char::is_control)
    {
        return Err(TrustError::new(
            "package_proof_node_path_invalid",
            format!("proof node {node:?} cannot name a flat Tablet module"),
        ));
    }
    Ok(())
}

fn validate_receipt_self_identity(receipt: &Value) -> Result<Sha256Digest, TrustError> {
    let schema = string_field(receipt, "schema")?;
    if ![
        "trellis-local-closure-proof-receipt/v1",
        "trellis-witness-refutation-proof-receipt/v1",
        "trellis-conditional-local-closure-proof-receipt/v1",
    ]
    .contains(&schema)
    {
        return Err(TrustError::new(
            "package_proof_receipt_schema_invalid",
            format!("unregistered proof receipt schema {schema:?}"),
        ));
    }
    let digest = self_digest(DomainTag::RawArtifact, receipt, "proof_receipt_sha256")?;
    if value_digest(receipt, "proof_receipt_sha256")? != digest {
        return Err(TrustError::new(
            "package_proof_receipt_digest_mismatch",
            "proof receipt self-digest is invalid",
        ));
    }
    Ok(digest)
}

fn receipt_local_closure_records<'a>(receipt: &'a Value) -> Result<Vec<&'a Value>, TrustError> {
    match string_field(receipt, "schema")? {
        "trellis-local-closure-proof-receipt/v1"
        | "trellis-conditional-local-closure-proof-receipt/v1" => receipt
            .get("local_closure_record")
            .map(|record| vec![record])
            .ok_or_else(|| {
                TrustError::new(
                    "package_proof_local_closure_missing",
                    "proof receipt lacks local_closure_record",
                )
            }),
        "trellis-witness-refutation-proof-receipt/v1" => Ok(vec![
            receipt.get("witness_local_closure_record").ok_or_else(|| {
                TrustError::new(
                    "package_witness_local_closure_missing",
                    "witness proof receipt lacks witness_local_closure_record",
                )
            })?,
            receipt.get("not_t_local_closure_record").ok_or_else(|| {
                TrustError::new(
                    "package_not_t_local_closure_missing",
                    "witness proof receipt lacks not_t_local_closure_record",
                )
            })?,
        ]),
        _ => Err(TrustError::new(
            "package_proof_receipt_schema_invalid",
            "unregistered proof receipt schema",
        )),
    }
}

/// Construct byte-deterministic package bytes from a replay-audited campaign
/// frontier and a closed set of typed cold-review artifacts.
pub fn build_package_archive(
    readiness: &PackageReadiness,
    generator_and_validator_tool_root: Sha256Digest,
    mut artifacts: Vec<PackageArtifact>,
) -> Result<Vec<u8>, TrustError> {
    validate_package_artifacts(&artifacts, readiness.gate_presentation_sha256)?;
    let proof_tree = readiness
        .proof_requirements
        .as_ref()
        .map(|requirements| verify_proof_artifact_set(&artifacts, requirements))
        .transpose()?;
    if readiness.expected_proof_tree_root != proof_tree.as_ref().map(|tree| tree.root) {
        return Err(TrustError::new(
            "package_proof_tree_not_current",
            "archive proof tree differs from the current package-ready runtime/repository state",
        ));
    }
    artifacts.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let index_entries: Vec<_> = artifacts
        .iter()
        .map(|artifact| {
            serde_json::json!({
                "role": artifact.role,
                "path": artifact.path,
                "byte_length": artifact.bytes.len(),
                "sha256_of_raw_bytes": raw_sha256(&artifact.bytes),
            })
        })
        .collect();
    let mut packet_index = serde_json::json!({
        "schema": "trellis-packet-index/v1",
        "entries": index_entries,
        "packet_content_root": tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&Value::Array(index_entries.clone()))?,
        ),
        "packet_index_sha256": Sha256Digest::ZERO,
    });
    let packet_index_digest = self_digest(
        DomainTag::ManifestNode,
        &packet_index,
        "packet_index_sha256",
    )?;
    packet_index["packet_index_sha256"] = Value::String(packet_index_digest.to_string());
    let packet_index_bytes = canonical_json_value(&packet_index)?;
    let mut binding = serde_json::json!({
        "schema": "trellis-package-trust-binding/v1",
        "journal_id": readiness.journal_id,
        "pre_authorization_journal_head": readiness.journal_head,
        "human_approval_event_hash": readiness.human_approval_event_hash,
        "semantic_root": readiness.semantic_root,
        "derived_result_root": readiness.derived_result_root,
        "approved_evidence_tool_input_root": readiness.approved_evidence_tool_input_root,
        "target_count": readiness.target_count,
        "claim_rows_root": readiness.claim_rows_root,
        "claim_document_sha256": raw_sha256(&readiness.claim_document_bytes),
        "gate_presentation_sha256": readiness.gate_presentation_sha256,
        "packet_index_sha256": raw_sha256(&packet_index_bytes),
        "generator_and_validator_tool_root": generator_and_validator_tool_root,
        "binding_sha256": Sha256Digest::ZERO,
    });
    if let Some(tree) = &proof_tree {
        binding
            .as_object_mut()
            .expect("trust binding is an object")
            .insert(
                "proof_tree_root".into(),
                Value::String(tree.root.to_string()),
            );
    }
    let binding_digest = self_digest(DomainTag::ManifestNode, &binding, "binding_sha256")?;
    binding["binding_sha256"] = Value::String(binding_digest.to_string());
    let mut entries: Vec<(String, Vec<u8>)> = artifacts
        .into_iter()
        .map(|artifact| (artifact.path, artifact.bytes))
        .collect();
    entries.push((TRUST_BINDING_PATH.to_owned(), canonical_json_value(&binding)?));
    entries.push((CLAIM_DOCUMENT_PATH.to_owned(), readiness.claim_document_bytes.clone()));
    entries.push((PACKET_INDEX_PATH.to_owned(), packet_index_bytes));
    entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(TrustError::new(
            "package_artifact_path_duplicate",
            "package artifact paths collide with each other or reserved roots",
        ));
    }
    let manifest_entries: Vec<_> = entries
        .iter()
        .map(|(path, bytes)| {
            serde_json::json!({
                "path": path,
                "byte_length": bytes.len(),
                "sha256_of_raw_bytes": raw_sha256(bytes),
            })
        })
        .collect();
    let mut manifest = serde_json::json!({
        "schema": "trellis-package-manifest/v1",
        "entries": manifest_entries,
        "manifest_sha256": Sha256Digest::ZERO,
    });
    let manifest_digest = self_digest(DomainTag::ManifestNode, &manifest, "manifest_sha256")?;
    manifest["manifest_sha256"] = Value::String(manifest_digest.to_string());
    let manifest_bytes = canonical_json_value(&manifest)?;
    let cursor = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o100644);
    for (path, bytes) in entries {
        writer
            .start_file(path, options)
            .map_err(|error| TrustError::new("package_zip_write_failed", error.to_string()))?;
        writer
            .write_all(&bytes)
            .map_err(|error| TrustError::new("package_zip_write_failed", error.to_string()))?;
    }
    writer
        .start_file(super::archive::PACKAGE_MANIFEST_PATH, options)
        .map_err(|error| TrustError::new("package_zip_write_failed", error.to_string()))?;
    writer
        .write_all(&manifest_bytes)
        .map_err(|error| TrustError::new("package_zip_write_failed", error.to_string()))?;
    let archive = writer
        .finish()
        .map_err(|error| TrustError::new("package_zip_write_failed", error.to_string()))?
        .into_inner();
    verify_package_archive(&archive)?;
    verify_single_claim_presentation(&archive, CLAIM_DOCUMENT_PATH)?;
    Ok(archive)
}

fn verify_proof_artifact_set(
    artifacts: &[PackageArtifact],
    requirements: &ProofPackageRequirements,
) -> Result<VerifiedProofTree, TrustError> {
    let toolchain_artifacts: BTreeMap<_, _> = artifacts
        .iter()
        .filter(|artifact| artifact.role == "toolchain_input")
        .map(|artifact| (artifact.path.as_str(), artifact))
        .collect();
    let required_toolchain_digest = |path: &str| -> Result<Sha256Digest, TrustError> {
        toolchain_artifacts
            .get(path)
            .map(|artifact| raw_sha256(&artifact.bytes))
            .ok_or_else(|| {
                TrustError::new(
                    "package_proof_toolchain_input_missing",
                    format!("strict proof binding requires {path}"),
                )
            })
    };
    let packaged_lean_toolchain = required_toolchain_digest("proof/lean-toolchain")?;
    let packaged_lake_manifest = required_toolchain_digest("proof/lake-manifest.json")?;
    let packaged_preamble = required_toolchain_digest("proof/Tablet/Preamble.lean")?;
    let receipt_artifacts: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.role == "proof_receipt")
        .collect();
    let source_artifacts: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.role == "proof_source")
        .collect();
    let mut packaged_receipts = BTreeMap::<Sha256Digest, (&Value, &PackageArtifact)>::new();
    let mut parsed_receipts = Vec::with_capacity(receipt_artifacts.len());
    for artifact in receipt_artifacts {
        let value = parse_json_strict(&artifact.bytes).map_err(|error| {
            TrustError::new("package_proof_receipt_json_invalid", error.to_string())
        })?;
        if canonical_json_value(&value)? != artifact.bytes {
            return Err(TrustError::new(
                "package_proof_receipt_not_canonical",
                "packaged proof receipt must be exact canonical JSON",
            ));
        }
        let digest = validate_receipt_self_identity(&value)?;
        let expected_path = format!("proof/receipts/{digest}.json");
        if artifact.path != expected_path {
            return Err(TrustError::new(
                "package_proof_receipt_path_mismatch",
                format!("proof receipt {digest} must use {expected_path}"),
            ));
        }
        parsed_receipts.push((digest, value, artifact));
    }
    for (digest, value, artifact) in &parsed_receipts {
        if packaged_receipts.insert(*digest, (value, artifact)).is_some() {
            return Err(TrustError::new(
                "package_proof_receipt_duplicate",
                format!("proof receipt {digest} is packaged more than once"),
            ));
        }
    }

    let expected_journal: BTreeMap<_, _> = requirements
        .journal_receipts
        .values()
        .map(|receipt| Ok((validate_receipt_self_identity(receipt)?, receipt)))
        .collect::<Result<_, TrustError>>()?;
    for (digest, expected) in &expected_journal {
        let Some((actual, _)) = packaged_receipts.get(digest) else {
            return Err(TrustError::new(
                "package_proof_receipt_omitted",
                format!("journaled proof receipt {digest} is absent from the packet"),
            ));
        };
        if *actual != *expected {
            return Err(TrustError::new(
                "package_proof_receipt_swapped",
                format!("packaged receipt {digest} differs from the journaled formal result"),
            ));
        }
    }

    let mut seen_candidates = BTreeSet::new();
    for (digest, (receipt, _)) in &packaged_receipts {
        match string_field(receipt, "schema")? {
            "trellis-conditional-local-closure-proof-receipt/v1" => {
                let candidate_digest = value_digest(receipt, "candidate_definition_sha256")?;
                let candidate = requirements
                    .conditional_candidates
                    .get(&candidate_digest)
                    .ok_or_else(|| {
                        TrustError::new(
                            "package_proof_receipt_extra",
                            format!(
                                "conditional receipt {digest} names no current journal-selected candidate"
                            ),
                        )
                    })?;
                super::pipeline::validate_conditional_candidate_proof_receipt(
                    candidate,
                    receipt,
                )?;
                if let Some(expected) = requirements.conditional_receipts.get(&candidate_digest) {
                    if *receipt != expected {
                        return Err(TrustError::new(
                            "package_conditional_receipt_stale",
                            format!(
                                "conditional receipt {digest} differs from the package-ready runtime checkpoint"
                            ),
                        ));
                    }
                }
                if !seen_candidates.insert(candidate_digest) {
                    return Err(TrustError::new(
                        "package_conditional_receipt_duplicate",
                        "current selected conditional candidate has multiple packaged proof receipts",
                    ));
                }
            }
            _ if !expected_journal.contains_key(digest) => {
                return Err(TrustError::new(
                    "package_proof_receipt_extra",
                    format!("proof receipt {digest} is unrelated to the current formal results"),
                ));
            }
            _ => {}
        }
    }
    let expected_candidates: BTreeSet<_> =
        requirements.conditional_candidates.keys().copied().collect();
    if seen_candidates != expected_candidates {
        return Err(TrustError::new(
            "package_conditional_receipt_omitted",
            "packet lacks the exact journal-bound receipt for a current selected conditional candidate",
        ));
    }
    if packaged_receipts.len() != expected_journal.len() + expected_candidates.len() {
        return Err(TrustError::new(
            "package_proof_receipt_set_mismatch",
            "packaged receipt set is not the exact target-plus-candidate inventory",
        ));
    }

    let mut source_by_path = BTreeMap::new();
    for artifact in source_artifacts {
        if source_by_path.insert(artifact.path.as_str(), artifact).is_some() {
            return Err(TrustError::new(
                "package_proof_source_duplicate",
                "proof source path occurs more than once",
            ));
        }
    }
    let mut required_source_paths = BTreeSet::new();
    let mut tree_leaves = Vec::new();
    for (receipt_digest, (receipt, artifact)) in &packaged_receipts {
        let mut source_leaves = Vec::new();
        for record in receipt_local_closure_records(receipt)? {
            let node = string_field(record, "node")?;
            validate_flat_lean_node(node)?;
            let active_decl = nonzero_digest_field(record, "active_decl_hash")?;
            let active_statement = nonzero_digest_field(record, "active_statement_hash")?;
            let preamble = nonzero_digest_field(record, "preamble_hash")?;
            let toolchain = nonzero_digest_field(record, "toolchain_hash")?;
            let lake_manifest = nonzero_digest_field(record, "lake_manifest_hash")?;
            nonzero_digest_field(record, "approved_axioms_hash")?;
            if toolchain != packaged_lean_toolchain
                || lake_manifest != packaged_lake_manifest
                || preamble != packaged_preamble
            {
                return Err(TrustError::new(
                    "package_proof_toolchain_binding_mismatch",
                    format!(
                        "proof receipt for {node} differs from packaged Lean toolchain, Lake manifest, or preamble"
                    ),
                ));
            }
            validate_local_closure_dependency_maps(record)?;
            let source_path = format!("proof/sources/{active_decl}.lean");
            required_source_paths.insert(source_path.clone());
            let source = source_by_path.get(source_path.as_str()).ok_or_else(|| {
                TrustError::new(
                    "package_proof_source_omitted",
                    format!("receipt {receipt_digest} requires source {source_path}"),
                )
            })?;
            if raw_sha256(&source.bytes) != active_decl {
                return Err(TrustError::new(
                    "package_proof_source_body_mismatch",
                    format!("packaged proof body for {node} differs from its receipt"),
                ));
            }
            let dependency_root = tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&serde_json::json!({
                    "boundary_theorems": record.get("boundary_theorems"),
                    "strict_theorem_deps": record.get("strict_theorem_deps"),
                    "strict_definition_deps": record.get("strict_definition_deps"),
                    "kernel_semantic_hashes": record.get("kernel_semantic_hashes"),
                }))?,
            );
            source_leaves.push(serde_json::json!({
                "node_id": node,
                "source_path": source_path,
                "source_body_sha256": active_decl,
                "active_statement_sha256": active_statement,
                "preamble_sha256": preamble,
                "dependency_closure_root": dependency_root,
            }));
        }
        source_leaves.sort_by(|left, right| {
            left["node_id"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .cmp(right["node_id"].as_str().unwrap_or("").as_bytes())
        });
        tree_leaves.push(serde_json::json!({
            "receipt_sha256": receipt_digest,
            "receipt_path": artifact.path,
            "receipt_schema": string_field(receipt, "schema")?,
            "target_id": string_field(receipt, "target_id")?,
            "sources": source_leaves,
        }));
    }
    let packaged_source_paths: BTreeSet<_> = source_by_path.keys().map(|path| (*path).to_owned()).collect();
    if packaged_source_paths != required_source_paths {
        return Err(TrustError::new(
            "package_proof_source_set_mismatch",
            "packet contains an omitted, unrelated, or stale proof source",
        ));
    }
    tree_leaves.sort_by(|left, right| {
        left["receipt_sha256"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["receipt_sha256"].as_str().unwrap_or("").as_bytes())
    });
    Ok(VerifiedProofTree {
        root: tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&Value::Array(tree_leaves))?,
        ),
    })
}

fn nonzero_digest_field(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    let digest = value_digest(value, field)?;
    if digest == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "package_proof_closure_zero_digest",
            format!("local closure {field} cannot use the zero sentinel"),
        ));
    }
    Ok(digest)
}

fn validate_local_closure_dependency_maps(record: &Value) -> Result<(), TrustError> {
    for field in [
        "boundary_theorems",
        "strict_theorem_deps",
        "strict_definition_deps",
        "kernel_semantic_hashes",
    ] {
        let map = record.get(field).and_then(Value::as_object).ok_or_else(|| {
            TrustError::new(
                "package_proof_dependency_map_missing",
                format!("local closure lacks object field {field}"),
            )
        })?;
        for (dependency, value) in map {
            let digest: Sha256Digest = value
                .as_str()
                .ok_or_else(|| {
                    TrustError::new(
                        "package_proof_dependency_digest_invalid",
                        format!("dependency {dependency} in {field} is not a digest"),
                    )
                })?
                .parse()?;
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "package_proof_dependency_digest_zero",
                    format!("dependency {dependency} in {field} uses the zero sentinel"),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct CurrentEpochConditionalSelection {
    target_id: String,
    profile: AuthoritativeRecord,
    candidate: AuthoritativeRecord,
    selection_event_hash: Sha256Digest,
}

fn derive_current_epoch_conditional_inventory(
    seed: &VerifiedSeedDefinitionClosure,
    bundles: &[Value],
    approval_sequence: u64,
) -> Result<
    (
        BTreeMap<Sha256Digest, AuthoritativeRecord>,
        BTreeMap<Sha256Digest, Value>,
    ),
    TrustError,
> {
    let mut selections_by_event = BTreeMap::<Sha256Digest, CurrentEpochConditionalSelection>::new();
    let mut candidates = BTreeMap::<Sha256Digest, AuthoritativeRecord>::new();
    let mut selected_targets = BTreeSet::new();

    for bundle in bundles {
        let event = decode_event(bundle)?;
        if event.sequence_number <= approval_sequence
            || event.event_kind != EventKind::ApprovedProfileSelected
        {
            continue;
        }
        let payload = decode_payload(bundle)?;
        let profile = seed
            .records_by_digest
            .get(&payload.subject_sha256)
            .filter(|record| {
                record.contract().record_schema == "trellis-qualification-profile/v1"
            })
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "package_selected_profile_not_seed_frozen",
                    format!(
                        "current profile-selection event {} names no exact seed profile",
                        event.event_hash
                    ),
                )
            })?;
        if bundle.get("subject_json") != Some(profile.value()) {
            return Err(TrustError::new(
                "package_selected_profile_bytes_mismatch",
                "profile-selection subject bytes differ from the exact seed profile",
            ));
        }
        let target_id = string_field(profile.value(), "target_id")?.to_owned();
        if payload.subject_id != target_id {
            return Err(TrustError::new(
                "package_selected_profile_target_mismatch",
                "profile-selection subject ID differs from its seed target",
            ));
        }
        if !selected_targets.insert(target_id.clone()) {
            return Err(TrustError::new(
                "package_selected_profile_duplicate",
                format!("target {target_id} has multiple current-epoch profile selections"),
            ));
        }
        let matching_candidates: Vec<_> = seed
            .records_by_digest
            .values()
            .filter(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
                    && value_digest(record.value(), "profile_definition_sha256")
                        .ok()
                        == Some(profile.digest())
                    && string_field(record.value(), "target_id").ok()
                        == Some(target_id.as_str())
            })
            .cloned()
            .collect();
        if matching_candidates.len() != 1 {
            return Err(TrustError::new(
                "package_selected_candidate_not_unique",
                format!(
                    "selected profile {} resolves to {} seed conditional candidates",
                    profile.digest(),
                    matching_candidates.len()
                ),
            ));
        }
        let candidate = matching_candidates[0].clone();
        if candidates
            .insert(candidate.digest(), candidate.clone())
            .is_some()
        {
            return Err(TrustError::new(
                "package_selected_candidate_duplicate",
                "one seed conditional candidate is selected more than once in the current epoch",
            ));
        }
        selections_by_event.insert(
            event.event_hash,
            CurrentEpochConditionalSelection {
                target_id,
                profile,
                candidate,
                selection_event_hash: event.event_hash,
            },
        );
    }

    let mut receipts = BTreeMap::<Sha256Digest, Value>::new();
    for bundle in bundles {
        let event = decode_event(bundle)?;
        if event.sequence_number <= approval_sequence
            || event.event_kind != EventKind::ConditionalStatementGenerated
        {
            continue;
        }
        let payload = decode_payload(bundle)?;
        let envelope = decode_raw_json_subject(bundle, "generated conditional statement")?;
        let selection_event_hash = value_digest(&envelope, "profile_selection_event_hash")?;
        let selection = selections_by_event
            .get(&selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "package_conditional_selection_missing",
                    "current generated-conditional event names no current-epoch profile selection",
                )
            })?;
        if event.previous_event_hash != selection.selection_event_hash
            || payload.subject_id != selection.target_id
            || string_field(&envelope, "target_id")? != selection.target_id
            || value_digest(&envelope, "profile_sha256")? != selection.profile.digest()
        {
            return Err(TrustError::new(
                "package_conditional_selection_binding_mismatch",
                "generated conditional statement differs from its exact selected target or profile",
            ));
        }
        let inputs = envelope
            .get("qualification_inputs")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                TrustError::new(
                    "package_conditional_inputs_missing",
                    "generated conditional statement lacks qualification_inputs",
                )
            })?;
        if inputs.get("profile") != Some(selection.profile.value())
            || inputs.get("conditional_theorem_candidate") != Some(selection.candidate.value())
        {
            return Err(TrustError::new(
                "package_conditional_seed_inputs_mismatch",
                "generated conditional inputs differ from the exact selected seed profile or \
                 candidate",
            ));
        }
        if string_field(&envelope, "statement_utf8")?
            != string_field(selection.candidate.value(), "statement_utf8")?
            || value_digest(&envelope, "statement_sha256")?
                != value_digest(
                    selection.candidate.value(),
                    "conditional_statement_sha256",
                )?
        {
            return Err(TrustError::new(
                "package_conditional_statement_candidate_mismatch",
                "generated theorem bytes differ from the exact selected seed candidate",
            ));
        }
        let receipt = inputs
            .get("conditional_proof_receipt")
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "package_conditional_receipt_missing",
                    "generated conditional inputs lack their checked candidate receipt",
                )
            })?;
        validate_receipt_self_identity(&receipt)?;
        super::pipeline::validate_conditional_candidate_proof_receipt(
            &selection.candidate,
            &receipt,
        )?;
        if receipts
            .insert(selection.candidate.digest(), receipt)
            .is_some()
        {
            return Err(TrustError::new(
                "package_conditional_receipt_duplicate",
                "selected candidate has multiple current generated-conditional receipts",
            ));
        }
    }
    if receipts.len() != candidates.len() {
        return Err(TrustError::new(
            "package_conditional_receipt_incomplete",
            format!(
                "{} current profile selections require generated-conditional receipts but only \
                 {} resolve",
                candidates.len(),
                receipts.len()
            ),
        ));
    }
    Ok((candidates, receipts))
}

pub(crate) fn derive_proof_requirements(
    seed: &VerifiedSeedDefinitionClosure,
    bundles: &[Value],
) -> Result<ProofPackageRequirements, TrustError> {
    let approval_sequence = bundles
        .iter()
        .map(decode_event)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|event| {
            matches!(
                event.event_kind,
                EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved
            )
        })
        .map(|event| event.sequence_number)
        .max()
        .ok_or_else(|| {
            TrustError::new(
                "package_proof_approval_missing",
                "proof inventory requires a current approved gate epoch",
            )
        })?;
    let mut expected_results = BTreeMap::<String, (String, Sha256Digest)>::new();
    for bundle in bundles {
        let event = decode_event(bundle)?;
        if event.sequence_number <= approval_sequence
            || event.event_kind != EventKind::ExternalClaimRowsGenerated
        {
            continue;
        }
        let claim = decode_raw_json_subject(bundle, "proof-inventory claim")?;
        let target = string_field(&claim, "target_id")?.to_owned();
        let kind = string_field(&claim, "result_kind")?.to_owned();
        let digest = match kind.as_str() {
            "positive_proof" => value_digest(&claim, "positive_proof_subject_sha256")?,
            "checked_negative_proof" => {
                value_digest(&claim, "negative_proof_subject_sha256")?
            }
            "formal_refutation" => value_digest(&claim, "formal_refutation_sha256")?,
            _ => {
                return Err(TrustError::new(
                    "package_proof_claim_kind_invalid",
                    format!("target {target} has unknown proof result kind {kind:?}"),
                ))
            }
        };
        if expected_results
            .insert(target.clone(), (kind, digest))
            .is_some()
        {
            return Err(TrustError::new(
                "package_proof_claim_duplicate",
                format!("target {target} has multiple active claim rows"),
            ));
        }
    }
    if expected_results.is_empty() {
        return Err(TrustError::new(
            "package_proof_claim_inventory_empty",
            "cannot derive proof inventory without terminal external claim rows",
        ));
    }
    let mut journal_receipts = BTreeMap::new();
    for bundle in bundles {
        let event = decode_event(bundle)?;
        if event.sequence_number <= approval_sequence {
            continue;
        }
        let payload = decode_payload(bundle)?;
        let Some((expected_kind, expected_digest)) = expected_results.get(&payload.subject_id)
        else {
            continue;
        };
        let receipt = match event.event_kind {
            EventKind::ProofChecked
                if matches!(
                    expected_kind.as_str(),
                    "positive_proof" | "checked_negative_proof"
                ) && payload.subject_sha256 == *expected_digest =>
            {
                let envelope = decode_raw_json_subject(bundle, "checked proof")?;
                envelope.get("proof_receipt").cloned()
            }
            EventKind::WitnessRefutationChecked
                if expected_kind == "formal_refutation"
                    && payload.subject_sha256 == *expected_digest =>
            {
                bundle
                    .get("subject_json")
                    .and_then(|formal| formal.get("formal_proof_receipt"))
                    .cloned()
            }
            _ => None,
        };
        if let Some(receipt) = receipt {
            let identity = format!("{expected_kind}:{}", payload.subject_id);
            if journal_receipts.insert(identity.clone(), receipt).is_some() {
                return Err(TrustError::new(
                    "package_journal_proof_receipt_ambiguous",
                    format!("current formal result {identity} resolves to multiple receipts"),
                ));
            }
        }
    }
    if journal_receipts.len() != expected_results.len() {
        return Err(TrustError::new(
            "package_journal_proof_receipt_incomplete",
            format!(
                "{} terminal results require receipts but only {} resolve from the journal",
                expected_results.len(),
                journal_receipts.len()
            ),
        ));
    }
    let (conditional_candidates, conditional_receipts) =
        derive_current_epoch_conditional_inventory(seed, bundles, approval_sequence)?;
    let mut requirements =
        ProofPackageRequirements::new(journal_receipts, conditional_candidates)?;
    requirements.bind_journal_conditional_receipts(conditional_receipts)?;
    Ok(requirements)
}

fn decode_raw_json_subject(bundle: &Value, description: &str) -> Result<Value, TrustError> {
    let encoded = bundle
        .get("subject_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "package_raw_subject_missing",
                format!("{description} event lacks subject_base64"),
            )
        })?;
    let bytes = BASE64_STANDARD.decode(encoded).map_err(|error| {
        TrustError::new("package_raw_subject_base64_invalid", error.to_string())
    })?;
    let value = parse_json_strict(&bytes).map_err(|error| {
        TrustError::new("package_raw_subject_json_invalid", error.to_string())
    })?;
    if canonical_json_value(&value)? != bytes {
        return Err(TrustError::new(
            "package_raw_subject_not_canonical",
            format!("{description} subject is not canonical JSON"),
        ));
    }
    Ok(value)
}

fn verify_packaged_proof_tree(
    archive_bytes: &[u8],
    index: &VerifiedPacketIndex,
    requirements: &ProofPackageRequirements,
) -> Result<VerifiedProofTree, TrustError> {
    let mut artifacts = Vec::new();
    for role in ["proof_receipt", "proof_source", "toolchain_input"] {
        for path in index.paths_by_role.get(role).into_iter().flatten() {
            artifacts.push(PackageArtifact {
                role: role.to_owned(),
                path: path.clone(),
                bytes: read_package_member(
                    archive_bytes,
                    path,
                    MAX_PACKET_ARTIFACT_BYTES,
                )?,
            });
        }
    }
    verify_proof_artifact_set(&artifacts, requirements)
}

fn validate_package_artifacts(
    artifacts: &[PackageArtifact],
    approved_gate_presentation: Sha256Digest,
) -> Result<(), TrustError> {
    let mut paths = std::collections::BTreeSet::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let allowed_roles = [
        "actor_key_manifest",
        "evidence_manifest",
        "evidence_leaf",
        "gate_presentation",
        "proof_receipt",
        "proof_source",
        "schema",
        "seed_definition_bundle",
        "seed_manifest",
        "source_snapshot",
        "toolchain_input",
    ];
    for artifact in artifacts {
        if !allowed_roles.contains(&artifact.role.as_str()) {
            return Err(TrustError::new(
                "package_artifact_role_invalid",
                format!("unregistered package artifact role {:?}", artifact.role),
            ));
        }
        if artifact.bytes.is_empty()
            || !paths.insert(artifact.path.clone())
            || [
                TRUST_BINDING_PATH,
                CLAIM_DOCUMENT_PATH,
                PACKET_INDEX_PATH,
                super::archive::PACKAGE_MANIFEST_PATH,
            ]
            .contains(&artifact.path.as_str())
        {
            return Err(TrustError::new(
                "package_artifact_invalid",
                "artifacts must be non-empty, uniquely named, and outside reserved roots",
            ));
        }
        *counts.entry(&artifact.role).or_default() += 1;
    }
    for role in [
        "actor_key_manifest",
        "evidence_manifest",
        "gate_presentation",
        "seed_definition_bundle",
        "seed_manifest",
    ] {
        if counts.get(role) != Some(&1) {
            return Err(TrustError::new(
                "package_artifact_required_role_count",
                format!("package needs exactly one {role} artifact"),
            ));
        }
    }
    for role in [
        "proof_receipt",
        "proof_source",
        "schema",
        "source_snapshot",
        "toolchain_input",
    ] {
        if counts.get(role).copied().unwrap_or(0) == 0 {
            return Err(TrustError::new(
                "package_artifact_required_role_missing",
                format!("package needs at least one {role} artifact"),
            ));
        }
    }
    let gate = artifacts
        .iter()
        .find(|artifact| artifact.role == "gate_presentation")
        .expect("role count checked above");
    if tagged_hash(DomainTag::GatePresentation, &gate.bytes) != approved_gate_presentation {
        return Err(TrustError::new(
            "package_gate_presentation_mismatch",
            "packaged gate bytes differ from the currently approved presentation",
        ));
    }
    Ok(())
}

fn single_packet_role_path<'a>(
    index: &'a VerifiedPacketIndex,
    role: &str,
) -> Result<&'a str, TrustError> {
    match index.paths_by_role.get(role).map(Vec::as_slice) {
        Some([path]) => Ok(path),
        _ => Err(TrustError::new(
            "package_singleton_role_invalid",
            format!("packet needs exactly one {role} artifact"),
        )),
    }
}

fn verify_packaged_evidence(
    archive_bytes: &[u8],
    index: &VerifiedPacketIndex,
    approved_root: Sha256Digest,
) -> Result<(), TrustError> {
    let manifest_path = single_packet_role_path(index, "evidence_manifest")?;
    let bytes = read_package_member(
        archive_bytes,
        manifest_path,
        MAX_PACKET_ARTIFACT_BYTES,
    )?;
    let manifest = parse_json_strict(&bytes)
        .map_err(|error| TrustError::new("packaged_evidence_json_invalid", error.to_string()))?;
    if canonical_json_value(&manifest)? != bytes
        || manifest.get("schema").and_then(Value::as_str)
            != Some("trellis-evidence-tool-manifest/v1")
    {
        return Err(TrustError::new(
            "packaged_evidence_manifest_invalid",
            "packaged evidence manifest is not exact canonical v1 JSON",
        ));
    }
    verify_self_digest(DomainTag::ManifestNode, &manifest, "manifest_sha256")?;
    let leaves = manifest.get("leaves").and_then(Value::as_array).ok_or_else(|| {
        TrustError::new("packaged_evidence_leaves_missing", "evidence manifest lacks leaves")
    })?;
    let indexed_paths: BTreeSet<_> = index
        .paths_by_role
        .get("evidence_leaf")
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let mut prior_sort_key: Option<Vec<u8>> = None;
    let mut logical_ids = BTreeSet::new();
    let mut relative_paths = BTreeSet::new();
    let mut dependencies: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut expected_paths = BTreeSet::new();
    for leaf in leaves {
        validate_packaged_evidence_leaf_fields(leaf)?;
        let kind = string_field(leaf, "kind")?;
        let logical_id = string_field(leaf, "logical_id")?;
        let relative = string_field(leaf, "relative_path")?;
        validate_packaged_evidence_relative_path(relative)?;
        let sort_key = canonical_json_value(&serde_json::json!([
            kind,
            logical_id,
            relative
        ]))?;
        if prior_sort_key
            .as_ref()
            .is_some_and(|previous| previous >= &sort_key)
        {
            return Err(TrustError::new(
                "packaged_evidence_order_invalid",
                "evidence leaves must be strictly sorted by kind, logical_id, relative_path",
            ));
        }
        prior_sort_key = Some(sort_key);
        if !logical_ids.insert(logical_id.to_owned())
            || !relative_paths.insert(relative.to_owned())
        {
            return Err(TrustError::new(
                "packaged_evidence_duplicate_leaf",
                "evidence logical IDs and relative paths must be unique",
            ));
        }
        let dependency_values = leaf
            .get("dependency_ids")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TrustError::new(
                    "packaged_evidence_dependencies_missing",
                    "evidence dependency_ids must be an array",
                )
            })?;
        let mut prior_dependency: Option<&str> = None;
        let mut decoded_dependencies = Vec::new();
        for dependency in dependency_values {
            let dependency = dependency.as_str().ok_or_else(|| {
                TrustError::new(
                    "packaged_evidence_dependency_invalid",
                    "evidence dependency IDs must be strings",
                )
            })?;
            if prior_dependency
                .is_some_and(|previous| previous.as_bytes() >= dependency.as_bytes())
            {
                return Err(TrustError::new(
                    "packaged_evidence_dependency_order_invalid",
                    "evidence dependency IDs must be unique UTF-8 byte sorted",
                ));
            }
            prior_dependency = Some(dependency);
            decoded_dependencies.push(dependency.to_owned());
        }
        dependencies.insert(logical_id.to_owned(), decoded_dependencies);
        validate_packaged_evidence_producer(leaf)?;
        let package_path = format!("evidence/{relative}");
        expected_paths.insert(package_path.clone());
        let body = read_package_member(
            archive_bytes,
            &package_path,
            MAX_PACKET_ARTIFACT_BYTES,
        )?;
        let declared_length = leaf
            .get("byte_length")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                TrustError::new("packaged_evidence_length_invalid", "leaf length is invalid")
            })?;
        let declared_digest: Sha256Digest = string_field(leaf, "sha256_of_raw_bytes")?.parse()?;
        if body.len() as u64 != declared_length || raw_sha256(&body) != declared_digest {
            return Err(TrustError::new(
                "packaged_evidence_leaf_mismatch",
                format!("packaged evidence leaf {relative:?} differs from its manifest"),
            ));
        }
    }
    for (logical_id, dependency_ids) in &dependencies {
        for dependency_id in dependency_ids {
            if !logical_ids.contains(dependency_id) {
                return Err(TrustError::new(
                    "packaged_evidence_dependency_missing",
                    format!("{logical_id} depends on missing {dependency_id}"),
                ));
            }
        }
    }
    reject_packaged_evidence_dependency_cycles(&dependencies)?;
    if expected_paths != indexed_paths {
        return Err(TrustError::new(
            "packaged_evidence_leaf_set_mismatch",
            "typed evidence-leaf set differs from the approved evidence manifest",
        ));
    }
    let computed_root = tagged_hash(
        DomainTag::EvidenceToolRoot,
        &canonical_json_value(&Value::Array(leaves.clone()))?,
    );
    if computed_root != approved_root
        || manifest
            .get("evidence_tool_input_root")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Sha256Digest>().ok())
            != Some(computed_root)
    {
        return Err(TrustError::new(
            "packaged_evidence_root_mismatch",
            "packaged evidence closure differs from the signed approval root",
        ));
    }
    Ok(())
}

fn validate_packaged_evidence_leaf_fields(leaf: &Value) -> Result<(), TrustError> {
    let object = leaf.as_object().ok_or_else(|| {
        TrustError::new(
            "packaged_evidence_leaf_invalid",
            "each packaged evidence leaf must be an object",
        )
    })?;
    const REQUIRED: [&str; 6] = [
        "kind",
        "logical_id",
        "relative_path",
        "byte_length",
        "sha256_of_raw_bytes",
        "dependency_ids",
    ];
    const ALLOWED: [&str; 9] = [
        "kind",
        "logical_id",
        "relative_path",
        "byte_length",
        "sha256_of_raw_bytes",
        "dependency_ids",
        "schema_id",
        "producer_id",
        "producer_hash",
    ];
    if REQUIRED.iter().any(|field| !object.contains_key(*field))
        || object.keys().any(|field| !ALLOWED.contains(&field.as_str()))
    {
        return Err(TrustError::new(
            "packaged_evidence_leaf_not_closed",
            "packaged evidence leaves contain exactly the registered leaf fields",
        ));
    }
    if leaf.get("schema_id").is_some_and(|value| !value.is_string()) {
        return Err(TrustError::new(
            "packaged_evidence_schema_id_invalid",
            "evidence schema_id must be a string when present",
        ));
    }
    Ok(())
}

fn validate_packaged_evidence_relative_path(path: &str) -> Result<(), TrustError> {
    let candidate = PathBuf::from(path);
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || candidate
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(TrustError::new(
            "packaged_evidence_relative_path_invalid",
            format!("invalid relative POSIX evidence path {path:?}"),
        ));
    }
    Ok(())
}

fn validate_packaged_evidence_producer(leaf: &Value) -> Result<(), TrustError> {
    match (leaf.get("producer_id"), leaf.get("producer_hash")) {
        (None, None) => Ok(()),
        (Some(producer_id), Some(producer_hash)) => {
            if producer_id.as_str().is_none()
                || producer_hash
                    .as_str()
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    .is_none()
            {
                return Err(TrustError::new(
                    "packaged_evidence_producer_invalid",
                    "producer_id must be a string and producer_hash must be a SHA-256 digest",
                ));
            }
            Ok(())
        }
        _ => Err(TrustError::new(
            "packaged_evidence_optional_pair_incomplete",
            "producer_id and producer_hash must occur together",
        )),
    }
}

fn reject_packaged_evidence_dependency_cycles(
    graph: &BTreeMap<String, Vec<String>>,
) -> Result<(), TrustError> {
    fn visit(
        logical_id: &str,
        graph: &BTreeMap<String, Vec<String>>,
        visiting: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> Result<(), TrustError> {
        if complete.contains(logical_id) {
            return Ok(());
        }
        if !visiting.insert(logical_id.to_owned()) {
            return Err(TrustError::new(
                "packaged_evidence_dependency_cycle",
                format!("evidence dependency cycle reaches {logical_id}"),
            ));
        }
        for dependency in graph.get(logical_id).into_iter().flatten() {
            visit(dependency, graph, visiting, complete)?;
        }
        visiting.remove(logical_id);
        complete.insert(logical_id.to_owned());
        Ok(())
    }

    let mut complete = BTreeSet::new();
    for logical_id in graph.keys() {
        visit(logical_id, graph, &mut BTreeSet::new(), &mut complete)?;
    }
    Ok(())
}

fn verify_packet_index(
    archive_bytes: &[u8],
    expected_gate_presentation: Sha256Digest,
) -> Result<VerifiedPacketIndex, TrustError> {
    let index_bytes = read_package_member(
        archive_bytes,
        PACKET_INDEX_PATH,
        MAX_PACKET_INDEX_BYTES,
    )?;
    let index = parse_json_strict(&index_bytes)
        .map_err(|error| TrustError::new("packet_index_json_invalid", error.to_string()))?;
    if canonical_json_value(&index)? != index_bytes
        || index.get("schema").and_then(Value::as_str) != Some("trellis-packet-index/v1")
    {
        return Err(TrustError::new(
            "packet_index_identity_invalid",
            "packet index must be exact canonical v1 JSON",
        ));
    }
    verify_self_digest(DomainTag::ManifestNode, &index, "packet_index_sha256")?;
    let entries = index.get("entries").and_then(Value::as_array).ok_or_else(|| {
        TrustError::new("packet_index_entries_missing", "packet index lacks entries")
    })?;
    let expected_root = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&Value::Array(entries.clone()))?,
    );
    if index
        .get("packet_content_root")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Sha256Digest>().ok())
        != Some(expected_root)
    {
        return Err(TrustError::new(
            "packet_content_root_mismatch",
            "packet content root does not match the typed artifact entries",
        ));
    }
    let mut paths_by_role: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut prior_path: Option<&str> = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut artifact_metadata = Vec::new();
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            TrustError::new("packet_index_entry_invalid", "packet index entry is not an object")
        })?;
        if object.len() != 4 {
            return Err(TrustError::new(
                "packet_index_entry_not_closed",
                "packet index entries have exactly role, path, length, and digest",
            ));
        }
        let role = string_field(entry, "role")?;
        let path = string_field(entry, "path")?;
        if prior_path.is_some_and(|previous| previous.as_bytes() >= path.as_bytes())
            || !seen.insert(path.to_owned())
        {
            return Err(TrustError::new(
                "packet_index_order_or_duplicate",
                "packet index paths must be unique and UTF-8 byte sorted",
            ));
        }
        prior_path = Some(path);
        let declared_length = entry
            .get("byte_length")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                TrustError::new("packet_index_length_invalid", "artifact length is invalid")
            })?;
        let declared_digest: Sha256Digest = string_field(entry, "sha256_of_raw_bytes")?.parse()?;
        let bytes = read_package_member(archive_bytes, path, MAX_PACKET_ARTIFACT_BYTES)?;
        if bytes.len() as u64 != declared_length || raw_sha256(&bytes) != declared_digest {
            return Err(TrustError::new(
                "packet_index_artifact_mismatch",
                format!("packet artifact {path:?} differs from its typed index"),
            ));
        }
        paths_by_role
            .entry(role.to_owned())
            .or_default()
            .push(path.to_owned());
        artifact_metadata.push(PackageArtifact {
            role: role.to_owned(),
            path: path.to_owned(),
            bytes,
        });
    }
    let archive_artifact_paths: std::collections::BTreeSet<_> = {
        let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes)).map_err(|error| {
            TrustError::new("package_zip_invalid", format!("cannot open ZIP: {error}"))
        })?;
        let mut paths = std::collections::BTreeSet::new();
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index).map_err(|error| {
                TrustError::new("package_zip_entry_invalid", error.to_string())
            })?;
            if !entry.is_dir()
                && ![
                    TRUST_BINDING_PATH,
                    CLAIM_DOCUMENT_PATH,
                    PACKET_INDEX_PATH,
                    super::archive::PACKAGE_MANIFEST_PATH,
                ]
                .contains(&entry.name())
            {
                paths.insert(entry.name().to_owned());
            }
        }
        paths
    };
    if seen != archive_artifact_paths {
        return Err(TrustError::new(
            "packet_index_not_complete",
            "typed packet index must name every non-reserved archive artifact exactly once",
        ));
    }
    validate_package_artifacts(&artifact_metadata, expected_gate_presentation)?;
    Ok(VerifiedPacketIndex {
        raw_sha256: raw_sha256(&index_bytes),
        paths_by_role,
    })
}

/// Commit and authenticate authorization for `archive_bytes` and return the
/// exact canonical detached sidecar.  A retry after a crash between durable
/// journal commit and receipt creation is accepted only when the transaction,
/// archive digest, generator closure, and committed head all match exactly.
pub(crate) fn authorize_package(
    journal: &mut TrustJournal,
    archive_bytes: &[u8],
    request: PackageAuthorizationRequest<'_>,
    readiness: &PackageReadiness,
) -> Result<AuthorizedPackage, TrustError> {
    if readiness
        .proof_requirements
        .as_ref()
        .is_some_and(|requirements| !requirements.runtime_conditional_receipts_validated)
    {
        return Err(TrustError::new(
            "package_authorization_runtime_proof_state_missing",
            "conditional-candidate package authorization requires the current package-ready runtime checkpoint",
        ));
    }
    verify_package_archive(archive_bytes)?;
    verify_single_claim_presentation(archive_bytes, CLAIM_DOCUMENT_PATH)?;
    let registry = SchemaRegistry::v1()?;
    let presentation = tagged_hash(DomainTag::PackagePresentation, archive_bytes);
    let approval = journal.current_approval().ok_or_else(|| {
        TrustError::new(
            "package_without_current_approval",
            "package authorization requires a current signed, unrevoked human approval",
        )
    })?;
    let committed_package_event = journal.committed_transaction_event_hash(request.transaction_id);
    let recovering_committed_package = committed_package_event.is_some();
    if readiness.journal_id != journal.journal_id()
        || readiness.human_approval_event_hash != approval.event_hash
        || readiness.semantic_root != journal.semantic_root()
        || readiness.derived_result_root != journal.derived_result_root()
        || readiness.approved_evidence_tool_input_root
            != approval.approved_evidence_tool_input_root
        || readiness.target_count == 0
        || readiness.claim_rows_root == Sha256Digest::ZERO
        || readiness.journal_head != journal.head()
    {
        return Err(TrustError::new(
            "package_readiness_stale_or_invalid",
            "semantic readiness certificate does not bind the exact current approved head",
        ));
    }
    let binding = parse_trust_binding(archive_bytes)?;
    let packet_index = verify_packet_index(
        archive_bytes,
        readiness.gate_presentation_sha256,
    )?;
    verify_packaged_evidence(
        archive_bytes,
        &packet_index,
        readiness.approved_evidence_tool_input_root,
    )?;
    let proof_tree = readiness
        .proof_requirements
        .as_ref()
        .map(|requirements| verify_packaged_proof_tree(archive_bytes, &packet_index, requirements))
        .transpose()?;
    if readiness.expected_proof_tree_root != proof_tree.as_ref().map(|tree| tree.root) {
        return Err(TrustError::new(
            "package_authorization_proof_tree_not_current",
            "packaged proof tree differs from the current package-ready checkpoint",
        ));
    }
    let claim_document = read_package_member(
        archive_bytes,
        CLAIM_DOCUMENT_PATH,
        MAX_CLAIM_DOCUMENT_BYTES,
    )?;
    if binding.journal_id != readiness.journal_id
        || (!recovering_committed_package
            && binding.pre_authorization_journal_head != readiness.journal_head)
        || binding.human_approval_event_hash != readiness.human_approval_event_hash
        || binding.semantic_root != readiness.semantic_root
        || binding.derived_result_root != readiness.derived_result_root
        || binding.approved_evidence_tool_input_root
            != readiness.approved_evidence_tool_input_root
        || binding.target_count != readiness.target_count
        || binding.claim_rows_root != readiness.claim_rows_root
        || binding.claim_document_sha256 != raw_sha256(&claim_document)
        || binding.claim_document_sha256 != raw_sha256(&readiness.claim_document_bytes)
        || binding.gate_presentation_sha256 != readiness.gate_presentation_sha256
        || binding.packet_index_sha256 != packet_index.raw_sha256
        || binding.generator_and_validator_tool_root
            != request.generator_and_validator_closure_root
        || binding.generator_and_validator_tool_root
            != readiness.approved_evidence_tool_input_root
        || binding.proof_tree_root != proof_tree.as_ref().map(|tree| tree.root)
        || claim_document != readiness.claim_document_bytes
    {
        return Err(TrustError::new(
            "package_trust_binding_not_ready",
            "archive trust binding differs from the audited campaign readiness certificate",
        ));
    }
    // The package-presentation digest already commits the entire archive,
    // including the self-digested trust binding.  Keep the schema field's
    // protocol meaning: it names the actual approved generator/validator
    // closure, not the trust-binding record's identity.
    let bound_generator_and_validator_root = binding.generator_and_validator_tool_root;

    let package_sequence = if let Some(event_hash) = committed_package_event {
        if journal.head().event_hash != event_hash {
            return Err(TrustError::new(
                "package_retry_not_at_head",
                "a committed package transaction may be recovered only at its exact durable head",
            ));
        }
        journal.head().sequence_number
    } else {
        let predecessor = journal.head();
        let subject_value = serde_json::json!({
            "schema": "trellis-package-authorization/v1",
            "human_approval_event_hash": approval.event_hash,
            "derived_result_closure_root": journal.derived_result_root(),
            "package_presentation_sha256": presentation,
            "generator_and_validator_closure_root": bound_generator_and_validator_root,
            "pre_authorization_journal_head": predecessor,
        });
        let subject = AuthoritativeRecord::parse(&registry, subject_value)?;
        let head = journal.append(AppendRequest {
            transaction_id: request.transaction_id.to_owned(),
            event_kind: EventKind::PackageAuthorized,
            subject_id: request.transaction_id.to_owned(),
            subject: Subject::CanonicalRecord(subject),
            actor: JournalActor::Kernel,
            semantic_root_after: journal.semantic_root(),
            derived_result_root_after: Sha256Digest::ZERO,
            authorization: None,
        })?;
        head.sequence_number
    };

    if package_sequence < 2 {
        return Err(TrustError::new(
            "package_chain_too_short",
            "a package event must follow a seed and human approval",
        ));
    }
    let package_bundle = journal.committed_bundle_value(package_sequence)?;
    let event = decode_event(&package_bundle)?;
    let payload = decode_payload(&package_bundle)?;
    if event.event_kind != EventKind::PackageAuthorized
        || event.transaction_id != request.transaction_id
        || event.sequence_number != package_sequence
    {
        return Err(TrustError::new(
            "package_retry_event_mismatch",
            "the selected committed transaction is not the requested package event",
        ));
    }
    let package_subject = package_subject(&package_bundle)?;
    let pre_head: JournalHead = value_as(package_subject, "pre_authorization_journal_head")?;
    let subject_presentation = value_digest(package_subject, "package_presentation_sha256")?;
    let subject_generator = value_digest(
        package_subject,
        "generator_and_validator_closure_root",
    )?;
    if subject_presentation != presentation
        || subject_generator != bound_generator_and_validator_root
        || pre_head != binding.pre_authorization_journal_head
        || (!recovering_committed_package && pre_head != readiness.journal_head)
        || pre_head.sequence_number.checked_add(1) != Some(package_sequence)
        || pre_head.event_hash != event.previous_event_hash
        || journal.head().event_hash != event.event_hash
    {
        return Err(TrustError::new(
            "package_retry_subject_mismatch",
            "committed package subject differs from the requested archive or closure",
        ));
    }
    let chain = (1..=pre_head.sequence_number)
        .map(|sequence| journal.committed_bundle_value(sequence))
        .collect::<Result<Vec<_>, _>>()?;
    let committed_head = journal.head();
    let receipt_context = JournalCommitReceiptContext {
        journal_id: journal.journal_id(),
        run_id: journal.run_id(),
        package_transaction_id: request.transaction_id,
        package_event_hash: event.event_hash,
        package_event_payload_sha256: payload.payload_sha256,
        predecessor_head: &pre_head,
        committed_head: &committed_head,
        package_presentation_sha256: presentation,
    };
    let receipt = sign_journal_commit_receipt(
        &registry,
        actor_manifest(journal),
        &receipt_context,
        request.journal_authority_identity,
        request.journal_key_id,
        request.journal_signing_key,
    )?;
    let mut sidecar = serde_json::json!({
        "schema": "trellis-package-authorization-sidecar/v1",
        "package_presentation_sha256": presentation,
        "package_authorization_bundle": package_bundle,
        "authentication_key_manifest": journal.actor_key_manifest_value(),
        "journal_commit_receipt": receipt,
        "canonical_genesis_head": journal.canonical_genesis_head(),
        "journal_chain_bundles": chain,
        "pre_authorization_journal_head": pre_head,
        "committed_authorization_journal_head": committed_head,
        "sidecar_sha256": Sha256Digest::ZERO,
    });
    let sidecar_sha256 = self_digest(
        DomainTag::PackageAuthorizationSidecar,
        &sidecar,
        "sidecar_sha256",
    )?;
    sidecar
        .as_object_mut()
        .expect("sidecar is an object")
        .insert(
            "sidecar_sha256".into(),
            Value::String(sidecar_sha256.to_string()),
        );
    AuthoritativeRecord::parse(&registry, sidecar.clone())?;
    let sidecar_bytes = canonical_json_value(&sidecar)?;
    Ok(AuthorizedPackage {
        package_presentation_sha256: presentation,
        authorization_head: journal.head(),
        sidecar_sha256,
        sidecar,
        sidecar_bytes,
    })
}

/// Verify an archive/sidecar pair without consulting package-provided paths,
/// checkpoints, or trust roots.  `roots` must be independently installed by
/// the verifier.
pub fn verify_authorized_package(
    archive_bytes: &[u8],
    sidecar_bytes: &[u8],
    roots: &ManifestAuthorityRoots,
) -> Result<VerifiedPackageAuthorization, TrustError> {
    verify_package_archive(archive_bytes)?;
    verify_single_claim_presentation(archive_bytes, CLAIM_DOCUMENT_PATH)?;
    let registry = SchemaRegistry::v1()?;
    let sidecar_value = parse_json_strict(sidecar_bytes)
        .map_err(|error| TrustError::new("sidecar_json_invalid", error.to_string()))?;
    if canonical_json_value(&sidecar_value)? != sidecar_bytes {
        return Err(TrustError::new(
            "sidecar_not_canonical",
            "detached sidecar must be exact canonical JSON",
        ));
    }
    let sidecar_record = AuthoritativeRecord::parse(&registry, sidecar_value.clone())?;
    let wire: SidecarWire = serde_json::from_value(sidecar_value)
        .map_err(|error| TrustError::new("sidecar_decode_failed", error.to_string()))?;
    if wire.schema != "trellis-package-authorization-sidecar/v1"
        || wire.sidecar_sha256 != sidecar_record.digest()
    {
        return Err(TrustError::new(
            "sidecar_identity_mismatch",
            "sidecar schema or embedded digest is invalid",
        ));
    }
    let presentation = tagged_hash(DomainTag::PackagePresentation, archive_bytes);
    if presentation != wire.package_presentation_sha256 {
        return Err(TrustError::new(
            "package_presentation_digest_mismatch",
            "archive bytes do not match the authorized presentation digest",
        ));
    }
    let binding = parse_trust_binding(archive_bytes)?;
    let packet_index = verify_packet_index(
        archive_bytes,
        binding.gate_presentation_sha256,
    )?;
    if binding.packet_index_sha256 != packet_index.raw_sha256 {
        return Err(TrustError::new(
            "package_packet_index_binding_mismatch",
            "trust binding does not name the exact typed packet index",
        ));
    }
    let archive_claim_document = read_package_member(
        archive_bytes,
        CLAIM_DOCUMENT_PATH,
        MAX_CLAIM_DOCUMENT_BYTES,
    )?;
    let actor_keys = ActorKeyManifest::verify(
        &registry,
        wire.authentication_key_manifest.clone(),
        roots,
    )?;
    let predecessor_journal = TrustJournal::replay_detached(
        actor_keys.clone(),
        &wire.canonical_genesis_head,
        &wire.journal_chain_bundles,
    )?;
    if predecessor_journal.head() != wire.pre_authorization_journal_head {
        return Err(TrustError::new(
            "sidecar_predecessor_head_mismatch",
            "complete detached chain does not end at the declared package predecessor",
        ));
    }
    let actor_manifest_path = single_packet_role_path(&packet_index, "actor_key_manifest")?;
    let packaged_actor_manifest = read_package_member(
        archive_bytes,
        actor_manifest_path,
        MAX_PACKET_ARTIFACT_BYTES,
    )?;
    if packaged_actor_manifest != actor_keys.canonical_bytes()? {
        return Err(TrustError::new(
            "package_actor_manifest_mismatch",
            "packaged actor-key manifest differs from the authenticated sidecar manifest",
        ));
    }
    let seed_manifest_path = single_packet_role_path(&packet_index, "seed_manifest")?;
    let packaged_seed = read_package_member(
        archive_bytes,
        seed_manifest_path,
        MAX_PACKET_ARTIFACT_BYTES,
    )?;
    let seed_bundle = wire.journal_chain_bundles.first().ok_or_else(|| {
        TrustError::new(
            "package_seed_bundle_missing",
            "detached journal chain lacks its sequence-one seed bundle",
        )
    })?;
    let seed_value = seed_bundle.get("subject_json").ok_or_else(|| {
        TrustError::new("package_seed_subject_missing", "detached seed event lacks subject_json")
    })?;
    if packaged_seed != canonical_json_value(seed_value)? {
        return Err(TrustError::new(
            "package_seed_manifest_mismatch",
            "packaged seed manifest differs from journal sequence one",
        ));
    }
    let seed_record = AuthoritativeRecord::parse(&registry, seed_value.clone())?;
    let approval = predecessor_journal.current_approval().ok_or_else(|| {
        TrustError::new(
            "sidecar_has_no_current_approval",
            "package predecessor has no current unrevoked human approval",
        )
    })?;
    if approval.gate_presentation_sha256 != binding.gate_presentation_sha256 {
        return Err(TrustError::new(
            "package_gate_approval_mismatch",
            "packaged immutable gate presentation differs from the signed approval",
        ));
    }
    verify_packaged_evidence(
        archive_bytes,
        &packet_index,
        approval.approved_evidence_tool_input_root,
    )?;
    let proof_tree = if binding.proof_tree_root.is_some() {
        let seed_definition_path =
            single_packet_role_path(&packet_index, "seed_definition_bundle")?;
        let seed_definition_bytes = read_package_member(
            archive_bytes,
            seed_definition_path,
            MAX_PACKET_ARTIFACT_BYTES,
        )?;
        let seed_closure = verify_seed_definition_bundle(&seed_record, &seed_definition_bytes)?;
        let requirements =
            derive_proof_requirements(&seed_closure, &wire.journal_chain_bundles)?;
        Some(verify_packaged_proof_tree(
            archive_bytes,
            &packet_index,
            &requirements,
        )?)
    } else {
        None
    };
    let (claim_count, claim_rows_root, reconstructed_claim_document) =
        reconstruct_claim_surface(&wire.journal_chain_bundles)?;
    let event = decode_event(&wire.package_authorization_bundle)?;
    let payload = decode_payload(&wire.package_authorization_bundle)?;
    let subject = package_subject(&wire.package_authorization_bundle)?;
    let subject_approval = value_digest(subject, "human_approval_event_hash")?;
    let subject_derived = value_digest(subject, "derived_result_closure_root")?;
    let subject_presentation = value_digest(subject, "package_presentation_sha256")?;
    let generator = value_digest(subject, "generator_and_validator_closure_root")?;
    let subject_predecessor: JournalHead = value_as(subject, "pre_authorization_journal_head")?;
    if event.event_kind != EventKind::PackageAuthorized
        || event.sequence_number != wire.pre_authorization_journal_head.sequence_number + 1
        || event.previous_event_hash != wire.pre_authorization_journal_head.event_hash
        || subject_predecessor != wire.pre_authorization_journal_head
        || subject_approval != approval.event_hash
        || subject_derived != predecessor_journal.derived_result_root()
        || subject_presentation != presentation
        || generator != binding.generator_and_validator_tool_root
        || binding.proof_tree_root != proof_tree.as_ref().map(|tree| tree.root)
        || binding.journal_id != event.journal_id
        || binding.pre_authorization_journal_head
            != wire.pre_authorization_journal_head
        || binding.human_approval_event_hash != approval.event_hash
        || binding.semantic_root != predecessor_journal.semantic_root()
        || binding.derived_result_root != predecessor_journal.derived_result_root()
        || binding.approved_evidence_tool_input_root
            != approval.approved_evidence_tool_input_root
        || binding.generator_and_validator_tool_root
            != approval.approved_evidence_tool_input_root
        || binding.target_count == 0
        || binding.target_count != claim_count
        || binding.claim_rows_root != claim_rows_root
        || binding.claim_document_sha256 != raw_sha256(&archive_claim_document)
        || binding.claim_document_sha256 != raw_sha256(&reconstructed_claim_document)
        || archive_claim_document != reconstructed_claim_document
        || wire.committed_authorization_journal_head
            != (JournalHead {
                journal_id: event.journal_id.clone(),
                sequence_number: event.sequence_number,
                event_hash: event.event_hash,
            })
    {
        return Err(TrustError::new(
            "package_authorization_binding_mismatch",
            "package event does not bind the replayed approval, closure, archive, and exact heads",
        ));
    }
    let mut complete_chain = wire.journal_chain_bundles.clone();
    complete_chain.push(wire.package_authorization_bundle.clone());
    let committed_journal = TrustJournal::replay_detached(
        actor_keys.clone(),
        &wire.canonical_genesis_head,
        &complete_chain,
    )?;
    if committed_journal.head() != wire.committed_authorization_journal_head
        || committed_journal.current_human_approval_event_hash() != Some(approval.event_hash)
    {
        return Err(TrustError::new(
            "package_committed_head_replay_mismatch",
            "package bundle is not the exact authenticated successor of the complete prefix",
        ));
    }
    let receipt_context = JournalCommitReceiptContext {
        journal_id: &event.journal_id,
        run_id: &event.run_id,
        package_transaction_id: &event.transaction_id,
        package_event_hash: event.event_hash,
        package_event_payload_sha256: payload.payload_sha256,
        predecessor_head: &wire.pre_authorization_journal_head,
        committed_head: &wire.committed_authorization_journal_head,
        package_presentation_sha256: presentation,
    };
    let receipt = verify_journal_commit_receipt(
        &registry,
        &actor_keys,
        wire.journal_commit_receipt,
        &receipt_context,
    )?;
    Ok(VerifiedPackageAuthorization {
        package_presentation_sha256: presentation,
        authorization_head: wire.committed_authorization_journal_head,
        human_approval_event_hash: approval.event_hash,
        derived_result_closure_root: subject_derived,
        generator_and_validator_closure_root: binding.generator_and_validator_tool_root,
        proof_tree_root: binding.proof_tree_root,
        journal_authority_identity: receipt.journal_authority_identity,
        journal_key_id: receipt.key_id,
    })
}

fn actor_manifest(journal: &TrustJournal) -> &ActorKeyManifest {
    // The public value accessor intentionally exposes no signing capability;
    // package authorization still needs the already root-verified manifest
    // owned by the journal.  Re-validating it against a package-supplied root
    // would weaken the online path, so the journal supplies this reference.
    journal.actor_key_manifest()
}

fn parse_trust_binding(archive_bytes: &[u8]) -> Result<TrustBindingWire, TrustError> {
    let bytes = read_package_member(
        archive_bytes,
        TRUST_BINDING_PATH,
        MAX_TRUST_BINDING_BYTES,
    )?;
    let value = parse_json_strict(&bytes)
        .map_err(|error| TrustError::new("package_trust_binding_json_invalid", error.to_string()))?;
    if canonical_json_value(&value)? != bytes {
        return Err(TrustError::new(
            "package_trust_binding_not_canonical",
            "archive trust binding must be exact canonical JSON",
        ));
    }
    let digest = verify_self_digest(DomainTag::ManifestNode, &value, "binding_sha256")?;
    let binding: TrustBindingWire = serde_json::from_value(value).map_err(|error| {
        TrustError::new("package_trust_binding_decode_failed", error.to_string())
    })?;
    if binding.schema != "trellis-package-trust-binding/v1"
        || binding.binding_sha256 != digest
    {
        return Err(TrustError::new(
            "package_trust_binding_identity_mismatch",
            "archive trust binding has an invalid schema or identity",
        ));
    }
    Ok(binding)
}

fn reconstruct_claim_surface(
    bundles: &[Value],
) -> Result<(usize, Sha256Digest, Vec<u8>), TrustError> {
    let mut claims: BTreeMap<String, (Value, JournalEvent)> = BTreeMap::new();
    for bundle in bundles {
        let event = decode_event(bundle)?;
        if event.event_kind != EventKind::ExternalClaimRowsGenerated {
            continue;
        }
        let payload = decode_payload(bundle)?;
        let encoded = bundle
            .get("subject_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new(
                    "offline_claim_subject_missing",
                    "claim-row event lacks its raw subject",
                )
            })?;
        let bytes = BASE64_STANDARD.decode(encoded).map_err(|error| {
            TrustError::new("offline_claim_base64_invalid", error.to_string())
        })?;
        let envelope = parse_json_strict(&bytes)
            .map_err(|error| TrustError::new("offline_claim_json_invalid", error.to_string()))?;
        if canonical_json_value(&envelope)? != bytes
            || envelope.get("schema").and_then(Value::as_str)
                != Some("trellis-external-claim-rows/v1")
        {
            return Err(TrustError::new(
                "offline_claim_not_canonical",
                "claim-row subject is not canonical v1 JSON",
            ));
        }
        let target_id = envelope
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new("offline_claim_target_missing", "claim lacks target_id")
            })?
            .to_owned();
        let terminal: Sha256Digest = value_digest(&envelope, "terminal_event_hash")?;
        let predecessor: Sha256Digest = value_digest(&envelope, "journal_predecessor_sha256")?;
        let rendered = envelope
            .get("rendered_utf8")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new("offline_claim_rendering_missing", "claim lacks rendered bytes")
            })?;
        let recomposed = format!(
            "{}\n{}\n{}\n{}\n",
            string_value(&envelope, "unrestricted_extracted_model_claim")?,
            string_value(&envelope, "source_counterevidence_validation")?,
            string_value(&envelope, "conditional_extracted_model_claim")?,
            string_value(&envelope, "actual_use_coverage")?,
        );
        if payload.subject_id != target_id
            || terminal != predecessor
            || event.previous_event_hash != terminal
            || rendered != recomposed
            || claims
                .insert(target_id.clone(), (envelope, event))
                .is_some()
        {
            return Err(TrustError::new(
                "offline_claim_binding_invalid",
                "claim rows are duplicated, misrendered, or not immediate after their terminal",
            ));
        }
    }
    if claims.is_empty() {
        return Err(TrustError::new(
            "offline_claim_surface_empty",
            "authenticated predecessor chain contains no external claim rows",
        ));
    }
    let mut leaves = Vec::with_capacity(claims.len());
    let mut document = String::from(
        "# Trellis proof claims\n\nOnly the four independent lines under each target are normative.\n\n",
    );
    for (target_id, (envelope, event)) in &claims {
        let result_kind = string_value(envelope, "result_kind")?;
        let terminal = value_digest(envelope, "terminal_event_hash")?;
        let leaf = match result_kind {
            "formal_refutation" => serde_json::json!({
                "result_kind": "formal_refutation",
                "target_id": target_id,
                "formal_refutation_sha256": value_digest(envelope, "formal_refutation_sha256")?,
                "history_summary_sha256": value_digest(envelope, "history_summary_sha256")?,
                "terminal_event_hash": terminal,
                "claim_rows_event_hash": event.event_hash,
            }),
            "positive_proof" => serde_json::json!({
                "result_kind": "positive_proof",
                "target_id": target_id,
                "positive_proof_subject_sha256": value_digest(envelope, "positive_proof_subject_sha256")?,
                "terminal_event_hash": terminal,
                "claim_rows_event_hash": event.event_hash,
            }),
            "checked_negative_proof" => serde_json::json!({
                "result_kind": "checked_negative_proof",
                "target_id": target_id,
                "negative_proof_subject_sha256": value_digest(envelope, "negative_proof_subject_sha256")?,
                "history_summary_sha256": value_digest(envelope, "history_summary_sha256")?,
                "terminal_event_hash": terminal,
                "claim_rows_event_hash": event.event_hash,
            }),
            _ => {
                return Err(TrustError::new(
                    "offline_claim_result_kind_invalid",
                    "claim rows name an unknown result kind",
                ))
            }
        };
        leaves.push(leaf);
        document.push_str("## ");
        document.push_str(target_id);
        document.push_str("\n\n");
        document.push_str(string_value(envelope, "rendered_utf8")?);
        document.push('\n');
    }
    let root = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&Value::Array(leaves))?,
    );
    Ok((claims.len(), root, document.into_bytes()))
}

fn string_value<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "package_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn decode_event(bundle: &Value) -> Result<JournalEvent, TrustError> {
    serde_json::from_value(
        bundle
            .get("event")
            .cloned()
            .ok_or_else(|| TrustError::new("journal_event_missing", "bundle lacks event"))?,
    )
    .map_err(|error| TrustError::new("journal_event_decode_failed", error.to_string()))
}

fn decode_payload(bundle: &Value) -> Result<JournalEventPayload, TrustError> {
    serde_json::from_value(
        bundle
            .get("payload")
            .cloned()
            .ok_or_else(|| TrustError::new("journal_payload_missing", "bundle lacks payload"))?,
    )
    .map_err(|error| TrustError::new("journal_payload_decode_failed", error.to_string()))
}

fn package_subject(bundle: &Value) -> Result<&Value, TrustError> {
    let value = bundle.get("subject_json").ok_or_else(|| {
        TrustError::new(
            "package_subject_missing",
            "package authorization bundle lacks canonical subject JSON",
        )
    })?;
    if value.get("schema").and_then(Value::as_str) != Some("trellis-package-authorization/v1") {
        return Err(TrustError::new(
            "package_subject_wrong_schema",
            "package event subject is not a v1 package authorization",
        ));
    }
    Ok(value)
}

fn value_digest(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "package_field_missing_or_invalid",
                format!("{field} must be a digest string"),
            )
        })?
        .parse()
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "package_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn value_as<T: for<'de> Deserialize<'de>>(value: &Value, field: &str) -> Result<T, TrustError> {
    serde_json::from_value(value.get(field).cloned().ok_or_else(|| {
        TrustError::new(
            "package_field_missing_or_invalid",
            format!("missing {field}"),
        )
    })?)
    .map_err(|error| TrustError::new("package_field_missing_or_invalid", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::{sign_actor_receipt, ActorRole, JournalActor, ReceiptContext};
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("schemas/JOURNAL_HASH_FIXTURES.v1.json"))
        .unwrap()
    }

    fn vector(name: &str) -> Value {
        fixture()["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|vector| vector["name"] == name)
            .unwrap()
            .clone()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn roots() -> ManifestAuthorityRoots {
        let mut roots = ManifestAuthorityRoots::default();
        roots
            .insert_hex(
                "fixture-v1-manifest-root",
                &hex(&SigningKey::from_bytes(&[3_u8; 32]).verifying_key().to_bytes()),
            )
            .unwrap();
        roots
    }

    fn manifest(registry: &SchemaRegistry, roots: &ManifestAuthorityRoots) -> ActorKeyManifest {
        let item = vector("actor_key_manifest_self");
        let mut value = item["payload"].clone();
        value
            .as_object_mut()
            .unwrap()
            .insert("manifest_sha256".into(), item["expected_sha256"].clone());
        ActorKeyManifest::verify(registry, value, roots).unwrap()
    }

    fn self_record(
        registry: &SchemaRegistry,
        vector_name: &str,
        digest_field: &str,
    ) -> AuthoritativeRecord {
        let item = vector(vector_name);
        let mut value = item["payload"].clone();
        value
            .as_object_mut()
            .unwrap()
            .insert(digest_field.into(), item["expected_sha256"].clone());
        AuthoritativeRecord::parse(registry, value).unwrap()
    }

    fn conditional_seed_fixture() -> VerifiedSeedDefinitionClosure {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        let registry = SchemaRegistry::v1().unwrap();
        let profile = AuthoritativeRecord::parse_as(
            &registry,
            "trellis-qualification-profile/v1",
            fixture["objects"]["qualification_profile"].clone(),
        )
        .unwrap();
        let candidate = AuthoritativeRecord::parse(
            &registry,
            fixture["objects"]["conditional_theorem_candidate"].clone(),
        )
        .unwrap();
        let records_by_digest = BTreeMap::from([
            (profile.digest(), profile),
            (candidate.digest(), candidate),
        ]);
        let canonical_values_by_digest = records_by_digest
            .iter()
            .map(|(digest, record)| (*digest, record.value().clone()))
            .collect();
        VerifiedSeedDefinitionClosure {
            seed_manifest_sha256: raw_sha256(b"conditional-seed"),
            bundle_sha256: raw_sha256(b"conditional-bundle"),
            records_by_digest,
            canonical_values_by_digest,
        }
    }

    fn conditional_candidate_receipt(
        candidate: &AuthoritativeRecord,
        declaration_marker: &[u8],
    ) -> Value {
        let toolchain = raw_sha256(b"conditional-lean-toolchain");
        let lean_executable = raw_sha256(b"conditional-lean-executable");
        let lake_executable = raw_sha256(b"conditional-lake-executable");
        let checker_script = raw_sha256(b"conditional-checker-script");
        let axioms = raw_sha256(b"conditional-approved-axioms");
        let local = serde_json::json!({
            "node": string_field(candidate.value(), "node_id").unwrap(),
            "toolchain_hash": toolchain,
            "lean_executable_hash": lean_executable,
            "lake_executable_hash": lake_executable,
            "checker_script_hash": checker_script,
            "approved_axioms_hash": axioms,
            "active_decl_hash": raw_sha256(declaration_marker),
            "active_statement_hash": value_digest(
                candidate.value(),
                "active_statement_sha256",
            )
            .unwrap(),
            "boundary_theorems": {},
            "strict_theorem_deps": {},
            "strict_definition_deps": {},
            "kernel_semantic_hashes": {},
            "kernel_axioms": ["propext"],
        });
        let semantic = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "boundary_theorems": {},
                "strict_theorem_deps": {},
                "strict_definition_deps": {},
                "kernel_semantic_hashes": {},
                "active_decl_hash": local["active_decl_hash"],
                "active_statement_hash": local["active_statement_hash"],
            }))
            .unwrap(),
        );
        let checker_axioms = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "checker_toolchain_sha256": toolchain,
                "lean_executable_sha256": lean_executable,
                "lake_executable_sha256": lake_executable,
                "checker_script_sha256": checker_script,
                "approved_axiom_closure_sha256": axioms,
                "kernel_axioms": ["propext"],
            }))
            .unwrap(),
        );
        let mut receipt = serde_json::json!({
            "schema": "trellis-conditional-local-closure-proof-receipt/v1",
            "candidate_definition_sha256": candidate.digest(),
            "profile_definition_sha256": value_digest(
                candidate.value(),
                "profile_definition_sha256",
            )
            .unwrap(),
            "target_id": string_field(candidate.value(), "target_id").unwrap(),
            "node_id": string_field(candidate.value(), "node_id").unwrap(),
            "conditional_statement_sha256": value_digest(
                candidate.value(),
                "conditional_statement_sha256",
            )
            .unwrap(),
            "active_statement_sha256": value_digest(
                candidate.value(),
                "active_statement_sha256",
            )
            .unwrap(),
            "checker_toolchain_sha256": toolchain,
            "approved_axiom_closure_sha256": axioms,
            "checker_and_axiom_closure_sha256": checker_axioms,
            "semantic_definition_closure_sha256": semantic,
            "local_closure_record": local,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::RawArtifact,
            &receipt,
            "proof_receipt_sha256",
        )
        .unwrap();
        receipt["proof_receipt_sha256"] = Value::String(digest.to_string());
        receipt
    }

    fn conditional_inventory_bundle(
        sequence_number: u64,
        event_kind: EventKind,
        event_hash: Sha256Digest,
        previous_event_hash: Sha256Digest,
        subject_id: &str,
        subject_sha256: Sha256Digest,
        subject_json: Option<Value>,
        raw_subject: Option<&Value>,
    ) -> Value {
        let event = serde_json::json!({
            "journal_schema": "trellis-journal-event/v1",
            "journal_id": "conditional-fixture-journal",
            "run_id": "conditional-fixture-run",
            "sequence_number": sequence_number,
            "transaction_id": format!("conditional-fixture-{sequence_number}"),
            "previous_event_hash": previous_event_hash,
            "event_kind": event_kind,
            "payload_schema_id": "trellis://schemas/journal-event-payload/v1",
            "payload_schema_sha256": raw_sha256(b"payload-schema"),
            "payload_hash": raw_sha256(format!("payload-{sequence_number}").as_bytes()),
            "semantic_root_before": raw_sha256(b"semantic-before"),
            "semantic_root_after": raw_sha256(b"semantic-after"),
            "derived_result_root_before": raw_sha256(b"derived-before"),
            "derived_result_root_after": raw_sha256(b"derived-after"),
            "actor_role": "kernel",
            "actor_identity": "trellis-kernel",
            "actor_authentication_method": "kernel_internal",
            "event_hash": event_hash,
        });
        let payload = serde_json::json!({
            "schema": "trellis-journal-event-payload/v1",
            "event_kind": event_kind,
            "subject_id": subject_id,
            "subject_kind": if subject_json.is_some() {
                "canonical_record"
            } else {
                "raw_artifact"
            },
            "subject_hash_tag": "raw-artifact",
            "subject_sha256": subject_sha256,
            "journal_predecessor_sha256": previous_event_hash,
            "payload_sha256": raw_sha256(format!("payload-{sequence_number}").as_bytes()),
        });
        let mut bundle = serde_json::json!({
            "event": event,
            "payload": payload,
        });
        if let Some(subject) = subject_json {
            bundle["subject_json"] = subject;
        }
        if let Some(subject) = raw_subject {
            bundle["subject_base64"] = Value::String(
                BASE64_STANDARD.encode(canonical_json_value(subject).unwrap()),
            );
        }
        bundle
    }

    fn approved_journal() -> (tempfile::TempDir, TrustJournal, ManifestAuthorityRoots) {
        let registry = SchemaRegistry::v1().unwrap();
        let roots = roots();
        let seed = self_record(&registry, "seed_manifest_self", "manifest_sha256");
        let directory = tempfile::tempdir().unwrap();
        let actor_manifest = manifest(&registry, &roots);
        let mut journal = TrustJournal::create(
            directory.path().join("trust-journal"),
            "fixture-journal",
            "fixture-run",
            actor_manifest.clone(),
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let approval = AuthoritativeRecord::parse(
            &registry,
            vector("human_approval_full")["payload"].clone(),
        )
        .unwrap();
        let subject = Subject::CanonicalRecord(approval);
        let predecessor = journal.head();
        let payload = journal
            .prepare_routine_gate_payload(
                EventKind::AdvanceGateApproved,
                "fixture-human-approval",
                "fixture-reviewer",
                &subject,
            )
            .unwrap();
        let receipt = sign_actor_receipt(
            &registry,
            &actor_manifest,
            &ReceiptContext {
                journal_id: journal.journal_id(),
                run_id: journal.run_id(),
                transaction_id: "fixture-approval-transaction",
                event_kind: EventKind::AdvanceGateApproved,
                event_payload_sha256: payload.payload_sha256,
                predecessor_head: &predecessor,
                actor_role: ActorRole::Reviewer,
                actor_identity: "fixture-reviewer",
                gate_or_revision_lane_id: "fixture-human-approval",
            },
            "fixture-reviewer-key",
            &SigningKey::from_bytes(&[1_u8; 32]),
        )
        .unwrap();
        journal
            .append(AppendRequest {
                transaction_id: "fixture-approval-transaction".into(),
                event_kind: EventKind::AdvanceGateApproved,
                subject_id: "fixture-human-approval".into(),
                subject,
                actor: JournalActor::Authenticated {
                    role: ActorRole::Reviewer,
                    identity: "fixture-reviewer".into(),
                    gate_or_revision_lane_id: "fixture-human-approval".into(),
                    receipt,
                },
                semantic_root_after: journal.semantic_root(),
                derived_result_root_after: journal.derived_result_root(),
                authorization: None,
            })
            .unwrap();
        let terminal = journal.head().event_hash;
        let unrestricted = "Unrestricted extracted-model claim: PROVED.";
        let source = "Source-counterevidence validation: NOT APPLICABLE to a proved unrestricted result.";
        let conditional =
            "Conditional extracted-model claim under no-approved-profile/C: NOT ATTEMPTED.";
        let applicability = "Actual-use coverage of C: NOT APPLICABLE.";
        let rendered =
            format!("{unrestricted}\n{source}\n{conditional}\n{applicability}\n");
        let claim = serde_json::json!({
            "schema": "trellis-external-claim-rows/v1",
            "result_kind": "positive_proof",
            "target_id": "fixture-target",
            "target_statement_sha256": "31".repeat(32),
            "positive_proof_subject_sha256": "32".repeat(32),
            "terminal_result_sha256": "32".repeat(32),
            "terminal_event_hash": terminal,
            "unrestricted_extracted_model_claim": unrestricted,
            "source_counterevidence_validation": source,
            "conditional_extracted_model_claim": conditional,
            "actual_use_coverage": applicability,
            "rendered_utf8": rendered,
            "journal_predecessor_sha256": terminal,
        });
        journal
            .append(AppendRequest {
                transaction_id: "fixture-claim-transaction".into(),
                event_kind: EventKind::ExternalClaimRowsGenerated,
                subject_id: "fixture-target".into(),
                subject: Subject::RawArtifact(canonical_json_value(&claim).unwrap()),
                actor: JournalActor::Kernel,
                semantic_root_after: journal.semantic_root(),
                derived_result_root_after: Sha256Digest::ZERO,
                authorization: None,
            })
            .unwrap();
        (directory, journal, roots)
    }

    fn package_archive(
        readiness: &PackageReadiness,
        generator_root: Sha256Digest,
    ) -> Vec<u8> {
        build_package_archive(readiness, generator_root, fixture_package_artifacts()).unwrap()
    }

    fn package_archive_with_claim_and_entries(
        readiness: &PackageReadiness,
        generator_root: Sha256Digest,
        claim_document: &[u8],
        additional_entries: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let mut altered_readiness = readiness.clone();
        altered_readiness.claim_document_bytes = claim_document.to_vec();
        let mut artifacts = fixture_package_artifacts();
        let mut prose_entries = Vec::new();
        for (path, bytes) in additional_entries {
            if fixture_is_prose_path(path) {
                prose_entries.push(((*path).to_owned(), (*bytes).to_vec()));
            } else {
                artifacts.push(PackageArtifact {
                    role: "source_snapshot".into(),
                    path: (*path).to_owned(),
                    bytes: (*bytes).to_vec(),
                });
            }
        }
        let archive = build_package_archive(&altered_readiness, generator_root, artifacts).unwrap();
        if prose_entries.is_empty() {
            archive
        } else {
            insert_untyped_archive_entries(&archive, prose_entries)
        }
    }

    fn fixture_is_prose_path(path: &str) -> bool {
        let basename = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        basename == "readme"
            || basename.starts_with("readme.")
            || [".md", ".markdown", ".txt", ".rst", ".adoc", ".html", ".htm"]
                .iter()
                .any(|extension| basename.ends_with(extension))
    }

    fn fixture_package_artifacts() -> Vec<PackageArtifact> {
        let registry = SchemaRegistry::v1().unwrap();
        let roots = roots();
        let actor_manifest = manifest(&registry, &roots).canonical_bytes().unwrap();
        let seed = self_record(&registry, "seed_manifest_self", "manifest_sha256");
        let leaves = serde_json::json!([]);
        let evidence_root = tagged_hash(
            DomainTag::EvidenceToolRoot,
            &canonical_json_value(&leaves).unwrap(),
        );
        let mut evidence_manifest = serde_json::json!({
            "schema": "trellis-evidence-tool-manifest/v1",
            "leaves": leaves,
            "evidence_tool_input_root": evidence_root,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let evidence_digest = self_digest(
            DomainTag::ManifestNode,
            &evidence_manifest,
            "manifest_sha256",
        )
        .unwrap();
        evidence_manifest["manifest_sha256"] = Value::String(evidence_digest.to_string());
        vec![
            PackageArtifact {
                role: "actor_key_manifest".into(),
                path: "trust/actor-key-manifest.json".into(),
                bytes: actor_manifest,
            },
            PackageArtifact {
                role: "seed_manifest".into(),
                path: "trust/seed-manifest.json".into(),
                bytes: canonical_json_value(seed.value()).unwrap(),
            },
            PackageArtifact {
                role: "seed_definition_bundle".into(),
                path: "trust/seed-definition-bundle.json".into(),
                bytes: b"{}".to_vec(),
            },
            PackageArtifact {
                role: "evidence_manifest".into(),
                path: "evidence/MANIFEST.json".into(),
                bytes: canonical_json_value(&evidence_manifest).unwrap(),
            },
            PackageArtifact {
                role: "gate_presentation".into(),
                path: "review/GATE_PRESENTATION.bin".into(),
                bytes: b"fixture gate presentation\n".to_vec(),
            },
            PackageArtifact {
                role: "proof_receipt".into(),
                path: "proof/receipt.json".into(),
                bytes: b"{}".to_vec(),
            },
            PackageArtifact {
                role: "proof_source".into(),
                path: "proof/Fixture.lean".into(),
                bytes: b"theorem fixture : True := by trivial\n".to_vec(),
            },
            PackageArtifact {
                role: "schema".into(),
                path: "schemas/fixture.schema.json".into(),
                bytes: b"{}".to_vec(),
            },
            PackageArtifact {
                role: "source_snapshot".into(),
                path: "source/lib.rs".into(),
                bytes: b"pub fn fixture() {}\n".to_vec(),
            },
            PackageArtifact {
                role: "toolchain_input".into(),
                path: "proof/lean-toolchain".into(),
                bytes: b"leanprover/lean4:v4.19.0\n".to_vec(),
            },
        ]
    }

    fn strict_receipt(
        target: &str,
        node: &str,
        source: &[u8],
    ) -> Value {
        let nonzero = raw_sha256(format!("fixture:{target}:{node}").as_bytes());
        let record = serde_json::json!({
            "node": node,
            "closure_version": "fixture-v1",
            "toolchain_hash": raw_sha256(b"leanprover/lean4:v4.19.0\n"),
            "lake_manifest_hash": raw_sha256(b"{\"version\":\"1.1.0\"}\n"),
            "preamble_hash": raw_sha256(b"namespace Tablet\n"),
            "approved_axioms_hash": nonzero,
            "active_decl_hash": raw_sha256(source),
            "active_statement_hash": nonzero,
            "kernel_axioms": [],
            "boundary_theorems": {},
            "strict_theorem_deps": {},
            "strict_definition_deps": {},
            "kernel_semantic_hashes": {},
            "accepted_at_snapshot_id": "fixture",
            "axcheck_status": "agreed"
        });
        let mut receipt = serde_json::json!({
            "schema": "trellis-local-closure-proof-receipt/v1",
            "target_id": target,
            "target_statement_sha256": nonzero,
            "polarity": "prove",
            "live_target_id": target,
            "node_id": node,
            "generated_statement_sha256": nonzero,
            "checker_toolchain_sha256": nonzero,
            "approved_axiom_closure_sha256": nonzero,
            "semantic_definition_closure_sha256": nonzero,
            "local_closure_record": record,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::RawArtifact,
            &receipt,
            "proof_receipt_sha256",
        )
        .unwrap();
        receipt["proof_receipt_sha256"] = Value::String(digest.to_string());
        receipt
    }

    fn strict_proof_fixture() -> (ProofPackageRequirements, Vec<PackageArtifact>) {
        let sources = [
            ("TargetA", "TargetA", b"theorem TargetA : True := by trivial\n".as_slice()),
            ("TargetB", "TargetB", b"theorem TargetB : True := by trivial\n".as_slice()),
        ];
        let mut receipts = BTreeMap::new();
        let mut artifacts = strict_toolchain_artifacts();
        for (target, node, source) in sources {
            let receipt = strict_receipt(target, node, source);
            let receipt_digest = value_digest(&receipt, "proof_receipt_sha256").unwrap();
            let source_digest = raw_sha256(source);
            receipts.insert(format!("positive_proof:{target}"), receipt.clone());
            artifacts.push(PackageArtifact {
                role: "proof_receipt".into(),
                path: format!("proof/receipts/{receipt_digest}.json"),
                bytes: canonical_json_value(&receipt).unwrap(),
            });
            artifacts.push(PackageArtifact {
                role: "proof_source".into(),
                path: format!("proof/sources/{source_digest}.lean"),
                bytes: source.to_vec(),
            });
        }
        (
            ProofPackageRequirements::new(receipts, BTreeMap::new()).unwrap(),
            artifacts,
        )
    }

    fn selected_conditional_bundles(
        seed: &VerifiedSeedDefinitionClosure,
        receipt: &Value,
    ) -> Vec<Value> {
        let profile = seed
            .records_by_digest
            .values()
            .find(|record| record.contract().record_schema == "trellis-qualification-profile/v1")
            .unwrap();
        let candidate = seed
            .records_by_digest
            .values()
            .find(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
            })
            .unwrap();
        let target_id = string_field(candidate.value(), "target_id").unwrap();
        let selection_event_hash = raw_sha256(b"selected-profile-event");
        let selection = conditional_inventory_bundle(
            11,
            EventKind::ApprovedProfileSelected,
            selection_event_hash,
            raw_sha256(b"approval-head"),
            target_id,
            profile.digest(),
            Some(profile.value().clone()),
            None,
        );
        let envelope = serde_json::json!({
            "schema": "trellis-generated-conditional-statement/v1",
            "target_id": target_id,
            "profile_sha256": profile.digest(),
            "profile_selection_event_hash": selection_event_hash,
            "statement_sha256": value_digest(candidate.value(), "conditional_statement_sha256").unwrap(),
            "statement_utf8": string_field(candidate.value(), "statement_utf8").unwrap(),
            "qualification_inputs": {
                "profile": profile.value(),
                "conditional_theorem_candidate": candidate.value(),
                "conditional_proof_receipt": receipt,
            },
        });
        let generated = conditional_inventory_bundle(
            12,
            EventKind::ConditionalStatementGenerated,
            raw_sha256(b"generated-conditional-event"),
            selection_event_hash,
            target_id,
            raw_sha256(&canonical_json_value(&envelope).unwrap()),
            None,
            Some(&envelope),
        );
        vec![selection, generated]
    }

    #[test]
    fn current_epoch_without_profile_selection_requires_no_conditional_proof() {
        let seed = conditional_seed_fixture();
        let (candidates, receipts) =
            derive_current_epoch_conditional_inventory(&seed, &[], 10).unwrap();
        assert!(candidates.is_empty());
        assert!(receipts.is_empty());
    }

    #[test]
    fn current_epoch_selection_requires_its_generated_statement_receipt() {
        let seed = conditional_seed_fixture();
        let profile = seed
            .records_by_digest
            .values()
            .find(|record| record.contract().record_schema == "trellis-qualification-profile/v1")
            .unwrap();
        let selection = conditional_inventory_bundle(
            11,
            EventKind::ApprovedProfileSelected,
            raw_sha256(b"selected-profile-event"),
            raw_sha256(b"approval-head"),
            string_field(profile.value(), "target_id").unwrap(),
            profile.digest(),
            Some(profile.value().clone()),
            None,
        );
        let error =
            derive_current_epoch_conditional_inventory(&seed, &[selection], 10).unwrap_err();
        assert_eq!(error.code, "package_conditional_receipt_incomplete");
    }

    #[test]
    fn current_epoch_conditional_receipt_is_derived_from_generated_inputs() {
        let seed = conditional_seed_fixture();
        let candidate = seed
            .records_by_digest
            .values()
            .find(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
            })
            .unwrap();
        let receipt = conditional_candidate_receipt(candidate, b"journal-declaration");
        let bundles = selected_conditional_bundles(&seed, &receipt);
        let (candidates, receipts) =
            derive_current_epoch_conditional_inventory(&seed, &bundles, 10).unwrap();
        assert_eq!(candidates.keys().copied().collect::<Vec<_>>(), vec![candidate.digest()]);
        assert_eq!(receipts.get(&candidate.digest()), Some(&receipt));
    }

    #[test]
    fn generated_conditional_rejects_swapped_seed_candidate() {
        let seed = conditional_seed_fixture();
        let candidate = seed
            .records_by_digest
            .values()
            .find(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
            })
            .unwrap();
        let receipt = conditional_candidate_receipt(candidate, b"journal-declaration");
        let mut bundles = selected_conditional_bundles(&seed, &receipt);
        let mut envelope = decode_raw_json_subject(&bundles[1], "fixture").unwrap();
        envelope["qualification_inputs"]["conditional_theorem_candidate"] = Value::Null;
        bundles[1]["subject_base64"] = Value::String(
            BASE64_STANDARD.encode(canonical_json_value(&envelope).unwrap()),
        );
        let error =
            derive_current_epoch_conditional_inventory(&seed, &bundles, 10).unwrap_err();
        assert_eq!(error.code, "package_conditional_seed_inputs_mismatch");
    }

    #[test]
    fn package_ready_runtime_rejects_stale_or_extra_conditional_receipts() {
        let seed = conditional_seed_fixture();
        let candidate = seed
            .records_by_digest
            .values()
            .find(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
            })
            .unwrap();
        let journal_receipt = conditional_candidate_receipt(candidate, b"journal-declaration");
        let runtime_receipt = conditional_candidate_receipt(candidate, b"stale-declaration");
        let mut requirements = ProofPackageRequirements::new(
            BTreeMap::from([(
                "formal_refutation:dag-target".into(),
                strict_receipt("dag-target", "DagTarget", b"theorem DagTarget : True := by trivial\n"),
            )]),
            BTreeMap::from([(candidate.digest(), candidate.clone())]),
        )
        .unwrap();
        requirements
            .bind_journal_conditional_receipts(BTreeMap::from([(
                candidate.digest(),
                journal_receipt.clone(),
            )]))
            .unwrap();
        assert!(!requirements.runtime_conditional_receipts_validated);
        let error = requirements
            .set_conditional_receipts(BTreeMap::from([(
                candidate.digest(),
                runtime_receipt,
            )]))
            .unwrap_err();
        assert_eq!(error.code, "package_conditional_receipt_stale");
        assert!(!requirements.runtime_conditional_receipts_validated);
        requirements
            .set_conditional_receipts(BTreeMap::from([(
                candidate.digest(),
                journal_receipt,
            )]))
            .unwrap();
        assert!(requirements.runtime_conditional_receipts_validated);

        let mut no_selection = ProofPackageRequirements::new(
            requirements.journal_receipts.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let error = no_selection
            .set_conditional_receipts(BTreeMap::from([(
                candidate.digest(),
                conditional_candidate_receipt(candidate, b"extra-declaration"),
            )]))
            .unwrap_err();
        assert_eq!(error.code, "package_conditional_receipt_set_mismatch");
    }

    fn strict_toolchain_artifacts() -> Vec<PackageArtifact> {
        [
            ("proof/lean-toolchain", b"leanprover/lean4:v4.19.0\n".as_slice()),
            ("proof/lake-manifest.json", b"{\"version\":\"1.1.0\"}\n".as_slice()),
            ("proof/Tablet/Preamble.lean", b"namespace Tablet\n".as_slice()),
        ]
        .into_iter()
        .map(|(path, bytes)| PackageArtifact {
            role: "toolchain_input".into(),
            path: path.into(),
            bytes: bytes.to_vec(),
        })
        .collect()
    }

    #[test]
    fn strict_proof_tree_rejects_proof_body_mutation() {
        let (requirements, mut artifacts) = strict_proof_fixture();
        artifacts
            .iter_mut()
            .find(|artifact| artifact.role == "proof_source")
            .unwrap()
            .bytes
            .extend_from_slice(b"-- mutated\n");

        let error = verify_proof_artifact_set(&artifacts, &requirements).unwrap_err();
        assert_eq!(error.code, "package_proof_source_body_mismatch");
    }

    #[test]
    fn strict_proof_tree_rejects_swapped_and_omitted_receipts() {
        let (requirements, mut artifacts) = strict_proof_fixture();
        let receipt_indexes: Vec<_> = artifacts
            .iter()
            .enumerate()
            .filter_map(|(index, artifact)| (artifact.role == "proof_receipt").then_some(index))
            .collect();
        let first_path = artifacts[receipt_indexes[0]].path.clone();
        artifacts[receipt_indexes[0]].path = artifacts[receipt_indexes[1]].path.clone();
        artifacts[receipt_indexes[1]].path = first_path;
        let error = verify_proof_artifact_set(&artifacts, &requirements).unwrap_err();
        assert_eq!(error.code, "package_proof_receipt_path_mismatch");

        let (requirements, mut artifacts) = strict_proof_fixture();
        let omitted = artifacts
            .iter()
            .position(|artifact| artifact.role == "proof_receipt")
            .unwrap();
        artifacts.remove(omitted);
        let error = verify_proof_artifact_set(&artifacts, &requirements).unwrap_err();
        assert_eq!(error.code, "package_proof_receipt_omitted");
    }

    #[test]
    fn strict_proof_tree_rejects_unrelated_extra_source() {
        let (requirements, mut artifacts) = strict_proof_fixture();
        let bytes = b"theorem Unrelated : True := by trivial\n".to_vec();
        artifacts.push(PackageArtifact {
            role: "proof_source".into(),
            path: format!("proof/sources/{}.lean", raw_sha256(&bytes)),
            bytes,
        });

        let error = verify_proof_artifact_set(&artifacts, &requirements).unwrap_err();
        assert_eq!(error.code, "package_proof_source_set_mismatch");
    }

    #[test]
    fn strict_builder_ignores_stale_mutable_receipt_directory() {
        let source = b"theorem Target : True := by trivial\n";
        let receipt = strict_receipt("Target", "Target", source);
        let mut journal_receipts = BTreeMap::new();
        journal_receipts.insert("positive_proof:Target".into(), receipt.clone());
        let requirements =
            ProofPackageRequirements::new(journal_receipts, BTreeMap::new()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let tablet = root.path().join("Tablet");
        std::fs::create_dir_all(&tablet).unwrap();
        std::fs::write(tablet.join("Target.lean"), source).unwrap();
        let stale = root
            .path()
            .join("checker-state/local-closure-records");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("Target.json"), b"{\"forged\":true}\n").unwrap();

        let mut artifacts = requirements.build_artifacts(&tablet).unwrap();
        artifacts.extend(strict_toolchain_artifacts());
        let receipt_artifact = artifacts
            .iter()
            .find(|artifact| artifact.role == "proof_receipt")
            .unwrap();
        assert_eq!(receipt_artifact.bytes, canonical_json_value(&receipt).unwrap());
        verify_proof_artifact_set(&artifacts, &requirements).unwrap();
    }

    #[test]
    fn strict_archive_binds_the_exact_proof_tree_root() {
        let (_directory, journal, _roots) = approved_journal();
        let mut readiness = readiness_from_journal(&journal);
        let (requirements, proof_artifacts) = strict_proof_fixture();
        let expected_root = verify_proof_artifact_set(&proof_artifacts, &requirements)
            .unwrap()
            .root;
        readiness.proof_requirements = Some(requirements);
        readiness.expected_proof_tree_root = Some(expected_root);
        let mut artifacts = fixture_package_artifacts();
        artifacts.retain(|artifact| {
            !matches!(
                artifact.role.as_str(),
                "proof_receipt" | "proof_source" | "toolchain_input"
            )
        });
        artifacts.extend(proof_artifacts);

        let archive = build_package_archive(
            &readiness,
            readiness.approved_evidence_tool_input_root,
            artifacts,
        )
        .unwrap();
        let binding = parse_trust_binding(&archive).unwrap();
        assert_eq!(binding.proof_tree_root, Some(expected_root));
        let index = verify_packet_index(&archive, readiness.gate_presentation_sha256).unwrap();
        let verified = verify_packaged_proof_tree(
            &archive,
            &index,
            readiness.proof_requirements.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(verified.root, expected_root);
    }

    fn fixture_evidence_leaf(
        kind: &str,
        logical_id: &str,
        relative_path: &str,
        bytes: &[u8],
        dependency_ids: &[&str],
    ) -> Value {
        serde_json::json!({
            "kind": kind,
            "logical_id": logical_id,
            "relative_path": relative_path,
            "byte_length": bytes.len(),
            "sha256_of_raw_bytes": raw_sha256(bytes),
            "dependency_ids": dependency_ids,
        })
    }

    fn package_archive_with_evidence(
        readiness: &PackageReadiness,
        leaves: Vec<Value>,
        evidence_files: &[(&str, &[u8])],
        declared_root: Option<Sha256Digest>,
        valid_self_digest: bool,
    ) -> (Vec<u8>, Sha256Digest) {
        let leaves_value = Value::Array(leaves);
        let computed_root = tagged_hash(
            DomainTag::EvidenceToolRoot,
            &canonical_json_value(&leaves_value).unwrap(),
        );
        let mut manifest = serde_json::json!({
            "schema": "trellis-evidence-tool-manifest/v1",
            "leaves": leaves_value,
            "evidence_tool_input_root": declared_root.unwrap_or(computed_root),
            "manifest_sha256": Sha256Digest::ZERO,
        });
        if valid_self_digest {
            let digest = self_digest(DomainTag::ManifestNode, &manifest, "manifest_sha256")
                .unwrap();
            manifest["manifest_sha256"] = Value::String(digest.to_string());
        }
        let mut artifacts = fixture_package_artifacts();
        artifacts
            .iter_mut()
            .find(|artifact| artifact.role == "evidence_manifest")
            .unwrap()
            .bytes = canonical_json_value(&manifest).unwrap();
        artifacts.extend(evidence_files.iter().map(|(relative_path, bytes)| {
            PackageArtifact {
                role: "evidence_leaf".into(),
                path: format!("evidence/{relative_path}"),
                bytes: bytes.to_vec(),
            }
        }));
        (
            build_package_archive(readiness, computed_root, artifacts).unwrap(),
            computed_root,
        )
    }

    fn verify_evidence_archive(
        archive: &[u8],
        readiness: &PackageReadiness,
        approved_root: Sha256Digest,
    ) -> Result<(), TrustError> {
        verify_package_archive(archive)?;
        let index = verify_packet_index(archive, readiness.gate_presentation_sha256)?;
        verify_packaged_evidence(archive, &index, approved_root)
    }

    fn insert_untyped_archive_entries(
        archive_bytes: &[u8],
        additional_entries: Vec<(String, Vec<u8>)>,
    ) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes)).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            if entry.name() == super::super::archive::PACKAGE_MANIFEST_PATH {
                continue;
            }
            let name = entry.name().to_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            entries.push((name, bytes));
        }
        entries.extend(additional_entries);
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let manifest_entries: Vec<_> = entries
            .iter()
            .map(|(path, bytes)| {
                serde_json::json!({
                    "path": path,
                    "byte_length": bytes.len(),
                    "sha256_of_raw_bytes": raw_sha256(bytes),
                })
            })
            .collect();
        let mut manifest = serde_json::json!({
            "schema": "trellis-package-manifest/v1",
            "entries": manifest_entries,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::ManifestNode,
            &manifest,
            "manifest_sha256",
        )
        .unwrap();
        manifest["manifest_sha256"] = Value::String(digest.to_string());
        let manifest_bytes = canonical_json_value(&manifest).unwrap();
        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o100644);
        for (path, bytes) in entries {
            writer.start_file(path, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        writer
            .start_file(super::super::archive::PACKAGE_MANIFEST_PATH, options)
            .unwrap();
        writer.write_all(&manifest_bytes).unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn readiness_from_journal(journal: &TrustJournal) -> PackageReadiness {
        let approval = journal.current_approval().unwrap();
        let bundles = journal.committed_bundle_values().unwrap();
        let (target_count, claim_rows_root, claim_document_bytes) =
            reconstruct_claim_surface(&bundles).unwrap();
        PackageReadiness {
            journal_id: journal.journal_id().to_owned(),
            journal_head: journal.head(),
            human_approval_event_hash: approval.event_hash,
            semantic_root: journal.semantic_root(),
            derived_result_root: journal.derived_result_root(),
            approved_evidence_tool_input_root: approval.approved_evidence_tool_input_root,
            gate_presentation_sha256: approval.gate_presentation_sha256,
            target_count,
            claim_rows_root,
            claim_document_bytes,
            proof_requirements: None,
            expected_proof_tree_root: None,
        }
    }

    #[test]
    fn package_archive_construction_is_byte_identical_for_the_same_artifacts() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let artifacts = fixture_package_artifacts();
        let mut reversed_artifacts = artifacts.clone();
        reversed_artifacts.reverse();

        let first = build_package_archive(&readiness, generator_root, artifacts).unwrap();
        let second =
            build_package_archive(&readiness, generator_root, reversed_artifacts).unwrap();

        assert_eq!(first, second);
        verify_package_archive(&first).unwrap();
        verify_packet_index(&first, readiness.gate_presentation_sha256).unwrap();
    }

    #[test]
    fn package_archive_rejects_a_missing_required_typed_role() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let mut artifacts = fixture_package_artifacts();
        artifacts.retain(|artifact| artifact.role != "proof_receipt");

        let error = build_package_archive(&readiness, generator_root, artifacts).unwrap_err();

        assert_eq!(error.code, "package_artifact_required_role_missing");
    }

    #[test]
    fn package_archive_rejects_gate_bytes_that_differ_from_the_immutable_approval() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let mut artifacts = fixture_package_artifacts();
        artifacts
            .iter_mut()
            .find(|artifact| artifact.role == "gate_presentation")
            .unwrap()
            .bytes = b"different gate presentation\n".to_vec();

        let error = build_package_archive(&readiness, generator_root, artifacts).unwrap_err();

        assert_eq!(error.code, "package_gate_presentation_mismatch");
    }

    #[test]
    fn packet_index_rejects_an_untyped_non_prose_archive_member() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let archive = package_archive(&readiness, generator_root);
        let archive_with_untyped_member = insert_untyped_archive_entries(
            &archive,
            vec![("artifacts/untyped.bin".to_owned(), b"untyped bytes".to_vec())],
        );

        verify_package_archive(&archive_with_untyped_member).unwrap();
        verify_single_claim_presentation(&archive_with_untyped_member, CLAIM_DOCUMENT_PATH)
            .unwrap();
        let error = verify_packet_index(
            &archive_with_untyped_member,
            readiness.gate_presentation_sha256,
        )
        .err()
        .expect("an untyped archive member must be rejected");

        assert_eq!(error.code, "packet_index_not_complete");
    }

    #[test]
    fn packaged_evidence_accepts_a_closed_exact_dependency_dag() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);
        let source_bytes = b"source\n";
        let checker_bytes = b"checker\n";
        let mut source = fixture_evidence_leaf("source", "source", "source", source_bytes, &[]);
        source["schema_id"] = Value::String("trellis-source/v1".into());
        let mut checker = fixture_evidence_leaf(
            "tool",
            "checker",
            "tools/checker",
            checker_bytes,
            &["source"],
        );
        checker["producer_id"] = Value::String("rustc".into());
        checker["producer_hash"] = Value::String("11".repeat(32));
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            vec![source, checker],
            &[("source", source_bytes), ("tools/checker", checker_bytes)],
            None,
            true,
        );

        verify_evidence_archive(&archive, &readiness, root).unwrap();
    }

    #[test]
    fn packaged_evidence_rejects_open_unsorted_or_duplicate_leaves() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);

        let mut open_leaf = fixture_evidence_leaf("tool", "a", "a", b"a", &[]);
        open_leaf["unregistered_override"] = Value::Bool(true);
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            vec![open_leaf],
            &[("a", b"a")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_leaf_not_closed"
        );

        let leaves = vec![
            fixture_evidence_leaf("tool", "b", "b", b"b", &[]),
            fixture_evidence_leaf("tool", "a", "a", b"a", &[]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("b", b"b")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_order_invalid"
        );

        let leaves = vec![
            fixture_evidence_leaf("tool", "same", "a", b"a", &[]),
            fixture_evidence_leaf("tool", "same", "b", b"b", &[]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("b", b"b")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_duplicate_leaf"
        );

        let leaves = vec![
            fixture_evidence_leaf("tool", "a", "same", b"same", &[]),
            fixture_evidence_leaf("tool", "b", "same", b"same", &[]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("same", b"same")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_duplicate_leaf"
        );
    }

    #[test]
    fn packaged_evidence_rejects_malformed_dependency_graphs_and_producers() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);

        let leaves = vec![
            fixture_evidence_leaf("tool", "a", "a", b"a", &["c", "b"]),
            fixture_evidence_leaf("tool", "b", "b", b"b", &[]),
            fixture_evidence_leaf("tool", "c", "c", b"c", &[]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("b", b"b"), ("c", b"c")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_dependency_order_invalid"
        );

        let leaves = vec![
            fixture_evidence_leaf("tool", "a", "a", b"a", &["b", "b"]),
            fixture_evidence_leaf("tool", "b", "b", b"b", &[]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("b", b"b")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_dependency_order_invalid"
        );

        let leaves = vec![fixture_evidence_leaf("tool", "a", "a", b"a", &["missing"])];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_dependency_missing"
        );

        let leaves = vec![
            fixture_evidence_leaf("tool", "a", "a", b"a", &["b"]),
            fixture_evidence_leaf("tool", "b", "b", b"b", &["a"]),
        ];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("b", b"b")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_dependency_cycle"
        );

        let mut leaf = fixture_evidence_leaf("tool", "a", "a", b"a", &[]);
        leaf["producer_id"] = Value::String("compiler".into());
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            vec![leaf],
            &[("a", b"a")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_optional_pair_incomplete"
        );

        let mut leaf = fixture_evidence_leaf("tool", "a", "a", b"a", &[]);
        leaf["producer_id"] = Value::String("compiler".into());
        leaf["producer_hash"] = Value::String("not-a-digest".into());
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            vec![leaf],
            &[("a", b"a")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_producer_invalid"
        );
    }

    #[test]
    fn packaged_evidence_rejects_path_bytes_set_and_identity_substitution() {
        let (_directory, journal, _roots) = approved_journal();
        let readiness = readiness_from_journal(&journal);

        let leaves = vec![fixture_evidence_leaf("tool", "a", "../a", b"a", &[])];
        let (archive, root) =
            package_archive_with_evidence(&readiness, leaves, &[], None, true);
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_relative_path_invalid"
        );

        let leaves = vec![fixture_evidence_leaf("tool", "a", "a", b"a", &[])];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"different")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_leaf_mismatch"
        );

        let leaves = vec![fixture_evidence_leaf("tool", "a", "a", b"a", &[])];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a"), ("extra", b"extra")],
            None,
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_leaf_set_mismatch"
        );

        let leaves = vec![fixture_evidence_leaf("tool", "a", "a", b"a", &[])];
        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves.clone(),
            &[("a", b"a")],
            Some(Sha256Digest::ZERO),
            true,
        );
        assert_eq!(
            verify_evidence_archive(&archive, &readiness, root)
                .unwrap_err()
                .code,
            "packaged_evidence_root_mismatch"
        );

        let (archive, root) = package_archive_with_evidence(
            &readiness,
            leaves,
            &[("a", b"a")],
            None,
            false,
        );
        assert!(verify_evidence_archive(&archive, &readiness, root).is_err());
    }

    #[test]
    fn exact_archive_and_authenticated_complete_chain_verify_offline() {
        let (_directory, mut journal, roots) = approved_journal();
        let signing = SigningKey::from_bytes(&[4_u8; 32]);
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let archive = package_archive(&readiness, generator_root);
        let authorized = authorize_package(
            &mut journal,
            &archive,
            PackageAuthorizationRequest {
                transaction_id: "fixture-package-transaction",
                generator_and_validator_closure_root: generator_root,
                journal_authority_identity: "fixture-journal-authority",
                journal_key_id: "fixture-journal-key",
                journal_signing_key: &signing,
            },
            &readiness,
        )
        .unwrap();
        assert_eq!(authorized.authorization_head.sequence_number, 4);
        assert_eq!(
            authorized.sidecar["package_authorization_bundle"]["subject_json"]
                ["generator_and_validator_closure_root"],
            Value::String(generator_root.to_string())
        );
        let verified =
            verify_authorized_package(&archive, &authorized.sidecar_bytes, &roots).unwrap();
        assert_eq!(verified.authorization_head, authorized.authorization_head);
        assert_eq!(verified.package_presentation_sha256, authorized.package_presentation_sha256);

        // The durable commit makes the receipt-creation step exactly
        // retryable after a crash, without appending a second event.  As the
        // public pipeline does, recovery audits the current committed package
        // head rather than reusing a stale predecessor certificate.
        let retry_readiness = readiness_from_journal(&journal);
        let retried = authorize_package(
            &mut journal,
            &archive,
            PackageAuthorizationRequest {
                transaction_id: "fixture-package-transaction",
                generator_and_validator_closure_root: generator_root,
                journal_authority_identity: "fixture-journal-authority",
                journal_key_id: "fixture-journal-key",
                journal_signing_key: &signing,
            },
            &retry_readiness,
        )
        .unwrap();
        assert_eq!(retried.sidecar_bytes, authorized.sidecar_bytes);
        assert_eq!(journal.head().sequence_number, 4);

        // The transaction ID is retryable, not reusable: even a fully
        // manifested archive with one extra machine artifact is divergent.
        let divergent_archive = package_archive_with_claim_and_entries(
            &readiness,
            generator_root,
            &readiness.claim_document_bytes,
            &[("artifacts/alternate.bin", b"different bytes")],
        );
        assert_eq!(
            authorize_package(
                &mut journal,
                &divergent_archive,
                PackageAuthorizationRequest {
                    transaction_id: "fixture-package-transaction",
                    generator_and_validator_closure_root: generator_root,
                    journal_authority_identity: "fixture-journal-authority",
                    journal_key_id: "fixture-journal-key",
                    journal_signing_key: &signing,
                },
                &retry_readiness,
            )
            .unwrap_err()
            .code,
            "package_retry_subject_mismatch"
        );
    }

    #[test]
    fn internally_consistent_claim_tampering_and_extra_prose_fail_closed() {
        let (_directory, mut journal, _roots) = approved_journal();
        let signing = SigningKey::from_bytes(&[4_u8; 32]);
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;

        let mut stronger_claim = readiness.claim_document_bytes.clone();
        stronger_claim.extend_from_slice(b"Verified in practice.\n");
        let tampered_archive = package_archive_with_claim_and_entries(
            &readiness,
            generator_root,
            &stronger_claim,
            &[],
        );
        // The adversarial archive has a valid complete manifest and a binding
        // that honestly hashes its altered bytes.  It still was not generated
        // from the authenticated claim-row events.
        verify_package_archive(&tampered_archive).unwrap();
        assert_eq!(
            authorize_package(
                &mut journal,
                &tampered_archive,
                PackageAuthorizationRequest {
                    transaction_id: "fixture-tampered-package-transaction",
                    generator_and_validator_closure_root: generator_root,
                    journal_authority_identity: "fixture-journal-authority",
                    journal_key_id: "fixture-journal-key",
                    journal_signing_key: &signing,
                },
                &readiness,
            )
            .unwrap_err()
            .code,
            "package_trust_binding_not_ready"
        );

        let extra_readme_archive = package_archive_with_claim_and_entries(
            &readiness,
            generator_root,
            &readiness.claim_document_bytes,
            &[("README.md", b"All targets are verified in practice.\n")],
        );
        verify_package_archive(&extra_readme_archive).unwrap();
        assert_eq!(
            authorize_package(
                &mut journal,
                &extra_readme_archive,
                PackageAuthorizationRequest {
                    transaction_id: "fixture-extra-presentation-transaction",
                    generator_and_validator_closure_root: generator_root,
                    journal_authority_identity: "fixture-journal-authority",
                    journal_key_id: "fixture-journal-key",
                    journal_signing_key: &signing,
                },
                &readiness,
            )
            .unwrap_err()
            .code,
            "package_unapproved_claim_presentation"
        );
    }

    #[test]
    fn readiness_cannot_outlive_current_human_approval() {
        let (_directory, mut journal, _roots) = approved_journal();
        let signing = SigningKey::from_bytes(&[4_u8; 32]);
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let archive = package_archive(&readiness, generator_root);

        journal
            .append(AppendRequest {
                transaction_id: "fixture-revocation-transaction".into(),
                event_kind: EventKind::Revoked,
                subject_id: "fixture-human-approval".into(),
                subject: Subject::RawArtifact(b"approval revoked\n".to_vec()),
                actor: JournalActor::Kernel,
                semantic_root_after: journal.semantic_root(),
                derived_result_root_after: Sha256Digest::ZERO,
                authorization: None,
            })
            .unwrap();

        assert_eq!(
            authorize_package(
                &mut journal,
                &archive,
                PackageAuthorizationRequest {
                    transaction_id: "fixture-package-after-revocation",
                    generator_and_validator_closure_root: generator_root,
                    journal_authority_identity: "fixture-journal-authority",
                    journal_key_id: "fixture-journal-key",
                    journal_signing_key: &signing,
                },
                &readiness,
            )
            .unwrap_err()
            .code,
            "package_without_current_approval"
        );
    }

    #[test]
    fn wrong_archive_and_fabricated_commit_signature_fail_closed() {
        let (_directory, mut journal, roots) = approved_journal();
        let signing = SigningKey::from_bytes(&[4_u8; 32]);
        let readiness = readiness_from_journal(&journal);
        let generator_root = readiness.approved_evidence_tool_input_root;
        let archive = package_archive(&readiness, generator_root);
        let authorized = authorize_package(
            &mut journal,
            &archive,
            PackageAuthorizationRequest {
                transaction_id: "fixture-package-transaction",
                generator_and_validator_closure_root: generator_root,
                journal_authority_identity: "fixture-journal-authority",
                journal_key_id: "fixture-journal-key",
                journal_signing_key: &signing,
            },
            &readiness,
        )
        .unwrap();
        assert!(verify_authorized_package(b"different", &authorized.sidecar_bytes, &roots).is_err());
        let extra_presentation = package_archive_with_claim_and_entries(
            &readiness,
            generator_root,
            &readiness.claim_document_bytes,
            &[("claims.html", b"<h1>Verified in practice</h1>")],
        );
        assert_eq!(
            verify_authorized_package(&extra_presentation, &authorized.sidecar_bytes, &roots)
                .unwrap_err()
                .code,
            "package_unapproved_claim_presentation"
        );

        let mut forged = authorized.sidecar;
        forged["journal_commit_receipt"]["signature_ed25519_hex"] =
            Value::String("00".repeat(64));
        let receipt_digest = self_digest(
            DomainTag::JournalCommitReceipt,
            &forged["journal_commit_receipt"],
            "receipt_sha256",
        )
        .unwrap();
        forged["journal_commit_receipt"]["receipt_sha256"] =
            Value::String(receipt_digest.to_string());
        let sidecar_digest = self_digest(
            DomainTag::PackageAuthorizationSidecar,
            &forged,
            "sidecar_sha256",
        )
        .unwrap();
        forged["sidecar_sha256"] = Value::String(sidecar_digest.to_string());
        let forged_bytes = canonical_json_value(&forged).unwrap();
        assert!(verify_authorized_package(
            &archive,
            &forged_bytes,
            &roots
        )
        .is_err());
    }
}
