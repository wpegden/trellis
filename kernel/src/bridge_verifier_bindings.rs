use crate::{LaneId, RequestKind, WrapperRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BridgeVerifierAgent {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeVerifierLaneBinding {
    pub lane_id: LaneId,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra_args: Vec<String>,
    pub fallback_models: Vec<String>,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BridgeActorBinding {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    #[serde(default)]
    pub label: String,
}

/// Conjunctive predicate over a worker `WrapperRequest`. Each `Option`
/// field is "don't gate on this" when `None`; when `Some(want)` the
/// rule matches only if the request's corresponding field equals `want`.
/// Empty `BindingMatch::default()` matches every worker request.
///
/// This is the data-driven generalisation of the legacy hardcoded gates
/// (`blockered_worker`, `easy_close_worker`). Operators express new
/// routing rules as `worker_rules` entries in `trellis.config.json`
/// without kernel rebuilds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct BindingMatch {
    /// `true`: blockers must be present. `false`: blockers must be empty.
    /// `None`: don't gate on blocker presence.
    #[serde(default)]
    pub blockers_present: Option<bool>,
    /// Match the request's `retry_outcome_kind` exactly. Most useful as
    /// `Some(RetryOutcomeKind::None)` to gate "fresh first try only".
    #[serde(default)]
    pub retry_outcome_kind: Option<crate::model::RetryOutcomeKind>,
    /// Match `worker_context.must_close_active` exactly.
    #[serde(default)]
    pub must_close_active: Option<bool>,
    /// Match `worker_context.allow_new_obligations` exactly.
    #[serde(default)]
    pub allow_new_obligations: Option<bool>,
    /// Match `worker_context.worker_profile` exactly.
    #[serde(default)]
    pub worker_profile: Option<crate::WorkerProfile>,
}

impl BindingMatch {
    pub fn matches(&self, request: &WrapperRequest) -> bool {
        if let Some(want) = self.blockers_present {
            let actual = !request.blockers.is_empty();
            if actual != want {
                return false;
            }
        }
        if let Some(want) = self.retry_outcome_kind {
            if request.retry_outcome_kind != want {
                return false;
            }
        }
        if let Some(want) = self.must_close_active {
            if request.worker_context.must_close_active != want {
                return false;
            }
        }
        if let Some(want) = self.allow_new_obligations {
            if request.worker_context.allow_new_obligations != want {
                return false;
            }
        }
        if let Some(want) = self.worker_profile {
            if request.worker_context.worker_profile != want {
                return false;
            }
        }
        true
    }
}

/// One entry in `worker_rules`: a binding to use when `when` matches
/// the worker request. Rules are evaluated in declaration order; the
/// first match wins. Ordered AFTER the legacy `blockered_worker` /
/// `easy_close_worker` gates and BEFORE the difficulty fallback chain
/// in `resolve_request_actor_bindings`. Phase-override rules are
/// evaluated before root rules, mirroring the legacy override pattern.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct BindingRule {
    /// Free-form name for diagnostics / logging. Not used for matching.
    #[serde(default)]
    pub name: String,
    /// Conjunctive predicate. `BindingMatch::default()` matches all
    /// worker requests, so a rule with empty `when` becomes a catch-all.
    #[serde(default)]
    pub when: BindingMatch,
    pub binding: BridgeActorBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestVerifierBindings {
    #[serde(default)]
    pub paper_verify_lane_bindings: Vec<BridgeVerifierLaneBinding>,
    #[serde(default)]
    pub corr_verify_lane_bindings: Vec<BridgeVerifierLaneBinding>,
    #[serde(default)]
    pub sound_verify_lane_bindings: Vec<BridgeVerifierLaneBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestActorBindings {
    #[serde(default)]
    pub worker_binding: BridgeActorBinding,
    #[serde(default)]
    pub reviewer_binding: BridgeActorBinding,
    #[serde(default)]
    pub stuck_math_audit_binding: BridgeActorBinding,
}

#[derive(Debug, Deserialize, Default)]
struct RuntimeBridgeConfigFile {
    #[serde(default)]
    worker: BridgeActorBinding,
    #[serde(default)]
    reviewer: BridgeActorBinding,
    #[serde(default)]
    stuck_math_audit: Option<BridgeActorBinding>,
    #[serde(default)]
    easy_worker: Option<BridgeActorBinding>,
    #[serde(default)]
    hard_worker: Option<BridgeActorBinding>,
    /// Worker binding to use when the worker request has any live
    /// blockers (`request.blockers` non-empty). Takes priority over the
    /// `easy_worker` / `hard_worker` / `worker` fallback chain. When
    /// unset, behaviour is unchanged. The intent is to route a stronger
    /// (or differently-tuned) model when the dispatch is likely to
    /// involve NL/correspondence repair rather than pure Lean work.
    #[serde(default)]
    blockered_worker: Option<BridgeActorBinding>,
    /// Worker binding to use on the FIRST attempt of a close-only request:
    /// `retry_outcome_kind == None` AND `worker_context.must_close_active`
    /// AND `!worker_context.allow_new_obligations` AND no live blockers.
    /// Stuck/Invalid retries (which carry a non-None retry_outcome_kind)
    /// fall through to the difficulty chain — escalates to the default
    /// worker model on retry. Mutually exclusive with `blockered_worker`
    /// by construction (gate requires `blockers.is_empty()`); when both
    /// are configured, blocker presence picks `blockered_worker`.
    #[serde(default)]
    easy_close_worker: Option<BridgeActorBinding>,
    /// Generic rule list for routing worker bindings, evaluated AFTER
    /// the legacy `blockered_worker` / `easy_close_worker` gates and
    /// BEFORE the difficulty chain. Each rule is a (predicate, binding)
    /// pair; the first matching rule's binding wins. Lets operators
    /// add new routing rules without kernel changes.
    #[serde(default)]
    worker_rules: Vec<BindingRule>,
    #[serde(default)]
    policy_path: Option<PathBuf>,
    #[serde(default)]
    verification: RuntimeBridgeVerificationConfig,
    #[serde(default)]
    workflow: RuntimeBridgeWorkflowConfig,
    /// Plan §4.6.1 dual-collector kill-switch. Defaults to `true`
    /// (Patch A/B/C deployment runs both the primary and the
    /// axiomization-check secondary collector in every local-closure
    /// probe). Operator flips to `false` once the divergence rate has
    /// been zero for an extended period (e.g. post one full
    /// formalization run); the wrapper then accepts the (skipped)
    /// cross-check trivially. Plumbed via
    /// `resolve_local_closure_axcheck_enabled` and applied in
    /// `runtime_cli_observations::run_local_closure_axioms` by passing
    /// `--no-axcheck` to the Lean script.
    #[serde(default = "default_local_closure_axcheck_enabled")]
    local_closure_axcheck_enabled: bool,
}

fn default_local_closure_axcheck_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize, Default)]
struct RuntimeBridgeVerificationConfig {
    #[serde(default)]
    correspondence_agents: Vec<BridgeVerifierAgent>,
    #[serde(default)]
    soundness_agents: Vec<BridgeVerifierAgent>,
    /// Optional dedicated agent pool for the substantiveness
    /// lane. When unset, the lane falls back to `correspondence_agents`
    /// (same `.tex`-versus-something rubric class as corr; see plan
    /// §1.6). Letting operators wire a dedicated pool keeps the door open
    /// for cost / latency tuning if the lane proves to need different
    /// model selection from corr.
    #[serde(default)]
    substantiveness_agents: Vec<BridgeVerifierAgent>,
}

#[derive(Debug, Deserialize, Default)]
struct RuntimeBridgeWorkflowConfig {
    #[serde(default)]
    phase_overrides: std::collections::BTreeMap<String, RuntimePhaseOverride>,
}

#[derive(Debug, Deserialize, Default)]
struct RuntimePhaseOverride {
    #[serde(default)]
    worker: Option<BridgeActorBinding>,
    #[serde(default)]
    easy_worker: Option<BridgeActorBinding>,
    #[serde(default)]
    hard_worker: Option<BridgeActorBinding>,
    #[serde(default)]
    blockered_worker: Option<BridgeActorBinding>,
    #[serde(default)]
    easy_close_worker: Option<BridgeActorBinding>,
    /// Per-phase generic worker_rules. Evaluated BEFORE the root-level
    /// `worker_rules` (mirroring the legacy override pattern). Empty
    /// vector by default — falls through to root rules.
    #[serde(default)]
    worker_rules: Vec<BindingRule>,
    #[serde(default)]
    reviewer: Option<BridgeActorBinding>,
    #[serde(default)]
    stuck_math_audit: Option<BridgeActorBinding>,
    #[serde(default)]
    worker_model: Option<String>,
    #[serde(default)]
    reviewer_model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RuntimeBridgePolicyFile {
    #[serde(default)]
    verification: RuntimeBridgeVerificationPolicy,
}

#[derive(Debug, Deserialize, Default)]
struct RuntimeBridgeVerificationPolicy {
    #[serde(default)]
    correspondence_agent_selectors: Vec<String>,
    #[serde(default)]
    soundness_agent_selectors: Vec<String>,
    /// Optional selector list for the substantiveness pool.
    /// Only consulted when
    /// `RuntimeBridgeVerificationConfig::substantiveness_agents`
    /// is non-empty; falls back to the corr selector list otherwise.
    #[serde(default)]
    substantiveness_agent_selectors: Vec<String>,
}

fn normalized_text(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn agent_matches_selector(agent: &BridgeVerifierAgent, selector: &str, index: usize) -> bool {
    let normalized = normalized_text(selector);
    if normalized.is_empty() {
        return false;
    }
    let provider = normalized_text(&agent.provider);
    let model = agent
        .model
        .as_deref()
        .map(normalized_text)
        .unwrap_or_default();
    let label = normalized_text(&agent.label);
    normalized == index.to_string()
        || normalized == provider
        || normalized == model
        || normalized == label
}

fn resolve_selected_agents(
    configured_agents: &[BridgeVerifierAgent],
    selectors: &[String],
) -> Vec<BridgeVerifierAgent> {
    let normalized_selectors: Vec<String> = selectors
        .iter()
        .map(|selector| selector.trim().to_string())
        .filter(|selector| !selector.is_empty())
        .collect();
    if normalized_selectors.is_empty() {
        return configured_agents.to_vec();
    }
    let mut resolved = Vec::new();
    let mut used_indices = BTreeSet::new();
    for selector in normalized_selectors {
        let mut matched_index = None;
        for (index, agent) in configured_agents.iter().enumerate() {
            if used_indices.contains(&index) {
                continue;
            }
            if agent_matches_selector(agent, &selector, index) {
                matched_index = Some(index);
                break;
            }
        }
        if let Some(index) = matched_index {
            used_indices.insert(index);
            resolved.push(configured_agents[index].clone());
        }
    }
    if resolved.is_empty() {
        configured_agents.to_vec()
    } else {
        resolved
    }
}

fn actor_binding(binding: &BridgeActorBinding) -> BridgeActorBinding {
    binding.clone()
}

fn with_model_override(
    binding: &BridgeActorBinding,
    model_override: Option<&String>,
) -> BridgeActorBinding {
    let mut out = actor_binding(binding);
    if let Some(model) = model_override
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        out.model = Some(model.to_string());
    }
    out
}

fn phase_name(phase: crate::Phase) -> &'static str {
    match phase {
        crate::Phase::TheoremStating => "theorem_stating",
        crate::Phase::ProofFormalization => "proof_formalization",
        crate::Phase::Cleanup => "proof_complete_style_cleanup",
        crate::Phase::Complete => "complete",
    }
}

fn phase_override<'a>(
    config: &'a RuntimeBridgeConfigFile,
    phase: crate::Phase,
) -> Option<&'a RuntimePhaseOverride> {
    config.workflow.phase_overrides.get(phase_name(phase))
}

fn lane_bindings(
    configured_agents: &[BridgeVerifierAgent],
    selectors: &[String],
    verify_lanes: &BTreeSet<LaneId>,
    kind_label: &str,
) -> Result<Vec<BridgeVerifierLaneBinding>, String> {
    if verify_lanes.is_empty() {
        return Ok(Vec::new());
    }
    let resolved_agents = resolve_selected_agents(configured_agents, selectors);
    let lanes: Vec<String> = verify_lanes.iter().cloned().collect();
    if resolved_agents.len() < lanes.len() {
        return Err(format!(
            "not enough configured {kind_label} agents for requested lanes"
        ));
    }
    Ok(lanes
        .into_iter()
        .zip(resolved_agents.into_iter())
        .map(|(lane_id, agent)| BridgeVerifierLaneBinding {
            lane_id,
            provider: agent.provider,
            model: agent.model,
            effort: agent.effort,
            extra_args: agent.extra_args,
            fallback_models: agent.fallback_models,
            label: agent.label,
        })
        .collect())
}

fn resolve_policy_path(config_path: &Path, raw_policy_path: Option<&Path>) -> PathBuf {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let policy_path = raw_policy_path.unwrap_or_else(|| Path::new("trellis.policy.json"));
    if policy_path.is_absolute() {
        policy_path.to_path_buf()
    } else {
        config_dir.join(policy_path)
    }
}

fn read_bridge_config(config_path: &Path) -> Result<RuntimeBridgeConfigFile, String> {
    let text = fs::read_to_string(config_path)
        .map_err(|err| format!("failed to read config {}: {err}", config_path.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse config {}: {err}", config_path.display()))
}

fn read_bridge_policy(policy_path: &Path) -> Result<RuntimeBridgePolicyFile, String> {
    if !policy_path.exists() {
        return Ok(RuntimeBridgePolicyFile::default());
    }
    let text = fs::read_to_string(policy_path)
        .map_err(|err| format!("failed to read policy {}: {err}", policy_path.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse policy {}: {err}", policy_path.display()))
}

pub fn resolve_request_verifier_bindings(
    config_path: &Path,
    request: &WrapperRequest,
) -> Result<RequestVerifierBindings, String> {
    let config = read_bridge_config(config_path)?;
    let policy_path = resolve_policy_path(config_path, config.policy_path.as_deref());
    let policy = read_bridge_policy(&policy_path)?;

    let corr_verify_lane_bindings = if matches!(request.kind, RequestKind::Corr) {
        lane_bindings(
            &config.verification.correspondence_agents,
            &policy.verification.correspondence_agent_selectors,
            &request.verify_lanes,
            "correspondence",
        )?
    } else {
        Vec::new()
    };

    let paper_verify_lane_bindings = if matches!(request.kind, RequestKind::Paper) {
        // Per-node paper scenario uses its own pool when configured, with
        // a fallback to the corr pool (Wes's preference per plan §10).
        // Target-package scenario continues to share the soundness pool
        // (existing behaviour, unchanged for backwards compat).
        let is_per_node_scenario = !request.substantiveness_verify_nodes.is_empty()
            && request.paper_verify_targets.is_empty();
        if is_per_node_scenario {
            let (agents, selectors) = if !config.verification.substantiveness_agents.is_empty() {
                (
                    config.verification.substantiveness_agents.as_slice(),
                    policy
                        .verification
                        .substantiveness_agent_selectors
                        .as_slice(),
                )
            } else {
                (
                    config.verification.correspondence_agents.as_slice(),
                    policy
                        .verification
                        .correspondence_agent_selectors
                        .as_slice(),
                )
            };
            lane_bindings(agents, selectors, &request.verify_lanes, "substantiveness")?
        } else {
            lane_bindings(
                &config.verification.soundness_agents,
                &policy.verification.soundness_agent_selectors,
                &request.verify_lanes,
                "paper-faithfulness",
            )?
        }
    } else {
        Vec::new()
    };

    let sound_verify_lane_bindings = if matches!(request.kind, RequestKind::Sound) {
        lane_bindings(
            &config.verification.soundness_agents,
            &policy.verification.soundness_agent_selectors,
            &request.verify_lanes,
            "soundness",
        )?
    } else {
        Vec::new()
    };

    Ok(RequestVerifierBindings {
        paper_verify_lane_bindings,
        corr_verify_lane_bindings,
        sound_verify_lane_bindings,
    })
}

/// Compute the desired number of verifier lanes from the operator's config +
/// policy at init time. Used by the kernel's `Init`/`InitFromConfig` flow to
/// override the `ProtocolState::verifier_lanes` default so single-agent panels
/// produce single-lane verification (one API call per check), while existing
/// 2-agent setups keep 2-lane behavior.
///
/// Returns the max of the resolved agent counts across the three verifier
/// panels (corr / sound / substantiveness). Per-panel agent count is computed
/// via the same `resolve_selected_agents` machinery used at request time:
/// - if selectors are non-empty, the count is the number of resolved agents
///   from the selector list (after de-dup by index)
/// - if selectors are empty, the count is the number of configured agents in
///   that pool (selector fallback semantics)
///
/// Substantiveness falls back to corr when its dedicated pool is empty (mirrors
/// the per-node paper scenario in `resolve_request_verifier_bindings`), so we
/// only count it separately when it has its own configured pool.
///
/// Floors at 1 because `ProtocolState::validate` requires non-empty
/// `verifier_lanes`. Callers wanting the legacy 2-lane default for code paths
/// that don't have a config (tests, etc.) should use `default_verifier_lanes`.
pub fn resolve_verifier_lane_count(config_path: &Path) -> Result<usize, String> {
    let config = read_bridge_config(config_path)?;
    let policy_path = resolve_policy_path(config_path, config.policy_path.as_deref());
    let policy = read_bridge_policy(&policy_path)?;

    let corr_count = resolve_selected_agents(
        &config.verification.correspondence_agents,
        &policy.verification.correspondence_agent_selectors,
    )
    .len();
    let sound_count = resolve_selected_agents(
        &config.verification.soundness_agents,
        &policy.verification.soundness_agent_selectors,
    )
    .len();
    // Substantiveness only contributes its own count when a dedicated pool is
    // configured; otherwise it falls back to corr (already counted above).
    let subst_count = if config.verification.substantiveness_agents.is_empty() {
        0
    } else {
        resolve_selected_agents(
            &config.verification.substantiveness_agents,
            &policy.verification.substantiveness_agent_selectors,
        )
        .len()
    };

    let max_count = corr_count.max(sound_count).max(subst_count);
    Ok(max_count.max(1))
}

/// Build a fresh `verifier_lanes` set of size `n` using the canonical `v1..vN`
/// lane-id convention. Used by the kernel's init flow to override the default
/// after computing `resolve_verifier_lane_count`.
pub fn build_verifier_lanes(n: usize) -> BTreeSet<LaneId> {
    let n = n.max(1);
    (1..=n).map(|i| format!("v{i}")).collect()
}

/// Plan §4.6.1 kill-switch resolver: reads the
/// `local_closure_axcheck_enabled` flag from the bridge config. Default
/// is `true` (run both the primary and the axiomization-check secondary
/// collector). Operator flips to `false` once confident the two
/// implementations agree across all nodes.
///
/// `runtime_cli_observations::run_local_closure_axioms` calls this with
/// the per-repo config path; when the flag is `false`, the wrapper
/// appends `--no-axcheck` to the Lean script's CLI args so the
/// secondary pass is skipped and the wrapper accepts the (skipped)
/// cross-check trivially.
pub fn resolve_local_closure_axcheck_enabled(config_path: &Path) -> Result<bool, String> {
    let config = read_bridge_config(config_path)?;
    Ok(config.local_closure_axcheck_enabled)
}

pub fn resolve_request_actor_bindings(
    config_path: &Path,
    request: &WrapperRequest,
) -> Result<RequestActorBindings, String> {
    let config = read_bridge_config(config_path)?;
    let phase_override = phase_override(&config, request.phase);
    let worker_binding = if request.kind == RequestKind::Worker {
        // When the worker request has any live blockers, prefer the
        // `blockered_worker` binding (typically a stronger model tuned
        // for NL / correspondence repair) over the difficulty-keyed
        // chain. Phase override wins over the root-level value, both
        // win over the difficulty fallback.
        let blockered_override = if !request.blockers.is_empty() {
            phase_override
                .and_then(|override_cfg| override_cfg.blockered_worker.as_ref())
                .or(config.blockered_worker.as_ref())
                .map(actor_binding)
        } else {
            None
        };
        // First try of a close-only request: ordered AFTER `blockered_override`
        // (so a live blocker still wins) and BEFORE the difficulty chain (so
        // the close-only first-try wins over the routine `easy_worker` /
        // `hard_worker`). Stuck/Invalid retries carry a non-None
        // `retry_outcome_kind` and fall through to the difficulty chain.
        // Phase override wins over root-level binding.
        let easy_close_override = if request.blockers.is_empty()
            && request.retry_outcome_kind == crate::model::RetryOutcomeKind::None
            && request.worker_context.must_close_active
            && !request.worker_context.allow_new_obligations
        {
            phase_override
                .and_then(|override_cfg| override_cfg.easy_close_worker.as_ref())
                .or(config.easy_close_worker.as_ref())
                .map(actor_binding)
        } else {
            None
        };
        // Generic operator-defined rules. Walked in order: phase rules
        // first (more specific), then root rules. First match wins. Lets
        // operators add new routing decisions without kernel changes.
        let user_rule_match = phase_override
            .map(|cfg| cfg.worker_rules.as_slice())
            .unwrap_or(&[])
            .iter()
            .chain(config.worker_rules.iter())
            .find(|rule| rule.when.matches(request))
            .map(|rule| actor_binding(&rule.binding));

        let binding = if let Some(binding) = blockered_override {
            binding
        } else if let Some(binding) = easy_close_override {
            binding
        } else if let Some(binding) = user_rule_match {
            binding
        } else {
            match request.worker_context.worker_profile {
                crate::WorkerProfile::ProofEasy => phase_override
                    .and_then(|override_cfg| override_cfg.easy_worker.as_ref())
                    .map(actor_binding)
                    .or_else(|| {
                        phase_override
                            .and_then(|override_cfg| override_cfg.worker.as_ref())
                            .map(actor_binding)
                    })
                    .or_else(|| config.easy_worker.as_ref().map(actor_binding))
                    .unwrap_or_else(|| actor_binding(&config.worker)),
                crate::WorkerProfile::ProofHard
                | crate::WorkerProfile::Cleanup
                | crate::WorkerProfile::FinalCleanup => phase_override
                    .and_then(|override_cfg| override_cfg.hard_worker.as_ref())
                    .map(actor_binding)
                    .or_else(|| {
                        phase_override
                            .and_then(|override_cfg| override_cfg.worker.as_ref())
                            .map(actor_binding)
                    })
                    .or_else(|| config.hard_worker.as_ref().map(actor_binding))
                    .unwrap_or_else(|| actor_binding(&config.worker)),
                crate::WorkerProfile::Theorem | crate::WorkerProfile::None => phase_override
                    .and_then(|override_cfg| override_cfg.worker.as_ref())
                    .map(actor_binding)
                    .or_else(|| {
                        phase_override.map(|override_cfg| {
                            with_model_override(&config.worker, override_cfg.worker_model.as_ref())
                        })
                    })
                    .unwrap_or_else(|| actor_binding(&config.worker)),
            }
        };
        if binding.provider.trim().is_empty() {
            return Err("worker binding is missing provider configuration".into());
        }
        binding
    } else {
        BridgeActorBinding::default()
    };

    // Cleanup-v2 (CLAUDES_NOTES_cleanup_v2_impl_plan.md §2 / step 20):
    // `RequestKind::Audit` is an LLM-driven structured-output task on the
    // same model class as the reviewer. Reuse `reviewer_binding`. Verified
    // by `audit_request_resolves_to_reviewer_binding` test in this module.
    let reviewer_binding = if matches!(request.kind, RequestKind::Review | RequestKind::Audit) {
        let binding = phase_override
            .and_then(|override_cfg| override_cfg.reviewer.as_ref())
            .map(actor_binding)
            .or_else(|| {
                phase_override.map(|override_cfg| {
                    with_model_override(&config.reviewer, override_cfg.reviewer_model.as_ref())
                })
            })
            .unwrap_or_else(|| actor_binding(&config.reviewer));
        if binding.provider.trim().is_empty() {
            return Err("reviewer binding is missing provider configuration".into());
        }
        binding
    } else {
        BridgeActorBinding::default()
    };

    let stuck_math_audit_binding = if request.kind == RequestKind::StuckMathAudit {
        let binding = phase_override
            .and_then(|override_cfg| override_cfg.stuck_math_audit.as_ref())
            .map(actor_binding)
            .or_else(|| config.stuck_math_audit.as_ref().map(actor_binding))
            .or_else(|| {
                phase_override
                    .and_then(|override_cfg| override_cfg.reviewer.as_ref())
                    .map(actor_binding)
            })
            .or_else(|| {
                phase_override.map(|override_cfg| {
                    with_model_override(&config.reviewer, override_cfg.reviewer_model.as_ref())
                })
            })
            .unwrap_or_else(|| actor_binding(&config.reviewer));
        if binding.provider.trim().is_empty() {
            return Err("stuck math audit binding is missing provider configuration".into());
        }
        binding
    } else {
        BridgeActorBinding::default()
    };

    Ok(RequestActorBindings {
        worker_binding,
        reviewer_binding,
        stuck_math_audit_binding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Blocker, BlockerKind, BlockerObject, GateKind, NodeId, NodeKind, Phase, TaskMode,
        WorkerAcceptanceContract, WorkerContext,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use tempfile::tempdir;

    fn sample_request(kind: RequestKind) -> WrapperRequest {
        WrapperRequest {
            id: 1,
            kind,
            cycle: 1,
            phase: Phase::TheoremStating,
            active_node: None,
            held_target: None,
            mode: TaskMode::Global,
            blockers: BTreeSet::<Blocker>::new(),
            blocked_targets: BTreeSet::new(),
            configured_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::new(),
            verify_targets: BTreeSet::new(),
            verify_lanes: BTreeSet::from(["v1".to_string(), "v2".to_string()]),
            paper_verify_lane_bindings: Vec::new(),
            corr_verify_lane_bindings: Vec::new(),
            sound_verify_lane_bindings: Vec::new(),
            worker_binding: BridgeActorBinding::default(),
            reviewer_binding: BridgeActorBinding::default(),
            stuck_math_audit_binding: BridgeActorBinding::default(),
            paper_verify_targets: BTreeSet::new(),
            substantiveness_verify_nodes: BTreeSet::new(),
            deviation_verify_id: None,
            deviation_verify_path: String::new(),
            authorized_deviations: BTreeMap::new(),
            current_deviation_files: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            corr_verify_nodes: BTreeSet::new(),
            corr_verify_targets: BTreeSet::new(),
            sound_verify_nodes: BTreeSet::new(),
            sound_verify_node: None,
            runtime_support_required: kind.requires_runtime_support(),
            coarse_dag_nodes: BTreeSet::new(),
            active_coarse_node: None,
            kernel_hinted_next_active_coarse_nodes: BTreeSet::new(),
            coarse_repair_mode: false,
            cycles_in_coarse_repair_mode: 0,
            coarse_anchor_starvation_unlocked: false,
            protected_semantic_change_confirmation: None,
            protected_reapproval_nodes: BTreeSet::new(),
            allowed_decisions: BTreeSet::new(),
            allowed_next_modes: BTreeSet::new(),
            kernel_hinted_next_active_nodes: BTreeSet::new(),
            proof_active_node_base_legal_candidates: BTreeSet::new(),
            coarse_repair_blocker_carriers: BTreeSet::new(),
            ever_shallow_coarse_closed: BTreeSet::new(),
            ever_shallow_coarse_closed_regressed: BTreeSet::new(),
            pending_global_repair_request: None,
            pending_global_repair_grant: None,
            latest_global_repair_audit_decline_reason: String::new(),
            global_repair_mode_enabled: true,
            consumed_global_repair_grant: false,
            targeted_next_active_nodes: BTreeSet::new(),
            allow_targeted_without_next_active: false,
            allowed_resets: BTreeSet::new(),
            resettable_theorem_stating_nodes: BTreeSet::new(),
            allowed_reset_blockers: BTreeSet::new(),
            allowed_override_blockers: BTreeSet::new(),
            sound_repair_ready_nodes: BTreeSet::new(),
            sound_verifier_requestable_nodes: BTreeSet::new(),
            sound_assessment_statuses: BTreeMap::new(),
            sound_reverification_context: None,
            cycles_since_clean: 0,
            no_sound_progress_window_cycles: 0,
            shallow_coarse_closed_count: 0,
            cycles_since_shallow_coarse_closed_count_increase: 0,
            last_clean_rewind_count: 0,
            stuck_math_audit: crate::model::StuckMathAuditState::default(),
            audit_plan: None,
            previous_audit_plan_snapshot: None,
            latest_stuck_math_audit_rejection_reason: String::new(),
            allowed_difficulty_update_nodes: BTreeSet::new(),
            current_present_nodes: BTreeSet::new(),
            current_proof_nodes: BTreeSet::new(),
            current_node_kinds: BTreeMap::<NodeId, NodeKind>::new(),
            current_deps: BTreeMap::new(),
            current_target_claims: BTreeMap::new(),
            current_paper_approved_fingerprints: BTreeMap::new(),
            approved_target_nodes: BTreeSet::new(),
            approved_corr_fingerprints: BTreeMap::new(),
            reviewer_comments: String::new(),
            latest_worker_summary: String::new(),
            latest_worker_comments: String::new(),
            latest_worker_needs_restructure_suggested_nodes: BTreeSet::new(),
            deterministic_worker_rejection_reasons: Vec::new(),
            latest_review_rejection_reasons: Vec::new(),
            review_verifier_evidence: crate::model::ReviewVerifierEvidence::default(),
            previous_corr_lane_findings: BTreeMap::new(),
            previous_substantiveness_lane_findings: BTreeMap::new(),
            previous_sound_lane_findings: BTreeMap::new(),
            retry_outcome_kind: crate::model::RetryOutcomeKind::None,
            retry_attempt: 0,
            post_advance_routing: false,
            fresh_context: false,
            prompt_contract_version: 0,
            project_invariants: crate::default_contract_value(),
            paper_contract: crate::default_contract_value(),
            corr_contract: crate::default_contract_value(),
            sound_contract: crate::default_contract_value(),
            worker_contract: crate::default_contract_value(),
            review_contract: crate::default_contract_value(),
            audit_contract: crate::default_contract_value(),
            stuck_math_audit_contract: crate::default_contract_value(),
            cleanup_audit_tasks_view: Vec::new(),
            cleanup_audit_scratchpad_view: String::new(),
            cleanup_audit_round_view: 0,
            cleanup_audit_burst_count_view: 0,
            cleanup_protected_statement_node_set_view: BTreeSet::new(),
            latest_audit_rejection_reason_view: String::new(),
            cleanup_force_done_view: false,
            worker_context: WorkerContext::default(),
            worker_acceptance: WorkerAcceptanceContract::default(),
            previous_paper_lane_findings: BTreeMap::new(),
            invalid_attempt: false,
            human_input_outstanding: false,
            gate_kind: GateKind::None,
            local_closure_unverified: BTreeMap::new(),
        }
    }

    fn write_config(temp: &Path, policy_name: &str) -> PathBuf {
        let config_path = temp.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "policy_path": policy_name,
                "verification": {
                    "correspondence_agents": [
                        {"provider": "claude", "model": "corr-a", "label": "claude-a"},
                        {"provider": "gemini", "model": "corr-b", "label": "gemini-b"}
                    ],
                    "soundness_agents": [
                        {"provider": "claude", "model": "snd-a", "label": "claude-a"},
                        {"provider": "gemini", "model": "snd-b", "label": "gemini-b"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        config_path
    }

    #[test]
    fn resolves_selected_agents_by_label() {
        let tmp = tempdir().expect("tempdir");
        let config_path = write_config(tmp.path(), "trellis.policy.json");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "correspondence_agent_selectors": ["gemini-b", "claude-a"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        let output =
            resolve_request_verifier_bindings(&config_path, &sample_request(RequestKind::Corr))
                .expect("resolve bindings");
        assert_eq!(
            output
                .corr_verify_lane_bindings
                .iter()
                .map(|binding| (
                    binding.lane_id.as_str(),
                    binding.provider.as_str(),
                    binding.label.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("v1", "gemini", "gemini-b"), ("v2", "claude", "claude-a")]
        );
        assert!(output.sound_verify_lane_bindings.is_empty());
    }

    #[test]
    fn falls_back_to_full_catalog_when_selectors_resolve_nothing() {
        let tmp = tempdir().expect("tempdir");
        let config_path = write_config(tmp.path(), "trellis.policy.json");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "soundness_agent_selectors": ["missing"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        let output =
            resolve_request_verifier_bindings(&config_path, &sample_request(RequestKind::Sound))
                .expect("resolve bindings");
        assert_eq!(
            output
                .sound_verify_lane_bindings
                .iter()
                .map(|binding| binding.provider.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "gemini"]
        );
    }

    #[test]
    fn errors_when_resolved_agents_are_fewer_than_requested_lanes() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "verification": {
                    "correspondence_agents": [
                        {"provider": "claude", "model": "corr-a", "label": "claude-a"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");

        let error =
            resolve_request_verifier_bindings(&config_path, &sample_request(RequestKind::Corr))
                .expect_err("bindings should fail");
        assert!(error.contains("not enough configured correspondence agents"));
    }

    #[test]
    fn worker_bindings_can_vary_by_phase_and_profile() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "codex", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "theorem_stating": {
                            "worker": {"provider": "claude", "model": "theorem-worker"}
                        },
                        "proof_formalization": {
                            "worker": {"provider": "codex", "model": "proof-worker"},
                            "easy_worker": {"provider": "gemini", "model": "proof-easy"},
                            "hard_worker": {"provider": "gemini", "model": "proof-hard"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let mut theorem_request = sample_request(RequestKind::Worker);
        theorem_request.phase = Phase::TheoremStating;
        theorem_request.worker_context.worker_profile = crate::WorkerProfile::Theorem;

        let theorem_bindings = resolve_request_actor_bindings(&config_path, &theorem_request)
            .expect("theorem bindings");
        assert_eq!(theorem_bindings.worker_binding.provider, "claude");
        assert_eq!(
            theorem_bindings.worker_binding.model.as_deref(),
            Some("theorem-worker")
        );

        let mut proof_easy_request = sample_request(RequestKind::Worker);
        proof_easy_request.phase = Phase::ProofFormalization;
        proof_easy_request.worker_context.worker_profile = crate::WorkerProfile::ProofEasy;

        let proof_easy_bindings = resolve_request_actor_bindings(&config_path, &proof_easy_request)
            .expect("proof easy bindings");
        assert_eq!(proof_easy_bindings.worker_binding.provider, "gemini");
        assert_eq!(
            proof_easy_bindings.worker_binding.model.as_deref(),
            Some("proof-easy")
        );

        let mut proof_hard_request = sample_request(RequestKind::Worker);
        proof_hard_request.phase = Phase::ProofFormalization;
        proof_hard_request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;

        let proof_hard_bindings = resolve_request_actor_bindings(&config_path, &proof_hard_request)
            .expect("proof hard bindings");
        assert_eq!(proof_hard_bindings.worker_binding.provider, "gemini");
        assert_eq!(
            proof_hard_bindings.worker_binding.model.as_deref(),
            Some("proof-hard")
        );
    }

    fn sample_blocker(node: &str) -> Blocker {
        Blocker {
            kind: BlockerKind::NodeCorr,
            object: BlockerObject::Node {
                node: NodeId::from(node),
            },
            fingerprint: format!("fp-{node}"),
            deferred: false,
        }
    }

    #[test]
    fn blockered_worker_overrides_difficulty_chain_when_blockers_present() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "blockered_worker": {"provider": "codex", "model": "global-blockered"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        // Hard request WITH a live blocker → blockered_worker wins.
        let mut blockered = sample_request(RequestKind::Worker);
        blockered.phase = Phase::ProofFormalization;
        blockered.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        blockered.blockers = BTreeSet::from([sample_blocker("a")]);
        let bindings =
            resolve_request_actor_bindings(&config_path, &blockered).expect("blockered bindings");
        assert_eq!(bindings.worker_binding.provider, "codex");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-blockered")
        );

        // Same request shape but no blockers → falls through to hard_worker.
        let mut clean = blockered.clone();
        clean.blockers = BTreeSet::new();
        let bindings =
            resolve_request_actor_bindings(&config_path, &clean).expect("clean bindings");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn blockered_worker_phase_override_wins_over_root() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "blockered_worker": {"provider": "codex", "model": "root-blockered"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "proof_formalization": {
                            "blockered_worker": {"provider": "codex", "model": "phase-blockered"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofEasy;
        request.blockers = BTreeSet::from([sample_blocker("a")]);
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("phase blockered");
        assert_eq!(bindings.worker_binding.provider, "codex");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("phase-blockered")
        );
    }

    fn close_only_request() -> WrapperRequest {
        // First-try, must-close-active, no-new-obligations, no blockers.
        // Default `retry_outcome_kind` is None on `sample_request` already.
        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.must_close_active = true;
        request.worker_context.allow_new_obligations = false;
        request
    }

    #[test]
    fn easy_close_worker_picked_on_fresh_close_attempt() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "easy_close_worker": {"provider": "gemini", "model": "global-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        let request = close_only_request();
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("easy-close bindings");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-easy-close"),
        );
    }

    #[test]
    fn easy_close_worker_skipped_after_stuck_retry() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "easy_close_worker": {"provider": "gemini", "model": "global-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        // Stuck retry → non-None retry_outcome_kind → falls through.
        let mut request = close_only_request();
        request.retry_outcome_kind = crate::model::RetryOutcomeKind::Stuck;
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("stuck retry bindings");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );

        // Same request shape but Invalid retry → also falls through.
        let mut request = close_only_request();
        request.retry_outcome_kind = crate::model::RetryOutcomeKind::Invalid;
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("invalid retry bindings");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn easy_close_worker_skipped_when_blockers_present() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "blockered_worker": {"provider": "codex", "model": "global-blockered"},
                "easy_close_worker": {"provider": "gemini", "model": "global-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        // close-only shape AND blockers present → blockered_worker wins,
        // easy_close_worker is skipped because the gate requires empty blockers.
        let mut request = close_only_request();
        request.blockers = BTreeSet::from([sample_blocker("a")]);
        let bindings = resolve_request_actor_bindings(&config_path, &request)
            .expect("blocker overrides easy-close");
        assert_eq!(bindings.worker_binding.provider, "codex");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-blockered"),
        );
    }

    #[test]
    fn easy_close_worker_skipped_when_new_obligations_allowed() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "easy_close_worker": {"provider": "gemini", "model": "global-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = close_only_request();
        request.worker_context.allow_new_obligations = true; // gate fails.
        let bindings = resolve_request_actor_bindings(&config_path, &request)
            .expect("new-obligations bindings");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn easy_close_worker_skipped_when_must_close_false() {
        // Documents that the gate naturally won't fire on Cleanup or
        // orphan-cleanup Worker requests where `must_close_active=false`
        // by construction (engine.rs:444-475 schedule_orphan_cleanup; also
        // any default-pending-task path which produces (false, true)).
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "easy_close_worker": {"provider": "gemini", "model": "global-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = close_only_request();
        request.worker_context.must_close_active = false; // gate fails.
        let bindings = resolve_request_actor_bindings(&config_path, &request)
            .expect("must-close-false bindings");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn easy_close_worker_phase_override_wins_over_root() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "easy_close_worker": {"provider": "gemini", "model": "root-easy-close"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "proof_formalization": {
                            "easy_close_worker": {"provider": "gemini", "model": "phase-easy-close"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let request = close_only_request();
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("phase easy-close");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("phase-easy-close"),
        );
    }

    #[test]
    fn easy_close_worker_phase_override_used_when_root_unset() {
        // Pure fallback case: no root binding, only the phase override.
        // Distinct from "phase wins over root" — here root has nothing to win
        // against.
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "proof_formalization": {
                            "easy_close_worker": {"provider": "gemini", "model": "phase-only-easy-close"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let request = close_only_request();
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("phase-only easy-close");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("phase-only-easy-close"),
        );
    }

    // ---- Generic worker_rules (data-driven gate) -----------------

    #[test]
    fn worker_rules_match_on_allow_new_obligations() {
        // Variant A scenario: route to codex any time allow_new_obligations
        // is true. Mirrors the planned use case for operator-defined
        // gate-driven routing without kernel rebuilds.
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "new_obligations",
                    "when": {"allow_new_obligations": true},
                    "binding": {"provider": "codex", "model": "gpt-5.5", "effort": "xhigh", "label": "codex-xhigh"}
                }]
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.allow_new_obligations = true;
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(bindings.worker_binding.provider, "codex");
        assert_eq!(bindings.worker_binding.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(bindings.worker_binding.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn worker_rules_skip_when_match_field_doesnt_match() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "new_obligations",
                    "when": {"allow_new_obligations": true},
                    "binding": {"provider": "codex", "model": "gpt-5.5", "effort": "xhigh", "label": "codex-xhigh"}
                }]
            })
            .to_string(),
        )
        .expect("write config");

        // allow_new_obligations=false: rule does not match → falls through.
        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.allow_new_obligations = false;
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn worker_rules_legacy_blockered_takes_priority() {
        // Confirms blockered_worker still wins over user rules — legacy
        // gates run BEFORE worker_rules.
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "blockered_worker": {"provider": "codex", "model": "global-blockered"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "new_obligations",
                    "when": {"allow_new_obligations": true},
                    "binding": {"provider": "anthropic", "model": "claude-loser"}
                }]
            })
            .to_string(),
        )
        .expect("write config");

        // Request matches BOTH blockered (blockers present) AND user rule
        // (allow_new_obligations=true). Legacy blockered wins.
        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.allow_new_obligations = true;
        request.blockers = BTreeSet::from([sample_blocker("a")]);
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(bindings.worker_binding.provider, "codex");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-blockered"),
        );
    }

    #[test]
    fn worker_rules_first_match_wins() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [
                    {"name": "first",
                     "when": {"allow_new_obligations": true},
                     "binding": {"provider": "codex", "model": "first-match"}},
                    {"name": "second",
                     "when": {"allow_new_obligations": true},
                     "binding": {"provider": "claude", "model": "second-match"}}
                ]
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.allow_new_obligations = true;
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("first-match")
        );
    }

    #[test]
    fn worker_rules_phase_override_wins_over_root() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "root-rule",
                    "when": {"allow_new_obligations": true},
                    "binding": {"provider": "codex", "model": "root-match"}
                }],
                "workflow": {
                    "phase_overrides": {
                        "proof_formalization": {
                            "worker_rules": [{
                                "name": "phase-rule",
                                "when": {"allow_new_obligations": true},
                                "binding": {"provider": "codex", "model": "phase-match"}
                            }]
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.worker_context.allow_new_obligations = true;
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("phase-match")
        );
    }

    #[test]
    fn worker_rules_empty_when_matches_everything() {
        // BindingMatch::default() is a catch-all. Useful as a fallback rule.
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "catchall",
                    "when": {},
                    "binding": {"provider": "codex", "model": "catch-all"}
                }]
            })
            .to_string(),
        )
        .expect("write config");

        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        let bindings = resolve_request_actor_bindings(&config_path, &request).expect("resolved");
        assert_eq!(bindings.worker_binding.model.as_deref(), Some("catch-all"));
    }

    #[test]
    fn worker_rules_composite_match_all_fields() {
        // Multiple match fields are conjunctive (AND).
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "worker_rules": [{
                    "name": "complex",
                    "when": {
                        "allow_new_obligations": true,
                        "must_close_active": false,
                        "retry_outcome_kind": "None"
                    },
                    "binding": {"provider": "codex", "model": "complex-match"}
                }]
            })
            .to_string(),
        )
        .expect("write config");

        // All three conditions met → match.
        let mut req = sample_request(RequestKind::Worker);
        req.phase = Phase::ProofFormalization;
        req.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        req.worker_context.allow_new_obligations = true;
        req.worker_context.must_close_active = false;
        let b = resolve_request_actor_bindings(&config_path, &req).expect("r");
        assert_eq!(b.worker_binding.model.as_deref(), Some("complex-match"));

        // Flip must_close_active → no match → falls through.
        let mut req2 = sample_request(RequestKind::Worker);
        req2.phase = Phase::ProofFormalization;
        req2.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        req2.worker_context.allow_new_obligations = true;
        req2.worker_context.must_close_active = true;
        let b2 = resolve_request_actor_bindings(&config_path, &req2).expect("r");
        assert_eq!(b2.worker_binding.model.as_deref(), Some("global-hard"));
    }

    #[test]
    fn easy_close_worker_unset_preserves_legacy_behavior() {
        // With no `easy_close_worker` configured, a close-only first-try
        // request still routes through the difficulty chain unchanged.
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        let request = close_only_request();
        let bindings =
            resolve_request_actor_bindings(&config_path, &request).expect("legacy bindings");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn blockered_worker_unset_preserves_legacy_behavior() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "gemini", "model": "global-worker"},
                "easy_worker": {"provider": "gemini", "model": "global-easy"},
                "hard_worker": {"provider": "gemini", "model": "global-hard"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"}
            })
            .to_string(),
        )
        .expect("write config");

        // With blockers present but no blockered_worker configured, the
        // selector falls back to the difficulty chain unchanged.
        let mut request = sample_request(RequestKind::Worker);
        request.phase = Phase::ProofFormalization;
        request.worker_context.worker_profile = crate::WorkerProfile::ProofHard;
        request.blockers = BTreeSet::from([sample_blocker("a")]);
        let bindings = resolve_request_actor_bindings(&config_path, &request)
            .expect("bindings without blockered_worker");
        assert_eq!(bindings.worker_binding.provider, "gemini");
        assert_eq!(
            bindings.worker_binding.model.as_deref(),
            Some("global-hard")
        );
    }

    #[test]
    fn phase_overrides_support_legacy_model_only_overrides() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "codex", "model": "global-worker"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "theorem_stating": {
                            "worker_model": "theorem-worker-model",
                            "reviewer_model": "theorem-reviewer-model"
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        let mut worker_request = sample_request(RequestKind::Worker);
        worker_request.phase = Phase::TheoremStating;
        worker_request.worker_context.worker_profile = crate::WorkerProfile::Theorem;
        let worker_bindings =
            resolve_request_actor_bindings(&config_path, &worker_request).expect("worker bindings");
        assert_eq!(worker_bindings.worker_binding.provider, "codex");
        assert_eq!(
            worker_bindings.worker_binding.model.as_deref(),
            Some("theorem-worker-model")
        );

        let mut review_request = sample_request(RequestKind::Review);
        review_request.phase = Phase::TheoremStating;
        let review_bindings =
            resolve_request_actor_bindings(&config_path, &review_request).expect("review bindings");
        assert_eq!(review_bindings.reviewer_binding.provider, "claude");
        assert_eq!(
            review_bindings.reviewer_binding.model.as_deref(),
            Some("theorem-reviewer-model")
        );
    }

    // Cleanup-v2 (CLAUDES_NOTES_cleanup_v2_impl_plan.md §2 / step 20):
    // `RequestKind::Audit` reuses the reviewer binding. This test pins the
    // bridge-side routing decision so the cleanup audit lane gets a
    // properly-configured LLM dispatch (matching what Review gets) rather
    // than the empty `BridgeActorBinding::default()` fallthrough.
    #[test]
    fn audit_request_resolves_to_reviewer_binding() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "worker": {"provider": "codex", "model": "global-worker"},
                "reviewer": {"provider": "claude", "model": "global-reviewer"},
                "workflow": {
                    "phase_overrides": {
                        "proof_complete_style_cleanup": {
                            "reviewer": {"provider": "claude", "model": "cleanup-reviewer-model"}
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("write config");

        // Phase::Cleanup is where audit requests fire; verify both the
        // global-level reuse (TheoremStating phase, no override) and the
        // phase-override path (Cleanup phase, override wins) so we know
        // the routing matches Review for both behaviors.
        let mut audit_request_global = sample_request(RequestKind::Audit);
        audit_request_global.phase = Phase::TheoremStating;
        let audit_bindings_global =
            resolve_request_actor_bindings(&config_path, &audit_request_global)
                .expect("audit bindings (global)");

        let mut review_request_global = sample_request(RequestKind::Review);
        review_request_global.phase = Phase::TheoremStating;
        let review_bindings_global =
            resolve_request_actor_bindings(&config_path, &review_request_global)
                .expect("review bindings (global)");

        // Audit must match Review at the same phase — full binding equality,
        // not just provider/model strings. This is the load-bearing
        // assertion that `RequestKind::Audit` is routed through the same
        // resolver branch as Review.
        assert_eq!(
            audit_bindings_global.reviewer_binding, review_bindings_global.reviewer_binding,
            "audit must resolve to the same reviewer binding as Review at the same phase"
        );
        assert_eq!(audit_bindings_global.reviewer_binding.provider, "claude");
        assert_eq!(
            audit_bindings_global.reviewer_binding.model.as_deref(),
            Some("global-reviewer")
        );
        // Audit must NOT populate worker_binding (it has no worker dispatch).
        assert_eq!(
            audit_bindings_global.worker_binding,
            BridgeActorBinding::default(),
            "audit must not populate worker_binding"
        );

        // Phase::Cleanup override: audit should pick up the cleanup phase's
        // reviewer override, matching what Review would get.
        let mut audit_request_cleanup = sample_request(RequestKind::Audit);
        audit_request_cleanup.phase = Phase::Cleanup;
        let audit_bindings_cleanup =
            resolve_request_actor_bindings(&config_path, &audit_request_cleanup)
                .expect("audit bindings (cleanup)");

        let mut review_request_cleanup = sample_request(RequestKind::Review);
        review_request_cleanup.phase = Phase::Cleanup;
        let review_bindings_cleanup =
            resolve_request_actor_bindings(&config_path, &review_request_cleanup)
                .expect("review bindings (cleanup)");

        assert_eq!(
            audit_bindings_cleanup.reviewer_binding, review_bindings_cleanup.reviewer_binding,
            "audit must resolve to the same reviewer binding as Review in Phase::Cleanup"
        );
        assert_eq!(
            audit_bindings_cleanup.reviewer_binding.model.as_deref(),
            Some("cleanup-reviewer-model")
        );
    }

    // K-2 regression: single-agent-per-panel config produces single-lane
    // verification (one API call per check) rather than the legacy 2-lane
    // hardcoded default. Verifies `resolve_verifier_lane_count` reports 1
    // when the config has 1 corr + 1 sound agent and the policy selects 1
    // each, matching the lane-count expected when single-judge
    // verification is desired.
    #[test]
    fn resolve_verifier_lane_count_returns_one_for_single_agent_panels() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "policy_path": "trellis.policy.json",
                "verification": {
                    "correspondence_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                    ],
                    "soundness_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "correspondence_agent_selectors": ["codex-xhigh"],
                    "soundness_agent_selectors": ["codex-xhigh"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        let count = resolve_verifier_lane_count(&config_path).expect("resolve lane count");
        assert_eq!(count, 1);
        let lanes = build_verifier_lanes(count);
        assert_eq!(lanes, BTreeSet::from(["v1".to_string()]));
    }

    // Backwards compat: 2-agent config (legacy connectivity_gnp template
    // shape) keeps producing 2 lanes so existing operator setups don't
    // change behavior.
    #[test]
    fn resolve_verifier_lane_count_returns_two_for_two_agent_panels() {
        let tmp = tempdir().expect("tempdir");
        let config_path = write_config(tmp.path(), "trellis.policy.json");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "correspondence_agent_selectors": ["claude-a", "gemini-b"],
                    "soundness_agent_selectors": ["claude-a", "gemini-b"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        let count = resolve_verifier_lane_count(&config_path).expect("resolve lane count");
        assert_eq!(count, 2);
        let lanes = build_verifier_lanes(count);
        assert_eq!(lanes, BTreeSet::from(["v1".to_string(), "v2".to_string()]));
    }

    // When selectors are empty the count comes from the configured-agent
    // pool size (selector fallback semantics — `resolve_selected_agents`
    // returns the full catalog).
    #[test]
    fn resolve_verifier_lane_count_uses_pool_size_when_selectors_empty() {
        let tmp = tempdir().expect("tempdir");
        let config_path = write_config(tmp.path(), "trellis.policy.json");
        // Policy intentionally absent — selector fallback must use full pool.
        let count = resolve_verifier_lane_count(&config_path).expect("resolve lane count");
        assert_eq!(count, 2);
    }

    // Substantiveness pool with more agents than corr/sound widens the lane
    // count; without a dedicated subst pool the falls-back-to-corr lane
    // count is correct.
    #[test]
    fn resolve_verifier_lane_count_includes_dedicated_substantiveness_pool() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "policy_path": "trellis.policy.json",
                "verification": {
                    "correspondence_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-a"}
                    ],
                    "soundness_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-a"}
                    ],
                    "substantiveness_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-a"},
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-b"},
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-c"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "correspondence_agent_selectors": ["codex-a"],
                    "soundness_agent_selectors": ["codex-a"],
                    "substantiveness_agent_selectors": ["codex-a", "codex-b", "codex-c"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        let count = resolve_verifier_lane_count(&config_path).expect("resolve lane count");
        assert_eq!(count, 3);
    }

    // Floor-at-1: even a config with no verification block still produces a
    // legal `verifier_lanes` (cannot be empty per `validate()`).
    #[test]
    fn resolve_verifier_lane_count_floors_at_one_for_empty_config() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(&config_path, "{}").expect("write empty config");
        let count = resolve_verifier_lane_count(&config_path).expect("resolve lane count");
        assert_eq!(count, 1);
        let lanes = build_verifier_lanes(count);
        assert_eq!(lanes, BTreeSet::from(["v1".to_string()]));
    }

    // K-2 regression integration: with the lane count reduced to 1, the
    // `lane_bindings` machinery does NOT emit the
    // "not enough configured X agents for requested lanes" error that
    // can arise when a single-agent panel requests one lane. Asserts the
    // single-lane request resolves to a single binding.
    #[test]
    fn single_lane_request_with_single_agent_config_resolves_cleanly() {
        let tmp = tempdir().expect("tempdir");
        let config_path = tmp.path().join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "policy_path": "trellis.policy.json",
                "verification": {
                    "correspondence_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                    ],
                    "soundness_agents": [
                        {"provider": "codex", "model": "gpt-5.5", "label": "codex-xhigh"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        fs::write(
            tmp.path().join("trellis.policy.json"),
            serde_json::json!({
                "verification": {
                    "correspondence_agent_selectors": ["codex-xhigh"],
                    "soundness_agent_selectors": ["codex-xhigh"]
                }
            })
            .to_string(),
        )
        .expect("write policy");

        // Build a Corr request carrying only the v1 lane (matching what the
        // K-2 fix produces from `resolve_verifier_lane_count` -> `build_verifier_lanes`).
        let mut request = sample_request(RequestKind::Corr);
        request.verify_lanes = build_verifier_lanes(1);

        let output = resolve_request_verifier_bindings(&config_path, &request)
            .expect("single-lane corr bindings should resolve cleanly");
        assert_eq!(output.corr_verify_lane_bindings.len(), 1);
        assert_eq!(output.corr_verify_lane_bindings[0].lane_id, "v1");
        assert_eq!(output.corr_verify_lane_bindings[0].provider, "codex");
        assert_eq!(output.corr_verify_lane_bindings[0].label, "codex-xhigh");

        // And a Sound request mirrors the behavior — the same single-agent
        // config supports the single-lane sound request without error.
        let mut sound_request = sample_request(RequestKind::Sound);
        sound_request.verify_lanes = build_verifier_lanes(1);
        let sound_output = resolve_request_verifier_bindings(&config_path, &sound_request)
            .expect("single-lane sound bindings should resolve cleanly");
        assert_eq!(sound_output.sound_verify_lane_bindings.len(), 1);
        assert_eq!(sound_output.sound_verify_lane_bindings[0].lane_id, "v1");
    }
}
