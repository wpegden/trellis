"""Owned serial delivery/recovery, without provider or compiler execution."""
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.agent_wrapper import serial_worker as serial
from trellis.dispatch import candidate


POLICY = dict(version=1, max_submissions=3, max_infrastructure_retries=2)


@pytest.mark.parametrize('outcome', ['ok', 'missing', 'changed'])
@pytest.mark.parametrize('started', [False, True])
def test_serial_wait_rechecks_receipt_after_launcher_exit(tmp_path, monkeypatch, outcome, started):
    data = candidate.canonical_json({'outcome': 'Valid'})
    (tmp_path / 'response.json').write_bytes(data)
    receipt = dict(returncode=0, response_sha256='bad' if outcome == 'changed' else candidate.digest(data))
    available = False
    reads = []
    def result():
        reads.append(True)
        return receipt if available and outcome != 'missing' else None
    def alive(_):
        nonlocal available
        available = True
        return False
    monkeypatch.setattr(serial, 'alive', alive)
    process = SimpleNamespace(root=tmp_path, result=result)
    if outcome == 'ok':
        assert serial._wait(process, {}, started=started) == (
            None if started else (receipt, {'outcome': 'Valid'}))
    else:
        with pytest.raises(RuntimeError, match='without a receipt' if outcome == 'missing' else 'owned receipt'):
            serial._wait(process, {}, started=started)
    assert len(reads) == 2


def fixture(tmp_path):
    repo, runtime = tmp_path / 'repo', tmp_path / 'runtime'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('original source')
    directory = runtime / 'worker-attempts/7-owned'
    directory.mkdir(parents=True)
    request = dict(id=7, cycle=2, kind='worker', worker_submission_policy=POLICY)
    attempt = dict(contract_sha256='a' * 64, policy=POLICY, turn=1, allowance_ms=1000,
                   consumed_ms=0, infrastructure_retries=0, phase=dict(phase='finished'))
    serial.write(runtime / 'worker-attempts/current.json',
                 dict(directory=directory.name, request_id=7, contract_sha256=attempt['contract_sha256']))
    serial.write(directory / 'base-manifest.json', candidate.source_manifest(repo))
    config = SimpleNamespace(repo_path=repo, source_path=tmp_path / 'config.json')
    return config, runtime, directory, request, attempt


def step(config, runtime, request, attempt):
    return serial.next_step(config, runtime, request, attempt, contract_sha256=attempt['contract_sha256'])


def test_serial_terminal_invalid_replays_after_native_rollback(tmp_path):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    source = config.repo_path / 'Tablet/A.lean'
    source.write_text('unaccepted worker edit')
    response = dict(kind='worker', request_id=7, cycle=2, outcome='Invalid', acceptance_check_passed=False)
    serial._terminal(directory, request, response, config.repo_path, enriched=True)
    assert step(config, runtime, request, attempt)['response'] == response
    source.write_text('original source')  # Native restore before a failed checkpoint sink.
    replay = step(config, runtime, request, attempt)
    assert replay['response'] == response and replay['enriched']
    source.write_text('unrelated third source')
    with pytest.raises(RuntimeError, match='sources changed'):
        step(config, runtime, request, attempt)


def test_serial_terminal_valid_never_replays_against_restored_source(tmp_path):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    source = config.repo_path / 'Tablet/A.lean'
    source.write_text('checked source')
    response = dict(kind='worker', request_id=7, cycle=2, outcome='Valid', acceptance_check_passed=True)
    serial._terminal(directory, request, response, config.repo_path, enriched=True)
    assert step(config, runtime, request, attempt)['response'] == response
    source.write_text('original source')
    with pytest.raises(RuntimeError, match='sources changed'):
        step(config, runtime, request, attempt)


def test_serial_owned_attempt_cannot_adopt_other_request_or_escape(tmp_path):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    for reference in [dict(directory='../outside', request_id=7, contract_sha256='a' * 64),
                      dict(directory=directory.name, request_id=8, contract_sha256='a' * 64),
                      dict(directory=directory.name, request_id=7, contract_sha256='b' * 64)]:
        serial.write(runtime / 'worker-attempts/current.json', reference)
        with pytest.raises(RuntimeError, match='different request'):
            step(config, runtime, request, attempt)


def test_serial_no_thread_expiry_keeps_terminal_failure(tmp_path, monkeypatch):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    attempt['phase'] = dict(phase='running', process=dict(pid=1))
    timing = dict(disposition='expired', elapsed_ms=1000)
    response = dict(kind='worker_submission', turn=1, raw=None, provider_execution=timing)
    reaped = []
    monkeypatch.setattr(serial, '_wait', lambda *_args, **_kwargs: (dict(returncode=0), response))
    monkeypatch.setattr(serial.AttemptProcess, 'stop_and_reap', lambda *_args: reaped.append(True))
    monkeypatch.setattr(serial.provider_clock, 'read', lambda *_args: timing)
    from trellis.runtime import bridge
    terminal = dict(kind='worker', request_id=7, cycle=2, outcome='Invalid', transport_failure=True)
    monkeypatch.setattr(bridge, '_save_malformed_worker_result', lambda **_kwargs: terminal)
    result = step(config, runtime, request, attempt)
    assert reaped and result['event']['observation'] == dict(kind='expired', process=attempt['phase']['process'])
    assert serial.read(directory / 'terminal.json')['response'] == terminal
    assert not (directory / 'runtime/worker-session.json').exists()


def test_serial_completion_requires_owned_clock_and_full_reaping(tmp_path, monkeypatch):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    attempt['phase'] = dict(phase='running', process=dict(pid=1))
    timing = dict(disposition='completed', elapsed_ms=100, thread_id='retained')
    response = dict(kind='worker_submission', turn=1, raw={'outcome': 'Valid'},
                    provider_execution=timing, provider_turn_completed=False)
    reaped = []
    monkeypatch.setattr(serial, '_wait', lambda *_args, **_kwargs: (dict(returncode=0), response))
    monkeypatch.setattr(serial.AttemptProcess, 'stop_and_reap', lambda *_args: reaped.append(True))
    monkeypatch.setattr(serial.provider_clock, 'read', lambda *_args: timing)
    with pytest.raises(RuntimeError, match='history did not finish'):
        step(config, runtime, request, attempt)
    assert reaped
    response['provider_turn_completed'] = True
    serial.write(directory / 'runtime/worker-session.json', dict(thread_id='retained'))
    observed = step(config, runtime, request, attempt)['event']['observation']
    assert observed['kind'] == 'turn_completed'
    assert observed['completion']['elapsed_ms'] == 100
    timing.update(disposition='expired', elapsed_ms=1200)
    assert step(config, runtime, request, attempt)['event']['observation']['completion']['elapsed_ms'] == 1000


@pytest.mark.parametrize('change', ['returncode', 'kind', 'turn'])
def test_serial_rejects_unowned_submission_envelope(tmp_path, monkeypatch, change):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    attempt['phase'] = dict(phase='running', process=dict(pid=1))
    result, response = dict(returncode=0), dict(kind='worker_submission', turn=1)
    if change == 'returncode': result['returncode'] = 2
    elif change == 'kind': response['kind'] = 'engineering_host_failure'
    else: response['turn'] = 2
    reaped = []
    monkeypatch.setattr(serial, '_wait', lambda *a, **kw: (result, response))
    monkeypatch.setattr(serial.AttemptProcess, 'stop_and_reap', lambda *a: reaped.append(True))
    with pytest.raises(RuntimeError, match='no owned submission'):
        step(config, runtime, request, attempt)
    assert reaped and not (directory / 'turns/1/delivery.json').exists()


def test_serial_retry_uses_owned_failed_check_source_without_rebuilding_baseline(tmp_path, monkeypatch):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    turn = directory / 'turns/1'
    identity = candidate.seal(config.repo_path, turn / 'candidate', task_id=7,
        base_manifest=serial.read(directory / 'base-manifest.json'), inputs={}, response={'outcome': 'Valid'})
    source = config.repo_path / 'Tablet/A.lean'
    source.write_text('trusted checker normalization')
    serial.write(turn / 'checks/0/failed.json', dict(candidate_sha256=identity,
        source_manifest=candidate.source_manifest(config.repo_path)))
    from trellis import config as config_module
    from trellis.runtime import bridge
    monkeypatch.setattr(config_module, 'load_config', lambda _: config)
    baseline = bridge._bridge_private_state_dir(config.repo_path, runtime) / 'trellis_worker_7_result.acceptance.json'
    serial.write(baseline, dict(original='baseline'))
    seen = []
    def check(**kwargs):
        seen.append(kwargs['acceptance_context'])
        return dict(kind='worker', outcome='Valid', acceptance_check_passed=True)
    monkeypatch.setattr(bridge, 'check_completed_worker', check)
    payload = dict(config_path=str(config.source_path), runtime_root=str(runtime), directory=str(directory),
                   request=request, turn=1, candidate_sha256=identity, check_round=1)
    result = serial.check(payload)
    assert seen == [dict(original='baseline')]
    assert result['candidate_sha256'] == identity
    source.write_text('unowned edit')
    with pytest.raises(RuntimeError, match='sources changed after sealing'):
        serial.check(payload)


def test_serial_resume_issues_exact_current_turn_permit(tmp_path, monkeypatch):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    execution = directory / 'turns/1/execution'
    record = dict(pid=42, start_ticks=10, boot_id='boot')
    pause = dict(ordinal=2, process=record, resume_allowed=True,
                 pause=dict(kind='provider_budget', reason='quota', provider='codex', role='worker'))
    serial.write(execution / 'provider-pause.json', pause)
    serial.write(runtime / 'protocol_state.json', dict(in_flight_request=request,
        worker_attempt=dict(request_id=7, attempt=attempt)))
    monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    serial.resume(runtime)
    permit = dict(ordinal=2, process=record)
    assert serial.read(execution / 'provider-resume.json') == permit
    serial.resume(runtime)
    assert serial.read(execution / 'provider-resume.json') == permit
    pause['resume_allowed'] = False
    serial.write(execution / 'provider-pause.json', pause)
    with pytest.raises(RuntimeError, match='continuation unavailable'):
        serial.resume(runtime)


def test_serial_rewind_guard_requires_owned_cleanup(tmp_path, monkeypatch):
    config, runtime, directory, request, attempt = fixture(tmp_path)
    execution = directory / 'turns/1/checks/0/execution'
    record = dict(pid=42, start_ticks=10, boot_id='boot')
    serial.write(execution / 'process.json', record)
    monkeypatch.setattr(serial, 'alive', lambda _: False)
    with pytest.raises(RuntimeError, match='stop and reap'):
        serial.require_idle(runtime)
    serial.write(execution / 'cleanup.json', record)
    serial.require_idle(runtime)
    monkeypatch.setattr(serial, 'alive', lambda _: True)
    with pytest.raises(RuntimeError, match='stop and reap'):
        serial.require_idle(runtime)


def test_serial_compiler_capability_uses_canonical_repo_and_owned_drain(tmp_path, monkeypatch):
    from trellis.dispatch import resources
    config, runtime, directory, request, attempt = fixture(tmp_path)
    serial.write(runtime / 'runtime_metadata.json', dict(repo_path=str(config.repo_path)))
    execution = directory / 'turns/1/execution'
    execution.mkdir(parents=True)
    issued, revoked, drained = [], [], []
    def grant(actual_runtime, repo, **kwargs):
        issued.append((actual_runtime, repo, kwargs))
        return dict(TRELLIS_CHECK_JOBS_TOKEN='owned', TRELLIS_CHECK_JOBS_SOCKET='socket')
    monkeypatch.setattr(resources.jobs, 'register_workspace', grant)
    monkeypatch.setattr(resources.jobs, 'revoke_workspace', lambda runtime, token: revoked.append(token))
    monkeypatch.setattr(resources.workspaces, 'register', lambda *_args, **_kwargs: pytest.fail('serial must not register a second authority workspace'))
    spec = dict(serial=True, runtime=str(runtime), repo=str(config.repo_path), task_runtime=str(directory / 'runtime'),
                compiler='managed', scope='serial-worker', lifetime=None)
    channels = resources.grant(execution, spec)
    assert issued == [(runtime, config.repo_path, dict(scope='serial-worker', lifetime=None))]
    assert channels['TRELLIS_CHECK_JOBS_TOKEN'] == 'owned'
    def advisory(payload, **kwargs):
        drained.append((payload, kwargs))
        return dict(drained=True)
    resources.drain(execution, advisory=advisory,
                    authority=lambda *_: pytest.fail('serial must preserve the canonical authority service'))
    assert revoked == ['owned']
    assert drained[0][0] == dict(action='release')
    assert drained[0][1]['token'] == 'owned'
    with pytest.raises(RuntimeError, match='already issued'):
        resources.grant(execution, spec)
    with pytest.raises(ValueError, match='canonical repository'):
        resources.grant(execution, dict(spec, repo=str(tmp_path / 'other')))

# The stopped fixture contains actual shared receipt/clock shapes. These tests
# execute the serial consumer while replacing only its launcher with a local fake.
from test_worker_transport import stopped


def serial_transport_fixture(tmp_path, stopped, *, ordinal=0):
    import shutil
    from trellis.dispatch import worker_transport
    old, task, response = stopped
    runtime=tmp_path/'serial-runtime';directory=runtime/'worker-attempts/owned'
    submission=directory/'turns/1'
    execution=worker_transport.execution_directory(submission,ordinal)
    shutil.copytree(old,execution)
    recipe=serial.read(execution/'recipe.json')
    recipe['payload']['worker_turn']['execution_record']=str(execution/'clock')
    serial.write(execution/'recipe.json',recipe)
    request=dict(recipe['payload']['request'],kind='Worker',worker_submission_policy=POLICY)
    attempt=dict(task['worker_attempt'],policy=dict(POLICY),contract_sha256='a'*64,
                 phase=dict(phase='running',process=task['process']),infrastructure_retries=ordinal)
    if ordinal:attempt['transport_retries']=ordinal
    serial.write(runtime/'worker-attempts/current.json',dict(directory='owned',request_id=request['id'],contract_sha256=attempt['contract_sha256']))
    repo=tmp_path/'serial-repo';repo.mkdir()
    config=SimpleNamespace(repo_path=repo,source_path=tmp_path/'serial-config.json')
    return config,runtime,directory,execution,request,attempt,response


@pytest.mark.parametrize('stopped',['codex'],indirect=True)
@pytest.mark.parametrize('cap',[0,2])
@pytest.mark.parametrize('change',['none','cleanup','clock','missing','live'])
def test_serial_closed_transport_exhaustion_is_a_fixed_hold_without_terminal(tmp_path,stopped,monkeypatch,cap,change):
    import copy
    from trellis.dispatch import worker_transport
    config,runtime,directory,execution,request,attempt,response=serial_transport_fixture(tmp_path,stopped,ordinal=cap)
    attempt['policy']['max_infrastructure_retries']=cap
    monkeypatch.setattr(serial,'_wait',lambda *a,**kw:(dict(returncode=0),response))
    monkeypatch.setattr(serial.AttemptProcess,'stop_and_reap',lambda *a:None)
    result=step(config,runtime,request,attempt)
    interrupted=result['event']['observation']['interruption']
    assert result['event']['observation']['kind']=='interrupted'
    finished=dict(attempt,phase=dict(phase='finished'),consumed_ms=interrupted['elapsed_ms'],session=interrupted['session'],last_interruption=interrupted)
    if change=='cleanup':serial.write(execution/'cleanup.json',dict(interrupted['process'],pid=999))
    elif change=='clock':
        changed=serial.read(execution/'clock/finished.json');changed['elapsed_ms']+=1;serial.write(execution/'clock/finished.json',changed)
    elif change=='missing':(execution/'cleanup.json').unlink()
    elif change=='live':monkeypatch.setattr(worker_transport,'alive',lambda p:True)
    before={p.relative_to(runtime).as_posix():p.read_bytes() for p in runtime.rglob('*') if p.is_file()}
    with pytest.raises((RuntimeError,FileNotFoundError)) as refusal:step(config,runtime,request,finished)
    assert ('retry allowance exhausted' in str(refusal.value))==(change=='none')
    assert not (directory/'terminal.json').exists() and not (execution.parent/'candidate').exists()
    assert before=={p.relative_to(runtime).as_posix():p.read_bytes() for p in runtime.rglob('*') if p.is_file()}
    serial.write(runtime/'protocol_state.json',dict(in_flight_request=request,worker_attempt=dict(attempt=finished)))
    serial.resume(runtime)
    assert not list(directory.rglob('*resume.json'))


@pytest.mark.parametrize('stopped',['codex'],indirect=True)
@pytest.mark.parametrize('pause_at',['none','before_wait','during_wait','before_allocation','after_recipe'])
def test_serial_transport_delay_and_permit_allocate_at_most_one_launcher(tmp_path,stopped,monkeypatch,pause_at):
    from trellis.dispatch import worker_transport
    from trellis.dispatch.preparation_capacity import PreparationDeferred
    from trellis.checker import jobs
    config,runtime,directory,execution,request,attempt,response=serial_transport_fixture(tmp_path,stopped)
    interrupted=worker_transport.interruption(execution,dict(process=attempt['phase']['process'],worker_attempt=attempt),response)
    attempt=dict(attempt,phase=dict(phase='transport_ready'),consumed_ms=interrupted['elapsed_ms'],session=interrupted['session'],last_interruption=interrupted,transport_retries=1,infrastructure_retries=1)
    serial.write(directory/'profile.json',dict(provider=dict(provider='codex',model='sol',effort='xhigh')))
    allocations=[];permits=[];waits=[]
    class Launcher:
        def __init__(self,path):
            self.root=path;path.mkdir(parents=True);allocations.append(path)
        def start_or_adopt(self,argv,payload,**kw):
            serial.write(self.root/'recipe.json',dict(argv=argv,payload=payload))
            if pause_at=='after_recipe':serial.write(runtime/'pause_request.json',{})
            return dict(pid=100,start_ticks=10,boot_id='boot')
        def permit(self,record):permits.append(record);serial.write(self.root/'permit.json',record)
    monkeypatch.setattr(serial,'AttemptProcess',Launcher)
    monkeypatch.setattr(jobs,'enabled',lambda:True)
    monkeypatch.setattr(serial,'_wait',lambda *a,**kw:dict(started_at_ms=99))
    original_ready=worker_transport.ready
    def ready(status,**kw):
        if pause_at=='before_allocation' and len(waits):serial.write(runtime/'pause_request.json',{})
        return original_ready(status,now_ms=0,**kw)
    monkeypatch.setattr(worker_transport,'ready',ready)
    def wait(_):
        assert not allocations and not permits
        waits.append(True)
        if pause_at=='during_wait':serial.write(runtime/'pause_request.json',{})
        else:worker_transport.resume(execution.parent,attempt,request['id'])
    monkeypatch.setattr(serial.time,'sleep',wait)
    if pause_at=='before_wait':serial.write(runtime/'pause_request.json',{})
    if pause_at=='none':
        result=step(config,runtime,request,attempt)
        assert result['event']['observation']['kind']=='launched' and len(allocations)==len(permits)==1 and len(waits)==1
    else:
        with pytest.raises(PreparationDeferred):step(config,runtime,request,attempt)
        assert len(allocations)==int(pause_at=='after_recipe') and not permits
