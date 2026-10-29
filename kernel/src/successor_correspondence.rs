//! Restore historical correspondence baselines at a stopped successor boundary.
//! Preparation authenticates predecessor evidence; replay uses the recorded delta.
use crate::{CorrStatus, NodeId, Phase, ProtocolCommand, ProtocolState, RequestKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restoration {
    pub state_sha256: String,
    pub package_digest: String,
    pub predecessor_state_sha256: String,
    pub history_checkpoint: String,
    pub baselines: BTreeMap<NodeId, String>,
}

pub fn require_idle(state: &ProtocolState) -> Result<(), String> {
    if !state.is_pv() || state.phase != Phase::Modeling || state.modeling_human_approved
        || state.in_flight_request.as_ref().is_some_and(|r| r.kind != RequestKind::Review)
        || state.dispatch.as_ref().is_some_and(|d| d.reviewer.is_some() || d.routing_pending
            || d.tasks.values().any(|t| t.status.occupies_slot() || t.pending_retry.is_some())
            || d.verifiers.occupied_slots() > 0)
    {
        return Err("successor correspondence restoration requires an idle Modeling boundary".into());
    }
    Ok(())
}

/// Current observations can change normally. New judgments, revocations and
/// identity changes must prevent an old approval from being reinstated.
pub fn touched(before: &ProtocolState, after: &ProtocolState) -> BTreeSet<NodeId> {
    before.live.present_nodes.union(&after.live.present_nodes).filter(|node|
        before.live.present_nodes.contains(*node) != after.live.present_nodes.contains(*node)
        || before.node_kinds.get(*node) != after.node_kinds.get(*node)
        || before.node_role.get(*node) != after.node_role.get(*node)
        || before.target_claims.get(*node) != after.target_claims.get(*node)
        || before.challenge_claims.get(*node) != after.challenge_claims.get(*node)
        || before.corr_status.get(*node) != after.corr_status.get(*node)
        || before.corr_approved_fingerprints.get(*node) != after.corr_approved_fingerprints.get(*node)
    ).cloned().collect()
}

impl Restoration {
    pub fn apply(&self, state: &ProtocolState) -> Result<crate::engine::TransitionOutcome, String> {
        require_idle(state)?;
        if self.state_sha256 != crate::dispatch_integration::state_digest(state)?
            || self.baselines.is_empty()
            || [&self.package_digest, &self.predecessor_state_sha256].iter()
                .any(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
            || ![40, 64].contains(&self.history_checkpoint.len())
            || !self.history_checkpoint.bytes().all(|b| b.is_ascii_hexdigit())
            || (state.corr_fingerprint_schema_version != 0
                && state.corr_fingerprint_schema_version != crate::runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION)
        {
            return Err("successor correspondence restoration does not match the captured boundary".into());
        }
        let mut next = state.clone();
        for (node, fingerprint) in &self.baselines {
            if !state.live.present_nodes.contains(node) || fingerprint.is_empty()
                || state.corr_status.contains_key(node) || state.corr_approved_fingerprints.contains_key(node)
            {
                return Err(format!("successor correspondence restoration would overwrite a judgment for {node}"));
            }
            next.corr_status.insert(node.clone(), CorrStatus::Pass);
            next.corr_approved_fingerprints.insert(node.clone(), fingerprint.clone());
        }
        // Preserve actual current observations. No observation is inferred from
        // the restored baseline; ordinary exact fingerprint equality still rules.
        next.corr_fingerprint_schema_version = crate::runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION;
        if let Some(request) = &state.in_flight_request {
            next.in_flight_request = Some(Box::new(next.expected_request(request.id, request.kind)));
        }
        if let Some(dispatch) = next.dispatch.as_mut() { dispatch.signal()?; }
        next.validate()?;
        Ok(crate::engine::TransitionOutcome { state: next, commands: vec![ProtocolCommand::CommitCheckpoint] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restoration_replays_and_never_rebaselines_or_overwrites() {
        let mut state = ProtocolState::default();
        state.pv_tablet_configured = true;
        state.phase = Phase::Modeling;
        state.live.modeling_draft = true;
        state.live.present_nodes.insert("A".into());
        state.node_kinds.insert("A".into(), crate::NodeKind::Proof);
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        state.commit_live();
        // Genesis may have no entry; an explicit Unknown is a newer judgment.
        state.corr_status.remove("A");
        let payload = Restoration {
            state_sha256: crate::dispatch_integration::state_digest(&state).unwrap(),
            package_digest: "a".repeat(64), predecessor_state_sha256: "b".repeat(64),
            history_checkpoint: "c".repeat(40), baselines: BTreeMap::from([("A".into(), "old".into())]),
        };
        let event = crate::ProtocolEvent::RestoreSuccessorCorrespondence { payload: payload.clone() };
        let result = crate::apply_event(state.clone(), event.clone()).unwrap();
        assert!(result.state.current_corr_unknown(&"A".into()));
        assert_eq!(result.state.live, state.live);
        assert_eq!(result.commands, vec![ProtocolCommand::CommitCheckpoint]);
        let replay = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(crate::apply_event(state.clone(), replay).unwrap().state, result.state);
        let mut drift = state.clone();
        drift.cycle += 1;
        assert!(payload.apply(&drift).is_err());
        let mut current = state.clone();
        current.live.corr_current_fingerprints.insert("A".into(), "changed".into());
        let mut fresh = payload.clone();
        fresh.state_sha256 = crate::dispatch_integration::state_digest(&current).unwrap();
        assert!(fresh.apply(&current).unwrap().state.current_corr_unknown(&"A".into()));
        current.live.corr_current_fingerprints.insert("A".into(), "old".into());
        fresh.state_sha256 = crate::dispatch_integration::state_digest(&current).unwrap();
        assert!(fresh.apply(&current).unwrap().state.current_corr_pass(&"A".into()));
        current.corr_status.insert("A".into(), CorrStatus::Unknown);
        fresh.state_sha256 = crate::dispatch_integration::state_digest(&current).unwrap();
        assert!(fresh.apply(&current).is_err());
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Worker)));
        fresh.state_sha256 = crate::dispatch_integration::state_digest(&state).unwrap();
        assert!(fresh.apply(&state).is_err());
    }
    #[test]
    fn history_observations_do_not_hide_judgments_or_revocations() {
        let mut before = ProtocolState::default();
        before.live.present_nodes.insert("A".into());
        let mut after = before.clone();
        after.live.corr_current_fingerprints.insert("A".into(), "observed".into());
        assert!(touched(&before, &after).is_empty());
        after.corr_status.insert("A".into(), CorrStatus::Unknown);
        assert_eq!(touched(&before, &after), BTreeSet::from(["A".into()]));
        assert_eq!(touched(&after, &before), BTreeSet::from(["A".into()]));
        after = before.clone();
        after.challenge_claims.insert("A".into(), BTreeSet::from(["goal".into()]));
        assert_eq!(touched(&before, &after), BTreeSet::from(["A".into()]));
        after = before.clone();
        after.live.present_nodes.clear();
        assert_eq!(touched(&before, &after), BTreeSet::from(["A".into()]));
    }
}
