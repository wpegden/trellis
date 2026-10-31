//! Release resolved verifier request payloads backed by an existing checkpoint.
//! This is representation maintenance, never verifier admission or acceptance.
//! No second archive is written: recovery reads the original committed history.
use crate::dispatch_verifiers::{VerifierIdentity, VerifierJob, VerifierStatus};
use crate::model::{NodeId, ProtocolState, TargetId, WrapperRequest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

const HISTORY: &str = ".trellis-history/supervisor_state.json";
const FORMAT: &str = "trellis-resolved-verifier-request/1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub nodes: BTreeSet<NodeId>,
    pub targets: BTreeSet<TargetId>,
    pub audit_goals: Option<BTreeSet<NodeId>>,
}
impl Summary {
    pub fn from_request(request: &WrapperRequest) -> Self {
        Self { nodes: request.verify_nodes.union(&request.substantiveness_verify_nodes).cloned().collect(),
            targets: request.verify_targets.clone(), audit_goals: request.scoped_audit.as_ref().map(|a| a.goals.clone()) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub format: String,
    pub checkpoint: String,
    pub job_id: u64,
    pub request_sha256: String,
    pub summary: Summary,
}
fn hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Reference {
    pub fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT || self.job_id == 0 || !hex(&self.checkpoint, &[40,64])
            || !hex(&self.request_sha256, &[64]) {
            return Err("invalid resolved verifier request checkpoint reference".into());
        }
        Ok(())
    }
}

/// Constructible only after reading exact Git history. Not accepted from JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    reference: Reference,
    identity: VerifierIdentity,
    status: VerifierStatus,
    request: Box<WrapperRequest>,
}
impl Verified {
    pub(crate) fn matches(&self, reference: &Reference, job: &VerifierJob) -> bool {
        self.reference == *reference && reference.job_id == job.id
            && self.identity == job.identity && self.status == job.status
            && job.request.as_deref() == Some(self.request.as_ref())
    }
}
fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git").arg("--no-replace-objects").arg("-C").arg(repo).args(args).output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() { return Err("resolved verifier checkpoint Git evidence unavailable".into()); }
    Ok(output.stdout)
}
fn ancestor(repo: &Path, commit: &str) -> Result<(), String> {
    if !hex(commit, &[40,64]) { return Err("invalid verifier checkpoint commit".into()); }
    git(repo, &["merge-base", "--is-ancestor", commit, "HEAD"])?;
    Ok(())
}
fn jobs(repo: &Path, commit: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    ancestor(repo, commit)?;
    let raw = git(repo, &["show", &format!("{commit}:{HISTORY}")])?;
    let value = serde_json::from_slice(&raw).map_err(|e| format!("invalid verifier checkpoint JSON: {e}"))?;
    let mut value = crate::shared_state_codec::decode_shared_state(value).map_err(|e| e.to_string())?;
    let jobs = value.pointer_mut("/state/dispatch/verifiers/jobs")
        .ok_or("checkpoint lacks verifier ledger")?.take();
    match jobs { serde_json::Value::Object(map) => Ok(map), _ => Err("invalid checkpoint verifier ledger".into()) }
}
fn historical(commit: &str, id: u64, value: serde_json::Value) -> Result<Option<Verified>, String> {
    let raw_request = value.get("request").filter(|v| !v.is_null());
    let Some(raw_request) = raw_request else { return Ok(None); };
    // Hash the original decoded JSON, not a typed reserialization with newer
    // optional defaults. The original Git object preserves these exact fields.
    let request_sha256 = crate::trusted_artifact_rebind::digest(raw_request);
    let job: VerifierJob = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if job.id != id || !matches!(job.status, VerifierStatus::Resolved { .. }) { return Ok(None); }
    let request = job.request.ok_or("checkpoint verifier request disappeared")?;
    let reference = Reference { format: FORMAT.into(), checkpoint: commit.into(), job_id: id,
        request_sha256, summary: Summary::from_request(&request) };
    reference.validate()?;
    Ok(Some(Verified { reference, identity: job.identity, status: job.status, request }))
}

pub fn eligible(state: &ProtocolState) -> bool {
    !state.pv_revision_ready() && state.phase != crate::Phase::Complete && state.stage != crate::Stage::Complete
        && state.dispatch.as_ref().is_some_and(|d| d.verifiers.jobs.values().any(|j|
            matches!(j.status, VerifierStatus::Resolved { .. }) && j.request.is_some()))
}

pub fn prepare(repo: &Path, state: &ProtocolState) -> Result<Option<crate::dispatch::DispatchEvent>, String> {
    if !eligible(state) { return Ok(None); }
    let commit = String::from_utf8(git(repo, &["rev-parse", "HEAD"])?).map_err(|e| e.to_string())?.trim().to_owned();
    let mut history = jobs(repo, &commit)?;
    let mut requests = BTreeMap::new(); let mut verified = BTreeMap::new();
    for (id, job) in &state.dispatch.as_ref().unwrap().verifiers.jobs {
        if !matches!(job.status, VerifierStatus::Resolved { .. }) || job.request.is_none() { continue; }
        let Some(value) = history.remove(&id.to_string()) else { continue; };
        let Some(proof) = historical(&commit, *id, value)? else { continue; };
        if proof.matches(&proof.reference, job) {
            requests.insert(*id, proof.reference.clone()); verified.insert(*id, proof);
        }
    }
    Ok((!requests.is_empty()).then_some(crate::dispatch::DispatchEvent::VerifierRequestsRetired { requests, verified }))
}

/// Resolve a historical request for diagnosis/reconstruction, never admission.
pub fn resolve(repo: &Path, reference: &Reference) -> Result<Box<WrapperRequest>, String> {
    reference.validate()?;
    let value = jobs(repo, &reference.checkpoint)?.remove(&reference.job_id.to_string())
        .ok_or("verifier checkpoint job missing")?;
    let proof = historical(&reference.checkpoint, reference.job_id, value)?.ok_or("verifier checkpoint request missing")?;
    if proof.reference != *reference { return Err("verifier checkpoint request binding changed".into()); }
    Ok(proof.request)
}

pub fn verify_event(repo: &Path, state: &ProtocolState, event: &mut crate::ProtocolEvent) -> Result<(), String> {
    let crate::ProtocolEvent::Dispatch { payload: crate::dispatch::DispatchEvent::VerifierRequestsRetired { requests, verified } } = event else { return Ok(()); };
    if requests.is_empty() { return Err("empty resolved verifier retirement".into()); }
    let ledger = state.dispatch.as_ref().ok_or("parallel dispatch is not enabled")?;
    let mut by_commit = BTreeMap::<String, Vec<u64>>::new();
    for (id, reference) in requests.iter() {
        reference.validate()?;
        if reference.job_id != *id { return Err("verifier checkpoint reference ID differs".into()); }
        by_commit.entry(reference.checkpoint.clone()).or_default().push(*id);
    }
    for (commit, ids) in by_commit {
        // Internally prepared proofs avoid decoding the same large checkpoint
        // twice; externally supplied/replayed events have no private proofs.
        ancestor(repo, &commit)?;
        let needs_read = ids.iter().any(|id| !ledger.verifiers.jobs.get(id).is_some_and(|job|
            verified.get(id).is_some_and(|p| p.matches(&requests[id], job))));
        if !needs_read { continue; }
        let mut history = jobs(repo, &commit)?;
        for id in ids {
            let job = ledger.verifiers.jobs.get(&id).ok_or("unknown retired verifier")?;
            if job.request.is_none() && job.retired_request.as_ref() == Some(&requests[&id]) { continue; }
            let value = history.remove(&id.to_string()).ok_or("verifier checkpoint job missing")?;
            let proof = historical(&commit, id, value)?.ok_or("verifier checkpoint request missing")?;
            if !proof.matches(&requests[&id], job) { return Err("verifier checkpoint differs from current resolved request".into()); }
            verified.insert(id, proof);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{CompilerCapability, DispatchConfig, DispatchEvent, DispatchLedger};
    use crate::dispatch_verifiers::VerifierLane;
    use crate::ProtocolEvent;

    fn state() -> ProtocolState {
        let mut state = ProtocolState::default();
        let mut ledger = DispatchLedger::new(DispatchConfig::default(), state.phase).unwrap();
        let identity = VerifierIdentity { lane: VerifierLane::Sound, panel_sha256: "panel".into(),
            input_hashes: BTreeMap::from([("A.lean".into(), "a".repeat(64))]), compiler: CompilerCapability::None };
        let id = ledger.verifiers.enqueue(identity, true, false).unwrap();
        let job = ledger.verifiers.jobs.get_mut(&id).unwrap();
        let mut request = WrapperRequest::default();
        request.reviewer_comments = "large exact frozen request λ".repeat(100);
        request.verify_nodes.insert(NodeId::from("A"));
        request.substantiveness_verify_nodes.insert(NodeId::from("B"));
        job.request = Some(Box::new(request));
        job.status = VerifierStatus::Resolved { result_sha256: Some("b".repeat(64)), stale_reason: None };
        state.dispatch = Some(ledger); state
    }
    fn commit(repo: &Path, value: serde_json::Value) -> String {
        std::fs::create_dir_all(repo.join(".trellis-history")).unwrap();
        std::fs::write(repo.join(HISTORY), serde_json::to_vec(&value).unwrap()).unwrap();
        git(repo, &["add", "."]).unwrap();
        git(repo, &["-c","user.name=Test","-c","user.email=test@example.invalid","commit","-m","checkpoint"]).unwrap();
        String::from_utf8(git(repo, &["rev-parse","HEAD"]).unwrap()).unwrap().trim().into()
    }
    fn fixture() -> (tempfile::TempDir, ProtocolState) {
        let repo=tempfile::tempdir().unwrap(); git(repo.path(), &["init","-q"]).unwrap();
        let state=state(); commit(repo.path(),serde_json::json!({"state":state})); (repo,state)
    }
    fn event(repo: &Path,state: &ProtocolState) -> ProtocolEvent {
        ProtocolEvent::Dispatch { payload: prepare(repo,state).unwrap().unwrap() }
    }
    fn apply(state: &mut ProtocolState,event: ProtocolEvent) {
        let ProtocolEvent::Dispatch {payload}=event else {panic!()};
        state.dispatch.as_mut().unwrap().apply(payload).unwrap();
    }
    #[test]
    fn checkpoint_retirement_recovery_summary_identity_and_idempotency() {
        let (repo,mut state)=fixture(); let before=state.clone();
        let old_status=crate::dispatch_coordinator::status(&state).unwrap()["verifiers"].clone();
        let prepared=event(repo.path(),&state);
        apply(&mut state,prepared.clone());
        let job=&state.dispatch.as_ref().unwrap().verifiers.jobs[&1];
        assert!(job.request.is_none());
        assert_eq!(job.identity,before.dispatch.as_ref().unwrap().verifiers.jobs[&1].identity);
        assert_eq!(job.status,before.dispatch.as_ref().unwrap().verifiers.jobs[&1].status);
        assert_eq!(*resolve(repo.path(),job.retired_request.as_ref().unwrap()).unwrap(),
            **before.dispatch.as_ref().unwrap().verifiers.jobs[&1].request.as_ref().unwrap());
        assert_eq!(old_status,crate::dispatch_coordinator::status(&state).unwrap()["verifiers"]);
        assert!(prepare(repo.path(),&state).unwrap().is_none());
        let mut replay:ProtocolEvent=serde_json::from_slice(&serde_json::to_vec(&prepared).unwrap()).unwrap();
        verify_event(repo.path(),&state,&mut replay).unwrap();apply(&mut state,replay);
        let q=&mut state.dispatch.as_mut().unwrap().verifiers;
        let identity=q.jobs[&1].identity.clone();
        assert_eq!(q.enqueue(identity.clone(),false,true).unwrap(),1);
        assert!(q.finish(1,&identity).unwrap());
    }
    #[test]
    fn untrusted_serialized_event_needs_git_proof_and_detects_tampering() {
        let (repo,state)=fixture();let prepared=event(repo.path(),&state);
        let bytes=serde_json::to_vec(&prepared).unwrap();
        let mut replay:ProtocolEvent=serde_json::from_slice(&bytes).unwrap();
        let ProtocolEvent::Dispatch {payload}=replay.clone() else {panic!()};
        let mut ledger=state.dispatch.clone().unwrap();let before=ledger.clone();
        assert!(ledger.apply(payload).is_err());assert_eq!(ledger,before);
        verify_event(repo.path(),&state,&mut replay).unwrap();apply(&mut state.clone(),replay);
        for field in ["request_sha256","summary"] {
            let mut forged:ProtocolEvent=serde_json::from_slice(&bytes).unwrap();
            let ProtocolEvent::Dispatch {payload:DispatchEvent::VerifierRequestsRetired{requests,..}}=&mut forged else {panic!()};
            if field=="request_sha256" {requests.get_mut(&1).unwrap().request_sha256="0".repeat(64)}
            else {requests.get_mut(&1).unwrap().summary.nodes.clear()}
            assert!(verify_event(repo.path(),&state,&mut forged).is_err());
        }
    }
    #[test]
    fn current_active_or_changed_job_is_never_retired() {
        let (repo,state)=fixture();
        for change in 0..4 {
            let mut changed=state.clone();let job=changed.dispatch.as_mut().unwrap().verifiers.jobs.get_mut(&1).unwrap();
            match change {
                0=>job.status=VerifierStatus::Checking{result_sha256:"b".repeat(64)},
                1=>job.identity.panel_sha256="new panel".into(),
                2=>job.request.as_mut().unwrap().reviewer_comments.push('!'),
                _=>job.status=VerifierStatus::Resolved{result_sha256:None,stale_reason:Some("superseded".into())},
            }
            assert!(prepare(repo.path(),&changed).unwrap().is_none());
            let mut replay:ProtocolEvent=serde_json::from_slice(&serde_json::to_vec(&event(repo.path(),&state)).unwrap()).unwrap();
            assert!(verify_event(repo.path(),&changed,&mut replay).is_err());
        }
    }
    #[test]
    fn uncommitted_tail_waits_and_historical_default_fields_recover() {
        let (repo,mut state)=fixture();
        let job=state.dispatch.as_mut().unwrap().verifiers.jobs.get_mut(&1).unwrap();
        job.request.as_mut().unwrap().reviewer_comments.push('!');
        assert!(prepare(repo.path(),&state).unwrap().is_none());
        let mut value=serde_json::json!({"state":state});
        value["state"]["dispatch"]["verifiers"]["jobs"]["1"]["request"].as_object_mut().unwrap().remove("active_node");
        commit(repo.path(),value);
        let prepared=event(repo.path(),&state);apply(&mut state,prepared);
        let job=&state.dispatch.as_ref().unwrap().verifiers.jobs[&1];
        assert!(resolve(repo.path(),job.retired_request.as_ref().unwrap()).unwrap().reviewer_comments.ends_with('!'));
    }
    #[test]
    fn rewind_and_missing_history_refuse_without_mutation() {
        let (repo,state)=fixture();let original=String::from_utf8(git(repo.path(),&["rev-parse","HEAD"]).unwrap()).unwrap();
        let mut newer=state.clone();newer.reviewer_comments="checkpoint2".into();
        commit(repo.path(),serde_json::json!({"state":newer}));
        let mut prepared=event(repo.path(),&newer);
        git(repo.path(),&["reset","--hard",original.trim()]).unwrap();
        assert!(verify_event(repo.path(),&newer,&mut prepared).is_err());
        let empty=tempfile::tempdir().unwrap();git(empty.path(),&["init","-q"]).unwrap();
        commit(empty.path(),serde_json::json!({"state":{}}));
        assert!(prepare(empty.path(),&state).is_err());
    }
    #[test]
    fn maintenance_does_not_route_or_change_last_clean_and_complete_is_frozen() {
        let (repo,state)=fixture();let prepared=event(repo.path(),&state);
        let outcome=crate::engine::apply_event(state.clone(),prepared.clone()).unwrap();
        assert_eq!(outcome.commands,vec![crate::ProtocolCommand::CommitCheckpoint]);
        assert_eq!(outcome.state.last_clean_commit,state.last_clean_commit);
        assert_eq!(outcome.state.in_flight_request,state.in_flight_request);
        assert_eq!(outcome.state.stage,state.stage);
        let mut complete=state.clone();complete.stage=crate::Stage::Complete;
        assert!(!eligible(&complete));assert!(prepare(repo.path(),&complete).unwrap().is_none());
        assert!(crate::engine::apply_event(complete,prepared).is_err());
    }
    #[test]
    fn verifier_retirement_preserves_stale_audit_requeue_and_reused_request_binding() {
        for stale in [false,true] {
            let repo=tempfile::tempdir().unwrap();git(repo.path(), &["init","-q"]).unwrap();
            let mut state=ProtocolState::default();state.phase=crate::Phase::ProofFormalization;
            state.live.present_nodes.insert(NodeId::from("A"));
            state.dispatch=Some(DispatchLedger::new(Default::default(),state.phase).unwrap());
            state.coarse_goals=Some(crate::coarse_goals::CoarseGoals {limit:2,goals:BTreeMap::new(),pending_audits:vec![]});
            state.coarse_goals.as_mut().unwrap().select(&NodeId::from("A"),1).unwrap();
            let intent=crate::coarse_audits::AuditIntent {goals:BTreeSet::from([NodeId::from("A")]),
                reason:"recheck this goal".into(),reviewer_requested:true,retry:1,feedback:"preserve question".into()};
            let inputs=BTreeMap::from([("Tablet/A.lean".into(),"a".repeat(64))]);
            crate::coarse_audits::submit(&mut state,intent.clone()).unwrap();
            crate::coarse_audits::enqueue_ready(&mut state,&inputs).unwrap();
            let job=state.dispatch.as_mut().unwrap().verifiers.jobs.get_mut(&1).unwrap();
            job.status=VerifierStatus::Resolved {result_sha256:Some("b".repeat(64)),
                stale_reason:stale.then(||"source changed".into())};
            let old_identity=job.identity.clone();
            if stale {crate::coarse_audits::requeue_stale(&mut state,1).unwrap();}
            else {crate::coarse_audits::submit(&mut state,intent.clone()).unwrap();}
            assert_eq!(state.coarse_goals.as_ref().unwrap().pending_audits,vec![intent.clone()]);
            commit(repo.path(),serde_json::json!({"state":state}));
            let prepared=event(repo.path(),&state);apply(&mut state,prepared);
            assert_eq!(state.coarse_goals.as_ref().unwrap().pending_audits,vec![intent]);
            assert!(state.dispatch.as_ref().unwrap().verifiers.jobs[&1].retired_request.is_some());
            crate::coarse_audits::enqueue_ready(&mut state,&inputs).unwrap();
            let expected_id=if stale {2}else{1};
            let next=&state.dispatch.as_ref().unwrap().verifiers.jobs[&expected_id];
            assert_eq!(next.identity,old_identity);
            assert!(next.request.is_some());assert!(next.retired_request.is_none());
            state.dispatch.as_ref().unwrap().verifiers.validate(8).unwrap();
        }
    }

}
