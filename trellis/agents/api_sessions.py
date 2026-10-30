"""Host-owned API conversation checkpoints; never stored in a writable agent home."""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import uuid

from trellis.agents.host_files import safe_directory, atomic_text


def state_root():
    return Path(os.environ.get('TRELLIS_API_STATE_DIR', '~/.local/state/trellis-api')).expanduser().resolve()


def read(directory, fd, name):
    try:
        opened = os.open(name, os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
    except FileNotFoundError:
        return None
    with os.fdopen(opened) as stream:
        return json.load(stream)


def checkpoint_path(thread):
    # A thread ID is never an arbitrary filesystem path.
    if str(uuid.UUID(thread)) != thread:
        raise ValueError('Invalid API conversation identity')
    return state_root() / (thread + '.json')


def require_checkpoint(binding):
    path = checkpoint_path(binding['thread_id'])
    with safe_directory(path.parent) as (directory, fd):
        value = read(directory, fd, path.name)
    if not value or value['binding'] != binding:
        raise RuntimeError('worker correction API conversation is unavailable or has a different binding')


@contextmanager
def conversation(*, config, work_dir, home, role, scope, fresh, exact_thread='', endpoint=''):
    binding = dict(provider=config.provider, model=config.model, reasoning_effort=config.effort,
                   home=str(Path(home).resolve()) if home else '', thread_id=exact_thread)
    # Match the existing worker/reviewer persistence rules; verification stays fresh.
    parts = scope.split(':')
    persistent = role == 'worker' or (role == 'reviewer' and len(parts) >= 3 and parts[2] == 'review')
    identity = dict(binding, work_dir=str(work_dir.resolve()), role=role, scope=scope, endpoint=endpoint,
                    extra_args=config.extra_args)
    identity.pop('thread_id')
    scope_key = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    lock_key = scope_key + '.scope'
    with safe_directory(state_root()) as (directory, fd):
        lock = os.open(lock_key + '.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600, dir_fd=fd)
        thread_lock = None
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            pointer_name = scope_key + '.scope.json'
            pointer = read(directory, fd, pointer_name) if persistent and not fresh and not exact_thread else None
            thread = exact_thread or (pointer or {}).get('thread_id') or str(uuid.uuid4())
            path = checkpoint_path(thread)
            thread_lock = os.open(thread + '.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600, dir_fd=fd)
            fcntl.flock(thread_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            value = read(directory, fd, path.name)
            binding['thread_id'] = thread
            if exact_thread and value is None:
                raise RuntimeError('API conversation is unavailable; refusing a fresh replacement')
            if value and (value['binding'] != binding or value['endpoint'] != endpoint):
                raise RuntimeError('API continuation changed its provider/model/home/effort/endpoint binding')
            value = value or dict(version=1, binding=binding, endpoint=endpoint, messages=[])

            def save():
                atomic_text(directory, fd, path.name, json.dumps(value))
                if persistent and not exact_thread:
                    atomic_text(directory, fd, pointer_name, json.dumps({'thread_id': thread}))

            # An interrupted command may have edited files before its result was
            # checkpointed. Close pending tool calls without executing them again.
            pending = {}
            for message in value['messages']:
                if message['role'] == 'assistant':
                    pending.update({call['id']: call for call in message.get('tool_calls') or []})
                elif message['role'] == 'tool':
                    pending.pop(message['tool_call_id'], None)
            for ident in pending:
                value['messages'].append({'role': 'tool', 'tool_call_id': ident,
                    'content': 'Execution was interrupted before a durable result was recorded. The command may have run. Inspect current files before deciding what to do; do not assume it is safe to repeat.'})
            save()
            yield value, save
        finally:
            if thread_lock is not None:
                os.close(thread_lock)
            os.close(lock)
