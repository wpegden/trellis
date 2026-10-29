//! Bounded, native-produced diagnostics. Never a freshness or reuse authority.
//! Operands are serialized by reference at the actual consumer. Filesystem
//! reads inside that consumer are NOT covered by these argument digests.
use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use trellis_kernel::model::LocalClosureProbeOutput;
use trellis_kernel::{NodeId, WorkerValidationStepResult};

const MAX_GROUP_BYTES: usize = 4 << 20;
const MAX_TRACE_BYTES: usize = 16 << 20;

#[derive(Debug, Serialize)]
pub struct OperandDigest {
    pub sha256: Option<String>,
    pub serialized_bytes: usize,
    pub status: &'static str,
}

struct DigestWriter {
    hash: Sha256,
    bytes: usize,
    limit: usize,
}
impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("shadow serialization budget exhausted"));
        }
        self.hash.update(bytes);
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct ReadGroup {
    pub name: &'static str,
    pub source_phase: &'static str,
    pub purpose: &'static str,
    pub status: &'static str,
    pub input: OperandDigest,
    pub result: Option<OperandDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_fact: Option<SuccessFact>,
}

#[derive(Debug, Serialize)]
pub struct SuccessFact {
    pub projection: &'static str,
    pub status: &'static str,
    pub digest: Option<OperandDigest>,
}

#[derive(Debug, Serialize)]
pub struct AcceptanceReadTrace {
    pub version: u8,
    pub coverage: &'static str,
    pub authority: &'static str,
    pub uncovered: &'static [&'static str],
    pub groups: Vec<ReadGroup>,
    #[serde(skip)]
    remaining_bytes: usize,
    #[serde(skip)]
    remaining_fact_bytes: usize,
}

impl Default for AcceptanceReadTrace {
    fn default() -> Self {
        Self {
            version: 1,
            coverage: "PARTIAL",
            authority: "diagnostic_only",
            uncovered: &[
                "filesystem and artifact reads inside consumers; host full input key remains required",
                "baseline lint, scoped build, correspondence, paper and sketch observations",
                "autofix, FILESPEC, fingerprinting, hydration and recursive certificate reads",
                "dispatch scope, graph conflicts, gates and final publication transition",
                "early returns and unexecuted operations; absence of a group is not a cache hit",
            ],
            groups: Vec::new(),
            remaining_bytes: MAX_TRACE_BYTES,
            remaining_fact_bytes: MAX_TRACE_BYTES,
        }
    }
}

impl AcceptanceReadTrace {
    fn bounded_digest<T: Serialize + ?Sized>(remaining: &mut usize, value: &T) -> OperandDigest {
        let mut writer = DigestWriter {
            hash: Sha256::new(),
            bytes: 0,
            limit: (*remaining).min(MAX_GROUP_BYTES),
        };
        let result = serde_json::to_writer(&mut writer, value);
        *remaining = remaining.saturating_sub(writer.bytes);
        OperandDigest {
            sha256: result
                .is_ok()
                .then(|| format!("{:x}", writer.hash.finalize())),
            serialized_bytes: writer.bytes,
            status: if result.is_ok() {
                "recorded"
            } else {
                "unavailable_or_budget_exhausted"
            },
        }
    }

    pub fn begin<T: Serialize + ?Sized>(
        &mut self,
        name: &'static str,
        source_phase: &'static str,
        purpose: &'static str,
        input: &T,
    ) -> usize {
        let input = Self::bounded_digest(&mut self.remaining_bytes, input);
        let index = self.groups.len();
        self.groups.push(ReadGroup {
            name,
            source_phase,
            purpose,
            status: "started",
            input,
            result: None,
            success_fact: None,
        });
        index
    }

    pub fn finish<T: Serialize + ?Sized>(&mut self, index: usize, result: &T) {
        let digest = Self::bounded_digest(&mut self.remaining_bytes, result);
        self.groups[index].result = Some(digest);
        self.groups[index].status = "completed";
    }

    /// Additional diagnostic of successful executed steps, never a donor or a
    /// cache key. Separate budgets keep this projection from truncating any of
    /// the original raw input/result diagnostics later in the trace.
    pub fn record_validation_success(
        &mut self,
        index: usize,
        steps: &[WorkerValidationStepResult],
        protected: &BTreeSet<NodeId>,
    ) {
        let successful = !steps.is_empty()
            && steps.iter().all(|step| {
                step.ok
                    && step.errors.is_empty()
                    && step.local_closure_results.values().all(|probe| {
                        probe.status == "ok"
                            && probe.errors.is_empty()
                            && probe.returncode == 0
                            && !probe.timed_out
                    })
            });
        let digest = successful.then(|| {
            Self::bounded_digest(
                &mut self.remaining_fact_bytes,
                &SuccessfulValidationFacts {
                    steps: StepFacts(steps),
                    protected_semantic_change_nodes: protected,
                },
            )
        });
        let status = match &digest {
            Some(digest) if digest.sha256.is_some() => "recorded_successful_steps",
            Some(_) => "unavailable_or_budget_exhausted",
            None => "no_success_fact",
        };
        self.groups[index].success_fact = Some(SuccessFact {
            projection: "worker_validation_success_v1",
            status,
            digest,
        });
    }
}

#[derive(Serialize)]
struct SuccessfulValidationFacts<'a> {
    steps: StepFacts<'a>,
    protected_semantic_change_nodes: &'a BTreeSet<NodeId>,
}

struct StepFacts<'a>(&'a [WorkerValidationStepResult]);
impl Serialize for StepFacts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for step in self.0 {
            sequence.serialize_element(&StepFact(step))?;
        }
        sequence.end()
    }
}

struct StepFact<'a>(&'a WorkerValidationStepResult);
impl Serialize for StepFact<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Exhaustive typed destructuring forces new fields to be considered.
        // Successful detail/build output is prose/telemetry; failed results
        // never enter this projection. The unchanged raw trace retains both.
        let WorkerValidationStepResult {
            kind,
            ok,
            detail: _,
            errors,
            build_output: _,
            allowed_nodes,
            local_closure_results,
        } = self.0;
        let mut fact = serializer.serialize_struct("SuccessfulValidationStep", 5)?;
        fact.serialize_field("kind", kind)?;
        fact.serialize_field("ok", ok)?;
        fact.serialize_field("errors", errors)?;
        fact.serialize_field("allowed_nodes", allowed_nodes)?;
        fact.serialize_field(
            "local_closure_results",
            &ClosureFacts(local_closure_results),
        )?;
        fact.end()
    }
}

struct ClosureFacts<'a>(&'a BTreeMap<NodeId, LocalClosureProbeOutput>);
impl Serialize for ClosureFacts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (node, probe) in self.0 {
            map.serialize_entry(node, &ClosureFact(probe))?;
        }
        map.end()
    }
}

struct ClosureFact<'a>(&'a LocalClosureProbeOutput);
impl Serialize for ClosureFact<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Preserve every structured certificate/transport/policy field, even
        // conservative diagnostic commitments such as olean_sha256. Only raw
        // streams and the explicitly diagnostic pretty-print are excluded.
        // No JSON filtering, whole-probe clone, or partial certificate view.

        let LocalClosureProbeOutput {
            checker_identity,
            premise_owners,
            status,
            principal_declaration,
            declaration_manifest,
            replay_declaration_manifest,
            direct_imports,
            visibility_manifests,
            exact_declaration_uses,
            source_sha256,
            olean_sha256,
            artifact_bundle,
            manifest_gap,
            kernel_axioms,
            oracles_used,
            extra_shyps,
            residual_oracles_used,
            boundary_theorems_reached,
            statement_hash,
            statement_type_hash,
            statement_repr_typed: _,
            theorem_exists,
            boundary_theorems,
            strict_theorem_deps,
            strict_definition_deps,
            seed_support_definition_deps,
            errors,
            raw_stdout: _,
            raw_stderr: _,
            returncode,
            timed_out,
            axiomization_check,
        } = self.0;
        let mut fact = serializer.serialize_struct("SuccessfulClosureProbe", 29)?;
        fact.serialize_field("checker_identity", checker_identity)?;
        fact.serialize_field("premise_owners", premise_owners)?;
        fact.serialize_field("status", status)?;
        fact.serialize_field("principal_declaration", principal_declaration)?;
        fact.serialize_field("declaration_manifest", declaration_manifest)?;
        fact.serialize_field("replay_declaration_manifest", replay_declaration_manifest)?;
        fact.serialize_field("direct_imports", direct_imports)?;
        fact.serialize_field("visibility_manifests", visibility_manifests)?;
        fact.serialize_field("exact_declaration_uses", exact_declaration_uses)?;
        fact.serialize_field("source_sha256", source_sha256)?;
        fact.serialize_field("olean_sha256", olean_sha256)?;
        fact.serialize_field("artifact_bundle", artifact_bundle)?;
        fact.serialize_field("manifest_gap", manifest_gap)?;
        fact.serialize_field("kernel_axioms", kernel_axioms)?;
        fact.serialize_field("oracles_used", oracles_used)?;
        fact.serialize_field("extra_shyps", extra_shyps)?;
        fact.serialize_field("residual_oracles_used", residual_oracles_used)?;
        fact.serialize_field("boundary_theorems_reached", boundary_theorems_reached)?;
        fact.serialize_field("statement_hash", statement_hash)?;
        fact.serialize_field("statement_type_hash", statement_type_hash)?;
        fact.serialize_field("theorem_exists", theorem_exists)?;
        fact.serialize_field("boundary_theorems", boundary_theorems)?;
        fact.serialize_field("strict_theorem_deps", strict_theorem_deps)?;
        fact.serialize_field("strict_definition_deps", strict_definition_deps)?;
        fact.serialize_field("seed_support_definition_deps", seed_support_definition_deps)?;
        fact.serialize_field("errors", errors)?;
        fact.serialize_field("returncode", returncode)?;
        fact.serialize_field("timed_out", timed_out)?;
        fact.serialize_field("axiomization_check", axiomization_check)?;
        fact.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_borrowed_serialization_has_no_partial_digest() {
        let mut trace = AcceptanceReadTrace::default();
        trace.remaining_bytes = 4;
        let index = trace.begin("test", "baseline", "observation", &"too long");
        assert!(trace.groups[index].input.sha256.is_none());
        assert!(trace.groups[index].input.serialized_bytes <= 4);
        trace.finish(index, &false);
        assert!(trace.groups[index]
            .result
            .as_ref()
            .unwrap()
            .sha256
            .is_none());
        assert_eq!(trace.coverage, "PARTIAL");
    }
    #[test]
    fn digest_is_exact_json_without_buffering_the_value() {
        let mut trace = AcceptanceReadTrace::default();
        let index = trace.begin("test", "baseline", "observation", &vec![1, 2]);
        assert_eq!(
            trace.groups[index].input.sha256.as_deref(),
            Some(format!("{:x}", Sha256::digest(b"[1,2]")).as_str())
        );
    }

    fn successful_steps() -> Vec<WorkerValidationStepResult> {
        vec![WorkerValidationStepResult {
            kind: "proof_worker_delta".into(),
            ok: true,
            allowed_nodes: BTreeSet::from([NodeId::from("A")]),
            local_closure_results: BTreeMap::from([(
                NodeId::from("A"),
                LocalClosureProbeOutput {
                    status: "ok".into(),
                    source_sha256: "source".into(),
                    ..LocalClosureProbeOutput::default()
                },
            )]),
            ..WorkerValidationStepResult::default()
        }]
    }

    fn fact_digest(
        steps: &[WorkerValidationStepResult],
        protected: &BTreeSet<NodeId>,
    ) -> Option<String> {
        let mut trace = AcceptanceReadTrace::default();
        let group = trace.begin(
            "validation.plan",
            "post_overlay_pre_autofix",
            "observation",
            &(),
        );
        trace.finish(group, &steps);
        trace.record_validation_success(group, steps, protected);
        assert_eq!(trace.coverage, "PARTIAL");
        assert_eq!(trace.authority, "diagnostic_only");
        trace.groups[group]
            .success_fact
            .as_ref()?
            .digest
            .as_ref()?
            .sha256
            .clone()
    }

    #[test]
    fn success_facts_ignore_only_success_diagnostics_and_keep_raw_trace() {
        let before = successful_steps();
        let mut after = before.clone();
        after[0].detail = "new successful explanatory prose".into();
        after[0].build_output = "build took 900 instead of 3 seconds".into();
        let probe = after[0]
            .local_closure_results
            .get_mut(&NodeId::from("A"))
            .unwrap();
        probe.raw_stdout = "new stdout".into();
        probe.raw_stderr = "new stderr".into();
        probe.statement_repr_typed = "diagnostic pretty printing only".into();
        assert_eq!(
            fact_digest(&before, &BTreeSet::new()),
            fact_digest(&after, &BTreeSet::new())
        );
        assert_ne!(
            serde_json::to_vec(&before).unwrap(),
            serde_json::to_vec(&after).unwrap()
        );
        let mut trace = AcceptanceReadTrace::default();
        let group = trace.begin("raw", "test", "observation", &());
        trace.finish(group, &after);
        let raw = serde_json::to_value(&trace.groups[group].result).unwrap();
        let budget = trace.remaining_bytes;
        trace.record_validation_success(group, &after, &BTreeSet::new());
        assert_eq!(
            raw,
            serde_json::to_value(&trace.groups[group].result).unwrap()
        );
        assert_eq!(
            budget, trace.remaining_bytes,
            "fact budget cannot consume raw-trace budget"
        );
    }

    #[test]
    fn success_facts_bind_scope_order_and_protected_semantic_changes() {
        let steps = successful_steps();
        let before = fact_digest(&steps, &BTreeSet::new());
        assert!(before.is_some());
        assert_ne!(
            before,
            fact_digest(&steps, &BTreeSet::from([NodeId::from("B")]))
        );
        let mut changed = steps.clone();
        changed[0].allowed_nodes.insert(NodeId::from("B"));
        assert_ne!(before, fact_digest(&changed, &BTreeSet::new()));
        changed = steps.clone();
        changed[0].kind = "scoped_tablet".into();
        assert_ne!(before, fact_digest(&changed, &BTreeSet::new()));
        let mut multiple = steps.clone();
        multiple.extend(changed);
        let order = fact_digest(&multiple, &BTreeSet::new());
        multiple.reverse();
        assert_ne!(order, fact_digest(&multiple, &BTreeSet::new()));
    }

    #[test]
    fn success_facts_keep_full_typed_closure_and_artifact_fields() {
        use trellis_kernel::node_certificate::{
            ArtifactBundlePart, DeclarationManifestEntry, ObservedDeclarationUse,
            VisibilityManifest,
        };
        use trellis_kernel::trust_base::Sha256Digest;
        let steps = successful_steps();
        let before = fact_digest(&steps, &BTreeSet::new());
        // These independently vary owner bytes, imports, replay ownership,
        // visibility, dependency semantics, policy and checker implementation.
        let mutations: Vec<Box<dyn Fn(&mut LocalClosureProbeOutput)>> = vec![
            Box::new(|p| p.source_sha256 = "changed-source".into()),
            Box::new(|p| p.olean_sha256 = "conservative-legacy-binding".into()),
            Box::new(|p| {
                p.artifact_bundle.push(ArtifactBundlePart {
                    level: "private".into(),
                    sha256: Sha256Digest::from_bytes([1; 32]),
                    size_bytes: 42,
                })
            }),
            Box::new(|p| p.direct_imports = Some(BTreeSet::new())),
            Box::new(|p| {
                p.declaration_manifest.push(DeclarationManifestEntry {
                    name: "A".into(),
                    kind: "theorem".into(),
                })
            }),
            Box::new(|p| {
                p.replay_declaration_manifest
                    .push(DeclarationManifestEntry {
                        name: "A".into(),
                        kind: "definition".into(),
                    })
            }),
            Box::new(|p| p.visibility_manifests.push(VisibilityManifest::default())),
            Box::new(|p| {
                p.exact_declaration_uses
                    .push(ObservedDeclarationUse::default())
            }),
            Box::new(|p| {
                p.checker_identity = Some(trellis_kernel::closure_identity::Identity::default())
            }),
            Box::new(|p| {
                p.premise_owners.insert(NodeId::from("Assumptions"));
            }),
            Box::new(|p| {
                p.kernel_axioms.insert("sorryAx".into());
            }),
            Box::new(|p| {
                p.boundary_theorems
                    .insert(NodeId::from("B"), "statement".into());
            }),
            Box::new(|p| {
                p.strict_theorem_deps
                    .insert(NodeId::from("B"), "statement".into());
            }),
            Box::new(|p| {
                p.strict_definition_deps
                    .insert(NodeId::from("D"), "meaning".into());
            }),
            Box::new(|p| {
                p.seed_support_definition_deps
                    .insert(NodeId::from("S"), "seed".into());
            }),
            Box::new(|p| {
                p.axiomization_check =
                    Some(trellis_kernel::model::AxiomizationCheckOutput::default())
            }),
            Box::new(|p| p.statement_type_hash = "type-sensitive".into()),
        ];
        for mutate in mutations {
            let mut changed = steps.clone();
            mutate(
                changed[0]
                    .local_closure_results
                    .get_mut(&NodeId::from("A"))
                    .unwrap(),
            );
            assert_ne!(before, fact_digest(&changed, &BTreeSet::new()));
        }
    }

    #[test]
    fn skipped_failed_or_inconsistent_execution_has_no_success_fact() {
        assert!(fact_digest(&[], &BTreeSet::new()).is_none());
        for failure in 0..6 {
            let mut steps = successful_steps();
            match failure {
                0 => steps[0].ok = false,
                1 => steps[0].errors.push("step error".into()),
                _ => {
                    let probe = steps[0]
                        .local_closure_results
                        .get_mut(&NodeId::from("A"))
                        .unwrap();
                    match failure {
                        2 => probe.status = "internal_error".into(),
                        3 => probe.errors.push("probe error".into()),
                        4 => probe.returncode = 1,
                        _ => probe.timed_out = true,
                    }
                }
            }
            assert!(fact_digest(&steps, &BTreeSet::new()).is_none());
        }
    }

    #[test]
    fn fact_budget_exhaustion_never_publishes_partial_digest() {
        let mut trace = AcceptanceReadTrace::default();
        let group = trace.begin("raw", "test", "observation", &());
        trace.remaining_fact_bytes = 4;
        let raw_budget = trace.remaining_bytes;
        trace.record_validation_success(group, &successful_steps(), &BTreeSet::new());
        let fact = trace.groups[group].success_fact.as_ref().unwrap();
        assert_eq!(fact.status, "unavailable_or_budget_exhausted");
        assert!(fact.digest.as_ref().unwrap().sha256.is_none());
        assert!(fact.digest.as_ref().unwrap().serialized_bytes <= 4);
        assert_eq!(raw_budget, trace.remaining_bytes);
    }
}
