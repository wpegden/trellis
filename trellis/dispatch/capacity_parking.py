"""Retain exhausted host checks without spending mathematical retry authority."""
from __future__ import annotations

import json
from pathlib import Path

from trellis.checker import resource_failure
from . import acceptance_receipts as receipts, candidate, staged_acceptance as stages

KIND = 'capacity_blocked'


def policy(driver):
    path = driver.runtime / 'checker-state/memory-policy.json'
    value = json.loads(receipts.regular(path))
    generation = value.get('generation')
    if (value.get('version') != 1 or value.get('runtime_root') != str(driver.runtime.resolve())
            or not isinstance(generation, str) or not 0 < len(generation) <= 128):
        raise candidate.CandidateError('checker memory policy has no current runtime binding')
    return value


def wake_key(driver):
    value = policy(driver)
    return dict(policy_generation=value['generation'])


def authenticate(directory):
    directory = Path(directory)
    captured = json.loads(receipts.regular(directory / 'canonical.json'))
    marker = resource_failure.capacity_record(directory / 'runtime')
    if marker is None or (directory / 'runtime/checker-state/resource-failure.json').exists():
        raise candidate.CandidateError('capacity stop lacks an exclusive host memory cause')
    if 'staged_plan' in captured or directory.parent.name == 'finalization':
        raw, values = stages.authenticate_execution(directory, captured['key'])
    else:
        raw, values = receipts._authenticate_execution(directory, captured['key'])
    if values['execution/result.json']['returncode'] == 0:
        raise candidate.CandidateError('a successful host completion cannot be parked as capacity failure')
    if any((directory / name).exists() for name in ('completed.json', 'staged-completed.json')):
        raise candidate.CandidateError('capacity failure cannot replace completed proof evidence')
    return captured, marker, raw, values


def evidence(driver, task, directory, *, current_producer=True):
    """Authenticate a drained memory outcome without publishing any proof."""
    from . import parking
    from .process import AttemptProcess
    from .recovery import _read as read_channel
    from trellis.checker.jobs import socket_path

    directory, owned, task_root, stage = stages._layout(directory)
    captured, marker, raw, values = authenticate(directory)
    identity = task['status'].get('candidate_sha256')
    key = captured['key']
    attempt = task.get('worker_attempt') or {}
    binding = candidate.digest(candidate.canonical_json(dict(assignment=task['assignment'], binding=task.get('binding'))))
    if (task_root != driver.root / str(task['id'])
            or task['status'] != dict(status='checking', candidate_sha256=identity)
            or attempt.get('phase') != dict(phase='checking', candidate_sha256=identity)
            or key.get('candidate_sha256') != identity or key.get('task_id') != task['id']
            or key.get('task_binding_sha256') != binding or key.get('submission_turn') != attempt.get('turn')
            or current_producer and key.get('implementation_identity') != parking._implementation(driver)):
        raise candidate.CandidateError('capacity failure belongs to another task, submission or implementation')
    channels = read_channel(directory / 'execution/compiler-channels.json')
    env = channels.get('environment', {})
    if (channels.get('runtime') != str(driver.runtime)
            or env.get('TRELLIS_CHECKER_SOCKET') != str(driver.runtime / 'sockets/checker.sock')
            or env.get('TRELLIS_CHECK_JOBS_SOCKET') != str(socket_path(driver.runtime))
            or any(not isinstance(env.get(k), str) or not env[k] for k in
                   ('TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_TOKEN'))):
        raise candidate.CandidateError('capacity stop has no exact compiler-channel binding')
    AttemptProcess(directory / 'execution').stop_and_reap(values['execution/process.json'])
    if authenticate(directory)[:2] != (captured, marker):
        raise candidate.CandidateError('capacity evidence changed during compiler cleanup')
    bindings = {str((directory / name).relative_to(driver.root)): candidate.digest(data) for name, data in raw.items()}
    marker_path = (resource_failure.MEMORY_MARKER if
        (directory / 'runtime' / resource_failure.MEMORY_MARKER).exists() else resource_failure.CAPACITY_MARKER)
    for name in ('runtime/' + marker_path, 'execution/compiler-channels.json'):
        bindings[str((directory / name).relative_to(driver.root))] = receipts.regular_digest(directory / name)
    # Store exact interrupted source bytes as hashes; these confer no facts and
    # need not resemble a completed post-overlay normalization.
    sources = receipts.checked_sources(directory / 'repo')
    owner_digest = None
    if stage == 'finalize':
        owner_capture = json.loads(receipts.regular(owned / 'canonical.json'))
        if stages.reusable(owned, owner_capture['key']) is None:
            raise candidate.CandidateError('capacity-blocked finalizer lost its original owner facts')
        owner_digest = receipts.regular_digest(owned / 'staged-completed.json')
    reason = 'Host memory outcome for ' + marker['detail']['operation']
    record = dict(version=1, kind=KIND, task_id=task['id'], candidate_sha256=identity,
        evidence=str(directory.relative_to(driver.root)), reason=reason, bindings=bindings,
        implementation_identity=key['implementation_identity'], checked_sources=sources,
        owner_completion_sha256=owner_digest, failed_policy_generation=marker['detail']['policy_generation'])
    return record, marker


def block(driver, task, directory):
    """Legacy reader; current memory outcomes use automatic delivery."""
    from . import parking
    record, _marker = evidence(driver, task, directory)
    raise parking.HostCheckBlocked(record)


def authenticate_record(driver, record):
    directory = driver.root / candidate.safe_relative(record['evidence'])
    _captured, marker, _raw, _values = authenticate(directory)
    if marker['detail']['policy_generation'] != record['failed_policy_generation']:
        raise candidate.CandidateError('parked memory-policy generation changed')
    if receipts.checked_sources(directory / 'repo') != record['checked_sources']:
        raise candidate.CandidateError('capacity-blocked source evidence changed')
    _directory, owned, _task, stage = stages._layout(directory)
    if stage == 'finalize':
        captured = json.loads(receipts.regular(owned / 'canonical.json'))
        if (receipts.regular_digest(owned / 'staged-completed.json') != record['owner_completion_sha256']
                or stages.reusable(owned, captured['key']) is None):
            raise candidate.CandidateError('capacity-blocked finalizer owner facts changed')
    return marker


def authorize_recovery(driver, task, record, key):
    """Scheduling authority only; fresh preparation/normal acceptance still run."""
    from . import parking
    if key['policy_generation'] == record['failed_policy_generation']:
        return False
    if parking._implementation(driver) != record['implementation_identity']:
        return False  # This narrow recovery does not approve an implementation migration.
    directory = driver.root / candidate.safe_relative(record['evidence'])
    captured, _marker, _raw, _values = authenticate(directory)
    binding = candidate.digest(candidate.canonical_json(dict(assignment=task['assignment'], binding=task.get('binding'))))
    if (captured['key'].get('task_binding_sha256') != binding
            or captured['key'].get('submission_turn') != task['worker_attempt']['turn']):
        raise candidate.CandidateError('capacity recovery task contract changed')
    authenticate_record(driver, record)
    authorization = dict(version=1, kind='capacity_recovery',
        task_id=task['id'], candidate_sha256=record['candidate_sha256'],
        source_key_sha256=candidate.digest(candidate.canonical_json(captured['key'])),
        capacity_marker_sha256=receipts.regular_digest(directory / 'runtime' / resource_failure.CAPACITY_MARKER),
        execution_result_sha256=receipts.regular_digest(directory / 'execution/result.json'),
        policy_generation=key['policy_generation'],
        previous_generation=record['failed_policy_generation'],
        bindings=record['bindings'])
    path = directory / 'retry-authorized.json'
    if path.exists():
        previous = json.loads(receipts.regular(path))
        generation = previous.get('policy_generation')
        if (not isinstance(generation, str) or not 0 < len(generation) <= 128
                or generation == record['failed_policy_generation']
                or previous != dict(authorization, policy_generation=generation)):
            raise candidate.CandidateError('capacity recovery authorization changed')
        # A crash may leave this immutable authorization before the park marker
        # advances. A later policy change need not replace that valid authority.
    else:
        receipts.write_once(path, authorization)
    return True


def validate_recovery(directory, record, previous):
    """Read-only compatibility for an already issued, pre-upgrade retry."""
    captured, marker, _raw, _values = authenticate(directory)
    generation = previous.get('policy_generation')
    expected = dict(version=1, kind='capacity_recovery',
        task_id=record['task_id'], candidate_sha256=record['candidate_sha256'],
        source_key_sha256=candidate.digest(candidate.canonical_json(captured['key'])),
        capacity_marker_sha256=receipts.regular_digest(directory / 'runtime' / resource_failure.CAPACITY_MARKER),
        execution_result_sha256=receipts.regular_digest(directory / 'execution/result.json'),
        policy_generation=generation, previous_generation=marker['detail']['policy_generation'],
        bindings=record['bindings'])
    if (not isinstance(generation, str) or not 0 < len(generation) <= 128
            or generation == marker['detail']['policy_generation'] or previous != expected):
        raise candidate.CandidateError('capacity recovery authorization changed')
