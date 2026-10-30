"""Retain exact terminal assignments for committed-batch recovery, compactly.

The existing archive format is already read by old live batch adoption and
capture-seal validation. Canonical Git and active owners retain ordinary bytes.
"""
from contextlib import contextmanager
from pathlib import Path

from . import retained_files as retained
from .terminal_retention import _json


class Deferred(Exception):
    pass


def collect(collector, directory, observed, *, apply=True):
    result = dict(status='absent', archived=0, removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    root, directory = retained.owner_directory(collector.root, directory)
    owner = retained.owner_identity(directory)
    if owner['kind'] != 'worker' or directory.name not in observed or collector._owner_pending(owner):
        return dict(result, status='not_terminal_or_pending')
    path = directory / 'assignment.json'
    if not path.exists() and not path.is_symlink():
        return result  # Archived readers validate the manifest/digest on use.
    info = retained.regular_path(path).stat()
    if not retained.MIN_BYTES <= info.st_size <= retained.MAX_BYTES:
        return dict(result, status='too_small_or_large')
    outputs = (path, *retained._paths(path), retained._skip_path(path))
    if any(not retained._untracked(item) for item in outputs):
        return dict(result, status='tracked')
    if retained._unchanged_no_savings(path):
        return dict(result, status='no_savings')
    guard = retained.terminal_owner_guard(root, directory, observed)
    witness = collector.witness()
    if witness.get('owners', witness['tasks']).get(directory.name) != observed[directory.name]:
        return dict(result, status='not_committed')
    if Path(_json(directory / 'prepared.json')['source_repo']).resolve() != collector.repo:
        raise ValueError('assignment preparation belongs to another project')
    if not apply:
        return dict(result, status='eligible', bytes=info.st_size, allocated_bytes=info.st_blocks * 512)

    @contextmanager
    def final_unlink():
        with collector.retirement_lock() as acquired:
            if not acquired:
                raise Deferred('cancelled' if collector.cancelled() else 'coordinator_busy')
            if collector.cancelled():
                raise Deferred('cancelled')
            if not collector.checkpoint_current(witness):
                raise ValueError('Git checkpoint changed during assignment archival')
            if collector._owner_pending(owner):
                raise ValueError('task publication still needs recovery')
            if Path(_json(directory / 'prepared.json')['source_repo']).resolve() != collector.repo:
                raise ValueError('assignment preparation changed project')
            guard()
            if any(not retained._untracked(item) for item in outputs):
                raise ValueError('assignment archive became Git tracked')
            if collector.cancelled():
                raise Deferred('cancelled')
            yield

    try:
        archived = retained.archive(path, unlink_context=final_unlink)
    except Deferred as error:
        return dict(result, status=str(error))
    if not archived:
        return dict(result, status='no_savings')
    remaining = [item.stat() for item in retained._paths(path)]
    return dict(result, status='archived', archived=1,
        removed_bytes=info.st_size - sum(item.st_size for item in remaining),
        removed_allocated_bytes=info.st_blocks * 512 - sum(item.st_blocks * 512 for item in remaining))
