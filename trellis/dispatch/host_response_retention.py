"""Lossless terminal host results; worker/provider responses remain ordinary files.

Only acceptance and finalization executions are selected. Their authenticated
readers hash the original decoded bytes, including after process adoption or
replay. Whole-task retirement prevents archiving an earlier donor still in use.
Restore exact plain paths using retained_files --runtime ROOT --task ID --restore;
that CLI holds the cleanup lease and establishes an operator pin first.
"""
from contextlib import contextmanager
from pathlib import Path

from . import retained_files as retained
from .assignment_retention import Deferred
from .terminal_retention import _json


def paths(directory):
    from .storage import _numbered_directories
    for attempt in _numbered_directories(directory / 'acceptance'):
        yield attempt / 'execution/response.json'
        for final in _numbered_directories(attempt / 'finalization'):
            yield final / 'execution/response.json'


def collect(collector, directory, observed, *, apply=True):
    result = dict(status='absent', archived=0, removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if owner['kind'] != 'worker' or directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    guard = witness = None
    for path in paths(directory):
        if collector.cancelled():
            return dict(result, status='cancelled')
        if not path.exists() and not path.is_symlink():
            continue
        info = retained.regular_path(path).stat()
        outputs = (path, *retained._paths(path), retained._skip_path(path))
        if (not retained.MIN_BYTES <= info.st_size <= retained.MAX_BYTES
                or any(not retained._untracked(item) for item in outputs)
                or retained._unchanged_no_savings(path)):
            continue
        if guard is None:
            guard = retained.terminal_owner_guard(root, directory, observed)
            witness = collector.witness()
            if witness.get('owners', witness['tasks']).get(directory.name) != observed[directory.name]:
                return dict(result, status='not_committed')
        prepared = path.parent.parent / 'prepared.json'
        signature = retained._signature(retained.regular_path(prepared).stat())
        if Path(_json(prepared)['source_repo']).resolve() != collector.repo:
            raise ValueError('host response preparation belongs to another project')
        if not apply:
            result['status'] = 'eligible'
            result['eligible'] = result.get('eligible', 0) + 1
            continue

        @contextmanager
        def final_unlink():
            with collector.retirement_lock() as acquired:
                if not acquired or collector.cancelled():
                    raise Deferred('cancelled' if collector.cancelled() else 'coordinator_busy')
                if not collector.checkpoint_current(witness):
                    raise ValueError('Git checkpoint changed during host response archival')
                if collector._owner_pending(owner):
                    raise ValueError('task publication still needs recovery')
                if retained._signature(retained.regular_path(prepared).stat()) != signature:
                    raise ValueError('host response preparation changed')
                guard()
                if any(not retained._untracked(item) for item in outputs):
                    raise ValueError('host response archive became Git tracked')
                if collector.cancelled():
                    raise Deferred('cancelled')
                yield
        try:
            archived = retained.archive(path, unlink_context=final_unlink)
        except Deferred as error:
            return dict(result, status=str(error))
        if archived:
            remaining = [item.stat() for item in retained._paths(path)]
            result['status'] = 'archived'
            result['archived'] += 1
            result['removed_bytes'] += info.st_size - sum(item.st_size for item in remaining)
            result['removed_allocated_bytes'] += info.st_blocks * 512 - sum(item.st_blocks * 512 for item in remaining)
    return result
