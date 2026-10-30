"""Trusted, pre-agent setup for a compiler-free execution namespace.

Run only inside the host-built bubblewrap namespace. Task storage must already
be mounted noexec by the host; only explicitly supplied, read-only
host runtime mounts permit execution. Writable copies and ELF-loader calls
therefore cannot execute a toolchain. A seccomp filter also excludes executable
anonymous memory, memfd and new mount namespaces. Node runs with --jitless.
Compiler processes use the separate, centrally admitted lake_compiler role.

This constrains native execution, not arbitrary computation in an interpreter.
It is not a CPU quota or a replacement for process and memory admission.
"""
from __future__ import annotations

import ctypes as C
import errno
import json
import os
from pathlib import Path
import platform
import resource
import stat
import sys


class Compare(C.Structure):
    _fields_ = [("arg", C.c_uint), ("op", C.c_int), ("a", C.c_uint64), ("b", C.c_uint64)]


def checked(result, operation):
    if result < 0:
        code = C.get_errno()
        raise OSError(code, f"compiler confinement: {operation}: {os.strerror(code)}")


def mounts(libc, executable_roots):
    """Verify the actual mount namespace. Never downgrade missing confinement."""
    if platform.machine() not in {"x86_64", "aarch64"}:
        raise RuntimeError("compiler confinement requires Linux x86_64 or aarch64")
    roots = [Path(p).resolve(strict=True) for p in executable_roots]
    for path in roots:
        if not os.statvfs(path).f_flag & os.ST_RDONLY:
            raise RuntimeError(f"compiler confinement requires a read-only executable mount: {path}")
    def unescape(path):
        for escaped, character in (("\\040", " "), ("\\011", "\t"), ("\\012", "\n"), ("\\134", "\\")):
            path = path.replace(escaped, character)
        return path
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split()
        path = Path(unescape(fields[4]))
        # mountinfo also lists covered mounts (e.g. the original /dev/zero).
        # Check the visible mount at the path; remount/unmount is prohibited
        # before untrusted code starts, so covered mounts cannot be recovered.
        try:
            visible = os.statvfs(path).f_flag
        except FileNotFoundError:
            continue
        flags = ({"ro"} if visible & os.ST_RDONLY else set()) | ({"noexec"} if visible & os.ST_NOEXEC else set())
        if "noexec" in flags:
            continue
        # The empty root is bubblewrap's read-only scaffold. Every mounted
        # child is independently checked, so this does not grant a subtree.
        resolved = path.resolve()
        if "ro" in flags and (path in (Path("/"), Path("/dev")) or any(resolved == p or resolved.is_relative_to(p) for p in roots)):
            continue
        # These kernel devices cannot map executable memory. /dev/zero is
        # deliberately absent: its mmap would bypass anonymous-mmap filtering.
        if path.as_posix() in ("/dev/null", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty") and stat.S_ISCHR(path.stat().st_mode):
            continue
        if "ro" in flags and stat.S_ISSOCK(path.stat().st_mode):
            continue
        raise RuntimeError(f"compiler confinement requires noexec task/data storage: {path}")
    if not os.statvfs("/proc").f_flag & os.ST_RDONLY:
        raise RuntimeError("compiler confinement requires read-only /proc (including process memory)")


def restrict_syscalls(libc):
    sec = C.CDLL("libseccomp.so.2", use_errno=True)
    sec.seccomp_init.argtypes = [C.c_uint32]
    sec.seccomp_init.restype = C.c_void_p
    sec.seccomp_syscall_resolve_name.argtypes = [C.c_char_p]
    sec.seccomp_rule_add_array.argtypes = [C.c_void_p, C.c_uint32, C.c_int, C.c_uint, C.POINTER(Compare)]
    sec.seccomp_load.argtypes = [C.c_void_p]
    sec.seccomp_release.argtypes = [C.c_void_p]
    ctx = sec.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW; bad architectures remain fatal.
    if not ctx:
        raise RuntimeError("cannot allocate compiler confinement filter")
    def deny(name, *comparisons, error=errno.EPERM):
        number = sec.seccomp_syscall_resolve_name(name.encode())
        if number == -1:
            return
        args = (Compare * len(comparisons))(*comparisons)
        result = sec.seccomp_rule_add_array(ctx, 0x50000 | error, number, len(args), args)
        if result < 0:
            raise RuntimeError(f"cannot restrict {name}: {os.strerror(-result)}")
    try:
        for name in ("mount", "umount2", "pivot_root", "move_mount", "open_tree", "fsopen", "fsmount",
                     "fspick", "mount_setattr", "unshare", "setns", "memfd_create", "execveat",
                     "personality", "ptrace", "process_vm_writev"):
            deny(name)
        deny("clone3", error=errno.ENOSYS)  # libc/providers can use the constrained clone fallback.
        for flag in (0x00020000, 0x02000000, 0x04000000, 0x08000000, 0x10000000,
                     0x20000000, 0x40000000, 0x00000080):
            deny("clone", Compare(0, 7, flag, flag))  # SCMP_CMP_MASKED_EQ
        for name in ("mmap", "mmap2"):
            deny(name, Compare(2, 7, 4, 4), Compare(3, 7, 0x20, 0x20))  # EXEC + ANONYMOUS
            deny(name, Compare(2, 7, 6, 6))  # no writable executable file mappings either
        for name in ("mprotect", "pkey_mprotect"):
            deny(name, Compare(2, 7, 4, 4))
        deny("shmat", Compare(2, 7, 0x8000, 0x8000))  # SHM_EXEC
        checked(libc.prctl(38, 1, 0, 0, 0), "no_new_privs")
        result = sec.seccomp_load(ctx)
        if result < 0:
            raise RuntimeError(f"cannot install compiler confinement filter: {os.strerror(-result)}")
    finally:
        sec.seccomp_release(ctx)


def drop_capabilities(libc):
    class Header(C.Structure):
        _fields_ = [("version", C.c_uint32), ("pid", C.c_int)]
    class Data(C.Structure):
        _fields_ = [(x, C.c_uint32) for x in ("effective", "permitted", "inheritable")]
    # Bounding-set drops require CAP_SETPCAP, which bwrap need not grant.
    # Clearing all current sets + no_new_privs is the required irreversible
    # boundary; namespace creation/mount operations are separately denied.
    checked(libc.capset(C.byref(Header(0x20080522, 0)), C.byref((Data * 2)())), "drop capabilities")


def restrict_descriptors():
    """An inherited host FD must not recover an executable data mount.

    In particular /proc/self/fd/1 can reopen a host-opened log file even when
    its pathname is hidden by a noexec bind. Reopen regular stdio through the
    visible noexec path before dropping the original descriptor.
    """
    import fcntl
    for fd in range(3):
        try:
            mode = os.fstat(fd).st_mode
        except OSError:
            continue
        if stat.S_ISREG(mode):
            if not os.fstatvfs(fd).f_flag & os.ST_NOEXEC:
                path = os.readlink(f"/proc/self/fd/{fd}")
                if not os.statvfs(path).f_flag & os.ST_NOEXEC:
                    raise RuntimeError(f"compiler confinement requires noexec redirected stdio: fd {fd}")
                flags = fcntl.fcntl(fd, fcntl.F_GETFL)
                replacement = os.open(path, (flags & (os.O_ACCMODE | os.O_APPEND)) | os.O_CLOEXEC)
                try:
                    os.lseek(replacement, os.lseek(fd, 0, os.SEEK_CUR), os.SEEK_SET)
                    os.dup2(replacement, fd)
                finally:
                    os.close(replacement)
        elif stat.S_ISCHR(mode):
            safe_devices = {os.stat(p).st_rdev for p in ('/dev/null', '/dev/full', '/dev/random', '/dev/urandom')}
            if os.fstat(fd).st_rdev not in safe_devices and not os.isatty(fd):
                raise RuntimeError(f"compiler confinement forbids inherited device fd {fd}")
        elif not (stat.S_ISFIFO(mode) or stat.S_ISSOCK(mode)):
            raise RuntimeError(f"compiler confinement does not support inherited descriptor {fd}")
    limit = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
    os.closerange(3, 1048576 if limit == resource.RLIM_INFINITY else limit)


def main():
    try:
        roots = json.loads(sys.argv[1])
        command = sys.argv[2:]
        if not command or not isinstance(roots, list) or not all(isinstance(p, str) for p in roots):
            raise ValueError("invalid host compiler confinement command")
        libc = C.CDLL(None, use_errno=True)
        mounts(libc, roots)
        restrict_descriptors()
        restrict_syscalls(libc)
        drop_capabilities(libc)
        os.execvpe(command[0], command, os.environ)
    except Exception as exc:
        print(f"trellis: compiler confinement failed: {exc}", file=sys.stderr)
        return 126


if __name__ == "__main__":
    sys.exit(main())
