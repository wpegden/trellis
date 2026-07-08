//! Tier 3 (plan §5.9) — fixture-based smoke tests for the Patch A
//! local-closure probe.
//!
//! Most probe tests here are skip-guarded rather than `#[ignore]`d: they
//! run on every `cargo test` and, via `fixture_unready_reason`, EXECUTE
//! when the fixture is built (this worktree, and any CI lane that runs
//! `lake build` under the fixture root first) and pass vacuously with a
//! clear skip message otherwise. That is what lets them catch a real
//! classifier regression of `isTabletGeneratedArtifact`. The integration
//! tests that mutate a fixture file and rerun `lake build` stay
//! `#[ignore]`d (they are too heavy and stateful for the default lane).
//!
//! Requirements when the skip-guarded tests do run:
//!
//! 1. An operator-built fixture under
//!    `kernel/tests/fixtures/local_closure_smoke/`. Build steps:
//!    ```
//!    cd kernel/tests/fixtures/local_closure_smoke
//!    lake build
//!    ```
//!    (No `lake exe cache get` needed — the fixture intentionally avoids
//!    Mathlib so the build is fast; only stdlib oleans are required.)
//!
//! 2. Either `lean` on `$PATH` (which the build above provides via the
//!    `lean-toolchain` file) or an explicit `TRELLIS_FIXTURE_LEAN` env
//!    var pointing at the lean binary.
//!
//! Run the skip-guarded lane (executes when the fixture is built):
//!
//! ```
//! (cd kernel/tests/fixtures/local_closure_smoke && lake build)
//! cargo test -p trellis-kernel --test local_closure_smoke
//! ```
//!
//! Run the `#[ignore]`d mutation integration tests too:
//!
//! ```
//! cargo test -p trellis-kernel local_closure_smoke -- --ignored --nocapture
//! ```
//!
//! These tests do NOT involve the live checker server (forbidden by the
//! current task's constraints — the supervisor is busy with a live run).
//! Instead they invoke the script directly via `lake env lean --run`,
//! mirroring what the server's `_handle_local_closure_axioms` does
//! internally. The Python-side wrapping is exercised by Tier 2.
//!
//! The DTO parsing exercised here is the same `parse_local_closure_response`
//! that `run_local_closure_axioms` uses on the server's verbatim envelope —
//! Tier 1 covers the parser's branches, this tier confirms the *script*
//! emits envelopes that those branches accept.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

mod common;
use common::project_tempdir;

/// Serialize tests that mutate the fixture or rebuild it. Cargo test
/// runs `#[test]` functions in parallel by default; the inductive
/// hash-change test (Patch C-K Fix 2) edits `Tablet/InductiveNat.lean`
/// and re-runs `lake build`, which races with concurrent probes from
/// the other smoke tests. The Mutex guarantees one-at-a-time execution
/// of any test that takes its guard.
fn fixture_mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Locate the fixture root relative to the kernel crate. We use
/// `CARGO_MANIFEST_DIR` so the path is stable regardless of where
/// `cargo test` is invoked from.
fn fixture_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("tests")
        .join("fixtures")
        .join("local_closure_smoke")
}

/// Locate the local-closure script. It lives at
/// `<repo>/scripts/lean_local_closure.lean`. We resolve via
/// `CARGO_MANIFEST_DIR`'s parent (the workspace root).
fn local_closure_script() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()
        .expect("kernel crate has a parent (workspace root)")
        .to_path_buf();
    workspace_root
        .join("scripts")
        .join("lean_local_closure.lean")
}

/// Decide whether the local-closure probe can actually run: the `lake`
/// build tool must be on `$PATH` and the fixture's `.olean` tree must
/// already be built. Returns `None` when ready; `Some(reason)` describing
/// the gap otherwise.
///
/// This is the Item 4 guard against a silent classifier regression. The
/// acceptance tests below are NOT `#[ignore]`d: when the fixture is built
/// (this worktree, and any CI lane that runs `lake build` under the
/// fixture root first), they EXECUTE and so FAIL on a real regression of
/// `isTabletGeneratedArtifact` back to name-prefixes. When the fixture is
/// absent (a checkout that hasn't built it), they print a clear skip
/// message and pass vacuously rather than panicking — the same envelope
/// the operator workflow expects. To force the guard locally, build the
/// fixture first:
///
/// ```
/// (cd kernel/tests/fixtures/local_closure_smoke && lake build)
/// cargo test -p trellis-kernel --test local_closure_smoke
/// ```
fn fixture_unready_reason() -> Option<String> {
    if which_lake().is_none() {
        return Some("`lake` not on $PATH; skipping local-closure probe".to_string());
    }
    let root = fixture_root();
    if !root.exists() {
        return Some(format!("fixture root missing: {}", root.display()));
    }
    // A representative olean: if `Tablet.olean` (the library root) is
    // present the `lake build` ran. We probe a leaf olean too so a
    // half-built tree is treated as not-ready.
    let lib = root.join(".lake").join("build").join("lib").join("lean");
    for required in &[
        "Tablet.olean",
        "Tablet/EnumColor.olean",
        "Tablet/Owner.olean",
        // FIX 1 / FIX 2 fixtures: ensure the soundness-fix lane treats a
        // tree built before these fixtures landed as not-ready (so the
        // forge/no-false-positive tests don't vacuously skip).
        "Tablet/AxiomForge.olean",
        "Tablet/ForgeProtectedEq.olean",
        "Tablet/RecDef.olean",
        // FIX 2 COVERAGE fixtures (Definition-kind authoring surfaces): a
        // tree built before these landed must be treated as not-ready so
        // the coverage tests don't vacuously skip.
        "Tablet/DefForgeLetRec.olean",
        "Tablet/StructForgeAux.olean",
        "Tablet/ClassForgeAux.olean",
        // FIX 2 parse-robustness fixtures (forge after imported notation /
        // custom command): same not-ready guard so the robustness tests
        // don't vacuously skip on a stale tree.
        "Tablet/ForgeAfterNotation.olean",
        "Tablet/ForgeAfterCommand.olean",
        // MACRO BAN fixtures (macro/syntax/elaborator-defining command ban):
        // a tree built before these landed must be treated as not-ready so
        // the macro-ban tests don't vacuously skip.
        "Tablet/MacroEqForge.olean",
        "Tablet/PlainMacro.olean",
        "Tablet/PlainElab.olean",
        "Tablet/LegitNotation.olean",
        // NAMESPACED-ROOT FIX fixture: a tree built before this landed must
        // be treated as not-ready so the namespaced-root test doesn't
        // vacuously skip.
        "Tablet/NamespacedRoot.olean",
        // NAMESPACED-DEP FIX fixtures (a namespaced consumer + its namespaced
        // deps): a tree built before these landed must be treated as
        // not-ready so the namespaced-dep test doesn't vacuously skip.
        "Tablet/NamespacedThm.olean",
        "Tablet/NamespacedStrictThm.olean",
        "Tablet/NamespacedDef.olean",
        "Tablet/NamespacedConsumer.olean",
        // NAMESPACED-FORGE soundness fixtures + AMBIGUOUS-ROOT fixture: a tree
        // built before these landed must be treated as not-ready so the
        // soundness/ambiguity tests don't vacuously skip.
        "Tablet/NamespacedForgeAux.olean",
        "Tablet/UsesNamespacedForgeAux.olean",
        // DEEPLY-NAMESPACED (flattened-stem) regression fixtures: a tree built
        // before the `declMatchesStem` fix landed must be treated as not-ready
        // so the flattened-stem dep + soundness tests don't vacuously skip.
        "Tablet/Nested_method.olean",
        "Tablet/UsesNestedMethod.olean",
        "Tablet/Nested_Forge.olean",
        "Tablet/UsesNestedForge.olean",
        "Tablet/AmbiguousRoot.olean",
    ] {
        let p = lib.join(required);
        if !p.exists() {
            return Some(format!(
                "fixture not built ({} missing). Run `cd {} && lake build` first.",
                p.display(),
                root.display(),
            ));
        }
    }
    None
}

/// Locate `lake` on `$PATH` without spawning it (cheap pre-check).
fn which_lake() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("lake");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Print the skip reason and return `true` when the fixture cannot run, so
/// callers can `if fixture_skip("name") { return; }`. Centralizes the
/// skip-with-clear-message behaviour.
fn fixture_skip(test: &str) -> bool {
    if let Some(reason) = fixture_unready_reason() {
        eprintln!("SKIP {test}: {reason}");
        true
    } else {
        false
    }
}

/// Run `lake env lean --run scripts/lean_local_closure.lean <node>
/// [extra_args...]` in the fixture root and return (stdout, stderr, exit
/// status). Mirrors the command shape used by
/// `_handle_local_closure_axioms` server-side.
fn run_probe_with_args(
    node: &str,
    extra_args: &[&str],
) -> Result<(String, String, Option<i32>), String> {
    let root = fixture_root();
    let script = local_closure_script();
    if !script.exists() {
        return Err(format!(
            "local-closure script missing: {}",
            script.display()
        ));
    }
    let output = Command::new("lake")
        .arg("env")
        .arg("lean")
        .arg("--run")
        .arg(&script)
        .arg(node)
        .args(extra_args)
        .current_dir(&root)
        .output()
        .map_err(|e| format!("lake env lean failed to spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((stdout, stderr, output.status.code()))
}

fn run_probe(node: &str) -> Result<(String, String, Option<i32>), String> {
    run_probe_with_args(node, &[])
}

/// FIX 2 coverage: invoke the Lean script in `--scan-only` mode — the
/// universal owner-file authoring gate that runs for every node kind
/// (including Definition-kind nodes the full closure probe skips). Mirrors
/// the kernel's `run_local_closure_owner_scan` invocation shape.
fn run_probe_scan_only(node: &str) -> Result<(String, String, Option<i32>), String> {
    run_probe_with_args(node, &["--scan-only"])
}

/// Extract the last non-empty line of stdout as the JSON envelope —
/// mirrors the server's parsing behaviour at server.py:1998-2008.
fn extract_json_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|s| s.trim().to_string())
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // strict_definition_deps is read only by the `#[ignore]` tests.
struct ProbeEnvelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    root_kind: String,
    #[serde(default)]
    kernel_axioms: Vec<String>,
    #[serde(default)]
    boundary_theorems: Vec<serde_json::Value>,
    #[serde(default)]
    strict_theorem_deps: Vec<serde_json::Value>,
    #[serde(default)]
    strict_definition_deps: Vec<serde_json::Value>,
    #[serde(default)]
    errors: Vec<String>,
    /// Plan §4.6.1 dual-collector cross-check. The merged script always
    /// emits this sub-object (even on early failures, where it carries
    /// `skipped: true`). The `#[ignore]`d fixture tests assert
    /// `agreed == true` on every fixture node to enforce the
    /// runtime invariant against the live Lean elaborator.
    #[serde(default)]
    axiomization_check: Option<AxiomizationCheckSummary>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct AxiomizationCheckSummary {
    #[serde(default)]
    kernel_axioms: Vec<String>,
    #[serde(default)]
    boundary_theorems: Vec<String>,
    #[serde(default)]
    agreed: bool,
    #[serde(default)]
    skipped: bool,
    #[serde(default)]
    primary_only_axioms: Vec<String>,
    #[serde(default)]
    axcheck_only_axioms: Vec<String>,
    #[serde(default)]
    primary_only_boundaries: Vec<String>,
    #[serde(default)]
    axcheck_only_boundaries: Vec<String>,
}

fn assert_axcheck_agreed(env: &ProbeEnvelope, node: &str) {
    let ax = env
        .axiomization_check
        .as_ref()
        .unwrap_or_else(|| panic!("{node}: axiomization_check missing from envelope: {env:?}"));
    assert!(
        !ax.skipped,
        "{node}: axiomization_check should run by default (skipped={}); envelope: {env:?}",
        ax.skipped,
    );
    assert!(
        ax.agreed,
        "{node}: axiomization cross-check disagrees with primary collector. \
         primary_only_axioms={:?}, axcheck_only_axioms={:?}, \
         primary_only_boundaries={:?}, axcheck_only_boundaries={:?}",
        ax.primary_only_axioms,
        ax.axcheck_only_axioms,
        ax.primary_only_boundaries,
        ax.axcheck_only_boundaries,
    );
}

fn parse_envelope(stdout: &str, stderr: &str) -> ProbeEnvelope {
    let line = extract_json_line(stdout).unwrap_or_else(|| {
        panic!("no JSON line on stdout; stdout=<<<{stdout}>>> stderr=<<<{stderr}>>>")
    });
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("parse JSON line failed: {e}; line={line}"))
}

const CANONICAL_FOUR: &[&str] = &["propext", "funext", "Classical.choice", "Quot.sound"];

fn axiom_subset_of_canonical_four(axioms: &[String]) -> bool {
    axioms.iter().all(|a| CANONICAL_FOUR.contains(&a.as_str()))
}

fn boundary_names(boundaries: &[serde_json::Value]) -> Vec<String> {
    boundaries
        .iter()
        .filter_map(|v| {
            v.get("name")
                .and_then(serde_json::Value::as_str)
                .map(|s| s.to_string())
        })
        .collect()
}

#[test]
fn local_closure_smoke_closed_node_passes_clean() {
    // Plan §5.9 Tier 3: `Closed` has no Tablet helpers and no `sorry`.
    // Probe must report `status=ok`, `kernel_axioms` ⊆ canonical four,
    // empty `boundary_theorems`, no errors.
    if fixture_skip("closed_node_passes_clean") {
        return;
    }
    let (stdout, stderr, code) = run_probe("Closed").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for Closed; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "Closed should probe ok; envelope: {env:?}"
    );
    assert_eq!(env.root_kind, "theorem");
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "Closed axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );
    assert!(
        env.boundary_theorems.is_empty(),
        "Closed has no Tablet helpers; got {:?}",
        env.boundary_theorems,
    );
    assert!(
        env.strict_theorem_deps.is_empty(),
        "Closed has no strict theorem deps; got {:?}",
        env.strict_theorem_deps,
    );
    assert!(
        env.errors.is_empty(),
        "Closed should have no errors; got {:?}",
        env.errors,
    );
    // Plan §4.6.1 dual-collector invariant: secondary axiomization
    // collector must produce the same set as the primary.
    assert_axcheck_agreed(&env, "Closed");
}

#[test]
fn local_closure_smoke_uses_helper_records_boundary() {
    // Plan §2.2 / §4.3 boundary cut: `UsesHelper` proof references
    // `Helper` (which carries `sorryAx`). The probe must record `Helper`
    // as a boundary theorem and stop at its statement, NOT walking its
    // `value`. Result: `kernel_axioms` ⊆ canonical four (no `sorryAx`
    // leakage from `Helper.value`), `boundary_theorems` ∋ `Helper`.
    if fixture_skip("uses_helper_records_boundary") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesHelper").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesHelper; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "ok");
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "UsesHelper kernel_axioms must be ⊆ canonical four (boundary cut hides Helper.value's sorryAx); got {:?}",
        env.kernel_axioms,
    );
    let boundaries = boundary_names(&env.boundary_theorems);
    assert!(
        boundaries
            .iter()
            .any(|n| n == "Tablet.Helper" || n == "Helper"),
        "UsesHelper must record Helper as a boundary; got {:?}",
        boundaries,
    );
    assert_axcheck_agreed(&env, "UsesHelper");
}

#[test]
fn local_closure_smoke_reserved_generated_artifact_is_transparent() {
    // `UsesReservedArtifact` explicitly references
    // `ReservedArtifactDef.congr_simp`, a Lean-reserved generated theorem.
    // The collector must transparent-walk that artifact rather than
    // recording `ReservedArtifactDef.congr_simp` as a Tablet boundary key.
    //
    // Adversarial point: the real authored dependency is the definition
    // `ReservedArtifactDef`, and it MUST still be recorded under
    // strict_definition_deps. Filtering must not hide the dependency from
    // invalidation.
    if fixture_skip("reserved_generated_artifact_is_transparent") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesReservedArtifact").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesReservedArtifact; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesReservedArtifact should probe ok; envelope: {env:?}"
    );
    let boundaries = boundary_names(&env.boundary_theorems);
    assert!(
        !boundaries
            .iter()
            .any(|n| n.ends_with(".congr_simp") || n.contains("congr_simp")),
        "reserved generated theorem must not be recorded as boundary; got {:?}",
        boundaries,
    );
    assert!(
        strict_def_hash(&env, "ReservedArtifactDef").is_some(),
        "transparent walk through congr_simp must still record the real \
         definition dependency; envelope: {env:?}",
    );
    assert_axcheck_agreed(&env, "UsesReservedArtifact");
}

#[test]
fn local_closure_smoke_active_sorry_surfaces_sorry_ax() {
    // The active node's proof itself is `by sorry`. Walking the value
    // under `ProofMayAssumeTheorems` reaches `sorryAx` (a kernel
    // axiom). Result: `kernel_axioms` ∋ `sorryAx`. Patch A is
    // observation-only so we don't fail the probe — Patch B's gate
    // will reject this when `must_close_active = true`.
    if fixture_skip("active_sorry_surfaces_sorry_ax") {
        return;
    }
    let (stdout, stderr, code) = run_probe("ActiveSorry").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 (probe reports the violation in JSON, not via exit code); stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "ok");
    assert!(
        env.kernel_axioms
            .iter()
            .any(|a| a == "sorryAx" || a == "Lean.sorryAx" || a.contains("sorryAx")),
        "ActiveSorry must surface sorryAx; got {:?}",
        env.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "ActiveSorry");
}

#[test]
fn local_closure_smoke_unmappable_name_fails_closed() {
    // The wrapper's contract: when the script can't find the named
    // declaration, the script returns `status = missing_declaration`
    // (or `internal_error`) and the wrapper surfaces that intact. The
    // gate code in Patch B treats any non-`ok` status as fail-closed.
    if fixture_skip("unmappable_name_fails_closed") {
        return;
    }
    let (stdout, stderr, code) = run_probe("ThisNodeDoesNotExist").expect("run probe");
    // Exit code may be 0 (script exited cleanly with structured error)
    // or non-zero (Lean elaboration error before main ran). Both are
    // valid fail-closed paths. Either way, the envelope's status MUST
    // NOT be `ok`.
    let _ = code;
    let _ = stderr;
    if let Some(line) = extract_json_line(&stdout) {
        if let Ok(env) = serde_json::from_str::<ProbeEnvelope>(&line) {
            assert_ne!(
                env.status, "ok",
                "fictitious name must NOT report ok; envelope={env:?}",
            );
            // The script is expected to report `missing_declaration`
            // or `elaboration_error`; we don't pin the exact string to
            // keep the test robust against the script's diagnostic
            // wording.
            return;
        }
    }
    // No JSON line at all is also a valid fail-closed signal — the
    // wrapper's `parse_local_closure_response` will compose an
    // internal-error envelope for it (Tier 1 covers that branch).
}

#[test]
fn local_closure_smoke_fixture_files_exist() {
    // This is the only test in this file that is NOT `#[ignore]`d —
    // it exercises only the on-disk fixture layout, no lean
    // invocation required, so it serves as a fast smoke test that
    // the fixture wasn't accidentally deleted from the tree.
    let root = fixture_root();
    assert!(root.exists(), "fixture root missing: {}", root.display());
    for required in &[
        "lakefile.lean",
        "lean-toolchain",
        "Tablet/Preamble.lean",
        "Tablet/Helper.lean",
        "Tablet/Closed.lean",
        "Tablet/UsesHelper.lean",
        "Tablet/ActiveSorry.lean",
        "Tablet/ReservedArtifactDef.lean",
        "Tablet/UsesReservedArtifact.lean",
        "Tablet/StructFields.lean",
        "Tablet/UsesStructField.lean",
        "Tablet/EnumColor.lean",
        "Tablet/UsesEnumCtor.lean",
        "Tablet/UsesCtorInjEq.lean",
        "Tablet/ClassMethod.lean",
        "Tablet/UsesClassMethod.lean",
        "Tablet/DiamondChild.lean",
        "Tablet/UsesDiamondParent.lean",
        "Tablet/Owner.lean",
        "Tablet/OwnerAux.lean",
        "Tablet/UsesOwnerAux.lean",
        "Tablet/OwnerToCtorIdxAux.lean",
        "Tablet/UsesOwnerToCtorIdx.lean",
        // FIX 1 (reserved-shaped axiom transparent-walk) fixtures.
        "Tablet/AxiomForge.lean",
        "Tablet/UsesAxiomForge.lean",
        // FIX 2 (owner-file reserved-shaped authored-name) forge fixtures.
        "Tablet/ForgeBareEq.lean",
        "Tablet/ForgeProtectedEq.lean",
        "Tablet/ForgePrivateEq.lean",
        "Tablet/ForgeUnderscore.lean",
        "Tablet/ForgeNamespace.lean",
        "Tablet/ForgeWhere.lean",
        // FIX 2 COVERAGE fixtures (Definition-kind authoring surfaces the
        // full closure probe never scans: `let rec`-bound, structure-aux,
        // class-aux, plus a consumer of the `where`-bound Definition forge).
        "Tablet/DefForgeLetRec.lean",
        "Tablet/StructForgeAux.lean",
        "Tablet/ClassForgeAux.lean",
        "Tablet/UsesDefForgeWhere.lean",
        // FIX 2 no-false-positive fixtures.
        "Tablet/RecDef.lean",
        "Tablet/UsesRecDef.lean",
        "Tablet/LegitAux.lean",
        // MACRO BAN fixtures: macro-forge (the residual) + consumer, plain
        // macro/elab, and the legit notation no-false-positive node.
        "Tablet/MacroEqForge.lean",
        "Tablet/UsesMacroEqForge.lean",
        "Tablet/PlainMacro.lean",
        "Tablet/PlainElab.lean",
        "Tablet/LegitNotation.lean",
    ] {
        let p = root.join(required);
        assert!(
            p.exists(),
            "fixture file missing: {}; expected at {}",
            required,
            p.display(),
        );
    }
    let script = local_closure_script();
    assert!(
        script.exists(),
        "local-closure script missing: {}",
        script.display(),
    );
}

#[test]
fn local_closure_smoke_helper_node_carries_sorry_textually() {
    // Cross-check: the `Helper.lean` fixture really does have `sorry`
    // in its source, so the boundary-cut test would actually be
    // demonstrating the gap (rather than a vacuous pass against a
    // helper that happens to be already closed).
    let helper = fixture_root().join("Tablet/Helper.lean");
    let text = std::fs::read_to_string(&helper).expect("read Helper.lean");
    assert!(
        text.contains("sorry"),
        "Helper.lean must carry `sorry` for the boundary-cut test to be meaningful",
    );
}

#[test]
fn local_closure_smoke_active_sorry_carries_sorry_textually() {
    let active = fixture_root().join("Tablet/ActiveSorry.lean");
    let text = std::fs::read_to_string(&active).expect("read ActiveSorry.lean");
    assert!(
        text.contains("sorry"),
        "ActiveSorry.lean must carry `sorry` for the sorryAx test to be meaningful",
    );
}

#[test]
fn local_closure_smoke_reserved_artifact_fixture_references_reserved_name() {
    let uses = fixture_root().join("Tablet/UsesReservedArtifact.lean");
    let text = std::fs::read_to_string(&uses).expect("read UsesReservedArtifact.lean");
    assert!(
        text.contains("ReservedArtifactDef.congr_simp"),
        "fixture must explicitly force realization of the reserved generated theorem",
    );
    assert!(
        !text.contains("theorem ReservedArtifactDef.congr_simp"),
        "fixture must not author the reserved theorem; Lean should realize it",
    );
}

#[test]
fn local_closure_smoke_closed_node_is_sorry_free() {
    // The "negative" sanity check: `Closed.lean` must NOT contain
    // `sorry` so its passing the probe is non-trivial. Strip
    // line-comments before scanning so the test is robust against
    // doc-comments that mention forbidden keywords.
    let closed = fixture_root().join("Tablet/Closed.lean");
    let text = std::fs::read_to_string(&closed).expect("read Closed.lean");
    let code: String = text
        .lines()
        .map(|line| {
            // Strip everything from `--` onwards on each line. This
            // is a coarse cut (won't handle block comments or
            // strings) but Closed.lean uses neither.
            line.split("--").next().unwrap_or("").to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("sorry"),
        "Closed.lean must be sorry-free for the closed-node test to be meaningful",
    );
    // Must not have a top-level `axiom Foo : ...` declaration. Allow
    // the word inside doc-comments (which we already stripped) but
    // also guard against an inline-axiom declaration.
    assert!(
        !code.contains("axiom "),
        "Closed.lean must not introduce a project axiom; non-comment code contains `axiom `",
    );
}

/// Patch C-K Fix 2 (audit MEDIUM-HIGH): the Lean local-closure
/// inductive semantic hash must mix constructor types, not just
/// constructor names. This `#[ignore]`d test mutates the fixture
/// `Tablet/InductiveNat.lean` between two probe invocations and
/// asserts the `strict_definition_deps[InductiveNat]` hash differs
/// across the mutation. Under the PRE-FIX hashing rule (type + ctor
/// names only), the hash would be stable because `v.type` is `Type`
/// for both variants and the ctor name `mk` is identical; the
/// constructor's parameter type `Nat` vs `Bool` was NOT mixed in.
/// The fix mixes each ctor's type into the same `hashExprs` list as
/// the inductive type itself, in deterministic ctor order.
///
/// Test plan:
/// 1. Probe `UsesInductive` against the original `InductiveNat` (ctor
///    type `Nat → InductiveNat`). Capture H1.
/// 2. Mutate `InductiveNat.lean` to `mk : Bool → InductiveNat`.
/// 3. Rebuild the fixture (`lake build`).
/// 4. Probe `UsesInductive` again. Capture H2.
/// 5. Restore `InductiveNat.lean` (best-effort cleanup; the rebuild
///    leaves the .olean tree in the Bool variant but the source is
///    restored so subsequent runs start from the canonical state).
/// 6. Assert H1 ≠ H2.
///
/// This is an integration test (mutates files, invokes lake) so it
/// stays `#[ignore]`d alongside the other smoke tests. Operators run
/// it manually with `cargo test --test local_closure_smoke -- --ignored`.
#[test]
#[ignore = "requires operator-built fixture; mutates Tablet/InductiveNat.lean and rebuilds — see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn inductive_constructor_type_change_changes_semantic_hash() {
    // Serialize against any other test that might race with our
    // fixture mutation + rebuild. Other smoke tests in this binary
    // don't probe InductiveNat, but `lake build` can contend with
    // concurrent `lake env lean` invocations on the .lake/ tree.
    let _serialize = fixture_mutation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let inductive_path = fixture_root().join("Tablet/InductiveNat.lean");
    let original = std::fs::read_to_string(&inductive_path).expect("read InductiveNat.lean");
    // Defensive precondition: the fixture must carry the canonical
    // Nat-flavoured constructor so the test's mutation step is the
    // ONLY thing changing the ctor type.
    assert!(
        original.contains("mk : Nat → InductiveNat"),
        "fixture InductiveNat.lean must declare `mk : Nat → InductiveNat`; got:\n{original}",
    );

    // Step 1: probe with the original (Nat) constructor.
    let (stdout1, stderr1, code1) = run_probe("UsesInductive").expect("probe Nat variant");
    assert_eq!(
        code1,
        Some(0),
        "probe must exit 0 for UsesInductive (Nat variant); stderr=<<<{stderr1}>>>"
    );
    let env1 = parse_envelope(&stdout1, &stderr1);
    assert_eq!(env1.status, "ok", "Nat variant probe must report ok");
    let h1 = strict_def_hash(&env1, "InductiveNat").unwrap_or_else(|| {
        panic!("Nat variant: strict_definition_deps[InductiveNat] missing; envelope: {env1:?}")
    });

    // Step 2 + 3: mutate to Bool and rebuild. Restore on test failure
    // via a guard so the fixture isn't left in the Bool state if any
    // assertion below panics.
    let mutated = original.replace("mk : Nat → InductiveNat", "mk : Bool → InductiveNat");
    assert_ne!(
        mutated, original,
        "mutation step must change the file (otherwise the test is vacuous)",
    );
    let _guard = FixtureRestoreGuard {
        path: inductive_path.clone(),
        original: original.clone(),
    };
    std::fs::write(&inductive_path, &mutated).expect("write Bool variant");
    let build_status = Command::new("lake")
        .arg("build")
        .current_dir(fixture_root())
        .status()
        .expect("lake build (Bool variant)");
    assert!(
        build_status.success(),
        "lake build must succeed for the Bool-mutated fixture; status: {build_status:?}",
    );

    // Step 4: probe with the Bool constructor.
    let (stdout2, stderr2, code2) = run_probe("UsesInductive").expect("probe Bool variant");
    assert_eq!(
        code2,
        Some(0),
        "probe must exit 0 for UsesInductive (Bool variant); stderr=<<<{stderr2}>>>"
    );
    let env2 = parse_envelope(&stdout2, &stderr2);
    assert_eq!(env2.status, "ok", "Bool variant probe must report ok");
    let h2 = strict_def_hash(&env2, "InductiveNat").unwrap_or_else(|| {
        panic!("Bool variant: strict_definition_deps[InductiveNat] missing; envelope: {env2:?}")
    });

    // Step 6: hashes must differ. The guard's Drop restores the file
    // and triggers a third `lake build` on the way out — we don't gate
    // the assertion on the restore-build success because a stale Bool
    // .olean is benign (next `lake build` will recompile).
    assert_ne!(
        h1, h2,
        "inductive ctor type change Nat → Bool must change the semantic hash \
         (Patch C-K Fix 2: ctor types are now mixed into the hash); \
         got h1={h1} h2={h2} for InductiveNat",
    );
}

/// Collect every dep `name` across all three dep maps of an envelope.
/// Used by the generated-member tests to assert (a) the structure /
/// inductive / class node is recorded as a real dependency and (b) no
/// generated `Owner.child` member leaked in as its own dep key.
fn all_dep_names(env: &ProbeEnvelope) -> Vec<String> {
    let mut names = boundary_names(&env.boundary_theorems);
    names.extend(boundary_names(&env.strict_theorem_deps));
    names.extend(boundary_names(&env.strict_definition_deps));
    names
}

/// True iff `node` appears among the recorded deps (with or without the
/// `Tablet.` prefix the raw script emits).
fn dep_recorded(env: &ProbeEnvelope, node: &str) -> bool {
    all_dep_names(env)
        .iter()
        .any(|n| n == node || n == &format!("Tablet.{node}"))
}

/// Run a consumer probe that is expected to accept a generated member of
/// `owner`: status `ok`, no `internal_error`, the owner node recorded as a
/// real dependency, and no dotted `owner.<member>` dep key leaked in.
fn assert_generated_member_accepted(node: &str, owner: &str) {
    let (stdout, stderr, code) = run_probe(node).expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for {node}; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_ne!(
        env.status, "internal_error",
        "{node}: generated member of {owner} must not trigger internal_error; envelope: {env:?}",
    );
    assert_eq!(env.status, "ok", "{node} should probe ok; envelope: {env:?}");
    assert!(
        dep_recorded(&env, owner),
        "{node}: dependency must resolve to the registered principal `{owner}`; deps: {:?}",
        all_dep_names(&env),
    );
    assert!(
        !all_dep_names(&env)
            .iter()
            .any(|n| n.starts_with(&format!("{owner}.")) || n.starts_with(&format!("Tablet.{owner}."))),
        "{node}: no generated `{owner}.<member>` key may leak as its own dep; deps: {:?}",
        all_dep_names(&env),
    );
    assert!(
        !env.errors.iter().any(|e| e.contains("private auxiliary")),
        "{node}: must not push a private-auxiliary rejection; errors: {:?}",
        env.errors,
    );
    assert_axcheck_agreed(&env, node);
}

#[test]
fn local_closure_smoke_structure_field_projection_accepted() {
    // (a) `UsesStructField` references the structure field projection
    // `StructFields.foo` (arbitrary user-chosen name). The probe must
    // transparent-walk the projection and resolve the dep to node
    // `StructFields`, NOT reject `StructFields.foo` as a private aux.
    if fixture_skip("structure_field_projection_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesStructField", "StructFields");
}

#[test]
fn local_closure_smoke_inductive_constructor_accepted() {
    // (b) `UsesEnumCtor` references the constructor `EnumColor.red` — a
    // NON-`mk`, arbitrary constructor name. Acceptance proves the fix
    // recognizes constructors via the environment, not a name suffix.
    if fixture_skip("inductive_constructor_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesEnumCtor", "EnumColor");
}

#[test]
fn local_closure_smoke_constructor_injeq_accepted() {
    // `UsesCtorInjEq` references the constructor-generated theorem
    // `InductiveNat.mk.injEq`. The probe must transparent-walk it and
    // resolve the real dependency to `InductiveNat`.
    if fixture_skip("constructor_injeq_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesCtorInjEq", "InductiveNat");
}

#[test]
fn local_closure_smoke_class_method_accepted() {
    // (c) `UsesClassMethod` references the class method projection
    // `ClassMethod.op` (a projection flagged `fromClass`).
    if fixture_skip("class_method_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesClassMethod", "ClassMethod");
}

#[test]
fn local_closure_smoke_extends_diamond_parent_coercion_accepted() {
    // (e) `UsesDiamondParent` references the non-subobject parent
    // coercion `DiamondChild.toTop`, recognized via
    // `Environment.getAuxParentProjectionInfo?`.
    if fixture_skip("extends_diamond_parent_coercion_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesDiamondParent", "DiamondChild");
}

#[test]
fn local_closure_smoke_handwritten_owner_to_ctor_idx_still_recorded() {
    // Soundness regression (fail-OPEN hole, ex-commit acb2f23): a
    // hand-authored `theorem Owner.toCtorIdx : True := trivial` collides in
    // NAME with the compiler's constructor-index helper suffix, but Lean
    // does NOT generate `toCtorIdx` for a structure owner, so the decl
    // compiles unblocked and is a genuine `thmInfo` private auxiliary. A
    // prior fixed-suffix classifier (`isFixedSuffixGeneratedArtifact`:
    // suffix match + `isInductiveCore? owner`) hid it as if generated:
    // `UsesOwnerToCtorIdx` probed `status: ok` with EMPTY dep lists, so the
    // Rust `validate_probe_present_nodes` never saw `Owner.toCtorIdx` to
    // reject — fail-OPEN. The collector must NOT transparent-walk it: the
    // dotted dep key MUST survive so the C-K guard can reject it as a
    // private auxiliary of node `Owner`.
    //
    // This test FAILS if `isFixedSuffixGeneratedArtifact` is reintroduced:
    // the `toCtorIdx` suffix + inductive `Owner` would match, the aux would
    // be transparent-walked away, and the dep key assertion below would
    // find no `Owner.toCtorIdx` entry. Mirrors the over-admission guard
    // shape of `..._handwritten_owner_aux_still_rejected`.
    if fixture_skip("handwritten_owner_to_ctor_idx_still_recorded") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesOwnerToCtorIdx").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnerToCtorIdx; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    // The raw probe must RECORD `Owner.toCtorIdx` as a dep key (it is a
    // `thmInfo`, reached via the consumer's proof body and recorded under
    // `boundary_theorems` by the PMAT boundary cut — the same place the
    // sibling `Owner.realAux` lands). The exact accumulator does not matter;
    // what matters for the fail-OPEN hole is that the dotted key is NOT
    // transparent-walked away, so it surfaces in *some* dep list and the
    // Rust `validate_probe_present_nodes` guard can reject it as a private
    // auxiliary of node `Owner`.
    let deps = all_dep_names(&env);
    assert!(
        deps.iter()
            .any(|n| n == "Tablet.Owner.toCtorIdx" || n == "Owner.toCtorIdx"),
        "hand-written Owner.toCtorIdx must remain a recorded dep key (NOT \
         transparent-walked as a generated artifact); deps: {deps:?}; envelope: {env:?}",
    );
    // The dual-collector cross-check must still agree (the axcheck side
    // also records the hand-written theorem as a boundary, since it is not
    // a generated artifact under the fix).
    assert_axcheck_agreed(&env, "UsesOwnerToCtorIdx");
}

#[test]
fn local_closure_smoke_handwritten_owner_aux_still_rejected() {
    // (d) over-admission guard via the Lean probe: `UsesOwnerAux`
    // depends on the hand-authored `theorem Owner.realAux`, which is a
    // `thmInfo` — NOT a projection/constructor/aux-recursor. It must
    // still be recorded as a dep key and rejected as a private
    // auxiliary of node `Owner` → `internal_error`. The reliable
    // pure-Rust form lives in `runtime_cli_observations.rs`
    // (`validate_probe_present_nodes_rejects_handwritten_owner_aux`);
    // this is the end-to-end mirror.
    if fixture_skip("handwritten_owner_aux_still_rejected") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesOwnerAux").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnerAux; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    // The raw probe records `Owner.realAux` as a dep key; the Rust C-K
    // guard is what flips status to internal_error against present_nodes
    // (exercised in the pure-Rust unit test). Here we assert the dep key
    // survives — it is NOT transparent-walked away by the fix.
    assert!(
        all_dep_names(&env)
            .iter()
            .any(|n| n == "Tablet.Owner.realAux" || n == "Owner.realAux"),
        "hand-written Owner.realAux must remain a recorded dep key (not transparent-walked); deps: {:?}",
        all_dep_names(&env),
    );
    assert_axcheck_agreed(&env, "UsesOwnerAux");
}

// ---------------------------------------------------------------------------
// FIX 1 — reserved-shaped axiom reached through the transparent-walk must be
//          RECORDED in `kernel_axioms`, never dropped.
// ---------------------------------------------------------------------------

#[test]
fn local_closure_smoke_reserved_axiom_surfaces_in_kernel_axioms() {
    // FIX 1 regression. `AxiomForge.lean` authors a reserved-shaped axiom
    // `axiom AxiomForge.eq_1 : 2 + 2 = 4`. `UsesAxiomForge` consumes it.
    // Before the fix, the transparent-walk branch's `.axiomInfo` arm did
    // `pure ()`, so a reserved-shaped Tablet axiom was dropped from
    // `kernel_axioms` entirely — a consumer could derive it (provable
    // `False` if the axiom were `False`) with no axiom surfaced to the
    // approved-axiom policy. After the fix the consumer's probe must
    // surface `AxiomForge.eq_1` in BOTH the primary and the axcheck
    // collectors' `kernel_axioms`.
    if fixture_skip("reserved_axiom_surfaces_in_kernel_axioms") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesAxiomForge").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesAxiomForge; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesAxiomForge should probe ok (it only consumes the axiom); envelope: {env:?}"
    );
    assert!(
        env.kernel_axioms
            .iter()
            .any(|a| a == "AxiomForge.eq_1" || a == "Tablet.AxiomForge.eq_1"),
        "reserved-shaped axiom must surface in kernel_axioms (FIX 1: transparent-walk \
         `.axiomInfo` arm now records instead of dropping); got {:?}",
        env.kernel_axioms,
    );
    // The dual collector must agree — FIX 1 was applied to both, so the
    // axcheck side must also carry the axiom.
    let ax = env
        .axiomization_check
        .as_ref()
        .expect("axiomization_check present");
    assert!(
        ax.kernel_axioms
            .iter()
            .any(|a| a == "AxiomForge.eq_1" || a == "Tablet.AxiomForge.eq_1"),
        "axcheck collector must also surface the reserved-shaped axiom (FIX 1 mirrored); got {:?}",
        ax.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "UsesAxiomForge");
}

// ---------------------------------------------------------------------------
// FIX 2 — owner-file authoring invariant: a node authoring a declaration
//          whose final name component is reserved-shaped is rejected at its
//          OWN acceptance, across every authoring surface.
// ---------------------------------------------------------------------------

/// Probe `node` (a forge fixture authoring a reserved-shaped auxiliary) and
/// assert the owner-file scan rejects it: status `internal_error`, with a
/// diagnostic naming the offending reserved component.
fn assert_owner_forge_rejected(node: &str, offending_component: &str) {
    let (stdout, stderr, code) = run_probe(node).expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for {node} (the probe reports the violation in JSON, \
         not via exit code); stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_ne!(
        env.status, "ok",
        "{node}: owner authoring a reserved-shaped auxiliary must NOT probe ok; envelope: {env:?}",
    );
    assert_eq!(
        env.status, "internal_error",
        "{node}: owner-file scan must flip status to internal_error; envelope: {env:?}",
    );
    assert!(
        env.errors
            .iter()
            .any(|e| e.contains("reserved-shaped") && e.contains(offending_component)),
        "{node}: rejection must name the offending reserved component `{offending_component}`; \
         errors: {:?}",
        env.errors,
    );
    // The forge node must not have leaked any dependency arrays (it failed
    // closed before the closure walk).
    assert!(
        env.boundary_theorems.is_empty()
            && env.strict_theorem_deps.is_empty()
            && env.strict_definition_deps.is_empty()
            && env.kernel_axioms.is_empty(),
        "{node}: a fail-closed owner rejection emits empty dep/axiom arrays; envelope: {env:?}",
    );
}

#[test]
fn local_closure_smoke_forge_bare_eq_rejected() {
    // Surface: bare `theorem ForgeBareEq.eq_1`.
    if fixture_skip("forge_bare_eq_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgeBareEq", "eq_1");
}

#[test]
fn local_closure_smoke_forge_protected_eq_rejected() {
    // Surface: `protected theorem ForgeProtectedEq.eq_1` — defeats the Rust
    // first-token line-scanner (the `protected` bypass), caught by the parse.
    if fixture_skip("forge_protected_eq_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgeProtectedEq", "eq_1");
}

#[test]
fn local_closure_smoke_forge_private_eq_rejected() {
    // Surface: `private theorem ForgePrivateEq.eq_1` — same line-scanner bypass.
    if fixture_skip("forge_private_eq_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgePrivateEq", "eq_1");
}

#[test]
fn local_closure_smoke_forge_underscore_rejected() {
    // Surface: `protected theorem ForgeUnderscore._helper` — the `_`-prefixed
    // family has no env provenance signal, so only the authorship parse
    // catches it.
    if fixture_skip("forge_underscore_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgeUnderscore", "_helper");
}

#[test]
fn local_closure_smoke_forge_namespace_rejected() {
    // Surface: name placed via `namespace ForgeNamespace … theorem eq_1 … end`.
    if fixture_skip("forge_namespace_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgeNamespace", "eq_1");
}

#[test]
fn local_closure_smoke_namespaced_root_resolves_and_passes() {
    // NAMESPACED-ROOT FIX (PV-shape regression): `NamespacedRoot` declares its
    // node-named theorem under a `namespace crate_ns` opened in the free
    // region above the `-- [TABLET NODE: …]` marker, so the on-disk decl is
    // `crate_ns.NamespacedRoot`, NOT the bare `NamespacedRoot`. Before the fix
    // the probe did `env.find? (bare name)` and reported a spurious
    // `missing_declaration`. The resolver must now find the unique
    // same-final-name decl in the node's own module `Tablet.NamespacedRoot`
    // and probe it `ok` (the theorem is sorry-free).
    if fixture_skip("namespaced_root_resolves_and_passes") {
        return;
    }
    let (stdout, stderr, code) = run_probe("NamespacedRoot").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for NamespacedRoot; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "namespaced root must resolve via the unique same-final-name decl in \
         its own module and probe ok (NOT missing_declaration); envelope: {env:?}",
    );
    assert_eq!(
        env.root_kind, "theorem",
        "namespaced root must classify as a theorem after resolution; envelope: {env:?}",
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "NamespacedRoot is sorry-free; axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );
    assert!(
        env.errors.is_empty(),
        "NamespacedRoot should resolve+verify with no errors; got {:?}",
        env.errors,
    );
    assert_axcheck_agreed(&env, "NamespacedRoot");
}

#[test]
fn local_closure_smoke_namespaced_dep_emits_bare_node_ids() {
    // NAMESPACED-DEP FIX (PV-shape regression, the SECOND fix): a NAMESPACED
    // consumer node depends on OTHER NAMESPACED nodes. Aeneas-shape: every
    // decl is `crate_ns.<Node>` on disk while the modules stay
    // `Tablet.<Node>`. `NamespacedConsumer`'s proof references the theorem
    // `NamespacedThm` (a boundary), the def `NamespacedDef` (a
    // strict_definition_dep), and — through that def's body — the theorem
    // `NamespacedStrictThm` (a strict_theorem_dep). All three cross-node dep
    // maps are therefore non-empty.
    //
    // Before the fix the probe emitted the namespaced on-disk `Name`s
    // (`crate_ns.NamespacedThm`, …), which have no `Tablet.` prefix for the
    // kernel parser to strip and so fail the Patch C-K present-node check
    // (keyed by BARE node ids). After the fix each dep `name` is its
    // `Tablet.`-stripped MODULE SUFFIX — the bare node id — which is exactly
    // the key `present_nodes` uses. This test asserts the emitted dep names
    // are the bare node ids (no `crate_ns.` namespace, no `Tablet.` prefix),
    // which is precisely what makes `validate_probe_present_nodes` pass.
    if fixture_skip("namespaced_dep_emits_bare_node_ids") {
        return;
    }
    let (stdout, stderr, code) = run_probe("NamespacedConsumer").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for NamespacedConsumer; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "namespaced consumer must resolve+verify ok (NOT internal_error); envelope: {env:?}",
    );
    assert_eq!(
        env.root_kind, "theorem",
        "namespaced consumer must classify as a theorem; envelope: {env:?}",
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "NamespacedConsumer is sorry-free; axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );

    // Collect every emitted cross-node dep `name`.
    let boundaries = boundary_names(&env.boundary_theorems);
    let strict_thms = boundary_names(&env.strict_theorem_deps);
    let strict_defs = boundary_names(&env.strict_definition_deps);

    // The boundary theorem is `NamespacedThm`, emitted as the BARE node id.
    assert!(
        boundaries.iter().any(|n| n == "NamespacedThm"),
        "boundary dep must be the bare node id `NamespacedThm`; got {boundaries:?}",
    );
    // The strict definition dep is `NamespacedDef`, emitted as the bare id.
    assert!(
        strict_defs.iter().any(|n| n == "NamespacedDef"),
        "strict_definition_deps must contain bare node id `NamespacedDef`; got {strict_defs:?}",
    );
    // The strict theorem dep (reached through the def's body) is
    // `NamespacedStrictThm`, emitted as the bare id.
    assert!(
        strict_thms.iter().any(|n| n == "NamespacedStrictThm"),
        "strict_theorem_deps must contain bare node id `NamespacedStrictThm`; got {strict_thms:?}",
    );

    // The CORE of the second fix: NO emitted dep name may carry the namespace
    // (`crate_ns.`) or the `Tablet.` module prefix — either form fails the
    // kernel's Patch C-K present-node check (keyed by bare node ids). This is
    // what `validate_probe_present_nodes` would otherwise reject.
    for name in boundaries.iter().chain(&strict_thms).chain(&strict_defs) {
        assert!(
            !name.contains("crate_ns") && !name.starts_with("Tablet."),
            "dep name `{name}` must be a BARE node id (no `crate_ns.` namespace, \
             no `Tablet.` prefix) so it maps to a kernel present_node",
        );
    }

    assert!(
        env.errors.is_empty(),
        "NamespacedConsumer should have no errors; got {:?}",
        env.errors,
    );
    assert_axcheck_agreed(&env, "NamespacedConsumer");

    // End-to-end note: every emitted dep `name` above is a bare node id that
    // is, by construction, equal to the kernel's `present_node` key for that
    // dep (a node id IS its `Tablet.`-stripped module suffix). So the kernel's
    // `validate_probe_present_nodes` (Patch C-K) — exercised directly by the
    // in-crate unit tests in `runtime_cli_observations.rs` — accepts these
    // names where it would have rejected the pre-fix namespaced
    // `crate_ns.<Node>` keys as "not in present_nodes".
}

#[test]
fn local_closure_smoke_namespaced_authored_aux_stays_dotted_and_rejected() {
    // SOUNDNESS: the dep-fix's principal-declaration guard (final component ==
    // module-suffix final component) is the ONLY thing preventing a namespaced
    // authored auxiliary from being silently re-keyed onto a bare present-node
    // id. `UsesNamespacedForgeAux` (a PV-shape namespaced node) depends on
    // `crate_ns.NamespacedForgeAux.realAux` — a hand-authored `thmInfo` (NOT a
    // generated member) declared in the DIFFERENT node `NamespacedForgeAux`'s
    // module `Tablet.NamespacedForgeAux`.
    //
    // The aux's on-disk final component is `realAux`, which differs from the
    // module-suffix node-id final component `NamespacedForgeAux`, so the guard
    // must NOT collapse it to the bare present-node id. The emitted dep `name`
    // must stay the DOTTED `Name` (`...NamespacedForgeAux.realAux`), which the
    // kernel's Patch C-K present-node guard rejects fail-closed. If the guard
    // were absent (blanket module-suffix mapping), the dep would become the
    // present node id `NamespacedForgeAux` and slip through — a soundness hole.
    if fixture_skip("namespaced_authored_aux_stays_dotted_and_rejected") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesNamespacedForgeAux").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesNamespacedForgeAux; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    // The probe itself reports ok (the authoring-scan only rejects
    // reserved-shaped names; `realAux` is not reserved-shaped, so the
    // cross-node rejection is the KERNEL's job at present-node validation).
    assert_eq!(
        env.status, "ok",
        "probe records the dep; the kernel rejects it (not the probe); envelope: {env:?}",
    );
    let boundaries = boundary_names(&env.boundary_theorems);
    // The CORE soundness claim: the aux dep is emitted as a DOTTED `Name`
    // whose final component is `realAux` — it is NOT collapsed to the bare
    // node id `NamespacedForgeAux`.
    let aux = boundaries
        .iter()
        .find(|n| n.ends_with(".realAux"))
        .unwrap_or_else(|| {
            panic!("namespaced authored aux must be recorded as a dotted dep ending `.realAux`; got {boundaries:?}")
        });
    assert!(
        aux.contains('.'),
        "namespaced authored aux dep `{aux}` must stay a dotted Name (NOT a bare node id)",
    );
    assert!(
        !boundaries.iter().any(|n| n == "NamespacedForgeAux"),
        "the aux must NOT be collapsed to the bare present-node id `NamespacedForgeAux`; got {boundaries:?}",
    );

    // End-to-end fail-closed proof: feed the real probe output's dep key into
    // the kernel's Patch C-K validator with `NamespacedForgeAux` ratified as a
    // present node (the realistic situation). The dotted aux key is NOT a
    // present node, so the validator must reject. This is the genuine
    // soundness assertion — the guard prevents the dotted key from becoming
    // the present node id `NamespacedForgeAux`. (The validator itself is
    // `pub(crate)`; the in-crate companion test
    // `validate_probe_present_nodes_rejects_real_namespaced_authored_aux` in
    // runtime_cli_observations.rs runs the SAME real probe output through the
    // actual `validate_probe_present_nodes` and asserts it fails closed. Here
    // we assert the probe-side invariant that validator relies on: the key is
    // dotted and not a present-node id.)
}

#[test]
fn local_closure_smoke_flattened_stem_dep_emits_bare_node_id() {
    // DEEPLY-NAMESPACED-DEP regression (the dec2flt-shape `declMatchesStem`
    // fix). `UsesNestedMethod` depends on the def `crate_ns.Nested.method`
    // (node `Nested_method`), whose FILESPEC stem FLATTENS its multi-segment
    // below-namespace path (`Nested.method` -> `Nested_method`). The decl's
    // FINAL component (`method`) differs from the module suffix
    // (`Nested_method`), so the PRIOR principal-declaration guard (final ==
    // module-suffix final) emitted the full namespaced `Name`
    // (`crate_ns.Nested.method`), which the kernel's Patch C-K validator
    // rejected (no `Tablet.` prefix, not a bare present-node id) — exactly the
    // dec2flt `dec2flt_biased_exact_correct_of_round_invariant` failure
    // (`BiasedFp.Insts.X.eq` rejected vs present node `BiasedFp_Insts_X_eq`).
    //
    // The FILESPEC name-parity test (`declMatchesStem`) recognizes the
    // principal via a trailing component run, so the emitted dep `name` is the
    // bare node id `Nested_method` — the present-node key. This asserts the
    // bare id is emitted (no `crate_ns.` namespace, no dotted aux form).
    if fixture_skip("flattened_stem_dep_emits_bare_node_id") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesNestedMethod").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesNestedMethod; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "flattened-stem consumer must resolve+verify ok (NOT internal_error); envelope: {env:?}",
    );
    let strict_defs = boundary_names(&env.strict_definition_deps);
    assert!(
        strict_defs.iter().any(|n| n == "Nested_method"),
        "flattened-stem def dep must emit the bare node id `Nested_method` \
         (the FILESPEC stem of `crate_ns.Nested.method`); got {strict_defs:?}",
    );
    // The core of the fix: NO emitted dep name may carry the namespace or stay
    // a dotted below-namespace path — either form fails Patch C-K.
    let all: Vec<String> = boundary_names(&env.boundary_theorems)
        .into_iter()
        .chain(boundary_names(&env.strict_theorem_deps))
        .chain(strict_defs.iter().cloned())
        .collect();
    for name in &all {
        assert!(
            !name.contains("crate_ns") && !name.starts_with("Tablet.") && !name.contains('.'),
            "flattened-stem dep `{name}` must be a BARE (underscored) node id, \
             not a namespaced or dotted `Name`, so it maps to a present_node",
        );
    }
    assert!(
        env.errors.is_empty(),
        "UsesNestedMethod should have no errors; got {:?}",
        env.errors,
    );
    assert_axcheck_agreed(&env, "UsesNestedMethod");
}

#[test]
fn local_closure_smoke_flattened_stem_authored_aux_stays_dotted_and_rejected() {
    // DEEPLY-NAMESPACED-FORGE soundness (the `declMatchesStem` guard at NESTED
    // depth). `UsesNestedForge` depends on the hand-authored namespaced
    // auxiliary `crate_ns.Nested.Forge.realAux` declared in the DIFFERENT node
    // `Nested_Forge`'s module. NO trailing component run of that name flattens
    // to the stem `Nested_Forge` (`realAux`, `Forge_realAux`,
    // `Nested_Forge_realAux`, … all differ), so the guard must leave the dep
    // key a DOTTED `Name`. The kernel's Patch C-K validator then rejects it
    // fail-closed. If the guard collapsed it to the bare present node id
    // `Nested_Forge`, the cross-node private-auxiliary reference would slip
    // through — the soundness hole, here at nested namespace depth.
    if fixture_skip("flattened_stem_authored_aux_stays_dotted_and_rejected") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesNestedForge").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesNestedForge; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "probe records the dep; the kernel rejects it (not the probe); envelope: {env:?}",
    );
    let boundaries = boundary_names(&env.boundary_theorems);
    let aux = boundaries
        .iter()
        .find(|n| n.ends_with(".realAux"))
        .unwrap_or_else(|| {
            panic!("nested authored aux must be recorded as a dotted dep ending `.realAux`; got {boundaries:?}")
        });
    assert!(
        aux.contains('.'),
        "nested authored aux dep `{aux}` must stay a dotted Name (NOT a bare node id)",
    );
    assert!(
        !boundaries.iter().any(|n| n == "Nested_Forge"),
        "the aux must NOT be collapsed to the bare present-node id `Nested_Forge`; got {boundaries:?}",
    );
}

#[test]
fn local_closure_smoke_ambiguous_root_rejected_not_arbitrary_pick() {
    // ROOT-FIX `ambiguous_declaration` coverage. `AmbiguousRoot` has TWO
    // non-generated declarations with final component `AmbiguousRoot` in its
    // own module `Tablet.AmbiguousRoot` (`crate_ns.AmbiguousRoot` and
    // `other_ns.AmbiguousRoot`), and NO top-level `AmbiguousRoot`. So the fast
    // path misses and the namespaced path finds >1 candidate. `resolveRoot`
    // must emit `status:"ambiguous_declaration"` — a distinct, safe
    // over-reject — never an arbitrary pick (which would be unsound: the two
    // declarations may have different statements/proofs).
    if fixture_skip("ambiguous_root_rejected_not_arbitrary_pick") {
        return;
    }
    let (stdout, stderr, code) = run_probe("AmbiguousRoot").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for AmbiguousRoot (violation reported in JSON); stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ambiguous_declaration",
        "two same-final-name decls in the node's own module must yield \
         `ambiguous_declaration`, never an arbitrary pick; envelope: {env:?}",
    );
    // The diagnostic must name BOTH candidates so an operator can resolve it.
    assert!(
        env.errors.iter().any(|e| {
            e.contains("multiple candidate")
                && e.contains("crate_ns.AmbiguousRoot")
                && e.contains("other_ns.AmbiguousRoot")
        }),
        "ambiguity diagnostic must list both candidate declarations; errors: {:?}",
        env.errors,
    );
    // Fail-closed: no dep/axiom arrays leaked (resolution failed before the
    // closure walk).
    assert!(
        env.boundary_theorems.is_empty()
            && env.strict_theorem_deps.is_empty()
            && env.strict_definition_deps.is_empty()
            && env.kernel_axioms.is_empty(),
        "an ambiguous-root rejection emits empty dep/axiom arrays; envelope: {env:?}",
    );
}

#[test]
fn local_closure_smoke_forge_where_bound_rejected() {
    // Surface: `where`-bound auxiliary `def ForgeWhere … where _helper := …`,
    // which Lean lifts to `ForgeWhere._helper`. The binder is nested in the
    // declVal (not a top-level declId), so the parse walk must collect
    // `letId` names under a `letRecDecl` ancestor.
    if fixture_skip("forge_where_bound_rejected") {
        return;
    }
    assert_owner_forge_rejected("ForgeWhere", "_helper");
}

#[test]
fn local_closure_smoke_forge_axiom_rejected_at_owner() {
    // Surface: `axiom AxiomForge.eq_1 : …`. Beyond FIX 1 surfacing it in a
    // consumer's `kernel_axioms`, FIX 2 rejects the AUTHORING node at its own
    // acceptance (the `axiom` keyword is also invisible to the Rust line
    // scanner's declaration-head list, so the parse is what catches it).
    if fixture_skip("forge_axiom_rejected_at_owner") {
        return;
    }
    assert_owner_forge_rejected("AxiomForge", "eq_1");
}

#[test]
fn local_closure_smoke_recursive_def_no_false_positive() {
    // No-false-positive: `RecDef` is a genuine recursive `def`. Lean
    // generates real `RecDef._sunfold`, `RecDef.match_1`, equation lemmas —
    // all `isInternalDetail`-shaped but COMPILER-GENERATED, never source
    // declaration commands. The owner-file scan must NOT see them, and the
    // node + its consumer must both probe ok with the generated internals
    // transparent-walked.
    if fixture_skip("recursive_def_no_false_positive") {
        return;
    }
    // The principal def node itself.
    let (stdout, stderr, code) = run_probe("RecDef").expect("run probe");
    assert_eq!(code, Some(0), "exit 0 for RecDef; stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "genuine recursive def must not be flagged by the owner-file scan; envelope: {env:?}"
    );
    assert!(
        env.errors.is_empty(),
        "RecDef should have no errors; got {:?}",
        env.errors
    );
    assert_axcheck_agreed(&env, "RecDef");

    // A consumer that forces realization of the equation lemmas / `_sunfold`.
    let (stdout, stderr, code) = run_probe("UsesRecDef").expect("run probe");
    assert_eq!(code, Some(0), "exit 0 for UsesRecDef; stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "consumer of a genuine recursive def must probe ok (generated internals \
         transparent-walked, no reserved-shape false positive); envelope: {env:?}"
    );
    // The real dependency `RecDef` is recorded; no generated `RecDef.<member>`
    // leaked in as its own key.
    assert!(
        dep_recorded(&env, "RecDef"),
        "UsesRecDef must record RecDef as a real dep; deps: {:?}",
        all_dep_names(&env),
    );
    assert!(
        !all_dep_names(&env)
            .iter()
            .any(|n| n.starts_with("RecDef.") || n.starts_with("Tablet.RecDef.")),
        "no generated RecDef.<member> may leak as a dep key; deps: {:?}",
        all_dep_names(&env),
    );
    assert_axcheck_agreed(&env, "UsesRecDef");
}

#[test]
fn local_closure_smoke_legit_protected_aux_no_false_positive() {
    // No-false-positive: `protected theorem LegitAux.helper` — final
    // component `helper` is NOT reserved-shaped, so it is allowed. The
    // principal `LegitAux` (final component == node name) is likewise never
    // flagged.
    if fixture_skip("legit_protected_aux_no_false_positive") {
        return;
    }
    let (stdout, stderr, code) = run_probe("LegitAux").expect("run probe");
    assert_eq!(code, Some(0), "exit 0 for LegitAux; stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "a legitimate non-reserved protected auxiliary must be allowed; envelope: {env:?}"
    );
    assert!(
        env.errors.is_empty(),
        "LegitAux should have no errors; got {:?}",
        env.errors
    );
    assert_axcheck_agreed(&env, "LegitAux");
}

// ---------------------------------------------------------------------------
// FIX 2 COVERAGE — the universal `--scan-only` owner-file authoring gate runs
//   for EVERY node kind, including Definition-kind nodes (def/abbrev/
//   structure/inductive/class) that the full closure probe deliberately
//   skips from `probe_candidates`. The residual being closed: a Definition
//   node authoring a `where`/`let rec`-bound reserved-shaped auxiliary is
//   scanned by neither the full probe (never runs on it) nor the Rust text
//   backstop (cannot see `where`/`let rec` binders). The scan-only mode
//   reuses the EXACT same `where`-aware `ownerFileScanRejection` the full
//   probe runs, decoupled from the closure-record machinery (empty dep/
//   axiom arrays, `axiomization_check.skipped=true`).
// ---------------------------------------------------------------------------

/// Probe `node` in `--scan-only` mode and assert the owner-file scan rejects
/// it: status `internal_error`, the reserved-shape diagnostic naming
/// `offending_component`, and the decoupled shape (empty dep/axiom arrays,
/// axcheck skipped — scan-only runs NO closure walk).
fn assert_scan_only_forge_rejected(node: &str, offending_component: &str) {
    let (stdout, stderr, code) = run_probe_scan_only(node).expect("run scan-only probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for {node} --scan-only (the scan reports the \
         violation in JSON, not via exit code); stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "internal_error",
        "{node} --scan-only: a Definition-kind node authoring a reserved-shaped \
         auxiliary must flip status to internal_error; envelope: {env:?}",
    );
    assert!(
        env.errors
            .iter()
            .any(|e| e.contains("reserved-shaped") && e.contains(offending_component)),
        "{node} --scan-only: rejection must name the offending reserved component \
         `{offending_component}`; errors: {:?}",
        env.errors,
    );
    // Decoupled from the closure-record machinery: scan-only emits no
    // dep/axiom keys and skips the axcheck.
    assert!(
        env.boundary_theorems.is_empty()
            && env.strict_theorem_deps.is_empty()
            && env.strict_definition_deps.is_empty()
            && env.kernel_axioms.is_empty(),
        "{node} --scan-only: must emit empty dep/axiom arrays (no closure walk); envelope: {env:?}",
    );
    let ax = env
        .axiomization_check
        .as_ref()
        .expect("scan-only still emits the axiomization_check sub-object");
    assert!(
        ax.skipped,
        "{node} --scan-only: axiomization_check must be skipped (no closure walk runs); envelope: {env:?}",
    );
}

/// Probe `node` in `--scan-only` mode and assert it passes clean: status
/// `ok`, no errors, decoupled empty arrays. Used for the no-false-positive
/// Definition-kind cases.
fn assert_scan_only_clean(node: &str) {
    let (stdout, stderr, code) = run_probe_scan_only(node).expect("run scan-only probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for {node} --scan-only; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "{node} --scan-only: a genuine (non-forged) node must pass the owner-file \
         scan clean; envelope: {env:?}",
    );
    assert!(
        env.errors.is_empty(),
        "{node} --scan-only: should have no errors; got {:?}",
        env.errors,
    );
}

#[test]
fn local_closure_smoke_scan_only_def_where_bound_rejected() {
    // The residual, exact: `ForgeWhere` is a Definition-kind node
    // (`def ForgeWhere … where _helper := …` ⇒ `ForgeWhere._helper`). The
    // full closure probe never runs on it (kernel `probe_candidates` skips
    // Definitions), and the Rust text backstop cannot see the `where`
    // binder. The universal scan-only gate catches it.
    if fixture_skip("scan_only_def_where_bound_rejected") {
        return;
    }
    assert_scan_only_forge_rejected("ForgeWhere", "_helper");
}

#[test]
fn local_closure_smoke_scan_only_def_let_rec_bound_rejected() {
    // Distinct surface: a Definition-kind node binding a reserved-shaped
    // auxiliary via `let rec` (`def DefForgeLetRec … let rec _aux := …` ⇒
    // `DefForgeLetRec._aux`). Same `letId`-under-`letRecDecl` AST path as
    // `where`, exercised via `let rec`.
    if fixture_skip("scan_only_def_let_rec_bound_rejected") {
        return;
    }
    assert_scan_only_forge_rejected("DefForgeLetRec", "_aux");
}

#[test]
fn local_closure_smoke_scan_only_structure_aux_rejected() {
    // A `structure` definition node authoring a reserved-shaped auxiliary
    // `StructForgeAux._helper`. The structure's own generated members (mk
    // constructor, field projections) must NOT be flagged — only the
    // hand-authored reserved-shaped auxiliary is.
    if fixture_skip("scan_only_structure_aux_rejected") {
        return;
    }
    assert_scan_only_forge_rejected("StructForgeAux", "_helper");
}

#[test]
fn local_closure_smoke_scan_only_class_aux_rejected() {
    // A `class` definition node authoring a reserved-shaped auxiliary
    // `ClassForgeAux._aux`.
    if fixture_skip("scan_only_class_aux_rejected") {
        return;
    }
    assert_scan_only_forge_rejected("ClassForgeAux", "_aux");
}

#[test]
fn local_closure_smoke_scan_only_forge_after_imported_notation_caught() {
    // PARSE-ROBUSTNESS (codex "additional risk"): the scan-only parse runs
    // against an `Init`-only environment and ignores parse errors. This
    // fixture's earlier declaration USES imported notation (`⟪ … ⟫` /
    // `myparens%` from `Tablet.NotationLib`) that is unknown under `Init`,
    // so that command parse-errors; the FORGED `eq_1` follows it. The scan
    // must still resync on the forge's command keyword and harvest+reject
    // `eq_1`. If `parseCommand` error-recovery failed to advance past the
    // unparseable command, the forge would evade the scan. The fixture
    // elaborates cleanly with its import present (it is realistic,
    // worker-authorable source), so the scan's parse errors are purely
    // import-induced — exactly the audited scenario.
    if fixture_skip("scan_only_forge_after_imported_notation_caught") {
        return;
    }
    assert_scan_only_forge_rejected("ForgeAfterNotation", "eq_1");
}

#[test]
fn local_closure_smoke_scan_only_forge_after_custom_command_caught() {
    // PARSE-ROBUSTNESS (codex "additional risk"), strongest form: an
    // unparseable *command head* precedes the forge. A custom command macro
    // (`declare_trivial …` from `Tablet.CommandLib`) is invoked as a
    // top-level command before the forged `eq_1`. Under `Init`-only,
    // `declare_trivial` is an unknown command token, so that whole command
    // parse-errors. The scan must still resync on the forge's `protected
    // theorem` head and harvest+reject `eq_1`.
    if fixture_skip("scan_only_forge_after_custom_command_caught") {
        return;
    }
    assert_scan_only_forge_rejected("ForgeAfterCommand", "eq_1");
}

// ---------------------------------------------------------------------------
// MACRO BAN — ordinary Tablet node files may not author a macro/syntax/
//   elaborator-DEFINING command (`macro`/`macro_rules`/`elab`/`elab_rules`/
//   `syntax`/`declare_syntax_cat`/`binder_predicate`). Such a command can
//   synthesize a top-level declaration whose name never appears literally in
//   source (e.g. a macro emitting `theorem Owner.eq_1`), which the syntactic
//   authored-name scan cannot see and the probe would transparent-walk as an
//   internal detail — a review-integrity bypass. Term-level `notation`/
//   `infix`/`prefix`/`postfix` are allowed (they cannot declare a top-level
//   constant). Enforced authoritatively in the same universal `--scan-only`
//   owner-file gate that runs for EVERY node kind.
// ---------------------------------------------------------------------------

/// Probe `node` in `--scan-only` mode and assert the owner-file scan rejects
/// it for defining a banned macro/elab/syntax command: status
/// `internal_error`, the macro-ban diagnostic naming `keyword`.
fn assert_scan_only_macro_ban_rejected(node: &str, keyword: &str) {
    let (stdout, stderr, code) = run_probe_scan_only(node).expect("run scan-only probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for {node} --scan-only (the scan reports the \
         violation in JSON, not via exit code); stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "internal_error",
        "{node} --scan-only: a node defining a macro/elab/syntax command must flip \
         status to internal_error; envelope: {env:?}",
    );
    assert!(
        env.errors
            .iter()
            .any(|e| e.contains("macro/syntax/elaborator") && e.contains(keyword)),
        "{node} --scan-only: rejection must name the offending command `{keyword}`; \
         errors: {:?}",
        env.errors,
    );
    // Decoupled from the closure-record machinery (scan-only runs no walk).
    assert!(
        env.boundary_theorems.is_empty()
            && env.strict_theorem_deps.is_empty()
            && env.strict_definition_deps.is_empty()
            && env.kernel_axioms.is_empty(),
        "{node} --scan-only: must emit empty dep/axiom arrays; envelope: {env:?}",
    );
}

#[test]
fn local_closure_smoke_scan_only_macro_forge_rejected_at_owner() {
    // THE RESIDUAL, closed at authoring time: `MacroEqForge` authors a
    // command `macro` that EXPANDS to a reserved-shaped declaration
    // (`theorem MacroEqForge.eq_1`, built via `mkIdent` so the *literal*
    // reserved name is synthesized while the string `eq_1` never appears as a
    // written `declId`). The syntactic authored-name scan cannot see `eq_1`;
    // without the macro ban the consumer would transparent-walk the
    // synthesized constant as an internal detail. The owner is now rejected
    // by the universal scan-only gate for defining a `macro` command at all.
    if fixture_skip("scan_only_macro_forge_rejected_at_owner") {
        return;
    }
    assert_scan_only_macro_ban_rejected("MacroEqForge", "macro");
}

#[test]
fn local_closure_smoke_scan_only_plain_macro_rejected() {
    // The ban is on the command FAMILY, not on what it emits: a plain term
    // `macro` whose expansion is harmless is still rejected.
    if fixture_skip("scan_only_plain_macro_rejected") {
        return;
    }
    assert_scan_only_macro_ban_rejected("PlainMacro", "macro");
}

#[test]
fn local_closure_smoke_scan_only_plain_elab_rejected() {
    // An `elab` command (distinct family member) is likewise rejected even
    // when its elaboration is harmless.
    if fixture_skip("scan_only_plain_elab_rejected") {
        return;
    }
    assert_scan_only_macro_ban_rejected("PlainElab", "elab");
}

#[test]
fn local_closure_smoke_scan_only_legit_notation_no_false_positive() {
    // No-false-positive: term-level `notation` + `infixl` are ALLOWED (they
    // cannot declare a top-level constant). The owner-file scan passes clean.
    if fixture_skip("scan_only_legit_notation_no_false_positive") {
        return;
    }
    assert_scan_only_clean("LegitNotation");
}

#[test]
fn local_closure_smoke_scan_only_no_false_positive_definition_kinds() {
    // No-false-positive across Definition kinds: a genuine recursive `def`
    // (real generated `_sunfold`/`match_1`/eq-lemmas), legitimate
    // `structure`/`class`/`inductive` definition nodes, and a legit
    // non-reserved auxiliary all pass the scan-only gate. The principal
    // declaration is never flagged.
    if fixture_skip("scan_only_no_false_positive_definition_kinds") {
        return;
    }
    for node in [
        "RecDef",       // genuine recursive def
        "StructFields", // legit structure
        "ClassMethod",  // legit class
        "EnumColor",    // legit inductive
        "InductiveNat", // legit inductive
        "LegitAux",     // legit non-reserved protected aux
        "Closed",       // plain theorem
    ] {
        assert_scan_only_clean(node);
    }
}

/// Helper for `inductive_constructor_type_change_changes_semantic_hash`:
/// extract the `semantic_hash` from `strict_definition_deps` for a
/// given dep name.
fn strict_def_hash(env: &ProbeEnvelope, dep_name: &str) -> Option<String> {
    for entry in &env.strict_definition_deps {
        let name = entry.get("name").and_then(serde_json::Value::as_str)?;
        // The script emits `Tablet.X`; the probe response may strip
        // the prefix when it's surfaced through `parse_local_closure_response`,
        // but the raw script output keeps the full name. Match both.
        if name == dep_name || name == format!("Tablet.{dep_name}") {
            return entry
                .get("semantic_hash")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

/// RAII guard that restores a fixture file's contents on drop, then
/// re-runs `lake build` so the .olean tree is consistent with the
/// canonical source. Used by
/// `inductive_constructor_type_change_changes_semantic_hash` to keep
/// the fixture from being left in a mutated state if the test panics.
struct FixtureRestoreGuard {
    path: PathBuf,
    original: String,
}

impl Drop for FixtureRestoreGuard {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
        let _ = Command::new("lake")
            .arg("build")
            .current_dir(fixture_root())
            .status();
    }
}

// Silence unused-import warnings for helpers that only the `#[ignore]`d
// tests use; they're load-bearing for the post-build operator workflow
// even though the `--ignored` filter hides them on a normal `cargo test`.
#[allow(dead_code)]
fn _unused_helper_silencer() {
    let _ = project_tempdir;
    let _ = local_closure_script;
}

#[allow(dead_code)]
fn _ensure_path_used(_p: &Path) {}
