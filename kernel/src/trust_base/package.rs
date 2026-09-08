//! Deterministic trust-package archive assembly and verification.
//!
//! Q7 (plan doc 32, Stage 3): the detached package-authorization sidecar and
//! every `PackageAuthorized`-event concept are deleted.  The final archive
//! EMBEDS the approval record — the approved-terminal `TrustRecord`
//! (`AdvanceGateApproved`, or `ProtectedReapprovalApproved` after an
//! approved exceptional revision; Stage-3 fix 5) payload plus presentation
//! hash and seed roots — in its manifest; the
//! runtime assembles/verifies the archive at the one deterministic
//! `RuntimePaths::package_archive_path` and finalization rides the
//! `FinalizeAuthorizedPackage` event's `PackageFinalizationRecord` payload.

use super::archive::{read_package_member, verify_package_archive, verify_single_claim_presentation};
use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash,
    verify_self_digest, DomainTag, Sha256Digest, TrustError,
};
use super::records::{EventKind, TrustRecord, TrustRecordSeedRoots};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;

pub const TRUST_BINDING_PATH: &str = "TRELLIS_TRUST_BINDING.json";
/// The ONE prose claim surface (draft-2.3 §16) — required, non-empty, and
/// byte-derivable from `claim_rows.json` alone.
pub const CLAIM_DOCUMENT_PATH: &str = "WHAT_THE_PROOFS_ASSUME.md";
/// The structured claim-rows twin the document is re-rendered from.  A
/// machine artifact (`.json`), so the sole-prose rule never applies to it.
pub const CLAIM_ROWS_PATH: &str = "claim_rows.json";
const PACKET_INDEX_PATH: &str = "TRELLIS_PACKET_INDEX.json";
const MAX_TRUST_BINDING_BYTES: u64 = 1024 * 1024;
const MAX_CLAIM_ROWS_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CLAIM_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PACKET_INDEX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ProofPackageRequirements {
    /// Exact proof receipts already committed by formal-result events, keyed
    /// by a stable kind/target identity.
    pub(crate) journal_receipts: BTreeMap<String, Value>,
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

/// The approval record embedded in a verified trust-finalization archive.
/// Every field is recomputed/re-verified from the archive bytes before this
/// value exists (A3: hash and enum comparisons only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedApprovalRecord {
    pub approval_record: TrustRecord,
    pub approval_record_sha256: Sha256Digest,
    pub presentation_sha256: Sha256Digest,
    pub seed_roots: TrustRecordSeedRoots,
    /// The digest-verified, schema-valid, re-render-checked claim rows.
    pub claim_rows: super::claim::ClaimRows,
    /// The exact archived `claim_rows.json` bytes (canonical JSON) — the
    /// runtime barrier byte-compares these against a fresh derivation from
    /// live state (the Stage-10 anti-staleness pattern).
    pub claim_rows_bytes: Vec<u8>,
}

/// Assembly request for the Q7 finalization archive.  `artifacts` carries
/// the (possibly empty) additional packet content; Stage 5/6 station
/// rebuilds extend it with proof receipts and sources.
/// `claim_rows_bytes` is the canonical `claim_rows.json` — the assembler
/// renders `WHAT_THE_PROOFS_ASSUME.md` from it and refuses rows that fail
/// the structural lint, so a claim-free or malformed archive cannot exist.
#[derive(Clone, Debug)]
pub struct TrustFinalizationArchiveRequest<'a> {
    pub approval_record: &'a TrustRecord,
    pub claim_rows_bytes: Vec<u8>,
    pub artifacts: Vec<PackageArtifact>,
}

#[allow(dead_code)] // Stage-2/3 keeper (plan doc 32): archive-assembly core
// retained for the Stage-5/6 proof-packet rebuild.
impl ProofPackageRequirements {
    pub(crate) fn new(
        journal_receipts: BTreeMap<String, Value>,
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
        Ok(Self { journal_receipts })
    }

    /// Stage-2 keeper (plan doc 32): archive-assembly core retained for
    /// Stage 3's Q7 embedded-approval reshape; its online caller (the
    /// pipeline package-readiness flow) was retired with the Stage-2 cut.
    #[allow(dead_code)]
    pub(crate) fn build_artifacts(
        &self,
        tablet_root: &Path,
    ) -> Result<Vec<PackageArtifact>, TrustError> {
        let receipts: Vec<Value> = self.journal_receipts.values().cloned().collect();
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

    /// Stage-2 keeper (plan doc 32): see `build_artifacts`.
    #[allow(dead_code)]
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
        "trellis-local-closure-proof-receipt/v1" => receipt
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

/// Assemble the byte-deterministic Q7 finalization archive.  The trust
/// binding EMBEDS the verified approved-terminal record — the routine
/// `AdvanceGateApproved` record, or the `ProtectedReapprovalApproved`
/// record that superseded it after an approved exceptional revision
/// (Stage-3 fix 5) — with its full digest, presentation hash, and the five
/// named seed roots; the runtime verifies the re-read renamed bytes before
/// emitting `FinalizeAuthorizedPackage` with the
/// `PackageFinalizationRecord` payload.
pub fn assemble_trust_finalization_archive(
    request: TrustFinalizationArchiveRequest<'_>,
) -> Result<Vec<u8>, TrustError> {
    let approval_record_sha256 = request.approval_record.verify()?;
    if !matches!(
        request.approval_record.kind,
        EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved
    ) {
        return Err(TrustError::new(
            "package_approval_record_kind_invalid",
            "the embedded approval record must be an approved human-gate trust record \
             (AdvanceGateApproved or ProtectedReapprovalApproved)",
        ));
    }
    let approval_value = serde_json::to_value(request.approval_record).map_err(|error| {
        TrustError::new("package_approval_record_encode_failed", error.to_string())
    })?;
    let seed_roots_value = serde_json::to_value(request.approval_record.seed_roots)
        .map_err(|error| {
            TrustError::new("package_seed_roots_encode_failed", error.to_string())
        })?;
    // The claim twins: parse + lint the structured rows, then render the ONE
    // prose surface from the rows ALONE, so the archived document is
    // byte-derivable by the state-free verifier.
    let claim_rows = super::claim::parse_claim_rows(&request.claim_rows_bytes)
        .map_err(|reason| TrustError::new("package_claim_rows_invalid", reason))?;
    let claim_document_bytes = super::claim::render_claim_document(&claim_rows);
    super::claim::lint_claim_rows(&claim_rows, &claim_document_bytes)
        .map_err(|reason| TrustError::new("package_claim_rows_lint_failed", reason))?;
    let claim_rows_sha256 = raw_sha256(&request.claim_rows_bytes);
    let claim_document_sha256 =
        tagged_hash(DomainTag::PackagePresentation, &claim_document_bytes);
    let mut artifacts = request.artifacts;
    // The claim twins ride the ordinary packet index / content root /
    // manifest like any other artifact; the binding additionally pins both
    // digests so the verifier can hard-require them.
    artifacts.push(PackageArtifact {
        role: "claim_rows".into(),
        path: CLAIM_ROWS_PATH.into(),
        bytes: request.claim_rows_bytes.clone(),
    });
    artifacts.push(PackageArtifact {
        role: "claim_document".into(),
        path: CLAIM_DOCUMENT_PATH.into(),
        bytes: claim_document_bytes,
    });
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
        "schema": "trellis-package-trust-binding/v3",
        "approval_record": approval_value,
        "approval_record_sha256": approval_record_sha256,
        "gate_presentation_sha256": request.approval_record.presentation_sha256,
        "seed_roots": seed_roots_value,
        "packet_index_sha256": raw_sha256(&packet_index_bytes),
        "claim_rows_sha256": claim_rows_sha256,
        "claim_document_sha256": claim_document_sha256,
        "binding_sha256": Sha256Digest::ZERO,
    });
    let binding_digest = self_digest(DomainTag::ManifestNode, &binding, "binding_sha256")?;
    binding["binding_sha256"] = Value::String(binding_digest.to_string());
    let mut entries: Vec<(String, Vec<u8>)> = artifacts
        .into_iter()
        .map(|artifact| (artifact.path, artifact.bytes))
        .collect();
    entries.push((TRUST_BINDING_PATH.to_owned(), canonical_json_value(&binding)?));
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

/// Verify a Q7 finalization archive without extraction and return the
/// embedded, digest-verified approval record.  Callers (the runtime barrier
/// and the pure `FinalizeAuthorizedPackage` recheck via
/// `PackageFinalizationRecord`) compare the returned hashes against state;
/// this function never consults state or the filesystem.
pub fn verify_trust_finalization_archive(
    bytes: &[u8],
) -> Result<EmbeddedApprovalRecord, TrustError> {
    verify_package_archive(bytes)?;
    verify_single_claim_presentation(bytes, CLAIM_DOCUMENT_PATH)?;
    let binding_bytes = read_package_member(bytes, TRUST_BINDING_PATH, MAX_TRUST_BINDING_BYTES)?;
    let binding = parse_json_strict(&binding_bytes)?;
    if binding.get("schema").and_then(Value::as_str)
        != Some("trellis-package-trust-binding/v3")
    {
        return Err(TrustError::new(
            "package_trust_binding_schema_invalid",
            "archive trust binding has the wrong schema",
        ));
    }
    verify_self_digest(DomainTag::ManifestNode, &binding, "binding_sha256")?;
    // The binding's packet-index digest is re-checked against the archive's
    // actual packet index (previously declared but never re-verified on the
    // read path).
    let packet_index_bytes =
        read_package_member(bytes, PACKET_INDEX_PATH, MAX_PACKET_INDEX_BYTES)?;
    if value_digest(&binding, "packet_index_sha256")? != raw_sha256(&packet_index_bytes) {
        return Err(TrustError::new(
            "package_packet_index_digest_mismatch",
            "binding packet_index_sha256 differs from the archived packet index",
        ));
    }
    // The claim twins are REQUIRED: a prose-free archive must not pass
    // vacuously.  `claim_rows.json` must be canonical, schema-valid, and
    // digest-bound; the claim document must be non-empty, digest-bound
    // under the package-presentation domain tag, byte-identical to a
    // re-render from the rows ALONE, structurally four lines per target,
    // and free of the forbidden collapse phrases.
    let claim_rows_bytes = read_package_member(bytes, CLAIM_ROWS_PATH, MAX_CLAIM_ROWS_BYTES)?;
    if value_digest(&binding, "claim_rows_sha256")? != raw_sha256(&claim_rows_bytes) {
        return Err(TrustError::new(
            "package_claim_rows_digest_mismatch",
            "binding claim_rows_sha256 differs from the archived claim rows",
        ));
    }
    let claim_rows_value = parse_json_strict(&claim_rows_bytes)?;
    super::schema::SchemaRegistry::v1()?
        .validate(super::claim::CLAIM_ROWS_SCHEMA_ID, &claim_rows_value)
        .map_err(|error| {
            TrustError::new(
                "package_claim_rows_schema_invalid",
                format!("claim_rows.json fails its embedded schema: {error}"),
            )
        })?;
    let claim_rows = super::claim::parse_claim_rows(&claim_rows_bytes)
        .map_err(|reason| TrustError::new("package_claim_rows_invalid", reason))?;
    if claim_rows.header.edition != super::claim::ClaimEdition::Finalization {
        return Err(TrustError::new(
            "package_claim_rows_edition_invalid",
            "archived claim rows must be the finalization edition",
        ));
    }
    let claim_document_bytes =
        read_package_member(bytes, CLAIM_DOCUMENT_PATH, MAX_CLAIM_DOCUMENT_BYTES)?;
    if claim_document_bytes.is_empty() {
        return Err(TrustError::new(
            "package_claim_document_empty",
            "the claim document is required and must be non-empty",
        ));
    }
    if value_digest(&binding, "claim_document_sha256")?
        != tagged_hash(DomainTag::PackagePresentation, &claim_document_bytes)
    {
        return Err(TrustError::new(
            "package_claim_document_digest_mismatch",
            "binding claim_document_sha256 differs from the archived claim document",
        ));
    }
    let expected_document = super::claim::render_claim_document(&claim_rows);
    if expected_document != claim_document_bytes {
        return Err(TrustError::new(
            "package_claim_document_render_mismatch",
            "the archived claim document is not the byte-exact render of claim_rows.json",
        ));
    }
    super::claim::lint_claim_rows(&claim_rows, &claim_document_bytes)
        .map_err(|reason| TrustError::new("package_claim_rows_lint_failed", reason))?;
    let approval_value = binding.get("approval_record").cloned().ok_or_else(|| {
        TrustError::new(
            "package_approval_record_missing",
            "archive trust binding lacks its embedded approval record",
        )
    })?;
    let approval_record: TrustRecord =
        serde_json::from_value(approval_value).map_err(|error| {
            TrustError::new("package_approval_record_decode_failed", error.to_string())
        })?;
    let approval_record_sha256 = approval_record.verify()?;
    if !matches!(
        approval_record.kind,
        EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved
    ) {
        return Err(TrustError::new(
            "package_approval_record_kind_invalid",
            "the embedded approval record must be an approved human-gate trust record \
             (AdvanceGateApproved or ProtectedReapprovalApproved)",
        ));
    }
    if value_digest(&binding, "approval_record_sha256")? != approval_record_sha256 {
        return Err(TrustError::new(
            "package_approval_record_digest_mismatch",
            "binding approval_record_sha256 differs from the re-hashed embedded record",
        ));
    }
    if value_digest(&binding, "gate_presentation_sha256")?
        != approval_record.presentation_sha256
    {
        return Err(TrustError::new(
            "package_presentation_binding_mismatch",
            "binding gate_presentation_sha256 differs from the embedded approval record",
        ));
    }
    let seed_roots_value = binding.get("seed_roots").cloned().ok_or_else(|| {
        TrustError::new(
            "package_seed_roots_missing",
            "archive trust binding lacks its seed roots",
        )
    })?;
    let seed_roots: TrustRecordSeedRoots = serde_json::from_value(seed_roots_value)
        .map_err(|error| {
            TrustError::new("package_seed_roots_decode_failed", error.to_string())
        })?;
    if seed_roots != approval_record.seed_roots {
        return Err(TrustError::new(
            "package_seed_roots_binding_mismatch",
            "binding seed roots differ from the embedded approval record",
        ));
    }
    // The claim-rows header must bind the SAME approval, presentation, and
    // seed roots the archive embeds — the rows cannot describe a different
    // gate than the one that authorized them.
    if claim_rows.header.approval_record_sha256 != Some(approval_record_sha256)
        || claim_rows.header.gate_presentation_sha256
            != Some(approval_record.presentation_sha256)
        || claim_rows.header.seed_roots != seed_roots
    {
        return Err(TrustError::new(
            "package_claim_rows_binding_mismatch",
            "claim rows header does not bind the embedded approval record, gate \
             presentation, and seed roots",
        ));
    }
    let presentation_sha256 = approval_record.presentation_sha256;
    Ok(EmbeddedApprovalRecord {
        approval_record,
        approval_record_sha256,
        presentation_sha256,
        seed_roots,
        claim_rows,
        claim_rows_bytes,
    })
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

    for digest in packaged_receipts.keys() {
        if !expected_journal.contains_key(digest) {
            return Err(TrustError::new(
                "package_proof_receipt_extra",
                format!("proof receipt {digest} is unrelated to the current formal results"),
            ));
        }
    }
    if packaged_receipts.len() != expected_journal.len() {
        return Err(TrustError::new(
            "package_proof_receipt_set_mismatch",
            "packaged receipt set is not the exact formal-result inventory",
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
            validate_local_closure_certificate_evidence(record)?;
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
                    "node_certificate": record.get("node_certificate"),
                    "node_certificate_evidence": record.get("node_certificate_evidence"),
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

fn validate_local_closure_certificate_evidence(record: &Value) -> Result<(), TrustError> {
    let certificate = record.get("node_certificate").and_then(Value::as_object);
    let evidence = record
        .get("node_certificate_evidence")
        .and_then(Value::as_object);
    if certificate.is_none() && evidence.is_none() {
        return Err(TrustError::new(
            "package_proof_certificate_evidence_missing",
            "local closure lacks exact node-certificate evidence",
        ));
    }

    let validate_uses = |uses: Option<&Vec<Value>>, label: &str| -> Result<(), TrustError> {
        let uses = uses.ok_or_else(|| {
            TrustError::new(
                "package_proof_exact_uses_missing",
                format!("{label} lacks an exact-use array"),
            )
        })?;
        for observed in uses {
            for field in ["owner", "reached_declaration", "declaration_kind"] {
                if observed.get(field).and_then(Value::as_str).is_none_or(str::is_empty) {
                    return Err(TrustError::new(
                        "package_proof_exact_use_invalid",
                        format!("{label} exact use has invalid {field}"),
                    ));
                }
            }
        }
        Ok(())
    };

    if let Some(certificate) = certificate {
        for field in [
            "declaration_manifest_root",
            "local_module_root",
            "logic_root",
            "build_root",
            "certified_node_root",
        ] {
            nonzero_digest_field(&Value::Object(certificate.clone()), field)?;
        }
        let roots = certificate
            .get("dependency_certificate_roots")
            .and_then(Value::as_object)
            .ok_or_else(|| TrustError::new(
                "package_proof_dependency_roots_missing",
                "node certificate lacks dependency_certificate_roots",
            ))?;
        for (owner, value) in roots {
            let digest: Sha256Digest = value.as_str().ok_or_else(|| TrustError::new(
                "package_proof_dependency_root_invalid",
                format!("certificate root for {owner} is not a digest"),
            ))?.parse()?;
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "package_proof_dependency_root_zero",
                    format!("certificate root for {owner} uses the zero sentinel"),
                ));
            }
        }
        validate_uses(
            certificate.get("exact_use_witnesses").and_then(Value::as_array),
            "node certificate",
        )?;
    }
    if let Some(evidence) = evidence {
        if evidence
            .get("principal_declaration")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(TrustError::new(
                "package_proof_principal_evidence_invalid",
                "node-certificate evidence lacks its exact principal",
            ));
        }
        validate_uses(
            evidence.get("exact_declaration_uses").and_then(Value::as_array),
            "node-certificate evidence",
        )?;
    }
    Ok(())
}

fn value_digest(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "package_digest_field_invalid",
                format!("{field} must be a SHA-256 string"),
            )
        })?
        .parse()
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "package_string_field_invalid",
            format!("{field} must be a string"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChallengePolarity, ChallengeResolution, NodeId};
    use crate::trust_base::claim::{
        claim_rows_bytes, ClaimEdition, ClaimHeader, ClaimRow, ClaimRows, ClosureSummary,
        TerminalOutcome, CLAIM_ROWS_SCHEMA,
    };
    use std::collections::BTreeSet;

    fn hexd(fill: char) -> Sha256Digest {
        std::iter::repeat(fill)
            .take(64)
            .collect::<String>()
            .parse()
            .expect("valid digest")
    }

    fn approval_record(kind: EventKind) -> TrustRecord {
        TrustRecord {
            kind,
            lane: None,
            gate_episode_id: "advance:fixture".into(),
            presentation_sha256: hexd('1'),
            seed_roots: TrustRecordSeedRoots {
                seed_manifest_sha256: hexd('2'),
                seed_definition_bundle_sha256: hexd('3'),
                evidence_tool_manifest_sha256: hexd('4'),
                authored_semantic_root: hexd('5'),
                approved_evidence_tool_input_root: hexd('6'),
            },
            cycle: 9,
            intended_event_log_index: 41,
            record_sha256: Sha256Digest::ZERO,
        }
        .seal()
        .expect("seal fixture record")
    }

    fn fixture_claim_rows_bytes(record: &TrustRecord) -> Vec<u8> {
        let proof_subject_sha256 = hexd('7');
        claim_rows_bytes(&ClaimRows {
            schema: CLAIM_ROWS_SCHEMA.into(),
            header: ClaimHeader {
                edition: ClaimEdition::Finalization,
                gate_episode_id: Some(record.gate_episode_id.clone()),
                seed_roots: record.seed_roots.clone(),
                launch_acknowledgment_sha256: hexd('8'),
                approval_record_sha256: Some(record.record_sha256),
                gate_presentation_sha256: Some(record.presentation_sha256),
                adaptation_ledger_sha256: hexd('9'),
                phase0: None,
                approved_axiom_floor: Vec::new(),
                authored_definitions: Vec::new(),
                cycle: record.cycle,
            },
            rows: vec![ClaimRow {
                target_id: "fixture:claim".into(),
                informal: "fixture".into(),
                statement_name: "claim".into(),
                statement_lean: "theorem claim : True := by".into(),
                statement_authority_sha256: hexd('a'),
                statement_sha256: hexd('b'),
                seeded_resolution: ChallengeResolution::Prove,
                selected_polarity: ChallengePolarity::Prove,
                terminal_outcome: TerminalOutcome::Proved {
                    proof_subject_sha256,
                },
                closure: ClosureSummary {
                    selected_target_id: "fixture:claim".into(),
                    selected_node: NodeId::from("claim"),
                    fresh: true,
                    kernel_axioms: BTreeSet::new(),
                    unapproved_axioms: BTreeSet::new(),
                },
            }],
        })
        .expect("canonical fixture claim rows")
    }

    /// Q7: the archive embeds the approval record + hashes, is
    /// byte-deterministic, and round-trips through extraction-free
    /// verification (`archive_embeds_approval_record_and_hashes`).
    #[test]
    fn archive_embeds_approval_record_and_hashes() {
        let record = approval_record(EventKind::AdvanceGateApproved);
        let bytes = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: vec![PackageArtifact {
                role: "proof_receipt".into(),
                path: "proof/receipts/fixture.json".into(),
                bytes: b"{}".to_vec(),
            }],
        })
        .expect("assemble archive");
        let again = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: vec![PackageArtifact {
                role: "proof_receipt".into(),
                path: "proof/receipts/fixture.json".into(),
                bytes: b"{}".to_vec(),
            }],
        })
        .expect("assemble archive again");
        assert_eq!(bytes, again, "assembly must be byte-deterministic");
        let embedded = verify_trust_finalization_archive(&bytes).expect("verify archive");
        assert_eq!(embedded.approval_record, record);
        assert_eq!(embedded.approval_record_sha256, record.record_sha256);
        assert_eq!(embedded.presentation_sha256, record.presentation_sha256);
        assert_eq!(embedded.seed_roots, record.seed_roots);
    }

    /// Stage-3 fix 5: a `ProtectedReapprovalApproved` terminal — the
    /// current approval after an approved exceptional revision — assembles
    /// and verifies exactly like the routine advance approval.
    #[test]
    fn archive_embeds_protected_reapproval_record() {
        let mut record = approval_record(EventKind::ProtectedReapprovalApproved);
        record.lane = Some("audit-lane-1".into());
        record.record_sha256 = Sha256Digest::ZERO;
        let record = record.seal().expect("seal protected fixture record");
        let bytes = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: Vec::new(),
        })
        .expect("assemble archive from the protected approval");
        let embedded = verify_trust_finalization_archive(&bytes)
            .expect("verify archive embedding the protected approval");
        assert_eq!(embedded.approval_record, record);
        assert_eq!(embedded.approval_record_sha256, record.record_sha256);
    }

    #[test]
    fn archive_assembly_refuses_non_approval_record_kinds() {
        let record = approval_record(EventKind::AdvanceGateFeedback);
        let error = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(error.code, "package_approval_record_kind_invalid");
    }

    #[test]
    fn archive_verification_rejects_tampered_embedded_record() {
        let record = approval_record(EventKind::AdvanceGateApproved);
        let bytes = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: Vec::new(),
        })
        .expect("assemble archive");
        // Any byte tampering breaks the safe-ZIP manifest verification.
        let mut tampered = bytes.clone();
        let needle = record.presentation_sha256.to_string();
        let haystack = tampered.clone();
        let position = haystack
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .expect("embedded presentation digest is in the archive bytes");
        tampered[position] = if tampered[position] == b'1' { b'2' } else { b'1' };
        assert!(verify_trust_finalization_archive(&tampered).is_err());
    }

    #[test]
    fn archive_rejects_second_prose_surface() {
        let record = approval_record(EventKind::AdvanceGateApproved);
        let error = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &record,
            claim_rows_bytes: fixture_claim_rows_bytes(&record),
            artifacts: vec![PackageArtifact {
                role: "prose".into(),
                path: "README.md".into(),
                bytes: b"smuggled prose".to_vec(),
            }],
        })
        .unwrap_err();
        assert_eq!(error.code, "package_unapproved_claim_presentation");
    }

    #[test]
    fn final_archive_reread_matches_live_rust_record_and_claim_row() {
        let target = crate::model::ChallengeTargetId::from("goal:generic_archive");
        let source = "#[test] fn witness() { println!(\"archive\"); }\n";
        let record = crate::trust_base::artifact::RustWitnessArtifactRecord {
            schema: crate::trust_base::artifact::RUST_WITNESS_ARTIFACT_SCHEMA.into(),
            target_id: target.clone(),
            relative_path: crate::trust_base::artifact::rust_witness_relative_path(&target),
            artifact_sha256: raw_sha256(source.as_bytes()),
            pinned_crate_tree_sha256: raw_sha256(b"generic crate tree"),
            execution: crate::trust_base::artifact::RustWitnessExecution::NotAttempted,
            correspondence: crate::trust_base::artifact::RustWitnessCorrespondence::NotReviewed,
            freeze_episode_id: "advance:generic-archive".into(),
            gate_episode_id: "advance:generic-archive".into(),
        };
        let payload = crate::trust_base::artifact::RustWitnessArtifactPayload {
            source_utf8: source.into(),
            runner_request: serde_json::json!({"frozen": "runner request"}),
            execution_receipt: None,
            correspondence_request: None,
            correspondence_verdict: None,
        };
        crate::trust_base::SchemaRegistry::v1()
            .unwrap()
            .validate(
                crate::trust_base::artifact::RUST_WITNESS_ARTIFACT_SCHEMA_ID,
                &serde_json::json!(&record),
            )
            .unwrap();
        let records = BTreeMap::from([(target.clone(), record.clone())]);
        let payloads = BTreeMap::from([(target, payload)]);
        let artifacts =
            crate::trust_base::artifact::artifact_package_members(&records, &payloads).unwrap();
        let approval = approval_record(EventKind::AdvanceGateApproved);
        let mut claim_rows: Value =
            serde_json::from_slice(&fixture_claim_rows_bytes(&approval)).unwrap();
        claim_rows["rows"][0]["seeded_resolution"] = serde_json::json!("decide");
        claim_rows["rows"][0]["selected_polarity"] = serde_json::json!("disprove");
        claim_rows["rows"][0]["terminal_outcome"] = serde_json::json!({
            "kind": "disproved",
            "proof_subject_sha256": raw_sha256(b"negative proof closure"),
            "artifact": {"kind": "present", "record": &record}
        });
        let claim_rows_bytes = canonical_json_value(&claim_rows).unwrap();
        let bytes = assemble_trust_finalization_archive(TrustFinalizationArchiveRequest {
            approval_record: &approval,
            claim_rows_bytes,
            artifacts: artifacts.clone(),
        })
        .unwrap();
        verify_trust_finalization_archive(&bytes).unwrap();
        for artifact in &artifacts {
            let archived = read_package_member(
                &bytes,
                &artifact.path,
                u64::try_from(artifact.bytes.len()).unwrap(),
            )
            .unwrap();
            assert_eq!(archived, artifact.bytes, "archive member {} drifted", artifact.path);
        }
        let archived_rows = read_package_member(&bytes, CLAIM_ROWS_PATH, MAX_CLAIM_ROWS_BYTES).unwrap();
        let archived_rows: Value = serde_json::from_slice(&archived_rows).unwrap();
        assert_eq!(
            archived_rows["rows"][0]["terminal_outcome"]["artifact"]["record"],
            serde_json::json!(&record)
        );
    }
}
