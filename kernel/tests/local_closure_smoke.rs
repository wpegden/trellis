//! Tier 3 (plan §5.9) — fixture-based smoke tests for the Patch A
//! local-closure probe.
//!
//! All tests in this file are gated `#[ignore]` because they require:
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
//! Run manually:
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

/// Sanity-check the fixture is built. Returns `Some(error)` if any
/// olean is missing, so individual tests can `eprintln!` and `return`
/// gracefully (rather than panicking — the `#[ignore]` flag already
/// guards against unintentional execution).
fn fixture_built_or_message() -> Option<String> {
    let root = fixture_root();
    if !root.exists() {
        return Some(format!("fixture root missing: {}", root.display()));
    }
    let lake_dir = root.join(".lake");
    if !lake_dir.exists() {
        return Some(format!(
            "fixture not built: {} missing. Run `cd {} && lake build` first.",
            lake_dir.display(),
            root.display(),
        ));
    }
    Some(format!(
        "fixture detected at {} (operator must verify .olean tree exists for Tablet.Helper, Tablet.Closed, Tablet.UsesHelper, Tablet.ActiveSorry)",
        root.display(),
    ))
    .filter(|_| false) // The detection above is informative only; let the test invoke lake.
}

/// Run `lake env lean --run scripts/lean_local_closure.lean <node>` in the
/// fixture root and return (stdout, stderr, exit status). Mirrors the
/// command shape used by `_handle_local_closure_axioms` server-side.
fn run_probe(node: &str) -> Result<(String, String, Option<i32>), String> {
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
        .current_dir(&root)
        .output()
        .map_err(|e| format!("lake env lean failed to spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((stdout, stderr, output.status.code()))
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
#[ignore = "requires operator-built fixture; see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn local_closure_smoke_closed_node_passes_clean() {
    // Plan §5.9 Tier 3: `Closed` has no Tablet helpers and no `sorry`.
    // Probe must report `status=ok`, `kernel_axioms` ⊆ canonical four,
    // empty `boundary_theorems`, no errors.
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
#[ignore = "requires operator-built fixture; see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn local_closure_smoke_uses_helper_records_boundary() {
    // Plan §2.2 / §4.3 boundary cut: `UsesHelper` proof references
    // `Helper` (which carries `sorryAx`). The probe must record `Helper`
    // as a boundary theorem and stop at its statement, NOT walking its
    // `value`. Result: `kernel_axioms` ⊆ canonical four (no `sorryAx`
    // leakage from `Helper.value`), `boundary_theorems` ∋ `Helper`.
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
#[ignore = "requires operator-built fixture; see kernel/tests/fixtures/local_closure_smoke/README.md"]
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
#[ignore = "requires operator-built fixture; see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn local_closure_smoke_active_sorry_surfaces_sorry_ax() {
    // The active node's proof itself is `by sorry`. Walking the value
    // under `ProofMayAssumeTheorems` reaches `sorryAx` (a kernel
    // axiom). Result: `kernel_axioms` ∋ `sorryAx`. Patch A is
    // observation-only so we don't fail the probe — Patch B's gate
    // will reject this when `must_close_active = true`.
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
#[ignore = "requires operator-built fixture; see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn local_closure_smoke_unmappable_name_fails_closed() {
    // The wrapper's contract: when the script can't find the named
    // declaration, the script returns `status = missing_declaration`
    // (or `internal_error`) and the wrapper surfaces that intact. The
    // gate code in Patch B treats any non-`ok` status as fail-closed.
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
    let _ = fixture_built_or_message;
    let _ = local_closure_script;
}

#[allow(dead_code)]
fn _ensure_path_used(_p: &Path) {}
