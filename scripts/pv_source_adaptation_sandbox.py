#!/usr/bin/env python3
"""Run production PV extraction in the sealed source-adaptation checker role."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from trellis.config import SandboxConfig
from trellis.sandbox import wrap_command


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--burst-home", type=Path, required=True)
    parser.add_argument("--readonly", type=Path, action="append", default=[])
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        parser.error("a command is required after --")

    work_dir = args.work_dir.resolve(strict=True)
    burst_home = args.burst_home.resolve(strict=True)
    readonly = [path.resolve(strict=True) for path in args.readonly]
    previous = os.environ.get("TRELLIS_SANDBOX_EXTRA_RO_PATHS")
    try:
        os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = os.pathsep.join(
            str(path) for path in readonly
        )
        wrapped = wrap_command(
            command,
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
            work_dir=work_dir,
            burst_home=burst_home,
            role="source_adaptation_checker",
        )
    finally:
        if previous is None:
            os.environ.pop("TRELLIS_SANDBOX_EXTRA_RO_PATHS", None)
        else:
            os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = previous
    return subprocess.run(wrapped, cwd=work_dir, env={}, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
