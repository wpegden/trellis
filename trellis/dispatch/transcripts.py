"""Publish completed chats once; share only advisory history with attempts."""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shutil
import stat
import tempfile

from trellis.chat_history import _request_id_from_artifact_name, chat_repo_path
from .candidate import canonical_json, digest, _sync_dir
from .publication import atomic


RECENT_ATTEMPTS = 40
CHAT_FILES = ('prompt.txt', 'output.log', 'transcript.jsonl', 'transcript.json')
_ATTEMPT = re.compile(r'(?:[1-9][0-9]*|(?:review|verifier|exclusive)-[0-9]+)\Z')
_JSON_LIMIT = 32 * 1024 * 1024
_CHAT_LIMIT = 128 * 1024 * 1024


def _role(kind):
    name = str(kind).lower()
    return {'parallel_review': 'review', 'stuckmathaudit': 'stuck_math_audit'}.get(name, name)


def _regular(path):
    # Completed agents cannot race these checks: cleanup precedes result.json.
    for parent in (path, *path.parents):
        if parent.is_symlink():
            raise ValueError('chat history cannot traverse a symlink')
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError('chat history requires a regular file')
    return path


def _read(path, limit=_JSON_LIMIT):
    with _regular(path).open('rb') as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError('chat history file exceeds the size limit')
    return data


def readonly_paths(repo):
    """Only published chats and immutable Git objects enter agent sandboxes."""
    chats = chat_repo_path(repo)
    paths = [path for path in (chats / 'parallel', chats / '.git/objects') if path.is_dir()]
    if any(path.resolve() != path for path in paths):
        raise ValueError('published chat history cannot traverse a symlink')
    return paths


def _publication_name(directory, turn, transport=0):
    name = directory.name if turn is None else f'{directory.name}-turn-{turn}'
    return f'{name}-transport-{transport}' if transport else name


def _publish(directory, published, prepared, turn=None, transport=0):
    request_id = prepared['request_id']
    execution = directory / 'execution' if turn is None else directory / 'turns' / str(turn) / 'execution'
    if transport:
        execution = execution.parent / 'transports' / str(transport) / 'execution'
    receipt = json.loads(_read(execution / 'result.json', 65536))
    destination = published / _publication_name(directory, turn, transport)
    if destination.exists():
        saved = json.loads(_read(destination / 'history.json', 65536))
        if saved['response_sha256'] != receipt['response_sha256']:
            raise ValueError('published chat history has a different completion receipt')
        return saved
    response_bytes = _read(execution / 'response.json')
    if digest(response_bytes) != receipt['response_sha256']:
        raise ValueError('chat history response does not match its completion receipt')
    response = json.loads(response_bytes)
    if not isinstance(response, dict):
        raise ValueError('chat history response must be an object')
    if response.get('kind') != 'parallel_review' and response.get('request_id') != request_id:
        raise ValueError('chat history response does not match its prepared request')
    if turn is not None and (response.get('kind') != 'worker_submission' or response.get('turn') != turn):
        raise ValueError('chat history response does not match its worker turn')
    context = prepared.get('history', {})
    row = {key: context[key] for key in ('cycle', 'kind', 'active_node', 'mode')
           if context.get(key) is not None}
    if 'cycle' not in row and type(response.get('cycle')) is int:
        row['cycle'] = response['cycle']
    if 'kind' not in row:
        row['kind'] = {'parallel_review': 'Review', 'worker_submission': 'Worker', 'stuck_math_audit': 'StuckMathAudit',
                       'human_gate': 'HumanGate'}.get(response.get('kind'), str(response.get('kind', '')).title())
    row.update(request_id=request_id, attempt=directory.name, advisory=True,
               returncode=receipt.get('returncode'),
               response_sha256=receipt['response_sha256'], chats=[])
    if turn is not None:
        row.update(submission_turn=turn, unaccepted=True)
        try:
            binding = json.loads(_read(execution / 'clock/config.json', 65536))['binding']
            row.update({key: binding[key][:256] for key in ('provider', 'model', 'reasoning_effort')
                        if isinstance(binding.get(key), str)})
        except (OSError, ValueError, KeyError, TypeError):
            pass
    detail = response.get('raw') if turn is not None else response.get('action')
    if not isinstance(detail, dict):
        detail = response
    for key in ('status', 'outcome', 'decision', 'next_mode', 'reset', 'summary', 'comments'):
        value = detail.get(key)
        if isinstance(value, str):
            row[key] = value[:600]
    if isinstance(response.get('assignments'), list):
        row['assignments_count'] = len(response['assignments'])
    temp = Path(tempfile.mkdtemp(prefix='.publish-', dir=published))
    try:
        live = directory / 'repo/.trellis/chats/live'
        if live.exists() or live.is_symlink():
            for parent in (live, live.parent, live.parent.parent, live.parent.parent.parent):
                if parent.is_symlink():
                    raise ValueError('chat history cannot traverse a symlink')
            submission_artifact = f'trellis_worker_{request_id}_submission-{turn}'
            if transport:
                submission_artifact += f'-transport-{transport}'
            artifacts = [path for path in live.iterdir()
                         if (path.name == submission_artifact if turn is not None else
                             path.name.startswith(f'trellis_{_role(response.get("kind"))}_')
                             and _request_id_from_artifact_name(path.name) == request_id)]
            if len(artifacts) > 16:
                raise ValueError('chat history exceeds the artifact limit')
            remaining = _CHAT_LIMIT
            for artifact in sorted(artifacts):
                if artifact.is_symlink() or not artifact.is_dir():
                    raise ValueError('chat history requires a regular artifact directory')
                copied = []
                for name in CHAT_FILES:
                    source = artifact / name
                    if not source.exists() and not source.is_symlink():
                        continue
                    size = _regular(source).stat().st_size
                    if size > remaining:
                        raise ValueError('chat history file exceeds the size limit')
                    remaining -= size
                    target = temp / artifact.name / name
                    target.parent.mkdir(exist_ok=True)
                    # Never link an agent-writable inode into shared history.
                    shutil.copyfile(source, target)
                    with target.open('rb') as stream:
                        os.fsync(stream.fileno())
                    copied.append(name)
                if copied:
                    _sync_dir(temp / artifact.name)
                    labels = {}
                    try:
                        call = json.loads(_read(artifact / 'call.json', 65536))
                        labels = {key: call[key][:256] for key in ('provider', 'model')
                                  if isinstance(call.get(key), str)}
                    except (OSError, ValueError, TypeError, AttributeError):
                        pass
                    for key, value in labels.items():
                        row.setdefault(key, value)
                    row['chats'].append(dict(artifact=artifact.name, files=copied, **labels))
        atomic(temp / 'history.json', canonical_json(row))
        os.rename(temp, destination)
        _sync_dir(published)
        return row
    finally:
        if temp.exists():
            shutil.rmtree(temp)


def prepare_history(runtime, repo, work_repo, *, request=None):
    """Called once per fresh workspace, under the coordinator lock.

    Discovery never descends into dispatch helpers or sibling source trees.
    Completed/reaped receipts make older installations immediately readable;
    later preparations reuse the published bytes, without recopying transcripts.
    """
    candidates = []
    for directory in (runtime / 'dispatch').iterdir():
        if not _ATTEMPT.fullmatch(directory.name) or directory.is_symlink():
            continue
        from .storage import execution_directories, worker_execution_coordinates
        try:
            executions = execution_directories(directory)
        except (OSError, ValueError):
            continue
        for execution in executions:
            if execution.parent.parent.name in ('acceptance', 'finalization'):
                continue
            coordinates = worker_execution_coordinates(execution)
            turn, transport = coordinates if coordinates else (None, 0)
            try:
                completed_at = _regular(execution / 'result.json').stat().st_mtime_ns
                process = _read(execution / 'process.json', 65536)
                if json.loads(process) != json.loads(_read(execution / 'cleanup.json', 65536)):
                    continue
                prepared = json.loads(_read(directory / 'prepared.json', 65536))
                request_id = prepared['request_id']
                if type(request_id) is int and request_id >= 0:
                    candidates.append((completed_at, request_id, directory, prepared, turn, transport))
            except (OSError, ValueError, KeyError):
                continue
    published = chat_repo_path(repo) / 'parallel'
    selected = sorted(candidates, key=lambda item: (item[0], item[1], item[2].name, item[4] or 0))[-RECENT_ATTEMPTS:]
    if not selected:
        return
    if published.resolve() != published:
        raise ValueError('published chat history cannot traverse a symlink')
    published.mkdir(parents=True, exist_ok=True)
    live = work_repo / '.trellis/chats/live'
    live.mkdir(parents=True, exist_ok=True)
    history = work_repo / '.trellis/logs/burst-history.jsonl'
    history.parent.mkdir(parents=True, exist_ok=True)
    with history.open('ab') as stream:
        for _, request_id, directory, prepared, turn, transport in selected:
            try:
                row = _publish(directory, published, prepared, turn, transport)
                paths = []
                for chat in row['chats']:
                    name = chat['artifact']
                    path = live / name
                    source = published / _publication_name(directory, turn, transport) / name
                    # Never let provider setup write through an inherited link
                    # with its own name. Duplicate names retain distinct paths.
                    reserved = request and request_id == request.get('id') and name.startswith(
                        f'trellis_{_role(request.get("kind"))}_')
                    if reserved or path.exists() or path.is_symlink():
                        paths.append(str(source))
                    else:
                        path.symlink_to(source)
                        paths.append(str(Path('.trellis/chats/live') / name))
                row = dict(row, chats=paths)
            except (OSError, ValueError, KeyError) as exc:
                row = dict(request_id=request_id, attempt=directory.name, advisory=True,
                           history_unavailable=str(exc)[:500])
                if turn is not None:
                    row['submission_turn'] = turn
            stream.write(canonical_json(row) + b'\n')
