"""Host-owned compiler channels, retained across coordinator restarts.

The attempt launcher issues these capabilities and survives its coordinator.
Agents never see this record or the authoritative channel for an NL-only task.
Service acknowledgements, not registry deletion or client exit, prove draining.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import time

from trellis.checker import jobs, workspaces
from trellis import check_job
from trellis.atomic_actions.checker_client import CheckerRpcError, client_release_workspace
from .candidate import canonical_json
from .publication import atomic


def grant(root, spec, *, loaded_recipe=None):
    """Called once by the durable launcher, before starting any host bridge."""
    root = Path(root).resolve()
    runtime = Path(spec['runtime']).resolve()
    task_runtime = Path(spec['task_runtime']).resolve()
    if spec.get('serial'):
        if not root.is_relative_to(runtime / 'worker-attempts'):
            raise ValueError('serial compiler channels must remain in their owned attempt')
        repo = Path(spec['repo']).resolve()
        metadata = json.loads((runtime / 'runtime_metadata.json').read_text())
        if repo != Path(metadata['repo_path']).resolve() or root.is_relative_to(repo):
            raise ValueError('serial compiler channels must match the canonical repository')
        record = root / 'compiler-channels.json'
        if record.exists():
            raise RuntimeError('serial compiler channels already issued; adopt the launcher')
        channels = dict(runtime=str(runtime), environment=jobs.register_workspace(runtime, repo,
            scope=spec['scope'], lifetime=spec['lifetime']))
        try:
            atomic(record, canonical_json(channels))
        except BaseException:
            _revoke(channels)
            raise
        return channels['environment']
    if not root.is_relative_to(runtime / 'dispatch') or not task_runtime.is_relative_to(runtime / 'dispatch'):
        raise ValueError('attempt resources must be inside this run\'s dispatch directory')
    repo = task_runtime.parent / 'repo'
    if (not root.is_relative_to(task_runtime.parent) or root.is_relative_to(repo)
            or root.is_relative_to(task_runtime)):
        raise ValueError('compiler credentials must belong to this task and remain outside its repository and runtime')
    if spec['compiler'] not in ('none', 'managed'):
        raise ValueError('unknown task compiler capability')
    record = root / 'compiler-channels.json'
    if record.exists():
        raise RuntimeError('compiler channels were already issued; adopt this attempt instead of issuing again')
    from .workspace import repair_packages
    repair_packages(runtime, task_runtime)
    recovery = spec.get('capacity_recovery', False)
    if type(recovery) is not bool:
        raise ValueError('invalid host capacity recovery capability')
    authority = workspaces.register(runtime, task_runtime, lifetime=spec['lifetime'],
        **({'capacity_recovery': True} if recovery else {}))
    channels = dict(runtime=str(runtime), environment=authority)
    try:
        if spec['compiler'] == 'managed':
            from trellis.checker.provider_progress import descriptor
            channels['environment'].update(jobs.register_workspace(runtime, repo,
                scope=spec['scope'], lifetime=spec['lifetime'],
                provider_progress=descriptor(root, spec, loaded_recipe=loaded_recipe)))
        atomic(record, canonical_json(channels))
    except BaseException:
        # No bridge can use the channels until their durable record exists.
        _revoke(channels)
        raise
    return channels['environment']


def _revoke(record):
    env = record['environment']
    if 'TRELLIS_CHECKER_TOKEN' in env:
        workspaces.revoke(record['runtime'], env['TRELLIS_CHECKER_TOKEN'])
    if 'TRELLIS_CHECK_JOBS_TOKEN' in env:
        jobs.revoke_workspace(record['runtime'], env['TRELLIS_CHECK_JOBS_TOKEN'])


def revoke(root):
    path = Path(root) / 'compiler-channels.json'
    if path.exists():
        _revoke(json.loads(path.read_text()))


def drain(root, *, timeout=30, authority=client_release_workspace, advisory=check_job.rpc):
    path = Path(root) / 'compiler-channels.json'
    if not path.exists():
        return
    record = json.loads(path.read_text())
    env = record['environment']
    _revoke(record)
    try:
        drain_channels(env, timeout=timeout, authority=authority, advisory=advisory)
    except CleanupNotAcknowledged as exc:
        # Diagnostic only; a failed write must not replace the actual failure.
        try:
            atomic(path.with_name('compiler-cleanup-failure.json'), canonical_json(
                dict(execution=str(path.parent), recorded_at=time.time(), **exc.diagnostic)))
        except OSError:
            pass
        raise CleanupNotAcknowledged(exc.diagnostic, execution=path.parent) from exc.__cause__


class CleanupNotAcknowledged(RuntimeError):
    def __init__(self, diagnostic, *, execution=None):
        self.diagnostic = diagnostic
        pending = ', '.join(f"{row['channel']}:{row['capability_sha256'][:12]} "
                            f"({row['attempts']} attempts, {row['last_result']})" for row in diagnostic['pending'])
        super().__init__('compiler cleanup was not acknowledged; retain the worker slot and recovery evidence'
                         + (f'; execution={execution}' if execution else '') + '; pending=' + pending)


def drain_channels(env, *, timeout=30, authority=client_release_workspace, advisory=check_job.rpc):
    """Wait for exact channel acknowledgements, including after a lost response.

    Release is idempotent. A timed-out request may still be draining on the
    server, so retry its token within one deadline without freeing its slot.
    """
    started = time.monotonic()
    deadline = started + timeout
    pending = [name for name in ('TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_TOKEN') if name in env]
    observations = {name: dict(channel='authority' if name == 'TRELLIS_CHECKER_TOKEN' else 'advisory',
        capability_sha256=hashlib.sha256(env[name].encode()).hexdigest(), attempts=0, last_result='not attempted')
        for name in pending}
    last_error = None
    while pending:
        for name in pending[:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise CleanupNotAcknowledged(dict(timeout_seconds=timeout, elapsed_seconds=time.monotonic()-started,
                    pending=[observations[key] for key in pending])) from last_error
            observation = observations[name]
            observation['attempts'] += 1
            try:
                if name == 'TRELLIS_CHECKER_TOKEN':
                    result = authority(env['TRELLIS_CHECKER_SOCKET'], env[name], deadline=deadline)
                else:
                    result = advisory({'action': 'release'}, socket_path=env['TRELLIS_CHECK_JOBS_SOCKET'],
                                      token=env[name], timeout=remaining)
            except CheckerRpcError as exc:
                if exc.kind != 'supervisor_unavailable':
                    raise
                last_error = exc
                observation['last_result'] = 'transport failure: ' + str(exc).replace(env[name], '[redacted capability]')[:500]
            except check_job.JobTransportError as exc:
                last_error = exc
                observation['last_result'] = 'transport failure: ' + str(exc).replace(env[name], '[redacted capability]')[:500]
            else:
                observation['last_result'] = 'drained' if result.get('drained') is True else 'not drained'
                if result.get('drained') is True:
                    pending.remove(name)
        if pending:
            time.sleep(min(.1, max(0, deadline - time.monotonic())))
