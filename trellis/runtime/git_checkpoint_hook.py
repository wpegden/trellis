"""Git checkpoint hook for the trellis supervisor runtime.

This is intentionally runtime-owned, not protocol-owned. It reads the
checkpoint hook payload emitted by the Rust runtime and turns it into a git
commit + lightweight tag using a clean trellis naming scheme.
"""

from __future__ import annotations

import json
import hashlib
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional

from trellis.config import ConfigError, load_config
from trellis.dispatch.history import checkpoint_requests
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


#: Git identity, archive push and history format share one lazy configuration
#: parse per checkpoint. commit_checkpoint clears this cache on each call;
#: ConfigError is cached as None within that checkpoint.
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
    # A resumed writer owns an explicit expected remote generation. Ordinary
    # tracking refs can advance during a fetch, so they are not its lease.
    resume_guard = repo / ".trellis" / "resume-mirror.json"
    guard = None
    source = "HEAD"
    lease = "--force-with-lease"
    if resume_guard.is_file():
        from trellis.remote_resume import atomic_json
        guard = json.loads(resume_guard.read_text())
        actual_url = _git_capture(repo, "remote", "get-url", remote, timeout=5.0)
        tip = _git_capture(repo, "ls-remote", "--heads", remote, f"refs/heads/{branch_name}", timeout=PUSH_TIMEOUT_SECONDS)
        rows = (tip.stdout or "").splitlines()
        actual = rows[0].split()[0] if tip.returncode == 0 and len(rows) == 1 else None
        source_result = _git_capture(repo, "rev-parse", "HEAD", timeout=5.0)
        source = (source_result.stdout or "").strip()
        expected = guard.get("expected_tip")
        # Recover a push which completed before its local receipt was written.
        if actual and actual == guard.get("pending_tip"):
            expected = actual
            guard["expected_tip"] = actual
            guard["initial"] = False
        matches = (guard.get("version") == 1 and guard.get("branch") == branch_name
                   and (actual_url.stdout or "").strip() == guard.get("remote_url")
                   and actual == expected and source_result.returncode == 0)
        if matches and guard.get("initial") and not guard.get("allow_initial_rewind"):
            matches = _git_capture(repo, "merge-base", "--is-ancestor", expected, source, timeout=5.0).returncode == 0
        if not matches:
            _append_jsonl_event(log_path, {"kind": "resume_remote_lineage_changed", "branch": branch_name,
                "expected_tip": expected, "actual_tip": actual,
                "error": "mirror refused: remote changed or an earlier checkpoint needs explicit rewind approval"})
            return
        lease = f"--force-with-lease=refs/heads/{branch_name}:{expected}"
        guard["pending_tip"] = source
        atomic_json(resume_guard, guard)
    head_push = _git_capture(
        repo, "push", lease, remote,
        f"{source}:{branch_name}",
        timeout=PUSH_TIMEOUT_SECONDS,
    )
    if guard is not None:
        if head_push.returncode != 0:
            _append_jsonl_event(log_path, {"kind": "resume_mirror_failed", "rc": head_push.returncode,
                "error": (head_push.stderr or "")[:400]})
            return
        guard.update(expected_tip=source, initial=False)
        guard.pop("pending_tip", None)
        atomic_json(resume_guard, guard)
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
    """Legacy serialization for plain state and other history JSON artifacts.

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
    # Keep ordinary line-oriented JSON, but omit depth-dependent indentation.
    # Shared state can itself approach GitHub's per-file limit; indentation
    # alone occupied 30 MB in a 102 MB checkpoint. Preserve keys, escaping and
    # newlines so Git can still delta individual records without compression.
    text = json.dumps(encoded, indent=0, sort_keys=False) + "\n"
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


def _checkpoint_document(payload):
    return {
        "event_count": int(payload.get("event_count", 0) or 0),
        "metadata": payload.get("metadata", {}),
        "checkpoint": payload.get("checkpoint", {}),
        "state": payload.get("state", {}),
        "commands": payload.get("commands", []),
    }


def _write_canonical_history(payload: Dict[str, Any], repo: Path) -> None:
    history_dir = project_history_dir(repo)
    history_dir.mkdir(parents=True, exist_ok=True)
    document = _checkpoint_document(payload)
    _write_text(
        supervisor_state_path(repo),
        _supervisor_state_text(checkpoint_requests(document, repo, store=True, references=payload.get("dispatch_request_references")), repo, shared=_shared_state_enabled(payload)),
    )
    bridge_dir = _bridge_dir(payload)
    _copy_bridge_json_if_present(bridge_dir, "latest_worker.json", worker_handoff_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_paper.json", paper_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_corr.json", corr_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_sound.json", sound_result_path(repo))
    _copy_bridge_json_if_present(bridge_dir, "latest_review.json", review_result_path(repo))


def _protect_dispatch_objects(repo: Path) -> None:
    """Already tracked objects are immutable; verify only newly staged ones."""
    from trellis.dispatch.history import DIRECTORY, verify_added_object
    changed = _git(repo, 'diff', '--cached', '--name-status', '--no-renames', '-z', '--', DIRECTORY).stdout
    fields = changed.split('\0')
    for index in range(0, len(fields) - 1, 2):
        status, name = fields[index:index + 2]
        match = re.fullmatch(re.escape(DIRECTORY) + r'/([0-9a-f]{64})\.zip', name)
        if status != 'A' or match is None:
            raise RuntimeError(f'checkpoint would change immutable dispatch request evidence: {name}')
        verify_added_object(repo / name, match.group(1))


def _remove_interrupted_dispatch_temps(repo: Path) -> None:
    # The native publisher has returned and the coordinator owns its lock.
    # Temporary names are never references; a crash may leave one behind.
    directory = repo / '.trellis-history/dispatch-requests'
    if not directory.exists():
        return
    if not stat.S_ISDIR(directory.lstat().st_mode):
        raise RuntimeError('dispatch history directory is not a regular directory')
    for path in directory.iterdir():
        if re.fullmatch(r'\.[0-9a-f]{64}-[0-9a-f]{32}\.tmp', path.name):
            if not stat.S_ISREG(path.lstat().st_mode):
                raise RuntimeError('interrupted dispatch temporary is not a regular file')
            path.unlink()


def _engineering_guard(payload):
    state = payload.get('state', {})
    engineering = state.get('engineering')
    guard = payload.get('engineering_guard')
    if state.get('phase') not in {'Engineering', 'engineering'}:
        if guard is not None:
            raise RuntimeError('Engineering ownership supplied outside engineering')
        return None
    if not isinstance(engineering, dict):
        raise RuntimeError('Native engineering checkpoint has no subject')
    if payload.get('engineering_handoff') is not None:
        if guard is not None:
            raise RuntimeError('Engineering handoff cannot use delivery ownership')
        _engineering_handoff(payload)
        return None
    fields = {'request_id', 'turn', 'request_sha256', 'candidate_sha256', 'checked_sha256', 'roots'}
    if not isinstance(guard, dict) or set(guard) != fields:
        raise RuntimeError('Engineering checkpoint requires its original directory ownership')
    consumption = engineering.get('last_consumption')
    if not isinstance(consumption, dict) or any(consumption.get(key) != guard[key] for key in fields - {'roots'}):
        raise RuntimeError('Engineering ownership differs from native consumption')
    roots = guard['roots']
    if (not isinstance(roots, list) or len(roots) != 2
            or any(not isinstance(pin, dict) or set(pin) != {'path', 'fd', 'device', 'inode'} for pin in roots)):
        raise RuntimeError('Engineering checkpoint directories are incomplete')
    for pin in roots:
        if (not isinstance(pin['path'], str) or not Path(pin['path']).is_absolute()
                or any(type(pin[key]) is not int or pin[key] < 0 for key in ('fd', 'device', 'inode'))
                or pin['fd'] < 3):
            raise RuntimeError('Invalid engineering directory identity')
    if roots[1]['path'] != str(Path(roots[0]['path']) / '.trellis') or roots[0]['fd'] == roots[1]['fd']:
        raise RuntimeError('Engineering checkpoint directories differ')
    return guard


def _engineering_handoff(payload):
    from trellis.modeling.engineering import digest
    state = payload.get('state', {})
    engineering = state.get('engineering', {})
    handoff = payload.get('engineering_handoff')
    if handoff is None:
        return None
    fields = {'predecessor_sha256', 'baseline_sha256', 'record_sha256', 'downstream_commit',
              'downstream_state_sha256', 'downstream_source_sha256'}
    if (state.get('phase') != 'Engineering' or state.get('stage') != 'EngineeringReady'
            or not isinstance(handoff, dict) or set(handoff) != fields
            or engineering.get('handoff') != handoff or not engineering.get('baseline')
            or digest(engineering['baseline']) != handoff['baseline_sha256']
            or state.get('in_flight_request') is not None or state.get('worker_attempt') is not None
            or any(not isinstance(handoff[key], str) or not re.fullmatch('[0-9a-f]{64}', handoff[key])
                   for key in fields - {'downstream_commit'})
            or not isinstance(handoff['downstream_commit'], str)
            or not re.fullmatch('[0-9a-f]{40}|[0-9a-f]{64}', handoff['downstream_commit'])):
        raise RuntimeError('Engineering handoff checkpoint capability differs')
    return handoff


def _check_engineering_handoff(payload, repo):
    from trellis.modeling import engineering as eng, history, performance
    handoff = _engineering_handoff(payload)
    record = eng.native_read(repo, handoff['record_sha256'])
    attempt_name = record['handoff_attempt']
    if not re.fullmatch(r'\.trellis/modeling/engineering/handoffs/[0-9a-f]{32}', attempt_name):
        raise RuntimeError('Engineering handoff record has no existing attempt')
    before_raw = history._read(repo / attempt_name / 'before-state.json')
    before = json.loads(before_raw)
    current_raw = history._read(repo / '.trellis/modeling/state.json')
    current = json.loads(current_raw)
    checkpoint = handoff['downstream_commit']
    shown = _git(repo, 'show', checkpoint + ':.trellis/modeling/state.json').stdout.encode('utf-8')
    if (record['ready_state_sha256'] != handoff['predecessor_sha256']
            or record['baseline_sha256'] != handoff['baseline_sha256']
            or record['downstream_source_sha256'] != handoff['downstream_source_sha256']
            or hashlib.sha256(before_raw).hexdigest() != record['before_state_sha256']
            or before.get('status') != 'engineering_ready' or before.get('stage') != 'engineering'
            or before['native_engineering']['state_sha256'] != handoff['predecessor_sha256']
            or current['engineering'].get('handoff_attempt') != attempt_name
            or not current['engineering'].get('handed_off')
            or current.get('stage') != 'setup' or current.get('status') != 'active'
            or current['engineering']['baseline'] != before['engineering']['baseline']
            or eng.native_read(repo, payload['state']['engineering']['baseline']['selection_receipt_sha256']) != before['engineering']['baseline']
            or current_raw != shown or hashlib.sha256(shown).hexdigest() != handoff['downstream_state_sha256']
            or _git(repo, 'rev-parse', 'HEAD').stdout.strip() != checkpoint
            or eng.native_handoff_source(repo) != handoff['downstream_source_sha256']):
        raise RuntimeError('Engineering handoff downstream checkpoint or evidence changed')
    files = performance._inventory(repo / 'upstream')
    for name, expected in files.items():
        content = subprocess.run(['git', 'show', checkpoint + ':upstream/' + name],
                                 cwd=repo, capture_output=True, check=True).stdout
        if hashlib.sha256(content).hexdigest() != expected:
            raise RuntimeError('Engineering handoff source differs from downstream checkpoint')
    protected = ['.trellis/modeling/state.json', 'upstream', attempt_name + '/before-state.json',
                 str(eng.STORE / 'native-records' / (handoff['record_sha256'] + '.json')),
                 str(eng.STORE / 'native-records' / (payload['state']['engineering']['baseline']['selection_receipt_sha256'] + '.json'))]
    if _git(repo, 'diff', '--cached', '--exit-code', checkpoint, '--', *protected, check=False).returncode:
        raise RuntimeError('Engineering handoff staged preparation differs from its downstream checkpoint')


def _check_engineering_roots(guard):
    from trellis.agents.host_files import safe_directory
    for pin in guard['roots']:
        original = os.fstat(pin['fd'])
        with safe_directory(Path(pin['path']), create=False) as (_, opened):
            current = os.fstat(opened)
        expected = (pin['device'], pin['inode'])
        if (not stat.S_ISDIR(original.st_mode) or (original.st_dev, original.st_ino) != expected
                or (current.st_dev, current.st_ino) != expected):
            raise RuntimeError('Original engineering directory changed before checkpoint')


def _engineering_already_committed(payload, repo):
    shown = _git(repo, 'show', 'HEAD:.trellis-history/supervisor_state.json', check=False)
    if shown.returncode != 0:
        return False
    document = decode_shared_state(json.loads(shown.stdout))
    return document == _checkpoint_document(payload)


def commit_checkpoint(payload: Dict[str, Any]) -> Optional[str]:
    _CONFIG_CACHE.clear()
    repo = _repo_path(payload)
    _ensure_git_repo(repo)
    engineering_guard = _engineering_guard(payload)
    engineering_handoff = _engineering_handoff(payload)
    if (engineering_guard is not None or engineering_handoff is not None) and _engineering_already_committed(payload, repo):
        # The native transaction is durable. Live delivery roots cannot revoke it.
        return _git(repo, 'rev-parse', 'HEAD').stdout.strip()
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

    _remove_interrupted_dispatch_temps(repo)
    _git(repo, "add", "-A")
    if engineering_guard is not None or engineering_handoff is not None:
        witness = supervisor_state_path(repo)
        if not witness.is_file():
            raise RuntimeError('Engineering checkpoint has no exact committed witness')
        _git(repo, 'add', '--force', '--', witness.relative_to(repo).as_posix())
    # Object closure includes event-only prepared requests, which may differ
    # from the bound state request. Repository ignore rules cannot omit it.
    if (repo / ".trellis-history/dispatch-requests").is_dir():
        _git(repo, "add", "-f", "--", ".trellis-history/dispatch-requests")
    _protect_dispatch_objects(repo)
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
            if engineering_guard is not None or engineering_handoff is not None:
                raise RuntimeError('Engineering checkpoint has no exact committed witness')
            return None

    message = _commit_message(payload)
    tag = _checkpoint_tag(payload)
    if not resume_completion:
        if engineering_guard is not None:
            _check_engineering_roots(engineering_guard)
        if engineering_handoff is not None:
            _check_engineering_handoff(payload, repo)
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


def main(*, after_checkpoint=None) -> int:
    payload = None
    try:
        payload = _load_payload()
        commit = commit_checkpoint(payload)
        json.dump({"ok": True, "commit": commit}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        status = 0
    except Exception as exc:  # pragma: no cover - CLI guard
        json.dump({"ok": False, "error": str(exc)}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        status = 1
    if payload is not None and after_checkpoint is not None:
        # Optional launcher actions share this already parsed payload. Their
        # failure must not change whether the checkpoint itself succeeded.
        try:
            after_checkpoint(payload, status)
        except Exception as exc:
            print(f"checkpoint post-action failed: {exc}", file=sys.stderr)
    return status


if __name__ == "__main__":  # pragma: no cover - CLI entry point
    raise SystemExit(main())
