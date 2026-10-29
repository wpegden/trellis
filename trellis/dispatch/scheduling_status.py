"""Coalesce read-only scheduler polls, never admission or acceptance checks."""
import copy
import stat
import time


def source_inputs(repo):
    """Stat the same input surfaces as native dispatch_observation::verifier_inputs.

    This is only cache invalidation: never source evidence or acceptance input.
    Include path and inode/ctime as well as size/mtime, so atomic replacement,
    deletion, rename and edits that restore mtime all force a fresh native read.
    Nonregular/unreadable inputs disable caching instead of bypassing native
    diagnostics. The short age bound covers external toolchain/recovery inputs.
    """
    excluded = {'.git', '.lake', '.trellis-history', '.pytest_cache', '__pycache__',
                'node_modules', '.viewer', 'target', 'last_invalid', '.trellis-stop-after-checkpoint'}
    result = []
    def retain(path, value):
        result.append((path.relative_to(repo).as_posix(), value.st_dev, value.st_ino, value.st_mode,
                       value.st_size, value.st_mtime_ns, value.st_ctime_ns))
    def visit(directory):
        retain(directory, directory.lstat())
        for path in sorted(directory.iterdir()):
            path.name.encode('utf-8')  # Native inputs reject non-UTF8 names.
            if path.name in excluded:
                continue
            relative = path.relative_to(repo)
            if relative.parts[0] == '.trellis' and len(relative.parts) > 1 and relative.parts[1] not in ('artifacts', 'scripts'):
                continue
            value = path.lstat()
            if stat.S_ISDIR(value.st_mode):
                visit(path)
            elif stat.S_ISREG(value.st_mode):
                retain(path, value)
            else:
                raise OSError('nonregular scheduling input')
    try:
        visit(repo)
    except (OSError, UnicodeError):
        return None
    return tuple(result)


class SchedulingStatus:
    """A short-lived projection of one exact published state.

    The coordinator and verifier lane poll the same large native state. Keep
    their identical reads from taking the publication lock repeatedly. Callers
    still run all host controls and preparation on every poll; every native
    event, launch decision and publication uses its normal fresh native load.
    Reuse is bounded to 30 seconds after successful validation completes (by
    default), then the next poll repeats native load/recovery/identity
    diagnostics. A slow read must still coalesce the next queued poll. External
    dependencies outside the exact key use this diagnostic age bound; native
    event/admission/publication paths remain independently fresh.
    """
    def __init__(self, *, seconds=30, clock=time.monotonic):
        self.seconds, self.clock = seconds, clock
        self.key = self.value = None
        self.started = 0
        self.hits = self.misses = 0

    @staticmethod
    def signature(paths):
        try:
            return tuple((s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
                         for s in (p.stat() for p in paths))
        except OSError:
            return None  # Missing/unreadable inputs must reach the native guard.

    def read(self, fetch, paths, *, inputs=None):
        # Caller holds coordinator.lock across signatures, native read and copy.
        key = self.signature(paths)
        now = self.clock()
        # This inexpensive source/evidence observation avoids hiding an edited
        # canonical source or referent until the age bound. It is not a native
        # acceptance check and cannot authorize any mutation.
        if key is not None and inputs is not None:
            observed = inputs()
            key = (key, observed) if observed is not None else None
        if key is not None and key == self.key and 0 <= now - self.started < self.seconds:
            self.hits += 1
            return copy.deepcopy(self.value)
        self.misses += 1
        self.key = self.value = None
        value = fetch()  # Failures are never cached.
        after = self.signature(paths)
        if after is not None and inputs is not None:
            observed = inputs()
            after = (after, observed) if observed is not None else None
        if key is not None and key == after:
            retained = copy.deepcopy(value)
            completed = self.clock()
            # Starting the age before an expensive native read can publish an
            # already-expired entry, serializing identical queued polls again.
            # Hits do not renew this completion-based diagnostic deadline.
            if completed >= now:
                self.value, self.key, self.started = retained, key, completed
        return value
