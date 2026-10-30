"""Managed checker launch mode and recovery of older confinement recipes.

New attempts use ordinary sandbox command guards and centrally managed checks.
Legacy FUSE/native boundaries remain only for recorded attempts and cleanup.
"""
from __future__ import annotations

from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import uuid

from .candidate import canonical_json
from .publication import atomic


class ConfinementError(RuntimeError):
    pass


NATIVE_BWRAP = Path('/usr/local/libexec/trellis/bwrap-0.11.1-trellis1')
NATIVE_BWRAP_ENV = 'TRELLIS_COMPILER_BWRAP'
MANAGED_AGENT_CHECKS_ENV = 'TRELLIS_MANAGED_AGENT_CHECKS'


def require_native_helper(path=None):
    """Only a protected, non-setuid host installation may perform mount setup."""
    path = Path(path or os.environ.get(NATIVE_BWRAP_ENV) or NATIVE_BWRAP)
    diagnostic = (f'recorded native-confinement attempt requires its original trusted helper at {path}; '
                  'finish or cancel that attempt before starting new managed work')
    if not path.is_absolute() or len(path.parts) < 2 or '..' in path.parts:
        raise ConfinementError(diagnostic)
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for index, component in enumerate(path.parts[1:]):
            last = index == len(path.parts) - 2
            flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            if not last:
                flags |= os.O_DIRECTORY
            child = os.open(component, flags, dir_fd=fd)
            os.close(fd)
            fd = child
            info = os.fstat(fd)
            if info.st_uid != 0 or info.st_mode & 0o022:
                raise ConfinementError(diagnostic + ' (the helper and its parents must be root-owned and not writable by other users)')
            if last:
                if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o6000 or not info.st_mode & 0o111:
                    raise ConfinementError(diagnostic + ' (expected an executable regular file without setuid/setgid)')
                try:
                    if os.getxattr(fd, 'security.capability'):
                        raise ConfinementError(diagnostic + ' (file capabilities are not permitted)')
                except OSError as error:
                    import errno
                    if error.errno not in (errno.ENODATA, errno.ENOTSUP):
                        raise
        return path
    except OSError as error:
        raise ConfinementError(diagnostic + f' ({error.strerror})') from error
    finally:
        os.close(fd)


def require_compiler_confinement():
    """Check host launch provenance, including previously recorded modes."""
    managed = os.environ.get(MANAGED_AGENT_CHECKS_ENV)
    native = os.environ.get(NATIVE_BWRAP_ENV)
    legacy = os.environ.get('TRELLIS_COMPILER_DATA_VIEW')
    if managed:
        if managed != '1' or native or legacy:
            raise ConfinementError('managed checker launch has inconsistent execution settings')
        return 'managed_commands'
    if native and legacy:
        raise ConfinementError('compiler confinement cannot use native binds and a legacy data view together')
    if native:
        return require_native_helper(native)
    if legacy:
        return require_view(legacy)
    raise ConfinementError('parallel dispatch requires host-managed checker setup before launching agents')


def confinement_recipe(root, previous, environment):
    root = str(Path(root).resolve())
    # A recorded legacy attempt retains its exact mount lease and recipe.
    # This branch cannot create a new FUSE attempt implicitly.
    if isinstance(previous, str):
        if previous != root:
            raise ConfinementError('legacy attempt has a different confinement root')
        return previous
    if previous is not None:
        if (not isinstance(previous, dict) or previous.get('root') != root
                or previous.get('kind') not in ('native_noexec', 'managed_commands')):
            raise ConfinementError('recorded attempt has an unknown or different checker launch recipe')
        return previous
    # Inherited settings cannot opt new work back into a retired mount mode.
    return dict(kind='managed_commands', root=root)


def require_view(view):
    view = Path(view).resolve(strict=True)
    if not os.path.ismount(view) or not os.statvfs(view).f_flag & os.ST_NOEXEC:
        raise ConfinementError("parallel dispatch requires a mounted noexec data view")
    return view


class PersistentDataView:
    """One host mount per run, with launcher-held leases across adoption.

    Preparing or reopening this object does not own the mount's lifetime.
    Every host attempt holds a shared lease until all descendants are reaped;
    close takes an exclusive lease and refuses if even one user remains.
    The parent is private host state, outside every agent-visible directory.
    """
    def __init__(self, root):
        self.root = Path(root).resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.view = self.root / 'view'

    @contextmanager
    def _management(self):
        with (self.root / 'management.lock').open('a+b') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield

    def _identity(self):
        require_view(self.view)
        # Bindfs mounts have no whitespace in the host-selected temporary
        # path normally; decode the kernel's escaping for all supported paths.
        def unescape(text):
            for code, character in [('\\040', ' '), ('\\011', '\t'), ('\\012', '\n'), ('\\134', '\\')]:
                text = text.replace(code, character)
            return text
        for line in Path('/proc/self/mountinfo').read_text().splitlines():
            before, after = line.split(' - ', 1)
            fields, backing = before.split(), after.split()
            if unescape(fields[4]) == str(self.view):
                if backing[0] != 'fuse.bindfs' or not {'noexec', 'nosuid', 'nodev'}.issubset(fields[5].split(',')):
                    raise ConfinementError('compiler data view has an unexpected filesystem or mount policy')
                return dict(mount_id=fields[0], device=fields[2], boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip())
        raise ConfinementError('compiler data view is absent from the host mount table')

    def _validate(self):
        if (self.root / 'closing.json').exists():
            raise ConfinementError('compiler data view is closing; finish its recorded cleanup before reuse')
        record = json.loads((self.root / 'mount.json').read_text())
        if record != self._identity():
            raise ConfinementError('compiler data view mount identity changed; preserve its users and investigate')
        return self.view

    def prepare(self, *, bindfs=None):
        with self._management():
            if (self.root / 'closing.json').exists():
                raise ConfinementError('compiler data view is closing; finish its recorded cleanup before reuse')
            if (self.root / 'mount.json').exists():
                return self._validate()
            intent = self.root / 'intent.json'
            expected = canonical_json(dict(source='/', destination=str(self.view), owner=os.geteuid()))
            if intent.exists() and intent.read_bytes() != expected:
                raise ConfinementError('compiler data view has a different mount intent')
            if not intent.exists():
                if os.path.ismount(self.view):
                    raise ConfinementError('refusing to adopt an unrecorded compiler data view')
                atomic(intent, expected)
            self.view.mkdir(exist_ok=True)
            # A crash after mounting but before recording is recoverable from
            # the durable intent; no launcher can use it before mount.json.
            if not os.path.ismount(self.view):
                helper = bindfs or shutil.which('bindfs')
                if not helper or not Path(helper).is_file():
                    raise ConfinementError('parallel compiler confinement requires the host bindfs helper')
                result = subprocess.run([str(helper), '--no-allow-other', '-o', 'noexec,nosuid,nodev', '/', str(self.view)],
                                        capture_output=True, text=True)
                if result.returncode:
                    raise ConfinementError('cannot mount compiler data view: ' + result.stderr.strip())
            atomic(self.root / 'mount.json', canonical_json(self._identity()))
            return self.view

    @contextmanager
    def use(self):
        with (self.root / 'users.lock').open('a+b') as lease:
            fcntl.flock(lease, fcntl.LOCK_SH)
            view = self._validate()
            from trellis.incremental_check import _process_identity
            receipt = self.root / 'users' / (uuid.uuid4().hex + '.json')
            atomic(receipt, canonical_json(_process_identity(os.getpid())))
            # An abnormal exit leaves a durable user record even when SIGKILL
            # closes its flock. Recovery must prove descendant cleanup; a dead
            # owner alone is never permission to unmount underneath its users.
            yield view
            receipt.unlink()
            fd = os.open(receipt.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)

    def close(self):
        with self._management(), (self.root / 'users.lock').open('a+b') as lease:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ConfinementError('compiler data view is still leased by an attempt; reap its users before unmounting') from None
            if any((self.root / 'users').glob('*.json')):
                raise ConfinementError('compiler data view has unreconciled users; preserve their cleanup evidence')
            closing = self.root / 'closing.json'
            if (self.root / 'mount.json').exists():
                recorded = json.loads((self.root / 'mount.json').read_text())
                if os.path.ismount(self.view):
                    if recorded != self._identity():
                        raise ConfinementError('compiler data view mount identity changed during cleanup')
                elif not closing.exists():
                    raise ConfinementError('compiler data view disappeared without a recorded cleanup')
                atomic(closing, canonical_json(recorded))
            elif os.path.ismount(self.view):
                raise ConfinementError('recover the recorded mount intent before closing this data view')
            if os.path.ismount(self.view):
                result = subprocess.run(['fusermount3', '-u', str(self.view)], capture_output=True, text=True)
                if result.returncode:
                    raise ConfinementError('compiler data view remains mounted; preserve its users: ' + result.stderr.strip())
            (self.root / 'mount.json').unlink(missing_ok=True)
            (self.root / 'intent.json').unlink(missing_ok=True)
            closing.unlink(missing_ok=True)
            fd = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)


@contextmanager
def data_view(parent, *, bindfs=None):
    """Own a view for bounded users; never lazily detach live use.

    All agent and host-bridge users must finish before leaving this context.
    A production coordinator which parks/adopts attempts must retain the mount
    across its restart, rather than tie this context to the event-loop lifetime.
    """
    helper = bindfs or shutil.which("bindfs")
    if not helper or not Path(helper).is_file():
        raise ConfinementError("parallel compiler confinement requires the host bindfs helper")
    parent = Path(parent).resolve()
    parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    root = Path(tempfile.mkdtemp(prefix="noexec-", dir=parent))
    mount = root / "view"
    mount.mkdir()
    # Alias host paths only as sources of the ordinary sandbox's allowlisted
    # binds. The view root itself must never be bound into the agent namespace.
    result = subprocess.run([str(helper), "--no-allow-other", "-o", "noexec,nosuid,nodev", "/", str(mount)],
                            capture_output=True, text=True)
    if result.returncode:
        shutil.rmtree(root)
        raise ConfinementError("cannot mount compiler data view: " + result.stderr.strip())
    try:
        yield require_view(mount)
    finally:
        result = subprocess.run(["fusermount3", "-u", str(mount)], capture_output=True, text=True)
        if result.returncode:
            raise ConfinementError("compiler data view remains mounted; preserve it and reap its users: " + result.stderr.strip())
        shutil.rmtree(root)


def restrict_bwrap(command, *, view=None, native_helper=None, executable_roots, scratch, compiler):
    """Rewrite a host-generated bubblewrap argv before its final command.

    Executable roots must be explicit trusted immutable runtime paths. Everything
    else, including Lean toolchains and downloaded/copied binaries, is data.
    The native guard checks the *resulting namespace*, not this argv description.
    """
    if compiler not in ("managed", "none"):
        raise ConfinementError("unknown task compiler capability")
    if native_helper is not None and view:
        raise ConfinementError('choose native compiler confinement or a legacy data view')
    if native_helper is None and not view:
        raise ConfinementError('compiler confinement requires a native helper or a recorded legacy data view')
    native = require_native_helper(native_helper) if native_helper is not None else None
    view = None if native else require_view(view)
    scratch = Path(scratch).resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    empty = scratch / "empty"
    empty.mkdir(exist_ok=True)
    zero = scratch / "zero-denied"
    zero.touch(exist_ok=True)
    shm = scratch / "shm"
    shm.mkdir(exist_ok=True)
    roots = [Path(p).resolve(strict=True) for p in executable_roots]
    if any(p == Path("/") for p in roots):
        raise ConfinementError("the entire filesystem cannot be executable")
    def data(path):
        p = Path(path).resolve(strict=True)
        return str(p if native else view / p.relative_to("/"))
    def data_bind(readonly):
        return ('--ro-noexec-bind' if readonly else '--noexec-bind') if native else ('--ro-bind' if readonly else '--bind')
    def trusted(path):
        p = Path(path).resolve(strict=True)
        return any(p == root or p.is_relative_to(root) for root in roots)
    args = list(command)
    if not args or Path(args[0]).name != "bwrap":
        raise ConfinementError("compiler confinement requires bubblewrap")
    output = [str(native) if native else args[0]]
    executable_destinations = []
    compiler_envs = {"TRELLIS_CHECKER_SOCKET", "TRELLIS_CHECKER_TOKEN", "TRELLIS_CHECK_JOBS_SOCKET",
                    "TRELLIS_CHECK_JOBS_TOKEN", "TRELLIS_PHASE0_CHECKER_SOCKET", "TRELLIS_PHASE0_CHECKER_TOKEN",
                    "INCREMENTAL_CHECK_ACTIVE_PREWARM_SOCK", "TRELLIS_INCREMENTAL_PREWARM", "TRELLIS_PREWARM_NODE"}
    hidden_sockets = {os.environ[name] for name in compiler_envs if name.endswith(("SOCKET", "SOCK"))
                      and os.environ.get(name)} if compiler == "none" else set()
    i = 1
    one = {"--dir", "--chdir", "--unsetenv", "--cap-drop", "--cap-add", "--remount-ro", "--hostname", "--uid", "--gid", "--perms", "--size"}
    zero_args = {"--die-with-parent", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-net", "--unshare-user", "--new-session", "--clearenv", "--disable-userns"}
    two = {"--setenv", "--symlink"}
    while i < len(args) and args[i].startswith("--"):
        option = args[i]
        if option in ("--bind", "--ro-bind", "--dev-bind"):
            source, destination = args[i + 1:i + 3]
            if view is not None and view.is_relative_to(Path(source).resolve()):
                raise ConfinementError("the host data view must remain outside every agent-visible directory")
            if source in hidden_sockets:
                i += 3
                continue
            if destination == "/dev":
                output.extend(["--dev", "/dev"])
            elif stat.S_ISSOCK(Path(source).stat().st_mode):
                output.extend(["--ro-bind", source, destination])
            elif option == "--ro-bind" and trusted(source):
                output.extend([option, source, destination])
                executable_destinations.append(destination)
            else:
                if option == "--dev-bind":
                    raise ConfinementError("unexpected device bind in parallel agent sandbox")
                output.extend([data_bind(option == '--ro-bind'), data(source), destination])
                # FUSE does not proxy Unix socket connections. Bind only the
                # actual socket inodes, never their possibly executable parent.
                if not native and Path(source).is_dir():
                    for child in Path(source).iterdir():
                        if child.is_socket() and str(child) not in hidden_sockets:
                            output.extend(["--ro-bind", str(child), str(Path(destination) / child.name)])
            i += 3
        elif option == "--proc":
            output.extend(args[i:i + 2] + ["--remount-ro", args[i + 1]])
            i += 2
        elif option == "--dev":
            output.extend(args[i:i + 2]); i += 2
        elif option == "--tmpfs":
            output.extend([data_bind(True), data(empty), args[i + 1]]); i += 2
        elif option in one:
            output.extend(args[i:i + 2]); i += 2
        elif option in two:
            # Withholding a token means removing it from the argv too. A
            # trailing unsetenv alone would leak it through saved launch scripts.
            if not (compiler == "none" and option == "--setenv" and args[i + 1] in compiler_envs):
                output.extend(args[i:i + 3])
            i += 3
        elif option in zero_args:
            output.append(option); i += 1
        else:
            raise ConfinementError(f"unclassified sandbox option for compiler confinement: {option}")
    guard = Path(__file__).with_name("compiler_guard.py").resolve()
    output.extend([data_bind(True), data(guard), str(guard),
                   data_bind(True), data(zero), "/dev/zero",
                   data_bind(False), data(shm), "/dev/shm", "--remount-ro", "/dev",
                   "--remount-ro", "/", "--setenv", "NODE_OPTIONS", "--jitless",
                   "--setenv", "TRELLIS_COMPILER_CAPABILITY", compiler,
                   "--unsetenv", "TRELLIS_COMPILER_DATA_VIEW", "--unsetenv", NATIVE_BWRAP_ENV])
    if compiler == "none":
        for name in sorted(compiler_envs):
            output.extend(["--unsetenv", name])
    output.extend(["/usr/bin/python3", "-I", "-S", str(guard), json.dumps(executable_destinations), *args[i:]])
    return output
