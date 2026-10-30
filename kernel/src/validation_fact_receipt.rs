//! Complete native phase-2 operands and successful facts for host-owned refresh.
//!
//! A receipt is not an artifact/environment attestation or a capability. It may
//! be consumed only together with an authenticated host completion and a fresh
//! complete replay-environment binding. Missing/oversized evidence is cold.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use trellis_kernel::{NodeId, WorkerValidationExecutionPlanStep, WorkerValidationStepResult};

const MAX_RECEIPT_HASH_BYTES: usize = 64 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationFactReceipt {
    pub schema_version: u8,
    pub acceptance_logic_identity: String,
    pub input_sha256: String,
    pub source_inputs_sha256: String,
    pub step_results_sha256: String,
    /// Actual observation roots, not every incidental materialized owner.
    /// Missing legacy evidence deserializes but remains a cold receipt.
    #[serde(default)]
    pub artifact_owners: BTreeSet<NodeId>,
    /// Environment recorded on both sides of the original actual consumers.
    /// A post-completion replay witness alone cannot establish what an earlier
    /// correspondence guard observed before a later build changed artifacts.
    #[serde(default)]
    pub observation_environment_sha256s: Option<BTreeMap<NodeId, String>>,
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
}

/// The canonical identity and the exact relocated bytes are supplied only by
/// the host launcher, whose prepared/completed records authenticate both.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedConfigBinding {
    pub canonical_config_sha256: String,
    pub prepared_config_sha256: String,
    #[serde(default)]
    pub supervisor_marker: Option<SupervisorMarkerBinding>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorMarkerBinding {
    pub canonical_sha256: Option<String>,
    pub prepared_sha256: String,
    pub expected: SupervisorMarker,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorMarker {
    pub worker_repo_path: std::path::PathBuf,
    pub supervisor_repo_path: std::path::PathBuf,
    pub supervisor_home: std::path::PathBuf,
    pub supervisor_cache: std::path::PathBuf,
}
impl PreparedConfigBinding {
    /// One host-generated carrier with an exact native-known layout. A worker
    /// cannot choose substitute paths/fields or rebind an arbitrary source.
    pub fn current_carriers_match(&self, repo: &std::path::Path) -> bool {
        let Some(binding) = &self.supervisor_marker else {
            return true;
        };
        let expected = &binding.expected;
        let root = &expected.worker_repo_path;
        let supervisor = root.join(".trellis/supervisor");
        if !root.is_absolute()
            || root.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || expected.supervisor_repo_path != supervisor.join("repo")
            || expected.supervisor_home != supervisor.join("home")
            || expected.supervisor_cache != supervisor.join("cache")
            || repo != expected.supervisor_repo_path
            || !is_sha256(&binding.prepared_sha256)
            || binding
                .canonical_sha256
                .as_ref()
                .is_some_and(|hash| !is_sha256(hash))
        {
            return false;
        }
        let path = repo.join(".trellis-supervisor.json");
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        if !metadata.is_file() || metadata.len() > 16 * 1024 {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return false;
            }
        }
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        format!("{:x}", Sha256::digest(&bytes)) == binding.prepared_sha256
            && serde_json::from_slice::<SupervisorMarker>(&bytes)
                .ok()
                .as_ref()
                == Some(expected)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationFactDonor {
    pub receipt: ValidationFactReceipt,
    pub step_results: Vec<WorkerValidationStepResult>,
    pub environment: ArtifactEnvironmentWitness,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactEnvironmentWitness {
    pub version: u8,
    pub donor_round: String,
    pub donor_completion_sha256: String,
    pub environment_sha256s: BTreeMap<NodeId, String>,
    pub covered_nodes: BTreeSet<NodeId>,
    /// Exact current absolute resolved files AND absent earlier search-path
    /// candidates. Checking selected files alone misses import shadowing.
    pub artifact_files: BTreeMap<std::path::PathBuf, Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct ValidationFactReuse {
    pub donor_round: String,
    pub donor_completion_sha256: String,
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct DigestWriter {
    hash: Sha256,
    remaining: usize,
}
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("validation receipt serialization limit"));
        }
        self.hash.update(bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Separate from bounded partial diagnostics: a digest exists only after the
/// entire typed value has serialized. Streaming avoids another large JSON copy.
pub fn complete_digest<T: Serialize + ?Sized>(domain: &str, value: &T) -> Option<String> {
    let mut writer = DigestWriter {
        hash: Sha256::new(),
        remaining: MAX_RECEIPT_HASH_BYTES,
    };
    writer.hash.update(b"trellis-validation-fact-v1\0");
    writer.hash.update(domain.as_bytes());
    writer.hash.update(b"\0");
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(format!("{:x}", writer.hash.finalize()))
}

pub fn source_inputs_digest(
    sources: &BTreeMap<String, String>,
    config: Option<&PreparedConfigBinding>,
) -> Option<String> {
    let Some(config) = config else {
        return complete_digest("source_inputs", sources);
    };
    if !is_sha256(&config.canonical_config_sha256)
        || !is_sha256(&config.prepared_config_sha256)
        || sources.get("trellis.config.json") != Some(&config.prepared_config_sha256)
    {
        return None;
    }
    // Explicit host carrier roles, never path-string or JSON-field filtering.
    let mut canonical_sources = sources.clone();
    canonical_sources.insert(
        "trellis.config.json".into(),
        config.canonical_config_sha256.clone(),
    );
    if let Some(marker) = &config.supervisor_marker {
        if !is_sha256(&marker.prepared_sha256)
            || sources.get(".trellis-supervisor.json") != Some(&marker.prepared_sha256)
            || marker
                .canonical_sha256
                .as_ref()
                .is_some_and(|hash| !is_sha256(hash))
        {
            return None;
        }
        if let Some(canonical) = &marker.canonical_sha256 {
            canonical_sources.insert(".trellis-supervisor.json".into(), canonical.clone());
        } else {
            canonical_sources.remove(".trellis-supervisor.json");
        }
    }
    complete_digest("source_inputs", &canonical_sources)
}

pub fn successful_plan(
    plan: &[WorkerValidationExecutionPlanStep],
    steps: &[WorkerValidationStepResult],
) -> bool {
    !plan.is_empty()
        && plan.len() == steps.len()
        && plan.iter().zip(steps).all(|(expected, step)| {
            step.kind == super::validation_step_progress_name(expected)
                && step.ok
                && step.errors.is_empty()
                && step.local_closure_results.values().all(|probe| {
                    probe.status == "ok"
                        && probe.errors.is_empty()
                        && probe.returncode == 0
                        && !probe.timed_out
                })
        })
}

impl ValidationFactReceipt {
    /// Later consumer preparation must not change any environment read by
    /// phase2. Additional final-only roots need their own before/after capture.
    pub fn observed_environment_matches(&self, current: &BTreeMap<NodeId, String>) -> bool {
        let Some(original) = &self.observation_environment_sha256s else { return false; };
        !self.artifact_owners.is_empty() && self.artifact_owners.iter().all(|node|
            original.get(node).is_some() && original.get(node) == current.get(node))
    }

    pub fn capture(
        input: &super::ExecuteWorkerValidationPlanInput,
        plan: &[WorkerValidationExecutionPlanStep],
        steps: &[WorkerValidationStepResult],
        protected: &BTreeSet<NodeId>,
        source_inputs_sha256: String,
    ) -> Option<Self> {
        if !successful_plan(plan, steps) {
            return None;
        }
        Some(Self {
            schema_version: 3,
            acceptance_logic_identity: trellis_kernel::model::acceptance_logic_identity(),
            input_sha256: complete_digest("actual_phase2_input", input)?,
            source_inputs_sha256,
            step_results_sha256: complete_digest("complete_step_results", steps)?,
            artifact_owners: required_artifact_owners(input).unwrap_or_default(),
            observation_environment_sha256s: None,
            protected_semantic_change_nodes: protected.clone(),
        })
    }
}

impl ArtifactEnvironmentWitness {
    pub fn matches_current_files(&self) -> bool {
        if self.version != 1
            || self.donor_round.is_empty()
            || !self.donor_round.bytes().all(|byte| byte.is_ascii_digit())
            || !is_sha256(&self.donor_completion_sha256)
            || self.environment_sha256s.is_empty()
            || self.covered_nodes.is_empty()
            || !self.covered_nodes.iter().all(|node| {
                self.environment_sha256s
                    .get(node)
                    .is_some_and(|hash| is_sha256(hash))
            })
            || self.artifact_files.is_empty()
        {
            return false;
        }
        self.artifact_files.iter().all(|(path, expected)| {
            if !path.is_absolute() {
                return false;
            }
            match (std::fs::symlink_metadata(path), expected) {
                (Err(error), None) => error.kind() == io::ErrorKind::NotFound,
                (Ok(metadata), Some(expected)) if metadata.is_file() && is_sha256(expected) => {
                    let Ok(mut file) = std::fs::File::open(path) else {
                        return false;
                    };
                    let mut hash = Sha256::new();
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        use std::io::Read;
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => hash.update(&buffer[..count]),
                            Err(_) => return false,
                        }
                    }
                    format!("{:x}", hash.finalize()) == *expected
                }
                _ => false,
            }
        })
    }
}

/// Authority surface: math LeanClosure and the explicit ordinary restructure lane.
/// Broader successful receipts may be recorded, but are not reusable here.
/// Parse Phase through its native aliases: bridge payloads use snake_case.
pub fn supported_refresh_request(
    request: &serde_json::Value,
    validation_kind: &str,
    plan: &[WorkerValidationExecutionPlanStep],
) -> bool {
    let phase = request
        .get("phase")
        .cloned()
        .and_then(|phase| serde_json::from_value::<trellis_kernel::Phase>(phase).ok());
    if phase != Some(trellis_kernel::Phase::ProofFormalization)
        || request
            .get("is_pv")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || request
            .get("trust_base_required_v1")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || request
            .get("cycle")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|cycle| cycle <= 1)
        || request
            .pointer("/task_execution/compiler")
            .and_then(serde_json::Value::as_str)
            != Some("managed")
    {
        return false;
    }
    let kind = request.pointer("/task_execution/task_kind").and_then(serde_json::Value::as_str);
    let [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
        active_node: Some(active), mode, authorized_nodes, protected_semantic_change_nodes,
        allow_new_obligations: false, must_close_active,
    }] = plan else { return false; };
    if request.get("active_node").and_then(serde_json::Value::as_str) != Some(active.as_str())
        || !protected_semantic_change_nodes.is_empty() { return false; }
    match kind {
        Some("lean_closure") => validation_kind == "proof_local"
            && *mode == trellis_kernel::WorkerProofDeltaMode::Local && *must_close_active
            && authorized_nodes == &BTreeSet::from([active.clone()]),
        Some("ordinary") => validation_kind == "proof_restructure"
            && *mode == trellis_kernel::WorkerProofDeltaMode::Restructure
            && authorized_nodes.contains(active)
            && ["capture_scoped_tablet_baseline_errors", "capture_baseline_correspondence_hashes"]
                .iter().all(|flag| request.pointer(&format!("/worker_acceptance/observation_plan/{flag}"))
                    == Some(&serde_json::Value::Bool(false))),
        _ => false,
    }
}

pub fn ordinary_delta_scope(input: &super::ExecuteWorkerValidationPlanInput)
    -> Option<super::runtime_cli_observations::ProofWorkerDeltaReceiptScope>
{
    let [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
        active_node: Some(active), mode: trellis_kernel::WorkerProofDeltaMode::Restructure,
        authorized_nodes, protected_semantic_change_nodes, allow_new_obligations: false, must_close_active,
    }] = input.validation_execution_plan.as_slice() else { return None; };
    if !protected_semantic_change_nodes.is_empty() || !input.node_role.is_empty()
        || !input.under_model_assumption_nodes.is_empty() || input.assumption_authoring_node.is_some()
        || input.modeling_draft || !input.seed_support_definition_nodes.is_empty()
        || !input.reserved_non_node_stems.is_empty()
    { return None; }
    let scope = super::runtime_cli_observations::proof_worker_delta_receipt_scope(
        &input.repo_path, &input.before_snapshot, &input.current_present_nodes, active,
        authorized_nodes, &input.declared_deleted_nodes, *must_close_active)?;
    // Initial deletion lane is for unclaimed ordinary helpers. Current native
    // legality and certificate reconciliation still decide whether deletion is valid.
    if scope.deleted_nodes.iter().any(|node| input.approved_target_nodes.contains(node)
        || input.current_target_claims.get(node).is_some_and(|claims| !claims.is_empty()))
    { return None; }
    Some(scope)
}

/// Exact delta-derived final roots; baseline membership remains pre-delta.
pub fn final_present_nodes(input: &super::ExecuteWorkerValidationPlanInput) -> Option<BTreeSet<NodeId>> {
    if matches!(input.validation_execution_plan.as_slice(), [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
        mode: trellis_kernel::WorkerProofDeltaMode::Restructure, ..
    }]) { Some(ordinary_delta_scope(input)?.final_present_nodes) }
    else { required_artifact_owners(input)?; Some(input.current_present_nodes.clone()) }
}

/// Active/helper/changed-owner observations plus actual approved correspondence
/// roots. Each root binds its complete imported-byte environment. Open scanned
/// owners count even when they emit no certificate.
pub fn required_artifact_owners(input: &super::ExecuteWorkerValidationPlanInput) -> Option<BTreeSet<NodeId>> {
    let mut owners = if matches!(input.validation_execution_plan.as_slice(), [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
        mode: trellis_kernel::WorkerProofDeltaMode::Restructure, ..
    }]) {
        ordinary_delta_scope(input)?.observation_nodes
    } else {
        let [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some(active), mode: trellis_kernel::WorkerProofDeltaMode::Local,
            authorized_nodes, protected_semantic_change_nodes, allow_new_obligations: false, must_close_active: true,
        }] = input.validation_execution_plan.as_slice() else { return None; };
        if authorized_nodes != &BTreeSet::from([active.clone()]) || !protected_semantic_change_nodes.is_empty()
            || !input.declared_deleted_nodes.is_empty() || !input.node_role.is_empty()
            || !input.under_model_assumption_nodes.is_empty() || input.assumption_authoring_node.is_some() || input.modeling_draft
        { return None; }
        BTreeSet::from([active.clone()])
    };
    let guarded = super::runtime_cli_observations::paper_target_corr_reopen_guard_nodes(
        &input.approved_target_nodes, &input.approved_corr_fingerprints);
    owners.extend(super::runtime_cli_observations::correspondence_semantic_payload_nodes(&guarded, &BTreeSet::new()));
    Some(owners)
}

impl ValidationFactDonor {
    /// The enclosing host invocation has already authenticated the completed
    /// donor and checked complete environment coverage. CLI self-check callers
    /// have no authority to publish their own output. This verifies every
    /// native phase-2 operand plus the exact current source/artifact bytes.
    pub fn current_facts(
        &self,
        input: &super::ExecuteWorkerValidationPlanInput,
        sources: Option<&str>,
    ) -> Option<super::ExecutedWorkerValidationPlanOutput> {
        let receipt = &self.receipt;
        if receipt.schema_version != 3
            || receipt.acceptance_logic_identity
                != trellis_kernel::model::acceptance_logic_identity()
            || Some(receipt.source_inputs_sha256.as_str()) != sources
            || complete_digest("actual_phase2_input", input).as_ref() != Some(&receipt.input_sha256)
            || complete_digest("complete_step_results", &self.step_results).as_ref()
                != Some(&receipt.step_results_sha256)
            || !successful_plan(&input.validation_execution_plan, &self.step_results)
        {
            return None;
        }
        let required = required_artifact_owners(input)?;
        if !receipt.observed_environment_matches(&self.environment.environment_sha256s) {
            return None;
        }
        if receipt.artifact_owners != required
            || !required.is_subset(&self.environment.covered_nodes)
            || trellis_kernel::tablet_target_for_repo(&input.repo_path)
                != trellis_kernel::backend::BackendId::Lean
        {
            return None;
        }
        if self
            .step_results
            .iter()
            .flat_map(|step| step.local_closure_results.keys())
            .any(|node| !self.environment.covered_nodes.contains(node))
            || !self.environment.matches_current_files()
        {
            return None;
        }
        Some(super::ExecutedWorkerValidationPlanOutput {
            step_results: self.step_results.clone(),
            protected_semantic_change_nodes: receipt.protected_semantic_change_nodes.clone(),
        })
    }

    pub fn provenance(&self) -> ValidationFactReuse {
        ValidationFactReuse {
            donor_round: self.environment.donor_round.clone(),
            donor_completion_sha256: self.environment.donor_completion_sha256.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Vec<WorkerValidationExecutionPlanStep> {
        vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some(NodeId::from("A")),
            mode: trellis_kernel::WorkerProofDeltaMode::Local,
            authorized_nodes: BTreeSet::from([NodeId::from("A")]),
            protected_semantic_change_nodes: BTreeSet::new(),
            allow_new_obligations: false,
            must_close_active: true,
        }]
    }
    fn steps() -> Vec<WorkerValidationStepResult> {
        vec![WorkerValidationStepResult {
            kind: "proof_worker_delta".into(),
            ok: true,
            ..WorkerValidationStepResult::default()
        }]
    }

    #[test]
    fn receipt_requires_complete_ordered_success_including_probe_transport() {
        let p = plan();
        let mut s = steps();
        assert!(successful_plan(&p, &s));
        assert!(!successful_plan(&[], &[]));
        assert!(!successful_plan(&p, &[]));
        s[0].kind = "scoped_tablet".into();
        assert!(!successful_plan(&p, &s));
        s = steps();
        s[0].errors.push("failed".into());
        assert!(!successful_plan(&p, &s));
        s = steps();
        let mut probe = trellis_kernel::LocalClosureProbeOutput::default();
        probe.status = "ok".into();
        s[0].local_closure_results.insert(NodeId::from("A"), probe);
        assert!(successful_plan(&p, &s));
        s[0].local_closure_results
            .get_mut(&NodeId::from("A"))
            .unwrap()
            .timed_out = true;
        assert!(!successful_plan(&p, &s));
    }

    #[test]
    fn full_receipt_binds_actual_inputs_results_sources_and_protected_set() {
        let p = plan();
        let mut s = steps();
        let mut protected = BTreeSet::new();
        let receipt = |s: &[WorkerValidationStepResult],
                       protected: &BTreeSet<NodeId>,
                       input: u8,
                       sources: &str| {
            let actual = super::super::ExecuteWorkerValidationPlanInput {
                expected_active_hash: input.to_string(),
                validation_execution_plan: p.clone(),
                ..super::super::ExecuteWorkerValidationPlanInput::default()
            };
            ValidationFactReceipt::capture(&actual, &p, s, protected, sources.into()).unwrap()
        };
        let initial = receipt(&s, &protected, 1, "source");
        assert_ne!(initial, receipt(&s, &protected, 2, "source"));
        assert_ne!(initial, receipt(&s, &protected, 1, "other source"));
        protected.insert(NodeId::from("A"));
        assert_ne!(initial, receipt(&s, &protected, 1, "source"));
        protected.clear();
        // Unlike the shadow projection, the donor receipt authenticates the
        // verbatim original result, including diagnostics retained as origin.
        s[0].build_output = "different original log".into();
        assert_ne!(initial, receipt(&s, &protected, 1, "source"));
    }

    #[test]
    fn digest_is_domain_separated_and_never_returns_a_partial_prefix() {
        assert_ne!(complete_digest("input", &1), complete_digest("result", &1));
        let mut writer = DigestWriter {
            hash: Sha256::new(),
            remaining: 2,
        };
        assert!(serde_json::to_writer(&mut writer, &"too large").is_err());
        assert!(writer.remaining <= 2);
    }

    #[test]
    fn relocated_host_config_has_one_explicit_verified_role() {
        let canonical = format!("{:x}", Sha256::digest(b"canonical config"));
        let private1 = format!(
            "{:x}",
            Sha256::digest(br#"{"repo_path":"/attempt/round1/repo","policy":"strict"}"#)
        );
        let private2 = format!(
            "{:x}",
            Sha256::digest(br#"{"repo_path":"/attempt/round2/repo","policy":"strict"}"#)
        );
        let mut sources1 = BTreeMap::from([
            ("trellis.config.json".into(), private1.clone()),
            ("Tablet/A.lean".into(), "same actual proof source".into()),
        ]);
        let mut sources2 = sources1.clone();
        sources2.insert("trellis.config.json".into(), private2.clone());
        let binding1 = PreparedConfigBinding {
            canonical_config_sha256: canonical.clone(),
            prepared_config_sha256: private1,
            supervisor_marker: None,
        };
        let mut binding2 = PreparedConfigBinding {
            canonical_config_sha256: canonical,
            prepared_config_sha256: private2,
            supervisor_marker: None,
        };
        assert_eq!(
            source_inputs_digest(&sources1, Some(&binding1)),
            source_inputs_digest(&sources2, Some(&binding2))
        );
        assert_ne!(
            source_inputs_digest(&sources1, None),
            source_inputs_digest(&sources2, None)
        );
        // A changed canonical config remains a different input even if its
        // host-prepared relocation has the same private file bytes.
        binding2.canonical_config_sha256 = "f".repeat(64);
        assert_ne!(
            source_inputs_digest(&sources1, Some(&binding1)),
            source_inputs_digest(&sources2, Some(&binding2))
        );
        sources1.insert("trellis.config.json".into(), "0".repeat(64));
        assert!(source_inputs_digest(&sources1, Some(&binding1)).is_none());
    }

    fn marker_fixture(
        root: &std::path::Path,
    ) -> (
        std::path::PathBuf,
        PreparedConfigBinding,
        BTreeMap<String, String>,
    ) {
        let repo = root.join(".trellis/supervisor/repo");
        std::fs::create_dir_all(&repo).unwrap();
        let expected = SupervisorMarker {
            worker_repo_path: root.into(),
            supervisor_repo_path: repo.clone(),
            supervisor_home: root.join(".trellis/supervisor/home"),
            supervisor_cache: root.join(".trellis/supervisor/cache"),
        };
        let bytes = serde_json::to_vec_pretty(&expected).unwrap();
        std::fs::write(repo.join(".trellis-supervisor.json"), &bytes).unwrap();
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let config = PreparedConfigBinding {
            canonical_config_sha256: "a".repeat(64),
            prepared_config_sha256: "b".repeat(64),
            supervisor_marker: Some(SupervisorMarkerBinding {
                canonical_sha256: None,
                prepared_sha256: hash.clone(),
                expected,
            }),
        };
        let sources = BTreeMap::from([
            ("trellis.config.json".into(), "b".repeat(64)),
            ("Tablet/A.lean".into(), "same source".into()),
            (".trellis-supervisor.json".into(), hash),
        ]);
        (repo, config, sources)
    }
    #[test]
    fn supervisor_marker_role_matches_real_relocated_rounds_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo1, binding1, sources1) = marker_fixture(&tmp.path().join("round1/repo"));
        let (repo2, binding2, sources2) = marker_fixture(&tmp.path().join("round2/repo"));
        assert!(binding1.current_carriers_match(&repo1));
        assert!(binding2.current_carriers_match(&repo2));
        assert!(!binding1.current_carriers_match(&repo2));
        assert_eq!(
            source_inputs_digest(&sources1, Some(&binding1)),
            source_inputs_digest(&sources2, Some(&binding2))
        );
        assert_ne!(
            source_inputs_digest(&sources1, None),
            source_inputs_digest(&sources2, None)
        );
        let canonical = BTreeMap::from([
            ("trellis.config.json".into(), "a".repeat(64)),
            ("Tablet/A.lean".into(), "same source".into()),
        ]);
        assert_eq!(
            source_inputs_digest(&sources1, Some(&binding1)),
            source_inputs_digest(&canonical, None)
        );
        let mut changed = binding2.clone();
        changed.supervisor_marker.as_mut().unwrap().canonical_sha256 = Some("f".repeat(64));
        assert_ne!(
            source_inputs_digest(&sources1, Some(&binding1)),
            source_inputs_digest(&sources2, Some(&changed))
        );
        let mut changed_sources = sources2.clone();
        changed_sources.insert(".trellis-supervisor.json".into(), "e".repeat(64));
        assert!(source_inputs_digest(&changed_sources, Some(&binding2)).is_none());
    }
    #[test]
    fn supervisor_marker_rejects_extra_fields_mutation_rebinding_and_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, mut binding, sources) = marker_fixture(&tmp.path().join("round1/repo"));
        let path = repo.join(".trellis-supervisor.json");
        let original = std::fs::read(&path).unwrap();
        let mut extra: serde_json::Value = serde_json::from_slice(&original).unwrap();
        extra["injected"] = serde_json::json!(true);
        let bytes = serde_json::to_vec(&extra).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(!binding.current_carriers_match(&repo));
        binding.supervisor_marker.as_mut().unwrap().prepared_sha256 =
            format!("{:x}", Sha256::digest(&bytes));
        assert!(!binding.current_carriers_match(&repo)); // exact hash cannot authorize unknown keys
        std::fs::write(&path, &original).unwrap();
        binding.supervisor_marker.as_mut().unwrap().prepared_sha256 =
            sources[".trellis-supervisor.json"].clone();
        binding
            .supervisor_marker
            .as_mut()
            .unwrap()
            .expected
            .supervisor_home = tmp.path().join("other home");
        assert!(!binding.current_carriers_match(&repo));
        binding
            .supervisor_marker
            .as_mut()
            .unwrap()
            .expected
            .supervisor_home = tmp.path().join("round1/repo/.trellis/supervisor/home");
        assert!(binding.current_carriers_match(&repo));
        #[cfg(unix)]
        {
            std::fs::hard_link(&path, repo.join("alias.json")).unwrap();
            assert!(!binding.current_carriers_match(&repo));
            std::fs::remove_file(repo.join("alias.json")).unwrap();
            std::fs::rename(&path, repo.join("actual.json")).unwrap();
            std::os::unix::fs::symlink("actual.json", &path).unwrap();
            assert!(!binding.current_carriers_match(&repo));
        }
    }

    fn donor_fixture() -> (
        tempfile::TempDir,
        super::super::ExecuteWorkerValidationPlanInput,
        ValidationFactDonor,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::create_dir(repo.join("Tablet")).unwrap();
        std::fs::write(repo.join("Tablet/A.lean"), "theorem A : True := True.intro").unwrap();
        let artifact = repo.join(".lake/build/lib/lean/Tablet/A.olean");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"artifact").unwrap();
        let input = super::super::ExecuteWorkerValidationPlanInput {
            repo_path: repo.to_path_buf(),
            validation_execution_plan: plan(),
            ..super::super::ExecuteWorkerValidationPlanInput::default()
        };
        let results = steps();
        let mut receipt = ValidationFactReceipt::capture(
            &input,
            &input.validation_execution_plan,
            &results,
            &BTreeSet::from([NodeId::from("Protected")]),
            "a".repeat(64),
        )
        .unwrap();
        receipt.observation_environment_sha256s =
            Some(BTreeMap::from([(NodeId::from("A"), "c".repeat(64))]));
        let donor = ValidationFactDonor {
            receipt,
            step_results: results,
            environment: ArtifactEnvironmentWitness {
                version: 1,
                donor_round: "1".into(),
                donor_completion_sha256: "b".repeat(64),
                environment_sha256s: BTreeMap::from([(NodeId::from("A"), "c".repeat(64))]),
                covered_nodes: BTreeSet::from([NodeId::from("A")]),
                artifact_files: BTreeMap::from([
                    (artifact, Some(format!("{:x}", Sha256::digest(b"artifact")))),
                    (repo.join("earlier-search-root/A.olean"), None),
                ]),
            },
        };
        (tmp, input, donor)
    }

    #[test]
    fn donor_rejects_post_only_or_different_original_observation_environment() {
        let (_tmp, input, mut donor) = donor_fixture();
        let source = "a".repeat(64);
        assert!(donor.current_facts(&input, Some(&source)).is_some());
        donor.receipt.observation_environment_sha256s = None;
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        donor.receipt.observation_environment_sha256s =
            Some(BTreeMap::from([(NodeId::from("A"), "d".repeat(64))]));
        assert!(donor.current_facts(&input, Some(&source)).is_none());
    }

    #[test]
    fn donor_requires_actual_operands_original_facts_and_complete_owner_coverage() {
        let (_tmp, mut input, mut donor) = donor_fixture();
        let source = "a".repeat(64);
        let accepted = donor.current_facts(&input, Some(&source)).unwrap();
        assert_eq!(
            accepted.protected_semantic_change_nodes,
            BTreeSet::from([NodeId::from("Protected")])
        );
        assert!(donor.current_facts(&input, None).is_none());
        input
            .current_deps
            .insert(NodeId::from("A"), BTreeSet::from([NodeId::from("B")]));
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        input.current_deps.clear();
        donor.step_results[0].build_output = "different original result".into();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        donor.step_results[0].build_output.clear();
        donor.environment.covered_nodes.clear();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
    }

    #[test]
    fn donor_rechecks_artifact_content_and_import_search_absences() {
        let (tmp, input, donor) = donor_fixture();
        let source = "a".repeat(64);
        assert!(donor.current_facts(&input, Some(&source)).is_some());
        let shadow = tmp.path().join("earlier-search-root/A.olean");
        std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
        std::fs::write(&shadow, "shadow artifact").unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        std::fs::remove_file(shadow).unwrap();
        let artifact = tmp.path().join(".lake/build/lib/lean/Tablet/A.olean");
        std::fs::write(artifact, b"modified").unwrap(); // same size; bytes are the authority
        assert!(donor.current_facts(&input, Some(&source)).is_none());
    }

    #[test]
    fn native_actual_input_relocates_only_the_typed_repository_location() {
        let mut input = super::super::ExecuteWorkerValidationPlanInput::default();
        input.repo_path = "/attempt/round1/repo".into();
        let receipt1 = ValidationFactReceipt::capture(
            &input,
            &plan(),
            &steps(),
            &BTreeSet::new(),
            "source".into(),
        )
        .unwrap();
        input.repo_path = "/attempt/round2/repo".into();
        let receipt2 = ValidationFactReceipt::capture(
            &input,
            &plan(),
            &steps(),
            &BTreeSet::new(),
            "source".into(),
        )
        .unwrap();
        assert_eq!(receipt1, receipt2);
        input.before_tablet_contents.insert(
            "A".into(),
            "/attempt/round1/repo is actual proof text".into(),
        );
        assert_ne!(
            receipt2,
            ValidationFactReceipt::capture(
                &input,
                &plan(),
                &steps(),
                &BTreeSet::new(),
                "source".into()
            )
            .unwrap()
        );
    }

    #[test]
    fn incompatible_receipt_and_malformed_host_envelopes_are_cold() {
        let (_tmp, input, mut donor) = donor_fixture();
        let source = "a".repeat(64);
        donor.receipt.schema_version += 1;
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        donor.receipt.schema_version = 3;
        donor.receipt.acceptance_logic_identity = "another native implementation".into();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        let context = serde_json::json!({"validation_kind":"proof_local"});
        let raw = serde_json::json!({"outcome":"unsupported worker outcome"});
        let ordinary = super::super::check_trellis_worker_result_output(
            input.repo_path.as_path(),
            context.clone(),
            raw.clone(),
        )
        .unwrap();
        let optional = super::super::check_trellis_worker_result_output_with_facts(
            input.repo_path.as_path(),
            context,
            raw,
            Some(serde_json::json!({"worker_claim":"trusted"})),
            Some(serde_json::json!({"reuse":true})),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(ordinary).unwrap(),
            serde_json::to_value(optional).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_artifacts_and_dangling_search_candidates_are_cold() {
        let (tmp, input, donor) = donor_fixture();
        let source = "a".repeat(64);
        let shadow = tmp.path().join("earlier-search-root/A.olean");
        std::fs::create_dir_all(shadow.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("missing"), &shadow).unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        std::fs::remove_file(shadow).unwrap();
        let artifact = tmp.path().join(".lake/build/lib/lean/Tablet/A.olean");
        let moved = artifact.with_extension("saved");
        std::fs::rename(&artifact, &moved).unwrap();
        std::os::unix::fs::symlink(moved, artifact).unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
    }

    #[test]
    fn authority_gate_matches_bridge_closure_context_and_rejects_other_lanes() {
        // Same serialized bridge shape as cr2 and the measured Packer task48:
        // lowercase phase, absent is_pv, managed single-owner local closure.
        let original = serde_json::json!({"phase":"proof_formalization", "cycle":7,
            "active_node":"A", "task_execution":{"task_kind":"lean_closure","compiler":"managed"}});
        assert!(supported_refresh_request(&original, "proof_local", &plan()));
        let mut current = original.clone();
        current["phase"] = "ProofFormalization".into();
        assert!(supported_refresh_request(&current, "proof_local", &plan()));
        for (field, value) in [
            ("phase", serde_json::json!("modeling")),
            ("is_pv", serde_json::json!(true)),
            ("cycle", serde_json::json!(1)),
            ("trust_base_required_v1", serde_json::json!(true)),
        ] {
            current = original.clone();
            current[field] = value;
            assert!(!supported_refresh_request(&current, "proof_local", &plan()));
        }
        current = original.clone();
        current["task_execution"]["task_kind"] = "ordinary".into();
        assert!(!supported_refresh_request(&current, "proof_local", &plan()));
        assert!(!supported_refresh_request(&original, "proof_easy", &plan()));
        assert!(!supported_refresh_request(
            &original,
            "proof_local",
            &[WorkerValidationExecutionPlanStep::ApprovedMeaningPreserving {}]
        ));
        let mut changed = plan();
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            allow_new_obligations,
            ..
        } = &mut changed[0]
        {
            *allow_new_obligations = true;
        }
        assert!(!supported_refresh_request(
            &original,
            "proof_local",
            &changed
        ));
    }

    #[test]
    fn actual_guard_roots_are_required_but_unrelated_materialization_is_not() {
        let (tmp, mut input, mut donor) = donor_fixture();
        let source = "a".repeat(64);
        std::fs::write(
            tmp.path().join("Tablet/C.lean"),
            "theorem C : True := True.intro",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".lake/build/lib/lean/Tablet/C.olean"),
            "legacy unrelated C",
        )
        .unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_some());
        input.approved_target_nodes.insert(NodeId::from("C"));
        input
            .approved_corr_fingerprints
            .insert(NodeId::from("C"), "approved C".into());
        assert!(donor.current_facts(&input, Some(&source)).is_none()); // changed actual operands
        donor.receipt = ValidationFactReceipt::capture(
            &input,
            &input.validation_execution_plan,
            &donor.step_results,
            &BTreeSet::new(),
            source.clone(),
        )
        .unwrap();
        donor.receipt.observation_environment_sha256s = Some(BTreeMap::from([
            (NodeId::from("A"), "c".repeat(64)),
            (NodeId::from("C"), "d".repeat(64)),
        ]));
        assert_eq!(
            donor.receipt.artifact_owners,
            BTreeSet::from([NodeId::from("A"), NodeId::from("C")])
        );
        assert!(donor.current_facts(&input, Some(&source)).is_none()); // C outside A's import closure still required
        donor.environment.covered_nodes.insert(NodeId::from("C"));
        donor
            .environment
            .environment_sha256s
            .insert(NodeId::from("C"), "d".repeat(64));
        donor.environment.artifact_files.insert(
            tmp.path().join(".lake/build/lib/lean/Tablet/C.olean"),
            Some(format!("{:x}", Sha256::digest(b"legacy unrelated C"))),
        );
        assert!(donor.current_facts(&input, Some(&source)).is_some());
        donor.receipt.artifact_owners.remove(&NodeId::from("C"));
        assert!(donor.current_facts(&input, Some(&source)).is_none()); // incomplete legacy/forged root list
    }

    #[test]
    fn shared_guard_selector_does_not_demand_unapproved_or_source_only_preamble_roots() {
        let (_tmp, mut input, _donor) = donor_fixture();
        input.approved_target_nodes = BTreeSet::from([
            NodeId::from("C"),
            NodeId::from("D"),
            NodeId::from("Preamble"),
        ]);
        input.approved_corr_fingerprints = BTreeMap::from([
            (NodeId::from("C"), "  ".into()),
            (NodeId::from("Preamble"), "source-only".into()),
        ]);
        assert_eq!(
            required_artifact_owners(&input).unwrap(),
            BTreeSet::from([NodeId::from("A")])
        );
        input
            .approved_corr_fingerprints
            .insert(NodeId::from("D"), "approved D".into());
        assert_eq!(
            required_artifact_owners(&input).unwrap(),
            BTreeSet::from([NodeId::from("A"), NodeId::from("D")])
        );
    }

    #[test]
    fn missing_owner_receipt_and_non_lean_backend_stay_cold() {
        let (tmp, input, mut donor) = donor_fixture();
        let source = "a".repeat(64);
        let saved = donor.receipt.clone();
        let mut legacy = serde_json::to_value(&saved).unwrap();
        legacy.as_object_mut().unwrap().remove("artifact_owners");
        donor.receipt = serde_json::from_value(legacy).unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
        donor.receipt = saved;
        std::fs::write(
            tmp.path().join("trellis.config.json"),
            r#"{"workflow":{"default_target":"isabelle_hol"}}"#,
        )
        .unwrap();
        assert!(donor.current_facts(&input, Some(&source)).is_none());
    }
    fn ordinary_plan(must_close_active: bool) -> Vec<WorkerValidationExecutionPlanStep> {
        vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some(NodeId::from("A")), mode: trellis_kernel::WorkerProofDeltaMode::Restructure,
            authorized_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("E"), NodeId::from("D")]),
            protected_semantic_change_nodes: BTreeSet::new(), allow_new_obligations: false, must_close_active,
        }]
    }
    fn ordinary_fixture(must_close_active: bool) -> (tempfile::TempDir, super::super::ExecuteWorkerValidationPlanInput) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("Tablet")).unwrap();
        for node in ["A", "E", "D", "C"] {
            std::fs::write(tmp.path().join(format!("Tablet/{node}.lean")), format!("theorem {node} : True := True.intro\n")).unwrap();
            std::fs::write(tmp.path().join(format!("Tablet/{node}.tex")), "statement\n").unwrap();
        }
        let input = super::super::ExecuteWorkerValidationPlanInput {
            repo_path: tmp.path().into(), active_node: Some(NodeId::from("A")),
            before_snapshot: super::super::runtime_cli_observations::snapshot_tablet_dir(tmp.path()),
            current_present_nodes: ["A", "E", "D", "C"].into_iter().map(NodeId::from).collect(),
            validation_execution_plan: ordinary_plan(must_close_active),
            ..super::super::ExecuteWorkerValidationPlanInput::default()
        };
        (tmp, input)
    }
    fn add_helper(repo: &std::path::Path, node: &str) {
        std::fs::write(repo.join(format!("Tablet/{node}.lean")), format!("import Tablet.A\ntheorem {node} : True := True.intro\n")).unwrap();
        std::fs::write(repo.join(format!("Tablet/{node}.tex")), "statement\n").unwrap();
    }
    fn ordinary_donor(input: &super::super::ExecuteWorkerValidationPlanInput) -> ValidationFactDonor {
        let roots = required_artifact_owners(input).unwrap();
        assert!(!roots.is_empty());
        let environments = roots.iter().map(|node| (node.clone(), "c".repeat(64))).collect::<BTreeMap<_, _>>();
        let mut receipt = ValidationFactReceipt::capture(input, &input.validation_execution_plan,
            &steps(), &BTreeSet::new(), "a".repeat(64)).unwrap();
        receipt.observation_environment_sha256s = Some(environments.clone());
        let artifact = input.repo_path.join("test-artifact.olean");
        std::fs::write(&artifact, b"bound artifact").unwrap();
        ValidationFactDonor { receipt, step_results: steps(), environment: ArtifactEnvironmentWitness {
            version: 1, donor_round: "1".into(), donor_completion_sha256: "b".repeat(64),
            environment_sha256s: environments, covered_nodes: roots,
            artifact_files: BTreeMap::from([(artifact, Some(format!("{:x}", Sha256::digest(b"bound artifact"))))]),
        } }
    }

    #[test]
    fn ordinary_refresh_gate_matches_both_live_plans_and_excludes_earlier_compilation() {
        let request = serde_json::json!({"phase":"proof_formalization", "cycle":846,
            "active_node":"A", "task_execution":{"task_kind":"ordinary","compiler":"managed"},
            "worker_acceptance":{"observation_plan":{"capture_scoped_tablet_baseline_errors":false,
                "capture_baseline_correspondence_hashes":false}}});
        for must_close in [false, true] {
            assert!(supported_refresh_request(&request, "proof_restructure", &ordinary_plan(must_close)));
        }
        for flag in ["capture_scoped_tablet_baseline_errors", "capture_baseline_correspondence_hashes"] {
            let mut changed = request.clone();
            changed["worker_acceptance"]["observation_plan"][flag] = true.into();
            assert!(!supported_refresh_request(&changed, "proof_restructure", &ordinary_plan(true)));
            changed["worker_acceptance"]["observation_plan"].as_object_mut().unwrap().remove(flag);
            assert!(!supported_refresh_request(&changed, "proof_restructure", &ordinary_plan(true)));
        }
        for (field, value) in [("phase", serde_json::json!("modeling")), ("cycle", serde_json::json!(1)),
            ("is_pv", serde_json::json!(true)), ("trust_base_required_v1", serde_json::json!(true))] {
            let mut changed = request.clone(); changed[field] = value;
            assert!(!supported_refresh_request(&changed, "proof_restructure", &ordinary_plan(true)));
        }
        assert!(!supported_refresh_request(&request, "proof_local", &ordinary_plan(true)));
        let mut plan = ordinary_plan(true);
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { allow_new_obligations, .. } = &mut plan[0] {
            *allow_new_obligations = true;
        }
        assert!(!supported_refresh_request(&request, "proof_restructure", &plan));
        plan = ordinary_plan(true);
        plan.push(WorkerValidationExecutionPlanStep::ApprovedMeaningPreserving {});
        assert!(!supported_refresh_request(&request, "proof_restructure", &plan));
    }

    #[test]
    fn ordinary_refresh_shared_roots_cover_birth_tex_only_open_scan_and_guard() {
        let (tmp, mut input) = ordinary_fixture(false);
        // Active is unchanged and not must-close, but the helper is observed.
        add_helper(tmp.path(), "H");
        assert_eq!(required_artifact_owners(&input).unwrap(), BTreeSet::from([NodeId::from("H")]));
        std::fs::write(tmp.path().join("Tablet/E.tex"), "changed statement\n").unwrap();
        assert_eq!(required_artifact_owners(&input).unwrap(), ["E", "H"].into_iter().map(NodeId::from).collect());
        std::fs::write(tmp.path().join("Tablet/A.lean"), "theorem A : True := by sorry\n").unwrap();
        input.approved_target_nodes.insert(NodeId::from("C"));
        input.approved_corr_fingerprints.insert(NodeId::from("C"), "approved C".into());
        let roots = required_artifact_owners(&input).unwrap();
        assert_eq!(roots, ["A", "C", "E", "H"].into_iter().map(NodeId::from).collect());
        assert!(!roots.contains(&NodeId::from("D"))); // unrelated materialization is not a phase2 read
        assert!(final_present_nodes(&input).unwrap().contains(&NodeId::from("H")));
        let mut donor = ordinary_donor(&input);
        let _guard = trellis_kernel::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
        assert!(donor.current_facts(&input, Some(&"a".repeat(64))).is_some());
        assert!(donor.step_results[0].local_closure_results.is_empty());
        donor.environment.covered_nodes.remove(&NodeId::from("A"));
        assert!(donor.current_facts(&input, Some(&"a".repeat(64))).is_none()); // scan-only owner still load-bearing
        donor.environment.covered_nodes.insert(NodeId::from("A"));
        donor.environment.environment_sha256s.remove(&NodeId::from("H"));
        assert!(donor.current_facts(&input, Some(&"a".repeat(64))).is_none());
    }

    #[test]
    fn ordinary_refresh_metadata_is_exactly_empty_new_helper_claims_and_easy_difficulty() {
        let (tmp, input) = ordinary_fixture(false);
        add_helper(tmp.path(), "H");
        let mut payload = super::super::CheckedWorkerPayload::default(); payload.outcome = "valid".into();
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input)); // missing explicit claim
        payload.target_claim_updates.insert("H".into(), BTreeSet::new());
        payload.difficulty_updates.insert("H".into(), "easy".into());
        assert!(super::super::refresh_payload_supported_for_input(&payload, &input));
        payload.target_claim_updates.get_mut("H").unwrap().insert("T".into());
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input));
        payload.target_claim_updates.get_mut("H").unwrap().clear();
        payload.difficulty_updates.insert("A".into(), "easy".into());
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input));
        payload.difficulty_updates.remove("A");
        payload.target_claim_updates.insert("A".into(), BTreeSet::new());
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input));
        payload.target_claim_updates.remove("A");
        payload.challenge_claim_updates.insert("H".into(), BTreeSet::new());
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input));
        payload.challenge_claim_updates.clear(); payload.outcome = "stuck".into();
        assert!(!super::super::refresh_payload_supported_for_input(&payload, &input));
    }

    #[test]
    fn ordinary_refresh_declared_deletion_contracts_final_membership_but_not_probe_membership() {
        let (tmp, mut input) = ordinary_fixture(true);
        let old_present = input.current_present_nodes.clone();
        for ext in ["lean", "tex"] { std::fs::remove_file(tmp.path().join(format!("Tablet/D.{ext}"))).unwrap(); }
        assert!(ordinary_delta_scope(&input).is_none()); // undeclared deletion
        input.declared_deleted_nodes.insert(NodeId::from("D"));
        let scope = ordinary_delta_scope(&input).unwrap();
        assert!(!scope.final_present_nodes.contains(&NodeId::from("D")));
        assert_eq!(input.current_present_nodes, old_present); // original executor input retained verbatim
        assert_eq!(required_artifact_owners(&input).unwrap(), BTreeSet::from([NodeId::from("A")]));
        let donor = ordinary_donor(&input);
        assert!(donor.current_facts(&input, Some(&"a".repeat(64))).is_some());
        let mut payload = super::super::CheckedWorkerPayload::default(); payload.outcome = "valid".into();
        payload.deleted_nodes.insert("D".into());
        assert!(super::super::refresh_payload_supported_for_input(&payload, &input));
        input.approved_target_nodes.insert(NodeId::from("D"));
        assert!(ordinary_delta_scope(&input).is_none()); // no silent dropping of deleted approved guard root
        input.approved_target_nodes.clear();
        input.current_target_claims.insert(NodeId::from("D"), BTreeSet::from([trellis_kernel::TargetId::from("T")]));
        assert!(ordinary_delta_scope(&input).is_none());
        input.current_target_claims.clear();
        std::fs::write(tmp.path().join("Tablet/D.tex"), "resurrected half\n").unwrap();
        assert!(ordinary_delta_scope(&input).is_none());
        assert!(donor.current_facts(&input, Some(&"a".repeat(64))).is_none());
    }

    #[test]
    fn ordinary_refresh_rejects_unauthorized_deletion_and_role_or_scope_expansion() {
        let (tmp, mut input) = ordinary_fixture(true);
        for ext in ["lean", "tex"] { std::fs::remove_file(tmp.path().join(format!("Tablet/C.{ext}"))).unwrap(); }
        input.declared_deleted_nodes.insert(NodeId::from("C"));
        assert!(ordinary_delta_scope(&input).is_none()); // C present but not authorized
        add_helper(tmp.path(), "C"); input.declared_deleted_nodes.clear();
        assert!(ordinary_delta_scope(&input).is_none()); // C now modified, still unauthorized
        // Restore a clean source, then independently test unsupported assumption roles.
        std::fs::write(tmp.path().join("Tablet/C.lean"), "theorem C : True := True.intro\n").unwrap();
        assert!(ordinary_delta_scope(&input).is_some());
        input.node_role.insert(NodeId::from("A"), trellis_kernel::PvRole::ExtractionModel);
        assert!(ordinary_delta_scope(&input).is_none());
        input.node_role.clear();
        input.under_model_assumption_nodes.insert(NodeId::from("A"));
        assert!(ordinary_delta_scope(&input).is_none());
        input.under_model_assumption_nodes.clear(); input.assumption_authoring_node = Some(NodeId::from("A"));
        assert!(ordinary_delta_scope(&input).is_none());
        input.assumption_authoring_node = None;
        if let WorkerValidationExecutionPlanStep::ProofWorkerDelta { protected_semantic_change_nodes, .. } = &mut input.validation_execution_plan[0] {
            protected_semantic_change_nodes.insert(NodeId::from("A"));
        }
        assert!(ordinary_delta_scope(&input).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_refresh_deleted_source_alias_or_unpaired_helper_is_cold() {
        let (tmp, mut input) = ordinary_fixture(true);
        for ext in ["lean", "tex"] { std::fs::remove_file(tmp.path().join(format!("Tablet/D.{ext}"))).unwrap(); }
        input.declared_deleted_nodes.insert(NodeId::from("D"));
        std::os::unix::fs::symlink("missing", tmp.path().join("Tablet/D.lean")).unwrap();
        assert!(ordinary_delta_scope(&input).is_none());
        std::fs::remove_file(tmp.path().join("Tablet/D.lean")).unwrap();
        assert!(ordinary_delta_scope(&input).is_some());
        std::fs::write(tmp.path().join("Tablet/H.lean"), "theorem H : True := True.intro").unwrap();
        assert!(ordinary_delta_scope(&input).is_none());
    }

    #[test]
    fn ordinary_refresh_helper_autofix_cannot_donate_pre_edit_facts() {
        let (tmp, input) = ordinary_fixture(true);
        add_helper(tmp.path(), "H");
        std::fs::write(tmp.path().join("Tablet/H.lean"), "theorem H : True := True.intro\n").unwrap();
        let mut donor = ordinary_donor(&input);
        let before = super::super::validation_source_inputs_digest(tmp.path(), None).unwrap();
        donor.receipt.source_inputs_sha256 = before.clone();
        assert!(donor.current_facts(&input, Some(&before)).is_some());
        super::super::normalize_node_lean_imports_on_disk(tmp.path(), "H").unwrap();
        let after = super::super::validation_source_inputs_digest(tmp.path(), None).unwrap();
        assert_ne!(before, after);
        assert!(donor.current_facts(&input, Some(&after)).is_none());
    }

    #[test]
    fn ordinary_refresh_final_consumer_preparation_preserves_phase2_environment() {
        let (_tmp, input) = ordinary_fixture(true);
        let mut donor = ordinary_donor(&input);
        let mut final_environment = donor.environment.environment_sha256s.clone();
        // Root was not a phase2 read, but is an actual later fingerprint and
        // certificate consumer. Preparing it is allowed only while preserving A.
        final_environment.insert(NodeId::from("Root"), "d".repeat(64));
        assert!(donor.receipt.observed_environment_matches(&final_environment));
        final_environment.insert(NodeId::from("A"), "e".repeat(64));
        assert!(!donor.receipt.observed_environment_matches(&final_environment));
        final_environment.remove(&NodeId::from("A"));
        assert!(!donor.receipt.observed_environment_matches(&final_environment));
        donor.receipt.observation_environment_sha256s = None;
        assert!(!donor.receipt.observed_environment_matches(&donor.environment.environment_sha256s));
    }

}
