//! Small fail-closed operator surface for trust-v1 bootstrap and seed
//! construction (Q1, plan doc 32 Stage 3: the journal-commit subcommands and
//! the ed25519 keyset machinery died with the journal; gate decisions flow
//! through the same runtime channel math mode uses; Stage 4 reshapes the
//! launch flow).

use serde_json::Value;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use trellis_kernel::trust_base::{
    build_campaign_seed, build_evidence_manifest, canonical_json_value, claim_rows_from_state,
    execute_phase0_checked_json, parse_json_strict, raw_sha256, replay_phase0_adaptation, source_tree_digest,
    source_tree_manifest_value, tagged_hash,
    validate_aeneas_model_refinement,
    validate_seed_manifest_semantics, verify_evidence_tool_manifest, AuthoritativeRecord,
    CampaignSeedRequest, CampaignTargetInput, CampaignTrustPlan, ClaimContext, DomainTag,
    EvidenceInput, ExecutionLimits, Phase0AdaptationLedger, Phase0ExecutionRequest,
    PinnedExecutionRequest, SchemaRegistry, Sha256Digest,
};
use trellis_kernel::{
    ChallengeResolution, Phase0Event, Phase0Genesis, Phase0ProductionResult,
    Phase0SealedGeneration, Phase0State, Phase0ValidationContext, ProtocolState,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("trust-v1 error: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    run_with_args(&args)
}

fn run_with_args(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("phase0-init") if args.len() == 6 => {
            let genesis_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 genesis")?;
            let ledger_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 ledger")?;
            let context_path = canonical_regular_file(Path::new(&args[4]), "Phase-0 validation context")?;
            let genesis = Phase0Genesis::parse_and_validate(
                &fs::read(genesis_path).map_err(io("read Phase-0 genesis"))?,
            ).map_err(|error| error.to_string())?;
            let context = Phase0ValidationContext::parse_and_validate(
                &fs::read(context_path).map_err(io("read Phase-0 validation context"))?,
            ).map_err(|error| error.to_string())?;
            let ledger_bytes = fs::read(ledger_path).map_err(io("read Phase-0 ledger"))?;
            let ledger = Phase0AdaptationLedger::parse_and_validate(
                &ledger_bytes,
                &context.goal_target_set().map_err(|error| error.to_string())?,
            ).map_err(|error| error.to_string())?;
            let state = Phase0State::initialize(genesis, ledger, &context)
                .map_err(|error| error.to_string())?;
            write_new(Path::new(&args[5]), &state.canonical_bytes().map_err(|error| error.to_string())?)?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-command-result/v1",
                "status": "initialized",
                "stage": state.stage,
                "generation": state.generation,
                "state_sha256": state.state_sha256,
            }))
        }
        Some("phase0-check") if args.len() == 4 => {
            let state_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 state")?;
            let context_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 validation context")?;
            let context = Phase0ValidationContext::parse_and_validate(
                &fs::read(context_path).map_err(io("read Phase-0 validation context"))?,
            ).map_err(|error| error.to_string())?;
            let state = Phase0State::parse_and_validate(
                &fs::read(state_path).map_err(io("read Phase-0 state"))?,
                &context,
            ).map_err(|error| error.to_string())?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-command-result/v1",
                "status": "verified",
                "stage": state.stage,
                "generation": state.generation,
                "phase0_handbacks": state.phase0_handbacks,
                "state_sha256": state.state_sha256,
            }))
        }
        Some("phase0-next") if args.len() == 4 => {
            let state_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 state")?;
            let context_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 validation context")?;
            let context = Phase0ValidationContext::parse_and_validate(
                &fs::read(context_path).map_err(io("read Phase-0 validation context"))?,
            ).map_err(|error| error.to_string())?;
            let state = Phase0State::parse_and_validate(
                &fs::read(state_path).map_err(io("read Phase-0 state"))?,
                &context,
            ).map_err(|error| error.to_string())?;
            let request = state.next_request(&context).map_err(|error| error.to_string())?;
            print_json(&serde_json::to_value(request).map_err(|error| error.to_string())?)
        }
        Some("phase0-apply") if args.len() == 6 => {
            let state_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 state")?;
            let context_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 validation context")?;
            let event_path = canonical_regular_file(Path::new(&args[4]), "Phase-0 event")?;
            let context = Phase0ValidationContext::parse_and_validate(
                &fs::read(context_path).map_err(io("read Phase-0 validation context"))?,
            ).map_err(|error| error.to_string())?;
            // `apply` validates the pre-state itself: against the pre-burst
            // snapshot for a worker revision (the live candidate already
            // holds the delta), against the live candidate otherwise.
            let state = Phase0State::parse_for_apply(
                &fs::read(state_path).map_err(io("read Phase-0 state"))?,
            ).map_err(|error| error.to_string())?;
            let event = Phase0Event::parse_canonical(
                &fs::read(event_path).map_err(io("read Phase-0 event"))?,
            ).map_err(|error| error.to_string())?;
            let state = state.apply(&context, event).map_err(|error| error.to_string())?;
            write_new(Path::new(&args[5]), &state.canonical_bytes().map_err(|error| error.to_string())?)?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-command-result/v1",
                "status": "applied",
                "stage": state.stage,
                "generation": state.generation,
                "phase0_handbacks": state.phase0_handbacks,
                "state_sha256": state.state_sha256,
            }))
        }
        Some("phase0-finalize") if args.len() == 6 => {
            let state_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 state")?;
            let context_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 validation context")?;
            let context = Phase0ValidationContext::parse_and_validate(
                &fs::read(context_path).map_err(io("read Phase-0 validation context"))?,
            ).map_err(|error| error.to_string())?;
            let state = Phase0State::parse_and_validate(
                &fs::read(state_path).map_err(io("read Phase-0 state"))?,
                &context,
            ).map_err(|error| error.to_string())?;
            let (state, sealed) = state.finalize(&context).map_err(|error| error.to_string())?;
            write_new(
                Path::new(&args[4]),
                &canonical_json_value(&serde_json::to_value(&sealed).map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?,
            )?;
            write_new(Path::new(&args[5]), &state.canonical_bytes().map_err(|error| error.to_string())?)?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-command-result/v1",
                "status": "sealed",
                "generation": state.generation,
                "sealed_generation_sha256": sealed.sealed_generation_sha256,
                "state_sha256": state.state_sha256,
            }))
        }
        Some("execute-phase0-checker") if args.len() == 6 => {
            let request_path = canonical_regular_file(Path::new(&args[2]), "Phase-0 checker request")?;
            let runner = canonical_regular_file(Path::new(&args[3]), "Phase-0 checker runner")?;
            let working = canonical_non_symlink_directory(Path::new(&args[4]), "Phase-0 checker working directory")?;
            let receipt_out = PathBuf::from(&args[5]);
            let request_bytes = fs::read(&request_path).map_err(io("read Phase-0 checker request"))?;
            let request = parse_json_strict(&request_bytes).map_err(|error| error.to_string())?;
            if canonical_json_value(&request).map_err(|error| error.to_string())? != request_bytes {
                return Err("Phase-0 checker request must be exact canonical JSON".into());
            }
            let genesis: Sha256Digest = required_string(
                &request, "phase0_genesis_sha256", "Phase-0 checker request",
            )?.parse().map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
            let expected_runner: Sha256Digest = required_string(
                &request, "runner_sha256", "Phase-0 checker request",
            )?.parse().map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
            let repository = canonical_non_symlink_directory(
                Path::new(required_string(
                    &request, "repository_root", "Phase-0 checker request",
                )?),
                "Phase-0 pinned repository root",
            )?;
            if !runner.starts_with(&repository) {
                return Err("Phase-0 checker runner lies outside the pinned repository root".into());
            }
            if raw_sha256(&fs::read(&runner).map_err(io("read Phase-0 checker runner"))?) != expected_runner {
                return Err("Phase-0 checker runner differs from request identity".into());
            }
            let limits = request.get("limits").and_then(Value::as_object)
                .ok_or_else(|| "Phase-0 checker request lacks limits".to_owned())?;
            let natural = |field: &str| -> Result<u64, String> {
                limits.get(field).and_then(Value::as_u64).filter(|value| *value > 0)
                    .ok_or_else(|| format!("Phase-0 checker limit {field} must be positive"))
            };
            let timeout = natural("timeout_seconds")?.checked_add(30)
                .ok_or_else(|| "Phase-0 checker timeout is too large".to_owned())?;
            natural("stdout_bytes")?;
            natural("stderr_bytes")?;
            let repository_utf8 = repository
                .to_str()
                .ok_or_else(|| "Phase-0 pinned repository root must be UTF-8".to_owned())?;
            let environment = BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned()),
                ("PYTHONPATH".to_owned(), repository_utf8.to_owned()),
            ]);
            let receipt = execute_phase0_checked_json(Phase0ExecutionRequest {
                phase0_genesis_sha256: genesis,
                purpose: "source_adaptation_checker",
                execution: PinnedExecutionRequest {
                    runner_path: &runner,
                    expected_runner_sha256: expected_runner,
                    command_id: "phase0-source-adaptation-checker",
                    working_directory: &working,
                    environment: &environment,
                    input: &request,
                    limits: ExecutionLimits {
                        timeout: Duration::from_secs(timeout),
                        max_stdin_bytes: 4 * 1024 * 1024,
                        // The checker output contains base64 copies of every
                        // bounded stage stream, so its broker capture is a
                        // separate fixed envelope rather than one stage's
                        // request limit.
                        max_stdout_bytes: 128 * 1024 * 1024,
                        max_stderr_bytes: 1024 * 1024,
                    },
                },
            }).map_err(|error| error.to_string())?;
            let receipt_bytes = canonical_json_value(receipt.value()).map_err(|error| error.to_string())?;
            write_new(&receipt_out, &receipt_bytes)?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-checker-execution-result/v1",
                "receipt_path": receipt_out,
                "receipt_sha256": receipt.digest(),
                "checker_output": receipt.parsed_output(),
            }))
        }
        Some("source-tree-manifest") if args.len() == 3 => {
            let root = canonical_non_symlink_directory(Path::new(&args[2]), "source tree")?;
            let value = source_tree_manifest_value(&root).map_err(|error| error.to_string())?;
            print_json(&value)
        }
        Some("replay-phase0-adaptation") if args.len() == 7 => {
            let unadapted = canonical_non_symlink_directory(
                Path::new(&args[2]),
                "unadapted source tree",
            )?;
            let ledger_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 ledger")?;
            let targets_path = canonical_regular_file(Path::new(&args[4]), "GOAL target set")?;
            let adapted = canonical_non_symlink_directory(
                Path::new(&args[5]),
                "adapted source tree",
            )?;
            let output_arg = Path::new(&args[6]);
            let output_name = output_arg
                .file_name()
                .ok_or_else(|| "replay output needs one final path component".to_owned())?;
            if output_name.to_str().is_none()
                || output_arg
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err("replay output path must be normalized UTF-8".into());
            }
            let output_parent = canonical_non_symlink_directory(
                output_arg.parent().unwrap_or_else(|| Path::new(".")),
                "replay output parent",
            )?;
            let output = output_parent.join(output_name);
            if output.starts_with(&unadapted) || output.starts_with(&adapted) {
                return Err("replay output must be outside both supplied source trees".into());
            }

            let target_bytes = fs::read(&targets_path).map_err(io("read GOAL target set"))?;
            let target_value = parse_json_strict(&target_bytes).map_err(|error| error.to_string())?;
            if canonical_json_value(&target_value).map_err(|error| error.to_string())?
                != target_bytes
            {
                return Err("GOAL target set must be exact canonical JSON".into());
            }
            require_exact_json_keys(&target_value, &["schema", "targets"], "GOAL target set")?;
            if required_string(&target_value, "schema", "GOAL target set")?
                != "trellis-phase0-goal-target-set/v1"
            {
                return Err("GOAL target set has the wrong schema".into());
            }
            let target_rows = target_value
                .get("targets")
                .and_then(Value::as_array)
                .ok_or_else(|| "GOAL target set targets must be an array".to_owned())?;
            let mut targets = BTreeSet::new();
            let mut prior: Option<&str> = None;
            for row in target_rows {
                let target = row
                    .as_str()
                    .ok_or_else(|| "GOAL target identifiers must be strings".to_owned())?;
                if !target.starts_with("goal:")
                    || target.chars().any(char::is_control)
                    || prior.is_some_and(|value| value.as_bytes() >= target.as_bytes())
                {
                    return Err("GOAL targets must be nonempty unique sorted goal: identifiers".into());
                }
                targets.insert(target.to_owned());
                prior = Some(target);
            }
            let ledger_bytes = fs::read(&ledger_path).map_err(io("read Phase-0 ledger"))?;
            let ledger = Phase0AdaptationLedger::parse_and_validate(&ledger_bytes, &targets)
                .map_err(|error| error.to_string())?;
            let root = replay_phase0_adaptation(
                &unadapted,
                &ledger,
                &targets,
                &adapted,
                &output,
            )
            .map_err(|error| error.to_string())?;
            print_json(&serde_json::json!({
                "schema": "trellis-phase0-adaptation-replay-result/v1",
                "status": "verified",
                "ledger_sha256": ledger.ledger_sha256,
                "replayed_tree_sha256": root,
                "output_root": output,
            }))
        }
        Some("inspect-claim-surface") if args.len() == 3 => {
            let state_path = canonical_regular_file(Path::new(&args[2]), "protocol state")?;
            let value = parse_json_strict(
                &fs::read(&state_path).map_err(io("read protocol state"))?,
            )
            .map_err(|error| error.to_string())?;
            let state: ProtocolState = serde_json::from_value(value)
                .map_err(|error| format!("protocol state is invalid: {error}"))?;
            let rows = claim_rows_from_state(&state, ClaimContext::pre_gate())?;
            print_json(
                &serde_json::to_value(rows)
                    .map_err(|error| format!("serialize claim surface: {error}"))?,
            )
        }
        Some("verify-runtime-distribution") if args.len() == 7 => {
            verify_runtime_distribution(
                Path::new(&args[2]),
                Path::new(&args[3]),
                Path::new(&args[4]),
                Path::new(&args[5]),
                Path::new(&args[6]),
            )
        }
        // Q7 (plan doc 32 Stages 3/4): the campaign launcher's final step
        // verifies the runtime-root finalization archive's embedded
        // approval record — the successor of the retired
        // `verify-package` + sidecar flow.
        Some("verify-finalization-archive") if args.len() == 3 => {
            let archive_path = canonical_regular_file(
                Path::new(&args[2]),
                "trust finalization archive",
            )?;
            let bytes = fs::read(&archive_path).map_err(io("read finalization archive"))?;
            let embedded = trellis_kernel::trust_base::verify_trust_finalization_archive(&bytes)
                .map_err(|error| error.to_string())?;
            print_json(&serde_json::json!({
                "status": "verified",
                "archive_path": archive_path,
                "archive_sha256": raw_sha256(&bytes),
                "approval_record_sha256": embedded.approval_record_sha256,
                "presentation_sha256": embedded.presentation_sha256,
            }))
        }
        Some("verify-phase0-production") if args.len() == 6 => {
            let campaign_repo = fs::canonicalize(&args[2]).map_err(io("resolve campaign repo"))?;
            let genesis_path = canonical_regular_file(Path::new(&args[3]), "Phase-0 genesis")?;
            let sealed_path = canonical_regular_file(
                Path::new(&args[4]),
                "Phase-0 sealed generation",
            )?;
            let genesis = Phase0Genesis::parse_and_validate(
                &fs::read(genesis_path).map_err(io("read Phase-0 genesis"))?,
            )
            .map_err(|error| error.to_string())?;
            let sealed = Phase0SealedGeneration::parse_and_validate(
                &fs::read(sealed_path).map_err(io("read sealed Phase-0 generation"))?,
                &genesis,
            )
            .map_err(|error| error.to_string())?;
            let result = trellis_kernel::phase0::verify_phase0_production(
                &campaign_repo,
                &genesis,
                &sealed,
            )
            .map_err(|error| error.to_string())?;
            let bytes = canonical_json_value(
                &serde_json::to_value(&result)
                    .map_err(|error| format!("serialize Phase-0 production result: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            write_new(Path::new(&args[5]), &bytes)?;
            print_json(&serde_json::json!({
                "status": "verified",
                "sealed_generation_sha256": result.sealed_generation_sha256,
                "production_result_sha256": result.production_result_sha256,
                "adapted_source_tree_sha256": result.adapted_source_tree_sha256,
                "production_generated_lean_sha256": result.production_generated_lean_sha256,
                "production_tablet_root_sha256": result.production_tablet_root_sha256,
            }))
        }
        Some("bootstrap-campaign") if args.len() == 10 => {
            let campaign_repo = fs::canonicalize(&args[2]).map_err(io("resolve campaign repo"))?;
            let trust_plan_path = canonical_regular_file(
                Path::new(&args[3]),
                "campaign trust plan",
            )?;
            let trust_plan_bytes = fs::read(&trust_plan_path).map_err(io("read campaign trust plan"))?;
            let trust_plan_value =
                parse_json_strict(&trust_plan_bytes).map_err(|error| error.to_string())?;
            if canonical_json_value(&trust_plan_value).map_err(|error| error.to_string())?
                != trust_plan_bytes
            {
                return Err("campaign trust plan must be exact canonical JSON".into());
            }
            let trust_plan = CampaignTrustPlan::parse_and_validate(&trust_plan_bytes)
                .map_err(|error| error.to_string())?;
            let kernel_binary = canonical_regular_file(
                Path::new(&args[4]),
                "trust kernel",
            )?;
            let output_target = PathBuf::from(&args[5]);
            let run_id = &args[6];
            let seed_plan_id = &args[7];
            let phase0_genesis_path = canonical_regular_file(
                Path::new(&args[8]),
                "Phase-0 genesis",
            )?;
            let phase0_sealed_path = canonical_regular_file(
                Path::new(&args[9]),
                "Phase-0 sealed generation",
            )?;
            let phase0_genesis_bytes =
                fs::read(&phase0_genesis_path).map_err(io("read Phase-0 genesis"))?;
            let phase0_genesis = Phase0Genesis::parse_and_validate(&phase0_genesis_bytes)
                .map_err(|error| error.to_string())?;
            let phase0_sealed_bytes = fs::read(&phase0_sealed_path)
                .map_err(io("read sealed Phase-0 generation"))?;
            let phase0_sealed = Phase0SealedGeneration::parse_and_validate(
                &phase0_sealed_bytes,
                &phase0_genesis,
            )
            .map_err(|error| error.to_string())?;
            trellis_kernel::phase0::verify_phase0_trust_plan(&trust_plan, &phase0_sealed)
                .map_err(|error| error.to_string())?;
            if !campaign_repo.is_dir() {
                return Err("campaign repo must be a directory".into());
            }
            let phase0_production = trellis_kernel::phase0::verify_phase0_production(
                &campaign_repo,
                &phase0_genesis,
                &phase0_sealed,
            )
            .map_err(|error| error.to_string())?;
            let phase0_production_path = campaign_repo
                .join(".trellis/phase0/PRODUCTION_RESULT.json");
            let recorded_phase0_production = Phase0ProductionResult::parse_and_validate(
                &fs::read(&phase0_production_path)
                    .map_err(io("read Phase-0 production result"))?,
            )
            .map_err(|error| error.to_string())?;
            if recorded_phase0_production != phase0_production {
                return Err(
                    "recorded Phase-0 production result differs from fresh independent verification"
                        .into(),
                );
            }
            regenerate_phase0_goal_binding_report(
                &campaign_repo,
                &phase0_genesis,
                &phase0_sealed,
            )?;
            let output_parent = fs::canonicalize(
                containing_directory(&output_target),
            )
            .map_err(io("resolve bootstrap output parent"))?;
            if output_parent.starts_with(&campaign_repo) {
                return Err(
                    "bootstrap authority output must be outside the rewindable campaign repo"
                        .into(),
                );
            }
            let staged_output = StagedDirectory::create(&output_target)?;
            let output = staged_output.path().to_path_buf();
            let evidence_root = output.join("evidence-root");
            fs::create_dir(&evidence_root).map_err(io("create evidence root"))?;
            fs::create_dir(evidence_root.join("bin")).map_err(io("create evidence bin"))?;
            fs::create_dir_all(evidence_root.join("source/crate"))
                .map_err(io("create evidence source tree"))?;
            fs::create_dir(evidence_root.join("campaign"))
                .map_err(io("create campaign evidence directory"))?;
            fs::create_dir(evidence_root.join("toolchain"))
                .map_err(io("create toolchain evidence directory"))?;
            copy_regular_file(
                &kernel_binary,
                &evidence_root.join("bin/trellis-trust-kernel"),
                true,
            )?;
            let campaign_config_path = campaign_repo.join("trellis.config.json");
            copy_regular_file(
                &campaign_config_path,
                &evidence_root.join("campaign/trellis.config.json"),
                false,
            )?;
            write_new(
                &evidence_root.join("campaign/TRUST_PLAN.json"),
                &trust_plan_bytes,
            )?;
            let crate_root = campaign_repo.join("crate");
            let source_tree_sha256 = source_tree_digest(&crate_root)
                .map_err(|error| error.to_string())?;
            let mut evidence_inputs = vec![
                EvidenceInput {
                    kind: "tool".into(),
                    logical_id: "trellis-trust-kernel".into(),
                    relative_path: "bin/trellis-trust-kernel".into(),
                    dependency_ids: Vec::new(),
                },
                EvidenceInput {
                    kind: "configuration".into(),
                    logical_id: "campaign-config".into(),
                    relative_path: "campaign/trellis.config.json".into(),
                    dependency_ids: Vec::new(),
                },
                EvidenceInput {
                    kind: "configuration".into(),
                    logical_id: "campaign-trust-plan".into(),
                    relative_path: "campaign/TRUST_PLAN.json".into(),
                    dependency_ids: vec!["campaign-config".into()],
                },
            ];
            copy_campaign_toolchain_closure(
                &campaign_repo,
                &evidence_root,
                &mut evidence_inputs,
            )?;
            fs::create_dir(evidence_root.join("model"))
                .map_err(io("create model evidence directory"))?;
            for (source_relative, evidence_name, logical_id) in [
                ("Tablet/Preamble.lean", "Preamble.lean", "aeneas-generated-preamble"),
                ("Tablet/Assumptions.lean", "Assumptions.lean", "aeneas-validity-definitions"),
                ("tcb_manifest.json", "tcb_manifest.json", "aeneas-tcb-manifest"),
            ] {
                copy_regular_file(
                    &campaign_repo.join(source_relative),
                    &evidence_root.join("model").join(evidence_name),
                    false,
                )?;
                evidence_inputs.push(EvidenceInput {
                    kind: "model_refinement_input".into(),
                    logical_id: logical_id.into(),
                    relative_path: format!("model/{evidence_name}"),
                    dependency_ids: vec![
                        "campaign-config".into(),
                        "extraction-determinism".into(),
                    ],
                });
            }
            for (relative, bytes) in collect_regular_files(&crate_root)? {
                let relative_utf8 = slash_path(&relative)?;
                let destination = evidence_root.join("source/crate").join(&relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(io("create evidence source parent"))?;
                }
                write_new(&destination, &bytes)?;
                evidence_inputs.push(EvidenceInput {
                    kind: "source".into(),
                    logical_id: format!("campaign-source:{relative_utf8}"),
                    relative_path: format!("source/crate/{relative_utf8}"),
                    dependency_ids: Vec::new(),
                });
            }
            if source_tree_digest(&evidence_root.join("source/crate"))
                .map_err(|error| error.to_string())?
                != source_tree_sha256
            {
                return Err("copied campaign source tree changed identity".into());
            }
            let extraction_tool_source_checkouts = copy_and_validate_extraction_evidence(
                &campaign_repo,
                &evidence_root,
                &mut evidence_inputs,
            )?;
            capture_trusted_platform_closure(
                &campaign_repo,
                &extraction_tool_source_checkouts,
                &evidence_root,
                &mut evidence_inputs,
            )?;
            copy_phase0_evidence(
                &campaign_repo,
                &evidence_root,
                &phase0_genesis_bytes,
                &phase0_sealed_bytes,
                &phase0_sealed,
                &phase0_production,
                &mut evidence_inputs,
            )?;
            let (evidence_manifest, evidence_manifest_bytes) =
                build_evidence_manifest(&evidence_root, &evidence_inputs)
                    .map_err(|error| error.to_string())?;
            let verified_evidence = verify_evidence_tool_manifest(
                &evidence_root,
                &evidence_manifest_bytes,
            )
            .map_err(|error| error.to_string())?;
            let toolchain_bytes = fs::read(evidence_root.join("toolchain/lean-toolchain"))
                .map_err(io("read pinned Lean toolchain"))?;
            verified_evidence
                .resolve_leaf_by_digest(
                    &evidence_root,
                    trellis_kernel::trust_base::raw_sha256(&toolchain_bytes),
                )
                .map_err(|error| {
                    format!(
                        "Lean toolchain is not uniquely resolvable in the approved evidence closure: {error}"
                    )
                })?;
            let evidence_manifest_path = output.join("EVIDENCE_TOOL_MANIFEST.json");
            write_new(&evidence_manifest_path, &evidence_manifest_bytes)?;

            let config_bytes = fs::read(&campaign_config_path).map_err(io("read campaign config"))?;
            let mut config = parse_json_strict(&config_bytes).map_err(|error| error.to_string())?;
            // GOAL.md, projected by setup into the config, is the sole
            // resolution authority. Bootstrap requires byte-level policy
            // agreement with the separately generated plan.
            let tablet_root = canonical_non_symlink_directory(
                &campaign_repo.join("Tablet"),
                "campaign Tablet root",
            )?;
            let model_refinement = config
                .get("pv_tablet")
                .and_then(|value| value.get("model_refinement"))
                .ok_or_else(|| {
                    "campaign config lacks pv_tablet.model_refinement".to_owned()
                })?;
            let tcb_manifest_bytes = fs::read(campaign_repo.join("tcb_manifest.json"))
                .map_err(io("read generated TCB manifest"))?;
            let tcb_manifest = parse_json_strict(&tcb_manifest_bytes)
                .map_err(|error| error.to_string())?;
            let approved_axioms_bytes = fs::read(campaign_repo.join("APPROVED_AXIOMS.json"))
                .map_err(io("read generated approved-axioms allowlist"))?;
            let approved_axioms_manifest = parse_json_strict(&approved_axioms_bytes)
                .map_err(|error| error.to_string())?;
            let mut targets = Vec::new();
            {
                let raw_targets = config
                    .get("pv_tablet")
                    .and_then(|value| value.get("verification_targets"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        "prose campaign config lacks pv_tablet.verification_targets".to_owned()
                    })?
                    .clone();
                let mut configured_target_ids = BTreeSet::new();
                let mut configured_targets = Vec::new();
                for item in &raw_targets {
                    let (name, resolution) = if let Some(name) = item.as_str() {
                        (name.trim(), ChallengeResolution::Decide)
                    } else if let Some(object) = item.as_object() {
                        let name = object
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .ok_or_else(|| {
                                "pv_tablet.verification_targets object entry lacks a \
                                 non-empty `name`"
                                    .to_owned()
                            })?;
                        let resolution = match object
                            .get("resolution")
                            .and_then(Value::as_str)
                            .unwrap_or("decide")
                        {
                            "decide" => ChallengeResolution::Decide,
                            "prove" => ChallengeResolution::Prove,
                            other => {
                                return Err(format!(
                                    "pv_tablet.verification_targets entry `{name}` has \
                                     resolution {other:?}; expected \"prove\" or \"decide\""
                                ));
                            }
                        };
                        (name, resolution)
                    } else {
                        return Err(
                            "pv_tablet.verification_targets entries must be strings or \
                             {\"name\", \"resolution\"} objects"
                                .to_owned(),
                        );
                    };
                    validate_single_path_component(name, "prose GOAL target")?;
                    let target_id = format!("goal:{name}");
                    if !configured_target_ids.insert(target_id.clone()) {
                        return Err(format!(
                            "pv_tablet.verification_targets repeats prose GOAL target `{name}`"
                        ));
                    }
                    configured_targets.push((name.to_owned(), target_id, resolution));
                }
                let planned_target_ids: BTreeSet<_> = trust_plan
                    .targets
                    .iter()
                    .map(|target| target.target_id.as_str())
                    .collect();
                let configured_refs: BTreeSet<&str> =
                    configured_target_ids.iter().map(String::as_str).collect();
                if planned_target_ids.len() != trust_plan.targets.len()
                    || configured_refs != planned_target_ids
                {
                    return Err(
                        "campaign trust plan target IDs must exactly cover configured targets"
                            .into(),
                    );
                }
                for (function, target_id, resolution) in &configured_targets {
                    let target_plan = trust_plan
                        .targets
                        .iter()
                        .find(|item| &item.target_id == target_id)
                        .expect("exactness checked above");
                    if target_plan.resolution != *resolution {
                        return Err(format!(
                            "resolution agreement failed for {target_id}: config has {:?}, \
                             plan has {:?}",
                            resolution, target_plan.resolution
                        ));
                    }
                    let model_refinement_sha256 = validate_aeneas_model_refinement(
                        model_refinement,
                        function,
                        &tablet_root,
                        &tcb_manifest,
                        &approved_axioms_manifest,
                    )
                    .map_err(|error| error.to_string())?;
                    targets.push(CampaignTargetInput {
                        target_id: target_id.clone(),
                        resolution: *resolution,
                        node_id: function.to_owned(),
                        lean_declaration: String::new(),
                        informal: format!(
                            "prose GOAL {resolution:?} target: the statement is worker-authored \
                             mid-run and bound at acceptance"
                        ),
                        model_refinement_sha256,
                    });
                }
            }
            let evidence_root_digest: Sha256Digest = required_string(
                &evidence_manifest,
                "evidence_tool_input_root",
                "evidence manifest",
            )?
            .parse()
            .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
            let trusted_platform_boundary_bytes = fs::read(
                evidence_root.join("trusted-platform/BOUNDARY.json"),
            )
            .map_err(io("read trusted-platform boundary"))?;
            let trusted_platform_boundary = parse_json_strict(
                &trusted_platform_boundary_bytes,
            )
            .map_err(|error| error.to_string())?;
            if canonical_json_value(&trusted_platform_boundary)
                .map_err(|error| error.to_string())?
                != trusted_platform_boundary_bytes
            {
                return Err("trusted-platform boundary must be exact canonical JSON".into());
            }
            let extraction_result_bytes = fs::read(evidence_root.join("extraction/RESULT.json"))
                .map_err(io("read extraction result"))?;
            let extraction_result = parse_json_strict(&extraction_result_bytes)
                .map_err(|error| error.to_string())?;
            let extraction_determinism_bytes = fs::read(
                evidence_root.join("extraction/DETERMINISM.json"),
            )
            .map_err(io("read extraction determinism receipt"))?;
            let extraction_determinism = parse_json_strict(&extraction_determinism_bytes)
                .map_err(|error| error.to_string())?;
            // W9: a prose campaign's GOAL referent (digest + bytes) becomes
            // a member of the authoritative seed record, so the launch
            // acknowledgment covers the prose exactly.
            let goal_file = config
                .get("pv_tablet")
                .and_then(|value| value.get("goal_file"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("GOAL.md")
                .to_owned();
            let pv_goal_prose = fs::read_to_string(campaign_repo.join(&goal_file))
                .map_err(io("read prose GOAL referent"))?;
            let phase0_roots = phase0_production.trust_roots();
            let seed = build_campaign_seed(CampaignSeedRequest {
                run_id,
                seed_plan_id,
                evidence: &verified_evidence,
                extraction_result: &extraction_result,
                extraction_determinism: &extraction_determinism,
                trusted_platform_boundary: &trusted_platform_boundary,
                source_tree_sha256,
                targets: &targets,
                trust_plan: &trust_plan,
                phase0: &phase0_roots,
                pv_goal_prose_utf8: Some(&pv_goal_prose),
            })
            .map_err(|error| error.to_string())?;
            let seed_manifest_path = output.join("SEED_MANIFEST.json");
            let seed_bundle_path = output.join("SEED_DEFINITION_BUNDLE.json");
            // Stage 4 (plan doc 32): the bootstrap render is the launch
            // acknowledgment (seed sight, no verdict); the
            // ADVANCE_GATE_PRESENTATION.bin name is reserved for the
            // Stage-10 live instrument.
            let acknowledgment_path = output.join("LAUNCH_ACKNOWLEDGMENT.bin");
            write_new(&seed_manifest_path, &seed.seed_manifest_bytes)?;
            write_new(&seed_bundle_path, &seed.seed_definition_bundle_bytes)?;
            write_new(&acknowledgment_path, &seed.launch_acknowledgment_bytes)?;
            config["trust_base"] = serde_json::json!({
                "protocol": "trellis-trust-v1",
                "seed_manifest_path": "SEED_MANIFEST.json",
                "seed_definition_bundle_path": "SEED_DEFINITION_BUNDLE.json",
                "evidence_tool_manifest_path": "EVIDENCE_TOOL_MANIFEST.json",
                "evidence_tool_root_path": "evidence-root",
                "launch_acknowledgment_path": "LAUNCH_ACKNOWLEDGMENT.bin",
                "seed_transaction_id": format!("seed:{run_id}"),
                "advance_gate_episode_id": format!("advance:{run_id}"),
            });
            let config_name = "trellis.config.trust-v1.json";
            let config_out = output.join(config_name);
            write_new(&config_out, &stable_runtime_config_bytes(&config)?)?;
            let output = staged_output.publish()?;
            let config_out = output.join(config_name);
            print_json(&serde_json::json!({
                "status": "bootstrapped",
                "config_path": config_out,
                "seed_manifest_sha256": seed.seed_manifest.digest(),
                "seed_definition_bundle_sha256": seed.seed_definition_bundle["bundle_sha256"],
                "authored_semantic_root": seed.seed_manifest.value()["authored_semantic_root"],
                "evidence_tool_input_root": evidence_root_digest,
                "launch_acknowledgment_sha256": tagged_hash(DomainTag::GatePresentation, &seed.launch_acknowledgment_bytes),
                "source_tree_sha256": source_tree_sha256,
                "target_count": targets.len(),
            }))
        }
        _ => Err(usage()),
    }
}

fn usage() -> String {
    "usage:\n  trellis_trust_cli phase0-init <genesis.json> <ledger.json> <context.json> <state-out.json>\n  trellis_trust_cli phase0-check <state.json> <context.json>\n  trellis_trust_cli phase0-next <state.json> <context.json>\n  trellis_trust_cli phase0-apply <state.json> <context.json> <event.json> <state-out.json>\n  trellis_trust_cli phase0-finalize <state.json> <context.json> <sealed-out.json> <state-out.json>\n  trellis_trust_cli execute-phase0-checker <request.json> <runner> <working-dir> <receipt-out.json>\n  trellis_trust_cli source-tree-manifest <source-root>\n  trellis_trust_cli replay-phase0-adaptation <unadapted-root> <ledger.json> <goal-targets.json> <adapted-root> <fresh-output-root>\n  trellis_trust_cli inspect-claim-surface <protocol-state.json>\n  trellis_trust_cli verify-runtime-distribution <seed-manifest.json> <evidence-manifest.json> <evidence-root-dir> <live-trellis-root> <runtime-cli-bin>\n  trellis_trust_cli verify-phase0-production <campaign-repo> <genesis.json> <sealed-generation.json> <production-result-out.json>\n  trellis_trust_cli bootstrap-campaign <campaign-repo> <trust-plan.json> <trust-kernel-bin> <trust-root-out> <run-id> <seed-plan-id> <genesis.json> <sealed-generation.json>\n  trellis_trust_cli verify-finalization-archive <trust-package-archive>".into()
}

fn regenerate_phase0_goal_binding_report(
    campaign_repo: &Path,
    genesis: &Phase0Genesis,
    sealed: &Phase0SealedGeneration,
) -> Result<(), String> {
    let reporter = canonical_regular_file(
        Path::new(&genesis.components.goal_binding_reporter.path),
        "Phase-0 GOAL-binding reporter",
    )?;
    if raw_sha256(&fs::read(&reporter).map_err(io("read GOAL-binding reporter"))?)
        != genesis.components.goal_binding_reporter.sha256
    {
        return Err("Phase-0 GOAL-binding reporter differs from genesis".into());
    }
    let output = Command::new("python3")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONPATH", &genesis.components.repository_root)
        .arg(&reporter)
        .arg("--goal").arg(campaign_repo.join("GOAL.md"))
        .arg("--pv-tablet").arg(campaign_repo.join("pv_tablet.json"))
        .arg("--adapted-tree").arg(campaign_repo.join("crate"))
        .arg("--unadapted-tree").arg(campaign_repo.join("unadapted-crate"))
        .arg("--ledger").arg(campaign_repo.join(".trellis/phase0/ADAPTATION_LEDGER.json"))
        .arg("--partition-manifest").arg(campaign_repo.join(".trellis/phase0/SOURCE_PARTITION_MANIFEST.json"))
        .arg("--require-complete")
        .output()
        .map_err(|error| format!("cannot run pinned GOAL-binding reporter: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pinned GOAL-binding reporter failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let expected = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.goal_binding_report)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    require_exact_phase0_goal_binding_report(&output.stdout, &expected)
}

fn require_exact_phase0_goal_binding_report(
    actual: &[u8],
    expected: &[u8],
) -> Result<(), String> {
    if actual != expected {
        return Err("production pv_tablet GOAL-binding report differs byte-for-byte from Phase 0"
            .into());
    }
    Ok(())
}

fn phase0_source_snapshot(root: &Path) -> Result<Vec<u8>, String> {
    let manifest = trellis_kernel::trust_base::source_tree_manifest(root)
        .map_err(|error| error.to_string())?;
    let files = collect_regular_files(root)?;
    let rows: Vec<Value> = files
        .into_iter()
        .map(|(relative, bytes)| {
            Ok(serde_json::json!({
                "relative_path": slash_path(&relative)?,
                "byte_length": bytes.len(),
                "sha256_of_raw_bytes": raw_sha256(&bytes),
                "bytes_base64": BASE64_STANDARD.encode(bytes),
            }))
        })
        .collect::<Result<_, String>>()?;
    let mut snapshot = serde_json::json!({
        "schema": "trellis-phase0-source-snapshot/v1",
        "source_tree_manifest": manifest,
        "files": rows,
        "snapshot_sha256": Sha256Digest::ZERO,
    });
    let digest = trellis_kernel::trust_base::self_digest(
        DomainTag::RawArtifact,
        &snapshot,
        "snapshot_sha256",
    )
    .map_err(|error| error.to_string())?;
    snapshot["snapshot_sha256"] = Value::String(digest.to_string());
    canonical_json_value(&snapshot).map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
fn copy_phase0_evidence(
    campaign_repo: &Path,
    evidence_root: &Path,
    genesis_bytes: &[u8],
    sealed_bytes: &[u8],
    sealed: &Phase0SealedGeneration,
    production: &Phase0ProductionResult,
    inputs: &mut Vec<EvidenceInput>,
) -> Result<(), String> {
    let root = evidence_root.join("phase0");
    fs::create_dir(&root).map_err(io("create Phase-0 evidence directory"))?;
    fs::create_dir(root.join("receipts")).map_err(io("create Phase-0 receipt evidence"))?;
    fs::create_dir(root.join("audits")).map_err(io("create Phase-0 audit evidence"))?;
    fs::create_dir_all(evidence_root.join("source/unadapted"))
        .map_err(io("create unadapted source evidence"))?;

    let bundle_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle)
            .map_err(|error| format!("serialize Phase-0 bundle: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let ledger_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.ledger)
            .map_err(|error| format!("serialize Phase-0 ledger: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let binding_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.goal_binding_report)
            .map_err(|error| format!("serialize Phase-0 GOAL binding: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let partition_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.source_partition_manifest)
            .map_err(|error| format!("serialize Phase-0 source partition: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let production_bytes = canonical_json_value(
        &serde_json::to_value(production)
            .map_err(|error| format!("serialize Phase-0 production result: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let goal_bytes = fs::read(campaign_repo.join("GOAL.md")).map_err(io("read Phase-0 GOAL"))?;
    let unadapted_snapshot = phase0_source_snapshot(&campaign_repo.join("unadapted-crate"))?;
    let adapted_snapshot = phase0_source_snapshot(&campaign_repo.join("crate"))?;
    let base_members: [(&str, &str, &str, &[u8]); 10] = [
        ("phase0_genesis", "phase0-genesis", "phase0/GENESIS.json", genesis_bytes),
        ("phase0_seal", "phase0-sealed-generation", "phase0/SEALED_GENERATION.json", sealed_bytes),
        ("phase0_bundle", "phase0-semantic-bundle", "phase0/BUNDLE.json", &bundle_bytes),
        ("phase0_ledger", "phase0-adaptation-ledger", "phase0/ADAPTATION_LEDGER.json", &ledger_bytes),
        ("phase0_goal", "phase0-goal", "phase0/GOAL.md", &goal_bytes),
        ("phase0_goal_binding", "phase0-goal-binding-report", "phase0/GOAL_BINDING_REPORT.json", &binding_bytes),
        ("phase0_source_partition", "phase0-source-partition-manifest", "phase0/SOURCE_PARTITION_MANIFEST.json", &partition_bytes),
        ("phase0_production", "phase0-production-result", "phase0/PRODUCTION_RESULT.json", &production_bytes),
        ("phase0_source_snapshot", "phase0-unadapted-source-snapshot", "phase0/UNADAPTED_SOURCE_SNAPSHOT.json", &unadapted_snapshot),
        ("phase0_source_snapshot", "phase0-adapted-source-snapshot", "phase0/ADAPTED_SOURCE_SNAPSHOT.json", &adapted_snapshot),
    ];
    for (kind, logical_id, relative_path, bytes) in base_members {
        write_new(&evidence_root.join(relative_path), bytes)?;
        let dependencies = if logical_id == "phase0-genesis" {
            Vec::new()
        } else {
            vec!["phase0-genesis".into()]
        };
        inputs.push(EvidenceInput {
            kind: kind.into(),
            logical_id: logical_id.into(),
            relative_path: relative_path.into(),
            dependency_ids: dependencies,
        });
    }

    for (relative, bytes) in collect_regular_files(&campaign_repo.join("unadapted-crate"))? {
        let relative_utf8 = slash_path(&relative)?;
        let destination = evidence_root.join("source/unadapted").join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(io("create unadapted source parent"))?;
        }
        write_new(&destination, &bytes)?;
        inputs.push(EvidenceInput {
            kind: "phase0_unadapted_source".into(),
            logical_id: format!("phase0-unadapted-source:{relative_utf8}"),
            relative_path: format!("source/unadapted/{relative_utf8}"),
            dependency_ids: vec!["phase0-sealed-generation".into()],
        });
    }
    if source_tree_digest(&evidence_root.join("source/unadapted"))
        .map_err(|error| error.to_string())?
        != production.unadapted_source_tree_sha256
    {
        return Err("copied unadapted Phase-0 source tree changed identity".into());
    }

    for receipt in &sealed.semantic_bundle.checker_receipts {
        let path = format!("phase0/receipts/{}.json", receipt.receipt_sha256);
        let bytes = canonical_json_value(&receipt.receipt).map_err(|error| error.to_string())?;
        write_new(&evidence_root.join(&path), &bytes)?;
        inputs.push(EvidenceInput {
            kind: "phase0_checker_receipt".into(),
            logical_id: format!("phase0-receipt:{}", receipt.receipt_sha256),
            relative_path: path,
            dependency_ids: vec!["phase0-genesis".into(), "phase0-adaptation-ledger".into()],
        });
    }
    for (scenario, requests, results) in [
        ("seam-repair", &sealed.seam_audit_requests, &sealed.seam_audit_results),
        (
            "source-correspondence",
            &sealed.correspondence_audit_requests,
            &sealed.correspondence_audit_results,
        ),
    ] {
        for request in requests {
            let logical_id = format!("phase0-{scenario}-request:{}", request.request_sha256);
            let path = format!("phase0/audits/{scenario}-request-{}.json", request.request_sha256);
            let bytes = canonical_json_value(
                &serde_json::to_value(request)
                    .map_err(|error| format!("serialize Phase-0 audit request: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            write_new(&evidence_root.join(&path), &bytes)?;
            inputs.push(EvidenceInput {
                kind: "phase0_audit_request".into(),
                logical_id,
                relative_path: path,
                dependency_ids: vec!["phase0-semantic-bundle".into()],
            });
        }
        for result in results {
            let request = requests
                .iter()
                .find(|request| request.request_sha256 == result.request_sha256)
                .ok_or_else(|| "Phase-0 audit result has no request".to_owned())?;
            let request_id = format!("phase0-{scenario}-request:{}", request.request_sha256);
            let path = format!("phase0/audits/{scenario}-verdict-{}.json", result.result_sha256);
            let bytes = canonical_json_value(
                &serde_json::to_value(result)
                    .map_err(|error| format!("serialize Phase-0 audit result: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            write_new(&evidence_root.join(&path), &bytes)?;
            inputs.push(EvidenceInput {
                kind: "phase0_audit_verdict".into(),
                logical_id: format!("phase0-{scenario}-verdict:{}", result.result_sha256),
                relative_path: path,
                dependency_ids: vec![request_id],
            });
        }
    }
    Ok(())
}

fn print_json(value: &Value) -> Result<(), String> {
    let bytes = canonical_json_value(value).map_err(|error| error.to_string())?;
    print!(
        "{}",
        String::from_utf8(bytes).expect("canonical JSON is UTF-8")
    );
    Ok(())
}

fn required_string<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context} field {field} must be a string"))
}

fn require_exact_json_keys(value: &Value, expected: &[&str], context: &str) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{context} must be an object"))?;
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<_> = expected.iter().copied().collect();
    if actual != expected {
        return Err(format!(
            "{context} has unknown or missing fields: expected {expected:?}, found {actual:?}"
        ));
    }
    Ok(())
}

fn required_digest(value: &Value, field: &str, context: &str) -> Result<Sha256Digest, String> {
    required_string(value, field, context)?
        .parse()
        .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

// The enclosing Trellis runtime configuration is operational input, not a
// trust-v1 authoritative record. Existing valid configurations contain finite
// durations such as `1.0`; the narrower authoritative-record canonicalizer
// intentionally rejects those. Serde's Value serializer is deterministic for
// the exact strictly parsed object and preserves those operational semantics.
fn stable_runtime_config_bytes(value: &Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|error| format!("serialize runtime config: {error}"))
}

fn copy_regular_file(source: &Path, destination: &Path, executable: bool) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(io("stat pinned input"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "pinned input must be a non-symlink regular file: {}",
            source.display()
        ));
    }
    write_new(destination, &fs::read(source).map_err(io("read pinned input"))?)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
            .map_err(io("set pinned executable mode"))?;
    }
    let _ = executable;
    Ok(())
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(io("set evidence runner executable mode"))?;
    }
    let _ = path;
    Ok(())
}

fn copy_campaign_toolchain_closure(
    campaign_repo: &Path,
    evidence_root: &Path,
    evidence_inputs: &mut Vec<EvidenceInput>,
) -> Result<(), String> {
    let lakefiles: Vec<_> = ["lakefile.lean", "lakefile.toml"]
        .into_iter()
        .filter(|name| campaign_repo.join(name).is_file())
        .collect();
    let [lakefile_name] = lakefiles.as_slice() else {
        return Err(
            "campaign repo must contain exactly one lakefile.lean or lakefile.toml".into(),
        );
    };
    for (name, logical_id, dependency_ids) in [
        ("lean-toolchain", "lean-toolchain", Vec::new()),
        (*lakefile_name, "lakefile", Vec::new()),
        (
            "lake-manifest.json",
            "lake-manifest",
            vec!["lakefile".to_owned(), "lean-toolchain".to_owned()],
        ),
        ("APPROVED_AXIOMS.json", "approved-axioms", Vec::new()),
    ] {
        let source = campaign_repo.join(name);
        let destination = evidence_root.join("toolchain").join(name);
        copy_regular_file(&source, &destination, false)?;
        evidence_inputs.push(EvidenceInput {
            kind: "toolchain_input".into(),
            logical_id: logical_id.into(),
            relative_path: format!("toolchain/{name}"),
            dependency_ids,
        });
    }
    Ok(())
}

fn validate_recorded_extraction_provenance(value: &Value) -> Result<u32, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "extraction provenance is not an object".to_owned())?;
    let required = [
        "extraction_toolchain",
        "extractor_stack",
        "extractor_toolchain_sha256",
        "source_digests",
        "not_recorded",
    ];
    for field in required {
        if !object.contains_key(field) {
            return Err(format!("extraction provenance lacks {field}"));
        }
    }
    for field in object.keys() {
        if !required.contains(&field.as_str()) && field != "docs_corpus" {
            return Err(format!(
                "extraction provenance carries unknown field {field:?}"
            ));
        }
    }
    let toolchain = value
        .get("extraction_toolchain")
        .and_then(Value::as_object)
        .ok_or_else(|| "extraction provenance lacks extraction_toolchain".to_owned())?;
    let not_recorded = value
        .get("not_recorded")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction provenance lacks not_recorded".to_owned())?;
    let mut not_recorded_fields = BTreeSet::new();
    for entry in not_recorded {
        require_exact_json_keys(entry, &["field", "reason"], "not_recorded entry")?;
        let field = required_string(entry, "field", "not_recorded entry")?;
        let reason = required_string(entry, "reason", "not_recorded entry")?;
        if field.trim().is_empty() || reason.trim().is_empty() {
            return Err(
                "extraction provenance not_recorded entry has an empty field or reason".into(),
            );
        }
        if !not_recorded_fields.insert(field) {
            return Err(format!(
                "extraction provenance repeats not_recorded field {field:?}"
            ));
        }
    }
    for field in [
        "charon",
        "charon_revision",
        "aeneas",
        "aeneas_revision",
        "lean",
        "rustc_vv",
        "target_triple",
        "cargo_lock_sha256",
        "extraction_profile",
        "panic_strategy",
    ] {
        match toolchain.get(field) {
            Some(Value::String(value)) if !value.trim().is_empty() => {}
            Some(_) => {
                return Err(format!(
                    "extraction provenance field extraction_toolchain.{field} is empty or has the wrong type"
                ))
            }
            None if not_recorded_fields.contains(field) => {}
            None => {
                return Err(format!(
                    "extraction provenance field extraction_toolchain.{field} is absent without a not_recorded reason"
                ))
            }
        }
    }
    for (field, type_ok) in [
        (
            "cargo_features",
            toolchain.get("cargo_features").is_some_and(Value::is_array),
        ),
        (
            "cfg",
            toolchain.get("cfg").is_some_and(Value::is_array),
        ),
        (
            "overflow_checks",
            toolchain
                .get("overflow_checks")
                .is_some_and(Value::is_boolean),
        ),
    ] {
        if toolchain.contains_key(field) && !type_ok {
            return Err(format!(
                "extraction provenance field extraction_toolchain.{field} has the wrong type"
            ));
        }
        if !toolchain.contains_key(field) && !not_recorded_fields.contains(field) {
            return Err(format!(
                "extraction provenance field extraction_toolchain.{field} is absent without a not_recorded reason"
            ));
        }
    }
    if toolchain
        .get("cfg")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
        && !not_recorded_fields.contains("cfg")
    {
        return Err(
            "extraction provenance carries no cfg values and gives no not_recorded reason".into(),
        );
    }
    // This one field cannot be an honest negative at the closure gate: the
    // recorded width is the Rust half of the mandatory Lean/Rust agreement
    // check below.
    let pointer_width = toolchain
        .get("target_pointer_width")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            "extraction provenance lacks an integer target_pointer_width".to_owned()
        })?;
    let stack: Vec<String> = value
        .get("extractor_stack")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction provenance lacks extractor_stack".to_owned())?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|entry| !entry.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    "extraction provenance extractor_stack has an empty/non-string member"
                        .to_owned()
                })
        })
        .collect::<Result<_, _>>()?;
    if stack.is_empty() {
        return Err("extraction provenance extractor_stack is empty".into());
    }
    let expected_hash =
        trellis_kernel::assumptions_registry::extraction_toolchain_sha256(
            &stack,
            value
                .get("extraction_toolchain")
                .expect("validated extraction_toolchain"),
        );
    if value
        .get("extractor_toolchain_sha256")
        .and_then(Value::as_str)
        != Some(expected_hash.as_str())
    {
        return Err("extraction provenance carries an invalid toolchain fingerprint".into());
    }
    Ok(pointer_width)
}

fn provenance_discloses_missing(value: &Value, field: &str) -> bool {
    value
        .get("not_recorded")
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.get("field").and_then(Value::as_str) == Some(field))
        })
}

fn copy_and_validate_extraction_evidence(
    campaign_repo: &Path,
    evidence_root: &Path,
    evidence_inputs: &mut Vec<EvidenceInput>,
) -> Result<Value, String> {
    let source_root = campaign_repo.join("extraction-evidence");
    if cfg!(test) && !source_root.exists() {
        fs::create_dir(&source_root).map_err(io("create synthetic extraction evidence"))?;
        let mut step_paths = BTreeMap::new();
        for tool in ["rustc", "charon", "aeneas", "cargo", "kernel-scan", "kernel-print"] {
            let path = campaign_repo.join(format!("synthetic-{tool}"));
            fs::write(&path, format!("synthetic {tool}\n"))
                .map_err(io("write synthetic extraction tool"))?;
            step_paths.insert(tool, path);
        }
        let extractor_script = campaign_repo.join("synthetic-extract_pv_model.py");
        fs::write(&extractor_script, b"# synthetic extractor generator\n")
            .map_err(io("write synthetic extraction generator"))?;
        let base_steps = vec![
            serde_json::json!({"tool":"rustc-version","argv":[step_paths["rustc"],"-vV"],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
            serde_json::json!({"tool":"charon","argv":[step_paths["charon"]],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
            serde_json::json!({"tool":"cargo-metadata","argv":[step_paths["cargo"],"metadata"],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
            serde_json::json!({"tool":"cargo-rustc-cfg","argv":[step_paths["cargo"],"rustc"],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
            serde_json::json!({"tool":"aeneas","argv":[step_paths["aeneas"]],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
        ];
        let mut primary_steps = base_steps.clone();
        primary_steps.extend([
            serde_json::json!({"tool":"kernel-scan-tablet-filespec","argv":[step_paths["kernel-scan"]],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
            serde_json::json!({"tool":"kernel-print-prescribed-region","argv":[step_paths["kernel-print"]],"working_directory":campaign_repo,"effective_environment":{"PATH":"/synthetic"}}),
        ]);
        let runs = vec![
            serde_json::json!({"run_role":"primary","steps":primary_steps}),
            serde_json::json!({"run_role":"determinism_replay","steps":base_steps}),
        ];
        let synthetic_extraction_toolchain = serde_json::json!({
            "charon":"0.1.216",
            "charon_revision":"1111111111111111111111111111111111111111",
            "aeneas":"2222222",
            "aeneas_revision":"2222222222222222222222222222222222222222",
            "lean":"4.fixture",
            "rustc_vv":"rustc fixture\nhost: x86_64-unknown-linux-gnu\n",
            "target_triple":"x86_64-unknown-linux-gnu",
            "target_pointer_width":64,
            "cargo_features":[],
            "cargo_lock_sha256":raw_sha256(b"synthetic lock"),
            "extraction_profile":"dev",
            "overflow_checks":true,
            "panic_strategy":"unwind",
            "cfg":["overflow_checks","panic=\"unwind\"","target_arch=\"x86_64\"","target_pointer_width=\"64\""]
        });
        let synthetic_extractor_stack = vec![
            "charon@0.1.216".to_string(),
            "aeneas@2222222".to_string(),
        ];
        let synthetic_toolchain_sha256 =
            trellis_kernel::assumptions_registry::extraction_toolchain_sha256(
                &synthetic_extractor_stack,
                &synthetic_extraction_toolchain,
            );
        let command = serde_json::json!({
            "schema":"trellis-extraction-command/v1",
            "log_concatenation_order":"runs_then_steps_in_array_order",
            "extractor_script_path":extractor_script,
            "extractor_script_sha256":raw_sha256(b"# synthetic extractor generator\n"),
            "extraction_provenance": {
                "extraction_toolchain": synthetic_extraction_toolchain,
                "extractor_stack":synthetic_extractor_stack,
                "extractor_toolchain_sha256":synthetic_toolchain_sha256,
                "source_digests":[{"source_file":"src/lib.rs","source_sha256":raw_sha256(b"synthetic source")}],
                "not_recorded":[]
            },
            "kernel_gate_checkers":[
                {"tool":"kernel-scan-tablet-filespec","path":step_paths["kernel-scan"],"sha256":raw_sha256(&fs::read(&step_paths["kernel-scan"]).unwrap())},
                {"tool":"kernel-print-prescribed-region","path":step_paths["kernel-print"],"sha256":raw_sha256(&fs::read(&step_paths["kernel-print"]).unwrap())}
            ],
            "tool_source_checkouts":[
                {"tool":"charon","repository_root":campaign_repo,"commit":"1111111111111111111111111111111111111111","origin":"https://example.invalid/charon.git","executable_relative_path":"synthetic-charon","clean_worktree":true},
                {"tool":"aeneas","repository_root":campaign_repo,"commit":"2222222222222222222222222222222222222222","origin":"https://example.invalid/aeneas.git","executable_relative_path":"synthetic-aeneas","clean_worktree":true}
            ],
            "runs":runs,
        });
        let command_bytes = canonical_json_value(&command).map_err(|error| error.to_string())?;
        let stdout = b"";
        let stderr = b"";
        let result_runs = command["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| serde_json::json!({
                "run_role":run["run_role"],
                "steps":run["steps"].as_array().unwrap().iter().map(|step| serde_json::json!({
                    "tool":step["tool"],"exit_status":0,"stdout_offset":0,"stdout_byte_length":0,"stdout_sha256":raw_sha256(stdout),"stderr_offset":0,"stderr_byte_length":0,"stderr_sha256":raw_sha256(stderr)
                })).collect::<Vec<_>>(),
                "generated_lean_sha256":raw_sha256(b"generated"),
                "tablet_root_sha256":raw_sha256(b"tablet"),
            }))
            .collect::<Vec<_>>();
        let result = serde_json::json!({
            "schema":"trellis-extraction-result/v2",
            "command_sha256":raw_sha256(&command_bytes),
            "stdout_sha256":raw_sha256(stdout),
            "stdout_byte_length":0,
            "stderr_sha256":raw_sha256(stderr),
            "stderr_byte_length":0,
            "runs":result_runs,
        });
        let result_bytes = canonical_json_value(&result).map_err(|error| error.to_string())?;
        let determinism = serde_json::json!({
            "schema":"trellis-extraction-determinism/v2",
            "status":"passed",
            "comparison":"generated_lean_and_complete_tablet_tree_byte_identity",
            "kernel_filespec_gate":"passed",
            "kernel_prescribed_region_gate":"passed",
            "namespace_context_roundtrip_gate":"passed",
            "run_roles":["primary","determinism_replay"],
            "generated_lean_sha256":[raw_sha256(b"generated"),raw_sha256(b"generated")],
            "tablet_root_sha256":[raw_sha256(b"tablet"),raw_sha256(b"tablet")],
            "generated_lean_byte_identical":true,
            "tablet_tree_byte_identical":true,
            "result_sha256":raw_sha256(&result_bytes),
        });
        for (name, bytes) in [
            ("COMMAND.json", command_bytes),
            ("STDOUT.bin", stdout.to_vec()),
            ("STDERR.bin", stderr.to_vec()),
            ("RESULT.json", result_bytes),
            (
                "DETERMINISM.json",
                canonical_json_value(&determinism).map_err(|error| error.to_string())?,
            ),
        ] {
            fs::write(source_root.join(name), bytes)
                .map_err(io("write synthetic extraction evidence"))?;
        }
    }

    let files = collect_regular_files(&source_root)?;
    let names: BTreeSet<_> = files
        .iter()
        .map(|(path, _)| slash_path(path))
        .collect::<Result<_, _>>()?;
    let expected: BTreeSet<_> = [
        "COMMAND.json",
        "DETERMINISM.json",
        "RESULT.json",
        "STDERR.bin",
        "STDOUT.bin",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if names != expected {
        return Err(format!(
            "extraction-evidence must contain exactly the five closed members; found {names:?}"
        ));
    }
    let by_name: BTreeMap<_, _> = files
        .into_iter()
        .map(|(path, bytes)| (slash_path(&path).unwrap(), bytes))
        .collect();
    let parse_canonical = |name: &str| -> Result<Value, String> {
        let bytes = &by_name[name];
        let value = parse_json_strict(bytes).map_err(|error| error.to_string())?;
        if canonical_json_value(&value).map_err(|error| error.to_string())? != *bytes {
            return Err(format!("extraction {name} must be exact canonical JSON"));
        }
        Ok(value)
    };
    let command = parse_canonical("COMMAND.json")?;
    let result = parse_canonical("RESULT.json")?;
    let determinism = parse_canonical("DETERMINISM.json")?;
    require_exact_json_keys(
        &command,
        &[
            "schema",
            "log_concatenation_order",
            "extractor_script_path",
            "extractor_script_sha256",
            "extraction_provenance",
            "kernel_gate_checkers",
            "tool_source_checkouts",
            "runs",
        ],
        "extraction command",
    )?;
    require_exact_json_keys(
        &result,
        &[
            "schema",
            "command_sha256",
            "stdout_sha256",
            "stdout_byte_length",
            "stderr_sha256",
            "stderr_byte_length",
            "runs",
        ],
        "extraction result",
    )?;
    require_exact_json_keys(
        &determinism,
        &[
            "schema",
            "status",
            "comparison",
            "kernel_filespec_gate",
            "kernel_prescribed_region_gate",
            "namespace_context_roundtrip_gate",
            "run_roles",
            "generated_lean_sha256",
            "tablet_root_sha256",
            "generated_lean_byte_identical",
            "tablet_tree_byte_identical",
            "result_sha256",
        ],
        "extraction determinism",
    )?;
    if required_string(&command, "schema", "extraction command")?
        != "trellis-extraction-command/v1"
        || required_string(&command, "log_concatenation_order", "extraction command")?
            != "runs_then_steps_in_array_order"
        || required_string(&result, "schema", "extraction result")?
            != "trellis-extraction-result/v2"
        || required_string(&determinism, "schema", "extraction determinism")?
            != "trellis-extraction-determinism/v2"
    {
        return Err("extraction evidence uses an unsupported schema or log ordering".into());
    }
    let command_provenance = command
        .get("extraction_provenance")
        .ok_or_else(|| "extraction command lacks extraction_provenance".to_owned())?;
    validate_recorded_extraction_provenance(command_provenance)?;
    let manifest_path = campaign_repo.join("tcb_manifest.json");
    let mut manifest = if cfg!(test) && !manifest_path.exists() {
        serde_json::json!({
            "schema": "pv-tcb-disclosure/v1",
            "tcb_disclosure": [],
        })
    } else {
        parse_json_strict(
            &fs::read(&manifest_path).map_err(io("read extraction TCB manifest"))?,
        )
        .map_err(|error| error.to_string())?
    };
    if cfg!(test) && manifest.get("extraction_provenance").is_none() {
        manifest
            .as_object_mut()
            .ok_or_else(|| "tcb_manifest.json is not an object".to_owned())?
            .insert(
                "extraction_provenance".to_owned(),
                command_provenance.clone(),
            );
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest)
                .map_err(|error| format!("serialize synthetic TCB manifest: {error}"))?,
        )
        .map_err(io("write synthetic TCB extraction provenance"))?;
    }
    let manifest_provenance = manifest
        .get("extraction_provenance")
        .ok_or_else(|| "tcb_manifest.json lacks extraction_provenance".to_owned())?;
    validate_recorded_extraction_provenance(manifest_provenance)?;
    for field in [
        "extraction_toolchain",
        "extractor_stack",
        "extractor_toolchain_sha256",
        "source_digests",
    ] {
        if manifest_provenance.get(field) != command_provenance.get(field) {
            return Err(format!(
                "tcb_manifest.json extraction_provenance.{field} differs from extraction evidence"
            ));
        }
    }
    let extractor_script_path = required_string(
        &command,
        "extractor_script_path",
        "extraction command",
    )?;
    if !Path::new(extractor_script_path).is_absolute() {
        return Err("extraction generator script path must be absolute".into());
    }
    let extractor_script = canonical_regular_file(
        Path::new(extractor_script_path),
        "extraction generator script",
    )?;
    let extractor_script_bytes = fs::read(&extractor_script)
        .map_err(io("read extraction generator script"))?;
    let declared_extractor_script: Sha256Digest = required_string(
        &command,
        "extractor_script_sha256",
        "extraction command",
    )?
    .parse()
    .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
    if declared_extractor_script != raw_sha256(&extractor_script_bytes) {
        return Err("live extraction generator script differs from COMMAND.json".into());
    }
    let stdout = &by_name["STDOUT.bin"];
    let stderr = &by_name["STDERR.bin"];
    for (field, actual) in [
        ("command_sha256", raw_sha256(&by_name["COMMAND.json"])),
        ("stdout_sha256", raw_sha256(stdout)),
        ("stderr_sha256", raw_sha256(stderr)),
    ] {
        let declared: Sha256Digest = required_string(&result, field, "extraction result")?
            .parse()
            .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
        if declared != actual {
            return Err(format!("extraction result {field} differs from exact evidence bytes"));
        }
    }
    if result.get("stdout_byte_length").and_then(Value::as_u64)
        != Some(stdout.len() as u64)
        || result.get("stderr_byte_length").and_then(Value::as_u64)
            != Some(stderr.len() as u64)
    {
        return Err("extraction aggregate log length is invalid".into());
    }

    let command_runs = command
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction command lacks runs".to_owned())?;
    let result_runs = result
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction result lacks runs".to_owned())?;
    if command_runs.len() != 2 || result_runs.len() != 2 {
        return Err("trust-v1 extraction evidence requires exactly primary and replay runs".into());
    }
    let tool_source_checkouts = command
        .get("tool_source_checkouts")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction command lacks tool_source_checkouts".to_owned())?;
    if tool_source_checkouts.len() != 2 {
        return Err("extraction command must bind exactly Charon and Aeneas source checkouts".into());
    }
    let mut stdout_cursor = 0_u64;
    let mut stderr_cursor = 0_u64;
    let mut executable_by_tool = BTreeMap::<String, PathBuf>::new();
    let mut generated_roots = Vec::new();
    let mut tablet_roots = Vec::new();
    for (index, (command_run, result_run)) in command_runs.iter().zip(result_runs).enumerate() {
        require_exact_json_keys(
            command_run,
            &["run_role", "steps"],
            "extraction command run",
        )?;
        require_exact_json_keys(
            result_run,
            &[
                "run_role",
                "steps",
                "generated_lean_sha256",
                "tablet_root_sha256",
            ],
            "extraction result run",
        )?;
        let expected_role = if index == 0 { "primary" } else { "determinism_replay" };
        if required_string(command_run, "run_role", "extraction command run")? != expected_role
            || required_string(result_run, "run_role", "extraction result run")? != expected_role
        {
            return Err("extraction run roles are missing, reordered, or inconsistent".into());
        }
        let command_steps = command_run
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| "extraction command run lacks steps".to_owned())?;
        let result_steps = result_run
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| "extraction result run lacks steps".to_owned())?;
        if result_steps.len() != command_steps.len()
            || (index == 1 && command_steps.len() != 5)
            || (index == 0 && command_steps.len() < 7)
        {
            return Err("extraction run has an incomplete or unexpected checker sequence".into());
        }
        for (step_index, (command_step, result_step)) in
            command_steps.iter().zip(result_steps).enumerate()
        {
            require_exact_json_keys(
                command_step,
                &["tool", "argv", "working_directory", "effective_environment"],
                "extraction command step",
            )?;
            require_exact_json_keys(
                result_step,
                &[
                    "tool",
                    "exit_status",
                    "stdout_offset",
                    "stdout_byte_length",
                    "stdout_sha256",
                    "stderr_offset",
                    "stderr_byte_length",
                    "stderr_sha256",
                ],
                "extraction result step",
            )?;
            let expected_tool = match (index, step_index) {
                (_, 0) => "rustc-version",
                (_, 1) => "charon",
                (_, 2) => "cargo-metadata",
                (_, 3) => "cargo-rustc-cfg",
                (_, 4) => "aeneas",
                (0, 5) => "kernel-scan-tablet-filespec",
                (0, 6..) => "kernel-print-prescribed-region",
                _ => {
                    return Err(
                        "extraction checker sequence has an unsupported step category".into(),
                    )
                }
            };
            if required_string(command_step, "tool", "extraction command step")? != expected_tool
                || required_string(result_step, "tool", "extraction result step")?
                    != expected_tool
                || result_step.get("exit_status").and_then(Value::as_i64) != Some(0)
            {
                return Err("extraction step identity/order/status is invalid".into());
            }
            let argv = command_step
                .get("argv")
                .and_then(Value::as_array)
                .ok_or_else(|| "extraction command step lacks argv".to_owned())?;
            let executable = argv
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "extraction argv[0] is missing".to_owned())?;
            if !Path::new(executable).is_absolute()
                || !command_step
                    .get("effective_environment")
                    .is_some_and(Value::is_object)
                || !Path::new(required_string(
                    command_step,
                    "working_directory",
                    "extraction command step",
                )?)
                .is_absolute()
            {
                return Err("extraction command must bind absolute executables/cwd and a closed environment".into());
            }
            let executable = canonical_regular_file(
                Path::new(executable),
                "extraction tool executable",
            )?;
            if let Some(prior) = executable_by_tool.insert(expected_tool.to_owned(), executable.clone()) {
                if prior != executable {
                    return Err(format!("A/B extraction used different {expected_tool} executables"));
                }
            }
            for (stream, bytes, cursor) in [
                ("stdout", stdout.as_slice(), &mut stdout_cursor),
                ("stderr", stderr.as_slice(), &mut stderr_cursor),
            ] {
                let offset = result_step
                    .get(format!("{stream}_offset"))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("extraction step lacks {stream}_offset"))?;
                let length = result_step
                    .get(format!("{stream}_byte_length"))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("extraction step lacks {stream}_byte_length"))?;
                if offset != *cursor || offset.checked_add(length).is_none() {
                    return Err(format!("extraction {stream} ranges are not contiguous"));
                }
                let end = offset + length;
                if end > bytes.len() as u64 {
                    return Err(format!("extraction {stream} range exceeds the aggregate log"));
                }
                let actual = raw_sha256(&bytes[offset as usize..end as usize]);
                let declared: Sha256Digest = required_string(
                    result_step,
                    &format!("{stream}_sha256"),
                    "extraction result step",
                )?
                .parse()
                .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
                if actual != declared {
                    return Err(format!("extraction step {stream} digest is invalid"));
                }
                *cursor = end;
            }
        }
        generated_roots.push(required_string(
            result_run,
            "generated_lean_sha256",
            "extraction result run",
        )?);
        tablet_roots.push(required_string(
            result_run,
            "tablet_root_sha256",
            "extraction result run",
        )?);
    }
    if stdout_cursor != stdout.len() as u64 || stderr_cursor != stderr.len() as u64 {
        return Err("extraction per-step log ranges do not cover the aggregate logs".into());
    }
    let gate_checker_records = command
        .get("kernel_gate_checkers")
        .and_then(Value::as_array)
        .ok_or_else(|| "extraction command lacks kernel_gate_checkers".to_owned())?;
    let expected_gate_tools = [
        "kernel-scan-tablet-filespec",
        "kernel-print-prescribed-region",
    ];
    if gate_checker_records.len() != expected_gate_tools.len() {
        return Err("extraction command must bind exactly two kernel gate checkers".into());
    }
    for (record, tool) in gate_checker_records.iter().zip(expected_gate_tools) {
        require_exact_json_keys(
            record,
            &["tool", "path", "sha256"],
            "extraction kernel gate checker",
        )?;
        if required_string(record, "tool", "extraction kernel gate checker")? != tool {
            return Err("extraction kernel gate checker order is invalid".into());
        }
        let path = required_string(record, "path", "extraction kernel gate checker")?;
        if !Path::new(path).is_absolute() {
            return Err("extraction kernel gate checker path must be absolute".into());
        }
        let checker = canonical_regular_file(
            Path::new(path),
            "extraction kernel gate checker executable",
        )?;
        let declared: Sha256Digest = required_string(
            record,
            "sha256",
            "extraction kernel gate checker",
        )?
        .parse()
        .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
        if declared != raw_sha256(&fs::read(&checker).map_err(io("read kernel gate checker"))?)
            || executable_by_tool.get(tool) != Some(&checker)
        {
            return Err(format!(
                "extraction {tool} path/digest differs from the executed checker"
            ));
        }
    }
    if !cfg!(test) {
        let configured_pv = parse_json_strict(
            &fs::read(campaign_repo.join("pv_tablet.json"))
                .map_err(io("read configured extraction toolchain"))?,
        )
        .map_err(|error| error.to_string())?;
        let configured_extraction = configured_pv
            .get("extraction_toolchain")
            .and_then(Value::as_object)
            .ok_or_else(|| "pv_tablet.json lacks extraction_toolchain".to_owned())?;

        let expected_toolchain_sha256 = required_string(
            command_provenance,
            "extractor_toolchain_sha256",
            "extraction provenance",
        )?;
        let source_digests: BTreeSet<(String, String)> = command_provenance
            .get("source_digests")
            .and_then(Value::as_array)
            .ok_or_else(|| "extraction provenance lacks source_digests".to_owned())?
            .iter()
            .map(|digest| {
                Ok((
                    required_string(digest, "source_file", "source digest")?.to_owned(),
                    required_string(digest, "source_sha256", "source digest")?.to_owned(),
                ))
            })
            .collect::<Result<_, String>>()?;

    let models = configured_pv
        .get("extraction_models")
        .and_then(Value::as_array)
        .ok_or_else(|| "pv_tablet.json lacks extraction_models".to_owned())?;
    if models.is_empty() {
        return Err("pv_tablet.json extraction_models is empty".into());
    }
    for (index, model) in models.iter().enumerate() {
        let provenance = model.get("provenance").ok_or_else(|| {
            format!("pv_tablet.json extraction_models[{index}] lacks provenance")
        })?;
        for field in [
            "problem_id",
            "source_file",
            "source_sha256",
            "extractor_toolchain_sha256",
        ] {
            if required_string(provenance, field, "per-target extraction provenance")?
                .trim()
                .is_empty()
            {
                return Err(format!(
                    "pv_tablet.json extraction_models[{index}] provenance.{field} is empty"
                ));
            }
        }
        if provenance
            .get("extractor_toolchain_sha256")
            .and_then(Value::as_str)
            != Some(expected_toolchain_sha256)
        {
            return Err(format!(
                "pv_tablet.json extraction_models[{index}] carries the wrong extractor toolchain fingerprint"
            ));
        }
        let source_pair = (
            required_string(provenance, "source_file", "per-target extraction provenance")?
                .to_owned(),
            required_string(provenance, "source_sha256", "per-target extraction provenance")?
                .to_owned(),
        );
        if !source_digests.contains(&source_pair) {
            return Err(format!(
                "pv_tablet.json extraction_models[{index}] source provenance is absent from the manifest"
            ));
        }
    }
    for (index, checkout_record) in tool_source_checkouts.iter().enumerate() {
        require_exact_json_keys(
            checkout_record,
            &[
                "tool",
                "repository_root",
                "commit",
                "origin",
                "executable_relative_path",
                "clean_worktree",
            ],
            "extraction tool source checkout",
        )?;
        let tool = if index == 0 { "charon" } else { "aeneas" };
        if required_string(checkout_record, "tool", "extraction tool checkout")? != tool
            || checkout_record.get("clean_worktree").and_then(Value::as_bool) != Some(true)
        {
            return Err("extraction tool checkout order or clean-worktree claim is invalid".into());
        }
        let recorded_root = required_string(
            checkout_record,
            "repository_root",
            "extraction tool checkout",
        )?;
        if !Path::new(recorded_root).is_absolute() {
            return Err("extraction tool repository root must be absolute".into());
        }
        let repository_root = canonical_non_symlink_directory(
            Path::new(recorded_root),
            "extraction tool source checkout",
        )?;
        if repository_root.to_string_lossy() != recorded_root {
            return Err("extraction tool repository root is not its canonical path".into());
        }
        let live_root = command_stdout(
            &repository_root,
            "git",
            &["rev-parse", "--show-toplevel"],
        )?;
        if live_root.trim() != recorded_root {
            return Err(format!("{tool} checkout no longer resolves to its recorded git root"));
        }
        let commit = required_string(checkout_record, "commit", "extraction tool checkout")?;
        if commit.len() != 40
            || !commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || command_stdout(&repository_root, "git", &["rev-parse", "HEAD"])?.trim()
                != commit
        {
            return Err(format!("{tool} source checkout differs from its recorded commit"));
        }
        let revision_field = format!("{tool}_revision");
        let recorded_revision = command_provenance
            .get("extraction_toolchain")
            .and_then(|value| value.get(&revision_field))
            .and_then(Value::as_str);
        match recorded_revision {
            Some(revision) if revision == commit => {}
            None if provenance_discloses_missing(command_provenance, &revision_field) => {}
            _ => {
                return Err(format!(
                    "{tool} source checkout commit differs from extraction provenance"
                ))
            }
        }
        let origin = command_stdout(
            &repository_root,
            "git",
            &["remote", "get-url", "origin"],
        )?;
        if origin.trim()
            != required_string(checkout_record, "origin", "extraction tool checkout")?
        {
            return Err(format!("{tool} source checkout origin differs from COMMAND.json"));
        }
        if !command_stdout(
            &repository_root,
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty()
        {
            return Err(format!("{tool} source checkout became dirty after extraction"));
        }
        let relative = required_string(
            checkout_record,
            "executable_relative_path",
            "extraction tool checkout",
        )?;
        let recorded_executable = read_regular_beneath_with_limit(
            &repository_root,
            relative,
            "extraction tool executable in source checkout",
            256 * 1024 * 1024,
        )?;
        let executed = executable_by_tool
            .get(tool)
            .ok_or_else(|| format!("extraction command lacks executable {tool}"))?;
        if raw_sha256(&recorded_executable)
            != raw_sha256(&fs::read(executed).map_err(io("read executed extraction tool"))?)
        {
            return Err(format!("{tool} executable does not match its checkout-relative path"));
        }
        let configured = configured_extraction
            .get(tool)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pv_tablet extraction_toolchain lacks {tool}"))?;
        if tool == "aeneas" {
            if configured.len() < 7 || !commit.starts_with(configured) {
                return Err("Aeneas checkout commit does not match configured extraction version".into());
            }
        } else {
            let cargo_toml = fs::read_to_string(repository_root.join("charon/Cargo.toml"))
                .map_err(io("read Charon package manifest"))?;
            if !cargo_toml
                .lines()
                .any(|line| line.trim() == format!("version = \"{configured}\""))
            {
                return Err("Charon package version does not match configured extraction version".into());
            }
        }
    }
    }
    if required_string(&determinism, "status", "extraction determinism")? != "passed"
        || required_string(&determinism, "comparison", "extraction determinism")?
            != "generated_lean_and_complete_tablet_tree_byte_identity"
        || required_string(
            &determinism,
            "kernel_filespec_gate",
            "extraction determinism",
        )? != "passed"
        || required_string(
            &determinism,
            "kernel_prescribed_region_gate",
            "extraction determinism",
        )? != "passed"
        || required_string(
            &determinism,
            "namespace_context_roundtrip_gate",
            "extraction determinism",
        )? != "passed"
        || determinism
            .get("generated_lean_byte_identical")
            .and_then(Value::as_bool)
            != Some(true)
        || determinism
            .get("tablet_tree_byte_identical")
            .and_then(Value::as_bool)
            != Some(true)
        || generated_roots[0] != generated_roots[1]
        || tablet_roots[0] != tablet_roots[1]
        || determinism
            .get("generated_lean_sha256")
            .and_then(Value::as_array)
            != Some(
                &generated_roots
                    .iter()
                    .map(|value| Value::String((*value).to_owned()))
                    .collect::<Vec<_>>(),
            )
        || determinism
            .get("tablet_root_sha256")
            .and_then(Value::as_array)
            != Some(
                &tablet_roots
                    .iter()
                    .map(|value| Value::String((*value).to_owned()))
                    .collect::<Vec<_>>(),
            )
    {
        return Err(
            "extraction A/B determinism receipt is not a checked byte-identity pass".into(),
        );
    }
    let result_digest = raw_sha256(&by_name["RESULT.json"]);
    let declared_result: Sha256Digest =
        required_string(&determinism, "result_sha256", "extraction determinism")?
            .parse()
            .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
    if declared_result != result_digest {
        return Err("extraction determinism receipt does not bind RESULT.json".into());
    }

    fs::create_dir_all(evidence_root.join("extraction/bin"))
        .map_err(io("create extraction evidence directory"))?;
    let mut tool_ids = Vec::new();
    for (tool, executable) in executable_by_tool {
        let logical_id = format!("extraction-tool:{tool}");
        copy_regular_file(
            &executable,
            &evidence_root.join("extraction/bin").join(&tool),
            true,
        )?;
        evidence_inputs.push(EvidenceInput {
            kind: "extraction_tool".into(),
            logical_id: logical_id.clone(),
            relative_path: format!("extraction/bin/{tool}"),
            dependency_ids: Vec::new(),
        });
        tool_ids.push(logical_id);
    }
    write_new(
        &evidence_root.join("extraction/extract_pv_model.py"),
        &extractor_script_bytes,
    )?;
    evidence_inputs.push(EvidenceInput {
        kind: "extraction_generator_source".into(),
        logical_id: "extraction-generator-script".into(),
        relative_path: "extraction/extract_pv_model.py".into(),
        dependency_ids: Vec::new(),
    });
    fs::create_dir_all(evidence_root.join("extraction"))
        .map_err(io("create extraction evidence directory"))?;
    for name in [
        "COMMAND.json",
        "STDOUT.bin",
        "STDERR.bin",
        "RESULT.json",
        "DETERMINISM.json",
    ] {
        write_new(&evidence_root.join("extraction").join(name), &by_name[name])?;
    }
    let mut command_dependencies = tool_ids;
    command_dependencies.push("extraction-generator-script".into());
    command_dependencies.extend(
        evidence_inputs
            .iter()
            .filter(|input| input.logical_id.starts_with("campaign-source:"))
            .map(|input| input.logical_id.clone()),
    );
    evidence_inputs.push(EvidenceInput {
        kind: "extraction_command".into(),
        logical_id: "extraction-command".into(),
        relative_path: "extraction/COMMAND.json".into(),
        dependency_ids: command_dependencies,
    });
    for (name, logical_id) in [
        ("STDOUT.bin", "extraction-stdout"),
        ("STDERR.bin", "extraction-stderr"),
    ] {
        evidence_inputs.push(EvidenceInput {
            kind: "extraction_log".into(),
            logical_id: logical_id.into(),
            relative_path: format!("extraction/{name}"),
            dependency_ids: vec!["extraction-command".into()],
        });
    }
    let result_dependencies = vec![
        "extraction-command".into(),
        "extraction-stderr".into(),
        "extraction-stdout".into(),
    ];
    evidence_inputs.push(EvidenceInput {
        kind: "extraction_result".into(),
        logical_id: "extraction-result".into(),
        relative_path: "extraction/RESULT.json".into(),
        dependency_ids: result_dependencies,
    });
    evidence_inputs.push(EvidenceInput {
        kind: "extraction_determinism".into(),
        logical_id: "extraction-determinism".into(),
        relative_path: "extraction/DETERMINISM.json".into(),
        dependency_ids: vec!["extraction-result".into()],
    });
    Ok(Value::Array(tool_source_checkouts.clone()))
}

/// The runtime source distribution the unit-test bootstrap fixture pins, in
/// the UTF-8 byte order the manifest requires. One member per class the live
/// walk selects — a Rust source, a Python bridge module, a prompt fragment —
/// so record-only verification is exercised against every shape that drifts.
const SYNTHETIC_RUNTIME_SOURCES: &[(&str, &[u8])] = &[
    (
        "kernel/src/bin/runtime_cli.rs",
        b"// synthetic runtime source fixture\n",
    ),
    ("trellis/bridge.py", b"# synthetic runtime bridge fixture\n"),
    ("trellis/config.py", b"# synthetic sandbox config fixture\n"),
    ("trellis/sandbox.py", b"# synthetic sandbox fixture\n"),
    (
        "scripts/pv_artifact_runner.py",
        b"#!/usr/bin/env python3\n# synthetic Rust witness runner fixture\n",
    ),
    (
        "trellis/prompt_fragments/worker.md",
        b"# synthetic prompt fragment fixture\n",
    ),
];

fn recorded_rust_target_pointer_width(campaign_repo: &Path) -> Result<u32, String> {
    let manifest_path = campaign_repo.join("tcb_manifest.json");
    let manifest = parse_json_strict(
        &fs::read(&manifest_path).map_err(io("read TCB manifest for platform agreement"))?,
    )
    .map_err(|error| error.to_string())?;
    let provenance = manifest
        .get("extraction_provenance")
        .ok_or_else(|| "platform agreement failed: tcb_manifest.json lacks extraction_provenance".to_owned())?;
    validate_recorded_extraction_provenance(provenance).map_err(|error| {
        format!("platform agreement failed: invalid extraction provenance: {error}")
    })
}

fn check_extraction_platform_agreement(
    lean_system_platform_num_bits: u32,
    rust_target_pointer_width: u32,
) -> Result<(), String> {
    if lean_system_platform_num_bits != rust_target_pointer_width {
        return Err(format!(
            "platform agreement failed: Lean System.Platform.numBits is \
             {lean_system_platform_num_bits}, but extraction provenance records Rust target \
             pointer width {rust_target_pointer_width}"
        ));
    }
    Ok(())
}

fn lean_system_platform_num_bits(lean: &Path, campaign_repo: &Path) -> Result<u32, String> {
    let mut child = Command::new(lean)
        .arg("--stdin")
        .current_dir(campaign_repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn Lean System.Platform.numBits probe: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "Lean System.Platform.numBits probe has no stdin".to_owned())?
        .write_all(b"import Init.System.Platform\n#eval System.Platform.numBits\n")
        .map_err(|error| format!("write Lean System.Platform.numBits probe: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for Lean System.Platform.numBits probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Lean System.Platform.numBits probe failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rendered = std::str::from_utf8(&output.stdout)
        .map_err(|_| "Lean System.Platform.numBits output is not UTF-8".to_owned())?
        .trim();
    rendered.parse::<u32>().map_err(|_| {
        format!("Lean System.Platform.numBits probe returned {rendered:?}, expected an integer")
    })
}

/// Capture the executable and dependency surface that is intentionally *not*
/// copied into a review packet.  The boundary is deliberately explicit: the
/// sole human gate approves exact Lean/Lake/checker bytes, clean git package
/// revisions, and a versioned statement that the remaining sysroot and package
/// trees are trusted-platform inputs rather than hermetic packet members.
fn capture_trusted_platform_closure(
    campaign_repo: &Path,
    extraction_tool_source_checkouts: &Value,
    evidence_root: &Path,
    evidence_inputs: &mut Vec<EvidenceInput>,
) -> Result<(), String> {
    if usize::BITS != 64 {
        return Err("trust-v1 evidence execution requires a 64-bit host".into());
    }
    fs::create_dir_all(evidence_root.join("trusted-platform/bin"))
        .map_err(io("create trusted-platform executable directory"))?;
    fs::create_dir_all(evidence_root.join("checker"))
        .map_err(io("create checker evidence directory"))?;
    fs::create_dir_all(evidence_root.join("tool-build/workspace"))
        .map_err(io("create evidence-tool build directory"))?;

    // Unit bootstrap fixtures intentionally use a nonexistent Lean toolchain
    // and a text placeholder for the campaign tool.  They exercise the seed
    // constructor, not the production platform capture.  Still install all
    // logical IDs so those tests cover manifest/gate closure shape.
    if cfg!(test) {
        check_extraction_platform_agreement(
            64,
            recorded_rust_target_pointer_width(campaign_repo)?,
        )?;
        for (name, logical_id) in [
            ("lean", "lean-checker-executable"),
            ("lake", "lake-driver-executable"),
            ("elan-proxy", "elan-toolchain-proxy-executable"),
        ] {
            write_new(
                &evidence_root.join("trusted-platform/bin").join(name),
                format!("synthetic {name} executable\n").as_bytes(),
            )?;
            evidence_inputs.push(EvidenceInput {
                kind: "trusted_platform_executable".into(),
                logical_id: logical_id.into(),
                relative_path: format!("trusted-platform/bin/{name}"),
                dependency_ids: Vec::new(),
            });
        }
        let checker_source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or_else(|| "kernel manifest directory has no parent".to_owned())?
            .join("scripts/lean_local_closure.lean");
        copy_regular_file(
            &checker_source,
            &evidence_root.join("checker/lean_local_closure.lean"),
            false,
        )?;
        evidence_inputs.push(EvidenceInput {
            kind: "checker_source".into(),
            logical_id: "local-closure-checker-script".into(),
            relative_path: "checker/lean_local_closure.lean".into(),
            dependency_ids: vec!["lean-checker-executable".into()],
        });
        // The fixture distribution carries one member of each class the live
        // walk selects — a Rust source, a Python bridge module, and a prompt
        // fragment — so record-only verification can be exercised against the
        // shape that actually drifts in practice.
        for relative in [
            "runtime/source/kernel/src/bin",
            "runtime/source/trellis/prompt_fragments",
        ] {
            fs::create_dir_all(evidence_root.join(relative))
                .map_err(io("create synthetic runtime source directory"))?;
        }
        for (relative, bytes) in SYNTHETIC_RUNTIME_SOURCES {
            let destination = evidence_root.join("runtime/source").join(relative);
            write_new(&destination, bytes)?;
            if *relative == "scripts/pv_artifact_runner.py" {
                set_executable(&destination)?;
            }
        }
        write_new(
            &evidence_root.join("bin/trellis-runtime-kernel"),
            b"synthetic runtime kernel binary\n",
        )?;
        let runtime_source = Value::Array(
            SYNTHETIC_RUNTIME_SOURCES
                .iter()
                .map(|(relative, bytes)| {
                    let logical_id = if *relative == "scripts/pv_artifact_runner.py" {
                        trellis_kernel::trust_base::RUST_WITNESS_RUNNER_LOGICAL_ID.to_owned()
                    } else {
                        format!("trellis-runtime-source:{relative}")
                    };
                    serde_json::json!({
                        "logical_id": logical_id,
                        "relative_path": relative,
                        "sha256": raw_sha256(bytes),
                    })
                })
                .collect(),
        );
        let runtime_source_root = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&runtime_source).map_err(|error| error.to_string())?,
        );
        let runtime_receipt = serde_json::json!({
            "schema": "trellis-runtime-kernel-build-receipt/v1",
            "mode": "synthetic-unit-fixture",
            "binary_sha256": raw_sha256(b"synthetic runtime kernel binary\n"),
            "source_manifest_sha256": runtime_source_root,
            "source_members": runtime_source.clone(),
        });
        let runtime_receipt_bytes =
            canonical_json_value(&runtime_receipt).map_err(|error| error.to_string())?;
        write_new(
            &evidence_root.join("tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json"),
            &runtime_receipt_bytes,
        )?;
        let runtime_manifest = serde_json::json!({
            "schema": "trellis-runtime-distribution-manifest/v1",
            "runtime_binary_relative_path": "bin/trellis-runtime-kernel",
            "runtime_binary_sha256": raw_sha256(b"synthetic runtime kernel binary\n"),
            "build_receipt_relative_path": "tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json",
            "build_receipt_sha256": raw_sha256(&runtime_receipt_bytes),
            "source_root_relative_path": "runtime/source",
            "source_manifest_sha256": runtime_source_root,
            "source_members": runtime_source,
        });
        write_new(
            &evidence_root.join("runtime/RUNTIME_DISTRIBUTION_MANIFEST.json"),
            &canonical_json_value(&runtime_manifest).map_err(|error| error.to_string())?,
        )?;
        for (relative, _) in SYNTHETIC_RUNTIME_SOURCES {
            let logical_id = if *relative == "scripts/pv_artifact_runner.py" {
                trellis_kernel::trust_base::RUST_WITNESS_RUNNER_LOGICAL_ID.to_owned()
            } else {
                format!("trellis-runtime-source:{relative}")
            };
            evidence_inputs.push(EvidenceInput {
                kind: "runtime_source".into(),
                logical_id,
                relative_path: format!("runtime/source/{relative}"),
                dependency_ids: if *relative == "scripts/pv_artifact_runner.py" {
                    vec![
                        "trellis-runtime-source:trellis/config.py".into(),
                        "trellis-runtime-source:trellis/sandbox.py".into(),
                    ]
                } else {
                    Vec::new()
                },
            });
        }
        let mut receipt_dependencies = vec!["trellis-runtime-kernel".to_owned()];
        receipt_dependencies.extend(
            SYNTHETIC_RUNTIME_SOURCES
                .iter()
                .map(|(relative, _)| {
                    if *relative == "scripts/pv_artifact_runner.py" {
                        trellis_kernel::trust_base::RUST_WITNESS_RUNNER_LOGICAL_ID.to_owned()
                    } else {
                        format!("trellis-runtime-source:{relative}")
                    }
                }),
        );
        for (kind, logical_id, relative_path, dependencies) in [
            (
                "tool",
                "trellis-runtime-kernel",
                "bin/trellis-runtime-kernel",
                Vec::new(),
            ),
            (
                "tool_build_receipt",
                "trellis-runtime-kernel-build-receipt",
                "tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json",
                receipt_dependencies,
            ),
            (
                "runtime_distribution_manifest",
                "trellis-runtime-distribution",
                "runtime/RUNTIME_DISTRIBUTION_MANIFEST.json",
                vec!["trellis-runtime-kernel-build-receipt".into()],
            ),
        ] {
            evidence_inputs.push(EvidenceInput {
                kind: kind.into(),
                logical_id: logical_id.into(),
                relative_path: relative_path.into(),
                dependency_ids: dependencies,
            });
        }
        let boundary = serde_json::json!({
            "schema": "trellis-trusted-platform-boundary/v1",
            "mode": "synthetic-unit-fixture",
            "target_arch": std::env::consts::ARCH,
            "target_os": std::env::consts::OS,
            "target_pointer_width": 64,
            "lean_system_platform_num_bits": 64,
            "unarchived_boundary": "unit-test-only",
        });
        write_new(
            &evidence_root.join("trusted-platform/BOUNDARY.json"),
            &canonical_json_value(&boundary).map_err(|error| error.to_string())?,
        )?;
        evidence_inputs.push(EvidenceInput {
            kind: "trusted_platform_boundary".into(),
            logical_id: "trusted-platform-boundary-v1".into(),
            relative_path: "trusted-platform/BOUNDARY.json".into(),
            dependency_ids: vec![
                "elan-toolchain-proxy-executable".into(),
                "lake-driver-executable".into(),
                "lean-checker-executable".into(),
                "local-closure-checker-script".into(),
                "trellis-runtime-distribution".into(),
            ],
        });
        return Ok(());
    }

    #[cfg(not(target_os = "linux"))]
    return Err("trust-v1 trusted-platform capture v1 supports Linux only".into());

    let lean_prefix = command_stdout(campaign_repo, "lean", &["--print-prefix"])?;
    let lean_prefix_path = canonical_non_symlink_directory(
        Path::new(lean_prefix.trim()),
        "selected Lean sysroot",
    )?;
    let actual_lean = canonical_regular_file(
        &lean_prefix_path.join("bin/lean"),
        "selected Lean checker executable",
    )?;
    let actual_lake = canonical_regular_file(
        &lean_prefix_path.join("bin/lake"),
        "selected Lake driver executable",
    )?;
    let lake_proxy = resolve_path_executable("lake")?;
    let lean_version = command_stdout(campaign_repo, "lean", &["--version"])?;
    let lean_num_bits = lean_system_platform_num_bits(&actual_lean, campaign_repo)?;
    check_extraction_platform_agreement(
        lean_num_bits,
        recorded_rust_target_pointer_width(campaign_repo)?,
    )?;
    let lake_version = command_stdout(campaign_repo, "lake", &["--version"])?;
    let toolchain_pin = fs::read_to_string(campaign_repo.join("lean-toolchain"))
        .map_err(io("read Lean toolchain pin"))?;
    validate_lean_version_pin(&toolchain_pin, &lean_version)?;
    for (source, name, logical_id) in [
        (&actual_lean, "lean", "lean-checker-executable"),
        (&actual_lake, "lake", "lake-driver-executable"),
        (&lake_proxy, "elan-proxy", "elan-toolchain-proxy-executable"),
    ] {
        copy_regular_file(
            source,
            &evidence_root.join("trusted-platform/bin").join(name),
            true,
        )?;
        evidence_inputs.push(EvidenceInput {
            kind: "trusted_platform_executable".into(),
            logical_id: logical_id.into(),
            relative_path: format!("trusted-platform/bin/{name}"),
            dependency_ids: vec!["lean-toolchain".into()],
        });
    }

    let trellis_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "kernel manifest directory has no parent".to_owned())?;
    let checker_source = canonical_regular_file(
        &trellis_root.join("scripts/lean_local_closure.lean"),
        "local-closure checker source",
    )?;
    copy_regular_file(
        &checker_source,
        &evidence_root.join("checker/lean_local_closure.lean"),
        false,
    )?;
    evidence_inputs.push(EvidenceInput {
        kind: "checker_source".into(),
        logical_id: "local-closure-checker-script".into(),
        relative_path: "checker/lean_local_closure.lean".into(),
        dependency_ids: vec![
            "lake-driver-executable".into(),
            "lean-checker-executable".into(),
        ],
    });

    let package_records = validate_lake_dependency_checkouts(campaign_repo)?;
    let lake_manifest_bytes = fs::read(campaign_repo.join("lake-manifest.json"))
        .map_err(io("read Lake dependency manifest"))?;

    let (runtime_build_receipt, runtime_distribution_manifest) =
        capture_runtime_distribution(trellis_root, evidence_root, evidence_inputs)?;

    let boundary = serde_json::json!({
        "schema": "trellis-trusted-platform-boundary/v1",
        "boundary_version": "linux-x86_64-lean-lake-git-dependencies-v1",
        "target_os": std::env::consts::OS,
        "target_arch": std::env::consts::ARCH,
        "target_pointer_width": lean_num_bits,
        "lean_system_platform_num_bits": lean_num_bits,
        "lean_toolchain_pin_utf8": toolchain_pin,
        "lean_version_utf8": lean_version.trim(),
        "lake_version_utf8": lake_version.trim(),
        "lean_sysroot_path_utf8": lean_prefix_path.to_string_lossy(),
        "lean_executable_sha256": raw_sha256(&fs::read(&actual_lean).map_err(io("read selected Lean executable"))?),
        "lake_executable_sha256": raw_sha256(&fs::read(&actual_lake).map_err(io("read selected Lake executable"))?),
        "elan_proxy_executable_sha256": raw_sha256(&fs::read(&lake_proxy).map_err(io("read Elan proxy executable"))?),
        "local_closure_checker_sha256": raw_sha256(&fs::read(&checker_source).map_err(io("read local-closure checker"))?),
        "lake_manifest_sha256": raw_sha256(&lake_manifest_bytes),
        "dependency_checkouts": package_records,
        "extraction_tool_source_checkouts": extraction_tool_source_checkouts,
        "runtime_kernel_build_receipt_sha256": raw_sha256(&canonical_json_value(&runtime_build_receipt).map_err(|error| error.to_string())?),
        "runtime_distribution_manifest_sha256": raw_sha256(&canonical_json_value(&runtime_distribution_manifest).map_err(|error| error.to_string())?),
        "unarchived_boundary": "The Lean sysroot libraries, git dependency working trees and generated olean/native caches, OS kernel, libc, dynamic loader, CPU, and filesystem are not copied into the packet. Approval trusts this exact versioned platform description, the copied executable bytes, the Lake manifest, each clean git commit, and the post-gate immutability of those unarchived runtime inputs. Later executable, manifest, and checker-script drift is rechecked by proof receipts and fails closed. Dependency-source or generated-cache drift is outside the approval but is part of this named trusted-platform assumption: host-level rollback or tampering there is not detected by the packet alone.",
    });
    write_new(
        &evidence_root.join("trusted-platform/BOUNDARY.json"),
        &canonical_json_value(&boundary).map_err(|error| error.to_string())?,
    )?;
    evidence_inputs.push(EvidenceInput {
        kind: "trusted_platform_boundary".into(),
        logical_id: "trusted-platform-boundary-v1".into(),
        relative_path: "trusted-platform/BOUNDARY.json".into(),
        dependency_ids: vec![
            "elan-toolchain-proxy-executable".into(),
            "lake-driver-executable".into(),
            "lake-manifest".into(),
            "lean-checker-executable".into(),
            "lean-toolchain".into(),
            "local-closure-checker-script".into(),
            "trellis-runtime-distribution".into(),
        ],
    });
    Ok(())
}


fn capture_runtime_distribution(
    trellis_root: &Path,
    evidence_root: &Path,
    evidence_inputs: &mut Vec<EvidenceInput>,
) -> Result<(Value, Value), String> {
    const PINNED_RUST_TOOLCHAIN: &str = "nightly-2026-06-01";
    if std::env::var_os("RUSTFLAGS").is_some()
        || std::env::var_os("CARGO_ENCODED_RUSTFLAGS").is_some()
        || std::env::var_os("CARGO_BUILD_TARGET").is_some()
    {
        return Err(
            "trust-v1 runtime rebuild requires RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS, and CARGO_BUILD_TARGET to be unset"
                .into(),
        );
    }
    let trellis_root = canonical_non_symlink_directory(trellis_root, "Trellis source root")?;
    let runtime_source_root = evidence_root.join("runtime/source");
    fs::create_dir_all(&runtime_source_root)
        .map_err(io("create runtime distribution source root"))?;
    let sources = collect_runtime_distribution_sources(&trellis_root, true)?;

    let mut source_members = Vec::with_capacity(sources.len());
    let mut source_ids = Vec::with_capacity(sources.len());
    for (relative, bytes) in &sources {
        let destination = runtime_source_root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(io("create runtime source parent"))?;
        }
        write_new(&destination, bytes)?;
        if relative == "scripts/pv_artifact_runner.py" {
            set_executable(&destination)?;
        }
        let logical_id = if relative == "scripts/pv_artifact_runner.py" {
            trellis_kernel::trust_base::RUST_WITNESS_RUNNER_LOGICAL_ID.to_owned()
        } else {
            format!("trellis-runtime-source:{relative}")
        };
        evidence_inputs.push(EvidenceInput {
            kind: "runtime_source".into(),
            logical_id: logical_id.clone(),
            relative_path: format!("runtime/source/{relative}"),
            dependency_ids: if relative == "scripts/pv_artifact_runner.py" {
                vec![
                    "trellis-runtime-source:trellis/config.py".into(),
                    "trellis-runtime-source:trellis/sandbox.py".into(),
                ]
            } else {
                Vec::new()
            },
        });
        source_ids.push(logical_id.clone());
        source_members.push(serde_json::json!({
            "logical_id": logical_id,
            "relative_path": relative,
            "sha256": raw_sha256(bytes),
        }));
    }
    let source_manifest_sha256 = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&Value::Array(source_members.clone()))
            .map_err(|error| error.to_string())?,
    );

    let cargo_proxy = resolve_path_executable("cargo")?;
    let rustc_proxy = resolve_path_executable("rustc")?;
    let rust_sysroot = command_stdout(&trellis_root.join("kernel"), "rustc", &["--print", "sysroot"])?;
    let rust_sysroot = canonical_non_symlink_directory(
        Path::new(rust_sysroot.trim()),
        "selected Rust sysroot",
    )?;
    let actual_rustc = canonical_regular_file(
        &rust_sysroot.join("bin/rustc"),
        "selected rustc executable",
    )?;
    let actual_cargo = canonical_regular_file(
        &rust_sysroot.join("bin/cargo"),
        "selected cargo executable",
    )?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".cargo")));
    let build_path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH is required for the runtime rebuild".to_owned())?;
    let build_home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is required for the runtime rebuild".to_owned())?;
    let mut build_environment = BTreeMap::from([
        ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
        ("HOME".to_owned(), build_home.to_string_lossy().into_owned()),
        ("LANG".to_owned(), "C.UTF-8".to_owned()),
        ("PATH".to_owned(), build_path.to_string_lossy().into_owned()),
        ("RUSTC".to_owned(), actual_rustc.to_string_lossy().into_owned()),
        ("RUSTUP_TOOLCHAIN".to_owned(), PINNED_RUST_TOOLCHAIN.to_owned()),
    ]);
    if let Some(cargo_home) = cargo_home.as_ref() {
        build_environment.insert(
            "CARGO_HOME".to_owned(),
            cargo_home.to_string_lossy().into_owned(),
        );
    }
    let rebuild_root = evidence_root.join("tool-build/runtime-kernel-rebuild");
    fs::create_dir(&rebuild_root).map_err(io("create runtime rebuild root"))?;
    let mut rebuild = Command::new(&actual_cargo);
    rebuild
        .current_dir(trellis_root.join("kernel"))
        .env_clear()
        .args([
            "build",
            "--release",
            "--locked",
            "--offline",
            "--bin",
            "trellis_runtime_cli",
        ])
        .arg("--manifest-path")
        .arg(trellis_root.join("kernel/Cargo.toml"))
        .arg("--target-dir")
        .arg(&rebuild_root);
    for (name, value) in &build_environment {
        rebuild.env(name, value);
    }
    let status = rebuild.status().map_err(io("spawn runtime kernel build"));
    let build_result = match status {
        Ok(status) if status.success() => {
            let binary = rebuild_root.join("release/trellis_runtime_cli");
            let bytes = fs::read(&binary).map_err(io("read rebuilt runtime kernel"))?;
            copy_regular_file(
                &binary,
                &evidence_root.join("bin/trellis-runtime-kernel"),
                true,
            )?;
            Ok(raw_sha256(&bytes))
        }
        Ok(status) => Err(format!("runtime kernel build failed with {status}")),
        Err(error) => Err(error),
    };
    let cleanup = fs::remove_dir_all(&rebuild_root).map_err(io("remove runtime rebuild root"));
    let binary_sha256 = build_result?;
    cleanup?;
    evidence_inputs.push(EvidenceInput {
        kind: "tool".into(),
        logical_id: "trellis-runtime-kernel".into(),
        relative_path: "bin/trellis-runtime-kernel".into(),
        dependency_ids: Vec::new(),
    });

    let rustc_version = command_stdout_path(&trellis_root, &actual_rustc, &["-vV"])?;
    let cargo_version = command_stdout_path(&trellis_root, &actual_cargo, &["-V"])?;
    let receipt = serde_json::json!({
        "schema": "trellis-runtime-kernel-build-receipt/v1",
        "build_recipe": "cargo build --release --locked --offline --bin trellis_runtime_cli --manifest-path kernel/Cargo.toml --target-dir <fresh-empty-dir>",
        "build_environment": build_environment,
        "rust_toolchain": PINNED_RUST_TOOLCHAIN,
        "binary_sha256": binary_sha256,
        "built_from_manifest_verified_live_source_closure": true,
        "cargo_executable_sha256": raw_sha256(&fs::read(&actual_cargo).map_err(io("read actual cargo"))?),
        "cargo_proxy_sha256": raw_sha256(&fs::read(&cargo_proxy).map_err(io("read cargo proxy"))?),
        "cargo_version_utf8": cargo_version.trim(),
        "rustc_executable_sha256": raw_sha256(&fs::read(&actual_rustc).map_err(io("read actual rustc"))?),
        "rustc_proxy_sha256": raw_sha256(&fs::read(&rustc_proxy).map_err(io("read rustc proxy"))?),
        "rustc_version_verbose_utf8": rustc_version.trim(),
        "source_manifest_sha256": source_manifest_sha256,
        "source_members": source_members.clone(),
        "cargo_profile": "release",
        "locked": true,
        "offline": true,
        "rustflags": "",
        "cargo_encoded_rustflags": "",
        "cargo_build_target_override": "",
        "target_pointer_width": usize::BITS,
        "unarchived_build_dependency_boundary": "Cargo registry sources and native dependencies selected by the exact Cargo.lock are not duplicated; the authoritative runtime is built once under the closed recipe and its exact bytes and curated runtime source distribution are seed-bound.",
    });
    // The receipt below depends on the exact compiler, cargo, and rustup
    // proxies that built the runtime kernel. Register them as
    // trusted-platform leaves here; this was previously done by the
    // evidence-tool harness build, which no longer exists.
    for (source, name, logical_id) in [
        (&actual_rustc, "rustc", "rustc-build-executable"),
        (&actual_cargo, "cargo", "cargo-build-executable"),
        (&rustc_proxy, "rustup-proxy", "rustup-toolchain-proxy-executable"),
        (&cargo_proxy, "cargo-proxy", "cargo-toolchain-proxy-executable"),
    ] {
        copy_regular_file(
            source,
            &evidence_root.join("trusted-platform/bin").join(name),
            true,
        )?;
        evidence_inputs.push(EvidenceInput {
            kind: "trusted_platform_executable".into(),
            logical_id: logical_id.into(),
            relative_path: format!("trusted-platform/bin/{name}"),
            dependency_ids: Vec::new(),
        });
    }
    let receipt_bytes = canonical_json_value(&receipt).map_err(|error| error.to_string())?;
    write_new(
        &evidence_root.join("tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json"),
        &receipt_bytes,
    )?;
    let mut receipt_dependencies = vec![
        "cargo-build-executable".into(),
        "cargo-toolchain-proxy-executable".into(),
        "rustc-build-executable".into(),
        "rustup-toolchain-proxy-executable".into(),
        "trellis-runtime-kernel".into(),
    ];
    receipt_dependencies.extend(source_ids.iter().cloned());
    evidence_inputs.push(EvidenceInput {
        kind: "tool_build_receipt".into(),
        logical_id: "trellis-runtime-kernel-build-receipt".into(),
        relative_path: "tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json".into(),
        dependency_ids: receipt_dependencies,
    });

    let manifest = serde_json::json!({
        "schema": "trellis-runtime-distribution-manifest/v1",
        "runtime_binary_relative_path": "bin/trellis-runtime-kernel",
        "runtime_binary_sha256": binary_sha256,
        "build_receipt_relative_path": "tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json",
        "build_receipt_sha256": raw_sha256(&receipt_bytes),
        "source_root_relative_path": "runtime/source",
        "source_manifest_sha256": source_manifest_sha256,
        "source_members": source_members,
    });
    write_new(
        &evidence_root.join("runtime/RUNTIME_DISTRIBUTION_MANIFEST.json"),
        &canonical_json_value(&manifest).map_err(|error| error.to_string())?,
    )?;
    evidence_inputs.push(EvidenceInput {
        kind: "runtime_distribution_manifest".into(),
        logical_id: "trellis-runtime-distribution".into(),
        relative_path: "runtime/RUNTIME_DISTRIBUTION_MANIFEST.json".into(),
        dependency_ids: vec!["trellis-runtime-kernel-build-receipt".into()],
    });
    Ok((receipt, manifest))
}

/// Verify the frozen runtime distribution and RECORD the live toolchain
/// identity beside it.
///
/// Two different things used to be checked here under one verdict. The frozen
/// side — the seed-root binding, the evidence closure, the canonical JSON and
/// closed key sets, the pinned binary, the build receipt and its cross-check,
/// the copied source bytes — is the distribution's own integrity: it cannot
/// drift through ordinary work, and a mismatch means the frozen artifacts are
/// corrupt. That side still refuses, by name.
///
/// The live side — whether the working tree still matches the pinned manifest
/// byte for byte, and whether the runtime binary on disk is the pinned one —
/// is not enforcement any more. Agents cannot write the source tree (it is
/// read-only bound, with a self-test in `trellis/sandbox.py`), and a host that
/// can rewrite it can rewrite this check too, so refusing bought provenance
/// theater at the price of a full re-pin for every prompt-fragment word. The
/// live observation is therefore computed, reported, and never fatal: the
/// caller appends the emitted JSON to a provenance trail.
fn verify_runtime_distribution(
    seed_manifest_path: &Path,
    evidence_manifest_path: &Path,
    evidence_root: &Path,
    live_trellis_root: &Path,
    runtime_binary: &Path,
) -> Result<(), String> {
    print_json(&observe_runtime_distribution(
        seed_manifest_path,
        evidence_manifest_path,
        evidence_root,
        live_trellis_root,
        runtime_binary,
    )?)
}

/// The verdict half of [`verify_runtime_distribution`], returning the
/// observation instead of printing it so callers and tests can read the
/// recorded figures.
fn observe_runtime_distribution(
    seed_manifest_path: &Path,
    evidence_manifest_path: &Path,
    evidence_root: &Path,
    live_trellis_root: &Path,
    runtime_binary: &Path,
) -> Result<Value, String> {
    let seed_bytes = fs::read(seed_manifest_path).map_err(io("read seed manifest"))?;
    let seed_value = parse_json_strict(&seed_bytes).map_err(|error| error.to_string())?;
    if canonical_json_value(&seed_value).map_err(|error| error.to_string())? != seed_bytes {
        return Err("seed manifest must be exact canonical JSON".into());
    }
    let registry = SchemaRegistry::v1().map_err(|error| error.to_string())?;
    let seed_record = AuthoritativeRecord::parse(&registry, seed_value)
        .map_err(|error| error.to_string())?;
    let seed_roots = validate_seed_manifest_semantics(&seed_record)
        .map_err(|error| error.to_string())?;
    let evidence_manifest_bytes =
        fs::read(evidence_manifest_path).map_err(io("read evidence manifest"))?;
    let evidence = verify_evidence_tool_manifest(evidence_root, &evidence_manifest_bytes)
        .map_err(|error| error.to_string())?;
    if evidence.evidence_tool_input_root != seed_roots.evidence_tool_input_root {
        return Err("runtime distribution evidence root differs from the seed-approved root".into());
    }
    let manifest_bytes = read_regular_beneath_with_limit(
        evidence_root,
        "runtime/RUNTIME_DISTRIBUTION_MANIFEST.json",
        "runtime distribution manifest",
        16 * 1024 * 1024,
    )?;
    let manifest = parse_json_strict(&manifest_bytes).map_err(|error| error.to_string())?;
    if canonical_json_value(&manifest).map_err(|error| error.to_string())? != manifest_bytes {
        return Err("runtime distribution manifest must be exact canonical JSON".into());
    }
    // Record-only verification changes what is reported, not what is pinned,
    // so capture still writes `.../v1` and every already-bootstrapped campaign
    // verifies unchanged. A later manifest schema is added to this list rather
    // than replacing the entry.
    if !matches!(
        required_string(&manifest, "schema", "runtime distribution manifest")?,
        "trellis-runtime-distribution-manifest/v1"
    ) {
        return Err("unsupported runtime distribution manifest schema".into());
    }
    require_exact_json_keys(
        &manifest,
        &[
            "schema",
            "runtime_binary_relative_path",
            "runtime_binary_sha256",
            "build_receipt_relative_path",
            "build_receipt_sha256",
            "source_root_relative_path",
            "source_manifest_sha256",
            "source_members",
        ],
        "runtime distribution manifest",
    )?;
    if required_string(
        &manifest,
        "runtime_binary_relative_path",
        "runtime distribution manifest",
    )? != "bin/trellis-runtime-kernel"
        || required_string(
            &manifest,
            "build_receipt_relative_path",
            "runtime distribution manifest",
        )? != "tool-build/RUNTIME_KERNEL_BUILD_RECEIPT.json"
        || required_string(
            &manifest,
            "source_root_relative_path",
            "runtime distribution manifest",
        )? != "runtime/source"
    {
        return Err("runtime distribution manifest uses an unsupported path layout".into());
    }
    let declared_binary: Sha256Digest = required_string(
        &manifest,
        "runtime_binary_sha256",
        "runtime distribution manifest",
    )?
    .parse()
    .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
    let evidence_binary = read_regular_beneath_with_limit(
        evidence_root,
        required_string(
            &manifest,
            "runtime_binary_relative_path",
            "runtime distribution manifest",
        )?,
        "pinned runtime binary",
        256 * 1024 * 1024,
    )?;
    // Frozen side: the copy inside the evidence root must still be the binary
    // the manifest declares.
    if raw_sha256(&evidence_binary) != declared_binary {
        return Err("pinned runtime CLI binary differs from the distribution manifest".into());
    }
    // Live side: recorded, never fatal. An unreadable path is still an error —
    // that is a broken invocation, not drift.
    let live_binary = fs::read(canonical_regular_file(runtime_binary, "runtime CLI binary")?)
        .map_err(io("read runtime CLI binary"))?;
    let live_binary_sha256 = raw_sha256(&live_binary);
    let runtime_binary_drift_observed = live_binary_sha256 != declared_binary;
    let receipt_bytes = read_regular_beneath_with_limit(
        evidence_root,
        required_string(
            &manifest,
            "build_receipt_relative_path",
            "runtime distribution manifest",
        )?,
        "runtime build receipt",
        32 * 1024 * 1024,
    )?;
    let declared_receipt: Sha256Digest = required_string(
        &manifest,
        "build_receipt_sha256",
        "runtime distribution manifest",
    )?
    .parse()
    .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
    if raw_sha256(&receipt_bytes) != declared_receipt {
        return Err("runtime build receipt differs from the distribution manifest".into());
    }
    let receipt = parse_json_strict(&receipt_bytes).map_err(|error| error.to_string())?;
    if canonical_json_value(&receipt).map_err(|error| error.to_string())? != receipt_bytes {
        return Err("runtime build receipt must be exact canonical JSON".into());
    }
    if required_string(&receipt, "schema", "runtime build receipt")?
        != "trellis-runtime-kernel-build-receipt/v1"
    {
        return Err("unsupported runtime build receipt schema".into());
    }
    let source_members = manifest
        .get("source_members")
        .and_then(Value::as_array)
        .ok_or_else(|| "runtime distribution manifest lacks source_members".to_owned())?;
    let declared_source_root: Sha256Digest = required_string(
        &manifest,
        "source_manifest_sha256",
        "runtime distribution manifest",
    )?
    .parse()
    .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
    let computed_source_root = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&Value::Array(source_members.clone()))
            .map_err(|error| error.to_string())?,
    );
    if computed_source_root != declared_source_root {
        return Err("runtime source manifest root mismatch".into());
    }
    if required_digest(&receipt, "binary_sha256", "runtime build receipt")? != declared_binary
        || required_digest(
            &receipt,
            "source_manifest_sha256",
            "runtime build receipt",
        )? != declared_source_root
        || receipt.get("source_members") != Some(&Value::Array(source_members.clone()))
    {
        return Err("runtime build receipt does not describe the declared distribution".into());
    }
    if source_members.is_empty() {
        return Err("runtime source distribution cannot be empty".into());
    }
    let mut declared_by_path: BTreeMap<String, Sha256Digest> = BTreeMap::new();
    let mut prior_path: Option<String> = None;
    for member in source_members {
        require_exact_json_keys(
            member,
            &["logical_id", "relative_path", "sha256"],
            "runtime source member",
        )?;
        let relative = required_string(member, "relative_path", "runtime source member")?;
        if prior_path
            .as_ref()
            .is_some_and(|prior| prior.as_bytes() >= relative.as_bytes())
        {
            return Err("runtime source members must be unique UTF-8 byte sorted".into());
        }
        prior_path = Some(relative.to_owned());
        if required_string(member, "logical_id", "runtime source member")?
            != format!("trellis-runtime-source:{relative}")
        {
            return Err(format!(
                "runtime source {relative} has the wrong logical identifier"
            ));
        }
        let declared: Sha256Digest = required_string(member, "sha256", "runtime source member")?
            .parse()
            .map_err(|error: trellis_kernel::trust_base::TrustError| error.to_string())?;
        // Frozen side: the copy carried inside the evidence root is what the
        // runtime would be rebuilt from, so it must match its declared digest.
        let copied = read_regular_beneath_with_limit(
            evidence_root,
            &format!("runtime/source/{relative}"),
            "copied runtime source",
            32 * 1024 * 1024,
        )?;
        if raw_sha256(&copied) != declared {
            return Err(format!(
                "pinned runtime source {relative} differs from the distribution manifest"
            ));
        }
        declared_by_path.insert(relative.to_owned(), declared);
    }

    // Live side, recorded only. The walk itself is fallible on a tree that
    // carries something the capture rule does not admit (a stray extension
    // under `kernel/src`, a symlink where a regular file is expected). That is
    // live drift too, so it is reported rather than raised: the scan is marked
    // incomplete and the per-class figures stay null instead of claiming a
    // comparison that was never made.
    let live_scan = canonical_non_symlink_directory(live_trellis_root, "live Trellis root")
        .and_then(|live_root| collect_runtime_distribution_sources(&live_root, false));
    let mut observation = serde_json::json!({
        "schema": "trellis-runtime-distribution-observation/v1",
        "status": "recorded",
        "enforcement": "record_only",
        "evidence_tool_input_root": evidence.evidence_tool_input_root,
        "declared_runtime_binary_sha256": declared_binary,
        "live_runtime_binary_sha256": live_binary_sha256,
        "runtime_binary_drift_observed": runtime_binary_drift_observed,
        "declared_source_manifest_sha256": declared_source_root,
        "declared_source_file_count": declared_by_path.len(),
        "live_source_scan": "incomplete",
        "live_source_scan_error": Value::Null,
        "live_source_file_count": Value::Null,
        "live_python_manifest_sha256": Value::Null,
        "live_python_file_count": Value::Null,
        "source_members_added": Value::Null,
        "source_members_removed": Value::Null,
        "source_members_changed": Value::Null,
        "source_membership_drift_observed": Value::Null,
        "source_content_drift_observed": Value::Null,
    });
    match live_scan {
        Err(error) => {
            observation["live_source_scan_error"] = Value::String(error);
        }
        Ok(live_sources) => {
            let mut added = 0usize;
            let mut changed = 0usize;
            for (relative, bytes) in &live_sources {
                match declared_by_path.get(relative) {
                    None => added += 1,
                    Some(declared) if raw_sha256(bytes) != *declared => changed += 1,
                    Some(_) => {}
                }
            }
            let removed = declared_by_path
                .keys()
                .filter(|relative| !live_sources.contains_key(*relative))
                .count();
            // The Python bridge and checker are the half of the toolchain the
            // binary digest says nothing about, so they get their own
            // identity: the same manifest-node hash shape the pinned source
            // root uses, over the live `trellis/**/*.py` set alone.
            let python_members: Vec<Value> = live_sources
                .iter()
                .filter(|(relative, _)| {
                    relative.starts_with("trellis/") && relative.ends_with(".py")
                })
                .map(|(relative, bytes)| {
                    serde_json::json!({
                        "relative_path": relative,
                        "sha256": raw_sha256(bytes),
                    })
                })
                .collect();
            let python_file_count = python_members.len();
            let live_python_manifest_sha256 = tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&Value::Array(python_members))
                    .map_err(|error| error.to_string())?,
            );
            observation["live_source_scan"] = Value::String("complete".into());
            observation["live_source_file_count"] = live_sources.len().into();
            observation["live_python_manifest_sha256"] =
                Value::String(live_python_manifest_sha256.to_string());
            observation["live_python_file_count"] = python_file_count.into();
            observation["source_members_added"] = added.into();
            observation["source_members_removed"] = removed.into();
            observation["source_members_changed"] = changed.into();
            observation["source_membership_drift_observed"] =
                Value::Bool(added != 0 || removed != 0);
            observation["source_content_drift_observed"] = Value::Bool(changed != 0);
        }
    }
    Ok(observation)
}

/// Return the complete runtime-affecting source surface. The production
/// capture requires all build/runtime roots; detached verification permits a
/// deliberately minimal synthetic fixture but still rejects every selected
/// live member not named by its seed-bound manifest.
fn collect_runtime_distribution_sources(
    trellis_root: &Path,
    require_complete: bool,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut sources = BTreeMap::new();
    let mut add_source = |relative: String, bytes: Vec<u8>| -> Result<(), String> {
        if sources.insert(relative.clone(), bytes).is_some() {
            return Err(format!("duplicate runtime distribution source {relative}"));
        }
        Ok(())
    };
    for relative in ["rust-toolchain.toml", "kernel/Cargo.toml", "kernel/Cargo.lock"] {
        let path = trellis_root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                add_source(
                    relative.into(),
                    fs::read(&path).map_err(io("read runtime build input"))?,
                )?;
            }
            Ok(_) => {
                return Err(format!(
                    "runtime distribution input is not a regular non-symlink file: {}",
                    path.display()
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound && !require_complete => {}
            Err(error) => {
                return Err(format!(
                    "read runtime distribution input {}: {error}",
                    path.display()
                ));
            }
        }
    }
    let build_rs = trellis_root.join("kernel/build.rs");
    match fs::symlink_metadata(&build_rs) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            add_source(
                "kernel/build.rs".into(),
                fs::read(&build_rs).map_err(io("read runtime build script"))?,
            )?;
        }
        Ok(_) => {
            return Err(format!(
                "runtime build script is not a regular non-symlink file: {}",
                build_rs.display()
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "read runtime build script {}: {error}",
                build_rs.display()
            ));
        }
    }
    for (directory, prefix) in [
        (trellis_root.join("kernel/src"), "kernel/src"),
        (trellis_root.join("trellis"), "trellis"),
        (trellis_root.join("scripts"), "scripts"),
    ] {
        let files = match fs::symlink_metadata(&directory) {
            Ok(_) => collect_regular_files(&directory)?,
            Err(error) if error.kind() == ErrorKind::NotFound && !require_complete => continue,
            Err(error) => {
                return Err(format!(
                    "read runtime distribution directory {}: {error}",
                    directory.display()
                ));
            }
        };
        for (relative, bytes) in files {
            let selected = match prefix {
                // The runtime is rebuilt from these copied bytes, so every
                // compile-time include has to travel with the Rust sources:
                // `trust_base/schemas/v2.3/*.schema.json` is `include_str!`ed
                // into `SCHEMA_SOURCES` and the crate does not compile without
                // it. Nothing else may live under `kernel/src` — the closure is
                // the reproducible-build receipt, and admitting a documentation
                // extension would fold prose churn into it. The arm stays
                // fail-closed so a future stray file can neither enter the
                // receipt nor silently escape it (`kernel/TRUST_BASE_SCHEMAS.md`).
                "kernel/src" => {
                    if !matches!(
                        relative.extension().and_then(|value| value.to_str()),
                        Some("rs" | "json")
                    ) {
                        return Err(format!(
                            "unexpected kernel source member {}: kernel/src carries Rust sources and compile-time-included JSON only",
                            relative.display()
                        ));
                    }
                    true
                }
                "trellis" => {
                    !relative
                        .components()
                        .any(|component| component.as_os_str() == "__pycache__")
                        && matches!(
                            relative.extension().and_then(|value| value.to_str()),
                            Some("py" | "md")
                        )
                }
                "scripts" => {
                    relative.components().count() == 1
                        && matches!(
                            relative.extension().and_then(|value| value.to_str()),
                            Some("sh" | "py" | "lean")
                        )
                }
                _ => unreachable!(),
            };
            if selected {
                add_source(format!("{prefix}/{}", slash_path(&relative)?), bytes)?;
            }
        }
    }
    Ok(sources)
}

fn validate_lake_dependency_checkouts(campaign_repo: &Path) -> Result<Vec<Value>, String> {
    let manifest_bytes = fs::read(campaign_repo.join("lake-manifest.json"))
        .map_err(io("read Lake dependency manifest"))?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("parse Lake dependency manifest: {error}"))?;
    let packages_dir = manifest
        .get("packagesDir")
        .and_then(Value::as_str)
        .ok_or_else(|| "Lake manifest lacks packagesDir".to_owned())?;
    if packages_dir != ".lake/packages" {
        return Err("trust-v1 requires Lake packagesDir to be exactly .lake/packages".into());
    }
    let packages = manifest
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Lake manifest lacks packages".to_owned())?;
    if packages.is_empty() {
        return Err("trust-v1 Lake dependency manifest cannot be empty".into());
    }
    let package_root = canonical_non_symlink_directory(
        &campaign_repo.join(packages_dir),
        "Lake dependency checkout root",
    )?;
    let mut records = Vec::with_capacity(packages.len());
    let mut names = BTreeSet::new();
    for package in packages {
        let name = required_string(package, "name", "Lake package")?;
        validate_single_path_component(name, "Lake package name")?;
        if !names.insert(name.to_owned()) {
            return Err(format!("Lake manifest repeats package {name:?}"));
        }
        if required_string(package, "type", "Lake package")? != "git" {
            return Err(format!(
                "trust-v1 requires every Lake dependency to be git-pinned; {name:?} is not"
            ));
        }
        let revision = required_string(package, "rev", "Lake git package")?;
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("Lake package {name:?} lacks an exact 40-hex revision"));
        }
        let checkout = canonical_non_symlink_directory(
            &package_root.join(name),
            "Lake git dependency checkout",
        )?;
        if !checkout.starts_with(&package_root) {
            return Err(format!("Lake package {name:?} escapes packagesDir"));
        }
        let declared_url = required_string(package, "url", "Lake git package")?;
        let origin = command_stdout(&checkout, "git", &["remote", "get-url", "origin"])?;
        if origin.trim().trim_end_matches(".git") != declared_url.trim_end_matches(".git") {
            return Err(format!(
                "Lake package {name:?} origin {} differs from manifest URL {declared_url}",
                origin.trim()
            ));
        }
        let head = command_stdout(&checkout, "git", &["rev-parse", "HEAD"])?;
        if head.trim() != revision {
            return Err(format!(
                "Lake package {name:?} checkout HEAD {} differs from manifest revision {revision}",
                head.trim()
            ));
        }
        let dirty = command_stdout(
            &checkout,
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
        )?;
        if !dirty.is_empty() {
            return Err(format!(
                "Lake package {name:?} checkout is dirty: {}",
                dirty.lines().next().unwrap_or("unknown change")
            ));
        }
        let subdirectory = match package.get("subDir") {
            None | Some(Value::Null) => "",
            Some(Value::String(value)) => value.as_str(),
            Some(_) => return Err(format!("Lake package {name:?} has a malformed subDir")),
        };
        let subdirectory_path = Path::new(subdirectory);
        if subdirectory.contains('\\')
            || subdirectory.chars().any(char::is_control)
            || subdirectory_path.is_absolute()
            || subdirectory_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "Lake package {name:?} subDir is not a normalized relative directory"
            ));
        }
        let config_name = required_string(package, "configFile", "Lake git package")?;
        validate_single_path_component(config_name, "Lake package config file")?;
        let config_relative = if subdirectory.is_empty() {
            config_name.to_owned()
        } else {
            format!("{subdirectory}/{config_name}")
        };
        let config_bytes = read_regular_beneath(
            &checkout,
            &config_relative,
            "Lake package config file",
        )?;
        let nested_manifest_name = package
            .get("manifestFile")
            .and_then(Value::as_str)
            .unwrap_or("lake-manifest.json");
        validate_single_path_component(nested_manifest_name, "nested Lake manifest file")?;
        let nested_manifest_relative = if subdirectory.is_empty() {
            nested_manifest_name.to_owned()
        } else {
            format!("{subdirectory}/{nested_manifest_name}")
        };
        let nested_manifest_path = checkout.join(&nested_manifest_relative);
        let nested_manifest_sha256 = match fs::symlink_metadata(&nested_manifest_path) {
            Ok(_) => Some(raw_sha256(&read_regular_beneath(
                &checkout,
                &nested_manifest_relative,
                "nested Lake dependency manifest",
            )?)),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "stat nested Lake manifest {}: {error}",
                    nested_manifest_path.display()
                ))
            }
        };
        records.push(serde_json::json!({
            "name": name,
            "url": declared_url,
            "revision": revision,
            "checkout_head": head.trim(),
            "checkout_clean": true,
            "subdirectory": if subdirectory.is_empty() { Value::Null } else { Value::String(subdirectory.to_owned()) },
            "config_file": config_name,
            "config_relative_path": config_relative,
            "config_file_sha256": raw_sha256(&config_bytes),
            "nested_manifest_relative_path": nested_manifest_relative,
            "nested_lake_manifest_sha256": nested_manifest_sha256,
        }));
    }
    records.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["name"].as_str().unwrap_or("").as_bytes())
    });
    Ok(records)
}

fn command_stdout(current_dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .current_dir(current_dir)
        .args(args)
        .output()
        .map_err(|error| format!("spawn {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {:?} failed with {}: {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} output is not UTF-8: {error}"))
}

fn validate_lean_version_pin(toolchain_pin: &str, lean_version: &str) -> Result<(), String> {
    let pin = toolchain_pin.trim();
    let expected = pin
        .strip_prefix("leanprover/lean4:v")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("unsupported Lean toolchain pin {pin:?}"))?;
    let reported = lean_version
        .trim()
        .strip_prefix("Lean (version ")
        .and_then(|value| value.split_once(',').map(|(version, _)| version))
        .ok_or_else(|| format!("cannot parse selected Lean version {lean_version:?}"))?;
    if reported != expected {
        return Err(format!(
            "selected Lean version {:?} does not match lean-toolchain {:?}",
            lean_version.trim(),
            pin
        ));
    }
    Ok(())
}

fn command_stdout_path(
    current_dir: &Path,
    program: &Path,
    args: &[&str],
) -> Result<String, String> {
    let output = Command::new(program)
        .current_dir(current_dir)
        .args(args)
        .output()
        .map_err(|error| format!("spawn {}: {error}", program.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} {:?} failed with {}: {}",
            program.display(),
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{} output is not UTF-8: {error}", program.display()))
}

fn resolve_rustup_toolchain_executable(
    rustup: &Path,
    toolchain: &str,
    tool: &str,
    current_dir: &Path,
) -> Result<PathBuf, String> {
    if toolchain.is_empty() || tool.is_empty() || toolchain.contains('/') || tool.contains('/') {
        return Err("rustup toolchain and tool must be non-empty path components".into());
    }
    let output = Command::new(rustup)
        .current_dir(current_dir)
        .args(["which", "--toolchain", toolchain, tool])
        .output()
        .map_err(|error| format!("spawn explicit rustup toolchain lookup: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "rustup could not resolve {tool} for explicit toolchain {toolchain:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let selected = std::str::from_utf8(&output.stdout)
        .map_err(|_| "rustup which output is not UTF-8".to_owned())?
        .trim();
    if selected.is_empty() || selected.contains('\n') {
        return Err("rustup which returned an invalid executable path".into());
    }
    canonical_regular_file(Path::new(selected), "explicit Rust toolchain executable")
}

fn resolve_path_executable(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('/') {
        return Err("executable name must be one PATH component".into());
    }
    let path = std::env::var_os("PATH").ok_or_else(|| "PATH is unset".to_owned())?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if fs::symlink_metadata(&candidate)
            .map(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return fs::canonicalize(&candidate)
                .map_err(|error| format!("resolve PATH executable {}: {error}", candidate.display()));
        }
    }
    Err(format!("required executable {name:?} is absent from PATH"))
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{label} must be a non-symlink regular file: {}",
            path.display()
        ));
    }
    fs::canonicalize(path)
        .map_err(|error| format!("resolve {label} {}: {error}", path.display()))
}

fn canonical_non_symlink_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("stat {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{label} must be a non-symlink directory: {}",
            path.display()
        ));
    }
    fs::canonicalize(path)
        .map_err(|error| format!("resolve {label} {}: {error}", path.display()))
}

fn read_regular_beneath(root: &Path, relative: &str, label: &str) -> Result<Vec<u8>, String> {
    let root = canonical_non_symlink_directory(root, label)?;
    let relative_path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\\')
        || relative.chars().any(char::is_control)
        || relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("{label} path is not a normalized relative path"));
    }
    let mut candidate = root.clone();
    let components: Vec<_> = relative_path.components().collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(piece) = component else {
            unreachable!("relative path components were validated")
        };
        candidate.push(piece);
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("stat {label} {}: {error}", candidate.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{label} path cannot contain symlinks: {}", candidate.display()));
        }
        if index + 1 == components.len() {
            if !metadata.is_file() {
                return Err(format!("{label} must be a regular file: {}", candidate.display()));
            }
        } else if !metadata.is_dir() {
            return Err(format!("{label} parent is not a directory: {}", candidate.display()));
        }
    }
    let resolved = fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve {label} {}: {error}", candidate.display()))?;
    if !resolved.starts_with(&root) {
        return Err(format!("{label} escapes its approved root"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    // Stage-3 fix 7 (cleanup residue): these diagnostics are parameterized
    // on `label` — the retired host-basis-fact subsystem's "basis fact"
    // wording misdirected operators debugging the live Lake config/manifest
    // reader path.
    let mut file = options
        .open(&resolved)
        .map_err(|error| format!("open {label} without following symlinks: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat opened {label}: {error}"))?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(format!("{label} must be a regular file no larger than 1 MiB"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {label}: {error}"))?;
    Ok(bytes)
}

fn read_regular_beneath_with_limit(
    root: &Path,
    relative: &str,
    label: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let root = canonical_non_symlink_directory(root, label)?;
    let relative_path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\\')
        || relative.chars().any(char::is_control)
        || relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("{label} path is not a normalized relative path"));
    }
    let mut candidate = root.clone();
    let components: Vec<_> = relative_path.components().collect();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(piece) = component else {
            unreachable!("relative path components were validated")
        };
        candidate.push(piece);
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("stat {label} {}: {error}", candidate.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("{label} path cannot contain symlinks"));
        }
        if index + 1 == components.len() {
            if !metadata.is_file() || metadata.len() > max_bytes {
                return Err(format!("{label} must be a regular file within its size limit"));
            }
        } else if !metadata.is_dir() {
            return Err(format!("{label} parent is not a directory"));
        }
    }
    let resolved = fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve {label} {}: {error}", candidate.display()))?;
    if !resolved.starts_with(&root) {
        return Err(format!("{label} escapes its approved root"));
    }
    fs::read(resolved).map_err(io("read approved regular file"))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    write_new_with_mode(path, bytes, false)
}

fn write_new_with_mode(path: &Path, bytes: &[u8], private: bool) -> Result<(), String> {
    let parent = containing_directory(path);
    if path.file_name().is_none() {
        return Err(format!("output path has no file name: {}", path.display()));
    }
    if path_entry_exists(path)? {
        return Err(format!("refusing to overwrite existing path {}", path.display()));
    }
    let (temporary_path, mut file) = create_temporary_file(parent, private)?;
    let result = (|| {
        file.write_all(bytes).map_err(io("write temporary output"))?;
        file.sync_all().map_err(io("fsync temporary output"))?;
        drop(file);
        rename_noreplace(&temporary_path, path).map_err(io("publish output without overwrite"))?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_temporary_file(parent: &Path, private: bool) -> Result<(PathBuf, fs::File), String> {
    for _ in 0..64 {
        let path = parent.join(format!(".trellis-write-{}.tmp", random_suffix()?));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if private { 0o600 } else { 0o644 });
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create temporary output: {error}")),
        }
    }
    Err("cannot allocate a unique temporary output path".into())
}

fn random_suffix() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("obtain operating-system randomness: {error}"))?;
    Ok(hex(&bytes))
}

fn path_entry_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect output path {}: {error}", path.display())),
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(ErrorKind::InvalidInput, "source path contains a NUL byte")
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "destination path contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    if source.is_dir() {
        fs::rename(source, destination)
    } else {
        fs::hard_link(source, destination)?;
        fs::remove_file(source)
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .map_err(io("open output directory for fsync"))?
        .sync_all()
        .map_err(io("fsync output directory"))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

struct StagedDirectory {
    target: PathBuf,
    staging: PathBuf,
    published: bool,
}

impl StagedDirectory {
    fn create(target: &Path) -> Result<Self, String> {
        if path_entry_exists(target)? {
            return Err(format!(
                "refusing to overwrite existing output directory {}",
                target.display()
            ));
        }
        let parent = containing_directory(target);
        let file_name = target
            .file_name()
            .ok_or_else(|| format!("output path has no final component: {}", target.display()))?;
        let canonical_parent = fs::canonicalize(parent).map_err(io("resolve output parent"))?;
        let target = canonical_parent.join(file_name);
        if path_entry_exists(&target)? {
            return Err(format!(
                "refusing to overwrite existing output directory {}",
                target.display()
            ));
        }
        for _ in 0..64 {
            let staging = canonical_parent.join(format!(
                ".trellis-directory-{}.staging",
                random_suffix()?
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&staging) {
                Ok(()) => {
                    return Ok(Self {
                        target,
                        staging,
                        published: false,
                    })
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create staging directory: {error}")),
            }
        }
        Err("cannot allocate a unique staging directory".into())
    }

    fn path(&self) -> &Path {
        &self.staging
    }

    fn publish(mut self) -> Result<PathBuf, String> {
        sync_directory_tree(&self.staging)?;
        rename_noreplace(&self.staging, &self.target)
            .map_err(io("publish output directory without overwrite"))?;
        let parent = self
            .target
            .parent()
            .ok_or_else(|| "published output has no parent".to_owned())?;
        sync_directory(parent)?;
        self.published = true;
        Ok(self.target.clone())
    }
}

impl Drop for StagedDirectory {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }
}

fn sync_directory_tree(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(io("read staging directory for fsync"))? {
        let entry = entry.map_err(io("read staging entry for fsync"))?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io("stat staging entry"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "staged output contains a symlink: {}",
                entry.path().display()
            ));
        }
        if metadata.is_dir() {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(root)
}

fn containing_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn collect_regular_files(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    fn walk(
        root: &Path,
        current: &Path,
        output: &mut Vec<(PathBuf, Vec<u8>)>,
    ) -> Result<(), String> {
        let mut entries = fs::read_dir(current)
            .map_err(io("read package artifact directory"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io("read package artifact entry"))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io("stat package artifact"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!("package artifact cannot be a symlink: {}", path.display()));
            }
            if metadata.is_dir() {
                walk(root, &path, output)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| format!("invalid package artifact path: {error}"))?
                    .to_path_buf();
                output.push((
                    relative,
                    fs::read(&path).map_err(io("read package artifact"))?,
                ));
            } else {
                return Err(format!("package artifact is not regular: {}", path.display()));
            }
        }
        Ok(())
    }
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("stat package artifact root {}: {error}", root.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(format!("required package artifact directory is absent: {}", root.display()));
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn slash_path(path: &Path) -> Result<String, String> {
    let mut pieces = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(piece) = component else {
            return Err(format!("package path is not normalized: {}", path.display()));
        };
        let piece = piece
            .to_str()
            .ok_or_else(|| format!("package path is not UTF-8: {}", path.display()))?;
        pieces.push(piece);
    }
    Ok(pieces.join("/"))
}

fn validate_single_path_component(value: &str, label: &str) -> Result<(), String> {
    let mut components = Path::new(value).components();
    let exactly_one_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || !exactly_one_normal
    {
        return Err(format!("{label} is not a safe file stem: {value:?}"));
    }
    Ok(())
}

fn io(action: &'static str) -> impl Fn(std::io::Error) -> String {
    move |error| format!("{action}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn bootstrap_manifest_equality_rejects_goal_binding_byte_drift() {
        let sealed = br#"{"schema":"trellis-phase0-goal-binding-report/v1"}"#;
        assert!(require_exact_phase0_goal_binding_report(sealed, sealed).is_ok());
        let error = require_exact_phase0_goal_binding_report(
            br#"{"schema":"trellis-phase0-goal-binding-report/v1","extra":true}"#,
            sealed,
        )
        .unwrap_err();
        assert!(error.contains("differs byte-for-byte"));
    }

    #[test]
    fn extraction_platform_agreement_rejects_pointer_width_mismatch() {
        check_extraction_platform_agreement(64, 64).expect("matching platforms");
        let error = check_extraction_platform_agreement(64, 32)
            .expect_err("a mismatched Rust extraction target must fail closed");
        eprintln!("{error}");
        assert!(error.contains("Lean System.Platform.numBits is 64"), "{error}");
        assert!(
            error.contains("Rust target pointer width 32"),
            "{error}"
        );
    }

    #[test]
    fn lean_toolchain_pin_matches_exact_reported_version_only() {
        let output = "Lean (version 4.30.0-rc2, x86_64-unknown-linux-gnu, commit 3dc1a088, Release)\n";
        validate_lean_version_pin("leanprover/lean4:v4.30.0-rc2\n", output).unwrap();
        assert!(validate_lean_version_pin(
            "leanprover/lean4:v4.30.0-rc1\n",
            output
        )
        .is_err());
        assert!(validate_lean_version_pin(
            "leanprover/lean4:v4.30.0-rc20\n",
            output
        )
        .is_err());
        assert!(validate_lean_version_pin("v4.30.0-rc2\n", output).is_err());
        assert!(validate_lean_version_pin(
            "leanprover/lean4:v4.30.0-rc2\n",
            "Lean 4.30.0-rc2"
        )
        .is_err());
    }

    #[test]
    fn staged_directory_publishes_whole_and_never_replaces_a_racer() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("keyset");
        let staged = StagedDirectory::create(&target).unwrap();
        let staging_path = staged.path().to_path_buf();
        write_new(&staging_path.join("public.json"), b"public").unwrap();
        write_new(&staging_path.join("private.json"), b"private").unwrap();
        assert!(!target.exists());

        let published = staged.publish().unwrap();
        assert_eq!(published, target);
        assert_eq!(fs::read(target.join("public.json")).unwrap(), b"public");
        assert_eq!(fs::read(target.join("private.json")).unwrap(), b"private");

        let collision = directory.path().join("collision");
        let staged = StagedDirectory::create(&collision).unwrap();
        let abandoned_staging = staged.path().to_path_buf();
        write_new(&abandoned_staging.join("new"), b"new").unwrap();
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("sentinel"), b"old").unwrap();
        assert!(staged.publish().is_err());
        assert_eq!(fs::read(collision.join("sentinel")).unwrap(), b"old");
        assert!(!abandoned_staging.exists());
    }

    #[test]
    fn atomic_file_writes_preserve_existing_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("artifact");
        write_new(&path, b"first").unwrap();
        assert!(write_new(&path, b"second").is_err());
        assert_eq!(fs::read(path).unwrap(), b"first");
        assert!(fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn pinned_inputs_reject_symlinks_before_canonicalization() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("binary");
        fs::write(&binary, b"binary").unwrap();
        let link = directory.path().join("binary-link");
        symlink(&binary, &link).unwrap();

        assert!(canonical_regular_file(&link, "fixture")
            .unwrap_err()
            .contains("non-symlink regular file"));
        assert!(collect_regular_files(&link).is_err());
        assert!(validate_single_path_component("Claim", "target node").is_ok());
        assert!(validate_single_path_component("../../outside", "target node").is_err());
        assert!(validate_single_path_component("nested/Claim", "target node").is_err());
    }

    #[test]
    fn synthetic_toolchain_inputs_form_a_resolvable_evidence_closure() {
        let directory = tempfile::tempdir().unwrap();
        let campaign = directory.path().join("campaign");
        let evidence = directory.path().join("evidence");
        fs::create_dir(&campaign).unwrap();
        fs::create_dir(&evidence).unwrap();
        fs::create_dir(evidence.join("toolchain")).unwrap();
        for (name, bytes) in [
            ("lean-toolchain", b"leanprover/lean4:v4.19.0\n".as_slice()),
            ("lakefile.lean", b"package fixture\n".as_slice()),
            ("lake-manifest.json", b"{}".as_slice()),
            ("APPROVED_AXIOMS.json", b"{\"global\":[],\"nodes\":{}}".as_slice()),
        ] {
            fs::write(campaign.join(name), bytes).unwrap();
        }
        let mut inputs = Vec::new();
        copy_campaign_toolchain_closure(&campaign, &evidence, &mut inputs).unwrap();
        let (_, manifest_bytes) = build_evidence_manifest(&evidence, &inputs).unwrap();
        let closure = verify_evidence_tool_manifest(&evidence, &manifest_bytes).unwrap();
        let toolchain_digest = trellis_kernel::trust_base::raw_sha256(
            b"leanprover/lean4:v4.19.0\n",
        );
        let resolved = closure
            .resolve_leaf_by_digest(&evidence, toolchain_digest)
            .unwrap();

        assert_eq!(resolved.logical_id, "lean-toolchain");
        let lake_manifest = inputs
            .iter()
            .find(|input| input.logical_id == "lake-manifest")
            .unwrap();
        assert_eq!(
            lake_manifest.dependency_ids,
            vec!["lakefile".to_owned(), "lean-toolchain".to_owned()]
        );
    }

    #[test]
    fn lake_dependency_boundary_rejects_source_and_revision_drift() {
        let directory = tempfile::tempdir().unwrap();
        let campaign = directory.path().join("campaign");
        let checkout = campaign.join(".lake/packages/fixture");
        fs::create_dir_all(&checkout).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Trellis Test"],
            vec!["config", "user.email", "trellis@example.invalid"],
            vec!["remote", "add", "origin", "https://example.invalid/fixture.git"],
        ] {
            assert!(Command::new("git")
                .current_dir(&checkout)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        fs::create_dir_all(checkout.join("backends/lean")).unwrap();
        fs::write(
            checkout.join("backends/lean/lakefile.lean"),
            b"package fixture\n",
        )
        .unwrap();
        assert!(Command::new("git")
            .current_dir(&checkout)
            .args(["add", "backends/lean/lakefile.lean"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(&checkout)
            .args(["commit", "-q", "-m", "fixture"])
            .status()
            .unwrap()
            .success());
        let head = command_stdout(&checkout, "git", &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let manifest = serde_json::json!({
            "version": "1.2.0",
            "packagesDir": ".lake/packages",
            "packages": [{
                "type": "git",
                "name": "fixture",
                "url": "https://example.invalid/fixture.git",
                "rev": head,
                "subDir": "backends/lean",
                "configFile": "lakefile.lean",
                "manifestFile": "lake-manifest.json"
            }]
        });
        fs::write(
            campaign.join("lake-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let records = validate_lake_dependency_checkouts(&campaign).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["subdirectory"], "backends/lean");
        assert_eq!(
            records[0]["config_relative_path"],
            "backends/lean/lakefile.lean"
        );

        fs::write(
            checkout.join("backends/lean/lakefile.lean"),
            b"package drifted\n",
        )
        .unwrap();
        assert!(validate_lake_dependency_checkouts(&campaign)
            .unwrap_err()
            .contains("dirty"));

        assert!(Command::new("git")
            .current_dir(&checkout)
            .args(["checkout", "--", "backends/lean/lakefile.lean"])
            .status()
            .unwrap()
            .success());
        let mut escaping_subdir = manifest.clone();
        escaping_subdir["packages"][0]["subDir"] = Value::String("../outside".into());
        fs::write(
            campaign.join("lake-manifest.json"),
            serde_json::to_vec(&escaping_subdir).unwrap(),
        )
        .unwrap();
        assert!(validate_lake_dependency_checkouts(&campaign)
            .unwrap_err()
            .contains("normalized relative directory"));
        let mut wrong_revision = manifest;
        wrong_revision["packages"][0]["rev"] = Value::String("0".repeat(40));
        fs::write(
            campaign.join("lake-manifest.json"),
            serde_json::to_vec(&wrong_revision).unwrap(),
        )
        .unwrap();
        assert!(validate_lake_dependency_checkouts(&campaign)
            .unwrap_err()
            .contains("differs from manifest revision"));
    }

    #[test]
    fn extraction_evidence_closure_rejects_log_and_determinism_drift() {
        let directory = tempfile::tempdir().unwrap();
        let campaign = directory.path().join("campaign");
        let evidence = directory.path().join("evidence");
        fs::create_dir(&campaign).unwrap();
        fs::create_dir(&evidence).unwrap();
        let mut inputs = Vec::new();
        copy_and_validate_extraction_evidence(&campaign, &evidence, &mut inputs).unwrap();
        assert!(inputs
            .iter()
            .any(|input| input.logical_id == "extraction-determinism"));

        fs::write(
            campaign.join("extraction-evidence/STDOUT.bin"),
            b"drift",
        )
        .unwrap();
        let evidence_after_log_drift = directory.path().join("evidence-log-drift");
        fs::create_dir(&evidence_after_log_drift).unwrap();
        assert!(copy_and_validate_extraction_evidence(
            &campaign,
            &evidence_after_log_drift,
            &mut Vec::new(),
        )
        .unwrap_err()
        .contains("stdout_sha256"));

        fs::write(campaign.join("extraction-evidence/STDOUT.bin"), b"").unwrap();
        let determinism_path = campaign.join("extraction-evidence/DETERMINISM.json");
        let mut determinism = parse_json_strict(&fs::read(&determinism_path).unwrap()).unwrap();
        determinism["status"] = Value::String("not_run".into());
        fs::write(&determinism_path, canonical_json_value(&determinism).unwrap()).unwrap();
        let evidence_after_receipt_drift = directory.path().join("evidence-receipt-drift");
        fs::create_dir(&evidence_after_receipt_drift).unwrap();
        assert!(copy_and_validate_extraction_evidence(
            &campaign,
            &evidence_after_receipt_drift,
            &mut Vec::new(),
        )
        .unwrap_err()
        .contains("determinism receipt"));
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct RuntimeReadableTrustBase {
        protocol: String,
        seed_manifest_path: String,
        seed_definition_bundle_path: String,
        evidence_tool_manifest_path: String,
        evidence_tool_root_path: String,
        launch_acknowledgment_path: String,
        seed_transaction_id: String,
        advance_gate_episode_id: String,
    }

    fn write_canonical(path: &Path, value: &Value) {
        fs::write(path, canonical_json_value(value).unwrap()).unwrap();
    }


    /// Record-only relaxes the LIVE comparison only. A frozen distribution
    /// that no longer describes itself — a seed manifest bound to a different
    /// evidence root, a build receipt whose bytes no longer match its pinned
    /// leaf — is corruption, not iteration, and still refuses by name.

    /// Normalize a repository-relative path, resolving `.` and `..` textually.
    /// Returns `None` for a path that escapes the repository root.
    fn normalized_relative(path: &Path) -> Option<String> {
        let mut stack: Vec<String> = Vec::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    stack.pop()?;
                }
                std::path::Component::Normal(part) => {
                    stack.push(part.to_str()?.to_owned());
                }
                _ => return None,
            }
        }
        Some(stack.join("/"))
    }

    /// Every `include_str!("literal")` / `include_bytes!("literal")` /
    /// `include!("literal")` site in one Rust source, as the literal path it
    /// names. Sites whose argument is not a single string literal (the
    /// `concat!` form inside the `schema_source!` macro definition) are left
    /// out; the schema pins those expand to are checked separately, against
    /// the `.schema.json` literals in `schema.rs`.
    fn single_literal_include_paths(source: &str) -> Vec<String> {
        // Line comments are prose, not build inputs — a doc comment that
        // merely names one of these macros must not be read as an include.
        let code = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut paths = Vec::new();
        for name in ["include_str!", "include_bytes!", "include!"] {
            let mut rest = code.as_str();
            while let Some(index) = rest.find(name) {
                rest = &rest[index + name.len()..];
                let Some(tail) = rest.trim_start().strip_prefix('(') else {
                    continue;
                };
                let Some(tail) = tail.trim_start().strip_prefix('"') else {
                    continue;
                };
                let Some(end) = tail.find('"') else { continue };
                paths.push(tail[..end].to_owned());
            }
        }
        paths
    }

    /// The runtime-distribution closure is the reproducible-build receipt AND
    /// the tree the runtime is rebuilt from, so every compile-time include the
    /// kernel pins has to survive the walk. Run the real walker over this
    /// repository's own tree: a synthetic fixture cannot catch an extension
    /// that only the production source tree actually contains, which is
    /// exactly how `kernel/src/trust_base/schemas/` bricked from-genesis
    /// bootstrap.
    ///
    /// Record-only verification does not retire this. What became advisory is
    /// the comparison against the LIVE tree; what the walker decides at
    /// CAPTURE — which bytes are copied into the frozen closure and named by
    /// the build receipt — is unchanged, and a closure missing a compile-time
    /// include is still unrebuildable. The fail-closed tail below keeps its
    /// meaning too: verification now reports the walker's refusal instead of
    /// raising it, but capture must still never fold an undeciding file into
    /// the receipt.
    #[test]
    fn runtime_distribution_closure_carries_every_compile_time_include() {
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the kernel crate sits inside the repository")
            .to_path_buf();
        let sources = collect_runtime_distribution_sources(&repository_root, true)
            .expect("this repository's own source tree must form a runtime distribution closure");

        let mut required = BTreeSet::new();
        let kernel_source_root = repository_root.join("kernel/src");
        for (relative, bytes) in collect_regular_files(&kernel_source_root).unwrap() {
            if relative.extension().and_then(|value| value.to_str()) != Some("rs") {
                continue;
            }
            let source = String::from_utf8(bytes).unwrap();
            let directory = Path::new("kernel/src").join(containing_directory(&relative));
            for literal in single_literal_include_paths(&source) {
                let resolved = normalized_relative(&directory.join(&literal)).unwrap_or_else(|| {
                    panic!("include {literal} in kernel/src/{} escapes the repository", relative.display())
                });
                required.insert(resolved);
            }
        }

        // The schema bodies reach `SCHEMA_SOURCES` through the
        // `schema_source!` macro as well as two longhand `include_str!`
        // sites; every `.schema.json` literal in `schema.rs` names a file
        // under `schemas/v2.3/` regardless of which spelling pinned it.
        let schema_source = fs::read_to_string(
            kernel_source_root.join("trust_base/schema.rs"),
        )
        .unwrap();
        let mut pinned_schemas = BTreeSet::new();
        for fragment in schema_source.split('"').skip(1).step_by(2) {
            if fragment.ends_with(".schema.json") {
                let name = fragment.rsplit('/').next().unwrap().to_owned();
                pinned_schemas.insert(name);
            }
        }
        assert!(
            !pinned_schemas.is_empty(),
            "schema.rs must pin at least one schema body"
        );
        for name in &pinned_schemas {
            required.insert(format!("kernel/src/trust_base/schemas/v2.3/{name}"));
        }

        for relative in &required {
            assert!(
                sources.contains_key(relative),
                "runtime distribution closure omits compile-time include {relative}"
            );
        }

        // Nothing may sit in the pinned schema directory without being pinned:
        // an unpinned file there would be carried into the build receipt
        // without deciding the binary.
        let on_disk: BTreeSet<String> = fs::read_dir(
            kernel_source_root.join("trust_base/schemas/v2.3"),
        )
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .collect();
        assert_eq!(
            on_disk, pinned_schemas,
            "the vendored schema directory and schema.rs's compile-time pins must agree"
        );

        // Fail-closed: an extension that decides nothing must not slip into the
        // build receipt, on either caller.
        let stray = tempfile::tempdir().unwrap();
        fs::create_dir_all(stray.path().join("kernel/src")).unwrap();
        for relative in ["rust-toolchain.toml", "kernel/Cargo.toml", "kernel/Cargo.lock"] {
            fs::write(stray.path().join(relative), b"").unwrap();
        }
        fs::write(stray.path().join("kernel/src/notes.txt"), b"prose").unwrap();
        for require_complete in [false, true] {
            let error = collect_runtime_distribution_sources(stray.path(), require_complete)
                .unwrap_err();
            assert!(
                error.contains("unexpected kernel source member notes.txt"),
                "stray kernel/src member must be refused: {error}"
            );
        }
    }
}
