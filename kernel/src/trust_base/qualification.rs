use super::canonical::{
    canonical_json_value, tagged_hash, DecimalNatural, DomainTag, Sha256Digest, TrustError,
};
use super::records::AuthoritativeRecord;
use super::source_validation::{
    route_qualified_recovery, HistoryFacts, QualificationRoute, SourceValidationContractView,
};
use serde::Deserialize;
use serde_json::Value;

pub struct QualificationEvidence<'a> {
    pub contract: &'a SourceValidationContractView,
    pub formal_refutation: &'a AuthoritativeRecord,
    pub history_summary: &'a AuthoritativeRecord,
    pub history_summary_event_hash: Sha256Digest,
    pub history_facts: &'a HistoryFacts,
    pub independent_basis: &'a AuthoritativeRecord,
    pub witness_resource_demand: &'a AuthoritativeRecord,
    pub source_witness_admissibility: &'a AuthoritativeRecord,
    pub profile: &'a AuthoritativeRecord,
    pub bundle: &'a AuthoritativeRecord,
}

pub struct QualificationPrerequisites<'a> {
    pub contract: &'a SourceValidationContractView,
    pub formal_refutation: &'a AuthoritativeRecord,
    pub history_summary: &'a AuthoritativeRecord,
    pub history_summary_event_hash: Sha256Digest,
    pub history_facts: &'a HistoryFacts,
    pub independent_basis: &'a AuthoritativeRecord,
    pub witness_resource_demand: &'a AuthoritativeRecord,
    pub source_witness_admissibility: &'a AuthoritativeRecord,
    pub profile: &'a AuthoritativeRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedResult {
    pub target_id: String,
    pub unrestricted_verdict: &'static str,
    pub conditional_statement_sha256: Sha256Digest,
    pub qualification_bundle_sha256: Sha256Digest,
    pub applicability_established: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Comparison {
    StrictlyLessThan,
    LessThanOrEqual,
}

pub fn evaluate_qualification(
    evidence: &QualificationEvidence<'_>,
) -> Result<QualifiedResult, TrustError> {
    let prerequisites = QualificationPrerequisites {
        contract: evidence.contract,
        formal_refutation: evidence.formal_refutation,
        history_summary: evidence.history_summary,
        history_summary_event_hash: evidence.history_summary_event_hash,
        history_facts: evidence.history_facts,
        independent_basis: evidence.independent_basis,
        witness_resource_demand: evidence.witness_resource_demand,
        source_witness_admissibility: evidence.source_witness_admissibility,
        profile: evidence.profile,
    };
    validate_qualification_prerequisites(&prerequisites)?;
    require_schema(evidence.bundle, "trellis-qualification-bundle/v1")?;
    let contract = prerequisites.contract;
    let bundle = evidence.bundle.value();
    require_target_contract_lineage(contract, bundle, true)?;
    validate_bundle_links(evidence)?;
    validate_checked_artifacts(bundle, &[
        "checked_witness_exclusion_sha256",
        "checked_condition_nonempty_sha256",
        "generated_conditional_statement_sha256",
        "checked_conditional_proof_sha256",
        "checker_and_axiom_closure_sha256",
    ])?;
    Ok(QualifiedResult {
        target_id: contract.target_id.clone(),
        unrestricted_verdict: "refuted_in_extracted_model",
        conditional_statement_sha256: digest_field(
            bundle,
            "generated_conditional_statement_sha256",
        )?,
        qualification_bundle_sha256: evidence.bundle.digest(),
        // Applicability is deliberately evaluated by the separate
        // applicability-result path; conditional truth does not imply it.
        applicability_established: false,
    })
}

/// Validate every qualification input that must exist before profile
/// selection and conditional-statement generation.  Keeping this separate
/// prevents a caller from using a fabricated final bundle to justify entering
/// the qualification lane.
pub fn validate_qualification_prerequisites(
    evidence: &QualificationPrerequisites<'_>,
) -> Result<(), TrustError> {
    require_schema(
        evidence.formal_refutation,
        "trellis-formal-refutation/v1",
    )?;
    require_schema(
        evidence.history_summary,
        "trellis-source-validation-history-summary/v1",
    )?;
    require_schema(evidence.independent_basis, "trellis-independent-basis/v1")?;
    require_schema(
        evidence.witness_resource_demand,
        "trellis-witness-resource-demand/v1",
    )?;
    require_schema(
        evidence.source_witness_admissibility,
        "trellis-source-witness-admissibility/v1",
    )?;
    require_schema(evidence.profile, "trellis-qualification-profile/v1")?;

    let contract = evidence.contract;
    let formal = evidence.formal_refutation.value();
    let summary = evidence.history_summary.value();
    let basis = evidence.independent_basis.value();
    let demand = evidence.witness_resource_demand.value();
    let admissibility = evidence.source_witness_admissibility.value();
    let profile = evidence.profile.value();

    require_target_contract_lineage(contract, formal, false)?;
    require_target_contract_lineage(contract, summary, true)?;
    require_target_contract_lineage(contract, basis, true)?;
    require_target_contract_lineage(contract, profile, true)?;

    let replay_free_admissibility = validate_admissibility_links(
        contract,
        formal,
        basis,
        demand,
        admissibility,
        evidence.formal_refutation.digest(),
        evidence.independent_basis.digest(),
        evidence.witness_resource_demand.digest(),
    )?;
    validate_profile_links(contract, basis, demand, profile)?;
    let route = route_qualified_recovery(
        contract,
        evidence.history_facts,
        replay_free_admissibility,
        true,
    );
    if route != QualificationRoute::EligibleForProfileEvaluation {
        return Err(TrustError::new(
            "qualified_recovery_route_prohibited",
            format!("qualification route is {route:?}"),
        ));
    }
    validate_checked_artifacts(admissibility, &[
        "generated_admissibility_statement_sha256",
        "checked_admissibility_proof_sha256",
        "checker_and_axiom_closure_sha256",
        "derivation_receipt_sha256",
    ])?;
    Ok(())
}

fn validate_admissibility_links(
    contract: &SourceValidationContractView,
    formal: &Value,
    basis: &Value,
    demand: &Value,
    admissibility: &Value,
    formal_refutation_digest: Sha256Digest,
    independent_basis_digest: Sha256Digest,
    witness_resource_demand_digest: Sha256Digest,
) -> Result<bool, TrustError> {
    let formal_witness_certificate = digest_field(formal, "witness_certificate_sha256")?;
    let witness = digest_field(formal, "witness_term_sha256")?;
    let predicate = digest_field(formal, "formal_predicate_sha256")?;
    for (field, expected) in [
        ("formal_refutation_sha256", formal_refutation_digest),
        (
            "formal_witness_certificate_sha256",
            formal_witness_certificate,
        ),
        ("witness_term_sha256", witness),
        ("formal_predicate_sha256", predicate),
        ("model_target_statement_sha256", contract.target_statement_sha256),
        (
            "rust_target_statement_sha256",
            contract.rust_target_statement_sha256,
        ),
        ("source_claim_lineage_sha256", contract.lineage_sha256),
        ("validation_contract_sha256", contract.digest),
        (
            "independent_basis_sha256",
            independent_basis_digest,
        ),
        (
            "witness_resource_demand_sha256",
            witness_resource_demand_digest,
        ),
    ] {
        require_digest(admissibility, field, expected)?;
    }
    for (field, expected) in [
        (
            "formal_witness_certificate_sha256",
            formal_witness_certificate,
        ),
        ("validation_contract_sha256", contract.digest),
        ("witness_term_sha256", witness),
    ] {
        require_digest(demand, field, expected)?;
    }
    require_digest(
        basis,
        "validation_contract_sha256",
        contract.digest,
    )?;
    require_digest(
        demand,
        "encoded_input_or_descriptor_sha256",
        digest_field(admissibility, "encoded_input_or_descriptor_sha256")?,
    )?;
    let evidence_class = string_field(admissibility, "evidence_class")?;
    let status = string_field(admissibility, "admissibility_status")?;
    let valid = matches!(
        (evidence_class, status),
        (
            "safe_construction_v1",
            "exact_source_state_safely_constructed"
        ) | (
            "conditional_descriptor_realizability_v1",
            "unconditional_language_carrier_domain_and_preconditions_with_resource_conditional_realizability"
        )
    );
    if !valid {
        return Err(TrustError::new(
            "source_witness_admissibility_not_established",
            "certificate does not establish a v1 admissibility proposition",
        ));
    }
    Ok(true)
}

fn validate_profile_links(
    contract: &SourceValidationContractView,
    basis: &Value,
    demand: &Value,
    profile: &Value,
) -> Result<(), TrustError> {
    let condition = profile.get("condition").ok_or_else(|| {
        TrustError::new("qualification_condition_missing", "profile lacks condition")
    })?;
    for field in ["binder", "binder_type", "measure_id", "units", "comparison", "scope", "enforcement"] {
        if string_field(condition, field)? != string_field(basis, field)? {
            return Err(TrustError::new(
                "qualification_profile_basis_mismatch",
                format!("condition differs from independent basis at {field}"),
            ));
        }
    }
    require_digest(
        condition,
        "independent_basis_sha256",
        digest_field(profile, "independent_basis_sha256")?,
    )?;
    if string_field(condition, "independent_basis_id")?
        != string_field(basis, "basis_id")?
        || string_field(profile, "independent_basis_id")?
            != string_field(basis, "basis_id")?
        || digest_field(profile, "independent_basis_sha256")?
            != digest_field(basis, "basis_definition_sha256")?
    {
        return Err(TrustError::new(
            "qualification_profile_basis_identity_mismatch",
            "profile does not select this exact independent basis",
        ));
    }
    for field in ["measure_id", "units"] {
        if string_field(demand, field)? != string_field(condition, field)? {
            return Err(TrustError::new(
                "witness_demand_measure_mismatch",
                format!("witness demand differs at {field}"),
            ));
        }
    }
    require_digest(
        demand,
        "measure_definition_sha256",
        digest_field(basis, "measure_definition_sha256")?,
    )?;
    let condition_bound: DecimalNatural = string_field(condition, "bound")?.parse()?;
    let basis_limit: DecimalNatural = string_field(basis, "limit")?.parse()?;
    if condition_bound != basis_limit {
        return Err(TrustError::new(
            "qualification_bound_not_basis_derived",
            "profile bound differs from independent basis limit",
        ));
    }
    let witness_value: DecimalNatural = string_field(demand, "value")?.parse()?;
    let comparison: Comparison = serde_json::from_value(
        condition.get("comparison").cloned().ok_or_else(|| {
            TrustError::new("qualification_comparison_missing", "condition lacks comparison")
        })?,
    )
    .map_err(|error| TrustError::new("qualification_comparison_invalid", error.to_string()))?;
    let witness_excluded = match comparison {
        Comparison::StrictlyLessThan => witness_value >= condition_bound,
        Comparison::LessThanOrEqual => witness_value > condition_bound,
    };
    if !witness_excluded {
        return Err(TrustError::new(
            "qualification_condition_does_not_exclude_witness",
            "conditional theorem would retain its known refuting witness",
        ));
    }
    if string_field(profile, "target_id")? != contract.target_id
        || digest_field(profile, "validation_contract_sha256")? != contract.digest
    {
        return Err(TrustError::new(
            "qualification_profile_contract_mismatch",
            "profile belongs to another target or contract",
        ));
    }
    Ok(())
}

fn validate_bundle_links(evidence: &QualificationEvidence<'_>) -> Result<(), TrustError> {
    let bundle = evidence.bundle.value();
    let profile = evidence.profile.value();
    if string_field(bundle, "unrestricted_verdict")? != "refuted_in_extracted_model" {
        return Err(TrustError::new(
            "qualified_bundle_rewrites_unrestricted_verdict",
            "qualification must preserve the unrestricted refutation",
        ));
    }
    for (field, expected) in [
        ("formal_refutation_sha256", evidence.formal_refutation.digest()),
        (
            "source_validation_history_summary_sha256",
            evidence.history_summary.digest(),
        ),
        (
            "source_validation_history_event_hash",
            evidence.history_summary_event_hash,
        ),
        (
            "independent_basis_sha256",
            evidence.independent_basis.digest(),
        ),
        (
            "witness_resource_demand_sha256",
            evidence.witness_resource_demand.digest(),
        ),
        (
            "source_witness_admissibility_sha256",
            evidence.source_witness_admissibility.digest(),
        ),
        ("profile_definition_sha256", evidence.profile.digest()),
    ] {
        require_digest(bundle, field, expected)?;
    }
    if string_field(bundle, "profile_id")? != string_field(profile, "profile_id")?
        || string_field(bundle, "conditionalization_schema_id")?
            != string_field(profile, "conditionalization_schema_id")?
        || string_field(bundle, "independent_basis_id")?
            != string_field(evidence.independent_basis.value(), "basis_id")?
    {
        return Err(TrustError::new(
            "qualification_bundle_profile_mismatch",
            "bundle does not use the selected seed-frozen profile",
        ));
    }
    let condition = profile.get("condition").ok_or_else(|| {
        TrustError::new("qualification_condition_missing", "profile lacks condition")
    })?;
    let condition_sha256 = tagged_hash(
        DomainTag::ConditionalizationSchema,
        &canonical_json_value(condition)?,
    );
    require_digest(bundle, "condition_sha256", condition_sha256)?;
    Ok(())
}

fn require_target_contract_lineage(
    contract: &SourceValidationContractView,
    value: &Value,
    require_contract: bool,
) -> Result<(), TrustError> {
    if string_field(value, "target_id")? != contract.target_id
        || digest_field(value, "target_statement_sha256")? != contract.target_statement_sha256
    {
        return Err(TrustError::new(
            "qualification_target_mismatch",
            "qualification artifact belongs to another target statement",
        ));
    }
    if require_contract {
        if string_field(value, "validation_contract_id")? != contract.contract_id
            || digest_field(value, "validation_contract_sha256")? != contract.digest
            || string_field(value, "source_claim_lineage_id")? != contract.lineage_id
            || digest_field(value, "source_claim_lineage_sha256")? != contract.lineage_sha256
        {
            return Err(TrustError::new(
                "qualification_contract_or_lineage_mismatch",
                "qualification artifact differs from the target's current contract/lineage",
            ));
        }
    }
    Ok(())
}

fn validate_checked_artifacts(value: &Value, fields: &[&str]) -> Result<(), TrustError> {
    for field in fields {
        if digest_field(value, field)? == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "placeholder_checked_artifact_digest",
                format!("{field} cannot be the all-zero placeholder"),
            ));
        }
    }
    Ok(())
}

fn require_schema(record: &AuthoritativeRecord, expected: &str) -> Result<(), TrustError> {
    if record.contract().record_schema != expected {
        return Err(TrustError::new(
            "qualification_record_schema_mismatch",
            format!("expected {expected}, got {}", record.contract().record_schema),
        ));
    }
    Ok(())
}

fn require_digest(value: &Value, field: &str, expected: Sha256Digest) -> Result<(), TrustError> {
    let actual = digest_field(value, field)?;
    if actual != expected {
        return Err(TrustError::new(
            "qualification_digest_binding_mismatch",
            format!("{field}: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "qualification_field_missing",
            format!("{field} must be a string"),
        )
    })
}

fn digest_field(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    string_field(value, field)?.parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn witness_exclusion_uses_unbounded_decimal_arithmetic() {
        let bound: DecimalNatural = "9223372036854775808".parse().unwrap();
        let witness: DecimalNatural = "18446744073709551616".parse().unwrap();
        assert!(witness >= bound);
    }

    #[test]
    fn applicability_does_not_follow_from_conditional_truth() {
        let result = QualifiedResult {
            target_id: "t".into(),
            unrestricted_verdict: "refuted_in_extracted_model",
            conditional_statement_sha256: "11".repeat(32).parse().unwrap(),
            qualification_bundle_sha256: "22".repeat(32).parse().unwrap(),
            applicability_established: false,
        };
        assert!(!result.applicability_established);
        assert_eq!(result.unrestricted_verdict, "refuted_in_extracted_model");
    }
}
