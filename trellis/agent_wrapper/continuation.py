"""Exact provider context binding for host-authorized correction turns."""
import json
from pathlib import Path

from trellis.dispatch.publication import atomic
from trellis.dispatch.candidate import canonical_json, regular_bytes


def session_binding(config, home, thread_id):
    if not home or not thread_id or not config.model or not config.effort:
        raise ValueError('worker correction requires a provider home, thread, model and effort')
    return dict(provider=config.provider, model=config.model, reasoning_effort=config.effort,
                home=str(Path(home).resolve()), thread_id=thread_id)


def record_session(path, binding):
    """Retain first-thread identity before completion; refuse context replacement."""
    path = Path(path)
    previous = regular_bytes(path.parent, path.name)
    if previous is not None:
        if json.loads(previous) != binding:
            raise RuntimeError('worker provider session differs from the retained attempt context')
        return
    atomic(path, canonical_json(binding))


def require_session(path, binding):
    path = Path(path)
    previous = regular_bytes(path.parent, path.name)
    if previous is None or json.loads(previous) != binding:
        raise RuntimeError('worker correction has no matching retained provider context')
    if binding['provider'] == 'codex':
        from trellis.codex_timing import find_rollout_path
        rollout = find_rollout_path(binding['thread_id'],
                                   sessions_root=Path(binding['home']) / '.codex/sessions')
        if rollout is not None and not rollout.is_file():
            from trellis.dispatch.retained_files import present
            if present(rollout):
                raise RuntimeError('worker correction requires raw session JSONL; restore the terminal owner '
                    'with trellis.dispatch.retained_files --runtime ROOT --task ID --restore '
                    '(or --owner review-ID/verifier-ID --restore) before native CLI use')
        if rollout is None or not rollout.is_file():
            raise RuntimeError('worker correction conversation is unavailable; retain its work for recovery')

    elif binding['provider'] in {'deepseek', 'kimi'}:
        from trellis.agents.api_sessions import require_checkpoint
        require_checkpoint(binding)
