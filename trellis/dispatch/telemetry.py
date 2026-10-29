"""Read-only measurements for reviewers/viewer; never acceptance evidence."""
import json
import copy
import os
import sqlite3
import time
import threading
from contextlib import contextmanager
from pathlib import Path

from .candidate import canonical_json
from .publication import atomic


# Only fixed operation names are retained, never native payloads or outputs.
_NATIVE_ACTIONS = frozenset(('dispatch_acceptance_footprint', 'dispatch_acceptance_state',
    'dispatch_archive_requests', 'dispatch_control', 'dispatch_prepare_verifier',
    'dispatch_prepared_request', 'dispatch_routing', 'dispatch_verifier_inputs',
    'dispatch_commit_publication', 'dispatch_observe_sources', 'dispatch_prepare_batch',
    'dispatch_prepare_publication', 'dispatch_prepare_retry', 'dispatch_publication_receipt',
    'dispatch_worker_failed', 'dispatch_rebase_staged_finalization',
    'dispatch_rebase_checked_response', 'plan_staged_worker', 'step'))

_CONTROL_EVENTS = frozenset(('requests_archived', 'worker_capacity_changed',
    'execution_pipeline_changed', 'agent_capacity_changed', 'compiler_capacity_changed',
    'sol_worker_capacity_changed', 'lightweight_budget_changed', 'model_profiles_changed',
    'review_started', 'assignments_dispatched', 'batch_prepared', 'launched',
    'provider_started', 'turn_completed', 'worker_interrupted', 'worker_expired',
    'check_retry', 'candidate_ready', 'attempt_stopping', 'processes_reaped',
    'retry_prepared', 'drain'))
_PHASES = frozenset(('acceptance.state_read', 'acceptance.source_observation',
    'source_observation.key', 'preparation.manifest', 'workspace.identity', 'workspace.source_validation',
    'workspace.copy', 'workspace.private_state_write', 'workspace.final_source_validation',
    'workspace.audit_materialization', 'workspace.sync', 'workspace.publish'))


_MAX_ACTIVE_NATIVE_OPERATIONS = 128


@contextmanager
def phase(measurements, name):
    """Optional, fixed-label timing; no change to the wrapped operation."""
    ticket = None
    try:
        if measurements is not None:
            ticket = measurements.begin('phase', name)
    except BaseException:
        pass
    ok = False
    try:
        yield
        ok = True
    finally:
        try:
            if measurements is not None:
                measurements.end(ticket, ok=ok)
        except BaseException:
            pass


class NativeMeasurements:
    """Bounded process-lifetime aggregates; no timers, files or protocol inputs.

    Wall times overlap (RPCs run inside held locks); caller CPU excludes child
    native CPU. These are attribution evidence, never scheduling authority.
    """
    def __init__(self):
        self._lock = threading.Lock()
        self._rows = {}
        self._active = {}
        self._active_dropped = 0

    def begin(self, kind, action=None):
        try:
            if kind == 'rpc':
                name = 'rpc:' + (action if action in _NATIVE_ACTIONS else 'other')
            elif kind == 'control':
                name = 'control:' + (action if action in _CONTROL_EVENTS or action == 'read' else 'other')
            elif kind == 'phase' and action in _PHASES:
                name = 'phase:' + action
            elif kind in ('lock_wait', 'lock_held'):
                name = kind
            else:
                return None
            start, cpu = time.monotonic_ns(), time.thread_time_ns()
            thread_id = threading.get_native_id()
            token = object()
            with self._lock:
                if len(self._active) < _MAX_ACTIVE_NATIVE_OPERATIONS:
                    self._active[token] = (name, start, thread_id)
                else:
                    self._active_dropped += 1
            return name, start, cpu, token
        except BaseException:
            return None

    def end(self, ticket, *, ok=True):
        try:
            if ticket is None:
                return
            name, start, cpu, token = ticket
            # Retire active attribution even if a later clock read fails.
            with self._lock:
                self._active.pop(token, None)
            elapsed = (time.monotonic_ns() - start) / 1e6
            cpu = (time.thread_time_ns() - cpu) / 1e6
            with self._lock:
                row = self._rows.setdefault(name, dict(calls=0, failures=0, wall_ms=0., caller_cpu_ms=0., max_wall_ms=0.))
                row['calls'] += 1
                row['failures'] += int(not ok)
                row['wall_ms'] += elapsed
                row['caller_cpu_ms'] += cpu
                row['max_wall_ms'] = max(row['max_wall_ms'], elapsed)
        except BaseException:
            pass

    def snapshot(self):
        try:
            with self._lock:
                now = time.monotonic_ns() if self._active else 0
                active = [dict(name=name, native_thread_id=thread_id,
                               elapsed_ms=max(0., (now - start) / 1e6))
                          for name, start, thread_id in self._active.values()]
                return dict(scope='coordinator_process_completed_operations',
                            additive=False, rows={name: dict(row) for name, row in self._rows.items()},
                            active=active, active_dropped_total=self._active_dropped,
                            active_scope='instantaneous_coordinator_threads',
                            active_sampling='Published by the existing coordinator sampling loop only; '
                                            'stale samples do not establish continuing work.')
        except BaseException:
            return None


class Measurements:
    def __init__(self, runtime):
        self.runtime = Path(runtime)
        self.files = {}
        self.updated = 0
        self.value = {}
        self.started_at = time.time()
        self._cleanup_sample = None
        try:
            from .process import identity
            self.process = identity(os.getpid())
        except Exception:
            self.process = None  # Missing diagnostics must not prevent startup.

    def _cleanup(self, collector, now):
        """The coordinator's collector is separate from any cleanup sidecar.

        Its summary property locks the drain counters and copies the nested
        maintenance snapshot under that worker's own lock. No filesystem or
        coordinator-lock operation is performed to read the summary.
        """
        value = dict(scope='coordinator_process_current_drain',
                     process=self.process, observer_started_at=self.started_at,
                     counter_semantics='drain counters reset with drain_id; git_maintenance is process-lifetime; excludes sidecars')
        if collector is None:
            return dict(value, status='not_started')
        try:
            # Keep the last successful sample independent of later snapshots
            # and of consumers mutating the returned telemetry value.
            snapshot = copy.deepcopy(collector.summary)
            self._cleanup_sample = dict(sampled_at=now, summary=snapshot)
            return dict(value, status='available', **copy.deepcopy(self._cleanup_sample))
        except Exception:
            return dict(value, status='unavailable',
                        last_success=copy.deepcopy(self._cleanup_sample))

    def _authority(self, path):
        stat = path.stat()
        previous = self.files.get(str(path))
        if not previous or previous['inode'] != stat.st_ino or previous['offset'] > stat.st_size:
            previous = dict(inode=stat.st_ino, offset=0, requests=0, queue_wait_ms=0., elapsed_ms=0.)
            self.files[str(path)] = previous
        with path.open('rb') as stream:
            stream.seek(previous['offset'])
            for line in stream:
                if not line.endswith(b'\n'):
                    break  # An append in progress is retried on the next sample.
                previous['offset'] += len(line)
                try:
                    row = json.loads(line)
                    metrics = row.get('measurement') or {}
                    if metrics.get('status') != 'ok':
                        continue
                    previous['requests'] += 1
                    previous['elapsed_ms'] += metrics['request_elapsed_ms']
                    previous['queue_wait_ms'] += sum(max(0, lock['acquired_ms'] - lock['waiting_ms'])
                        for lock in metrics.get('locks', []) if lock['mode'] == 'admission' and lock.get('acquired_ms') is not None)
                except (ValueError, TypeError, KeyError):
                    continue
        return {k: previous[k] for k in ('requests', 'queue_wait_ms', 'elapsed_ms')}

    def sample(self, tasks, now=None, *, native=None, scheduling=None, cleanup=None):
        now = time.time() if now is None else now
        if now - self.updated < 15:
            return self.value
        rows = {}
        for id in tasks:
            directory = self.runtime / 'dispatch' / str(id)
            logs = [directory / 'runtime/checker-state/server.log']
            logs += list((directory / 'acceptance').glob('*/runtime/checker-state/server.log'))
            logs += list((directory / 'acceptance').glob('*/finalization/*/runtime/checker-state/server.log'))
            logs += list((directory / 'turns').glob('*/runtime/checker-state/server.log'))
            for path in logs:
                if not path.exists():
                    continue
                row = rows.setdefault(str(id), dict(queue_wait_ms=0., authoritative_requests=0,
                    authoritative_elapsed_ms=0., advisory_jobs=0, advisory_execution_ms=0., queued=0))
                authority = self._authority(path)
                row['queue_wait_ms'] += authority['queue_wait_ms']
                row['authoritative_requests'] += authority['requests']
                row['authoritative_elapsed_ms'] += authority['elapsed_ms']
        database = self.runtime / 'checker-state/jobs/jobs.sqlite3'
        if database.exists():
            with sqlite3.connect(f'file:{database}?mode=ro', uri=True, timeout=.1) as db:
                # Project only metrics and host scopes; capability/owner tokens
                # and raw job/result payloads never enter the output.
                for scope, submitted, started, finished, state, queue_ms, execution_ms in db.execute('''
                    SELECT json_extract(request,'$.scope'), submitted, started, finished, state,
                      json_extract(result,'$.queue_ms'), json_extract(result,'$.execution_ms') FROM jobs'''):
                    id = str(scope).split('/')[0]
                    if id not in rows and id not in tasks:
                        continue
                    row = rows.setdefault(id, dict(queue_wait_ms=0., authoritative_requests=0,
                        authoritative_elapsed_ms=0., advisory_jobs=0, advisory_execution_ms=0., queued=0))
                    row['advisory_jobs'] += 1
                    row['queue_wait_ms'] += max(0., queue_ms if queue_ms is not None else ((started or finished or now)-submitted)*1000)
                    row['advisory_execution_ms'] += execution_ms or 0
                    row['queued'] += int(state == 'queued')
        self.value = dict(sampled_at=now, tasks=rows)
        if native is not None:
            self.value['native'] = native.snapshot()
        if scheduling is not None:
            self.value['scheduling'] = dict(scheduling)
        self.value['coordinator_cleanup'] = self._cleanup(cleanup, now)
        self.updated = now
        atomic(self.runtime / 'dispatch/telemetry.json', canonical_json(self.value))
        return self.value
