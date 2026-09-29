//! Operational currency of closure evidence. No I/O and no certificate re-keying.
use crate::{LocalClosureRecord, NodeId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const COLLECTOR: &str = include_str!("../../scripts/lean_local_closure.lean");
pub const VERSION: &str = "patch_c_v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    Current,
    IdentityStale,
    SourceStale,
    PolicyRejected,
    Missing,
    CorruptEvidence,
    IdentityUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub category: Currency,
    pub axis: String,
    pub recorded: String,
    pub expected: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub closure_version: String,
    pub toolchain_hash: String,
    pub lean_executable_hash: String,
    pub lake_executable_hash: String,
    pub checker_script_hash: String,
    pub lake_manifest_hash: String,
    pub preamble_hash: String,
}

impl Identity {
    pub fn of(record: &LocalClosureRecord) -> Self {
        Self {
            closure_version: record.closure_version.clone(),
            toolchain_hash: record.toolchain_hash.clone(),
            lean_executable_hash: record.lean_executable_hash.clone(),
            lake_executable_hash: record.lake_executable_hash.clone(),
            checker_script_hash: record.checker_script_hash.clone(),
            lake_manifest_hash: record.lake_manifest_hash.clone(),
            preamble_hash: record.preamble_hash.clone(),
        }
    }

    pub fn axes(&self) -> BTreeMap<&'static str, &str> {
        BTreeMap::from([
            ("closure_version", self.closure_version.as_str()),
            ("toolchain_hash", self.toolchain_hash.as_str()),
            ("lean_executable_hash", self.lean_executable_hash.as_str()),
            ("lake_executable_hash", self.lake_executable_hash.as_str()),
            ("checker_script_hash", self.checker_script_hash.as_str()),
            ("lake_manifest_hash", self.lake_manifest_hash.as_str()),
            ("preamble_hash", self.preamble_hash.as_str()),
        ])
    }

    pub fn unavailable(&self, lean: bool) -> Vec<Finding> {
        self.axes()
            .into_iter()
            .filter_map(|(axis, value)| {
                let required = match axis {
                    "lake_manifest_hash" => !lean,
                    "lean_executable_hash" | "lake_executable_hash" | "checker_script_hash" => lean,
                    _ => true,
                };
                (required && value.is_empty()).then(|| Finding {
                    category: Currency::IdentityUnavailable,
                    axis: axis.into(),
                    recorded: String::new(),
                    expected: "required nonempty identity".into(),
                })
            })
            .collect()
    }

    pub fn compare(&self, record: &LocalClosureRecord, lean: bool) -> Vec<Finding> {
        let old = Self::of(record);
        let axes = old.axes();
        let mut findings = self.unavailable(lean);
        for (axis, expected) in self.axes() {
            let recorded = axes[axis];
            if recorded != expected {
                findings.push(Finding {
                    category: Currency::IdentityStale,
                    axis: axis.into(),
                    recorded: recorded.into(),
                    expected: expected.into(),
                });
            }
        }
        findings
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerCurrency {
    pub tier: String,
    pub node: NodeId,
    pub findings: Vec<Finding>,
    pub source_checked: bool,
    pub artifacts_checked: bool,
}

impl OwnerCurrency {
    pub fn current(&self) -> bool {
        self.findings.is_empty()
    }
    pub fn repair_required(&self) -> bool {
        self.findings.iter().any(|f| {
            matches!(
                f.category,
                Currency::IdentityStale
                    | Currency::PolicyRejected
                    | Currency::Missing
                    | Currency::IdentityUnavailable
            )
        })
    }
    pub fn add(
        &mut self,
        category: Currency,
        axis: &str,
        recorded: impl ToString,
        expected: impl ToString,
    ) {
        self.findings.push(Finding {
            category,
            axis: axis.into(),
            recorded: recorded.to_string(),
            expected: expected.to_string(),
        });
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairRequired {
    pub owners: Vec<OwnerCurrency>,
    pub direct_stale: BTreeSet<NodeId>,
    pub expanded_stale: BTreeSet<NodeId>,
    pub repair_command: String,
}

/// Preserve a structured error through older CLI layers which use String errors.
/// The wire boundary decodes this envelope and never writes a crash breadcrumb.
pub const REPAIR_PREFIX: &str = "ClosureIdentityRepairRequired:";

pub fn decode_repair(message: &str) -> Option<RepairRequired> {
    let (_, json) = message.split_once(REPAIR_PREFIX)?;
    serde_json::from_str(json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn embedded_collector_preserves_deployed_raw_bytes() {
        let file = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../scripts/lean_local_closure.lean"
        ))
        .unwrap();
        assert_eq!(COLLECTOR.as_bytes(), file);
        assert_eq!(
            format!("{:x}", Sha256::digest(COLLECTOR.as_bytes())),
            "c2876ad4a8a736c97e83ae1eef972cc417d2bd63ed6d0ce2800e19861e7f427e"
        );
    }

    #[test]
    fn missing_required_identity_never_compares_current() {
        let i = Identity::default();
        assert_eq!(i.unavailable(true).len(), 6);
        assert_eq!(i.unavailable(false).len(), 4);
    }
}
