use crate::model::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Pure, state-aware acceptance input for a cleanup worker transition.
///
/// Both the worker-runnable normalizer and the engine build this value from
/// their pre/post views and call [`evaluate_cleanup_transition_preflight`].
/// Keeping the policy here prevents either caller from silently growing a
/// second cleanup acceptance predicate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupTransitionPreflightInput {
    pub enforce_formalization_validity: bool,
    pub baseline_formalization_valid: bool,
    pub post_formalization_valid: bool,
    pub baseline_live_orphans: BTreeSet<NodeId>,
    pub post_live_orphans: BTreeSet<NodeId>,
}

/// Deterministic result of the shared cleanup transition predicate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupTransitionPreflight {
    pub baseline_formalization_valid: bool,
    pub post_formalization_valid: bool,
    pub baseline_live_orphans: BTreeSet<NodeId>,
    pub post_live_orphans: BTreeSet<NodeId>,
    pub newly_created_live_orphans: BTreeSet<NodeId>,
    pub formalization_regressed: bool,
    pub rejection_reasons: Vec<String>,
}

impl CleanupTransitionPreflight {
    pub fn accepted(&self) -> bool {
        self.rejection_reasons.is_empty()
    }

    /// Compare the acceptance-significant projection. A checker cannot prove
    /// that a pre-existing invalid baseline was repaired solely from a
    /// successful preserving pass, so the raw post-validity observation may
    /// differ while both sides still make the same transition decision.
    pub fn acceptance_projection(&self) -> (bool, &BTreeSet<NodeId>, &[String]) {
        (
            self.formalization_regressed,
            &self.newly_created_live_orphans,
            &self.rejection_reasons,
        )
    }
}

/// The single cleanup transition acceptance predicate.
///
/// Responsibility is delta-based. A burst must not make a valid
/// formalization invalid and must not create a new live orphan. Orphans and
/// other invalidity that were already present remain explicit in the result,
/// but do not make an unrelated burst impossible to accept.
pub fn evaluate_cleanup_transition_preflight(
    input: CleanupTransitionPreflightInput,
) -> CleanupTransitionPreflight {
    let newly_created_live_orphans = input
        .post_live_orphans
        .difference(&input.baseline_live_orphans)
        .cloned()
        .collect::<BTreeSet<_>>();
    let formalization_regressed = input.enforce_formalization_validity
        && input.baseline_formalization_valid
        && !input.post_formalization_valid;
    let mut rejection_reasons = Vec::new();
    if formalization_regressed {
        rejection_reasons.push(
            "cleanup worker burst regressed formalization_valid: the baseline was valid and the post-transition state is not"
                .to_string(),
        );
    }
    if !newly_created_live_orphans.is_empty() {
        rejection_reasons.push(format!(
            "valid worker response leaves NEW live orphan nodes: {:?}",
            newly_created_live_orphans
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        ));
    }
    CleanupTransitionPreflight {
        baseline_formalization_valid: input.baseline_formalization_valid,
        post_formalization_valid: input.post_formalization_valid,
        baseline_live_orphans: input.baseline_live_orphans,
        post_live_orphans: input.post_live_orphans,
        newly_created_live_orphans,
        formalization_regressed,
        rejection_reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_orphans_are_reported_but_not_charged_to_the_burst() {
        let baseline = BTreeSet::from([NodeId::from("old")]);
        let result = evaluate_cleanup_transition_preflight(CleanupTransitionPreflightInput {
            enforce_formalization_validity: true,
            baseline_formalization_valid: true,
            post_formalization_valid: true,
            baseline_live_orphans: baseline.clone(),
            post_live_orphans: baseline,
        });
        assert!(result.accepted());
        assert!(result.newly_created_live_orphans.is_empty());
    }

    #[test]
    fn new_orphan_and_formalization_regression_are_both_reported() {
        let result = evaluate_cleanup_transition_preflight(CleanupTransitionPreflightInput {
            enforce_formalization_validity: true,
            baseline_formalization_valid: true,
            post_formalization_valid: false,
            baseline_live_orphans: BTreeSet::from([NodeId::from("old")]),
            post_live_orphans: BTreeSet::from([NodeId::from("old"), NodeId::from("new")]),
        });
        assert!(!result.accepted());
        assert!(result.formalization_regressed);
        assert_eq!(
            result.newly_created_live_orphans,
            BTreeSet::from([NodeId::from("new")])
        );
        assert_eq!(result.rejection_reasons.len(), 2);
    }
}
