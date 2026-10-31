"""Exact human edits remain feedback across fresh audits and original-source reviews."""
from copy import deepcopy
from io import BytesIO
import json
import zipfile

import pytest

from scripts.pv_revision import read_decision
from trellis.pv_revision import code_edits
from trellis.pv_revision.review import file_view, review_index
from trellis.pv_revision.store import export_review_package
from test_pv_revision_driver import make_driver, plan, review, worker, verdict
from test_pv_revision_review import gate, frozen


def edit(package, text='requested\r\nwithout final newline', path='lib.rs'):
    entry = next((e for e in package['source_manifest']['files'] if e['relative_path'] == path), None)
    return dict(path=path, base_sha256=entry['sha256_of_raw_bytes'] if entry else None, text=text)


def test_repeated_amendments_keep_original_diff_and_exact_feedback(tmp_path):
    responses = []
    driver = make_driver(tmp_path, responses)
    references = []
    previous_sources = []
    for round_number, content in enumerate(('first proposal\n', 'second proposal\n', 'third proposal\n')):
        responses.extend([plan(), review('work'), worker(content), verdict(), review('complete'), verdict()])
        while responses:
            driver.step()
        assert driver.state['status'] == 'awaiting_human', driver.state.get('feedback')
        package = driver.state['human_package']
        assert package.get('human_code_edits', []) == references
        patch = package['aggregate_diff']['patch']
        assert '-old\n' in patch and '+' + content in patch
        assert all('-' + old_content not in patch for _, old_content in previous_sources)
        displayed = file_view(driver.root, package['digest'], 'lib.rs', mode='edit')
        assert displayed['original_text'] == 'old\n' and displayed['text'] == content
        for source, old_content in previous_sources:
            assert (driver.root / source / 'lib.rs').read_text() == old_content
        if round_number == 2:
            with zipfile.ZipFile(export_review_package(driver.root, driver.state)) as archive:
                for reference in references:
                    assert archive.read(reference['path']) == (driver.root / reference['path']).read_bytes()
            break
        previous_sources.append((package['source'], content))
        requested = edit(package)
        driver.decide('amend', digest=package['digest'], reason='Specific source change', code_edits=[requested])
        reference = driver.state['human_decisions'][-1]['code_edits']
        references.append(reference)
        assert code_edits.load(driver.root, reference)['edits'] == [requested]
        assert 'text' not in json.dumps(reference)
        assert driver.state['source'] == 'sources/original'
        assert driver.state['next'] == {'role': 'audit'}
        assert reference['path'] in driver.state['feedback']
        assert (driver.root / 'sources/original/lib.rs').read_text() == 'old\n'
    context = json.loads((driver.root / 'requests/00013-audit/context.json').read_text())
    assert references[1]['path'] in json.dumps(context)


@pytest.mark.parametrize('defect', ['stale', 'hash', 'path', 'duplicate', 'binary', 'no_op', 'size', 'lines', 'action', 'shape'])
def test_invalid_edits_do_not_change_gate_or_write_feedback(tmp_path, defect):
    driver = gate(tmp_path, notes=False)
    package = driver.state['human_package']
    submitted = edit(package)
    action, digest = 'amend', package['digest']
    edits = [submitted]
    if defect == 'stale': digest = 'f' * 64
    elif defect == 'hash': submitted['base_sha256'] = 'f' * 64
    elif defect == 'path': submitted['path'] = '../state.json'
    elif defect == 'duplicate': edits.append(submitted)
    elif defect == 'binary': submitted['text'] = 'no\0binary'
    elif defect == 'no_op': submitted['text'] = 'fixed\n'
    elif defect == 'size': submitted['text'] = 'x' * (code_edits.MAX_FILE_BYTES + 1)
    elif defect == 'lines': submitted['text'] = '\n' * 20001
    elif defect == 'action': action = 'accept'
    elif defect == 'shape': submitted['extra'] = True
    before = deepcopy(driver.state)
    disk_before = driver.path.read_bytes()
    with pytest.raises(ValueError):
        driver.decide(action, digest=digest, reason='requested', code_edits=edits)
    assert driver.state == before and driver.path.read_bytes() == disk_before
    assert not list((driver.root / 'human-edits').glob('*'))


def test_feedback_tampering_blocks_agent_use(tmp_path):
    driver = gate(tmp_path, notes=False)
    package = driver.state['human_package']
    driver.decide('amend', digest=package['digest'], reason='requested', code_edits=[edit(package)])
    path = driver.root / driver.state['human_decisions'][-1]['code_edits']['path']
    path.chmod(0o644); path.write_text('{}')
    with pytest.raises(ValueError, match='evidence changed'):
        driver._current()


def test_file_edits_restore_delete_and_unchanged_inventory(tmp_path):
    root, state = frozen(tmp_path)
    package = state['human_package']
    index = review_index(package)
    unchanged = next(e for e in index['source_files'] if e['operation'] == 'unchanged')
    assert unchanged['path'] not in [e['path'] for e in index['files']]
    # Binary files remain noneditable, even when unchanged.
    assert file_view(root, package['digest'], 'keep.bin', mode='edit')['binary']
    requested = [edit(package, None, 'added.rs'), edit(package, 'restored\r\n', 'deleted.rs')]
    reference = code_edits.save(root, package, requested)
    assert code_edits.load(root, reference)['edits'] == requested
    assert file_view(root, package['digest'], 'deleted.rs', mode='edit')['text'] == ''
    assert (root / package['source'] / 'added.rs').read_text() == 'added\n'
    assert not (root / package['source'] / 'deleted.rs').exists()


def test_encoded_json_and_stdin_limits(tmp_path, monkeypatch):
    driver = gate(tmp_path, notes=False)
    package = driver.state['human_package']
    monkeypatch.setattr(code_edits, 'MAX_REQUEST_BYTES', 1024)
    with pytest.raises(ValueError, match='Encoded'):
        code_edits.save(driver.root, package, [edit(package, '\x01' * 500)])
    request = dict(action='amend', digest=package['digest'], reason='requested', code_edits=[edit(package)])
    assert read_decision(BytesIO(json.dumps(request).encode())) == request
    for bad in ({}, [], dict(request, unexpected=True), dict(request, reason=3)):
        with pytest.raises(ValueError):
            read_decision(BytesIO(json.dumps(bad).encode()))
    with pytest.raises(ValueError, match='Encoded'):
        read_decision(BytesIO(b'x' * 1025))


def test_stdin_controller_decision_uses_existing_gate_and_locks(tmp_path, monkeypatch, capsys):
    from types import SimpleNamespace
    from scripts import pv_revision
    driver = gate(tmp_path, notes=False)
    package = driver.state['human_package']
    request = dict(action='amend', digest=package['digest'], reason='requested', code_edits=[edit(package)])
    monkeypatch.setattr(pv_revision.sys, 'stdin', SimpleNamespace(buffer=BytesIO(json.dumps(request).encode())))
    monkeypatch.setattr(pv_revision.lifecycle, 'revision_root', lambda _: driver.root)
    monkeypatch.setattr(pv_revision.lifecycle, 'status', lambda _: {'state': 'active'})
    monkeypatch.setattr('scripts.pv_native_create.runtime_job', lambda _: None)
    monkeypatch.setattr(pv_revision, 'ProductionChecker', lambda *_: None)
    assert pv_revision.main(['decide-json', str(tmp_path / 'runtime')]) == 0
    state = json.loads(driver.path.read_text())
    assert state['status'] == 'active' and state['next'] == {'role': 'audit'}
    assert code_edits.load(driver.root, state['human_decisions'][-1]['code_edits'])['edits'] == request['code_edits']


def test_pending_amendment_contents_survive_incomplete_campaign_export(tmp_path, monkeypatch):
    from test_pv_campaign_archive import generation, contents
    from trellis.pv_revision.archive import export_campaign
    from trellis.pv_revision.driver import initialize, Driver
    item = generation(tmp_path, complete=False)
    root = item['runtime'] / 'pv-revision'
    initialize(root, item['repo'] / 'upstream', item['goals'], ['G1'])
    audit = plan('retire'); audit['dispositions'][0]['goal_id'] = 'G1'
    responses = [audit, review('complete'), verdict()]
    def agents(root, state, task):
        response = responses.pop(0)
        return response(root, state, task) if callable(response) else response
    driver = Driver(root, agents=agents)
    while responses:
        driver.step()
    package = driver.state['human_package']
    requested = edit(package, 'pub fn value() -> u8 { 1 }\n')
    driver.decide('amend', digest=package['digest'], reason='Change the code instead', code_edits=[requested])
    _, files = contents(export_campaign(item['runtime'], allow_incomplete=True))
    reference = driver.state['human_decisions'][-1]['code_edits']
    stored = json.loads(files['generations/0000/revision/' + reference['path']])
    assert stored['edits'] == [requested]
    real_references = code_edits.references
    def replaced_after_read(root, state):
        result = real_references(root, state)
        target = root / result[0]['path']
        target.chmod(0o644); target.write_text('{}')
        return result
    monkeypatch.setattr(code_edits, 'references', replaced_after_read)
    incomplete, files = contents(export_campaign(item['runtime'], allow_incomplete=True))
    assert any('evidence hash mismatch' in gap['detail'] for gap in incomplete['gaps'])
    assert 'generations/0000/revision/' + reference['path'] not in files
