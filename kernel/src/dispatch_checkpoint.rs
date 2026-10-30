//! Reconcile an exclusive worker with a proven metadata-only checkpoint.
//! This preserves the sealed candidate and still requires ordinary acceptance.
use crate::{ProtocolState, ProtocolEvent, ProtocolCommand};
use crate::dispatch::{TaskId, TaskStatus};
use crate::dispatch_routing::exclusive_digest;
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Command};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusiveCheckpointRebind {
    pub task_id: TaskId,
    pub candidate_sha256: String,
    pub previous_commit: String,
    pub checkpoint_commit: String,
    pub previous_digest: String,
    pub contract_sha256: String,
    #[serde(skip)]
    verified: bool,
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git").arg("--no-replace-objects").arg("-C").arg(repo).args(args).output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() { return Err("cannot verify exclusive checkpoint Git evidence".into()); }
    Ok(output.stdout)
}

fn contract_digest(task: &crate::dispatch::WorkerTask, seed: &crate::dispatch_routing::ExclusiveWorker) -> Result<String, String> {
    // Only these fields change as the retained retry is launched and sealed.
    // Bind all remaining attempt context, including the report acceptance installs.
    let mut task = task.clone();
    task.status = TaskStatus::Reserved;
    task.launched_at_ms = None;
    task.process = None;
    Ok(crate::trusted_artifact_rebind::digest(&serde_json::json!({
        "task": task, "seed": seed,
    })))
}

impl ExclusiveCheckpointRebind {
    pub fn prepare(state: &ProtocolState, task_id: TaskId) -> Result<Option<Self>, String> {
        let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
        let seed = ledger.exclusive_workers.get(&task_id).ok_or("task has no exclusive worker seed")?;
        if seed.canonical_sha256 == exclusive_digest(state)? { return Ok(None); }
        let task = ledger.tasks.get(&task_id).ok_or("unknown exclusive worker task")?;
        let TaskStatus::Checking { candidate_sha256 } = &task.status else {
            return Err("exclusive checkpoint recovery requires a sealed checking candidate".into());
        };
        let payload = Self {
            task_id, candidate_sha256: candidate_sha256.clone(),
            previous_commit: seed.state.last_clean_commit.clone().ok_or("exclusive seed has no LastClean commit")?,
            checkpoint_commit: state.last_clean_commit.clone().ok_or("canonical state has no LastClean commit")?,
            previous_digest: seed.canonical_sha256.clone(), contract_sha256: contract_digest(task, seed)?, verified: false,
        };
        payload.check_state(state)?;
        Ok(Some(payload))
    }

    fn check_state(&self, state: &ProtocolState) -> Result<(), String> {
        let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
        let task = ledger.tasks.get(&self.task_id).ok_or("unknown exclusive worker task")?;
        let seed = ledger.exclusive_workers.get(&self.task_id).ok_or("task has no exclusive worker seed")?;
        if state.stage != crate::Stage::Reviewer || state.phase != ledger.phase
            || state.in_flight_request.is_some() || ledger.reviewer.is_some()
            || ledger.verifiers.jobs.values().any(|j| !matches!(j.status, crate::dispatch_verifiers::VerifierStatus::Resolved { .. }))
            || ledger.tasks.values().any(|t| t.id != self.task_id && (t.status.occupies_slot() || t.pending_retry.is_some()))
            || !task.assignment.scope.exclusive || task.retry_of.is_none()
            || task.status != (TaskStatus::Checking { candidate_sha256: self.candidate_sha256.clone() })
            || seed.canonical_sha256 != self.previous_digest
            || contract_digest(task, seed)? != self.contract_sha256
            || seed.state.last_clean_commit.as_deref() != Some(self.previous_commit.as_str())
            || ![Some(self.previous_commit.as_str()), Some(self.checkpoint_commit.as_str())].contains(&state.last_clean_commit.as_deref())
        { return Err("exclusive checkpoint recovery does not match the retained attempt".into()); }
        let mut previous = state.clone_without_dispatch();
        previous.last_clean_commit = Some(self.previous_commit.clone());
        if exclusive_digest(&previous)? != self.previous_digest {
            return Err("exclusive checkpoint recovery would change more than the LastClean commit".into());
        }
        Ok(())
    }

    pub fn verify_git(&mut self, repo: &Path) -> Result<(), String> {
        self.verified = false;
        for commit in [&self.previous_commit, &self.checkpoint_commit] {
            if ![40, 64].contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid exclusive checkpoint commit".into());
            }
        }
        let parents = git(repo, &["rev-list", "--parents", "-n1", &self.checkpoint_commit])?;
        if String::from_utf8_lossy(&parents).split_whitespace().collect::<Vec<_>>()
            != vec![self.checkpoint_commit.as_str(), self.previous_commit.as_str()] {
            return Err("exclusive checkpoint must directly follow the retained LastClean commit".into());
        }
        git(repo, &["merge-base", "--is-ancestor", &self.checkpoint_commit, "HEAD"])?;
        let changed = git(repo, &["diff-tree", "--no-commit-id", "--name-only", "--no-renames", "-r", "-z",
            &self.previous_commit, &self.checkpoint_commit])?;
        for path in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            if path.starts_with(b".trellis-history/") { continue; }
            if path != b"trellis.config.json" {
                return Err("exclusive checkpoint changed files outside runtime history".into());
            }
            // The parallel rollout may have committed the already-disabled
            // legacy sidecar. No other launch-policy change is equivalent.
            let mut old: serde_json::Value = serde_json::from_slice(&git(repo,
                &["show", &format!("{}:trellis.config.json", self.previous_commit)])?).map_err(|e| e.to_string())?;
            let new: serde_json::Value = serde_json::from_slice(&git(repo,
                &["show", &format!("{}:trellis.config.json", self.checkpoint_commit)])?).map_err(|e| e.to_string())?;
            if old.pointer("/sidecar/enabled") != Some(&serde_json::json!(true))
                || new.pointer("/sidecar/enabled") != Some(&serde_json::json!(false)) {
                return Err("exclusive checkpoint changed launch policy".into());
            }
            old["sidecar"]["enabled"] = serde_json::json!(false);
            if old != new { return Err("exclusive checkpoint changed launch policy".into()); }
        }
        let bytes = git(repo, &["show", &format!("{}:.trellis-history/supervisor_state.json", self.checkpoint_commit)])?;
        let mut document = crate::shared_state_codec::decode_shared_state(
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?)?;
        if document["commands"] != serde_json::json!([{"command": "commit_checkpoint"}]) {
            return Err("exclusive checkpoint is not a history checkpoint".into());
        }
        // Verify the archived predecessor against the immutable object tree.
        // The checkpoint can precede its own final event-log line on older runs.
        let mut task: crate::dispatch::WorkerTask = serde_json::from_value(
            document["state"]["dispatch"]["tasks"][self.task_id.to_string()].clone()).map_err(|e| e.to_string())?;
        if let crate::dispatch_history::FrozenRequest::Stored(reference) = &task.assignment.request {
            task.assignment.request = std::sync::Arc::new(serde_json::from_value::<crate::model::WrapperRequest>(
                crate::dispatch_history::read_git_value(repo, &self.checkpoint_commit, reference)?).map_err(|e| e.to_string())?).into();
        }
        let seed: crate::dispatch_routing::ExclusiveWorker = serde_json::from_value(
            document["state"]["dispatch"]["exclusive_workers"][self.task_id.to_string()].clone()).map_err(|e| e.to_string())?;
        if contract_digest(&task, &seed)? != self.contract_sha256 {
            return Err("archive checkpoint differs from the retained worker contract".into());
        }
        let predecessor = task.retry_of.ok_or("checkpoint has no exclusive retry predecessor")?;
        let request = document["state"]["dispatch"]["tasks"][predecessor.to_string()]["assignment"]["request"].clone();
        let crate::dispatch_history::FrozenRequest::Archived(reference) = serde_json::from_value(request).map_err(|e| e.to_string())? else {
            return Err("checkpoint did not archive the retry predecessor".into());
        };
        crate::dispatch_history::read_git_value(repo, &self.checkpoint_commit, &reference)?;
        // The complete non-dispatch projection must be the original digest,
        // including every LastClean mirror, verdict, and source fingerprint.
        document["state"]["dispatch"] = serde_json::Value::Null;
        document["state"]["in_flight_request"] = serde_json::Value::Null;
        let checkpoint: ProtocolState = serde_json::from_value(document["state"].take()).map_err(|e| e.to_string())?;
        if checkpoint.last_clean_commit.as_deref() != Some(self.previous_commit.as_str())
            || exclusive_digest(&checkpoint)? != self.previous_digest {
            return Err("archive checkpoint differs from the retained canonical state".into());
        }
        self.verified = true;
        Ok(())
    }

    pub fn apply(&self, state: &ProtocolState) -> Result<crate::engine::TransitionOutcome, String> {
        if !self.verified { return Err("exclusive checkpoint recovery requires verified Git evidence".into()); }
        self.check_state(state)?;
        let mut next = state.clone();
        // Replay may still carry the old pointer: reproduce the proven sink
        // effect as well as rebinding the private worker's rollback context.
        next.last_clean_commit = Some(self.checkpoint_commit.clone());
        let digest = exclusive_digest(&next)?;
        let seed = next.dispatch.as_mut().unwrap().exclusive_workers.get_mut(&self.task_id).unwrap();
        seed.state.last_clean_commit = Some(self.checkpoint_commit.clone());
        seed.canonical_sha256 = digest;
        next.validate()?;
        // The retained request belongs to its original build. Ordinary
        // acceptance derives and validates a current request without changing
        // that frozen evidence; use the same path across a binary upgrade.
        crate::dispatch_integration::acceptance_state(&next, self.task_id,
            &crate::dispatch::DependencyGraph { edges: next.deps.clone() })?;
        Ok(crate::engine::TransitionOutcome { state: next, commands: vec![ProtocolCommand::CommitCheckpoint] })
    }
}

pub fn verify_event(repo: &Path, event: &mut ProtocolEvent) -> Result<(), String> {
    if let ProtocolEvent::RebindExclusiveCheckpoint { payload } = event { payload.verify_git(repo)?; }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{apply_event, model::*};
    use crate::dispatch::{DependencyGraph, DispatchEvent, ProcessIdentity};
    use crate::dispatch_routing::RoutingEvent;
    use std::{collections::{BTreeMap, BTreeSet}, fs};

    pub(crate) fn retry_fixture(previous_commit: &str) -> ProtocolState {
        let mut state = crate::runtime::test_support::base_state();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 5;
        state.active_node = Some("a".into());
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        state.commit_live();
        state.last_clean_commit = Some(previous_commit.into());
        state.dispatch = Some(crate::dispatch::DispatchLedger::new(Default::default(), state.phase).unwrap());
        state.request_seq = 1;
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Worker)));
        let graph = DependencyGraph { edges: state.live.present_nodes.iter().map(|n| (n.clone(), BTreeSet::new())).collect() };
        let inputs = BTreeMap::from([("Tablet/a.lean".into(), "1".repeat(64))]);
        let mut state = apply_event(state, ProtocolEvent::DispatchRouting { payload: RoutingEvent::AdoptWorker {
            graph: graph.clone(), inputs: inputs.clone(), base_snapshot: "b".repeat(64), now_ms: 1,
        }}).unwrap().state;
        let process = ProcessIdentity { pid: 123, start_ticks: 1, boot_id: "test".into() };
        state.dispatch.as_mut().unwrap().launched(1, process, 2).unwrap();
        state.dispatch.as_mut().unwrap().candidate(1, "c".repeat(64), 3).unwrap();
        let context = crate::dispatch_integration::acceptance_state(&state, 1, &graph).unwrap();
        let payload = crate::dispatch_integration::WorkerPublication { closure_stuck_to_review: false, fresh_checked_inputs: None,
            submitted_outcome: None,
            task_id: 1, candidate_sha256: "c".repeat(64),
            base_state_sha256: crate::dispatch_integration::state_digest(&state).unwrap(), decided_at_ms: 4,
            inputs: inputs.clone(), proof_body_rebase_inputs: BTreeMap::new(),
            actual_scope: state.dispatch.as_ref().unwrap().tasks[&1].assignment.scope.clone(),
            graph_before: graph.clone(), graph_after: graph.clone(),
            response: WorkerResponse { request_id: context.in_flight_request.as_ref().unwrap().id,
                cycle: context.cycle, status: ResponseStatus::Malformed, outcome: WorkerOutcome::Invalid,
                transport_failure: true, ..Default::default() },
        };
        let state = crate::dispatch_integration::fail(&state, &payload).unwrap().state;
        let state = apply_event(state, ProtocolEvent::Dispatch { payload: DispatchEvent::ProcessesReaped { task_id: 1, now_ms: 5 } }).unwrap().state;
        let retry = crate::dispatch_coordinator::prepare_retry(&state, 1, graph, inputs,
            BTreeMap::new(), "d".repeat(64), 6).unwrap().unwrap();
        apply_event(state, ProtocolEvent::Dispatch { payload: retry.event }).unwrap().state
    }

    pub(crate) fn commit(repo: &Path, message: &str) -> String {
        git(repo, &["add", "-A"]).unwrap();
        git(repo, &["commit", "-m", message]).unwrap();
        String::from_utf8(git(repo, &["rev-parse", "HEAD"]).unwrap()).unwrap().trim().into()
    }

    pub(crate) fn fixture(edit: impl FnOnce(&Path, &mut ProtocolState)) -> (tempfile::TempDir, ProtocolState) {
        fixture_with_seed(|_| {}, edit)
    }

    fn fixture_with_seed(edit_seed: impl FnOnce(&mut ProtocolState), edit: impl FnOnce(&Path, &mut ProtocolState)) -> (tempfile::TempDir, ProtocolState) {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]).unwrap();
        git(repo.path(), &["config", "user.name", "test"]).unwrap();
        git(repo.path(), &["config", "user.email", "test@example.invalid"]).unwrap();
        fs::create_dir(repo.path().join("Tablet")).unwrap();
        fs::write(repo.path().join("Tablet/a.lean"), "theorem a : True := by trivial\n").unwrap();
        fs::write(repo.path().join("trellis.config.json"), r#"{"sidecar":{"enabled":true},"worker":"sol"}"#).unwrap();
        let previous = commit(repo.path(), "before archive");
        git(repo.path(), &["tag", "supervisor2/clean-000000"]).unwrap();
        let mut state = retry_fixture(&previous);
        edit_seed(&mut state);
        let payload = crate::dispatch_history::archive_event(repo.path(), &state).unwrap().unwrap();
        let mut state = apply_event(state, ProtocolEvent::Dispatch { payload }).unwrap().state;
        let mut checkpoint = state.clone();
        edit(repo.path(), &mut checkpoint);
        let reference = crate::dispatch_history::publish(repo.path(), checkpoint.dispatch.as_ref().unwrap().tasks[&2].assignment.request.inline().unwrap()).unwrap();
        let mut document = serde_json::json!({"state": checkpoint,
            "commands": [{"command":"commit_checkpoint"}], "event_count": 8});
        document["state"]["dispatch"]["tasks"]["2"]["assignment"]["request"] =
            serde_json::to_value(crate::dispatch_history::FrozenRequest::Stored(reference)).unwrap();
        fs::write(repo.path().join(".trellis-history/supervisor_state.json"),
            serde_json::to_vec(&crate::trusted_checkpoint_boundary::compact_document(&document).unwrap()).unwrap()).unwrap();
        let child = commit(repo.path(), "archive checkpoint");
        git(repo.path(), &["tag", "supervisor2/clean-000008"]).unwrap();
        state.last_clean_commit = Some(child);
        state.dispatch.as_mut().unwrap().launched(2, ProcessIdentity { pid: 234, start_ticks: 1, boot_id: "test".into() }, 7).unwrap();
        state.dispatch.as_mut().unwrap().candidate(2, "e".repeat(64), 8).unwrap();
        (repo, state)
    }

    fn large_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(body).unwrap().join().unwrap();
    }

    #[test]
    fn exclusive_checkpoint_recovery_replays_the_proven_pointer_and_preserves_candidate() {
        large_stack(|| {
            let (repo, before) = fixture(|repo, _| {
                fs::write(repo.join("trellis.config.json"), r#"{"sidecar":{"enabled":false},"worker":"sol"}"#).unwrap();
            });
            let graph = DependencyGraph { edges: before.deps.clone() };
            assert!(crate::dispatch_integration::acceptance_state(&before, 2, &graph).is_err());
            let mut payload = ExclusiveCheckpointRebind::prepare(&before, 2).unwrap().unwrap();
            assert!(payload.apply(&before).is_err());
            payload.verify_git(repo.path()).unwrap();
            let out = apply_event(before.clone(), ProtocolEvent::RebindExclusiveCheckpoint { payload: payload.clone() }).unwrap();
            let mut expected = before.clone();
            let seed = expected.dispatch.as_mut().unwrap().exclusive_workers.get_mut(&2).unwrap();
            seed.state.last_clean_commit = before.last_clean_commit.clone();
            seed.canonical_sha256 = exclusive_digest(&before).unwrap();
            assert_eq!(out.state, expected);
            assert_eq!(out.commands, vec![ProtocolCommand::CommitCheckpoint]);
            assert!(crate::dispatch_integration::acceptance_state(&out.state, 2, &graph).is_ok());
            assert!(ExclusiveCheckpointRebind::prepare(&out.state, 2).unwrap().is_none());
            let wire = serde_json::to_vec(&payload).unwrap();
            let mut replay: ExclusiveCheckpointRebind = serde_json::from_slice(&wire).unwrap();
            assert!(replay.apply(&before).is_err(), "serialized callers cannot forge verification");
            replay.verify_git(repo.path()).unwrap();
            let mut original_pointer = before.clone();
            original_pointer.last_clean_commit = Some(replay.previous_commit.clone());
            assert_eq!(replay.apply(&original_pointer).unwrap().state, out.state);
            assert_eq!(replay.apply(&before).unwrap().state, out.state);
        });
    }

    #[test]
    fn exclusive_checkpoint_recovery_preserves_prior_build_request() {
        large_stack(|| {
            let (repo, before) = fixture_with_seed(|state| {
                let request = state.dispatch.as_mut().unwrap().exclusive_workers.get_mut(&2)
                    .unwrap().state.in_flight_request.as_mut().unwrap();
                request.acceptance_logic_identity = "prior-build".into();
                request.worker_contract = serde_json::json!({"prior_build": true});
            }, |_, _| {});
            let mut payload = ExclusiveCheckpointRebind::prepare(&before, 2).unwrap().unwrap();
            payload.verify_git(repo.path()).unwrap();
            let out = payload.apply(&before).unwrap();
            assert_eq!(out.state.dispatch.as_ref().unwrap().exclusive_workers[&2].state.in_flight_request,
                before.dispatch.as_ref().unwrap().exclusive_workers[&2].state.in_flight_request);
            let acceptance = crate::dispatch_integration::acceptance_state(&out.state, 2,
                &DependencyGraph { edges: out.state.deps.clone() }).unwrap();
            assert_eq!(acceptance.in_flight_request.as_ref().unwrap().acceptance_logic_identity,
                crate::model::acceptance_logic_identity());
            acceptance.validate().unwrap();
        });
    }

    #[test]
    fn exclusive_checkpoint_recovery_rejects_state_contract_and_candidate_changes() {
        large_stack(|| {
            let (repo, before) = fixture(|_, _| {});
            let mut payload = ExclusiveCheckpointRebind::prepare(&before, 2).unwrap().unwrap();
            payload.verify_git(repo.path()).unwrap();
            for kind in 0..9 {
                let mut changed = before.clone();
                match kind {
                    0 => { changed.sound_status.insert("a".into(), SoundStatus::Fail); }
                    1 => { changed.last_clean_live.open_nodes.clear(); }
                    2 => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().status = TaskStatus::Checking { candidate_sha256: "f".repeat(64) }; }
                    3 => { changed.dispatch.as_mut().unwrap().exclusive_workers.get_mut(&2).unwrap().state.reviewer_comments.push_str("different contract"); }
                    4 => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().binding.timeout_seconds += 1; }
                    5 => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().report.as_mut().unwrap().comments.push_str("changed"); }
                    6 => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().pending_retry = Some(Default::default()); }
                    7 => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().retry_refusal = Some("changed".into()); }
                    _ => { changed.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().checker_wait_ms += 1; }
                }
                assert!(payload.apply(&changed).is_err(), "accepted changed case {kind}");
            }
        });
    }

    #[test]
    fn exclusive_checkpoint_recovery_rejects_source_policy_and_checkpoint_changes() {
        large_stack(|| {
            for kind in 0..9 {
                let (repo, state) = fixture(|repo, checkpoint| match kind {
                    0 => fs::write(repo.join("Tablet/a.lean"), "theorem a : False := by sorry\n").unwrap(),
                    1 => fs::write(repo.join("trellis.config.json"), r#"{"sidecar":{"enabled":false},"worker":"other"}"#).unwrap(),
                    2 => { checkpoint.sound_status.insert("a".into(), SoundStatus::Fail); }
                    3 => { checkpoint.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().binding.timeout_seconds += 1; }
                    4 => { checkpoint.dispatch.as_mut().unwrap().exclusive_workers.get_mut(&2).unwrap().state.reviewer_comments.push_str("changed"); }
                    5 => { checkpoint.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().report.as_mut().unwrap().comments.push_str("changed"); }
                    6 => { checkpoint.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().pending_retry = Some(Default::default()); }
                    7 => { checkpoint.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().retry_refusal = Some("changed".into()); }
                    _ => { checkpoint.dispatch.as_mut().unwrap().tasks.get_mut(&2).unwrap().checker_wait_ms += 1; }
                });
                let mut payload = ExclusiveCheckpointRebind::prepare(&state, 2).unwrap().unwrap();
                assert!(payload.verify_git(repo.path()).is_err(), "accepted changed checkpoint case {kind}");
            }
        });
    }
}
