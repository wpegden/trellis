"""Git checkpoint hook for the trellis supervisor runtime.

This is intentionally runtime-owned, not protocol-owned. It reads the
checkpoint hook payload emitted by the Rust runtime and turns it into a git
commit + lightweight tag using a clean trellis naming scheme.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, Optional

from trellis.config import ConfigError, load_config
from trellis.chat_history import commit_chat_checkpoint, rebuild_cycle_chat_dirs
from trellis.history_artifacts import (
    corr_result_path,
    paper_result_path,
    project_history_dir,
    review_result_path,
    sound_result_path,
    supervisor_state_path,
    worker_handoff_path,
)


def _git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=str(repo),
        capture_output=True,
        text=True,
        check=check,
    )


def _load_payload() -> Dict[str, Any]:
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:  # pragma: no cover - CLI guard
        raise RuntimeError(f"invalid checkpoint payload JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError("checkpoint payload must be a JSON object")
    return payload


def _repo_path(payload: Dict[str, Any]) -> Path:
    metadata = payload.get("metadata")
    if not isinstance(metadata, dict):
        raise RuntimeError("checkpoint payload is missing metadata")
    repo_raw = str(metadata.get("repo_path", "") or "").strip()
    if not repo_raw:
        raise RuntimeError("checkpoint metadata is missing repo_path")
    repo = Path(repo_raw).resolve()
    if not repo.is_dir():
        raise RuntimeError(f"repo_path does not exist: {repo}")
    return repo


def _maybe_load_config(payload: Dict[str, Any]):
    metadata = payload.get("metadata")
    if not isinstance(metadata, dict):
        return None
    config_raw = str(metadata.get("config_path", "") or "").strip()
    if not config_raw:
        return None
    config_path = Path(config_raw).resolve()
    if not config_path.exists():
        return None
    try:
        return load_config(config_path)
    except ConfigError:
        return None


def _maybe_apply_git_identity(payload: Dict[str, Any], repo: Path) -> None:
    config = _maybe_load_config(payload)
    if config is None:
        return
    _git(repo, "config", "user.name", config.git.author_name)
    _git(repo, "config", "user.email", config.git.author_email)


PUSH_TIMEOUT_SECONDS = 60.0


def _maybe_push_to_archive(payload: Dict[str, Any], repo: Path) -> None:
    """Push HEAD + tags + trellis-rewound/* to the archive remote.

    Gated on `config.git.remote_url` being set. Force-with-lease is used
    because the supervisor occasionally rewinds master via LastClean reset;
    the lease prevents clobbering an unexpected remote state. The
    `trellis-rewound/*` refspec is append-only (each rewind creates a new
    branch ref), so it doesn't need force.

    NEVER raises and NEVER blocks the supervisor: every git command has a
    hard timeout and any failure is logged to .trellis/logs/git-push-events.jsonl
    rather than propagated.
    """
    config = _maybe_load_config(payload)
    if config is None:
        return
    remote_url = (config.git.remote_url or "").strip()
    if not remote_url:
        return
    remote_name = (config.git.remote_name or "trellis-archive").strip() or "trellis-archive"

    try:
        _ensure_remote(repo, remote_name, remote_url)
        _do_push(repo, remote_name, log_path=_push_log_path(repo))
    except Exception as exc:
        # Final safety net. Push must NEVER derail the supervisor — log
        # to the push events file and continue.
        _log_push_event(_push_log_path(repo), {
            "kind": "push_unexpected_failure",
            "remote": remote_name,
            "error": repr(exc),
        })


def _push_log_path(repo: Path) -> Path:
    return repo / ".trellis" / "logs" / "git-push-events.jsonl"


def _log_push_event(path: Path, payload: Dict[str, Any]) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        rec = {"ts": __import__("time").time(), **payload}
        with path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, default=str) + "\n")
    except Exception:
        pass


def _git_capture(
    repo: Path,
    *args: str,
    timeout: float,
) -> subprocess.CompletedProcess[str]:
    """`_git` with an explicit timeout. On TimeoutExpired returns a synthetic
    CompletedProcess(returncode=-1) so the caller can branch uniformly.
    """
    try:
        return subprocess.run(
            ["git", *args],
            cwd=str(repo),
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        return subprocess.CompletedProcess(
            args=["git", *args],
            returncode=-1,
            stdout="",
            stderr=f"timeout after {timeout}s: {exc}",
        )


def _ensure_remote(repo: Path, name: str, url: str) -> None:
    """Make sure the named remote exists with the requested URL. Adds or
    updates as needed. Best-effort — failures here are logged by the caller.
    """
    existing = _git_capture(repo, "remote", "get-url", name, timeout=5.0)
    if existing.returncode == 0:
        current = (existing.stdout or "").strip()
        if current == url:
            return
        update = _git_capture(repo, "remote", "set-url", name, url, timeout=5.0)
        _log_push_event(_push_log_path(repo), {
            "kind": "remote_set_url",
            "remote": name,
            "url": url,
            "rc": update.returncode,
            "stderr_head": (update.stderr or "")[:200],
        })
        return
    add = _git_capture(repo, "remote", "add", name, url, timeout=5.0)
    _log_push_event(_push_log_path(repo), {
        "kind": "remote_add",
        "remote": name,
        "url": url,
        "rc": add.returncode,
        "stderr_head": (add.stderr or "")[:200],
    })


def _do_push(repo: Path, remote: str, *, log_path: Path) -> None:
    """Push HEAD (force-with-lease), all tags, and the
    `trellis-rewound/*` refs to the archive remote. Each ref class is
    pushed in its own subprocess so a transient failure on one doesn't
    block the others. All within a strict timeout. Errors logged.
    """
    # Push the current branch with force-with-lease. HEAD lets git resolve
    # the local branch name; we mirror it onto the same name remotely.
    branch = _git_capture(repo, "rev-parse", "--abbrev-ref", "HEAD", timeout=5.0)
    branch_name = (branch.stdout or "").strip() or "HEAD"
    head_push = _git_capture(
        repo, "push", "--force-with-lease", remote,
        f"HEAD:{branch_name}",
        timeout=PUSH_TIMEOUT_SECONDS,
    )
    _log_push_event(log_path, {
        "kind": "push_head",
        "remote": remote,
        "branch": branch_name,
        "rc": head_push.returncode,
        "stderr_head": (head_push.stderr or "")[:400],
    })

    tag_push = _git_capture(
        repo, "push", remote, "--tags",
        timeout=PUSH_TIMEOUT_SECONDS,
    )
    _log_push_event(log_path, {
        "kind": "push_tags",
        "remote": remote,
        "rc": tag_push.returncode,
        "stderr_head": (tag_push.stderr or "")[:400],
    })

    # Append-only rewound branches. Refspec form `refs/heads/X:refs/heads/X`
    # pushes only what matches. No force needed: trellis-rewound/* is
    # treated as immutable history.
    rewound_push = _git_capture(
        repo, "push", remote,
        "refs/heads/trellis-rewound/*:refs/heads/trellis-rewound/*",
        timeout=PUSH_TIMEOUT_SECONDS,
    )
    _log_push_event(log_path, {
        "kind": "push_rewound",
        "remote": remote,
        "rc": rewound_push.returncode,
        "stderr_head": (rewound_push.stderr or "")[:400],
    })


def _checkpoint_tag(payload: Dict[str, Any]) -> str:
    event_count = int(payload.get("event_count", 0) or 0)
    return f"supervisor2/checkpoint-{event_count:06d}"


def _commit_message(payload: Dict[str, Any]) -> str:
    checkpoint = payload.get("checkpoint", {})
    state = payload.get("state", {})
    cycle = int(checkpoint.get("cycle", 0) or 0)
    phase = str(checkpoint.get("phase", "") or "").strip() or str(state.get("phase", "") or "").strip()
    stage = str(state.get("stage", "") or "").strip()
    active = str(checkpoint.get("active_node", "") or "").strip()
    pieces = [
        f"supervisor2 checkpoint {int(payload.get('event_count', 0) or 0):06d}",
        f"cycle {cycle}",
    ]
    if phase:
        pieces.append(phase)
    if stage:
        pieces.append(stage)
    if active:
        pieces.append(active)
    return " | ".join(pieces)


def _repo_has_staged_changes(repo: Path) -> bool:
    result = _git(repo, "diff", "--cached", "--quiet", check=False)
    return result.returncode != 0


def _ensure_git_repo(repo: Path) -> None:
    result = _git(repo, "rev-parse", "--is-inside-work-tree", check=False)
    if result.returncode != 0 or result.stdout.strip() != "true":
        raise RuntimeError(f"not a git repository: {repo}")


def _write_json(path: Path, payload: Dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")


def _bridge_dir(payload: Dict[str, Any]) -> Path:
    root_raw = str(payload.get("root", "") or "").strip()
    return Path(root_raw).resolve() / "bridge"


def _copy_bridge_json_if_present(bridge_dir: Path, source_name: str, dest_path: Path) -> None:
    source = bridge_dir / source_name
    if not source.is_file():
        return
    try:
        data = json.loads(source.read_text(encoding="utf-8"))
    except Exception as exc:
        raise RuntimeError(f"invalid bridge artifact {source_name}: {exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeError(f"bridge artifact {source_name} must contain a JSON object")
    _write_json(dest_path, data)


def _write_canonical_history(payload: Dict[str, Any], repo: Path) -> None:
    history_dir = project_history_dir(repo)
    history_dir.mkdir(parents=True, exist_ok=True)
    _write_json(
        supervisor_state_path(repo),
        {
            "event_count": int(payload.get("event_count", 0) or 0),
            "metadata": payload.get("metadata", {}),
            "checkpoint": payload.get("checkpoint", {}),
            "state": payload.get("state", {}),
            "commands": payload.get("commands", []),
        },
    )
    bridge_dir = _bridge_dir(payload)
    _copy_bridge_json_if_present(bridge_dir, "latest_worker.json", worker_handoff_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_paper.json", paper_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_corr.json", corr_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_sound.json", sound_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_review.json", review_result_path(repo))


def commit_checkpoint(payload: Dict[str, Any]) -> Optional[str]:
    repo = _repo_path(payload)
    _ensure_git_repo(repo)
    _maybe_apply_git_identity(payload, repo)
    _write_canonical_history(payload, repo)

    _git(repo, "add", "-A")
    if not _repo_has_staged_changes(repo):
        return None

    message = _commit_message(payload)
    tag = _checkpoint_tag(payload)
    _git(repo, "commit", "-m", message)
    _git(repo, "tag", "-d", tag, check=False)
    _git(repo, "tag", tag)
    if bool(payload.get("is_clean", False)):
        event_count = int(payload.get("event_count", 0) or 0)
        clean_tag = f"supervisor2/clean-{event_count:06d}"
        _git(repo, "tag", "-d", clean_tag, check=False)
        _git(repo, "tag", clean_tag)
    head = _git(repo, "rev-parse", "HEAD")
    checkpoint = payload.get("checkpoint", {})
    cycle = int(checkpoint.get("cycle", 0) or 0)
    runtime_root = Path(str(payload.get("root", "") or "")).resolve() if str(payload.get("root", "") or "").strip() else None
    if cycle > 0:
        rebuild_cycle_chat_dirs(repo, runtime_root=runtime_root)
        commit_chat_checkpoint(repo, tag=f"cycle-{cycle}")
    # Mirror the new commit + tags + any rewound branches to the configured
    # archive remote. Cosmetic / observability only — push failures are
    # logged but never propagate. See _maybe_push_to_archive.
    _maybe_push_to_archive(payload, repo)
    return head.stdout.strip()


def main() -> int:
    try:
        payload = _load_payload()
        commit = commit_checkpoint(payload)
        json.dump({"ok": True, "commit": commit}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0
    except Exception as exc:  # pragma: no cover - CLI guard
        json.dump({"ok": False, "error": str(exc)}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 1


if __name__ == "__main__":  # pragma: no cover - CLI entry point
    raise SystemExit(main())
