"""Only ratification permits an omitted or blank human reason."""
from io import BytesIO
import json
from types import SimpleNamespace

import pytest

from scripts import pv_revision
from trellis.pv_revision.driver import Driver
from test_pv_revision_review import gate


@pytest.mark.parametrize('fields', [{}, {'reason': ''}, {'reason': '  '},
                                   {'reason': 'Reviewed exactly.'}, {'reason': 'x' * 20000}])
def test_accept_optional_reason_preserves_exact_record(tmp_path, fields):
    driver = gate(tmp_path, notes=False)
    before = driver.path.read_bytes()
    package = driver.state['human_package']
    with pytest.raises(ValueError, match='stale'):
        driver.decide('accept', digest='0' * 64, **fields)
    assert driver.path.read_bytes() == before
    driver.decide('accept', digest=package['digest'], **fields)
    restored = Driver(driver.root)
    assert restored.state['status'] == 'accepted_for_modeling'
    assert restored.state['human_package'] == package
    assert restored.state['human_decisions'][-1] == {
        'action': 'accept', 'digest': package['digest'], 'reason': fields.get('reason', ''),
        'amendment': None, 'ratified_candidate_digest': package['candidate_digest'],
        'attached_evidence_digest': package['evidence_digest'], 'evidence_ratified': False,
    }


@pytest.mark.parametrize('action', ['accept', 'deny', 'amend'])
def test_reason_validation_is_transactional(tmp_path, action):
    driver = gate(tmp_path, notes=False)
    before = driver.path.read_bytes()
    fields = [{'reason': value} for value in [None, False, 3, [], {}, 'x' * 20001]]
    if action != 'accept':
        fields += [{}, {'reason': ''}, {'reason': ' \n '}]
    for reason in fields:
        request = dict(action=action, digest=driver.state['human_package']['digest'],
                       amendment='Change the requirement', **reason)
        with pytest.raises(ValueError, match='[Rr]eason'):
            driver.decide(**request)
        assert driver.path.read_bytes() == before
        with pytest.raises(ValueError, match='[Rr]eason'):
            pv_revision.read_decision(BytesIO(json.dumps(request).encode()))


@pytest.mark.parametrize('action', ['accept', 'deny', 'amend'])
@pytest.mark.parametrize('reason', ['', '  ', 'Reviewed.', 'x' * 20000])
def test_decision_json_keeps_provided_reason(action, reason):
    request = dict(action=action, digest='a' * 64, reason=reason)
    if action != 'accept' and not reason.strip():
        with pytest.raises(ValueError, match='reason'):
            pv_revision.read_decision(BytesIO(json.dumps(request).encode()))
    else:
        assert pv_revision.read_decision(BytesIO(json.dumps(request).encode())) == request


@pytest.mark.parametrize('fields', [{}, {'reason': ''}])
@pytest.mark.parametrize('command', ['decide', 'decide-json'])
@pytest.mark.parametrize('action', ['accept', 'deny', 'amend'])
def test_cli_blank_reason(tmp_path, monkeypatch, capsys, command, action, fields):
    driver = gate(tmp_path, notes=False)
    before = driver.path.read_bytes()
    request = dict(action=action, digest=driver.state['human_package']['digest'], **fields)
    if action == 'amend':
        request['amendment'] = 'Change the requirement'
    monkeypatch.setattr(pv_revision.sys, 'stdin', SimpleNamespace(buffer=BytesIO(json.dumps(request).encode())))
    monkeypatch.setattr(pv_revision.lifecycle, 'revision_root', lambda _: driver.root)
    monkeypatch.setattr(pv_revision.lifecycle, 'status', lambda _: {})
    monkeypatch.setattr('scripts.pv_native_create.runtime_job', lambda _: None)
    monkeypatch.setattr(pv_revision, 'ProductionChecker', lambda *_: None)
    args = [command, str(tmp_path / 'runtime')]
    if command == 'decide':
        for key, value in request.items():
            args += ['--' + key, value]
    result = pv_revision.main(args)
    if action == 'accept':
        assert result == 0
        state = json.loads(driver.path.read_text())
        assert state['status'] == 'accepted_for_modeling'
        assert state['human_decisions'][-1]['reason'] == ''
    else:
        assert result == 1
        assert 'human reason must be nonempty text' in capsys.readouterr().out
        assert driver.path.read_bytes() == before


def test_blank_approval_cannot_bypass_failed_scope(tmp_path):
    from test_pv_revision_comments import gate as comment_gate
    from test_pv_revision_code_edits import edit
    from test_pv_revision_driver import verdict
    driver, responses = comment_gate(tmp_path)
    package = driver.state['human_package']
    driver.decide('amend', digest=package['digest'], reason='Wording', comment_only=True,
                  code_edits=[edit(package, '/// Revised wording.\nfn count() -> i32 { 1 }\n')])
    responses.append(verdict('fail'))
    driver.step()
    before = driver.path.read_bytes()
    with pytest.raises(ValueError, match='not passed scope'):
        driver.decide('accept', digest=driver.state['human_package']['digest'])
    assert driver.path.read_bytes() == before


def test_blank_approval_rechecks_source_bytes(tmp_path):
    driver = gate(tmp_path, notes=False)
    before = driver.path.read_bytes()
    (driver.root / driver.state['source'] / 'lib.rs').write_text('tampered')
    with pytest.raises(ValueError, match='source changed'):
        driver.decide('accept', digest=driver.state['human_package']['digest'])
    assert driver.path.read_bytes() == before
