"""Sticky host resource failures, scoped to one private acceptance workspace.

The checker writes outside the agent's writable mounts. A fresh host retry
gets a new workspace; no agent-facing reset or worker-supplied verdict exists.
"""
import json
import hashlib
import fcntl
from contextlib import contextmanager
import os
from pathlib import Path
import time

from trellis.json_io import save_json

HOST_RUNTIME_ENV = "TRELLIS_HOST_ACCEPTANCE_RUNTIME"
CAPACITY_MARKER = "checker-state/capacity-blocked.json"
MEMORY_MARKER = "checker-state/memory-outcome.json"
MEMORY_PHASE = "checker-state/memory-phase.json"


class CapacityBlocked(RuntimeError):
    """A drained host memory outcome requires native negative delivery/requeue."""

    def __init__(self, record):
        self.record = record
        super().__init__('Host checker memory limit reached; see retained resource diagnostics')


def capacity_record(runtime):
    root = Path(runtime).resolve()
    path = root / MEMORY_MARKER
    if not path.exists():
        path = root / CAPACITY_MARKER  # Read retained pre-repair executions.
    if not path.exists():
        return None
    from trellis.dispatch.acceptance_receipts import regular
    data = regular(path)
    if len(data) > 16384:
        raise RuntimeError('host capacity evidence exceeds its bound')
    value = json.loads(data)
    detail = value.get('detail', {})
    if (value.get('version') != 1 or value.get('kind') not in
            {'capacity_blocked', 'resource_rejected', 'host_pressure_requeue'}
            or value.get('runtime_root') != str(root)
            or detail.get('drained') is not True
            or detail.get('cause') not in {'host_pressure', 'execution_limit', 'cgroup_oom'}
            or any(not isinstance(detail.get(k), str) or not 0 < len(detail[k]) <= 256
                   for k in ('operation', 'input_key', 'policy_generation'))
            or any(type(detail.get(k)) is not int or detail[k] < 0 for k in
                   ('attempt_count', 'reservation_bytes', 'limit_bytes', 'sampled_peak_bytes'))):
        raise RuntimeError('invalid host capacity evidence')
    return value


def memory_phase(runtime, phase, candidate_sha256):
    """Host-only attribution: baseline failure cannot establish proof invalidity."""
    if phase not in {'baseline', 'candidate'}:
        raise ValueError('unknown host memory phase')
    save_json(Path(runtime) / MEMORY_PHASE,
              dict(version=1, phase=phase, candidate_sha256=candidate_sha256), mode=0o600)


def record_capacity(runtime, detail):
    """Host callback only; never grants a verdict or clears a prior failure."""
    root = Path(runtime).resolve()
    kind = detail.get('kind', 'capacity_blocked')
    if kind not in {'capacity_blocked', 'resource_rejected', 'host_pressure_requeue'}:
        raise ValueError('unknown checker memory outcome')
    path = root / (CAPACITY_MARKER if kind == 'capacity_blocked' else MEMORY_MARKER)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Preserve each drained leaf. An actual own-limit exhaustion dominates a
    # concurrent pressure requeue, so a fresh host stage cannot erase it.
    with path.with_suffix('.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        phase_path = root / MEMORY_PHASE
        phase = json.loads(phase_path.read_text()) if phase_path.exists() else None
        value = dict(version=1, kind=kind, runtime_root=str(root),
                     detail=dict(detail), phase=phase, timestamp=time.time())
        previous = capacity_record(root) if path.exists() else None
        promote = (previous is not None and previous['kind'] == 'host_pressure_requeue'
                   and kind == 'resource_rejected')
        if previous is not None:
            history = root / 'checker-state/memory-outcomes'
            history.mkdir(exist_ok=True)
            for record in (previous, value):
                identity = hashlib.sha256(json.dumps(record, sort_keys=True).encode()).hexdigest()
                saved = history / (identity + '.json')
                if not saved.exists():
                    save_json(saved, record, mode=0o600)
        if previous is None or promote:
            save_json(path, value, mode=0o600)
        capacity_record(root)


@contextmanager
def host_scope(runtime):
    """Propagate the exact round to trusted native observation subprocesses."""
    previous = os.environ.get(HOST_RUNTIME_ENV)
    os.environ[HOST_RUNTIME_ENV] = str(Path(runtime).resolve())
    try:
        yield
    finally:
        if previous is None:
            os.environ.pop(HOST_RUNTIME_ENV, None)
        else:
            os.environ[HOST_RUNTIME_ENV] = previous


def record_transport_failure(kind, message):
    root = os.environ.get(HOST_RUNTIME_ENV)
    if root and kind in {"resource_exhausted", "supervisor_unavailable", "malformed_response", "internal_error"}:
        record(root, f"{kind}: {message}")


def record(runtime, reason):
    root = Path(runtime).resolve()
    save_json(root / "checker-state/resource-failure.json",
              dict(runtime_root=str(root), reason=str(reason)[:2000], timestamp=time.time()), mode=0o600)


def require_healthy(runtime):
    path = Path(runtime) / "checker-state/resource-failure.json"
    try:
        failure = json.loads(path.read_text())
    except FileNotFoundError:
        capacity = capacity_record(runtime)
        if capacity is not None:
            raise CapacityBlocked(capacity)
        return
    # Invalid/unreadable evidence fails the host operation too. This must
    # never be normalized into an ordinary mathematical worker failure.
    raise RuntimeError(f"host checker resource failure; submission retained for infrastructure retry: {failure}; evidence: {path}")
