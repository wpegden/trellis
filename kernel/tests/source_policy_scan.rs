//! Exercise the production source-policy scanner without importing or building
//! project modules. Unknown project grammar must not become policy evidence.
//! Uses the smoke fixture's Lean pin, or TRELLIS_POLICY_LEAN for another version.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

mod common;

fn lean_prefix() -> &'static PathBuf {
    static PREFIX: OnceLock<PathBuf> = OnceLock::new();
    PREFIX.get_or_init(|| {
        let lean = std::env::var_os("TRELLIS_POLICY_LEAN").unwrap_or_else(|| "lean".into());
        let output = Command::new(lean)
            .arg("--print-prefix")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/local_closure_smoke"))
            .env_remove("LEAN_SYSROOT")
            .env_remove("LEAN_PATH")
            .output()
            .expect("source-policy tests require the pinned Lean toolchain");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
    })
}

fn scan(source: Option<&str>) -> serde_json::Value {
    let temp = common::project_tempdir();
    std::fs::create_dir(temp.path().join("Tablet")).unwrap();
    if let Some(source) = source {
        std::fs::write(temp.path().join("Tablet/Test.lean"), source).unwrap();
    }
    // Importing this source would fail. A scan must neither load the project
    // dependency nor elaborate the owner's commands (including #eval).
    std::fs::write(temp.path().join("Dependency.lean"), "invalid Lean source\n").unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().join("scripts/lean_local_closure.lean");
    let output = Command::new(lean_prefix().join("bin/lean"))
        .args(["--run", script.to_str().unwrap(), "Test", "--scan-only"])
        .current_dir(temp.path())
        .env("LEAN_SYSROOT", lean_prefix())
        .env_remove("LEAN_PATH")
        .output().expect("run source-policy scanner");
    assert!(output.status.success(), "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!temp.path().join("executed").exists(), "scanner executed owner code");
    serde_json::from_slice(&output.stdout).expect("scanner JSON envelope")
}

fn assert_status(source: &str, status: &str) -> serde_json::Value {
    let result = scan(Some(source));
    assert_eq!(result["status"], status, "source={source}\nresult={result}");
    result
}

#[test]
fn imported_attributes_do_not_invent_commands() {
    for attributes in [
        "@[rust_fun \"core::num::dec2flt::parse\"]",
        "@[custom \"binder_predicate\"]",
        "@[custom \"macro\" (keep := [true])]",
    ] {
        assert_status(&format!("import Dependency\n/-- Generated declaration. -/\n{attributes} axiom Test : Nat\n"), "ok");
    }
}

#[test]
fn original_commands_have_source_locations() {
    for (source, keyword) in [
        ("macro \"bad\" : term => `(1)", "macro"),
        ("macro_rules | `(1) => `(2)", "macro_rules"),
        ("elab \"bad\" : term => pure default", "elab"),
        ("elab_rules : term | `(1) => pure default", "elab_rules"),
        ("syntax \"bad\" : term", "syntax"),
        ("syntax X := ident", "syntax (abbrev)"),
        ("declare_syntax_cat bad", "declare_syntax_cat"),
        ("binder_predicate x \" *** \" y:term => `($x > $y)", "binder_predicate"),
        ("initialize N : IO.Ref Nat ← IO.mkRef 0", "initialize / builtin_initialize"),
        ("builtin_initialize N : IO.Ref Nat ← IO.mkRef 0", "initialize / builtin_initialize"),
    ] {
        let result = assert_status(&format!("import Lean\n{source}\n"), "policy_rejection");
        assert!(result["errors"].to_string().contains(&format!("{keyword} at Tablet/Test.lean:2:1")), "{result}");
    }
}

#[test]
fn core_extension_commands_are_recognized() {
    let result = assert_status("import Lean\nrun_cmd pure ()\nrun_elab pure ()\nrun_meta pure ()\ndef Test : Nat := by_elab return Lean.mkNatLit 0\n", "policy_rejection");
    for (keyword, location) in [("run_cmd", "2:1"), ("run_elab", "3:1"), ("run_meta", "4:1"), ("by_elab", "5:19")] {
        assert!(result["errors"].to_string().contains(&format!("{keyword} at Tablet/Test.lean:{location}")), "{result}");
    }
}

#[test]
fn recovery_and_wrappers_do_not_hide_authored_commands() {
    for source in [
        "@[custom \"value\"] macro \"bad\" : term => `(1)",
        "unknown_command foo bar\nmacro \"bad\" : term => `(1)",
        "def Test := «unknown»⟪abc⟫\nrun_cmd pure ()",
        "set_option pp.universes true in macro \"bad\" : term => `(1)",
        "macro",
    ] {
        assert_status(&format!("import Dependency\n{source}\n"), "policy_rejection");
    }
}

#[test]
fn local_definitions_remain_allowed() {
    assert_status("import Lean\nlocal macro \"okay\" : term => `(1)\nset_option pp.universes true in local macro \"alsoOkay\" : term => `(1)\nlocal binder_predicate x \" *** \" y:term => `($x > $y)\nlocal syntax \"okaySyntax\" : term\nlocal elab \"okayElab\" : term => pure default\n", "ok");
}

#[test]
fn escaped_identifiers_comments_and_literals_are_not_commands() {
    assert_status("import Lean\n/- macro run_cmd binder_predicate -/\ndef «by_elab» : Nat := 0\ndef Test := \"by_elab run_meta partial\"\n", "ok");
}

#[test]
fn partial_modifier_requires_authored_evidence() {
    for prefix in ["", "@[custom \"value\"] "] {
        assert_status(&format!("import Dependency\n{prefix}partial def Test : Nat := Test\n"), "policy_rejection");
    }
}

#[test]
fn source_scan_does_not_execute_project_code() {
    assert_status("import Dependency\n#eval IO.FS.writeFile \"executed\" \"bad\"\ndef Test := 1\n", "ok");
}

#[test]
fn missing_source_is_an_internal_error() {
    let result = scan(None);
    assert_eq!(result["status"], "internal_error", "{result}");
}
