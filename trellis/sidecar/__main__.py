"""CLI entry: ``python3 -m trellis.sidecar <runtime_root> --repo <live_repo>``.

Production wiring of the grunt MANAGER (queue redesign §2.4): bootstrap
one isolated workspace per grunt, then run the manager loop — assign
READY queue rows to free grunts as ``trellis.sidecar.attempt`` child
processes (own session each, so cancellation is one ``killpg``), cancel
attempts whose entry left the export, reap results. Inert (exit 0 with
a note) when the config block is absent or disabled.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Mapping

from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.daemon import (
    GruntSlot,
    ProcRootUnavailableError,
    SidecarDaemon,
    SingletonError,
    acquire_pid_lock,
)


def _grunt_dir(runtime_root: Path, grunt: int) -> Path:
    return Path(runtime_root) / "sidecar" / "grunts" / str(grunt)


def _make_spawn_fn(runtime_root: Path, config_path: Path):
    def spawn(
        grunt: int,
        row: Mapping[str, Any],
        export: Mapping[str, Any],
        attempt_id: str,
    ) -> GruntSlot:
        gdir = _grunt_dir(runtime_root, grunt)
        gdir.mkdir(parents=True, exist_ok=True)
        result_path = gdir / f"result-{attempt_id}.json"
        log_path = gdir / f"attempt-{attempt_id}.log"
        cmd = [
            sys.executable,
            "-m",
            "trellis.sidecar.attempt",
            str(runtime_root),
            "--grunt",
            str(grunt),
            "--node",
            str(row["node"]),
            "--entry-seq",
            str(row["entry_seq"]),
            "--snapshot",
            str(export.get("snapshot_sha", "")),
            "--node-file-sha",
            str(row.get("node_file_sha256", "")),
            "--statement-prefix-sha",
            str(row.get("statement_prefix_sha256", "")),
            "--attempt-id",
            attempt_id,
            "--result",
            str(result_path),
            "--config",
            str(config_path),
        ]
        with open(log_path, "ab") as log_file:
            proc = subprocess.Popen(
                cmd,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                # Own session: killpg(SIGTERM/SIGKILL) takes the whole
                # attempt down, warm lean server included (§2.3).
                start_new_session=True,
            )
        try:
            assigned_at_cycle = int(export.get("cycle", 0) or 0)
        except (TypeError, ValueError):
            assigned_at_cycle = 0
        return GruntSlot(
            grunt=grunt,
            node=str(row["node"]),
            entry_seq=int(row["entry_seq"]),
            attempt_id=attempt_id,
            started_at_ms=int(time.time() * 1000),
            proc=proc,
            result_path=result_path,
            # The export cycle this assignment came from: rides into the
            # published spent-generation outcome so a kernel rewind past
            # this cycle drops it.
            assigned_at_cycle=assigned_at_cycle,
            # Content the attempt is against, for the kernel's per-content
            # attempt cap (rides into every attempted.json row).
            node_file_sha256=str(row.get("node_file_sha256", "")),
        )

    return spawn


def _make_bootstrap_fn(repo: Path, runtime_root: Path):
    def bootstrap(grunt: int) -> None:
        from trellis.sidecar.workspace import bootstrap_workspace

        result = bootstrap_workspace(repo, runtime_root, grunt=grunt)
        print(
            f"sidecar: grunt {grunt} workspace ready (cloned={result.cloned}, "
            f"tablet artifacts copied={result.tablet_artifacts_copied})"
        )

    return bootstrap


def _make_reset_fn(runtime_root: Path):
    def reset(grunt: int, snapshot_sha: str) -> None:
        from trellis.sidecar.workspace import refresh_to_snapshot

        if not snapshot_sha:
            return
        result = refresh_to_snapshot(runtime_root, snapshot_sha, grunt=grunt)
        if not result.ok:
            print(f"sidecar: grunt {grunt} reset deferred: {result.reason}")

    return reset


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="trellis.sidecar")
    parser.add_argument("runtime_root", type=Path)
    parser.add_argument("--repo", type=Path, required=True, help="live tablet repo")
    parser.add_argument(
        "--config",
        type=Path,
        default=None,
        help="trellis.config.json (default: <repo>/trellis.config.json)",
    )
    args = parser.parse_args(argv)
    config_path = args.config or (args.repo / "trellis.config.json")
    config = SidecarConfig.load(config_path)
    if not config.enabled:
        print("sidecar: no enabled `sidecar` block in config; exiting (inert)")
        return 0
    # F5 startup gate: sandbox role configured + bwrap missing = refuse
    # to run compile steps at all (no silent unsandboxed fallback).
    from trellis.sidecar.workspace import SandboxUnavailableError, ensure_sandbox_available

    try:
        ensure_sandbox_available(config.sandbox_role, config.allow_unsandboxed)
    except SandboxUnavailableError as exc:
        print(f"sidecar daemon refusing to start: {exc}", file=sys.stderr)
        return 2
    try:
        _lock_fd = acquire_pid_lock(args.runtime_root)
    except SingletonError as exc:
        print(f"sidecar daemon refusing to start: {exc}", file=sys.stderr)
        return 2

    daemon = SidecarDaemon(
        runtime_root=args.runtime_root,
        config=config,
        spawn_fn=_make_spawn_fn(args.runtime_root, config_path),
        reset_workspace_fn=_make_reset_fn(args.runtime_root),
        # Bootstrap is now the daemon's to SEQUENCE: it runs after
        # adoption and only for grunts with no attempt still running in
        # their workspace (a bootstrap copies oleans and seeds sidecars
        # — a second writer into a tree that is mid-`lake build`).
        bootstrap_fn=_make_bootstrap_fn(args.repo, args.runtime_root),
    )
    try:
        daemon.run_forever()
    except ProcRootUnavailableError as exc:
        print(f"sidecar daemon refusing to start: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
