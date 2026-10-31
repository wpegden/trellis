"""Authenticate stopped worker transport before a native continuation decision."""
import json
import os
from pathlib import Path
import time

from trellis.agent_wrapper import provider_clock
from trellis.agent_wrapper.continuation import require_session
from .candidate import canonical_json, digest
from .process import alive


def execution_directory(submission, ordinal):
    return submission / 'transports' / str(ordinal) / 'execution' if ordinal else submission / 'execution'


def retry_delay_seconds(ordinal):
    if type(ordinal) is not int or ordinal < 1:
        raise RuntimeError('transport delay requires a positive native retry ordinal')
    return min(120 * 2 ** min(ordinal - 1, 3), 900)


def interruption(execution, task, response):
    """No writable transcript/raw field can authorize another provider launch."""
    return _interruption(execution, task, response)


def historical_interruption(execution, task, response, successor_admission):
    """Verify consumed evidence against its actual executed successor."""
    return _interruption(execution, task, response, successor_admission=successor_admission)


def _interruption(execution, task, response, *, successor_admission=None):
    execution = Path(execution)
    def read(name):
        return json.loads((execution / name).read_bytes())
    attempt = task['worker_attempt']
    process = read('process.json')
    if (process != task['process'] or read('permit.json') != process
            or read('cleanup.json') != process or alive(process)):
        raise RuntimeError('transport continuation requires exact stopped launcher cleanup')
    result = read('result.json')
    data = (execution / 'response.json').read_bytes()
    if result['returncode'] != 0 or result['response_sha256'] != digest(data) or json.loads(data) != response:
        raise RuntimeError('transport continuation response differs from its launch receipt')
    recipe = read('recipe.json')
    engineering = recipe['payload']['request'].get('kind') == 'Engineering'
    turn = recipe['payload']['worker_turn']
    clock = execution / 'clock'
    timing = provider_clock.read(clock, 'finished')
    exited = provider_clock.read(clock, 'exited')
    session_receipt = provider_clock.read(clock, 'session')
    config = read('clock/config.json')
    if (response.get('kind') != 'worker_submission' or response.get('turn') != attempt['turn']
            or response.get('request_id') != recipe['payload']['request']['id']
            or response.get('cycle') != recipe['payload']['request']['cycle']
            or response.get('provider_turn_completed') is not False
            or response.get('provider_execution') != timing or not timing or not exited
            or timing['disposition'] != 'interrupted'
            or timing.get('detail') != 'Provider exited without a completed turn'
            or timing.get('provider_budget') or exited['owner'] != timing['owner'] or alive(timing['owner'])
            or engineering and timing.get('provider_error') != 'server_overloaded'
            or not session_receipt or session_receipt['owner'] != timing['owner']
            or session_receipt.get('thread_id') != timing.get('thread_id') or not timing.get('thread_id')
            or turn['turn'] != attempt['turn'] or config['allowance_ms'] != turn['remaining_ms']
            or config['allowance_ms'] != attempt['allowance_ms'] - attempt['consumed_ms']
            or Path(turn['execution_record']).resolve() != clock.resolve()):
        raise RuntimeError('transport continuation lacks an authenticated stopped provider clock')
    session = dict(config['binding'], thread_id=timing['thread_id'])
    if attempt.get('session') is not None and attempt['session'] != session:
        raise RuntimeError('transport interruption changed its original conversation')
    require_session(Path(turn.get('session_runtime', recipe['payload']['runtime_root'])) / 'worker-session.json', session)
    # Hash original host evidence, including immutable recipe and all clock segments.
    pins = {name: digest((execution / name).read_bytes()) for name in
            ('recipe.json', 'process.json', 'permit.json', 'cleanup.json', 'result.json', 'response.json')}
    for directory in provider_clock._segments(clock):
        for name in ('config', 'owner', 'started', 'session', 'finished', 'exited'):
            path = directory / (name + '.json')
            pins[str(path.relative_to(execution))] = digest(path.read_bytes())
    if engineering:
        from trellis.agent_wrapper.protocol import SingleAgentRequest
        from trellis.agents import engineering_context
        saved = read('engineering-request.json')
        single = SingleAgentRequest.from_dict(saved['request'])
        if (saved['submission'] != turn or turn['engineering_request'] != recipe['payload']['request']
                or turn['engineering_attempt'] != dict(attempt, phase=turn['engineering_attempt']['phase'])):
            raise RuntimeError('engineering interruption differs from its original native turn')
        ordinal = turn.get('transport_ordinal', 0) + 1
        if successor_admission is None:
            retained, files = engineering_context.retained_interruption(single, turn, ordinal)
        else:
            retained, files = engineering_context.historical_interruption(single, turn, ordinal, successor_admission)
        if retained['session'] != session:
            raise RuntimeError('engineering interruption changed its retained context')
        pins.update((str(path), digest(path.read_bytes())) for path in files)
    return dict(process=process, turn=attempt['turn'], elapsed_ms=timing['elapsed_ms'],
                session=session, evidence_sha256=digest(canonical_json(pins)))


def wait_status(submission, attempt, request_id):
    """Read-only deadline derived from the exact stopped execution, never now."""
    if attempt['phase']['phase'] != 'transport_ready':
        return None
    ordinal = attempt['transport_retries']
    if os.path.lexists(execution_directory(Path(submission), ordinal)):
        raise RuntimeError('transport continuation already has newer execution evidence')
    previous = execution_directory(Path(submission), ordinal - 1)
    response = json.loads((previous / 'response.json').read_bytes())
    recipe = json.loads((previous / 'recipe.json').read_bytes())
    turn = recipe['payload']['worker_turn']
    if (recipe['payload']['request']['id'] != request_id or turn['turn'] != attempt['turn']
            or turn.get('transport_ordinal', 0) + 1 != ordinal):
        raise RuntimeError('transport wait differs from its original request or ordinal')
    original_attempt = dict(attempt, consumed_ms=attempt['consumed_ms'] - attempt['last_interruption']['elapsed_ms'])
    if recipe['payload']['request'].get('kind') == 'Engineering':
        original_attempt = recipe['payload']['worker_turn']['engineering_attempt']
        if (any(original_attempt[key] != attempt[key] for key in ('turn', 'contract_sha256', 'allowance_ms', 'policy'))
                or original_attempt['consumed_ms'] + attempt['last_interruption']['elapsed_ms'] != attempt['consumed_ms']):
            raise RuntimeError('engineering transport wait changed its original allowance')
    stopped = interruption(previous, dict(process=attempt['last_interruption']['process'],
        worker_attempt=original_attempt), response)
    if stopped != attempt['last_interruption']:
        raise RuntimeError('transport wait differs from native interruption')
    timing = provider_clock.read(previous / 'clock', 'finished')
    delay_seconds = retry_delay_seconds(ordinal)
    deadline = timing['started_at_ms'] + timing['elapsed_ms'] + delay_seconds * 1000
    value = dict(request_id=request_id, turn=attempt['turn'], transport_ordinal=ordinal,
        evidence_sha256=stopped['evidence_sha256'], next_retry_at_ms=deadline,
        remaining_ms=attempt['allowance_ms'] - attempt['consumed_ms'])
    path = previous / 'transport-wait.json'
    if path.exists() and json.loads(path.read_bytes()) != value:
        raise RuntimeError('transport wait changed its retained deadline or evidence')
    resumed = previous / 'transport-resume.json'
    if resumed.exists() and json.loads(resumed.read_bytes()) != value:
        raise RuntimeError('transport resume permit belongs to another interruption')
    return dict(value, provider_error=timing.get('provider_error'), manual_resume_allowed=True,
        manual_permit=resumed.exists(), delay_seconds=delay_seconds)


def wait_record(status):
    return {key: status[key] for key in ('request_id', 'turn', 'transport_ordinal',
        'evidence_sha256', 'next_retry_at_ms', 'remaining_ms')}


def prepare_wait(submission, attempt, request_id):
    status = wait_status(submission, attempt, request_id)
    if status is None:
        raise RuntimeError('transport delay requires a native continuation')
    previous = execution_directory(Path(submission), status['transport_ordinal'] - 1)
    from .publication import atomic
    path = previous / 'transport-wait.json'
    if not path.exists():
        atomic(path, canonical_json(wait_record(status)))
    return status


def resume(submission, attempt, request_id, *, expected=None):
    status = wait_status(submission, attempt, request_id)
    if status is None:
        raise RuntimeError('transport resume requires a native continuation')
    if expected is not None and expected != {key: public_status(status)[key]
            for key in ('request_id','turn','ordinal','evidence_sha256')}:
        raise RuntimeError('transport resume belongs to a stale interruption')
    previous = execution_directory(Path(submission), status['transport_ordinal'] - 1)
    from .publication import atomic
    path = previous / 'transport-resume.json'
    if not path.exists():
        atomic(path, canonical_json(wait_record(status)))
    return status


def public_status(status, *, resume_allowed=True):
    return dict(code='server_overloaded' if status.get('provider_error') == 'server_overloaded' else 'transport_interrupted',
        request_id=status['request_id'], turn=status['turn'], ordinal=status['transport_ordinal'],
        evidence_sha256=status['evidence_sha256'], next_retry_at_ms=status['next_retry_at_ms'],
        remaining_ms=status['remaining_ms'], resume_allowed=resume_allowed)


def serial_status(runtime, state):
    from trellis.agent_wrapper.serial_worker import _root
    record = state.get('worker_attempt')
    if record is None or record['attempt']['phase']['phase'] != 'transport_ready':
        return None
    request = state['in_flight_request']; attempt = record['attempt']
    directory = _root(Path(runtime), request, attempt)
    return wait_status(directory / 'turns' / str(attempt['turn']), attempt, request['id'])


def parallel_status(root, task):
    attempt = task.get('worker_attempt')
    if (attempt is None or attempt['phase']['phase'] != 'transport_ready'
            or task['status']['status'] != 'reserved' or task.get('process') is not None):
        return None
    submission = Path(root) / str(task['id']) / 'turns' / str(attempt['turn'])
    previous = execution_directory(submission, attempt['transport_retries'] - 1)
    recipe = json.loads((previous / 'recipe.json').read_bytes())
    return wait_status(submission, attempt, recipe['payload']['request']['id'])


def ready(status, *, now_ms=None, permit_path=None):
    manual = status['manual_permit']
    if permit_path is not None and Path(permit_path).exists():
        if json.loads(Path(permit_path).read_bytes()) != wait_record(status):
            raise RuntimeError('transport resume permit belongs to another interruption')
        manual = True
    return manual or (int(time.time() * 1000) if now_ms is None else now_ms) >= status['next_retry_at_ms']
