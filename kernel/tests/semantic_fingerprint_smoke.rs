//! Regression tests for the correspondence/semantic fingerprint script
//! `scripts/lean_semantic_fingerprint.lean`.
//!
//! These mirror `local_closure_smoke.rs`'s
//! `inductive_constructor_type_change_changes_semantic_hash`, but exercise
//! the `lean_semantic_fingerprint.lean` path (the twin of
//! `lean_local_closure.lean`). They assert the correspondence fingerprint
//! of a consumer node CHANGES when a semantic property of a dependency
//! changes:
//!
//! * an inductive constructor TYPE change,
//! * a structure field-TYPE change,
//! * an instance (def) VALUE change.
//! * a transitive theorem PROOF-BODY change does not change the semantic
//!   fingerprint (and therefore does not stale correspondence approval).
//!
//! For the first two, the `.inductInfo` arm does NOT mix constructor types
//! into the inductive's own line hash (that inline edit was reverted, since
//! it would shift every data-decl node's stored fingerprint and spuriously
//! trip `corr_reopen_triggered` on deploy). Instead the sensitivity comes
//! from the separate per-ctor lines: each constructor is recursed into via
//! `allRefs` and emits its own `const|<ctor>|ctor|...|typehash=` line into
//! the hashed payload, so retyping a ctor (or, for a structure, changing a
//! field type carried by the ctor's type) already moves the closure's
//! overall fingerprint through that constructor's line. The instance case
//! is covered by the existing `.defnInfo` value walk and guards against a
//! regression in the def-value contribution.
//!
//! Like the local-closure smoke tests, every test that invokes Lean is
//! `#[ignore]`d: the fingerprint path can only be exercised against a
//! built fixture (`lake build` under
//! `kernel/tests/fixtures/local_closure_smoke/`), which is an operator
//! step. Run manually:
//!
//! ```
//! cargo test -p trellis-kernel --test semantic_fingerprint_smoke -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Serialize tests that mutate the shared fixture + rebuild it, so they
/// don't race each other or the local-closure smoke tests' own mutation
/// test on the `.lake/` tree.
fn fixture_mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn fixture_root() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("tests")
        .join("fixtures")
        .join("local_closure_smoke")
}

fn fingerprint_script() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .parent()
        .expect("kernel crate has a parent (workspace root)")
        .join("scripts")
        .join("lean_semantic_fingerprint.lean")
}

/// Run `lake env lean --run scripts/lean_semantic_fingerprint.lean <node>`
/// in the fixture root and return the `FP\t<node>\t<payload>` payload for
/// the requested node, or an `Err` describing the failure.
fn run_fingerprint(node: &str) -> Result<String, String> {
    let script = fingerprint_script();
    if !script.exists() {
        return Err(format!("fingerprint script missing: {}", script.display()));
    }
    let source_path = fixture_root().join("Tablet").join(format!("{node}.lean"));
    let source = std::fs::read_to_string(&source_path)
        .map_err(|error| format!("cannot read {}: {error}", source_path.display()))?;
    let principal = trellis_kernel::filespec::exact_principal_name(&source, node)?;
    let output = Command::new("lake")
        .arg("env")
        .arg("lean")
        .arg("--run")
        .arg(&script)
        .arg(node)
        .arg(format!("--principal={principal}"))
        .current_dir(fixture_root())
        .output()
        .map_err(|e| format!("lake env lean failed to spawn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // The script emits one `FP\t<node>\t<payload>` (or `ERR\t...`) line per
    // requested node. Take the payload field of the matching FP line.
    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("FP"), Some(n), Some(payload)) if n == node => {
                return Ok(payload.to_string());
            }
            (Some("ERR"), Some(n), Some(err)) if n == node => {
                return Err(format!("fingerprint ERR for {n}: {err}"));
            }
            _ => {}
        }
    }
    Err(format!(
        "no FP line for {node}; stdout=<<<{stdout}>>> stderr=<<<{stderr}>>>"
    ))
}

/// RAII guard that restores a fixture file's contents on drop and re-runs
/// `lake build` so the `.olean` tree is consistent with the canonical
/// source even if a test panics mid-mutation.
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

/// Core of every test below: capture the consumer's fingerprint, apply
/// `mutate` to a dependency fixture file (replacing `from` with `to`),
/// rebuild, re-capture, restore, and assert the fingerprint changed.
fn assert_fingerprint_changes_on_mutation(consumer: &str, dep_file: &str, from: &str, to: &str) {
    let _serialize = fixture_mutation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dep_path = fixture_root().join(dep_file);
    let original =
        std::fs::read_to_string(&dep_path).unwrap_or_else(|e| panic!("read {dep_file}: {e}"));
    assert!(
        original.contains(from),
        "fixture {dep_file} must contain `{from}` for the mutation to be meaningful; got:\n{original}",
    );

    let fp1 = run_fingerprint(consumer).expect("fingerprint baseline");

    let mutated = original.replace(from, to);
    assert_ne!(mutated, original, "mutation must change {dep_file}");
    let _guard = FixtureRestoreGuard {
        path: dep_path.clone(),
        original: original.clone(),
    };
    std::fs::write(&dep_path, &mutated).expect("write mutated fixture");
    let build = Command::new("lake")
        .arg("build")
        .current_dir(fixture_root())
        .status()
        .expect("lake build (mutated)");
    assert!(
        build.success(),
        "lake build must succeed for mutated {dep_file}"
    );

    let fp2 = run_fingerprint(consumer).expect("fingerprint mutated");

    assert_ne!(
        fp1, fp2,
        "correspondence fingerprint of `{consumer}` must change when `{dep_file}` \
         is mutated (`{from}` → `{to}`); got identical payloads:\n{fp1}",
    );
}

/// Acceptance counterpart to `assert_fingerprint_changes_on_mutation`: a
/// theorem proof is logic/build identity, not semantic identity. Mutating a
/// transitive theorem's body must leave the consumer's semantic payload exact.
fn assert_fingerprint_stable_on_proof_mutation(
    consumer: &str,
    dep_file: &str,
    from: &str,
    to: &str,
) {
    let _serialize = fixture_mutation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dep_path = fixture_root().join(dep_file);
    let original =
        std::fs::read_to_string(&dep_path).unwrap_or_else(|e| panic!("read {dep_file}: {e}"));
    assert!(
        original.contains(from),
        "fixture {dep_file} must contain `{from}` for the mutation to be meaningful; got:\n{original}",
    );

    let fp1 = run_fingerprint(consumer).expect("fingerprint baseline");
    let mutated = original.replace(from, to);
    assert_ne!(mutated, original, "mutation must change {dep_file}");
    let _guard = FixtureRestoreGuard {
        path: dep_path.clone(),
        original,
    };
    std::fs::write(&dep_path, &mutated).expect("write mutated fixture");
    let build = Command::new("lake")
        .arg("build")
        .current_dir(fixture_root())
        .status()
        .expect("lake build (mutated)");
    assert!(
        build.success(),
        "lake build must succeed for mutated {dep_file}"
    );

    let fp2 = run_fingerprint(consumer).expect("fingerprint mutated");
    assert_eq!(
        fp1, fp2,
        "proof-only change in transitive dependency `{dep_file}` must not move \
         consumer `{consumer}`'s semantic/correspondence payload",
    );
}

#[test]
#[ignore = "requires operator-built fixture; mutates a Tablet/*.lean and rebuilds — see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn fingerprint_changes_on_inductive_constructor_type_change() {
    // Twin of local_closure_smoke's
    // `inductive_constructor_type_change_changes_semantic_hash`, on the
    // correspondence-fingerprint path. The ctor type change is detected by
    // the `.inductInfo` arm's inline ctor-type mixing.
    assert_fingerprint_changes_on_mutation(
        "UsesInductive",
        "Tablet/InductiveNat.lean",
        "mk : Nat → InductiveNat",
        "mk : Bool → InductiveNat",
    );
}

#[test]
#[ignore = "requires operator-built fixture; mutates a Tablet/*.lean and rebuilds — see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn fingerprint_changes_on_structure_field_type_change() {
    // A structure's field types are carried by its constructor's type, so
    // a field-type change moves the inductive's inline hash contributed by
    // the hardened `.inductInfo` arm.
    assert_fingerprint_changes_on_mutation(
        "UsesFieldTyped",
        "Tablet/FieldTyped.lean",
        "val : Nat",
        "val : Bool",
    );
}

#[test]
#[ignore = "requires operator-built fixture; mutates a Tablet/*.lean and rebuilds — see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn fingerprint_changes_on_instance_value_change() {
    // An instance elaborates to a `def`; its value enters the fingerprint
    // via the `.defnInfo` arm. Mutating the instance method body must move
    // the consumer's fingerprint.
    assert_fingerprint_changes_on_mutation(
        "UsesInstanceValue",
        "Tablet/InstanceValue.lean",
        "op := fun n => n",
        "op := fun n => n + 1",
    );
}

#[test]
#[ignore = "requires operator-built fixture; mutates a Tablet/*.lean and rebuilds — see kernel/tests/fixtures/local_closure_smoke/README.md"]
fn fingerprint_stable_on_transitive_theorem_proof_body_change() {
    assert_fingerprint_stable_on_proof_mutation(
        "UsesHelper",
        "Tablet/Helper.lean",
        "theorem Helper : True := by sorry",
        "theorem Helper : True := by trivial",
    );
}

#[test]
fn fingerprint_smoke_fixture_files_exist() {
    // Non-ignored guard: the Part 2 regression fixtures must stay in the
    // tree. Mirrors local_closure_smoke's existence check.
    let root = fixture_root();
    for required in &[
        "Tablet/InductiveNat.lean",
        "Tablet/UsesInductive.lean",
        "Tablet/FieldTyped.lean",
        "Tablet/UsesFieldTyped.lean",
        "Tablet/InstanceValue.lean",
        "Tablet/UsesInstanceValue.lean",
    ] {
        let p = root.join(required);
        assert!(p.exists(), "fixture file missing: {}", p.display());
    }
    assert!(
        fingerprint_script().exists(),
        "fingerprint script missing: {}",
        fingerprint_script().display(),
    );
}
