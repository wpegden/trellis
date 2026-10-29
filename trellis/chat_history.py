"""Canonical project-local chat history paths and git operations."""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import subprocess
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from trellis.project_paths import (
    project_chats_dir,
    project_event_log_dir,
    project_state_dir_for_repo,
)


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


_EVENT_LOG_CYCLE_FILE_RE = re.compile(r"^cycle-\d{6,}\.jsonl$")


def _event_log_cycle_files(event_log_dir: Path) -> List[Path]:
    """Lexically-sorted per-cycle event-log files (`cycle-NNNNNN.jsonl`).
    Lexical order equals cycle order equals global index order. Returns an
    empty list when the directory is absent."""
    if not event_log_dir.is_dir():
        return []
    files = [
        path
        for path in event_log_dir.iterdir()
        if path.is_file() and _EVENT_LOG_CYCLE_FILE_RE.match(path.name)
    ]
    files.sort(key=lambda path: path.name)
    return files


def _stream_event_log_records(event_log_dir: Path, *, files=None, on_error=None):
    """Yield parsed records across the per-cycle files in global index order.
    The completed files are immutable; only the current cycle's file may grow,
    and a reader may observe a trailing partial line there — line-oriented
    iteration with per-line JSON parsing skips a partial/garbled final line
    safely. Lines that fail to parse are skipped."""
    for path in _event_log_cycle_files(event_log_dir) if files is None else files:
        try:
            with path.open("r", encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    stripped = line.strip()
                    if not stripped:
                        continue
                    try:
                        yield json.loads(stripped)
                    except Exception:
                        # A trailing partial line on the live cycle file, or a
                        # garbled record — skip it.
                        continue
        except OSError as error:
            if on_error is not None:
                on_error(error)
            continue


def _request_cycles_from_event_log(event_log_dir: Path, *, files=None, on_error=None) -> Dict[int, int]:
    """Return {request_id: cycle} built from `issue_request` commands across
    the per-cycle event-log files. Per-cycle layout makes the request→cycle
    mapping structural (no byte-offset cache needed); the map is small (one
    entry per issued request)."""
    mapping: Dict[int, int] = {}
    for record in _stream_event_log_records(event_log_dir, files=files, on_error=on_error):
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
    return mapping


def rebuild_cycle_chat_dirs(repo_path: Path, *, runtime_root: Optional[Path] = None) -> Dict[int, List[str]]:
    chats = ensure_chat_repo(repo_path)
    live_dir = chats / "live"
    if not live_dir.is_dir():
        return {}

    # The event log now lives in the tracked repo tree
    # (`<repo>/.trellis-history/event-log/`), so the request→cycle map is
    # derived from `repo_path` directly; `runtime_root` is no longer needed
    # to locate it. The parameter is retained for call-site compatibility.
    from . import chat_projection_cache as cache
    cache_path = chats.parent / 'chat-projection-cache.json'
    previous = cache.load(cache_path)
    event_dir = project_event_log_dir(repo_path)
    def parse_cycle(path):
        failed = []
        mapping = _request_cycles_from_event_log(event_dir, files=[path], on_error=failed.append)
        return mapping, not failed
    request_cycles, event_cache = cache.request_cycles(
        _event_log_cycle_files(event_dir), previous.get('events', {}), parse_cycle)
    if not request_cycles:
        return {}

    cycle_artifacts: Dict[int, List[Tuple[str, Path]]] = {}
    for artifact_dir in sorted(child for child in live_dir.iterdir() if child.is_dir()):
        request_id = _request_id_from_artifact_name(artifact_dir.name)
        if request_id is None:
            continue
        cycle = request_cycles.get(request_id)
        if cycle is None:
            continue
        cycle_artifacts.setdefault(cycle, []).append((artifact_dir.name, artifact_dir))

    expected_cycles = {chat_cycle_dir_name(cycle) for cycle in cycle_artifacts}
    for cycle_dir in chats.iterdir():
        if cycle_dir.is_dir() and ARTIFACT_CYCLE_DIR_RE.fullmatch(cycle_dir.name):
            if cycle_dir.is_symlink():
                raise ValueError('chat cycle projection must not be a symlink')
            if cycle_dir.name not in expected_cycles:
                shutil.rmtree(cycle_dir)

    artifact_cache = {}
    materialized: Dict[int, List[str]] = {}
    for cycle, artifacts in cycle_artifacts.items():
        cycle_root = chats / chat_cycle_dir_name(cycle)
        if cycle_root.is_symlink():
            raise ValueError('chat cycle projection must not be a symlink')
        cycle_root.mkdir(parents=True, exist_ok=True)
        expected_artifacts = {name for name, _ in artifacts}
        for child in cycle_root.iterdir():
            if child.name not in expected_artifacts:
                if child.is_dir() and not child.is_symlink():
                    shutil.rmtree(child)
                else:
                    child.unlink()
        names: List[str] = []
        for artifact_name, source_dir in artifacts:
            target_dir = cycle_root / artifact_name
            key = f'{cycle_root.name}/{artifact_name}'
            entry = cache.copy_artifact(source_dir, target_dir,
                                        previous.get('artifacts', {}).get(key))
            if entry is not None:
                artifact_cache[key] = entry
            names.append(artifact_name)
        materialized[cycle] = names
    cache.save(cache_path, dict(events=event_cache, artifacts=artifact_cache))
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
    engineering: bool = False,
) -> Path:
    chats = chat_repo_path(repo_path) if engineering else ensure_chat_repo(repo_path)
    cycle_dir = _cycle_dir_name_from_log_dir(log_dir)
    artifact_dir = chats / cycle_dir / _artifact_prefix(artifact_prefix, role)
    if engineering:
        from trellis.agents.host_files import safe_directory
        with safe_directory(artifact_dir):
            pass
    else:
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
    engineering: bool = False,
) -> Path:
    artifact_dir = chat_artifact_dir(repo_path, log_dir=log_dir, artifact_prefix=artifact_prefix,
                                     role=role, engineering=engineering)
    canonical = artifact_dir / canonical_name
    log_path = log_dir / log_filename
    if engineering:
        from trellis.agents.host_files import safe_directory
        if Path(canonical_name).name != canonical_name or Path(log_filename).name != log_filename:
            raise ValueError('Engineering chat filenames require one path component')
        with safe_directory(log_dir) as (_, fd):
            try:
                os.unlink(log_filename, dir_fd=fd)
            except FileNotFoundError:
                pass
            os.symlink(str(canonical), log_filename, dir_fd=fd)
        return canonical
    log_path.parent.mkdir(parents=True, exist_ok=True)
    # Resolve symlinks in the target so the link is valid inside bwrap
    # sandboxes that bind-mount the realpath (e.g. when /path/to/trellis/math
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


def _parse_codex_output_entries(
    text: str, *, tool_names: Optional[Dict[str, str]] = None,
) -> List[Dict[str, Any]]:
    entries: List[Dict[str, Any]] = []
    tool_names = {} if tool_names is None else tool_names
    for raw_line in str(text or "").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except Exception:
            continue
        if not isinstance(rec, dict):
            continue
        entry = None
        payload = rec.get("payload")
        if rec.get("type") == "response_item" and isinstance(payload, dict):
            payload_type = payload.get("type")
            if payload_type == "message":
                parts: List[str] = []
                _collect_text_parts(payload.get("content"), parts)
                role = str(payload.get("role") or "entry")
                entry = _normalize_entry(role, "\n\n".join(parts), "message", role)
            elif payload_type == "reasoning":
                parts = []
                _collect_text_parts(payload.get("summary"), parts)
                entry = _normalize_entry("assistant", "\n\n".join(parts), "thinking", "Reasoning summary")
            elif payload_type in {"function_call", "custom_tool_call", "function_call_output", "custom_tool_call_output"}:
                call_id = str(payload.get("call_id") or "")
                is_output = payload_type.endswith("_output")
                if is_output:
                    name = tool_names.get(call_id, "tool")
                    value = payload.get("output")
                else:
                    name = ".".join(str(payload[key]) for key in ("namespace", "name") if payload.get(key)) or "tool"
                    tool_names[call_id] = name
                    value = payload.get("input") if payload_type == "custom_tool_call" else payload.get("arguments")
                parts = []
                _collect_text_parts(value, parts)
                body = "\n\n".join(parts) if parts else json.dumps(value, ensure_ascii=False)
                entry = _normalize_entry("tool", f"{name}\n\n{body}", "command",
                                         "Tool output" if is_output else "Tool call")
                entry["call_id"] = call_id[:160]
                entry["tool_name"] = name[:160]
                if is_output:
                    try:
                        result = json.loads(value) if isinstance(value, str) else value
                    except ValueError:
                        result = None
                    if isinstance(result, dict):
                        code = result.get("exit_code", result.get("returncode"))
                        if isinstance(code, int):
                            entry["exit_code"] = code
            # Session metadata and encrypted reasoning are not transcript entries.
            if entry:
                if rec.get("timestamp"):
                    entry["timestamp"] = str(rec["timestamp"])[:80]
                entries.append(entry)
            continue
        item = rec.get("item")
        event_type = rec.get("type")
        if event_type == "event_msg" and isinstance(payload, dict) and payload.get("type") == "item_completed":
            raw_item = payload.get("item")
            if isinstance(raw_item, dict) and raw_item.get("type") == "CommandExecution":
                item = {**raw_item, "type": "command_execution"}
                event_type = "item.completed"
        if rec.get("type") == "item.completed" and isinstance(item, dict) and item.get("type") == "agent_message":
            entry = _normalize_entry("assistant", str(item.get("text") or ""), "message", "Assistant")
            if entry:
                entries.append(entry)
            continue
        if rec.get("type") == "item.completed" and isinstance(item, dict) and item.get("type") == "reasoning":
            entry = _normalize_entry("assistant", str(item.get("text") or ""), "thinking", "Reasoning summary")
            if entry:
                entries.append(entry)
            continue
        if (
            isinstance(item, dict)
            and item.get("type") == "command_execution"
            and event_type in {"item.started", "item.completed"}
        ):
            command = item.get("command") or ""
            command = shlex.join(str(part) for part in command) if isinstance(command, list) else str(command).strip()
            output = str(item.get("aggregated_output") or "").strip()
            label = "Command (running)" if event_type == "item.started" else "Command"
            combined = "\n\n".join(part for part in (command, output) if part)
            entry = _normalize_entry("tool", combined, "command", label)
            if entry:
                if rec.get("timestamp"):
                    entry["timestamp"] = str(rec["timestamp"])[:80]
                if rec.get("type") == "event_msg":
                    entry["exit_code"] = item.get("exit_code")
                    if item.get("process_id") is not None:
                        entry["process_id"] = str(item["process_id"])[:160]
                entries.append(entry)
    return entries


def _parse_jsonl_transcript_entries(text: str) -> List[Dict[str, Any]]:
    entries: List[Dict[str, Any]] = []
    tool_names: Dict[str, str] = {}
    for raw_line in str(text or "").splitlines():
        line = raw_line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except Exception:
            continue
        if not isinstance(rec, dict):
            continue
        if rec.get("type") in {"response_item", "event_msg"}:
            entries.extend(_parse_codex_output_entries(raw_line, tool_names=tool_names))
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


def _append_transcript_entry(result: Dict[str, Any], entry: Dict[str, Any], max_chars: int) -> bool:
    result["entries"].append(entry)
    if len(json.dumps(result, ensure_ascii=False)) > max_chars - 100:
        result["entries"].pop()
        result["has_more"] = True
        return False
    return True


def _inspect_transcript_directory(path: Path, *, after_line: int, limit: int,
                                  max_chars: int) -> Dict[str, Any]:
    result = {"path": str(path.resolve()), "kind": "directory", "entries": [],
              "next_line": after_line, "has_more": False,
              "hint": "Select a file to inspect and filter its records. For more directory entries, use --after-line with next_line."}
    number = 0
    for child in sorted(path.iterdir(), key=lambda item: item.name):
        is_directory = child.is_dir()
        if not is_directory and not (child.suffix.casefold() in {".jsonl", ".ndjson", ".log"} and child.is_file()):
            continue
        number += 1
        if number <= after_line:
            continue
        if len(result["entries"]) >= limit:
            result["has_more"] = True
            return result
        entry = {"path": str(child.absolute()), "kind": "directory" if is_directory else "transcript"}
        if not is_directory:
            entry["bytes"] = child.stat().st_size
        if not _append_transcript_entry(result, entry, max_chars):
            return result
        result["next_line"] = number
    return result


def inspect_transcript(path: Path, *, after_line: int = 0, kind: str = "all",
                       contains: str = "", limit: int = 10, max_chars: int = 12000) -> Dict[str, Any]:
    """List a transcript directory or page visible JSONL entries with shared parsers.

    Character budgets apply after JSON encoding, so long records and escaped
    tool output remain bounded. Line numbers locate the unchanged full evidence.
    """
    limit, max_chars = max(1, min(limit, 50)), max(2000, min(max_chars, 16000))
    if path.is_dir():
        return _inspect_transcript_directory(path, after_line=after_line, limit=limit, max_chars=max_chars)
    result = {"path": str(path.resolve()), "entries": [], "next_line": after_line, "has_more": False}
    needle = contains.casefold()
    tool_names: Dict[str, str] = {}
    with path.open(encoding="utf-8", errors="replace") as stream:
        for number, raw in enumerate(stream, 1):
            if number <= after_line:
                _parse_codex_output_entries(raw, tool_names=tool_names)
                continue
            try:
                record = json.loads(raw)
            except ValueError:
                result["next_line"] = number
                continue
            if not isinstance(record, dict) or record.get("type") == "item.started":
                result["next_line"] = number
                continue
            entries = _parse_codex_output_entries(raw, tool_names=tool_names) or _parse_jsonl_transcript_entries(raw)
            item = record.get("item") or {}
            exit_code = item.get("exit_code") if isinstance(item, dict) else None
            for entry in entries:
                exit_code = entry.get("exit_code", exit_code)
                if kind == "failure" and (entry["kind"] != "command" or exit_code in (None, 0)):
                    continue
                if kind not in {"all", "failure"} and kind != entry["kind"]:
                    continue
                text = entry["text"]
                match = text.casefold().find(needle) if needle else 0
                if match < 0:
                    continue
                if len(result["entries"]) >= limit:
                    result["has_more"] = True
                    return result
                start = max(0, match - 250)
                preview = text[start:start + 2400]
                entry = {**entry, "role": entry["role"][:80], "title": entry["title"][:120],
                         "text": preview, "line": number, "exit_code": exit_code,
                         "text_chars": len(text), "text_offset": start,
                         "truncated": start > 0 or len(preview) < len(text)}
                # A page always admits its first matching entry and makes progress.
                while len(json.dumps(entry, ensure_ascii=False)) > max_chars - 1000:
                    entry["text"] = entry["text"][:len(entry["text"]) // 2]
                    entry["truncated"] = True
                if not _append_transcript_entry(result, entry, max_chars):
                    return result
            result["next_line"] = number
    return result


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
