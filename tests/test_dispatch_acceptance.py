import json
import shutil
from types import SimpleNamespace

import pytest

from trellis.dispatch import acceptance, candidate


PREFIX = b"import Tablet.Preamble\ntheorem C : True :=\r\n-- BODY\r\n"


def fixture(tmp_path, response=None):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    sources = {'Tablet/A.lean': b'own old', 'Tablet/C.lean': PREFIX + b'by sorry\n',
               'Tablet/C.tex': b'statement', 'Tablet/D.lean': b'definition', 'lakefile.toml': b'config'}
    for path, data in sources.items():
        (repo / path).write_bytes(data)
    base = candidate.source_manifest(repo)
    inputs = {path: candidate.digest(data) for path, data in sources.items()}
    worker = tmp_path / 'worker'
    shutil.copytree(repo, worker)
    (worker / 'Tablet/A.lean').write_bytes(b'own proposed')
    sealed = tmp_path / 'sealed'
    candidate.seal(worker, sealed, task_id=1, base_manifest=base, inputs=inputs, response=response or {'outcome': 'Valid'})
    return repo, sealed, inputs, {'Tablet/C.lean': candidate.digest(PREFIX)}


def current_inputs(repo, inputs):
    return {path: candidate.digest((repo / path).read_bytes()) for path in inputs}


def test_overlay_readonly_proof_body_preserves_current_dependency(tmp_path):
    repo, sealed, inputs, prefixes = fixture(tmp_path)
    (repo / 'Tablet/C.lean').write_bytes(PREFIX + b'by trivial\n')
    captured = current_inputs(repo, inputs)
    acceptance.overlay(sealed, repo, captured, prefixes)
    assert (repo / 'Tablet/A.lean').read_bytes() == b'own proposed'
    assert (repo / 'Tablet/C.lean').read_bytes() == PREFIX + b'by trivial\n'


@pytest.mark.parametrize('change', ['prefix', 'own', 'tex', 'definition', 'config', 'legacy', 'race', 'missing_marker', 'duplicate_marker'])
def test_overlay_rebase_rejects_protected_changes_without_partial_writes(tmp_path, change):
    repo, sealed, inputs, prefixes = fixture(tmp_path)
    (repo / 'Tablet/C.lean').write_bytes(PREFIX + b'by trivial\n')
    if change == 'prefix':
        (repo / 'Tablet/C.lean').write_bytes(PREFIX.replace(b'True', b'False') + b'by sorry\n')
    elif change in ('own', 'tex', 'definition', 'config'):
        path = {'own': 'Tablet/A.lean', 'tex': 'Tablet/C.tex', 'definition': 'Tablet/D.lean', 'config': 'lakefile.toml'}[change]
        (repo / path).write_bytes(b'changed')
    elif change == 'legacy':
        prefixes = {}
    elif change == 'missing_marker':
        (repo / 'Tablet/C.lean').write_bytes(b'theorem C : True := by trivial')
    elif change == 'duplicate_marker':
        (repo / 'Tablet/C.lean').write_bytes(PREFIX + b'-- BODY\nby trivial')
    captured = current_inputs(repo, inputs)
    if change == 'race':
        (repo / 'Tablet/C.lean').write_bytes(PREFIX + b'by constructor\n')
    before = candidate.source_manifest(repo)
    with pytest.raises(candidate.CandidateError, match='stale task input'):
        acceptance.overlay(sealed, repo, captured, prefixes)
    assert candidate.source_manifest(repo) == before


@pytest.mark.parametrize('outcome,edited', [('stuck', False), ('stuck', True), ('valid', True)])
def test_stale_dependency_skips_baseline_without_touching_candidate(tmp_path, monkeypatch, outcome, edited):
    import trellis.checking
    import trellis.config
    import trellis.supervisor_workspace
    repo, sealed, inputs, prefixes = fixture(tmp_path, response={'outcome': outcome})
    if not edited:
        sealed = tmp_path / 'unchanged-sealed'
        candidate.seal(repo, sealed, task_id=1, base_manifest=candidate.source_manifest(repo),
                       inputs=inputs, response={'outcome': outcome})
    evidence = candidate.read(sealed)
    # An upstream publication completed while this candidate waited in the
    # semantic queue. Even an unchanged Stuck report used that older input.
    (repo / 'Tablet/C.lean').write_bytes(PREFIX.replace(b'True', b'False') + b'by sorry\n')
    before = candidate.source_manifest(repo)
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    state = dict(in_flight_request=dict(id=4, cycle=1, phase='ProofFormalization',
        active_node='A', task_execution={'task_kind': 'lean_closure'}))
    (runtime / 'protocol_state.json').write_text(json.dumps(state))
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo))
    def forbidden(*args, **kwargs): pytest.fail('stale input reached expensive acceptance work')
    monkeypatch.setattr(trellis.supervisor_workspace, 'sync_supervisor_workspace', forbidden)
    monkeypatch.setattr(trellis.checking, 'build_trellis_worker_acceptance_context', forbidden)
    monkeypatch.setattr(trellis.checking, 'normalize_trellis_worker_result_data', forbidden)
    result = acceptance.check(dict(config_path=str(repo / 'trellis.config.json'),
        runtime_root=str(runtime), candidate=str(sealed), captured_inputs=current_inputs(repo, inputs),
        proof_body_rebase_inputs=prefixes))
    assert result == dict(candidate_sha256=evidence[1], stale_reason='stale task input: Tablet/C.lean')
    assert candidate.source_manifest(repo) == before and candidate.read(sealed) == evidence
    assert not (runtime / 'acceptance-context.json').exists()
    assert not (runtime / 'acceptance-result.json').exists()


def test_overlay_rechecks_conflicts_after_baseline_preparation(tmp_path, monkeypatch):
    import trellis.checking
    import trellis.config
    import trellis.supervisor_workspace
    repo, sealed, inputs, prefixes = fixture(tmp_path)
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps({'in_flight_request': {'id': 4, 'cycle': 1}}))
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo,
        workflow=SimpleNamespace(paper_tex_path=None), goal_file=None))
    monkeypatch.setattr(trellis.supervisor_workspace, 'sync_supervisor_workspace',
        lambda _: {'authoritative_repo_path': repo})
    monkeypatch.setattr(trellis.supervisor_workspace, 'propagate_tablet_back_to_worker', lambda *args, mirror_build: None if mirror_build is False else pytest.fail('host must not mirror builds'))
    def baseline(*args, **kwargs):
        (repo / 'Tablet/A.lean').write_bytes(b'changed during preparation')
        return {'ok': True, 'data': {}}
    monkeypatch.setattr(trellis.checking, 'build_trellis_worker_acceptance_context', baseline)
    monkeypatch.setattr(trellis.checking, 'normalize_trellis_worker_result_data',
        lambda *args, **kwargs: pytest.fail('write conflict reached normalization'))
    result = acceptance.check(dict(config_path=str(repo / 'trellis.config.json'), runtime_root=str(runtime),
        candidate=str(sealed), captured_inputs=inputs, proof_body_rebase_inputs=prefixes))
    assert result['stale_reason'] == 'candidate source conflicts with captured work: Tablet/A.lean'
    assert (repo / 'Tablet/A.lean').read_bytes() == b'changed during preparation'


@pytest.mark.parametrize('resource_failure', [False, True])
@pytest.mark.parametrize('retry_artifacts', [False, True])
def test_rebased_candidate_still_runs_fresh_authoritative_acceptance(tmp_path, monkeypatch, resource_failure, retry_artifacts):
    import trellis.checking
    import trellis.config
    import trellis.supervisor_workspace
    import trellis.dispatch.enrichment
    repo, sealed, inputs, prefixes = fixture(tmp_path, response={
        'outcome': 'Valid', 'acceptance_read_trace': {'forged_worker_trace': True}})
    current = PREFIX + b'by trivial\n'
    (repo / 'Tablet/C.lean').write_bytes(current)
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps({'in_flight_request': {'id': 4, 'cycle': 1}}))
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo,
        workflow=SimpleNamespace(paper_tex_path=None), goal_file=None))
    monkeypatch.setattr(trellis.supervisor_workspace, 'sync_supervisor_workspace',
        lambda _: {'authoritative_repo_path': repo})
    monkeypatch.setattr(trellis.supervisor_workspace, 'propagate_tablet_back_to_worker', lambda *args, mirror_build: None if mirror_build is False else pytest.fail('host must not mirror builds'))
    calls = []
    if retry_artifacts:
        from trellis.dispatch import replay_donation
        def install(_runtime):
            assert (repo / 'Tablet/A.lean').read_bytes() == b'own proposed'
            calls.append('install_artifacts')
            return dict(status='installed', artifact_files={}, covered_nodes=[])
        monkeypatch.setattr(replay_donation, 'install_via_checker', install)
        monkeypatch.setattr(replay_donation, '_marker', lambda _: (
            tmp_path / 'drained-host', {'donor_kind': 'drained_host_execution'}, None))
    def baseline(authority, request, **kwargs):
        assert (authority / 'Tablet/C.lean').read_bytes() == current
        assert (authority / 'Tablet/A.lean').read_bytes() == b'own old'
        calls.append('baseline')
        return {'ok': True, 'data': {'current': True, 'acceptance_read_trace': {'native_baseline': True}}}
    def normalize(raw, *, repo, acceptance_context):
        assert acceptance_context == {'current': True, 'acceptance_read_trace': {'native_baseline': True}}
        assert raw['acceptance_read_trace'] == {'forged_worker_trace': True}
        assert (repo / 'Tablet/C.lean').read_bytes() == current
        assert (repo / 'Tablet/A.lean').read_bytes() == b'own proposed'
        calls.append('normalize')
        if resource_failure:
            from trellis.checker.resource_failure import record
            record(runtime, 'host reserve reached')
        return {'response': {'status': 'Ok', 'outcome': 'Invalid', 'acceptance_check_passed': False},
            'acceptance_read_trace': {'native_check': True}}
    monkeypatch.setattr(trellis.checking, 'build_trellis_worker_acceptance_context', baseline)
    monkeypatch.setattr(trellis.checking, 'normalize_trellis_worker_result_data', normalize)
    monkeypatch.setattr(trellis.dispatch.enrichment, 'enrich', lambda runtime, response: response)
    payload = {'config_path': str(repo / 'trellis.config.json'), 'runtime_root': str(runtime),
        'candidate': str(sealed), 'captured_inputs': current_inputs(repo, inputs), 'proof_body_rebase_inputs': prefixes}
    if resource_failure:
        with pytest.raises(RuntimeError, match='submission retained for infrastructure retry'):
            acceptance.check(payload)
        assert not (runtime / 'acceptance-result.json').exists()
        assert candidate.read(sealed)[0]['response']['outcome'] == 'Valid'
        return
    result = acceptance.check(payload)
    assert calls == (['baseline', 'install_artifacts', 'normalize'] if retry_artifacts else ['baseline', 'normalize'])
    assert result['response']['outcome'] == 'Invalid'
    assert result['response']['acceptance_check_passed'] is False
    assert result['acceptance_read_trace'] == dict(version=1, producer='host_acceptance_native',
        baseline={'native_baseline': True}, check={'native_check': True})


def stuck_fixture(tmp_path, partial=None):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_bytes(PREFIX.replace(b'C', b'A') + b'by sorry\n')
    (repo / 'Tablet/A.tex').write_text('Original statement')
    (repo / 'Tablet/Unrelated.tex').write_text('Outside the assigned dependency closure')
    captured = candidate.source_manifest(repo)
    captured['Tablet/Missing.tex'] = 'absent'
    scoped = {path: value for path, value in captured.items() if path != 'Tablet/Unrelated.tex'}
    sealed = tmp_path / 'sealed'
    worker = repo
    if partial is not None:
        worker = tmp_path / 'worker'
        shutil.copytree(repo, worker)
        content = (worker / 'Tablet/A.lean').read_bytes()
        if partial == 'import':
            content = b'import Tablet.MissingLemma\n' + content
        else:
            content = content.replace(b'by sorry', b'by\n  exact unfinished_proof')
        (worker / 'Tablet/A.lean').write_bytes(content)
    candidate.seal(worker, sealed, task_id=1, base_manifest=candidate.source_manifest(repo),
        inputs=scoped, response={'outcome': 'stuck', 'summary': 'Missing lemma', 'comments': '',
                                'system_feedback': 'Retain this checker diagnostic'})
    state = dict(in_flight_request=dict(id=4, cycle=1, phase='ProofFormalization', active_node='A',
        task_execution={'task_kind': 'lean_closure'}), live={'host_fingerprints': 'preserved'})
    return repo, sealed, captured, state


@pytest.mark.parametrize('partial', [None, 'body', 'import'])
def test_unchanged_stuck_uses_native_contract_and_preserves_host_baseline(tmp_path, monkeypatch, partial):
    import trellis.runtime.kernel_cli
    repo, sealed, captured, state = stuck_fixture(tmp_path, partial)
    candidate.check_lean_closure(sealed, repo, 'A')
    before, evidence = candidate.source_manifest(repo), candidate.read(sealed)
    assert bool(evidence[0]['changes']) == (partial is not None)
    assert set(candidate.read(sealed)[0]['inputs']) < set(captured), 'production uses scoped candidate inputs'
    calls = []
    def native(payload):
        calls.append(payload)
        return {'status': 'dispatch_ok', 'output': {'response': {'kind': 'worker', 'status': 'Ok',
            'outcome': 'Stuck', 'snapshot': payload['baseline'], 'acceptance_check_passed': False}}}
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', native)
    result = acceptance.unchanged_closure_stuck(repo, state, candidate.read(sealed)[0], captured)
    assert len(calls) == 1
    assert calls[0]['action'] == 'check_unchanged_closure_stuck'
    assert calls[0]['raw_payload'] == evidence[0]['response']
    assert calls[0]['raw_payload']['system_feedback'] == 'Retain this checker diagnostic'
    assert candidate.source_manifest(repo) == before and candidate.read(sealed) == evidence
    assert result['response']['snapshot'] == state['live']
    assert result['acceptance_path'] == 'unchanged_closure_stuck'


@pytest.mark.parametrize('change', ['valid', 'invalid', 'ordinary', 'cleanup', 'source', 'import',
    'new_file', 'deleted_file', 'previously_absent', 'changed_capture', 'missing_capture',
    'missing_absence', 'changed_absence', 'unscoped_change', 'symlink', 'fifo'])
def test_stuck_fast_path_does_not_admit_changed_or_ineligible_input(tmp_path, monkeypatch, change):
    import os
    import trellis.runtime.kernel_cli
    repo, sealed, captured, state = stuck_fixture(tmp_path)
    record = candidate.read(sealed)[0]
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli',
        lambda _: pytest.fail('ineligible attempt reached the fast native gate'))
    if change in ('valid', 'invalid'): record['response']['outcome'] = change
    elif change == 'ordinary': state['in_flight_request']['task_execution']['task_kind'] = 'ordinary'
    elif change == 'cleanup': state['in_flight_request']['phase'] = 'Cleanup'
    elif change in ('source', 'import'): (repo / 'Tablet/A.lean').write_bytes(b'import Tablet.New\n' + PREFIX)
    elif change == 'new_file': (repo / 'Tablet/Extra.lean').write_text('new')
    elif change == 'deleted_file': (repo / 'Tablet/A.tex').unlink()
    elif change == 'previously_absent': (repo / 'Tablet/Missing.tex').write_text('now present')
    elif change == 'changed_capture': captured['Tablet/A.lean'] = 'changed'
    elif change == 'missing_capture': captured.pop('Tablet/A.lean')
    elif change == 'missing_absence': captured.pop('Tablet/Missing.tex')
    elif change == 'changed_absence': captured['Tablet/Missing.tex'] = candidate.digest(b'new')
    elif change == 'unscoped_change': (repo / 'Tablet/Unrelated.tex').write_text('unobserved change')
    elif change == 'symlink': (repo / 'Tablet/Alias.lean').symlink_to('A.lean')
    elif change == 'fifo': os.mkfifo(repo / 'Tablet/pipe')
    if change in ('symlink', 'fifo'):
        with pytest.raises(candidate.CandidateError): acceptance.unchanged_closure_stuck(repo, state, record, captured)
    else:
        assert acceptance.unchanged_closure_stuck(repo, state, record, captured) is None


@pytest.mark.parametrize('path', ['Tablet/A.lean', 'lakefile.toml', 'missing-support.json'])
def test_unchanged_stuck_detects_mutation_during_native_check(tmp_path, monkeypatch, path):
    import trellis.runtime.kernel_cli
    repo, sealed, captured, state = stuck_fixture(tmp_path)
    if path == 'lakefile.toml':
        (repo / path).write_text('original')
        captured[path] = candidate.digest(b'original')
    elif path == 'missing-support.json':
        captured[path] = 'absent'
    def native(_):
        (repo / path).write_text('changed during normalization')
        return {'status': 'dispatch_ok', 'output': {'response': {'outcome': 'Stuck'}}}
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', native)
    with pytest.raises(candidate.CandidateError, match='changed during'):
        acceptance.unchanged_closure_stuck(repo, state, candidate.read(sealed)[0], captured)


@pytest.mark.parametrize('response', [None, [], 'malformed'])
def test_unchanged_stuck_without_native_response_falls_back(tmp_path, monkeypatch, response):
    import trellis.runtime.kernel_cli
    repo, sealed, captured, state = stuck_fixture(tmp_path)
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', lambda _: {
        'status': 'dispatch_ok', 'output': {'response': response, 'errors': ['source contract rejected']}})
    assert acceptance.unchanged_closure_stuck(repo, state, candidate.read(sealed)[0], captured) is None


@pytest.mark.parametrize('partial', [None, 'body', 'import'])
def test_host_stuck_skips_compiling_baseline_and_persists_native_result(tmp_path, monkeypatch, capsys, partial):
    import trellis.config
    import trellis.checking
    import trellis.runtime.kernel_cli
    import trellis.supervisor_workspace
    repo, sealed, captured, state = stuck_fixture(tmp_path, partial)
    candidate.check_lean_closure(sealed, repo, 'A')
    before, evidence = candidate.source_manifest(repo), candidate.read(sealed)
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps(state))
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo))
    def forbidden(*args, **kwargs): pytest.fail('closure Stuck attempted source overlay or semantic acceptance')
    monkeypatch.setattr(acceptance, 'overlay', forbidden)
    monkeypatch.setattr(trellis.supervisor_workspace, 'sync_supervisor_workspace', forbidden)
    monkeypatch.setattr(trellis.checking, 'build_trellis_worker_acceptance_context', forbidden)
    monkeypatch.setattr(trellis.checking, 'normalize_trellis_worker_result_data', forbidden)
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', lambda _: {'status': 'dispatch_ok',
        'output': {'ok': True, 'response': {'kind': 'worker', 'status': 'Ok', 'outcome': 'Stuck',
            'acceptance_check_passed': False, 'snapshot': state['live']}}})
    result = acceptance.check(dict(config_path=str(repo/'trellis.config.json'), runtime_root=str(runtime),
        candidate=str(sealed), captured_inputs=captured))
    assert result == json.loads((runtime/'acceptance-result.json').read_text())
    assert result['candidate_sha256'] == candidate.read(sealed)[1]
    assert candidate.source_manifest(repo) == before and candidate.read(sealed) == evidence
    if partial is not None:
        assert 'partial edits retained as unaccepted evidence; no proof checking or source publication' in capsys.readouterr().err


@pytest.mark.parametrize('failure', ['resource', 'native'])
def test_partial_stuck_host_failure_retains_infrastructure_classification(tmp_path, monkeypatch, failure):
    import trellis.config
    import trellis.runtime.kernel_cli
    from trellis.checker.resource_failure import record as record_failure
    repo, sealed, captured, state = stuck_fixture(tmp_path, 'body')
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps(state))
    before, evidence = candidate.source_manifest(repo), candidate.read(sealed)
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo))
    def native(_):
        if failure == 'resource':
            pytest.fail('sticky host resource failure must precede native acceptance')
        raise RuntimeError('native host check unavailable')
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', native)
    if failure == 'resource':
        record_failure(runtime, 'host reserve reached')
    with pytest.raises(RuntimeError, match='submission retained for infrastructure retry' if failure == 'resource' else 'native host check unavailable'):
        acceptance.check(dict(config_path=str(repo/'trellis.config.json'), runtime_root=str(runtime),
            candidate=str(sealed), captured_inputs=captured))
    assert not (runtime / 'acceptance-result.json').exists()
    assert candidate.source_manifest(repo) == before and candidate.read(sealed) == evidence


def test_partial_malformed_transport_keeps_early_bridge_response(tmp_path, monkeypatch):
    import trellis.config
    repo, _, captured, state = stuck_fixture(tmp_path, 'body')
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps(state))
    bridge = dict(status='Malformed', outcome='Invalid', transport_failure=True,
                  preflight_error='provider unavailable', comments='retained transport diagnostic')
    sealed = tmp_path / 'malformed'
    candidate.seal(tmp_path / 'worker', sealed, task_id=1, base_manifest=candidate.source_manifest(repo),
        inputs=captured, response=None, bridge_response=bridge)
    before, evidence = candidate.source_manifest(repo), candidate.read(sealed)
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: SimpleNamespace(repo_path=repo))
    def forbidden(*args, **kwargs): pytest.fail('Malformed bridge response entered source acceptance')
    monkeypatch.setattr(acceptance, 'unchanged_closure_stuck', forbidden)
    monkeypatch.setattr(acceptance, 'overlay', forbidden)
    result = acceptance.check(dict(config_path=str(repo/'trellis.config.json'), runtime_root=str(runtime),
        candidate=str(sealed), captured_inputs=captured))
    assert result['response'] == dict(bridge, request_id=4, cycle=1)
    assert result['candidate_sha256'] == evidence[1]
    assert candidate.source_manifest(repo) == before and candidate.read(sealed) == evidence
