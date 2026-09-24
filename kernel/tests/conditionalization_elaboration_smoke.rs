//! Real-Lean meaning smoke for generic conditional sealing. Rust byte tests
//! pin the transformation; this test makes every supported binder form and
//! every anti-vacuity obligation elaborate in the pinned fixture toolchain.

use std::path::PathBuf;
use std::process::Command;

use trellis_kernel::trust_base::{
    build_conditional_correspondence_request, seal_conditional_theorem,
    stamp_conditional_proposal, ConditionalCorrespondenceVerdict,
    ConditionalTheoremProposal, ConditionalTriggerClassification,
};
use trellis_kernel::{
    ChallengeResolution, ChallengeTargetId, ChallengeTargetKind, ChallengeTargetSpec,
    ProtocolState, StatementProvenance,
};

mod common;
use common::project_tempdir;

const ALLOW_FIXTURE_SKIP_ENV: &str = "TRELLIS_ALLOW_FIXTURE_SKIP";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("local_closure_smoke")
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(binary))
        .find(|candidate| candidate.is_file())
}

fn smoke_skip(test: &str) -> bool {
    let reason = if let Some(explicit) = std::env::var_os("TRELLIS_FIXTURE_LEAN") {
        let path = PathBuf::from(explicit);
        (!path.is_file()).then(|| format!("{} is not a file", path.display()))
    } else if which("lake").is_none() {
        Some("`lake` is unavailable and TRELLIS_FIXTURE_LEAN is unset".into())
    } else if !fixture_root().join("lean-toolchain").is_file() {
        Some("the pinned fixture has no lean-toolchain".into())
    } else {
        None
    };
    let Some(reason) = reason else { return false };
    if std::env::var(ALLOW_FIXTURE_SKIP_ENV).ok().as_deref() == Some("1") {
        eprintln!("SKIP {test}: {reason}");
        return true;
    }
    panic!("{test}: real Lean fixture unavailable: {reason}");
}

fn elaborate(source: &str) -> (Option<i32>, String) {
    let directory = project_tempdir();
    let path = directory.path().join("GenericConditional.lean");
    std::fs::write(&path, source).unwrap();
    let output = match std::env::var_os("TRELLIS_FIXTURE_LEAN") {
        Some(lean) => Command::new(lean).arg(&path).output(),
        None => Command::new("lake")
            .args(["env", "lean"])
            .arg(&path)
            .current_dir(fixture_root())
            .output(),
    }
    .expect("spawn pinned Lean elaborator");
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn seal(
    lean: &str,
    condition: &str,
    arguments: Option<Vec<&str>>,
) -> trellis_kernel::trust_base::SealedConditionalTheorem {
    let target = ChallengeTargetId::from("goal:lean-smoke");
    let mut state = ProtocolState::default();
    state.configured_challenge_targets.insert(
        target.clone(),
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "Original".into(),
            lean: lean.into(),
            resolution: ChallengeResolution::Decide,
            statement_provenance: StatementProvenance::KernelDerived,
            ..ChallengeTargetSpec::default()
        },
    );
    let stamped = stamp_conditional_proposal(
        &state,
        ConditionalTheoremProposal {
            target_id: target,
            condition_lean: condition.into(),
            condition_informal: "fixture condition".into(),
            rationale: "fixture rationale".into(),
            trigger: ConditionalTriggerClassification::UnconditionalNotEstablished,
            concrete_counterexample_arguments: arguments.map(|items| {
                items.into_iter().map(str::to_owned).collect()
            }),
            ..ConditionalTheoremProposal::default()
        },
        1,
        Vec::new(),
    )
    .unwrap();
    let request = build_conditional_correspondence_request(
        &state,
        &stamped,
        serde_json::json!({"fixture": true}),
        serde_json::Value::Null,
    )
    .unwrap();
    let verdict = ConditionalCorrespondenceVerdict {
        request_sha256: request.request_sha256,
        proposal_sha256: stamped.proposal_sha256,
        boundary_expression_correct: true,
        condition_relevant: true,
        realizable_non_vacuous: true,
        obligation_preserved_on_domain: true,
        rust_axioms_backed_by_approved_assumptions: true,
        findings: "fixture authorizes the exact shape".into(),
    };
    seal_conditional_theorem(&state, &stamped, &request, &verdict)
        .unwrap()
        .1
}

struct Case<'a> {
    lean: &'a str,
    condition: &'a str,
    arguments: Option<Vec<&'a str>>,
    proof: &'a str,
    inhabited: &'a str,
    excluded: Option<&'a str>,
    expected_type: &'a str,
}

#[test]
fn generic_seals_elaborate_for_every_binder_and_obligation_shape() {
    if smoke_skip("generic_seals_elaborate_for_every_binder_and_obligation_shape") {
        return;
    }
    let cases = [
        Case {
            lean: "theorem Original : True ↔ True := by",
            condition: "1 < 2",
            arguments: None,
            proof: "  intro _\n  exact ⟨id, id⟩",
            inhabited: "  decide",
            excluded: None,
            expected_type: "1 < 2 → (True ↔ True)",
        },
        Case {
            lean: "theorem Original (n : Nat) : n = n := by",
            condition: "n ≠ 0",
            arguments: Some(vec!["0"]),
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨1, by decide⟩",
            excluded: Some("  simp"),
            expected_type: "∀ (n : Nat), n ≠ 0 → (n = n)",
        },
        Case {
            lean: "theorem Original {α : Type} (x : α) : x = x := by",
            condition: "Nonempty α",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨Nat, 0, ⟨0⟩⟩",
            excluded: None,
            expected_type: "∀ {α : Type} (x : α), Nonempty α → (x = x)",
        },
        Case {
            lean: "theorem Original ⦃n : Nat⦄ : n = n := by",
            condition: "n < n + 1",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨0, by decide⟩",
            excluded: None,
            expected_type: "∀ ⦃n : Nat⦄, n < n + 1 → (n = n)",
        },
        Case {
            lean: "theorem Original {{n : Nat}} : n = n := by",
            condition: "n < n + 1",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨0, by decide⟩",
            excluded: None,
            expected_type: "∀ ⦃n : Nat⦄, n < n + 1 → (n = n)",
        },
        Case {
            lean: "theorem Original [inst : Inhabited Nat] (n : Nat) : n = n := by",
            condition: "n < n + 1",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  refine ⟨inferInstance, ?_⟩\n  exact ⟨0, by decide⟩",
            excluded: None,
            expected_type: "∀ [inst : Inhabited Nat] (n : Nat), n < n + 1 → (n = n)",
        },
        Case {
            lean: "theorem Original (n : Nat) (v : Fin (n + 1)) : v = v := by",
            condition: "n < n + 1",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨0, 0, by decide⟩",
            excluded: None,
            expected_type: "∀ (n : Nat) (v : Fin (n + 1)), n < n + 1 → (v = v)",
        },
        Case {
            lean: "theorem Original (_ : Nat) : True := by",
            condition: "1 < 2",
            arguments: None,
            proof: "  intro _\n  trivial",
            inhabited: "  exact ⟨0, by decide⟩",
            excluded: None,
            expected_type: "∀ (_conditionalBinder1 : Nat), 1 < 2 → True",
        },
        Case {
            lean: "theorem Original : ∀ (n : Nat), ∀ {m : Nat}, n = m → n = m := by",
            condition: "n = m",
            arguments: None,
            proof: "  intro h _\n  exact h",
            inhabited: "  exact ⟨0, 0, rfl⟩",
            excluded: None,
            expected_type: "∀ (n : Nat) {m : Nat}, n = m → (n = m → n = m)",
        },
        Case {
            lean: "theorem Original : forall (n : Nat), n = n := by",
            condition: "n < n + 1",
            arguments: None,
            proof: "  intro _\n  rfl",
            inhabited: "  exact ⟨0, by decide⟩",
            excluded: None,
            expected_type: "∀ (n : Nat), n < n + 1 → (n = n)",
        },
    ];
    let mut source = String::from("set_option autoImplicit false\n");
    for case in cases {
        let sealed = seal(case.lean, case.condition, case.arguments);
        source.push_str(&format!(
            "\n{}\n{}\n\n{}\n{}\n",
            sealed.proof.statement_lean,
            case.proof,
            sealed.inhabited.statement_lean,
            case.inhabited,
        ));
        if let (Some(obligation), Some(body)) = (sealed.counterexample_excluded, case.excluded) {
            source.push_str(&format!("\n{}\n{}\n", obligation.statement_lean, body));
        }
        source.push_str(&format!(
            "\nexample : {} := @{}\n",
            case.expected_type, sealed.proof.theorem_name
        ));
    }
    let (code, output) = elaborate(&source);
    assert_eq!(
        code,
        Some(0),
        "every kernel seal must elaborate in the real fixture:\n{output}\n--- source ---\n{source}"
    );
    assert!(!output.to_ascii_lowercase().contains("error"), "{output}");
}
