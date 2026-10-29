"""Authenticate stopped worker transport before a native continuation decision."""
import json
from pathlib import Path

from trellis.agent_wrapper import provider_clock
from trellis.agent_wrapper.continuation import require_session
from .candidate import canonical_json, digest
from .process import alive


def execution_directory(submission, ordinal):
    return submission / 'transports' / str(ordinal) / 'execution' if ordinal else submission / 'execution'


def interruption(execution, task, response):
    """No writable transcript/raw field can authorize another provider launch."""
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
            or not session_receipt or session_receipt['owner'] != timing['owner']
            or session_receipt.get('thread_id') != timing.get('thread_id') or not timing.get('thread_id')
            or turn['turn'] != attempt['turn'] or config['allowance_ms'] != turn['remaining_ms']
            or config['allowance_ms'] != attempt['allowance_ms'] - attempt['consumed_ms']
            or Path(turn['execution_record']).resolve() != clock.resolve()):
        raise RuntimeError('transport continuation lacks an authenticated stopped provider clock')
    session = dict(config['binding'], thread_id=timing['thread_id'])
    require_session(Path(turn.get('session_runtime', recipe['payload']['runtime_root'])) / 'worker-session.json', session)
    # Hash original host evidence, including immutable recipe and all clock segments.
    pins = {name: digest((execution / name).read_bytes()) for name in
            ('recipe.json', 'process.json', 'permit.json', 'cleanup.json', 'result.json', 'response.json')}
    for directory in provider_clock._segments(clock):
        for name in ('config', 'owner', 'started', 'session', 'finished', 'exited'):
            path = directory / (name + '.json')
            pins[str(path.relative_to(execution))] = digest(path.read_bytes())
    return dict(process=process, turn=attempt['turn'], elapsed_ms=timing['elapsed_ms'],
                session=session, evidence_sha256=digest(canonical_json(pins)))
