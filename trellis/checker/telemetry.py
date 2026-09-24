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
        self.children = []
        self.child_count = 0
        self.child_elapsed_ms = 0
        self.pending_logs = []

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
        result["subprocess_count"] = self.child_count
        result["subprocess_elapsed_ms"] = self.child_elapsed_ms
        result["subprocesses"] = self.children
        result["subprocess_records_dropped"] = self.child_count - len(self.children)
        return result


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
        if len(state.locks) >= 16:
            state.failed = True
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
