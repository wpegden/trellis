#!/usr/bin/env python3
"""Build a static public Trellis tablet viewer from a finished tablet repo.

This is the orchestration wrapper for `export_public_tablet_viewer.py`.
It prepares the Lean build, precomputes recursive Mathlib imports, exports
the viewer, and optionally writes a `.tar.gz` artifact.

By default, Lean/Lake work is deliberately throttled: one Lake job, one Lean
thread, `nice -n 19`, idle I/O priority, and CPU affinity to core 0. Use
`--no-throttle` only on a machine where saturating Lean is acceptable.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path
from typing import Iterable


ROOT_DIR = Path(__file__).resolve().parents[1]
PRECOMPUTE = ROOT_DIR / "scripts" / "precompute_tablet_mathlib_imports.py"
EXPORTER = ROOT_DIR / "scripts" / "export_public_tablet_viewer.py"


def _run(cmd: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> None:
    print("+ " + " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=str(cwd), env=env, check=True)


def _throttle_prefix(args: argparse.Namespace) -> list[str]:
    if args.no_throttle:
        return []
    prefix = ["nice", "-n", str(args.nice)]
    if shutil.which("ionice"):
        prefix.extend(["ionice", "-c3"])
    if args.cpu and shutil.which("taskset"):
        prefix.extend(["taskset", "-c", args.cpu])
    return prefix


def _lean_env(args: argparse.Namespace) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("LEAN_NUM_THREADS", "1")
    env.setdefault("OMP_NUM_THREADS", "1")
    env.setdefault("LAKE_JOBS", str(args.lake_jobs))
    return env


def _maybe_title_arg(args: argparse.Namespace) -> list[str]:
    return ["--title", args.title] if args.title else []


def _maybe_github_arg(args: argparse.Namespace) -> list[str]:
    return ["--github-base", args.github_base] if args.github_base else []


def _tar_viewer(out: Path, tar_path: Path) -> None:
    tar_path.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(tar_path, "w:gz") as archive:
        for path in sorted(out.rglob("*")):
            archive.add(path, arcname=path.relative_to(out))
    print(f"wrote {tar_path}")


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", help="finished Trellis/tablet repo containing Tablet/")
    parser.add_argument("out", help="output directory for the static viewer")
    parser.add_argument("--title", default="", help="viewer title; default is README heading or repo name")
    parser.add_argument(
        "--github-base",
        default="",
        help="base GitHub blob URL, e.g. https://github.com/<owner>/<repo>/blob/<branch>",
    )
    parser.add_argument(
        "--semantic",
        choices=["auto", "state", "lean", "skip"],
        default="auto",
        help=(
            "semantic-closure source: auto uses supervisor_state.json if present, "
            "else Lean; state requires/uses a state JSON; lean forces Lean; skip omits closures"
        ),
    )
    parser.add_argument(
        "--semantic-state-json",
        default="",
        help="explicit completed supervisor_state.json for semantic closures",
    )
    parser.add_argument("--timeout-secs", type=float, default=1800.0)
    parser.add_argument("--semantic-batch-size", type=int, default=1)
    parser.add_argument("--semantic-sleep-secs", type=float, default=3.0)
    parser.add_argument("--lake-jobs", type=int, default=1)
    parser.add_argument("--nice", type=int, default=19)
    parser.add_argument("--cpu", default="0", help="CPU affinity for throttled Lean/Lake commands")
    parser.add_argument("--no-throttle", action="store_true", help="do not wrap Lean/Lake commands in nice/ionice/taskset")
    parser.add_argument("--no-cache-get", action="store_true", help="skip `lake exe cache get`")
    parser.add_argument("--no-build", action="store_true", help="skip `lake build Tablet`")
    parser.add_argument("--no-tar", action="store_true", help="do not write OUT.tar.gz")
    parser.add_argument(
        "--mathlib-imports-json",
        default="",
        help="reuse an existing precomputed Mathlib import JSON instead of writing OUT/data/mathlib-imports.json",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)

    repo = Path(args.repo).expanduser().resolve()
    out = Path(args.out).expanduser().resolve()
    if not (repo / "Tablet").is_dir():
        raise SystemExit(f"Tablet directory not found: {repo / 'Tablet'}")
    if not EXPORTER.is_file():
        raise SystemExit(f"exporter not found: {EXPORTER}")
    if not PRECOMPUTE.is_file():
        raise SystemExit(f"mathlib precompute script not found: {PRECOMPUTE}")

    out.mkdir(parents=True, exist_ok=True)
    data_dir = out / "data"
    data_dir.mkdir(parents=True, exist_ok=True)
    env = _lean_env(args)
    prefix = _throttle_prefix(args)

    if not args.no_cache_get:
        _run([*prefix, "lake", "exe", "cache", "get"], cwd=repo, env=env)
    if not args.no_build:
        _run([*prefix, "lake", "build", "Tablet"], cwd=repo, env=env)

    mathlib_json = (
        Path(args.mathlib_imports_json).expanduser().resolve()
        if args.mathlib_imports_json
        else data_dir / "mathlib-imports.json"
    )
    if not args.mathlib_imports_json:
        _run([sys.executable, str(PRECOMPUTE), str(repo), str(mathlib_json)], cwd=ROOT_DIR)

    export_cmd = [
        *prefix,
        sys.executable,
        str(EXPORTER),
        str(repo),
        str(out),
        *_maybe_title_arg(args),
        *_maybe_github_arg(args),
        "--timeout-secs",
        str(args.timeout_secs),
        "--semantic-batch-size",
        str(args.semantic_batch_size),
        "--semantic-sleep-secs",
        str(args.semantic_sleep_secs),
        "--mathlib-imports-json",
        str(mathlib_json),
    ]
    if args.semantic == "skip":
        export_cmd.append("--skip-semantic-closure")
    elif args.semantic == "lean":
        export_cmd.append("--force-lean-semantic")
    elif args.semantic in {"state", "auto"} and args.semantic_state_json:
        export_cmd.extend(["--semantic-state-json", str(Path(args.semantic_state_json).expanduser().resolve())])
    elif args.semantic == "state":
        candidate = repo / ".trellis-history" / "supervisor_state.json"
        if not candidate.is_file():
            raise SystemExit(f"--semantic state requested, but no state JSON found at {candidate}")
        export_cmd.extend(["--semantic-state-json", str(candidate)])

    _run(export_cmd, cwd=ROOT_DIR, env=env)

    if not args.no_tar:
        _tar_viewer(out, out.with_suffix(out.suffix + ".tar.gz") if out.suffix else Path(str(out) + ".tar.gz"))

    print(f"viewer directory: {out}")
    print(f"deploy example: rsync -av --delete {out}/ user@host:/path/to/public-web-dir/<name>/")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
