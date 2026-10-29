use std::{fs, path::Path, process::Command};
use serde_json::{json, Value};
use trellis_kernel::paper_implementation_io::load_subject;

#[test]
fn python_export_roundtrips_through_strict_paper_admission() {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for loops in [1, 3] {
        let root = tempfile::tempdir().unwrap();
        let output = Command::new("python3")
            .current_dir(project)
            .env("PYTHONPATH", project)
            .env("TRELLIS_TRELLIS_KERNEL_CMD", env!("CARGO_BIN_EXE_trellis_runtime_cli"))
            .arg(project.join("tests/fixtures/paper_principal_export.py"))
            .arg(root.path()).arg(loops.to_string()).output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let repo = root.path().join("export");
        let config = repo.join("trellis.config.json");
        let manifest_path = repo.join("paper-implementation/manifest.json");
        let original = fs::read(&manifest_path).unwrap();
        let mut manifest: Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(manifest["entry_points"][0]["declaration"], "fixture.run");
        assert!(load_subject(&repo, &config).unwrap().is_some());

        // A first-helper registration remains a hard admission failure.
        manifest["entry_points"][0]["declaration"] = json!("fixture.run_loop0.body");
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(load_subject(&repo, &config).unwrap_err().contains("does not match its extracted principal"));
        fs::write(&manifest_path, original).unwrap();

        // The same declared name cannot mask a changed model body or namespace.
        let node = repo.join("Tablet/run.lean");
        let original_node = fs::read_to_string(&node).unwrap();
        fs::write(&node, original_node.replace("def run (n : Nat) : Nat :=", "def run (n : Nat) : Nat := 1 +")).unwrap();
        assert!(load_subject(&repo, &config).unwrap_err().contains("byte-conformance"));
        fs::write(&node, original_node.replace("namespace fixture", "namespace replaced")).unwrap();
        assert!(load_subject(&repo, &config).is_err());
        fs::write(&node, original_node).unwrap();
        fs::write(repo.join("crate/src/lib.rs"), "pub fn run(n: u32) -> u32 { n + 1 }\n").unwrap();
        assert!(load_subject(&repo, &config).unwrap_err().contains("pinned input changed"));
    }
}
