"""Discard terminal diagnostic copies already preserved by completed host output.

acceptance.check writes acceptance-result.json and returns the same object;
main adds checked_sources before emitting execution/response.json. The former
has no recovery reader. Never discard a field not present with its exact JSON
type/value in the authenticated, durable retained host response.
"""
import json
import os
from pathlib import Path

from . import retained_files as retained
from .candidate import canonical_json, digest, _sync_dir
from .host_response_retention import paths as host_responses
from .terminal_retention import _json

MAX_BYTES = 64 << 20  # Bound decoded preparation memory, not cleanup work.


def _objects(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate host result key')
        result[key] = value
    return result


def _json_bytes(data):
    def invalid(value):
        raise ValueError('nonfinite host result value')
    return json.loads(data, object_pairs_hook=_objects, parse_constant=invalid)


def _snapshot(paths):
    return {path: retained._signature(retained.regular_path(path).stat())
            if path.exists() or path.is_symlink() else None for path in paths}


def _read(path):
    if retained.logical_size(path) > MAX_BYTES:
        raise ValueError('host diagnostic exceeds bounded comparison size')
    data = bytearray()
    for chunk in retained.chunks(path):
        data.extend(chunk)
        if len(data) > MAX_BYTES:
            raise ValueError('host diagnostic exceeds bounded comparison size')
    return bytes(data)


def collect(collector, directory, observed, *, apply=True):
    result = dict(status='absent', removed=0, retained_files=0,
                  removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, Path(directory))
    owner = retained.owner_identity(directory)
    if owner['kind'] != 'worker' or directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    guard = witness = None
    for response in host_responses(directory):
        if collector.cancelled():
            return dict(result, status='cancelled')
        attempt = response.parent.parent
        path = attempt / 'runtime/acceptance-result.json'
        if not path.exists() and not path.is_symlink():
            continue
        info = retained.regular_path(path).stat()
        if not retained._untracked(path):
            result['retained_files'] += 1
            continue
        if guard is None:
            guard = retained.terminal_owner_guard(root, directory, observed)
            witness = collector.witness()
            if witness.get('owners', witness['tasks']).get(directory.name) != observed[directory.name]:
                return dict(result, status='not_committed')
        prepared, completion = attempt / 'prepared.json', response.parent / 'result.json'
        representations = (response, *retained._paths(response))
        inputs = (path, prepared, completion, *representations)
        before = _snapshot(inputs)
        if before[path] != retained._signature(info):
            raise ValueError('host diagnostic changed before comparison')
        try:
            if Path(_json(prepared)['source_repo']).resolve() != collector.repo:
                raise ValueError('host result preparation belongs to another project')
            raw, host_bytes = _read(path), _read(response)
            complete = _json_bytes(_read(completion))
            if (type(complete.get('returncode')) is not int or complete['returncode'] != 0
                    or complete.get('response_sha256') != digest(host_bytes)):
                raise ValueError('host response differs from successful completion receipt')
            diagnostic, host = _json_bytes(raw), _json_bytes(host_bytes)
            if not isinstance(diagnostic, dict) or not diagnostic or not isinstance(host, dict):
                raise ValueError('host diagnostic is not an object')
            if (any(key not in host for key in diagnostic)
                    or canonical_json(diagnostic) != canonical_json({key: host[key] for key in diagnostic})):
                raise ValueError('host diagnostic is not exactly retained by completed response')
        except (FileNotFoundError, ValueError, TypeError, AttributeError):
            result['retained_files'] += 1
            continue
        if _snapshot(inputs) != before:
            raise ValueError('host diagnostic inputs changed during comparison')
        if not apply:
            result['status'] = 'eligible'
            result['eligible'] = result.get('eligible', 0) + 1
            continue
        # Preserve the counterpart durably before unlinking a duplicate. Large
        # decode/comparison and all output fsyncs run outside coordinator.lock.
        durable = [p for p in (completion, *representations) if before[p] is not None]
        for saved in durable:
            with retained.regular_path(saved).open('rb') as stream:
                os.fsync(stream.fileno())
        for parent in {p.parent for p in durable}:
            _sync_dir(parent)
        with collector.retirement_lock() as acquired:
            if not acquired or collector.cancelled():
                return dict(result, status='cancelled' if collector.cancelled() else 'coordinator_busy')
            if not collector.checkpoint_current(witness):
                return dict(result, status='checkpoint_changed')
            if collector._owner_pending(owner):
                return dict(result, status='publication_pending')
            guard()
            if _snapshot(inputs) != before or not retained._untracked(path):
                raise ValueError('host diagnostic inputs changed before retirement')
            if collector.cancelled():
                return dict(result, status='cancelled')
            path.unlink()
            _sync_dir(path.parent)
            result['removed'] += 1
            result['removed_bytes'] += info.st_size
            result['removed_allocated_bytes'] += info.st_blocks * 512
            result['status'] = 'retired'
    return result
