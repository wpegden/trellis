// Integration tests for the verdicts[]-shaped substantiveness artifact
// validator. Exercises `validate_substantiveness_result_data` directly
// via the public crate API so we can run them while the lib's `mod
// tests` modules are blocked by pre-existing K-8 NodeId/TargetId
// migration breakage.

use serde_json::json;
use trellis_kernel::validate_substantiveness_result_data;

#[test]
fn pass_with_optional_comment_validates() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Pass"},
                {"node": "BarThm", "verdict": "Pass", "comment": "ok"},
            ],
        },
        "overall": "APPROVE",
        "summary": "two passes",
        "comments": "",
    }));
    assert!(result.ok, "errors={:?}", result.errors);
}

#[test]
fn fail_requires_non_empty_comment() {
    let bad = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "FAIL",
            "verdicts": [{"node": "FooLemma", "verdict": "Fail"}],
        },
        "overall": "REJECT",
        "summary": "fail without comment",
        "comments": "",
    }));
    assert!(!bad.ok);
    assert!(
        bad.errors
            .iter()
            .any(|e| e.contains("comment must be non-empty when verdict is Fail")),
        "errors={:?}",
        bad.errors
    );

    let ok = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "FAIL",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Fail", "comment": "merge with Bar"},
            ],
        },
        "overall": "REJECT",
        "summary": "one fail",
        "comments": "",
    }));
    assert!(ok.ok, "errors={:?}", ok.errors);
}

#[test]
fn not_done_yet_admits_optional_comment() {
    let no_comment = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [{"node": "FooLemma", "verdict": "NotDoneYet"}],
        },
        "overall": "APPROVE",
        "summary": "ran out of time",
        "comments": "",
    }));
    assert!(no_comment.ok, "errors={:?}", no_comment.errors);

    let with_comment = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma", "verdict": "NotDoneYet", "comment": "got partway"},
            ],
        },
        "overall": "APPROVE",
        "summary": "partial work",
        "comments": "",
    }));
    assert!(with_comment.ok, "errors={:?}", with_comment.errors);
}

#[test]
fn pass_decision_with_fail_verdict_rejected() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Pass"},
                {"node": "BarThm", "verdict": "Fail", "comment": "merge"},
            ],
        },
        "overall": "APPROVE",
        "summary": "inconsistent",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("decision must be FAIL when any verdict is Fail")),
        "errors={:?}",
        result.errors
    );
}

#[test]
fn fail_decision_with_no_fail_verdicts_rejected() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "FAIL",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Pass"},
                {"node": "BarThm", "verdict": "NotDoneYet"},
            ],
        },
        "overall": "REJECT",
        "summary": "wrong",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("decision must be PASS when no verdict is Fail")),
        "errors={:?}",
        result.errors
    );
}

#[test]
fn duplicate_node_verdicts_rejected() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Pass"},
                {"node": "FooLemma", "verdict": "NotDoneYet"},
            ],
        },
        "overall": "APPROVE",
        "summary": "duplicate",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("duplicate verdict for node \"FooLemma\"")),
        "errors={:?}",
        result.errors
    );
}

#[test]
fn not_done_yet_suffix_on_node_rejected() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma[NotDoneYet]", "verdict": "NotDoneYet"},
            ],
        },
        "overall": "APPROVE",
        "summary": "wrong shape",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result.errors.iter().any(|e| e.contains("[NotDoneYet]")),
        "errors={:?}",
        result.errors
    );
}

#[test]
fn empty_verdicts_list_admitted() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [],
        },
        "overall": "APPROVE",
        "summary": "empty",
        "comments": "",
    }));
    assert!(result.ok, "errors={:?}", result.errors);
}

#[test]
fn invalid_verdict_value_rejected() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [
                {"node": "FooLemma", "verdict": "Maybe"},
            ],
        },
        "overall": "APPROVE",
        "summary": "bad verdict value",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("verdict must be one of ['Pass', 'Fail', 'NotDoneYet']")),
        "errors={:?}",
        result.errors
    );
}

#[test]
fn overall_must_match_decision() {
    let result = validate_substantiveness_result_data(&json!({
        "substantiveness": {
            "decision": "PASS",
            "verdicts": [{"node": "FooLemma", "verdict": "Pass"}],
        },
        "overall": "REJECT",
        "summary": "mismatch",
        "comments": "",
    }));
    assert!(!result.ok);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("overall must be APPROVE")),
        "errors={:?}",
        result.errors
    );
}
