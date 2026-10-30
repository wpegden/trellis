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

/// The issuance identity committed by a node certificate. This is a check of
/// retained evidence against itself, not a comparison with today's toolchain.
pub fn certificate_toolchain_root(record: &LocalClosureRecord) -> crate::trust_base::Sha256Digest {
    crate::trust_base::raw_sha256(format!(
        "toolchain={}\nlean={}\nlake={}\nchecker={}\nmanifest={}\npreamble={}",
        record.toolchain_hash, record.lean_executable_hash, record.lake_executable_hash,
        record.checker_script_hash, record.lake_manifest_hash, record.preamble_hash,
    ).as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificateIntegrityFailure {
    pub tier: String,
    pub node: NodeId,
    pub field: String,
    pub recorded: String,
    pub expected: String,
}

/// A mismatching retained commitment is not routine currentness drift and a
/// second finalizer cannot repair it. Do not change or normalize any tier.
pub fn certificate_integrity_failure(state: &crate::ProtocolState) -> Option<CertificateIntegrityFailure> {
    for (tier, records) in [
        ("live", &state.local_closure_records),
        ("committed", &state.committed_local_closure_records),
        ("last-clean", &state.last_clean_local_closure_records),
    ] {
        for (node, record) in records {
            let Some(certificate) = &record.node_certificate else { continue; };
            let expected = certificate_toolchain_root(record);
            if certificate.toolchain_root != expected {
                return Some(CertificateIntegrityFailure {
                    tier: tier.into(), node: node.clone(), field: "toolchain_root".into(),
                    recorded: certificate.toolchain_root.to_string(), expected: expected.to_string(),
                });
            }
        }
    }
    None
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
        // Pin the reviewed collector, including native PV premise-owner support
        // and the Lean-native source-policy scanner merged with that workflow.
        assert_eq!(
            format!("{:x}", Sha256::digest(COLLECTOR.as_bytes())),
            "c8e4a03273e23b9a8f2e141020652a4e0b7b0a8ab0883366ea9d031ca3f37aa1"
        );
    }

    #[test]
    fn missing_required_identity_never_compares_current() {
        let i = Identity::default();
        assert_eq!(i.unavailable(true).len(), 6);
        assert_eq!(i.unavailable(false).len(), 4);
    }

    #[test]
    fn certificate_integrity_distinguishes_corruption_from_toolchain_drift() {
        // Exact six issuance fields from the retained task128 certificate.
        let mut record = LocalClosureRecord {
            node: "P2634_T1_PredecessorElementary".into(),
            toolchain_hash: "302cd63c54178885b89e669f33b38f12f4dd7ae7e5cac537b3203e3768d8fb2b".into(),
            lean_executable_hash: "e8baaa71855a616dc351028f3ad2200051b0671f423a1696a100e809302d5550".into(),
            lake_executable_hash: "60330ab6f07dce20f3fa9ebb08e8b984ea9549eac172afeb15d9d2227060e2b3".into(),
            checker_script_hash: "c8e4a03273e23b9a8f2e141020652a4e0b7b0a8ab0883366ea9d031ca3f37aa1".into(),
            lake_manifest_hash: "4fdd7a6e1c7682fa953f455afd57a10bd36dc86744f743504f5d6add3370bee1".into(),
            preamble_hash: "4f1e178181e2efa32fd33df5ec590133c9be645b41131de6ccaa9cc09a6947a5".into(),
            ..Default::default()
        };
        let expected = certificate_toolchain_root(&record);
        assert_eq!(expected.to_string(), "7ab8c7585b19d28c6db10c68633ad88ab90bc489de5221e32f6cc166fbdd560d");
        record.node_certificate = Some(crate::NodeCertificate { toolchain_root: expected, ..Default::default() });
        let mut state = crate::ProtocolState::default();
        state.local_closure_records.insert(record.node.clone(), record.clone());
        state.committed_local_closure_records = state.local_closure_records.clone();
        state.last_clean_local_closure_records = state.local_closure_records.clone();
        assert_eq!(certificate_integrity_failure(&state), None);

        for tier in ["live", "committed", "last-clean"] {
            let mut bad = state.clone();
            let records = match tier {
                "live" => &mut bad.local_closure_records,
                "committed" => &mut bad.committed_local_closure_records,
                _ => &mut bad.last_clean_local_closure_records,
            };
            records.get_mut(&record.node).unwrap().node_certificate.as_mut().unwrap().toolchain_root =
                serde_json::from_value(serde_json::json!("7ab8c7585b19d28c6db10c68633ad88cb90bc489de5221e32f6cc166fbdd560d")).unwrap();
            let before = bad.clone();
            let failure = certificate_integrity_failure(&bad).unwrap();
            assert_eq!(failure.tier, tier);
            assert_eq!(failure.node, record.node);
            assert_eq!(failure.expected, expected.to_string());
            assert_ne!(failure.recorded, failure.expected);
            assert_eq!(bad, before, "never repair or normalize retained authority");
        }
        // A different, internally consistent historical toolchain is ordinary
        // currency evidence. This check does not compare it with today's one.
        record.lean_executable_hash = "different historical Lean".into();
        assert_ne!(certificate_toolchain_root(&record), expected);
        record.node_certificate.as_mut().unwrap().toolchain_root = certificate_toolchain_root(&record);
        state.local_closure_records.insert(record.node.clone(), record.clone());
        assert_eq!(certificate_integrity_failure(&state), None);
        record.node_certificate = None;
        state.local_closure_records.insert(record.node.clone(), record);
        assert_eq!(certificate_integrity_failure(&state), None);
    }
}
