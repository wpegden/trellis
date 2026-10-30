//! Persistent coarse commitments. Selection changes attention, never ownership
//! of an unfinished obligation. Legacy runs have no registry and retain their
//! original singleton transitions and frozen request representation.
use crate::model::{AuditPlan, Blocker, BlockerObject, NodeId, Phase, ProtocolState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus { Active, Complete, Eliminated }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseGoal {
    pub status: GoalStatus,
    pub started_cycle: u32,
    pub completed_attempts: u64,
    pub attempts_since_progress: u64,
    pub last_audit_attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub remaining_open: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub remaining_blockers: Option<usize>,
    /// Only the actionable plan is inline. Historical reports are stored once
    /// in verified history; this must not become a growing list of old plans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<AuditPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_head: Option<crate::coarse_audits::HistoryReference>,
}

impl CoarseGoal {
    fn new(cycle: u32) -> Self {
        Self { status: GoalStatus::Active, started_cycle: cycle, completed_attempts: 0,
            attempts_since_progress: 0, last_audit_attempt: 0, remaining_open: None, remaining_blockers: None, plan: None, history_head: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseGoals {
    pub limit: usize,
    /// At most one small record per fixed coarse milestone, not per attempt.
    pub goals: BTreeMap<NodeId, CoarseGoal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_audits: Vec<crate::coarse_audits::AuditIntent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseGoalView {
    pub completed_attempts: u64,
    pub attempts_since_progress: u64,
    pub last_audit_attempt: u64,
    pub history_head: Option<crate::coarse_audits::HistoryReference>,
    pub plan_tasks: Vec<crate::model::AuditTask>,
    pub plan_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoarseGoalsView {
    pub goals: BTreeMap<NodeId, CoarseGoalView>,
    pub pending_audits: Vec<crate::coarse_audits::AuditIntent>,
    pub limit: usize,
    pub available: usize,
    pub active: BTreeSet<NodeId>,
}

impl CoarseGoals {
    pub fn active(&self) -> BTreeSet<NodeId> {
        self.goals.iter().filter(|(_, g)| g.status == GoalStatus::Active)
            .map(|(n, _)| n.clone()).collect()
    }

    pub fn available(&self) -> usize { self.limit.saturating_sub(self.active().len()) }

    pub fn select(&mut self, node: &NodeId, cycle: u32) -> Result<(), String> {
        match self.goals.get(node) {
            Some(g) if g.status == GoalStatus::Active => return Ok(()),
            Some(_) => return Err(format!("coarse goal {node} is complete or eliminated")),
            None => {}
        }
        if self.available() == 0 {
            return Err("all coarse-goal places are committed; finish an active goal before opening another (unfinished goals cannot be parked)".into());
        }
        self.goals.insert(node.clone(), CoarseGoal::new(cycle));
        Ok(())
    }
}

/// Shallow progress stops BEFORE other coarse boundaries. Full support cones
/// still determine worker permission and mathematical evidence dependencies.
pub fn shallow_region(state: &ProtocolState, anchor: &NodeId) -> BTreeSet<NodeId> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![anchor.clone()];
    while let Some(node) = pending.pop() {
        if !state.live.present_nodes.contains(&node) || !seen.insert(node.clone()) { continue; }
        pending.extend(state.deps.get(&node).into_iter().flatten()
            .filter(|n| !state.coarse_dag_nodes.contains(*n)).cloned());
    }
    seen
}

/// Preserve global approvals; only project which existing blockers concern a
/// region. Deviation and unlocalized target obligations remain conservative.
pub fn relevant_blockers(state: &ProtocolState, region: &BTreeSet<NodeId>) -> BTreeSet<Blocker> {
    filter_blockers(state, region, &state.global_blockers())
}
fn filter_blockers(state: &ProtocolState, region: &BTreeSet<NodeId>, blockers: &BTreeSet<Blocker>) -> BTreeSet<Blocker> {
    blockers.iter().filter(|b| match &b.object {
        BlockerObject::Node { node } => region.contains(node),
        BlockerObject::Target { target } => state.live.coverage.get(target)
            .is_none_or(|nodes| nodes.is_empty() || !nodes.is_disjoint(region)),
        BlockerObject::Deviation { .. } => true,
    }).cloned().collect()
}

impl ProtocolState {
    /// Apply admission in the same order as worker reservations. A rejected
    /// assignment consumes neither worker capacity nor a coarse commitment.
    /// Called both by preparation and by the durable event replay path.
    pub fn coarse_filter_dispatch(&self, event: crate::dispatch::DispatchEvent) -> Result<crate::dispatch::DispatchEvent, String> {
        use crate::dispatch::{DispatchEvent, PreparedItem, WorkerTaskKind};
        let Some(mut goals) = self.coarse_goals.clone() else { return Ok(event) };
        if matches!(event, DispatchEvent::AssignmentsDispatched { .. }) { return Err("multiple coarse goals require the native prepared-batch admission path".into()); }
        let DispatchEvent::BatchPrepared { review_id, response_sha256, items, graph, now_ms } = event else { return Ok(event) };
        let mut shadow = self.dispatch.clone().ok_or("coarse admission requires dispatch")?;
        if shadow.receipts.contains_key(&review_id) {
            return Ok(DispatchEvent::BatchPrepared { review_id, response_sha256, items, graph, now_ms });
        }
        let mut seen = BTreeSet::new();
        let mut filtered = Vec::new();
        for item in items {
            let item = if let PreparedItem::Ready { assignment } = &item {
                let id = assignment.assignment_id.clone();
                if !crate::dispatch::valid_key(&id) || !seen.insert(id.clone()) { item }
                else {
                    let mut proposed = goals.clone();
                    let result = (|| {
                        if crate::coarse_audits::conflicts(self, &assignment.scope)? {
                            return Err("assignment would change evidence reserved for a cone audit; compatible work on other goals may continue".into());
                        }
                        if self.phase == Phase::ProofFormalization && assignment.task_kind == WorkerTaskKind::Ordinary
                            && !assignment.scope.exclusive && !self.coarse_dag_nodes.is_empty() {
                            let node = assignment.request.inline()?.active_coarse_node.as_ref()
                                .ok_or("ordinary proof work requires an active coarse goal")?;
                            if !self.coarse_dag_nodes.contains(node) || !self.live.present_nodes.contains(node) {
                                return Err("assignment names an absent or non-coarse goal".into());
                            }
                            proposed.select(node, self.cycle)?;
                        }
                        shadow.reserve(review_id, assignment.clone(), &graph, now_ms)?;
                        Ok::<(), String>(())
                    })();
                    match result {
                        Ok(()) => { goals = proposed; item },
                        Err(reason) => PreparedItem::Refused { assignment_id: id, reason },
                    }
                }
            } else {
                if let PreparedItem::Refused { assignment_id, .. } = &item { seen.insert(assignment_id.clone()); }
                item
            };
            filtered.push(item);
        }
        Ok(DispatchEvent::BatchPrepared { review_id, response_sha256, items: filtered, graph, now_ms })
    }

    pub fn commit_reserved_coarse_goals(&mut self, review_id: u32) -> Result<(), String> {
        if self.coarse_goals.is_none() || self.phase != Phase::ProofFormalization { return Ok(()) }
        let ledger = self.dispatch.as_ref().ok_or("coarse admission requires dispatch")?;
        let receipt = ledger.receipts.get(&review_id).ok_or("coarse admission has no receipt")?;
        let nodes: Vec<_> = receipt.decisions.iter().filter_map(|d| d.task_id)
            .filter_map(|id| ledger.tasks.get(&id))
            .filter(|t| t.assignment.task_kind.is_ordinary() && !t.assignment.scope.exclusive)
            .map(|t| t.assignment.request.inline().map(|r| r.active_coarse_node.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        for node in nodes.into_iter().flatten() {
            let goals = self.coarse_goals.as_mut().unwrap();
            if !goals.goals.contains_key(&node) { goals.select(&node, self.cycle)?; }
        }
        Ok(())
    }

    pub fn enable_coarse_goals(&mut self, limit: usize) -> Result<(), String> {
        if self.tablet_target != crate::backend::BackendId::Lean {
            return Err("multiple coarse goals currently require Lean; set coarse_goal_slots to null to retain single-goal dispatch on this backend".into());
        }
        if limit == 0 { return Err("coarse-goal capacity must be positive".into()); }
        if let Some(goals) = &self.coarse_goals {
            return if goals.limit == limit { Ok(()) } else {
                Err("coarse goals are already enabled with a different capacity".into())
            };
        }
        let mut goals = CoarseGoals { limit, goals: BTreeMap::new(), pending_audits: Vec::new() };
        if self.phase == Phase::ProofFormalization {
            if let Some(anchor) = &self.active_coarse_node {
                if !self.coarse_dag_nodes.contains(anchor) || !self.live.present_nodes.contains(anchor) {
                    return Err("cannot migrate an absent or non-coarse active goal".into());
                }
                goals.select(anchor, self.cycle)?;
                // Preserve the old plan as provenance; migration never creates
                // a new audit or a new mathematical authorization.
                goals.goals.get_mut(anchor).unwrap().plan = self.audit_plan.clone();
            }
        }
        self.coarse_goals = Some(goals);
        if self.phase == Phase::ProofFormalization && !self.audit_lane_in_flight() {
            self.audit_plan = None;
            self.superseded_audit_plan = None;
            self.stuck_math_audit = Default::default();
        }
        Ok(())
    }

    pub fn coarse_goal_candidates(&self) -> BTreeSet<NodeId> {
        let Some(goals) = &self.coarse_goals else { return BTreeSet::new() };
        if self.phase != Phase::ProofFormalization { return BTreeSet::new(); }
        let closed = crate::model::shallowly_closed_coarse_nodes(&self.live.present_nodes,
            &self.live.open_nodes, &self.deps, &self.coarse_dag_nodes);
        let blockers = self.global_blockers();
        self.coarse_dag_nodes.iter().filter(|node| self.live.present_nodes.contains(*node)
            && (goals.goals.get(*node).is_some_and(|g| g.status == GoalStatus::Active)
                || (goals.available() > 0 && !goals.goals.contains_key(*node) && (!closed.contains(*node) || !filter_blockers(self, &shallow_region(self, node), &blockers).is_empty()))))
            .cloned().collect()
    }

    pub fn coarse_goals_view(&self) -> Option<CoarseGoalsView> {
        self.coarse_goals.as_ref().map(|goals| CoarseGoalsView {
            limit: goals.limit, available: goals.available(), active: goals.active(),
            goals: goals.goals.iter().filter(|(_,g)| g.status == GoalStatus::Active).map(|(n,g)|(n.clone(),CoarseGoalView {
                completed_attempts: g.completed_attempts, attempts_since_progress: g.attempts_since_progress,
                last_audit_attempt: g.last_audit_attempt, history_head: g.history_head.clone(),
                plan_available: g.plan.is_some(),
                plan_tasks: g.plan.as_ref().map(|p|p.tasks.clone()).unwrap_or_default(),
            })).collect(),
            pending_audits: goals.pending_audits.clone(),
        })
    }

    /// Ignore only unrelated verification frontiers for an authorized private
    /// assignment. All canonical statuses and evidence remain present.
    pub fn pending_coarse_task_ready(&self) -> bool {
        if self.dispatch.is_some() || self.phase != Phase::ProofFormalization
            || self.coarse_task_goal.is_none() || self.pending_task.is_none() { return false; }
        let mut nodes = self.current_worker_authorized_nodes();
        nodes.extend(self.active_node.iter().cloned());
        let region = self.dep_closure(&nodes, &self.live.present_nodes, &self.deps);
        self.coarse_prerequisite_failures_for(&region, &nodes, &self.pending_task.as_ref().unwrap().task_blockers).is_empty()
    }

    pub fn coarse_prerequisites_ready(&self, region: &BTreeSet<NodeId>) -> bool {
        self.coarse_prerequisite_failures(region).is_empty()
    }

    pub fn coarse_prerequisite_failures(&self, region: &BTreeSet<NodeId>) -> Vec<String> {
        self.coarse_prerequisite_failures_for(region, &BTreeSet::new(), &BTreeSet::new())
    }

    pub fn coarse_prerequisite_failures_for(&self, region: &BTreeSet<NodeId>,
        writable: &BTreeSet<NodeId>, repairs: &BTreeSet<Blocker>) -> Vec<String> {
        // A declared repair must be able to fix its own failed judgment. This
        // does not waive unknown prerequisites or any canonical approval.
        let repairing = |kind, node: &NodeId| writable.contains(node) && repairs.iter().any(|b|
            b.kind == kind && b.object == (BlockerObject::Node { node: node.clone() }));
        let mut result = Vec::new();
        for target in self.paper_verify_targets().iter().filter(|target|
            self.live.coverage.get(*target).is_none_or(|nodes| nodes.is_empty() || !nodes.is_disjoint(region))) {
            result.push(format!("paper/faithfulness target {target}"));
        }
        for id in self.deviation_verify_ids() { result.push(format!("deviation {id}")); }
        for node in self.substantiveness_verify_nodes().intersection(region) {
            if !repairing(crate::model::BlockerKind::Substantiveness, node) { result.push(format!("substantiveness {node}")); }
        }
        for node in self.corr_verify_nodes().intersection(region) {
            if !repairing(crate::model::BlockerKind::NodeCorr, node) { result.push(format!("correspondence {node}")); }
        }
        result
    }

    pub fn select_coarse_goal(&mut self, node: &NodeId) -> Result<(), String> {
        if self.coarse_goals.is_none() { return Ok(()); }
        if !self.coarse_goal_candidates().contains(node) {
            return Err(format!("coarse goal {node} is not available; unfinished commitments cannot be replaced"));
        }
        self.coarse_goals.as_mut().unwrap().select(node, self.cycle)
    }

    /// Ordinary assignments can write below another coarse boundary. Their
    /// selected commitment lasts until reaping and any recorded retry finish,
    /// even when no owned file lies in that commitment's shallow region.
    pub(crate) fn bound_coarse_task_goals(&self) -> Result<BTreeSet<NodeId>, String> {
        let mut bound = BTreeSet::new();
        let Some(ledger) = &self.dispatch else { return Ok(bound) };
        for task in ledger.tasks.values().filter(|task|
            (task.status.occupies_slot() || task.pending_retry.is_some())
            && task.assignment.task_kind.is_ordinary() && !task.assignment.scope.exclusive) {
            let request = task.assignment.request.inline()?;
            if request.phase != Phase::ProofFormalization { continue; }
            let goal = request.active_coarse_node.as_ref().ok_or("ordinary task has no coarse-goal binding")?;
            if task.assignment.authorized_worker.as_ref().and_then(|a| a.coarse_goal()) != Some(goal) {
                return Err("ordinary task's frozen and authorized coarse goals differ".into());
            }
            let record = self.coarse_goals.as_ref().and_then(|goals| goals.goals.get(goal))
                .ok_or("ordinary task names an unknown coarse commitment")?;
            if record.status != GoalStatus::Eliminated
                && (!self.live.present_nodes.contains(goal) || !self.coarse_dag_nodes.contains(goal)) {
                return Err("ordinary task's coarse commitment is absent or no longer coarse".into());
            }
            bound.insert(goal.clone());
        }
        Ok(bound)
    }

    /// Maintenance changes only the new flag and prematurely completed bound
    /// records. It does not run unrelated progress or pending-audit pruning.
    pub fn activate_coarse_task_retention(&mut self) -> Result<bool, String> {
        let empty_modeling = self.phase == Phase::Modeling
            && self.coarse_goals.as_ref().is_some_and(|goals| goals.goals.is_empty());
        if !(self.phase == Phase::ProofFormalization || empty_modeling)
            || self.stage != crate::model::Stage::Reviewer
            || self.dispatch.is_none() || self.coarse_goals.is_none()
            || self.in_flight_request.as_ref().is_some_and(|r| r.kind != crate::model::RequestKind::Review) {
            return Err("coarse task retention requires a parallel proof reviewer or empty Modeling reviewer boundary".into());
        }
        let bound = self.bound_coarse_task_goals()?;
        if self.coarse_task_retention_enabled { return Ok(false); }
        if self.dispatch.as_ref().unwrap().reviewer.is_some() {
            return Err("finish the pending reviewer before activating coarse task retention".into());
        }
        self.coarse_task_retention_enabled = true;
        for node in bound {
            let goal = self.coarse_goals.as_mut().unwrap().goals.get_mut(&node).unwrap();
            if goal.status == GoalStatus::Complete { goal.status = GoalStatus::Active; }
        }
        Ok(true)
    }

    /// Called after native mathematical transitions. A regression restores its
    /// obligation even when ordinary admission capacity is full; it never evicts
    /// another goal to make room. New admission remains blocked until room exists.
    pub fn reconcile_coarse_goals(&mut self) {
        let Some(goals) = &self.coarse_goals else { return };
        if self.phase != Phase::ProofFormalization { return; }
        let occupied: BTreeSet<_> = self.dispatch.as_ref().into_iter()
            .flat_map(|d| d.tasks.values())
            .filter(|t| t.status.occupies_slot() || t.pending_retry.is_some())
            .flat_map(|t| t.assignment.scope.nodes.iter().cloned()).collect();
        // Invalid bindings are rejected by validate_coarse_goals below; they
        // must never create a new commitment during reconciliation.
        let bound = if self.coarse_task_retention_enabled {
            self.bound_coarse_task_goals().unwrap_or_default()
        } else { BTreeSet::new() };
        let all_blockers = self.global_blockers();
        let next: Vec<_> = goals.goals.iter().map(|(node, goal)| {
            if !self.live.present_nodes.contains(node) {
                return (node.clone(), if goal.status == GoalStatus::Complete { GoalStatus::Eliminated } else { goal.status }, 0, 0);
            }
            if goal.status == GoalStatus::Eliminated {
                // Reusing a removed name does not resurrect its authorization.
                return (node.clone(), GoalStatus::Eliminated, 0, 0);
            }
            let region = shallow_region(self, node);
            let open = region.intersection(&self.live.open_nodes).count();
            let blockers = filter_blockers(self, &region, &all_blockers).len();
            let complete = open == 0 && region.is_disjoint(&occupied) && !bound.contains(node) && blockers == 0;
            (node.clone(), if complete { GoalStatus::Complete } else { GoalStatus::Active }, open, blockers)
        }).collect();
        let goals = self.coarse_goals.as_mut().unwrap();
        for (node, status, open, blockers) in next {
            let goal = goals.goals.get_mut(&node).unwrap();
            if goal.remaining_open.is_some_and(|old| open < old) || goal.remaining_blockers.is_some_and(|old| blockers < old) {
                goal.attempts_since_progress = 0;
            }
            goal.status = status;
            goal.remaining_open = Some(open);
            goal.remaining_blockers = Some(blockers);
        }
        // A worker already in flight can finish or eliminate a goal before
        // its waiting audit captures evidence. The old question is obsolete.
        let active = goals.active();
        goals.pending_audits.retain(|audit| audit.goals.is_subset(&active));
    }

    /// Project attention for one private assignment. Specialized run-wide
    /// audit roles are never transplanted into an ordinary cone task.
    pub fn project_coarse_goal(&mut self, node: &NodeId) -> Result<(), String> {
        let goal = self.coarse_goals.as_ref().and_then(|g| g.goals.get(node))
            .ok_or("unknown coarse commitment")?;
        self.audit_plan = goal.plan.clone();
        self.superseded_audit_plan = None;
        self.stuck_math_audit = crate::model::StuckMathAuditState::default();
        // Per-goal plans are advice, not the global stuck-process latch.
        self.active_coarse_node = Some(node.clone());
        self.coarse_task_goal = Some(node.clone());
        Ok(())
    }

    /// Count actual completed attempts, never reviewer/checkpoint churn. A
    /// shared node's real improvement resets every affected goal's stagnation.
    pub fn record_coarse_attempt(&mut self, before: &ProtocolState, task_id: u64) -> Result<(), String> {
        let Some(old_goals) = &before.coarse_goals else { return Ok(()) };
        let task = before.dispatch.as_ref().and_then(|d| d.tasks.get(&task_id))
            .ok_or("completed attempt has no reservation")?;
        let request = task.assignment.request.inline()?;
        let selected = task.assignment.task_kind.is_ordinary().then_some(request.active_coarse_node.as_ref()).flatten();
        for (node, goal) in &old_goals.goals {
            if goal.status == GoalStatus::Active && !self.live.present_nodes.contains(node) {
                if request.mode != crate::model::TaskMode::CoarseRestructure || !task.assignment.scope.nodes.contains(node) {
                    return Err(format!("removing active coarse goal {node} requires an authorized coarse structural repair"));
                }
                self.coarse_goals.as_mut().unwrap().goals.get_mut(node).unwrap().status = GoalStatus::Eliminated;
            }
        }
        let mut updates = Vec::new();
        for (node, goal) in &old_goals.goals {
            if goal.status != GoalStatus::Active { continue; }
            let region = shallow_region(before, node);
            let closed = region.intersection(&before.live.open_nodes)
                .any(|n| self.live.present_nodes.contains(n) && !self.live.open_nodes.contains(n));
            let old_blockers = relevant_blockers(before, &region);
            let new_blockers = relevant_blockers(self, &region);
            let improved = closed || (new_blockers.len() < old_blockers.len() && new_blockers.is_subset(&old_blockers));
            let attempted = selected == Some(node) || !task.assignment.scope.nodes.is_disjoint(&region);
            updates.push((node.clone(), improved, attempted));
        }
        if let Some(goals) = &mut self.coarse_goals {
            for (node, improved, attempted) in updates {
                let goal = goals.goals.get_mut(&node).unwrap();
                if attempted {
                    goal.completed_attempts = goal.completed_attempts.checked_add(1).ok_or("coarse attempt counter exhausted")?;
                    goal.attempts_since_progress = goal.attempts_since_progress.saturating_add(1);
                }
                if improved { goal.attempts_since_progress = 0; }
            }
        }
        self.reconcile_coarse_goals();
        Ok(())
    }

    pub fn validate_coarse_goals(&self) -> Result<(), String> {
        let Some(goals) = &self.coarse_goals else { return Ok(()) };
        if goals.limit == 0 { return Err("coarse-goal capacity must be positive".into()); }
        if self.coarse_task_retention_enabled { self.bound_coarse_task_goals()?; }
        for (node, goal) in &goals.goals {
            if goal.status != GoalStatus::Eliminated && (!self.coarse_dag_nodes.contains(node) || !self.live.present_nodes.contains(node)) {
                return Err(format!("coarse commitment {node} is not a coarse milestone"));
            }
            if goal.last_audit_attempt > goal.completed_attempts
                || goal.attempts_since_progress > goal.completed_attempts {
                return Err(format!("invalid coarse progress accounting for {node}"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> ProtocolState {
        let mut s = ProtocolState::default();
        s.phase = Phase::ProofFormalization;
        s.live.present_nodes = ["A", "B", "C", "fine", "base"].map(NodeId::from).into();
        s.live.open_nodes = ["A", "B", "C"].map(NodeId::from).into();
        s.coarse_dag_nodes = ["A", "B", "C", "base"].map(NodeId::from).into();
        s.deps = BTreeMap::from([("A".into(), ["fine".into(), "base".into()].into()),
            ("B".into(), ["base".into()].into())]);
        s
    }
    #[test]
    fn commitment_capacity_has_no_stall_escape_or_focus_replacement() {
        let mut s = state(); s.active_coarse_node = Some("A".into());
        s.enable_coarse_goals(2).unwrap(); s.select_coarse_goal(&"B".into()).unwrap();
        s.cycles_in_coarse_repair_mode = u32::MAX;
        assert_eq!(s.coarse_goal_candidates(), ["A".into(), "B".into()].into());
        assert!(s.select_coarse_goal(&"C".into()).is_err());
        s.active_coarse_node = None; // An ordinary cone reset must not free it.
        assert!(s.select_coarse_goal(&"C".into()).is_err());
        s.select_coarse_goal(&"A".into()).unwrap();
        assert_eq!(s.coarse_goals.as_ref().unwrap().active().len(), 2);
    }
    #[test]
    fn shared_support_is_not_exclusive_and_shallow_progress_stops_at_coarse_boundaries() {
        let s = state();
        assert_eq!(shallow_region(&s, &"A".into()), ["A".into(), "fine".into()].into());
        assert_eq!(shallow_region(&s, &"B".into()), ["B".into()].into());
    }
    #[test]
    fn completion_regression_and_authorized_elimination_preserve_commitments() {
        let mut s = state(); s.active_coarse_node = Some("A".into()); s.enable_coarse_goals(2).unwrap();
        s.select_coarse_goal(&"B".into()).unwrap();
        for n in s.live.present_nodes.clone() {
            s.corr_status.insert(n.clone(), crate::model::CorrStatus::Pass);
            s.live.corr_current_fingerprints.insert(n.clone(), "fp".into());
            s.corr_approved_fingerprints.insert(n.clone(), "fp".into());
            s.substantiveness_status.insert(n.clone(), crate::model::SubstantivenessStatus::Pass);
            s.live.substantiveness_current_fingerprints.insert(n.clone(), "sub".into());
            s.substantiveness_approved_fingerprints.insert(n, "sub".into());
        }
        s.live.open_nodes.remove(&NodeId::from("A"));
        s.reconcile_coarse_goals();
        assert_eq!(s.coarse_goals.as_ref().unwrap().goals[&NodeId::from("A")].status, GoalStatus::Complete);
        s.select_coarse_goal(&"C".into()).unwrap();
        s.live.open_nodes.insert("A".into()); s.reconcile_coarse_goals();
        assert_eq!(s.coarse_goals.as_ref().unwrap().active().len(), 3);
        assert_eq!(s.coarse_goals.as_ref().unwrap().available(), 0);
        s.live.present_nodes.remove(&NodeId::from("B")); s.reconcile_coarse_goals();
        assert!(s.validate_coarse_goals().is_err(), "absence cannot silently park unfinished work");
        s.coarse_goals.as_mut().unwrap().goals.get_mut(&NodeId::from("B")).unwrap().status = GoalStatus::Eliminated;
        s.validate_coarse_goals().unwrap();
        s.live.present_nodes.insert("B".into()); s.reconcile_coarse_goals();
        assert!(!s.coarse_goals.as_ref().unwrap().active().contains(&NodeId::from("B")));
    }

    #[test]
    fn absent_feature_preserves_historical_state_encoding() {
        let s = ProtocolState::default();
        assert!(serde_json::to_value(&s).unwrap().get("coarse_goals").is_none());
        let restored: ProtocolState = serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap();
        assert!(restored.coarse_goals.is_none());
    }
}

/// Existing dismissal fields target a named goal as a non-acting action. This
/// changes advisory bookkeeping only; ordinary global plans keep their rules.
pub fn plan_action(request: &crate::model::WrapperRequest, response: &crate::model::ReviewResponse) -> Option<Result<(), String>> {
    use crate::model::*;
    if request.coarse_goals.is_none() || response.next_active_coarse.is_none()
        || (!response.dismiss_audit_plan && response.dismissed_tasks.is_empty()) { return None; }
    Some((|| {
        if request.kind != RequestKind::Review || request.phase != Phase::ProofFormalization
            || response.decision != ReviewDecisionKind::Continue || response.reset != ResetChoice::None {
            return Err("coarse plan dismissal requires a proof-formalization Continue with reset=none".into());
        }
        let allowed = ["request_id", "cycle", "status", "decision", "reset", "next_active_coarse",
            "dismiss_audit_plan", "dismissed_tasks", "comments", "reason", "memory_challenges"];
        let actual = serde_json::to_value(response).map_err(|e|e.to_string())?;
        let defaults = serde_json::to_value(ReviewResponse::default()).unwrap();
        if let Some((key,_)) = actual.as_object().unwrap().iter().find(|(key,value)|
            !allowed.contains(&key.as_str()) && defaults.get(*key) != Some(*value)) {
            return Err(format!("coarse plan dismissal is non-acting; omit {key} and dispatch work separately"));
        }
        if response.status != ResponseStatus::Ok { return Err("malformed coarse plan dismissal".into()); }
        let node = response.next_active_coarse.as_ref().unwrap();
        let goal = request.coarse_goals.as_ref().unwrap().goals.get(node)
            .filter(|g| g.plan_available).ok_or_else(|| format!("active coarse goal {node} has no current audit plan"))?;
        let mut view = request.clone();
        view.coarse_goals = None;
        view.stuck_math_audit.active = true;
        view.audit_plan = Some(AuditPlan { tasks: goal.plan_tasks.clone(), ..Default::default() });
        if let Some(reason) = view.review_response_audit_plan_rejection_reason(response) { return Err(reason); }
        Ok(())
    })())
}
