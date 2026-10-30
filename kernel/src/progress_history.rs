//! Generic per-checkpoint snapshot buffer for "no-progress-window" audit
//! gates.
//!
//! The buffer records, at each `commit_live`, which nodes were *present*
//! and which were *progressed* under some caller-defined predicate.
//! Consumers (today: Sound; tomorrow: Lean-closure) instantiate the
//! `nontrivial_origin` and "is progressed" semantics; the primitive
//! `no_progress_window_eligible` answers the question "did no surviving
//! node go from not-progressed at some origin C' >= k snapshots ago, to
//! progressed at the latest snapshot?".
//!
//! Index semantics: each `push_snapshot` bumps `next_index` (monotonic).
//! The window length `k` is expressed in snapshots, not protocol cycles,
//! so the predicate is robust to test harnesses that drive multiple
//! commits without bumping `state.cycle` and to engine cycle counter
//! drift after rewinds.
//!
//! One-shot debounce: `note_dispatched` marks every currently-buffered
//! snapshot as ineligible window origin, so a single stagnation streak
//! fires the audit exactly once per dispatch (the latch + dispatch
//! cooldown handle re-firing).

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// Hard ceiling on the snapshot buffer length. Sized for the largest
/// reasonable `k` (window length) plus headroom; today both threshold
/// defaults are 5, so 64 covers any realistic env override. The cap
/// bounds disk cost for state files persisted in `protocol_state.json`.
pub const PROGRESS_HISTORY_MAX_SNAPSHOTS: usize = 64;

/// Per-checkpoint snapshot of which nodes were present in the committed
/// view and which of those satisfied the consumer's "progressed"
/// predicate at that checkpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CycleSnapshot {
    /// Monotonic snapshot index assigned at push time. Distinct from
    /// `ProtocolState::cycle` so that test harnesses can drive multiple
    /// `commit_live` calls without bumping `cycle`, and so that
    /// post-rewind cycle drift doesn't corrupt window length math.
    pub snapshot_index: u64,
    /// Nodes present in the committed view at this checkpoint.
    pub present: BTreeSet<NodeId>,
    /// Subset of `present` for which the consumer's "progressed"
    /// predicate held at this checkpoint.
    pub progressed: BTreeSet<NodeId>,
}

/// FIFO buffer of `CycleSnapshot`s plus a one-shot debounce marker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProgressHistory {
    /// Oldest snapshot first; bounded by `PROGRESS_HISTORY_MAX_SNAPSHOTS`.
    pub snapshots: VecDeque<CycleSnapshot>,
    /// Highest `snapshot_index` that was current at the most recent
    /// `note_dispatched` call. Any snapshot whose `snapshot_index <=
    /// last_dispatched_index` is ineligible as a window origin, so a
    /// single stagnation streak fires its audit exactly once.
    pub last_dispatched_index: Option<u64>,
    /// Next `snapshot_index` to assign on push. Starts at 0 and
    /// increments monotonically per push.
    pub next_index: u64,
}

impl ProgressHistory {
    /// Append a new snapshot, popping the oldest if the buffer exceeds
    /// the cap. The pushed snapshot's `snapshot_index` is `next_index`.
    pub fn push_snapshot(&mut self, present: BTreeSet<NodeId>, progressed: BTreeSet<NodeId>) {
        let snapshot = CycleSnapshot {
            snapshot_index: self.next_index,
            present,
            progressed,
        };
        self.next_index = self.next_index.saturating_add(1);
        self.snapshots.push_back(snapshot);
        while self.snapshots.len() > PROGRESS_HISTORY_MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
    }

    /// Mark the current window-origin set as ineligible. Called from
    /// the audit-dispatch site so the same stagnation streak does not
    /// re-fire the gate on subsequent checkpoints.
    pub fn note_dispatched(&mut self) {
        let latest = self.snapshots.back().map(|s| s.snapshot_index);
        // If there's no latest, there's nothing to gate against either.
        if let Some(idx) = latest {
            self.last_dispatched_index = Some(idx);
        }
    }

    /// Drop all buffered snapshots and the debounce marker. Used by
    /// rewinds (`apply_last_clean_reset`) so the post-rewind state
    /// starts its progress history clean.
    pub fn clear(&mut self) {
        self.snapshots.clear();
        self.last_dispatched_index = None;
        self.next_index = 0;
    }

    /// Latest snapshot (newest), if any.
    pub fn latest(&self) -> Option<&CycleSnapshot> {
        self.snapshots.back()
    }
}

/// Generic no-progress-window predicate.
///
/// Returns true iff there exists some snapshot S in `history` that:
///   * is at least `k` snapshots older than `history.latest()` (i.e.
///     `latest.snapshot_index - S.snapshot_index >= k`);
///   * post-dates the most recent `note_dispatched` marker;
///   * satisfies `nontrivial_origin(S)` (caller-defined "this origin
///     describes a real stagnation candidate", e.g. at least one node
///     was unprogressed at S); AND
///   * for every node n in `S.present ∩ latest.present`, n was NOT
///     "progressed at latest while unprogressed at S" — i.e. no
///     surviving node made the unprogressed→progressed transition over
///     the window.
///
/// `k == 0` is treated as "no window required" — the latest snapshot
/// itself is a candidate origin (degenerate case, useful for tests).
pub fn no_progress_window_eligible<F>(
    history: &ProgressHistory,
    k: u32,
    mut nontrivial_origin: F,
) -> bool
where
    F: FnMut(&CycleSnapshot) -> bool,
{
    let Some(latest) = history.latest() else {
        return false;
    };
    let k = u64::from(k);
    // Lower bound on eligible origin index: 0 when no prior dispatch,
    // else (last_dispatched_index + 1) so snapshots from the previous
    // streak (including the dispatch checkpoint itself) are excluded.
    let dispatch_floor = match history.last_dispatched_index {
        Some(d) => d.saturating_add(1),
        None => 0,
    };
    for origin in history.snapshots.iter() {
        if origin.snapshot_index < dispatch_floor {
            continue;
        }
        if latest.snapshot_index.saturating_sub(origin.snapshot_index) < k {
            // Origin is too recent: window not yet wide enough.
            // Snapshots are oldest-first, so younger ones follow.
            break;
        }
        if !nontrivial_origin(origin) {
            continue;
        }
        let any_progressed = origin
            .present
            .intersection(&latest.present)
            .any(|n| !origin.progressed.contains(n) && latest.progressed.contains(n));
        if !any_progressed {
            return true;
        }
    }
    false
}

/// Auditor-facing depth: of all snapshots that would qualify as a
/// no-progress-window origin (per `no_progress_window_eligible`'s
/// definition with the same `nontrivial_origin` predicate), return the
/// age (in snapshots) of the OLDEST one — i.e. how far back the
/// stagnation actually extends. Returns 0 if no snapshot qualifies.
///
/// Reported in `WrapperRequest` so the auditor can tell "the gate
/// fired at k=5 but the stagnation actually goes back 9 snapshots"
/// versus "the gate just barely fired at exactly k".
pub fn oldest_no_progress_window_depth<F>(
    history: &ProgressHistory,
    k: u32,
    mut nontrivial_origin: F,
) -> u32
where
    F: FnMut(&CycleSnapshot) -> bool,
{
    let Some(latest) = history.latest() else {
        return 0;
    };
    let k = u64::from(k);
    let dispatch_floor = match history.last_dispatched_index {
        Some(d) => d.saturating_add(1),
        None => 0,
    };
    let mut best: u64 = 0;
    for origin in history.snapshots.iter() {
        if origin.snapshot_index < dispatch_floor {
            continue;
        }
        let depth = latest.snapshot_index.saturating_sub(origin.snapshot_index);
        if depth < k {
            // Snapshots are oldest-first; everything past here is even
            // younger and so even further below the window length.
            break;
        }
        if !nontrivial_origin(origin) {
            continue;
        }
        let any_progressed = origin
            .present
            .intersection(&latest.present)
            .any(|n| !origin.progressed.contains(n) && latest.progressed.contains(n));
        if !any_progressed && depth > best {
            best = depth;
        }
    }
    u32::try_from(best).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<I: AsRef<str>>(items: &[I]) -> BTreeSet<NodeId> {
        items.iter().map(|s| NodeId::from(s.as_ref())).collect()
    }

    /// `nontrivial_origin = always true` — useful sanity-check that the
    /// window arithmetic is right.
    fn always(_s: &CycleSnapshot) -> bool {
        true
    }

    #[test]
    fn empty_history_is_not_eligible() {
        let history = ProgressHistory::default();
        assert!(!no_progress_window_eligible(&history, 4, always));
        assert_eq!(oldest_no_progress_window_depth(&history, 4, always), 0);
    }

    #[test]
    fn fewer_than_k_snapshots_is_not_eligible() {
        let mut history = ProgressHistory::default();
        for _ in 0..4 {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        // k=4 requires latest.index - origin.index >= 4, i.e. origin
        // <= 0 (with 4 snapshots indexed 0..=3, latest=3). origin=0 has
        // depth 3 < 4. So nothing qualifies.
        assert!(!no_progress_window_eligible(&history, 4, always));
    }

    #[test]
    fn k_plus_one_snapshots_with_no_progress_is_eligible() {
        let mut history = ProgressHistory::default();
        for _ in 0..5 {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        // 5 snapshots, indices 0..=4. Origin=0 has depth 4, which is
        // >= k=4. Surviving = {a}. Origin: a unprogressed. Latest: a
        // unprogressed. No progress; eligible.
        assert!(no_progress_window_eligible(&history, 4, always));
        assert_eq!(oldest_no_progress_window_depth(&history, 4, always), 4);
    }

    #[test]
    fn progressed_at_latest_blocks_eligibility() {
        let mut history = ProgressHistory::default();
        for _ in 0..4 {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        history.push_snapshot(set(&["a"]), set(&["a"]));
        // Surviving = {a}. Origin=0: a unprogressed. Latest: a
        // progressed. The transition unprog->prog blocks eligibility.
        assert!(!no_progress_window_eligible(&history, 4, always));
    }

    #[test]
    fn nontrivial_origin_filter_excludes_already_clean_origins() {
        let mut history = ProgressHistory::default();
        // Origin index 0: a progressed (nothing to "make progress" from)
        history.push_snapshot(set(&["a"]), set(&["a"]));
        for _ in 0..4 {
            history.push_snapshot(set(&["a"]), set(&["a"]));
        }
        // Filter: nontrivial iff some node unprogressed at origin
        let nontrivial = |s: &CycleSnapshot| s.present.iter().any(|n| !s.progressed.contains(n));
        // Origin=0 has nothing unprogressed → filtered. No eligible
        // origin remains.
        assert!(!no_progress_window_eligible(&history, 4, nontrivial));
    }

    #[test]
    fn note_dispatched_marks_existing_snapshots_ineligible() {
        let mut history = ProgressHistory::default();
        for _ in 0..5 {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        assert!(no_progress_window_eligible(&history, 4, always));
        history.note_dispatched();
        // After dispatch, every existing snapshot is below the floor.
        assert!(!no_progress_window_eligible(&history, 4, always));
        // Push more snapshots until origin index >= floor+1 has depth >= k.
        for _ in 0..5 {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        assert!(no_progress_window_eligible(&history, 4, always));
    }

    #[test]
    fn buffer_cap_drops_oldest() {
        let mut history = ProgressHistory::default();
        for _ in 0..(PROGRESS_HISTORY_MAX_SNAPSHOTS + 3) {
            history.push_snapshot(set(&["a"]), BTreeSet::new());
        }
        assert_eq!(history.snapshots.len(), PROGRESS_HISTORY_MAX_SNAPSHOTS);
        // Oldest index now is 3 (we dropped 0..=2).
        assert_eq!(history.snapshots.front().unwrap().snapshot_index, 3);
    }
}
