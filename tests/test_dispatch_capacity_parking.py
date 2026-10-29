import copy
import json
import time

import pytest

from trellis.checker import resource_failure
from trellis.dispatch import candidate, capacity_parking, parking, staged_acceptance as stages
from test_dispatch_recovery import recovered, put
from test_dispatch_parking import parked_fixture, _admission


@pytest.fixture
def capacity(parked_fixture):
    f = parked_fixture
    (f.directory / 'staged-completed.json').unlink()
    (f.directory / 'infrastructure-failure.json').unlink()
    result = json.loads((f.directory / 'execution/result.json').read_text())
    result['returncode'] = 1
    put(f.directory / 'execution/result.json', result)
    resource_failure.record_capacity(f.directory / 'runtime', dict(
        drained=True, cause='cgroup_oom', operation='kernel_replay', input_key='exact-input',
        policy_generation='0', attempt_count=3, reservation_bytes=24 << 30,
        limit_bytes=32 << 30, sampled_peak_bytes=32 << 30))
    put(f.driver.runtime / 'checker-state/memory-policy.json', dict(
        version=1, runtime_root=str(f.driver.runtime.resolve()), generation='0'))
    return f


def blocked(f):
    with pytest.raises(parking.HostCheckBlocked) as error:
        capacity_parking.block(f.driver, f.task, f.directory)
    return error.value


def change_policy(f, generation):
    put(f.driver.runtime / 'checker-state/memory-policy.json', dict(
        version=1, runtime_root=str(f.driver.runtime.resolve()), generation=generation))


def test_legacy_memory_stop_automatically_requeues_without_spending_counters(capacity):
    f = capacity
    before = copy.deepcopy(f.state)
    owner = (f.owned / 'staged-completed.json').read_bytes()
    result = f.driver._retry_check(1, f.identity, f.directory, 'host memory stop')
    assert result == {'retry_after_seconds': 2}
    assert f.state == before
    assert (f.owned / 'staged-completed.json').read_bytes() == owner
    assert stages.reusable(f.owned, json.loads(owner)['key'])['status'] == 'facts'
    assert not (f.directory / 'infrastructure-failure.json').exists()
    assert not (f.directory / 'staged-completed.json').exists()
    assert not (f.driver.root / '1' / parking.MARKER).exists()
    retry = json.loads((f.directory / 'retry-authorized.json').read_text())
    assert retry['kind'] == 'host_memory_requeue'
    assert not any(c.get('action') == 'dispatch_control' or 'pause' in c for c in f.calls)


def test_historical_park_resumes_without_policy_change_or_current_producer(capacity):
    f = capacity
    record = parking.persist(f.driver, blocked(f), _admission(f))
    f.driver._parking_implementation = 'new-production-version'
    (f.driver.runtime / 'checker-state/memory-policy.json').unlink()
    assert parking._poll(f.driver, record, parking._bindings(f.driver, record))[0] == 'resume'
    retry = json.loads((f.directory / 'retry-authorized.json').read_text())
    assert retry['kind'] == 'host_memory_requeue'
    assert f.attempt['infrastructure_retries'] == 2
    assert not (f.owned / 'retry-authorized.json').exists()


@pytest.mark.parametrize('damage', ['success', 'completed', 'generic_failure', 'capacity_cause',
    'capacity_not_drained', 'capacity_runtime', 'channels', 'permit', 'cleanup', 'live', 'drain',
    'phase', 'binding', 'owner', 'pending'])
def test_untrusted_or_mixed_failure_does_not_get_capacity_park(capacity, monkeypatch, damage):
    f = capacity
    if damage == 'success':
        result = json.loads((f.directory / 'execution/result.json').read_text())
        result['returncode'] = 0
        put(f.directory / 'execution/result.json', result)
    elif damage == 'completed':
        put(f.directory / 'staged-completed.json', {})
    elif damage == 'generic_failure':
        resource_failure.record(f.directory / 'runtime', 'independent transport failure')
    elif damage.startswith('capacity_'):
        path = f.directory / 'runtime' / resource_failure.CAPACITY_MARKER
        marker = json.loads(path.read_text())
        if damage == 'capacity_cause': marker['detail']['cause'] = 'SIGKILL'
        elif damage == 'capacity_not_drained': marker['detail']['drained'] = False
        else: marker['runtime_root'] = '/another/runtime'
        put(path, marker)
    elif damage in ('permit', 'cleanup', 'channels'):
        name = 'compiler-channels' if damage == 'channels' else damage
        (f.directory / f'execution/{name}.json').unlink()
    elif damage == 'live':
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif damage == 'drain':
        def fail(*_): raise RuntimeError('compiler drain failed')
        monkeypatch.setattr('trellis.dispatch.process.AttemptProcess.stop_and_reap', fail)
    elif damage == 'phase': f.attempt['phase'] = {'phase': 'ready'}
    elif damage == 'binding': f.task['assignment']['scope']['nodes'] = ['different']
    elif damage == 'owner': (f.owned / 'execution/response.json').write_text('{}')
    else:
        def fail(): raise RuntimeError('pending canonical publication')
        f.driver.native._require_recovered_publication = fail
    with pytest.raises(Exception) as error:
        f.driver._retry_check(1, f.identity, f.directory, 'host memory stop')
    assert not isinstance(error.value, parking.HostCheckBlocked)
    assert not (f.driver.root / '1' / parking.MARKER).exists()
    assert f.attempt['infrastructure_retries'] == 2


def test_parked_partial_sources_cannot_change_before_recovery(capacity):
    f = capacity
    record = parking.persist(f.driver, blocked(f), _admission(f))
    (f.directory / 'repo/Tablet/A.lean').write_text('changed')
    change_policy(f, '1')
    with pytest.raises(candidate.CandidateError, match='source evidence changed'):
        parking._poll(f.driver, record, parking._bindings(f.driver, record))


def test_recovery_enters_next_finalization_reusing_original_owner(capacity, monkeypatch):
    import asyncio
    from trellis.dispatch import staged_driver
    f = capacity
    capture_path = f.owned / 'canonical.json'
    capture = json.loads(capture_path.read_text())
    capture['staged_plan'] = {'owner_nodes': ['A'], 'read_nodes': ['A']}
    put(capture_path, capture)
    owner_output = json.loads((f.owned / 'execution/response.json').read_text())
    owner_output['facts']['read_nodes'] = ['A']
    f.save(f.owned, owner_output)
    (f.owned / 'staged-completed.json').unlink()
    stages.complete(f.owned, owner_output)
    record = parking.persist(f.driver, blocked(f), _admission(f))
    change_policy(f, '1')
    assert parking._poll(f.driver, record, parking._bindings(f.driver, record))[0] == 'resume'
    executed = []
    async def execute(driver, directory, task_id, identity, stage):
        assert stage == 'owned' and directory == f.owned
        executed.append(directory)
        return stages.reusable(directory, capture['key'])
    def prepare(driver, task_id, identity, directory, *, owned=None, checked=None):
        assert directory == f.directory.with_name('4')
        assert owned == f.owned and checked == owner_output
        return None  # Stop at the next normal preparation boundary.
    monkeypatch.setattr(staged_driver, '_execute', execute)
    monkeypatch.setattr(staged_driver, '_prepare', prepare)
    assert asyncio.run(staged_driver.run(f.driver, 1, f.identity))
    assert executed == [f.owned]
    assert f.attempt['infrastructure_retries'] == 2


@pytest.mark.parametrize('staged', [False, True])
def test_interrupted_owner_or_legacy_stage_parks_without_fabricating_completion(capacity, staged):
    import shutil
    f = capacity
    detail = resource_failure.capacity_record(f.directory / 'runtime')['detail']
    shutil.rmtree(f.owned / 'finalization')
    f.directory = f.owned
    (f.directory / 'staged-completed.json').unlink()
    captured = json.loads((f.directory / 'canonical.json').read_text())
    if staged:
        captured['staged_plan'] = {'owner_nodes': ['A']}
        put(f.directory / 'canonical.json', captured)
    else:
        recipe = json.loads((f.directory / 'execution/recipe.json').read_text())
        recipe['command'] = ['python', '-m', 'trellis.dispatch.acceptance']
        put(f.directory / 'execution/recipe.json', recipe)
    result = json.loads((f.directory / 'execution/result.json').read_text())
    result['returncode'] = 1
    put(f.directory / 'execution/result.json', result)
    resource_failure.record_capacity(f.directory / 'runtime', detail)
    record = parking.persist(f.driver, blocked(f), _admission(f))
    assert record['owner_completion_sha256'] is None
    change_policy(f, '1')
    assert parking._poll(f.driver, record, parking._bindings(f.driver, record))[0] == 'resume'
    assert not (f.directory / 'staged-completed.json').exists()
    assert not (f.directory / 'completed.json').exists()
    assert f.attempt['infrastructure_retries'] == 2


def test_memory_requeue_releases_finalization_gate_without_releasing_semantic_lease(capacity, monkeypatch):
    import asyncio
    from trellis.dispatch.acceptance_queue import Footprint
    from trellis.dispatch import driver as module
    f = capacity
    async def run():
        sleeping = asyncio.Event()
        resume = asyncio.Event()
        async def delay(seconds):
            assert seconds == 2
            assert not f.driver.finalization_gate.locked()
            assert f.driver.acceptance_queue.active[1] == f.footprint
            sleeping.set()
            await resume.wait()
        monkeypatch.setattr(module.asyncio, 'sleep', delay)
        monkeypatch.setattr('trellis.dispatch.failure_report.eligible', lambda *_: False)
        f.driver._handoff = lambda *_: None
        f.state['dispatch']['tasks']['2'] = dict(id=2, assignment={},
            status=dict(status='checking', candidate_sha256='other'))
        f.driver._acceptance_footprint = lambda task_id, *_: (f.footprint if task_id == 1
            else Footprint(frozenset({'B'}), frozenset({'B'})))
        calls = []
        async def integrate(task_id, identity, **_):
            calls.append(task_id)
            async with f.driver.finalization_gate:
                if task_id == 1 and calls.count(1) == 1:
                    await f.driver._handle_check_failure(1, identity, f.directory, 'memory stop')
        f.driver._integrate_reserved = integrate
        first = asyncio.create_task(f.driver.integrate(1, f.identity))
        await asyncio.wait_for(sleeping.wait(), 1)
        await asyncio.wait_for(f.driver.integrate(2, 'other'), 1)
        assert not first.done()
        resume.set()
        await asyncio.wait_for(first, 1)
        assert calls == [1, 2, 1]
        assert not f.driver.acceptance_queue.active
    asyncio.run(run())


def test_crash_after_authorization_then_second_policy_change_reuses_marker(capacity):
    f = capacity
    record = parking.persist(f.driver, blocked(f), _admission(f))
    change_policy(f, '1')
    assert capacity_parking.authorize_recovery(f.driver, f.task, record, {'policy_generation': '1'})
    authorization = (f.directory / 'retry-authorized.json').read_bytes()
    change_policy(f, '2')
    assert parking._poll(f.driver, record, parking._bindings(f.driver, record))[0] == 'resume'
    assert (f.directory / 'retry-authorized.json').read_bytes() == authorization
    assert f.attempt['infrastructure_retries'] == 2


def test_corrupt_existing_capacity_authorization_is_not_adopted(capacity):
    f = capacity
    record = parking.persist(f.driver, blocked(f), _admission(f))
    change_policy(f, '1')
    assert capacity_parking.authorize_recovery(f.driver, f.task, record, {'policy_generation': '1'})
    path = f.directory / 'retry-authorized.json'
    authorization = json.loads(path.read_text())
    authorization['candidate_sha256'] = 'different'
    put(path, authorization)
    with pytest.raises(candidate.CandidateError, match='authorization changed'):
        capacity_parking.authorize_recovery(f.driver, f.task, record, {'policy_generation': '1'})


@pytest.mark.parametrize('supported,old', [(False, None), (True, None), (True, False), (True, True)])
def test_only_new_supported_host_launch_gets_capacity_recovery(tmp_path, supported, old):
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    directory = tmp_path / 'dispatch/1/acceptance/1'
    execution = directory / 'execution'; execution.mkdir(parents=True)
    put(directory / 'dispatch-request.json', {'kind': 'worker'})
    recipe = execution / 'recipe.json'
    if old is not None:
        put(recipe, {'checker': dict(lifetime=None, **({'capacity_recovery': True} if old else {}))})
    before = recipe.read_bytes() if recipe.exists() else None
    calls = []
    process = SimpleNamespace(start_or_adopt=lambda *args, **kwargs: calls.append(kwargs) or {'pid': 1})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime, driver.repo = tmp_path / 'dispatch', tmp_path, tmp_path / 'repo'
    driver.view, driver.environment = None, {}
    driver.process_factory = lambda _: process
    driver._process(directory, compiler='managed', seconds=None,
        command=['python3', '-m', 'trellis.dispatch.acceptance'], payload={'candidate': 'sealed'},
        capacity_recovery=supported)
    assert calls[0]['checker'].get('capacity_recovery', False) == (supported and old is not False)
    assert (recipe.read_bytes() if recipe.exists() else None) == before


@pytest.mark.parametrize('grant', [False, True])
def test_launcher_forwards_only_explicit_host_capacity_grant(tmp_path, monkeypatch, grant):
    from trellis.dispatch import resources
    from test_dispatch_resources import runtime_at
    runtime = runtime_at(tmp_path / 'runtime')
    private = runtime_at(runtime / 'dispatch/1/acceptance/1/runtime')
    root = private.parent / 'execution'; root.mkdir()
    observed = []
    monkeypatch.setattr(resources.workspaces, 'register', lambda *args, **kwargs:
        observed.append(kwargs) or {'TRELLIS_CHECKER_TOKEN': 'host-test-token'})
    resources.grant(root, dict(runtime=str(runtime), task_runtime=str(private), compiler='none',
        scope='1/acceptance/1', lifetime=None, **({'capacity_recovery': True} if grant else {})))
    assert observed == [dict(lifetime=None, **({'capacity_recovery': True} if grant else {}))]


def memory_outcome(f, *, kind='resource_rejected', cause='cgroup_oom', phase='candidate'):
    runtime = f.directory / 'runtime'
    (runtime / resource_failure.CAPACITY_MARKER).unlink()
    resource_failure.memory_phase(runtime, phase, f.identity)
    resource_failure.record_capacity(runtime, dict(
        version=2, kind=kind, drained=True, cause=cause, operation='kernel_replay',
        input_key='exact-input', policy_generation='0', attempt_count=2,
        reservation_bytes=16 << 30, limit_bytes=32 << 30,
        sampled_peak_bytes=30 << 30, cgroup_peak_bytes=32 << 30,
        attempted_limit_bytes=[16 << 30, 32 << 30], last_attempted_limit_bytes=32 << 30,
        pressure_requeues=4, pressure_execution_seconds=15))


def serial_outcome(f, phase='candidate', pressure=False):
    from test_checker_serial_recovery import pool, retry, stop
    state = retry(pool(f.directory / 'retry-history'))
    stop(state)
    for _ in range(3 if pressure else 1):
        stop(state, 'host_pressure' if pressure else 'cgroup_oom')
    from trellis.checker.memory_retry import ResourceOutcome
    if pressure:
        state.data['admission_deadline'] = time.time() - 1
    with pytest.raises(ResourceOutcome) as error:
        state.check_wait() if pressure else state.remaining(20)
    runtime = f.directory / 'runtime'
    (runtime / resource_failure.CAPACITY_MARKER).unlink()
    resource_failure.memory_phase(runtime, phase, f.identity)
    resource_failure.record_capacity(runtime, error.value.detail)


def test_serial_exhaustion_delivers_cost_feedback_through_native_correction(capacity):
    f = capacity
    serial_outcome(f)
    payloads = []
    f.driver.native._call = lambda payload: {'in_flight_request': {'id': 8, 'cycle': 12}}
    f.driver.native.prepare_publication = lambda payload: payload
    f.driver.native.commit_failure = payloads.append
    assert f.driver._retry_check(1, f.identity, f.directory, 'host failure') == {'rejected': True}
    assert len(payloads) == 1
    response = payloads[0]['response']
    assert response['outcome'] == 'Invalid' and payloads[0]['submitted_outcome'] == 'Valid'
    for text in ('48.00 GiB', '56.00 GiB', '1 thread(s)', 'No proof invalidity', 'Splitting files alone', 'assigned scope'):
        assert text in response['comments']
    assert (f.driver.root / '1/turns/1/feedback.json').exists()
    assert not (f.directory / 'retry-authorized.json').exists()


@pytest.mark.parametrize('phase,pressure', [('baseline', False)])
def test_serial_infrastructure_failure_uses_existing_bounded_counter(capacity, phase, pressure):
    f = capacity
    serial_outcome(f, phase, pressure)
    f.attempt['infrastructure_retries'] = 0
    f.driver.native.commit_failure = lambda *_: pytest.fail('not a candidate correction')
    f.driver.native._call = f.calls.append
    assert f.driver._retry_check(1, f.identity, f.directory, 'host failure') is None
    authorization = json.loads((f.directory / 'retry-authorized.json').read_text())
    assert authorization['kind'] == 'bounded_infrastructure_retry'
    assert authorization['event']['retry'] == 1
    assert 'candidate is retained' in authorization['reason']
    if phase == 'baseline':
        assert 'before applying your edits' in authorization['reason']
    assert not (f.driver.root / '1/turns/1/feedback.json').exists()


@pytest.mark.parametrize('phase', ['candidate', 'baseline'])
@pytest.mark.parametrize('legacy_reason', [None, 'serial_headroom_unavailable',
                                          'serial_pressure_launches_exhausted', 'recovery_execution_deadline'])
def test_capacity_wait_does_not_spend_exhausted_infrastructure_allowance(capacity, phase, legacy_reason):
    f = capacity
    serial_outcome(f, phase=phase, pressure=True)
    if legacy_reason:
        path = f.directory / 'runtime' / resource_failure.MEMORY_MARKER
        marker = json.loads(path.read_text())
        marker['detail'].pop('capacity_wait')
        marker['detail'].update(version=3, terminal_reason=legacy_reason, exhausted_reason=legacy_reason)
        put(path, marker)
    before = copy.deepcopy(f.state)
    f.driver._pause = lambda *args, **kw: pytest.fail('capacity wait paused coordinator')
    f.driver.native.commit_failure = lambda *_: pytest.fail('capacity wait rejected candidate')
    assert f.driver._retry_check(1, f.identity, f.directory, 'host failure') == {'retry_after_seconds': 2}
    assert f.state == before
    assert f.task['status'] == dict(status='checking', candidate_sha256=f.identity)
    assert (f.directory / 'memory-delivery.json').exists()
    assert json.loads((f.directory / 'retry-authorized.json').read_text())['kind'] == 'host_memory_requeue'


@pytest.mark.parametrize('phase', ['candidate', 'baseline'])
def test_exhausted_candidate_delivers_native_invalid_with_measured_costs(capacity, phase):
    f = capacity
    memory_outcome(f, phase=phase)
    if phase == 'baseline':
        path = f.directory / 'runtime' / resource_failure.MEMORY_MARKER
        marker = json.loads(path.read_text())
        marker['detail'].update(admission_policy='available', maximum_job_bytes=32 << 30)
        put(path, marker)
    attempt = copy.deepcopy(f.attempt)
    owner = (f.owned / 'staged-completed.json').read_bytes()
    payloads = []
    def call(payload):
        assert payload['action'] == 'dispatch_acceptance_state'
        return {'in_flight_request': {'id': 8, 'cycle': 12}}
    f.driver.native._call = call
    f.driver.native.prepare_publication = lambda payload: payload
    f.driver.native.commit_failure = payloads.append
    assert f.driver._retry_check(1, f.identity, f.directory, 'host failure') == {'rejected': True}
    assert len(payloads) == 1
    payload = payloads[0]
    assert payload['response']['outcome'] == 'Invalid'
    assert payload['response']['status'] == 'Ok'
    assert payload['submitted_outcome'] == 'Valid'
    assert payload['response']['request_id'] == 8 and payload['response']['cycle'] == 12
    reason = payload['response']['comments']
    assert 'sampled process-tree peak 30.00 GiB' in reason
    assert 'cgroup peak 32.00 GiB' in reason
    assert 'executed limits 16.00 GiB, 32.00 GiB' in reason
    assert 'kernel_replay' in reason
    if phase == 'baseline':
        assert 'candidate was not assessed' in reason and 'no proof invalidity' in reason
        assert 'Simplify the proof' not in reason
    else:
        assert 'Simplify the proof' in reason
    assert payload['graph_before'] == payload['graph_after']
    assert json.loads((f.driver.root / '1/turns/1/feedback.json').read_text()) == payload
    assert (f.directory / 'memory-delivery.json').exists()
    assert not (f.directory / 'retry-authorized.json').exists()
    assert not (f.directory / 'staged-completed.json').exists()
    assert (f.owned / 'staged-completed.json').read_bytes() == owner
    assert f.attempt == attempt  # Only native commit_failure may correct/resolve.


@pytest.mark.parametrize('kind,cause,phase', [
    ('host_pressure_requeue', 'host_pressure', 'candidate'),
    ('resource_rejected', 'execution_limit', 'baseline'),
])
def test_host_pressure_and_pre_overlay_baseline_do_not_reject_candidate(capacity, kind, cause, phase):
    f = capacity
    memory_outcome(f, kind=kind, cause=cause, phase=phase)
    before = copy.deepcopy(f.state)
    assert f.driver._retry_check(1, f.identity, f.directory, 'memory stop') == {'retry_after_seconds': 2}
    assert f.state == before
    assert not (f.driver.root / '1/turns/1/feedback.json').exists()
    assert not any(call.get('action') == 'dispatch_control' for call in f.calls)
    raw = (f.directory / 'retry-authorized.json').read_bytes()
    # Re-entry adopts its exact authorization even if another retained round exists.
    put(f.owned / 'finalization/5/memory-delivery.json', {})
    assert f.driver._retry_check(1, f.identity, f.directory, 'memory stop') == {'retry_after_seconds': 2}
    assert (f.directory / 'retry-authorized.json').read_bytes() == raw


@pytest.mark.parametrize('damage', ['missing', 'count', 'last', 'negative'])
def test_candidate_rejection_requires_actual_executed_limits(capacity, damage):
    f = capacity
    memory_outcome(f)
    path = f.directory / 'runtime' / resource_failure.MEMORY_MARKER
    marker = json.loads(path.read_text())
    detail = marker['detail']
    if damage == 'missing': del detail['attempted_limit_bytes']
    elif damage == 'count': detail['attempt_count'] = 3
    elif damage == 'last': detail['last_attempted_limit_bytes'] = 64 << 30
    else: detail['attempted_limit_bytes'][0] = -1
    put(path, marker)
    with pytest.raises(candidate.CandidateError, match='executed limit evidence'):
        f.driver._retry_check(1, f.identity, f.directory, 'memory stop')
    assert not (f.driver.root / '1/turns/1/feedback.json').exists()


@pytest.mark.parametrize('stage', ['owned', 'finalize'])
def test_staged_memory_rejection_returns_to_coordinator_without_restarting_host(capacity, monkeypatch, stage):
    import asyncio
    from trellis.dispatch import staged_driver
    from trellis.dispatch.driver import HostCheckFailure
    f = capacity
    captured = json.loads((f.owned / 'canonical.json').read_text())
    captured['staged_plan'] = {'owner_nodes': ['A'], 'read_nodes': ['A']}
    put(f.owned / 'canonical.json', captured)
    checked = json.loads((f.owned / 'execution/response.json').read_text())
    checked['facts']['read_nodes'] = ['A']
    calls = []
    async def execute(driver, directory, task_id, identity, actual):
        calls.append(actual)
        if actual == stage: raise HostCheckFailure('controlled candidate limit')
        return checked
    async def handle(*args):
        calls.append('negative_delivery')
        f.attempt['phase'] = {'phase': 'ready'}  # Native correction already committed.
        return True
    monkeypatch.setattr(staged_driver, '_execute', execute)
    monkeypatch.setattr(f.driver, '_handle_check_failure', handle)
    assert asyncio.run(staged_driver.run(f.driver, 1, f.identity)) is True
    assert calls == (['owned', 'negative_delivery'] if stage == 'owned'
                     else ['owned', 'finalize', 'negative_delivery'])
    assert not f.driver.finalization_gate.locked()
