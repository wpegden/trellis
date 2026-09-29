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
    // W9 (the v2.4 seed member from W10): a prose campaign's GOAL referent
    // travels IN the authoritative seed record — digest AND bytes, so the
    // gate package is self-contained. The pair is all-or-nothing and must
    // cohere: sha256(pv_goal_prose_utf8) == pv_goal_prose_sha256.
    let goal_digest = value.get("pv_goal_prose_sha256");
    let goal_bytes = value.get("pv_goal_prose_utf8");
    let pv_goal_prose_sha256 = match (goal_digest, goal_bytes) {
        (None, None) => None,
        (Some(digest), Some(bytes)) => {
            let digest: Sha256Digest = digest
                .as_str()
                .ok_or_else(|| {
                    TrustError::new(
                        "seed_goal_prose_invalid",
                        "pv_goal_prose_sha256 must be a hex string",
                    )
                })?
                .parse()?;
            let bytes = bytes.as_str().ok_or_else(|| {
                TrustError::new(
                    "seed_goal_prose_invalid",
                    "pv_goal_prose_utf8 must be a string",
                )
            })?;
            let recomputed = super::canonical::raw_sha256(bytes.as_bytes());
            if recomputed != digest {
                return Err(TrustError::new(
                    "seed_goal_prose_digest_mismatch",
                    format!(
                        "pv_goal_prose_sha256 {digest} does not hash the embedded \
                         pv_goal_prose_utf8 bytes (recomputed {recomputed})"
                    ),
                ));
            }
            Some(digest)
        }
        _ => {
            return Err(TrustError::new(
                "seed_goal_prose_invalid",
                "pv_goal_prose_sha256 and pv_goal_prose_utf8 travel together: the seed \
                 record carries the prose GOAL digest AND bytes, or neither",
            ));
        }
    };
    Ok(SeedRoots {
        authored_semantic_root,
        evidence_tool_input_root,
        pv_goal_prose_sha256,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedRoots {
    pub authored_semantic_root: Sha256Digest,
    pub evidence_tool_input_root: Sha256Digest,
    /// W9: the seed-carried prose GOAL referent digest (coherence with the
    /// embedded bytes already checked). `None` for mode-B/math seeds.
    pub pv_goal_prose_sha256: Option<Sha256Digest>,
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
        "evidence_tool_input" => DefinitionRule {
            rank: 0,
            allowed_tags: &["evidence-tool-input"],
            schema_id: None,
        },
        "adaptation_ledger_row" => DefinitionRule {
            rank: 0,
            allowed_tags: &["adaptation-ledger-row"],
            schema_id: None,
        },
        "phase0_source_adaptation" => DefinitionRule {
            rank: 0,
            allowed_tags: &["raw-artifact"],
            schema_id: Some("trellis://campaign/phase0-source-adaptation/v1"),
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

    /// Inline replacement for the retired REGISTRATION_HASH_DAG fixture
    /// (deleted with the journal machinery, Stage 3 plan doc 32): a minimal
    /// schema-valid seed manifest with two retained rank-0 definitions,
    /// authored root and self digest recomputed.
    fn fixture_seed_value() -> Value {
        let definitions = serde_json::json!([
            {
                "record_kind": "target_definition",
                "record_id": "target-1",
                "record_schema_id": "trellis://campaign/target-definition/v1",
                "record_sha256": "11".repeat(32),
                "domain_tag": "target-definition",
            },
            {
                "record_kind": "conditionalization_schema",
                "record_id": "conditionalization-v1",
                "record_schema_id": "trellis://campaign/conditionalization-schema/v1",
                "record_sha256": "22".repeat(32),
                "domain_tag": "conditionalization-schema",
            }
        ]);
        let authored_root = tagged_hash(
            DomainTag::AuthoredSemanticRoot,
            &canonical_json_value(&definitions).unwrap(),
        );
        let mut seed = serde_json::json!({
            "schema": "trellis-seed-authored-definition-manifest/v1",
            "protocol_id": "trellis-trust-v1",
            "run_id": "fixture-run",
            "seed_plan_id": "fixture-plan",
            "definitions": definitions,
            "authored_semantic_root": authored_root,
            "approved_evidence_tool_input_root": "33".repeat(32),
            "manifest_sha256": Sha256Digest::ZERO,
        });
        let digest = super::super::canonical::self_digest(
            DomainTag::SeedAuthoredDefinitionManifest,
            &seed,
            "manifest_sha256",
        )
        .unwrap();
        seed["manifest_sha256"] = Value::String(digest.to_string());
        seed
    }

    fn fixture_seed() -> AuthoritativeRecord {
        AuthoritativeRecord::parse(&SchemaRegistry::v1().unwrap(), fixture_seed_value()).unwrap()
    }

    #[test]
    fn inline_seed_manifest_has_recomputable_ordered_root() {
        let roots = validate_seed_manifest_semantics(&fixture_seed()).unwrap();
        assert_ne!(roots.authored_semantic_root, Sha256Digest::ZERO);
    }

    #[test]
    fn same_rank_definition_reordering_is_valid_after_rehashing() {
        let seed = fixture_seed();
        let mut value = seed.into_value();
        value["definitions"].as_array_mut().unwrap().swap(0, 1);
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
        validate_seed_manifest_semantics(&record)
            .expect("definitions in the same reduced registration class have no authority order");
    }

    /// W9 (the v2.4 seed member): the prose GOAL referent travels in the
    /// authoritative seed record as a coherent digest+bytes PAIR — a valid
    /// pair validates and surfaces its digest in the roots; a digest that
    /// does not hash the embedded bytes refuses; a one-sided member
    /// refuses; and the member's absence stays fully valid (mode-B seeds
    /// byte-identical). Fail-before: the schema's additionalProperties
    /// gate rejected the members outright, so no seed could carry the
    /// referent at all.
    #[test]
    fn seed_goal_prose_member_travels_coherently_or_not_at_all() {
        let reseal = |mut value: Value| -> AuthoritativeRecord {
            value["manifest_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
            let digest = super::super::canonical::self_digest(
                DomainTag::SeedAuthoredDefinitionManifest,
                &value,
                "manifest_sha256",
            )
            .unwrap();
            value["manifest_sha256"] = Value::String(digest.to_string());
            AuthoritativeRecord::parse(&SchemaRegistry::v1().unwrap(), value).unwrap()
        };
        let goal = "Verify f.\n";
        let goal_digest = super::super::canonical::raw_sha256(goal.as_bytes());

        // Coherent pair: accepted, digest surfaced.
        let mut value = fixture_seed_value();
        value["pv_goal_prose_sha256"] = Value::String(goal_digest.to_string());
        value["pv_goal_prose_utf8"] = Value::String(goal.into());
        let roots = validate_seed_manifest_semantics(&reseal(value)).unwrap();
        assert_eq!(roots.pv_goal_prose_sha256, Some(goal_digest));

        // Member absent: valid, no digest.
        let roots = validate_seed_manifest_semantics(&fixture_seed()).unwrap();
        assert_eq!(roots.pv_goal_prose_sha256, None);

        // Digest that does not hash the bytes: refused by name.
        let mut value = fixture_seed_value();
        value["pv_goal_prose_sha256"] = Value::String("44".repeat(32));
        value["pv_goal_prose_utf8"] = Value::String(goal.into());
        let error = validate_seed_manifest_semantics(&reseal(value)).unwrap_err();
        assert_eq!(error.code, "seed_goal_prose_digest_mismatch");

        // One-sided members: refused by name.
        for member in ["pv_goal_prose_sha256", "pv_goal_prose_utf8"] {
            let mut value = fixture_seed_value();
            if member == "pv_goal_prose_sha256" {
                value[member] = Value::String(goal_digest.to_string());
            } else {
                value[member] = Value::String(goal.into());
            }
            let error = validate_seed_manifest_semantics(&reseal(value)).unwrap_err();
            assert_eq!(error.code, "seed_goal_prose_invalid", "{member}");
        }
    }

    #[test]
    fn retired_seed_definition_kinds_fail_loud() {
        assert!(definition_rule("qualification_profile").is_err());
        assert!(definition_rule("basis_fact_class_registry").is_err());
        assert!(definition_rule("measure_catalog").is_err());
        assert!(definition_rule("conditional_theorem_candidate").is_err());
    }
}
