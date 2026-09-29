//! Toolchain conformance for the shipped Lean programs on the certificate path.
//!
//! These programs run at a user's site under whatever Lean the run pins, not
//! under the toolchain they were developed against. Numeric-literal defaulting
//! and API drift therefore fail at the user, silently and late.
//!
//! That has already happened twice: `lean_module_manifest.lean` returned
//! unannotated numerals from an `IO UInt32` entry point, which v4.33 accepts
//! and v4.30 rejects. It was fixed once and regressed in a later rewrite,
//! taking every module of a v4.30 corpus down with it. A recurring defect is
//! a missing test, so this is the test.
//!
//! Each program is typechecked against every supported toolchain that is
//! installed. A missing toolchain is reported and skipped rather than passing
//! silently — a green run on zero toolchains would be exactly the vacuous
//! signal this exists to prevent.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Toolchains a run may pin. Extend when support is added, not when a
/// failure appears.
const SUPPORTED_TOOLCHAINS: &[&str] = &[
    "leanprover--lean4---v4.30.0-rc1",
    "leanprover--lean4---v4.30.0-rc2",
    "leanprover--lean4---v4.32.0-rc1",
    "leanprover--lean4---v4.33.0",
];

/// Lean programs whose output the certificate path trusts.
const CERTIFICATE_PATH_SCRIPTS: &[&str] = &[
    "lean_module_manifest.lean",
    "lean_local_closure.lean",
    "lean_semantic_fingerprint.lean",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("kernel/ has a parent")
        .to_path_buf()
}

fn toolchain_root(name: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HOME").ok()?)
        .join(".elan/toolchains")
        .join(name);
    dir.join("bin/lean").exists().then_some(dir)
}

/// A compile error names the script with a `path:line:col:` prefix. Runtime
/// output (a usage message from a program invoked with no arguments) does
/// not, which is what lets one invocation serve as a typecheck.
fn compile_errors(output: &str, script: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.contains(script) && line.contains("error"))
        .map(str::to_string)
        .collect()
}

#[test]
fn certificate_path_scripts_typecheck_on_every_supported_toolchain() {
    let root = repo_root();
    let mut checked = 0usize;
    let mut missing: Vec<&str> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for toolchain in SUPPORTED_TOOLCHAINS {
        let Some(tc) = toolchain_root(toolchain) else {
            missing.push(toolchain);
            continue;
        };
        for script in CERTIFICATE_PATH_SCRIPTS {
            let path = root.join("scripts").join(script);
            if !path.exists() {
                failures.push(format!("{script}: missing from scripts/"));
                continue;
            }
            let out = Command::new(tc.join("bin/lean"))
                .arg("--run")
                .arg(&path)
                .env_remove("ELAN_TOOLCHAIN")
                .env("LEAN_PATH", tc.join("lib/lean"))
                .output()
                .unwrap_or_else(|e| panic!("spawn lean {toolchain}: {e}"));

            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let errors = compile_errors(&combined, script);
            if !errors.is_empty() {
                failures.push(format!("{toolchain} / {script}:\n    {}", errors.join("\n    ")));
            }
            checked += 1;
        }
    }

    if !missing.is_empty() {
        eprintln!("toolchain conformance: not installed, skipped: {missing:?}");
    }

    assert!(
        checked > 0,
        "no supported toolchain was installed, so nothing was typechecked; \
         install one under ~/.elan/toolchains rather than treating this as a pass"
    );
    assert!(
        failures.is_empty(),
        "shipped certificate-path Lean programs failed to typecheck ({checked} combinations run):\n{}",
        failures.join("\n")
    );
}
