"""Sample an owned closure clock for advisory tool replies, without clock authority.

The launcher alone supplies the capability descriptor. No path or allowance in
an agent RPC is used, and sampling never writes a receipt or grants execution.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import stat
import time


def _signature(value):
    return [value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns]


def _read(path, limit=65536):
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC), 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > limit:
            raise ValueError('clock feedback requires a bounded independent host record')
        raw = stream.read(limit + 1)
        if len(raw) > limit or _signature(os.fstat(stream.fileno())) != _signature(before):
            raise ValueError('clock feedback record changed while reading')
    return json.loads(raw), hashlib.sha256(raw).hexdigest(), _signature(before)


def descriptor(root, spec, *, loaded_recipe=None):
    """Optional metadata for a new, permitted parallel closure launcher only."""
    try:
        from trellis.dispatch.process import identity
        root = Path(root)
        if loaded_recipe is None:
            # Compatibility for direct callers; the durable launcher supplies
            # its already loaded recipe, including normal large closure inputs.
            recipe, recipe_hash, signature = _read(root / 'recipe.json', 16 * 1024 * 1024)
        else:
            recipe, recipe_hash, signature = loaded_recipe
            current = (root / 'recipe.json').stat(follow_symlinks=False)
            if (not isinstance(recipe_hash, str) or len(recipe_hash) != 64
                    or not stat.S_ISREG(current.st_mode) or current.st_nlink != 1
                    or signature != _signature(current)):
                return None
        payload = recipe['payload']
        request = payload['request']
        task = request['task_execution']
        turn = payload['worker_turn']
        process = _read(root / 'process.json')[0]
        maximum = request['worker_submission_policy']['max_infrastructure_retries']
        if (recipe['checker'] != spec or spec.get('serial')
                or str(request['kind']).lower() != 'worker'
                or task['task_kind'] != 'lean_closure' or task['compiler'] != 'managed'
                or process != identity(os.getpid()) or _read(root / 'permit.json')[0] != process
                or Path(turn['execution_record']) != root / 'clock'
                or type(turn['remaining_ms']) is not int or turn['remaining_ms'] <= 0
                or type(turn['turn']) is not int or turn['turn'] <= 0
                or type(maximum) is not int or not 0 <= maximum < 64):
            return None
        expected = {key: task['binding'][key] for key in ('provider', 'model', 'reasoning_effort')}
        if any(not isinstance(value, str) or not value for value in expected.values()):
            return None
        return dict(version=1, execution=str(root), process=process,
                    recipe_sha256=recipe_hash, recipe_signature=signature,
                    task_id=task['task_id'], turn=turn['turn'], remaining_ms=turn['remaining_ms'],
                    provider=expected, session=turn.get('session'), maximum_segments=maximum + 1)
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, AttributeError):
        # This optional display must never prevent an otherwise valid launch.
        return None


def sample(binding):
    """Return a fresh advisory sample, or omit unavailable/untrusted timing."""
    try:
        from trellis.agent_wrapper import provider_clock
        from trellis.dispatch.process import alive
        if not isinstance(binding, dict) or binding.get('version') != 1:
            return None
        root = Path(binding['execution'])
        if (not alive(binding['process'])
                or _signature((root / 'recipe.json').stat(follow_symlinks=False)) != binding['recipe_signature']
                or _read(root / 'process.json')[0] != binding['process']
                or _read(root / 'permit.json')[0] != binding['process']):
            return None
        clock = root / 'clock'
        if clock.is_symlink():
            return None
        config = _read(clock / 'config.json')[0]
        if (config['allowance_ms'] != binding['remaining_ms']
                or any(config['binding'].get(key) != value for key, value in binding['provider'].items())
                or (binding['session'] is not None and config['binding'] != binding['session'])
                or (binding['session'] is None and config['binding'].get('thread_id') != '')):
            return None
        segments = clock / 'segments'
        if segments.is_symlink():
            return None
        children = list(segments.iterdir()) if segments.exists() else []
        if len(children) + 1 > binding['maximum_segments']:
            return None
        # Bound every file _segments reads before using its existing exact
        # hash, debit, order and conversation-chain validation.
        for directory in [clock, *children]:
            if directory.is_symlink() or not directory.is_dir():
                return None
            _read(directory / 'config.json')
            if (directory / 'finished.json').exists():
                _read(directory / 'finished.json')
        paths = provider_clock._segments(clock)
        active = paths[-1]
        if any(os.path.lexists(active / name) for name in ('finished.json', 'exited.json')):
            return None
        active_config, config_hash, _ = _read(active / 'config.json')
        owner = _read(active / 'owner.json')[0]
        started = _read(active / 'started.json')[0]
        session = _read(active / 'session.json')[0]
        if (owner['config_sha256'] != config_hash or not alive(owner['owner'])
                or any(record.get('config_sha256') != config_hash or record.get('owner') != owner['owner']
                       for record in (started, session))
                or started['allowance_ms'] != active_config['allowance_ms']
                or not isinstance(session.get('thread_id'), str) or not session['thread_id']
                or (active_config['binding'].get('thread_id')
                    and session['thread_id'] != active_config['binding']['thread_id'])):
            return None
        now = time.monotonic_ns()
        start = started['monotonic_ns']
        allowance = active_config['allowance_ms']
        if type(start) is not int or not 0 <= start <= now or type(allowance) is not int or allowance <= 0:
            return None
        remaining = max(0, allowance - (now - start) // 1_000_000)
        if remaining == 0 or any(os.path.lexists(active / name) for name in ('finished.json', 'exited.json')):
            return None
        return dict(remaining_ms=remaining, sampled_monotonic_ns=now,
                    task_id=binding['task_id'], turn=binding['turn'], segment=len(paths))
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, AttributeError):
        return None


def decorate(reply, capability, kind):
    """Add feedback only to a transient terminal compiler reply, after capture."""
    if kind not in {'build', 'incremental', 'scratch', 'file', 'local_closure'}:
        return reply
    if reply.get('state') in {'queued', 'running'} or not isinstance(reply.get('result'), dict):
        return reply
    progress = sample(capability.get('provider_progress'))
    if progress is None:
        return reply
    result = dict(reply['result'])
    stdout, stderr = result.get('stdout', ''), result.get('stderr', '')
    if not isinstance(stdout, str) or not isinstance(stderr, str):
        return reply
    if not stdout and not stderr and reply['state'] in {'succeeded', 'failed'} and reply.get('current_inputs_match') is not False:
        stderr = f"check-job: {reply['state']}\n"
    if stderr and not stderr.endswith('\n'):
        stderr += '\n'
    stderr += (f"Host timing advisory: about {progress['remaining_ms'] // 1000} seconds of authorized execution "
               "allowance remained when this reply was prepared; compiler waits count toward it. "
               "Use the remaining allowance for concrete closure work while the theorem remains open, "
               "reserving time for submission.\n")
    result['stderr'] = stderr
    return dict(reply, result=result, provider_progress=progress)
