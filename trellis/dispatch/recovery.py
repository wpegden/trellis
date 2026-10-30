"""Plan or apply one bounded recovery of a drained finalization infrastructure failure.

Run with the coordinator stopped. The default is read-only; --apply records an
operational retry grant, never a proof decision, policy reset or process launch.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import time

from . import acceptance_receipts as receipts, candidate, staged_acceptance as stages
from . import staged_driver


def _read(path):
    return json.loads(receipts.regular(path))


def _attempt_digest(attempt):
    # Native hashes serde_json::Value (sorted keys, UTF-8, no newline).
    return candidate.digest(json.dumps(attempt, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode())


def _evidence(driver, task, directory):
    """Caller holds coordinator.lock; authenticate current sources and old evidence."""
    directory, owned, task_root, stage = stages._layout(directory)
    if stage != 'finalize' or task_root != driver.root / str(task['id']):
        raise candidate.CandidateError('recovery requires this task finalization')
    # Even an incomplete later launch supersedes this failure. Do not select a
    # convenient older Cold when a newer owned/finalization execution exists.
    for row in staged_driver._directories(task_root / 'acceptance'):
        if int(row.name) > int(owned.name):
            if any((row / name).exists() for name in ('canonical.json', 'execution')):
                raise candidate.CandidateError('later acceptance preparation or execution exists')
    if any(int(row.name) > int(directory.name) for row in staged_driver._directories(owned / 'finalization')):
        raise candidate.CandidateError('later finalization preparation or execution exists')
    captured = _read(directory / 'canonical.json')
    identity = captured['key']['candidate_sha256']
    attempt = task.get('worker_attempt') or {}
    if (task['status'] != dict(status='checking', candidate_sha256=identity)
            or attempt.get('phase') != dict(phase='checking', candidate_sha256=identity)):
        raise candidate.CandidateError('recovery candidate is not currently Checking')
    receipt, final = staged_driver._authenticate_finalization_retry(directory)
    # Every executed managed launcher durably issues both channels before its
    # child starts. Missing records cannot be interpreted as a drain ACK.
    channels = _read(directory / 'execution/compiler-channels.json')
    from trellis.checker.jobs import socket_path
    env = channels.get('environment', {})
    if (channels.get('runtime') != str(driver.runtime)
            or env.get('TRELLIS_CHECKER_SOCKET') != str(driver.runtime / 'sockets/checker.sock')
            or env.get('TRELLIS_CHECK_JOBS_SOCKET') != str(socket_path(driver.runtime))
            or any(not isinstance(env.get(name), str) or not env[name] for name in
                   ('TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_TOKEN'))):
        raise candidate.CandidateError('recovery compiler channels lack their exact runtime binding')
    # authenticate() checks the exact original donor marker and owner completion.
    owner_key = _read(owned / 'canonical.json')['key']
    owner = stages.reusable(owned, owner_key)
    if not owner or owner.get('status') != 'facts' or owner.get('facts', {}).get('owner_ok') is not True:
        raise candidate.CandidateError('recovery requires successful original owned facts')
    if not staged_driver._sealed_authored_inputs_match(directory, captured, final):
        raise candidate.CandidateError('recovery authored or host inputs changed')
    # This grants permission to run a fresh current finalizer, not permission
    # to reuse its old observations. Unrelated publications/generated docs may
    # change. Only the sealed write preconditions must still merge; the fresh
    # finalizer rechecks actual imported environments, policy and consumers.
    binding = candidate.digest(candidate.canonical_json(dict(assignment=task['assignment'], binding=task.get('binding'))))
    if (captured['key'].get('task_binding_sha256') != binding
            or captured['key'].get('submission_turn') != attempt.get('turn')
            or captured['config_sha256'] != candidate.digest(receipts.regular(driver.config_path))):
        raise candidate.CandidateError('current recovery task, configuration or submission binding changed')
    record, _identity = candidate.read(stages._sealed(task_root, captured['key']))
    for path, change in record['changes'].items():
        if path in ('Tablet/INDEX.md', 'Tablet/README.md'):
            continue
        data = candidate.regular_bytes(driver.repo, path)
        if (candidate.digest(data) if data is not None else None) != change['before']:
            raise candidate.CandidateError('current recovery authored write precondition changed: ' + path)
    limit = attempt['policy']['max_infrastructure_retries']
    if type(limit) is not int or not 0 <= limit < (1 << 32) - 1:
        raise candidate.CandidateError('recovery retry ordinal is out of range')
    grant = dict(candidate_sha256=identity, retry=limit + 1,
        acceptance_round=int(owned.name), finalization=int(directory.name),
        completion_sha256=candidate.digest(candidate.canonical_json(receipt)))
    failure_path = directory / 'infrastructure-failure.json'
    # An older coordinator may stop before adopting the completion or reaching
    # _retry_check. The authenticated Cold is evidence; an absent diagnostic is
    # not invented here. Normal resumed retry writes its original event first.
    if failure_path.exists():
        failure = _read(failure_path)['event']
        if failure != dict(kind='check_retry', task_id=task['id'], candidate_sha256=identity, retry=grant['retry']):
            raise candidate.CandidateError('recovery does not match original exhausted retry event')
    return grant, receipt, final


def plan(driver, task_id, acceptance_round, finalization):
    """Read-only stopped-owner preflight. Native apply owns its own locks later."""
    if any(type(n) is not int or n < 1 for n in (task_id, acceptance_round, finalization)):
        raise ValueError('task, acceptance round and finalization must be positive integers')
    with driver.native.own_coordinator(), driver.native.locked():
        driver.require_no_halt()
        driver.native._require_recovered_publication()
        task = driver.state()['dispatch']['tasks'][str(task_id)]
        directory = driver.root / str(task_id) / 'acceptance' / str(acceptance_round) / 'finalization' / str(finalization)
        grant, receipt, _final = _evidence(driver, task, directory)
        attempt = task['worker_attempt']
        existing = attempt.get('infrastructure_recovery_grant')
        if existing is not None and existing != grant:
            raise candidate.CandidateError('this attempt already has a different recovery grant')
        expected_counters = (grant['retry'] - 1, grant['retry']) if existing else (grant['retry'] - 1,)
        if attempt['infrastructure_retries'] not in expected_counters:
            raise candidate.CandidateError('recovery requires exactly the exhausted frozen allowance')
        return dict(status='already_granted' if existing else 'planned', task_id=task_id,
            grant=grant, attempt_sha256=_attempt_digest(attempt),
            completion_adoption_required=not (directory / 'staged-completed.json').exists(),
            drain_ack_required=(directory / 'execution/compiler-channels.json').exists(),
            evidence=str(directory))


def apply(driver, task_id, acceptance_round, finalization):
    expected = plan(driver, task_id, acceptance_round, finalization)
    # Adopt exactly the ordinary completed/reaped output; no new checker work.
    # Revalidate under stopped ownership before writing the optional receipt.
    with driver.native.own_coordinator(), driver.native.locked():
        driver.require_no_halt()
        driver.native._require_recovered_publication()
        task = driver.state()['dispatch']['tasks'][str(task_id)]
        if _attempt_digest(task['worker_attempt']) != expected['attempt_sha256']:
            raise candidate.CandidateError('attempt changed after recovery planning')
        directory = Path(expected['evidence'])
        grant, _receipt, final = _evidence(driver, task, directory)
        if grant != expected['grant']:
            raise candidate.CandidateError('completion changed after recovery planning')
        # A dead launcher/cleanup receipt alone does not acknowledge its
        # central compiler channels. Reuse exact idempotent drain, never launch.
        from .process import AttemptProcess
        AttemptProcess(directory / 'execution').stop_and_reap(_read(directory / 'execution/process.json'))
        stages.complete(directory, final)
        if receipts.regular_digest(directory / 'staged-completed.json') != grant['completion_sha256']:
            raise candidate.CandidateError('adopted completion differs from recovery grant')
    # The native action acquires stopped owner/controller/coordinator locks and
    # CAS-checks Attempt. Never nest independently opened flock locks here.
    with maintenance_capability(driver.runtime):
        output = driver.native._call(dict(action='dispatch_grant_check_recovery', root=str(driver.runtime),
            task_id=task_id, grant=grant, attempt_sha256=expected['attempt_sha256']))
    recorded = output['tasks'][str(task_id)]
    if recorded['worker_attempt'].get('infrastructure_recovery_grant') != grant:
        raise candidate.CandidateError('native recovery response does not contain the exact grant')
    return dict(expected, status='granted', task_status=recorded['status'],
                infrastructure_retries=recorded['worker_attempt']['infrastructure_retries'])


def retry_event(driver, task, directory, event):
    """Called under the live driver's lock for the one beyond-policy retry."""
    attempt = task['worker_attempt']
    if not isinstance(attempt.get('infrastructure_recovery_grant'), dict):
        raise candidate.CandidateError('no one-use recovery grant exists')
    grant, _receipt, _final = _evidence(driver, task, directory)
    if (attempt.get('infrastructure_recovery_grant') != grant
            or attempt['infrastructure_retries'] not in (grant['retry'] - 1, grant['retry'])
            or event != dict(kind='check_retry', task_id=task['id'], candidate_sha256=grant['candidate_sha256'], retry=grant['retry'])
            or receipts.regular_digest(directory / 'staged-completed.json') != grant['completion_sha256']):
        raise candidate.CandidateError('retry does not match its one-use authenticated recovery grant')
    return dict(event, recovery_completion_sha256=grant['completion_sha256'])


@contextmanager
def maintenance_capability(runtime):
    """Add/remove one supervisor credential without replacing existing entries."""
    from trellis.runtime import bridge
    from .confinement import NATIVE_BWRAP_ENV, MANAGED_AGENT_CHECKS_ENV
    runtime = Path(runtime).resolve()
    path = bridge._burst_tokens_path(runtime)
    token = bridge._mint_burst_token()
    with bridge._exclusive_lock(path):
        current = _read(path) if path.exists() else dict(runtime_root=str(runtime), tokens=[], entries=[])
        bridge._validate_burst_tokens_runtime(path, current)
        if not isinstance(current.get('tokens'), list) or not isinstance(current.get('entries'), list):
            raise ValueError('invalid maintenance credential registry')
        current['tokens'].append(token)
        current['entries'].append(dict(token=token, ts=time.time(), burst_id='host-recovery-maintenance',
                                       kind='supervisor', request_id=0, cycle=0))
        bridge._atomic_write_burst_tokens(path, current)
    settings = dict(TRELLIS_CHECKER_TOKEN=token, TRELLIS_CHECKER_SOCKET=str(runtime / 'sockets/checker.sock'),
        TRELLIS_KERNEL_CACHE_ROOT=str(runtime), TRELLIS_RUNTIME_CHECKPOINT_HOOK=os.environ.get(
            'TRELLIS_RUNTIME_CHECKPOINT_HOOK', str(Path(__file__).resolve().parents[2] / 'scripts/trellis_git_checkpoint_hook.sh')),
        **{MANAGED_AGENT_CHECKS_ENV: '1', NATIVE_BWRAP_ENV: None, 'TRELLIS_COMPILER_DATA_VIEW': None})
    previous = {key: os.environ.get(key) for key in settings}
    try:
        for key, value in settings.items():
            if value is None: os.environ.pop(key, None)
            else: os.environ[key] = value
        yield
    finally:
        for key, value in previous.items():
            if value is None: os.environ.pop(key, None)
            else: os.environ[key] = value
        bridge._unregister_burst_token(runtime, token=token)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runtime', type=Path)
    parser.add_argument('--task-id', type=int, required=True)
    parser.add_argument('--acceptance-round', type=int, required=True)
    parser.add_argument('--finalization', type=int, required=True)
    parser.add_argument('--apply', action='store_true', help='record this one operational recovery grant')
    args = parser.parse_args(argv)
    from .driver import ProductionDriver
    driver = ProductionDriver(args.runtime)
    result = (apply if args.apply else plan)(driver, args.task_id, args.acceptance_round, args.finalization)
    print(json.dumps(result, sort_keys=True, indent=2))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
