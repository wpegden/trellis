"""Byte-triggered incremental packing of canonical Git objects.

Runs independently of dispatch and its locks. It never expires history or
reflogs, prunes unreachable objects, rewrites existing packs, or edits config.
The 1 GiB threshold triggers work; it is not a per-pass work limit. Old clone
links are released separately by git_retention after pack generations change.
"""
from contextlib import contextmanager
import fcntl
import logging
import os
from pathlib import Path
import signal
import stat
import subprocess
import threading
import sys
import time

from .git_retention import _canonical_objects, _directories, _env, _git, OID

DEFAULT_THRESHOLD = 1024 ** 3
DEFAULT_INTERVAL = 60.0
REPACK = ['nice', '-n', '19', 'ionice', '-c', '3', 'git', '--no-replace-objects',
          '-c', 'core.hooksPath=/dev/null', '-c', 'pack.threads=1',
          '-c', 'pack.windowMemory=256m', 'repack', '-d', '-l']


class Cancelled(Exception):
    pass


class Busy(Exception):
    pass


def _regular(path):
    value = path.lstat()
    if not stat.S_ISREG(value.st_mode):
        raise ValueError('nonregular canonical Git file')
    return value


def _loose(objects):
    result = {}
    for parent in objects.iterdir():
        if len(parent.name) != 2 or any(c not in '0123456789abcdef' for c in parent.name):
            continue
        _directories(parent)
        for path in parent.iterdir():
            if not OID.fullmatch(parent.name + path.name):
                raise ValueError('unexpected canonical loose-object name')
            try:
                info = _regular(path)
            except FileNotFoundError:
                continue  # Native concurrent packing may have just removed it.
            result[parent.name + path.name] = info.st_blocks * 512
    return result


def _packs(objects):
    result = {}
    directory = objects / 'pack'
    if not directory.exists():
        return result
    _directories(directory)
    for path in directory.iterdir():
        if path.suffix in ('.pack', '.idx', '.rev', '.keep'):
            info = _regular(path)
            # Shared-clone link removal changes ctime/nlink, not content.
            result[path.name] = (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)
    return result


def _reachable(repo):
    return set(_git(repo, 'rev-list', '--objects', '--no-object-names', '--all',
                    '--reflog', '--indexed-objects', 'HEAD').decode().splitlines())


def _external_maintenance(repo, proc=Path('/proc')):
    for name in ('gc.pid', 'gc.pid.lock', 'maintenance.lock', 'objects/maintenance.lock'):
        path = repo / '.git' / name
        if path.exists() or path.is_symlink():
            raise Busy('existing Git maintenance marker')
    for path in (repo / '.git/objects/pack').glob('*.lock'):
        raise Busy('existing Git pack lock')
    for directory in proc.iterdir():
        if not directory.name.isdigit():
            continue
        try:
            args = (directory / 'cmdline').read_bytes().split(b'\0')
        except FileNotFoundError:
            continue
        if not args or not args[0]:
            continue
        name = Path(os.fsdecode(args[0])).name
        commands = {'gc', 'maintenance', 'repack', 'pack-objects', 'prune', 'prune-packed'}
        if ((name == 'git' and any(os.fsdecode(word) in commands for word in args[1:]))
                or name in {'git-' + word for word in commands}):
            # Conservative host-wide exclusion. Uncooperative native Git may
            # still start afterwards; Git supplies concurrent-writer safety.
            raise Busy('another Git maintenance process is running')


@contextmanager
def _leases(repo, objects):
    # Keep all control files OUTSIDE objects: git clone --local copies even
    # non-object files there. Native maintenance/process checks are observations,
    # not exclusion of uncooperative Git. Git provides concurrent-writer safety.
    own = repo / '.git/trellis-byte-maintenance.flock'
    fd = os.open(own, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise ValueError('invalid Git maintenance lease')
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Busy('Git maintenance lease is held') from error
        yield fd
    finally:
        # The child supervisor inherits this same open file description, so a
        # killed parent cannot release exclusion while its child still packs.
        os.close(fd)


def _verify(repo, before, old_packs):
    current = _packs(repo / '.git/objects')
    if any(current.get(name) != value for name, value in old_packs.items()):
        raise ValueError('preexisting Git pack changed during incremental packing')
    ordered = sorted(before)
    rows = _git(repo, 'cat-file', '--batch-check=%(objectname) %(objecttype)',
                data=('\n'.join(ordered) + '\n').encode()).decode().splitlines()
    if len(rows) != len(ordered) or any(row.split()[0] != oid or row.endswith(' missing') for oid, row in zip(ordered, rows)):
        raise ValueError('captured Git object missing after incremental packing')
    # Object reachability and graph edges, not blob-content decompression.
    _git(repo, 'fsck', '--connectivity-only', '--no-dangling')


def collect(repo, *, threshold_bytes=DEFAULT_THRESHOLD, cancelled=lambda: False, execute=None):
    """One finite observation; all expensive work is off coordinator locks."""
    if type(threshold_bytes) is not int or threshold_bytes <= 0:
        raise ValueError('Git byte threshold must be positive')
    if cancelled():
        raise Cancelled()
    repo = Path(repo).absolute()
    objects = _canonical_objects(repo)
    loose = _loose(objects)
    result = dict(status='below_threshold', reachable_loose_bytes=None,
                  loose_bytes=sum(loose.values()), loose_objects=len(loose), repacked=False)
    if result['loose_bytes'] < threshold_bytes:
        return result
    reachable = _reachable(repo)
    result['reachable_loose_bytes'] = sum(size for oid, size in loose.items() if oid in reachable)
    if result['reachable_loose_bytes'] < threshold_bytes:
        return result
    if cancelled():
        raise Cancelled()
    with _leases(repo, objects) as lease_fd:
        _external_maintenance(repo)
        _canonical_objects(repo)
        # Repeat after admission: another tool may have packed before our lease.
        loose = _loose(objects)
        reachable = _reachable(repo)
        result['reachable_loose_bytes'] = sum(size for oid, size in loose.items() if oid in reachable)
        if result['reachable_loose_bytes'] < threshold_bytes:
            return result
        old_packs = _packs(objects)
        before = reachable | loose.keys()
        config = (repo / '.git/config').read_bytes()
        disk = os.statvfs(repo)
        if disk.f_bavail * disk.f_frsize < 2 * sum(loose.values()) + 1024 ** 3:
            raise Busy('insufficient temporary Git packing headroom')
        if cancelled():
            raise Cancelled()
        started = time.monotonic()
        if execute is None:
            _execute_owned(repo, lease_fd, cancelled)
        else:
            execute(repo, lease_fd)
        if cancelled():
            raise Cancelled()
        _canonical_objects(repo)
        if (repo / '.git/config').read_bytes() != config:
            raise ValueError('Git config changed during incremental packing')
        _verify(repo, before, old_packs)
        after = _loose(objects)
        result.update(status='packed', repacked=True, seconds=time.monotonic() - started,
                      after_loose_bytes=sum(after.values()), after_loose_objects=len(after),
                      verified_objects=len(before))
        return result


def _execute_owned(repo, lease_fd, cancelled):
    if cancelled():
        raise Cancelled()
    # Supervisor keeps the inherited flock for the complete native operation.
    # The pipe gate prevents starting work if its owner died before admission.
    gate = ('import os,sys,subprocess\n'
            'if os.read(0,1)!=b"x":sys.exit(0)\n'
            'child=subprocess.Popen(sys.argv[1:])\n'
            'sys.exit(child.wait())\n')
    process = subprocess.Popen([sys.executable, '-c', gate, *REPACK], cwd=repo, env=_env(),
                               start_new_session=True, stdin=subprocess.PIPE, pass_fds=(lease_fd,),
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    def exited():
        # Observe WITHOUT reaping. The owned session leader's PID cannot
        # be reused until we finish signalling the owned process group.
        return os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    def signal_group(number):
        try:
            os.killpg(process.pid, number)
        except ProcessLookupError:
            pass
    try:
        if cancelled():
            raise Cancelled()
        process.stdin.write(b'x'); process.stdin.close()
        while exited() is None:
            if cancelled():
                signal_group(signal.SIGTERM)
                deadline = time.monotonic() + 2
                while exited() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                raise Cancelled()
            time.sleep(0.1)
    finally:
        # Even an unexpectedly exited Git parent must not leave packing
        # descendants behind when the owned maintenance lease is released.
        # WNOWAIT keeps this exact group identity reserved until this signal.
        signal_group(signal.SIGKILL)
        if not process.stdin.closed:
            process.stdin.close()
        code = process.wait()
    if code:
        raise subprocess.CalledProcessError(code, REPACK)



class Worker:
    """Independent periodic lane; cleanup idle does not wait for this daemon.

    schedule() and close() do no filesystem work. The worker checks even when
    protocol state is unchanged. Only explicit shutdown cancels an owned child;
    there is no elapsed-time packing cutoff.
    """
    def __init__(self, repo, *, threshold_bytes=DEFAULT_THRESHOLD, interval=DEFAULT_INTERVAL):
        if (type(threshold_bytes) is not int or threshold_bytes <= 0
                or not 0 < interval < float('inf')):
            raise ValueError('Git maintenance threshold and interval must be positive')
        self.repo, self.threshold_bytes, self.interval = Path(repo), threshold_bytes, interval
        self.stop = threading.Event()
        self.guard = threading.Lock()
        self.thread = None
        self.report = dict(scans=0, repacks=0, errors=0, busy=0, status='not_started')

    @property
    def summary(self):
        with self.guard:
            return dict(self.report)

    def schedule(self):
        with self.guard:
            if self.stop.is_set() or self.thread is not None:
                return
            self.thread = threading.Thread(target=self._run, name='canonical-git-maintenance', daemon=True)
            try:
                self.thread.start()
            except RuntimeError:
                self.thread = None
                raise

    def close(self):
        self.stop.set()

    def wait(self, timeout=None):
        with self.guard:
            thread = self.thread
        if thread is not None:
            thread.join(timeout)
            return not thread.is_alive()
        return True

    def _execute(self, repo, lease_fd):
        _execute_owned(repo, lease_fd, self.stop.is_set)


    def _run(self):
        while not self.stop.is_set():
            with self.guard:
                self.report.update(status='running', started_at=time.time())
            try:
                result = collect(self.repo, threshold_bytes=self.threshold_bytes,
                                 cancelled=self.stop.is_set, execute=self._execute)
                with self.guard:
                    self.report.pop('error', None); self.report.pop('error_type', None)
                    self.report.update(result, checked_at=time.time())
                    self.report['scans'] += 1
                    self.report['repacks'] += int(result['repacked'])
            except Cancelled:
                break
            except Busy as error:
                with self.guard:
                    self.report.update(status='busy', error=str(error), checked_at=time.time())
                    self.report['busy'] += 1
            except Exception as error:
                with self.guard:
                    self.report.update(status='error', error_type=type(error).__name__, checked_at=time.time())
                    self.report['errors'] += 1
                logging.getLogger(__name__).warning('Canonical incremental Git packing deferred', exc_info=True)
            self.stop.wait(self.interval)
        with self.guard:
            self.report['status'] = 'stopped'
