//! Closed campaign target-resolution plan and generic adaptation ledger.

use super::canonical::{Sha256Digest, TrustError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const CAMPAIGN_TRUST_PLAN_SCHEMA_V4: &str = "trellis-campaign-trust-plan/v4";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignTrustPlan {
    pub schema: String,
    #[serde(default)]
    pub adaptation_ledger: Vec<AdaptationLedgerEntry>,
    pub targets: Vec<TargetTrustPlan>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetTrustPlan {
    pub target_id: String,
    pub resolution: crate::model::ChallengeResolution,
}

/// One generic source/model adaptation. These rows disclose provenance and
/// meaning changes; they never authorize polarity or proof closure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptationLedgerEntry {
    pub id: String,
    pub seam_class: AdaptationSeamClass,
    pub paths: Vec<AdaptationPathDelta>,
    pub citation: AdaptationCitation,
    pub affected_targets: Vec<String>,
    pub meaning_change: bool,
    pub status: AdaptationLedgerStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdaptationSeamClass {
    #[serde(rename = "packaging_shim")]
    PackagingShim,
    #[serde(rename = "i")]
    SeamI,
    #[serde(rename = "ii")]
    SeamII,
    #[serde(rename = "iii")]
    SeamIII,
    #[serde(rename = "seed")]
    Seed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptationPathDelta {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<Sha256Digest>,
    pub after_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdaptationCitation {
    LanguageGuarantee { statement: String },
    UpstreamReference { reference: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptationLedgerStatus {
    Seed,
    Authorized,
    Ratified,
    Rejected,
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
        if self.schema != CAMPAIGN_TRUST_PLAN_SCHEMA_V4 {
            return Err(error("campaign trust plan needs its v4 schema"));
        }
        if self.targets.is_empty() {
            return Err(error(
                "the PV trust framework requires at least one challenge target",
            ));
        }

        let mut target_ids = BTreeSet::new();
        for target in &self.targets {
            validate_id(&target.target_id)?;
            if !target_ids.insert(target.target_id.as_str()) {
                return Err(error("campaign trust plan repeats a target ID"));
            }
        }

        let mut row_ids = BTreeSet::new();
        for row in &self.adaptation_ledger {
            validate_id(&row.id)?;
            if !row_ids.insert(row.id.as_str()) {
                return Err(error("campaign trust plan repeats an adaptation row ID"));
            }
            if row.status != AdaptationLedgerStatus::Seed {
                return Err(error("plan adaptation rows must have seed status"));
            }
            if row.paths.is_empty() || row.affected_targets.is_empty() {
                return Err(error(
                    "plan adaptation rows need paths and affected targets",
                ));
            }
            for path in &row.paths {
                validate_text(&path.path, "adaptation path")?;
                if path.after_sha256 == Sha256Digest::ZERO {
                    return Err(error("adaptation path has a zero after digest"));
                }
            }
            for target in &row.affected_targets {
                validate_id(target)?;
                if !target_ids.contains(target.as_str()) {
                    return Err(error(format!(
                        "adaptation row {} names unknown target {target}",
                        row.id
                    )));
                }
            }
            match &row.citation {
                AdaptationCitation::LanguageGuarantee { statement } => {
                    validate_text(statement, "language guarantee")?
                }
                AdaptationCitation::UpstreamReference { reference } => {
                    validate_text(reference, "upstream reference")?
                }
            }
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

fn error(message: impl Into<String>) -> TrustError {
    TrustError::new("campaign_trust_plan_invalid", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_closed_resolution_rows() {
        let value = json!({
            "schema": CAMPAIGN_TRUST_PLAN_SCHEMA_V4,
            "adaptation_ledger": [],
            "targets": [
                {"target_id": "goal:a", "resolution": "decide"},
                {"target_id": "goal:b", "resolution": "prove"}
            ]
        });
        let plan = CampaignTrustPlan::parse_and_validate(
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.targets[0].resolution, crate::model::ChallengeResolution::Decide);
        assert_eq!(plan.targets[1].resolution, crate::model::ChallengeResolution::Prove);
    }

    #[test]
    fn rejects_execution_derived_fields() {
        let value = json!({
            "schema": CAMPAIGN_TRUST_PLAN_SCHEMA_V4,
            "adaptation_ledger": [],
            "targets": [{
                "target_id": "goal:a",
                "resolution": "decide",
                "seed_contract": {"kind": "routing_only"}
            }]
        });
        assert!(CampaignTrustPlan::parse_and_validate(
            &serde_json::to_vec(&value).unwrap(),
        )
        .is_err());
    }
}
