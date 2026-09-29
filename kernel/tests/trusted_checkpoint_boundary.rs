#![cfg(feature = "test-support")]
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use trellis_kernel::{
    runtime::test_support::{base_state, set_event_count},
    *,
};

fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
struct RealHook {
    repo: PathBuf,
    source: PathBuf,
}
impl CheckpointSink for RealHook {
    fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
        let mut archived = payload.clone();
        archived.metadata.repo_path = Some(self.repo.clone());
        let dir = self.repo.join(".trellis-history/event-log");
        fs::create_dir_all(&dir).unwrap();
        for path in runtime::event_log_cycle_files(&payload.event_log_dir).unwrap() {
            fs::copy(&path, dir.join(path.file_name().unwrap())).unwrap();
        }
        let mut child = Command::new("python3")
            .args(["-m", "trellis.runtime.git_checkpoint_hook"])
            .env("PYTHONPATH", &self.source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&archived).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
fn replay(root: &Path, seed: &Path, count: u64, output: &Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trellis_runtime_cli"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&json!({"action":"replay_to_event_count",
        "root":root,"stop_after_event_count":count,"seed_checkpoint_path":seed,"dry_run_state_path":output})).unwrap()).unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn real_hook_legacy_boundary_replays_from_earlier_and_original_seeds() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    for args in [
        ["init", "-q"].as_slice(),
        &["config", "user.name", "Fixture"],
        &["config", "user.email", "fixture@example.invalid"],
    ] {
        git(&repo, args);
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut before = base_state();
    before.stage = Stage::Reviewer;
    before.cycle = 4;
    before.request_seq = 1;
    before.corr_fingerprint_schema_version = 4;
    let fingerprint = |name| {
        json!({"own_tex":"text","lean_semantic_closure":name,"preamble_tex":"preamble"}).to_string()
    };
    for value in before.live.corr_current_fingerprints.values_mut() {
        *value = fingerprint("old");
    }
    before.live.target_fingerprints = before.live.corr_current_fingerprints.clone();
    before.corr_approved_fingerprints = before.live.corr_current_fingerprints.clone();
    before.committed = before.live.clone();
    before.in_flight_request = Some(Box::new(before.expected_request(1, RequestKind::Review)));
    let config_path = root.join("config.json");
    let mut config: Value =
        serde_json::from_slice(&fs::read(source.join("examples/trellis.config.json")).unwrap())
            .unwrap();
    config["repo_path"] = json!(repo);
    config["git"]["remote_url"] = Value::Null;
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let runtime_root = root.join("runtime");
    let mut runtime = SupervisorRuntime::initialize_with_metadata(
        RuntimePaths::new(&runtime_root),
        before,
        RuntimeMetadata {
            config_path: Some(config_path),
            ..Default::default()
        },
    )
    .unwrap();
    let earlier = root.join("earlier.json");
    fs::write(
        &earlier,
        json!({"event_count":0,"state":runtime.state()}).to_string(),
    )
    .unwrap();
    set_event_count(&mut runtime, 1);
    let old_dir = runtime::event_log_dir_for(&runtime_root, runtime.metadata());
    fs::create_dir_all(&old_dir).unwrap();
    let prior = EventLogRecord {
        index: 0,
        event: ProtocolEvent::StartCycle,
        commands: vec![],
        cycle: 4,
        phase: runtime.state().phase,
        stage: Stage::Reviewer,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    fs::write(
        old_dir.join("cycle-000004.jsonl"),
        format!("  {}\n", serde_json::to_string(&prior).unwrap()),
    )
    .unwrap();
    runtime
        .step_injected_event_with_checkpoint_sink(
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: 1,
                    cycle: 4,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    reset: ResetChoice::None,
                    next_mode: TaskMode::Global,
                    ..Default::default()
                }),
            },
            &mut RealHook {
                repo: repo.clone(),
                source: source.to_owned(),
            },
        )
        .unwrap();
    let selected_raw = git(
        &repo,
        &["show", "HEAD:.trellis-history/supervisor_state.json"],
    );
    let selected = root.join("selected.json");
    fs::write(&selected, &selected_raw).unwrap();
    let document =
        shared_state_codec::decode_shared_state(serde_json::from_slice(&selected_raw).unwrap())
            .unwrap();
    assert_eq!(document["event_count"], 1);
    assert!(document.get("event_count_convention").is_none());
    let mut state: ProtocolState = serde_json::from_value(document["state"].clone()).unwrap();
    model::recompute_local_closure_reverse_indices(&mut state);
    fs::write(
        runtime_root.join("protocol_state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    fs::write(
        runtime_root.join("runtime_metadata.json"),
        serde_json::to_vec(&document["metadata"]).unwrap(),
    )
    .unwrap();
    fs::write(
        runtime_root.join("checkpoint.json"),
        serde_json::to_vec(&document["checkpoint"]).unwrap(),
    )
    .unwrap();
    let log = repo.join(".trellis-history/event-log/cycle-000004.jsonl");
    let prefix = fs::read(&log).unwrap();
    assert_eq!(prefix.iter().filter(|b| **b == b'\n').count(), 1);
    let mut after = state.clone();
    let mut observed = state.live.clone();
    for value in observed.corr_current_fingerprints.values_mut() {
        *value = fingerprint("new");
    }
    observed.target_fingerprints = observed.corr_current_fingerprints.clone();
    trusted_artifact_rebind::rebind_observed_snapshot(&mut after, observed).unwrap();
    after.committed = after.live.clone();
    let event =
        trusted_artifact_rebind::TrustedArtifactRebind::between(&state, &after, "a".repeat(64))
            .unwrap()
            .compact()
            .unwrap();
    assert!(event.fields.is_empty());
    assert_eq!(event.version, 2);
    let original = fs::read(runtime_root.join("protocol_state.json")).unwrap();
    trusted_rebind_transaction::publish(&runtime_root, &original, &after, event.clone()).unwrap();
    let published = fs::read(&log).unwrap();
    assert!(published.starts_with(&prefix));
    assert_eq!(published.iter().filter(|b| **b == b'\n').count(), 3);
    let records: Vec<EventLogRecord> = String::from_utf8(published.clone())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert!(matches!(
        records[1].event,
        ProtocolEvent::TrustedCheckpointBoundary { .. }
    ));
    assert!(matches!(
        records[2].event,
        ProtocolEvent::TrustedArtifactRebind { .. }
    ));
    assert!(records[1].commands.is_empty() && records[2].commands.is_empty());
    let boundary = match &records[1].event {
        ProtocolEvent::TrustedCheckpointBoundary { payload } => payload,
        _ => unreachable!(),
    };
    for field in [
        "source_commit",
        "checkpoint_raw_sha256",
        "checkpoint_document_sha256",
        "event_prefix_sha256",
        "legacy_event_index",
        "version",
    ] {
        let mut value = serde_json::to_value(boundary).unwrap();
        value[field] = if matches!(field, "legacy_event_index" | "version") {
            json!(99)
        } else {
            json!("f".repeat(if field == "source_commit" { 40 } else { 64 }))
        };
        let bad: trusted_checkpoint_boundary::TrustedCheckpointBoundary =
            serde_json::from_value(value).unwrap();
        assert!(
            bad.validate_context(&repo, 1, &prefix).is_err(),
            "tampered {field} qualified"
        );
    }
    // An internally self-consistent carrier must still reject non-idle scope.
    let mut bad = boundary.clone();
    let mut decoded = shared_state_codec::decode_shared_state(bad.checkpoint.clone()).unwrap();
    decoded["state"]["stage"] = json!("Worker");
    bad.checkpoint_document_sha256 = trusted_artifact_rebind::digest(&decoded);
    bad.checkpoint = trusted_checkpoint_boundary::compact_document(&decoded).unwrap();
    assert!(bad.apply().is_err());
    let local_seed = root.join("local-seed-copy.json");
    fs::copy(runtime_root.join("trusted-rebind-seed.json"), &local_seed).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&local_seed).unwrap()).unwrap()["event_count"],
        2
    );
    fs::remove_file(runtime_root.join("trusted-rebind-seed.json")).unwrap();
    for (index, seed) in [&earlier, &selected, &local_seed].iter().enumerate() {
        let output = root.join(format!("replay-{index}.json"));
        let result = replay(&runtime_root, seed, 3, &output);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let actual: ProtocolState = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(actual, after);
    }
    assert!(trusted_rebind_transaction::publish(&runtime_root, &original, &after, event).is_err());
    trusted_rebind_transaction::recover(&runtime_root).unwrap();
    assert_eq!(fs::read(&log).unwrap(), published);
    // Even a selected checkpoint which skips the anchor must verify its prefix.
    let mut tampered = published.clone();
    tampered[0] = b'\t';
    fs::write(&log, tampered).unwrap();
    assert!(!replay(&runtime_root, &selected, 3, &root.join("bad.json"))
        .status
        .success());
    // JSON escaping cannot hide an anchor from provenance validation.
    let escaped = fs::read_to_string(&log).unwrap().replace(
        "trusted_checkpoint_boundary",
        "trusted_checkpoint_\\u0062oundary",
    );
    fs::write(&log, escaped).unwrap();
    assert!(
        !replay(&runtime_root, &selected, 3, &root.join("escaped-bad.json"))
            .status
            .success()
    );
    fs::write(&log, &published).unwrap();
    // A second move directly from this annotated migration uses one event.
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "migrated checkpoint"]);
    let next_root = root.join("second-runtime");
    fs::create_dir(&next_root).unwrap();
    for name in [
        "protocol_state.json",
        "runtime_metadata.json",
        "checkpoint.json",
    ] {
        fs::copy(runtime_root.join(name), next_root.join(name)).unwrap();
    }
    let next_original = fs::read(next_root.join("protocol_state.json")).unwrap();
    let next_event =
        trusted_artifact_rebind::TrustedArtifactRebind::between(&after, &after, "b".repeat(64))
            .unwrap()
            .compact()
            .unwrap();
    trusted_rebind_transaction::publish(&next_root, &next_original, &after, next_event).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 4);
    // After a subsequent ordinary runtime checkpoint the source is legacy
    // again. Its next remote move needs another boundary, not an extra skip.
    let producer = root.join("later-producer");
    let mut incoming = after.clone();
    incoming.stage = Stage::Reviewer;
    let request = incoming.issue_request(RequestKind::Review);
    let mut later = SupervisorRuntime::initialize_with_metadata(
        RuntimePaths::new(&producer),
        incoming,
        RuntimeMetadata {
            config_path: Some(root.join("config.json")),
            ..Default::default()
        },
    )
    .unwrap();
    set_event_count(&mut later, 4);
    let producer_logs = runtime::event_log_dir_for(&producer, later.metadata());
    fs::create_dir_all(&producer_logs).unwrap();
    fs::copy(&log, producer_logs.join(log.file_name().unwrap())).unwrap();
    later
        .step_injected_event_with_checkpoint_sink(
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(ReviewResponse {
                    request_id: request.id,
                    cycle: request.cycle,
                    status: ResponseStatus::Ok,
                    decision: ReviewDecisionKind::Continue,
                    next_active: Some("a".into()),
                    reset: ResetChoice::None,
                    next_mode: TaskMode::Global,
                    ..Default::default()
                }),
            },
            &mut RealHook {
                repo: repo.clone(),
                source: source.to_owned(),
            },
        )
        .unwrap();
    let third_document = shared_state_codec::decode_shared_state(
        serde_json::from_slice(
            &fs::read(repo.join(".trellis-history/supervisor_state.json")).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(third_document["event_count"], 4);
    let third_seed = root.join("third-selected.json");
    fs::write(&third_seed, serde_json::to_vec(&third_document).unwrap()).unwrap();
    let third_root = root.join("third-runtime");
    fs::create_dir(&third_root).unwrap();
    let mut third: ProtocolState = serde_json::from_value(third_document["state"].clone()).unwrap();
    model::recompute_local_closure_reverse_indices(&mut third);
    for (name, value) in [
        ("protocol_state.json", &third_document["state"]),
        ("runtime_metadata.json", &third_document["metadata"]),
        ("checkpoint.json", &third_document["checkpoint"]),
    ] {
        fs::write(third_root.join(name), serde_json::to_vec(value).unwrap()).unwrap();
    }
    let third_original = fs::read(third_root.join("protocol_state.json")).unwrap();
    let third_event =
        trusted_artifact_rebind::TrustedArtifactRebind::between(&third, &third, "c".repeat(64))
            .unwrap()
            .compact()
            .unwrap();
    trusted_rebind_transaction::publish(&third_root, &third_original, &third, third_event).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 6);
    for seed in [&earlier, &third_seed] {
        let result = replay(&third_root, seed, 6, &root.join("third-replay.json"));
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let actual: ProtocolState =
            serde_json::from_slice(&fs::read(root.join("third-replay.json")).unwrap()).unwrap();
        assert_eq!(actual, third);
    }
}

#[test]
#[ignore = "requires an explicitly copied large research checkpoint; no checker/provider/build"]
fn representative_large_shared_boundary_and_rebind_replay() {
    use sha2::{Digest, Sha256};
    use trusted_artifact_rebind::{digest, TrustedArtifactRebind};
    use trusted_checkpoint_boundary::{compact_document, TrustedCheckpointBoundary};
    let root = PathBuf::from(std::env::var("TRELLIS_RESUME_RESEARCH_COPY").unwrap());
    let output = PathBuf::from(std::env::var("TRELLIS_RESUME_SCALE_OUTPUT").unwrap());
    fs::create_dir_all(&output).unwrap();
    let raw = fs::read(root.join("protocol_state.json")).unwrap();
    let mut before: ProtocolState = serde_json::from_slice(&raw).unwrap();
    model::recompute_local_closure_reverse_indices(&mut before);
    trusted_artifact_rebind::check_scope(&before).unwrap();
    let original_digest = digest(&serde_json::to_value(&before).unwrap());
    let mut after = before.clone();
    let mut changed = 0;
    // Representative byte-different bundle identities in every tier. These
    // synthetic values measure publication/replay, not artifact admission or
    // a real rebuild; the small Lean test supplies the actual certificates.
    for (tier, records) in [
        ("live", &mut after.local_closure_records),
        ("committed", &mut after.committed_local_closure_records),
        ("last_clean", &mut after.last_clean_local_closure_records),
    ] {
        for (owner, record) in records {
            for part in &mut record.node_certificate.as_mut().unwrap().artifact_bundle {
                part.sha256 = serde_json::from_value(json!(format!(
                    "{:x}",
                    Sha256::digest(format!("rebuilt:{tier}:{owner}:{}", part.sha256))
                )))
                .unwrap();
            }
            changed += 1;
        }
    }
    let mut plain = TrustedArtifactRebind::between(&before, &after, "a".repeat(64)).unwrap();
    // Include the full snapshot/approval surfaces too, as a representation
    // change can replace those along with all three rebuilt record maps.
    let mut values = serde_json::to_value(&before).unwrap();
    for name in [
        "live",
        "committed",
        "last_clean_live",
        "corr_approved_fingerprints",
        "last_clean_corr_approved_fingerprints",
    ] {
        let value = values[name].take();
        plain.fields.insert(
            name.into(),
            trusted_artifact_rebind::FieldReplacement {
                before_sha256: digest(&value),
                after: value,
            },
        );
    }
    drop(values);
    let plain_bytes = serde_json::to_vec(&plain).unwrap().len();
    let compact = plain.compact().unwrap();
    let rebind_record = EventLogRecord {
        index: 1,
        event: ProtocolEvent::TrustedArtifactRebind { payload: compact },
        commands: vec![],
        cycle: before.cycle,
        phase: before.phase,
        stage: before.stage,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    let rebind_bytes = serde_json::to_vec(&rebind_record).unwrap();
    let document = json!({"event_count":0,"state":before});
    let document_digest = digest(&document);
    let checkpoint = compact_document(&document).unwrap();
    drop(document);
    let boundary = TrustedCheckpointBoundary {
        version: 1,
        source_commit: "0".repeat(40),
        checkpoint_raw_sha256: "1".repeat(64),
        checkpoint_document_sha256: document_digest,
        event_prefix_sha256: format!("{:x}", Sha256::digest([])),
        legacy_event_index: 0,
        checkpoint,
    };
    let anchor_record = EventLogRecord {
        index: 0,
        event: ProtocolEvent::TrustedCheckpointBoundary { payload: boundary },
        commands: vec![],
        cycle: before.cycle,
        phase: before.phase,
        stage: before.stage,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    let anchor_bytes = serde_json::to_vec(&anchor_record).unwrap();
    fs::write(
        output.join("anchor.jsonl"),
        [anchor_bytes.as_slice(), b"\n"].concat(),
    )
    .unwrap();
    fs::write(
        output.join("rebind.jsonl"),
        [rebind_bytes.as_slice(), b"\n"].concat(),
    )
    .unwrap();
    let metrics = json!({"records_changed":changed,"source_state_bytes":raw.len(),"plain_rebind_bytes":plain_bytes,
        "shared_rebind_event_bytes":rebind_bytes.len(),"shared_boundary_event_bytes":anchor_bytes.len(),
        "new_cycle_bytes":anchor_bytes.len()+rebind_bytes.len()+2,"kind":"representative synthetic bundle identities, original production fingerprints, no artifact admission"});
    assert!(
        anchor_bytes.len() + rebind_bytes.len() + 2 < 100 * 1024 * 1024,
        "{metrics}"
    );
    let restored = apply_event(ProtocolState::default(), anchor_record.event).unwrap();
    assert!(restored.commands.is_empty());
    assert_eq!(
        digest(&serde_json::to_value(&restored.state).unwrap()),
        original_digest
    );
    let migrated = apply_event(restored.state, rebind_record.event).unwrap();
    assert!(migrated.commands.is_empty());
    assert_eq!(migrated.state, after);
    fs::write(
        output.join("metrics.json"),
        serde_json::to_vec_pretty(&metrics).unwrap(),
    )
    .unwrap();
    eprintln!("{metrics}");
}

#[test]
#[ignore = "requires the representative large carrier fixture; no artifact admission"]
fn representative_large_actual_publication() {
    use trusted_artifact_rebind::{digest, FieldReplacement, TrustedArtifactRebind};
    use trusted_checkpoint_boundary::compact_document;
    let output = PathBuf::from(std::env::var("TRELLIS_RESUME_SCALE_OUTPUT").unwrap());
    let anchor: EventLogRecord =
        serde_json::from_slice(&fs::read(output.join("anchor.jsonl")).unwrap()).unwrap();
    let boundary = match anchor.event {
        ProtocolEvent::TrustedCheckpointBoundary { payload } => payload,
        _ => unreachable!(),
    };
    let before = boundary.apply().unwrap();
    assert!(!before.strict_dep_consumers.is_empty());
    let authoritative = serde_json::to_value(&before).unwrap();
    let mut hydrated: ProtocolState = serde_json::from_value(authoritative.clone()).unwrap();
    assert!(hydrated.strict_dep_consumers.is_empty());
    model::recompute_local_closure_reverse_indices(&mut hydrated);
    assert_eq!(serde_json::to_value(&hydrated).unwrap(), authoritative);
    assert_eq!(hydrated, before);
    model::recompute_local_closure_reverse_indices(&mut hydrated);
    assert_eq!(hydrated, before);
    drop(hydrated);
    drop(authoritative);
    let record: EventLogRecord =
        serde_json::from_slice(&fs::read(output.join("rebind.jsonl")).unwrap()).unwrap();
    let payload = match record.event {
        ProtocolEvent::TrustedArtifactRebind { payload } => payload,
        _ => unreachable!(),
    };
    let after = payload.apply(&before).unwrap();
    let repo = output.join("published-repo");
    let root = output.join("published-runtime");
    fs::create_dir(&repo).unwrap();
    fs::create_dir(&root).unwrap();
    for args in [
        ["init", "-q"].as_slice(),
        &["config", "user.name", "Scale Fixture"],
        &["config", "user.email", "fixture@example.invalid"],
    ] {
        git(&repo, args);
    }
    let metadata = RuntimeMetadata {
        repo_path: Some(repo.clone()),
        ..Default::default()
    };
    let checkpoint = RuntimeCheckpoint {
        cycle: before.cycle,
        phase: before.phase,
        gate_kind: before.gate_kind,
        active_node: before.active_node.clone(),
        committed: before.committed.clone(),
        cleanup_unreachable_deletion: None,
    };
    let original = serde_json::to_vec(&before).unwrap();
    fs::write(root.join("protocol_state.json"), &original).unwrap();
    fs::write(
        root.join("runtime_metadata.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("checkpoint.json"),
        serde_json::to_vec(&checkpoint).unwrap(),
    )
    .unwrap();
    let log =
        runtime::event_log_cycle_file(&runtime::event_log_dir_for(&root, &metadata), before.cycle);
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let prior = EventLogRecord {
        index: 0,
        event: ProtocolEvent::StartCycle,
        commands: vec![],
        cycle: before.cycle,
        phase: before.phase,
        stage: before.stage,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: vec![],
    };
    let prefix = format!(" {}\n", serde_json::to_string(&prior).unwrap());
    fs::write(&log, &prefix).unwrap();
    let mut canonical = compact_document(
        &json!({"event_count":1,"state":before,"checkpoint":checkpoint,"metadata":metadata}),
    )
    .unwrap();
    let canonical_path = repo.join(".trellis-history/supervisor_state.json");
    fs::write(&canonical_path, serde_json::to_vec(&canonical).unwrap()).unwrap();
    fs::copy(&canonical_path, output.join("published-selected.json")).unwrap();
    canonical["event_count"] = json!(0);
    fs::write(
        output.join("published-earlier.json"),
        serde_json::to_vec(&canonical).unwrap(),
    )
    .unwrap();
    drop(canonical);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "Original large checkpoint"]);
    trusted_rebind_transaction::publish(&root, &original, &after, payload).unwrap();
    let first_cycle_bytes = fs::metadata(&log).unwrap().len();
    trusted_rebind_transaction::recover(&root).unwrap();
    assert_eq!(fs::metadata(&log).unwrap().len(), first_cycle_bytes);
    // Match the existing remote job's canonical compaction before Git commit.
    let document: Value = serde_json::from_slice(&fs::read(&canonical_path).unwrap()).unwrap();
    fs::write(
        &canonical_path,
        serde_json::to_vec(&compact_document(&document).unwrap()).unwrap(),
    )
    .unwrap();
    drop(document);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "First large migration"]);
    // A repeated move at the same logical cycle uses a full-size field patch
    // at identical values, measuring accumulation without claiming a rebuild.
    let mut repeat = TrustedArtifactRebind::between(&after, &after, "b".repeat(64)).unwrap();
    let mut values = serde_json::to_value(&after).unwrap();
    for name in [
        "live",
        "committed",
        "last_clean_live",
        "corr_approved_fingerprints",
        "last_clean_corr_approved_fingerprints",
        "local_closure_records",
        "committed_local_closure_records",
        "last_clean_local_closure_records",
    ] {
        let value = values[name].take();
        repeat.fields.insert(
            name.into(),
            FieldReplacement {
                before_sha256: digest(&value),
                after: value,
            },
        );
    }
    drop(values);
    let second_original = fs::read(root.join("protocol_state.json")).unwrap();
    trusted_rebind_transaction::publish(&root, &second_original, &after, repeat.compact().unwrap())
        .unwrap();
    let document: Value = serde_json::from_slice(&fs::read(&canonical_path).unwrap()).unwrap();
    fs::write(
        &canonical_path,
        serde_json::to_vec(&compact_document(&document).unwrap()).unwrap(),
    )
    .unwrap();
    drop(document);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "Repeated large migration"]);
    fs::remove_file(root.join("trusted-rebind-seed.json")).unwrap();
    let bytes = fs::read(&log).unwrap();
    assert!(bytes.starts_with(prefix.as_bytes()));
    assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 4);
    let lengths: Vec<_> = bytes
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| s.len())
        .collect();
    let blob_listing = String::from_utf8(git(&repo, &["ls-tree", "-rl", "HEAD"])).unwrap();
    let largest_blob = blob_listing
        .lines()
        .map(|line| {
            line.split_whitespace()
                .nth(3)
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .max()
        .unwrap();
    assert!(largest_blob < 100 * 1024 * 1024);
    for (label, seed) in [
        ("earlier", "published-earlier.json"),
        ("selected", "published-selected.json"),
    ] {
        fs::write(
            output.join(format!("published-{label}-request.json")),
            serde_json::to_vec(&json!({"action":"replay_to_event_count",
            "root":root,"stop_after_event_count":4,"seed_checkpoint_path":output.join(seed),
            "dry_run_state_path":output.join(format!("published-{label}-replayed.json"))}))
            .unwrap(),
        )
        .unwrap();
    }
    let metrics = json!({"first_cycle_bytes":first_cycle_bytes,"repeated_cycle_bytes":bytes.len(),"event_line_bytes":lengths,
        "canonical_shared_bytes":fs::metadata(&canonical_path).unwrap().len(),"largest_tracked_blob_bytes":largest_blob,
        "original_state_bytes":original.len(),"kind":"real publisher, synthetic bundle identities; repeat patch has identical values; no artifact admission"});
    fs::write(
        output.join("published-metrics.json"),
        serde_json::to_vec_pretty(&metrics).unwrap(),
    )
    .unwrap();
    eprintln!("{metrics}");
}
