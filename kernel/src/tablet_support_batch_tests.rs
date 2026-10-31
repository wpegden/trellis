use super::*;
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    fs::create_dir_all(p.join("Tablet")).unwrap();
    fs::create_dir_all(p.join(".trellis/scripts")).unwrap();
    fs::create_dir_all(p.join(".lake/build/lib/lean/Tablet")).unwrap();
    fs::write(p.join(".trellis/scripts/check.py"), "checker").unwrap();
    fs::write(p.join("lean-toolchain"), "toolchain").unwrap();
    fs::write(p.join("Tablet/Preamble.lean"), "def p := 1\n").unwrap();
    fs::write(
        p.join("Tablet/A.lean"),
        "import Tablet.Preamble\ndef a := 2\n",
    )
    .unwrap();
    for node in ["Preamble", "A"] {
        let artifact = p.join(format!(".lake/build/lib/lean/Tablet/{node}.olean"));
        fs::write(&artifact, b"artifact").unwrap();
        let record = serde_json::json!({"schema_version":3,"source_closure_sha256":crate::cache_key::lean_closure_cache_key(p,node).unwrap(),"kernel_replay":{
            "attestation_version":4,"checker":"leanchecker","mode":"ordinary","toolchain_sha256":crate::cache_key::hash_bytes(b"toolchain"),
            "artifact_bundle":[{"level":"exported","sha256":crate::cache_key::hash_bytes(b"artifact"),"size_bytes":8}],
            "declaration_manifest_version":2,"trusted_import_policy_version":1,"trusted_direct_imports":[],"declaration_manifest":[],"visibility_manifests":[{"level":"exported","declarations":[]}]}});
        fs::write(
            format!("{}.srcclosure", artifact.display()),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
    d
}
#[test]
fn batch_artifact_guard_matches_cold_and_preserves_failures() {
    for change in ["none", "artifact", "sidecar", "missing", "private"] {
        let d = fixture();
        let nodes = [NodeId::from("A")].into();
        let p = d.path();
        assert!(tablet_olean_closures_current(p, &nodes));
        let artifact = p.join(".lake/build/lib/lean/Tablet/A.olean");
        match change {
            "artifact" => fs::write(&artifact, b"changed!").unwrap(),
            "sidecar" => fs::write(format!("{}.srcclosure", artifact.display()), b"bad").unwrap(),
            "missing" => fs::remove_file(&artifact).unwrap(),
            "private" => fs::write(format!("{}.private", artifact.display()), b"private").unwrap(),
            _ => {}
        }
        assert_eq!(
            tablet_olean_closures_current(p, &nodes),
            tablet_olean_closures_current_uncached(p, &nodes),
            "{change}"
        );
        assert_eq!(tablet_olean_closures_current(p, &nodes), change == "none");
    }
}
#[test]
fn issuer_source_or_negative_config_mutation_discards_captured_keys() {
    for name in [
        "Tablet/A.lean",
        "lakefile.toml",
        ".trellis/scripts/check.py",
    ] {
        let d = fixture();
        let names = ["A".to_string()].into();
        let keys = crate::cache_key::SourceKeys::capture(d.path(), &names).unwrap();
        assert_eq!(tablet_olean_closures_from_keys(d.path(), &keys), Some(true));
        fs::write(d.path().join(name), b"changed").unwrap();
        assert_eq!(tablet_olean_closures_from_keys(d.path(), &keys), None);
        let nodes = [NodeId::from("A")].into();
        assert!(!tablet_olean_closures_current(d.path(), &nodes));
        assert_eq!(
            tablet_olean_closures_current(d.path(), &nodes),
            tablet_olean_closures_current_uncached(d.path(), &nodes)
        );
    }
}
