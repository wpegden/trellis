use super::canonical::{canonical_json_value, tagged_hash, DomainTag, Sha256Digest, TrustError};
use super::records::AuthoritativeRecord;
use serde_json::Value;
use std::collections::BTreeSet;

/// Validate the portion of the registration DAG that is carried by the seed
/// manifest itself.  This is intentionally semantic validation on top of JSON
/// Schema: the schema alone cannot enforce order, tag/kind correspondence, or
/// recomputation of the authored root.
pub fn validate_seed_manifest_semantics(
    seed: &AuthoritativeRecord,
) -> Result<SeedRoots, TrustError> {
    if seed.contract().record_schema != "trellis-seed-authored-definition-manifest/v1" {
        return Err(TrustError::new(
            "seed_manifest_schema_mismatch",
            "seed validator requires a v1 seed authored-definition manifest",
        ));
    }
    let value = seed.value();
    let definitions = value
        .get("definitions")
        .and_then(Value::as_array)
        .ok_or_else(|| TrustError::new("seed_definitions_missing", "definitions must be an array"))?;
    let mut identities = BTreeSet::new();
    let mut digests = BTreeSet::new();
    let mut previous_rank = 0_u8;
    for (index, definition) in definitions.iter().enumerate() {
        let kind = string_field(definition, "record_kind")?;
        let id = string_field(definition, "record_id")?;
        let schema_id = string_field(definition, "record_schema_id")?;
        let tag = string_field(definition, "domain_tag")?;
        let digest: Sha256Digest = string_field(definition, "record_sha256")?.parse()?;
        if !identities.insert((kind.to_owned(), id.to_owned())) || !digests.insert(digest) {
            return Err(TrustError::new(
                "duplicate_seed_definition",
                format!("definition {index} repeats an identity or digest"),
            ));
        }
        let rule = definition_rule(kind)?;
        if !rule.allowed_tags.contains(&tag) {
            return Err(TrustError::new(
                "seed_definition_tag_mismatch",
                format!("{kind}/{id} cannot use domain tag {tag}"),
            ));
        }
        DomainTag::parse_registered(tag)?;
        if let Some(expected_schema) = rule.schema_id {
            if schema_id != expected_schema {
                return Err(TrustError::new(
                    "seed_definition_schema_mismatch",
                    format!("{kind}/{id} requires schema {expected_schema}"),
                ));
            }
        }
        if index > 0 && rule.rank < previous_rank {
            return Err(TrustError::new(
                "seed_definition_order_backedge",
                format!("{kind}/{id} appears after a dependent registration class"),
            ));
        }
        previous_rank = rule.rank;
    }

    let authored_semantic_root = tagged_hash(
        DomainTag::AuthoredSemanticRoot,
        &canonical_json_value(&Value::Array(definitions.clone()))?,
    );
    let declared_authored: Sha256Digest =
        string_field(value, "authored_semantic_root")?.parse()?;
    if declared_authored != authored_semantic_root {
        return Err(TrustError::new(
            "seed_authored_root_mismatch",
            format!("declared {declared_authored}, recomputed {authored_semantic_root}"),
        ));
    }
    let evidence_tool_input_root: Sha256Digest =
        string_field(value, "approved_evidence_tool_input_root")?.parse()?;
    if evidence_tool_input_root == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "seed_evidence_root_zero",
            "approved evidence/tool root cannot be the zero sentinel",
        ));
    }
    Ok(SeedRoots {
        authored_semantic_root,
        evidence_tool_input_root,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedRoots {
    pub authored_semantic_root: Sha256Digest,
    pub evidence_tool_input_root: Sha256Digest,
}

struct DefinitionRule {
    rank: u8,
    allowed_tags: &'static [&'static str],
    schema_id: Option<&'static str>,
}

fn definition_rule(kind: &str) -> Result<DefinitionRule, TrustError> {
    let rule = match kind {
        "target_definition" => DefinitionRule {
            rank: 0,
            allowed_tags: &["target-definition"],
            schema_id: None,
        },
        "conditionalization_schema" => DefinitionRule {
            rank: 0,
            allowed_tags: &["conditionalization-schema"],
            schema_id: None,
        },
        "source_interpretation_definition" => DefinitionRule {
            rank: 0,
            allowed_tags: &["source-interpretation"],
            schema_id: None,
        },
        "precondition_definition" => DefinitionRule {
            rank: 0,
            allowed_tags: &["precondition-definition"],
            schema_id: None,
        },
        "carrier_refinement_definition" => DefinitionRule {
            rank: 0,
            allowed_tags: &["carrier-refinement-definition"],
            schema_id: None,
        },
        "build_definition" => DefinitionRule {
            rank: 0,
            allowed_tags: &["build-definition"],
            schema_id: None,
        },
        "basis_fact_class_registry" => DefinitionRule {
            rank: 0,
            allowed_tags: &["basis-fact-class-registry"],
            schema_id: Some("trellis://schemas/basis-fact-class-registry/v1"),
        },
        "semantic_validator" => DefinitionRule {
            rank: 0,
            allowed_tags: &["semantic-validator", "measure-grammar"],
            schema_id: None,
        },
        "evidence_tool_input" => DefinitionRule {
            rank: 0,
            allowed_tags: &["evidence-tool-input"],
            schema_id: None,
        },
        "source_claim_lineage" => DefinitionRule {
            rank: 1,
            allowed_tags: &["source-claim-lineage"],
            schema_id: Some("trellis://schemas/source-claim-lineage/v1"),
        },
        "source_validation_contract" => DefinitionRule {
            rank: 2,
            allowed_tags: &["source-validation-contract"],
            schema_id: Some("trellis://schemas/source-validation-contract/v1"),
        },
        "measure_catalog" => DefinitionRule {
            rank: 3,
            allowed_tags: &["measure-catalog"],
            schema_id: Some("trellis://schemas/measure-catalog/v1"),
        },
        "independent_basis_derivation" => DefinitionRule {
            rank: 4,
            allowed_tags: &["independent-basis-derivation"],
            schema_id: Some("trellis://schemas/independent-basis-derivation/v1"),
        },
        "independent_basis" => DefinitionRule {
            rank: 5,
            allowed_tags: &["independent-basis"],
            schema_id: Some("trellis://schemas/independent-basis/v1"),
        },
        "qualification_profile" => DefinitionRule {
            rank: 6,
            allowed_tags: &["qualification-profile"],
            schema_id: Some("trellis://schemas/qualification-profile/v1"),
        },
        "conditional_theorem_candidate" => DefinitionRule {
            rank: 7,
            allowed_tags: &["conditional-theorem-candidate"],
            schema_id: Some("trellis://schemas/conditional-theorem-candidate/v1"),
        },
        "qualification_profile_catalog" => DefinitionRule {
            rank: 8,
            allowed_tags: &["qualification-profile-catalog"],
            schema_id: Some("trellis://schemas/qualification-profile-catalog/v1"),
        },
        _ => {
            return Err(TrustError::new(
                "unknown_seed_definition_kind",
                format!("unregistered seed definition kind {kind:?}"),
            ))
        }
    };
    Ok(rule)
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "seed_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::SchemaRegistry;

    fn fixture_seed() -> AuthoritativeRecord {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        AuthoritativeRecord::parse(
            &SchemaRegistry::v1().unwrap(),
            fixture["objects"]["seed_manifest"].clone(),
        )
        .unwrap()
    }

    #[test]
    fn registration_fixture_has_recomputable_ordered_root() {
        let roots = validate_seed_manifest_semantics(&fixture_seed()).unwrap();
        assert_ne!(roots.authored_semantic_root, Sha256Digest::ZERO);
    }

    #[test]
    fn reordered_definition_is_rejected_even_after_rehashing() {
        let seed = fixture_seed();
        let mut value = seed.into_value();
        value["definitions"].as_array_mut().unwrap().swap(0, 12);
        let recomputed = tagged_hash(
            DomainTag::AuthoredSemanticRoot,
            &canonical_json_value(&value["definitions"]).unwrap(),
        );
        value["authored_semantic_root"] = Value::String(recomputed.to_string());
        let digest = super::super::canonical::self_digest(
            DomainTag::SeedAuthoredDefinitionManifest,
            &value,
            "manifest_sha256",
        )
        .unwrap();
        value["manifest_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&SchemaRegistry::v1().unwrap(), value).unwrap();
        assert!(validate_seed_manifest_semantics(&record).is_err());
    }
}
