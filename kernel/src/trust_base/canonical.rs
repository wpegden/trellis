use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

const HASH_PREFIX: &[u8] = b"trellis-trust-v1\0";
const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustError {
    pub code: &'static str,
    pub detail: String,
}

impl TrustError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for TrustError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub const ZERO: Self = Self([0; 32]);

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Sha256Digest {
    type Err = TrustError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(TrustError::new(
                "invalid_sha256",
                "SHA-256 digests must be 64 lowercase hexadecimal characters",
            ));
        }
        let mut bytes = [0_u8; 32];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

fn hex_nibble(byte: u8) -> Result<u8, TrustError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(TrustError::new("invalid_hex", "invalid hexadecimal digit")),
    }
}

/// Canonical arbitrary-sized natural used for load-bearing resource values.
///
/// Arithmetic is deliberately not implicit. Comparisons operate on the
/// canonical decimal representation, so no host-width truncation is possible.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DecimalNatural(String);

impl DecimalNatural {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == "0"
    }
}

impl FromStr for DecimalNatural {
    type Err = TrustError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = value == "0"
            || (!value.starts_with('0')
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && !value.is_empty());
        if !valid {
            return Err(TrustError::new(
                "invalid_decimal_natural",
                "natural must match 0|[1-9][0-9]*",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl Ord for DecimalNatural {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for DecimalNatural {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for DecimalNatural {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for DecimalNatural {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DecimalNatural {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Closed v1 domain-tag registry. Callers cannot supply arbitrary tags.
///
/// Stage 3 (plan doc 32, Q1): the journal/auth/qualification families were
/// pruned with the machinery that consumed them; every tag still used by
/// approval/gate/archive/seed hashing survives, and `TrustDecision` (the
/// `TrustRecord` digest domain) is added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DomainTag {
    SourceClaimLineage,
    SourceValidationContract,
    SourceValidationAttempt,
    SourceValidationOutcome,
    FormalRefutation,
    SeedAuthoredDefinitionManifest,
    HumanApproval,
    TrustDecision,
    RawArtifact,
    ManifestNode,
    GitCheckoutRoot,
    ReadonlyClosureRoot,
    AuthoredSemanticRoot,
    EvidenceToolRoot,
    GatePresentation,
    PackagePresentation,
    TargetDefinition,
    ConditionalizationSchema,
    SourceInterpretation,
    PreconditionDefinition,
    CarrierRefinementDefinition,
    BuildDefinition,
    SemanticValidator,
    EvidenceToolInput,
    /// Stage 4 (plan doc 32): one per-target seed-contract registry entry.
    SeedTargetContract,
    /// Stage 4: the root over the complete serialized seed-contract registry
    /// (embedded in the launch acknowledgment and stored in state).
    SeedContractRegistryRoot,
    /// Stage 4: one adaptation-ledger row (seed rows are bundle definitions).
    AdaptationLedgerRow,
}

impl DomainTag {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceClaimLineage => "source-claim-lineage",
            Self::SourceValidationContract => "source-validation-contract",
            Self::SourceValidationAttempt => "source-validation-attempt",
            Self::SourceValidationOutcome => "source-validation-outcome",
            Self::FormalRefutation => "formal-refutation",
            Self::SeedAuthoredDefinitionManifest => "seed-authored-definition-manifest",
            Self::HumanApproval => "human-approval",
            Self::TrustDecision => "trust-decision",
            Self::RawArtifact => "raw-artifact",
            Self::ManifestNode => "manifest-node",
            Self::GitCheckoutRoot => "git-checkout-root",
            Self::ReadonlyClosureRoot => "readonly-closure-root",
            Self::AuthoredSemanticRoot => "authored-semantic-root",
            Self::EvidenceToolRoot => "evidence-tool-root",
            Self::GatePresentation => "gate-presentation",
            Self::PackagePresentation => "package-presentation",
            Self::TargetDefinition => "target-definition",
            Self::ConditionalizationSchema => "conditionalization-schema",
            Self::SourceInterpretation => "source-interpretation",
            Self::PreconditionDefinition => "precondition-definition",
            Self::CarrierRefinementDefinition => "carrier-refinement-definition",
            Self::BuildDefinition => "build-definition",
            Self::SemanticValidator => "semantic-validator",
            Self::EvidenceToolInput => "evidence-tool-input",
            Self::SeedTargetContract => "seed-target-contract",
            Self::SeedContractRegistryRoot => "seed-contract-registry-root",
            Self::AdaptationLedgerRow => "adaptation-ledger-row",
        }
    }

    pub fn parse_registered(value: &str) -> Result<Self, TrustError> {
        ALL_DOMAIN_TAGS
            .iter()
            .copied()
            .find(|tag| tag.as_str() == value)
            .ok_or_else(|| {
                TrustError::new(
                    "unregistered_domain_tag",
                    format!("{value:?} is not in the v1 domain-tag registry"),
                )
            })
    }
}

const ALL_DOMAIN_TAGS: &[DomainTag] = &[
    DomainTag::SourceClaimLineage,
    DomainTag::SourceValidationContract,
    DomainTag::SourceValidationAttempt,
    DomainTag::SourceValidationOutcome,
    DomainTag::FormalRefutation,
    DomainTag::SeedAuthoredDefinitionManifest,
    DomainTag::HumanApproval,
    DomainTag::TrustDecision,
    DomainTag::RawArtifact,
    DomainTag::ManifestNode,
    DomainTag::AuthoredSemanticRoot,
    DomainTag::EvidenceToolRoot,
    DomainTag::GatePresentation,
    DomainTag::PackagePresentation,
    DomainTag::TargetDefinition,
    DomainTag::ConditionalizationSchema,
    DomainTag::SourceInterpretation,
    DomainTag::PreconditionDefinition,
    DomainTag::CarrierRefinementDefinition,
    DomainTag::BuildDefinition,
    DomainTag::SemanticValidator,
    DomainTag::EvidenceToolInput,
    DomainTag::SeedTargetContract,
    DomainTag::SeedContractRegistryRoot,
    DomainTag::AdaptationLedgerRow,
];

pub fn raw_sha256(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::from_bytes(Sha256::digest(bytes).into())
}

/// Parse authoritative JSON without serde_json's normal last-key-wins
/// behavior.  Duplicate names are rejected at every nesting depth before a
/// `Value` exists, so canonicalization cannot accidentally bless ambiguous
/// input bytes.
pub fn parse_json_strict(bytes: &[u8]) -> Result<Value, TrustError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJsonValue::deserialize(&mut deserializer)
        .map_err(|error| TrustError::new("json_parse_failed", error.to_string()))?
        .0;
    deserializer
        .end()
        .map_err(|error| TrustError::new("json_trailing_data", error.to_string()))?;
    Ok(value)
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> de::Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value with no duplicate object names")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictJsonValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJsonValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(StrictJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object name {key:?}"
                )));
            }
            let value = object.next_value::<StrictJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(StrictJsonValue(Value::Object(values)))
    }
}

pub fn tagged_hash(tag: DomainTag, payload: &[u8]) -> Sha256Digest {
    let tag = tag.as_str().as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(HASH_PREFIX);
    hasher.update((tag.len() as u32).to_be_bytes());
    hasher.update(tag);
    hasher.update((payload.len() as u64).to_be_bytes());
    hasher.update(payload);
    Sha256Digest::from_bytes(hasher.finalize().into())
}

pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, TrustError> {
    let value = serde_json::to_value(value).map_err(|error| {
        TrustError::new("canonical_serialization_failed", error.to_string())
    })?;
    canonical_json_value(&value)
}

pub fn canonical_json_value(value: &Value) -> Result<Vec<u8>, TrustError> {
    let mut output = Vec::new();
    write_canonical(value, &mut output)?;
    Ok(output)
}

pub fn self_digest(
    tag: DomainTag,
    record: &Value,
    self_field: &str,
) -> Result<Sha256Digest, TrustError> {
    let mut stripped = record.clone();
    let object = stripped.as_object_mut().ok_or_else(|| {
        TrustError::new("record_not_object", "a self-digested record must be an object")
    })?;
    if object.remove(self_field).is_none() {
        return Err(TrustError::new(
            "self_digest_field_missing",
            format!("record does not contain {self_field}"),
        ));
    }
    Ok(tagged_hash(tag, &canonical_json_value(&stripped)?))
}

pub fn verify_self_digest(
    tag: DomainTag,
    record: &Value,
    self_field: &str,
) -> Result<Sha256Digest, TrustError> {
    let embedded = record
        .get(self_field)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "self_digest_field_invalid",
                format!("{self_field} must be a SHA-256 string"),
            )
        })?
        .parse::<Sha256Digest>()?;
    let computed = self_digest(tag, record, self_field)?;
    if embedded != computed {
        return Err(TrustError::new(
            "self_digest_mismatch",
            format!("{self_field}: embedded {embedded}, computed {computed}"),
        ));
    }
    Ok(computed)
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), TrustError> {
    write_canonical_at(value, output, "$")
}

fn write_canonical_at(
    value: &Value,
    output: &mut Vec<u8>,
    path: &str,
) -> Result<(), TrustError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::String(string) => {
            let encoded = serde_json::to_string(string).map_err(|error| {
                TrustError::new("canonical_string_failed", error.to_string())
            })?;
            output.extend_from_slice(encoded.as_bytes());
        }
        Value::Number(number) => {
            let integer = number.as_u64().ok_or_else(|| {
                TrustError::new(
                    "noncanonical_json_number",
                    format!(
                        "authoritative JSON permits only nonnegative integers; found {number} at {path}"
                    ),
                )
            })?;
            if integer > MAX_SAFE_JSON_INTEGER {
                return Err(TrustError::new(
                    "unsafe_json_integer",
                    format!(
                        "JSON integers must be at most 2^53-1; use a decimal string at {path}"
                    ),
                ));
            }
            output.extend_from_slice(integer.to_string().as_bytes());
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, item) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_at(item, output, &format!("{path}[{index}]"))?;
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            let mut entries: Vec<_> = object.iter().collect();
            // RFC 8785 sorts property names as arrays of UTF-16 code units.
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                let encoded = serde_json::to_string(key).map_err(|error| {
                    TrustError::new("canonical_key_failed", error.to_string())
                })?;
                output.extend_from_slice(encoded.as_bytes());
                output.push(b':');
                write_canonical_at(item, output, &format!("{path}.{key}"))?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

pub fn validate_relative_posix_path(path: &str) -> Result<(), TrustError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(TrustError::new(
            "invalid_relative_posix_path",
            format!("{path:?} is not a normalized relative POSIX path"),
        ));
    }
    Ok(())
}

pub fn validate_sorted_unique_utf8(values: &[String]) -> Result<(), TrustError> {
    if values
        .windows(2)
        .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
    {
        return Err(TrustError::new(
            "set_not_sorted_unique",
            "set arrays must be strictly sorted by UTF-8 bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 3 (plan doc 32): the journal-fixture hash-vector test died
    /// with `schemas/JOURNAL_HASH_FIXTURES.v1.json`.  This pins the pruned
    /// registry instead: every tag round-trips through `parse_registered`,
    /// the count is exact, and the retired journal/auth/qualification tag
    /// strings are rejected.
    #[test]
    fn domain_tag_registry_is_closed_and_journal_free() {
        // Stage 4 (plan doc 32) added seed-target-contract,
        // seed-contract-registry-root, and adaptation-ledger-row.
        assert_eq!(ALL_DOMAIN_TAGS.len(), 25);
        for tag in ALL_DOMAIN_TAGS {
            assert_eq!(DomainTag::parse_registered(tag.as_str()).unwrap(), *tag);
        }
        assert_eq!(
            DomainTag::parse_registered("trust-decision").unwrap(),
            DomainTag::TrustDecision
        );
        for retired in [
            "journal-event",
            "journal-event-payload",
            "journal-event-bundle",
            "journal-event-policy",
            "journal-commit-receipt",
            "journal-genesis",
            "journal-payload",
            "journal-commit-signing",
            "actor-authentication-key-manifest",
            "actor-authentication-receipt",
            "actor-authentication-signing",
            "actor-authentication-key-manifest-authorization",
            "package-authorization",
            "package-authorization-sidecar", // retired (Stage 3)
            "audit-authorization",
            "qualification-profile",
            "qualification-profile-catalog",
            "qualification-bundle",
            "conditional-theorem-candidate",
            "measure-catalog",
            "independent-basis",
            "independent-basis-derivation",
            "basis-fact-class-registry",
            "reflection-validation-result",
            "derived-result-root",
            "manifest-leaf",
        ] {
            assert!(
                DomainTag::parse_registered(retired).is_err(),
                "retired tag {retired} must be unregistered"
            );
        }
    }

    #[test]
    fn canonicalization_rejects_non_v1_numbers() {
        assert!(canonical_json_value(&serde_json::json!(-1)).is_err());
        assert!(canonical_json_value(&serde_json::json!(1.5)).is_err());
        assert!(canonical_json_value(&serde_json::json!(9_007_199_254_740_992_u64)).is_err());
    }

    #[test]
    fn strict_parser_rejects_duplicate_names_at_any_depth() {
        assert!(parse_json_strict(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse_json_strict(br#"{"a":{"b":1,"b":2}}"#).is_err());
        assert_eq!(parse_json_strict(br#"{"a":1}"#).unwrap()["a"], 1);
    }

    #[test]
    fn decimal_natural_is_unbounded_and_ordered() {
        let small: DecimalNatural = "9223372036854775808".parse().unwrap();
        let large: DecimalNatural = "100000000000000000000000000000000000000".parse().unwrap();
        assert!(small < large);
        assert!("00".parse::<DecimalNatural>().is_err());
    }

    #[test]
    fn self_digest_omits_field_instead_of_blank_substitution() {
        // The digest is computed over the record WITH the self field
        // removed (never blank-substituted), then embedded and re-verified.
        let mut record = serde_json::json!({
            "schema": "trellis-fixture/v1",
            "payload": {"a": 1, "b": "two"},
            "record_sha256": "00".repeat(32),
        });
        let computed =
            self_digest(DomainTag::TrustDecision, &record, "record_sha256").unwrap();
        let mut stripped = record.clone();
        stripped.as_object_mut().unwrap().remove("record_sha256");
        assert_eq!(
            computed,
            tagged_hash(
                DomainTag::TrustDecision,
                &canonical_json_value(&stripped).unwrap()
            )
        );
        record["record_sha256"] = Value::String(computed.to_string());
        assert_eq!(
            verify_self_digest(DomainTag::TrustDecision, &record, "record_sha256").unwrap(),
            computed
        );
        record["payload"]["a"] = serde_json::json!(2);
        assert!(
            verify_self_digest(DomainTag::TrustDecision, &record, "record_sha256").is_err()
        );
    }
}
