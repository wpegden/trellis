"""Git checkpoint hook for the trellis supervisor runtime.

This is intentionally runtime-owned, not protocol-owned. It reads the
checkpoint hook payload emitted by the Rust runtime and turns it into a git
commit + lightweight tag using a clean trellis naming scheme.
"""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional

from trellis.config import ConfigError, load_config
from trellis.chat_history import commit_chat_checkpoint, rebuild_cycle_chat_dirs
from trellis.history_artifacts import (
    corr_result_path,
    decode_shared_state,
    encode_shared_state,
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


#: One hook process handles exactly one checkpoint, and `load_config` spawns
#: the kernel CLI (`resolve_main_result_targets`) on every call, so the three
#: consumers below -- git identity, archive push, history format -- share a
#: single parse instead of paying for three subprocesses per checkpoint.
#: Failures are cached as `None` for the same reason.
_CONFIG_CACHE: Dict[str, Any] = {}


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
    key = str(config_path)
    if key in _CONFIG_CACHE:
        return _CONFIG_CACHE[key]
    try:
        config = load_config(config_path)
    except ConfigError:
        config = None
    _CONFIG_CACHE[key] = config
    return config


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
        _append_jsonl_event(_push_log_path(repo), {
            "kind": "push_unexpected_failure",
            "remote": remote_name,
            "error": repr(exc),
        })


def _push_log_path(repo: Path) -> Path:
    return repo / ".trellis" / "logs" / "git-push-events.jsonl"


def _append_jsonl_event(path: Path, payload: Dict[str, Any]) -> None:
    """Append one timestamped record to a `.trellis/logs/*.jsonl` file.

    Best-effort by construction: observability must never be able to fail a
    checkpoint. `.trellis/` is gitignored, so these files are invisible to the
    checkpoint's own `git add -A`.
    """
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
        _append_jsonl_event(_push_log_path(repo), {
            "kind": "remote_set_url",
            "remote": name,
            "url": url,
            "rc": update.returncode,
            "stderr_head": (update.stderr or "")[:200],
        })
        return
    add = _git_capture(repo, "remote", "add", name, url, timeout=5.0)
    _append_jsonl_event(_push_log_path(repo), {
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
    _append_jsonl_event(log_path, {
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
    _append_jsonl_event(log_path, {
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
    _append_jsonl_event(log_path, {
        "kind": "push_rewound",
        "remote": remote,
        "rc": rewound_push.returncode,
        "stderr_head": (rewound_push.stderr or "")[:400],
    })


def _checkpoint_tag(payload: Dict[str, Any]) -> str:
    event_count = int(payload.get("event_count", 0) or 0)
    return f"supervisor2/checkpoint-{event_count:06d}"


TRUST_DECISION_TAG_PREFIX = "supervisor2/trust-decision-"
TRUST_DECISION_RECORD_DIR = ".trellis-history/trust-decisions"


def _trust_decision_carrier(payload: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    """The Q1 gate-decision carrier (plan doc 32 Stage 3): the exact
    prospective event-log line plus the full trust-record digest.  Absent
    from every math-mode payload (skip-serialized)."""
    carrier = payload.get("trust_record")
    if carrier is None:
        return None
    if not isinstance(carrier, dict):
        raise RuntimeError("trust_record payload must be a JSON object")
    line_json = carrier.get("line_json")
    digest = carrier.get("record_sha256")
    if not isinstance(line_json, str) or not line_json.strip():
        raise RuntimeError("trust_record.line_json must be a non-empty string")
    if not isinstance(digest, str) or len(digest) != 64:
        raise RuntimeError("trust_record.record_sha256 must be a 64-hex digest")
    return {"line_json": line_json, "record_sha256": digest}


def _write_trust_decision_record_file(repo: Path, carrier: Dict[str, Any]) -> Path:
    """Write the tracked canonical decision-record file BEFORE the commit so
    the checkpoint commit contains it.  This writer NEVER deletes or
    overwrites a divergent record: an existing identical file is idempotent
    success; different content under the same digest name is a hard error.
    """
    directory = repo / TRUST_DECISION_RECORD_DIR
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{carrier['record_sha256']}.json"
    content = carrier["line_json"] + "\n"
    if path.exists():
        existing = path.read_text(encoding="utf-8")
        if existing != content:
            raise RuntimeError(
                f"trust decision record file {path} exists with different content; "
                "refusing to overwrite (never-delete rule)"
            )
        return path
    path.write_text(content, encoding="utf-8")
    return path


def _write_trust_decision_tag(repo: Path, carrier: Dict[str, Any]) -> str:
    """Create the lightweight `supervisor2/trust-decision-<digest12>` tag on
    HEAD.  This is a SEPARATE code path from the checkpoint/clean tag writer
    and MUST NOT reuse its delete-then-recreate pattern (plan doc 32 Stage
    3, pinned trap): `git tag -d` / `git tag -f` are forbidden on
    `trust-decision-*` refs.  An existing tag whose commit carries an
    identical record file for the same digest is idempotent success; the
    same name with different content is a hard error.
    """
    digest = carrier["record_sha256"]
    tag = f"{TRUST_DECISION_TAG_PREFIX}{digest[:12]}"
    existing = _git(repo, "tag", "-l", tag, check=False)
    if existing.returncode == 0 and existing.stdout.strip():
        shown = _git(
            repo,
            "show",
            f"{tag}:{TRUST_DECISION_RECORD_DIR}/{digest}.json",
            check=False,
        )
        if shown.returncode == 0 and shown.stdout == carrier["line_json"] + "\n":
            return tag
        raise RuntimeError(
            f"trust decision tag {tag} already exists with different record content; "
            "refusing (same-name-different-content is a hard error)"
        )
    _git(repo, "tag", tag)
    return tag


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


HISTORY_JSON_FILE_MODE = 0o664


def _history_json_text(payload: Any) -> str:
    """The one serialization used for every `.trellis-history/*.json` artifact.

    `json.dumps(..., indent=2, sort_keys=False) + "\\n"` — byte-identical to
    the historical non-atomic write, so the committed blobs keep their shape
    and git delta compression is undisturbed. `trellis.json_io.save_json` is
    deliberately not reused here: it encodes with `ensure_ascii=False`
    (different bytes) and takes an flock whose `.lock` sidecar would land
    inside the tracked history directory and be swept up by `git add -A`.
    """
    return json.dumps(payload, indent=2, sort_keys=False) + "\n"


def _write_json(path: Path, payload: Dict[str, Any]) -> None:
    """Atomically publish a canonical history artifact."""
    _write_text(path, _history_json_text(payload))


def _write_text(path: Path, text: str) -> None:
    """Atomically publish `text` at `path`.

    Readers (the viewer, the operator rewind procedure, the kernel's recovery
    path) and `git add -A` all observe `.trellis-history/*.json` while the
    supervisor is mid-checkpoint, so the target path must never hold a
    partial prefix: write to a sibling temp file, fsync, then `os.replace`.
    The temp file is created in the target's own directory so the rename
    stays within one filesystem, which is what makes it atomic.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        target_mode = stat.S_IMODE(path.stat().st_mode)
    except OSError:
        target_mode = HISTORY_JSON_FILE_MODE
    fd, tmp_name = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(tmp_name, target_mode)
        os.replace(tmp_name, path)
    finally:
        try:
            os.unlink(tmp_name)
        except FileNotFoundError:
            pass


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


#: Default for `config.history.shared_state` when no config is reachable from
#: the checkpoint payload. Encoding is the safe default: every reader (Python,
#: Rust, the viewer's JS, the `scripts/` analysis tools) decodes both forms
#: structurally and has done so since before any writer emitted the new one, so
#: the plain form buys nothing while the encoded form keeps the run's committed
#: history clear of GitHub's 100 MB per-file refusal.
SHARED_STATE_DEFAULT = True

#: Auto-latch: a plain-form `supervisor_state.json` at or above this size is
#: encoded no matter what the knob says. GitHub warns at 50 MB and hard-refuses
#: at 100 MB; the run grows this file monotonically (~0.26 MiB/hour), so a knob
#: left off must not be able to wedge the run's own history against that wall.
#: 90 MiB leaves ~38 hours of headroom at the observed growth rate -- enough for
#: the latch to be noticed and acted on before the file could reach 100 MB.
SHARED_STATE_LATCH_BYTES = 90 * 1024 * 1024


def _history_format_log_path(repo: Path) -> Path:
    return repo / ".trellis" / "logs" / "history-format-events.jsonl"


def _shared_state_enabled(payload: Dict[str, Any]) -> bool:
    config = _maybe_load_config(payload)
    if config is None:
        return SHARED_STATE_DEFAULT
    return bool(config.history.shared_state)


def _supervisor_state_text(document: Dict[str, Any], repo: Path, *, shared: bool) -> str:
    """Serialize the supervisor-state document in the selected on-disk form.

    Round-trip fidelity is verified here, on every write, before the bytes are
    published: this file is the run's only durable protocol state, and a
    silently lossy encode would be discovered as a corrupt resume rather than
    as a failed checkpoint. A `SharedStateError` (or a failed verification)
    propagates: the kernel's checkpoint sink treats it as a sink failure, rolls
    the step back and halts, which is recoverable by setting
    `history.shared_state` to false in trellis.config.json. Falling back to the
    plain form silently would instead leave the latch case -- the one that
    exists precisely because the file is near the 100 MB wall -- writing the
    oversized bytes it was added to prevent.
    """
    if not shared:
        plain = _history_json_text(document)
        size = len(plain.encode("utf-8"))
        if size < SHARED_STATE_LATCH_BYTES:
            return plain
        _log_shared_state_latch(repo, plain_bytes=size)
        shared = True
    encoded = encode_shared_state(document)
    text = _history_json_text(encoded)
    if decode_shared_state(json.loads(text)) != document:
        raise RuntimeError(
            "shared-state encoding of supervisor_state.json did not round-trip; "
            "refusing to publish it (set history.shared_state=false in "
            "trellis.config.json to fall back to the plain form)"
        )
    return text


def _log_shared_state_latch(repo: Path, *, plain_bytes: int) -> None:
    """Record a latch firing. The hook's stderr is piped and dropped by the
    kernel's `ProcessCheckpointHook` on success, so the durable record is the
    gitignored `.trellis/logs/` JSONL; stderr is emitted too for the case where
    the checkpoint fails and the kernel surfaces the hook's output.
    """
    message = (
        f"trellis: supervisor_state.json plain form is {plain_bytes} bytes, at or above "
        f"the {SHARED_STATE_LATCH_BYTES}-byte auto-latch; encoding as shared state "
        "despite history.shared_state=false"
    )
    print(message, file=sys.stderr)
    _append_jsonl_event(
        _history_format_log_path(repo),
        {
            "kind": "shared_state_auto_latch",
            "plain_bytes": plain_bytes,
            "latch_bytes": SHARED_STATE_LATCH_BYTES,
        },
    )


def _write_canonical_history(payload: Dict[str, Any], repo: Path) -> None:
    history_dir = project_history_dir(repo)
    history_dir.mkdir(parents=True, exist_ok=True)
    document = {
        "event_count": int(payload.get("event_count", 0) or 0),
        "metadata": payload.get("metadata", {}),
        "checkpoint": payload.get("checkpoint", {}),
        "state": payload.get("state", {}),
        "commands": payload.get("commands", []),
    }
    _write_text(
        supervisor_state_path(repo),
        _supervisor_state_text(document, repo, shared=_shared_state_enabled(payload)),
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
    # Q1 gate-decision transaction (plan doc 32 Stage 3, Codex R3-8 hook
    # ordering): on a decision-bearing payload the tracked record file is
    # written into the repo tree BEFORE the commit (so the checkpoint commit
    # contains it), and the lightweight decision tag is the hook's LAST
    # fallible operation below — "hook failed => no decision tag" holds by
    # construction.
    trust_carrier = _trust_decision_carrier(payload)
    if trust_carrier is not None:
        _write_trust_decision_record_file(repo, trust_carrier)

    _git(repo, "add", "-A")
    resume_completion = False
    if not _repo_has_staged_changes(repo):
        if trust_carrier is not None:
            # Same-answer retry after a post-commit failure (Stage-3 fix 3,
            # Codex 3): a prior attempt committed the identical record file
            # but failed before the decision tag (checkpoint/clean tag, chat
            # ops, or the tag write itself). HEAD already carries the exact
            # record bytes, so there is nothing new to commit — RESUME the
            # tag/chat completion instead of wedging on "nothing to commit".
            digest = trust_carrier["record_sha256"]
            shown = _git(
                repo,
                "show",
                f"HEAD:{TRUST_DECISION_RECORD_DIR}/{digest}.json",
                check=False,
            )
            if shown.returncode == 0 and shown.stdout == trust_carrier["line_json"] + "\n":
                resume_completion = True
            else:
                raise RuntimeError(
                    "decision-bearing checkpoint found nothing to commit; the trust "
                    "record file must stage a change"
                )
        else:
            return None

    message = _commit_message(payload)
    tag = _checkpoint_tag(payload)
    if not resume_completion:
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
    # The decision tag is written LAST among fallible operations (Codex
    # R3-8): every failure above leaves no tag, so the runtime's probe
    # correctly rolls the decision back and re-presents the gate.  Only the
    # trailing archive mirror push (which never propagates errors by
    # documented contract) may follow it.
    if trust_carrier is not None:
        _write_trust_decision_tag(repo, trust_carrier)
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
