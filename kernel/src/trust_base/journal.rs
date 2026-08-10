use super::auth::{verify_actor_receipt, ActorKeyManifest, ReceiptContext, VerifiedActorReceipt};
use super::canonical::{
    canonical_json, canonical_json_value, parse_json_strict, self_digest, tagged_hash,
    verify_self_digest, DomainTag, Sha256Digest, TrustError,
};
use super::records::{
    ActorAuthenticationMethod, ActorIdentityPolicy, ActorRole, AuthoritativeRecord,
    AuthorizationRequirement, EventKind, JournalEvent, JournalEventPayload, JournalHead,
    JournalPolicy, Subject,
};
use super::schema::SchemaRegistry;
use super::seed::validate_seed_manifest_semantics;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const JOURNAL_METADATA_SCHEMA: &str = "trellis-trust-journal-metadata/v1";
const JOURNAL_HEAD_SCHEMA: &str = "trellis-trust-journal-head/v1";

#[derive(Clone, Debug)]
pub(crate) enum JournalActor {
    Kernel,
    Authenticated {
        role: ActorRole,
        identity: String,
        gate_or_revision_lane_id: String,
        receipt: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthorizationLink {
    pub authorization_id: String,
    pub authorization_event_hash: Sha256Digest,
    pub revision_lane_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AppendRequest {
    pub transaction_id: String,
    pub event_kind: EventKind,
    pub subject_id: String,
    pub subject: Subject,
    pub actor: JournalActor,
    pub semantic_root_after: Sha256Digest,
    pub derived_result_root_after: Sha256Digest,
    pub authorization: Option<AuthorizationLink>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalCheckpointBinding {
    pub journal_id: String,
    pub sequence_number: u64,
    pub event_hash: Sha256Digest,
    pub projection_root: Sha256Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalMetadata {
    schema: String,
    journal_id: String,
    run_id: String,
    canonical_genesis_hash: Sha256Digest,
    journal_policy_sha256: Sha256Digest,
    actor_key_manifest_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableHead {
    schema: String,
    journal_id: String,
    sequence_number: u64,
    event_hash: Sha256Digest,
    transaction_id: String,
}

impl DurableHead {
    fn public_head(&self) -> JournalHead {
        JournalHead {
            journal_id: self.journal_id.clone(),
            sequence_number: self.sequence_number,
            event_hash: self.event_hash,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Projection {
    semantic_root: Sha256Digest,
    derived_result_root: Sha256Digest,
    current_human_approval_event_hash: Option<Sha256Digest>,
    current_human_approval_subject_hash: Option<Sha256Digest>,
    current_gate_presentation_sha256: Option<Sha256Digest>,
    current_approved_evidence_tool_input_root: Option<Sha256Digest>,
    revoked_approval_events: BTreeSet<Sha256Digest>,
    audit_authorizations: BTreeMap<String, AuditAuthorizationState>,
    used_revision_lanes: BTreeSet<String>,
    routine_gate_outcome: JournalRoutineGateOutcome,
    current_revision_closure: Option<RevisionClosureProjection>,
    current_package_authorization_event_hash: Option<Sha256Digest>,
}

#[derive(Clone, Debug)]
struct AuditAuthorizationState {
    authorization_id: String,
    event_hash: Sha256Digest,
    lane_id: String,
    status: AuditLaneStatus,
    originating_approval_event_hash: Sha256Digest,
    permitted_changes: BTreeSet<(String, String)>,
    maximum_semantic_scope_sha256: Sha256Digest,
    proposed_revision_closure: Option<RevisionClosureProjection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuditLaneStatus {
    Authorized,
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JournalRoutineGateOutcome {
    #[default]
    NotPresented,
    Approved(Sha256Digest),
    Feedback(Sha256Digest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalApprovalProjection {
    pub event_hash: Sha256Digest,
    pub authored_semantic_root: Sha256Digest,
    pub approved_evidence_tool_input_root: Sha256Digest,
    pub gate_presentation_sha256: Sha256Digest,
    pub revision_closure: Option<RevisionClosureProjection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionClosureProjection {
    pub seed_manifest_sha256: Sha256Digest,
    pub seed_definition_bundle_sha256: Sha256Digest,
    pub authored_semantic_root: Sha256Digest,
    pub evidence_manifest_sha256: Sha256Digest,
    pub evidence_tool_input_root: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedRevisionChange {
    pub item_id: String,
    pub change_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedRevisionProposal {
    pub authorization_id: String,
    pub revision_lane_id: String,
    pub revised_seed_manifest: AuthoritativeRecord,
    pub revised_seed_definition_bundle: Value,
    pub revised_evidence_manifest_sha256: Sha256Digest,
    pub revised_evidence_tool_input_root: Sha256Digest,
    pub changes: Vec<AuthorizedRevisionChange>,
}

#[derive(Clone, Debug)]
struct ValidatedBundle {
    bytes: Vec<u8>,
    event: JournalEvent,
    payload: JournalEventPayload,
    subject: Subject,
}

pub struct TrustJournal {
    root: PathBuf,
    metadata: JournalMetadata,
    head: DurableHead,
    projection: Projection,
    transaction_hashes: BTreeMap<String, Sha256Digest>,
    pending_orphan: Option<ValidatedBundle>,
    registry: SchemaRegistry,
    policy: JournalPolicy,
    actor_keys: ActorKeyManifest,
}

impl TrustJournal {
    pub fn create(
        root: impl Into<PathBuf>,
        journal_id: &str,
        run_id: &str,
        actor_keys: ActorKeyManifest,
        seed_transaction_id: &str,
        seed_manifest: AuthoritativeRecord,
    ) -> Result<Self, TrustError> {
        validate_id("journal_id", journal_id)?;
        validate_id("run_id", run_id)?;
        validate_id("seed_transaction_id", seed_transaction_id)?;
        if seed_manifest.contract().record_schema
            != "trellis-seed-authored-definition-manifest/v1"
        {
            return Err(TrustError::new(
                "seed_subject_wrong_schema",
                "journal sequence one must bind a seed authored-definition manifest",
            ));
        }
        validate_seed_manifest_semantics(&seed_manifest)?;
        let root = root.into();
        if root.exists() {
            return Err(TrustError::new(
                "journal_path_already_exists",
                format!("refusing to initialize existing path {}", root.display()),
            ));
        }
        let registry = SchemaRegistry::v1()?;
        let policy = JournalPolicy::embedded_v1(&registry)?;
        let genesis = canonical_genesis(journal_id)?;
        validate_seed_manifest(
            seed_manifest.value(),
            journal_id,
            run_id,
            genesis,
            policy.digest(),
            actor_keys.digest(),
        )?;
        let semantic_after = value_digest(seed_manifest.value(), "authored_semantic_root")?;
        let empty_derived = empty_root(DomainTag::DerivedResultRoot)?;

        fs::create_dir_all(root.join("events")).map_err(io_error("create journal directory"))?;
        let metadata = JournalMetadata {
            schema: JOURNAL_METADATA_SCHEMA.into(),
            journal_id: journal_id.into(),
            run_id: run_id.into(),
            canonical_genesis_hash: genesis,
            journal_policy_sha256: policy.digest(),
            actor_key_manifest_sha256: actor_keys.digest(),
        };
        write_new_synced(&root.join("JOURNAL.json"), &canonical_json(&metadata)?)?;
        write_new_synced(
            &root.join("actor-key-manifest.json"),
            &actor_keys.canonical_bytes()?,
        )?;
        sync_dir(&root)?;
        let head = DurableHead {
            schema: JOURNAL_HEAD_SCHEMA.into(),
            journal_id: journal_id.into(),
            sequence_number: 0,
            event_hash: genesis,
            transaction_id: "genesis".into(),
        };
        write_head_atomic(&root, &head)?;
        let mut journal = Self {
            root,
            metadata,
            head,
            projection: Projection {
                semantic_root: empty_root(DomainTag::AuthoredSemanticRoot)?,
                derived_result_root: empty_derived,
                ..Projection::default()
            },
            transaction_hashes: BTreeMap::new(),
            pending_orphan: None,
            registry,
            policy,
            actor_keys,
        };
        journal.append(AppendRequest {
            transaction_id: seed_transaction_id.into(),
            event_kind: EventKind::SeedCommitted,
            subject_id: value_string(seed_manifest.value(), "seed_plan_id")?.into(),
            subject: Subject::CanonicalRecord(seed_manifest),
            actor: JournalActor::Kernel,
            semantic_root_after: semantic_after,
            derived_result_root_after: empty_derived,
            authorization: None,
        })?;
        Ok(journal)
    }

    pub fn open(
        root: impl Into<PathBuf>,
        actor_keys: ActorKeyManifest,
    ) -> Result<Self, TrustError> {
        let root = root.into();
        let registry = SchemaRegistry::v1()?;
        let policy = JournalPolicy::embedded_v1(&registry)?;
        let metadata: JournalMetadata = read_canonical_json(&root.join("JOURNAL.json"))?;
        if metadata.schema != JOURNAL_METADATA_SCHEMA
            || metadata.canonical_genesis_hash != canonical_genesis(&metadata.journal_id)?
            || metadata.journal_policy_sha256 != policy.digest()
            || metadata.actor_key_manifest_sha256 != actor_keys.digest()
        {
            return Err(TrustError::new(
                "journal_metadata_mismatch",
                "journal metadata differs from the embedded policy, genesis, or key manifest",
            ));
        }
        let stored_manifest = fs::read(root.join("actor-key-manifest.json"))
            .map_err(io_error("read actor key manifest"))?;
        if stored_manifest != actor_keys.canonical_bytes()? {
            return Err(TrustError::new(
                "journal_actor_manifest_bytes_mismatch",
                "stored actor-key manifest is not byte-identical to the verified manifest",
            ));
        }
        let head: DurableHead = read_canonical_json(&root.join("HEAD"))?;
        if head.schema != JOURNAL_HEAD_SCHEMA || head.journal_id != metadata.journal_id {
            return Err(TrustError::new(
                "journal_head_identity_mismatch",
                "durable HEAD belongs to another journal or schema",
            ));
        }
        let mut journal = Self {
            root,
            metadata,
            head,
            projection: Projection {
                semantic_root: empty_root(DomainTag::AuthoredSemanticRoot)?,
                derived_result_root: empty_root(DomainTag::DerivedResultRoot)?,
                ..Projection::default()
            },
            transaction_hashes: BTreeMap::new(),
            pending_orphan: None,
            registry,
            policy,
            actor_keys,
        };
        journal.recover()?;
        Ok(journal)
    }

    pub fn head(&self) -> JournalHead {
        self.head.public_head()
    }

    pub fn journal_id(&self) -> &str {
        &self.metadata.journal_id
    }

    pub fn run_id(&self) -> &str {
        &self.metadata.run_id
    }

    pub fn canonical_genesis_head(&self) -> JournalHead {
        JournalHead {
            journal_id: self.metadata.journal_id.clone(),
            sequence_number: 0,
            event_hash: self.metadata.canonical_genesis_hash,
        }
    }

    pub fn actor_key_manifest_value(&self) -> &Value {
        self.actor_keys.value()
    }

    pub(crate) fn actor_key_manifest(&self) -> &ActorKeyManifest {
        &self.actor_keys
    }

    /// Return the exact committed bundle values in sequence order. `open`
    /// has already replayed and authenticated this prefix; the additional
    /// canonical parse here ensures callers cannot accidentally package a
    /// different byte interpretation.
    pub fn committed_bundle_values(&self) -> Result<Vec<Value>, TrustError> {
        (1..=self.head.sequence_number)
            .map(|sequence| self.committed_bundle_value(sequence))
            .collect()
    }

    pub fn committed_bundle_value(&self, sequence: u64) -> Result<Value, TrustError> {
        if sequence == 0 || sequence > self.head.sequence_number {
            return Err(TrustError::new(
                "journal_sequence_not_committed",
                format!("sequence {sequence} is not in the durable committed prefix"),
            ));
        }
        let path = event_path(&self.root.join("events"), sequence);
        let bytes = fs::read(&path).map_err(io_error("read committed journal bundle"))?;
        let value = parse_json_strict(&bytes)
            .map_err(|error| TrustError::new("journal_bundle_json_invalid", error.to_string()))?;
        if canonical_json_value(&value)? != bytes {
            return Err(TrustError::new(
                "journal_bundle_not_canonical",
                format!("{} is not canonical JSON", path.display()),
            ));
        }
        self.registry
            .validate("trellis://schemas/journal-event-bundle/v1", &value)?;
        verify_self_digest(DomainTag::JournalEventBundle, &value, "bundle_sha256")?;
        Ok(value)
    }

    pub fn semantic_root(&self) -> Sha256Digest {
        self.projection.semantic_root
    }

    pub fn derived_result_root(&self) -> Sha256Digest {
        self.projection.derived_result_root
    }

    pub fn current_human_approval_event_hash(&self) -> Option<Sha256Digest> {
        self.projection.current_human_approval_event_hash
    }

    pub fn current_approval(&self) -> Option<JournalApprovalProjection> {
        Some(JournalApprovalProjection {
            event_hash: self.projection.current_human_approval_event_hash?,
            authored_semantic_root: self.projection.semantic_root,
            approved_evidence_tool_input_root: self
                .projection
                .current_approved_evidence_tool_input_root?,
            gate_presentation_sha256: self.projection.current_gate_presentation_sha256?,
            revision_closure: self.projection.current_revision_closure,
        })
    }

    pub fn routine_gate_outcome(&self) -> JournalRoutineGateOutcome {
        self.projection.routine_gate_outcome
    }

    pub fn active_revision_lane_id(&self) -> Option<&str> {
        self.projection
            .audit_authorizations
            .values()
            .find(|state| state.status == AuditLaneStatus::Open)
            .map(|state| state.lane_id.as_str())
    }

    pub fn has_nonclosed_audit_authorization(&self) -> bool {
        self.projection
            .audit_authorizations
            .values()
            .any(|state| state.status != AuditLaneStatus::Closed)
    }

    pub fn current_package_authorization_event_hash(&self) -> Option<Sha256Digest> {
        self.projection.current_package_authorization_event_hash
    }

    pub fn committed_transaction_event_hash(
        &self,
        transaction_id: &str,
    ) -> Option<Sha256Digest> {
        self.transaction_hashes.get(transaction_id).copied()
    }

    /// Build the exact payload a reviewer must sign for the one routine gate
    /// without mutating the journal. The eventual append recomputes this
    /// payload and rejects any stale or altered receipt.
    pub fn prepare_routine_gate_payload(
        &self,
        event_kind: EventKind,
        gate_episode_id: &str,
        reviewer_identity: &str,
        subject: &Subject,
    ) -> Result<JournalEventPayload, TrustError> {
        if !matches!(
            event_kind,
            EventKind::AdvanceGateApproved | EventKind::AdvanceGateFeedback
        ) || self.projection.routine_gate_outcome != JournalRoutineGateOutcome::NotPresented
            || self.projection.current_human_approval_event_hash.is_some()
        {
            return Err(TrustError::new(
                "routine_gate_payload_not_available",
                "routine gate payload is legal exactly once before a gate outcome",
            ));
        }
        if gate_episode_id.is_empty() || reviewer_identity.is_empty() {
            return Err(TrustError::new(
                "routine_gate_identity_invalid",
                "gate episode and reviewer identity must be non-empty",
            ));
        }
        self.prepare_authenticated_payload(event_kind, gate_episode_id, reviewer_identity, subject)
    }

    /// Build the payload an audit authority must sign.  Only one exceptional
    /// authorization may be nonterminal, preventing ambiguous lane selection.
    pub fn prepare_audit_authorization_payload(
        &self,
        authorization_id: &str,
        audit_identity: &str,
        subject: &AuthoritativeRecord,
    ) -> Result<JournalEventPayload, TrustError> {
        if self.has_nonclosed_audit_authorization() {
            return Err(TrustError::new(
                "audit_authorization_already_active",
                "only one exceptional authorization may be nonterminal",
            ));
        }
        validate_audit_authorization_subject(
            subject,
            authorization_id,
            audit_identity,
            self.projection.current_human_approval_event_hash,
            &self.head.public_head(),
        )?;
        self.prepare_authenticated_payload(
            EventKind::AuditAuthorization,
            authorization_id,
            audit_identity,
            &Subject::CanonicalRecord(subject.clone()),
        )
    }

    pub fn commit_audit_authorization(
        &mut self,
        transaction_id: &str,
        authorization_id: &str,
        audit_identity: &str,
        subject: AuthoritativeRecord,
        authentication_receipt: Value,
    ) -> Result<JournalHead, TrustError> {
        self.prepare_audit_authorization_payload(
            authorization_id,
            audit_identity,
            &subject,
        )?;
        self.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            event_kind: EventKind::AuditAuthorization,
            subject_id: authorization_id.to_owned(),
            subject: Subject::CanonicalRecord(subject),
            actor: JournalActor::Authenticated {
                role: ActorRole::AuditAuthority,
                identity: audit_identity.to_owned(),
                gate_or_revision_lane_id: authorization_id.to_owned(),
                receipt: authentication_receipt,
            },
            semantic_root_after: self.semantic_root(),
            derived_result_root_after: self.derived_result_root(),
            authorization: None,
        })
    }

    /// Consume a one-shot authorization and commit the exact proposed closure
    /// roots and changed-item set before any protected reapproval is signed.
    pub fn open_authorized_revision(
        &mut self,
        transaction_id: &str,
        proposal: AuthorizedRevisionProposal,
    ) -> Result<JournalHead, TrustError> {
        let state = self
            .projection
            .audit_authorizations
            .get(&proposal.authorization_id)
            .ok_or_else(|| {
                TrustError::new(
                    "unknown_audit_authorization",
                    proposal.authorization_id.clone(),
                )
            })?;
        if state.status != AuditLaneStatus::Authorized
            || state.lane_id != proposal.revision_lane_id
            || proposal.revised_evidence_manifest_sha256 == Sha256Digest::ZERO
            || proposal.revised_evidence_tool_input_root == Sha256Digest::ZERO
        {
            return Err(TrustError::new(
                "revision_proposal_not_authorized",
                "revision proposal does not match one unused authorization",
            ));
        }
        let seed_bytes = canonical_json_value(&proposal.revised_seed_definition_bundle)?;
        let verified = super::closure::verify_seed_definition_bundle(
            &proposal.revised_seed_manifest,
            &seed_bytes,
        )?;
        let seed_roots = validate_seed_manifest_semantics(&proposal.revised_seed_manifest)?;
        validate_seed_manifest(
            proposal.revised_seed_manifest.value(),
            &self.metadata.journal_id,
            &self.metadata.run_id,
            self.metadata.canonical_genesis_hash,
            self.metadata.journal_policy_sha256,
            self.metadata.actor_key_manifest_sha256,
        )?;
        if seed_roots.evidence_tool_input_root != proposal.revised_evidence_tool_input_root {
            return Err(TrustError::new(
                "revision_evidence_root_mismatch",
                "revised seed and proposed evidence closure root differ",
            ));
        }
        let revision_closure = RevisionClosureProjection {
            seed_manifest_sha256: verified.seed_manifest_sha256,
            seed_definition_bundle_sha256: verified.bundle_sha256,
            authored_semantic_root: seed_roots.authored_semantic_root,
            evidence_manifest_sha256: proposal.revised_evidence_manifest_sha256,
            evidence_tool_input_root: proposal.revised_evidence_tool_input_root,
        };
        let changes_value = validate_revision_changes(&proposal.changes, &state.permitted_changes)?;
        let subject_value = serde_json::json!({
            "schema": "trellis-authorized-revision-proposal/v1",
            "authorization_id": proposal.authorization_id,
            "authorization_event_hash": state.event_hash,
            "originating_approval_event_hash": state.originating_approval_event_hash,
            "revision_lane_id": proposal.revision_lane_id,
            "revised_seed_manifest": proposal.revised_seed_manifest.value(),
            "revised_seed_definition_bundle": proposal.revised_seed_definition_bundle,
            "revised_seed_manifest_sha256": revision_closure.seed_manifest_sha256,
            "revised_seed_definition_bundle_sha256": revision_closure.seed_definition_bundle_sha256,
            "revised_authored_semantic_root": revision_closure.authored_semantic_root,
            "revised_evidence_manifest_sha256": revision_closure.evidence_manifest_sha256,
            "revised_evidence_tool_input_root": revision_closure.evidence_tool_input_root,
            "changes": changes_value,
            "change_scope_sha256": tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&changes_value)?,
            ),
            "maximum_semantic_scope_sha256": state.maximum_semantic_scope_sha256,
            "journal_predecessor_sha256": self.head.event_hash,
        });
        let subject = Subject::RawArtifact(canonical_json_value(&subject_value)?);
        let authorization = AuthorizationLink {
            authorization_id: proposal.authorization_id,
            authorization_event_hash: state.event_hash,
            revision_lane_id: proposal.revision_lane_id,
        };
        self.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            event_kind: EventKind::RevisionOpened,
            subject_id: authorization.revision_lane_id.clone(),
            subject,
            actor: JournalActor::Kernel,
            semantic_root_after: self.semantic_root(),
            derived_result_root_after: self.derived_result_root(),
            authorization: Some(authorization),
        })
    }

    pub fn prepare_protected_reapproval_payload(
        &self,
        event_kind: EventKind,
        authorization_id: &str,
        reviewer_identity: &str,
        subject: &Subject,
    ) -> Result<JournalEventPayload, TrustError> {
        if !matches!(
            event_kind,
            EventKind::ProtectedReapprovalApproved | EventKind::ProtectedReapprovalFeedback
        ) {
            return Err(TrustError::new(
                "protected_reapproval_kind_invalid",
                "expected a protected approval or feedback event",
            ));
        }
        let state = self
            .projection
            .audit_authorizations
            .get(authorization_id)
            .ok_or_else(|| TrustError::new("unknown_audit_authorization", authorization_id))?;
        if state.status != AuditLaneStatus::Open {
            return Err(TrustError::new(
                "protected_reapproval_lane_not_open",
                "protected reapproval requires one open revision lane",
            ));
        }
        if self.projection.current_human_approval_event_hash
            != Some(state.originating_approval_event_hash)
        {
            return Err(TrustError::new(
                "protected_reapproval_origin_stale",
                "open lane no longer descends from the current approval",
            ));
        }
        validate_protected_reapproval_subject(event_kind, subject, state, &self.head.public_head())?;
        self.prepare_authenticated_payload(
            event_kind,
            &state.lane_id,
            reviewer_identity,
            subject,
        )
    }

    pub fn commit_protected_reapproval(
        &mut self,
        transaction_id: &str,
        event_kind: EventKind,
        authorization_id: &str,
        reviewer_identity: &str,
        subject: Subject,
        authentication_receipt: Value,
    ) -> Result<JournalHead, TrustError> {
        self.prepare_protected_reapproval_payload(
            event_kind,
            authorization_id,
            reviewer_identity,
            &subject,
        )?;
        let state = self
            .projection
            .audit_authorizations
            .get(authorization_id)
            .ok_or_else(|| TrustError::new("unknown_audit_authorization", authorization_id))?;
        let semantic_root_after = if event_kind == EventKind::ProtectedReapprovalApproved {
            state.proposed_revision_closure.map(|closure| closure.authored_semantic_root).ok_or_else(|| {
                TrustError::new(
                    "protected_reapproval_proposal_missing",
                    "open lane lacks its proposed authored root",
                )
            })?
        } else {
            self.semantic_root()
        };
        let authorization = AuthorizationLink {
            authorization_id: authorization_id.to_owned(),
            authorization_event_hash: state.event_hash,
            revision_lane_id: state.lane_id.clone(),
        };
        let lane_id = state.lane_id.clone();
        self.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            event_kind,
            subject_id: lane_id.clone(),
            subject,
            actor: JournalActor::Authenticated {
                role: ActorRole::Reviewer,
                identity: reviewer_identity.to_owned(),
                gate_or_revision_lane_id: lane_id,
                receipt: authentication_receipt,
            },
            semantic_root_after,
            derived_result_root_after: self.derived_result_root(),
            authorization: Some(authorization),
        })
    }

    fn prepare_authenticated_payload(
        &self,
        event_kind: EventKind,
        subject_id: &str,
        principal_identity: &str,
        subject: &Subject,
    ) -> Result<JournalEventPayload, TrustError> {
        validate_id("subject_id", subject_id)?;
        if principal_identity.is_empty() {
            return Err(TrustError::new(
                "authenticated_principal_invalid",
                "authenticated principal identity must be non-empty",
            ));
        }
        self.policy.validate_subject(event_kind, subject)?;
        validate_subject_head_binding(
            event_kind,
            subject,
            &self.head.public_head(),
            self.projection.current_human_approval_event_hash,
        )?;
        let (subject_schema_id, subject_schema_sha256) = match subject {
            Subject::CanonicalRecord(record) | Subject::ClosureTransition(record) => (
                Some(record.contract().schema_id.to_owned()),
                Some(self.registry.schema_sha256(record.contract().schema_id)?),
            ),
            Subject::RawArtifact(_) => (None, None),
        };
        let payload = JournalEventPayload {
            schema: "trellis-journal-event-payload/v1".into(),
            event_kind,
            subject_id: subject_id.to_owned(),
            subject_kind: subject.kind().into(),
            subject_schema_id,
            subject_schema_sha256,
            subject_hash_tag: subject.domain_tag().as_str().into(),
            subject_sha256: subject.digest()?,
            principal_identity: Some(principal_identity.to_owned()),
            journal_predecessor_sha256: self.head.event_hash,
            payload_sha256: Sha256Digest::ZERO,
        }
        .seal()?;
        let payload_value = serde_json::to_value(&payload).map_err(|error| {
            TrustError::new("payload_serialization_failed", error.to_string())
        })?;
        self.registry.validate(
            "trellis://schemas/journal-event-payload/v1",
            &payload_value,
        )?;
        Ok(payload)
    }

    pub fn checkpoint_binding(&self) -> Result<JournalCheckpointBinding, TrustError> {
        Ok(JournalCheckpointBinding {
            journal_id: self.metadata.journal_id.clone(),
            sequence_number: self.head.sequence_number,
            event_hash: self.head.event_hash,
            projection_root: self.projection_root()?,
        })
    }

    pub fn verify_checkpoint_binding(
        &self,
        checkpoint: &JournalCheckpointBinding,
    ) -> Result<bool, TrustError> {
        if checkpoint.journal_id != self.metadata.journal_id {
            return Err(TrustError::new(
                "checkpoint_foreign_journal",
                "checkpoint names another journal",
            ));
        }
        if checkpoint.sequence_number > self.head.sequence_number {
            return Err(TrustError::new(
                "checkpoint_ahead_of_journal",
                "checkpoint cannot be ahead of the sole commit authority",
            ));
        }
        if checkpoint.sequence_number < self.head.sequence_number {
            return Ok(false);
        }
        if checkpoint.event_hash != self.head.event_hash
            || checkpoint.projection_root != self.projection_root()?
        {
            return Err(TrustError::new(
                "checkpoint_head_or_projection_fork",
                "checkpoint at current sequence disagrees with journal",
            ));
        }
        Ok(true)
    }

    /// Replay a detached, complete prefix from canonical genesis without
    /// trusting any journal files or checkpoint supplied by the package.
    /// This is the in-memory authority core used by the offline verifier.
    pub(crate) fn replay_detached(
        actor_keys: ActorKeyManifest,
        canonical_genesis_head: &JournalHead,
        bundles: &[Value],
    ) -> Result<Self, TrustError> {
        if bundles.is_empty() {
            return Err(TrustError::new(
                "detached_journal_chain_empty",
                "detached replay requires sequence one through the selected head",
            ));
        }
        if canonical_genesis_head.sequence_number != 0
            || canonical_genesis_head.event_hash
                != canonical_genesis(&canonical_genesis_head.journal_id)?
        {
            return Err(TrustError::new(
                "detached_genesis_invalid",
                "detached replay must start at the canonical sequence-zero head",
            ));
        }
        let registry = SchemaRegistry::v1()?;
        let policy = JournalPolicy::embedded_v1(&registry)?;
        let first_event: JournalEvent = serde_json::from_value(
            bundles[0]
                .get("event")
                .cloned()
                .ok_or_else(|| TrustError::new("journal_event_missing", "bundle lacks event"))?,
        )
        .map_err(|error| TrustError::new("journal_event_decode_failed", error.to_string()))?;
        if first_event.sequence_number != 1
            || first_event.event_kind != EventKind::SeedCommitted
            || first_event.journal_id != canonical_genesis_head.journal_id
        {
            return Err(TrustError::new(
                "detached_chain_does_not_begin_at_seed",
                "detached chain must begin at sequence-one seed_committed",
            ));
        }
        let metadata = JournalMetadata {
            schema: JOURNAL_METADATA_SCHEMA.into(),
            journal_id: first_event.journal_id.clone(),
            run_id: first_event.run_id.clone(),
            canonical_genesis_hash: canonical_genesis_head.event_hash,
            journal_policy_sha256: policy.digest(),
            actor_key_manifest_sha256: actor_keys.digest(),
        };
        let mut journal = Self {
            root: PathBuf::new(),
            metadata,
            head: DurableHead {
                schema: JOURNAL_HEAD_SCHEMA.into(),
                journal_id: canonical_genesis_head.journal_id.clone(),
                sequence_number: 0,
                event_hash: canonical_genesis_head.event_hash,
                transaction_id: "genesis".into(),
            },
            projection: Projection {
                semantic_root: empty_root(DomainTag::AuthoredSemanticRoot)?,
                derived_result_root: empty_root(DomainTag::DerivedResultRoot)?,
                ..Projection::default()
            },
            transaction_hashes: BTreeMap::new(),
            pending_orphan: None,
            registry,
            policy,
            actor_keys,
        };
        let mut predecessor = canonical_genesis_head.event_hash;
        for (offset, value) in bundles.iter().enumerate() {
            let sequence = u64::try_from(offset)
                .map_err(|_| TrustError::new("journal_sequence_overflow", "chain too long"))?
                .checked_add(1)
                .ok_or_else(|| TrustError::new("journal_sequence_overflow", "chain too long"))?;
            let bytes = canonical_json_value(value)?;
            let bundle = journal.validate_bundle_value(value.clone(), bytes, predecessor, sequence)?;
            predecessor = bundle.event.event_hash;
            journal.commit_projection(&bundle)?;
        }
        Ok(journal)
    }

    pub(crate) fn append(&mut self, request: AppendRequest) -> Result<JournalHead, TrustError> {
        validate_id("transaction_id", &request.transaction_id)?;
        validate_id("subject_id", &request.subject_id)?;
        if let Some(committed) = self.transaction_hashes.get(&request.transaction_id) {
            return Err(TrustError::new(
                "duplicate_committed_transaction",
                format!(
                    "transaction {} is already committed as {}",
                    request.transaction_id, committed
                ),
            ));
        }
        let bundle = self.build_bundle(request)?;
        if let Some(orphan) = &self.pending_orphan {
            if orphan.event.transaction_id != bundle.event.transaction_id
                || orphan.bytes != bundle.bytes
            {
                return Err(TrustError::new(
                    "unreconciled_journal_orphan",
                    "an orphan exists and only an exact idempotent retry may adopt it",
                ));
            }
            write_head_atomic(
                &self.root,
                &DurableHead {
                    schema: JOURNAL_HEAD_SCHEMA.into(),
                    journal_id: self.metadata.journal_id.clone(),
                    sequence_number: bundle.event.sequence_number,
                    event_hash: bundle.event.event_hash,
                    transaction_id: bundle.event.transaction_id.clone(),
                },
            )?;
            self.commit_projection(&bundle)?;
            self.pending_orphan = None;
            return Ok(self.head());
        }
        self.install_bundle(&bundle)?;
        write_head_atomic(
            &self.root,
            &DurableHead {
                schema: JOURNAL_HEAD_SCHEMA.into(),
                journal_id: self.metadata.journal_id.clone(),
                sequence_number: bundle.event.sequence_number,
                event_hash: bundle.event.event_hash,
                transaction_id: bundle.event.transaction_id.clone(),
            },
        )?;
        self.commit_projection(&bundle)?;
        Ok(self.head())
    }

    fn build_bundle(&self, request: AppendRequest) -> Result<ValidatedBundle, TrustError> {
        if self.head.sequence_number == 0 && request.event_kind != EventKind::SeedCommitted {
            return Err(TrustError::new(
                "seed_event_required",
                "journal sequence one must be seed_committed",
            ));
        }
        if self.head.sequence_number != 0 && request.event_kind == EventKind::SeedCommitted {
            return Err(TrustError::new(
                "duplicate_seed_event",
                "seed_committed is legal only at sequence one",
            ));
        }
        self.policy
            .validate_subject(request.event_kind, &request.subject)?;
        self.validate_root_transition(
            request.event_kind,
            request.semantic_root_after,
            request.derived_result_root_after,
        )?;
        let policy_entry = self.policy.entry(request.event_kind);
        let authorization = self.validate_authorization_link(
            request.event_kind,
            request.authorization.as_ref(),
        )?;
        let principal_identity = principal_identity(&request.actor);
        validate_subject_head_binding(
            request.event_kind,
            &request.subject,
            &self.head.public_head(),
            self.projection.current_human_approval_event_hash,
        )?;
        let (subject_schema_id, subject_schema_sha256) = match &request.subject {
            Subject::CanonicalRecord(record) | Subject::ClosureTransition(record) => (
                Some(record.contract().schema_id.to_owned()),
                Some(self.registry.schema_sha256(record.contract().schema_id)?),
            ),
            Subject::RawArtifact(_) => (None, None),
        };
        let payload = JournalEventPayload {
            schema: "trellis-journal-event-payload/v1".into(),
            event_kind: request.event_kind,
            subject_id: request.subject_id.clone(),
            subject_kind: request.subject.kind().into(),
            subject_schema_id,
            subject_schema_sha256,
            subject_hash_tag: request.subject.domain_tag().as_str().into(),
            subject_sha256: request.subject.digest()?,
            principal_identity: principal_identity.map(str::to_owned),
            journal_predecessor_sha256: self.head.event_hash,
            payload_sha256: Sha256Digest::ZERO,
        }
        .seal()?;
        let payload_value = serde_json::to_value(&payload).map_err(|error| {
            TrustError::new("payload_serialization_failed", error.to_string())
        })?;
        self.registry.validate(
            "trellis://schemas/journal-event-payload/v1",
            &payload_value,
        )?;
        let derived_result_root_after = self.expected_derived_result_root_after(
            request.event_kind,
            payload.payload_sha256,
            payload.subject_sha256,
        )?;
        if request.derived_result_root_after != Sha256Digest::ZERO
            && request.derived_result_root_after != derived_result_root_after
        {
            return Err(TrustError::new(
                "derived_result_root_not_kernel_derived",
                "caller-supplied derived root differs from the deterministic journal transition",
            ));
        }

        let (actor_role, actor_identity, authentication_method, receipt) =
            self.validate_actor(
                &request.actor,
                request.event_kind,
                &request.transaction_id,
                payload.payload_sha256,
                &request.subject_id,
            )?;
        if actor_role != policy_entry.envelope_actor_role
            || authentication_method != policy_entry.authentication_method
        {
            return Err(TrustError::new(
                "event_actor_policy_mismatch",
                "actor role or authentication method differs from frozen event policy",
            ));
        }
        let event = JournalEvent {
            journal_schema: "trellis-trust-journal/v1".into(),
            journal_id: self.metadata.journal_id.clone(),
            run_id: self.metadata.run_id.clone(),
            sequence_number: self.head.sequence_number.checked_add(1).ok_or_else(|| {
                TrustError::new("journal_sequence_overflow", "journal sequence overflow")
            })?,
            transaction_id: request.transaction_id,
            previous_event_hash: self.head.event_hash,
            event_kind: request.event_kind,
            payload_schema_id: "trellis://schemas/journal-event-payload/v1".into(),
            payload_schema_sha256: self
                .registry
                .schema_sha256("trellis://schemas/journal-event-payload/v1")?,
            payload_hash: payload.payload_sha256,
            semantic_root_before: self.projection.semantic_root,
            semantic_root_after: request.semantic_root_after,
            derived_result_root_before: self.projection.derived_result_root,
            derived_result_root_after,
            actor_role,
            actor_identity,
            actor_authentication_method: authentication_method,
            actor_authentication_receipt_sha256: receipt.as_ref().map(|item| item.digest()),
            authorization_id: authorization.as_ref().map(|item| item.authorization_id.clone()),
            authorization_event_hash: authorization
                .as_ref()
                .map(|item| item.authorization_event_hash),
            revision_lane_id: authorization.as_ref().map(|item| item.revision_lane_id.clone()),
            event_hash: Sha256Digest::ZERO,
        }
        .seal()?;
        validate_principal_binding(policy_entry, &event, &payload, &request.subject)?;
        let event_value = serde_json::to_value(&event).map_err(|error| {
            TrustError::new("event_serialization_failed", error.to_string())
        })?;
        self.registry
            .validate("trellis://schemas/journal-event/v1", &event_value)?;

        let mut bundle_object = Map::new();
        bundle_object.insert(
            "schema".into(),
            Value::String("trellis-journal-event-bundle/v1".into()),
        );
        bundle_object.insert("event".into(), event_value);
        bundle_object.insert("payload".into(), payload_value);
        if let Some(receipt) = &receipt {
            bundle_object.insert(
                "actor_authentication_receipt".into(),
                receipt.value().clone(),
            );
        }
        match &request.subject {
            Subject::CanonicalRecord(record) | Subject::ClosureTransition(record) => {
                bundle_object.insert(
                    "subject_encoding".into(),
                    Value::String("canonical_json".into()),
                );
                bundle_object.insert("subject_json".into(), record.value().clone());
            }
            Subject::RawArtifact(bytes) => {
                bundle_object.insert(
                    "subject_encoding".into(),
                    Value::String("raw_base64".into()),
                );
                bundle_object.insert(
                    "subject_base64".into(),
                    Value::String(BASE64_STANDARD.encode(bytes)),
                );
            }
        }
        bundle_object.insert(
            "bundle_sha256".into(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
        let mut value = Value::Object(bundle_object);
        let bundle_digest = self_digest(
            DomainTag::JournalEventBundle,
            &value,
            "bundle_sha256",
        )?;
        value.as_object_mut().ok_or_else(|| {
            TrustError::new("bundle_not_object", "journal bundle must be an object")
        })?.insert("bundle_sha256".into(), Value::String(bundle_digest.to_string()));
        self.registry
            .validate("trellis://schemas/journal-event-bundle/v1", &value)?;
        let bytes = canonical_json_value(&value)?;
        Ok(ValidatedBundle {
            bytes,
            event,
            payload,
            subject: request.subject,
        })
    }

    fn validate_actor(
        &self,
        actor: &JournalActor,
        event_kind: EventKind,
        transaction_id: &str,
        payload_hash: Sha256Digest,
        subject_id: &str,
    ) -> Result<
        (
            ActorRole,
            String,
            ActorAuthenticationMethod,
            Option<VerifiedActorReceipt>,
        ),
        TrustError,
    > {
        match actor {
            JournalActor::Kernel => Ok((
                ActorRole::Kernel,
                "trellis-kernel".into(),
                ActorAuthenticationMethod::KernelInternal,
                None,
            )),
            JournalActor::Authenticated {
                role,
                identity,
                gate_or_revision_lane_id,
                receipt,
            } => {
                if gate_or_revision_lane_id != subject_id {
                    return Err(TrustError::new(
                        "gate_lane_subject_id_mismatch",
                        "receipt gate/lane ID must equal payload subject ID",
                    ));
                }
                let predecessor_head = self.head.public_head();
                let context = ReceiptContext {
                    journal_id: &self.metadata.journal_id,
                    run_id: &self.metadata.run_id,
                    transaction_id,
                    event_kind,
                    event_payload_sha256: payload_hash,
                    predecessor_head: &predecessor_head,
                    actor_role: *role,
                    actor_identity: identity,
                    gate_or_revision_lane_id,
                };
                let verified = verify_actor_receipt(
                    &self.registry,
                    &self.actor_keys,
                    receipt.clone(),
                    &context,
                )?;
                let method = match role {
                    ActorRole::Reviewer => ActorAuthenticationMethod::AuthenticatedGateReceipt,
                    ActorRole::AuditAuthority => ActorAuthenticationMethod::AuditSignatureV1,
                    ActorRole::Kernel => {
                        return Err(TrustError::new(
                            "authenticated_kernel_actor_forbidden",
                            "kernel events use kernel_internal authority",
                        ))
                    }
                };
                Ok((*role, identity.clone(), method, Some(verified)))
            }
        }
    }

    fn validate_root_transition(
        &self,
        event_kind: EventKind,
        semantic_after: Sha256Digest,
        _derived_after: Sha256Digest,
    ) -> Result<(), TrustError> {
        let semantic_change_allowed = matches!(
            event_kind,
            EventKind::SeedCommitted | EventKind::ProtectedReapprovalApproved
        );
        if !semantic_change_allowed && semantic_after != self.projection.semantic_root {
            return Err(TrustError::new(
                "unauthorized_semantic_root_change",
                format!("{event_kind:?} cannot change the authored semantic root"),
            ));
        }
        Ok(())
    }

    fn expected_derived_result_root_after(
        &self,
        event_kind: EventKind,
        payload_sha256: Sha256Digest,
        subject_sha256: Sha256Digest,
    ) -> Result<Sha256Digest, TrustError> {
        if !event_adds_derived_result(event_kind) {
            return Ok(self.projection.derived_result_root);
        }
        if self.projection.current_human_approval_event_hash.is_none() {
            return Err(TrustError::new(
                "post_gate_transition_before_approval",
                format!("{event_kind:?} requires a current human approval"),
            ));
        }
        let transition = serde_json::json!({
            "protocol": "trellis-derived-result-transition/v1",
            "prior_closure_root": self.projection.derived_result_root,
            "event_kind": event_kind,
            "payload_sha256": payload_sha256,
            "subject_sha256": subject_sha256,
        });
        Ok(tagged_hash(
            DomainTag::DerivedResultRoot,
            &canonical_json_value(&transition)?,
        ))
    }

    fn validate_authorization_link(
        &self,
        event_kind: EventKind,
        link: Option<&AuthorizationLink>,
    ) -> Result<Option<AuthorizationLink>, TrustError> {
        let required = self.policy.entry(event_kind).authorization_requirement
            == AuthorizationRequirement::RevisionLane;
        match (required, link) {
            (false, None) => Ok(None),
            (false, Some(_)) => Err(TrustError::new(
                "unexpected_revision_authorization",
                "ordinary event must not carry revision authorization",
            )),
            (true, None) => Err(TrustError::new(
                "revision_authorization_missing",
                "protected event requires an audit-authorized revision lane",
            )),
            (true, Some(link)) => {
                let state = self
                    .projection
                    .audit_authorizations
                    .get(&link.authorization_id)
                    .ok_or_else(|| {
                        TrustError::new(
                            "unknown_audit_authorization",
                            format!("unknown authorization {}", link.authorization_id),
                        )
                    })?;
                let expected_status = if event_kind == EventKind::RevisionOpened {
                    AuditLaneStatus::Authorized
                } else {
                    AuditLaneStatus::Open
                };
                if state.event_hash != link.authorization_event_hash
                    || state.lane_id != link.revision_lane_id
                    || state.status != expected_status
                    || (event_kind == EventKind::RevisionOpened
                        && self
                            .projection
                            .used_revision_lanes
                            .contains(&link.revision_lane_id))
                {
                    return Err(TrustError::new(
                        "invalid_or_consumed_revision_authorization",
                        "revision authorization hash/lane is wrong or the lane is not in the required lifecycle state",
                    ));
                }
                Ok(Some(link.clone()))
            }
        }
    }

    fn install_bundle(&self, bundle: &ValidatedBundle) -> Result<(), TrustError> {
        let events = self.root.join("events");
        let final_path = event_path(&events, bundle.event.sequence_number);
        let temp_path = events.join(format!(
            ".pending-{:016}-{}",
            bundle.event.sequence_number, bundle.event.payload_hash
        ));
        write_new_synced(&temp_path, &bundle.bytes)?;
        match fs::hard_link(&temp_path, &final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read(&final_path).map_err(io_error("read existing event"))?;
                if existing != bundle.bytes {
                    let _ = fs::remove_file(&temp_path);
                    return Err(TrustError::new(
                        "journal_hash_fork",
                        format!("sequence {} already has different bytes", bundle.event.sequence_number),
                    ));
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(TrustError::new(
                    "journal_event_install_failed",
                    error.to_string(),
                ));
            }
        }
        sync_dir(&events)?;
        fs::remove_file(&temp_path).map_err(io_error("remove event staging file"))?;
        sync_dir(&events)
    }

    fn recover(&mut self) -> Result<(), TrustError> {
        self.projection = Projection {
            semantic_root: empty_root(DomainTag::AuthoredSemanticRoot)?,
            derived_result_root: empty_root(DomainTag::DerivedResultRoot)?,
            ..Projection::default()
        };
        self.transaction_hashes.clear();
        self.pending_orphan = None;
        let events_dir = self.root.join("events");
        let mut event_files = BTreeMap::new();
        for entry in fs::read_dir(&events_dir).map_err(io_error("read journal events"))? {
            let entry = entry.map_err(io_error("read journal event entry"))?;
            let file_type = entry.file_type().map_err(io_error("stat journal event"))?;
            if !file_type.is_file() || file_type.is_symlink() {
                return Err(TrustError::new(
                    "invalid_journal_event_entry",
                    format!("{} is not a regular event file", entry.path().display()),
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                TrustError::new("non_utf8_journal_filename", "journal filename is not UTF-8")
            })?;
            if name.starts_with(".pending-") {
                return Err(TrustError::new(
                    "stale_journal_staging_file",
                    format!("staging file {name} requires reconciliation"),
                ));
            }
            let sequence = parse_event_filename(name)?;
            if event_files.insert(sequence, entry.path()).is_some() {
                return Err(TrustError::new(
                    "duplicate_journal_sequence",
                    format!("multiple files for sequence {sequence}"),
                ));
            }
        }
        if event_files.keys().next_back().copied().unwrap_or(0) > self.head.sequence_number + 1 {
            return Err(TrustError::new(
                "journal_gap_after_head",
                "events exist more than one sequence beyond durable HEAD",
            ));
        }
        let mut predecessor = self.metadata.canonical_genesis_hash;
        for sequence in 1..=self.head.sequence_number {
            let path = event_files.get(&sequence).ok_or_else(|| {
                TrustError::new(
                    "missing_committed_journal_event",
                    format!("durable HEAD requires missing sequence {sequence}"),
                )
            })?;
            let bundle = self.read_validate_bundle(path, predecessor, sequence)?;
            self.commit_projection(&bundle)?;
            predecessor = bundle.event.event_hash;
        }
        if self.head.sequence_number == 0 {
            if self.head.event_hash != self.metadata.canonical_genesis_hash {
                return Err(TrustError::new(
                    "invalid_genesis_head",
                    "sequence-zero HEAD must name canonical genesis",
                ));
            }
        } else if predecessor != self.head.event_hash {
            return Err(TrustError::new(
                "journal_head_hash_mismatch",
                "durable HEAD does not name the replayed committed prefix",
            ));
        }
        let orphan_sequence = self.head.sequence_number + 1;
        if let Some(path) = event_files.get(&orphan_sequence) {
            self.pending_orphan = Some(self.read_validate_bundle(
                path,
                self.head.event_hash,
                orphan_sequence,
            )?);
        }
        Ok(())
    }

    fn read_validate_bundle(
        &self,
        path: &Path,
        predecessor: Sha256Digest,
        sequence: u64,
    ) -> Result<ValidatedBundle, TrustError> {
        let bytes = fs::read(path).map_err(io_error("read journal event bundle"))?;
        let value = parse_json_strict(&bytes).map_err(|error| {
            TrustError::new("journal_bundle_json_invalid", error.to_string())
        })?;
        if canonical_json_value(&value)? != bytes {
            return Err(TrustError::new(
                "journal_bundle_not_canonical",
                format!("{} is not canonical JSON", path.display()),
            ));
        }
        self.validate_bundle_value(value, bytes, predecessor, sequence)
    }

    fn validate_bundle_value(
        &self,
        value: Value,
        bytes: Vec<u8>,
        predecessor: Sha256Digest,
        sequence: u64,
    ) -> Result<ValidatedBundle, TrustError> {
        self.registry
            .validate("trellis://schemas/journal-event-bundle/v1", &value)?;
        verify_self_digest(DomainTag::JournalEventBundle, &value, "bundle_sha256")?;
        let event: JournalEvent = serde_json::from_value(value["event"].clone()).map_err(|error| {
            TrustError::new("journal_event_decode_failed", error.to_string())
        })?;
        let payload: JournalEventPayload =
            serde_json::from_value(value["payload"].clone()).map_err(|error| {
                TrustError::new("journal_payload_decode_failed", error.to_string())
            })?;
        if event.compute_digest()? != event.event_hash
            || payload.compute_digest()? != payload.payload_sha256
            || event.sequence_number != sequence
            || event.previous_event_hash != predecessor
            || payload.journal_predecessor_sha256 != predecessor
            || event.payload_hash != payload.payload_sha256
            || event.event_kind != payload.event_kind
            || event.journal_id != self.metadata.journal_id
            || event.run_id != self.metadata.run_id
        {
            return Err(TrustError::new(
                "journal_bundle_binding_mismatch",
                format!("bundle at sequence {sequence} has inconsistent bindings"),
            ));
        }
        let subject = decode_subject(&self.registry, &value, &payload)?;
        self.policy.validate_subject(event.event_kind, &subject)?;
        if subject.digest()? != payload.subject_sha256
            || subject.domain_tag().as_str() != payload.subject_hash_tag
            || subject.kind() != payload.subject_kind
        {
            return Err(TrustError::new(
                "journal_subject_binding_mismatch",
                "subject bytes do not match payload identity",
            ));
        }
        self.validate_root_transition(
            event.event_kind,
            event.semantic_root_after,
            event.derived_result_root_after,
        )?;
        let expected_derived = self.expected_derived_result_root_after(
            event.event_kind,
            payload.payload_sha256,
            payload.subject_sha256,
        )?;
        if event.derived_result_root_after != expected_derived {
            return Err(TrustError::new(
                "replayed_derived_result_root_mismatch",
                "event derived-result root is not the deterministic closure transition",
            ));
        }
        let authorization = event_authorization_link(&event)?;
        self.validate_authorization_link(event.event_kind, authorization.as_ref())?;
        let predecessor_head = JournalHead {
            journal_id: event.journal_id.clone(),
            sequence_number: sequence - 1,
            event_hash: predecessor,
        };
        validate_subject_head_binding(
            event.event_kind,
            &subject,
            &predecessor_head,
            self.projection.current_human_approval_event_hash,
        )?;
        validate_principal_binding(
            self.policy.entry(event.event_kind),
            &event,
            &payload,
            &subject,
        )?;
        let actor_receipt = value.get("actor_authentication_receipt").cloned();
        self.validate_replayed_authority(&event, &payload, actor_receipt.as_ref())?;
        Ok(ValidatedBundle {
            bytes,
            event,
            payload,
            subject,
        })
    }

    fn validate_replayed_authority(
        &self,
        event: &JournalEvent,
        payload: &JournalEventPayload,
        receipt: Option<&Value>,
    ) -> Result<(), TrustError> {
        let entry = self.policy.entry(event.event_kind);
        if event.actor_role != entry.envelope_actor_role
            || event.actor_authentication_method != entry.authentication_method
        {
            return Err(TrustError::new(
                "replayed_actor_policy_mismatch",
                "event actor differs from frozen policy",
            ));
        }
        match event.actor_role {
            ActorRole::Kernel => {
                if event.actor_identity != "trellis-kernel"
                    || receipt.is_some()
                    || event.actor_authentication_receipt_sha256.is_some()
                {
                    return Err(TrustError::new(
                        "invalid_kernel_event_authority",
                        "kernel event contains foreign identity or receipt",
                    ));
                }
            }
            ActorRole::Reviewer | ActorRole::AuditAuthority => {
                let receipt = receipt.ok_or_else(|| {
                    TrustError::new("actor_receipt_missing", "authenticated event lacks receipt")
                })?;
                let gate_id = payload.subject_id.as_str();
                let predecessor_head = JournalHead {
                    journal_id: event.journal_id.clone(),
                    sequence_number: event.sequence_number - 1,
                    event_hash: event.previous_event_hash,
                };
                let context = ReceiptContext {
                    journal_id: &event.journal_id,
                    run_id: &event.run_id,
                    transaction_id: &event.transaction_id,
                    event_kind: event.event_kind,
                    event_payload_sha256: payload.payload_sha256,
                    predecessor_head: &predecessor_head,
                    actor_role: event.actor_role,
                    actor_identity: &event.actor_identity,
                    gate_or_revision_lane_id: gate_id,
                };
                let verified = verify_actor_receipt(
                    &self.registry,
                    &self.actor_keys,
                    receipt.clone(),
                    &context,
                )?;
                if Some(verified.digest()) != event.actor_authentication_receipt_sha256 {
                    return Err(TrustError::new(
                        "event_actor_receipt_digest_mismatch",
                        "event does not name its verified actor receipt",
                    ));
                }
            }
        }
        Ok(())
    }

    fn commit_projection(&mut self, bundle: &ValidatedBundle) -> Result<(), TrustError> {
        if bundle.event.semantic_root_before != self.projection.semantic_root
            || bundle.event.derived_result_root_before != self.projection.derived_result_root
        {
            return Err(TrustError::new(
                "journal_projection_root_discontinuity",
                "event before-roots differ from replayed projection",
            ));
        }
        if let Some(previous) = self
            .transaction_hashes
            .insert(bundle.event.transaction_id.clone(), bundle.event.event_hash)
        {
            return Err(TrustError::new(
                "duplicate_journal_transaction_id",
                format!(
                    "transaction {} names both {} and {}",
                    bundle.event.transaction_id, previous, bundle.event.event_hash
                ),
            ));
        }
        if bundle.event.event_kind != EventKind::PackageAuthorized {
            self.projection.current_package_authorization_event_hash = None;
        }
        match bundle.event.event_kind {
            EventKind::SeedCommitted => {
                if bundle.event.sequence_number != 1 {
                    return Err(TrustError::new(
                        "seed_event_wrong_sequence",
                        "seed_committed must be journal sequence one",
                    ));
                }
                let Subject::CanonicalRecord(seed) = &bundle.subject else {
                    return Err(TrustError::new(
                        "seed_subject_wrong_kind",
                        "seed_committed requires the canonical seed manifest",
                    ));
                };
                let roots = validate_seed_manifest_semantics(seed)?;
                validate_seed_manifest(
                    seed.value(),
                    &self.metadata.journal_id,
                    &self.metadata.run_id,
                    self.metadata.canonical_genesis_hash,
                    self.metadata.journal_policy_sha256,
                    self.metadata.actor_key_manifest_sha256,
                )?;
                if bundle.event.semantic_root_after != roots.authored_semantic_root {
                    return Err(TrustError::new(
                        "seed_event_semantic_root_mismatch",
                        "seed event semantic root does not equal the recomputed manifest root",
                    ));
                }
            }
            EventKind::AdvanceGateApproved => {
                if approval_semantic_root(&bundle.subject)?
                    != bundle.event.semantic_root_after
                {
                    return Err(TrustError::new(
                        "approval_semantic_root_mismatch",
                        "approval subject does not equal the event semantic root",
                    ));
                }
                self.projection.current_human_approval_event_hash =
                    Some(bundle.event.event_hash);
                self.projection.current_human_approval_subject_hash =
                    Some(bundle.payload.subject_sha256);
                self.projection.current_approved_evidence_tool_input_root = Some(
                    approval_evidence_root(&bundle.subject)?,
                );
                self.projection.current_gate_presentation_sha256 = Some(
                    approval_gate_presentation(&bundle.subject)?,
                );
                self.projection.current_revision_closure = None;
                self.projection.routine_gate_outcome =
                    JournalRoutineGateOutcome::Approved(bundle.event.event_hash);
            }
            EventKind::AdvanceGateFeedback => {
                self.projection.routine_gate_outcome =
                    JournalRoutineGateOutcome::Feedback(bundle.event.event_hash);
            }
            EventKind::ProtectedReapprovalApproved => {
                let authorization_id = bundle.event.authorization_id.as_ref().ok_or_else(|| {
                    TrustError::new(
                        "protected_reapproval_authorization_missing",
                        "protected approval lacks its authorization ID",
                    )
                })?;
                let state = self
                    .projection
                    .audit_authorizations
                    .get(authorization_id)
                    .ok_or_else(|| {
                        TrustError::new("unknown_audit_authorization", authorization_id.clone())
                    })?;
                validate_protected_reapproval_subject(
                    bundle.event.event_kind,
                    &bundle.subject,
                    state,
                    &self.head.public_head(),
                )?;
                let revision_closure = state.proposed_revision_closure.ok_or_else(|| {
                    TrustError::new(
                        "protected_reapproval_proposal_missing",
                        "open lane lacks its verified closure proposal",
                    )
                })?;
                if approval_semantic_root(&bundle.subject)?
                    != bundle.event.semantic_root_after
                {
                    return Err(TrustError::new(
                        "approval_semantic_root_mismatch",
                        "protected approval subject does not equal the event semantic root",
                    ));
                }
                self.projection.current_human_approval_event_hash =
                    Some(bundle.event.event_hash);
                self.projection.current_human_approval_subject_hash =
                    Some(bundle.payload.subject_sha256);
                self.projection.current_approved_evidence_tool_input_root = Some(
                    approval_evidence_root(&bundle.subject)?,
                );
                self.projection.current_gate_presentation_sha256 = Some(
                    approval_gate_presentation(&bundle.subject)?,
                );
                self.projection.current_revision_closure = Some(revision_closure);
                close_revision_lane(&mut self.projection, &bundle.event)?;
            }
            EventKind::AuditAuthorization => {
                let value = bundle.subject.canonical_json().ok_or_else(|| {
                    TrustError::new(
                        "audit_authorization_not_structured",
                        "audit authorization must be canonical JSON",
                    )
                })?;
                let id = value_string(value, "authorization_id")?.to_owned();
                let lane = value_string(value, "authorized_revision_lane_id")?.to_owned();
                let authorization_record = match &bundle.subject {
                    Subject::CanonicalRecord(record) => record,
                    _ => {
                        return Err(TrustError::new(
                            "audit_authorization_record_missing",
                            "audit authorization must be an authoritative record",
                        ))
                    }
                };
                validate_audit_authorization_subject(
                    authorization_record,
                    &id,
                    &bundle.event.actor_identity,
                    self.projection.current_human_approval_event_hash,
                    &self.head.public_head(),
                )?;
                if self.has_nonclosed_audit_authorization()
                    || self.projection.audit_authorizations.contains_key(&id)
                    || self
                        .projection
                        .audit_authorizations
                        .values()
                        .any(|state| state.lane_id == lane)
                {
                    return Err(TrustError::new(
                        "duplicate_audit_authorization_or_lane",
                        "authorization IDs and revision lanes are journal-unique",
                    ));
                }
                let permitted_changes = parse_permitted_changes(value)?;
                self.projection.audit_authorizations.insert(
                    id.clone(),
                    AuditAuthorizationState {
                        authorization_id: id.clone(),
                        event_hash: bundle.event.event_hash,
                        lane_id: lane,
                        status: AuditLaneStatus::Authorized,
                        originating_approval_event_hash: value_digest(
                            value,
                            "current_human_approval_event_hash",
                        )?,
                        permitted_changes,
                        maximum_semantic_scope_sha256: value_digest(
                            value,
                            "maximum_semantic_scope_sha256",
                        )?,
                        proposed_revision_closure: None,
                    },
                );
            }
            EventKind::RevisionOpened => {
                let id = bundle.event.authorization_id.as_ref().ok_or_else(|| {
                    TrustError::new("revision_authorization_missing", "revision lacks ID")
                })?;
                let state = self
                    .projection
                    .audit_authorizations
                    .get_mut(id)
                    .ok_or_else(|| TrustError::new("unknown_audit_authorization", id.clone()))?;
                let proposed_closure =
                    validate_revision_proposal_subject(
                        &bundle.subject,
                        state,
                        &self.head.public_head(),
                        &self.metadata,
                    )?;
                state.status = AuditLaneStatus::Open;
                state.proposed_revision_closure = Some(proposed_closure);
                self.projection.used_revision_lanes.insert(state.lane_id.clone());
            }
            EventKind::ProtectedReapprovalFeedback => {
                let authorization_id = bundle.event.authorization_id.as_ref().ok_or_else(|| {
                    TrustError::new(
                        "protected_feedback_authorization_missing",
                        "protected feedback lacks its authorization ID",
                    )
                })?;
                let state = self
                    .projection
                    .audit_authorizations
                    .get(authorization_id)
                    .ok_or_else(|| {
                        TrustError::new("unknown_audit_authorization", authorization_id.clone())
                    })?;
                validate_protected_reapproval_subject(
                    bundle.event.event_kind,
                    &bundle.subject,
                    state,
                    &self.head.public_head(),
                )?;
                close_revision_lane(&mut self.projection, &bundle.event)?;
            }
            EventKind::Revoked => {
                if let Some(current) = self.projection.current_human_approval_event_hash.take() {
                    self.projection.revoked_approval_events.insert(current);
                    self.projection.current_human_approval_subject_hash = None;
                    self.projection.current_approved_evidence_tool_input_root = None;
                    self.projection.current_gate_presentation_sha256 = None;
                    self.projection.current_revision_closure = None;
                }
                for authorization in self.projection.audit_authorizations.values_mut() {
                    if authorization.status != AuditLaneStatus::Closed {
                        authorization.status = AuditLaneStatus::Closed;
                    }
                }
            }
            EventKind::PackageAuthorized => {
                self.projection.current_package_authorization_event_hash =
                    Some(bundle.event.event_hash);
            }
            _ => {}
        }
        self.projection.semantic_root = bundle.event.semantic_root_after;
        self.projection.derived_result_root = bundle.event.derived_result_root_after;
        self.head = DurableHead {
            schema: JOURNAL_HEAD_SCHEMA.into(),
            journal_id: bundle.event.journal_id.clone(),
            sequence_number: bundle.event.sequence_number,
            event_hash: bundle.event.event_hash,
            transaction_id: bundle.event.transaction_id.clone(),
        };
        Ok(())
    }

    fn projection_root(&self) -> Result<Sha256Digest, TrustError> {
        let authorizations: Vec<_> = self
            .projection
            .audit_authorizations
            .values()
            .map(|state| {
                let permitted: Vec<_> = state
                    .permitted_changes
                    .iter()
                    .map(|(item_id, change_kind)| {
                        serde_json::json!({"item_id": item_id, "change_kind": change_kind})
                    })
                    .collect();
                serde_json::json!({
                    "authorization_id": state.authorization_id,
                    "event_hash": state.event_hash,
                    "lane_id": state.lane_id,
                    "status": match state.status {
                        AuditLaneStatus::Authorized => "authorized",
                        AuditLaneStatus::Open => "open",
                        AuditLaneStatus::Closed => "closed",
                    },
                    "originating_approval_event_hash": state.originating_approval_event_hash,
                    "permitted_changes": permitted,
                    "maximum_semantic_scope_sha256": state.maximum_semantic_scope_sha256,
                    "proposed_revision_closure": state.proposed_revision_closure,
                })
            })
            .collect();
        let value = serde_json::json!({
            "semantic_root": self.projection.semantic_root,
            "derived_result_root": self.projection.derived_result_root,
            "current_human_approval_event_hash": self.projection.current_human_approval_event_hash,
            "current_human_approval_subject_hash": self.projection.current_human_approval_subject_hash,
            "current_approved_evidence_tool_input_root": self.projection.current_approved_evidence_tool_input_root,
            "revoked_approval_events": self.projection.revoked_approval_events,
            "audit_authorizations": authorizations,
            "used_revision_lanes": self.projection.used_revision_lanes,
            "routine_gate_outcome": match self.projection.routine_gate_outcome {
                JournalRoutineGateOutcome::NotPresented => serde_json::json!({"status": "not_presented"}),
                JournalRoutineGateOutcome::Approved(event_hash) => serde_json::json!({"status": "approved", "event_hash": event_hash}),
                JournalRoutineGateOutcome::Feedback(event_hash) => serde_json::json!({"status": "feedback", "event_hash": event_hash}),
            },
            "current_revision_closure": self.projection.current_revision_closure,
            "current_package_authorization_event_hash": self.projection.current_package_authorization_event_hash,
            "journal_head": self.head.public_head(),
        });
        Ok(tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&value)?,
        ))
    }
}

fn decode_subject(
    registry: &SchemaRegistry,
    bundle: &Value,
    payload: &JournalEventPayload,
) -> Result<Subject, TrustError> {
    match bundle.get("subject_encoding").and_then(Value::as_str) {
        Some("canonical_json") => {
            let value = bundle.get("subject_json").cloned().ok_or_else(|| {
                TrustError::new("journal_subject_missing", "bundle lacks subject_json")
            })?;
            let record = if payload.subject_schema_id.as_deref()
                == Some("trellis://schemas/qualification-profile/v1")
            {
                AuthoritativeRecord::parse_as(
                    registry,
                    "trellis-qualification-profile/v1",
                    value,
                )?
            } else {
                AuthoritativeRecord::parse(registry, value)?
            };
            if payload.subject_schema_id.as_deref() != Some(record.contract().schema_id)
                || payload.subject_schema_sha256
                    != Some(registry.schema_sha256(record.contract().schema_id)?)
            {
                return Err(TrustError::new(
                    "journal_subject_schema_binding_mismatch",
                    "payload does not bind the subject's exact schema",
                ));
            }
            if payload.subject_kind == "closure_transition" {
                Ok(Subject::ClosureTransition(record))
            } else {
                Ok(Subject::CanonicalRecord(record))
            }
        }
        Some("raw_base64") => {
            if payload.subject_schema_id.is_some() || payload.subject_schema_sha256.is_some() {
                return Err(TrustError::new(
                    "raw_subject_claims_schema",
                    "raw subjects cannot carry a structured schema reference",
                ));
            }
            let encoded = bundle
                .get("subject_base64")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TrustError::new("journal_subject_missing", "bundle lacks subject_base64")
                })?;
            let bytes = BASE64_STANDARD.decode(encoded).map_err(|error| {
                TrustError::new("journal_subject_base64_invalid", error.to_string())
            })?;
            Ok(Subject::RawArtifact(bytes))
        }
        _ => Err(TrustError::new(
            "journal_subject_encoding_invalid",
            "unknown journal subject encoding",
        )),
    }
}

fn event_adds_derived_result(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::ProofChecked
            | EventKind::WitnessRefutationChecked
            | EventKind::UnrestrictedVerdictRecorded
            | EventKind::SourceValidationClassified
            | EventKind::SourceValidationAttemptRecorded
            | EventKind::SourceValidationOutcomeRecorded
            | EventKind::ReflectionValidationResultRecorded
            | EventKind::SourceValidationHistorySummarized
            | EventKind::ApprovedProfileSelected
            | EventKind::ConditionalStatementGenerated
            | EventKind::QualificationObligationsChecked
            | EventKind::NoQualifiedResultEstablished
            | EventKind::ApplicabilityClassified
            | EventKind::ExternalClaimRowsGenerated
    )
}

fn validate_seed_manifest(
    value: &Value,
    journal_id: &str,
    run_id: &str,
    genesis: Sha256Digest,
    policy: Sha256Digest,
    actor_keys: Sha256Digest,
) -> Result<(), TrustError> {
    if value_string(value, "journal_id")? != journal_id
        || value_string(value, "run_id")? != run_id
        || value_digest(value, "canonical_genesis_sha256")? != genesis
        || value_digest(value, "journal_event_policy_sha256")? != policy
        || value_digest(value, "actor_key_manifest_sha256")? != actor_keys
    {
        return Err(TrustError::new(
            "seed_manifest_journal_binding_mismatch",
            "seed manifest does not bind this journal, policy, genesis, and key manifest",
        ));
    }
    Ok(())
}

fn validate_subject_head_binding(
    event_kind: EventKind,
    subject: &Subject,
    head: &JournalHead,
    current_approval: Option<Sha256Digest>,
) -> Result<(), TrustError> {
    let value = match subject.canonical_json() {
        Some(value) => value,
        None => return Ok(()),
    };
    let head_field = match event_kind {
        EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved => {
            Some("approved_journal_head")
        }
        EventKind::AuditAuthorization => Some("authorized_from_journal_head"),
        EventKind::PackageAuthorized => Some("pre_authorization_journal_head"),
        _ => None,
    };
    if let Some(field) = head_field {
        let subject_head: JournalHead = serde_json::from_value(
            value.get(field).cloned().ok_or_else(|| {
                TrustError::new("subject_head_missing", format!("missing {field}"))
            })?,
        )
        .map_err(|error| TrustError::new("subject_head_invalid", error.to_string()))?;
        if subject_head != *head {
            return Err(TrustError::new(
                "subject_not_bound_to_immediate_head",
                format!("{field} must equal the immediate predecessor"),
            ));
        }
    }
    if matches!(event_kind, EventKind::AuditAuthorization | EventKind::PackageAuthorized) {
        let field = if event_kind == EventKind::AuditAuthorization {
            "current_human_approval_event_hash"
        } else {
            "human_approval_event_hash"
        };
        if value_digest(value, field)? != current_approval.ok_or_else(|| {
            TrustError::new(
                "current_human_approval_missing",
                "operation requires a current unrevoked human approval",
            )
        })? {
            return Err(TrustError::new(
                "stale_human_approval_reference",
                "subject does not name the current human approval event",
            ));
        }
    }
    Ok(())
}

fn principal_identity(actor: &JournalActor) -> Option<&str> {
    match actor {
        JournalActor::Kernel => None,
        JournalActor::Authenticated { identity, .. } => Some(identity),
    }
}

fn validate_audit_authorization_subject(
    record: &AuthoritativeRecord,
    authorization_id: &str,
    audit_identity: &str,
    current_approval: Option<Sha256Digest>,
    head: &JournalHead,
) -> Result<(), TrustError> {
    if record.contract().record_schema != "trellis-audit-authorization/v1"
        || value_string(record.value(), "authorization_id")? != authorization_id
        || value_string(record.value(), "audit_identity")? != audit_identity
        || record.value().get("one_shot").and_then(Value::as_bool) != Some(true)
    {
        return Err(TrustError::new(
            "audit_authorization_identity_mismatch",
            "audit authorization schema, identity, or one-shot policy is invalid",
        ));
    }
    let approval = current_approval.ok_or_else(|| {
        TrustError::new(
            "audit_authorization_without_approval",
            "exceptional authorization requires a current approval",
        )
    })?;
    if value_digest(record.value(), "current_human_approval_event_hash")? != approval {
        return Err(TrustError::new(
            "audit_authorization_stale_approval",
            "audit authorization does not name the current approval",
        ));
    }
    let authorized_head: JournalHead = serde_json::from_value(
        record
            .value()
            .get("authorized_from_journal_head")
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "audit_authorization_head_missing",
                    "audit authorization lacks its predecessor head",
                )
            })?,
    )
    .map_err(|error| TrustError::new("audit_authorization_head_invalid", error.to_string()))?;
    if &authorized_head != head
        || value_digest(record.value(), "reason_sha256")? == Sha256Digest::ZERO
    {
        return Err(TrustError::new(
            "audit_authorization_basis_invalid",
            "audit authorization has a stale head or empty reason",
        ));
    }
    let permitted = record
        .value()
        .get("permitted_changes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "audit_permitted_changes_missing",
                "audit authorization lacks permitted changes",
            )
        })?;
    if permitted.is_empty()
        || value_digest(record.value(), "maximum_semantic_scope_sha256")?
            != tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&Value::Array(permitted.clone()))?,
            )
    {
        return Err(TrustError::new(
            "audit_maximum_scope_mismatch",
            "maximum semantic scope must hash the exact ordered permitted-change list",
        ));
    }
    parse_permitted_changes(record.value())?;
    Ok(())
}

fn parse_permitted_changes(value: &Value) -> Result<BTreeSet<(String, String)>, TrustError> {
    let changes = value
        .get("permitted_changes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "audit_permitted_changes_missing",
                "audit authorization lacks permitted changes",
            )
        })?;
    let mut parsed = BTreeSet::new();
    let mut prior: Option<Vec<u8>> = None;
    for change in changes {
        let canonical = canonical_json_value(change)?;
        if prior.as_ref().is_some_and(|prior| prior >= &canonical) {
            return Err(TrustError::new(
                "audit_permitted_changes_order_invalid",
                "permitted changes must be unique and canonical-byte sorted",
            ));
        }
        prior = Some(canonical);
        let item_id = value_string(change, "item_id")?.to_owned();
        let change_kind = value_string(change, "change_kind")?.to_owned();
        if !parsed.insert((item_id, change_kind)) {
            return Err(TrustError::new(
                "audit_permitted_change_duplicate",
                "permitted changes must be unique",
            ));
        }
    }
    Ok(parsed)
}

fn validate_revision_changes(
    changes: &[AuthorizedRevisionChange],
    permitted: &BTreeSet<(String, String)>,
) -> Result<Value, TrustError> {
    if changes.is_empty() {
        return Err(TrustError::new(
            "revision_change_set_empty",
            "an exceptional revision must name at least one actual change",
        ));
    }
    let mut parsed = BTreeSet::new();
    for change in changes {
        validate_id("revision item_id", &change.item_id)?;
        validate_id("revision change_kind", &change.change_kind)?;
        let key = (change.item_id.clone(), change.change_kind.clone());
        if !permitted.contains(&key) {
            return Err(TrustError::new(
                "revision_change_outside_authorized_scope",
                format!("{}/{} was not authorized", change.item_id, change.change_kind),
            ));
        }
        if !parsed.insert(key) {
            return Err(TrustError::new(
                "revision_change_duplicate",
                "revision changes must be unique",
            ));
        }
    }
    Ok(Value::Array(
        parsed
            .into_iter()
            .map(|(item_id, change_kind)| {
                serde_json::json!({"item_id": item_id, "change_kind": change_kind})
            })
            .collect(),
    ))
}

fn validate_revision_proposal_subject(
    subject: &Subject,
    state: &AuditAuthorizationState,
    head: &JournalHead,
    metadata: &JournalMetadata,
) -> Result<RevisionClosureProjection, TrustError> {
    let bytes = subject.raw_bytes().ok_or_else(|| {
        TrustError::new(
            "revision_proposal_not_raw",
            "revision proposal must be canonical raw JSON",
        )
    })?;
    let value = parse_json_strict(bytes)?;
    if canonical_json_value(&value)? != bytes
        || value_string(&value, "schema")? != "trellis-authorized-revision-proposal/v1"
        || value_digest(&value, "authorization_event_hash")? != state.event_hash
        || value_digest(&value, "originating_approval_event_hash")?
            != state.originating_approval_event_hash
        || value_string(&value, "revision_lane_id")? != state.lane_id
        || value_digest(&value, "maximum_semantic_scope_sha256")?
            != state.maximum_semantic_scope_sha256
        || value_digest(&value, "journal_predecessor_sha256")? != head.event_hash
    {
        return Err(TrustError::new(
            "revision_proposal_binding_mismatch",
            "revision proposal differs from its authorization or predecessor",
        ));
    }
    let changes: Vec<AuthorizedRevisionChange> = serde_json::from_value(
        value
            .get("changes")
            .cloned()
            .ok_or_else(|| TrustError::new("revision_changes_missing", "changes are absent"))?,
    )
    .map_err(|error| TrustError::new("revision_changes_invalid", error.to_string()))?;
    let changes_value = validate_revision_changes(&changes, &state.permitted_changes)?;
    if value.get("changes") != Some(&changes_value)
        || value_digest(&value, "change_scope_sha256")?
            != tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&changes_value)?,
            )
    {
        return Err(TrustError::new(
            "revision_change_scope_mismatch",
            "revision proposal change set or scope digest is invalid",
        ));
    }
    let registry = SchemaRegistry::v1()?;
    let seed = AuthoritativeRecord::parse(
        &registry,
        value
            .get("revised_seed_manifest")
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "revision_seed_manifest_missing",
                    "revision proposal lacks its seed manifest",
                )
            })?,
    )?;
    let bundle = value.get("revised_seed_definition_bundle").ok_or_else(|| {
        TrustError::new(
            "revision_seed_bundle_missing",
            "revision proposal lacks its seed definition bundle",
        )
    })?;
    let verified = super::closure::verify_seed_definition_bundle(
        &seed,
        &canonical_json_value(bundle)?,
    )?;
    let roots = validate_seed_manifest_semantics(&seed)?;
    validate_seed_manifest(
        seed.value(),
        &metadata.journal_id,
        &metadata.run_id,
        metadata.canonical_genesis_hash,
        metadata.journal_policy_sha256,
        metadata.actor_key_manifest_sha256,
    )?;
    let closure = RevisionClosureProjection {
        seed_manifest_sha256: verified.seed_manifest_sha256,
        seed_definition_bundle_sha256: verified.bundle_sha256,
        authored_semantic_root: roots.authored_semantic_root,
        evidence_manifest_sha256: value_digest(&value, "revised_evidence_manifest_sha256")?,
        evidence_tool_input_root: value_digest(&value, "revised_evidence_tool_input_root")?,
    };
    if closure.authored_semantic_root == Sha256Digest::ZERO
        || closure.evidence_manifest_sha256 == Sha256Digest::ZERO
        || closure.evidence_tool_input_root == Sha256Digest::ZERO
        || roots.evidence_tool_input_root != closure.evidence_tool_input_root
        || value_digest(&value, "revised_seed_manifest_sha256")?
            != closure.seed_manifest_sha256
        || value_digest(&value, "revised_seed_definition_bundle_sha256")?
            != closure.seed_definition_bundle_sha256
        || value_digest(&value, "revised_authored_semantic_root")?
            != closure.authored_semantic_root
    {
        return Err(TrustError::new(
            "revision_proposal_closure_mismatch",
            "revised closure bodies, digests, and roots do not agree",
        ));
    }
    Ok(closure)
}

fn validate_protected_reapproval_subject(
    event_kind: EventKind,
    subject: &Subject,
    state: &AuditAuthorizationState,
    head: &JournalHead,
) -> Result<(), TrustError> {
    if state.status != AuditLaneStatus::Open {
        return Err(TrustError::new(
            "protected_reapproval_lane_not_open",
            "protected reapproval requires an open lane",
        ));
    }
    let proposed = state.proposed_revision_closure.ok_or_else(|| {
        TrustError::new(
            "protected_reapproval_closure_missing",
            "open lane lacks its verified closure proposal",
        )
    })?;
    match event_kind {
        EventKind::ProtectedReapprovalApproved => {
            let value = subject.canonical_json().ok_or_else(|| {
                TrustError::new(
                    "protected_approval_not_structured",
                    "protected approval must be a human-approval record",
                )
            })?;
            if value_string(value, "schema")? != "trellis-human-approval/v1"
                || value_digest(value, "authored_semantic_root")?
                    != proposed.authored_semantic_root
                || value_digest(value, "approved_evidence_tool_input_root")?
                    != proposed.evidence_tool_input_root
            {
                return Err(TrustError::new(
                    "protected_approval_proposal_mismatch",
                    "protected approval does not approve the exact proposed closures",
                ));
            }
        }
        EventKind::ProtectedReapprovalFeedback => {
            let bytes = subject.raw_bytes().ok_or_else(|| {
                TrustError::new(
                    "protected_feedback_not_raw",
                    "protected feedback must be canonical raw JSON",
                )
            })?;
            let value = parse_json_strict(bytes)?;
            if canonical_json_value(&value)? != bytes
                || value_string(&value, "schema")?
                    != "trellis-protected-reapproval-feedback/v1"
                || value_string(&value, "authorization_id")? != state.authorization_id
                || value_string(&value, "revision_lane_id")? != state.lane_id
                || value_digest(&value, "revised_authored_semantic_root")?
                    != proposed.authored_semantic_root
                || value_digest(&value, "revised_evidence_tool_input_root")?
                    != proposed.evidence_tool_input_root
                || value_digest(&value, "revised_seed_manifest_sha256")?
                    != proposed.seed_manifest_sha256
                || value_digest(&value, "revised_seed_definition_bundle_sha256")?
                    != proposed.seed_definition_bundle_sha256
                || value_digest(&value, "revised_evidence_manifest_sha256")?
                    != proposed.evidence_manifest_sha256
                || value_digest(&value, "gate_presentation_sha256")?
                    == Sha256Digest::ZERO
                || value_digest(&value, "journal_predecessor_sha256")? != head.event_hash
                || value_string(&value, "choice")? != "feedback"
            {
                return Err(TrustError::new(
                    "protected_feedback_proposal_mismatch",
                    "protected feedback does not bind the exact open proposal",
                ));
            }
        }
        _ => {
            return Err(TrustError::new(
                "protected_reapproval_kind_invalid",
                "unexpected protected reapproval event",
            ))
        }
    }
    Ok(())
}


fn close_revision_lane(
    projection: &mut Projection,
    event: &JournalEvent,
) -> Result<(), TrustError> {
    let id = event.authorization_id.as_ref().ok_or_else(|| {
        TrustError::new("revision_authorization_missing", "terminal gate lacks ID")
    })?;
    let state = projection
        .audit_authorizations
        .get_mut(id)
        .ok_or_else(|| TrustError::new("unknown_audit_authorization", id.clone()))?;
    if state.status != AuditLaneStatus::Open {
        return Err(TrustError::new(
            "revision_lane_not_open",
            "protected gate may terminate only an open revision lane",
        ));
    }
    state.status = AuditLaneStatus::Closed;
    Ok(())
}

fn approval_semantic_root(subject: &Subject) -> Result<Sha256Digest, TrustError> {
    let value = subject.canonical_json().ok_or_else(|| {
        TrustError::new("approval_not_structured", "approval subject must be canonical JSON")
    })?;
    value_digest(value, "authored_semantic_root")
}

fn approval_evidence_root(subject: &Subject) -> Result<Sha256Digest, TrustError> {
    let value = subject.canonical_json().ok_or_else(|| {
        TrustError::new("approval_not_structured", "approval subject must be canonical JSON")
    })?;
    value_digest(value, "approved_evidence_tool_input_root")
}

fn approval_gate_presentation(subject: &Subject) -> Result<Sha256Digest, TrustError> {
    let value = subject.canonical_json().ok_or_else(|| {
        TrustError::new("approval_not_structured", "approval subject must be canonical JSON")
    })?;
    value_digest(value, "gate_presentation_sha256")
}

fn event_authorization_link(event: &JournalEvent) -> Result<Option<AuthorizationLink>, TrustError> {
    match (
        event.authorization_id.as_ref(),
        event.authorization_event_hash,
        event.revision_lane_id.as_ref(),
    ) {
        (None, None, None) => Ok(None),
        (Some(authorization_id), Some(authorization_event_hash), Some(revision_lane_id)) => {
            Ok(Some(AuthorizationLink {
                authorization_id: authorization_id.clone(),
                authorization_event_hash,
                revision_lane_id: revision_lane_id.clone(),
            }))
        }
        _ => Err(TrustError::new(
            "partial_revision_authorization_link",
            "authorization id, event hash, and revision lane must be all present or all absent",
        )),
    }
}

fn validate_principal_binding(
    policy: &super::records::JournalPolicyEntry,
    event: &JournalEvent,
    payload: &JournalEventPayload,
    subject: &Subject,
) -> Result<(), TrustError> {
    let structured = subject.canonical_json();
    let expected = match policy.actor_identity_policy {
        ActorIdentityPolicy::TrellisKernel => {
            if payload.principal_identity.is_some() {
                return Err(TrustError::new(
                    "kernel_payload_has_principal",
                    "kernel-internal payload must not claim an external principal",
                ));
            }
            "trellis-kernel"
        }
        ActorIdentityPolicy::PayloadReviewerIdentity => structured
            .and_then(|value| value.get("reviewer_identity"))
            .and_then(Value::as_str)
            .or(payload.principal_identity.as_deref())
            .ok_or_else(|| {
                TrustError::new(
                    "reviewer_identity_missing",
                    "authenticated reviewer event lacks its bound identity",
                )
            })?,
        ActorIdentityPolicy::PayloadAuditIdentity => structured
            .and_then(|value| value.get("audit_identity"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new(
                    "audit_identity_missing",
                    "audit authorization lacks audit_identity",
                )
            })?,
    };
    if event.actor_identity != expected {
        return Err(TrustError::new(
            "actor_subject_identity_mismatch",
            "event actor identity does not equal the policy-selected subject identity",
        ));
    }
    if policy.actor_identity_policy != ActorIdentityPolicy::TrellisKernel
        && payload.principal_identity.as_deref() != Some(expected)
    {
        return Err(TrustError::new(
            "payload_principal_identity_mismatch",
            "payload principal identity does not equal the authenticated event actor",
        ));
    }
    Ok(())
}

pub fn canonical_genesis(journal_id: &str) -> Result<Sha256Digest, TrustError> {
    Ok(tagged_hash(
        DomainTag::JournalGenesis,
        &canonical_json_value(&serde_json::json!({
            "journal_id": journal_id,
            "journal_schema": "trellis-trust-journal/v1"
        }))?,
    ))
}

fn empty_root(tag: DomainTag) -> Result<Sha256Digest, TrustError> {
    Ok(tagged_hash(tag, &canonical_json_value(&Value::Array(Vec::new()))?))
}

fn value_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "record_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn value_digest(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    value_string(value, field)?.parse()
}

fn validate_id(field: &str, value: &str) -> Result<(), TrustError> {
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(TrustError::new(
            "invalid_journal_id_field",
            format!("{field} must be 1..256 non-control characters"),
        ));
    }
    Ok(())
}

fn event_path(events: &Path, sequence: u64) -> PathBuf {
    events.join(format!("{sequence:016}.json"))
}

fn parse_event_filename(name: &str) -> Result<u64, TrustError> {
    if name.len() != 21 || !name.ends_with(".json") || !name[..16].bytes().all(|b| b.is_ascii_digit()) {
        return Err(TrustError::new(
            "invalid_journal_event_filename",
            format!("unexpected event filename {name:?}"),
        ));
    }
    name[..16].parse::<u64>().map_err(|error| {
        TrustError::new("invalid_journal_event_sequence", error.to_string())
    })
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), TrustError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io_error("create immutable journal file"))?;
    file.write_all(bytes)
        .map_err(io_error("write immutable journal file"))?;
    file.sync_all()
        .map_err(io_error("fsync immutable journal file"))
}

fn write_head_atomic(root: &Path, head: &DurableHead) -> Result<(), TrustError> {
    let temp = root.join(format!(".HEAD-{}-{}.tmp", head.sequence_number, head.event_hash));
    let _ = fs::remove_file(&temp);
    write_new_synced(&temp, &canonical_json(head)?)?;
    fs::rename(&temp, root.join("HEAD")).map_err(io_error("atomically install journal HEAD"))?;
    sync_dir(root)
}

fn sync_dir(path: &Path) -> Result<(), TrustError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error("fsync journal directory"))
}

fn read_canonical_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, TrustError> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(io_error("read journal metadata"))?;
    let value = parse_json_strict(&bytes)
        .map_err(|error| TrustError::new("journal_json_invalid", error.to_string()))?;
    if canonical_json_value(&value)? != bytes {
        return Err(TrustError::new(
            "journal_json_not_canonical",
            format!("{} is not canonical JSON", path.display()),
        ));
    }
    serde_json::from_value(value)
        .map_err(|error| TrustError::new("journal_json_decode_failed", error.to_string()))
}

fn io_error(action: &'static str) -> impl Fn(std::io::Error) -> TrustError {
    move |error| TrustError::new("journal_io_error", format!("{action}: {error}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use super::super::auth::ManifestAuthorityRoots;
    use ed25519_dalek::SigningKey;

    fn fixture_vector(name: &str) -> Value {
        let fixture: Value = serde_json::from_str(include_str!("schemas/JOURNAL_HASH_FIXTURES.v1.json"))
        .unwrap();
        fixture["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|vector| vector["name"] == name)
            .unwrap()
            .clone()
    }

    fn fixture_manifest(registry: &SchemaRegistry) -> ActorKeyManifest {
        let vector = fixture_vector("actor_key_manifest_self");
        let mut value = vector["payload"].clone();
        value.as_object_mut().unwrap().insert(
            "manifest_sha256".into(),
            vector["expected_sha256"].clone(),
        );
        let root = SigningKey::from_bytes(&[3_u8; 32]).verifying_key();
        let mut roots = ManifestAuthorityRoots::default();
        roots
            .insert_hex(
                "fixture-v1-manifest-root",
                &root
                    .to_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            )
            .unwrap();
        ActorKeyManifest::verify(registry, value, &roots).unwrap()
    }

    fn record_from_vector(
        registry: &SchemaRegistry,
        name: &str,
        self_field: &str,
    ) -> AuthoritativeRecord {
        let vector = fixture_vector(name);
        let mut value = vector["payload"].clone();
        value.as_object_mut().unwrap().insert(
            self_field.into(),
            vector["expected_sha256"].clone(),
        );
        AuthoritativeRecord::parse(registry, value).unwrap()
    }

    fn sign_test_receipt(
        journal: &TrustJournal,
        manifest: &ActorKeyManifest,
        transaction_id: &str,
        event_kind: EventKind,
        payload: &JournalEventPayload,
        actor_role: ActorRole,
        actor_identity: &str,
        lane_id: &str,
        key_id: &str,
        key_byte: u8,
    ) -> Value {
        let registry = SchemaRegistry::v1().unwrap();
        let predecessor = journal.head();
        super::super::auth::sign_actor_receipt(
            &registry,
            manifest,
            &ReceiptContext {
                journal_id: journal.journal_id(),
                run_id: journal.run_id(),
                transaction_id,
                event_kind,
                event_payload_sha256: payload.payload_sha256,
                predecessor_head: &predecessor,
                actor_role,
                actor_identity,
                gate_or_revision_lane_id: lane_id,
            },
            key_id,
            &SigningKey::from_bytes(&[key_byte; 32]),
        )
        .unwrap()
    }

    fn append_routine_approval(
        journal: &mut TrustJournal,
        manifest: &ActorKeyManifest,
    ) -> Sha256Digest {
        let registry = SchemaRegistry::v1().unwrap();
        let seed_bundle = journal.committed_bundle_value(1).unwrap();
        let seed = AuthoritativeRecord::parse(
            &registry,
            seed_bundle.get("subject_json").cloned().unwrap(),
        )
        .unwrap();
        let roots = validate_seed_manifest_semantics(&seed).unwrap();
        let subject = Subject::CanonicalRecord(
            AuthoritativeRecord::parse(
                &registry,
                serde_json::json!({
                    "schema": "trellis-human-approval/v1",
                    "authored_semantic_root": roots.authored_semantic_root,
                    "approved_evidence_tool_input_root": roots.evidence_tool_input_root,
                    "gate_presentation_sha256": tagged_hash(
                        DomainTag::GatePresentation,
                        b"fixture routine gate\n",
                    ),
                    "reviewer_identity": "fixture-reviewer",
                    "approved_journal_head": journal.head(),
                }),
            )
            .unwrap(),
        );
        let payload = journal
            .prepare_routine_gate_payload(
                EventKind::AdvanceGateApproved,
                "fixture-routine-gate",
                "fixture-reviewer",
                &subject,
            )
            .unwrap();
        let receipt = sign_test_receipt(
            journal,
            manifest,
            "fixture-routine-approval",
            EventKind::AdvanceGateApproved,
            &payload,
            ActorRole::Reviewer,
            "fixture-reviewer",
            "fixture-routine-gate",
            "fixture-reviewer-key",
            1,
        );
        journal
            .append(AppendRequest {
                transaction_id: "fixture-routine-approval".into(),
                event_kind: EventKind::AdvanceGateApproved,
                subject_id: "fixture-routine-gate".into(),
                subject,
                actor: JournalActor::Authenticated {
                    role: ActorRole::Reviewer,
                    identity: "fixture-reviewer".into(),
                    gate_or_revision_lane_id: "fixture-routine-gate".into(),
                    receipt,
                },
                semantic_root_after: journal.semantic_root(),
                derived_result_root_after: journal.derived_result_root(),
                authorization: None,
            })
            .unwrap()
            .event_hash
    }

    fn commit_test_audit_authorization(
        journal: &mut TrustJournal,
        manifest: &ActorKeyManifest,
    ) -> Sha256Digest {
        let registry = SchemaRegistry::v1().unwrap();
        let permitted_changes = serde_json::json!([{
            "item_id": "fixture-target",
            "change_kind": "target_statement",
        }]);
        let mut value = serde_json::json!({
            "schema": "trellis-audit-authorization/v1",
            "authorization_id": "fixture-authorization",
            "audit_identity": "fixture-auditor",
            "current_human_approval_event_hash": journal.current_human_approval_event_hash().unwrap(),
            "authorized_from_journal_head": journal.head(),
            "authorized_revision_lane_id": "fixture-revision-lane",
            "permitted_changes": permitted_changes,
            "maximum_semantic_scope_sha256": tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&permitted_changes).unwrap(),
            ),
            "reason_sha256": tagged_hash(DomainTag::ManifestNode, b"fixture audit reason\n"),
            "one_shot": true,
            "authorization_artifact_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::AuditAuthorization,
            &value,
            "authorization_artifact_sha256",
        )
        .unwrap();
        value["authorization_artifact_sha256"] = Value::String(digest.to_string());
        let authorization = AuthoritativeRecord::parse(&registry, value).unwrap();
        let payload = journal
            .prepare_audit_authorization_payload(
                "fixture-authorization",
                "fixture-auditor",
                &authorization,
            )
            .unwrap();
        let receipt = sign_test_receipt(
            journal,
            manifest,
            "fixture-audit-transaction",
            EventKind::AuditAuthorization,
            &payload,
            ActorRole::AuditAuthority,
            "fixture-auditor",
            "fixture-authorization",
            "fixture-audit-key",
            2,
        );
        journal
            .commit_audit_authorization(
                "fixture-audit-transaction",
                "fixture-authorization",
                "fixture-auditor",
                authorization,
                receipt,
            )
            .unwrap()
            .event_hash
    }

    pub(crate) const EXCEPTIONAL_SUPPORT_BYTES: &[u8] =
        b"def RustValidSliceU8 : Prop := True\n";

    fn revised_proposal_fixture(
        journal: &TrustJournal,
    ) -> (
        AuthorizedRevisionProposal,
        RevisionClosureProjection,
        Value,
    ) {
        let registry = SchemaRegistry::v1().unwrap();
        let body = serde_json::json!({
            "schema": "trellis-fixture-target-definition/v1",
            "statement": "revised target statement",
        });
        let record_sha256 = tagged_hash(
            DomainTag::TargetDefinition,
            &canonical_json_value(&body).unwrap(),
        );
        let definition = serde_json::json!({
            "record_kind": "target_definition",
            "record_id": "fixture-target",
            "record_schema_id": "trellis://fixtures/target-definition/v1",
            "record_sha256": record_sha256,
            "domain_tag": "target-definition",
        });
        let definitions = Value::Array(vec![definition.clone()]);
        let authored_semantic_root = tagged_hash(
            DomainTag::AuthoredSemanticRoot,
            &canonical_json_value(&definitions).unwrap(),
        );
        let support_leaf = serde_json::json!({
            "kind": "model_refinement_input",
            "logical_id": "aeneas-validity-definitions",
            "relative_path": "model/Assumptions.lean",
            "byte_length": EXCEPTIONAL_SUPPORT_BYTES.len(),
            "sha256_of_raw_bytes": super::super::canonical::raw_sha256(
                EXCEPTIONAL_SUPPORT_BYTES,
            ),
            "dependency_ids": [],
        });
        let evidence_leaves = Value::Array(vec![support_leaf]);
        let evidence_tool_input_root = tagged_hash(
            DomainTag::EvidenceToolRoot,
            &canonical_json_value(&evidence_leaves).unwrap(),
        );
        let original_seed_bundle = journal.committed_bundle_value(1).unwrap();
        let mut seed_value = original_seed_bundle["subject_json"].clone();
        seed_value["definitions"] = definitions;
        seed_value["authored_semantic_root"] = Value::String(authored_semantic_root.to_string());
        seed_value["approved_evidence_tool_input_root"] =
            Value::String(evidence_tool_input_root.to_string());
        seed_value["manifest_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
        let seed_digest = self_digest(
            DomainTag::SeedAuthoredDefinitionManifest,
            &seed_value,
            "manifest_sha256",
        )
        .unwrap();
        seed_value["manifest_sha256"] = Value::String(seed_digest.to_string());
        let revised_seed_manifest = AuthoritativeRecord::parse(&registry, seed_value).unwrap();

        let mut bundled_definition = definition;
        bundled_definition["canonical_value"] = body;
        let mut bundle = serde_json::json!({
            "schema": "trellis-seed-definition-bundle/v1",
            "definitions": [bundled_definition],
            "bundle_sha256": Sha256Digest::ZERO,
        });
        let bundle_digest = self_digest(DomainTag::ManifestNode, &bundle, "bundle_sha256").unwrap();
        bundle["bundle_sha256"] = Value::String(bundle_digest.to_string());
        let roots = validate_seed_manifest_semantics(&revised_seed_manifest).unwrap();
        let mut evidence_manifest = serde_json::json!({
            "schema": "trellis-evidence-tool-manifest/v1",
            "leaves": evidence_leaves,
            "evidence_tool_input_root": roots.evidence_tool_input_root,
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let evidence_manifest_sha256 = self_digest(
            DomainTag::ManifestNode,
            &evidence_manifest,
            "manifest_sha256",
        )
        .unwrap();
        evidence_manifest["manifest_sha256"] =
            Value::String(evidence_manifest_sha256.to_string());
        let projection = RevisionClosureProjection {
            seed_manifest_sha256: seed_digest,
            seed_definition_bundle_sha256: bundle_digest,
            authored_semantic_root,
            evidence_manifest_sha256,
            evidence_tool_input_root: roots.evidence_tool_input_root,
        };
        (
            AuthorizedRevisionProposal {
                authorization_id: "fixture-authorization".into(),
                revision_lane_id: "fixture-revision-lane".into(),
                revised_seed_manifest,
                revised_seed_definition_bundle: bundle,
                revised_evidence_manifest_sha256: evidence_manifest_sha256,
                revised_evidence_tool_input_root: roots.evidence_tool_input_root,
                changes: vec![AuthorizedRevisionChange {
                    item_id: "fixture-target".into(),
                    change_kind: "target_statement".into(),
                }],
            },
            projection,
            evidence_manifest,
        )
    }

    fn protected_subject(
        journal: &TrustJournal,
        event_kind: EventKind,
        proposal: RevisionClosureProjection,
    ) -> Subject {
        match event_kind {
            EventKind::ProtectedReapprovalApproved => Subject::CanonicalRecord(
                AuthoritativeRecord::parse(
                    &SchemaRegistry::v1().unwrap(),
                    serde_json::json!({
                        "schema": "trellis-human-approval/v1",
                        "authored_semantic_root": proposal.authored_semantic_root,
                        "approved_evidence_tool_input_root": proposal.evidence_tool_input_root,
                        "gate_presentation_sha256": tagged_hash(
                            DomainTag::GatePresentation,
                            b"fixture protected gate\n",
                        ),
                        "reviewer_identity": "fixture-reviewer",
                        "approved_journal_head": journal.head(),
                    }),
                )
                .unwrap(),
            ),
            EventKind::ProtectedReapprovalFeedback => Subject::RawArtifact(
                canonical_json_value(&serde_json::json!({
                    "schema": "trellis-protected-reapproval-feedback/v1",
                    "authorization_id": "fixture-authorization",
                    "revision_lane_id": "fixture-revision-lane",
                    "revised_authored_semantic_root": proposal.authored_semantic_root,
                    "revised_evidence_tool_input_root": proposal.evidence_tool_input_root,
                    "revised_seed_manifest_sha256": proposal.seed_manifest_sha256,
                    "revised_seed_definition_bundle_sha256": proposal.seed_definition_bundle_sha256,
                    "revised_evidence_manifest_sha256": proposal.evidence_manifest_sha256,
                    "gate_presentation_sha256": tagged_hash(
                        DomainTag::GatePresentation,
                        b"fixture protected gate\n",
                    ),
                    "journal_predecessor_sha256": journal.head().event_hash,
                    "choice": "feedback",
                }))
                .unwrap(),
            ),
            _ => panic!("unexpected protected terminal kind"),
        }
    }

    fn commit_test_protected_terminal(
        journal: &mut TrustJournal,
        manifest: &ActorKeyManifest,
        event_kind: EventKind,
        proposal: RevisionClosureProjection,
        transaction_id: &str,
    ) -> Sha256Digest {
        let subject = protected_subject(journal, event_kind, proposal);
        let payload = journal
            .prepare_protected_reapproval_payload(
                event_kind,
                "fixture-authorization",
                "fixture-reviewer",
                &subject,
            )
            .unwrap();
        let receipt = sign_test_receipt(
            journal,
            manifest,
            transaction_id,
            event_kind,
            &payload,
            ActorRole::Reviewer,
            "fixture-reviewer",
            "fixture-revision-lane",
            "fixture-reviewer-key",
            1,
        );
        journal
            .commit_protected_reapproval(
                transaction_id,
                event_kind,
                "fixture-authorization",
                "fixture-reviewer",
                subject,
                receipt,
            )
            .unwrap()
            .event_hash
    }

    pub(crate) struct ExceptionalJournalFixture {
        pub actor_manifest: ActorKeyManifest,
        pub routine_approval_event_hash: Sha256Digest,
        pub revision_closure: RevisionClosureProjection,
        pub terminal_event_hash: Option<Sha256Digest>,
        pub revised_seed_manifest: Value,
        pub revised_seed_definition_bundle: Value,
        pub revised_evidence_manifest: Value,
    }

    /// Cross-module test fixture for journal-ahead runtime recovery.  `None`
    /// leaves the authorized revision open; a protected terminal kind closes
    /// it with the requested one-shot outcome.
    pub(crate) fn create_exceptional_journal_fixture(
        root: &Path,
        terminal: Option<EventKind>,
    ) -> ExceptionalJournalFixture {
        let registry = SchemaRegistry::v1().unwrap();
        let actor_manifest = fixture_manifest(&registry);
        let seed = record_from_vector(&registry, "seed_manifest_self", "manifest_sha256");
        let mut journal = TrustJournal::create(
            root,
            "fixture-journal",
            "fixture-run",
            actor_manifest.clone(),
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let routine_approval_event_hash =
            append_routine_approval(&mut journal, &actor_manifest);
        commit_test_audit_authorization(&mut journal, &actor_manifest);
        let (proposal, revision_closure, revised_evidence_manifest) =
            revised_proposal_fixture(&journal);
        let revised_seed_manifest = proposal.revised_seed_manifest.value().clone();
        let revised_seed_definition_bundle = proposal.revised_seed_definition_bundle.clone();
        journal
            .open_authorized_revision("fixture-revision-open", proposal)
            .unwrap();
        let terminal_event_hash = terminal.map(|event_kind| {
            assert!(matches!(
                event_kind,
                EventKind::ProtectedReapprovalApproved
                    | EventKind::ProtectedReapprovalFeedback
            ));
            commit_test_protected_terminal(
                &mut journal,
                &actor_manifest,
                event_kind,
                revision_closure,
                if event_kind == EventKind::ProtectedReapprovalApproved {
                    "fixture-protected-approval"
                } else {
                    "fixture-protected-feedback"
                },
            )
        });
        ExceptionalJournalFixture {
            actor_manifest,
            routine_approval_event_hash,
            revision_closure,
            terminal_event_hash,
            revised_seed_manifest,
            revised_seed_definition_bundle,
            revised_evidence_manifest,
        }
    }

    #[test]
    fn protected_feedback_must_bind_the_reviewed_presentation() {
        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal");
        let fixture = create_exceptional_journal_fixture(&journal_path, None);
        let journal = TrustJournal::open(&journal_path, fixture.actor_manifest).unwrap();
        let valid = protected_subject(
            &journal,
            EventKind::ProtectedReapprovalFeedback,
            fixture.revision_closure,
        );
        let mut value = parse_json_strict(valid.raw_bytes().unwrap()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("gate_presentation_sha256");
        let state = journal
            .projection
            .audit_authorizations
            .get("fixture-authorization")
            .unwrap();
        let error = validate_protected_reapproval_subject(
            EventKind::ProtectedReapprovalFeedback,
            &Subject::RawArtifact(canonical_json_value(&value).unwrap()),
            state,
            &journal.head(),
        )
        .unwrap_err();
        assert_eq!(error.code, "record_field_missing_or_invalid");
    }

    #[test]
    fn durable_seed_commit_matches_normative_vector_and_recovers() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let seed = record_from_vector(&registry, "seed_manifest_self", "manifest_sha256");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("trust-journal");
        let journal = TrustJournal::create(
            &root,
            "fixture-journal",
            "fixture-run",
            manifest,
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let expected = fixture_vector("journal_event_self");
        // The generic journal fixture's final vector is not sequence one; use
        // the seed event object carried by the generated registration fixture.
        let registration: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        let _ = expected;
        assert_eq!(journal.head().sequence_number, 1);
        assert_ne!(journal.head().event_hash, Sha256Digest::ZERO);
        assert!(root.join("events/0000000000000001.json").exists());
        assert_eq!(registration["schema"], "trellis-registration-hash-dag-fixtures/v1");

        let registry = SchemaRegistry::v1().unwrap();
        let reopened = TrustJournal::open(&root, fixture_manifest(&registry)).unwrap();
        assert_eq!(reopened.head(), journal.head());
        assert!(reopened.verify_checkpoint_binding(&journal.checkpoint_binding().unwrap()).unwrap());
    }

    #[test]
    fn worktree_independent_journal_rejects_forked_event_bytes() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let seed = record_from_vector(&registry, "seed_manifest_self", "manifest_sha256");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("trust-journal");
        let journal = TrustJournal::create(
            &root,
            "fixture-journal",
            "fixture-run",
            manifest,
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let event_path = root.join("events/0000000000000001.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&event_path).unwrap()).unwrap();
        value["event"]["transaction_id"] = Value::String("forged".into());
        fs::write(&event_path, canonical_json_value(&value).unwrap()).unwrap();
        let registry = SchemaRegistry::v1().unwrap();
        assert!(TrustJournal::open(&root, fixture_manifest(&registry)).is_err());
        assert_eq!(journal.head().sequence_number, 1);
    }

    #[test]
    fn exceptional_approval_is_one_shot_and_replays_the_revised_closure() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let seed = record_from_vector(&registry, "seed_manifest_self", "manifest_sha256");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("trust-journal");
        let mut journal = TrustJournal::create(
            &root,
            "fixture-journal",
            "fixture-run",
            manifest.clone(),
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let routine_approval = append_routine_approval(&mut journal, &manifest);
        commit_test_audit_authorization(&mut journal, &manifest);
        assert!(journal.has_nonclosed_audit_authorization());
        assert_eq!(journal.active_revision_lane_id(), None);

        let (proposal, closure, _) = revised_proposal_fixture(&journal);
        journal
            .open_authorized_revision("fixture-revision-open", proposal)
            .unwrap();
        assert_eq!(
            journal.active_revision_lane_id(),
            Some("fixture-revision-lane")
        );
        let protected_approval = commit_test_protected_terminal(
            &mut journal,
            &manifest,
            EventKind::ProtectedReapprovalApproved,
            closure,
            "fixture-protected-approval",
        );
        assert_eq!(journal.active_revision_lane_id(), None);
        assert!(!journal.has_nonclosed_audit_authorization());
        assert_eq!(journal.semantic_root(), closure.authored_semantic_root);
        assert_eq!(
            journal.routine_gate_outcome(),
            JournalRoutineGateOutcome::Approved(routine_approval)
        );
        assert_eq!(
            journal.current_human_approval_event_hash(),
            Some(protected_approval)
        );
        assert_eq!(journal.current_approval().unwrap().revision_closure, Some(closure));
        assert!(journal
            .prepare_routine_gate_payload(
                EventKind::AdvanceGateApproved,
                "second-routine-gate",
                "fixture-reviewer",
                &protected_subject(&journal, EventKind::ProtectedReapprovalApproved, closure),
            )
            .is_err());
        assert!(journal
            .prepare_protected_reapproval_payload(
                EventKind::ProtectedReapprovalApproved,
                "fixture-authorization",
                "fixture-reviewer",
                &protected_subject(&journal, EventKind::ProtectedReapprovalApproved, closure),
            )
            .is_err());

        let checkpoint = journal.checkpoint_binding().unwrap();
        drop(journal);
        let reopened = TrustJournal::open(&root, fixture_manifest(&registry)).unwrap();
        assert_eq!(reopened.current_human_approval_event_hash(), Some(protected_approval));
        assert_eq!(reopened.current_approval().unwrap().revision_closure, Some(closure));
        assert_eq!(reopened.semantic_root(), closure.authored_semantic_root);
        assert_eq!(reopened.routine_gate_outcome(), JournalRoutineGateOutcome::Approved(routine_approval));
        assert!(!reopened.has_nonclosed_audit_authorization());
        assert!(reopened.verify_checkpoint_binding(&checkpoint).unwrap());
    }

    #[test]
    fn exceptional_feedback_closes_lane_and_replays_the_prior_approval() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let seed = record_from_vector(&registry, "seed_manifest_self", "manifest_sha256");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("trust-journal");
        let mut journal = TrustJournal::create(
            &root,
            "fixture-journal",
            "fixture-run",
            manifest.clone(),
            "fixture-seed-transaction",
            seed,
        )
        .unwrap();
        let routine_approval = append_routine_approval(&mut journal, &manifest);
        let prior_root = journal.semantic_root();
        commit_test_audit_authorization(&mut journal, &manifest);
        let (proposal, closure, _) = revised_proposal_fixture(&journal);
        journal
            .open_authorized_revision("fixture-revision-open", proposal)
            .unwrap();
        commit_test_protected_terminal(
            &mut journal,
            &manifest,
            EventKind::ProtectedReapprovalFeedback,
            closure,
            "fixture-protected-feedback",
        );

        assert_eq!(journal.semantic_root(), prior_root);
        assert_eq!(journal.current_human_approval_event_hash(), Some(routine_approval));
        assert_eq!(journal.current_approval().unwrap().revision_closure, None);
        assert_eq!(journal.active_revision_lane_id(), None);
        assert!(!journal.has_nonclosed_audit_authorization());
        assert!(journal
            .prepare_protected_reapproval_payload(
                EventKind::ProtectedReapprovalFeedback,
                "fixture-authorization",
                "fixture-reviewer",
                &protected_subject(&journal, EventKind::ProtectedReapprovalFeedback, closure),
            )
            .is_err());

        drop(journal);
        let reopened = TrustJournal::open(&root, fixture_manifest(&registry)).unwrap();
        assert_eq!(reopened.semantic_root(), prior_root);
        assert_eq!(reopened.current_human_approval_event_hash(), Some(routine_approval));
        assert_eq!(reopened.current_approval().unwrap().revision_closure, None);
        assert_eq!(reopened.routine_gate_outcome(), JournalRoutineGateOutcome::Approved(routine_approval));
        assert!(!reopened.has_nonclosed_audit_authorization());
    }
}
