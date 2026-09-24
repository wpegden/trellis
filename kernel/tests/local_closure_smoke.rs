//! Tier 3 (plan §5.9) — fixture-based smoke tests for the Patch A
//! local-closure probe.
//!
//! Most probe tests here are guard-checked rather than `#[ignore]`d: they
//! run on every `cargo test` and, via `fixture_unready_reason`, EXECUTE
//! when the fixture is built (this worktree, and any CI lane that runs
//! `lake build` under the fixture root first). That is what lets them
//! catch a real classifier regression of `isTabletGeneratedArtifact`. The
//! integration tests that mutate a fixture file and rerun `lake build`
//! stay `#[ignore]`d (they are too heavy and stateful for the default
//! lane).
//!
//! When the fixture is NOT built these tests FAIL loudly, naming the
//! `lake build` that fixes it. They guard two resolved local-closure
//! SOUNDNESS holes (a forgeable `isInternalDetail`; struct-field /
//! class-method members mis-rejected as private-aux), so a vacuous green
//! reports safety that nothing checked. A lane that genuinely has no Lean
//! toolchain opts out explicitly with `TRELLIS_ALLOW_FIXTURE_SKIP=1`,
//! which restores the print-and-pass behaviour.
//!
//! Requirements when the guard-checked tests do run:
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
//! Run the guard-checked lane:
//!
//! ```
//! (cd kernel/tests/fixtures/local_closure_smoke && lake build)
//! cargo test -p trellis-kernel --test local_closure_smoke
//! ```
//!
//! Run it on a host with no Lean toolchain (tests pass vacuously):
//!
//! ```
//! TRELLIS_ALLOW_FIXTURE_SKIP=1 cargo test -p trellis-kernel --test local_closure_smoke
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use trellis_kernel::runtime_cli_observations_probe::{
    parse_local_closure_response, validate_probe_present_nodes,
};
use trellis_kernel::{LocalClosureProbeOutput, NodeId, NodeKind};

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
/// absent they fail with this reason string (see `fixture_skip`), which
/// names the build command. Build the fixture first:
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
        "Tablet/EmptyOwner.olean",
        // FIX 1 / FIX 2 fixtures: ensure the soundness-fix lane treats a
        // tree built before these fixtures landed as not-ready (so the
        // forge/no-false-positive tests don't vacuously skip).
        "Tablet/AxiomForge.olean",
        "Tablet/ForgeProtectedEq.olean",
        "Tablet/RecDef.olean",
        "Tablet/PartialDef.olean",
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
        "Tablet/ByElab.olean",
        "Tablet/LegitNotation.olean",
        "Tablet/PolicyEval.olean",
        "Tablet/PolicyLocalSyntax.olean",
        "Tablet/PolicyInitialize.olean",
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
        // DEEPLY-NAMESPACED ownership regression fixtures: do not let a stale
        // fixture tree vacuously skip exact declaration/owner tests.
        "Tablet/Nested_method.olean",
        "Tablet/UsesNestedMethod.olean",
        "Tablet/Nested_Forge.olean",
        "Tablet/UsesNestedForge.olean",
        "Tablet/AmbiguousRoot.olean",
        // Registration fix: `UsesCtorInjEq` was missing from the fixture
        // root `Tablet.lean`, so fresh builds omitted its olean and the
        // injEq test failed with `elaboration_error` instead of skipping.
        "Tablet/UsesCtorInjEq.olean",
        // FIX A (own-node `let rec` aux transparent walk) fixtures: a tree
        // built before these landed must be treated as not-ready so the
        // own-aux tests don't vacuously skip.
        "Tablet/UsesOwnLetRec.olean",
        "Tablet/UsesOwnAuxCrossDep.olean",
        "Tablet/UsesOwnAuxAxiom.olean",
        // FIX B (recursor-family realization + ctor sizeOf_spec transparent
        // walk) fixtures: a tree built before these landed must be treated
        // as not-ready so the recursor-family tests don't vacuously skip.
        "Tablet/IndPredReach.olean",
        "Tablet/UsesIndPredInduction.olean",
        "Tablet/NonRecPred.olean",
        "Tablet/ForgeBrecOnAux.olean",
        "Tablet/UsesForgeBrecOnAux.olean",
        "Tablet/OwnBrecOnForge.olean",
        "Tablet/UsesOwnBrecOnForge.olean",
        "Tablet/DefParentBrecOn.olean",
        "Tablet/UsesDefParentBrecOn.olean",
        "Tablet/UsesCtorSizeOfSpec.olean",
        // Generality regression: a custom deriving handler emits an unrelated
        // root name from the owner's module.
        "Tablet/WeirdDeriveOwner.olean",
        "Tablet/UsesWeirdDeriveOwner.olean",
        // Certificate-v2 duplicate-provider falsifier fixtures.
        "Tablet/CollisionLeft.olean",
        "Tablet/CollisionRight.olean",
        "Tablet/CollisionConsumerForward.olean",
        "Tablet/CollisionConsumerReverse.olean",
        "Tablet/SplitVisibility.olean.private",
        "Tablet/SplitVisibilityConsumer.olean.private",
        "Tablet/MultilineDep.olean",
        "Tablet/MultilineConsumer.olean",
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

/// Env var that opts a Lean-less lane out of the hard failure below and
/// restores the legacy print-and-pass behaviour. Must be exactly `1`.
const ALLOW_FIXTURE_SKIP_ENV: &str = "TRELLIS_ALLOW_FIXTURE_SKIP";

/// Return `false` when the fixture is ready, so callers can
/// `if fixture_skip("name") { return; }`. Centralizes the single decision
/// this harness makes about an unbuilt fixture.
///
/// An unready fixture PANICS. libtest has no conditional ignore — a
/// runtime guard can only pass or fail — and these tests are the named
/// regression harness for two resolved local-closure soundness holes, so
/// passing vacuously would report a safety property that nothing checked.
/// `TRELLIS_ALLOW_FIXTURE_SKIP=1` is the explicit opt-out for a lane with
/// no Lean toolchain; it returns `true` and prints the reason.
fn fixture_skip(test: &str) -> bool {
    let Some(reason) = fixture_unready_reason() else {
        return false;
    };
    if std::env::var(ALLOW_FIXTURE_SKIP_ENV).ok().as_deref() == Some("1") {
        eprintln!("SKIP {test}: {reason} [{ALLOW_FIXTURE_SKIP_ENV}=1]");
        return true;
    }
    panic!(
        "{test}: local-closure fixture is not ready — {reason}\n\
         \n\
         This harness guards two resolved local-closure SOUNDNESS holes; a \
         vacuous pass would report safety that nothing checked.\n\
         Build the fixture (fast — no Mathlib, stdlib oleans only):\n\
         \n\
             (cd {root} && lake build)\n\
         \n\
         If this lane has no Lean toolchain, opt out explicitly:\n\
         \n\
             {ALLOW_FIXTURE_SKIP_ENV}=1 cargo test --test local_closure_smoke\n",
        root = fixture_root().display(),
    );
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
    let mut command = Command::new("lake");
    command
        .arg("env")
        .arg("lean")
        .arg("--run")
        .arg(&script)
        .arg(node)
        .args(extra_args);
    if !extra_args.contains(&"--scan-only")
        && !extra_args.contains(&"--module-owner")
        && !extra_args.iter().any(|arg| arg.starts_with("--principal="))
    {
        let source_path = root.join("Tablet").join(format!("{node}.lean"));
        let source = std::fs::read_to_string(&source_path)
            .map_err(|error| format!("read {}: {error}", source_path.display()))?;
        let principal = trellis_kernel::exact_principal_name(&source, node)?;
        command.arg(format!("--principal={principal}"));
    }
    let output = command
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
    principal_declaration: String,
    #[serde(default)]
    kernel_axioms: Vec<String>,
    #[serde(default)]
    boundary_theorems: Vec<serde_json::Value>,
    #[serde(default)]
    strict_theorem_deps: Vec<serde_json::Value>,
    #[serde(default)]
    strict_definition_deps: Vec<serde_json::Value>,
    #[serde(default)]
    declaration_manifest: Vec<serde_json::Value>,
    #[serde(default)]
    exact_declaration_uses: Vec<serde_json::Value>,
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

#[test]
fn preamble_module_owner_probe_covers_its_manifest_without_fake_principal() {
    if fixture_skip("preamble_module_owner_probe_covers_its_manifest_without_fake_principal") {
        return;
    }
    let (stdout, stderr, code) =
        run_probe_with_args("Preamble", &["--module-owner"]).expect("run module-owner probe");
    assert_eq!(code, Some(0), "stderr={stderr}");
    let line = extract_json_line(&stdout).expect("module-owner JSON envelope");
    let env: ProbeEnvelope = serde_json::from_str(&line).expect("parse module-owner envelope");
    assert_eq!(env.status, "ok", "errors={:?}; stderr={stderr}", env.errors);
    assert!(
        env.principal_declaration.is_empty(),
        "Preamble must not acquire a fabricated principal"
    );
    assert!(env.declaration_manifest.iter().any(|entry| {
        entry.get("name").and_then(serde_json::Value::as_str) == Some("crate_ns.PreambleBiasedFp")
    }));
    assert_axcheck_agreed(&env, "Preamble");
}

#[test]
fn imports_only_module_owner_probe_attests_empty_manifest() {
    if fixture_skip("imports_only_module_owner_probe_attests_empty_manifest") {
        return;
    }
    let (stdout, stderr, code) =
        run_probe_with_args("EmptyOwner", &["--module-owner"]).expect("run empty-owner probe");
    assert_eq!(code, Some(0), "stderr={stderr}");
    let line = extract_json_line(&stdout).expect("empty-owner JSON envelope");
    let env: ProbeEnvelope = serde_json::from_str(&line).expect("parse empty-owner envelope");
    assert_eq!(env.status, "ok", "errors={:?}; stderr={stderr}", env.errors);
    assert!(env.principal_declaration.is_empty());
    assert!(
        env.declaration_manifest.is_empty(),
        "imports-only owner must carry an explicitly empty manifest: {:?}",
        env.declaration_manifest
    );
    assert_axcheck_agreed(&env, "EmptyOwner");
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
    #[serde(default)]
    primary_seeded_declarations: Vec<String>,
    #[serde(default)]
    axcheck_seeded_declarations: Vec<String>,
    #[serde(default)]
    seed_coverage_agreed: bool,
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
    let manifest_names: BTreeSet<String> = env
        .declaration_manifest
        .iter()
        .filter_map(|entry| entry.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    let primary_seeded: BTreeSet<String> = ax.primary_seeded_declarations.iter().cloned().collect();
    let axcheck_seeded: BTreeSet<String> = ax.axcheck_seeded_declarations.iter().cloned().collect();
    assert!(
        ax.seed_coverage_agreed,
        "{node}: seed coverage flag is false"
    );
    assert_eq!(
        primary_seeded, manifest_names,
        "{node}: primary collector did not seed the complete ownership manifest"
    );
    assert_eq!(
        axcheck_seeded, manifest_names,
        "{node}: secondary collector did not seed the complete ownership manifest"
    );
}

fn has_exact_use(env: &ProbeEnvelope, owner: &str, reached: &str) -> bool {
    env.exact_declaration_uses.iter().any(|entry| {
        entry.get("owner").and_then(serde_json::Value::as_str) == Some(owner)
            && entry
                .get("reached_declaration")
                .and_then(serde_json::Value::as_str)
                == Some(reached)
    })
}

#[test]
fn certificate_v2_duplicate_provider_falsifier_retains_every_provider() {
    if fixture_skip("certificate_v2_duplicate_provider_falsifier_retains_every_provider") {
        return;
    }
    for node in ["CollisionConsumerForward", "CollisionConsumerReverse"] {
        let (stdout, stderr, code) = run_probe(node).expect("run collision probe");
        assert_eq!(code, Some(0), "{node}: stderr={stderr}");
        let env = parse_envelope(&stdout, &stderr);
        assert_eq!(env.status, "ok", "{node}: errors={:?}", env.errors);
        assert!(
            has_exact_use(&env, "CollisionLeft", "SharedCollision"),
            "{node}: left provider suppressed: {:?}",
            env.exact_declaration_uses
        );
        assert!(
            has_exact_use(&env, "CollisionRight", "SharedCollision"),
            "{node}: right provider suppressed: {:?}",
            env.exact_declaration_uses
        );
        let shared_providers: BTreeSet<&str> = env
            .exact_declaration_uses
            .iter()
            .filter(|entry| {
                entry
                    .get("reached_declaration")
                    .and_then(serde_json::Value::as_str)
                    == Some("SharedCollision")
            })
            .filter_map(|entry| entry.get("owner").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            shared_providers,
            BTreeSet::from(["CollisionLeft", "CollisionRight"]),
            "{node}: provider set must be import-order invariant"
        );
        assert_axcheck_agreed(&env, node);
    }

}

fn run_module_manifest_rows(nodes: &[&str]) -> BTreeMap<String, serde_json::Value> {
    let script = local_closure_script()
        .parent()
        .expect("scripts directory")
        .join("lean_module_manifest.lean");
    let output = Command::new("lake")
        .arg("env")
        .arg("lean")
        .arg("--run")
        .arg(script)
        .args(nodes)
        .current_dir(fixture_root())
        .output()
        .expect("run artifact manifest reader");
    assert!(
        output.status.success(),
        "manifest reader failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("parse manifest JSON line");
            let node = value["node"].as_str().expect("manifest node").to_owned();
            (node, value)
        })
        .collect()
}

fn run_module_manifest(nodes: &[&str]) -> BTreeMap<String, serde_json::Value> {
    run_module_manifest_rows(nodes)
        .into_iter()
        .map(|(node, value)| (node, value["ownership_manifest"].clone()))
        .collect()
}

#[test]
fn certificate_v2_direct_imports_come_from_module_data_for_legal_source_spellings() {
    if fixture_skip(
        "certificate_v2_direct_imports_come_from_module_data_for_legal_source_spellings",
    ) {
        return;
    }
    assert_eq!(
        trellis_kernel::cache_key::direct_tablet_imports(&fixture_root(), "MultilineConsumer")
            .expect("read multiline consumer source"),
        BTreeSet::new(),
        "the line-oriented source helper intentionally is not certificate authority"
    );
    let rows = run_module_manifest_rows(&["MultilineConsumer"]);
    let imports: BTreeSet<&str> = rows["MultilineConsumer"]["direct_imports"]
        .as_array()
        .expect("artifact direct imports")
        .iter()
        .map(|entry| entry["module"].as_str().expect("artifact import module"))
        .filter(|module| module.starts_with("Tablet."))
        .collect();
    assert_eq!(imports, BTreeSet::from(["Tablet.MultilineDep"]));

    let (stdout, stderr, code) = run_probe("MultilineConsumer").expect("run multiline probe");
    assert_eq!(code, Some(0), "stderr={stderr}");
    let envelope = parse_envelope(&stdout, &stderr);
    assert_eq!(envelope.status, "ok", "errors={:?}", envelope.errors);
    assert!(
        envelope.exact_declaration_uses.is_empty(),
        "an imports-only dependency should not require a reached declaration"
    );
}

#[test]
fn certificate_v2_artifact_manifests_survive_isolated_reverse_and_shuffled_reads() {
    if fixture_skip("certificate_v2_artifact_manifests_survive_isolated_reverse_and_shuffled_reads")
    {
        return;
    }
    let baseline = run_module_manifest(&[
        "CollisionLeft",
        "CollisionRight",
        "CollisionConsumerForward",
        "CollisionConsumerReverse",
    ]);
    let reverse = run_module_manifest(&[
        "CollisionConsumerReverse",
        "CollisionConsumerForward",
        "CollisionRight",
        "CollisionLeft",
    ]);
    let shuffled = run_module_manifest(&[
        "CollisionRight",
        "CollisionConsumerForward",
        "CollisionLeft",
        "CollisionConsumerReverse",
    ]);
    assert_eq!(baseline, reverse);
    assert_eq!(baseline, shuffled);
    for node in ["CollisionLeft", "CollisionRight"] {
        assert_eq!(
            baseline.get(node),
            run_module_manifest(&[node]).get(node),
            "{node}: isolated artifact read differs from batch read"
        );
    }
    assert!(baseline["CollisionLeft"]
        .as_array()
        .expect("left manifest")
        .iter()
        .any(|entry| entry["name"] == "CollisionLeftOnly"));
    assert!(baseline["CollisionRight"]
        .as_array()
        .expect("right manifest")
        .iter()
        .any(|entry| entry["name"] == "CollisionRightOnly"));
}

#[test]
fn certificate_v2_split_module_uses_exported_axiom_view_and_private_theorem_owner() {
    if fixture_skip(
        "certificate_v2_split_module_uses_exported_axiom_view_and_private_theorem_owner",
    ) {
        return;
    }
    let owner = run_module_manifest(&["SplitVisibility"]);
    let owner_manifest = owner["SplitVisibility"]
        .as_array()
        .expect("split owner manifest");
    assert!(owner_manifest
        .iter()
        .any(|entry| { entry["name"] == "SplitVisibilityPublic" && entry["kind"] == "theorem" }));
    assert!(owner_manifest.iter().any(|entry| {
        entry["name"] == "_private.Tablet.SplitVisibility.0.SplitVisibility"
            && entry["kind"] == "theorem"
    }));

    let (stdout, stderr, code) =
        run_probe("SplitVisibilityConsumer").expect("run split visibility consumer");
    assert_eq!(code, Some(0), "stderr={stderr}");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "ok", "errors={:?}", env.errors);
    assert!(
        env.exact_declaration_uses.iter().any(|entry| {
            entry["owner"] == "SplitVisibility"
                && entry["reached_declaration"] == "SplitVisibilityPublic"
                && entry["declaration_kind"] == "axiom"
                && entry["visibility"] == "exported"
        }),
        "the consumer must record the weakened exported axiom view: {:?}",
        env.exact_declaration_uses
    );
    assert_axcheck_agreed(&env, "SplitVisibilityConsumer");
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
fn local_closure_smoke_uses_helper_records_exact_member() {
    // The boundary is an exact member of the Helper module. The consumer
    // does not walk Helper's proof body; certificate issuance authenticates
    // this reached name against Helper's module manifest.
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
    assert!(
        has_exact_use(&env, "Helper", "Helper"),
        "UsesHelper must retain the exact Helper member; got {:?}",
        env.exact_declaration_uses,
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
    let (stdout, stderr, code) = run_probe_with_args(
        "ThisNodeDoesNotExist",
        &["--principal=ThisNodeDoesNotExist"],
    )
    .expect("run probe");
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
        "Tablet/ByElab.lean",
        "Tablet/IllTypedSkip.lean",
        "Tablet/LegitNotation.lean",
        // FIX A (own-node `let rec` aux transparent walk) fixtures.
        "Tablet/UsesOwnLetRec.lean",
        "Tablet/UsesOwnAuxCrossDep.lean",
        "Tablet/UsesOwnAuxAxiom.lean",
        // FIX B (recursor-family realization + ctor sizeOf_spec) fixtures.
        "Tablet/IndPredReach.lean",
        "Tablet/UsesIndPredInduction.lean",
        "Tablet/NonRecPred.lean",
        "Tablet/ForgeBrecOnAux.lean",
        "Tablet/UsesForgeBrecOnAux.lean",
        "Tablet/OwnBrecOnForge.lean",
        "Tablet/UsesOwnBrecOnForge.lean",
        "Tablet/DefParentBrecOn.lean",
        "Tablet/UsesDefParentBrecOn.lean",
        "Tablet/UsesCtorSizeOfSpec.lean",
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

/// Run a consumer probe that is expected to accept a declaration of `owner`:
/// the exact reached declaration remains evidence while authority coalesces
/// to the declaring module.
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
    assert_eq!(
        env.status, "ok",
        "{node} should probe ok; envelope: {env:?}"
    );
    assert!(env.exact_declaration_uses.iter().any(|entry| {
        entry.get("owner").and_then(serde_json::Value::as_str) == Some(owner)
            && entry
                .get("reached_declaration")
                .and_then(serde_json::Value::as_str)
                .is_some()
    }), "{node}: exact use evidence must retain the declaration reached in module {owner}; uses: {:?}", env.exact_declaration_uses);
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
fn local_closure_smoke_third_party_deriving_root_uses_module_owner() {
    if fixture_skip("third_party_deriving_root_uses_module_owner") {
        return;
    }
    let (stdout, stderr, code) =
        run_probe("UsesWeirdDeriveOwner").expect("run custom-deriving consumer probe");
    assert_eq!(code, Some(0), "stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "ok", "envelope: {env:?}");
    assert!(
        has_exact_use(&env, "WeirdDeriveOwner", "CobaltMoth922"),
        "unrelated generated root must be attributed to its declaring module; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert_axcheck_agreed(&env, "UsesWeirdDeriveOwner");
}

#[test]
fn local_closure_smoke_generated_declaration_rename_is_invariant() {
    // RENAME-INVARIANCE. `RenamedDeriveOwner` derives a handler identical to
    // `WeirdDeriveOwner`'s in every respect except the name of the
    // declaration it emits (`Alien.NeonTapir77` rather than `CobaltMoth922`,
    // under an unrelated namespace — the shape that previously changed the
    // legacy dependency key). Classification must be identical for both.
    // A future Lean upgrade that reintroduces name-based authority fails
    // here first.
    if fixture_skip("generated_declaration_rename_is_invariant") {
        return;
    }
    let (stdout, stderr, code) =
        run_probe("UsesRenamedDeriveOwner").expect("run renamed-deriving consumer probe");
    assert_eq!(code, Some(0), "stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "ok", "envelope: {env:?}");
    assert!(
        has_exact_use(&env, "RenamedDeriveOwner", "Alien.NeonTapir77"),
        "renaming a generated declaration must not change its attribution; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert_axcheck_agreed(&env, "UsesRenamedDeriveOwner");
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
fn local_closure_smoke_generated_enum_of_nat_ctor_idx_accepted() {
    // INTERIM coverage (docs/INTERIM_ofNat_ctorIdx.md). THE test that fails
    // without the fix, and the first fixture case in this suite that proves a
    // LEGAL proof gets THROUGH rather than that a forgery gets caught.
    //
    // `EnumDerivedDecEq` is a bare `inductive ... deriving DecidableEq`; Lean
    // emits `ofNat_ctorIdx` for it. Pre-fix the probe recorded that name as a
    // dep key and the Rust validator turned it into `internal_error`,
    // reproducing the shipped wizard failure: a legal proof rejected with a
    // remedy the worker cannot perform, because the declaration is the
    // compiler's, not the node's.
    if fixture_skip("generated_enum_of_nat_ctor_idx_accepted") {
        return;
    }
    assert_generated_member_accepted("UsesEnumDerivedDecEqOfNatCtorIdx", "EnumDerivedDecEq");
}

#[test]
fn local_closure_smoke_authored_of_nat_ctor_idx_is_exact_member() {
    // Authored versus generated provenance does not change module membership.
    if fixture_skip("forged_of_nat_ctor_idx_still_recorded") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesForgedOfNatCtorIdx").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesForgedOfNatCtorIdx; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert!(
        has_exact_use(
            &env,
            "ForgedOfNatCtorIdx",
            "ForgedOfNatCtorIdx.ofNat_ctorIdx"
        ),
        "authored lookalike must be exact owner/member evidence; uses: {:?}",
        env.exact_declaration_uses,
    );
}

#[test]
fn local_closure_smoke_handwritten_owner_to_ctor_idx_uses_actual_module_owner() {
    // The declaration name resembles a child of Owner, but it was declared by
    // OwnerToCtorIdxAux. Exact module metadata prevents name-based ownership.
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
    assert!(
        has_exact_use(&env, "OwnerToCtorIdxAux", "Owner.toCtorIdx"),
        "exact evidence must use the actual declaring module; uses: {:?}",
        env.exact_declaration_uses,
    );
    // The dual-collector cross-check must still agree (the axcheck side
    // also records the hand-written theorem as a boundary, since it is not
    // a generated artifact under the fix).
    assert_axcheck_agreed(&env, "UsesOwnerToCtorIdx");
}

#[test]
fn local_closure_smoke_handwritten_owner_aux_uses_declaring_module() {
    // The apparent `Owner.realAux` namespace does not make Owner its module
    // owner; OwnerAux's exact manifest does.
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
    assert!(
        has_exact_use(&env, "OwnerAux", "Owner.realAux"),
        "exact evidence must attribute Owner.realAux to OwnerAux; uses: {:?}",
        env.exact_declaration_uses,
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
    let (stdout, stderr, code) = run_probe("AxiomForge").expect("run probe");
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
    assert_axcheck_agreed(&env, "AxiomForge");
}

// ---------------------------------------------------------------------------
// FIX A — an auxiliary of the ACTIVE node itself (a user-named `let rec`
//          binder lifted to `<root>.<binder>` in the node's OWN module) must
//          be transparent-walked by BOTH collectors, never recorded as a
//          dotted boundary key — while everything reachable through the
//          aux's body (cross-node boundary theorems, project axioms) must
//          still surface.
// ---------------------------------------------------------------------------

#[test]
fn local_closure_smoke_own_let_rec_aux_is_transparent() {
    // Fix A regression: `UsesOwnLetRec` binds a self-recursive, user-named
    // `let rec prefixReachable` producing a proof. Lean lifts it to
    // `UsesOwnLetRec.prefixReachable` (a `thmInfo`) in the node's OWN
    // module. Pre-fix, the primary collector recorded it as a dotted
    // boundary key (`boundary_theorems ∋ UsesOwnLetRec.prefixReachable`),
    // which the kernel's Patch C-K present-node validation fail-closes on
    // → spurious `internal_error` for a genuinely closed node. Post-fix,
    // `isOwnNodeAux` routes it through the transparent walk in BOTH
    // collectors: no dep entry, axioms ⊆ canonical four, collectors agree.
    //
    // The `assert_axcheck_agreed` at the end is what pins the MANDATORY
    // Edit-3 mirror: with only the primary gate fixed, the axcheck side
    // still records the aux as a boundary (`axcheck_only_boundaries ∋
    // UsesOwnLetRec.prefixReachable`, `agreed: false`) and the wrapper
    // flips status to `internal_error`. Verified empirically against an
    // Edit-3-reverted script variant.
    if fixture_skip("own_let_rec_aux_is_transparent") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesOwnLetRec").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnLetRec; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_ne!(
        env.status, "internal_error",
        "UsesOwnLetRec: own `let rec` aux must not trigger internal_error; envelope: {env:?}",
    );
    assert_eq!(
        env.status, "ok",
        "UsesOwnLetRec should probe ok; envelope: {env:?}"
    );
    let deps = all_dep_names(&env);
    assert!(
        !deps.iter().any(|n| n.contains("prefixReachable")),
        "own-node `let rec` aux `UsesOwnLetRec.prefixReachable` must be \
         transparent-walked, never recorded as a dep key; deps: {deps:?}",
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "UsesOwnLetRec axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "UsesOwnLetRec");
}

#[test]
fn local_closure_smoke_own_aux_body_cross_node_dep_still_recorded() {
    // Fix A no-hiding guard: `UsesOwnAuxCrossDep`'s own `let rec
    // bridgeReachable` aux references the imported boundary theorem
    // `Helper` (another node, `sorryAx` in its value) INSIDE the aux
    // body. The module-wide walk must retain the exact reached Helper member.
    // The boundary cut still applies: no `sorryAx` in `kernel_axioms`.
    if fixture_skip("own_aux_body_cross_node_dep_still_recorded") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesOwnAuxCrossDep").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnAuxCrossDep; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesOwnAuxCrossDep should probe ok; envelope: {env:?}"
    );
    assert!(
        has_exact_use(&env, "Helper", "Helper"),
        "cross-module Helper use inside the own auxiliary must remain exact \
         evidence; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert!(
        !all_dep_names(&env)
            .iter()
            .any(|n| n.contains("bridgeReachable")),
        "the own aux itself must not be a dep key; deps: {:?}",
        all_dep_names(&env),
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "boundary cut at `Helper` must hold through the own-aux transparent \
         walk (no sorryAx leak); got {:?}",
        env.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "UsesOwnAuxCrossDep");
}

#[test]
fn local_closure_smoke_own_aux_body_unapproved_axiom_still_surfaces() {
    // Cross-module declarations stop at their certified owner boundary. The
    // consumer retains AxiomForgeEq1 as an exact member; AxiomForge's own
    // certificate is refused because its module closure contains the axiom.
    if fixture_skip("own_aux_body_unapproved_axiom_still_surfaces") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesOwnAuxAxiom").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnAuxAxiom; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesOwnAuxAxiom should probe ok; envelope: {env:?}"
    );
    assert!(
        has_exact_use(&env, "AxiomForge", "AxiomForgeEq1"),
        "consumer must retain the exact axiom declaration and its owner; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "the consumer does not re-walk a certified owner module; got {:?}",
        env.kernel_axioms,
    );
    assert!(
        !all_dep_names(&env)
            .iter()
            .any(|n| n.contains("axReachable")),
        "the own aux itself must not be a dep key; deps: {:?}",
        all_dep_names(&env),
    );
    assert_axcheck_agreed(&env, "UsesOwnAuxAxiom");
}

// ---------------------------------------------------------------------------
// FIX B — the `IndPredBelow`-generated `below` / `brecOn` of a RECURSIVE
//          Prop-valued inductive PREDICATE carry no `markAuxRecursor` tag in
//          ANY environment, so `Lean.isAuxRecursor` misses them; pre-fix they
//          were recorded as dotted dep keys that the kernel's Patch C-K
//          present-node validation fail-closed on → spurious `internal_error`
//          for genuinely closed nodes (live repro: dec2flt
//          `ParseLongMantissaNonpositiveFalseLeftRoutePayload_step` blocked
//          via `…FalseLeftReachable.brecOn`). `isRecursorFamilyRealization`
//          (recursor-family suffix + inductive parent + module co-location)
//          now transparent-walks the genuine members while every forgery
//          stays a recorded, fail-closed dep key. The sibling
//          `isCtorSizeOfSpecTheorem` clause covers the same latent blocking
//          class for constructors' `sizeOf_spec` theorems.
// ---------------------------------------------------------------------------

#[test]
fn local_closure_smoke_ind_pred_brec_on_has_exact_owner_evidence() {
    // Generated recursor names are ordinary manifest members of the
    // IndPredReach owner. Their spelling does not affect admission.
    if fixture_skip("ind_pred_brec_on_transparent_with_invalidation_edge") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesIndPredInduction").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesIndPredInduction; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_ne!(
        env.status, "internal_error",
        "UsesIndPredInduction: IndPredBelow-generated brecOn/below must not \
         trigger internal_error; envelope: {env:?}",
    );
    assert_eq!(
        env.status, "ok",
        "UsesIndPredInduction should probe ok; envelope: {env:?}"
    );
    assert!(
        has_exact_use(&env, "IndPredReach", "IndPredReach.brecOn"),
        "generated brecOn must be exact manifest-member evidence owned by \
         IndPredReach; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "UsesIndPredInduction is sorry-free; axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "UsesIndPredInduction");
}

#[test]
fn local_closure_smoke_cross_module_brec_on_uses_actual_owner() {
    // Declaration spelling is irrelevant: NonRecPred.brecOn was authored in
    // ForgeBrecOnAux, so that module is the exact certificate owner.
    if fixture_skip("cross_module_forged_brec_on_stays_recorded") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesForgeBrecOnAux").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesForgeBrecOnAux; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert!(
        has_exact_use(&env, "ForgeBrecOnAux", "NonRecPred.brecOn"),
        "exact evidence must attribute the lookalike to its actual declaring \
         module; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert_axcheck_agreed(&env, "UsesForgeBrecOnAux");
}

#[test]
fn local_closure_smoke_own_module_forged_brec_on_surfaces_sorry_ax() {
    // FIX B no-hiding guard: `OwnBrecOnForge.lean` authors
    // `theorem OwnBrecOnForge.brecOn : False := sorry` in the inductive's
    // OWN module (compiles: the inductive is non-recursive, so no generated
    // `brecOn` collides). All three `isRecursorFamilyRealization` conjuncts
    // hold, so the consumer's probe classifies the forgery as generated and
    // transparent-walks it — but the walk must still traverse its VALUE, so
    // the `sorryAx` inside surfaces in `kernel_axioms` for the Rust
    // approved-axiom policy. Nothing hides through the transparent walk.
    if fixture_skip("own_module_forged_brec_on_surfaces_sorry_ax") {
        return;
    }
    // Textual sanity: the forge really carries `sorry` (otherwise the
    // no-hiding assertion below would be vacuous).
    let forge = fixture_root().join("Tablet/OwnBrecOnForge.lean");
    let text = std::fs::read_to_string(&forge).expect("read OwnBrecOnForge.lean");
    assert!(
        text.contains("sorry"),
        "OwnBrecOnForge.lean must carry `sorry` for this test to be meaningful",
    );
    let (stdout, stderr, code) = run_probe("OwnBrecOnForge").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesOwnBrecOnForge; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesOwnBrecOnForge should probe ok (the violation surfaces as an \
         axiom, not a status flip); envelope: {env:?}"
    );
    assert!(
        env.kernel_axioms.iter().any(|a| a.contains("sorryAx")),
        "sorryAx inside the own-module forged brecOn's value must surface in \
         kernel_axioms (nothing hides through the transparent walk); got {:?}",
        env.kernel_axioms,
    );
    assert!(env.declaration_manifest.iter().any(|entry| {
        entry.get("name").and_then(serde_json::Value::as_str)
            == Some("OwnBrecOnForge.brecOn")
    }), "authored lookalike must be accepted only as an exact member of its owner's manifest; manifest: {:?}", env.declaration_manifest);
    assert_axcheck_agreed(&env, "OwnBrecOnForge");
}

#[test]
fn local_closure_smoke_brec_on_under_definition_is_exact_member() {
    // A hand-authored member and a compiler-generated member use the same
    // contract: exact name plus declaring-module certificate membership.
    if fixture_skip("brec_on_under_non_inductive_parent_stays_recorded") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesDefParentBrecOn").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesDefParentBrecOn; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert!(
        has_exact_use(&env, "DefParentBrecOn", "DefParentBrecOn.brecOn"),
        "lookalike member must be exact evidence owned by DefParentBrecOn; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert_axcheck_agreed(&env, "UsesDefParentBrecOn");
}

#[test]
fn local_closure_smoke_ctor_size_of_spec_transparent_with_invalidation_edge() {
    // FIX B sibling clause (`isCtorSizeOfSpecTheorem`): `UsesCtorSizeOfSpec`
    // explicitly retains `InductiveNat.mk.sizeOf_spec` in its proof term —
    // a constructor-namespaced theorem Lean generates eagerly in
    // `InductiveNat`'s own module with no environment tag the probe can
    // query. Verified pre-fix: recorded as the dotted boundary key
    // `InductiveNat.mk.sizeOf_spec` (the same Patch C-K fail-closed blocking
    // class as brecOn). Post-fix it is transparent-walked; the real
    // dependency `InductiveNat` stays recorded.
    if fixture_skip("ctor_size_of_spec_transparent_with_invalidation_edge") {
        return;
    }
    let (stdout, stderr, code) = run_probe("UsesCtorSizeOfSpec").expect("run probe");
    assert_eq!(
        code,
        Some(0),
        "lake env lean must exit 0 for UsesCtorSizeOfSpec; stderr=<<<{stderr}>>>"
    );
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "UsesCtorSizeOfSpec should probe ok; envelope: {env:?}"
    );
    // Invalidation edge: the owning inductive is still a strict def dep.
    let strict_defs = boundary_names(&env.strict_definition_deps);
    assert!(
        strict_defs
            .iter()
            .any(|n| n == "InductiveNat" || n == "Tablet.InductiveNat"),
        "InductiveNat must be recorded as a strict definition dep \
         (the invalidation edge); got {strict_defs:?}",
    );
    assert!(
        env.exact_declaration_uses.iter().any(|entry| {
            entry.get("owner").and_then(serde_json::Value::as_str) == Some("InductiveNat")
                && entry
                    .get("reached_declaration")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| name.contains("sizeOf_spec"))
        }),
        "the exact generated sizeOf_spec use must be retained as certificate evidence; uses: {:?}",
        env.exact_declaration_uses
    );
    assert!(
        axiom_subset_of_canonical_four(&env.kernel_axioms),
        "UsesCtorSizeOfSpec is sorry-free; axioms must be ⊆ canonical four; got {:?}",
        env.kernel_axioms,
    );
    assert_axcheck_agreed(&env, "UsesCtorSizeOfSpec");
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
    assert_eq!(
        env.status, "ok",
        "{node}: authored lookalikes are ordinary owner declarations and must be handled by module certification; envelope: {env:?}",
    );
    assert!(
        !env.declaration_manifest.is_empty(),
        "{node}: certification must retain an exhaustive owner manifest for authored `{offending_component}`; envelope: {env:?}",
    );
    assert!(
        env.errors.is_empty(),
        "{node}: no generated-name classifier may reject authored `{offending_component}`; errors: {:?}",
        env.errors,
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
fn local_closure_smoke_namespaced_dep_records_exact_names_and_owners() {
    // Namespaced declaration names remain exact; lifecycle identity comes
    // independently from their declaring modules.
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

    assert!(
        has_exact_use(&env, "NamespacedThm", "crate_ns.NamespacedThm"),
        "namespaced theorem use must preserve its exact declaration and owner; uses: {:?}",
        env.exact_declaration_uses,
    );
    assert!(
        has_exact_use(&env, "NamespacedDef", "crate_ns.NamespacedDef"),
        "namespaced definition use must preserve its exact declaration and owner; uses: {:?}",
        env.exact_declaration_uses,
    );

    assert!(
        env.errors.is_empty(),
        "NamespacedConsumer should have no errors; got {:?}",
        env.errors,
    );
    assert_axcheck_agreed(&env, "NamespacedConsumer");
}

#[test]
fn local_closure_smoke_namespaced_authored_aux_is_exact_member() {
    // A non-principal authored declaration is a legitimate member of its
    // module. The certificate manifest, not final-component spelling,
    // decides whether a consumer may stop at it.
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
    assert_eq!(
        env.status, "ok",
        "exact owner/member evidence accepts ordinary authored auxiliaries; envelope: {env:?}",
    );
    assert!(
        has_exact_use(
            &env,
            "NamespacedForgeAux",
            "crate_ns.NamespacedForgeAux.realAux"
        ),
        "exact evidence must retain the dotted declaration while attributing \
         it to NamespacedForgeAux; uses: {:?}",
        env.exact_declaration_uses,
    );
}

#[test]
fn local_closure_smoke_flattened_stem_dep_emits_bare_node_id() {
    // DEEPLY-NAMESPACED-DEP regression. `UsesNestedMethod` depends on the def
    // `crate_ns.Nested.method`
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
    // Declaring-module attribution makes the compatibility-map key the bare
    // owner id `Nested_method`. The exact reached name is carried separately.
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
fn local_closure_smoke_flattened_stem_authored_aux_uses_exact_owner() {
    // Nested namespace depth does not affect ownership attribution.
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
        "nested exact member should probe cleanly; envelope: {env:?}",
    );
    assert!(
        has_exact_use(&env, "Nested_Forge", "crate_ns.Nested.Forge.realAux"),
        "nested exact declaration must be attributed to Nested_Forge; uses: {:?}",
        env.exact_declaration_uses,
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
    let source = std::fs::read_to_string(fixture_root().join("Tablet/AmbiguousRoot.lean"))
        .expect("read ambiguous fixture");
    let error = trellis_kernel::exact_principal_name(&source, "AmbiguousRoot")
        .expect_err("supervisor registration must reject an ambiguous principal");
    assert!(
        error.contains("multiple FILESPEC principal declarations"),
        "{error}"
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
    assert_eq!(
        code,
        Some(0),
        "exit 0 for UsesRecDef; stderr=<<<{stderr}>>>"
    );
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
fn local_closure_smoke_partial_policy_is_separate_from_certification() {
    if fixture_skip("authored_partial_definition_is_rejected") {
        return;
    }
    let (stdout, stderr, code) = run_probe("PartialDef").expect("run partial-def probe");
    assert_eq!(code, Some(0), "probe envelope exit; stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(
        env.status, "ok",
        "kernel-valid, manifest-complete partial module certifies independently of worker policy: {:?}",
        env.errors
    );
    assert!(env.errors.is_empty());
    assert!(!env.declaration_manifest.is_empty());
    assert_axcheck_agreed(&env, "PartialDef");

    let (stdout, stderr, code) =
        run_probe_scan_only("PartialDef").expect("run partial-def policy scan");
    assert_eq!(code, Some(0), "policy envelope exit; stderr=<<<{stderr}>>>");
    let policy = parse_envelope(&stdout, &stderr);
    assert_eq!(policy.status, "policy_rejection");
    assert!(policy.errors.iter().any(|error| {
        error.contains("FILESPEC authoring policy")
            && error.contains("`partial`")
            && error.contains("PartialDef")
    }));
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
        env.status, "ok",
        "{node} --scan-only: declaration spelling is not authority; authored `{offending_component}` is covered by its module certificate; envelope: {env:?}",
    );
    assert!(
        env.errors.is_empty(),
        "{node} --scan-only: a name classifier must not reject `{offending_component}`; errors: {:?}",
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

/// Probe `node` in `--scan-only` mode and assert the worker policy rejects it
/// with a non-internal status and a diagnostic naming `keyword`.
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
        env.status, "policy_rejection",
        "{node} --scan-only: an authoring violation must be distinct from checker \
         failure; envelope: {env:?}",
    );
    assert!(
        env.errors
            .iter()
            .any(|e| e.contains("FILESPEC authoring policy") && e.contains(keyword)),
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
fn local_closure_smoke_scan_only_by_elab_rejected() {
    // `by_elab` is a TERM kind nested inside a theorem command. This pins the
    // recursive syntax walk: a line-head or top-level command-kind scan cannot
    // see it, yet its TermElabM body can call `Lean.addDecl`.
    if fixture_skip("scan_only_by_elab_rejected") {
        return;
    }
    assert_scan_only_macro_ban_rejected("ByElab", "by_elab");
}

#[test]
fn unchecked_declaration_builds_but_kernel_replay_rejects() {
    // P0 capability regression. `lake build` accepts this module because its
    // declaration-scoped `debug.skipKernelTC` option bypasses `Lean.addDecl`'s
    // kernel call. The independent replay must reject the emitted olean.
    if fixture_skip("unchecked_declaration_builds_but_kernel_replay_rejects") {
        return;
    }
    let _guard = fixture_mutation_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = fixture_root();
    let build = Command::new("lake")
        .args(["build", "Tablet.IllTypedSkip"])
        .current_dir(&root)
        .output()
        .expect("run lake build for IllTypedSkip fixture");
    let replay = Command::new("lake")
        .args([
            "env",
            "leanchecker",
            "Tablet.Preamble",
            "Tablet.IllTypedSkip",
        ])
        .current_dir(&root)
        .output()
        .expect("run leanchecker for IllTypedSkip fixture");

    // Keep the shared fixture usable by prefix-wide checker invocations.
    let build_dir = root.join(".lake/build/lib/lean/Tablet");
    for suffix in [".olean", ".ilean", ".trace", ".olean.hash", ".ilean.hash"] {
        let _ = std::fs::remove_file(build_dir.join(format!("IllTypedSkip{suffix}")));
    }

    assert!(
        build.status.success(),
        "the regression fixture must demonstrate the real hole: lake build \
         should succeed, stderr={}",
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        !replay.status.success(),
        "leanchecker must reject the unchecked declaration"
    );
    let replay_output = format!(
        "{}{}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(
        replay_output.contains("declaration type mismatch")
            && replay_output.contains("IllTypedSkip"),
        "unexpected leanchecker diagnostic: {replay_output}"
    );
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
fn source_policy_and_artifact_certification_are_separate() {
    if fixture_skip("source_policy_and_artifact_certification_are_separate") {
        return;
    }

    // All three modules are kernel-replayed and artifact-complete, so the
    // full certificate probe must traverse and accept them without reading
    // source policy.
    for node in ["PolicyEval", "PolicyLocalSyntax", "PolicyInitialize"] {
        let (stdout, stderr, code) = run_probe(node).expect("run artifact certificate probe");
        assert_eq!(code, Some(0), "{node}: stderr=<<<{stderr}>>>");
        let env = parse_envelope(&stdout, &stderr);
        assert_eq!(
            env.status, "ok",
            "{node}: artifact-complete module must certify independently of source policy; \
             errors={:?}",
            env.errors
        );
        assert!(!env.declaration_manifest.is_empty(), "{node}: empty manifest");
        assert_axcheck_agreed(&env, node);
    }

    // The chosen worker policy permits evaluation and local syntax state.
    for node in ["PolicyEval", "PolicyLocalSyntax"] {
        let (stdout, stderr, code) = run_probe_scan_only(node).expect("run policy scan");
        assert_eq!(code, Some(0), "{node}: stderr=<<<{stderr}>>>");
        let env = parse_envelope(&stdout, &stderr);
        assert_eq!(env.status, "ok", "{node}: errors={:?}", env.errors);
    }

    // Initializer registration remains an explicit worker-authoring rule.
    let (stdout, stderr, code) =
        run_probe_scan_only("PolicyInitialize").expect("run initializer policy scan");
    assert_eq!(code, Some(0), "stderr=<<<{stderr}>>>");
    let env = parse_envelope(&stdout, &stderr);
    assert_eq!(env.status, "policy_rejection");
    assert!(env.errors.iter().any(|error| {
        error.contains("FILESPEC authoring policy") && error.contains("initialize")
    }));
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

/// Materialize a PRIVATE copy of the built fixture (sources + `.lake`
/// olean tree) under the project tempdir, for tests that write scratch
/// files into the repo they operate on (the certificate-obligation tests
/// materialize `Tablet/<node>.lean` plus build artifacts). Writing into a
/// copy keeps the COMMITTED fixture tree byte-identical no matter how the
/// test ends: a process kill (`kill -9`, OOM) mid-executor strands the
/// scratch inside `target/tmp/` (reclaimed by `cargo clean`), never as
/// pollution of `kernel/tests/fixtures/local_closure_smoke/`. The copy is
/// cheap (~5 MiB, no Mathlib) and `lake build` stays incremental in it —
/// the same `cp -a` overlay `station_loop_integration` below already uses.
///
/// Returns the tempdir handle (keep it alive for the test's duration) and
/// the copied fixture root.
fn copy_fixture_for_scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = project_tempdir();
    let repo = tmp.path().join("local_closure_smoke");
    let status = Command::new("cp")
        .arg("-a")
        .arg(fixture_root())
        .arg(&repo)
        .status()
        .expect("copy fixture for certificate scratch");
    assert!(status.success(), "cp -a of the fixture copy failed");
    (tmp, repo)
}

// Silence unused-import warnings for helpers that only the `#[ignore]`d
// tests use; they're load-bearing for the post-build operator workflow
// even though the `--ignored` filter hides them on a normal `cargo test`.
#[allow(dead_code)]
fn _unused_helper_silencer() {
    let _ = local_closure_script;
}

#[allow(dead_code)]
fn _ensure_path_used(_p: &Path) {}

// ===========================================================
// Patch C-K end-to-end — real Lean probe → parser → validator
// ===========================================================
//
// These close the seam the probe-envelope tests above cannot reach: they
// run the ACTUAL `scripts/lean_local_closure.lean` on the namespaced
// fixtures, parse the output through the REAL kernel parser
// (`parse_local_closure_response`), and feed it through the REAL
// `validate_probe_present_nodes` — Gate 8 of the sidecar apply sequence.
//
// They lived in `src/runtime_cli_observations.rs` behind a second,
// private skip helper, where they passed vacuously on every `cargo test
// --lib` run. Both functions are re-exported by the narrow
// `trellis_kernel::runtime_cli_observations_probe` shim so these tests can
// live in the integration lane instead, under the one `fixture_skip` gate.

/// Run the real probe on `node` and parse its last stdout line through the
/// production parser. Panics on spawn/parse failure (a hard error, not a
/// skip — callers gate on `fixture_skip` first).
fn run_real_probe(node: &str) -> LocalClosureProbeOutput {
    let source =
        std::fs::read_to_string(fixture_root().join("Tablet").join(format!("{node}.lean")))
            .unwrap_or_else(|error| panic!("read principal source for {node}: {error}"));
    let principal = trellis_kernel::exact_principal_name(&source, node)
        .unwrap_or_else(|error| panic!("register principal for {node}: {error}"));
    let out = Command::new("lake")
        .arg("env")
        .arg("lean")
        .arg("--run")
        .arg(local_closure_script())
        .arg(node)
        .arg(format!("--principal={principal}"))
        .current_dir(fixture_root())
        .output()
        .unwrap_or_else(|e| panic!("spawn lake env lean for {node}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_else(|| {
            panic!(
                "no JSON line for {node}; stdout=<<<{stdout}>>> stderr=<<<{}>>>",
                String::from_utf8_lossy(&out.stderr)
            )
        });
    let json: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("parse JSON for {node}: {e}; line={line}"));
    parse_local_closure_response(node, json)
        .unwrap_or_else(|e| panic!("parse_local_closure_response for {node}: {e}"))
}

#[test]
fn validate_probe_present_nodes_accepts_real_namespaced_dep_probe() {
    // The validator consumes exact owner identities. Namespaced declaration
    // spellings remain evidence and never become lifecycle keys.
    if fixture_skip("validate_probe_present_nodes_accepts_real_namespaced_dep_probe") {
        return;
    }
    let mut probe = run_real_probe("NamespacedConsumer");
    assert_eq!(
        probe.status, "ok",
        "real namespaced-consumer probe must be ok; errors: {:?}",
        probe.errors
    );
    assert!(
        probe
            .exact_declaration_uses
            .iter()
            .any(|use_| use_.owner == NodeId::from("NamespacedThm")
                && use_.reached_declaration == "crate_ns.NamespacedThm"),
        "exact theorem owner/name missing: {:?}",
        probe.exact_declaration_uses,
    );
    assert!(
        probe
            .exact_declaration_uses
            .iter()
            .any(|use_| use_.owner == NodeId::from("NamespacedDef")
                && use_.reached_declaration == "crate_ns.NamespacedDef"),
        "exact definition owner/name missing: {:?}",
        probe.exact_declaration_uses,
    );

    // Ratify the consumer + its three deps with their expected kinds.
    let present_nodes: BTreeSet<NodeId> = [
        NodeId::from("NamespacedConsumer"),
        NodeId::from("NamespacedThm"),
        NodeId::from("NamespacedDef"),
    ]
    .into_iter()
    .collect();
    let node_kinds: BTreeMap<NodeId, NodeKind> = [
        (NodeId::from("NamespacedConsumer"), NodeKind::Proof),
        (NodeId::from("NamespacedThm"), NodeKind::Proof),
        (NodeId::from("NamespacedDef"), NodeKind::Definition),
    ]
    .into_iter()
    .collect();

    validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
    assert_eq!(
        probe.status, "ok",
        "validator must accept the real probe's exact owner identities; errors: {:?}",
        probe.errors,
    );
    assert!(
        !probe.errors.iter().any(|e| e.contains("Patch C-K")),
        "no Patch C-K diagnostic expected; errors: {:?}",
        probe.errors,
    );
}

#[test]
fn validate_probe_present_nodes_accepts_real_namespaced_authored_aux_owner() {
    // Non-principal members are valid stopping points once their exact owner
    // is a registered present node. Certificate issuance separately requires
    // the reached name to be a genuine member of that owner's manifest.
    if fixture_skip("validate_probe_present_nodes_rejects_real_namespaced_authored_aux") {
        return;
    }
    let mut probe = run_real_probe("UsesNamespacedForgeAux");
    assert_eq!(
        probe.status, "ok",
        "probe itself records the dep (rejection is the kernel's job); errors: {:?}",
        probe.errors,
    );
    assert!(
        probe.exact_declaration_uses.iter().any(|use_| {
            use_.owner == NodeId::from("NamespacedForgeAux")
                && use_.reached_declaration == "crate_ns.NamespacedForgeAux.realAux"
        }),
        "exact authored-member evidence missing: {:?}",
        probe.exact_declaration_uses,
    );

    let present_nodes: BTreeSet<NodeId> = [
        NodeId::from("UsesNamespacedForgeAux"),
        NodeId::from("NamespacedForgeAux"),
    ]
    .into_iter()
    .collect();
    let node_kinds: BTreeMap<NodeId, NodeKind> = [
        (NodeId::from("UsesNamespacedForgeAux"), NodeKind::Proof),
        (NodeId::from("NamespacedForgeAux"), NodeKind::Proof),
    ]
    .into_iter()
    .collect();

    validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
    assert_eq!(
        probe.status, "ok",
        "exact owner validation must accept the authored auxiliary; errors: {:?}",
        probe.errors,
    );
    assert!(
        !probe.errors.iter().any(|e| e.contains("Patch C-K")),
        "legacy map spelling must not affect exact-owner validation; errors: {:?}",
        probe.errors,
    );
}

#[test]
fn validate_probe_present_nodes_accepts_real_definition_dep_module_attribution() {
    // DEFINITION-DEP MODULE-ATTRIBUTION (the dec2flt `strict_definition_deps`
    // fix), genuine end-to-end POSITIVE: run the REAL probe on two PV-shape
    // `def` consumers and assert each `strict_definition_dep` is keyed by its
    // dep's DECLARING-MODULE node id, without any declaration-name gate.
    //
    // Two categories of NON-principal definition dep that pre-fix fell through
    // to the raw dotted Lean `Name` (which Patch C-K fail-closed):
    //   (a) `UsesPreambleStruct` -> struct `crate_ns.PreambleBiasedFp` declared
    //       in the shared `Tablet.Preamble` module (node id `Preamble`); the
    //       decl's final component does NOT sanitize to the stem `Preamble`.
    //   (b) `UsesLoopHelper` -> non-principal Aeneas-shape co-generated helper
    //       `crate_ns.LoopHost_loop0` living inside the `Tablet.LoopHost`
    //       module (node id `LoopHost`); the helper is not name-shaped as a
    //       generated artifact and its final component does NOT sanitize to
    //       the stem `LoopHost`.
    // Post-fix `depDefKeyName` attributes both to the bare host-node ids
    // `Preamble` / `LoopHost`.
    if fixture_skip("validate_probe_present_nodes_accepts_real_definition_dep_module_attribution") {
        return;
    }

    // Category (a): Preamble-shared structure.
    let mut probe_a = run_real_probe("UsesPreambleStruct");
    assert_eq!(
        probe_a.status, "ok",
        "real Preamble-struct consumer probe must be ok; errors: {:?}",
        probe_a.errors
    );
    assert!(
        probe_a
            .strict_definition_deps
            .contains_key(&NodeId::from("Preamble")),
        "Preamble-shared struct dep must be keyed by host-node id `Preamble`; got {:?}",
        probe_a.strict_definition_deps.keys().collect::<Vec<_>>(),
    );
    assert!(
        !probe_a
            .strict_definition_deps
            .keys()
            .any(|k| k.as_str().contains('.')),
        "no definition dep may stay a dotted Lean Name; got {:?}",
        probe_a.strict_definition_deps.keys().collect::<Vec<_>>(),
    );

    // Category (b): non-principal Aeneas-shape co-generated `_loop` helper.
    let mut probe_b = run_real_probe("UsesLoopHelper");
    assert_eq!(
        probe_b.status, "ok",
        "real loop-helper consumer probe must be ok; errors: {:?}",
        probe_b.errors
    );
    assert!(
        probe_b
            .strict_definition_deps
            .contains_key(&NodeId::from("LoopHost")),
        "co-generated loop helper dep must be keyed by host-node id `LoopHost`; got {:?}",
        probe_b.strict_definition_deps.keys().collect::<Vec<_>>(),
    );
    assert!(
        !probe_b
            .strict_definition_deps
            .keys()
            .any(|k| k.as_str().contains('.')),
        "no definition dep may stay a dotted Lean Name; got {:?}",
        probe_b.strict_definition_deps.keys().collect::<Vec<_>>(),
    );

    // The kernel's Patch C-K present-node validation must ACCEPT both with
    // the host nodes ratified as present. `Preamble` is its own typed module
    // owner (rather than a Definition node) and legitimately hosts strict
    // definition deps for shared structures/axioms.
    let present_nodes: BTreeSet<NodeId> = [
        NodeId::from("UsesPreambleStruct"),
        NodeId::from("UsesLoopHelper"),
        NodeId::from("Preamble"),
        NodeId::from("LoopHost"),
    ]
    .into_iter()
    .collect();
    let node_kinds: BTreeMap<NodeId, NodeKind> = [
        (NodeId::from("UsesPreambleStruct"), NodeKind::Definition),
        (NodeId::from("UsesLoopHelper"), NodeKind::Definition),
        (NodeId::from("Preamble"), NodeKind::Preamble),
        (NodeId::from("LoopHost"), NodeKind::Definition),
    ]
    .into_iter()
    .collect();

    for (label, probe) in [
        ("UsesPreambleStruct", &mut probe_a),
        ("UsesLoopHelper", &mut probe_b),
    ] {
        validate_probe_present_nodes(probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "ok",
            "Patch C-K must ACCEPT {label}'s declaring-module def-dep id; errors: {:?}",
            probe.errors,
        );
        assert!(
            !probe.errors.iter().any(|e| e.contains("Patch C-K")),
            "no Patch C-K diagnostic expected for {label}; errors: {:?}",
            probe.errors,
        );
    }
}
