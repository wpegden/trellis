//! Cryptographic node/module certificates (artifact-membership v2).
//!
//! The replay-attested `ModuleData.constants` manifest is authoritative.  The
//! certificate stores and hashes it as a commitment; it is not a third
//! declaration enumeration.  Completeness comes from Lean's serialization
//! invariant, not from comparing aliases of the same list.  Membership is
//! intentionally non-exclusive because Lean permits multiple artifacts to
//! serialize the same theorem name with different proof values.

use crate::model::NodeId;
use crate::trust_base::Sha256Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const HASH_PREFIX: &[u8] = b"trellis-node-certificate-v2\0";

fn hash_parts(domain: &str, parts: impl IntoIterator<Item = Vec<u8>>) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(HASH_PREFIX);
    hasher.update((domain.len() as u32).to_be_bytes());
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    Sha256Digest::from_bytes(hasher.finalize().into())
}

fn digest_part(digest: Sha256Digest) -> Vec<u8> {
    digest.as_bytes().to_vec()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct DeclarationManifestEntry {
    /// Exact fully-qualified Lean declaration name.
    pub name: String,
    /// Environment constructor (`theorem`, `definition`, `inductive`, ...).
    pub kind: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestGap {
    /// In the artifact-local closure read but absent from replay evidence.
    pub closure_only: Vec<DeclarationManifestEntry>,
    /// In replay evidence but absent from the artifact-local closure read.
    pub replay_only: Vec<DeclarationManifestEntry>,
}

impl ManifestGap {
    pub fn is_empty(&self) -> bool {
        self.closure_only.is_empty() && self.replay_only.is_empty()
    }
}

pub fn manifest_gap(
    closure_manifest: &[DeclarationManifestEntry],
    replay_manifest: &[DeclarationManifestEntry],
) -> ManifestGap {
    let closure: BTreeSet<_> = closure_manifest.iter().cloned().collect();
    let replay: BTreeSet<_> = replay_manifest.iter().cloned().collect();
    ManifestGap {
        closure_only: closure.difference(&replay).cloned().collect(),
        replay_only: replay.difference(&closure).cloned().collect(),
    }
}

pub fn declaration_manifest_root(entries: &[DeclarationManifestEntry]) -> Sha256Digest {
    let sorted: BTreeSet<_> = entries.iter().cloned().collect();
    hash_parts(
        "declaration-manifest",
        sorted.into_iter().map(|entry| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(entry.name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(entry.name.as_bytes());
            bytes.extend_from_slice(&(entry.kind.len() as u64).to_be_bytes());
            bytes.extend_from_slice(entry.kind.as_bytes());
            bytes
        }),
    )
}

/// One exact member of the ordered artifact bundle replayed by LeanChecker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactBundlePart {
    /// `exported`, `server`, or `private`.
    pub level: String,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
}

/// Logical constants visible at one artifact level.  Levels are cumulative.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisibilityManifest {
    pub level: String,
    pub declarations: Vec<DeclarationManifestEntry>,
}

pub fn artifact_bundle_root(parts: &[ArtifactBundlePart]) -> Sha256Digest {
    hash_parts(
        "ordered-artifact-bundle",
        parts.iter().map(|part| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(part.level.len() as u64).to_be_bytes());
            bytes.extend_from_slice(part.level.as_bytes());
            bytes.extend_from_slice(&part.size_bytes.to_be_bytes());
            bytes.extend_from_slice(part.sha256.as_bytes());
            bytes
        }),
    )
}

pub fn visibility_manifest_root(manifest: &VisibilityManifest) -> Sha256Digest {
    hash_parts(
        "visibility-manifest",
        [
            manifest.level.as_bytes().to_vec(),
            digest_part(declaration_manifest_root(&manifest.declarations)),
        ],
    )
}

fn observed_uses_root(uses: &[ObservedDeclarationUse]) -> Sha256Digest {
    let sorted: BTreeSet<_> = uses.iter().cloned().collect();
    hash_parts(
        "observed-declaration-uses",
        sorted.into_iter().map(|observed| {
            let fields = [
                observed.owner.as_str(),
                observed.reached_declaration.as_str(),
                observed.declaration_kind.as_str(),
                observed.visibility.as_str(),
            ];
            let mut bytes = Vec::new();
            for field in fields {
                bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
                bytes.extend_from_slice(field.as_bytes());
            }
            bytes
        }),
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservedDeclarationUse {
    pub owner: NodeId,
    pub reached_declaration: String,
    pub declaration_kind: String,
    /// Artifact visibility through which this provider is available to the
    /// consumer (`exported`, `server`, or `private`).
    pub visibility: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeCertificateEvidence {
    pub principal_declaration: String,
    pub declaration_manifest: Vec<DeclarationManifestEntry>,
    pub replay_declaration_manifest: Vec<DeclarationManifestEntry>,
    /// Direct imports serialized in the replay-attested artifact's
    /// `ModuleData.imports`. `None` denotes legacy evidence that did not
    /// establish the import set; an imports-free module is `Some(empty)`.
    pub direct_imports: Option<BTreeSet<String>>,
    /// Replay-attested per-level manifests, ordered exactly like the bundle.
    pub visibility_manifests: Vec<VisibilityManifest>,
    pub exact_declaration_uses: Vec<ObservedDeclarationUse>,
    /// Exact bytes observed after replay and closure probing.  Issuance reads
    /// both files again and requires equality, closing the probe-to-issue race.
    pub source_sha256: String,
    pub artifact_bundle: Vec<ArtifactBundlePart>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateEdgeSemantics {
    PrincipalAssumption,
    #[default]
    CertifiedModuleUse,
    SemanticDefinitionUse,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExactUseWitness {
    pub consumer: NodeId,
    pub owner: NodeId,
    pub reached_declaration: String,
    pub declaration_kind: String,
    pub visibility: String,
    pub owner_root: Sha256Digest,
    pub membership_witness: Sha256Digest,
    pub semantics: CertificateEdgeSemantics,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeCertificate {
    pub version: String,
    pub node: NodeId,
    pub module_name: String,
    pub principal_declaration: String,
    pub declaration_manifest: Vec<DeclarationManifestEntry>,
    pub declaration_manifest_root: Sha256Digest,
    pub visibility_manifests: Vec<VisibilityManifest>,
    pub visibility_manifest_roots: BTreeMap<String, Sha256Digest>,
    pub source_sha256: Sha256Digest,
    pub artifact_bundle: Vec<ArtifactBundlePart>,
    pub artifact_bundle_root: Sha256Digest,
    /// Commits the exact source and exact replay-attested module artifact.
    pub local_module_root: Sha256Digest,
    /// Statement/type-side meaning used by correspondence approval.
    pub semantic_root: Sha256Digest,
    /// Any local declaration/proof or recursively certified dependency.
    pub logic_root: Sha256Digest,
    /// Source/artifact/import/environment/toolchain identity.
    pub build_root: Sha256Digest,
    pub dependency_certificate_roots: BTreeMap<NodeId, Sha256Digest>,
    pub axiom_policy_root: Sha256Digest,
    pub toolchain_root: Sha256Digest,
    pub certified_node_root: Sha256Digest,
    pub exact_use_witnesses: Vec<ExactUseWitness>,
    pub observed_declaration_uses: Vec<ObservedDeclarationUse>,
}

impl NodeCertificate {
    pub const VERSION: &'static str = "node-certificate-v2";

    pub fn manifest_entry(&self, exact_name: &str) -> Option<&DeclarationManifestEntry> {
        self.declaration_manifest
            .iter()
            .find(|entry| entry.name == exact_name)
    }

    pub fn membership_witness(&self, entry: &DeclarationManifestEntry) -> Sha256Digest {
        hash_parts(
            "manifest-membership",
            [
                digest_part(self.declaration_manifest_root),
                digest_part(self.local_module_root),
                entry.name.as_bytes().to_vec(),
                entry.kind.as_bytes().to_vec(),
            ],
        )
    }

    pub fn visible_manifest_entry(
        &self,
        visibility: &str,
        exact_name: &str,
    ) -> Option<&DeclarationManifestEntry> {
        self.visibility_manifests
            .iter()
            .find(|manifest| manifest.level == visibility)
            .and_then(|manifest| {
                manifest
                    .declarations
                    .iter()
                    .find(|entry| entry.name == exact_name)
            })
    }

    pub fn visible_membership_witness(
        &self,
        visibility: &str,
        entry: &DeclarationManifestEntry,
    ) -> Option<Sha256Digest> {
        let manifest_root = self.visibility_manifest_roots.get(visibility)?;
        Some(hash_parts(
            "visible-manifest-membership",
            [
                visibility.as_bytes().to_vec(),
                digest_part(*manifest_root),
                digest_part(self.artifact_bundle_root),
                entry.name.as_bytes().to_vec(),
                entry.kind.as_bytes().to_vec(),
            ],
        ))
    }

    pub fn roots_are_current(&self) -> bool {
        let module_owner_principal_is_valid = if self.node.as_str() == "Preamble" {
            self.principal_declaration.is_empty()
        } else {
            !self.principal_declaration.is_empty()
        };
        let expected_levels = ["exported", "server", "private"];
        let shape_is_current = !self.artifact_bundle.is_empty()
            && self.artifact_bundle.len() <= expected_levels.len()
            && self.artifact_bundle.len() == self.visibility_manifests.len()
            && self
                .artifact_bundle
                .iter()
                .zip(&self.visibility_manifests)
                .enumerate()
                .all(|(index, (part, manifest))| {
                    part.level == expected_levels[index]
                        && manifest.level == part.level
                        && part.sha256 != Sha256Digest::ZERO
                        && part.size_bytes > 0
                        && manifest
                            .declarations
                            .iter()
                            .map(|entry| &entry.name)
                            .collect::<BTreeSet<_>>()
                            .len()
                            == manifest.declarations.len()
                })
            && self.visibility_manifests.last().is_some_and(|manifest| {
                declaration_manifest_root(&manifest.declarations) == self.declaration_manifest_root
            })
            && self.visibility_manifests.windows(2).all(|levels| {
                levels[0].declarations.iter().all(|earlier| {
                    levels[1]
                        .declarations
                        .iter()
                        .any(|later| later.name == earlier.name)
                })
            });
        let manifest_names_unique = self
            .declaration_manifest
            .iter()
            .map(|entry| &entry.name)
            .collect::<BTreeSet<_>>()
            .len()
            == self.declaration_manifest.len();
        let expected_local_root = local_module_root(
            &self.module_name,
            self.source_sha256,
            self.artifact_bundle_root,
        );
        let expected_logic_root = hash_parts(
            "logic-root",
            std::iter::once(digest_part(expected_local_root)).chain(
                self.dependency_certificate_roots
                    .values()
                    .copied()
                    .map(digest_part),
            ),
        );
        let expected_build_root = hash_parts(
            "build-root",
            [
                digest_part(expected_local_root),
                digest_part(expected_logic_root),
                digest_part(self.toolchain_root),
                digest_part(self.axiom_policy_root),
            ],
        );
        let observed_keys: BTreeSet<_> = self
            .observed_declaration_uses
            .iter()
            .map(|observed| {
                (
                    observed.owner.clone(),
                    observed.reached_declaration.clone(),
                    observed.declaration_kind.clone(),
                    observed.visibility.clone(),
                )
            })
            .collect();
        let witness_keys: BTreeSet<_> = self
            .exact_use_witnesses
            .iter()
            .map(|witness| {
                (
                    witness.owner.clone(),
                    witness.reached_declaration.clone(),
                    witness.declaration_kind.clone(),
                    witness.visibility.clone(),
                )
            })
            .collect();
        let witnesses_are_current = observed_keys.len() == self.observed_declaration_uses.len()
            && witness_keys.len() == self.exact_use_witnesses.len()
            && observed_keys == witness_keys
            && self.exact_use_witnesses.iter().all(|witness| {
                witness.consumer == self.node
                    && witness.membership_witness != Sha256Digest::ZERO
                    && self
                        .dependency_certificate_roots
                        .get(&witness.owner)
                        .is_some_and(|root| *root == witness.owner_root)
            });
        if self.version != Self::VERSION
            || self.certified_node_root == Sha256Digest::ZERO
            || self.source_sha256 == Sha256Digest::ZERO
            || self.semantic_root == Sha256Digest::ZERO
            || self.axiom_policy_root == Sha256Digest::ZERO
            || self.toolchain_root == Sha256Digest::ZERO
            || self.module_name != format!("Tablet.{}", self.node)
            || !module_owner_principal_is_valid
            || !manifest_names_unique
            || self.declaration_manifest_root
                != declaration_manifest_root(&self.declaration_manifest)
            || !shape_is_current
            || self.artifact_bundle_root != artifact_bundle_root(&self.artifact_bundle)
            || self.visibility_manifest_roots
                != self
                    .visibility_manifests
                    .iter()
                    .map(|manifest| (manifest.level.clone(), visibility_manifest_root(manifest)))
                    .collect()
            || (!self.principal_declaration.is_empty()
                && self.manifest_entry(&self.principal_declaration).is_none())
            || self.local_module_root != expected_local_root
            || self.logic_root != expected_logic_root
            || self.build_root != expected_build_root
            || self
                .dependency_certificate_roots
                .values()
                .any(|root| *root == Sha256Digest::ZERO)
            || !witnesses_are_current
        {
            return false;
        }
        let rebuilt = certificate_root(
            self.local_module_root,
            self.declaration_manifest_root,
            &self.visibility_manifest_roots,
            &self.dependency_certificate_roots,
            &self.observed_declaration_uses,
            self.axiom_policy_root,
            self.toolchain_root,
        );
        rebuilt == self.certified_node_root
    }
}

pub struct CertificateInputs<'a> {
    pub node: &'a NodeId,
    pub module_name: &'a str,
    pub principal_declaration: &'a str,
    /// Artifact-local closure read.  Equality with replay evidence detects a
    /// race or plumbing error; it is not an independent completeness oracle.
    pub closure_manifest: &'a [DeclarationManifestEntry],
    /// Authoritative replay-attested `ModuleData.constants` commitment.
    pub replay_manifest: &'a [DeclarationManifestEntry],
    pub visibility_manifests: &'a [VisibilityManifest],
    pub artifact_bundle: &'a [ArtifactBundlePart],
    pub source_sha256: Sha256Digest,
    pub semantic_root: Sha256Digest,
    pub dependency_certificate_roots: &'a BTreeMap<NodeId, Sha256Digest>,
    pub axiom_policy_root: Sha256Digest,
    pub toolchain_root: Sha256Digest,
    pub observed_uses: &'a [ObservedDeclarationUse],
    pub owner_certificates: &'a BTreeMap<NodeId, NodeCertificate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertificateIssuanceError {
    ManifestGap(ManifestGap),
    MissingPrincipal(String),
    InvalidPrincipalRegistration(NodeId),
    DuplicateManifestEntry(String),
    DuplicateVisibilityLevel(String),
    ArtifactBundleShape(String),
    MissingDependencyCertificate(NodeId),
    StaleDependencyCertificate(NodeId),
    MissingReachedDeclaration {
        owner: NodeId,
        declaration: String,
    },
    ReachedDeclarationKindMismatch {
        owner: NodeId,
        declaration: String,
        observed: String,
        certified: String,
    },
    InvalidDigest(&'static str),
}

impl std::fmt::Display for CertificateIssuanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestGap(gap) => write!(f, "ManifestGap: {gap:?}"),
            Self::MissingPrincipal(name) => {
                write!(f, "registered principal `{name}` is not an exact manifest member")
            }
            Self::InvalidPrincipalRegistration(node) => write!(
                f,
                "certificate owner `{node}` has an invalid principal registration mode"
            ),
            Self::DuplicateManifestEntry(name) => {
                write!(f, "certificate manifest repeats exact declaration `{name}`")
            }
            Self::DuplicateVisibilityLevel(level) => {
                write!(f, "certificate repeats artifact visibility level `{level}`")
            }
            Self::ArtifactBundleShape(detail) => {
                write!(f, "unsupported artifact bundle: {detail}")
            }
            Self::MissingDependencyCertificate(node) => {
                write!(f, "direct dependency `{node}` has no certificate")
            }
            Self::StaleDependencyCertificate(node) => {
                write!(f, "direct dependency `{node}` has a stale certificate root")
            }
            Self::MissingReachedDeclaration { owner, declaration } => write!(
                f,
                "reached declaration `{declaration}` is absent from owner `{owner}`'s manifest"
            ),
            Self::ReachedDeclarationKindMismatch {
                owner,
                declaration,
                observed,
                certified,
            } => write!(
                f,
                "reached declaration `{declaration}` in owner `{owner}` has observed kind `{observed}` but certificate kind `{certified}`"
            ),
            Self::InvalidDigest(axis) => write!(f, "certificate input `{axis}` is zero"),
        }
    }
}

impl std::error::Error for CertificateIssuanceError {}

pub fn local_module_root(
    module_name: &str,
    source_sha256: Sha256Digest,
    artifact_bundle_root: Sha256Digest,
) -> Sha256Digest {
    hash_parts(
        "local-module-root",
        [
            module_name.as_bytes().to_vec(),
            digest_part(source_sha256),
            digest_part(artifact_bundle_root),
        ],
    )
}

pub fn certificate_root(
    local_module_root: Sha256Digest,
    declaration_manifest_root: Sha256Digest,
    visibility_manifest_roots: &BTreeMap<String, Sha256Digest>,
    dependency_certificate_roots: &BTreeMap<NodeId, Sha256Digest>,
    observed_uses: &[ObservedDeclarationUse],
    axiom_policy_root: Sha256Digest,
    toolchain_root: Sha256Digest,
) -> Sha256Digest {
    let visibility_root = hash_parts(
        "visibility-manifest-roots",
        visibility_manifest_roots.iter().map(|(level, root)| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(level.len() as u64).to_be_bytes());
            bytes.extend_from_slice(level.as_bytes());
            bytes.extend_from_slice(root.as_bytes());
            bytes
        }),
    );
    let dependency_root = hash_parts(
        "dependency-certificate-roots",
        dependency_certificate_roots.iter().map(|(node, root)| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&(node.as_str().len() as u64).to_be_bytes());
            bytes.extend_from_slice(node.as_str().as_bytes());
            bytes.extend_from_slice(root.as_bytes());
            bytes
        }),
    );
    hash_parts(
        "certified-node-root",
        [
            digest_part(local_module_root),
            digest_part(declaration_manifest_root),
            digest_part(visibility_root),
            digest_part(dependency_root),
            digest_part(observed_uses_root(observed_uses)),
            digest_part(axiom_policy_root),
            digest_part(toolchain_root),
        ],
    )
}

pub fn issue_certificate(
    inputs: CertificateInputs<'_>,
) -> Result<NodeCertificate, CertificateIssuanceError> {
    for (axis, digest) in [
        ("source_sha256", inputs.source_sha256),
        ("semantic_root", inputs.semantic_root),
        ("axiom_policy_root", inputs.axiom_policy_root),
        ("toolchain_root", inputs.toolchain_root),
    ] {
        if digest == Sha256Digest::ZERO {
            return Err(CertificateIssuanceError::InvalidDigest(axis));
        }
    }
    let gap = manifest_gap(inputs.closure_manifest, inputs.replay_manifest);
    if !gap.is_empty() {
        return Err(CertificateIssuanceError::ManifestGap(gap));
    }
    let unique: BTreeSet<_> = inputs.replay_manifest.iter().cloned().collect();
    let mut seen_names = BTreeSet::new();
    if let Some(duplicate) = inputs
        .replay_manifest
        .iter()
        .find(|entry| !seen_names.insert(entry.name.clone()))
    {
        return Err(CertificateIssuanceError::DuplicateManifestEntry(
            duplicate.name.clone(),
        ));
    }
    if inputs.artifact_bundle.is_empty()
        || inputs.artifact_bundle.len() != inputs.visibility_manifests.len()
    {
        return Err(CertificateIssuanceError::ArtifactBundleShape(format!(
            "{} artifact parts but {} visibility manifests",
            inputs.artifact_bundle.len(),
            inputs.visibility_manifests.len()
        )));
    }
    let expected_levels = ["exported", "server", "private"];
    let mut seen_levels = BTreeSet::new();
    for (index, (part, manifest)) in inputs
        .artifact_bundle
        .iter()
        .zip(inputs.visibility_manifests)
        .enumerate()
    {
        if index >= expected_levels.len()
            || part.level != expected_levels[index]
            || manifest.level != part.level
            || part.sha256 == Sha256Digest::ZERO
            || part.size_bytes == 0
        {
            return Err(CertificateIssuanceError::ArtifactBundleShape(format!(
                "invalid ordered part/manifest at index {index}: part={part:?} manifest_level={}",
                manifest.level
            )));
        }
        if !seen_levels.insert(part.level.clone()) {
            return Err(CertificateIssuanceError::DuplicateVisibilityLevel(
                part.level.clone(),
            ));
        }
        let mut names = BTreeSet::new();
        if let Some(duplicate) = manifest
            .declarations
            .iter()
            .find(|entry| !names.insert(entry.name.clone()))
        {
            return Err(CertificateIssuanceError::DuplicateManifestEntry(
                duplicate.name.clone(),
            ));
        }
    }
    if inputs.visibility_manifests.windows(2).any(|levels| {
        levels[0].declarations.iter().any(|earlier| {
            !levels[1]
                .declarations
                .iter()
                .any(|later| later.name == earlier.name)
        })
    }) {
        return Err(CertificateIssuanceError::ArtifactBundleShape(
            "visibility manifests are not cumulative by declaration name".to_string(),
        ));
    }
    let Some(ownership_level) = inputs.visibility_manifests.last() else {
        return Err(CertificateIssuanceError::ArtifactBundleShape(
            "missing ownership level".to_string(),
        ));
    };
    if declaration_manifest_root(&ownership_level.declarations)
        != declaration_manifest_root(inputs.replay_manifest)
    {
        return Err(CertificateIssuanceError::ArtifactBundleShape(
            "last visibility manifest is not the ownership manifest".to_string(),
        ));
    }
    let module_owner_principal_is_valid = if inputs.node.as_str() == "Preamble" {
        inputs.principal_declaration.is_empty()
    } else {
        !inputs.principal_declaration.is_empty()
    };
    if !module_owner_principal_is_valid {
        return Err(CertificateIssuanceError::InvalidPrincipalRegistration(
            inputs.node.clone(),
        ));
    }
    if inputs.module_name != format!("Tablet.{}", inputs.node) {
        return Err(CertificateIssuanceError::ArtifactBundleShape(format!(
            "module {} does not match owner {}",
            inputs.module_name, inputs.node
        )));
    }
    if !inputs.principal_declaration.is_empty()
        && !inputs
            .replay_manifest
            .iter()
            .any(|entry| entry.name == inputs.principal_declaration)
    {
        return Err(CertificateIssuanceError::MissingPrincipal(
            inputs.principal_declaration.to_string(),
        ));
    }
    for (dependency, root) in inputs.dependency_certificate_roots {
        let Some(certificate) = inputs.owner_certificates.get(dependency) else {
            return Err(CertificateIssuanceError::MissingDependencyCertificate(
                dependency.clone(),
            ));
        };
        if !certificate.roots_are_current() || certificate.certified_node_root != *root {
            return Err(CertificateIssuanceError::StaleDependencyCertificate(
                dependency.clone(),
            ));
        }
    }

    let manifest_root = declaration_manifest_root(inputs.replay_manifest);
    let bundle_root = artifact_bundle_root(inputs.artifact_bundle);
    let visibility_roots: BTreeMap<_, _> = inputs
        .visibility_manifests
        .iter()
        .map(|manifest| (manifest.level.clone(), visibility_manifest_root(manifest)))
        .collect();
    let local_root = local_module_root(inputs.module_name, inputs.source_sha256, bundle_root);
    let logic_root = hash_parts(
        "logic-root",
        std::iter::once(digest_part(local_root)).chain(
            inputs
                .dependency_certificate_roots
                .values()
                .copied()
                .map(digest_part),
        ),
    );
    let build_root = hash_parts(
        "build-root",
        [
            digest_part(local_root),
            digest_part(logic_root),
            digest_part(inputs.toolchain_root),
            digest_part(inputs.axiom_policy_root),
        ],
    );
    let root = certificate_root(
        local_root,
        manifest_root,
        &visibility_roots,
        inputs.dependency_certificate_roots,
        inputs.observed_uses,
        inputs.axiom_policy_root,
        inputs.toolchain_root,
    );

    let mut certificate = NodeCertificate {
        version: NodeCertificate::VERSION.to_string(),
        node: inputs.node.clone(),
        module_name: inputs.module_name.to_string(),
        principal_declaration: inputs.principal_declaration.to_string(),
        declaration_manifest: unique.into_iter().collect(),
        declaration_manifest_root: manifest_root,
        visibility_manifests: inputs.visibility_manifests.to_vec(),
        visibility_manifest_roots: visibility_roots,
        source_sha256: inputs.source_sha256,
        artifact_bundle: inputs.artifact_bundle.to_vec(),
        artifact_bundle_root: bundle_root,
        local_module_root: local_root,
        semantic_root: inputs.semantic_root,
        logic_root,
        build_root,
        dependency_certificate_roots: inputs.dependency_certificate_roots.clone(),
        axiom_policy_root: inputs.axiom_policy_root,
        toolchain_root: inputs.toolchain_root,
        certified_node_root: root,
        exact_use_witnesses: Vec::new(),
        observed_declaration_uses: inputs.observed_uses.to_vec(),
    };

    for observed in inputs.observed_uses {
        let Some(owner) = inputs.owner_certificates.get(&observed.owner) else {
            return Err(CertificateIssuanceError::MissingDependencyCertificate(
                observed.owner.clone(),
            ));
        };
        if inputs
            .dependency_certificate_roots
            .get(&observed.owner)
            .is_none_or(|root| *root != owner.certified_node_root)
        {
            return Err(CertificateIssuanceError::MissingDependencyCertificate(
                observed.owner.clone(),
            ));
        }
        let Some(entry) =
            owner.visible_manifest_entry(&observed.visibility, &observed.reached_declaration)
        else {
            return Err(CertificateIssuanceError::MissingReachedDeclaration {
                owner: observed.owner.clone(),
                declaration: observed.reached_declaration.clone(),
            });
        };
        if entry.kind != observed.declaration_kind {
            return Err(CertificateIssuanceError::ReachedDeclarationKindMismatch {
                owner: observed.owner.clone(),
                declaration: observed.reached_declaration.clone(),
                observed: observed.declaration_kind.clone(),
                certified: entry.kind.clone(),
            });
        }
        let semantics = if observed.reached_declaration == owner.principal_declaration {
            CertificateEdgeSemantics::PrincipalAssumption
        } else if matches!(
            entry.kind.as_str(),
            "definition" | "inductive" | "constructor" | "recursor"
        ) {
            CertificateEdgeSemantics::SemanticDefinitionUse
        } else {
            CertificateEdgeSemantics::CertifiedModuleUse
        };
        certificate.exact_use_witnesses.push(ExactUseWitness {
            consumer: inputs.node.clone(),
            owner: observed.owner.clone(),
            reached_declaration: observed.reached_declaration.clone(),
            declaration_kind: observed.declaration_kind.clone(),
            visibility: observed.visibility.clone(),
            owner_root: owner.certified_node_root,
            membership_witness: owner
                .visible_membership_witness(&observed.visibility, entry)
                .ok_or_else(|| CertificateIssuanceError::MissingReachedDeclaration {
                    owner: observed.owner.clone(),
                    declaration: observed.reached_declaration.clone(),
                })?,
            semantics,
        });
    }
    certificate.exact_use_witnesses.sort_by(|a, b| {
        (&a.owner, &a.reached_declaration, &a.visibility).cmp(&(
            &b.owner,
            &b.reached_declaration,
            &b.visibility,
        ))
    });
    Ok(certificate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::raw_sha256;

    fn digest(label: &str) -> Sha256Digest {
        raw_sha256(label.as_bytes())
    }

    fn visibility(manifest: &[DeclarationManifestEntry]) -> Vec<VisibilityManifest> {
        vec![VisibilityManifest {
            level: "exported".to_string(),
            declarations: manifest.to_vec(),
        }]
    }

    fn bundle(label: &str) -> Vec<ArtifactBundlePart> {
        vec![ArtifactBundlePart {
            level: "exported".to_string(),
            sha256: digest(label),
            size_bytes: label.len() as u64 + 1,
        }]
    }

    fn leaf(node: &str, principal: &str, proof: &str) -> NodeCertificate {
        let node = NodeId::from(node);
        let manifest = vec![DeclarationManifestEntry {
            name: principal.to_string(),
            kind: "theorem".to_string(),
        }];
        issue_certificate(CertificateInputs {
            node: &node,
            module_name: &format!("Tablet.{node}"),
            principal_declaration: principal,
            closure_manifest: &manifest,
            replay_manifest: &manifest,
            visibility_manifests: &visibility(&manifest),
            artifact_bundle: &bundle(&format!("olean-{proof}")),
            source_sha256: digest(proof),
            semantic_root: digest("P"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn proof_only_change_moves_logic_build_and_certificate_but_not_semantic() {
        let before = leaf("Y", "Y.fact", "proof-one");
        let after = leaf("Y", "Y.fact", "proof-two");
        assert_eq!(before.semantic_root, after.semantic_root);
        assert_ne!(before.logic_root, after.logic_root);
        assert_ne!(before.build_root, after.build_root);
        assert_ne!(before.certified_node_root, after.certified_node_root);
    }

    #[test]
    fn v1_certificate_can_never_satisfy_a_v2_boundary() {
        let mut certificate = leaf("Y", "Y.fact", "proof");
        certificate.version = "node-certificate-v1".into();
        assert!(!certificate.roots_are_current());
    }

    #[test]
    fn every_ordered_artifact_part_moves_the_v2_certificate_root() {
        let node = NodeId::from("Split");
        let ownership = vec![DeclarationManifestEntry {
            name: "Split".into(),
            kind: "theorem".into(),
        }];
        let split_visibility = vec![
            VisibilityManifest {
                level: "exported".into(),
                declarations: vec![DeclarationManifestEntry {
                    name: "Split".into(),
                    kind: "axiom".into(),
                }],
            },
            VisibilityManifest {
                level: "server".into(),
                declarations: vec![DeclarationManifestEntry {
                    name: "Split".into(),
                    kind: "axiom".into(),
                }],
            },
            VisibilityManifest {
                level: "private".into(),
                declarations: ownership.clone(),
            },
        ];
        let original_bundle = vec![
            ArtifactBundlePart {
                level: "exported".into(),
                sha256: digest("public"),
                size_bytes: 10,
            },
            ArtifactBundlePart {
                level: "server".into(),
                sha256: digest("server"),
                size_bytes: 20,
            },
            ArtifactBundlePart {
                level: "private".into(),
                sha256: digest("private"),
                size_bytes: 30,
            },
        ];
        let issue = |artifact_bundle: &[ArtifactBundlePart]| {
            issue_certificate(CertificateInputs {
                node: &node,
                module_name: "Tablet.Split",
                principal_declaration: "Split",
                closure_manifest: &ownership,
                replay_manifest: &ownership,
                visibility_manifests: &split_visibility,
                artifact_bundle,
                source_sha256: digest("same-source"),
                semantic_root: digest("same-semantic"),
                dependency_certificate_roots: &BTreeMap::new(),
                axiom_policy_root: digest("axioms"),
                toolchain_root: digest("toolchain"),
                observed_uses: &[],
                owner_certificates: &BTreeMap::new(),
            })
            .unwrap()
        };
        let baseline = issue(&original_bundle);
        for index in 0..original_bundle.len() {
            let mut changed = original_bundle.clone();
            changed[index].sha256 = digest(&format!("changed-part-{index}"));
            let certificate = issue(&changed);
            assert_ne!(
                baseline.artifact_bundle_root,
                certificate.artifact_bundle_root
            );
            assert_ne!(baseline.local_module_root, certificate.local_module_root);
            assert_ne!(
                baseline.certified_node_root,
                certificate.certified_node_root
            );
        }
        let mut stale = baseline.clone();
        stale.artifact_bundle[1].sha256 = digest("uncommitted-server-change");
        assert!(!stale.roots_are_current());
    }

    #[test]
    fn issuance_rejects_non_cumulative_visibility_or_wrong_module_identity() {
        let node = NodeId::from("Owner");
        let public = DeclarationManifestEntry {
            name: "Owner.public".into(),
            kind: "axiom".into(),
        };
        let ownership = vec![DeclarationManifestEntry {
            name: "Owner".into(),
            kind: "theorem".into(),
        }];
        let visibility_manifests = vec![
            VisibilityManifest {
                level: "exported".into(),
                declarations: vec![public],
            },
            VisibilityManifest {
                level: "server".into(),
                declarations: ownership.clone(),
            },
        ];
        let artifact_bundle = vec![
            ArtifactBundlePart {
                level: "exported".into(),
                sha256: digest("public"),
                size_bytes: 1,
            },
            ArtifactBundlePart {
                level: "server".into(),
                sha256: digest("server"),
                size_bytes: 1,
            },
        ];
        let issue = |module_name: &str, visibility_manifests: &[VisibilityManifest]| {
            issue_certificate(CertificateInputs {
                node: &node,
                module_name,
                principal_declaration: "Owner",
                closure_manifest: &ownership,
                replay_manifest: &ownership,
                visibility_manifests,
                artifact_bundle: &artifact_bundle,
                source_sha256: digest("source"),
                semantic_root: digest("semantic"),
                dependency_certificate_roots: &BTreeMap::new(),
                axiom_policy_root: digest("axioms"),
                toolchain_root: digest("toolchain"),
                observed_uses: &[],
                owner_certificates: &BTreeMap::new(),
            })
        };
        assert!(matches!(
            issue("Tablet.Owner", &visibility_manifests),
            Err(CertificateIssuanceError::ArtifactBundleShape(_))
        ));

        let cumulative = vec![
            VisibilityManifest {
                level: "exported".into(),
                declarations: ownership.clone(),
            },
            VisibilityManifest {
                level: "server".into(),
                declarations: ownership.clone(),
            },
        ];
        assert!(matches!(
            issue("Tablet.NotOwner", &cumulative),
            Err(CertificateIssuanceError::ArtifactBundleShape(_))
        ));
    }

    #[test]
    fn split_module_boundary_witnesses_exported_axiom_but_owns_private_theorem() {
        let owner_node = NodeId::from("SplitVisibility");
        let public_axiom = DeclarationManifestEntry {
            name: "SplitVisibilityPublic".into(),
            kind: "axiom".into(),
        };
        let ownership = vec![
            DeclarationManifestEntry {
                name: "SplitVisibilityPublic".into(),
                kind: "theorem".into(),
            },
            DeclarationManifestEntry {
                name: "_private.Tablet.SplitVisibility.0.SplitVisibility".into(),
                kind: "theorem".into(),
            },
        ];
        let split_visibility = vec![
            VisibilityManifest {
                level: "exported".into(),
                declarations: vec![public_axiom.clone()],
            },
            VisibilityManifest {
                level: "server".into(),
                declarations: vec![public_axiom.clone()],
            },
            VisibilityManifest {
                level: "private".into(),
                declarations: ownership.clone(),
            },
        ];
        let split_bundle = vec![
            ArtifactBundlePart {
                level: "exported".into(),
                sha256: digest("split-exported"),
                size_bytes: 1,
            },
            ArtifactBundlePart {
                level: "server".into(),
                sha256: digest("split-server"),
                size_bytes: 2,
            },
            ArtifactBundlePart {
                level: "private".into(),
                sha256: digest("split-private"),
                size_bytes: 3,
            },
        ];
        let owner = issue_certificate(CertificateInputs {
            node: &owner_node,
            module_name: "Tablet.SplitVisibility",
            principal_declaration: "_private.Tablet.SplitVisibility.0.SplitVisibility",
            closure_manifest: &ownership,
            replay_manifest: &ownership,
            visibility_manifests: &split_visibility,
            artifact_bundle: &split_bundle,
            source_sha256: digest("split-source"),
            semantic_root: digest("split-semantic"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(
            owner
                .visible_manifest_entry("exported", "SplitVisibilityPublic")
                .unwrap()
                .kind,
            "axiom"
        );
        assert_eq!(
            owner.manifest_entry("SplitVisibilityPublic").unwrap().kind,
            "theorem"
        );

        let consumer_node = NodeId::from("Consumer");
        let consumer_manifest = vec![DeclarationManifestEntry {
            name: "Consumer".into(),
            kind: "theorem".into(),
        }];
        let roots = BTreeMap::from([(owner_node.clone(), owner.certified_node_root)]);
        let owners = BTreeMap::from([(owner_node.clone(), owner)]);
        let consumer = issue_certificate(CertificateInputs {
            node: &consumer_node,
            module_name: "Tablet.Consumer",
            principal_declaration: "Consumer",
            closure_manifest: &consumer_manifest,
            replay_manifest: &consumer_manifest,
            visibility_manifests: &visibility(&consumer_manifest),
            artifact_bundle: &bundle("consumer"),
            source_sha256: digest("consumer-source"),
            semantic_root: digest("consumer-semantic"),
            dependency_certificate_roots: &roots,
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[ObservedDeclarationUse {
                owner: owner_node,
                reached_declaration: "SplitVisibilityPublic".into(),
                declaration_kind: "axiom".into(),
                visibility: "exported".into(),
            }],
            owner_certificates: &owners,
        })
        .unwrap();
        assert!(consumer.roots_are_current());
        assert_eq!(consumer.exact_use_witnesses.len(), 1);
        assert_eq!(consumer.exact_use_witnesses[0].declaration_kind, "axiom");
        let mut erased_point_of_use = consumer.clone();
        erased_point_of_use.observed_declaration_uses.clear();
        erased_point_of_use.exact_use_witnesses.clear();
        assert!(!erased_point_of_use.roots_are_current());
    }

    #[test]
    fn module_owner_certificate_needs_no_fabricated_principal() {
        let node = NodeId::from("Preamble");
        let manifest = vec![DeclarationManifestEntry {
            name: "crate_ns.PreambleStructure".into(),
            kind: "inductive".into(),
        }];
        let certificate = issue_certificate(CertificateInputs {
            node: &node,
            module_name: "Tablet.Preamble",
            principal_declaration: "",
            closure_manifest: &manifest,
            replay_manifest: &manifest,
            visibility_manifests: &visibility(&manifest),
            artifact_bundle: &bundle("preamble-olean"),
            source_sha256: digest("preamble-source"),
            semantic_root: digest("preamble-semantic"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .expect("a manifest-complete module owner does not need a fake principal");
        assert!(certificate.principal_declaration.is_empty());
        assert!(certificate.roots_are_current());
        assert!(certificate
            .manifest_entry("crate_ns.PreambleStructure")
            .is_some());
    }

    #[test]
    fn empty_module_certificate_commits_established_empty_manifest() {
        let node = NodeId::from("Preamble");
        let empty: Vec<DeclarationManifestEntry> = Vec::new();
        let certificate = issue_certificate(CertificateInputs {
            node: &node,
            module_name: "Tablet.Preamble",
            principal_declaration: "",
            closure_manifest: &empty,
            replay_manifest: &empty,
            visibility_manifests: &visibility(&empty),
            artifact_bundle: &bundle("imports-only-preamble-olean"),
            source_sha256: digest("imports-only-preamble-source"),
            semantic_root: digest("imports-only-preamble-semantic"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .expect("three established empty declaration sets are equal and certifiable");

        assert!(certificate.declaration_manifest.is_empty());
        assert_eq!(
            certificate.declaration_manifest_root,
            declaration_manifest_root(&empty),
            "the certificate must commit the empty set, not omit the manifest root"
        );
        assert!(certificate.roots_are_current());
    }

    #[test]
    fn principal_registration_mode_is_bound_to_owner_identity() {
        let ordinary = NodeId::from("Ordinary");
        let preamble = NodeId::from("Preamble");
        let ordinary_manifest = vec![DeclarationManifestEntry {
            name: "Ordinary".into(),
            kind: "theorem".into(),
        }];
        let preamble_manifest = vec![DeclarationManifestEntry {
            name: "crate_ns.PreambleStructure".into(),
            kind: "inductive".into(),
        }];
        for (node, principal, manifest) in [
            (&ordinary, "", ordinary_manifest.as_slice()),
            (
                &preamble,
                "crate_ns.PreambleStructure",
                preamble_manifest.as_slice(),
            ),
        ] {
            let error = issue_certificate(CertificateInputs {
                node,
                module_name: &format!("Tablet.{node}"),
                principal_declaration: principal,
                closure_manifest: manifest,
                replay_manifest: manifest,
                visibility_manifests: &visibility(manifest),
                artifact_bundle: &bundle("olean"),
                source_sha256: digest("source"),
                semantic_root: digest("semantic"),
                dependency_certificate_roots: &BTreeMap::new(),
                axiom_policy_root: digest("axioms"),
                toolchain_root: digest("toolchain"),
                observed_uses: &[],
                owner_certificates: &BTreeMap::new(),
            })
            .expect_err("principal registration mode must match the owner identity");
            assert!(matches!(
                error,
                CertificateIssuanceError::InvalidPrincipalRegistration(_)
            ));
        }
    }

    #[test]
    fn recursive_root_moves_when_only_dependency_proof_changes() {
        let y1 = leaf("Y", "Y.fact", "proof-one");
        let y2 = leaf("Y", "Y.fact", "proof-two");
        let x_node = NodeId::from("X");
        let x_manifest = vec![DeclarationManifestEntry {
            name: "X.hidden".into(),
            kind: "theorem".into(),
        }];
        let build_x = |owner: NodeCertificate| {
            let roots = BTreeMap::from([(NodeId::from("Y"), owner.certified_node_root)]);
            let owners = BTreeMap::from([(NodeId::from("Y"), owner)]);
            issue_certificate(CertificateInputs {
                node: &x_node,
                module_name: "Tablet.X",
                principal_declaration: "X.hidden",
                closure_manifest: &x_manifest,
                replay_manifest: &x_manifest,
                visibility_manifests: &visibility(&x_manifest),
                artifact_bundle: &bundle("same-x-olean"),
                source_sha256: digest("same-x-source"),
                semantic_root: digest("P"),
                dependency_certificate_roots: &roots,
                axiom_policy_root: digest("axioms"),
                toolchain_root: digest("toolchain"),
                observed_uses: &[ObservedDeclarationUse {
                    owner: NodeId::from("Y"),
                    reached_declaration: "Y.fact".into(),
                    declaration_kind: "theorem".into(),
                    visibility: "exported".into(),
                }],
                owner_certificates: &owners,
            })
            .unwrap()
        };
        let x1 = build_x(y1);
        let x2 = build_x(y2);
        assert_eq!(x1.local_module_root, x2.local_module_root);
        assert_ne!(x1.certified_node_root, x2.certified_node_root);
    }

    #[test]
    fn manifest_set_inequality_is_typed_and_fails_issuance() {
        let node = NodeId::from("X");
        let metadata = vec![
            DeclarationManifestEntry {
                name: "X".into(),
                kind: "theorem".into(),
            },
            DeclarationManifestEntry {
                name: "X.h".into(),
                kind: "theorem".into(),
            },
        ];
        let replay = vec![metadata[0].clone()];
        let error = issue_certificate(CertificateInputs {
            node: &node,
            module_name: "Tablet.X",
            principal_declaration: "X",
            closure_manifest: &metadata,
            replay_manifest: &replay,
            visibility_manifests: &visibility(&metadata),
            artifact_bundle: &bundle("olean"),
            source_sha256: digest("source"),
            semantic_root: digest("semantic"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .unwrap_err();
        let CertificateIssuanceError::ManifestGap(gap) = error else {
            panic!("unexpected error")
        };
        assert_eq!(gap.closure_only, vec![metadata[1].clone()]);
    }

    #[test]
    fn exact_use_must_be_manifest_member_not_merely_same_owner() {
        let owner = leaf("X", "X", "proof");
        let consumer_node = NodeId::from("B");
        let manifest = vec![DeclarationManifestEntry {
            name: "B".into(),
            kind: "theorem".into(),
        }];
        let roots = BTreeMap::from([(NodeId::from("X"), owner.certified_node_root)]);
        let owners = BTreeMap::from([(NodeId::from("X"), owner)]);
        let error = issue_certificate(CertificateInputs {
            node: &consumer_node,
            module_name: "Tablet.B",
            principal_declaration: "B",
            closure_manifest: &manifest,
            replay_manifest: &manifest,
            visibility_manifests: &visibility(&manifest),
            artifact_bundle: &bundle("olean"),
            source_sha256: digest("source"),
            semantic_root: digest("semantic"),
            dependency_certificate_roots: &roots,
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[ObservedDeclarationUse {
                owner: NodeId::from("X"),
                reached_declaration: "X.h".into(),
                declaration_kind: "theorem".into(),
                visibility: "exported".into(),
            }],
            owner_certificates: &owners,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            CertificateIssuanceError::MissingReachedDeclaration { .. }
        ));
    }

    fn owner_with_generated_member(member: &str) -> NodeCertificate {
        let node = NodeId::from("WeirdDeriveOwner");
        let manifest = vec![
            DeclarationManifestEntry {
                name: "WeirdDeriveOwner".into(),
                kind: "inductive".into(),
            },
            DeclarationManifestEntry {
                name: member.into(),
                kind: "theorem".into(),
            },
        ];
        issue_certificate(CertificateInputs {
            node: &node,
            module_name: "Tablet.WeirdDeriveOwner",
            principal_declaration: "WeirdDeriveOwner",
            closure_manifest: &manifest,
            replay_manifest: &manifest,
            visibility_manifests: &visibility(&manifest),
            artifact_bundle: &bundle(&format!("olean-{member}")),
            source_sha256: digest(&format!("source-{member}")),
            semantic_root: digest("owner-semantic"),
            dependency_certificate_roots: &BTreeMap::new(),
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[],
            owner_certificates: &BTreeMap::new(),
        })
        .unwrap()
    }

    fn issue_generated_member_consumer(
        member: &str,
        owner: Option<NodeCertificate>,
    ) -> Result<NodeCertificate, CertificateIssuanceError> {
        let node = NodeId::from("Consumer");
        let manifest = vec![DeclarationManifestEntry {
            name: "Consumer".into(),
            kind: "theorem".into(),
        }];
        let mut roots = BTreeMap::new();
        let mut owners = BTreeMap::new();
        if let Some(owner) = owner {
            roots.insert(NodeId::from("WeirdDeriveOwner"), owner.certified_node_root);
            owners.insert(NodeId::from("WeirdDeriveOwner"), owner);
        }
        issue_certificate(CertificateInputs {
            node: &node,
            module_name: "Tablet.Consumer",
            principal_declaration: "Consumer",
            closure_manifest: &manifest,
            replay_manifest: &manifest,
            visibility_manifests: &visibility(&manifest),
            artifact_bundle: &bundle("consumer-olean"),
            source_sha256: digest("consumer-source"),
            semantic_root: digest("consumer-semantic"),
            dependency_certificate_roots: &roots,
            axiom_policy_root: digest("axioms"),
            toolchain_root: digest("toolchain"),
            observed_uses: &[ObservedDeclarationUse {
                owner: NodeId::from("WeirdDeriveOwner"),
                reached_declaration: member.into(),
                declaration_kind: "theorem".into(),
                visibility: "exported".into(),
            }],
            owner_certificates: &owners,
        })
    }

    #[test]
    fn lookalike_stopping_requires_and_accepts_genuine_exact_member_certificate() {
        let member = "CobaltMoth922";
        let absent = issue_generated_member_consumer(member, None).unwrap_err();
        assert!(matches!(
            absent,
            CertificateIssuanceError::MissingDependencyCertificate(ref owner)
                if owner == &NodeId::from("WeirdDeriveOwner")
        ));

        let accepted =
            issue_generated_member_consumer(member, Some(owner_with_generated_member(member)))
                .expect("genuine exact manifest membership must authorize the stopping point");
        assert_eq!(accepted.exact_use_witnesses.len(), 1);
        assert_eq!(
            accepted.exact_use_witnesses[0].semantics,
            CertificateEdgeSemantics::CertifiedModuleUse
        );
    }

    #[test]
    fn nonprincipal_generated_member_rename_does_not_change_treatment() {
        let issue = |member: &str| {
            issue_generated_member_consumer(member, Some(owner_with_generated_member(member)))
                .expect("renamed exact member remains certifiable")
        };
        let before = issue("CobaltMoth922");
        let after = issue("RenamedThirdPartyRoot");
        for certificate in [&before, &after] {
            assert_eq!(certificate.exact_use_witnesses.len(), 1);
            let witness = &certificate.exact_use_witnesses[0];
            assert_eq!(witness.owner, NodeId::from("WeirdDeriveOwner"));
            assert_eq!(
                witness.semantics,
                CertificateEdgeSemantics::CertifiedModuleUse
            );
        }
        assert_ne!(
            before.exact_use_witnesses[0].reached_declaration,
            after.exact_use_witnesses[0].reached_declaration
        );
    }
}
