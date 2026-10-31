//! Typed native observations for a narrowly scoped publication refresh.
//! These values carry observations, not an old worker verdict or process state.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use trellis_kernel::model::{
    DeviationId, PvRole, RefPaperId, ReferencePaperSpec, SoundFingerprintParts,
};
use trellis_kernel::{NodeId, NodeKind, TargetId, WorkingSnapshot};

/// Actual operands after applying the fresh response's structural updates.
/// This deliberately has no cycle, request id, summary, or completed verdict:
/// none is consumed by the fingerprint observer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FingerprintInput {
    pub soundness_mode: String,
    pub configured_targets: BTreeSet<TargetId>,
    pub present_nodes: BTreeSet<NodeId>,
    pub node_kinds: BTreeMap<NodeId, NodeKind>,
    pub target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    pub deviation_files: BTreeMap<DeviationId, String>,
    pub node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    pub configured_reference_papers: BTreeMap<RefPaperId, ReferencePaperSpec>,
    pub node_reference_grounds: BTreeMap<NodeId, BTreeSet<RefPaperId>>,
    pub approved_paper_fingerprints: BTreeMap<TargetId, String>,
    pub paper_source_path: Option<PathBuf>,
    pub goal_prose_path: Option<PathBuf>,
    pub node_role: BTreeMap<NodeId, PvRole>,
    pub under_model_assumption_nodes: BTreeSet<NodeId>,
}

/// Exactly the fields written by populate_response_fingerprints. In particular
/// this cannot overwrite fresh source-derived presence/open sets or request IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintFacts {
    pub coverage: BTreeMap<TargetId, BTreeSet<NodeId>>,
    pub target_fingerprints: BTreeMap<NodeId, String>,
    pub corr_current_fingerprints: BTreeMap<NodeId, String>,
    pub paper_current_fingerprints: BTreeMap<TargetId, String>,
    pub deviation_current_fingerprints: BTreeMap<DeviationId, String>,
    pub substantiveness_current_fingerprints: BTreeMap<NodeId, String>,
    pub sound_current_fingerprints: BTreeMap<NodeId, String>,
    pub sound_current_fingerprint_parts: BTreeMap<NodeId, SoundFingerprintParts>,
    pub sketch_proof_nodes: BTreeSet<NodeId>,
    pub placeholder_definition_nodes: BTreeSet<NodeId>,
    pub protected_closure_nodes_per_target: BTreeMap<TargetId, BTreeSet<NodeId>>,
}

impl FingerprintFacts {
    pub fn capture(snapshot: &WorkingSnapshot) -> Self {
        Self {
            coverage: snapshot.coverage.clone(),
            target_fingerprints: snapshot.target_fingerprints.clone(),
            corr_current_fingerprints: snapshot.corr_current_fingerprints.clone(),
            paper_current_fingerprints: snapshot.paper_current_fingerprints.clone(),
            deviation_current_fingerprints: snapshot.deviation_current_fingerprints.clone(),
            substantiveness_current_fingerprints: snapshot
                .substantiveness_current_fingerprints
                .clone(),
            sound_current_fingerprints: snapshot.sound_current_fingerprints.clone(),
            sound_current_fingerprint_parts: snapshot.sound_current_fingerprint_parts.clone(),
            sketch_proof_nodes: snapshot.sketch_proof_nodes.clone(),
            placeholder_definition_nodes: snapshot.placeholder_definition_nodes.clone(),
            protected_closure_nodes_per_target: snapshot.protected_closure_nodes_per_target.clone(),
        }
    }
    pub fn apply(&self, snapshot: &mut WorkingSnapshot) {
        snapshot.coverage = self.coverage.clone();
        snapshot.target_fingerprints = self.target_fingerprints.clone();
        snapshot.corr_current_fingerprints = self.corr_current_fingerprints.clone();
        snapshot.paper_current_fingerprints = self.paper_current_fingerprints.clone();
        snapshot.deviation_current_fingerprints = self.deviation_current_fingerprints.clone();
        snapshot.substantiveness_current_fingerprints =
            self.substantiveness_current_fingerprints.clone();
        snapshot.sound_current_fingerprints = self.sound_current_fingerprints.clone();
        snapshot.sound_current_fingerprint_parts = self.sound_current_fingerprint_parts.clone();
        snapshot.sketch_proof_nodes = self.sketch_proof_nodes.clone();
        snapshot.placeholder_definition_nodes = self.placeholder_definition_nodes.clone();
        snapshot.protected_closure_nodes_per_target =
            self.protected_closure_nodes_per_target.clone();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FingerprintReceipt {
    pub input_sha256: String,
    pub facts_sha256: String,
    pub referents: BTreeMap<PathBuf, Option<String>>,
    pub facts: FingerprintFacts,
    pub shadow_differential_agreement: bool,
}

impl FingerprintInput {
    /// These observations may read paths outside repo. Bind the actual files,
    /// including absent referents; a repo-only source manifest is insufficient.
    pub fn referents(&self, repo: &Path) -> Option<BTreeMap<PathBuf, Option<String>>> {
        use sha2::{Digest, Sha256};
        let paths = self
            .paper_source_path
            .iter()
            .chain(self.goal_prose_path.iter())
            .cloned()
            .chain(self.deviation_files.values().map(PathBuf::from))
            .chain(
                self.configured_reference_papers
                    .values()
                    .map(|paper| PathBuf::from(&paper.tex_path)),
            );
        let mut bindings = BTreeMap::new();
        for path in paths {
            let path = if path.is_absolute() {
                path
            } else {
                repo.join(path)
            };
            let value = match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Ok(metadata) if metadata.is_file() => {
                    Some(format!("{:x}", Sha256::digest(std::fs::read(&path).ok()?)))
                }
                _ => return None,
            };
            bindings.insert(path, value);
        }
        Some(bindings)
    }
}

/// Actual baseline observation operands for the supported math closure lane.
/// Request IDs, cycle and prose are assembled fresh, outside this observer.
#[derive(Clone, Debug, Serialize)]
pub struct BaselineInput {
    pub request_kind: trellis_kernel::RequestKind,
    pub lean_closure: bool,
    pub validation_execution_plan: Vec<trellis_kernel::WorkerValidationExecutionPlanStep>,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub active_node: Option<NodeId>,
    pub under_model_assumption_nodes: BTreeSet<NodeId>,
    pub assumption_authoring_node: Option<NodeId>,
    pub observation_plan: trellis_kernel::model::WorkerAcceptanceObservationPlan,
    pub collect_observations: bool,
    pub phase: trellis_kernel::Phase,
    pub is_pv: bool,
    pub node_role: BTreeMap<NodeId, PvRole>,
    pub current_node_kinds: BTreeMap<NodeId, NodeKind>,
    pub validation_kind: trellis_kernel::WorkerValidationKind,
    pub authorized_nodes: BTreeSet<NodeId>,
    pub configured_targets: BTreeSet<TargetId>,
    pub current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    pub current_paper_approved_fingerprints: BTreeMap<TargetId, String>,
    pub goal_prose_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineFacts {
    pub observations: trellis_kernel::WorkerGateObservationOutput,
    pub baseline_errors: Vec<String>,
    pub baseline_correspondence_hashes: BTreeMap<NodeId, String>,
    pub baseline_lint_diagnostics: BTreeMap<NodeId, Vec<String>>,
    pub current_paper_current_fingerprints: BTreeMap<TargetId, String>,
    pub current_open_nodes: BTreeSet<NodeId>,
    pub current_sketch_proof_nodes: Option<BTreeSet<NodeId>>,
    pub before_closure_source_digest: Option<String>,
}

impl BaselineFacts {
    pub fn from_context(context: &super::PreparedWorkerGateOutput) -> Self {
        Self {
            observations: trellis_kernel::WorkerGateObservationOutput {
                before_snapshot: context.before_snapshot.clone(),
                before_tablet_contents: context.before_tablet_contents.clone(),
                imports_before: context.imports_before.clone(),
                expected_active_hash: context.expected_active_hash.clone(),
                baseline_declaration_hashes: context.baseline_declaration_hashes.clone(),
            },
            baseline_errors: context.baseline_errors.clone(),
            baseline_correspondence_hashes: context.baseline_correspondence_hashes.clone(),
            baseline_lint_diagnostics: context.baseline_lint_diagnostics.clone(),
            current_paper_current_fingerprints: context.current_paper_current_fingerprints.clone(),
            current_open_nodes: context.current_open_nodes.clone(),
            current_sketch_proof_nodes: context.current_sketch_proof_nodes.clone(),
            before_closure_source_digest: context.before_closure_source_digest.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineReceipt {
    pub input_sha256: String,
    pub facts_sha256: String,
    /// Raw original pre-overlay map. Refresh verifies the original private
    /// config carrier and current canonical config role explicitly.
    pub source_inputs: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertificateFacts {
    pub checker_identities: (String, String, String),
    pub policy_nodes: BTreeSet<NodeId>,
    pub probes: BTreeMap<NodeId, trellis_kernel::LocalClosureProbeOutput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicationObservations {
    pub fingerprints: Vec<FingerprintReceipt>,
    pub certificate: CertificateFacts,
}

impl FingerprintReceipt {
    pub fn apply_current(
        &self,
        repo: &Path,
        input: &FingerprintInput,
        snapshot: &mut WorkingSnapshot,
    ) -> Result<(), String> {
        use super::validation_fact_receipt::complete_digest;
        if !self.shadow_differential_agreement
            || complete_digest("fingerprint_input", input).as_ref() != Some(&self.input_sha256)
            || complete_digest("fingerprint_facts", &self.facts).as_ref()
                != Some(&self.facts_sha256)
            || input.referents(repo).as_ref() != Some(&self.referents)
        {
            return Err("receipt-only refresh: fingerprint operands or referents changed".into());
        }
        self.facts.apply(snapshot);
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Environment {
    pub version: u8,
    pub environment_sha256s: BTreeMap<NodeId, String>,
    pub artifact_files: BTreeMap<PathBuf, Option<String>>,
}
impl Environment {
    pub fn covers(&self, roots: &BTreeSet<NodeId>) -> bool {
        self.version == 1
            && !roots.is_empty()
            && !self.artifact_files.is_empty()
            && roots.iter().all(|root| {
                self.environment_sha256s.get(root).is_some_and(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                })
            })
    }
    pub fn agrees(&self, original: &Self, roots: &BTreeSet<NodeId>) -> bool {
        self.covers(roots)
            && original.covers(roots)
            && roots.iter().all(|node| {
                self.environment_sha256s.get(node) == original.environment_sha256s.get(node)
            })
    }
    pub fn witness(
        &self,
        roots: &BTreeSet<NodeId>,
    ) -> super::validation_fact_receipt::ArtifactEnvironmentWitness {
        super::validation_fact_receipt::ArtifactEnvironmentWitness {
            version: self.version,
            donor_round: "0".into(),
            donor_completion_sha256: "0".repeat(64),
            covered_nodes: roots.clone(),
            environment_sha256s: self.environment_sha256s.clone(),
            artifact_files: self.artifact_files.clone(),
        }
    }
    pub fn current(&self, roots: &BTreeSet<NodeId>) -> bool {
        self.covers(roots) && self.witness(roots).matches_current_files()
    }
}

#[cfg(test)]
mod tests {
    use super::super::validation_fact_receipt::complete_digest;
    use super::*;

    fn fingerprint_input() -> FingerprintInput {
        FingerprintInput {
            soundness_mode: "V2Strict".into(),
            configured_targets: BTreeSet::new(),
            present_nodes: BTreeSet::from([NodeId::from("A")]),
            node_kinds: BTreeMap::new(),
            target_claims: BTreeMap::new(),
            deviation_files: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            configured_reference_papers: BTreeMap::new(),
            node_reference_grounds: BTreeMap::new(),
            approved_paper_fingerprints: BTreeMap::new(),
            paper_source_path: None,
            goal_prose_path: None,
            node_role: BTreeMap::new(),
            under_model_assumption_nodes: BTreeSet::new(),
        }
    }
    #[test]
    fn publication_refresh_fingerprint_projection_preserves_fresh_source_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let input = fingerprint_input();
        let mut observed = WorkingSnapshot::default();
        observed
            .corr_current_fingerprints
            .insert(NodeId::from("A"), "typed statement".into());
        observed.open_nodes.insert(NodeId::from("OLD"));
        let facts = FingerprintFacts::capture(&observed);
        let receipt = FingerprintReceipt {
            input_sha256: complete_digest("fingerprint_input", &input).unwrap(),
            facts_sha256: complete_digest("fingerprint_facts", &facts).unwrap(),
            referents: BTreeMap::new(),
            facts,
            shadow_differential_agreement: true,
        };
        let mut current = WorkingSnapshot::default();
        current.present_nodes.insert(NodeId::from("A"));
        current.open_nodes.insert(NodeId::from("CURRENT"));
        receipt
            .apply_current(tmp.path(), &input, &mut current)
            .unwrap();
        assert_eq!(
            current.open_nodes,
            BTreeSet::from([NodeId::from("CURRENT")])
        );
        assert_eq!(current.present_nodes, input.present_nodes);
        assert_eq!(
            current.corr_current_fingerprints,
            observed.corr_current_fingerprints
        );
        let mut changed = input.clone();
        changed.soundness_mode = "V1".into();
        assert!(receipt
            .apply_current(tmp.path(), &changed, &mut current)
            .is_err());
        changed = input.clone();
        changed.present_nodes.insert(NodeId::from("B"));
        assert!(receipt
            .apply_current(tmp.path(), &changed, &mut current)
            .is_err());
        let mut corrupted = receipt.clone();
        corrupted.facts.corr_current_fingerprints.clear();
        assert!(corrupted
            .apply_current(tmp.path(), &input, &mut current)
            .is_err());
    }
    #[test]
    fn publication_refresh_external_referents_and_absence_are_exact() {
        let repo = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let paper = external.path().join("paper.tex");
        let mut input = fingerprint_input();
        input.paper_source_path = Some(paper.clone());
        let absent = input.referents(repo.path()).unwrap();
        assert_eq!(absent[&paper], None);
        std::fs::write(&paper, "original").unwrap();
        let first = input.referents(repo.path()).unwrap();
        assert_ne!(first, absent);
        std::fs::write(&paper, "modified").unwrap();
        assert_ne!(input.referents(repo.path()).unwrap(), first);
        #[cfg(unix)]
        {
            std::fs::remove_file(&paper).unwrap();
            std::os::unix::fs::symlink("elsewhere", &paper).unwrap();
            assert!(input.referents(repo.path()).is_none());
        }
    }
    #[test]
    fn publication_refresh_environment_rejects_changed_bytes_and_import_shadowing() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("A.olean");
        let absent = tmp.path().join("earlier/A.olean");
        std::fs::write(&file, "first").unwrap();
        let roots = BTreeSet::from([NodeId::from("A")]);
        let env = Environment {
            version: 1,
            environment_sha256s: BTreeMap::from([(NodeId::from("A"), "a".repeat(64))]),
            artifact_files: BTreeMap::from([
                (
                    file.clone(),
                    Some(format!("{:x}", Sha256::digest(b"first"))),
                ),
                (absent.clone(), None),
            ]),
        };
        assert!(env.current(&roots));
        std::fs::create_dir(absent.parent().unwrap()).unwrap();
        std::fs::write(&absent, "shadow").unwrap();
        assert!(!env.current(&roots));
        std::fs::remove_file(&absent).unwrap();
        std::fs::write(&file, "other").unwrap();
        assert!(!env.current(&roots));
        let mut changed = env.clone();
        changed
            .environment_sha256s
            .insert(NodeId::from("A"), "b".repeat(64));
        assert!(!changed.agrees(&env, &roots));
        assert!(!env.covers(&BTreeSet::from([NodeId::from("C")])));
    }
    #[test]
    fn publication_refresh_baseline_uses_actual_operands_but_fresh_cycle() {
        let mut request = trellis_kernel::WrapperRequest::default();
        request.current_present_nodes.insert(NodeId::from("A"));
        let key = |request: &trellis_kernel::WrapperRequest| {
            complete_digest(
                "baseline_input",
                &super::super::worker_baseline_input(request, true, None),
            )
            .unwrap()
        };
        let original = key(&request);
        request.cycle += 1;
        request.latest_worker_summary = "another completion".into();
        request.latest_worker_comments = "updated comments".into();
        assert_eq!(key(&request), original);
        request.current_present_nodes.insert(NodeId::from("C"));
        assert_ne!(key(&request), original);
        request.current_present_nodes.remove(&NodeId::from("C"));
        request
            .worker_acceptance
            .observation_plan
            .capture_baseline_correspondence_hashes = true;
        assert_ne!(key(&request), original);
    }
    #[test]
    fn publication_refresh_checker_entry_panics_under_receipt_only_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let result = std::panic::catch_unwind(|| {
            let _guard = trellis_kernel::receipt_only::Guard::enter((
                "lean".into(),
                "lake".into(),
                "collector".into(),
            ));
            let _ = super::super::runtime_cli_observations::run_repo_command_json(
                tmp.path(),
                "compile-node",
                &[],
            );
        });
        assert!(result.is_err());
        assert!(!trellis_kernel::receipt_only::active());
    }
    #[test]
    fn publication_refresh_cold_does_not_call_checker_or_relabel_old_output() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = trellis_kernel::receipt_only::Guard::enter((
            "lean".into(),
            "lake".into(),
            "collector".into(),
        ));
        let result = super::super::try_refresh_trellis_worker_result(
            tmp.path(),
            tmp.path(),
            &trellis_kernel::WrapperRequest::default(),
            serde_json::json!({}),
            serde_json::json!({"ok":true}),
            serde_json::json!({"outcome":"valid"}),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
        assert_eq!(result["status"], "cold");
        assert!(result.get("output").is_none());
    }
    #[test]
    fn publication_refresh_payload_rejects_transform_and_structural_work() {
        let mut payload = super::super::CheckedWorkerPayload::default();
        payload.outcome = "valid".into();
        assert!(super::super::refresh_payload_supported(&payload));
        payload
            .challenge_claim_updates
            .insert("A".into(), BTreeSet::from(["authored:T".into()]));
        assert!(!super::super::refresh_payload_supported(&payload));
        payload.challenge_claim_updates.clear();
        payload
            .target_claim_updates
            .insert("A".into(), BTreeSet::new());
        assert!(!super::super::refresh_payload_supported(&payload));
    }
}
