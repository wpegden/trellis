//! Transitive input closure and seed-definition byte verification.

use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, tagged_hash, verify_self_digest,
    DomainTag, Sha256Digest, TrustError,
};
use super::records::AuthoritativeRecord;
use super::schema::{RecordContract, SchemaRegistry};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MASKED_TRUSTED_PLATFORM_SENTINEL: &str =
    "__trellis_masked_trusted_platform_tool_identity__";

fn mask_entire_subtree_preserving_shape(
    value: &serde_json::Value,
    sentinel: &str,
) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| mask_entire_subtree_preserving_shape(value, sentinel))
                .collect(),
        ),
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, child)| {
                    (
                        key.clone(),
                        mask_entire_subtree_preserving_shape(child, sentinel),
                    )
                })
                .collect(),
        ),
        _ => serde_json::Value::String(sentinel.to_owned()),
    }
}

/// Apply a closed, compiled-in JSON projection. Masked members retain their
/// key and receive a fixed sentinel; subtree members mask their complete
/// value at that key. Nothing in this projection is caller-configurable.
fn mask_closed_members(
    value: &serde_json::Value,
    masked_value_fields: &[&str],
    masked_subtree_fields: &[&str],
    sentinel: &str,
    recursive: bool,
) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) if recursive => {
            serde_json::Value::Array(
                values
                    .iter()
                    .map(|value| {
                        mask_closed_members(
                            value,
                            masked_value_fields,
                            masked_subtree_fields,
                            sentinel,
                            recursive,
                        )
                    })
                    .collect(),
            )
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, child)| {
                    let masked = if masked_subtree_fields.contains(&key.as_str()) {
                        mask_entire_subtree_preserving_shape(child, sentinel)
                    } else if masked_value_fields.contains(&key.as_str()) {
                        serde_json::Value::String(sentinel.to_owned())
                    } else if recursive {
                        mask_closed_members(
                            child,
                            masked_value_fields,
                            masked_subtree_fields,
                            sentinel,
                            recursive,
                        )
                    } else {
                        child.clone()
                    };
                    (key.clone(), masked)
                })
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}


/// Project a trusted-platform boundary to the platform facts that must remain
/// byte-identical across a runtime build. Receipt/manifest locator digests are
/// masked while meaning-bearing platform facts remain exact.
pub fn mask_trusted_platform_boundary(value: &serde_json::Value) -> serde_json::Value {
    const MASKED_DIGEST_FIELDS: [&str; 2] = [
        "runtime_kernel_build_receipt_sha256",
        "runtime_distribution_manifest_sha256",
    ];
    mask_closed_members(
        value,
        &MASKED_DIGEST_FIELDS,
        &[],
        MASKED_TRUSTED_PLATFORM_SENTINEL,
        false,
    )
}

#[derive(Clone, Debug)]
pub struct VerifiedSeedDefinitionClosure {
    pub seed_manifest_sha256: Sha256Digest,
    pub bundle_sha256: Sha256Digest,
    pub records_by_digest: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    pub canonical_values_by_digest: BTreeMap<Sha256Digest, serde_json::Value>,
}


pub const ADAPTATION_LEDGER_ROW_SCHEMA: &str = "trellis-campaign-adaptation-ledger-row/v1";
pub const PHASE0_SOURCE_ADAPTATION_DEFINITION_SCHEMA: &str =
    "trellis-campaign-phase0-source-adaptation/v1";

pub fn seed_phase0_trust_roots_projection(
    closure: &VerifiedSeedDefinitionClosure,
) -> Result<crate::phase0::Phase0TrustRoots, TrustError> {
    let mut matches = closure.canonical_values_by_digest.values().filter(|body| {
        body.get("schema").and_then(serde_json::Value::as_str)
            == Some(PHASE0_SOURCE_ADAPTATION_DEFINITION_SCHEMA)
    });
    let body = matches.next().ok_or_else(|| {
        TrustError::new(
            "phase0_source_adaptation_definition_missing",
            "verified seed lacks its Phase-0 source-adaptation definition",
        )
    })?;
    if matches.next().is_some() {
        return Err(TrustError::new(
            "phase0_source_adaptation_definition_duplicate",
            "verified seed repeats its Phase-0 source-adaptation definition",
        ));
    }
    let roots: crate::phase0::Phase0TrustRoots = serde_json::from_value(
        body.get("roots")
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "phase0_source_adaptation_definition_invalid",
                    "Phase-0 source-adaptation definition lacks roots",
                )
            })?,
    )
    .map_err(|error| {
        TrustError::new(
            "phase0_source_adaptation_definition_invalid",
            error.to_string(),
        )
    })?;
    roots.validate().map_err(|error| {
        TrustError::new("phase0_source_adaptation_definition_invalid", error.detail)
    })?;
    Ok(roots)
}


/// Project the seed adaptation-ledger rows from an already verified seed
/// closure (plan doc 32 Stage 4, Codex 10 — the carrier and its first
/// writer land together).  Rows are ordered by id for determinism and must
/// carry seed status.
pub fn seed_adaptation_ledger_projection(
    closure: &VerifiedSeedDefinitionClosure,
) -> Result<Vec<super::campaign_plan::AdaptationLedgerEntry>, TrustError> {
    let mut rows = Vec::new();
    let mut ids = BTreeSet::new();
    for body in closure.canonical_values_by_digest.values().filter(|body| {
        body.get("schema").and_then(serde_json::Value::as_str)
            == Some(ADAPTATION_LEDGER_ROW_SCHEMA)
    }) {
        let row_value = body.get("row").ok_or_else(|| {
            TrustError::new(
                "adaptation_ledger_definition_invalid",
                "adaptation-ledger definition lacks its row body",
            )
        })?;
        let row: super::campaign_plan::AdaptationLedgerEntry =
            serde_json::from_value(row_value.clone()).map_err(|error| {
                TrustError::new("adaptation_ledger_definition_invalid", error.to_string())
            })?;
        if row.status != super::campaign_plan::AdaptationLedgerStatus::Seed {
            return Err(TrustError::new(
                "adaptation_ledger_definition_invalid",
                "seed-borne adaptation-ledger rows must carry seed status",
            ));
        }
        if !ids.insert(row.id.clone()) {
            return Err(TrustError::new(
                "adaptation_ledger_definition_invalid",
                format!("verified seed repeats adaptation-ledger row {:?}", row.id),
            ));
        }
        rows.push(row);
    }
    rows.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    Ok(rows)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEvidenceClosure {
    pub manifest_sha256: Sha256Digest,
    pub evidence_tool_input_root: Sha256Digest,
    pub file_count: usize,
    pub leaves_by_logical_id: BTreeMap<String, VerifiedEvidenceLeaf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedEvidenceLeaf {
    pub kind: String,
    pub relative_path: String,
    pub byte_length: u64,
    pub raw_sha256: Sha256Digest,
    pub dependency_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEvidenceLeaf {
    pub logical_id: String,
    pub kind: String,
    pub relative_path: String,
    pub path: PathBuf,
    pub raw_sha256: Sha256Digest,
}

/// Project the closed v1 set of seed-owned Lean definition carriers from an
/// already verified evidence closure.  The logical ID, evidence kind, and
/// path are all load-bearing: a same-named arbitrary evidence leaf must not
/// become a dependency outside the ordinary Tablet node lifecycle.
pub fn seed_support_definition_projection(
    closure: &VerifiedEvidenceClosure,
) -> Result<BTreeMap<crate::model::NodeId, crate::model::TrustSeedSupportDefinition>, TrustError> {
    const LOGICAL_ID: &str = "aeneas-validity-definitions";
    const EVIDENCE_PATH: &str = "model/Assumptions.lean";

    let leaf = closure.leaves_by_logical_id.get(LOGICAL_ID).ok_or_else(|| {
        TrustError::new(
            "seed_support_definition_missing",
            format!("evidence closure lacks required logical ID {LOGICAL_ID}"),
        )
    })?;
    if leaf.kind != "model_refinement_input" || leaf.relative_path != EVIDENCE_PATH {
        return Err(TrustError::new(
            "seed_support_definition_wrong_type",
            format!(
                "{LOGICAL_ID} must be kind model_refinement_input at {EVIDENCE_PATH}"
            ),
        ));
    }
    if leaf.raw_sha256 == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "seed_support_definition_placeholder_digest",
            format!("{LOGICAL_ID} carries the zero digest sentinel"),
        ));
    }

    Ok(BTreeMap::from([(
        crate::model::NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE),
        crate::model::TrustSeedSupportDefinition {
            logical_id: LOGICAL_ID.to_owned(),
            evidence_relative_path: EVIDENCE_PATH.to_owned(),
            raw_sha256: leaf.raw_sha256,
            definition_utf8: None,
        },
    )]))
}

fn verified_seed_support_definition_bytes(
    tablet_root: &Path,
    projection: &BTreeMap<crate::model::NodeId, crate::model::TrustSeedSupportDefinition>,
) -> Result<BTreeMap<crate::model::NodeId, Vec<u8>>, TrustError> {
    let tablet_metadata = fs::symlink_metadata(tablet_root).map_err(|error| {
        TrustError::new(
            "seed_support_tablet_root_unavailable",
            format!("{}: {error}", tablet_root.display()),
        )
    })?;
    if !tablet_metadata.file_type().is_dir() {
        return Err(TrustError::new(
            "seed_support_tablet_root_wrong_file_type",
            format!("{} must be a real directory", tablet_root.display()),
        ));
    }
    let mut verified = BTreeMap::new();
    for (node, support) in projection {
        let path = tablet_root.join(format!("{}.lean", node.as_str()));
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            TrustError::new(
                "seed_support_definition_unavailable",
                format!("{}: {error}", path.display()),
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(TrustError::new(
                "seed_support_definition_wrong_file_type",
                format!("{} must be a regular file", path.display()),
            ));
        }
        let bytes = fs::read(&path).map_err(|error| {
            TrustError::new(
                "seed_support_definition_unreadable",
                format!("{}: {error}", path.display()),
            )
        })?;
        if raw_sha256(&bytes) != support.raw_sha256 {
            return Err(TrustError::new(
                "seed_support_definition_digest_mismatch",
                format!(
                    "{} differs from authenticated evidence leaf {}",
                    path.display(),
                    support.logical_id
                ),
            ));
        }
        if support
            .definition_utf8
            .as_ref()
            .is_some_and(|definition| definition.as_bytes() != bytes)
        {
            return Err(TrustError::new(
                "seed_support_definition_body_mismatch",
                format!(
                    "{} differs from the reviewable body recorded for {}",
                    path.display(),
                    support.logical_id
                ),
            ));
        }
        verified.insert(node.clone(), bytes);
    }
    Ok(verified)
}

/// Revalidate the campaign-side copies of the projected support definitions.
/// These are importable Lean files, so a proof probe observes the campaign
/// copy rather than the evidence-root copy.  Reject symlinks and require exact
/// bytes before a non-node dependency may be admitted.
pub fn verify_seed_support_definition_files(
    tablet_root: &Path,
    projection: &BTreeMap<crate::model::NodeId, crate::model::TrustSeedSupportDefinition>,
) -> Result<(), TrustError> {
    verified_seed_support_definition_bytes(tablet_root, projection).map(|_| ())
}

/// Return the authenticated projection with exact UTF-8 bodies attached.
/// The body is claim-surface data, not new authority: its bytes must match
/// the already sealed raw digest before they enter protocol state.
pub fn hydrate_seed_support_definition_files(
    tablet_root: &Path,
    projection: &BTreeMap<crate::model::NodeId, crate::model::TrustSeedSupportDefinition>,
) -> Result<BTreeMap<crate::model::NodeId, crate::model::TrustSeedSupportDefinition>, TrustError> {
    let verified = verified_seed_support_definition_bytes(tablet_root, projection)?;
    let mut hydrated = projection.clone();
    for (node, bytes) in verified {
        let definition_utf8 = String::from_utf8(bytes).map_err(|error| {
            TrustError::new(
                "seed_support_definition_not_utf8",
                format!("{}: {error}", node.as_str()),
            )
        })?;
        hydrated
            .get_mut(&node)
            .expect("verified key came from projection")
            .definition_utf8 = Some(definition_utf8);
    }
    Ok(hydrated)
}

impl VerifiedEvidenceClosure {
    /// Revalidate and digest the one trusted-platform boundary leaf after
    /// applying its closed tool-invariant projection. The evidence manifest
    /// continues to authenticate the raw artifact; this digest is the
    /// separately persisted comparison pin used by distribution re-pins.
    pub fn trusted_platform_boundary_projection_sha256(
        &self,
        evidence_root: &Path,
    ) -> Result<Sha256Digest, TrustError> {
        const LOGICAL_ID: &str = "trusted-platform-boundary-v1";
        const EVIDENCE_PATH: &str = "trusted-platform/BOUNDARY.json";

        let leaf = self.leaves_by_logical_id.get(LOGICAL_ID).ok_or_else(|| {
            TrustError::new(
                "trusted_platform_boundary_missing",
                format!("evidence closure lacks required logical ID {LOGICAL_ID}"),
            )
        })?;
        if leaf.kind != "trusted_platform_boundary" || leaf.relative_path != EVIDENCE_PATH {
            return Err(TrustError::new(
                "trusted_platform_boundary_wrong_type",
                format!(
                    "{LOGICAL_ID} must be kind trusted_platform_boundary at {EVIDENCE_PATH}"
                ),
            ));
        }
        let path = self.resolve_leaf_path(evidence_root, LOGICAL_ID)?;
        let bytes = fs::read(&path).map_err(|error| {
            TrustError::new(
                "trusted_platform_boundary_unreadable",
                format!("{}: {error}", path.display()),
            )
        })?;
        if bytes.len() as u64 != leaf.byte_length || raw_sha256(&bytes) != leaf.raw_sha256 {
            return Err(TrustError::new(
                "trusted_platform_boundary_digest_mismatch",
                format!("{} changed after evidence verification", path.display()),
            ));
        }
        let boundary = parse_json_strict(&bytes).map_err(|error| {
            TrustError::new(
                "trusted_platform_boundary_json_invalid",
                error.to_string(),
            )
        })?;
        if canonical_json_value(&boundary)? != bytes {
            return Err(TrustError::new(
                "trusted_platform_boundary_not_canonical",
                "trusted-platform boundary must be exact canonical JSON",
            ));
        }
        if boundary.get("schema").and_then(serde_json::Value::as_str)
            != Some("trellis-trusted-platform-boundary/v1")
        {
            return Err(TrustError::new(
                "trusted_platform_boundary_wrong_schema",
                "expected trellis-trusted-platform-boundary/v1",
            ));
        }
        Ok(raw_sha256(&canonical_json_value(
            &mask_trusted_platform_boundary(&boundary),
        )?))
    }

    /// Resolve one logical evidence member beneath the configured evidence
    /// root and revalidate the exact file at the point of use.
    ///
    /// Manifest verification proves the closure at load time. Runtime tool
    /// execution happens later, so callers must not simply join the recorded
    /// relative path and assume the filesystem is unchanged. This lookup
    /// rejects symlinks in every relative component and rechecks the leaf's
    /// byte length and digest before returning its canonical path. The pinned
    /// execution layer repeats the final-file digest check immediately before
    /// spawning the process.
    pub fn resolve_leaf_path(
        &self,
        evidence_root: &Path,
        logical_id: &str,
    ) -> Result<PathBuf, TrustError> {
        let leaf = self.leaves_by_logical_id.get(logical_id).ok_or_else(|| {
            TrustError::new(
                "approved_evidence_leaf_missing",
                format!("evidence closure lacks logical ID {logical_id}"),
            )
        })?;
        validate_relative_path(&leaf.relative_path)?;
        if leaf.raw_sha256 == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "approved_evidence_leaf_placeholder_digest",
                format!("evidence leaf {logical_id} carries the zero sentinel"),
            ));
        }

        let root = fs::canonicalize(evidence_root).map_err(|error| {
            TrustError::new(
                "evidence_root_unavailable",
                format!("{}: {error}", evidence_root.display()),
            )
        })?;
        if !fs::metadata(&root)
            .map_err(|error| TrustError::new("evidence_root_unavailable", error.to_string()))?
            .is_dir()
        {
            return Err(TrustError::new(
                "evidence_root_not_directory",
                format!("{} is not a directory", root.display()),
            ));
        }

        let components: Vec<_> = Path::new(&leaf.relative_path).components().collect();
        let mut candidate = root.clone();
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(component) = component else {
                return Err(TrustError::new(
                    "evidence_relative_path_invalid",
                    format!("invalid relative POSIX path {:?}", leaf.relative_path),
                ));
            };
            candidate.push(component);
            let metadata = fs::symlink_metadata(&candidate).map_err(|error| {
                TrustError::new(
                    "evidence_leaf_unavailable",
                    format!("{}: {error}", candidate.display()),
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(TrustError::new(
                    "evidence_leaf_symlink_forbidden",
                    format!("{} is a symlink", candidate.display()),
                ));
            }
            let is_final = index + 1 == components.len();
            if (is_final && !metadata.is_file()) || (!is_final && !metadata.is_dir()) {
                return Err(TrustError::new(
                    "evidence_leaf_path_type_mismatch",
                    format!("{} has the wrong file type", candidate.display()),
                ));
            }
        }

        let bytes = fs::read(&candidate).map_err(|error| {
            TrustError::new(
                "evidence_leaf_unreadable",
                format!("{}: {error}", candidate.display()),
            )
        })?;
        if bytes.len() as u64 != leaf.byte_length || raw_sha256(&bytes) != leaf.raw_sha256 {
            return Err(TrustError::new(
                "evidence_leaf_changed_since_verification",
                format!("{} differs from approved leaf {logical_id}", candidate.display()),
            ));
        }
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            TrustError::new(
                "evidence_leaf_unavailable",
                format!("{}: {error}", candidate.display()),
            )
        })?;
        if !canonical.starts_with(&root) {
            return Err(TrustError::new(
                "evidence_leaf_escapes_root",
                format!("{} escapes {}", canonical.display(), root.display()),
            ));
        }
        Ok(canonical)
    }

    /// Resolve the unique manifest member carrying a contract-pinned digest.
    /// Runtime source-validation contracts pin executable hashes rather than
    /// logical IDs, so this returns both values needed by `SourceToolInvocation`.
    /// Byte-identical duplicate leaves are rejected as ambiguous.
    pub fn resolve_leaf_by_digest(
        &self,
        evidence_root: &Path,
        digest: Sha256Digest,
    ) -> Result<ResolvedEvidenceLeaf, TrustError> {
        if digest == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "approved_evidence_leaf_placeholder_digest",
                "cannot resolve the zero sentinel as an approved evidence leaf",
            ));
        }
        let mut matches = self
            .leaves_by_logical_id
            .iter()
            .filter(|(_, leaf)| leaf.raw_sha256 == digest);
        let (logical_id, leaf) = matches.next().ok_or_else(|| {
            TrustError::new(
                "approved_evidence_digest_missing",
                format!("evidence closure lacks digest {digest}"),
            )
        })?;
        if matches.next().is_some() {
            return Err(TrustError::new(
                "approved_evidence_digest_ambiguous",
                format!("multiple evidence leaves carry digest {digest}"),
            ));
        }
        let path = self.resolve_leaf_path(evidence_root, logical_id)?;
        Ok(ResolvedEvidenceLeaf {
            logical_id: logical_id.clone(),
            kind: leaf.kind.clone(),
            relative_path: leaf.relative_path.clone(),
            path,
            raw_sha256: leaf.raw_sha256,
        })
    }
}

/// Verify that every seed manifest entry has one exact canonical definition
/// body and that the body hashes under the seed-declared, closed domain tag.
pub fn verify_seed_definition_bundle(
    seed: &AuthoritativeRecord,
    bundle_bytes: &[u8],
) -> Result<VerifiedSeedDefinitionClosure, TrustError> {
    super::seed::validate_seed_manifest_semantics(seed)?;
    let value = parse_json_strict(bundle_bytes)
        .map_err(|error| TrustError::new("seed_bundle_json_invalid", error.to_string()))?;
    if canonical_json_value(&value)? != bundle_bytes {
        return Err(TrustError::new(
            "seed_bundle_not_canonical",
            "seed definition bundle must be exact canonical JSON",
        ));
    }
    if string_field(&value, "schema")? != "trellis-seed-definition-bundle/v1" {
        return Err(TrustError::new(
            "seed_bundle_wrong_schema",
            "expected trellis-seed-definition-bundle/v1",
        ));
    }
    let bundle_sha256 = verify_self_digest(
        DomainTag::ManifestNode,
        &value,
        "bundle_sha256",
    )?;
    let declared = seed
        .value()
        .get("definitions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| TrustError::new("seed_definitions_missing", "seed lacks definitions"))?;
    let supplied = value
        .get("definitions")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| TrustError::new("seed_bundle_definitions_missing", "bundle lacks definitions"))?;
    if declared.len() != supplied.len() {
        return Err(TrustError::new(
            "seed_bundle_definition_count_mismatch",
            "bundle must contain exactly one body for every seed definition",
        ));
    }
    let registry = SchemaRegistry::v1()?;
    let mut records = BTreeMap::new();
    let mut values = BTreeMap::new();
    for (index, (identity, item)) in declared.iter().zip(supplied).enumerate() {
        for field in [
            "record_kind",
            "record_id",
            "record_schema_id",
            "record_sha256",
            "domain_tag",
        ] {
            if identity.get(field) != item.get(field) {
                return Err(TrustError::new(
                    "seed_bundle_identity_mismatch",
                    format!("definition {index} differs from the seed at {field}"),
                ));
            }
        }
        let body = item.get("canonical_value").ok_or_else(|| {
            TrustError::new(
                "seed_bundle_body_missing",
                format!("definition {index} lacks canonical_value"),
            )
        })?;
        let tag = DomainTag::parse_registered(string_field(item, "domain_tag")?)?;
        let expected: Sha256Digest = string_field(item, "record_sha256")?.parse()?;
        let schema_id = string_field(item, "record_schema_id")?;
        let kind = string_field(item, "record_kind")?;
        let digest = match registered_record_for_seed(&registry, kind, schema_id, body.clone())? {
            Some(record) => {
                if record.contract().domain_tag != tag {
                    return Err(TrustError::new(
                        "seed_bundle_record_tag_mismatch",
                        format!("definition {index} schema and domain tag disagree"),
                    ));
                }
                let digest = record.digest();
                if records.insert(digest, record).is_some() {
                    return Err(TrustError::new(
                        "seed_bundle_duplicate_digest",
                        "seed definition bodies must have unique identities",
                    ));
                }
                digest
            }
            None => tagged_hash(tag, &canonical_json_value(body)?),
        };
        if digest != expected {
            return Err(TrustError::new(
                "seed_bundle_body_digest_mismatch",
                format!("definition {index} body hashes to {digest}, expected {expected}"),
            ));
        }
        if values.insert(digest, body.clone()).is_some() {
            return Err(TrustError::new(
                "seed_bundle_duplicate_digest",
                "seed definition bodies must have unique identities",
            ));
        }
    }
    Ok(VerifiedSeedDefinitionClosure {
        seed_manifest_sha256: seed.digest(),
        bundle_sha256,
        records_by_digest: records,
        canonical_values_by_digest: values,
    })
}

/// Recompute the seed-approved evidence/tool input root from a complete,
/// dependency-checked manifest and exact files under `base_dir`.
pub fn verify_evidence_tool_manifest(
    base_dir: &Path,
    manifest_bytes: &[u8],
) -> Result<VerifiedEvidenceClosure, TrustError> {
    let value = parse_json_strict(manifest_bytes)
        .map_err(|error| TrustError::new("evidence_manifest_json_invalid", error.to_string()))?;
    if canonical_json_value(&value)? != manifest_bytes {
        return Err(TrustError::new(
            "evidence_manifest_not_canonical",
            "evidence manifest must be exact canonical JSON",
        ));
    }
    if string_field(&value, "schema")? != "trellis-evidence-tool-manifest/v1" {
        return Err(TrustError::new(
            "evidence_manifest_wrong_schema",
            "expected trellis-evidence-tool-manifest/v1",
        ));
    }
    let manifest_sha256 = verify_self_digest(
        DomainTag::ManifestNode,
        &value,
        "manifest_sha256",
    )?;
    let leaves = value
        .get("leaves")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| TrustError::new("evidence_manifest_leaves_missing", "leaves missing"))?;
    let mut prior_sort_key: Option<Vec<u8>> = None;
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut dependencies: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut verified_leaves = BTreeMap::new();
    for leaf in leaves {
        let kind = string_field(leaf, "kind")?;
        let logical_id = string_field(leaf, "logical_id")?;
        let relative = string_field(leaf, "relative_path")?;
        validate_relative_path(relative)?;
        let sort_key = canonical_json_value(&serde_json::json!([kind, logical_id, relative]))?;
        if prior_sort_key.as_ref().is_some_and(|prior| prior >= &sort_key) {
            return Err(TrustError::new(
                "evidence_manifest_order_invalid",
                "leaves must be strictly sorted by kind, logical_id, relative_path",
            ));
        }
        prior_sort_key = Some(sort_key);
        if !ids.insert(logical_id.to_owned()) || !paths.insert(relative.to_owned()) {
            return Err(TrustError::new(
                "evidence_manifest_duplicate_leaf",
                "logical IDs and relative paths must be unique",
            ));
        }
        let deps = leaf
            .get("dependency_ids")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                TrustError::new("evidence_dependencies_missing", "dependency_ids missing")
            })?;
        let mut prior_dep: Option<&str> = None;
        let mut decoded = Vec::new();
        for dep in deps {
            let dep = dep.as_str().ok_or_else(|| {
                TrustError::new("evidence_dependency_invalid", "dependency must be a string")
            })?;
            if prior_dep.is_some_and(|prior| prior.as_bytes() >= dep.as_bytes()) {
                return Err(TrustError::new(
                    "evidence_dependency_order_invalid",
                    "dependency IDs must be unique UTF-8 byte sorted",
                ));
            }
            prior_dep = Some(dep);
            decoded.push(dep.to_owned());
        }
        dependencies.insert(logical_id.to_owned(), decoded);
        let path = base_dir.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            TrustError::new(
                "evidence_input_missing",
                format!("{}: {error}", path.display()),
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(TrustError::new(
                "evidence_input_not_regular",
                format!("{} is not a regular file", path.display()),
            ));
        }
        let bytes = fs::read(&path).map_err(|error| {
            TrustError::new("evidence_input_unreadable", format!("{}: {error}", path.display()))
        })?;
        let declared_len = leaf
            .get("byte_length")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| TrustError::new("evidence_length_invalid", "byte_length invalid"))?;
        let declared_digest: Sha256Digest = string_field(leaf, "sha256_of_raw_bytes")?.parse()?;
        if declared_len != bytes.len() as u64 || declared_digest != raw_sha256(&bytes) {
            return Err(TrustError::new(
                "evidence_input_digest_mismatch",
                format!("{} differs from its leaf", path.display()),
            ));
        }
        validate_optional_pair(leaf, "producer_id", "producer_hash")?;
        verified_leaves.insert(
            logical_id.to_owned(),
            VerifiedEvidenceLeaf {
                kind: kind.to_owned(),
                relative_path: relative.to_owned(),
                byte_length: declared_len,
                raw_sha256: declared_digest,
                dependency_ids: dependencies
                    .get(logical_id)
                    .cloned()
                    .expect("dependencies were inserted above"),
            },
        );
    }
    for (id, deps) in &dependencies {
        for dep in deps {
            if !ids.contains(dep) {
                return Err(TrustError::new(
                    "evidence_dependency_missing",
                    format!("{id} depends on missing {dep}"),
                ));
            }
        }
    }
    reject_dependency_cycles(&dependencies)?;
    let actual_paths = regular_files_under(base_dir)?;
    if actual_paths != paths {
        return Err(TrustError::new(
            "evidence_manifest_not_complete",
            "evidence root contains a missing or unmanifested regular file",
        ));
    }
    let evidence_tool_input_root = tagged_hash(
        DomainTag::EvidenceToolRoot,
        &canonical_json_value(&serde_json::Value::Array(leaves.clone()))?,
    );
    let declared_root: Sha256Digest = string_field(&value, "evidence_tool_input_root")?.parse()?;
    if declared_root != evidence_tool_input_root {
        return Err(TrustError::new(
            "evidence_root_mismatch",
            format!("declared {declared_root}, recomputed {evidence_tool_input_root}"),
        ));
    }
    Ok(VerifiedEvidenceClosure {
        manifest_sha256,
        evidence_tool_input_root,
        file_count: leaves.len(),
        leaves_by_logical_id: verified_leaves,
    })
}

fn registered_record_for_seed(
    registry: &SchemaRegistry,
    kind: &str,
    schema_id: &str,
    value: serde_json::Value,
) -> Result<Option<AuthoritativeRecord>, TrustError> {
    // Q4 (Stage 1) retired qualification profiles; Stage 3 removed the
    // schema-selected `parse_as` path with the journal family.  A seed that
    // still carries the retired kind fails loud instead of silently loading.
    if kind == "qualification_profile" {
        return Err(TrustError::new(
            "seed_bundle_retired_record_kind",
            "qualification profiles were retired (Q4, plan doc 32)",
        ));
    }
    let schema = value.get("schema").and_then(serde_json::Value::as_str);
    let Some(schema) = schema else {
        return Ok(None);
    };
    let contract = match RecordContract::for_record_schema(schema) {
        Ok(contract) => contract,
        Err(_) => return Ok(None),
    };
    if contract.schema_id != schema_id {
        return Err(TrustError::new(
            "seed_bundle_schema_id_mismatch",
            "record discriminator and seed schema ID disagree",
        ));
    }
    AuthoritativeRecord::parse(registry, value).map(Some)
}

fn reject_dependency_cycles(graph: &BTreeMap<String, Vec<String>>) -> Result<(), TrustError> {
    fn visit(
        id: &str,
        graph: &BTreeMap<String, Vec<String>>,
        visiting: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> Result<(), TrustError> {
        if complete.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            return Err(TrustError::new(
                "evidence_dependency_cycle",
                format!("dependency cycle reaches {id}"),
            ));
        }
        for dep in graph.get(id).into_iter().flatten() {
            visit(dep, graph, visiting, complete)?;
        }
        visiting.remove(id);
        complete.insert(id.to_owned());
        Ok(())
    }
    let mut complete = BTreeSet::new();
    for id in graph.keys() {
        visit(id, graph, &mut BTreeSet::new(), &mut complete)?;
    }
    Ok(())
}

fn regular_files_under(root: &Path) -> Result<BTreeSet<String>, TrustError> {
    fn walk(root: &Path, path: &Path, output: &mut BTreeSet<String>) -> Result<(), TrustError> {
        let mut entries = fs::read_dir(path)
            .map_err(|error| TrustError::new("evidence_root_unreadable", error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| TrustError::new("evidence_root_unreadable", error.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| TrustError::new("evidence_root_unreadable", error.to_string()))?;
            if metadata.file_type().is_symlink() {
                return Err(TrustError::new(
                    "evidence_symlink_forbidden",
                    format!("{} is a symlink", entry.path().display()),
                ));
            }
            if metadata.is_dir() {
                walk(root, &entry.path(), output)?;
            } else if metadata.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|error| TrustError::new("evidence_path_invalid", error.to_string()))?
                    .components()
                    .map(|component| component.as_os_str().to_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("/");
                validate_relative_path(&relative)?;
                output.insert(relative);
            } else {
                return Err(TrustError::new(
                    "evidence_special_file_forbidden",
                    format!("{} is not a regular file", entry.path().display()),
                ));
            }
        }
        Ok(())
    }
    let mut files = BTreeSet::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn validate_relative_path(path: &str) -> Result<(), TrustError> {
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
            "evidence_relative_path_invalid",
            format!("invalid relative POSIX path {path:?}"),
        ));
    }
    Ok(())
}

fn validate_optional_pair(
    value: &serde_json::Value,
    left: &str,
    right: &str,
) -> Result<(), TrustError> {
    if value.get(left).is_some() != value.get(right).is_some() {
        return Err(TrustError::new(
            "evidence_optional_pair_incomplete",
            format!("{left} and {right} must occur together"),
        ));
    }
    Ok(())
}

fn string_field<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(serde_json::Value::as_str).ok_or_else(|| {
        TrustError::new(
            "closure_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn support_evidence_closure(bytes: &[u8]) -> VerifiedEvidenceClosure {
        VerifiedEvidenceClosure {
            manifest_sha256: raw_sha256(b"manifest"),
            evidence_tool_input_root: raw_sha256(b"evidence-root"),
            file_count: 1,
            leaves_by_logical_id: BTreeMap::from([(
                "aeneas-validity-definitions".to_owned(),
                VerifiedEvidenceLeaf {
                    kind: "model_refinement_input".to_owned(),
                    relative_path: "model/Assumptions.lean".to_owned(),
                    byte_length: bytes.len() as u64,
                    raw_sha256: raw_sha256(bytes),
                    dependency_ids: Vec::new(),
                },
            )]),
        }
    }

    #[test]
    fn seed_support_projection_and_campaign_file_are_exactly_bound() {
        let bytes = b"def RustValidSliceU8 : Prop := True\n";
        let closure = support_evidence_closure(bytes);
        let projection = seed_support_definition_projection(&closure).unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("Assumptions.lean"), bytes).unwrap();

        verify_seed_support_definition_files(directory.path(), &projection).unwrap();
        let hydrated =
            hydrate_seed_support_definition_files(directory.path(), &projection).unwrap();
        assert_eq!(
            projection[&crate::model::NodeId::from("Assumptions")].raw_sha256,
            raw_sha256(bytes)
        );
        assert_eq!(
            hydrated[&crate::model::NodeId::from("Assumptions")]
                .definition_utf8
                .as_deref(),
            Some("def RustValidSliceU8 : Prop := True\n")
        );

        std::fs::write(
            directory.path().join("Assumptions.lean"),
            b"def RustValidSliceU8 : Prop := False\n",
        )
        .unwrap();
        let error = verify_seed_support_definition_files(directory.path(), &projection)
            .unwrap_err();
        assert_eq!(error.code, "seed_support_definition_digest_mismatch");
    }

    #[test]
    fn seed_support_projection_rejects_wrongly_typed_leaf() {
        let mut closure = support_evidence_closure(b"support");
        closure
            .leaves_by_logical_id
            .get_mut("aeneas-validity-definitions")
            .unwrap()
            .kind = "checker_source".to_owned();
        let error = seed_support_definition_projection(&closure).unwrap_err();
        assert_eq!(error.code, "seed_support_definition_wrong_type");
    }

    #[cfg(unix)]
    #[test]
    fn seed_support_campaign_file_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let bytes = b"def RustValidSliceU8 : Prop := True\n";
        let projection = seed_support_definition_projection(&support_evidence_closure(bytes))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("Assumptions.lean");
        std::fs::write(&target, bytes).unwrap();
        symlink(&target, directory.path().join("Assumptions.lean")).unwrap();

        let error = verify_seed_support_definition_files(directory.path(), &projection)
            .unwrap_err();
        assert_eq!(error.code, "seed_support_definition_wrong_file_type");
    }

    #[cfg(unix)]
    #[test]
    fn seed_support_campaign_tablet_root_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let bytes = b"def RustValidSliceU8 : Prop := True\n";
        let projection = seed_support_definition_projection(&support_evidence_closure(bytes))
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Assumptions.lean"), bytes).unwrap();
        let tablet = directory.path().join("Tablet");
        symlink(outside.path(), &tablet).unwrap();

        let error = verify_seed_support_definition_files(&tablet, &projection).unwrap_err();
        assert_eq!(error.code, "seed_support_tablet_root_wrong_file_type");
    }

    #[test]
    fn evidence_root_recomputes_exact_files_and_dependency_dag() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("tools")).unwrap();
        fs::write(directory.path().join("source.rs"), b"fn main() {}\n").unwrap();
        fs::write(directory.path().join("tools/checker"), b"checker-v1\n").unwrap();
        let leaves = serde_json::json!([
            {
                "kind": "source",
                "logical_id": "source",
                "relative_path": "source.rs",
                "byte_length": 13,
                "sha256_of_raw_bytes": raw_sha256(b"fn main() {}\n"),
                "dependency_ids": [],
            },
            {
                "kind": "tool",
                "logical_id": "checker",
                "relative_path": "tools/checker",
                "byte_length": 11,
                "sha256_of_raw_bytes": raw_sha256(b"checker-v1\n"),
                "producer_id": "rustc",
                "producer_hash": "11".repeat(32),
                "dependency_ids": ["source"],
            }
        ]);
        let root = tagged_hash(
            DomainTag::EvidenceToolRoot,
            &canonical_json_value(&leaves).unwrap(),
        );
        let mut manifest = serde_json::json!({
            "schema": "trellis-evidence-tool-manifest/v1",
            "leaves": leaves,
            "evidence_tool_input_root": root,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let digest = super::super::canonical::self_digest(
            DomainTag::ManifestNode,
            &manifest,
            "manifest_sha256",
        )
        .unwrap();
        manifest["manifest_sha256"] = serde_json::Value::String(digest.to_string());
        let bytes = canonical_json_value(&manifest).unwrap();
        let verified = verify_evidence_tool_manifest(directory.path(), &bytes).unwrap();
        assert_eq!(verified.evidence_tool_input_root, root);
        assert_eq!(
            verified.resolve_leaf_path(directory.path(), "checker").unwrap(),
            fs::canonicalize(directory.path().join("tools/checker")).unwrap()
        );
        let resolved = verified
            .resolve_leaf_by_digest(directory.path(), raw_sha256(b"checker-v1\n"))
            .unwrap();
        assert_eq!(resolved.logical_id, "checker");
        assert_eq!(
            resolved.path,
            fs::canonicalize(directory.path().join("tools/checker")).unwrap()
        );

        fs::write(directory.path().join("tools/checker"), b"changed\n").unwrap();
        let error = verified
            .resolve_leaf_path(directory.path(), "checker")
            .unwrap_err();
        assert_eq!(error.code, "evidence_leaf_changed_since_verification");
        assert!(verify_evidence_tool_manifest(directory.path(), &bytes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn evidence_leaf_lookup_rejects_symlinked_path_components() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("checker"), b"checker-v1\n").unwrap();
        symlink(outside.path(), directory.path().join("tools")).unwrap();
        let closure = VerifiedEvidenceClosure {
            manifest_sha256: raw_sha256(b"manifest"),
            evidence_tool_input_root: raw_sha256(b"root"),
            file_count: 1,
            leaves_by_logical_id: BTreeMap::from([(
                "checker".to_owned(),
                VerifiedEvidenceLeaf {
                    kind: "tool".to_owned(),
                    relative_path: "tools/checker".to_owned(),
                    byte_length: 11,
                    raw_sha256: raw_sha256(b"checker-v1\n"),
                    dependency_ids: Vec::new(),
                },
            )]),
        };
        let error = closure
            .resolve_leaf_path(directory.path(), "checker")
            .unwrap_err();
        assert_eq!(error.code, "evidence_leaf_symlink_forbidden");
    }

    #[test]
    fn evidence_digest_lookup_rejects_ambiguous_logical_ids() {
        let leaf = VerifiedEvidenceLeaf {
            kind: "tool".to_owned(),
            relative_path: "checker-a".to_owned(),
            byte_length: 7,
            raw_sha256: raw_sha256(b"checker"),
            dependency_ids: Vec::new(),
        };
        let mut duplicate = leaf.clone();
        duplicate.relative_path = "checker-b".to_owned();
        let closure = VerifiedEvidenceClosure {
            manifest_sha256: raw_sha256(b"manifest"),
            evidence_tool_input_root: raw_sha256(b"root"),
            file_count: 2,
            leaves_by_logical_id: BTreeMap::from([
                ("checker-a".to_owned(), leaf),
                ("checker-b".to_owned(), duplicate),
            ]),
        };
        let error = closure
            .resolve_leaf_by_digest(Path::new("."), raw_sha256(b"checker"))
            .unwrap_err();
        assert_eq!(error.code, "approved_evidence_digest_ambiguous");
    }

    #[test]
    fn dependency_cycle_and_unmanifested_file_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"a").unwrap();
        fs::write(directory.path().join("extra"), b"x").unwrap();
        let leaves = serde_json::json!([{
            "kind": "tool",
            "logical_id": "a",
            "relative_path": "a",
            "byte_length": 1,
            "sha256_of_raw_bytes": raw_sha256(b"a"),
            "dependency_ids": ["a"],
        }]);
        let root = tagged_hash(
            DomainTag::EvidenceToolRoot,
            &canonical_json_value(&leaves).unwrap(),
        );
        let mut manifest = serde_json::json!({
            "schema": "trellis-evidence-tool-manifest/v1",
            "leaves": leaves,
            "evidence_tool_input_root": root,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let digest = super::super::canonical::self_digest(
            DomainTag::ManifestNode,
            &manifest,
            "manifest_sha256",
        )
        .unwrap();
        manifest["manifest_sha256"] = serde_json::Value::String(digest.to_string());
        assert!(verify_evidence_tool_manifest(
            directory.path(),
            &canonical_json_value(&manifest).unwrap()
        )
        .is_err());
    }
}
