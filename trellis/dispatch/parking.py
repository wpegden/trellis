"""Task-local waiting for authenticated, drained host infrastructure recovery.

This retains Checking and its source reservation. It grants no retry, proof
fact or verdict, and never treats an arbitrary exception as task-local.
"""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
import stat

from . import acceptance_receipts as receipts, candidate, staged_acceptance as stages, staged_driver
from .publication import atomic

MARKER = 'host-check-blocked.json'


class HostCheckBlocked(RuntimeError):
    def __init__(self, record):
        self.record = record
        super().__init__('Host check blocked: ' + record['reason'])


def _read(path):
    data = receipts.regular(path)
    if len(data) > 1 << 20:
        raise candidate.CandidateError('host parking marker exceeds its bound')
    return json.loads(data)


def _implementation(driver):
    # Deployed implementations are immutable for a coordinator lifetime;
    # ordinary deployment restarts it. Do not hash the source tree every tick.
    if not hasattr(driver, '_parking_implementation'):
        driver._parking_implementation = receipts.implementation_identity()
    return driver._parking_implementation


def _authentication(directory):
    directory, owned, _task, stage = stages._layout(directory)
    if stage == 'owned':
        _capture, receipt, result = staged_driver._owned_capture_completion(directory)
    else:
        receipt, result = staged_driver._authenticate_finalization_retry(directory)
        key = staged_driver._read(owned / 'canonical.json')['key']
        owner = stages.reusable(owned, key)
        if not owner or owner.get('status') != 'facts' or owner.get('facts', {}).get('owner_ok') is not True:
            raise candidate.CandidateError('parked finalization lacks its original successful owner facts')
    if receipt != staged_driver._read(directory / 'staged-completed.json'):
        raise candidate.CandidateError('parked completion receipt changed')
    if not staged_driver._sealed_authored_inputs_match(directory, staged_driver._read(directory / 'canonical.json'), result):
        raise candidate.CandidateError('parked source or host input binding changed')
    return receipt, result


def block(driver, task, directory, reason, event):
    """Only the exhausted retry boundary calls this; caller holds native lock."""
    directory, owned, task_root, _stage = stages._layout(directory)
    identity = task['status'].get('candidate_sha256')
    if (task_root != driver.root / str(task['id'])
            or task['status'] != dict(status='checking', candidate_sha256=identity)
            or task['worker_attempt'].get('phase') != dict(phase='checking', candidate_sha256=identity)
            or event != dict(kind='check_retry', task_id=task['id'], candidate_sha256=identity,
                             retry=task['worker_attempt']['infrastructure_retries'] + 1)):
        raise candidate.CandidateError('parked failure differs from its current Checking attempt')
    receipt, _result = _authentication(directory)  # Unknown/untrusted failure is global.
    binding = candidate.digest(candidate.canonical_json(dict(assignment=task['assignment'], binding=task.get('binding'))))
    if (receipt['key']['candidate_sha256'] != identity
            or receipt['key'].get('task_binding_sha256') != binding
            or receipt['key'].get('submission_turn') != task['worker_attempt']['turn']):
        raise candidate.CandidateError('parked completion belongs to another candidate or task binding')
    from .recovery import _read as read_channel
    from .process import AttemptProcess
    from trellis.checker.jobs import socket_path
    channels = read_channel(directory / 'execution/compiler-channels.json')
    env = channels.get('environment', {})
    if (channels.get('runtime') != str(driver.runtime)
            or env.get('TRELLIS_CHECKER_SOCKET') != str(driver.runtime / 'sockets/checker.sock')
            or env.get('TRELLIS_CHECK_JOBS_SOCKET') != str(socket_path(driver.runtime))
            or any(not isinstance(env.get(k), str) or not env[k] for k in
                   ('TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_TOKEN'))):
        raise candidate.CandidateError('parked compiler channels have no exact runtime binding')
    AttemptProcess(directory / 'execution').stop_and_reap(staged_driver._read(directory / 'execution/process.json'))
    if _authentication(directory)[0] != receipt:
        raise candidate.CandidateError('parked completion changed during compiler drain')
    bindings = {}
    for stage_dir in {directory, owned}:
        completed = staged_driver._read(stage_dir / 'staged-completed.json')
        for name, digest in completed['files'].items():
            path = stage_dir / name
            bindings[str(path.relative_to(driver.root))] = digest
        for name in ('staged-completed.json', 'execution/compiler-channels.json'):
            path = stage_dir / name
            if path.exists():
                bindings[str(path.relative_to(driver.root))] = receipts.regular_digest(path)
    failure = directory / 'infrastructure-failure.json'
    bindings[str(failure.relative_to(driver.root))] = receipts.regular_digest(failure)
    record = dict(version=1, task_id=task['id'], candidate_sha256=identity,
        evidence=str(directory.relative_to(driver.root)), reason=str(reason)[:2000],
        completion_sha256=receipts.regular_digest(directory / 'staged-completed.json'),
        bindings=bindings, implementation_identity=_implementation(driver))
    raise HostCheckBlocked(record)


def retire_replaced(root, task):
    path = Path(root) / str(task['id']) / MARKER
    if not path.exists():
        return
    record = _read(path)
    if record.get('state') == 'parked' and task['status'] != dict(
            status='checking', candidate_sha256=record.get('candidate_sha256')):
        record.update(state='retired', retired_by_native_status=task['status'])
        atomic(path, candidate.canonical_json(record))


def summary(root, task):
    if task['status'].get('status') != 'checking':
        return None
    path = Path(root) / str(task['id']) / MARKER
    if not path.exists():
        return None
    record = _read(path)
    if record.get('state') != 'parked' or record.get('candidate_sha256') != task['status'].get('candidate_sha256'):
        return None
    return dict(summary='Host check blocked: ' + record['reason'], reason=record['reason'],
                evidence=record['evidence'], slot_retained=True)


def _key(driver, task, record):
    if record.get('kind') == 'capacity_blocked':
        from .capacity_parking import wake_key
        return wake_key(driver)
    retry = driver.root / record['evidence'] / 'retry-authorized.json'
    return dict(implementation_identity=_implementation(driver),
        grant=task.get('worker_attempt', {}).get('infrastructure_recovery_grant'),
        retry_authorization_sha256=receipts.regular_digest(retry) if retry.exists() else None)


def _signature(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise candidate.CandidateError('parked evidence is no longer an independent regular file')
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _bindings(driver, record, previous=None):
    result = {}
    for relative, digest in record['bindings'].items():
        path = driver.root / candidate.safe_relative(relative)
        signature = _signature(path)
        if previous is None or previous.get(relative) != signature:
            if receipts.regular_digest(path) != digest:
                raise candidate.CandidateError('parked immutable evidence changed: ' + relative)
        result[relative] = signature
    return result


def persist(driver, blocked, admission):
    record = dict(blocked.record, state='parked', admission_sha256=candidate.digest(candidate.canonical_json(admission)))
    task = driver.state()['dispatch']['tasks'][str(record['task_id'])]
    record['last_wake_key'] = (dict(policy_generation=record['failed_policy_generation'])
        if record.get('kind') == 'capacity_blocked' else _key(driver, task, record))
    path = driver.root / str(record['task_id']) / MARKER
    atomic(path, candidate.canonical_json(record))
    driver._notify()
    return record


def load(driver, task_id, identity, admission):
    path = driver.root / str(task_id) / MARKER
    if not path.exists():
        return None
    record = _read(path)
    if record.get('state') != 'parked':
        return None
    if (record.get('version') != 1 or record.get('task_id') != task_id
            or record.get('candidate_sha256') != identity
            or record.get('admission_sha256') != candidate.digest(candidate.canonical_json(admission))):
        raise candidate.CandidateError('parked task and retained semantic admission differ')
    directory = driver.root / candidate.safe_relative(record['evidence'])
    if stages._layout(directory)[2] != driver.root / str(task_id):
        raise candidate.CandidateError('parked evidence belongs to another task')
    _bindings(driver, record)
    if record.get('kind') == 'capacity_blocked':
        from .capacity_parking import authenticate_record
        authenticate_record(driver, record)
    else:
        _authentication(directory)
    return record


def _poll(driver, record, signatures):
    """Small current status/receipt metadata first; full auth only on key change."""
    with driver.native.locked():
        driver.native._require_recovered_publication()
        driver.require_no_halt()
        tasks = getattr(driver, '_host_park_tasks', None)
        task = (tasks if tasks is not None else driver.state()['dispatch']['tasks'])[str(record['task_id'])]
        if task['status'] != dict(status='checking', candidate_sha256=record['candidate_sha256']):
            return 'retired', signatures
        if _read(driver.root / str(record['task_id']) / MARKER) != record:
            raise candidate.CandidateError('parked host marker changed outside recovery')
        if receipts.regular_digest(driver.root / str(record['task_id']) / 'acceptance-admission.json') != record['admission_sha256']:
            raise candidate.CandidateError('parked semantic admission changed')
        signatures = _bindings(driver, record, signatures)
        if record.get('kind') == 'capacity_blocked':
            # Retained releases parked memory misses pending a policy change.
            # Authenticate that old evidence, then use today's normal negative
            # delivery/requeue. No grant or native retry counter is involved.
            from . import resource_outcome
            from .capacity_parking import authenticate_record
            authenticate_record(driver, record)
            state = driver.state()
            task = state['dispatch']['tasks'][str(record['task_id'])]
            outcome = resource_outcome.handle(driver, state, task,
                driver.root / candidate.safe_relative(record['evidence']))
            record['state'] = 'retired' if outcome.get('rejected') else 'resuming'
            atomic(driver.root / str(record['task_id']) / MARKER, candidate.canonical_json(record))
            return ('retired' if outcome.get('rejected') else 'resume'), signatures
        key = _key(driver, task, record)
        if key == record['last_wake_key']:
            return 'waiting', signatures
        directory = driver.root / record['evidence']
        _authentication(directory)  # Corruption remains global, never a rejected wake.
        grant = key['grant']
        attempt = task['worker_attempt']
        eligible = False
        if grant is not None:
            _directory, owned, _task, stage = stages._layout(directory)
            expected = dict(candidate_sha256=record['candidate_sha256'],
                retry=attempt['policy']['max_infrastructure_retries'] + 1,
                acceptance_round=int(owned.name), finalization=int(directory.name),
                completion_sha256=record['completion_sha256'])
            if stage == 'finalize' and grant == expected:
                eligible = attempt['infrastructure_retries'] < grant['retry']
                if not eligible and key['retry_authorization_sha256'] is not None:
                    marker = staged_driver._read(directory / 'retry-authorized.json')
                    eligible = (attempt['infrastructure_retries'] == grant['retry']
                        and marker.get('kind') == 'bounded_infrastructure_retry'
                        and marker.get('event') == dict(kind='check_retry', task_id=task['id'],
                            candidate_sha256=record['candidate_sha256'], retry=grant['retry'],
                            recovery_completion_sha256=record['completion_sha256']))
        record['last_wake_key'] = key  # Persist rejected changes; no repeated authentication.
        if eligible:
            record['state'] = 'resuming'
        atomic(driver.root / str(record['task_id']) / MARKER, candidate.canonical_json(record))
        return ('resume' if eligible else 'waiting'), signatures


async def wait(driver, record):
    from .driver import _run_blocking
    if not hasattr(driver, '_host_park_waiters'):
        driver._host_park_waiters = {}
    pulse = asyncio.Event()
    driver._host_park_waiters[record['task_id']] = pulse
    try:
        signatures = await _run_blocking(_bindings, driver, record)
        while True:
            action, signatures = await _run_blocking(_poll, driver, record, signatures)
            if action != 'waiting':
                if action == 'retired':
                    record['state'] = 'retired'
                    await _run_blocking(atomic, driver.root / str(record['task_id']) / MARKER,
                                       candidate.canonical_json(record))
                return action
            await pulse.wait()  # Existing coordinator snapshot/notification cadence.
            pulse.clear()
    finally:
        driver._host_park_waiters.pop(record['task_id'], None)
