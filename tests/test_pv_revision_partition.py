"""Production ratification stays separate from reproducible audit evidence."""
from copy import deepcopy
import zipfile

import pytest

from trellis.json_io import save_json
from trellis.pv_revision.driver import Driver
from trellis.pv_revision.review import current_package, file_view, review_index
from trellis.pv_revision.store import (
    LEGACY_REVIEW_SCHEMA, REVIEW_SCHEMA, RevisionStoreError,
    build_review_package_from_state, export_review_package,
    ratified_source, ratified_source_manifest, source_manifest, write_review_package,
)
from test_pv_revision_store import driver_state


def gate(tmp_path, *, legacy=False):
    root, state = driver_state(tmp_path)
    source = root / state['source']
    (source / 'harness.py').write_text('print("regression evidence")\n')
    (source / 'measurements.txt').write_text('Measured observations\n')
    authorization = state['tasks']['T1']
    authorization.update(paths=['code.rs', 'harness.py', 'measurements.txt'],
                         evidence_paths=['measurements.txt'], checks=[{
                             'id': 'regression', 'argv': ['python3', '{harness}/harness.py'],
                             'harness_paths': ['harness.py']}])
    state['source_manifest'] = state['attempts'][0]['manifest'] = source_manifest(source)
    state.update(version=1, status='awaiting_human', phase='human_gate', sequence=7,
                 human_decisions=[{'action': 'amend', 'reason': 'Preserve performance guidance'}])
    package = build_review_package_from_state(root, state, schema=LEGACY_REVIEW_SCHEMA if legacy else REVIEW_SCHEMA)
    state['human_package'] = package
    write_review_package(root, package)
    save_json(root / 'state.json', state)
    return Driver(root)


def test_production_projection_export_and_evidence_navigation(tmp_path):
    driver = gate(tmp_path)
    package = driver.state['human_package']
    assert [row['path'] for row in package['aggregate_diff']['changes']] == ['code.rs']
    assert {row['path'] for row in package['evidence_diff']['changes']} == {'harness.py', 'measurements.txt'}
    source = ratified_source(driver.root, package)
    assert source_manifest(source) == ratified_source_manifest(package)
    assert not (source / 'harness.py').exists()
    assert not (source / 'measurements.txt').exists()
    assert (source / 'code.rs').read_text() == 'new\n'
    index = review_index(package)
    assert {row['path']: row['group'] for row in index['files']} == {
        'code.rs': 'production', 'harness.py': 'evidence', 'measurements.txt': 'evidence'}
    assert file_view(driver.root, package['digest'], 'harness.py', mode='after')['rows'][0]['text'].startswith('print')
    with zipfile.ZipFile(export_review_package(driver.root, driver.state)) as archive:
        assert 'harness.py' not in archive.read('candidate.patch').decode()
        assert 'harness.py' in archive.read('evidence.patch').decode()
        assert archive.read('evidence/harness/harness.py') == archive.read('sources/candidate/harness.py')
        assert 'sources/production/harness.py' not in archive.namelist()
        assert archive.read('sources/production/code.rs') == b'new\n'
    driver.decide('accept', digest=package['digest'], reason='Approve program and requirements')
    decision = driver.state['human_decisions'][-1]
    assert decision['ratified_candidate_digest'] == package['candidate_digest']
    assert decision['attached_evidence_digest'] == package['evidence_digest']
    assert decision['evidence_ratified'] is False


def test_evidence_changes_do_not_change_ratified_program_but_stale_the_review(tmp_path):
    driver = gate(tmp_path)
    before = driver.state['human_package']
    (driver.root / driver.state['source'] / 'harness.py').write_text('print("new test")\n')
    driver.state['source_manifest'] = driver.state['attempts'][0]['manifest'] = source_manifest(driver.root / driver.state['source'])
    after = driver._package(driver.state)
    assert before['candidate_digest'] == after['candidate_digest']
    assert before['evidence_digest'] != after['evidence_digest']
    assert before['digest'] != after['digest']
    with pytest.raises(ValueError, match='stale'):
        driver.decide('accept', digest=before['digest'], reason='Old review')


@pytest.mark.parametrize('path,match', [('code.rs', 'original production'), ('absent.py', 'missing'), ('../escape', 'invalid relative')])
def test_evidence_declarations_cannot_hide_original_code_or_missing_files(tmp_path, path, match):
    driver = gate(tmp_path)
    driver.state['tasks']['T1']['evidence_paths'] = [path]
    with pytest.raises(ValueError, match=match):
        driver._package(driver.state)


def test_guarded_repackaging_preserves_checked_bytes_verdicts_and_pending_decision(tmp_path):
    driver = gate(tmp_path, legacy=True)
    before = deepcopy(driver.state)
    old = before['human_package']
    assert driver._package(before) == old
    assert 'harness.py' in old['aggregate_diff']['patch']
    original_state_bytes = driver.path.read_bytes()
    reason = 'Reviewed all production references; the new standalone scripts are used only by check commands.'
    preview = driver.repackage_review(digest=old['digest'], separation_reason=reason)
    assert not preview['applied']
    assert driver.path.read_bytes() == original_state_bytes
    assert not (driver.root / 'reviews' / preview['digest']).exists()
    assert preview['production_changes'] == ['code.rs']
    assert preview['evidence_files'] == ['harness.py', 'measurements.txt']
    with pytest.raises(ValueError, match='state changed'):
        driver.repackage_review(digest=old['digest'], separation_reason=reason, apply=True, state_digest='stale')
    result = driver.repackage_review(digest=old['digest'], separation_reason=reason, apply=True, state_digest=preview['state_digest'])
    assert result['digest'] == preview['digest'] and result['applied']
    assert {k: v for k, v in driver.state.items() if k not in {'human_package', 'review_repackaging'}} == {
        k: v for k, v in before.items() if k != 'human_package'}
    assert driver.state['status'] == 'awaiting_human'
    assert driver._package(driver.state) == driver.state['human_package']
    assert current_package(driver.root, result['digest'])['scope_passed']
    assert (driver.root / 'reviews' / old['digest'] / 'review.json').exists()
    with pytest.raises(RevisionStoreError, match='changed'):
        current_package(driver.root, old['digest'])
    with pytest.raises(ValueError, match='stale'):
        driver.decide('accept', digest=old['digest'], reason='Stale page')
    assert source_manifest(driver.root / driver.state['source']) == before['source_manifest']


@pytest.mark.parametrize('mutation,match', [('accepted', 'pending human'), ('scope', 'scope-passed'), ('source', 'source changed')])
def test_repackaging_rejects_nonpending_failed_or_mutated_input(tmp_path, mutation, match):
    driver = gate(tmp_path, legacy=True)
    if mutation == 'accepted':
        driver.state['status'] = 'accepted_for_modeling'
    elif mutation == 'scope':
        driver.state['aggregate_scope']['decision'] = 'fail'
        driver.state['human_package'] = driver._package(driver.state)
    else:
        (driver.root / driver.state['source'] / 'code.rs').write_text('changed')
    with pytest.raises(ValueError, match=match):
        driver.repackage_review(digest=driver.state['human_package']['digest'], separation_reason='Checked separation')


def test_legacy_ratification_and_source_scope_are_not_reinterpreted(tmp_path):
    driver = gate(tmp_path, legacy=True)
    package = driver.state['human_package']
    driver.decide('accept', digest=package['digest'], reason='Legacy approval')
    assert 'ratified_candidate_digest' not in driver.state['human_decisions'][-1]
    assert ratified_source_manifest(package) == driver.state['source_manifest']
    assert (ratified_source(driver.root, package) / 'harness.py').exists()
    assert driver._package(driver.state) == package


def test_repackage_cli_uses_controller_lock_and_requires_preview_state(tmp_path, monkeypatch, capsys):
    import fcntl
    import json
    from scripts import pv_revision
    driver = gate(tmp_path, legacy=True)
    monkeypatch.setattr(pv_revision.lifecycle, 'revision_root', lambda _: driver.root)
    monkeypatch.setattr('scripts.pv_native_create.runtime_job', lambda _: None)
    # Preview/upgrade must never initialize a fresh audit or call an agent.
    monkeypatch.setattr(pv_revision.lifecycle, 'initialize_runtime', lambda _: pytest.fail('unexpected audit initialization'))
    arguments = ['repackage-review', str(tmp_path), '--digest', driver.state['human_package']['digest'],
                 '--separation-reason', 'Standalone harness has no incoming production references']
    driver.state['tools'] = {}
    driver.save()
    assert pv_revision.main(arguments) == 0
    preview = json.loads(capsys.readouterr().out)
    with (driver.root / 'runner.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert pv_revision.main(arguments + ['--apply', '--state-digest', preview['state_digest']]) == 1
    assert Driver(driver.root).state['human_package']['schema'] == LEGACY_REVIEW_SCHEMA
    capsys.readouterr()
    assert pv_revision.main(arguments + ['--apply', '--state-digest', preview['state_digest']]) == 0
    assert json.loads(capsys.readouterr().out)['applied']
    assert Driver(driver.root).state['status'] == 'awaiting_human'
