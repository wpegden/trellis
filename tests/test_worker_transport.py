import copy
import json
from pathlib import Path

import pytest

from trellis.dispatch import worker_transport as transport
from trellis.dispatch.candidate import canonical_json, digest
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, canonical_json(value))


@pytest.fixture(params=['codex', 'deepseek', 'kimi'])
def stopped(tmp_path, monkeypatch, request):
    execution = tmp_path / '99/turns/1/execution'
    clock = execution / 'clock'
    process = dict(pid=100, start_ticks=10, boot_id='boot')
    owner = dict(pid=101, start_ticks=11, boot_id='boot')
    thread_id = 'original-thread' if request.param == 'codex' else '12345678-1234-4234-8234-123456789abc'
    session = dict(provider=request.param, model='sol', reasoning_effort='xhigh',
                   home=str(tmp_path / 'home'), thread_id=thread_id)
    put(Path(session['home']) / '.codex/sessions/2026/10/10/rollout-2026-10-10T13-00-00-original-thread.jsonl', {})
    if request.param != 'codex':
        state_root = tmp_path / 'api-state'
        monkeypatch.setenv('TRELLIS_API_STATE_DIR', str(state_root))
        put(state_root / (thread_id + '.json'), dict(binding=session, endpoint='https://test/v1', messages=[]))
    config = dict(allowance_ms=14400000, binding=dict(session, thread_id=''))
    put(clock / 'config.json', config)
    common = dict(config_sha256=digest(canonical_json(config)), owner=owner)
    timing = dict(common, disposition='interrupted', detail='Provider exited without a completed turn',
                  elapsed_ms=2447592, thread_id=thread_id, provider_budget='', started_at_ms=10)
    for name, value in [('owner', common), ('started', dict(common, started_at_ms=10)),
                        ('session', dict(common, thread_id=thread_id)), ('finished', timing),
                        ('exited', dict(common, returncode=0))]:
        put(clock / (name + '.json'), value)
    runtime = tmp_path / 'runtime'
    put(runtime / 'worker-session.json', session)
    turn = dict(turn=1, remaining_ms=14400000, execution_record=str(clock))
    put(execution / 'recipe.json', dict(payload=dict(worker_turn=turn, runtime_root=str(runtime), request=dict(id=2820, cycle=867))))
    for name in ['process', 'permit', 'cleanup']:
        put(execution / (name + '.json'), process)
    response = dict(kind='worker_submission', turn=1, request_id=2820, cycle=867, provider_execution=timing,
                    provider_turn_completed=False, raw={'outcome': 'valid'}, transport_error='stream disconnected')
    put(execution / 'response.json', response)
    put(execution / 'result.json', dict(returncode=0, response_sha256=digest(canonical_json(response))))
    task = dict(id=99, process=process, worker_attempt=dict(turn=1, allowance_ms=14400000, consumed_ms=0))
    monkeypatch.setattr(transport, 'alive', lambda _: False)
    return execution, task, response


def test_owned_interruption_preserves_unsubmitted_raw_and_authenticates_remaining_clock(stopped):
    execution, task, response = stopped
    before = {str(p): p.read_bytes() for p in execution.rglob('*') if p.is_file()}
    event = transport.interruption(execution, task, response)
    assert event['elapsed_ms'] == 2447592
    assert event['session']['thread_id'] == response['provider_execution']['thread_id']
    assert event['turn'] == 1 and len(event['evidence_sha256']) == 64
    assert {str(p): p.read_bytes() for p in execution.rglob('*') if p.is_file()} == before
    assert response['raw'] == {'outcome': 'valid'}  # Never fills missing fields or calls this a submission.


@pytest.mark.parametrize('mutation', ['cleanup', 'receipt', 'thread', 'clock', 'live', 'budget', 'completed'])
def test_interruption_refuses_changed_or_unauthenticated_receipts(stopped, monkeypatch, mutation):
    execution, task, response = stopped
    if mutation == 'cleanup':
        put(execution / 'cleanup.json', dict(task['process'], pid=999))
    elif mutation == 'receipt':
        put(execution / 'result.json', dict(returncode=0, response_sha256='0' * 64))
    elif mutation == 'thread':
        session = json.loads((execution.parents[3] / 'runtime/worker-session.json').read_text())
        put(execution.parents[3] / 'runtime/worker-session.json', dict(session, thread_id='other'))
    elif mutation == 'clock':
        task['worker_attempt']['consumed_ms'] = 1
    elif mutation == 'live':
        monkeypatch.setattr(transport, 'alive', lambda _: True)
    elif mutation in ('budget', 'completed'):
        response = copy.deepcopy(response)
        if mutation == 'budget':
            response['provider_execution']['provider_budget'] = 'budget exhausted'
            put(execution / 'clock/finished.json', response['provider_execution'])
        else:
            response['provider_turn_completed'] = True
        put(execution / 'response.json', response)
        put(execution / 'result.json', dict(returncode=0, response_sha256=digest(canonical_json(response))))
    with pytest.raises((RuntimeError, ValueError)):
        transport.interruption(execution, task, response)


def test_transport_execution_layout_keeps_original_receipts_and_submission_paths(tmp_path):
    from trellis.dispatch.storage import execution_directories, worker_execution_coordinates
    root = tmp_path / '99'
    original = root / 'turns/1/execution'
    resumed = transport.execution_directory(root / 'turns/1', 1)
    original.mkdir(parents=True); resumed.mkdir(parents=True)
    assert original in execution_directories(root) and resumed in execution_directories(root)
    assert worker_execution_coordinates(original) == (1, 0)
    assert worker_execution_coordinates(resumed) == (1, 1)
    assert worker_execution_coordinates(root / 'acceptance/1/execution') is None


@pytest.mark.parametrize('ordinal,seconds', [(1,120),(2,240),(3,480),(4,900),(5,900),(2**32-1,900)])
def test_shared_transport_delay_uses_native_transport_count(ordinal, seconds):
    assert transport.retry_delay_seconds(ordinal) == seconds


@pytest.mark.parametrize('ordinal', [0,-1,True,None,'1'])
def test_transport_delay_rejects_missing_native_ordinal(ordinal):
    with pytest.raises(RuntimeError):transport.retry_delay_seconds(ordinal)


def paused_attempt(stopped):
    execution, task, response = stopped
    observed = transport.interruption(execution, task, response)
    attempt = dict(task['worker_attempt'], phase=dict(phase='transport_ready'),
        transport_retries=1, last_interruption=observed, consumed_ms=observed['elapsed_ms'])
    return execution.parent, attempt, response['request_id']


def test_transport_wait_reconstructs_same_owned_deadline_without_refill(stopped):
    submission, attempt, request_id = paused_attempt(stopped)
    status = transport.wait_status(submission, attempt, request_id)
    assert status['next_retry_at_ms'] == 10 + attempt['last_interruption']['elapsed_ms'] + 120000
    assert status['remaining_ms'] == attempt['allowance_ms'] - attempt['consumed_ms']
    assert not (submission/'execution/transport-wait.json').exists(), 'status stays read-only'
    assert not transport.ready(status, now_ms=status['next_retry_at_ms'] - 1)
    assert transport.ready(status, now_ms=status['next_retry_at_ms'])
    saved = transport.prepare_wait(submission, attempt, request_id)
    raw=(submission/'execution/transport-wait.json').read_bytes()
    assert transport.prepare_wait(submission, attempt, request_id)==saved==status
    assert (submission/'execution/transport-wait.json').read_bytes()==raw
    assert transport.wait_status(submission, attempt, request_id)==status


def test_manual_transport_permit_expedites_only_exact_interruption(stopped):
    submission, attempt, request_id = paused_attempt(stopped)
    status = transport.resume(submission, attempt, request_id)
    assert not status['manual_permit']
    resumed = transport.wait_status(submission, attempt, request_id)
    assert resumed['manual_permit'] and transport.ready(resumed, now_ms=0)
    assert resumed['remaining_ms']==status['remaining_ms']
    wrong = transport.wait_record(status);wrong['evidence_sha256']='f'*64
    put(submission/'execution/transport-resume.json', wrong)
    with pytest.raises(RuntimeError,match='permit belongs'):transport.wait_status(submission, attempt, request_id)


def test_transport_wait_rejects_clock_or_deadline_drift(stopped):
    submission, attempt, request_id = paused_attempt(stopped)
    status=transport.prepare_wait(submission, attempt, request_id)
    wrong=transport.wait_record(status);wrong['next_retry_at_ms']+=1
    put(submission/'execution/transport-wait.json', wrong)
    with pytest.raises(RuntimeError,match='deadline or evidence'):transport.wait_status(submission, attempt, request_id)


def test_transport_wait_cannot_grant_foreign_request_permit(stopped):
    submission,attempt,request_id=paused_attempt(stopped)
    with pytest.raises(RuntimeError,match='original request or ordinal'):
        transport.resume(submission,attempt,request_id+1)
    assert not (submission/'execution/transport-resume.json').exists()


def test_rescheduled_completion_never_emits_candidate_ready(tmp_path):
    from trellis.dispatch.native import NativeLedger
    from trellis.dispatch.coordination import Completion
    def forbidden(*args, **kwargs):
        pytest.fail('no-submission callback must not issue native candidate_ready')
    native = NativeLedger(tmp_path, kernel=forbidden)
    assert native.complete_delivery(dict(id=99), Completion(99), lambda: 0) is None


@pytest.mark.parametrize('task_kind', ['ordinary', 'lean_closure'])
def test_transport_prompt_reuses_thread_and_distinct_artifact_without_claiming_verdict(tmp_path, task_kind):
    from dataclasses import replace
    from test_worker_submission import single_request
    from trellis.agent_wrapper.worker_submission import prepare_turn
    from trellis.agent_wrapper.protocol import ArtifactSpec
    from trellis.agent_wrapper.continuation import session_binding
    single = replace(single_request(tmp_path), artifact=ArtifactSpec('trellis_worker_7_submission-1-transport-1.json', 'worker'))
    session = session_binding(single.provider, single.burst_home, 'original-thread')
    resumed = prepare_turn(single, dict(turn=1, remaining_ms=11952408, session=session,
        execution_record=str(tmp_path / 'clock'), transport_ordinal=1, transport_continuation=True,
        retain_baseline=True), tmp_path / 'runtime', task_kind=task_kind)
    assert resumed.resume_thread_id == 'original-thread'
    assert resumed.timeout_seconds == 11952.408
    assert 'no host proof verdict' in resumed.prompt
    assert 'submission-1-transport-1.raw.json' in resumed.prompt
    assert 'including deleted_nodes' in resumed.prompt
    assert ('Continue concrete Lean proof development' in resumed.prompt) == (task_kind == 'lean_closure')
    if task_kind == 'lean_closure':
        assert '11952.4 seconds' in resumed.prompt


@pytest.mark.parametrize('task_kind', ['ordinary', 'lean_closure'])
def test_new_correction_after_transport_keeps_authoritative_feedback_prompt(tmp_path, task_kind):
    from dataclasses import replace
    from test_worker_submission import single_request
    from trellis.agent_wrapper.worker_submission import prepare_turn, native_digest
    from trellis.agent_wrapper.protocol import ArtifactSpec
    from trellis.agent_wrapper.continuation import session_binding
    single = replace(single_request(tmp_path), artifact=ArtifactSpec('trellis_worker_7_submission-2-transport-1.json', 'worker'))
    feedback = dict(response=dict(comments='Correct the witnessed scope defect'))
    session = session_binding(single.provider, single.burst_home, 'original-thread')
    resumed = prepare_turn(single, dict(turn=2, remaining_ms=6000, session=session,
        execution_record=str(tmp_path / 'clock'), transport_ordinal=1, transport_continuation=False,
        feedback=feedback, feedback_sha256=native_digest(feedback)), tmp_path / 'runtime', task_kind=task_kind)
    assert 'Submission 1 failed authoritative checking' in resumed.prompt
    assert 'Correct the witnessed scope defect' in resumed.prompt
    assert 'no host proof verdict' not in resumed.prompt
    assert resumed.resume_thread_id == 'original-thread'
    assert ('Continue concrete Lean proof development' in resumed.prompt) == (task_kind == 'lean_closure')
    if task_kind == 'lean_closure':
        assert '6 seconds' in resumed.prompt
    assert resumed.timeout_seconds == 6


def test_driver_interruption_returns_no_candidate_and_does_not_pause(stopped):
    import asyncio
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    execution, task, response = stopped
    task.update(status=dict(status='running'), assignment=dict(compiler='managed'))
    task['worker_attempt'].update(phase=dict(phase='running'), contract_sha256='a' * 64)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = execution.parents[3]
    events = []
    driver._process = lambda *args, **kwargs: (SimpleNamespace(root=execution, stop_and_reap=lambda _: None), task['process'])
    driver._handoff = lambda *args: None
    driver._pause = lambda *args, **kwargs: pytest.fail('authenticated transient failure must not pause the run')
    async def wait(*args, poll):
        await poll()
        return response
    async def observe(event):
        events.append(event)
    driver._wait = wait
    driver.observe = observe
    result = asyncio.run(driver._launch_submission(task))
    assert result.candidate_sha256 is None and result.failure is None
    assert len(events) == 1 and events[0]['kind'] == 'worker_interrupted'
    assert events[0]['interruption']['elapsed_ms'] == 2447592
    assert not (execution.parent / 'candidate').exists()
