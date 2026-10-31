//! PV #1 keystone — faithfulness-lane fragment selection.
//!
//! A Paper request with a non-empty `paper_verify_targets` frontier and an
//! empty substantiveness frontier hits the target-package (faithfulness)
//! scenario. We assert:
//!   - all-math (is_pv == false): the math `verifier/paper_faithfulness/*`
//!     fragments + `canonical/FAITHFULNESS.md` (byte-identical baseline).
//!   - PV mode A (is_pv == true): the PV `pv/verifier/paper_faithfulness/*`
//!     fragments + shared `canonical/FAITHFULNESS.md` and the
//!     `pv/canonical/FAITHFULNESS.md` GOAL.md referent supplement.
//!
//! Mode B is covered structurally: it never produces a non-empty
//! `paper_verify_targets`, so this scenario is unreachable there — the
//! integration test `runtime_cli_init_pv_mode_b_*` pins the empty
//! `configured_targets` that guarantees it.

use trellis_kernel::model::{RequestKind, TargetId, WrapperRequest};
use trellis_kernel::request_contracts::paper_contract_payload;

fn target_package_paper_request(is_pv: bool) -> WrapperRequest {
    WrapperRequest {
        kind: RequestKind::Paper,
        is_pv,
        // Non-empty targets + empty substantiveness ⇒ the target-package
        // (faithfulness) fall-through, not the per-node scenario.
        paper_verify_targets: [TargetId::from("f")].into_iter().collect(),
        ..WrapperRequest::default()
    }
}

fn fragments_of(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("prompt_fragments")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn all_math_faithfulness_selects_math_fragments() {
    let payload = paper_contract_payload(&target_package_paper_request(false), None);
    let fragments = fragments_of(&payload);
    assert!(
        fragments.contains(&"verifier/paper_faithfulness/05_fresh_target_package.md".to_string()),
        "math run selects the math faithfulness scenario fragment: {fragments:?}"
    );
    assert!(
        fragments.contains(&"verifier/paper_faithfulness/40_rubric.md".to_string()),
        "math run selects the math faithfulness rubric: {fragments:?}"
    );
    assert!(
        fragments.contains(&"canonical/FAITHFULNESS.md".to_string()),
        "math run inlines the math canonical FAITHFULNESS: {fragments:?}"
    );
    assert!(
        !fragments
            .iter()
            .any(|f| f.starts_with("pv/verifier/paper_faithfulness/")),
        "math run must not select any PV faithfulness fragment: {fragments:?}"
    );
}

#[test]
fn pv_mode_a_faithfulness_selects_pv_goal_referent_fragments() {
    let payload = paper_contract_payload(&target_package_paper_request(true), None);
    let fragments = fragments_of(&payload);
    assert!(
        fragments
            .contains(&"pv/verifier/paper_faithfulness/05_fresh_target_package.md".to_string()),
        "PV run selects the PV (GOAL.md) faithfulness scenario fragment: {fragments:?}"
    );
    assert!(
        fragments.contains(&"pv/verifier/paper_faithfulness/40_rubric.md".to_string()),
        "PV run selects the PV faithfulness rubric: {fragments:?}"
    );
    assert!(
        fragments.contains(&"pv/canonical/FAITHFULNESS.md".to_string()),
        "PV run inlines the PV canonical FAITHFULNESS (GOAL.md referent): {fragments:?}"
    );
    // It must NOT leak the math paper-faithfulness fragments.
    assert!(
        !fragments.contains(&"verifier/paper_faithfulness/05_fresh_target_package.md".to_string()),
        "PV run must not select the math faithfulness scenario fragment: {fragments:?}"
    );
    assert!(
        !fragments.contains(&"verifier/paper_faithfulness/40_rubric.md".to_string()),
        "PV run must not select the math faithfulness rubric: {fragments:?}"
    );
    assert!(
        fragments.contains(&"canonical/FAITHFULNESS.md".to_string()),
        "PV run must retain shared canonical faithfulness rules: {fragments:?}"
    );
}

#[test]
fn pv_goal_faithfulness_preserves_shared_rules_and_goal_referents() {
    let shared = include_str!("../../trellis/prompt_fragments/canonical/FAITHFULNESS.md");
    let pv = include_str!("../../trellis/prompt_fragments/pv/canonical/FAITHFULNESS.md");
    for protected_rule in [
        "at least as strong", "not collectively stronger", "quantifier scopes",
        "ambient hypotheses",
    ] {
        assert!(shared.contains(protected_rule), "shared faithfulness must retain {protected_rule:?}");
    }
    for protected_referent in [
        "`GOAL.md`", "function subject", "input domain", "observations",
        "completion conditions", "extracted model",
    ] {
        assert!(pv.contains(protected_referent), "PV faithfulness must retain {protected_referent:?}");
    }
}
