"""Host-owned hard-limit retries and pressure requeues; no proof authority."""
from __future__ import annotations

import hashlib
import json
import time
from pathlib import Path

from trellis.json_io import save_json

GIB = 2**30
PREPARED = {'semantic', 'local_closure', 'axioms'}


def eligible(operation):
    return (operation in PREPARED or operation == 'kernel_replay'
            or bool(operation and operation.startswith('advisory:')
                    and operation.endswith(':dependencies_reused')))


def seed(operation):
    return (16 if operation == 'kernel_replay' else 8) * GIB


def ceiling(operation):
    return (32 if operation == 'kernel_replay' else 24) * GIB


class MemoryStopped(BaseException):
    """Only host sampling or a recorded cgroup OOM creates this exception."""
    def __init__(self, cause, peak_bytes, reason, *, cgroup_peak_bytes=0):
        assert cause in {'host_pressure', 'execution_limit', 'cgroup_oom'}
        self.cause, self.peak_bytes, self.reason = cause, peak_bytes, str(reason)
        self.cgroup_peak_bytes = cgroup_peak_bytes
        super().__init__(self.reason)


class ResourceOutcome(RuntimeError):
    def __init__(self, detail):
        self.detail = detail
        message = ('compiler exhausted its host memory limits' if detail['kind'] == 'resource_rejected'
                   else 'host memory pressure interrupted checking; retained input will be requeued')
        if detail.get('bounded_recovery'):
            from .serial_recovery import feedback
            message = feedback(detail)
        super().__init__(message)


class ExecutionBudgetExpired(TimeoutError):
    """Hard-limit attempts consumed the ordinary command execution allowance."""


def command_key(owner, operation, args, kwargs, *, shared_headroom=False):
    # Owner is a host-issued private workspace, not a worker-provided path.
    # Normalize generated scratch filenames by their fixed host probe bytes.
    # This is a conservative workload key: changed source may retain a higher
    # floor; it cannot reset unchanged work's miss count or certify old output.
    repo = Path(args[0] if args else kwargs['repo'])
    command = []
    for arg in (args[1] if len(args) > 1 else kwargs['args']):
        path = repo / str(arg)
        if str(arg).endswith('.lean') and path.is_file() and path.stat().st_size <= 1 << 20:
            command.append({'lean_source_sha256': hashlib.sha256(path.read_bytes()).hexdigest()})
        else:
            command.append(str(arg))
    # Shared identity carries only larger budgets, never attempt counts or
    # output. Exact probe text/args survive a fresh private workspace path.
    value = [None if shared_headroom else owner, operation,
             None if shared_headroom else str(repo.resolve()), command, kwargs.get('bwrap_role')]
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


class RetryState:
    MAX_ATTEMPTS = 3

    def __init__(self, scheduler, key, operation, initial, *, headroom_key=None, persist=True):
        self.scheduler, self.key, self.operation = scheduler, key, operation
        self.headroom_key = headroom_key
        self.path = scheduler.retry_root / key[:2] / (key + '.json') if scheduler.retry_root and persist else None
        maximum = int(scheduler.maximum_job_bytes)
        self.data = dict(version=2, input_key=key, operation=operation,
                         policy_generation=scheduler.generation, attempt_count=0,
                         attempted_limit_bytes=[], last_attempted_limit_bytes=0,
                         reservation_bytes=min(initial, maximum),
                         limit_bytes=min(max(ceiling(operation), initial), maximum),
                         discarded_seconds=0.0, hard_execution_seconds=0.0,
                         pressure_execution_seconds=0.0, pressure_requeues=0,
                         launch_count=0, sampled_peak_bytes=0, cgroup_peak_bytes=0)
        if scheduler.available_memory:
            self.data.update(admission_policy='available', maximum_job_bytes=maximum)
        if self.path and self.path.exists():
            raw = json.loads(self.path.read_text())
            if (raw.get('input_key'), raw.get('operation'), raw.get('policy_generation')) == (
                    key, operation, scheduler.generation):
                if raw.get('version') == 2:
                    self.data.update(raw)
                elif raw.get('version') == 1:
                    # Old records mix pressure with hard stops and do not say
                    # which ceilings actually ran. Retain headroom, not an
                    # unproven exhausted-attempt classification.
                    for field in ('reservation_bytes', 'limit_bytes'):
                        self.data[field] = min(maximum, max(self.data[field], raw[field]))
        if headroom_key is not None:
            with scheduler.condition:
                learned = scheduler.recovery_headroom.get(headroom_key, (0, 0))
            self.data['reservation_bytes'] = max(self.data['reservation_bytes'], learned[0])
            self.data['limit_bytes'] = max(self.data['limit_bytes'], learned[1])
        self.data['reservation_bytes'] = min(maximum, max(initial, self.data['reservation_bytes']))
        self.data['limit_bytes'] = min(maximum, max(self.data['reservation_bytes'], self.data['limit_bytes']))
        if scheduler.available_memory:
            # Available-memory admission has no reservation-sized trial. A
            # single job gets the configured hard ceiling immediately.
            self.data.update(admission_policy='available', maximum_job_bytes=maximum)
            self.data['limit_bytes'] = maximum
        if self.data['attempt_count'] == self.MAX_ATTEMPTS - 1:
            self.data['limit_bytes'] = maximum  # The last permitted hard trial gets the actual host maximum.

    def save(self):
        if self.path:
            self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            if not self.path.exists() and sum(1 for _ in self.path.parent.iterdir()) >= 256:
                raise RuntimeError('memory retry history is full; retain the compiler input')
            save_json(self.path, self.data, mode=0o600)

    def outcome(self, kind, reason):
        detail = dict(self.data, kind=kind,
                      cause=('host_pressure' if kind == 'host_pressure_requeue' else self.data['last_hard_cause']),
                      exhausted_reason=reason, drained=True,
                      cgroup_peak_scope='compiler group lifetime; includes file cache')
        for key in ('reservation_bytes', 'limit_bytes', 'sampled_peak_bytes', 'cgroup_peak_bytes'):
            detail[key] = int(detail[key])
        return ResourceOutcome(detail)

    def remaining(self, timeout):
        attempted = self.data['attempted_limit_bytes']
        if attempted and (len(attempted) >= self.MAX_ATTEMPTS
                          or attempted[-1] >= self.scheduler.maximum_job_bytes):
            raise self.outcome('resource_rejected', 'hard_limit_attempts_exhausted')
        if self.data['hard_execution_seconds'] >= timeout:
            raise ExecutionBudgetExpired('compiler recovery consumed its execution time limit')
        return max(.001, timeout - self.data['hard_execution_seconds'])

    def begin(self):
        self.data['launch_count'] += 1
        if self.data.get('last_cause'):
            self.save()

    def pressure_delay(self):
        # No pressure-attempt ceiling. Delay outside compiler admission and
        # remain cancellable; the next lease still obeys every memory guard.
        return min(5.0, .25 * 2 ** min(5, max(0, self.data['pressure_requeues'] - 1)))

    def wait_pressure(self, cancelled):
        deadline = time.monotonic() + self.pressure_delay()
        while True:
            cancelled()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            time.sleep(min(.1, remaining))

    def stopped(self, failure, elapsed):
        self.data['discarded_seconds'] += elapsed
        self.data['sampled_peak_bytes'] = max(self.data['sampled_peak_bytes'], failure.peak_bytes)
        self.data['cgroup_peak_bytes'] = max(self.data['cgroup_peak_bytes'], failure.cgroup_peak_bytes)
        self.data['last_cause'] = failure.cause
        self.data['controlled_requeues'] = self.data.get('controlled_requeues', 0) + 1
        if failure.cause == 'host_pressure':
            self.data['pressure_requeues'] += 1
            self.data['pressure_execution_seconds'] += elapsed
        else:
            actual = int(self.data['limit_bytes'])
            self.data['attempted_limit_bytes'].append(actual)
            self.data['attempt_count'] = len(self.data['attempted_limit_bytes'])
            self.data['last_attempted_limit_bytes'] = actual
            self.data['last_hard_cause'] = failure.cause
            self.data['hard_execution_seconds'] += elapsed
            maximum = self.scheduler.maximum_job_bytes
            raised = max(2 * self.data['reservation_bytes'], 1.25 * failure.peak_bytes + 2 * GIB)
            self.data['reservation_bytes'] = int(min(maximum, raised))
            self.data['limit_bytes'] = int(min(maximum, max(2 * actual, raised + 4 * GIB)))
            if self.data['attempt_count'] == self.MAX_ATTEMPTS - 1:
                self.data['limit_bytes'] = maximum
            if self.headroom_key is not None:
                with self.scheduler.condition:
                    history = self.scheduler.recovery_headroom
                    previous = history.pop(self.headroom_key, (0, 0))
                    history[self.headroom_key] = (max(previous[0], self.data['reservation_bytes']),
                                                  max(previous[1], self.data['limit_bytes']))
                    while len(history) > 128:
                        history.pop(next(iter(history)))
        self.save()

    def succeeded(self):
        # Preserve learned headroom, but a completed command does not spend
        # the next command's allowance. Pressure work remains in telemetry.
        self.data.update(last_success_launches=self.data['launch_count'],
                         last_discarded_seconds=self.data['discarded_seconds'],
                         attempt_count=0, attempted_limit_bytes=[], last_attempted_limit_bytes=0,
                         discarded_seconds=0.0, hard_execution_seconds=0.0,
                         pressure_execution_seconds=0.0, pressure_requeues=0, launch_count=0)
        if self.data.get('last_cause'):
            self.save()
