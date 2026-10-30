// Included in the existing CLI observation test module, using its real cache
// implementation and a disposable checker transport stub.
fn result_key_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let cache = tmp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    write_minimal_lake_repo(&repo);
    for node in ["A", "B"] {
        write(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!(
                "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\ntheorem {node} : True := trivial\n"
            ),
        );
        write(
            &repo.join(format!(".lake/build/lib/lean/Tablet/{node}.olean")),
            "old",
        );
    }
    write(
        &repo.join(".trellis/scripts/check.py"),
        r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
repo = Path(sys.argv[2])
with (repo / 'calls').open('a') as f: f.write('call\n')
mode = repo / 'mutation'
if mode.exists():
    kind = mode.read_text(); mode.unlink()
    if kind == 'source':
        p = repo / 'Tablet/A.lean'; p.write_text(p.read_text() + '-- new source\n')
    elif kind == 'config':
        (repo / 'lakefile.toml').write_text('new config')
    elif kind != 'late':
        (repo / ('.lake/build/lib/lean/Tablet/' + kind + '.olean')).write_text('rebuilt')
nodes = [sys.argv[i+1] for i, a in enumerate(sys.argv) if a == '--node']
payload = {n: {'ok': True, 'payload': (repo / ('.lake/build/lib/lean/Tablet/' + n + '.olean')).read_text(), 'error': ''} for n in nodes}
if 'kind' in locals() and kind == 'late':
    (repo / '.lake/build/lib/lean/Tablet/A.olean').write_text('rebuilt')
print(json.dumps(payload))
"#,
    );
    (tmp, repo, cache)
}
fn result_key_calls(repo: &Path) -> usize {
    fs::read_to_string(repo.join("calls"))
        .unwrap_or_default()
        .lines()
        .count()
}
#[test]
fn result_key_batch_rebuild_stays_uncached_until_stable_observation() {
    let (_tmp, repo, cache) = result_key_fixture();
    with_disk_cache_root(&cache, || {
        write(&repo.join("mutation"), "A");
        let nodes = [NodeId::from("A")].into();
        let first = observe_lean_semantic_payloads(&repo, &nodes).unwrap();
        assert_eq!(first["A"].payload, "rebuilt");
        assert_eq!(result_key_calls(&repo), 1);
        // Test disk population too, rather than only the process-local tier.
        clear_lean_semantic_payload_cache_for_tests(&repo);
        assert_eq!(
            observe_lean_semantic_payloads(&repo, &nodes).unwrap()["A"].payload,
            "rebuilt"
        );
        assert_eq!(result_key_calls(&repo), 2, "changed-key first result must not be cached");
        clear_lean_semantic_payload_cache_for_tests(&repo);
        observe_lean_semantic_payloads(&repo, &nodes).unwrap();
        assert_eq!(result_key_calls(&repo), 2, "stable second result is reusable");
    });
}
#[test]
fn result_key_batch_checker_rebuild_invalidates_cached_sibling() {
    let (_tmp, repo, cache) = result_key_fixture();
    with_disk_cache_root(&cache, || {
        let a = [NodeId::from("A")].into();
        assert_eq!(
            observe_lean_semantic_payloads(&repo, &a).unwrap()["A"].payload,
            "old"
        );
        write(&repo.join("mutation"), "A");
        let both = [NodeId::from("A"), NodeId::from("B")].into();
        let observed = observe_lean_semantic_payloads(&repo, &both).unwrap();
        assert_eq!(observed["A"].payload, "rebuilt");
        assert_eq!(observed["B"].payload, "old");
        assert_eq!(
            result_key_calls(&repo),
            3,
            "initial A, B miss, bounded fresh fallback"
        );
        observe_lean_semantic_payloads(&repo, &both).unwrap();
        assert_eq!(result_key_calls(&repo), 3);
    });
}
#[test]
fn result_key_batch_checker_source_or_config_drift_retries_once() {
    for mutation in ["source", "config"] {
        let (_tmp, repo, cache) = result_key_fixture();
        with_disk_cache_root(&cache, || {
            write(&repo.join("mutation"), mutation);
            let nodes = [NodeId::from("A")].into();
            observe_lean_semantic_payloads(&repo, &nodes).unwrap();
            assert_eq!(result_key_calls(&repo), 2, "{mutation}");
            observe_lean_semantic_payloads(&repo, &nodes).unwrap();
            assert_eq!(result_key_calls(&repo), 2);
        });
    }
}

#[test]
fn result_key_batch_artifact_rewrite_after_payload_does_not_bind_new_key() {
    let (_tmp, repo, cache) = result_key_fixture();
    with_disk_cache_root(&cache, || {
        write(&repo.join("mutation"), "late");
        let nodes = [NodeId::from("A")].into();
        assert_eq!(observe_lean_semantic_payloads(&repo, &nodes).unwrap()["A"].payload, "old");
        assert_eq!(result_key_calls(&repo), 1);
        assert_eq!(observe_lean_semantic_payloads(&repo, &nodes).unwrap()["A"].payload, "rebuilt");
        assert_eq!(result_key_calls(&repo), 2, "old payload must not bind the rewritten artifact");
        observe_lean_semantic_payloads(&repo, &nodes).unwrap();
        assert_eq!(result_key_calls(&repo), 2);
    });
}
