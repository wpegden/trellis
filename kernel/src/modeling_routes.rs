//! Shared preparation/native modeling-route review policy.
use crate::model::{ModelingResolution, ModelingRouteAttempt, ModelingRouteReview, NodeId, TargetId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelingRouteRole { Planner, Critic, Audit }

impl ModelingRouteReview {
    pub fn next_role(&self) -> ModelingRouteRole {
        if self.attempts.len() >= 2 { ModelingRouteRole::Audit }
        else if self.pending_route.is_some() { ModelingRouteRole::Critic }
        else { ModelingRouteRole::Planner }
    }

    pub fn submit_plan(&mut self, route: String) -> Result<(), String> {
        if self.next_role() != ModelingRouteRole::Planner || route.trim().is_empty() {
            return Err("A modeling planner must supply a route or a reasoned no-route proposal in its turn".into());
        }
        self.pending_route = Some(route);
        Ok(())
    }

    pub fn submit_critique(&mut self, decision: String, report: String, feedback: String) -> Result<(), String> {
        if self.next_role() != ModelingRouteRole::Critic || !matches!(decision.as_str(), "accept" | "reject")
            || report.trim().is_empty() || (decision == "reject" && feedback.trim().is_empty()) {
            return Err("A modeling critic must assess the current proposal with accept/reject and reasons".into());
        }
        self.attempts.push(ModelingRouteAttempt {
            route_tex: self.pending_route.take().unwrap(), critic_decision: decision,
            critic_report: report, critic_feedback: feedback,
        });
        Ok(())
    }

    pub fn validate_resolution(&self, basis: &str, pursued: &BTreeSet<TargetId>, resolution: &ModelingResolution) -> Result<(), String> {
        if self.next_role() != ModelingRouteRole::Audit || self.attempts.len() != 2 || self.basis_fingerprint != basis {
            return Err("Modeling decision requires two completed planner/critic rounds on the current source and goal basis".into());
        }
        match resolution {
            ModelingResolution::Approve { scope, limitations, tasks, declined_targets, .. } => {
                let ids: BTreeSet<_> = tasks.iter().map(|t| t.id.trim()).collect();
                if scope.trim().is_empty() || tasks.is_empty() || ids.len() != tasks.len()
                    || tasks.iter().any(|t| t.id.trim().is_empty() || t.title.trim().is_empty() || t.body.trim().is_empty()) {
                    return Err("Route approval requires the exact scope and concrete, uniquely identified implementation/validation tasks".into());
                }
                if !declined_targets.is_subset(&self.targets) || !declined_targets.is_subset(pursued)
                    || declined_targets == pursued || (!declined_targets.is_empty() && limitations.trim().is_empty()) {
                    return Err("Declines must be justified reviewed goals and leave a pursued task; otherwise end the process".into());
                }
            }
            ModelingResolution::Reopen | ModelingResolution::Fail => {}
        }
        Ok(())
    }
}

/// The preparation controller uses the same state transitions and artifact
/// validators as native Modeling, through the existing runtime CLI.
#[derive(Debug, Deserialize)]
pub struct ModelingRouteStepInput {
    pub review: ModelingRouteReview,
    pub role: ModelingRouteRole,
    pub raw_payload: serde_json::Value,
    pub basis_fingerprint: String,
    pub pursued_targets: BTreeSet<TargetId>,
    /// Current declared consequential paper exceptions. This carries scope,
    /// not an approval; omitted legacy callers keep their existing policy.
    #[serde(default)]
    pub paper_modeling_choices: Option<BTreeSet<NodeId>>,
}

pub fn step(mut input: ModelingRouteStepInput) -> Result<serde_json::Value, String> {
    use crate::artifact_validation::{validate_trellis_audit_result_data, validate_trellis_stuck_math_audit_result_data};
    if input.review.next_role() != input.role || input.review.basis_fingerprint != input.basis_fingerprint {
        return Err("Stale or out-of-turn modeling-route response".into());
    }
    let validated = if input.role == ModelingRouteRole::Audit {
        validate_trellis_audit_result_data(&input.raw_payload)
    } else { validate_trellis_stuck_math_audit_result_data(&input.raw_payload) };
    if !validated.ok { return Err(validated.errors.join("; ")); }
    let data = validated.data.unwrap();
    let text = |key: &str| data[key].as_str().unwrap_or("").to_owned();
    match input.role {
        ModelingRouteRole::Planner => {
            let route = text("route_tex");
            input.review.submit_plan(if route.trim().is_empty() { text("report") } else { route })?;
        }
        ModelingRouteRole::Critic => input.review.submit_critique(text("gap_decision"), text("report"), text("gap_feedback"))?,
        ModelingRouteRole::Audit => {
            if text("outcome") != "modeling_accept" || text("scratchpad_replace").trim().is_empty()
                || data["new_tasks"].as_array().is_some_and(|a| !a.is_empty())
                || data["task_modifications"].as_array().is_some_and(|a| !a.is_empty()) {
                return Err("Route audit requires modeling_accept, an assessment, and empty cleanup fields".into());
            }
            let resolution: ModelingResolution = serde_json::from_value(data["modeling_resolution"].clone()).map_err(|e| e.to_string())?;
            input.review.validate_resolution(&input.basis_fingerprint, &input.pursued_targets, &resolution)?;
            if let Some(declared) = &input.paper_modeling_choices {
                let selected = crate::paper_implementation::approved_route_choice_ids(declared, &resolution)?;
                if !crate::paper_implementation::route_choices_reviewed(&input.review, &selected) {
                    return Err("Paper-model route approval requires every selected exact choice ID in both planner/critic proposals".into());
                }
            }
        }
    }
    Ok(serde_json::json!({"review": input.review, "next_role": input.review.next_role(), "data": data}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn audit_input(first: &str, second: &str, tasks: &[&str], choices: Option<&[&str]>) -> ModelingRouteStepInput {
        let mut review = ModelingRouteReview { basis_fingerprint: "source-and-paper-basis".into(),
            targets: BTreeSet::from(["paper-target".into()]), ..Default::default() };
        for proposal in [first, second] {
            review.submit_plan(proposal.into()).unwrap();
            review.submit_critique("accept".into(), "Reviewed the route's source correspondence obligations.".into(), String::new()).unwrap();
        }
        ModelingRouteStepInput { review, role: ModelingRouteRole::Audit,
            raw_payload: json!({"outcome": "modeling_accept", "scratchpad_replace": "Approve only the explicitly reviewed choice tasks.",
                "new_tasks": [], "task_modifications": [], "modeling_resolution": {"action": "approve", "scope": "The selected modeling choices.",
                    "tasks": tasks.iter().map(|id| json!({"id": id, "title": "Establish source correspondence",
                        "body": "Implement the reviewed model and discharge its existing checked obligations."})).collect::<Vec<_>>(),
                    "declined_targets": [], "reopen_setup": false}}),
            basis_fingerprint: "source-and-paper-basis".into(), pursued_targets: BTreeSet::from(["paper-target".into()]),
            paper_modeling_choices: choices.map(|ids| ids.iter().map(|id| NodeId::from(*id)).collect()) }
    }

    #[test]
    fn preparation_modeling_choices_final_audit_requires_declared_reviewed_scope() {
        let result = step(audit_input("First route `paper-model:A`.", "Second route (paper-model:A).",
            &["paper-model:A"], Some(&["A", "B"]))).unwrap();
        assert_eq!(result["data"]["modeling_resolution"]["tasks"][0]["id"], "paper-model:A");
        assert!(result.get("approved").is_none(), "the adapter must not synthesize an approval authority");
        assert!(step(audit_input("Route paper-model:Unknown", "Route paper-model:Unknown",
            &["paper-model:Unknown"], Some(&["A"]))).unwrap_err().contains("declared consequential"));
        for (first, second) in [("Route paper-model:A", "Ordinary support only"),
                                ("Ordinary support only", "Route paper-model:A")] {
            assert!(step(audit_input(first, second, &["paper-model:A"], Some(&["A"])))
                .unwrap_err().contains("both planner/critic proposals"));
        }
        assert!(step(audit_input("Route paper-model:A", "Route paper-model:A",
            &["paper-model:A", "paper-model:B"], Some(&["A", "B"]))).is_err(),
            "one reviewed choice cannot authorize another declared choice");
    }

    #[test]
    fn preparation_modeling_choices_exact_markers_not_substrings() {
        for marker in ["paper-model:AB", "paper-model:A.extra", "paper-model:A_suffix", "paper-model:A'",
                       "paper-model:A-other", "notpaper-model:A", "prefix-paper-model:A", "αpaper-model:A"] {
            assert!(step(audit_input(marker, "Exact paper-model:A", &["paper-model:A"], Some(&["A"]))).is_err(),
                "marker {marker} impersonated an exact choice");
        }
        for marker in ["paper-model:A", "`paper-model:A`", "(paper-model:A)", "Choice: paper-model:A, then its obligations"] {
            assert!(step(audit_input(marker, marker, &["paper-model:A"], Some(&["A"]))).is_ok(), "{marker}");
        }
    }

    #[test]
    fn preparation_modeling_choices_optional_legacy_input_and_ordinary_helpers() {
        let legacy = audit_input("First route", "Second route", &["paper-model:Unknown"], None);
        assert!(step(legacy).is_ok());
        assert!(step(audit_input("First route", "Second route", &["paper-model:Unknown"], Some(&[]))).is_err());
        assert!(step(audit_input("Ordinary proof route", "Ordinary proof route", &["prove-helper"], Some(&[]))).is_ok());
        let omitted: ModelingRouteStepInput = serde_json::from_value(json!({"review": {}, "role": "planner",
            "raw_payload": {}, "basis_fingerprint": "", "pursued_targets": []})).unwrap();
        assert!(omitted.paper_modeling_choices.is_none());
    }

    #[test]
    fn preparation_modeling_choices_do_not_bypass_rounds_basis_or_terminal_decisions() {
        let mut input = audit_input("Route paper-model:A", "Route paper-model:A", &["paper-model:A"], Some(&["A"]));
        input.basis_fingerprint = "changed-source".into();
        assert!(step(input).unwrap_err().contains("Stale"));
        let mut input = audit_input("Route paper-model:A", "Route paper-model:A", &["paper-model:A"], Some(&["A"]));
        input.review.attempts.pop();
        assert!(step(input).unwrap_err().contains("out-of-turn"));
        for action in ["fail", "reopen"] {
            let mut input = audit_input("No supported model", "No supported model", &["unused"], Some(&["A"]));
            input.raw_payload["modeling_resolution"] = json!({"action": action});
            assert_eq!(step(input).unwrap()["data"]["modeling_resolution"], json!({"action": action}));
        }
        // Omitting a resolution still fails in the existing artifact/route path.
        let mut input = audit_input("Route paper-model:A", "Route paper-model:A", &["paper-model:A"], Some(&["A"]));
        input.raw_payload["modeling_resolution"] = Value::Null;
        assert!(step(input).is_err());
    }
}
