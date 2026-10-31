"""Best-effort request measurements; never consulted by certification.

Only fixed-name aggregates are retained. No source, hashes, or responses are
stored here. Clock/counter failures discard telemetry, not application work.
"""
from __future__ import annotations

from contextlib import contextmanager
from contextvars import ContextVar
from functools import wraps
import os
import time


_current = ContextVar("checker_measurement", default=None)
_instance = f"{os.getpid()}:{time.time_ns()}"
_CAPTURE_READ_LIMIT = 16384
_CAPTURE_ORIGINS = frozenset(('replay_before', 'replay_after', 'donation_install',
                              'readonly_current', 'other'))


def safe(fn):
    """Contain even injected BaseException failures in measurement code only."""
    @wraps(fn)
    def wrapped(*args, **kwargs):
        try:
            return fn(*args, **kwargs)
        except BaseException:
            try:
                state = _current.get()
                if state is not None:
                    state.failed = True
            except BaseException:
                pass
            return None
    return wrapped


class Measurement:
    def __init__(self):
        self.started_ns = time.monotonic_ns()
        self.started_cpu_ns = time.thread_time_ns()
        self.started_ts = time.time()
        self.failed = False
        self.bytes_enabled = os.environ.get("TRELLIS_CHECKER_TELEMETRY_BYTES") == "1"
        self.resources_enabled = os.environ.get("TRELLIS_CHECKER_TELEMETRY_RESOURCES") == "1"
        self.spans = {}
        self.stack = []
        self.locks = []
        self.lock_count = 0
        self.children = []
        self.child_count = 0
        self.child_elapsed_ms = 0
        self.pending_logs = []
        self.portable_captures = []
        self.portable_capture_count = 0
        self.capture_detail_count = 0
        self.capture_read_count = 0
        self.active_capture = None
        self.capture_detail_enabled = True
        self.omitted_parallel_capture_detail = False
        self.import_memo_before = _import_memo_info()

    def begin(self, name, value=None):
        row = self.spans.setdefault(name, dict(calls=0, wall_ns=0, cpu_ns=0,
                                             self_wall_ns=0, self_cpu_ns=0,
                                             chars=0, bytes=0))
        row["calls"] += 1
        if isinstance(value, str):
            row["chars"] += len(value)
            if self.bytes_enabled:
                row["bytes"] += len(value.encode("utf-8"))
        elif isinstance(value, bytes):
            row["bytes"] += len(value)
        frame = [row, time.monotonic_ns(), time.thread_time_ns(), 0, 0, name]
        self.stack.append(frame)
        return frame

    def end(self, frame):
        wall = time.monotonic_ns() - frame[1]
        cpu = time.thread_time_ns() - frame[2]
        assert self.stack.pop() is frame
        row = frame[0]
        row["wall_ns"] += wall
        row["cpu_ns"] += cpu
        row["self_wall_ns"] += wall - frame[3]
        row["self_cpu_ns"] += cpu - frame[4]
        if self.stack:
            self.stack[-1][3] += wall
            self.stack[-1][4] += cpu

    def snapshot(self):
        finished_ns = time.monotonic_ns()
        result = dict(schema=1, server_instance=_instance,
                      status="incomplete" if self.failed else "ok",
                      request_started_ts=self.started_ts, request_finished_ts=time.time(),
                      request_elapsed_ms=(finished_ns - self.started_ns) / 1e6,
                      request_thread_cpu_ms=(time.thread_time_ns() - self.started_cpu_ns) / 1e6,
                      bytes_enabled=self.bytes_enabled,
                      resources_enabled=self.resources_enabled)
        result["spans"] = {
            name: {("%s_ms" % key[:-3] if key.endswith("_ns") else key):
                   (value / 1e6 if key.endswith("_ns") else value)
                   for key, value in row.items()}
            for name, row in self.spans.items()
        }
        if not self.bytes_enabled and "strip_lean_comments" in result["spans"]:
            result["spans"]["strip_lean_comments"]["bytes"] = None
        result["locks"] = self.locks
        result["lock_count"] = self.lock_count
        result["lock_records_dropped"] = self.lock_count - len(self.locks)
        result["portable_captures"] = self.portable_captures
        result["portable_capture_records_dropped"] = self.portable_capture_count - len(self.portable_captures)
        result["portable_capture_detail_scope"] = "request_thread_only"
        result["portable_capture_detail_omitted_parallel"] = self.omitted_parallel_capture_detail
        result["subprocess_count"] = self.child_count
        result["subprocess_elapsed_ms"] = self.child_elapsed_ms
        result["subprocesses"] = self.children
        result["subprocess_records_dropped"] = self.child_count - len(self.children)
        memo_after = _import_memo_info()
        # The parser memo is shared by all checker workspaces and threads.
        # These low-cost snapshots include concurrent activity; they are not
        # request-attributed misses or extra evidence about filesystem reads.
        result["lean_import_memo"] = dict(scope="process_interval",
            before=self.import_memo_before, after=memo_after,
            delta={name: memo_after[name] - self.import_memo_before[name]
                   for name in ("hits", "misses", "evictions")}
                if self.import_memo_before is not None and memo_after is not None else None)
        return result


@safe
def portable_capture(stats):
    """Bounded capture diagnostics, without artifact contents or commitments."""
    state = _current.get()
    if state is None or state.failed:
        return
    state.portable_capture_count += 1
    if len(state.portable_captures) < 8:
        keys = ('status', 'reason', 'failure_class', 'bytes_hashed', 'files_hashed', 'cached_bytes', 'cached_files', 'content_revalidation_reads', 'modules',
                'inventory_wall_secs_including_queue', 'hash_wall_secs')
        row = {key: stats[key] for key in keys if key in stats}
        detail_cost = _capture_snapshot(state)
        if detail_cost is not None:
            row.update(detail_cost)
        detail = stats.get('failure_detail')
        if isinstance(detail, dict) and isinstance(detail.get('path'), str):
            row['failure_detail'] = dict(path=detail['path'][-320:], changed_fields=[
                name for name in ('presence', 'st_dev', 'st_ino', 'st_mode', 'st_size',
                                  'st_mtime_ns', 'st_ctime_ns', 'st_nlink')
                if name in detail.get('changed_fields', [])])
        state.portable_captures.append(row)


@safe
def _capture_begin(origin):
    state = _current.get()
    if state is None or state.failed:
        return None
    previous = state.active_capture
    row = None
    if not state.capture_detail_enabled:
        state.omitted_parallel_capture_detail = True
    elif state.capture_detail_count < 8:
        state.capture_detail_count += 1
        row = dict(cost_partition_version=1,
                   read_scope='hash_budget_file_block',
                   origin=origin if isinstance(origin, str) and origin in _CAPTURE_ORIGINS else 'other',
                   started_ms=(time.monotonic_ns() - state.started_ns) / 1e6,
                   timed_read_attempts=0, timed_read_bytes=0, timed_initial_reads=0,
                   timed_revalidation_reads=0, read_timing_complete=True,
                   timing_budget_exhausted=False, completed_phases=[],
                   read_binding_wall_ms=0, read_binding_thread_cpu_ms=0,
                   read_final_validation_wall_ms=0, read_final_validation_thread_cpu_ms=0)
    state.active_capture = (row, 'binding') if row is not None else None
    return state, previous


@safe
def _capture_restore(ticket):
    if ticket is not None:
        state, previous = ticket
        state.active_capture = previous


def capture_cost(fn):
    """Scope diagnostics to one capture, never to a reusable hash budget."""
    @wraps(fn)
    def wrapped(*args, **kwargs):
        ticket = _capture_begin(kwargs.get('telemetry_origin', 'other'))
        try:
            return fn(*args, **kwargs)
        finally:
            _capture_restore(ticket)
    return wrapped


@safe
def _capture_snapshot(state):
    if state.active_capture is None:
        return None
    row, _ = state.active_capture
    return dict(row, finished_ms=(time.monotonic_ns() - state.started_ns) / 1e6)


@safe
def capture_phase_begin(name):
    state = _current.get()
    if state is None or state.failed or state.active_capture is None:
        return None
    if name not in ('inventory', 'hash_phase', 'final_validation'):
        return None
    row, previous = state.active_capture
    ticket = state, row, previous, name, time.monotonic_ns(), time.thread_time_ns()
    if name == 'final_validation':
        state.active_capture = row, 'final_validation'
    return ticket


@safe
def capture_phase_end(ticket, *, completed=True):
    if ticket is None:
        return
    state, row, previous, name, wall, cpu = ticket
    try:
        if not state.failed:
            row[name + '_wall_ms'] = (time.monotonic_ns() - wall) / 1e6
            row[name + '_thread_cpu_ms'] = (time.thread_time_ns() - cpu) / 1e6
            if completed:
                row['completed_phases'].append(name)
    finally:
        if name == 'final_validation':
            state.active_capture = row, previous


@safe
def capture_read_begin(budget, *, revalidation):
    state = _current.get()
    if state is None or state.failed or state.active_capture is None:
        return None
    row, phase = state.active_capture
    if state.capture_read_count >= _CAPTURE_READ_LIMIT:
        row['read_timing_complete'] = False
        row['timing_budget_exhausted'] = True
        return None
    state.capture_read_count += 1
    return (state, row, phase, time.monotonic_ns(), time.thread_time_ns(),
            budget.bytes, revalidation)


@safe
def capture_read_end(ticket, budget, *, completed):
    if ticket is None:
        return
    state, row, phase, wall, cpu, before_bytes, revalidation = ticket
    if state.failed:
        return
    prefix = 'read_' + phase
    row[prefix + '_wall_ms'] += (time.monotonic_ns() - wall) / 1e6
    row[prefix + '_thread_cpu_ms'] += (time.thread_time_ns() - cpu) / 1e6
    row['timed_read_attempts'] += 1
    # Budget-counted bytes: a rejected chunk is deliberately not included.
    row['timed_read_bytes'] += budget.bytes - before_bytes
    row['timed_revalidation_reads' if revalidation else 'timed_initial_reads'] += 1
    row['read_timing_complete'] &= completed


@safe
def _omit_parallel_capture_detail(ticket):
    if ticket is not None:
        ticket[0].capture_detail_enabled = False


@safe
def _import_memo_info():
    from trellis.atomic_actions.observations import _lean_import_memo
    return _lean_import_memo.info()


@safe
def begin(name, value=None):
    state = _current.get()
    if state is not None and not state.failed:
        return state, state.begin(name, value)


@safe
def end(ticket):
    if ticket is not None:
        state, frame = ticket
        if not state.failed:
            state.end(frame)


def measured(name, *, value_arg=None):
    def decorate(fn):
        @wraps(fn)
        def wrapped(*args, **kwargs):
            # Argument extraction is measurement, too (keyword calls included).
            ticket = _begin_call(name, value_arg, args, kwargs)
            try:
                return fn(*args, **kwargs)
            finally:
                end(ticket)
        return wrapped
    return decorate


@contextmanager
def phase(name):
    """Measure an existing block with the same fail-open path as measured()."""
    ticket = begin(name)
    try:
        yield
    finally:
        end(ticket)


@safe
def _begin_call(name, value_arg, args, kwargs):
    value = None
    if value_arg is not None:
        index, key = value_arg
        value = args[index] if len(args) > index else kwargs.get(key)
    return begin(name, value)


@safe
def _start_request():
    if os.environ.get("TRELLIS_CHECKER_TELEMETRY", "1") == "0":
        return None
    state = Measurement()
    return state, _current.set(state)


@safe
def _finish_request(ticket):
    if ticket is None:
        return
    state, token = ticket
    try:
        try:
            extra = {"measurement": state.snapshot()}
        except BaseException:
            extra = {"measurement": {"schema": 1, "status": "unavailable"}}
        for emit, args, kwargs in state.pending_logs:
            # Log failures cannot replace the already computed RPC response.
            try:
                emit(*args, **kwargs, **extra)
            except BaseException:
                pass
    finally:
        _current.reset(token)


def request(fn):
    @wraps(fn)
    def wrapped(*args, **kwargs):
        ticket = _start_request()
        try:
            return fn(*args, **kwargs)
        finally:
            _finish_request(ticket)
    return wrapped


def parallel_call(fn, value):
    """An independent measurement stack for a compiler-pool thread."""
    ticket = _start_request()
    _omit_parallel_capture_detail(ticket)
    try:
        result = fn(value)
        return result, ticket[0] if ticket else None
    finally:
        if ticket:
            _current.reset(ticket[1])


@safe
def merge_parallel(child):
    """Merge joined children on the request thread, never sharing span stacks."""
    parent = _current.get()
    if parent is None or child is None:
        return
    parent.failed |= child.failed
    parent.omitted_parallel_capture_detail |= child.omitted_parallel_capture_detail
    for name, row in child.spans.items():
        target = parent.spans.setdefault(name, {key: 0 for key in row})
        for key, value in row.items():
            target[key] += value
    parent.child_count += child.child_count
    parent.child_elapsed_ms += child.child_elapsed_ms
    offset = (child.started_ns - parent.started_ns) / 1e6
    for row in child.children[:max(0, 256 - len(parent.children))]:
        parent.children.append({**row, "started_ms": row["started_ms"] + offset})
    parent.lock_count += child.lock_count
    for row in child.locks[:max(0, 16 - len(parent.locks))]:
        parent.locks.append({key: value + offset if key.endswith("_ms") and value is not None
                            else value for key, value in row.items()})


@safe
def _defer(emit, args, kwargs):
    state = _current.get()
    if state is not None and len(state.pending_logs) < 2:
        state.pending_logs.append((emit, args, kwargs))
        return True
    return False


def deferred_log(fn):
    @wraps(fn)
    def wrapped(*args, **kwargs):
        if not _defer(fn, args, kwargs):
            # Existing logging is best effort as well; do not let serialization
            # or I/O errors in a diagnostic change an RPC outcome.
            try:
                return fn(*args, **kwargs)
            except BaseException:
                return None
    return wrapped


@safe
def _lock_event(mode, event, ticket=None):
    state = _current.get()
    if state is None or state.failed:
        return None
    offset = (time.monotonic_ns() - state.started_ns) / 1e6
    if event == "waiting":
        state.lock_count += 1
        if len(state.locks) >= 16:
            return None
        ticket = dict(mode=mode, waiting_ms=offset, acquired_ms=None, released_ms=None)
        state.locks.append(ticket)
    elif ticket is not None:
        ticket[event + "_ms"] = offset
    return ticket


@contextmanager
def lock(gate, mode):
    ticket = _lock_event(mode, "waiting")
    try:
        with gate:
            _lock_event(mode, "acquired", ticket)
            yield
    finally:
        if ticket is not None and ticket["acquired_ms"] is not None:
            _lock_event(mode, "released", ticket)


@safe
def child_started(process, kind, started_ns):
    state = _current.get()
    if state is None:
        return None
    state.child_count += 1
    row = dict(kind=kind, started_ms=(started_ns - state.started_ns) / 1e6,
               phases=[frame[5] for frame in state.stack],
               elapsed_ms=None, returncode=None, wait4_cpu_ms=None,
               wait4_maxrss_kib=None, resources=None)
    if len(state.children) < 256:
        state.children.append(row)
    sampler = None
    if state.resources_enabled:
        from trellis.checker.telemetry_resources import start_sampler
        sampler = start_sampler(process.pid)
    row['resource_sampling'] = ('sampled' if sampler is not None else
        'unavailable_or_sampler_limit' if state.resources_enabled else 'disabled')
    return state, row, started_ns, sampler


@safe
def child_finished(ticket, process):
    if ticket is None:
        return
    state, row, started, sampler = ticket
    if sampler is not None:
        row["resources"] = sampler.stop()
    row["elapsed_ms"] = (time.monotonic_ns() - started) / 1e6
    state.child_elapsed_ms += row["elapsed_ms"]
    row["returncode"] = process.returncode
    usage = process.child_rusage
    if usage is not None:
        row["wait4_cpu_ms"] = (usage.ru_utime + usage.ru_stime) * 1000
        row["wait4_maxrss_kib"] = usage.ru_maxrss


@safe
def compiler_memory(ticket, peak, lease):
    if ticket is not None:
        ticket[1].update(peak_tree_rss_bytes=peak or None,
                         reservation_bytes=lease.get("estimate"),
                         execution_limit_bytes=lease.get('limit_bytes') or None,
                         memory_attempt=lease.get('memory_attempt', 1),
                         cgroup=lease.get('cgroup_measurement'),
                         compiler_operation=lease.get("operation") or "cold_or_mutating",
                         cpu_units=lease.get('cpu_units'),
                         memory_metric="sampled process-tree RSS; shared pages may be counted twice")
