//! Semantic validation for seed-frozen structural measures and numeric bases.
//!
//! Schema validation is intentionally insufficient here: qualification must
//! not accept a clean-looking `len(input) < B` when `B` was smuggled in from
//! the known witness.  This module walks the complete typed provenance graph,
//! recomputes every value and sub-object digest, and accepts only registered
//! structural measures and registered independent fact classes.

use super::canonical::{
    verify_self_digest, DecimalNatural, DomainTag, Sha256Digest, TrustError,
};
use super::closure::VerifiedSeedDefinitionClosure;
use super::records::AuthoritativeRecord;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedIndependentBasis {
    pub basis_sha256: Sha256Digest,
    pub derivation_sha256: Sha256Digest,
    pub measure_definition_sha256: Sha256Digest,
    pub limit: DecimalNatural,
}

#[derive(Clone, Debug)]
struct EvaluatedNode {
    value: DecimalNatural,
    tuple: ResourceTuple,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResourceTuple {
    units: String,
    comparison: String,
    scope: String,
    enforcement: String,
}

/// Validate an `IndependentBasis` and its entire seed-frozen dependency
/// closure.  No caller-provided definition outside `seed` is consulted.
pub fn validate_independent_basis(
    seed: &VerifiedSeedDefinitionClosure,
    basis: &AuthoritativeRecord,
) -> Result<ValidatedIndependentBasis, TrustError> {
    require_schema(basis, "trellis-independent-basis/v1")?;
    require_exact_seed_record(seed, basis)?;
    let basis_value = basis.value();

    let derivation_digest = digest_field(basis_value, "basis_derivation_sha256")?;
    let derivation = seed_record(
        seed,
        derivation_digest,
        "trellis-independent-basis-derivation/v1",
    )?;
    if string_field(basis_value, "basis_derivation_id")?
        != string_field(derivation.value(), "derivation_id")?
    {
        return Err(error(
            "basis_derivation_identity_mismatch",
            "basis names a different derivation ID",
        ));
    }

    let registry_digest = digest_field(derivation.value(), "fact_class_registry_sha256")?;
    let registry = seed_record(
        seed,
        registry_digest,
        "trellis-basis-fact-class-registry/v1",
    )?;
    if string_field(derivation.value(), "fact_class_registry_id")?
        != string_field(registry.value(), "registry_id")?
    {
        return Err(error(
            "basis_fact_registry_identity_mismatch",
            "derivation names a different fact-class registry ID",
        ));
    }
    let registry_entries = validate_registry(registry)?;

    let measure_digest = digest_field(basis_value, "measure_definition_sha256")?;
    validate_measure(seed, basis_value, measure_digest)?;

    let tuple = tuple_from(basis_value)?;
    for field in ["binder", "binder_type", "measure_id"] {
        require_equal_string(basis_value, derivation.value(), field)?;
    }
    require_equal_digest(basis_value, derivation.value(), "measure_definition_sha256")?;
    require_tuple(derivation.value(), &tuple)?;
    if string_field(basis_value, "limit")?
        != string_field(derivation.value(), "derived_limit")?
    {
        return Err(error(
            "basis_derived_limit_mismatch",
            "basis limit differs from the derivation result",
        ));
    }

    let nodes = derivation
        .value()
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| error("basis_nodes_missing", "derivation lacks nodes"))?;
    let mut by_id = BTreeMap::new();
    let mut prior_id: Option<&str> = None;
    for node in nodes {
        let id = string_field(node, "node_id")?;
        if prior_id.is_some_and(|prior| prior.as_bytes() >= id.as_bytes()) {
            return Err(error(
                "basis_node_order_invalid",
                "derivation nodes must be unique and UTF-8 byte sorted by node_id",
            ));
        }
        prior_id = Some(id);
        verify_self_digest(
            DomainTag::IndependentBasisDerivationNode,
            node,
            "node_sha256",
        )?;
        require_tuple(node, &tuple)?;
        let dependencies = dependencies(node)?;
        let mut prior_dependency: Option<&str> = None;
        for dependency in &dependencies {
            if prior_dependency.is_some_and(|prior| prior.as_bytes() >= dependency.as_bytes()) {
                return Err(error(
                    "basis_dependency_order_invalid",
                    "dependency IDs must be unique and UTF-8 byte sorted",
                ));
            }
            prior_dependency = Some(dependency);
        }
        if by_id.insert(id.to_owned(), node).is_some() {
            return Err(error("basis_node_duplicate", "duplicate derivation node ID"));
        }
    }

    let root = string_field(derivation.value(), "root_node_id")?;
    let mut visiting = BTreeSet::new();
    let mut evaluated = BTreeMap::new();
    let mut visit_count = BTreeMap::new();
    let result = evaluate_node(
        root,
        &by_id,
        &registry_entries,
        &tuple,
        &mut visiting,
        &mut evaluated,
        &mut visit_count,
    )?;
    if evaluated.len() != by_id.len() {
        return Err(error(
            "basis_derivation_has_unreachable_nodes",
            "every declared derivation node must be reachable from the root",
        ));
    }
    if visit_count.values().any(|count| *count != 1) {
        return Err(error(
            "basis_derivation_shared_dependency",
            "v1 basis derivations must be trees; a dependency may be reached exactly once",
        ));
    }
    let declared_limit: DecimalNatural = string_field(derivation.value(), "derived_limit")?.parse()?;
    if result.value != declared_limit || result.tuple != tuple {
        return Err(error(
            "basis_root_value_mismatch",
            "recomputed root tuple/value differs from the declared derivation",
        ));
    }

    Ok(ValidatedIndependentBasis {
        basis_sha256: basis.digest(),
        derivation_sha256: derivation_digest,
        measure_definition_sha256: measure_digest,
        limit: declared_limit,
    })
}

fn validate_registry(
    registry: &AuthoritativeRecord,
) -> Result<BTreeMap<String, Value>, TrustError> {
    let entries = registry
        .value()
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| error("basis_registry_entries_missing", "fact registry lacks entries"))?;
    let expected_classes = [
        "configuration_limit",
        "deployment_limit",
        "external_attestation_limit",
        "host_capability_limit",
        "source_limit",
    ];
    if entries.len() != expected_classes.len() {
        return Err(error(
            "basis_registry_not_closed",
            "v1 fact registry must contain all five classes exactly once",
        ));
    }
    let mut output = BTreeMap::new();
    for (entry, expected) in entries.iter().zip(expected_classes) {
        if string_field(entry, "fact_class")? != expected {
            return Err(error(
                "basis_registry_order_or_class_invalid",
                "fact registry must contain the closed class list in byte order",
            ));
        }
        verify_self_digest(
            DomainTag::BasisFactClassRegistryEntry,
            entry,
            "entry_sha256",
        )?;
        output.insert(expected.to_owned(), entry.clone());
    }
    Ok(output)
}

fn validate_measure(
    seed: &VerifiedSeedDefinitionClosure,
    basis: &Value,
    expected_digest: Sha256Digest,
) -> Result<(), TrustError> {
    let mut matches = Vec::new();
    for record in seed.records_by_digest.values().filter(|record| {
        record.contract().record_schema == "trellis-measure-catalog/v1"
    }) {
        let measures = record
            .value()
            .get("measures")
            .and_then(Value::as_array)
            .ok_or_else(|| error("measure_catalog_invalid", "measure catalog lacks measures"))?;
        let mut prior_id: Option<&str> = None;
        for measure in measures {
            let id = string_field(measure, "measure_id")?;
            if prior_id.is_some_and(|prior| prior.as_bytes() >= id.as_bytes()) {
                return Err(error(
                    "measure_catalog_order_invalid",
                    "measures must be unique and UTF-8 byte sorted",
                ));
            }
            prior_id = Some(id);
            let digest = verify_self_digest(
                DomainTag::MeasureDefinition,
                measure,
                "measure_definition_sha256",
            )?;
            let expression = measure.get("expression_ast").ok_or_else(|| {
                error("measure_expression_missing", "measure lacks expression_ast")
            })?;
            if string_field(expression, "kind")? != "structural_input_byte_length_v1"
                || string_field(expression, "binder")? != string_field(measure, "binder")?
            {
                return Err(error(
                    "measure_expression_not_v1_structural",
                    "v1 admits only the structural input byte-length projection",
                ));
            }
            if digest == expected_digest {
                matches.push(measure);
            }
        }
    }
    if matches.len() != 1 {
        return Err(error(
            "basis_measure_not_uniquely_registered",
            "basis measure must resolve to exactly one seed-frozen definition",
        ));
    }
    let measure = matches[0];
    for field in ["binder", "binder_type", "measure_id", "units"] {
        require_equal_string(basis, measure, field)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn evaluate_node(
    id: &str,
    nodes: &BTreeMap<String, &Value>,
    registry: &BTreeMap<String, Value>,
    tuple: &ResourceTuple,
    visiting: &mut BTreeSet<String>,
    evaluated: &mut BTreeMap<String, EvaluatedNode>,
    visit_count: &mut BTreeMap<String, usize>,
) -> Result<EvaluatedNode, TrustError> {
    *visit_count.entry(id.to_owned()).or_default() += 1;
    if let Some(value) = evaluated.get(id) {
        return Ok(value.clone());
    }
    if !visiting.insert(id.to_owned()) {
        return Err(error("basis_derivation_cycle", "basis graph contains a cycle"));
    }
    let node = nodes.get(id).ok_or_else(|| {
        error(
            "basis_dependency_missing",
            format!("derivation references missing node {id}"),
        )
    })?;
    require_tuple(node, tuple)?;
    let dependencies = dependencies(node)?;
    let result = match string_field(node, "node_kind")? {
        "registered_fact" => {
            if !dependencies.is_empty() || string_field(node, "registration_epoch")? != "seed" {
                return Err(error(
                    "basis_fact_dependencies_or_epoch_invalid",
                    "registered fact leaves have no dependencies and must be seed-frozen",
                ));
            }
            let class = string_field(node, "fact_class")?;
            let entry = registry.get(class).ok_or_else(|| {
                error("basis_fact_class_unregistered", format!("unregistered fact class {class}"))
            })?;
            require_equal_string(node, entry, "fact_schema_id")?;
            require_equal_digest(node, entry, "fact_schema_sha256")?;
            require_digest(
                node,
                "fact_class_registry_entry_sha256",
                digest_field(entry, "entry_sha256")?,
            )?;
            require_list_contains(entry, "allowed_scopes", &tuple.scope)?;
            require_list_contains(entry, "allowed_enforcements", &tuple.enforcement)?;
            for field in [
                "fact_content_sha256",
                "evidence_artifact_sha256",
                "validity_interval_sha256",
            ] {
                require_nonzero_digest(node, field)?;
            }
            if class == "external_attestation_limit" {
                // The record shape is closed, but cryptographic attestation
                // authority is not inferred from opaque fields. Until the
                // seed supplies a separately verified attestation manifest,
                // fail closed instead of treating a signature-shaped string
                // as independent evidence.
                return Err(error(
                    "external_basis_attestation_not_verified",
                    "external-attestation facts require an independently rooted verifier",
                ));
            }
            require_nonzero_digest(node, "fact_validator_sha256")?;
            EvaluatedNode {
                value: string_field(node, "fact_value")?.parse()?,
                tuple: tuple.clone(),
            }
        }
        "derived" => {
            let mut values = Vec::with_capacity(dependencies.len());
            for dependency in &dependencies {
                values.push(evaluate_node(
                    dependency,
                    nodes,
                    registry,
                    tuple,
                    visiting,
                    evaluated,
                    visit_count,
                )?);
            }
            require_nonzero_digest(node, "derivation_validator_sha256")?;
            require_nonzero_digest(node, "derivation_receipt_sha256")?;
            let computed = match string_field(node, "primitive")? {
                "identity_v1" if values.len() == 1 => values[0].value.clone(),
                "minimum_v1" if values.len() >= 2 => values
                    .iter()
                    .map(|value| value.value.clone())
                    .min()
                    .expect("minimum has at least two dependencies"),
                _ => {
                    return Err(error(
                        "basis_derivation_primitive_invalid",
                        "derived node has an unknown primitive or invalid arity",
                    ))
                }
            };
            let declared: DecimalNatural = string_field(node, "derived_value")?.parse()?;
            if computed != declared {
                return Err(error(
                    "basis_derived_node_value_mismatch",
                    format!("derived node {id} declares the wrong value"),
                ));
            }
            EvaluatedNode {
                value: computed,
                tuple: tuple.clone(),
            }
        }
        _ => return Err(error("basis_node_kind_invalid", "unknown basis node kind")),
    };
    visiting.remove(id);
    evaluated.insert(id.to_owned(), result.clone());
    Ok(result)
}

fn dependencies(value: &Value) -> Result<Vec<String>, TrustError> {
    value
        .get("dependency_node_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| error("basis_dependencies_missing", "node lacks dependency IDs"))?
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                error("basis_dependency_invalid", "dependency ID must be a string")
            })
        })
        .collect()
}

fn tuple_from(value: &Value) -> Result<ResourceTuple, TrustError> {
    Ok(ResourceTuple {
        units: string_field(value, "units")?.to_owned(),
        comparison: string_field(value, "comparison")?.to_owned(),
        scope: string_field(value, "scope")?.to_owned(),
        enforcement: string_field(value, "enforcement")?.to_owned(),
    })
}

fn require_tuple(value: &Value, expected: &ResourceTuple) -> Result<(), TrustError> {
    if tuple_from(value)? != *expected {
        return Err(error(
            "basis_resource_tuple_mismatch",
            "units/comparison/scope/enforcement changed inside the derivation",
        ));
    }
    Ok(())
}

fn require_exact_seed_record(
    seed: &VerifiedSeedDefinitionClosure,
    record: &AuthoritativeRecord,
) -> Result<(), TrustError> {
    if seed.records_by_digest.get(&record.digest()) != Some(record) {
        return Err(error(
            "basis_record_not_seed_frozen",
            "basis record is not byte-identical to a seed definition",
        ));
    }
    Ok(())
}

fn seed_record<'a>(
    seed: &'a VerifiedSeedDefinitionClosure,
    digest: Sha256Digest,
    schema: &str,
) -> Result<&'a AuthoritativeRecord, TrustError> {
    let record = seed.records_by_digest.get(&digest).ok_or_else(|| {
        error(
            "basis_seed_dependency_missing",
            format!("seed lacks dependency {digest}"),
        )
    })?;
    require_schema(record, schema)?;
    Ok(record)
}

fn require_schema(record: &AuthoritativeRecord, expected: &str) -> Result<(), TrustError> {
    if record.contract().record_schema != expected {
        return Err(error(
            "basis_record_schema_mismatch",
            format!("expected {expected}, got {}", record.contract().record_schema),
        ));
    }
    Ok(())
}

fn require_equal_string(left: &Value, right: &Value, field: &str) -> Result<(), TrustError> {
    if string_field(left, field)? != string_field(right, field)? {
        return Err(error(
            "basis_string_binding_mismatch",
            format!("records differ at {field}"),
        ));
    }
    Ok(())
}

fn require_equal_digest(left: &Value, right: &Value, field: &str) -> Result<(), TrustError> {
    require_digest(left, field, digest_field(right, field)?)
}

fn require_digest(value: &Value, field: &str, expected: Sha256Digest) -> Result<(), TrustError> {
    let actual = digest_field(value, field)?;
    if actual != expected {
        return Err(error(
            "basis_digest_binding_mismatch",
            format!("{field}: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

fn require_nonzero_digest(value: &Value, field: &str) -> Result<(), TrustError> {
    if digest_field(value, field)? == Sha256Digest::ZERO {
        return Err(error(
            "basis_zero_artifact_digest",
            format!("{field} cannot be zero"),
        ));
    }
    Ok(())
}

fn require_list_contains(value: &Value, field: &str, expected: &str) -> Result<(), TrustError> {
    let values = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| error("basis_registry_list_missing", format!("missing {field}")))?;
    if !values.iter().any(|value| value.as_str() == Some(expected)) {
        return Err(error(
            "basis_registry_tuple_not_allowed",
            format!("registry {field} does not permit {expected}"),
        ));
    }
    Ok(())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        error(
            "basis_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn digest_field(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    string_field(value, field)?.parse()
}

fn error(code: &'static str, detail: impl Into<String>) -> TrustError {
    TrustError::new(code, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::{AuthoritativeRecord, SchemaRegistry};

    fn fixture_seed() -> VerifiedSeedDefinitionClosure {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        let registry = SchemaRegistry::v1().unwrap();
        let mut records = BTreeMap::new();
        let mut values = BTreeMap::new();
        for key in [
            "basis_fact_class_registry",
            "independent_basis_derivation",
            "independent_basis",
            "measure_catalog",
        ] {
            let value = fixture["objects"][key].clone();
            let record = AuthoritativeRecord::parse(&registry, value.clone()).unwrap();
            values.insert(record.digest(), value);
            records.insert(record.digest(), record);
        }
        VerifiedSeedDefinitionClosure {
            seed_manifest_sha256: "22".repeat(32).parse().unwrap(),
            bundle_sha256: "11".repeat(32).parse().unwrap(),
            records_by_digest: records,
            canonical_values_by_digest: values,
        }
    }

    #[test]
    fn fixture_basis_recomputes_through_registered_fact() {
        let seed = fixture_seed();
        let basis = seed
            .records_by_digest
            .values()
            .find(|record| record.contract().record_schema == "trellis-independent-basis/v1")
            .unwrap();
        let result = validate_independent_basis(&seed, basis).unwrap();
        assert_eq!(result.limit.as_str(), "1048576");
    }

    #[test]
    fn witness_derived_numeric_leaf_cannot_be_added_or_hidden() {
        let mut seed = fixture_seed();
        let digest = seed
            .records_by_digest
            .iter()
            .find(|(_, record)| {
                record.contract().record_schema == "trellis-independent-basis-derivation/v1"
            })
            .map(|(digest, _)| *digest)
            .unwrap();
        let mut value = seed.records_by_digest[&digest].value().clone();
        value["nodes"][0]["fact_content_sha256"] = Value::String("ff".repeat(32));
        // Even if a caller changes a fact and leaves the outer basis link
        // untouched, the node self-digest and seed identity both fail.
        assert!(AuthoritativeRecord::parse(&SchemaRegistry::v1().unwrap(), value).is_err());
        let basis = seed
            .records_by_digest
            .values()
            .find(|record| record.contract().record_schema == "trellis-independent-basis/v1")
            .unwrap()
            .clone();
        assert!(validate_independent_basis(&seed, &basis).is_ok());
        seed.records_by_digest.remove(&digest);
        assert!(validate_independent_basis(&seed, &basis).is_err());
    }
}
