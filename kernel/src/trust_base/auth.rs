use super::canonical::{canonical_json_value, tagged_hash, DomainTag, Sha256Digest, TrustError};
use super::records::{ActorAuthenticationMethod, ActorRole, AuthoritativeRecord, EventKind, JournalHead};
use super::schema::SchemaRegistry;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct ManifestAuthorityRoots {
    roots: BTreeMap<String, VerifyingKey>,
}

impl ManifestAuthorityRoots {
    pub fn insert_hex(
        &mut self,
        authority_id: impl Into<String>,
        public_key_hex: &str,
    ) -> Result<(), TrustError> {
        let key = verifying_key_from_hex(public_key_hex)?;
        if self.roots.insert(authority_id.into(), key).is_some() {
            return Err(TrustError::new(
                "duplicate_manifest_authority",
                "manifest authority IDs must be unique",
            ));
        }
        Ok(())
    }

    fn get(&self, authority_id: &str) -> Option<&VerifyingKey> {
        self.roots.get(authority_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManifestActorRole {
    Reviewer,
    AuditAuthority,
    JournalAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeyPurpose {
    GateReview,
    AuditAuthorization,
    JournalCommit,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyEntryWire {
    key_id: String,
    actor_role: ManifestActorRole,
    actor_identity: String,
    purpose: KeyPurpose,
    algorithm: String,
    public_key_ed25519_hex: String,
    valid_from_sequence: u64,
    valid_through_sequence: Option<u64>,
}

#[derive(Clone, Debug)]
struct ActorKey {
    key_id: String,
    actor_role: ManifestActorRole,
    actor_identity: String,
    purpose: KeyPurpose,
    verifying_key: VerifyingKey,
    valid_from_sequence: u64,
    valid_through_sequence: Option<u64>,
}

impl ActorKey {
    fn valid_at(&self, sequence: u64) -> bool {
        sequence >= self.valid_from_sequence
            && self
                .valid_through_sequence
                .is_none_or(|through| sequence <= through)
    }

    fn interval_overlaps(&self, other: &Self) -> bool {
        let self_end = self.valid_through_sequence.unwrap_or(u64::MAX);
        let other_end = other.valid_through_sequence.unwrap_or(u64::MAX);
        self.valid_from_sequence <= other_end && other.valid_from_sequence <= self_end
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyManifestWire {
    schema: String,
    protocol_id: String,
    key_history_policy: String,
    manifest_authority_id: String,
    keys: Vec<KeyEntryWire>,
    authorization_signing_digest_sha256: Sha256Digest,
    authorization_signature_ed25519_hex: String,
    manifest_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct ActorKeyManifest {
    record: AuthoritativeRecord,
    authority_id: String,
    keys: BTreeMap<String, ActorKey>,
}

impl ActorKeyManifest {
    pub fn verify(
        registry: &SchemaRegistry,
        value: Value,
        roots: &ManifestAuthorityRoots,
    ) -> Result<Self, TrustError> {
        let record = AuthoritativeRecord::parse(registry, value.clone())?;
        if record.contract().record_schema != "trellis-actor-authentication-key-manifest/v1" {
            return Err(TrustError::new(
                "wrong_key_manifest_schema",
                "expected actor authentication key manifest",
            ));
        }
        let wire: KeyManifestWire = serde_json::from_value(value.clone()).map_err(|error| {
            TrustError::new("key_manifest_decode_failed", error.to_string())
        })?;
        if wire.schema != "trellis-actor-authentication-key-manifest/v1"
            || wire.protocol_id != "trellis-trust-v1"
            || wire.key_history_policy != "seed_pinned_immutable_all_epochs_v1"
            || wire.manifest_sha256 != record.digest()
        {
            return Err(TrustError::new(
                "key_manifest_identity_mismatch",
                "manifest identity or immutable-history policy is invalid",
            ));
        }
        let mut authorization_value = value;
        let object = authorization_value.as_object_mut().ok_or_else(|| {
            TrustError::new("key_manifest_not_object", "key manifest must be an object")
        })?;
        for field in [
            "authorization_signing_digest_sha256",
            "authorization_signature_ed25519_hex",
            "manifest_sha256",
        ] {
            object.remove(field).ok_or_else(|| {
                TrustError::new("key_manifest_field_missing", format!("missing {field}"))
            })?;
        }
        let signing_digest = tagged_hash(
            DomainTag::ActorAuthenticationKeyManifestAuthorization,
            &canonical_json_value(&authorization_value)?,
        );
        if signing_digest != wire.authorization_signing_digest_sha256 {
            return Err(TrustError::new(
                "key_manifest_signing_digest_mismatch",
                "manifest authorization signing digest is incorrect",
            ));
        }
        let root = roots.get(&wire.manifest_authority_id).ok_or_else(|| {
            TrustError::new(
                "untrusted_manifest_authority",
                format!("{} is not installed in the verifier", wire.manifest_authority_id),
            )
        })?;
        verify_signature(
            root,
            signing_digest,
            &wire.authorization_signature_ed25519_hex,
            "manifest_authorization_signature_invalid",
        )?;

        let mut keys = BTreeMap::new();
        for entry in wire.keys {
            if entry.algorithm != "Ed25519" {
                return Err(TrustError::new(
                    "unsupported_actor_key_algorithm",
                    format!("key {} is not Ed25519", entry.key_id),
                ));
            }
            if entry
                .valid_through_sequence
                .is_some_and(|through| through < entry.valid_from_sequence)
            {
                return Err(TrustError::new(
                    "invalid_actor_key_interval",
                    format!("key {} has a reversed validity interval", entry.key_id),
                ));
            }
            let expected_purpose = match entry.actor_role {
                ManifestActorRole::Reviewer => KeyPurpose::GateReview,
                ManifestActorRole::AuditAuthority => KeyPurpose::AuditAuthorization,
                ManifestActorRole::JournalAuthority => KeyPurpose::JournalCommit,
            };
            if entry.purpose != expected_purpose {
                return Err(TrustError::new(
                    "actor_key_wrong_purpose",
                    format!("key {} has the wrong purpose", entry.key_id),
                ));
            }
            let key = ActorKey {
                key_id: entry.key_id.clone(),
                actor_role: entry.actor_role,
                actor_identity: entry.actor_identity,
                purpose: entry.purpose,
                verifying_key: verifying_key_from_hex(&entry.public_key_ed25519_hex)?,
                valid_from_sequence: entry.valid_from_sequence,
                valid_through_sequence: entry.valid_through_sequence,
            };
            if keys.insert(entry.key_id.clone(), key).is_some() {
                return Err(TrustError::new(
                    "duplicate_actor_key_id",
                    format!("duplicate actor key {}", entry.key_id),
                ));
            }
        }
        let key_values: Vec<&ActorKey> = keys.values().collect();
        for (index, left) in key_values.iter().enumerate() {
            for right in &key_values[index + 1..] {
                if left.actor_role == right.actor_role
                    && left.actor_identity == right.actor_identity
                    && left.purpose == right.purpose
                    && left.interval_overlaps(right)
                {
                    return Err(TrustError::new(
                        "overlapping_actor_key_epochs",
                        format!("keys {} and {} overlap", left.key_id, right.key_id),
                    ));
                }
            }
        }
        let roles: BTreeSet<_> = keys.values().map(|key| key.actor_role).collect();
        if ![
            ManifestActorRole::Reviewer,
            ManifestActorRole::AuditAuthority,
            ManifestActorRole::JournalAuthority,
        ]
        .iter()
        .all(|role| roles.contains(role))
        {
            return Err(TrustError::new(
                "actor_key_roles_incomplete",
                "manifest needs distinct reviewer, audit, and journal-authority keys",
            ));
        }
        Ok(Self {
            record,
            authority_id: wire.manifest_authority_id,
            keys,
        })
    }

    pub fn digest(&self) -> Sha256Digest {
        self.record.digest()
    }

    pub fn authority_id(&self) -> &str {
        &self.authority_id
    }

    pub fn value(&self) -> &Value {
        self.record.value()
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, TrustError> {
        canonical_json_value(self.record.value())
    }

    fn key(&self, key_id: &str) -> Option<&ActorKey> {
        self.keys.get(key_id)
    }

    pub(crate) fn journal_key(
        &self,
        key_id: &str,
        actor_identity: &str,
        sequence: u64,
    ) -> Result<&VerifyingKey, TrustError> {
        let key = self.key(key_id).ok_or_else(|| {
            TrustError::new("unregistered_actor_key", format!("unknown key {key_id}"))
        })?;
        if key.actor_role != ManifestActorRole::JournalAuthority
            || key.purpose != KeyPurpose::JournalCommit
            || key.actor_identity != actor_identity
            || !key.valid_at(sequence)
        {
            return Err(TrustError::new(
                "journal_key_not_authorized",
                "journal commit key has the wrong identity, purpose, or epoch",
            ));
        }
        Ok(&key.verifying_key)
    }
}

#[derive(Clone, Debug)]
pub struct ReceiptContext<'a> {
    pub journal_id: &'a str,
    pub run_id: &'a str,
    pub transaction_id: &'a str,
    pub event_kind: EventKind,
    pub event_payload_sha256: Sha256Digest,
    pub predecessor_head: &'a JournalHead,
    pub actor_role: ActorRole,
    pub actor_identity: &'a str,
    pub gate_or_revision_lane_id: &'a str,
}

#[derive(Clone, Debug)]
pub struct VerifiedActorReceipt {
    record: AuthoritativeRecord,
    key_id: String,
}

impl VerifiedActorReceipt {
    pub fn digest(&self) -> Sha256Digest {
        self.record.digest()
    }

    pub fn value(&self) -> &Value {
        self.record.value()
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptWire {
    schema: String,
    authentication_method: ActorAuthenticationMethod,
    journal_id: String,
    run_id: String,
    transaction_id: String,
    event_kind: EventKind,
    event_payload_sha256: Sha256Digest,
    predecessor_head: JournalHead,
    actor_role: ActorRole,
    actor_identity: String,
    key_id: String,
    gate_or_revision_lane_id: String,
    signing_digest_sha256: Sha256Digest,
    signature_ed25519_hex: String,
    receipt_sha256: Sha256Digest,
}

pub fn verify_actor_receipt(
    registry: &SchemaRegistry,
    manifest: &ActorKeyManifest,
    value: Value,
    context: &ReceiptContext<'_>,
) -> Result<VerifiedActorReceipt, TrustError> {
    let record = AuthoritativeRecord::parse(registry, value.clone())?;
    if record.contract().record_schema != "trellis-actor-authentication-receipt/v1" {
        return Err(TrustError::new(
            "wrong_actor_receipt_schema",
            "expected actor authentication receipt",
        ));
    }
    let wire: ReceiptWire = serde_json::from_value(value.clone()).map_err(|error| {
        TrustError::new("actor_receipt_decode_failed", error.to_string())
    })?;
    if wire.schema != "trellis-actor-authentication-receipt/v1"
        || wire.receipt_sha256 != record.digest()
        || wire.journal_id != context.journal_id
        || wire.run_id != context.run_id
        || wire.transaction_id != context.transaction_id
        || wire.event_kind != context.event_kind
        || wire.event_payload_sha256 != context.event_payload_sha256
        || wire.predecessor_head != *context.predecessor_head
        || wire.actor_role != context.actor_role
        || wire.actor_identity != context.actor_identity
        || wire.gate_or_revision_lane_id != context.gate_or_revision_lane_id
    {
        return Err(TrustError::new(
            "actor_receipt_context_mismatch",
            "receipt does not bind the exact journal event context",
        ));
    }
    let (expected_method, expected_manifest_role, expected_purpose) = match context.actor_role {
        ActorRole::Reviewer => (
            ActorAuthenticationMethod::AuthenticatedGateReceipt,
            ManifestActorRole::Reviewer,
            KeyPurpose::GateReview,
        ),
        ActorRole::AuditAuthority => (
            ActorAuthenticationMethod::AuditSignatureV1,
            ManifestActorRole::AuditAuthority,
            KeyPurpose::AuditAuthorization,
        ),
        ActorRole::Kernel => {
            return Err(TrustError::new(
                "kernel_receipt_forbidden",
                "kernel_internal events do not carry actor receipts",
            ))
        }
    };
    if wire.authentication_method != expected_method {
        return Err(TrustError::new(
            "actor_receipt_method_mismatch",
            "receipt authentication method is wrong for actor role",
        ));
    }
    let key = manifest.key(&wire.key_id).ok_or_else(|| {
        TrustError::new("unregistered_actor_key", format!("unknown key {}", wire.key_id))
    })?;
    let event_sequence = context.predecessor_head.sequence_number.checked_add(1).ok_or_else(|| {
        TrustError::new("journal_sequence_overflow", "event sequence cannot be represented")
    })?;
    if key.actor_role != expected_manifest_role
        || key.purpose != expected_purpose
        || key.actor_identity != wire.actor_identity
        || !key.valid_at(event_sequence)
    {
        return Err(TrustError::new(
            "actor_key_not_authorized",
            "receipt key has the wrong role, identity, purpose, or epoch",
        ));
    }
    let mut signing_value = value;
    let object = signing_value.as_object_mut().ok_or_else(|| {
        TrustError::new("actor_receipt_not_object", "actor receipt must be an object")
    })?;
    for field in ["signing_digest_sha256", "signature_ed25519_hex", "receipt_sha256"] {
        object.remove(field).ok_or_else(|| {
            TrustError::new("actor_receipt_field_missing", format!("missing {field}"))
        })?;
    }
    let signing_digest = tagged_hash(
        DomainTag::ActorAuthenticationSigning,
        &canonical_json_value(&signing_value)?,
    );
    if signing_digest != wire.signing_digest_sha256 {
        return Err(TrustError::new(
            "actor_receipt_signing_digest_mismatch",
            "receipt signing digest is incorrect",
        ));
    }
    verify_signature(
        &key.verifying_key,
        signing_digest,
        &wire.signature_ed25519_hex,
        "actor_receipt_signature_invalid",
    )?;
    Ok(VerifiedActorReceipt {
        record,
        key_id: wire.key_id,
    })
}

pub fn sign_actor_receipt(
    registry: &SchemaRegistry,
    manifest: &ActorKeyManifest,
    context: &ReceiptContext<'_>,
    key_id: &str,
    signing_key: &SigningKey,
) -> Result<Value, TrustError> {
    let (method, manifest_role, purpose) = match context.actor_role {
        ActorRole::Reviewer => (
            ActorAuthenticationMethod::AuthenticatedGateReceipt,
            ManifestActorRole::Reviewer,
            KeyPurpose::GateReview,
        ),
        ActorRole::AuditAuthority => (
            ActorAuthenticationMethod::AuditSignatureV1,
            ManifestActorRole::AuditAuthority,
            KeyPurpose::AuditAuthorization,
        ),
        ActorRole::Kernel => {
            return Err(TrustError::new(
                "kernel_receipt_forbidden",
                "kernel_internal events do not carry actor receipts",
            ))
        }
    };
    let sequence = context
        .predecessor_head
        .sequence_number
        .checked_add(1)
        .ok_or_else(|| TrustError::new("journal_sequence_overflow", "event overflow"))?;
    let key = manifest.key(key_id).ok_or_else(|| {
        TrustError::new("unregistered_actor_key", format!("unknown key {key_id}"))
    })?;
    if key.actor_role != manifest_role
        || key.purpose != purpose
        || key.actor_identity != context.actor_identity
        || !key.valid_at(sequence)
        || key.verifying_key != signing_key.verifying_key()
    {
        return Err(TrustError::new(
            "actor_signing_key_not_authorized",
            "private key does not match the manifest role, identity, purpose, or epoch",
        ));
    }
    let mut value = serde_json::json!({
        "schema": "trellis-actor-authentication-receipt/v1",
        "authentication_method": method,
        "journal_id": context.journal_id,
        "run_id": context.run_id,
        "transaction_id": context.transaction_id,
        "event_kind": context.event_kind,
        "event_payload_sha256": context.event_payload_sha256,
        "predecessor_head": context.predecessor_head,
        "actor_role": context.actor_role,
        "actor_identity": context.actor_identity,
        "key_id": key_id,
        "gate_or_revision_lane_id": context.gate_or_revision_lane_id,
    });
    let signing_digest = tagged_hash(
        DomainTag::ActorAuthenticationSigning,
        &canonical_json_value(&value)?,
    );
    let signature = signing_key.sign(signing_digest.as_bytes());
    {
        let object = value.as_object_mut().expect("json object");
        object.insert(
            "signing_digest_sha256".into(),
            Value::String(signing_digest.to_string()),
        );
        object.insert(
            "signature_ed25519_hex".into(),
            Value::String(hex_lower(&signature.to_bytes())),
        );
        object.insert(
            "receipt_sha256".into(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
    }
    let receipt_digest = super::canonical::self_digest(
        DomainTag::ActorAuthenticationReceipt,
        &value,
        "receipt_sha256",
    )?;
    value.as_object_mut().expect("json object").insert(
        "receipt_sha256".into(),
        Value::String(receipt_digest.to_string()),
    );
    registry.validate_record(&value)?;
    verify_actor_receipt(registry, manifest, value.clone(), context)?;
    Ok(value)
}

pub(crate) fn verify_signature(
    key: &VerifyingKey,
    digest: Sha256Digest,
    signature_hex: &str,
    error_code: &'static str,
) -> Result<(), TrustError> {
    let bytes = decode_hex_exact::<64>(signature_hex, "invalid_ed25519_signature_encoding")?;
    let signature = Signature::from_bytes(&bytes);
    key.verify(digest.as_bytes(), &signature)
        .map_err(|_| TrustError::new(error_code, "Ed25519 signature verification failed"))
}

fn verifying_key_from_hex(value: &str) -> Result<VerifyingKey, TrustError> {
    let bytes = decode_hex_exact::<32>(value, "invalid_ed25519_public_key_encoding")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| {
        TrustError::new(
            "invalid_ed25519_public_key",
            "public key does not encode a valid Ed25519 point",
        )
    })
}

pub(crate) fn decode_hex_exact<const N: usize>(
    value: &str,
    code: &'static str,
) -> Result<[u8; N], TrustError> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TrustError::new(
            code,
            format!("expected {} lowercase hexadecimal characters", N * 2),
        ));
    }
    let mut output = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        output[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(output)
}

#[derive(Clone, Debug)]
pub struct JournalCommitReceiptContext<'a> {
    pub journal_id: &'a str,
    pub run_id: &'a str,
    pub package_transaction_id: &'a str,
    pub package_event_hash: Sha256Digest,
    pub package_event_payload_sha256: Sha256Digest,
    pub predecessor_head: &'a JournalHead,
    pub committed_head: &'a JournalHead,
    pub package_presentation_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct VerifiedJournalCommitReceipt {
    record: AuthoritativeRecord,
    pub journal_authority_identity: String,
    pub key_id: String,
}

impl VerifiedJournalCommitReceipt {
    pub fn digest(&self) -> Sha256Digest {
        self.record.digest()
    }

    pub fn value(&self) -> &Value {
        self.record.value()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalCommitReceiptWire {
    schema: String,
    authentication_method: String,
    journal_id: String,
    run_id: String,
    package_transaction_id: String,
    package_event_hash: Sha256Digest,
    package_event_payload_sha256: Sha256Digest,
    predecessor_head: JournalHead,
    committed_head: JournalHead,
    package_presentation_sha256: Sha256Digest,
    journal_authority_identity: String,
    key_id: String,
    commit_stage: String,
    signing_digest_sha256: Sha256Digest,
    signature_ed25519_hex: String,
    receipt_sha256: Sha256Digest,
}

pub fn verify_journal_commit_receipt(
    registry: &SchemaRegistry,
    manifest: &ActorKeyManifest,
    value: Value,
    context: &JournalCommitReceiptContext<'_>,
) -> Result<VerifiedJournalCommitReceipt, TrustError> {
    let record = AuthoritativeRecord::parse(registry, value.clone())?;
    if record.contract().record_schema != "trellis-journal-commit-receipt/v1" {
        return Err(TrustError::new(
            "wrong_journal_commit_receipt_schema",
            "expected journal commit receipt",
        ));
    }
    let wire: JournalCommitReceiptWire = serde_json::from_value(value.clone()).map_err(|error| {
        TrustError::new("journal_commit_receipt_decode_failed", error.to_string())
    })?;
    if wire.schema != "trellis-journal-commit-receipt/v1"
        || wire.authentication_method != "journal_commit_signature_v1"
        || wire.commit_stage != "after_durable_head_install_v1"
        || wire.receipt_sha256 != record.digest()
        || wire.journal_id != context.journal_id
        || wire.run_id != context.run_id
        || wire.package_transaction_id != context.package_transaction_id
        || wire.package_event_hash != context.package_event_hash
        || wire.package_event_payload_sha256 != context.package_event_payload_sha256
        || wire.predecessor_head != *context.predecessor_head
        || wire.committed_head != *context.committed_head
        || wire.package_presentation_sha256 != context.package_presentation_sha256
        || wire.committed_head.sequence_number
            != wire.predecessor_head.sequence_number.checked_add(1).ok_or_else(|| {
                TrustError::new("journal_sequence_overflow", "commit receipt sequence overflow")
            })?
        || wire.committed_head.event_hash != wire.package_event_hash
    {
        return Err(TrustError::new(
            "journal_commit_receipt_context_mismatch",
            "commit receipt does not bind the exact post-durable package event",
        ));
    }
    let mut signing_value = value;
    let object = signing_value.as_object_mut().ok_or_else(|| {
        TrustError::new(
            "journal_commit_receipt_not_object",
            "journal commit receipt must be an object",
        )
    })?;
    for field in ["signing_digest_sha256", "signature_ed25519_hex", "receipt_sha256"] {
        object.remove(field).ok_or_else(|| {
            TrustError::new(
                "journal_commit_receipt_field_missing",
                format!("missing {field}"),
            )
        })?;
    }
    let signing_digest = tagged_hash(
        DomainTag::JournalCommitSigning,
        &canonical_json_value(&signing_value)?,
    );
    if signing_digest != wire.signing_digest_sha256 {
        return Err(TrustError::new(
            "journal_commit_signing_digest_mismatch",
            "journal commit receipt signing digest is incorrect",
        ));
    }
    let key = manifest.journal_key(
        &wire.key_id,
        &wire.journal_authority_identity,
        wire.committed_head.sequence_number,
    )?;
    verify_signature(
        key,
        signing_digest,
        &wire.signature_ed25519_hex,
        "journal_commit_signature_invalid",
    )?;
    Ok(VerifiedJournalCommitReceipt {
        record,
        journal_authority_identity: wire.journal_authority_identity,
        key_id: wire.key_id,
    })
}

pub fn sign_journal_commit_receipt(
    registry: &SchemaRegistry,
    manifest: &ActorKeyManifest,
    context: &JournalCommitReceiptContext<'_>,
    journal_authority_identity: &str,
    key_id: &str,
    signing_key: &SigningKey,
) -> Result<Value, TrustError> {
    let authorized = manifest.journal_key(
        key_id,
        journal_authority_identity,
        context.committed_head.sequence_number,
    )?;
    if *authorized != signing_key.verifying_key() {
        return Err(TrustError::new(
            "journal_signing_key_mismatch",
            "private journal authority key does not match the seed-pinned manifest",
        ));
    }
    let mut value = serde_json::json!({
        "schema": "trellis-journal-commit-receipt/v1",
        "authentication_method": "journal_commit_signature_v1",
        "journal_id": context.journal_id,
        "run_id": context.run_id,
        "package_transaction_id": context.package_transaction_id,
        "package_event_hash": context.package_event_hash,
        "package_event_payload_sha256": context.package_event_payload_sha256,
        "predecessor_head": context.predecessor_head,
        "committed_head": context.committed_head,
        "package_presentation_sha256": context.package_presentation_sha256,
        "journal_authority_identity": journal_authority_identity,
        "key_id": key_id,
        "commit_stage": "after_durable_head_install_v1",
    });
    let signing_digest = tagged_hash(
        DomainTag::JournalCommitSigning,
        &canonical_json_value(&value)?,
    );
    let signature = signing_key.sign(signing_digest.as_bytes());
    {
        let object = value.as_object_mut().expect("json object");
        object.insert(
            "signing_digest_sha256".into(),
            Value::String(signing_digest.to_string()),
        );
        object.insert(
            "signature_ed25519_hex".into(),
            Value::String(hex_lower(&signature.to_bytes())),
        );
        object.insert(
            "receipt_sha256".into(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
    }
    let receipt_digest = super::canonical::self_digest(
        DomainTag::JournalCommitReceipt,
        &value,
        "receipt_sha256",
    )?;
    value.as_object_mut().expect("json object").insert(
        "receipt_sha256".into(),
        Value::String(receipt_digest.to_string()),
    );
    registry.validate_record(&value)?;
    verify_journal_commit_receipt(registry, manifest, value.clone(), context)?;
    Ok(value)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated hexadecimal input"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn vector(name: &str) -> Value {
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
        let signing = vector("actor_key_manifest_self");
        let mut value = signing["payload"].clone();
        value.as_object_mut().unwrap().insert(
            "manifest_sha256".into(),
            signing["expected_sha256"].clone(),
        );
        let root_signing = SigningKey::from_bytes(&[3_u8; 32]);
        let mut roots = ManifestAuthorityRoots::default();
        roots
            .insert_hex(
                "fixture-v1-manifest-root",
                &hex(&root_signing.verifying_key().to_bytes()),
            )
            .unwrap();
        ActorKeyManifest::verify(registry, value, &roots).unwrap()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn root_authorized_manifest_and_actor_receipt_verify() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        assert_eq!(manifest.authority_id(), "fixture-v1-manifest-root");

        let signing = vector("actor_receipt_self");
        let mut receipt = signing["payload"].clone();
        receipt.as_object_mut().unwrap().insert(
            "receipt_sha256".into(),
            signing["expected_sha256"].clone(),
        );
        let wire: ReceiptWire = serde_json::from_value(receipt.clone()).unwrap();
        let context = ReceiptContext {
            journal_id: &wire.journal_id,
            run_id: &wire.run_id,
            transaction_id: &wire.transaction_id,
            event_kind: wire.event_kind,
            event_payload_sha256: wire.event_payload_sha256,
            predecessor_head: &wire.predecessor_head,
            actor_role: wire.actor_role,
            actor_identity: &wire.actor_identity,
            gate_or_revision_lane_id: &wire.gate_or_revision_lane_id,
        };
        let verified = verify_actor_receipt(&registry, &manifest, receipt, &context).unwrap();
        assert_eq!(verified.digest(), wire.receipt_sha256);
    }

    #[test]
    fn receipt_signature_cannot_be_reused_at_another_head() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let signing = vector("actor_receipt_self");
        let mut receipt = signing["payload"].clone();
        receipt.as_object_mut().unwrap().insert(
            "receipt_sha256".into(),
            signing["expected_sha256"].clone(),
        );
        let wire: ReceiptWire = serde_json::from_value(receipt.clone()).unwrap();
        let wrong_head = JournalHead {
            sequence_number: wire.predecessor_head.sequence_number + 1,
            ..wire.predecessor_head.clone()
        };
        let context = ReceiptContext {
            journal_id: &wire.journal_id,
            run_id: &wire.run_id,
            transaction_id: &wire.transaction_id,
            event_kind: wire.event_kind,
            event_payload_sha256: wire.event_payload_sha256,
            predecessor_head: &wrong_head,
            actor_role: wire.actor_role,
            actor_identity: &wire.actor_identity,
            gate_or_revision_lane_id: &wire.gate_or_revision_lane_id,
        };
        assert!(verify_actor_receipt(&registry, &manifest, receipt, &context).is_err());
    }

    #[test]
    fn post_durable_journal_commit_receipt_verifies() {
        let registry = SchemaRegistry::v1().unwrap();
        let manifest = fixture_manifest(&registry);
        let vector = vector("journal_commit_receipt_self");
        let mut receipt = vector["payload"].clone();
        receipt.as_object_mut().unwrap().insert(
            "receipt_sha256".into(),
            vector["expected_sha256"].clone(),
        );
        let wire: JournalCommitReceiptWire =
            serde_json::from_value(receipt.clone()).unwrap();
        let context = JournalCommitReceiptContext {
            journal_id: &wire.journal_id,
            run_id: &wire.run_id,
            package_transaction_id: &wire.package_transaction_id,
            package_event_hash: wire.package_event_hash,
            package_event_payload_sha256: wire.package_event_payload_sha256,
            predecessor_head: &wire.predecessor_head,
            committed_head: &wire.committed_head,
            package_presentation_sha256: wire.package_presentation_sha256,
        };
        let verified =
            verify_journal_commit_receipt(&registry, &manifest, receipt, &context).unwrap();
        assert_eq!(verified.digest(), wire.receipt_sha256);
        assert_eq!(verified.journal_authority_identity, "fixture-journal-authority");
    }
}
