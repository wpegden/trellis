use super::canonical::{Sha256Digest, TrustError};
use super::records::AuthoritativeRecord;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceValidationMethod {
    CheckedRefutationReflectionV1,
    ExactRustExecutionV1,
    NotDefinedForClaimShapeV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelNegativeCarrierRequirement {
    WitnessSpecific,
    TheoremLevel,
}

impl SourceValidationMethod {
    /// The approved contract, never target identity or theorem syntax,
    /// decides which checked model-negative carrier the runtime must await.
    pub fn model_negative_carrier_requirement(self) -> ModelNegativeCarrierRequirement {
        match self {
            Self::ExactRustExecutionV1 => ModelNegativeCarrierRequirement::WitnessSpecific,
            Self::CheckedRefutationReflectionV1 | Self::NotDefinedForClaimShapeV1 => {
                ModelNegativeCarrierRequirement::TheoremLevel
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationPermission {
    ResourceProfilesIfIndependentScopeLimit,
    Prohibited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimShape {
    Universal,
    Existential,
    Implication,
    TerminationOrLiveness,
    TraceProperty,
    RelationalOrHyperproperty,
    HigherOrderOrAbstractDomain,
    Other,
}

#[derive(Clone, Debug)]
pub struct SourceValidationContractView {
    pub contract_id: String,
    pub digest: Sha256Digest,
    pub target_id: String,
    pub target_statement_sha256: Sha256Digest,
    pub lineage_id: String,
    pub lineage_sha256: Sha256Digest,
    pub rust_target_statement_sha256: Sha256Digest,
    pub claim_shape: ClaimShape,
    pub method: SourceValidationMethod,
    pub qualification_permission: QualificationPermission,
    pub source_validator_sha256: Option<Sha256Digest>,
    pub observation_oracle_sha256: Option<Sha256Digest>,
    pub reflection_theorem_sha256: Option<Sha256Digest>,
    pub reflection_proof_artifact_sha256: Option<Sha256Digest>,
    pub reflection_checker_sha256: Option<Sha256Digest>,
    pub reflection_result_schema_id: Option<String>,
    pub harness_cohort_basis: Option<HarnessCohortBasis>,
    pub preconditions: BTreeMap<String, PreconditionSpec>,
}

#[derive(Clone, Debug)]
pub struct PreconditionSpec {
    pub normalized_statement_sha256: Sha256Digest,
    pub checker_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct HarnessCohortBasis {
    pub source_tree_sha256: Sha256Digest,
    pub toolchain_build_basis_sha256: Sha256Digest,
    pub runner_sha256: Sha256Digest,
    pub environment_contract_sha256: Sha256Digest,
    pub concretization_schema_sha256: Sha256Digest,
    pub erasure_relation_sha256: Sha256Digest,
    pub raw_observation_schema_sha256: Sha256Digest,
}

impl SourceValidationContractView {
    pub fn from_record(record: &AuthoritativeRecord) -> Result<Self, TrustError> {
        if record.contract().record_schema != "trellis-source-validation-contract/v1" {
            return Err(TrustError::new(
                "wrong_source_validation_contract_schema",
                "expected a source-validation contract",
            ));
        }
        let value = record.value();
        let method: SourceValidationMethod = decode_field(value, "validation_method")?;
        let claim_shape: ClaimShape = decode_field(value, "claim_shape")?;
        let qualification_permission: QualificationPermission =
            decode_field(value, "qualification_permission")?;
        let source_validator_sha256 = optional_digest(value, "source_validator_sha256")?;
        let observation_oracle_sha256 = optional_digest(value, "observation_oracle_sha256")?;
        let reflection_theorem_sha256 = optional_digest(value, "reflection_theorem_sha256")?;
        let reflection_proof_artifact_sha256 =
            optional_digest(value, "reflection_proof_artifact_sha256")?;
        let reflection_checker_sha256 = optional_digest(value, "reflection_checker_sha256")?;
        let reflection_result_schema_id = value
            .get("reflection_result_schema_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let harness_cohort_basis = if method == SourceValidationMethod::ExactRustExecutionV1 {
            Some(HarnessCohortBasis {
                source_tree_sha256: digest_field(value, "source_tree_sha256")?,
                toolchain_build_basis_sha256: digest_field(
                    value,
                    "toolchain_build_basis_sha256",
                )?,
                runner_sha256: digest_field(value, "runner_sha256")?,
                environment_contract_sha256: digest_field(value, "environment_contract_sha256")?,
                concretization_schema_sha256: digest_field(
                    value,
                    "concretization_schema_sha256",
                )?,
                erasure_relation_sha256: digest_field(value, "erasure_relation_sha256")?,
                raw_observation_schema_sha256: digest_field(
                    value,
                    "raw_observation_schema_sha256",
                )?,
            })
        } else {
            None
        };
        let mut precondition_specs = BTreeMap::new();
        if let Some(preconditions) = value.get("preconditions").and_then(Value::as_array) {
            for precondition in preconditions {
                let id = string_field(precondition, "precondition_id")?.to_owned();
                if precondition_specs
                    .insert(
                        id.clone(),
                        PreconditionSpec {
                            normalized_statement_sha256: digest_field(
                                precondition,
                                "normalized_statement_sha256",
                            )?,
                            checker_sha256: digest_field(precondition, "checker_sha256")?,
                        },
                    )
                    .is_some()
                {
                    return Err(TrustError::new(
                        "duplicate_source_precondition",
                        format!("duplicate precondition {id}"),
                    ));
                }
            }
        }
        let view = Self {
            contract_id: string_field(value, "contract_id")?.into(),
            digest: record.digest(),
            target_id: string_field(value, "target_id")?.into(),
            target_statement_sha256: digest_field(value, "target_statement_sha256")?,
            lineage_id: string_field(value, "source_claim_lineage_id")?.into(),
            lineage_sha256: digest_field(value, "source_claim_lineage_sha256")?,
            rust_target_statement_sha256: digest_field(value, "rust_target_statement_sha256")?,
            claim_shape,
            method,
            qualification_permission,
            source_validator_sha256,
            observation_oracle_sha256,
            reflection_theorem_sha256,
            reflection_proof_artifact_sha256,
            reflection_checker_sha256,
            reflection_result_schema_id,
            harness_cohort_basis,
            preconditions: precondition_specs,
        };
        view.validate_semantics(value)?;
        Ok(view)
    }

    fn validate_semantics(&self, value: &Value) -> Result<(), TrustError> {
        match self.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                if self.qualification_permission != QualificationPermission::Prohibited
                    || self.source_validator_sha256.is_some()
                    || self.observation_oracle_sha256.is_some()
                    || self.harness_cohort_basis.is_some()
                    || self.reflection_theorem_sha256.is_none()
                    || self.reflection_theorem_sha256 == Some(Sha256Digest::ZERO)
                    || self.reflection_proof_artifact_sha256.is_none()
                    || self.reflection_proof_artifact_sha256 == Some(Sha256Digest::ZERO)
                    || self.reflection_checker_sha256.is_none()
                    || self.reflection_checker_sha256 == Some(Sha256Digest::ZERO)
                    || self.reflection_result_schema_id.as_deref()
                        != Some("trellis://schemas/reflection-validation-result/v1")
                {
                    return Err(TrustError::new(
                        "reflection_contract_has_execution_authority",
                        "checked reflection needs non-placeholder theorem, proof, and checker pins and cannot carry replay or qualification authority",
                    ));
                }
            }
            SourceValidationMethod::ExactRustExecutionV1 => {
                if self.claim_shape != ClaimShape::Universal
                    || self.source_validator_sha256.is_none()
                    || self.observation_oracle_sha256.is_none()
                    || self.harness_cohort_basis.is_none()
                    || self.reflection_theorem_sha256.is_some()
                    || self.reflection_proof_artifact_sha256.is_some()
                    || self.reflection_checker_sha256.is_some()
                    || self.reflection_result_schema_id.is_some()
                {
                    return Err(TrustError::new(
                        "exact_execution_contract_not_finite_universal",
                        "v1 exact execution needs the registered finite deterministic universal class",
                    ));
                }
                let preconditions = value
                    .get("preconditions")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        TrustError::new(
                            "source_preconditions_missing",
                            "exact execution contract needs typed preconditions",
                        )
                    })?;
                if preconditions.len() != self.preconditions.len() || self.preconditions.is_empty() {
                    return Err(TrustError::new(
                        "source_precondition_registry_invalid",
                        "exact execution needs a non-empty unique precondition registry",
                    ));
                }
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                if self.qualification_permission != QualificationPermission::Prohibited
                    || self.source_validator_sha256.is_some()
                    || self.observation_oracle_sha256.is_some()
                    || self.harness_cohort_basis.is_some()
                    || self.reflection_theorem_sha256.is_some()
                    || self.reflection_proof_artifact_sha256.is_some()
                    || self.reflection_checker_sha256.is_some()
                    || self.reflection_result_schema_id.is_some()
                {
                    return Err(TrustError::new(
                        "undefined_contract_has_operational_authority",
                        "unsupported claim shapes must fail closed",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayRole {
    ExactWitness,
    PositiveControl,
    CorroboratingProxy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    ExactCounterexampleObserved,
    Reproduced,
    SourceModelMismatch,
    LanguageInadmissible,
    IndependentScopeLimit,
    ValidationBudgetExceeded,
    HarnessPolicyRefusal,
    ControlPassed,
    ControlFailed,
    ProxyExecuted,
    ValidationInconclusive,
    NotDefinedForClaimShape,
    InvalidValidationEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decisiveness {
    Decisive,
    Corroborative,
    Inconclusive,
    UnsupportedClaimClass,
    Invalid,
}

#[derive(Clone, Debug)]
pub struct ValidatedOutcome {
    pub digest: Sha256Digest,
    pub role: Option<ReplayRole>,
    pub status: ValidationStatus,
    pub decisiveness: Decisiveness,
    pub harness_cohort_sha256: Option<Sha256Digest>,
    pub independent_basis_sha256: Option<Sha256Digest>,
    pub witness_resource_demand_sha256: Option<Sha256Digest>,
}

pub fn validate_outcome(
    contract: &SourceValidationContractView,
    outcome: &AuthoritativeRecord,
    attempt: Option<&AuthoritativeRecord>,
) -> Result<ValidatedOutcome, TrustError> {
    if outcome.contract().record_schema != "trellis-source-validation-outcome/v1" {
        return Err(TrustError::new(
            "wrong_source_validation_outcome_schema",
            "expected a source-validation outcome",
        ));
    }
    let value = outcome.value();
    require_contract_binding(contract, value)?;
    let method: SourceValidationMethod = decode_field(value, "validation_method")?;
    if method != contract.method {
        return Err(TrustError::new(
            "source_validation_method_mismatch",
            "outcome method differs from gate-frozen contract",
        ));
    }
    let outcome_validator = optional_digest(value, "source_validator_sha256")?;
    match contract.method {
        SourceValidationMethod::ExactRustExecutionV1
            if outcome_validator != contract.source_validator_sha256 =>
        {
            return Err(TrustError::new(
                "source_validator_mismatch",
                "execution outcome does not bind the contract's exact source validator",
            ));
        }
        SourceValidationMethod::ExactRustExecutionV1 => {}
        _ if outcome_validator.is_some() => {
            return Err(TrustError::new(
                "non_execution_outcome_has_source_validator",
                "reflection/undefined outcomes cannot acquire execution authority",
            ));
        }
        _ => {}
    }
    let status: ValidationStatus = decode_field(value, "status")?;
    let role: Option<ReplayRole> = optional_decode_field(value, "role")?;
    let decisiveness: Decisiveness = decode_field(value, "decisiveness")?;
    let attempt_digest = optional_digest(value, "attempt_sha256")?;
    match (method, attempt, attempt_digest) {
        (SourceValidationMethod::ExactRustExecutionV1, Some(attempt), Some(digest)) => {
            validate_attempt(contract, attempt)?;
            if attempt.digest() != digest {
                return Err(TrustError::new(
                    "source_attempt_digest_mismatch",
                    "outcome names a different validation attempt",
                ));
            }
            let attempt_role: ReplayRole = decode_field(attempt.value(), "role")?;
            if role != Some(attempt_role) {
                return Err(TrustError::new(
                    "source_attempt_role_mismatch",
                    "outcome role differs from attempt role",
                ));
            }
            for field in ["harness_cohort_sha256", "full_tuple_sha256"] {
                if digest_field(value, field)? != digest_field(attempt.value(), field)? {
                    return Err(TrustError::new(
                        "source_attempt_tuple_mismatch",
                        format!("outcome and attempt differ at {field}"),
                    ));
                }
            }
        }
        (SourceValidationMethod::ExactRustExecutionV1, _, _) => {
            return Err(TrustError::new(
                "exact_source_attempt_missing",
                "exact execution outcome must bind its raw attempt",
            ))
        }
        (_, None, None) => {}
        _ => {
            return Err(TrustError::new(
                "unexpected_source_attempt",
                "reflection/undefined results cannot bind execution attempts",
            ))
        }
    }
    validate_role_status(role, status, decisiveness)?;
    Ok(ValidatedOutcome {
        digest: outcome.digest(),
        role,
        status,
        decisiveness,
        harness_cohort_sha256: optional_digest(value, "harness_cohort_sha256")?,
        independent_basis_sha256: optional_digest(value, "independent_basis_sha256")?,
        witness_resource_demand_sha256: optional_digest(
            value,
            "witness_resource_demand_sha256",
        )?,
    })
}

pub fn validate_attempt(
    contract: &SourceValidationContractView,
    attempt: &AuthoritativeRecord,
) -> Result<(), TrustError> {
    if attempt.contract().record_schema != "trellis-source-validation-attempt/v1" {
        return Err(TrustError::new(
            "wrong_source_validation_attempt_schema",
            "expected a source-validation attempt",
        ));
    }
    if contract.method != SourceValidationMethod::ExactRustExecutionV1 {
        return Err(TrustError::new(
            "execution_attempt_not_defined",
            "contract does not permit exact Rust execution",
        ));
    }
    let value = attempt.value();
    require_contract_binding(contract, value)?;
    let cohort = contract.harness_cohort_basis.as_ref().ok_or_else(|| {
        TrustError::new("harness_cohort_basis_missing", "exact contract has no cohort basis")
    })?;
    for (field, expected) in [
        ("source_tree_sha256", cohort.source_tree_sha256),
        (
            "toolchain_build_basis_sha256",
            cohort.toolchain_build_basis_sha256,
        ),
        ("runner_binary_sha256", cohort.runner_sha256),
        (
            "environment_contract_sha256",
            cohort.environment_contract_sha256,
        ),
        (
            "concretization_schema_sha256",
            cohort.concretization_schema_sha256,
        ),
        ("erasure_relation_sha256", cohort.erasure_relation_sha256),
        (
            "raw_observation_schema_sha256",
            cohort.raw_observation_schema_sha256,
        ),
    ] {
        if digest_field(value, field)? != expected {
            return Err(TrustError::new(
                "attempt_contract_closure_mismatch",
                format!("attempt differs from contract at {field}"),
            ));
        }
    }
    let execution = value.get("execution").ok_or_else(|| {
        TrustError::new("attempt_execution_missing", "attempt lacks execution record")
    })?;
    if execution
        .get("source_call_started")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !execution
            .get("preflight_performed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err(TrustError::new(
            "attempt_bypassed_preflight",
            "source call started without non-allocating preflight",
        ));
    }
    let preconditions = value
        .get("precondition_results")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "attempt_precondition_results_missing",
                "attempt lacks typed precondition results",
            )
        })?;
    let mut ids = BTreeSet::new();
    for result in preconditions {
        let id = string_field(result, "precondition_id")?;
        if !ids.insert(id) {
            return Err(TrustError::new(
                "duplicate_attempt_precondition_result",
                format!("duplicate result for {id}"),
            ));
        }
        let expected = contract.preconditions.get(id).ok_or_else(|| {
            TrustError::new(
                "unexpected_attempt_precondition",
                format!("attempt contains unregistered precondition {id}"),
            )
        })?;
        if digest_field(result, "normalized_statement_sha256")?
            != expected.normalized_statement_sha256
            || digest_field(result, "checker_sha256")? != expected.checker_sha256
        {
            return Err(TrustError::new(
                "attempt_precondition_definition_mismatch",
                format!("attempt changed the registered definition of {id}"),
            ));
        }
    }
    let expected_ids: BTreeSet<&str> = contract.preconditions.keys().map(String::as_str).collect();
    if ids != expected_ids {
        return Err(TrustError::new(
            "attempt_precondition_results_incomplete",
            "attempt must contain exactly one result for every contract precondition",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryFacts {
    pub decisive_source_refutation_present: bool,
    pub source_model_mismatch_unresolved: bool,
    pub language_inadmissibility_unresolved: bool,
    pub contract_invalid: bool,
    pub independent_scope_limit_present: bool,
    pub failed_harness_cohorts: BTreeSet<Sha256Digest>,
    pub latest_by_role: BTreeMap<String, Sha256Digest>,
}

impl HistoryFacts {
    pub fn derive(outcomes: &[ValidatedOutcome]) -> Self {
        let mut facts = Self::default();
        for outcome in outcomes {
            if let Some(role) = outcome.role {
                facts
                    .latest_by_role
                    .insert(format!("{role:?}"), outcome.digest);
            }
            match outcome.status {
                ValidationStatus::ExactCounterexampleObserved | ValidationStatus::Reproduced => {
                    facts.decisive_source_refutation_present = true;
                }
                ValidationStatus::SourceModelMismatch => {
                    facts.source_model_mismatch_unresolved = true;
                }
                ValidationStatus::LanguageInadmissible => {
                    facts.language_inadmissibility_unresolved = true;
                }
                ValidationStatus::IndependentScopeLimit => {
                    facts.independent_scope_limit_present = true;
                }
                ValidationStatus::ControlFailed | ValidationStatus::InvalidValidationEvidence => {
                    if let Some(cohort) = outcome.harness_cohort_sha256 {
                        facts.failed_harness_cohorts.insert(cohort);
                    }
                }
                _ => {}
            }
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualificationRoute {
    ProhibitedCheckedReflection,
    ProhibitedDecisiveSourceRefutation,
    HaltSourceModelMismatch,
    CorrectBoundaryOrAdmissibility,
    ProhibitedUnsupportedClaimShape,
    ProhibitedInvalidContract,
    EligibleForProfileEvaluation,
    NoIndependentQualificationBasis,
}

pub fn route_qualified_recovery(
    contract: &SourceValidationContractView,
    history: &HistoryFacts,
    replay_free_admissibility_established: bool,
    independent_profile_available: bool,
) -> QualificationRoute {
    if history.source_model_mismatch_unresolved {
        return QualificationRoute::HaltSourceModelMismatch;
    }
    if history.decisive_source_refutation_present {
        return QualificationRoute::ProhibitedDecisiveSourceRefutation;
    }
    if history.language_inadmissibility_unresolved {
        return QualificationRoute::CorrectBoundaryOrAdmissibility;
    }
    if history.contract_invalid {
        return QualificationRoute::ProhibitedInvalidContract;
    }
    match contract.method {
        SourceValidationMethod::CheckedRefutationReflectionV1 => {
            QualificationRoute::ProhibitedCheckedReflection
        }
        SourceValidationMethod::NotDefinedForClaimShapeV1 => {
            QualificationRoute::ProhibitedUnsupportedClaimShape
        }
        SourceValidationMethod::ExactRustExecutionV1 => {
            if contract.qualification_permission
                != QualificationPermission::ResourceProfilesIfIndependentScopeLimit
                || !replay_free_admissibility_established
                || !independent_profile_available
            {
                QualificationRoute::NoIndependentQualificationBasis
            } else {
                QualificationRoute::EligibleForProfileEvaluation
            }
        }
    }
}

fn validate_role_status(
    role: Option<ReplayRole>,
    status: ValidationStatus,
    decisiveness: Decisiveness,
) -> Result<(), TrustError> {
    let valid = match status {
        ValidationStatus::ExactCounterexampleObserved | ValidationStatus::Reproduced => {
            role == Some(ReplayRole::ExactWitness) && decisiveness == Decisiveness::Decisive
        }
        ValidationStatus::ControlPassed => {
            role == Some(ReplayRole::PositiveControl)
                && decisiveness == Decisiveness::Corroborative
        }
        ValidationStatus::ControlFailed => {
            role == Some(ReplayRole::PositiveControl) && decisiveness == Decisiveness::Invalid
        }
        ValidationStatus::ProxyExecuted => {
            role == Some(ReplayRole::CorroboratingProxy)
                && decisiveness == Decisiveness::Corroborative
        }
        ValidationStatus::SourceModelMismatch | ValidationStatus::LanguageInadmissible => {
            role == Some(ReplayRole::ExactWitness) && decisiveness == Decisiveness::Invalid
        }
        ValidationStatus::IndependentScopeLimit
        | ValidationStatus::ValidationBudgetExceeded
        | ValidationStatus::HarnessPolicyRefusal
        | ValidationStatus::ValidationInconclusive => decisiveness == Decisiveness::Inconclusive,
        ValidationStatus::NotDefinedForClaimShape => {
            role.is_none() && decisiveness == Decisiveness::UnsupportedClaimClass
        }
        ValidationStatus::InvalidValidationEvidence => decisiveness == Decisiveness::Invalid,
    };
    if !valid {
        return Err(TrustError::new(
            "invalid_source_status_role_combination",
            "central status, role, and decisiveness are inconsistent",
        ));
    }
    Ok(())
}

fn require_contract_binding(
    contract: &SourceValidationContractView,
    value: &Value,
) -> Result<(), TrustError> {
    for (field, expected) in [
        ("validation_contract_sha256", contract.digest),
        ("source_claim_lineage_sha256", contract.lineage_sha256),
    ] {
        if digest_field(value, field)? != expected {
            return Err(TrustError::new(
                "source_record_contract_binding_mismatch",
                format!("source record differs at {field}"),
            ));
        }
    }
    for (field, expected) in [
        ("target_id", contract.target_id.as_str()),
        ("validation_contract_id", contract.contract_id.as_str()),
        ("source_claim_lineage_id", contract.lineage_id.as_str()),
    ] {
        if string_field(value, field)? != expected {
            return Err(TrustError::new(
                "source_record_contract_binding_mismatch",
                format!("source record differs at {field}"),
            ));
        }
    }
    Ok(())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "source_record_field_missing",
            format!("{field} must be a string"),
        )
    })
}

fn digest_field(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    string_field(value, field)?.parse()
}

fn optional_digest(value: &Value, field: &str) -> Result<Option<Sha256Digest>, TrustError> {
    value
        .get(field)
        .map(|item| {
            item.as_str()
                .ok_or_else(|| {
                    TrustError::new(
                        "source_record_field_invalid",
                        format!("{field} must be a digest string"),
                    )
                })?
                .parse()
        })
        .transpose()
}

fn decode_field<T: for<'de> Deserialize<'de>>(
    value: &Value,
    field: &str,
) -> Result<T, TrustError> {
    serde_json::from_value(value.get(field).cloned().ok_or_else(|| {
        TrustError::new("source_record_field_missing", format!("missing {field}"))
    })?)
    .map_err(|error| TrustError::new("source_record_field_invalid", error.to_string()))
}

fn optional_decode_field<T: for<'de> Deserialize<'de>>(
    value: &Value,
    field: &str,
) -> Result<Option<T>, TrustError> {
    value
        .get(field)
        .cloned()
        .map(|item| {
            serde_json::from_value(item).map_err(|error| {
                TrustError::new("source_record_field_invalid", error.to_string())
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::schema::SchemaRegistry;

    fn fixture_contract() -> SourceValidationContractView {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        let registry = SchemaRegistry::v1().unwrap();
        let record = AuthoritativeRecord::parse(
            &registry,
            fixture["objects"]["source_validation_contract"].clone(),
        )
        .unwrap();
        SourceValidationContractView::from_record(&record).unwrap()
    }

    #[test]
    fn registered_exact_contract_is_shape_specific() {
        let contract = fixture_contract();
        assert_eq!(contract.claim_shape, ClaimShape::Universal);
        assert_eq!(contract.method, SourceValidationMethod::ExactRustExecutionV1);
        assert!(contract.harness_cohort_basis.is_some());
    }

    #[test]
    fn negative_carrier_is_selected_by_contract_not_claim_identity() {
        assert_eq!(
            SourceValidationMethod::ExactRustExecutionV1
                .model_negative_carrier_requirement(),
            ModelNegativeCarrierRequirement::WitnessSpecific
        );
        for method in [
            SourceValidationMethod::CheckedRefutationReflectionV1,
            SourceValidationMethod::NotDefinedForClaimShapeV1,
        ] {
            assert_eq!(
                method.model_negative_carrier_requirement(),
                ModelNegativeCarrierRequirement::TheoremLevel
            );
        }
    }

    #[test]
    fn non_witness_methods_never_enter_qualified_recovery() {
        let mut contract = fixture_contract();
        contract.method = SourceValidationMethod::CheckedRefutationReflectionV1;
        contract.qualification_permission = QualificationPermission::Prohibited;
        assert_eq!(
            route_qualified_recovery(&contract, &HistoryFacts::default(), false, true),
            QualificationRoute::ProhibitedCheckedReflection
        );
        contract.method = SourceValidationMethod::NotDefinedForClaimShapeV1;
        assert_eq!(
            route_qualified_recovery(&contract, &HistoryFacts::default(), false, true),
            QualificationRoute::ProhibitedUnsupportedClaimShape
        );
    }

    #[test]
    fn unsupported_claim_class_is_exactly_bound_to_not_defined_status() {
        validate_role_status(
            None,
            ValidationStatus::NotDefinedForClaimShape,
            Decisiveness::UnsupportedClaimClass,
        )
        .unwrap();
        assert!(validate_role_status(
            None,
            ValidationStatus::NotDefinedForClaimShape,
            Decisiveness::Inconclusive,
        )
        .is_err());

        for status in [
            ValidationStatus::ValidationInconclusive,
            ValidationStatus::InvalidValidationEvidence,
            ValidationStatus::ExactCounterexampleObserved,
        ] {
            assert!(validate_role_status(None, status, Decisiveness::UnsupportedClaimClass)
                .is_err());
        }
    }

    #[test]
    fn reflection_contract_requires_seed_pinned_checker() {
        let mut contract = fixture_contract();
        contract.method = SourceValidationMethod::CheckedRefutationReflectionV1;
        contract.qualification_permission = QualificationPermission::Prohibited;
        contract.source_validator_sha256 = None;
        contract.observation_oracle_sha256 = None;
        contract.harness_cohort_basis = None;
        contract.reflection_theorem_sha256 = Some("11".repeat(32).parse().unwrap());
        contract.reflection_proof_artifact_sha256 = Some("22".repeat(32).parse().unwrap());
        contract.reflection_result_schema_id =
            Some("trellis://schemas/reflection-validation-result/v1".to_owned());
        contract.reflection_checker_sha256 = None;
        let error = contract
            .validate_semantics(&serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, "reflection_contract_has_execution_authority");

        contract.reflection_checker_sha256 = Some(Sha256Digest::ZERO);
        assert!(contract
            .validate_semantics(&serde_json::json!({}))
            .is_err());

        contract.reflection_checker_sha256 = Some("33".repeat(32).parse().unwrap());
        contract
            .validate_semantics(&serde_json::json!({}))
            .unwrap();
    }

    #[test]
    fn exact_counterexample_dominates_scope_and_profiles() {
        let contract = fixture_contract();
        let history = HistoryFacts {
            decisive_source_refutation_present: true,
            independent_scope_limit_present: true,
            ..HistoryFacts::default()
        };
        assert_eq!(
            route_qualified_recovery(&contract, &history, true, true),
            QualificationRoute::ProhibitedDecisiveSourceRefutation
        );
    }

    #[test]
    fn no_replay_is_not_itself_a_qualification_blocker() {
        let contract = fixture_contract();
        assert_eq!(
            route_qualified_recovery(&contract, &HistoryFacts::default(), true, true),
            QualificationRoute::EligibleForProfileEvaluation
        );
    }

    #[test]
    fn harness_refusal_never_creates_independent_basis() {
        let contract = fixture_contract();
        assert_eq!(
            route_qualified_recovery(&contract, &HistoryFacts::default(), true, false),
            QualificationRoute::NoIndependentQualificationBasis
        );
    }

    #[test]
    fn supporting_host_unexecutable_witness_uses_reasoned_route() {
        let mut contract = fixture_contract();
        contract.target_id = "supporting_decide_claim".to_owned();

        // Failure to materialize/execute is not reproduction. With an
        // independently approved profile and replay-free admissibility, the
        // route is conditional proof work for this supporting claim.
        let resource_limited = HistoryFacts {
            independent_scope_limit_present: true,
            ..HistoryFacts::default()
        };
        assert_eq!(
            route_qualified_recovery(&contract, &resource_limited, true, true),
            QualificationRoute::EligibleForProfileEvaluation
        );

        // A carrier/language failure is different: it requires correction and
        // cannot be laundered into qualification by the same profile.
        let language_invalid = HistoryFacts {
            language_inadmissibility_unresolved: true,
            independent_scope_limit_present: true,
            ..HistoryFacts::default()
        };
        assert_eq!(
            route_qualified_recovery(&contract, &language_invalid, true, true),
            QualificationRoute::CorrectBoundaryOrAdmissibility
        );
    }
}
