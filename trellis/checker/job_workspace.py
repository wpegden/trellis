"""Host-captured advisory inputs and private build workspaces.

No candidate paths or commands are accepted over the job protocol. Workspace
roots are registered by the trusted launcher. Source reads use directory fds
and O_NOFOLLOW, and compilation sees only the captured source epoch. Advisory
artifacts are never imported into the authoritative checker workspace.
"""
from __future__ import annotations
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat

from trellis.check_job import SCRATCH_ROOTS

MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_INPUT_FILES = 16384
CONFIG_FILES = {"lean-toolchain", "lakefile.lean", "lakefile.toml", "lake-manifest.json", "trellis.config.json", "trellis.policy.json"}


def capture(repo, *, metadata_only=False):
    result = {}
    total = 0
    directories = 0

    def visit(fd, prefix, *, top=False):
        nonlocal total, directories
        directories += 1
        if directories > MAX_INPUT_FILES:
            raise ValueError("check inputs exceed the 16384 directory limit")
        for name in sorted(os.listdir(fd)):
            if metadata_only and name != "lean-toolchain":
                continue
            rel = f"{prefix}/{name}" if prefix else name
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if stat.S_ISDIR(info.st_mode):
                # Capture the Lean tree and scratch inputs, not build caches,
                # git objects, logs, or project reference material.
                wanted = (not name.startswith(".") and name not in {"node_modules", "target", "reference", "crate", "Dormant"}) or rel in {".trellis", ".trellis/scratch", ".trellis/scripts"}
                if prefix == ".trellis":
                    wanted = name in {"scratch", "scripts", "tmp", "stuck-math-audit"}
                elif prefix == ".trellis/tmp":
                    wanted = name == "correspondence"
                if not wanted:
                    continue
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                try:
                    visit(child, rel)
                finally:
                    os.close(child)
            elif prefix == ".trellis/tmp":
                # Only the named correspondence subtree is an input; the
                # rest of tmp includes runtime and sandbox bookkeeping.
                continue
            # The protected PV manifest selects immutable compiled libraries.
            # Keep it in the captured epoch so the compiler's existing pinned
            # library mounts apply to advisory mirrors as well.
            elif rel.endswith(".lean") or (top and name in CONFIG_FILES) or rel in {
                ".trellis/scripts/check.py", "modeling-input/manifest.json"
            }:
                if not stat.S_ISREG(info.st_mode):
                    raise ValueError(f"check input must be a regular file: {rel}")
                file_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
                try:
                    if not stat.S_ISREG(os.fstat(file_fd).st_mode):
                        raise ValueError(f"check input changed type: {rel}")
                    with os.fdopen(file_fd, "rb", closefd=False) as stream:
                        data = stream.read(MAX_INPUT_BYTES - total + 1)
                finally:
                    os.close(file_fd)
                total += len(data)
                if total > MAX_INPUT_BYTES or len(result) >= MAX_INPUT_FILES:
                    raise ValueError("check inputs exceed the 64 MiB / 16384 file limit")
                result[rel] = base64.b64encode(data).decode("ascii")
            elif stat.S_ISLNK(info.st_mode) and (rel in {"Tablet", "modeling-input"} or prefix == "Tablet"):
                raise ValueError(f"symlink in check input: {rel}")

    root = os.open(repo, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        visit(root, "", top=True)
    finally:
        os.close(root)
    encoded = json.dumps(result, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest(), encoded


def prepare_mirror(source, mirror, snapshot):
    from trellis.supervisor_workspace import _ensure_supervisor_lake_packages
    mirror.mkdir(parents=True, exist_ok=True)
    # Package inputs are sandbox read-only in the registered workspace and in
    # this workspace. Own mutable artifacts separately; never hardlink them.
    package_marker = mirror.parent / "packages-ready"
    if not package_marker.exists():
        _ensure_supervisor_lake_packages(source, mirror)
        package_marker.touch()
    marker = mirror.parent / "input-files.json"
    first_capture = not marker.exists()
    previous = json.loads(marker.read_text()) if marker.exists() else []
    inputs = json.loads(snapshot)
    changed = set()
    for rel in previous:
        if rel not in inputs:
            (mirror / rel).unlink(missing_ok=True)
            changed.add(rel)
    for rel, value in inputs.items():
        data = base64.b64decode(value)
        dst = mirror / rel
        if dst.is_file() and not dst.is_symlink() and dst.read_bytes() == data:
            continue
        dst.parent.mkdir(parents=True, exist_ok=True)
        # Only the service can write source paths (compiler binds are RO).
        dst.write_bytes(data)
        changed.add(rel)
    (mirror / ".trellis/scratch").mkdir(parents=True, exist_ok=True)
    if first_capture and (mirror / 'Tablet').is_dir():
        _seed_tablet_build(source, mirror)
    marker.write_text(json.dumps(list(inputs)))
    return changed


def _seed_tablet_build(source, mirror):
    """Seed an advisory mirror from validated supervisor outputs, once.

    Validation uses the captured mirror sources, including edits at submission.
    The existing seed routine copies into a fresh private supervisor and checks
    content closures, artifact bytes and replay attestations. Move only its
    validated build cache into this advisory workspace; no advisory output is
    sent back to the supervisor or treated as acceptance evidence.
    """
    from trellis.dispatch.workspace import _seed_authoritative_build
    from trellis.supervisor_workspace import authoritative_repo_path

    staging = authoritative_repo_path(mirror)
    build = mirror / '.lake/build'
    if staging.exists() or staging.is_symlink() or build.exists() or build.is_symlink():
        return 0
    try:
        count = _seed_authoritative_build(source, mirror)
        seeded = staging / '.lake/build'
        if count and seeded.is_dir():
            build.parent.mkdir(parents=True, exist_ok=True)
            seeded.rename(build)
        (mirror.parent / 'tablet-build-seed.json').write_text(json.dumps(
            dict(validated_nodes=count, source='registered workspace supervisor',
                 captured_sources=True, advisory_only=True)))
        return count
    finally:
        # This tree was created here for validation, never a running authority.
        if staging.exists():
            shutil.rmtree(staging)


def compiler_command(repo, args, *, readonly_build=False, lean_threads=None):
    from trellis.config import SandboxConfig
    from trellis.host_runtime import worker_elan_home
    from trellis.sandbox import wrap_command
    home = repo.parent / "home"
    home.mkdir(exist_ok=True)
    cmd = wrap_command(args, sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                       work_dir=repo, burst_home=home, role="lake_compiler")
    # Append overrides AFTER the normal writable binds, before the command.
    # A bind placed at argv[1] would be shadowed by a later parent mount.
    index = len(cmd) - len(args) - 2
    assert cmd[index] == "--chdir"
    extra = ["--setenv", "ELAN_HOME", str(worker_elan_home()),
             "--setenv", "LEAN_NUM_THREADS", lean_threads or os.environ.get("TRELLIS_LEAN_PARALLELISM", "6"),
             "--unsetenv", "TRELLIS_CHECKER_TOKEN", "--unsetenv", "TRELLIS_CHECKER_SOCKET",
             "--unsetenv", "TRELLIS_CHECK_JOBS_TOKEN", "--unsetenv", "TRELLIS_CHECK_JOBS_SOCKET"]
    for path in (repo / "Tablet", *(repo / root for root in SCRATCH_ROOTS)):
        if path.exists():
            extra += ["--ro-bind", str(path), str(path)]
    if readonly_build:
        build = repo / ".lake/build"
        if build.exists():
            extra += ["--ro-bind", str(build), str(build)]
    cmd[index:index] = extra
    return cmd
