use super::canonical::{raw_sha256, DomainTag, Sha256Digest, TrustError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordContract {
    pub schema_id: &'static str,
    pub record_schema: &'static str,
    pub domain_tag: DomainTag,
    pub self_digest_field: Option<&'static str>,
}

impl RecordContract {
    pub fn for_record_schema(schema: &str) -> Result<Self, TrustError> {
        RECORD_CONTRACTS
            .iter()
            .copied()
            .find(|contract| contract.record_schema == schema)
            .ok_or_else(|| {
                TrustError::new(
                    "unregistered_record_schema",
                    format!("{schema:?} is not a closed v1 record schema"),
                )
            })
    }
}

#[derive(Clone, Debug)]
struct CompiledSchema {
    root: Value,
    raw_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct SchemaRegistry {
    schemas: BTreeMap<&'static str, CompiledSchema>,
}

impl SchemaRegistry {
    pub fn v1() -> Result<Self, TrustError> {
        let mut schemas = BTreeMap::new();
        for (expected_id, bytes) in SCHEMA_SOURCES {
            let root: Value = serde_json::from_str(bytes).map_err(|error| {
                TrustError::new(
                    "embedded_schema_invalid",
                    format!("{expected_id}: {error}"),
                )
            })?;
            if root.get("$id").and_then(Value::as_str) != Some(*expected_id) {
                return Err(TrustError::new(
                    "embedded_schema_id_mismatch",
                    format!("embedded schema does not declare {expected_id}"),
                ));
            }
            schemas.insert(
                *expected_id,
                CompiledSchema {
                    root,
                    raw_sha256: raw_sha256(bytes.as_bytes()),
                },
            );
        }
        Ok(Self { schemas })
    }

    pub fn schema_sha256(&self, schema_id: &str) -> Result<Sha256Digest, TrustError> {
        self.schemas
            .get(schema_id)
            .map(|schema| schema.raw_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "unknown_schema_id",
                    format!("schema {schema_id:?} is not embedded in the v1 kernel"),
                )
            })
    }

    pub fn validate(&self, schema_id: &str, instance: &Value) -> Result<(), TrustError> {
        let schema = self.schemas.get(schema_id).ok_or_else(|| {
            TrustError::new(
                "unknown_schema_id",
                format!("schema {schema_id:?} is not embedded in the v1 kernel"),
            )
        })?;
        validate_node(&self.schemas, &schema.root, &schema.root, instance, "$", 0)
    }

    pub fn validate_record(&self, record: &Value) -> Result<RecordContract, TrustError> {
        let record_schema = record.get("schema").and_then(Value::as_str).ok_or_else(|| {
            TrustError::new(
                "record_schema_missing",
                "authoritative record must contain a string schema field",
            )
        })?;
        let contract = RecordContract::for_record_schema(record_schema)?;
        self.validate(contract.schema_id, record)?;
        Ok(contract)
    }

    pub fn len(&self) -> usize {
        self.schemas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }

    pub fn embedded_sources() -> Vec<(&'static str, &'static [u8])> {
        SCHEMA_SOURCES
            .iter()
            .map(|(id, source)| (*id, source.as_bytes()))
            .collect()
    }
}

fn validate_node(
    registry: &BTreeMap<&'static str, CompiledSchema>,
    root: &Value,
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
) -> Result<(), TrustError> {
    if depth > 128 {
        return Err(schema_error(path, "schema recursion exceeds v1 limit"));
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if reference.starts_with("#/") {
            let target = resolve_pointer(root, &reference[1..]).ok_or_else(|| {
                schema_error(path, format!("unresolved local schema reference {reference}"))
            })?;
            validate_node(registry, root, target, instance, path, depth + 1)?;
        } else {
            let target = registry.get(reference).ok_or_else(|| {
                schema_error(path, format!("unregistered external schema reference {reference}"))
            })?;
            validate_node(
                registry,
                &target.root,
                &target.root,
                instance,
                path,
                depth + 1,
            )?;
        }
    }

    if let Some(expected_value) = schema.get("type") {
        let expected_types: Vec<&str> = match expected_value {
            Value::String(expected) => vec![expected.as_str()],
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_str().ok_or_else(|| {
                        schema_error(path, "embedded type-array item is not a string")
                    })
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(schema_error(
                    path,
                    "embedded schema type must be a string or string array",
                ))
            }
        };
        let matches_type = |expected: &str| match expected {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "integer" => instance
                .as_number()
                .is_some_and(|number| number.as_i64().is_some() || number.as_u64().is_some()),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => false,
        };
        if expected_types.iter().any(|kind| {
            !matches!(*kind, "object" | "array" | "string" | "integer" | "boolean" | "null")
        }) {
            return Err(schema_error(path, "embedded schema uses unsupported type"));
        }
        if !expected_types.iter().any(|kind| matches_type(kind)) {
            return Err(schema_error(path, format!("expected one of {expected_types:?}")));
        }
    }

    if let Some(expected) = schema.get("const") {
        if instance != expected {
            return Err(schema_error(path, "value differs from schema const"));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|candidate| candidate == instance) {
            return Err(schema_error(path, "value is outside closed enum"));
        }
    }

    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            validate_node(registry, root, branch, instance, path, depth + 1)?;
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if !branches
            .iter()
            .any(|branch| validate_node(registry, root, branch, instance, path, depth + 1).is_ok())
        {
            return Err(schema_error(path, "no anyOf branch matched"));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = branches
            .iter()
            .filter(|branch| validate_node(registry, root, branch, instance, path, depth + 1).is_ok())
            .count();
        if matches != 1 {
            return Err(schema_error(
                path,
                format!("oneOf requires one match, found {matches}"),
            ));
        }
    }
    if let Some(negated) = schema.get("not") {
        if validate_node(registry, root, negated, instance, path, depth + 1).is_ok() {
            return Err(schema_error(path, "negated schema matched"));
        }
    }
    if let Some(condition) = schema.get("if") {
        let branch = if validate_node(registry, root, condition, instance, path, depth + 1).is_ok() {
            schema.get("then")
        } else {
            schema.get("else")
        };
        if let Some(branch) = branch {
            validate_node(registry, root, branch, instance, path, depth + 1)?;
        }
    }

    if let Some(object) = instance.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required {
                let field = field.as_str().ok_or_else(|| {
                    schema_error(path, "embedded required entry is not a string")
                })?;
                if !object.contains_key(field) {
                    return Err(schema_error(
                        path,
                        format!("required property {field:?} is missing"),
                    ));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (field, field_schema) in properties {
                if let Some(value) = object.get(field) {
                    validate_node(
                        registry,
                        root,
                        field_schema,
                        value,
                        &format!("{path}/{field}"),
                        depth + 1,
                    )?;
                }
            }
        }
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let declared: BTreeSet<&str> = properties
                .into_iter()
                .flat_map(|entries| entries.keys().map(String::as_str))
                .collect();
            if let Some(extra) = object.keys().find(|field| !declared.contains(field.as_str())) {
                return Err(schema_error(
                    path,
                    format!("additional property {extra:?} is forbidden"),
                ));
            }
        }
    }

    if let Some(array) = instance.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
            if array.len() < minimum as usize {
                return Err(schema_error(path, format!("requires at least {minimum} items")));
            }
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64) {
            if array.len() > maximum as usize {
                return Err(schema_error(path, format!("permits at most {maximum} items")));
            }
        }
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            for left in 0..array.len() {
                if array[left + 1..].iter().any(|right| right == &array[left]) {
                    return Err(schema_error(path, "array items are not unique"));
                }
            }
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, value) in array.iter().enumerate() {
                validate_node(
                    registry,
                    root,
                    item_schema,
                    value,
                    &format!("{path}/{index}"),
                    depth + 1,
                )?;
            }
        }
        if let Some(contains_schema) = schema.get("contains") {
            let matches = array
                .iter()
                .filter(|value| {
                    validate_node(registry, root, contains_schema, value, path, depth + 1).is_ok()
                })
                .count();
            let minimum = schema
                .get("minContains")
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;
            if matches < minimum {
                return Err(schema_error(
                    path,
                    format!("contains matched {matches}, requires {minimum}"),
                ));
            }
        }
    }

    if let Some(string) = instance.as_str() {
        let length = string.chars().count() as u64;
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64) {
            if length < minimum {
                return Err(schema_error(path, format!("string shorter than {minimum}")));
            }
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
            if length > maximum {
                return Err(schema_error(path, format!("string longer than {maximum}")));
            }
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            if !matches_closed_pattern(pattern, string)? {
                return Err(schema_error(path, format!("string does not match {pattern}")));
            }
        }
    }

    if instance
        .as_number()
        .is_some_and(|number| number.as_i64().is_some() || number.as_u64().is_some())
    {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_i64) {
            if instance
                .as_i64()
                .is_some_and(|integer| integer < minimum)
            {
                return Err(schema_error(path, format!("integer below {minimum}")));
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_u64) {
            if instance
                .as_u64()
                .is_some_and(|integer| integer > maximum)
            {
                return Err(schema_error(path, format!("integer above {maximum}")));
            }
        }
    }
    Ok(())
}

fn resolve_pointer<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut current = root;
    for raw_segment in pointer.strip_prefix('/')?.split('/') {
        let segment = raw_segment.replace("~1", "/").replace("~0", "~");
        current = current.get(&segment)?;
    }
    Some(current)
}

fn matches_closed_pattern(pattern: &str, value: &str) -> Result<bool, TrustError> {
    let result = match pattern {
        "^[0-9a-f]{64}$" => is_lower_hex(value, 64),
        "^[0-9a-f]{128}$" => is_lower_hex(value, 128),
        "^(0|[1-9][0-9]*)$" => {
            value == "0"
                || (!value.is_empty()
                    && !value.starts_with('0')
                    && value.bytes().all(|byte| byte.is_ascii_digit()))
        }
        "^[1-9][0-9]*$" => {
            !value.is_empty()
                && !value.starts_with('0')
                && value.bytes().all(|byte| byte.is_ascii_digit())
        }
        "^trellis://schemas/[a-z0-9-]+/v1$" => value
            .strip_prefix("trellis://schemas/")
            .and_then(|tail| tail.strip_suffix("/v1"))
            .is_some_and(|middle| {
                !middle.is_empty()
                    && middle
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            }),
        "^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$" => {
            is_canonical_base64_shape(value)
        }
        "^reference/rust-witnesses/target-[0-9a-f]{64}/witness\\.rs$" => value
            .strip_prefix("reference/rust-witnesses/target-")
            .and_then(|tail| tail.strip_suffix("/witness.rs"))
            .is_some_and(|digest| is_lower_hex(digest, 64)),
        other => {
            return Err(TrustError::new(
                "unregistered_schema_pattern",
                format!("embedded schema pattern {other:?} is not implemented"),
            ))
        }
    };
    Ok(result)
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_base64_shape(value: &str) -> bool {
    if value.len() % 4 != 0 {
        return false;
    }
    let padding = value.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return false;
    }
    let content_len = value.len() - padding;
    value.as_bytes()[..content_len].iter().all(|byte| {
        byte.is_ascii_uppercase()
            || byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || *byte == b'+'
            || *byte == b'/'
    }) && value.as_bytes()[content_len..].iter().all(|byte| *byte == b'=')
        && match padding {
            0 => true,
            1 => content_len % 4 == 3,
            2 => content_len % 4 == 2,
            _ => false,
        }
}

fn schema_error(path: &str, detail: impl Into<String>) -> TrustError {
    TrustError::new("schema_validation_failed", format!("{path}: {}", detail.into()))
}

/// Compile-time pin of a v2.3 schema source.
///
/// The path is relative to THIS source file, so the bytes are pulled from
/// `kernel/src/trust_base/schemas/v2.3/` — inside the crate's manifest root.
/// It must never reach outside the crate (no `CARGO_MANIFEST_DIR` + `/../`):
/// the schema bytes are part of the trust base's TCB, and an out-of-crate
/// include makes the crate unbuildable from a plain checkout, breaks
/// `cargo package` / `cargo vendor`, and lets the pin be silently satisfied
/// by whatever happens to be sitting beside the repo.
///
/// `kernel/TRUST_BASE_SCHEMAS.md` records the full discipline. That directory
/// carries `*.schema.json` and nothing else: the runtime-distribution closure
/// sweeps `kernel/src` into the reproducible-build receipt, so only files that
/// decide the built binary may live under it.
macro_rules! schema_source {
    ($file:literal) => {
        include_str!(concat!("schemas/v2.3/", $file))
    };
}

const SCHEMA_SOURCES: &[(&str, &str)] = &[
    ("trellis://schemas/approval-and-package/v1", schema_source!("approval-and-package.schema.json")),
    ("trellis://schemas/seed-authored-definition-manifest/v1", schema_source!("seed-authored-definition-manifest.schema.json")),
    // W2 (endgame rebuild): the finalization archive's `claim_rows.json` —
    // the structured carrier of the §16 four-line rows the claim document
    // is re-rendered from.
    ("trellis://schemas/claim-rows/v3", schema_source!("claim-rows.schema.json")),
    ("trellis://schemas/rust-witness-artifact/v1", schema_source!("rust-witness-artifact.schema.json")),
];

const RECORD_CONTRACTS: &[RecordContract] = &[
    RecordContract { schema_id: "trellis://schemas/seed-authored-definition-manifest/v1", record_schema: "trellis-seed-authored-definition-manifest/v1", domain_tag: DomainTag::SeedAuthoredDefinitionManifest, self_digest_field: Some("manifest_sha256") },
    RecordContract { schema_id: "trellis://schemas/approval-and-package/v1", record_schema: "trellis-human-approval/v1", domain_tag: DomainTag::HumanApproval, self_digest_field: None },
    RecordContract { schema_id: "trellis://schemas/rust-witness-artifact/v1", record_schema: "trellis-rust-witness-artifact/v1", domain_tag: DomainTag::RawArtifact, self_digest_field: None },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 3 (plan doc 32): the journal family
    /// (journal-event[-payload|-bundle|-policy], journal-commit-receipt,
    /// actor-authentication family, package-authorization sidecar) and
    /// reflection-validation-result were deleted with Q1 (17 -> 8; Stage 6
    /// re-adds the two assessment schemas; W2 adds claim-rows).
    #[test]
    fn embeds_all_normative_schemas() {
        let registry = SchemaRegistry::v1().unwrap();
        assert_eq!(registry.len(), 4);
    }

    /// Retired record schemas (journal family + reflection results) are
    /// REJECTED as unknown; the surviving human-approval shape validates.
    #[test]
    fn retired_journal_family_records_are_rejected() {
        let registry = SchemaRegistry::v1().unwrap();
        for retired_schema in [
            "trellis-journal-event-payload/v1",
            "trellis-journal-event-bundle/v1",
            "trellis-journal-event-policy/v1",
            "trellis-journal-commit-receipt/v1",
            "trellis-actor-authentication-key-manifest/v1",
            "trellis-actor-authentication-receipt/v1",
            "trellis-package-authorization-sidecar/v1", // retired (Stage 3)
            "trellis-package-authorization/v1",
            "trellis-audit-authorization/v1",
            "trellis-reflection-validation-result/v1",
        ] {
            let error = registry
                .validate_record(&serde_json::json!({ "schema": retired_schema }))
                .unwrap_err();
            assert_eq!(error.code, "unregistered_record_schema", "{retired_schema}");
        }
        let approval = serde_json::json!({
            "schema": "trellis-human-approval/v1",
            "authored_semantic_root": "11".repeat(32),
            "approved_evidence_tool_input_root": "22".repeat(32),
            "gate_presentation_sha256": "33".repeat(32),
        });
        registry.validate_record(&approval).expect("human approval validates");
    }

    #[test]
    fn closed_records_reject_unknown_fields() {
        let registry = SchemaRegistry::v1().unwrap();
        let mut approval = serde_json::json!({
            "schema": "trellis-human-approval/v1",
            "authored_semantic_root": "11".repeat(32),
            "approved_evidence_tool_input_root": "22".repeat(32),
            "gate_presentation_sha256": "33".repeat(32),
        });
        approval
            .as_object_mut()
            .unwrap()
            .insert("smuggled".into(), Value::Bool(true));
        let error = registry.validate_record(&approval).unwrap_err();
        assert_eq!(error.code, "schema_validation_failed");
    }
}
