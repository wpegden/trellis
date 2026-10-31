#!/usr/bin/env python3
"""Inspect, run, and ratify a completed PV generation's revision proposal."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from trellis.pv_revision import lifecycle
from trellis.pv_revision.driver import Driver, validate_human_reason
from trellis.pv_revision.validation import ProductionChecker


def read_decision(stream):
    from trellis.pv_revision.code_edits import MAX_REQUEST_BYTES
    raw = stream.read(MAX_REQUEST_BYTES + 1)
    if len(raw) > MAX_REQUEST_BYTES:
        raise ValueError('Encoded code amendment exceeds 8 MiB')
    value = json.loads(raw)
    if not isinstance(value, dict) or not {'action', 'digest'} <= set(value) or set(value) - {
            'action', 'digest', 'reason', 'amendment', 'code_edits', 'comment_only'}:
        raise ValueError('Invalid human decision fields')
    if not all(isinstance(value[k], str) for k in ('action', 'digest')):
        raise ValueError('Action and digest must be text')
    validate_human_reason(value['action'], value.get('reason', ''))
    return value


def schedule(runtime: Path) -> None:
    from scripts.pv_native_create import runtime_job
    from trellis.runtime.bridge import any_halt_marker_present
    if not lifecycle.read_handoff(runtime) or any_halt_marker_present(runtime):
        raise ValueError("No resumable PV revision handoff")
    owner = runtime_job(runtime)
    if not owner:
        raise ValueError("A standalone revision uses the run command")
    job_dir, job, _ = owner
    active = Path(job["runtime_root"]).resolve() == runtime.resolve()
    pending = job.get("revision", {})
    if not active and not (pending.get("pending") and pending.get("predecessor_runtime") == str(runtime)):
        raise ValueError("This revision belongs to a retired generation")
    subprocess.run(["bash", str(ROOT / "scripts/pv_create_run.sh"), "reopen", job["slug"]],
                   check=True, env={**os.environ, "TRELLIS_PROJECTS_ROOT": str(job_dir.parent.parent.parent)})


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for action in ("status", "package", "review", "export", "init", "run", "step", "schedule", "pause"):
        sub.add_parser(action).add_argument("runtime", type=Path)
    view = sub.add_parser("review-file")
    view.add_argument("runtime", type=Path)
    view.add_argument("--digest", required=True)
    view.add_argument("--path", required=True)
    view.add_argument("--mode", default="diff")
    view.add_argument("--context", type=int, default=3)
    view.add_argument("--offset", type=int, default=0)
    sub.add_parser("campaign-status").add_argument("runtime", type=Path)
    campaign = sub.add_parser("campaign-export")
    campaign.add_argument("runtime", type=Path)
    campaign.add_argument("--allow-incomplete", action="store_true")
    decide = sub.add_parser("decide")
    decide.add_argument("runtime", type=Path)
    decide.add_argument("--action", choices=("accept", "deny", "amend"), required=True)
    decide.add_argument("--digest", required=True)
    decide.add_argument("--reason", default="", help="Required for deny or amend; optional for accept")
    decide.add_argument("--amendment")
    sub.add_parser("decide-json").add_argument("runtime", type=Path)
    repackage = sub.add_parser('repackage-review')
    repackage.add_argument('runtime', type=Path)
    repackage.add_argument('--digest', required=True)
    repackage.add_argument('--apply', action='store_true')
    repackage.add_argument('--state-digest')
    repackage.add_argument('--separation-reason', required=True,
                           help='Explain how production independence from the excluded evidence was checked')
    redirect = sub.add_parser('comment-amendment')
    redirect.add_argument('runtime', type=Path)
    redirect.add_argument('--digest', required=True)
    redirect.add_argument('--reason', required=True)
    redirect.add_argument('--apply', action='store_true')
    redirect.add_argument('--state-digest')
    for action in ("retry", "reaudit"):
        command = sub.add_parser(action)
        command.add_argument("runtime", type=Path)
        command.add_argument("--reason", required=True)
    args = parser.parse_args(argv)
    runtime = args.runtime.resolve()
    root = lifecycle.revision_root(runtime)
    try:
        if args.command == "status":
            print(json.dumps(lifecycle.status(runtime)))
        elif args.command == "campaign-status":
            from trellis.pv_revision.archive import campaign_status
            print(json.dumps(campaign_status(runtime)))
        elif args.command == "campaign-export":
            from trellis.pv_revision.archive import export_campaign
            path = export_campaign(runtime, allow_incomplete=args.allow_incomplete)
            with path.open("rb") as archive:
                sha256 = hashlib.file_digest(archive, "sha256").hexdigest()
            print(json.dumps({"path": str(path), "sha256": sha256}))
        elif args.command == "schedule":
            schedule(runtime)
        elif args.command == "pause":
            root.mkdir(exist_ok=True)
            (root / "pause").touch()
        elif args.command == "review-file":
            from trellis.pv_revision.review import file_view
            print(json.dumps(file_view(root, args.digest, args.path, mode=args.mode, context=args.context, offset=args.offset)))
        elif args.command in {"package", "review", "export"}:
            driver = Driver(root)
            driver._current()
            if driver.state["status"] not in {"awaiting_human", "accepted_for_modeling", "successor_preparing", "successor_started", "all_requirements_retired"}:
                raise ValueError("Revision has no reviewable human package")
            package = driver._package(driver.state)
            if package != driver.state.get("human_package"):
                raise ValueError("Human package no longer matches the candidate")
            if args.command == "review":
                from trellis.pv_revision.review import review_index
                print(json.dumps(review_index(package)))
            elif args.command == "export":
                from trellis.pv_revision.store import export_review_package
                print(json.dumps({"path": str(export_review_package(root, driver.state)), "digest": package["digest"]}))
            else:
                print(json.dumps(package))
        else:
            # Serialize operator decisions against the automatic controller.
            from scripts.pv_native_create import runtime_job, job_lock
            from contextlib import nullcontext
            owner = runtime_job(runtime)
            with job_lock(owner[0]) if owner else nullcontext():
                if args.command not in {"decide", "decide-json", "retry", "reaudit", "repackage-review", "comment-amendment"}:
                    lifecycle.initialize_runtime(runtime)
                with (root / "runner.lock").open("a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    driver = Driver(root, checker=ProductionChecker(root, json.loads((root / "state.json").read_text())["tools"]))
                    if args.command == 'comment-amendment':
                        print(json.dumps(driver.redirect_comment_amendment(digest=args.digest, reason=args.reason,
                            apply=args.apply, state_digest=args.state_digest)))
                        return 0
                    if args.command == 'repackage-review':
                        print(json.dumps(driver.repackage_review(digest=args.digest, separation_reason=args.separation_reason,
                                                               apply=args.apply, state_digest=args.state_digest)))
                        return 0
                    elif args.command == "decide":
                        driver.decide(args.action, digest=args.digest, reason=args.reason, amendment=args.amendment)
                    elif args.command == "decide-json":
                        driver.decide(**read_decision(sys.stdin.buffer))
                    elif args.command in {"retry", "reaudit"}:
                        from trellis.runtime.bridge import any_halt_marker_present
                        if any_halt_marker_present(runtime) or any_halt_marker_present(root.with_name(root.name + "-runtime")):
                            raise ValueError("Resolve the preserved runtime halt before retrying revision work")
                        driver.recover(reason=args.reason, reaudit=args.command == "reaudit")
                    elif args.command in {"run", "step"}:
                        (root / "pause").unlink(missing_ok=True)
                        while driver.state["status"] == "active" and not (root / "pause").exists():
                            driver.step()
                            if args.command == "step":
                                break
                    print(json.dumps(lifecycle.status(runtime)))
        return 0
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": str(error)}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
