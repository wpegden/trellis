import copy
from contextlib import contextmanager
import json
from pathlib import Path
import shutil
from types import SimpleNamespace

import pytest

from trellis.dispatch import acceptance_receipts as receipts, candidate, recovery, staged_acceptance as stages
from trellis.dispatch.driver import ProductionDriver, acceptance_identity
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


@pytest.fixture
def recovered(tmp_path, monkeypatch):
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'test-implementation')
    def inputs(repo):
        result = candidate.source_manifest(repo)
        for name in ('trellis.config.json', '.trellis/scripts/check.py'):
            result[name] = candidate.digest((Path(repo) / name).read_bytes())
        return result
    monkeypatch.setattr(receipts, 'input_hashes', inputs)
    runtime = tmp_path / 'runtime'
    repo = tmp_path / 'canonical'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (repo / 'trellis.config.json').write_text('{}')
    (repo / '.trellis/scripts').mkdir(parents=True)
    (repo / '.trellis/scripts/check.py').write_text('host checker')
    before = candidate.source_manifest(repo)
    source = dict(manifest=before, input_hashes=inputs(repo), graph={'edges': {'A': []}},
                  authority_inputs={'.trellis/scripts/check.py': inputs(repo)['.trellis/scripts/check.py']})
    source['input_hashes'].pop('.trellis/scripts/check.py')
    worker = tmp_path / 'worker'
    shutil.copytree(repo, worker)
    (worker / 'Tablet/A.lean').write_text('theorem A : True := by trivial\n')
    root = runtime / 'dispatch'
    sealed = root / '1/turns/1/candidate'
    identity = candidate.seal(worker, sealed, task_id=1, base_manifest=before,
        inputs=source['input_hashes'], response={'outcome': 'valid'}, submission={'turn': 1})
    attempt = dict(policy={'version': 1, 'max_submissions': 3, 'max_infrastructure_retries': 2},
        contract_sha256='c' * 64, allowance_ms=1000, consumed_ms=70, turn=1, submissions=1,
        infrastructure_retries=2, session=None, phase=dict(phase='checking', candidate_sha256=identity))
    task = dict(id=1, assignment={'scope': {'nodes': ['A']}, 'compiler': 'managed'}, binding={},
        status=dict(status='checking', candidate_sha256=identity), worker_attempt=attempt)
    state = dict(cycle=4, dispatch={'tasks': {'1': task}})
    private = dict(in_flight_request=dict(id=4, cycle=4, kind='Worker', phase='ProofFormalization',
                                        acceptance_logic_identity='test-implementation'))
    owned = root / '1/acceptance/4'
    directory = owned / 'finalization/3'
    def prepare(where, stage):
        work = where / 'repo'
        shutil.copytree(worker, work)
        authority = work / '.trellis/supervisor/repo'
        shutil.copytree(worker, authority)
        key = receipts.key(1, identity, task, acceptance_identity(private), source)
        capture = dict(key=key, source=source, acceptance_sha256=key['acceptance_sha256'],
                       config_sha256=candidate.digest(b'{}'))
        put(where / 'canonical.json', capture)
        put(where / 'base-manifest.json', before)
        put(where / 'runtime/protocol_state.json', private)
        put(where / 'runtime/runtime_metadata.json', {'repo_path': str(work)})
        put(where / 'prepared.json', dict(prepared_config_sha256=candidate.digest(b'{}'),
            identity=candidate.digest(candidate.canonical_json(dict(state=private, sources=before, config_sha256=capture['config_sha256'])))))
        process = dict(pid=99999999, start_ticks=1, boot_id='expired-test-process')
        for name in ('process', 'permit', 'cleanup'):
            put(where / f'execution/{name}.json', process)
        put(where / 'execution/recipe.json', dict(command=['python', *stages._COMMAND],
            payload=dict(stage=stage, config_path=str(work / 'trellis.config.json'), runtime_root=str(where / 'runtime'),
                candidate=str(sealed), captured_inputs=source['input_hashes'], source_graph=source['graph']),
            checker=dict(runtime=str(runtime), task_runtime=str(where / 'runtime'), compiler='managed', scope=str(where.relative_to(root)))))
        from trellis.checker.jobs import socket_path
        put(where / 'execution/compiler-channels.json', dict(runtime=str(runtime), environment=dict(
            TRELLIS_CHECKER_SOCKET=str(runtime / 'sockets/checker.sock'), TRELLIS_CHECKER_TOKEN='authority-test-token',
            TRELLIS_CHECK_JOBS_SOCKET=str(socket_path(runtime)), TRELLIS_CHECK_JOBS_TOKEN='compiler-test-token')))
        return key
    def save(where, output):
        put(where / 'execution/response.json', output)
        put(where / 'execution/result.json', dict(returncode=0, response_sha256=receipts.regular_digest(where / 'execution/response.json')))
    owner_key = prepare(owned, 'owned')
    owner = dict(stage='owned', status='facts', candidate_sha256=identity,
        facts=dict(owner_ok=True, schema_version=1, owner_nodes=['A'], acceptance_logic_identity='test-implementation'),
        checked_sources=receipts.checked_sources(owned / 'repo'))
    save(owned, owner)
    stages.complete(owned, owner)
    final_key = prepare(directory, 'finalize')
    marker = stages.prepare_donor(directory, owner)
    final = dict(stage='finalize', status='cold', candidate_sha256=identity,
        retry_class='finalization_environment_capture', reason='capture command failed',
        donor_completion_sha256=marker['donor_completion_sha256'], checked_sources=receipts.checked_sources(directory / 'repo'))
    save(directory, final)
    event = dict(kind='check_retry', task_id=1, candidate_sha256=identity, retry=3)
    put(directory / 'infrastructure-failure.json', dict(event=event, reason=final['reason']))
    locked = []
    @contextmanager
    def lock():
        locked.append(True)
        try: yield
        finally: locked.pop()
    calls = []
    monkeypatch.setattr('trellis.dispatch.process.AttemptProcess.stop_and_reap',
        lambda process, record: calls.append({'drained': str(process.root), 'process': record}))
    def native(payload):
        calls.append(payload)
        if payload['action'] == 'dispatch_grant_check_recovery':
            assert not locked, 'native maintenance acquires its own locks'
            assert payload['attempt_sha256'] == recovery._attempt_digest(task['worker_attempt'])
            task['worker_attempt']['infrastructure_recovery_grant'] = payload['grant']
            return {'tasks': {'1': copy.deepcopy(task)}}
        assert payload['action'] == 'dispatch_control'
        assert locked
        assert payload['event']['recovery_completion_sha256'] == task['worker_attempt']['infrastructure_recovery_grant']['completion_sha256']
        task['worker_attempt']['infrastructure_retries'] = 3
        return {}
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime, driver.repo, driver.root, driver.config_path = runtime, repo, root, repo / 'trellis.config.json'
    driver.state = lambda: state
    driver.require_no_halt = lambda: None
    driver._pause = lambda *a, **kw: calls.append({'pause': a})
    driver.native = SimpleNamespace(locked=lock, own_coordinator=lock, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: copy.deepcopy(source), _call=native)
    return SimpleNamespace(driver=driver, directory=directory, owned=owned, identity=identity, attempt=attempt,
        final=final, task=task, state=state, source=source, save=save, calls=calls, event=event)


def test_readonly_plan_and_apply_adopt_exact_reaped_completion_without_proof_work(recovered):
    f = recovered
    before = {p: p.read_bytes() for p in f.owned.rglob('*.json')}
    result = recovery.plan(f.driver, 1, 4, 3)
    assert result['status'] == 'planned' and result['completion_adoption_required']
    assert not (f.directory / 'staged-completed.json').exists()
    original_attempt = copy.deepcopy(f.attempt)
    assert f.calls == []
    applied = recovery.apply(f.driver, 1, 4, 3)
    assert applied['grant'] == result['grant']
    assert 'native' not in applied and applied['infrastructure_retries'] == 2
    assert all(path.read_bytes() == data for path, data in before.items())
    assert {k: v for k, v in f.attempt.items() if k != 'infrastructure_recovery_grant'} == original_attempt
    assert receipts.regular_digest(f.directory / 'staged-completed.json') == result['grant']['completion_sha256']
    assert recovery.plan(f.driver, 1, 4, 3)['status'] == 'already_granted'


@pytest.mark.parametrize('damage', ['live', 'cleanup', 'permit', 'channels', 'channel_runtime', 'response', 'receipt', 'owner', 'later_final', 'later_round',
    'invalid', 'semantic_cold', 'authored', 'current_source', 'config', 'task', 'candidate', 'allowance', 'prior_grant', 'failure_event'])
def test_recovery_refuses_unauthenticated_stale_or_non_infrastructure_work(recovered, monkeypatch, damage):
    f = recovered
    if damage == 'live':
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif damage in ('cleanup', 'permit'):
        (f.directory / f'execution/{damage}.json').unlink()
    elif damage == 'channels':
        (f.directory / 'execution/compiler-channels.json').unlink()
    elif damage == 'channel_runtime':
        put(f.directory / 'execution/compiler-channels.json', {'runtime': '/other', 'environment': {}})
    elif damage == 'response':
        put(f.directory / 'execution/response.json', {'status': 'cold'})
    elif damage == 'receipt':
        put(f.directory / 'staged-completed.json', {'forged': True})
    elif damage == 'owner':
        (f.owned / 'execution/response.json').write_text('{}')
    elif damage == 'later_final':
        (f.directory.with_name('4') / 'execution').mkdir(parents=True)
    elif damage == 'later_round':
        (f.owned.with_name('5') / 'execution').mkdir(parents=True)
    elif damage in ('invalid', 'semantic_cold'):
        f.final.pop('retry_class')
        f.final['reason'] = 'changed artifact environment'
        if damage == 'invalid':
            f.final['status'] = 'checked'; f.final['output'] = {'response': {'outcome': 'Invalid'}}
        f.save(f.directory, f.final)
    elif damage == 'authored':
        (f.directory / 'repo/Tablet/A.lean').write_text('new source')
    elif damage == 'current_source':
        (f.driver.repo / 'Tablet/A.lean').write_text('someone changed the owned source')
    elif damage == 'config':
        f.driver.config_path.write_text('{"different":true}')
    elif damage == 'task':
        f.task['assignment']['scope']['nodes'] = ['B']
    elif damage == 'candidate':
        f.task['status']['candidate_sha256'] = 'd' * 64
    elif damage == 'allowance':
        f.attempt['infrastructure_retries'] = 1
    elif damage == 'prior_grant':
        f.attempt['infrastructure_recovery_grant'] = {'different': True}
    else:
        put(f.directory / 'infrastructure-failure.json', {'event': dict(f.event, retry=2)})
    with pytest.raises((OSError, ValueError, KeyError, candidate.CandidateError)):
        recovery.plan(f.driver, 1, 4, 3)
    assert f.calls == []


def test_retry_crash_after_native_increment_preserves_failure_and_replays_exact_event(recovered, monkeypatch):
    f = recovered
    recovery.apply(f.driver, 1, 4, 3)
    failure = (f.directory / 'infrastructure-failure.json').read_bytes()
    from trellis.dispatch import driver as module
    ordinary_atomic = module.atomic
    def crash(path, data):
        if path.name == 'retry-authorized.json': raise OSError('simulated crash')
        return ordinary_atomic(path, data)
    monkeypatch.setattr(module, 'atomic', crash)
    with pytest.raises(OSError, match='simulated crash'):
        f.driver._retry_check(1, f.identity, f.directory, 'retry')
    assert f.attempt['infrastructure_retries'] == 3
    assert (f.directory / 'infrastructure-failure.json').read_bytes() == failure
    monkeypatch.setattr(module, 'atomic', ordinary_atomic)
    f.driver._retry_check(1, f.identity, f.directory, 'retry')
    authorized = json.loads((f.directory / 'retry-authorized.json').read_text())
    assert authorized['event']['recovery_completion_sha256'] == f.attempt['infrastructure_recovery_grant']['completion_sha256']
    assert (f.directory / 'infrastructure-failure.json').read_bytes() == failure
    assert sum(call.get('action') == 'dispatch_control' for call in f.calls) == 2


def test_maintenance_capability_preserves_existing_registry_and_runtime_stamp(tmp_path):
    from trellis.runtime import bridge
    runtime = tmp_path / 'runtime'
    path = bridge._burst_tokens_path(runtime)
    original = dict(runtime_root=str(runtime.resolve()), tokens=['existing'],
        entries=[dict(token='existing', kind='supervisor', ts=1)], extra='retained')
    put(path, original)
    with recovery.maintenance_capability(runtime):
        current = json.loads(path.read_text())
        assert current['tokens'][0] == 'existing' and len(current['tokens']) == 2
        assert current['entries'][0] == original['entries'][0]
        assert current['runtime_root'] == original['runtime_root']
    assert json.loads(path.read_text()) == original
    put(path, dict(original, runtime_root='/different'))
    with pytest.raises(ValueError, match='does not belong'):
        with recovery.maintenance_capability(runtime): pass


def test_missing_legacy_completion_classifies_only_after_full_authentication(recovered, monkeypatch):
    f = recovered
    from trellis.dispatch import staged_driver
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'new-reviewed-implementation')
    monkeypatch.setattr(stages, 'reviewed_owned_producers', lambda: {'test-implementation'})
    f.final.pop('retry_class')
    f.final['reason'] = 'staged normalization changed source; owned facts remain retained'
    f.final['replay_donation'] = {'status': 'installed'}
    f.save(f.directory, f.final)
    assert not staged_driver._retryable_finalization(f.directory, f.final)
    assert recovery.plan(f.driver, 1, 4, 3)['completion_adoption_required']
    assert not (f.directory / 'staged-completed.json').exists()
    # A self-consistent postfailure source inventory is not enough: it must
    # still equal the sealed authored delta and original host operands.
    (f.directory / 'repo/Tablet/A.lean').write_text('different authored proof')
    f.final['checked_sources'] = receipts.checked_sources(f.directory / 'repo')
    f.save(f.directory, f.final)
    with pytest.raises(candidate.CandidateError):
        recovery.plan(f.driver, 1, 4, 3)


def test_crash_after_completion_adoption_before_native_grant_is_restartable(recovered):
    f = recovered
    original_call = f.driver.native._call
    def fail(_payload):
        raise RuntimeError('simulated native transport failure')
    f.driver.native._call = fail
    with pytest.raises(RuntimeError, match='transport failure'):
        recovery.apply(f.driver, 1, 4, 3)
    assert (f.directory / 'staged-completed.json').exists()
    assert 'infrastructure_recovery_grant' not in f.attempt
    assert f.attempt['infrastructure_retries'] == 2
    f.driver.native._call = original_call
    assert recovery.apply(f.driver, 1, 4, 3)['status'] == 'granted'


def test_grant_does_not_authorize_another_finalization_or_changed_completion(recovered):
    f = recovered
    recovery.apply(f.driver, 1, 4, 3)
    grant = copy.deepcopy(f.attempt['infrastructure_recovery_grant'])
    f.attempt['infrastructure_recovery_grant']['finalization'] = 4
    with pytest.raises(RuntimeError, match='host recovery grant unavailable'):
        f.driver._retry_check(1, f.identity, f.directory, 'retry')
    assert not any(call.get('action') == 'dispatch_control' for call in f.calls)
    f.attempt['infrastructure_recovery_grant'] = grant
    f.final['reason'] = 'different failure'
    f.save(f.directory, f.final)
    with pytest.raises(RuntimeError, match='host recovery grant unavailable'):
        f.driver._retry_check(1, f.identity, f.directory, 'retry')
    assert f.attempt['infrastructure_retries'] == 2


def test_unrelated_publication_and_generated_docs_do_not_block_permission_to_recheck(recovered):
    f = recovered
    (f.driver.repo / 'Tablet/Sibling.lean').write_text('theorem Sibling : True := by trivial')
    (f.driver.repo / 'Tablet/INDEX.md').write_text('new generated index')
    f.driver.native.observe_sources = lambda *_: pytest.fail('grant must not redo global source observation')
    assert recovery.plan(f.driver, 1, 4, 3)['status'] == 'planned'
    assert recovery.apply(f.driver, 1, 4, 3)['status'] == 'granted'
    f.driver._retry_check(1, f.identity, f.directory, 'retry')
    assert f.attempt['infrastructure_retries'] == 3


def test_drain_failure_prevents_completion_adoption_and_native_grant(recovered, monkeypatch):
    f = recovered
    def fail(_process, _record):
        raise RuntimeError('compiler cleanup was not acknowledged')
    monkeypatch.setattr('trellis.dispatch.process.AttemptProcess.stop_and_reap', fail)
    assert recovery.plan(f.driver, 1, 4, 3)['drain_ack_required']
    with pytest.raises(RuntimeError, match='cleanup was not acknowledged'):
        recovery.apply(f.driver, 1, 4, 3)
    assert not (f.directory / 'staged-completed.json').exists()
    assert 'infrastructure_recovery_grant' not in f.attempt
    assert f.calls == []


def test_unadopted_cold_without_failure_diagnostic_grants_then_normal_retry_records_event(recovered):
    f = recovered
    failure = f.directory / 'infrastructure-failure.json'
    failure.unlink()
    assert not (f.directory / 'staged-completed.json').exists()
    assert not (f.directory / 'retry-authorized.json').exists()
    assert recovery.plan(f.driver, 1, 4, 3)['completion_adoption_required']
    recovery.apply(f.driver, 1, 4, 3)
    assert not failure.exists(), 'maintenance must not invent a failed retry event'
    f.driver._retry_check(1, f.identity, f.directory, 'original retained Cold')
    assert json.loads(failure.read_text())['event'] == f.event
    assert json.loads((f.directory / 'retry-authorized.json').read_text())['event'] == dict(
        f.event, recovery_completion_sha256=f.attempt['infrastructure_recovery_grant']['completion_sha256'])
