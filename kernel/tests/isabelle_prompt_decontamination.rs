//! GAP 2 gate: on an isabelle_hol run the worker/reviewer/verifier prompt
//! corpus must be Isabelle-native — no Lean tokens (`lake`, `mathlib`,
//! `.lean`, `-- BODY`, `:= by`) survive in any SELECTED fragment. And the Lean
//! arm of every `isa_or_lean` route is unchanged, so a Lean (or target-unset)
//! repo emits the byte-identical fragment list it did before the hook.

use std::path::{Path, PathBuf};

use trellis_kernel::model::{
    Phase, RequestKind, TargetId, WorkerContext, WorkerProfile, WorkerValidationKind,
    WrapperRequest,
};
use trellis_kernel::request_contracts::{
    correspondence_contract_payload, paper_contract_payload, review_contract_payload,
    soundness_contract_payload, worker_contract_payload,
};

/// `prompt_fragments/` lives at `<kernel>/../trellis/prompt_fragments`.
fn fragment_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("trellis")
        .join("prompt_fragments")
}

fn fragments_of(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("prompt_fragments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Lean-specific tokens that must NOT appear in any backend-routed Isabelle
/// fragment. `.lean` covers FILESPEC `.lean` / `lake env lean`; `-- BODY` is
/// the Lean FILESPEC body marker; `:= by` is a Lean proof opener;
/// `lake`/`mathlib` are Lean-toolchain names. (Substring match.)
const LEAN_TOKENS: &[&str] = &["lake", "mathlib", ".lean", "-- BODY", ":= by", "Lean-closed"];

/// A selected fragment is *backend-routed* iff an `_isabelle` sibling exists on
/// disk — i.e. Slice 2 (or prior Isabelle work) was responsible for routing
/// it. Those are the fragments the de-contamination gate covers. The shared
/// foundational scheme / canonical-rubric docs (`common/00_trellis_scheme_brief.md`,
/// `canonical/*.md`, `TRELLIS_FORMALIZATION_SCHEME*.md`) are deliberately NOT
/// backend-split here — making them Isabelle-native is a separate, larger
/// effort tracked as the deferred remainder — so the gate skips them.
fn isabelle_sibling_path(fragment_id: &str, root: &Path) -> Option<PathBuf> {
    let isa_id = fragment_id.strip_suffix(".md")?.to_string() + "_isabelle.md";
    let candidate = root.join(&isa_id);
    candidate.is_file().then_some(candidate)
}

/// Render-and-grep: every selected fragment must resolve on disk (the bridge
/// would otherwise raise at render time). For the backend-routed fragments
/// (those WITH an `_isabelle` sibling), assert that on an isabelle run the
/// SELECTED one is the `_isabelle` variant AND its content holds no Lean token.
fn assert_routed_fragments_isabelle_clean(fragments: &[String], role: &str) {
    let root = fragment_root();
    for fragment_id in fragments {
        let path = root.join(fragment_id);
        let body = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "{role}: selected fragment {fragment_id} does not resolve on disk ({}): {err}",
                path.display()
            )
        });
        // If this fragment is the LEAN variant of a backend-routed pair, it
        // must not have been selected on an isabelle run.
        if !fragment_id.ends_with("_isabelle.md") {
            assert!(
                isabelle_sibling_path(fragment_id, &root).is_none(),
                "{role}: Lean variant {fragment_id} selected on an isabelle run \
                 (its _isabelle sibling exists but was not routed)"
            );
            // Not a backend-routed fragment (no isabelle sibling) → deferred
            // shared doc; skip the Lean-token grep.
            continue;
        }
        for line in body.lines() {
            // A negated contrastive mention (e.g. the isabelle FILESPEC's
            // "There is no `-- BODY` marker") is correct Isabelle guidance, not
            // contamination — skip lines that explicitly say the Lean marker is
            // absent.
            if line.contains("no `-- BODY`") {
                continue;
            }
            for tok in LEAN_TOKENS {
                assert!(
                    !line.contains(tok),
                    "{role}: routed isabelle fragment {fragment_id} contains Lean token {tok:?}\n--- line ---\n{line}\n--- full body ---\n{body}"
                );
            }
        }
    }
}

fn write_isabelle_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("Tablet")).unwrap();
    std::fs::write(
        dir.path().join("trellis.config.json"),
        serde_json::to_string(&serde_json::json!({
            "workflow": {"default_target": "isabelle_hol"}
        }))
        .unwrap(),
    )
    .unwrap();
    dir
}

/// A proof-formalization worker request in `restructure` scope with
/// `allow_new_obligations=false` + `must_close_active=true`, so the routed
/// proof gates (06/07), the restructure scope fragment (05), the operational/
/// failure-triage/helper-decomposition guidance (10/15/20), and the
/// always-selected authority/scratchpad/outcomes fragments are all selected —
/// i.e. the worker's Lean `sorry` source in one shot.
fn maximal_proof_worker_request() -> WrapperRequest {
    WrapperRequest {
        kind: RequestKind::Worker,
        phase: Phase::ProofFormalization,
        worker_context: WorkerContext {
            worker_profile: WorkerProfile::ProofHard,
            validation_kind: WorkerValidationKind::ProofRestructure,
            allow_new_obligations: false,
            must_close_active: true,
            ..WorkerContext::default()
        },
        ..WrapperRequest::default()
    }
}

#[test]
fn isabelle_worker_prompt_has_no_lean_tokens() {
    let repo = write_isabelle_repo();
    let req = maximal_proof_worker_request();
    let fragments = fragments_of(&worker_contract_payload(&req, Some(repo.path())));
    // The proof gates + restructure scope + guidance + scratchpad + 45_outcomes
    // + 20_authority are the worker's Lean `sorry` source; confirm the isabelle
    // siblings were routed in (not the Lean ones).
    for expected in [
        "worker/proof_formalization/06_gate_no_new_obligations_isabelle.md",
        "worker/proof_formalization/07_gate_must_close_active_isabelle.md",
        "worker/proof_formalization/05_scope_restructure_isabelle.md",
        "worker/proof_formalization/15_failure_triage_isabelle.md",
        "worker/proof_formalization/20_helper_decomposition_isabelle.md",
        "worker/common/20_authority_isabelle.md",
        "worker/common/31_scratchpad_isabelle.md",
        "worker/common/45_outcomes_isabelle.md",
    ] {
        assert!(
            fragments.iter().any(|f| f == expected),
            "expected isabelle fragment {expected}; got {fragments:?}"
        );
    }
    // And NONE of their Lean originals leak in.
    for lean in [
        "worker/proof_formalization/07_gate_must_close_active.md",
        "worker/common/45_outcomes.md",
        "shared/25_filespec.md",
        "shared/20_read_files.md",
    ] {
        assert!(
            fragments.iter().all(|f| f != lean),
            "Lean fragment {lean} leaked into the isabelle worker prompt; got {fragments:?}"
        );
    }
    assert_routed_fragments_isabelle_clean(&fragments, "worker(isabelle)");
}

#[test]
fn isabelle_reviewer_prompt_has_no_lean_tokens() {
    let repo = write_isabelle_repo();
    for phase in [
        Phase::TheoremStating,
        Phase::ProofFormalization,
        Phase::Cleanup,
    ] {
        let req = WrapperRequest {
            kind: RequestKind::Review,
            phase,
            ..WrapperRequest::default()
        };
        let fragments = fragments_of(&review_contract_payload(&req, Some(repo.path())));
        assert!(
            fragments.iter().any(|f| f == "shared/25_filespec_isabelle.md"),
            "reviewer ({phase:?}) should route the isabelle FILESPEC; got {fragments:?}"
        );
        assert_routed_fragments_isabelle_clean(&fragments, "reviewer(isabelle)");
    }
}

#[test]
fn isabelle_verifier_prompts_have_no_lean_tokens() {
    let repo = write_isabelle_repo();

    let mut paper = WrapperRequest {
        kind: RequestKind::Paper,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    paper.paper_verify_targets = std::iter::once(TargetId::from("T1")).collect();
    let paper_frags = fragments_of(&paper_contract_payload(&paper, Some(repo.path())));
    assert!(
        paper_frags
            .iter()
            .any(|f| f == "shared/20_read_files_isabelle.md"),
        "paper verifier should route the isabelle read-files; got {paper_frags:?}"
    );
    assert_routed_fragments_isabelle_clean(&paper_frags, "verifier/paper(isabelle)");

    let corr = WrapperRequest {
        kind: RequestKind::Corr,
        phase: Phase::ProofFormalization,
        ..WrapperRequest::default()
    };
    let corr_frags = fragments_of(&correspondence_contract_payload(&corr, Some(repo.path())));
    // The corr scratchpad is an operative `lake env lean …probe.lean`
    // instruction on Lean; on isabelle it must route to the `_isabelle`
    // sibling, and the Lean original must not leak in.
    assert!(
        corr_frags
            .iter()
            .any(|f| f == "verifier/correspondence/07_scratchpad_isabelle.md"),
        "corr verifier should route the isabelle scratchpad; got {corr_frags:?}"
    );
    assert!(
        corr_frags
            .iter()
            .all(|f| f != "verifier/correspondence/07_scratchpad.md"),
        "Lean corr scratchpad leaked into the isabelle corr prompt; got {corr_frags:?}"
    );
    assert_routed_fragments_isabelle_clean(&corr_frags, "verifier/corr(isabelle)");
    // Belt-and-suspenders: the *concatenated* rendered corr fragment corpus
    // must hold no `lake env lean` operative command (the must-fix this slice
    // closes), regardless of which fragment it lived in.
    let root = fragment_root();
    for fragment_id in &corr_frags {
        let body = std::fs::read_to_string(root.join(fragment_id)).unwrap_or_default();
        assert!(
            !body.contains("lake env lean"),
            "corr(isabelle) fragment {fragment_id} still carries `lake env lean`:\n{body}"
        );
    }

    let sound = WrapperRequest {
        kind: RequestKind::Sound,
        phase: Phase::ProofFormalization,
        ..WrapperRequest::default()
    };
    let sound_frags = fragments_of(&soundness_contract_payload(&sound, Some(repo.path())));
    assert_routed_fragments_isabelle_clean(&sound_frags, "verifier/sound(isabelle)");
}

/// Behavior-preservation: a Lean repo (target set) and the no-repo default both
/// produce the SAME fragment list — the Lean arm of every `isa_or_lean` is
/// unchanged, so no Lean prompt drifts.
#[test]
fn lean_fragment_lists_match_no_repo_default() {
    let lean_dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(lean_dir.path().join("Tablet")).unwrap();
    std::fs::write(
        lean_dir.path().join("trellis.config.json"),
        serde_json::to_string(&serde_json::json!({
            "workflow": {"default_target": "lean"}
        }))
        .unwrap(),
    )
    .unwrap();
    let lean = Some(lean_dir.path());

    // worker
    let wreq = maximal_proof_worker_request();
    assert_eq!(
        fragments_of(&worker_contract_payload(&wreq, lean)),
        fragments_of(&worker_contract_payload(&wreq, None)),
        "lean worker fragment list drifted from the no-repo default"
    );

    // reviewer
    for phase in [Phase::TheoremStating, Phase::ProofFormalization, Phase::Cleanup] {
        let rreq = WrapperRequest {
            kind: RequestKind::Review,
            phase,
            ..WrapperRequest::default()
        };
        assert_eq!(
            fragments_of(&review_contract_payload(&rreq, lean)),
            fragments_of(&review_contract_payload(&rreq, None)),
            "lean reviewer ({phase:?}) fragment list drifted from the no-repo default"
        );
    }

    // verifiers
    let mut paper = WrapperRequest {
        kind: RequestKind::Paper,
        phase: Phase::TheoremStating,
        ..WrapperRequest::default()
    };
    paper.paper_verify_targets = std::iter::once(TargetId::from("T1")).collect();
    assert_eq!(
        fragments_of(&paper_contract_payload(&paper, lean)),
        fragments_of(&paper_contract_payload(&paper, None)),
    );
    let corr = WrapperRequest {
        kind: RequestKind::Corr,
        phase: Phase::ProofFormalization,
        ..WrapperRequest::default()
    };
    assert_eq!(
        fragments_of(&correspondence_contract_payload(&corr, lean)),
        fragments_of(&correspondence_contract_payload(&corr, None)),
    );
    let sound = WrapperRequest {
        kind: RequestKind::Sound,
        phase: Phase::ProofFormalization,
        ..WrapperRequest::default()
    };
    assert_eq!(
        fragments_of(&soundness_contract_payload(&sound, lean)),
        fragments_of(&soundness_contract_payload(&sound, None)),
    );
}

/// Sanity: the resolver root actually exists where the test expects it.
#[test]
fn fragment_root_resolves() {
    assert!(
        fragment_root().join("shared/25_filespec_isabelle.md").is_file(),
        "fragment_root() does not point at prompt_fragments/: {}",
        fragment_root().display()
    );
    let _ = Path::new(".");
}
