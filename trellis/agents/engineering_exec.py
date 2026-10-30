"""Stdlib-only, isolated entrypoint for an admitted engineering namespace."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import sys


ENV_KEYS = frozenset({"LANG", "LC_ALL", "LC_CTYPE", "TZ", "OPENAI_API_KEY", "OPENAI_BASE_URL",
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy",
    "all_proxy", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
    "CODEX_PERMISSION_PROFILE", "CODEX_SANDBOX_NETWORK_DISABLED"})


def project_environment(inherited: dict, home: str, runtime_path: str, work: str | None = None) -> dict[str, str]:
    """Pure projection; callers admit the explicit home and runtime selections."""
    return {**{key: value for key, value in inherited.items() if key in ENV_KEYS},
        "PATH": runtime_path, "HOME": home, "PWD": work or home, "CODEX_HOME": home + "/.codex",
        "CARGO_HOME": home + "/.cargo", "CARGO_TARGET_DIR": home + "/target",
        "PYTHONDONTWRITEBYTECODE": "1", "TERM": "xterm-256color",
        "TMPDIR": "/trellis-tmp", "TMP": "/trellis-tmp", "TEMP": "/trellis-tmp",
        "TRELLIS_SANDBOX_ROLE": "engineering"}


def digest(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def open_regular(path: Path):
    """Open each component without following links, including renamed parents."""
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("Engineering binding requires absolute ordinary paths")
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in path.parts[1:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = child
        opened = os.open(path.name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
        if not stat.S_ISREG(os.fstat(opened).st_mode):
            os.close(opened)
            raise ValueError("Engineering binding requires regular files")
        return os.fdopen(opened, "rb")
    finally:
        os.close(fd)


def file_identity(path: Path) -> dict:
    with open_regular(path) as stream:
        info = os.fstat(stream.fileno())
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
        after = os.fstat(stream.fileno())
        if (info.st_size, info.st_mtime_ns, info.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError("Engineering admitted file changed while reading")
        return {"sha256": digest, "mode": stat.S_IMODE(info.st_mode)}


def tree_identity(root: Path, ignored=()) -> dict:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("Engineering binding requires ordinary directories")
    def refuse_error(error):
        raise error
    result = {}
    for directory, dirs, files in os.walk(root, onerror=refuse_error):
        dirs[:] = sorted(name for name in dirs if (Path(directory) / name).relative_to(root).as_posix() not in ignored)
        for name in dirs:
            if (Path(directory) / name).is_symlink():
                raise ValueError("Engineering binding directory is aliased")
        for name in sorted(files):
            if (Path(directory) / name).relative_to(root).as_posix() not in ignored:
                path = Path(directory) / name
                result[path.relative_to(root).as_posix()] = file_identity(path)
    return result


def verify(binding_path: Path, expected: str, argv: list[str], inherited: dict) -> dict:
    with open_regular(binding_path) as stream:
        raw = stream.read()
    if hashlib.sha256(raw).hexdigest() != expected:
        raise ValueError("Engineering launch binding changed")
    binding = json.loads(raw)
    if binding["argv"] != argv or not argv or not Path(argv[0]).is_absolute():
        raise ValueError("Engineering ordered launch command changed")
    if str(Path(__file__).resolve()) not in binding["files"]:
        raise ValueError("Engineering entrypoint is unbound")
    for name in binding["absent"]:
        if os.path.lexists(name):
            raise ValueError("Engineering excluded context appeared")
    for name, target in binding["links"].items():
        if os.readlink(name) != target:
            raise ValueError("Engineering admitted file link changed")
    for name, identity in binding["files"].items():
        if file_identity(Path(name)) != identity:
            raise ValueError("Engineering admitted file changed")
    for name, identity in binding["directories"].items():
        path = Path(name)
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode) or [info.st_dev, info.st_ino] != identity:
            raise ValueError("Engineering admitted directory changed")
    for tree in binding["trees"]:
        if tree_identity(Path(tree["path"]), tree["ignored"]) != tree["files"]:
            raise ValueError("Engineering admitted inputs changed")
    environment = project_environment(inherited, binding["home"], binding["runtime_path"], binding["work"])
    if digest(environment) != binding["environment_sha256"]:
        raise ValueError("Engineering allowed environment changed")
    return environment


def main(argv=None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binding", required=True, type=Path)
    parser.add_argument("--binding-sha256", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        if not args.command or args.command[0] != "--":
            raise ValueError("Engineering launch command is absent")
        command = args.command[1:]
        environment = verify(args.binding, args.binding_sha256, command, dict(os.environ))
        os.execve(command[0], command, environment)
    except (OSError, ValueError, KeyError, TypeError):
        # Neither provider credentials nor malformed binding contents are diagnostics.
        print("Engineering launch admission failed before provider execution", file=sys.stderr)
        return 125


if __name__ == "__main__":
    raise SystemExit(main())
