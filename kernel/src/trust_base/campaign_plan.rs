//! Strict, authored input for constructing campaign-specific trust records.
//!
//! The kernel never infers that a syntactically universal theorem has a Rust
//! replay oracle.  A seed author must register either an explicit unsupported
//! route or the complete claim-specific exact-execution interpretation.  This
//! file is bootstrap input; the resulting definitions, profiles, and theorem
//! candidates are separately canonicalized into the approved seed closure.

use super::canonical::{DecimalNatural, TrustError};
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignTrustPlan {
    pub schema: String,
    pub targets: Vec<TargetTrustPlan>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetTrustPlan {
    pub target_id: String,
    pub source_validation: TargetSourceValidationPlan,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "validation_method",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TargetSourceValidationPlan {
    NotDefinedForClaimShapeV1 {
        claim_shape: ClaimShapeName,
        source_counterevidence_class: UnsupportedEvidenceClass,
        reason: String,
    },
    ExactRustExecutionV1 {
        public_entry_point: String,
        rust_target_statement_utf8: String,
        source_claim_interpretation_utf8: String,
        binder_domain_schema_utf8: String,
        preconditions: Vec<SourcePreconditionPlan>,
        entry_point_type_utf8: String,
        environment_contract_utf8: String,
        concretization_schema_utf8: String,
        erasure_relation_utf8: String,
        raw_observation_schema_utf8: String,
        negative_evidence_soundness_statement_utf8: String,
        qualification: Option<ResourceQualificationPlan>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimShapeName {
    Universal,
    Existential,
    Implication,
    TerminationOrLiveness,
    TraceProperty,
    RelationalOrHyperproperty,
    HigherOrderOrAbstractDomain,
    Other,
}

impl ClaimShapeName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Universal => "universal",
            Self::Existential => "existential",
            Self::Implication => "implication",
            Self::TerminationOrLiveness => "termination_or_liveness",
            Self::TraceProperty => "trace_property",
            Self::RelationalOrHyperproperty => "relational_or_hyperproperty",
            Self::HigherOrderOrAbstractDomain => "higher_order_or_abstract_domain",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedEvidenceClass {
    NonFinitelyObservable,
    NoApprovedSourceOracle,
    Unclassified,
}

impl UnsupportedEvidenceClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonFinitelyObservable => "non_finitely_observable",
            Self::NoApprovedSourceOracle => "no_approved_source_oracle",
            Self::Unclassified => "unclassified",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePreconditionPlan {
    pub precondition_id: String,
    pub normalized_statement_utf8: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceQualificationPlan {
    pub profile_id: String,
    pub conditionalization_schema_id: String,
    pub binder: String,
    pub binder_type: String,
    pub measure_id: String,
    pub units: ResourceUnits,
    pub comparison: ResourceComparison,
    pub scope: ResourceScope,
    pub enforcement: ResourceEnforcement,
    pub bound: String,
    pub basis_fact: IndependentBasisFactPlan,
    pub permitted_applicability: Vec<PermittedApplicability>,
    pub conditional_candidate_id: String,
    pub conditional_candidate_node_id: String,
    pub conditional_statement_utf8: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndependentBasisFactPlan {
    pub fact_class: BasisFactClass,
    pub fact_schema_id: String,
    pub producer_identity: String,
    /// A seed-approved evidence leaf captured before the human gate.  The
    /// builder resolves this logical ID to its exact raw digest; callers may
    /// not inject an unattached hash-shaped value.
    pub evidence_logical_id: String,
    /// Normalized relative path below the trust-plan directory. Bootstrap
    /// copies this pre-gate artifact without following symlinks.
    pub evidence_path: String,
    pub validity_interval_utf8: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BasisFactClass {
    SourceLimit,
    ConfigurationLimit,
    DeploymentLimit,
    HostCapabilityLimit,
}

impl BasisFactClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceLimit => "source_limit",
            Self::ConfigurationLimit => "configuration_limit",
            Self::DeploymentLimit => "deployment_limit",
            Self::HostCapabilityLimit => "host_capability_limit",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceUnits {
    Bytes,
    Elements,
}

impl ResourceUnits {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceComparison {
    StrictlyLessThan,
    LessThanOrEqual,
}

impl ResourceComparison {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StrictlyLessThan => "strictly_less_than",
            Self::LessThanOrEqual => "less_than_or_equal",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceScope {
    Source,
    Configuration,
    Deployment,
    Host,
}

impl ResourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Configuration => "configuration",
            Self::Deployment => "deployment",
            Self::Host => "host",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceEnforcement {
    SourceEnforced,
    ConfigurationEnforced,
    DeploymentEnforced,
    Observed,
}

impl ResourceEnforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceEnforced => "source_enforced",
            Self::ConfigurationEnforced => "configuration_enforced",
            Self::DeploymentEnforced => "deployment_enforced",
            Self::Observed => "observed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PermittedApplicability {
    FormallyDerived,
    MechanicallyEnforced,
    ExternallyAttested,
    Unestablished,
}

impl PermittedApplicability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FormallyDerived => "formally_derived",
            Self::MechanicallyEnforced => "mechanically_enforced",
            Self::ExternallyAttested => "externally_attested",
            Self::Unestablished => "unestablished",
        }
    }
}

impl CampaignTrustPlan {
    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let value = super::canonical::parse_json_strict(bytes)?;
        let plan: Self = serde_json::from_value(value).map_err(|error| {
            TrustError::new("campaign_trust_plan_invalid", error.to_string())
        })?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        if self.schema != "trellis-campaign-trust-plan/v1" || self.targets.is_empty() {
            return Err(error(
                "campaign trust plan needs its v1 schema and at least one target",
            ));
        }
        let mut target_ids = BTreeSet::new();
        for target in &self.targets {
            validate_id(&target.target_id)?;
            if !target_ids.insert(target.target_id.as_str()) {
                return Err(error("campaign trust plan repeats a target ID"));
            }
            match &target.source_validation {
                TargetSourceValidationPlan::NotDefinedForClaimShapeV1 { reason, .. } => {
                    validate_text(reason, "unsupported-route reason")?;
                }
                TargetSourceValidationPlan::ExactRustExecutionV1 {
                    public_entry_point,
                    rust_target_statement_utf8,
                    source_claim_interpretation_utf8,
                    binder_domain_schema_utf8,
                    preconditions,
                    entry_point_type_utf8,
                    environment_contract_utf8,
                    concretization_schema_utf8,
                    erasure_relation_utf8,
                    raw_observation_schema_utf8,
                    negative_evidence_soundness_statement_utf8,
                    qualification,
                } => {
                    validate_id(public_entry_point)?;
                    for (name, value) in [
                        ("Rust target statement", rust_target_statement_utf8),
                        ("source claim interpretation", source_claim_interpretation_utf8),
                        ("binder-domain schema", binder_domain_schema_utf8),
                        ("entry-point type", entry_point_type_utf8),
                        ("environment contract", environment_contract_utf8),
                        ("concretization schema", concretization_schema_utf8),
                        ("erasure relation", erasure_relation_utf8),
                        ("raw observation schema", raw_observation_schema_utf8),
                        (
                            "negative-evidence soundness statement",
                            negative_evidence_soundness_statement_utf8,
                        ),
                    ] {
                        validate_text(value, name)?;
                    }
                    if preconditions.is_empty() {
                        return Err(error("exact Rust execution needs a precondition"));
                    }
                    let mut precondition_ids = BTreeSet::new();
                    for precondition in preconditions {
                        validate_id(&precondition.precondition_id)?;
                        validate_text(
                            &precondition.normalized_statement_utf8,
                            "source precondition",
                        )?;
                        if !precondition_ids.insert(precondition.precondition_id.as_str()) {
                            return Err(error("exact Rust execution repeats a precondition ID"));
                        }
                    }
                    if let Some(qualification) = qualification {
                        qualification.validate()?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl ResourceQualificationPlan {
    fn validate(&self) -> Result<(), TrustError> {
        for id in [
            &self.profile_id,
            &self.conditionalization_schema_id,
            &self.binder,
            &self.binder_type,
            &self.measure_id,
            &self.conditional_candidate_id,
            &self.conditional_candidate_node_id,
            &self.basis_fact.fact_schema_id,
            &self.basis_fact.producer_identity,
            &self.basis_fact.evidence_logical_id,
        ] {
            validate_id(id)?;
        }
        validate_relative_path(&self.basis_fact.evidence_path)?;
        validate_text(
            &self.conditional_statement_utf8,
            "conditional theorem statement",
        )?;
        validate_text(
            &self.basis_fact.validity_interval_utf8,
            "basis validity interval",
        )?;
        self.bound.parse::<DecimalNatural>()?;
        if self.bound == "0" {
            return Err(error("qualification bound must be positive"));
        }
        if self.measure_id != "structural-input-byte-length-v1"
            || self.units != ResourceUnits::Bytes
        {
            return Err(error(
                "v1 qualification admits only structural input byte length",
            ));
        }
        let expected = match self.basis_fact.fact_class {
            BasisFactClass::SourceLimit => {
                (ResourceScope::Source, ResourceEnforcement::SourceEnforced)
            }
            BasisFactClass::ConfigurationLimit => (
                ResourceScope::Configuration,
                ResourceEnforcement::ConfigurationEnforced,
            ),
            BasisFactClass::DeploymentLimit => (
                ResourceScope::Deployment,
                ResourceEnforcement::DeploymentEnforced,
            ),
            BasisFactClass::HostCapabilityLimit => {
                (ResourceScope::Host, ResourceEnforcement::Observed)
            }
        };
        if (self.scope, self.enforcement) != expected {
            return Err(error(
                "basis fact class does not authorize the requested scope/enforcement",
            ));
        }
        if self.permitted_applicability.is_empty() {
            return Err(error("qualification applicability set is empty"));
        }
        let mut prior = None;
        for item in &self.permitted_applicability {
            if prior.is_some_and(|value| value >= *item) {
                return Err(error(
                    "permitted applicability must be unique and enum-order sorted",
                ));
            }
            prior = Some(*item);
        }
        Ok(())
    }
}

fn validate_id(value: &str) -> Result<(), TrustError> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(|character| character.is_control())
    {
        return Err(error("campaign trust plan contains an invalid identifier"));
    }
    Ok(())
}

fn validate_text(value: &str, name: &str) -> Result<(), TrustError> {
    if value.trim().is_empty() || value.as_bytes().contains(&0) {
        return Err(error(format!("campaign trust plan has invalid {name}")));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), TrustError> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(error("campaign trust plan contains an unsafe evidence path"));
    }
    Ok(())
}

fn error(message: impl Into<String>) -> TrustError {
    TrustError::new("campaign_trust_plan_invalid", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_does_not_infer_replay_from_logical_shape() {
        let plan = CampaignTrustPlan::parse_and_validate(
            br#"{"schema":"trellis-campaign-trust-plan/v1","targets":[{"source_validation":{"claim_shape":"universal","reason":"No target-specific source oracle has been approved.","source_counterevidence_class":"no_approved_source_oracle","validation_method":"not_defined_for_claim_shape_v1"},"target_id":"generic-universal"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            plan.targets[0].source_validation,
            TargetSourceValidationPlan::NotDefinedForClaimShapeV1 { .. }
        ));
    }

    #[test]
    fn witness_derived_or_unscoped_basis_cannot_enter_plan() {
        let raw = br#"{"schema":"trellis-campaign-trust-plan/v1","targets":[{"source_validation":{"binder_domain_schema_utf8":"byte slice","concretization_schema_utf8":"RLE bytes","entry_point_type_utf8":"fn(&[u8]) -> Result","environment_contract_utf8":"closed","erasure_relation_utf8":"same bytes","negative_evidence_soundness_statement_utf8":"a panic refutes totality","preconditions":[{"normalized_statement_utf8":"valid shared byte slice","precondition_id":"valid-slice"}],"public_entry_point":"entry","qualification":{"basis_fact":{"evidence_logical_id":"witness","evidence_path":"basis/host.json","fact_class":"host_capability_limit","fact_schema_id":"host-limit/v1","producer_identity":"witness","validity_interval_utf8":"run"},"binder":"input","binder_type":"SliceU8","bound":"99","comparison":"strictly_less_than","conditional_candidate_id":"candidate","conditional_candidate_node_id":"conditional","conditional_statement_utf8":"theorem conditional : True := by","conditionalization_schema_id":"forall-precondition/v1","enforcement":"deployment_enforced","measure_id":"structural-input-byte-length-v1","permitted_applicability":["unestablished"],"profile_id":"profile","scope":"deployment","units":"bytes"},"raw_observation_schema_utf8":"panic or return","rust_target_statement_utf8":"all calls return","source_claim_interpretation_utf8":"panic freedom","validation_method":"exact_rust_execution_v1"},"target_id":"claim"}]}"#;
        let error = CampaignTrustPlan::parse_and_validate(raw).unwrap_err();
        assert_eq!(error.code, "campaign_trust_plan_invalid");
    }
}
