"""Execution-bounded recovery; capacity waits are scheduling, not failures."""
from __future__ import annotations

import hashlib
import json
import os
import time
from pathlib import Path

from trellis.json_io import save_json
from .memory_retry import GIB, RetryState
from .admission import admitted, cooperative_poll


@admitted
def command_identity(repo, command):
    """Reuse captured source identities, independent of private workspace names.

    Generated command inputs are named by content. Build outputs are excluded:
    a cold interrupted build must keep its allowance when it is reprepared.
    Replay additionally binds the actual selected artifact bundles at its caller.
    """
    from .job_workspace import CONFIG_FILES
    # This workspace is already host-prepared. Stream its source digests;
    # do not apply advisory transport-size limits to authoritative checking.
    sources = {}
    def file_digest(path):
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for chunk in iter(lambda: stream.read(1 << 20), b''):
                cooperative_poll()
                digest.update(chunk)
        return digest.hexdigest()
    for directory, dirs, files in os.walk(repo):
        cooperative_poll()
        relative = Path(directory).relative_to(repo).as_posix()
        dirs[:] = [name for name in dirs if
            (not name.startswith('.') and name not in {'node_modules', 'target', 'reference', 'crate', 'Dormant'})
            or relative == '.' and name == '.trellis']
        if relative == '.trellis':
            dirs[:] = [name for name in dirs if name in {'scratch', 'scripts', 'tmp', 'stuck-math-audit'}]
        elif relative == '.trellis/tmp':
            dirs[:] = [name for name in dirs if name == 'correspondence']
        for name in files:
            path = Path(directory) / name
            label = path.relative_to(repo).as_posix()
            if (name.endswith('.lean') or relative == '.' and name in CONFIG_FILES
                    or label == 'modeling-input/manifest.json'):
                sources[label] = file_digest(path)
    normalized = []
    for arg in command:
        path = Path(repo) / str(arg)
        if str(arg).endswith('.lean') and path.is_file():
            normalized.append({'source': file_digest(path)})
            try:
                sources.pop(str(path.relative_to(repo)), None)
            except ValueError:
                pass
        else:
            normalized.append(str(arg).replace(str(repo), '<workspace>'))
    return sources, normalized


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


CAPACITY_REASONS = frozenset({'matching_input_admission_deadline', 'shared_admission_deadline',
    'serial_admission_deadline', 'serial_headroom_unavailable', 'serial_pressure_launches_exhausted',
    'helper_admission_deadline', 'cold_materialization_interrupted'})


def is_capacity_wait(detail):
    old_wait_deadline = (detail.get('version') == 3
        and detail.get('exhausted_reason') == 'recovery_execution_deadline'
        and detail.get('hard_execution_seconds', 0) < detail.get('timeout', 0))
    return (detail.get('kind') == 'host_pressure_requeue'
            and (detail.get('capacity_wait') is True or old_wait_deadline
                 or detail.get('exhausted_reason') in CAPACITY_REASONS))


def acquire_lock(scheduler, key, operation, timeout, cancelled):
    """A duplicate caller cannot wait forever or edit the running input's ledger."""
    lock = scheduler.retry_lock(key)
    deadline = time.monotonic() + scheduler.serial_wait_seconds
    while not lock.acquire(timeout=.25):
        cancelled()
        if time.monotonic() >= deadline:
            pending = SerialRetry(scheduler, key, operation, scheduler.maximum_job_bytes, persist=False)
            pending.initialize(timeout)
            pending.data['admission_deadline'] = time.time()
            raise pending.waiting('matching_input_admission_deadline')
    return lock


class SerialRetry(RetryState):
    MAX_RECORDED_ATTEMPTS = 4

    def __init__(self, scheduler, key, operation, initial, **kwargs):
        super().__init__(scheduler, key, operation, initial, **kwargs)
        self.data.update(version=5, bounded_recovery=True, serial=False,
                         serial_launches=0, serial_limit_bytes=0, attempts=[],
                         execution_seconds=0.0, in_flight=False)
        if self.path and self.path.exists():
            raw = json.loads(self.path.read_text())
            if (raw.get('version') in {3, 4, 5} and raw.get('policy_generation') == scheduler.generation
                    and raw.get('input_key') == key and not raw.get('completed')):
                self.data.update(raw)
                self.data['version'] = 5
                old_wait_deadline = (raw.get('version') == 3
                    and raw.get('terminal_reason') == 'recovery_execution_deadline'
                    and raw.get('hard_execution_seconds', 0) < raw.get('timeout', 0))
                if self.data.get('terminal_reason') in CAPACITY_REASONS or old_wait_deadline:
                    self.data.pop('terminal_reason')
                    self.data['capacity_wait'] = True
                if (scheduler.adaptive and not self.data['in_flight']
                        and not self.data['attempted_limit_bytes']):
                    # Old pressure-only retries drained the entire checker.
                    # Keep their history and allowance, but retry in the pool.
                    self.data['serial'] = False

    def initialize(self, timeout, *, targets=()):
        if 'timeout' not in self.data:
            now = time.time()
            targets = list(targets)
            if self.path:
                target_path = self.path.with_suffix('.targets.json')
                target_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                save_json(target_path, targets, mode=0o600)
                self.data['target_evidence'] = str(target_path)
            self.data.update(timeout=timeout, queued_at=now,
                             admission_deadline=now + self.scheduler.serial_wait_seconds,
                             serial_floor_bytes=int(self.scheduler.serial_floor_bytes),
                             serial_target_bytes=int(self.scheduler.serial_maximum_bytes),
                             available_bytes=int(self.scheduler.available() or 0) if self.scheduler.available else 0,
                             targets=[str(target)[:80] for target in targets[:16]],
                             target_count=len(targets), target_set_sha256=digest(targets))
            self.save()
        if self.data.get('capacity_wait'):
            self.data.update(capacity_wait=False,
                             admission_deadline=time.time() + self.scheduler.serial_wait_seconds)
            self.save()

    def unavailable(self, reason):
        self.data['terminal_reason'] = reason
        self.save()
        return self.outcome('host_pressure_requeue', reason)

    def waiting(self, reason):
        # Release the host stage for a cancellable scheduling delay. Preserve
        # the input, executed hard-limit allowance and all measured costs.
        self.data['capacity_wait'] = True
        self.save()
        return self.outcome('host_pressure_requeue', reason)

    def remaining(self, timeout):
        self.initialize(timeout)
        if self.data.get('serial_exhausted'):
            raise self.outcome('resource_rejected', 'serial_own_cap_exhausted')
        if self.data.get('terminal_reason'):
            raise self.unavailable(self.data['terminal_reason'])
        if self.data['in_flight']:
            raise self.unavailable('previous_launch_outcome_unknown')
        # Waiting and pressure-preempted work cannot exhaust the candidate's
        # execution allowance. Hard-limit trials still share one time budget.
        left = self.data['timeout'] - self.data['hard_execution_seconds']
        if left <= 0:
            raise self.unavailable('recovery_execution_deadline')
        return left

    def admission(self):
        """Called under the scheduler condition, after local compilers drain."""
        self.remaining(self.data['timeout'])
        available = self.scheduler.available() if self.scheduler.available else None
        if available is None:
            raise self.unavailable('memory_availability_unavailable')
        floor = self.scheduler.serial_floor_bytes
        # memory.max is a ceiling, not a reservation. Requiring the complete
        # ceiling to be free stranded even small serial jobs on an idle host.
        budget = self.scheduler.serial_maximum_bytes
        groups = self.scheduler.groups
        if groups is not None:
            budget = min(budget, groups.headroom() - floor)
        self.data.update(available_bytes=int(available), admissible_limit_bytes=int(max(0, budget)),
                         serial_floor_bytes=int(floor), serial_target_bytes=int(self.scheduler.serial_maximum_bytes))
        failed_limit = max(self.data['attempted_limit_bytes'], default=0)
        if available <= floor or budget <= failed_limit:
            # An enclosing group's temporary load must not turn escalation
            # into a smaller hard-limit trial and a candidate rejection.
            return False, 'waiting for memory headroom for serial execution'
        self.data.update(serial_limit_bytes=int(budget), limit_bytes=int(budget),
                         serial_admission='available_headroom')
        self.save()
        return True, None

    def check_wait(self):
        self.remaining(self.data['timeout'])
        if time.time() >= self.data['admission_deadline']:
            raise self.waiting('serial_admission_deadline' if self.data['serial'] else 'shared_admission_deadline')

    def lease_options(self):
        return dict(recovery=self, exclusive=self.data['serial'],
                    controlled=True, limit_bytes=self.data['limit_bytes'],
                    cpu_units=1 if self.scheduler.cpu_accounting and
                        (self.data['serial'] or self.data['pressure_requeues']) else None)

    def begin(self):
        import threading
        self.remaining(self.data['timeout'])
        record = self.scheduler.active[threading.get_ident()]
        self.data['limit_bytes'] = int(record['limit_bytes'])
        self.data['launch_count'] += 1
        if self.data['serial']:
            self.data['serial_launches'] += 1
        self.data['in_flight'] = True
        self.started = time.monotonic()
        attempt = dict(serial=self.data['serial'], limit_bytes=self.data['limit_bytes'],
                       threads=self.scheduler.lean_threads(), sampled_peak_bytes=0, cgroup_peak_bytes=0,
                       other_groups=max(0, len(self.scheduler.active) - 1),
                       available_bytes=self.scheduler.available() if self.scheduler.available else None)
        self.data['attempts'].append(attempt)
        if len(self.data['attempts']) > self.MAX_RECORDED_ATTEMPTS:
            # Keep the original shared attempt and the most recent retries;
            # lifetime counters retain the total without growing the marker.
            del self.data['attempts'][1]
            self.data['omitted_attempts'] = self.data.get('omitted_attempts', 0) + 1
        record['recovery_attempt'] = attempt
        self.save()  # A crash cannot silently refill this launch's allowance.

    def stopped(self, failure, elapsed):
        self.data['in_flight'] = False
        self.data['execution_seconds'] += elapsed
        self.data['discarded_seconds'] += elapsed
        self.data['last_cause'] = failure.cause
        self.data['available_bytes'] = int(self.scheduler.available() or 0) if self.scheduler.available else 0
        self.data['sampled_peak_bytes'] = max(self.data['sampled_peak_bytes'], failure.peak_bytes)
        self.data['cgroup_peak_bytes'] = max(self.data['cgroup_peak_bytes'], failure.cgroup_peak_bytes)
        attempt = self.data['attempts'][-1]
        attempt.update(cause=failure.cause,
                       sampled_peak_bytes=max(attempt['sampled_peak_bytes'], failure.peak_bytes),
                       cgroup_peak_bytes=max(attempt['cgroup_peak_bytes'], failure.cgroup_peak_bytes),
                       execution_seconds=elapsed)
        if failure.cause == 'host_pressure':
            self.data['pressure_requeues'] += 1
            self.data['pressure_execution_seconds'] += elapsed
        else:
            self.data['attempted_limit_bytes'].append(self.data['limit_bytes'])
            self.data['attempt_count'] += 1
            self.data['last_attempted_limit_bytes'] = self.data['limit_bytes']
            self.data['last_hard_cause'] = failure.cause
            self.data['hard_execution_seconds'] += elapsed
            if self.data['serial']:
                self.data['serial_exhausted'] = True
        if not self.data['serial'] and (failure.cause != 'host_pressure' or not self.scheduler.adaptive):
            self.data.update(serial=True, admission_deadline=time.time() + self.scheduler.serial_wait_seconds)
        self.save()

    def succeeded(self):
        elapsed = time.monotonic() - self.started
        self.data['execution_seconds'] += elapsed
        self.data['attempts'][-1].update(outcome='completed', execution_seconds=elapsed)
        self.data.update(in_flight=False, completed=True)
        if self.path and not self.data['serial']:
            # Ordinary successful probes must not fill the bounded failure
            # ledger over a long run. Failed/recovered work retains evidence.
            self.path.unlink(missing_ok=True)
            self.path.with_suffix('.targets.json').unlink(missing_ok=True)
            return
        self.save()


def state(scheduler, *args, **kwargs):
    return (SerialRetry if scheduler.available_memory else RetryState)(scheduler, *args, **kwargs)


def feedback(detail):
    def gib(value):
        return f'{value / GIB:.2f} GiB'
    attempts = []
    for attempt in detail.get('attempts', []):
        label = 'Exclusive retry' if attempt['serial'] else 'Shared attempt'
        attempts.append(f"{label}: cap {gib(attempt['limit_bytes'])}, {attempt['threads']} thread(s), "
            f"sampled process-tree peak {gib(attempt['sampled_peak_bytes'])}, "
            f"cgroup peak {gib(attempt['cgroup_peak_bytes'])}, "
            f"{attempt['other_groups']} other compiler groups in this checker; "
            f"stop: {attempt.get('cause', 'outcome unavailable')}.")
    summary = ' '.join(attempts)
    if detail.get('omitted_attempts'):
        summary += f" {detail['omitted_attempts']} intermediate pressure attempts omitted; cumulative costs retained."
    targets = ', '.join(detail.get('targets', []))
    if detail.get('target_count', 0) > len(detail.get('targets', [])):
        targets += f" (first {len(detail['targets'])} of {detail['target_count']}; full set digest {detail['target_set_sha256']})"
    if detail.get('target_evidence'):
        targets += f"; full target set: {detail['target_evidence']}"
    if detail.get('kind') == 'resource_rejected' and detail.get('serial_exhausted'):
        authority = 'Managed' if detail['operation'].startswith('advisory:') else 'Authoritative'
        message = (f"{authority} {detail['operation']} could not complete within the available memory budget. "
            f"{summary} Cgroup peaks include file cache. Serial admission: {detail['serial_admission']}. "
            f"Targets: {targets}. No proof invalidity was established. Within your assigned scope, "
            "reduce large proof terms or expensive reduction/elaboration; factor repeated reasoning into "
            "separately checked lemmas with small interfaces. Splitting files alone may not reduce the "
            "memory needed to import dependencies and replay a large proof term. If the needed change "
            "lies outside your scope, use the existing "
            "needs_restructure or stuck outcome permitted by your contract and identify the required change. "
            "Do not resubmit unchanged work expecting another memory escalation.")
    elif is_capacity_wait(detail):
        message = (f"Host checking is waiting for memory capacity: {detail.get('exhausted_reason')}. "
            f"{summary} Available memory: {gib(detail.get('available_bytes', 0))}. "
            "The candidate and its executed allowance are retained. Checking will requeue automatically; "
            "no proof verdict was reached and no worker correction is required.")
    else:
        message = (f"Host checking could not complete: {detail.get('exhausted_reason', 'resource interruption')}. "
            f"{summary} Available memory: {gib(detail.get('available_bytes', 0))}; "
            f"serial target {gib(detail.get('serial_target_bytes', 0))}, "
            f"floor {gib(detail.get('serial_floor_bytes', 0))}; "
            f"charged execution {detail.get('hard_execution_seconds', 0):.2f}s "
            f"of {detail.get('timeout', 0):.2f}s. "
            "The candidate is retained; no proof verdict was reached. This is infrastructure feedback, "
            "not a request to simplify an unassessed proof.")
    return message
