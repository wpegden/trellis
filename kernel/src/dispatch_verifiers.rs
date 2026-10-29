//! Scheduling identity and freshness for independent verifier jobs. A finished
//! job supplies evidence to the existing panel/acceptance logic, not approval.
use crate::dispatch::{CompilerCapability, ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifierLane {
    Paper,
    Substantiveness,
    Sound,
    Correspondence,
    Audit,
}

impl VerifierLane {
    pub fn permits_compilation(self) -> bool {
        matches!(self, Self::Correspondence | Self::Audit)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierIdentity {
    pub lane: VerifierLane,
    /// Includes configured membership, model bindings and reconciliation
    /// policy. Changing a worker profile never changes this identity.
    pub panel_sha256: String,
    pub input_hashes: BTreeMap<String, String>,
    pub compiler: CompilerCapability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VerifierStatus {
    Queued,
    Running { process: ProcessIdentity, launched_at_ms: u64 },
    /// Completion includes checker use and normalization, and does not release
    /// the slot until the panel process tree has been reaped.
    Checking { result_sha256: String },
    Resolved { result_sha256: Option<String>, stale_reason: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierJob {
    pub id: u64,
    pub identity: VerifierIdentity,
    pub required: bool,
    pub reviewer_requested: bool,
    pub status: VerifierStatus,
    #[serde(default)]
    pub request: Option<Box<crate::model::WrapperRequest>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_request: Option<crate::verifier_history::Reference>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierQueue {
    pub next_id: u64,
    #[serde(deserialize_with = "crate::dispatch::deserialize_id_map")]
    pub jobs: BTreeMap<u64, VerifierJob>,
}

impl Default for VerifierQueue {
    fn default() -> Self { Self { next_id: 1, jobs: BTreeMap::new() } }
}

impl VerifierQueue {
    pub fn enqueue(&mut self, identity: VerifierIdentity, required: bool, reviewer_requested: bool) -> Result<u64, String> {
        if !required && !reviewer_requested { return Err("verifier job has neither a required nor a reviewer-requested origin".into()); }
        if identity.input_hashes.is_empty() || identity.panel_sha256.is_empty() {
            return Err("verifier job requires captured inputs and a panel identity".into());
        }
        if identity.compiler == CompilerCapability::Managed && !identity.lane.permits_compilation() {
            return Err(format!("{:?} verification does not permit compiler access", identity.lane));
        }
        if let Some(job) = self.jobs.values_mut().find(|j| j.identity == identity
            && !matches!(&j.status, VerifierStatus::Resolved { stale_reason: Some(_), .. })) {
            job.required |= required;
            job.reviewer_requested |= reviewer_requested;
            return Ok(job.id);
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or("verifier ID counter exhausted")?;
        self.jobs.insert(id, VerifierJob { id, identity, required, reviewer_requested, status: VerifierStatus::Queued, request: None, retired_request: None });
        Ok(id)
    }

    pub fn occupied_slots(&self) -> usize {
        self.jobs.values().filter(|j| matches!(j.status, VerifierStatus::Running { .. } | VerifierStatus::Checking { .. })).count()
    }

    pub fn next_queued(&self, capacity: usize) -> Option<u64> {
        if self.occupied_slots() >= capacity { return None; }
        self.jobs.values().filter(|j| j.status == VerifierStatus::Queued)
            .min_by_key(|j| (!j.required, !j.reviewer_requested, j.id)).map(|j| j.id)
    }

    pub fn launch(&mut self, id: u64, process: ProcessIdentity, now_ms: u64, capacity: usize) -> Result<(), String> {
        if self.occupied_slots() >= capacity { return Err("all verifier slots are occupied".into()); }
        let job = self.jobs.get_mut(&id).ok_or("unknown verifier job")?;
        if job.status != VerifierStatus::Queued { return Err("verifier job is not queued".into()); }
        if process.pid == 0 || process.boot_id.is_empty() { return Err("invalid verifier process identity".into()); }
        job.status = VerifierStatus::Running { process, launched_at_ms: now_ms };
        Ok(())
    }

    pub fn candidate(&mut self, id: u64, result_sha256: String) -> Result<(), String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown verifier job")?;
        if job.status == (VerifierStatus::Checking { result_sha256: result_sha256.clone() }) { return Ok(()); }
        if !matches!(job.status, VerifierStatus::Running { .. }) { return Err("verifier job is not running".into()); }
        if result_sha256.len() != 64 || !result_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("verifier result requires a SHA-256 identity".into());
        }
        job.status = VerifierStatus::Checking { result_sha256 };
        Ok(())
    }

    /// Called after process cleanup. True permits ordinary acceptance to
    /// examine this evidence; it does not imply a verifier Pass.
    pub fn finish(&mut self, id: u64, current: &VerifierIdentity) -> Result<bool, String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown verifier job")?;
        let result = match &job.status {
            VerifierStatus::Checking { result_sha256 } => result_sha256.clone(),
            VerifierStatus::Resolved { stale_reason, .. } => return Ok(stale_reason.is_none() && &job.identity == current),
            _ => return Err("verifier has no completed evidence to check".into()),
        };
        let stale_reason = if &job.identity == current { None }
            else { Some("verifier inputs or panel policy changed while the job was running".into()) };
        let fresh = stale_reason.is_none();
        job.status = VerifierStatus::Resolved { result_sha256: Some(result), stale_reason };
        Ok(fresh)
    }

    /// Resolve a sealed, reaped result whose admitted target became ineligible.
    /// Native callers authenticate the exact candidate before this transition.
    pub(crate) fn finish_stale(&mut self, id: u64, reason: String) -> Result<(), String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown verifier job")?;
        let VerifierStatus::Checking { result_sha256 } = &job.status else {
            return Err("stale completion requires a sealed verifier result".into());
        };
        if reason.trim().is_empty() { return Err("stale completion requires a reason".into()); }
        job.status = VerifierStatus::Resolved { result_sha256: Some(result_sha256.clone()), stale_reason: Some(reason) };
        Ok(())
    }

    /// Only queued work is discarded here. Running panels must be cancelled
    /// and reaped by their owning host attempt before their slot is released.
    pub fn supersede_queued(&mut self, id: u64, reason: String) -> Result<(), String> {
        let job = self.jobs.get_mut(&id).ok_or("unknown verifier job")?;
        if job.status != VerifierStatus::Queued || reason.trim().is_empty() {
            return Err("superseding requires a queued verifier job and a reason".into());
        }
        job.status = VerifierStatus::Resolved { result_sha256: None, stale_reason: Some(reason) };
        Ok(())
    }

    pub fn validate(&self, capacity: usize) -> Result<(), String> {
        if self.occupied_slots() > capacity { return Err("verifier capacity exceeded".into()); }
        for (id, job) in &self.jobs {
            if id != &job.id || *id >= self.next_id { return Err("invalid verifier job identity".into()); }
            if let Some(reference) = &job.retired_request {
                reference.validate()?;
                if reference.job_id != job.id || job.request.is_some() || !matches!(job.status, VerifierStatus::Resolved { .. }) {
                    return Err("retired verifier request is attached to an active or inline job".into());
                }
            }
            if job.identity.compiler == CompilerCapability::Managed && !job.identity.lane.permits_compilation() {
                return Err("text-only verifier has compiler capability".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(lane: VerifierLane) -> VerifierIdentity {
        VerifierIdentity { lane, panel_sha256: "panel".into(), compiler: CompilerCapability::None,
            input_hashes: BTreeMap::from([("A.tex".into(), "original".into())]) }
    }
    fn process() -> ProcessIdentity { ProcessIdentity { pid: 10, start_ticks: 20, boot_id: "boot".into() } }

    #[test]
    fn explicit_and_automatic_requests_share_exact_evidence_job() {
        let mut q = VerifierQueue::default(); let k = identity(VerifierLane::Sound);
        let a = q.enqueue(k.clone(), true, false).unwrap();
        assert_eq!(q.enqueue(k, false, true).unwrap(), a);
        assert!(q.jobs[&a].required && q.jobs[&a].reviewer_requested);
        assert_eq!(q.jobs.len(), 1);
    }
    #[test]
    fn input_and_panel_changes_cannot_reuse_old_result() {
        for change_panel in [false, true] {
            let mut q = VerifierQueue::default(); let k = identity(VerifierLane::Sound);
            let id = q.enqueue(k.clone(), true, false).unwrap();
            q.launch(id, process(), 10, 1).unwrap(); q.candidate(id, "a".repeat(64)).unwrap();
            let mut next = k.clone();
            if change_panel { next.panel_sha256 = "new policy".into(); }
            else { next.input_hashes.insert("A.tex".into(), "edited".into()); }
            assert!(!q.finish(id, &next).unwrap());
            assert!(matches!(q.jobs[&id].status, VerifierStatus::Resolved { result_sha256: Some(_), stale_reason: Some(_) }));
            assert_ne!(q.enqueue(next, true, false).unwrap(), id);
        }
    }
    #[test]
    fn text_only_lanes_refuse_compiler_capability() {
        let mut q = VerifierQueue::default();
        for lane in [VerifierLane::Paper, VerifierLane::Substantiveness, VerifierLane::Sound] {
            let mut k = identity(lane); k.compiler = CompilerCapability::Managed;
            assert!(q.enqueue(k, true, false).unwrap_err().contains("does not permit compiler"));
        }
        let mut k = identity(VerifierLane::Correspondence); k.compiler = CompilerCapability::Managed;
        assert!(q.enqueue(k, true, false).is_ok());
    }
    #[test]
    fn verifier_capacity_is_separate_and_held_until_finish() {
        let mut q = VerifierQueue::default(); let k = identity(VerifierLane::Sound);
        let a = q.enqueue(k.clone(), true, false).unwrap();
        let b = q.enqueue(identity(VerifierLane::Paper), true, false).unwrap();
        q.launch(a, process(), 10, 1).unwrap();
        assert!(q.next_queued(1).is_none());
        q.candidate(a, "a".repeat(64)).unwrap();
        assert!(q.launch(b, process(), 20, 1).is_err());
        assert!(q.finish(a, &k).unwrap());
        assert_eq!(q.next_queued(1), Some(b));
        q.validate(1).unwrap();
    }
}
