//! Golden canonical encodings for the reduced terminal-result lattice.

use trellis_kernel::model::ChallengeTargetId;
use trellis_kernel::trust_base::{RustArtifactSummary, Sha256Digest, TerminalOutcome};

fn digest(fill: char) -> Sha256Digest {
    fill.to_string().repeat(64).parse().unwrap()
}

fn canonical_string(value: &TerminalOutcome) -> String {
    String::from_utf8(trellis_kernel::trust_base::canonical_json(value).unwrap()).unwrap()
}

#[test]
fn terminal_outcome_canonical_bytes_and_digests_are_pinned() {
    let cases = [
        (
            TerminalOutcome::Disproved {
                proof_subject_sha256: digest('1'),
                artifact: RustArtifactSummary::Absent,
            },
            "{\"artifact\":{\"kind\":\"absent\"},\"kind\":\"disproved\",\"proof_subject_sha256\":\"1111111111111111111111111111111111111111111111111111111111111111\"}",
            "29eeb7a92c8416fc41de10bb44780f7c672a88e29784057583bf3d21b1512711",
        ),
        (
            TerminalOutcome::Proved {
                proof_subject_sha256: digest('2'),
            },
            "{\"kind\":\"proved\",\"proof_subject_sha256\":\"2222222222222222222222222222222222222222222222222222222222222222\"}",
            "dd885f99944f2656e6ef7db86cd6728b281b1c345004074634262a81c9acf952",
        ),
        (
            TerminalOutcome::ConditionalTheorem {
                approval_record_sha256: digest('3'),
                condition_lean: "x > 0".into(),
                conditional_target_id: ChallengeTargetId::from("conditional:goal"),
                proof_subject_sha256: digest('5'),
                sealed_statement_lean: "theorem c : True := by".into(),
                sealed_statement_sha256: digest('6'),
            },
            "{\"approval_record_sha256\":\"3333333333333333333333333333333333333333333333333333333333333333\",\"condition_lean\":\"x > 0\",\"conditional_target_id\":\"conditional:goal\",\"kind\":\"conditional_theorem\",\"proof_subject_sha256\":\"5555555555555555555555555555555555555555555555555555555555555555\",\"sealed_statement_lean\":\"theorem c : True := by\",\"sealed_statement_sha256\":\"6666666666666666666666666666666666666666666666666666666666666666\"}",
            "417fef5a865382b6febc6236def7afe6ab5494d192dfcb0dd4abb3ab275ca5fa",
        ),
    ];

    for (outcome, expected_bytes, expected_digest) in cases {
        let bytes = canonical_string(&outcome);
        assert_eq!(bytes, expected_bytes);
        assert_eq!(
            trellis_kernel::trust_base::raw_sha256(bytes.as_bytes()).to_string(),
            expected_digest
        );
        assert_eq!(
            serde_json::from_str::<TerminalOutcome>(&bytes).unwrap(),
            outcome
        );
    }
}

#[test]
fn retired_terminal_authority_is_not_accepted() {
    for kind in [
        "disproved_source_confirmed",
        "disproved_with_conditional",
        "disproved_unresolved",
        "give_up",
    ] {
        let wire = serde_json::json!({
            "kind": kind,
            "proof_subject_sha256": "11".repeat(32)
        });
        assert!(serde_json::from_value::<TerminalOutcome>(wire).is_err());
    }
}
