"""Retire reconstructible advisory mirrors without deleting under admission locks.

A private queue entry is the whole renamed owner, including its readiness
markers. Its name binds the original directory inode; restart can finish a
partial deletion without consulting or touching a newly created mirror.
"""
import json
import logging
import os
from pathlib import Path
import re
import shutil
import stat
import threading
import uuid

from .job_workspace import MAX_INPUT_FILES

_KEY = re.compile(r'[a-f0-9]{64}')
_QUEUED = re.compile(r'([a-f0-9]{64})\.([a-f0-9]{32})\.([0-9]+)\.([0-9]+)')
_TOP = {'repo', 'home', 'input-files.json', 'packages-ready', 'tablet-build-seed.json'}


def _directory(path):
    for ancestor in (path, *path.parents):
        if not stat.S_ISDIR(ancestor.lstat().st_mode):
            raise ValueError('mirror retirement directory is aliased or not a directory')


def _sync(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def qualified(directory):
    """Recognize completed host-owned mirrors, preserving unusual layouts."""
    if not _KEY.fullmatch(directory.name):
        return False  # Tool-info has a different, small preparation lifecycle.
    try:
        _directory(directory)
        entries = {p.name: p.lstat() for p in directory.iterdir()}
        if entries.keys() - _TOP or not {'repo', 'input-files.json', 'packages-ready'} <= entries.keys():
            return False
        for name, info in entries.items():
            if name in {'repo', 'home'}:
                if not stat.S_ISDIR(info.st_mode): return False
            elif not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                return False
        # A manual investigation repository is not disposable compiler cache.
        git = directory / 'repo/.git'
        if git.exists() or git.is_symlink():
            return False
        marker = directory / 'input-files.json'
        if entries['input-files.json'].st_size > 4 << 20:
            return False
        inputs = json.loads(marker.read_bytes())
        if not isinstance(inputs, list) or len(inputs) > MAX_INPUT_FILES:
            return False
        if any(not isinstance(p, str) or not p or Path(p).is_absolute()
               or any(part in {'', '.', '..'} for part in p.split('/')) for p in inputs):
            return False
        return entries == {p.name: p.lstat() for p in directory.iterdir()}
    except (OSError, ValueError, TypeError):
        return False


class MirrorRetirement:
    def __init__(self, root, stop):
        self.root = Path(root)
        self.queue = self.root / 'workspaces-gc'
        self.stop = stop
        self.wake = threading.Event()
        self.thread = None
        self.last_error = None
        self.removed = 0

    def detach(self, directory, before_detach):
        # Caller owns admission -> database locks and has established no live
        # capability, job/claim, compiler lease, or resident for this owner.
        if not qualified(directory):
            return False
        _directory(self.root)
        self.queue.mkdir(mode=0o700, exist_ok=True)
        _directory(self.queue)
        if self.queue.stat().st_uid != os.geteuid() or self.queue.stat().st_mode & 0o077:
            raise ValueError('mirror retirement queue must be private and owned')
        info = directory.lstat()
        destination = self.queue / f'{directory.name}.{uuid.uuid4().hex}.{info.st_dev}.{info.st_ino}'
        if destination.exists() or destination.is_symlink():
            raise ValueError('mirror retirement queue collision')
        before_detach()
        directory.rename(destination)
        # Flush both rename parents before any deletion, in the background.
        # A crash before that flush can only retain a reconstructible mirror.
        self.wake.set()
        return True

    def drain(self):
        """One pass, outside compiler/admission/database locks; retry on wake."""
        if not self.queue.exists() and not self.queue.is_symlink():
            return
        _directory(self.queue)
        info = self.queue.stat()
        if info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise ValueError('mirror retirement queue must be private and owned')
        if not shutil.rmtree.avoids_symlink_attacks:
            raise RuntimeError('mirror retirement requires descriptor-based rmtree')
        # Root sync persists the queue itself; source and destination sync
        # persist detach before destructive work. No coordinator locks held.
        _sync(self.root)
        _sync(self.root / 'workspaces')
        _sync(self.queue)
        for path in self.queue.iterdir():
            if self.stop.is_set():
                return
            match = _QUEUED.fullmatch(path.name)
            if match is None:
                continue
            info = path.lstat()
            if not stat.S_ISDIR(info.st_mode) or (info.st_dev, info.st_ino) != tuple(map(int, match.group(3, 4))):
                continue
            try:
                shutil.rmtree(path)
                _sync(self.queue)
                self.removed += 1
            except OSError as error:
                # The same inode/name remains recoverable after partial work.
                # Other queued mirrors can still drain; no immediate retry spin.
                self._error(error)

    def _error(self, error):
        message = str(error)
        if message != self.last_error:
            logging.getLogger(__name__).warning("Advisory mirror retirement deferred: %s", message)
        self.last_error = message

    def _run(self):
        while not self.stop.is_set():
            self.wake.wait(30)
            self.wake.clear()
            if self.stop.is_set(): return
            try:
                self.drain()
            except (OSError, ValueError, RuntimeError) as error:
                self._error(error)

    def start(self):
        # JobService starts this only after reaping recorded old processes.
        self.thread = threading.Thread(target=self._run, name='check-mirror-retirement', daemon=True)
        self.thread.start()
        self.wake.set()
