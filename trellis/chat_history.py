"""Canonical project-local chat history paths and git operations."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from trellis.project_paths import project_chats_dir, project_state_dir_for_repo


FINAL_CYCLE_TAG_RE = re.compile(r"^cycle-(\d+)$")
CHECKPOINT_TAG_RE = re.compile(r"^cycle-(\d+)-(worker|verification)$")
CHECKPOINT_STAGE_ORDER = {"worker": 0, "verification": 1}
ARTIFACT_CYCLE_DIR_RE = re.compile(r"^cycle-(\d{4})$")
_ARTIFACT_REQUEST_ID_RE = [
    re.compile(r"^trellis_worker_(\d+)_result$"),
    re.compile(r"^trellis_review_(\d+)_decision$"),
    re.compile(r"^trellis_corr_(\d+)_v\d+$"),
    re.compile(r"^trellis_paper_(\d+)_v\d+$"),
    re.compile(r"^trellis_sound_(\d+)_v\d+$"),
    re.compile(r"^trellis_stuck_math_audit_(\d+)_result$"),
]


def chat_repo_path(repo_path: Path) -> Path:
    return project_chats_dir(repo_path / ".trellis")


def ensure_chat_repo(repo_path: Path) -> Path:
    repo = chat_repo_path(repo_path)
    repo.mkdir(parents=True, exist_ok=True)
    if not (repo / ".git").exists():
        _git(repo, "init")
        _git(repo, "config", "user.name", "trellis-chats")
        _git(repo, "config", "user.email", "trellis-chats@localhost")
        readme = repo / "README.md"
        if not readme.exists():
            readme.write_text(
                "# Project Chat History\n\n"
                "This nested git repo stores canonical per-cycle agent conversations.\n",
                encoding="utf-8",
            )
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-m", "Initialize local chat history repo")
    return repo


def _git(repo: Path, *args: str, check: bool = True, timeout: int = 30) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=str(repo),
        capture_output=True,
        text=True,
        timeout=timeout,
        check=check,
    )


def _root_commit(repo: Path) -> Optional[str]:
    result = _git(repo, "rev-list", "--max-parents=0", "HEAD", check=False)
    if result.returncode != 0:
        return None
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    return lines[0] if lines else None


def _tag_sort_key(tag: str) -> tuple[int, int]:
    tag = str(tag or "").strip()
    final_match = FINAL_CYCLE_TAG_RE.fullmatch(tag)
    if final_match:
        return (int(final_match.group(1)), 2)
    checkpoint_match = CHECKPOINT_TAG_RE.fullmatch(tag)
    if checkpoint_match:
        return (int(checkpoint_match.group(1)), CHECKPOINT_STAGE_ORDER[checkpoint_match.group(2)])
    return (-1, -1)


def chat_cycle_dir_name(cycle: int) -> str:
    return f"cycle-{int(cycle):04d}"


def _cycle_dir_name_from_log_dir(log_dir: Path) -> str:
    name = log_dir.name
    if ARTIFACT_CYCLE_DIR_RE.fullmatch(name):
        return name
    return "live"


def _request_id_from_artifact_name(name: str) -> Optional[int]:
    for pattern in _ARTIFACT_REQUEST_ID_RE:
        match = pattern.fullmatch(str(name or "").strip())
        if match:
            return int(match.group(1))
    return None


def _runtime_root_from_repo(repo_path: Path) -> Optional[Path]:
    runtime_root = repo_path / ".trellis" / "runtime"
    if not runtime_root.is_dir():
        return None
    candidates = [child for child in runtime_root.iterdir() if child.is_dir() and child.name.endswith("-runtime")]
    if not candidates:
        return None
    candidates.sort(key=lambda path: path.stat().st_mtime, reverse=True)
    return candidates[0]


_EVENT_LOG_CYCLES_CACHE_NAME = ".event_log_request_cycles.cache.json"
_EVENT_LOG_READ_CHUNK = 1024 * 1024  # 1 MiB


def _stream_event_log_records(event_log_path: Path, start_offset: int):
    """Yield (offset_after_line, parsed_record) for each complete newline-
    terminated JSON record at or after `start_offset`. Trailing partial lines
    (no `\n`) are left unconsumed; the yielded offset is always a safe resume
    point on the next call. Lines that fail to parse as JSON are skipped but
    their offset is still advanced past the newline.
    """
    offset = start_offset
    with event_log_path.open("rb") as f:
        f.seek(offset)
        buf = b""
        while True:
            chunk = f.read(_EVENT_LOG_READ_CHUNK)
            if not chunk:
                break
            buf += chunk
            while True:
                nl = buf.find(b"\n")
                if nl < 0:
                    break
                line = buf[:nl]
                buf = buf[nl + 1:]
                offset += nl + 1
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    record = json.loads(stripped.decode("utf-8", errors="replace"))
                except Exception:
                    continue
                yield offset, record


def _request_cycles_from_event_log(event_log_path: Path) -> Dict[int, int]:
    """Return {request_id: cycle} built from `issue_request` commands in the
    event log. Incremental: caches the parsed map to a sidecar JSON file keyed
    on the last-known safe (post-newline) byte offset; subsequent calls only
    re-parse bytes appended after that offset. The event log is append-only,
    so this is safe — file shrinkage triggers a full re-read.
    """
    mapping: Dict[int, int] = {}
    if not event_log_path.is_file():
        return mapping
    try:
        cur_size = event_log_path.stat().st_size
    except OSError:
        return mapping

    cache_path = event_log_path.with_name(_EVENT_LOG_CYCLES_CACHE_NAME)
    start_offset = 0
    if cache_path.is_file():
        try:
            cached = json.loads(cache_path.read_text(encoding="utf-8"))
            if isinstance(cached, dict):
                cached_offset = int(cached.get("offset", 0))
                cached_mapping = cached.get("mapping")
                if (
                    isinstance(cached_mapping, dict)
                    and 0 <= cached_offset <= cur_size
                ):
                    mapping = {int(k): int(v) for k, v in cached_mapping.items()}
                    start_offset = cached_offset
        except Exception:
            pass

    if start_offset == cur_size:
        return mapping

    last_offset = start_offset
    try:
        for offset_after, record in _stream_event_log_records(event_log_path, start_offset):
            last_offset = offset_after
            commands = record.get("commands")
            if not isinstance(commands, list):
                continue
            for command in commands:
                if not isinstance(command, dict) or command.get("command") != "issue_request":
                    continue
                request = command.get("request")
                if not isinstance(request, dict):
                    continue
                try:
                    request_id = int(request.get("id"))
                    cycle = int(request.get("cycle"))
                except Exception:
                    continue
                mapping[request_id] = cycle
    except OSError:
        return mapping

    try:
        tmp = cache_path.with_suffix(cache_path.suffix + ".tmp")
        tmp.write_text(
            json.dumps({
                "offset": last_offset,
                "mapping": {str(k): v for k, v in mapping.items()},
            }),
            encoding="utf-8",
        )
        tmp.replace(cache_path)
    except OSError:
        pass

    return mapping


def rebuild_cycle_chat_dirs(repo_path: Path, *, runtime_root: Optional[Path] = None) -> Dict[int, List[str]]:
    chats = ensure_chat_repo(repo_path)
    live_dir = chats / "live"
    if not live_dir.is_dir():
        return {}

    if runtime_root is None:
        runtime_root = _runtime_root_from_repo(repo_path)
    if runtime_root is None:
        return {}

    event_log_path = runtime_root / "event_log.jsonl"
    request_cycles = _request_cycles_from_event_log(event_log_path)
    if not request_cycles:
        return {}

    for cycle_dir in chats.iterdir():
        if cycle_dir.is_dir() and ARTIFACT_CYCLE_DIR_RE.fullmatch(cycle_dir.name):
            shutil.rmtree(cycle_dir)

    cycle_artifacts: Dict[int, List[Tuple[str, Path]]] = {}
    for artifact_dir in sorted(child for child in live_dir.iterdir() if child.is_dir()):
        request_id = _request_id_from_artifact_name(artifact_dir.name)
        if request_id is None:
            continue
        cycle = request_cycles.get(request_id)
        if cycle is None:
            continue
        cycle_artifacts.setdefault(cycle, []).append((artifact_dir.name, artifact_dir))

    materialized: Dict[int, List[str]] = {}
    for cycle, artifacts in cycle_artifacts.items():
        cycle_root = chats / chat_cycle_dir_name(cycle)
        cycle_root.mkdir(parents=True, exist_ok=True)
        names: List[str] = []
        for artifact_name, source_dir in artifacts:
            target_dir = cycle_root / artifact_name
            shutil.copytree(source_dir, target_dir, dirs_exist_ok=True)
            names.append(artifact_name)
        materialized[cycle] = names
    return materialized


def _artifact_prefix(prefix: Optional[str], role: str) -> str:
    base = prefix or role
    base = re.sub(r"[^A-Za-z0-9_.-]+", "_", str(base)).strip("._-") or role
    return base[:80]


def chat_artifact_dir(
    repo_path: Path,
    *,
    log_dir: Path,
    artifact_prefix: Optional[str],
    role: str,
) -> Path:
    chats = ensure_chat_repo(repo_path)
    cycle_dir = _cycle_dir_name_from_log_dir(log_dir)
    artifact_dir = chats / cycle_dir / _artifact_prefix(artifact_prefix, role)
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return artifact_dir


def ensure_chat_file_link(
    repo_path: Path,
    *,
    log_dir: Path,
    artifact_prefix: Optional[str],
    role: str,
    log_filename: str,
    canonical_name: str,
) -> Path:
    artifact_dir = chat_artifact_dir(repo_path, log_dir=log_dir, artifact_prefix=artifact_prefix, role=role)
    canonical = artifact_dir / canonical_name
    log_path = log_dir / log_filename
    log_path.parent.mkdir(parents=True, exist_ok=True)
    # Resolve symlinks in the target so the link is valid inside bwrap
    # sandboxes that bind-mount the realpath (e.g. when ${TRELLIS_ROOT:-/path/to/trellis}/math
    # symlinks to /mnt/2ndSSD/math, sandboxes only bind the /mnt path).
    desired_target = os.path.realpath(str(canonical))
    try:
        if log_path.is_symlink() and os.readlink(log_path) == desired_target:
            return canonical
        if log_path.exists() or log_path.is_symlink():
            log_path.unlink()
    except FileNotFoundError:
        pass
    log_path.symlink_to(Path(desired_target))
    return canonical


def copy_chat_artifact(
    repo_path: Path,
    *,
    log_dir: Path,
    artifact_prefix: Optional[str],
    role: str,
    source_path: Path,
    canonical_name: str,
    symlink_name: Optional[str] = None,
) -> Path:
    artifact_dir = chat_artifact_dir(repo_path, log_dir=log_dir, artifact_prefix=artifact_prefix, role=role)
    target = artifact_dir / canonical_name
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_path, target)
    if symlink_name:
        log_path = log_dir / symlink_name
        try:
            if log_path.exists() or log_path.is_symlink():
                log_path.unlink()
        except FileNotFoundError:
            pass
        log_path.symlink_to(Path(os.path.realpath(str(target))))
    return target


def commit_chat_checkpoint(repo_path: Path, *, tag: str) -> Optional[str]:
    chats = ensure_chat_repo(repo_path)
    _git(chats, "add", "-A")
    diff = _git(chats, "diff", "--cached", "--quiet", check=False)

    head_exists = _git(chats, "rev-parse", "--verify", "HEAD", check=False).returncode == 0
    if diff.returncode != 0:
        _git(chats, "commit", "-m", f"{tag}: chat snapshot")
    elif not head_exists:
        return None

    _git(chats, "tag", "-d", tag, check=False)
    _git(chats, "tag", tag)
    result = _git(chats, "rev-parse", "HEAD")
    return result.stdout.strip() or None


def commit_chat_attempt(
    repo_path: Path,
    *,
    cycle: int,
    attempt: int,
    label: str,
) -> Optional[str]:
    chats = ensure_chat_repo(repo_path)
    _git(chats, "add", "-A")
    diff = _git(chats, "diff", "--cached", "--quiet", check=False)
    if diff.returncode == 0:
        return None
    _git(
        chats,
        "commit",
        "-m",
        f"cycle-{int(cycle)} attempt-{int(attempt)}: {label}",
    )
    result = _git(chats, "rev-parse", "HEAD")
    return result.stdout.strip() or None


def rewind_chat_history(repo_path: Path, *, tag: str) -> None:
    chats = chat_repo_path(repo_path)
    if not (chats / ".git").exists():
        return

    normalized = str(tag or "").strip()
    if normalized == "initial":
        actual_ref = _root_commit(chats)
        target_key = (0, -1)
    else:
        check = _git(chats, "rev-parse", normalized, check=False)
        if check.returncode != 0:
            return
        actual_ref = normalized
        target_key = _tag_sort_key(normalized)

    if not actual_ref:
        return

    _git(chats, "reset", "--hard", actual_ref)
    _git(chats, "clean", "-fdx", timeout=120)

    tags_result = _git(chats, "tag", "-l", "cycle-*", check=False)
    if tags_result.returncode != 0:
        return
    for existing in [t.strip() for t in tags_result.stdout.splitlines() if t.strip()]:
        if _tag_sort_key(existing) > target_key:
            _git(chats, "tag", "-d", existing, check=False)


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except Exception:
        return ""


def _normalize_entry(role: str, text: str, kind: str = "message", title: str = "") -> Optional[Dict[str, str]]:
    trimmed = str(text or "").strip()
    if not trimmed:
        return None
    return {
        "role": role or "entry",
        "kind": kind,
        "title": title or "",
        "text": trimmed,
    }


def _collect_text_parts(value: Any, parts: List[str]) -> None:
    if isinstance(value, str):
        trimmed = value.strip()
        if trimmed:
            parts.append(trimmed)
        return
    if isinstance(value, list):
        for item in value:
            _collect_text_parts(item, parts)
        return
    if not isinstance(value, dict):
        return
    text = value.get("text")
    if isinstance(text, str) and text.strip():
        parts.append(text.strip())
    for key in ("content", "parts", "chunks", "value"):
        if key in value:
            _collect_text_parts(value.get(key), parts)


def _parse_codex_output_entries(text: str) -> List[Dict[str, str]]:
    entries: List[Dict[str, str]] = []
    for raw_line in str(text or "").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except Exception:
            continue
        item = rec.get("item")
        if rec.get("type") == "item.completed" and isinstance(item, dict) and item.get("type") == "agent_message":
            entry = _normalize_entry("assistant", str(item.get("text") or ""), "message", "Assistant")
            if entry:
                entries.append(entry)
            continue
        if (
            isinstance(item, dict)
            and item.get("type") == "command_execution"
            and rec.get("type") in {"item.started", "item.completed"}
        ):
            command = str(item.get("command") or "").strip()
            output = str(item.get("aggregated_output") or "").strip()
            label = "Command (running)" if rec.get("type") == "item.started" else "Command"
            combined = "\n\n".join(part for part in (command, output) if part)
            entry = _normalize_entry("tool", combined, "command", label)
            if entry:
                entries.append(entry)
    return entries


def _parse_jsonl_transcript_entries(text: str) -> List[Dict[str, str]]:
    entries: List[Dict[str, str]] = []
    for raw_line in str(text or "").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except Exception:
            continue
        msg = rec.get("message") if isinstance(rec.get("message"), dict) else rec
        role = ""
        if isinstance(msg, dict):
            role = str(msg.get("role") or rec.get("role") or rec.get("type") or "")
        parts: List[str] = []
        _collect_text_parts(msg.get("content") if isinstance(msg, dict) else rec.get("content"), parts)
        if not parts:
            _collect_text_parts(msg, parts)
        entry = _normalize_entry(role, "\n\n".join(parts), "message", role or "Entry")
        if entry:
            entries.append(entry)
    return entries


def _parse_json_transcript_entries(text: str) -> List[Dict[str, str]]:
    try:
        data = json.loads(text)
    except Exception:
        return []
    entries: List[Dict[str, str]] = []
    messages = data.get("messages") if isinstance(data, dict) else None
    if isinstance(messages, list):
        for msg in messages:
            if not isinstance(msg, dict):
                continue
            role = str(msg.get("role") or msg.get("author") or msg.get("speaker") or "")
            parts: List[str] = []
            _collect_text_parts(msg.get("content"), parts)
            if not parts:
                _collect_text_parts(msg.get("parts"), parts)
            if not parts:
                _collect_text_parts(msg, parts)
            entry = _normalize_entry(role, "\n\n".join(parts), "message", role or "Entry")
            if entry:
                entries.append(entry)
    if entries:
        return entries
    parts: List[str] = []
    _collect_text_parts(data, parts)
    fallback = _normalize_entry("entry", "\n\n".join(parts), "message", "Transcript")
    return [fallback] if fallback else []


def _artifact_title(name: str) -> str:
    if name == "worker_handoff":
        return "Worker"
    if name == "reviewer_decision":
        return "Reviewer"
    m = re.match(r"^correspondence_result_(\d+)$", name)
    if m:
        return f"Correspondence {int(m.group(1)) + 1}"
    m = re.match(r"^nl_proof_(.+)_(\d+)$", name)
    if m:
        return f"Soundness {m.group(1)} ({int(m.group(2)) + 1})"
    return name.replace("_", " ")


def _build_artifact_chat_data(artifact: str, files: Dict[str, str]) -> Dict[str, Any]:
    entries: List[Dict[str, str]] = []
    prompt_entry = _normalize_entry("prompt", files.get("prompt", ""), "prompt", "Prompt")
    if prompt_entry:
        entries.append(prompt_entry)
    entries.extend(_parse_jsonl_transcript_entries(files.get("transcriptJsonl", "")))
    entries.extend(_parse_json_transcript_entries(files.get("transcriptJson", "")))
    if not any(entry.get("role") == "assistant" for entry in entries):
        entries.extend(_parse_codex_output_entries(files.get("output", "")))
    return {
        "id": artifact,
        "title": _artifact_title(artifact),
        "entries": entries,
    }


def _read_working_tree_chat_files(repo_path: Path, cycle: int, artifact: str) -> Dict[str, str]:
    base = ensure_chat_repo(repo_path) / chat_cycle_dir_name(cycle) / artifact
    return {
        "prompt": _read_text(base / "prompt.txt"),
        "output": _read_text(base / "output.log"),
        "transcriptJsonl": _read_text(base / "transcript.jsonl"),
        "transcriptJson": _read_text(base / "transcript.json"),
    }


def _read_git_chat_file(chats_repo: Path, tag: str, rel_path: str) -> str:
    result = _git(chats_repo, "show", f"{tag}:{rel_path}", check=False, timeout=30)
    if result.returncode != 0:
        return ""
    return result.stdout


def _read_git_chat_files(repo_path: Path, cycle: int, artifact: str) -> Dict[str, str]:
    chats_repo = ensure_chat_repo(repo_path)
    base = f"{chat_cycle_dir_name(cycle)}/{artifact}"
    tag = f"cycle-{cycle}"
    return {
        "prompt": _read_git_chat_file(chats_repo, tag, f"{base}/prompt.txt"),
        "output": _read_git_chat_file(chats_repo, tag, f"{base}/output.log"),
        "transcriptJsonl": _read_git_chat_file(chats_repo, tag, f"{base}/transcript.jsonl"),
        "transcriptJson": _read_git_chat_file(chats_repo, tag, f"{base}/transcript.json"),
    }


def _list_working_tree_chat_artifacts(repo_path: Path, cycle: int) -> List[str]:
    root = ensure_chat_repo(repo_path) / chat_cycle_dir_name(cycle)
    if not root.exists():
        return []
    return sorted(entry.name for entry in root.iterdir() if entry.is_dir())


def _list_git_chat_artifacts(repo_path: Path, cycle: int) -> List[str]:
    chats_repo = ensure_chat_repo(repo_path)
    prefix = f"{chat_cycle_dir_name(cycle)}/"
    result = _git(chats_repo, "ls-tree", "-r", "--name-only", f"cycle-{cycle}", "--", prefix, check=False)
    if result.returncode != 0:
        return []
    artifacts = {
        line[len(prefix):].split("/", 1)[0]
        for line in result.stdout.splitlines()
        if line.startswith(prefix) and line[len(prefix):]
    }
    return sorted(artifacts)


def read_live_chats(repo_path: Path, cycle: int) -> Dict[str, Any]:
    artifacts = _list_working_tree_chat_artifacts(repo_path, cycle)
    return {
        "cycle": cycle,
        "source": "live",
        "artifacts": [_build_artifact_chat_data(artifact, _read_working_tree_chat_files(repo_path, cycle, artifact)) for artifact in artifacts],
    }


def read_runtime_live_chats(
    repo_path: Path,
    cycle: int,
    *,
    request_cycles: Optional[Dict[int, int]] = None,
) -> Dict[str, Any]:
    live_root = project_state_dir_for_repo(repo_path) / "chats" / "live"
    artifacts: List[Dict[str, Any]] = []
    if live_root.is_dir():
        for artifact_dir in sorted(child for child in live_root.iterdir() if child.is_dir()):
            if request_cycles is not None:
                request_id = _request_id_from_artifact_name(artifact_dir.name)
                if request_id is None or request_cycles.get(request_id) != cycle:
                    continue
            files = {
                "prompt": _read_text(artifact_dir / "prompt.txt"),
                "output": _read_text(artifact_dir / "output.log"),
                "transcriptJsonl": _read_text(artifact_dir / "transcript.jsonl"),
                "transcriptJson": _read_text(artifact_dir / "transcript.json"),
            }
            artifacts.append(_build_artifact_chat_data(artifact_dir.name, files))
    return {
        "cycle": cycle,
        "source": "live",
        "artifacts": artifacts,
    }


def read_historical_chats(repo_path: Path, cycle: int) -> Dict[str, Any]:
    artifacts = _list_git_chat_artifacts(repo_path, cycle)
    return {
        "cycle": cycle,
        "source": f"cycle-{cycle}",
        "artifacts": [_build_artifact_chat_data(artifact, _read_git_chat_files(repo_path, cycle, artifact)) for artifact in artifacts],
    }
