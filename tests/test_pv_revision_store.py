"""Production review identity, source deletion lineage and scoped evidence."""
from copy import deepcopy
import hashlib
from pathlib import Path

import pytest

from trellis.json_io import save_json
from trellis.pv_revision.store import (
    INVALIDATION_LANES, RevisionStoreError, build_review_package_from_state,
    changed_paths, copy_source_tree, effective_invalidation, source_manifest,
    tree_diff, validate_invalidation, write_review_package,
)


def driver_state(tmp_path):
    root = tmp_path / 'driver'
    original = root / 'sources/original'
    original.mkdir(parents=True)
    (original / 'code.rs').write_text('old\n')
    (original / 'keep.bin').write_bytes(b'\x00\xfforiginal')
    copy_source_tree(original, root / 'sources/attempt-1')
    (root / 'sources/attempt-1/code.rs').write_text('new\n')
    old, new = source_manifest(original), source_manifest(root / 'sources/attempt-1')
    goals = [{'id': 'G1', 'text': 'Required behavior'}]
    collection = {'context': 'Assumption A', 'goals': goals}
    auth = {'id': 'T1', 'paths': ['code.rs']}
    attempt = {'id': 'attempt-1', 'task_id': 'T1', 'base_source': str(original), 'source': 'sources/attempt-1',
               'base': old, 'manifest': new, 'authorization': auth, 'accepted': True,
               'scope': {'decision': 'pass'}, 'checks': {'ok': True}, 'response': {'alternatives': []}}
    state = dict(round=0, original_collection=collection, proposed_collection=deepcopy(collection),
                 original_goals=goals, proposed_goals=deepcopy(goals), negated_goals=['G1'],
                 original_manifest=old, source_manifest=new, source='sources/attempt-1',
                 plan={'alternatives': [], 'invalidate': ['audit-only']}, tasks={'T1': auth},
                 attempts=[attempt], aggregate_scope={'decision': 'pass'},
                 handoff={'before_fingerprints': {'P1': 'proof'}, 'dependencies': {'P2': ['G1']}})
    return root, state


def test_deletion_binary_addition_and_original_lineage(tmp_path):
    root, state = driver_state(tmp_path)
    source = root / state['source']
    (source / 'code.rs').unlink()
    (source / 'keep.bin').write_bytes(b'\xff\x00changed')
    (source / 'new.rs').write_text('fn value() -> u8 { 2 }\n')
    state['source_manifest'] = state['attempts'][0]['manifest'] = source_manifest(source)
    state['attempts'][0]['authorization']['paths'] = ['code.rs', 'keep.bin', 'new.rs']
    package = build_review_package_from_state(root, state)
    changes = {item['path']: item for item in package['aggregate_diff']['changes']}
    assert changes['code.rs']['operation'] == 'delete'
    assert changes['keep.bin']['binary']
    assert changes['new.rs']['operation'] == 'add'
    assert (root / 'sources/original/code.rs').read_text() == 'old\n'
    assert (root / 'sources/original/keep.bin').read_bytes() == b'\x00\xfforiginal'
    assert package['proposed_collection']['context'] == 'Assumption A'
    assert package['tests_are_proofs'] is False
    assert package['retirement_is_proof'] is False


@pytest.mark.parametrize('mutation', ['source', 'goals', 'authorization', 'receipt'])
def test_review_rejects_or_changes_identity_for_every_stale_component(tmp_path, mutation):
    root, state = driver_state(tmp_path)
    package = build_review_package_from_state(root, state)
    if mutation == 'source':
        (root / state['source'] / 'code.rs').write_text('tampered')
        with pytest.raises(RevisionStoreError, match='source evidence changed'):
            build_review_package_from_state(root, state)
        return
    if mutation == 'goals':
        state['proposed_collection']['context'] = 'Unapproved shared context'
    elif mutation == 'authorization':
        state['tasks']['T1']['paths'] = ['keep.bin']
    else:
        state['attempts'][0]['checks'] = {'ok': False}
    assert build_review_package_from_state(root, state)['digest'] != package['digest']


def test_scope_failure_keeps_both_copies_and_is_visible(tmp_path):
    root, state = driver_state(tmp_path)
    state['attempts'][0]['authorization']['paths'] = ['keep.bin']
    package = build_review_package_from_state(root, state)
    assert package['task_diffs'][0]['mechanical_scope_violations'] == ['code.rs']
    assert not package['scope_passed']
    assert (root / 'sources/original/code.rs').read_text() == 'old\n'
    assert (root / state['source'] / 'code.rs').read_text() == 'new\n'


def test_invalidation_unions_audit_fingerprints_dependencies_and_source_preservation():
    assert effective_invalidation(['audit'], {'changed': 'old', 'removed': 'x'}, {'changed': 'new'},
        {'dependent': ['changed'], 'transitive': ['dependent']}, ['preserved'], source_changed=True) == [
            'audit', 'changed', 'dependent', 'preserved', 'removed', 'transitive']


@pytest.mark.parametrize('bad_path', ['../outside', '/outside', 'a/../../outside', 'a\\outside'])
def test_unsafe_task_paths_rejected(tmp_path, bad_path):
    root, state = driver_state(tmp_path)
    state['attempts'][0]['authorization']['paths'] = [bad_path]
    with pytest.raises(ValueError):
        build_review_package_from_state(root, state)


def test_symlink_source_rejected_and_original_evidence_preserved(tmp_path):
    root, state = driver_state(tmp_path)
    (root / state['source'] / 'link').symlink_to(root / 'sources/original/code.rs')
    with pytest.raises(ValueError, match='symlink'):
        build_review_package_from_state(root, state)
    assert (root / 'sources/original/code.rs').exists()


def test_driver_review_adapter_writes_bound_readable_review(tmp_path):
    root, state = driver_state(tmp_path)
    package = build_review_package_from_state(root, state)
    assert package['effective_invalidation'] == ['G1', 'P1', 'P2', 'audit-only']
    assert package['task_diffs'][0]['scope_passed']
    assert package['task_diffs'][0]['diff']['base_source_digest'] == state['original_manifest']['source_tree_sha256']
    folder = write_review_package(root, package)
    assert (folder / 'README.md').exists()
    assert '-old' in (folder / 'candidate.patch').read_text()


def test_rejected_attempt_tamper_invalidates_package(tmp_path):
    root, state = driver_state(tmp_path)
    rejected = deepcopy(state['attempts'][0])
    rejected.update(id='rejected', source='sources/rejected', accepted=False, scope={'decision': 'fail'})
    copy_source_tree(root / 'sources/attempt-1', root / 'sources/rejected')
    state['attempts'].append(rejected)
    build_review_package_from_state(root, state)
    (root / 'sources/rejected/code.rs').unlink()
    with pytest.raises(RevisionStoreError, match='task snapshot'):
        build_review_package_from_state(root, state)


def test_structured_audit_lanes_union_mechanical_all_lanes(tmp_path):
    root, state = driver_state(tmp_path)
    state['handoff']['baseline'] = {'nodes': ['P1', 'P2', 'audit-only']}
    state['plan']['invalidation'] = [{'node': 'audit-only', 'lanes': ['soundness'], 'reason': 'Changed axiom meaning'}]
    package = build_review_package_from_state(root, state)
    assert package['audit_invalidation'][0]['lanes'] == ['soundness']
    for lane in INVALIDATION_LANES:
        assert {'G1', 'P1', 'P2'} <= set(package['per_lane_invalidation'][lane])
    assert 'audit-only' in package['per_lane_invalidation']['soundness']
    state['plan']['invalidation'][0]['node'] = 'typo-node'
    with pytest.raises(RevisionStoreError, match='unknown node'):
        build_review_package_from_state(root, state)


@pytest.mark.parametrize('lanes', [[], ['unknown']])
def test_invalid_audit_lane_rejected(lanes):
    with pytest.raises(RevisionStoreError, match='lane'):
        validate_invalidation([{'node': 'G1', 'lanes': lanes, 'reason': 'A reason'}])


@pytest.mark.parametrize('mutation', ['stdout', 'receipt', 'missing', 'outside'])
def test_production_check_artifact_tamper_rejects_review(tmp_path, mutation):
    root, state = driver_state(tmp_path)
    output = root / 'checks/attempt-1'
    output.mkdir(parents=True)
    stdout = output / 'stdout'
    stdout.write_text('check passed\n')
    checks = {'ok': True, 'artifacts': {str(stdout): hashlib.sha256(stdout.read_bytes()).hexdigest()}, 'checks': []}
    receipt = output / 'result.json'
    save_json(receipt, checks)
    checks.update(evidence=str(receipt), evidence_sha256=hashlib.sha256(receipt.read_bytes()).hexdigest())
    state['attempts'][0]['checks'] = checks
    build_review_package_from_state(root, state)
    if mutation == 'stdout':
        stdout.write_text('changed')
    elif mutation == 'receipt':
        receipt.write_text('{}')
    elif mutation == 'missing':
        stdout.unlink()
    else:
        checks['artifacts'] = {str(root / 'sources/original/code.rs'): hashlib.sha256((root / 'sources/original/code.rs').read_bytes()).hexdigest()}
    with pytest.raises(RevisionStoreError, match='check evidence'):
        build_review_package_from_state(root, state)



def test_shared_context_change_invalidates_goal_meaning_without_source_change(tmp_path):
    root, state = driver_state(tmp_path)
    state['source'] = 'sources/original'
    state['source_manifest'] = state['original_manifest']
    state['attempts'] = []
    state['proposed_collection']['context'] = 'New assumption'
    state['plan'] = {'invalidation': []}
    package = build_review_package_from_state(root, state)
    assert package['aggregate_diff']['changes'] == []
    assert package['effective_invalidation'] == ['G1', 'P2']
    assert all(nodes == ['G1', 'P2'] for nodes in package['per_lane_invalidation'].values())


def test_review_readme_includes_concrete_requirements_scope_and_alternatives(tmp_path):
    from trellis.pv_revision.store import review_readme
    root, state = driver_state(tmp_path)
    state['proposed_collection']['goals'][0]['text'] = 'Return the bounded result'
    state['proposed_goals'] = state['proposed_collection']['goals']
    state['plan'].update(summary='Repair the boundary case',
        dispositions=[{'goal_id': 'G1', 'remedy': 'modify', 'reason': 'State the supported domain',
                       'replacement_goals': state['proposed_goals'], 'task_ids': ['T1']}],
        alternatives=[{'remedy': 'retire', 'rationale': 'Would lose required behavior', 'evidence': ['negative proof']}])
    state['tasks']['T1']['authorization'] = 'Only repair bounded arithmetic'
    state['attempts'][0]['response']['alternatives'] = [
        {'approach': 'Guard before addition', 'status': 'tested', 'rationale': 'Preserves valid inputs', 'evidence': ['boundary regression']}]
    state['attempts'][0]['checks'] = {'ok': True, 'checks': [
        {'id': 'boundary', 'side': 'candidate', 'exit_code': 0, 'expected_exit': 0, 'matched': True}]}
    readme = review_readme(build_review_package_from_state(root, state))
    for value in ('Required behavior', 'Return the bounded result', 'Assumption A',
                  'Only repair bounded arithmetic', 'Would lose required behavior', 'Guard before addition',
                  'boundary (candidate): exit 0', 'Tests do not constitute proofs', 'Retirement is not proof'):
        assert value in readme


def test_zip_export_is_self_contained_exact_and_excludes_runtime_material(tmp_path):
    import json
    import zipfile
    from trellis.pv_revision.store import export_review_package
    root, state = driver_state(tmp_path)
    (root / state['source'] / 'keep.bin').unlink()
    state['source_manifest'] = state['attempts'][0]['manifest'] = source_manifest(root / state['source'])
    state['attempts'][0]['authorization']['paths'].append('keep.bin')
    predecessor = root / 'predecessor'
    (predecessor / 'Tablet').mkdir(parents=True)
    (predecessor / 'Tablet/Negative.lean').write_text('theorem original_negative : True := True.intro\n')
    save_json(predecessor / 'tablet-manifest.json', source_manifest(predecessor / 'Tablet'))
    save_json(predecessor / 'protocol_state.json', {'proof': 'original negative evidence'})
    state['handoff']['protocol_state_sha256'] = hashlib.sha256((predecessor / 'protocol_state.json').read_bytes()).hexdigest()
    save_json(root / 'trellis.config.json', {'secret': 'excluded runtime configuration'})
    (root / 'logs').mkdir()
    (root / 'logs/full.log').write_text('excluded full log')
    checks_dir = root / 'checks/attempt-1'
    checks_dir.mkdir(parents=True)
    stdout = checks_dir / 'stdout'
    stdout.write_text('boundary passed\n')
    checks = {'ok': True, 'artifacts': {str(stdout): hashlib.sha256(stdout.read_bytes()).hexdigest()}, 'checks': []}
    receipt = checks_dir / 'result.json'
    save_json(receipt, checks)
    checks.update(evidence=str(receipt), evidence_sha256=hashlib.sha256(receipt.read_bytes()).hexdigest())
    state['attempts'][0]['checks'] = checks
    (root / 'sources/original/code.rs').chmod(0o755)
    state['human_package'] = build_review_package_from_state(root, state)
    path = export_review_package(root, state)
    assert path == export_review_package(root, state)
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        assert 'sources/original/keep.bin' in names
        assert 'sources/candidate/keep.bin' not in names
        assert archive.read('sources/original/keep.bin') == b'\x00\xfforiginal'
        assert archive.read('sources/candidate/code.rs') == b'new\n'
        assert archive.read('checks/attempt-1/stdout') == b'boundary passed\n'
        assert archive.getinfo('sources/original/code.rs').external_attr >> 16 & 0o111 == 0o111
        assert 'predecessor/Tablet/Negative.lean' in names
        assert 'predecessor/protocol_state.json' in names
        assert 'patches/0000.patch' in names
        assert 'trellis.config.json' not in names and 'logs/full.log' not in names
        manifest = json.loads(archive.read('archive-manifest.json'))
        for row in manifest['files']:
            raw = archive.read(row['path'])
            assert len(raw) == row['byte_length']
            assert hashlib.sha256(raw).hexdigest() == row['sha256']
    receipt = json.loads(path.with_suffix('.json').read_text())
    assert receipt['archive_sha256'] == hashlib.sha256(path.read_bytes()).hexdigest()
    (root / state['source'] / 'code.rs').write_text('unapproved source')
    with pytest.raises(RevisionStoreError, match='source evidence changed'):
        export_review_package(root, state)


def test_zip_rejects_mutated_predecessor_proof_snapshot(tmp_path):
    from trellis.pv_revision.store import export_review_package
    root, state = driver_state(tmp_path)
    proof = root / 'predecessor/Tablet'
    proof.mkdir(parents=True)
    (proof / 'Original.lean').write_text('original proof')
    save_json(root / 'predecessor/tablet-manifest.json', source_manifest(proof))
    (proof / 'Original.lean').write_text('changed proof')
    with pytest.raises(RevisionStoreError, match='snapshot changed'):
        export_review_package(root, state)
    assert not (root / 'exports').exists()


def test_operator_recovery_changes_review_identity(tmp_path):
    root, state = driver_state(tmp_path)
    original = build_review_package_from_state(root, state)
    state['operator_actions'] = [{'action': 'recover', 'reason': 'Corrected checker environment'}]
    updated = build_review_package_from_state(root, state)
    assert original['digest'] != updated['digest']
    assert updated['operator_actions'] == state['operator_actions']


def test_preserved_protocol_tamper_invalidates_existing_review(tmp_path):
    root, state = driver_state(tmp_path)
    protocol = root / 'predecessor/protocol_state.json'
    save_json(protocol, {'negative': 'proof evidence'})
    state['handoff']['protocol_state_sha256'] = hashlib.sha256(protocol.read_bytes()).hexdigest()
    build_review_package_from_state(root, state)
    save_json(protocol, {'negative': 'changed evidence'})
    with pytest.raises(RevisionStoreError, match='protocol evidence changed'):
        build_review_package_from_state(root, state)


def test_final_candidate_check_receipts_are_bound_validated_and_exported(tmp_path):
    import zipfile
    from trellis.pv_revision.store import export_review_package, review_readme
    root, state = driver_state(tmp_path)
    original = build_review_package_from_state(root, state)
    stdout = root / 'checks/final-candidate/stdout'
    stdout.parent.mkdir(parents=True)
    stdout.write_text('all final checks passed')
    check = {'ok': True, 'source': state['source_manifest'], 'artifacts': {
        str(stdout): hashlib.sha256(stdout.read_bytes()).hexdigest()}, 'checks': []}
    receipt = stdout.parent / 'result.json'
    save_json(receipt, check)
    check.update(evidence=str(receipt), evidence_sha256=hashlib.sha256(receipt.read_bytes()).hexdigest())
    state['aggregate_checks'] = {'T1': check}
    package = build_review_package_from_state(root, state)
    assert package['candidate_digest'] == original['candidate_digest']
    assert package['evidence_digest'] != original['evidence_digest']
    assert package['digest'] != original['digest']
    assert 'Final candidate checks passed: True' in review_readme(package)
    with zipfile.ZipFile(export_review_package(root, state)) as archive:
        assert archive.read('checks/final-candidate/stdout') == b'all final checks passed'
        assert 'checks/final-candidate/result.json' in archive.namelist()
    stdout.write_text('changed final evidence')
    with pytest.raises(RevisionStoreError, match='artifact changed'):
        build_review_package_from_state(root, state)


def test_final_candidate_check_source_must_match_current_candidate(tmp_path):
    root, state = driver_state(tmp_path)
    state['aggregate_checks'] = {'T1': {'ok': True, 'source': state['original_manifest']}}
    with pytest.raises(RevisionStoreError, match='different final source'):
        build_review_package_from_state(root, state)


def test_failed_aggregate_history_remains_bound_and_exported_after_later_pass(tmp_path):
    import zipfile
    from trellis.pv_revision.store import export_review_package, review_readme
    root, state = driver_state(tmp_path)
    original = build_review_package_from_state(root, state)
    stdout = root / 'checks/failed-aggregate/stdout'
    stdout.parent.mkdir(parents=True)
    stdout.write_text('earlier candidate regression failed')
    checks = {'ok': False, 'source': state['original_manifest'], 'checks': [], 'artifacts': {
        str(stdout): hashlib.sha256(stdout.read_bytes()).hexdigest()}}
    receipt = stdout.parent / 'result.json'
    save_json(receipt, checks)
    checks.update(evidence=str(receipt), evidence_sha256=hashlib.sha256(receipt.read_bytes()).hexdigest())
    state['aggregate_check_history'] = [{'sequence': 3, 'round': 0,
        'source_manifest': state['original_manifest'], 'checks': {'T1': checks}}]
    state['aggregate_checks'] = {'T1': {'ok': True, 'source': state['source_manifest']}}
    package = build_review_package_from_state(root, state)
    assert package['candidate_digest'] == original['candidate_digest']
    assert package['evidence_digest'] != original['evidence_digest']
    assert package['digest'] != original['digest']
    assert 'T1: passed=False' in review_readme(package)
    with zipfile.ZipFile(export_review_package(root, state)) as archive:
        assert archive.read('checks/failed-aggregate/stdout') == b'earlier candidate regression failed'
        assert 'checks/failed-aggregate/result.json' in archive.namelist()
    state['aggregate_check_history'][0]['source_manifest'] = state['source_manifest']
    with pytest.raises(RevisionStoreError, match='historical aggregate check receipt'):
        build_review_package_from_state(root, state)
    state['aggregate_check_history'][0]['source_manifest'] = state['original_manifest']
    stdout.write_text('changed historical evidence')
    with pytest.raises(RevisionStoreError, match='artifact changed'):
        build_review_package_from_state(root, state)
