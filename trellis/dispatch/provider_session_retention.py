"""Opt-in lossless terminal worker/reviewer/verifier session archival.

Only owned dated Codex session JSONL is selected. Global homes, credentials,
unknown layouts, hardlinks and active owners remain untouched. Native Codex
resume requires raw paths: retained_files --restore restores them under a pin.
Exclusive lanes lack a committed owner lifecycle and remain outside this scope.
"""
from contextlib import contextmanager
from datetime import date
from pathlib import Path
import stat

from . import retained_files as retained
from .provider_retention import _guard
from .assignment_retention import Deferred


def _directories(path):
    if path.is_symlink():
        raise ValueError('provider sessions cannot traverse a symlink')
    if path.exists():
        yield from sorted(path.iterdir())


def paths(directory):
    from .storage import _attempt_directories
    from trellis.codex_rollouts import logical_path
    for attempt in _attempt_directories(directory):
        parent = attempt / 'runtime/burst-homes'
        if any(p.is_symlink() for p in (parent, *parent.parents)):
            raise ValueError('provider session home cannot traverse a symlink')
        for burst in _directories(parent):
            if burst.is_symlink():
                raise ValueError('provider session home is an alias')
            sessions = burst / '.codex/sessions'
            if (burst / '.codex').is_symlink():
                raise ValueError('provider session home is an alias')
            for year in _directories(sessions):
                if len(year.name) != 4 or not year.name.isascii() or not year.name.isdigit():
                    continue
                for month in _directories(year):
                    if len(month.name) != 2 or not month.name.isascii() or not month.name.isdigit():
                        continue
                    for day in _directories(month):
                        if len(day.name) != 2 or not day.name.isascii() or not day.name.isdigit():
                            continue
                        try: date(int(year.name), int(month.name), int(day.name))
                        except ValueError: continue
                        names = {logical for child in _directories(day)
                                 if (logical := logical_path(child)) is not None}
                        yield from sorted(names)


def collect(collector, directory, observed, *, apply=True, runtime_admission=None):
    result = dict(status='absent', archived=0, skipped=0, removed_bytes=0, removed_allocated_bytes=0)
    if collector.cancelled():
        return dict(result, status='cancelled')
    _, directory = retained.owner_directory(collector.root, Path(directory))
    recheck = None
    for path in paths(directory):
        if collector.cancelled():
            return dict(result, status='cancelled')
        if not path.exists() and not path.is_symlink():
            continue
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            result['skipped'] += 1
            continue
        retained.regular_path(path)
        if not retained.MIN_BYTES <= info.st_size <= retained.MAX_BYTES:
            continue
        outputs = (path, *retained._paths(path), retained._skip_path(path))
        if any(not retained._untracked(item) for item in outputs) or retained._unchanged_no_savings(path):
            result['skipped'] += 1
            continue
        if recheck is None:
            try: recheck, _ = _guard(collector, directory, observed)
            except Deferred as error: return dict(result, status=str(error))
            if runtime_admission is not None:
                runtime_admission()
        if not apply:
            result['status'] = 'eligible'
            result['eligible'] = result.get('eligible', 0) + 1
            continue
        @contextmanager
        def final_unlink():
            with collector.retirement_lock() as acquired:
                if not acquired or collector.cancelled():
                    raise Deferred('cancelled' if collector.cancelled() else 'coordinator_busy')
                recheck()
                if runtime_admission is not None:
                    runtime_admission()
                if any(not retained._untracked(item) for item in outputs):
                    raise ValueError('provider session became Git tracked')
                if collector.cancelled():
                    raise Deferred('cancelled')
                yield
        try:
            archived = retained.archive(path, unlink_context=final_unlink, preserve_mtime=True)
        except Deferred as error:
            return dict(result, status=str(error))
        if archived:
            remaining = [item.stat() for item in retained._paths(path)]
            result['archived'] += 1
            result['status'] = 'archived'
            result['removed_bytes'] += info.st_size - sum(item.st_size for item in remaining)
            result['removed_allocated_bytes'] += info.st_blocks * 512 - sum(item.st_blocks * 512 for item in remaining)
    return result
