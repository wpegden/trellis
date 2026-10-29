"""Deliver host memory failures through native repair, without operator waits."""
from __future__ import annotations

import json

from . import acceptance_receipts as receipts, candidate, capacity_parking


class MemoryRequeue(Exception):
    """Unwind finalization locks, retaining only the semantic reservation."""

    def __init__(self, seconds):
        self.seconds = seconds
        super().__init__('Host memory pressure; retained candidate will retry automatically')


def authenticated_hard_limit(marker, identity):
    """Executed own-cap exhaustion is operational feedback, never a proof verdict."""
    detail, phase = marker['detail'], marker.get('phase')
    candidate_phase = dict(version=1, phase='candidate', candidate_sha256=identity)
    baseline_phase = dict(version=1, phase='baseline', candidate_sha256=identity)
    baseline = (phase == baseline_phase and detail.get('admission_policy') == 'available')
    if detail.get('bounded_recovery') and (baseline or not detail.get('serial_exhausted')):
        return False
    if (marker.get('kind') != 'resource_rejected'
            or detail.get('kind') != 'resource_rejected'
            or detail.get('cause') not in {'execution_limit', 'cgroup_oom'}
            or (phase != candidate_phase and not baseline)):
        return False
    limits = detail.get('attempted_limit_bytes')
    last = detail.get('last_attempted_limit_bytes')
    if (not isinstance(limits, list) or not limits or len(limits) > 3
            or any(type(value) is not int or value <= 0 for value in limits)
            or type(last) is not int or last != limits[-1]
            or detail['attempt_count'] != len(limits)):
        raise candidate.CandidateError('memory rejection lacks actual executed limit evidence')
    if baseline and (type(detail.get('maximum_job_bytes')) is not int
                     or last != detail['maximum_job_bytes']):
        raise candidate.CandidateError('baseline memory feedback requires executed maximum limit evidence')
    return True


def reason(marker, evidence):
    detail = marker['detail']
    if detail.get('bounded_recovery'):
        from trellis.checker.serial_recovery import feedback
        if marker.get('phase', {}).get('phase') == 'baseline':
            detail = dict(detail, kind='host_pressure_requeue')
            return (feedback(detail) + ' The submitted candidate was not assessed; the failure occurred '
                    f'before applying your edits. Full host diagnostics: {evidence}.')
        return feedback(detail) + f' Full host diagnostics: {evidence}.'
    def gib(value):
        return f'{value / (1 << 30):.2f} GiB'
    limits = ', '.join(gib(v) for v in detail['attempted_limit_bytes'])
    peak = f"sampled process-tree peak {gib(detail['sampled_peak_bytes'])}"
    if detail.get('cgroup_peak_bytes'):
        peak += f"; cgroup peak {gib(detail['cgroup_peak_bytes'])} (includes file cache)"
    if marker.get('phase', {}).get('phase') == 'baseline':
        return (f"Host baseline {detail['operation']} exceeded its hard memory limit: {peak}; "
                f"executed limits {limits}. The submitted candidate was not assessed; no proof invalidity "
                f"was established. Report this baseline resource constraint with the retained candidate "
                f"and existing scope. Host resource evidence: {evidence}.")
    return (f"Authoritative {detail['operation']} check exceeded its memory limit: {peak}; "
            f"executed limits {limits}. Simplify the proof or expensive elaboration and submit again. "
            f"Host resource evidence: {evidence}.")


def handle(driver, state, task, directory):
    """Caller owns the native lock; all process cleanup precedes delivery."""
    record, marker = capacity_parking.evidence(driver, task, directory, current_producer=False)
    identity = record['candidate_sha256']
    receipts.write_once(directory / 'memory-delivery.json', dict(
        version=1, candidate_sha256=identity, evidence=record, measurement=marker))
    if authenticated_hard_limit(marker, identity):
        baseline = marker.get('phase', {}).get('phase') == 'baseline'
        driver._reject_candidate_locked(state, task, identity, reason(marker, record['evidence']),
                                       summary=('Host baseline exceeded its memory limit; candidate not assessed'
                                                if baseline else 'Authoritative check exceeded its memory limit'))
        return dict(rejected=True)
    from trellis.checker.serial_recovery import is_capacity_wait
    if marker['detail'].get('bounded_recovery') and not is_capacity_wait(marker['detail']):
        # The driver delivers the same retained evidence through its existing
        # bounded infrastructure counter, instead of the legacy requeue loop.
        return dict(infrastructure_reason=reason(marker, record['evidence']))
    # Host pressure, pre-overlay baseline work, and legacy ambiguous stops
    # cannot consume worker allowance or a native infrastructure retry. Retry
    # against the same sealed candidate; positive owner receipts remain intact.
    # A short cancellable delay outside coordinator.lock prevents a retry spin.
    previous = list((driver.root / str(task['id']) / 'acceptance').glob('*/memory-delivery.json'))
    previous += list((driver.root / str(task['id']) / 'acceptance').glob('*/finalization/*/memory-delivery.json'))
    delay = min(30, 2 ** min(len(previous), 5))
    retry_path = directory / 'retry-authorized.json'
    if retry_path.exists():
        previous = json.loads(receipts.regular(retry_path))
        if previous.get('kind') == 'capacity_recovery':
            # A pre-upgrade crash could leave the old immutable authorization
            # before its park marker advanced. Validate it without demanding a
            # new policy change or interpreting it as mathematical authority.
            capacity_parking.validate_recovery(directory, record, previous)
            return dict(retry_after_seconds=delay)
        delay = previous.get('retry_after_seconds')
        if type(delay) is not int or not 1 <= delay <= 30:
            raise candidate.CandidateError('invalid memory requeue delay')
    authorization = dict(version=1, kind='host_memory_requeue',
        task_id=task['id'], candidate_sha256=identity,
        source_key_sha256=candidate.digest(candidate.canonical_json(
            json.loads(receipts.regular(directory / 'canonical.json'))['key'])),
        memory_delivery_sha256=receipts.regular_digest(directory / 'memory-delivery.json'),
        retry_after_seconds=delay)
    receipts.write_once(retry_path, authorization)
    return dict(retry_after_seconds=delay)
