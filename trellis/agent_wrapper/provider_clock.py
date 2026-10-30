"""Owned provider execution clock, outside the writable agent workspace.

The sandbox launcher sends a start marker after bootstrap, immediately before
invoking Codex. Its stdout then carries the provider's JSON event stream. This
monitor owns the pipe and arbitrates completion versus expiry independently of
the coordinator, native lock, and wrapper cleanup. Agent-visible logs are only
a copy; they are never read back as timing authority.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import selectors
import signal
import subprocess
import sys
import time
import queue
import stat
import threading
from contextlib import contextmanager
from concurrent.futures import ThreadPoolExecutor

# Engineering invokes this trusted host file under -I -S, outside its namespace.
if __package__ is None:
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from trellis.dispatch.candidate import canonical_json, digest
from trellis.dispatch.publication import atomic


def engineering_host_files():
    root = Path(__file__).resolve().parents[1]
    return [root / path for path in ('agent_wrapper/provider_clock.py', '__init__.py',
        'dispatch/__init__.py', 'dispatch/candidate.py', 'dispatch/publication.py',
        'dispatch/process.py', 'incremental_check.py', 'provider_budget.py', 'pv_execution.py',
        'agents/__init__.py', 'agents/host_files.py')]


@contextmanager
def _engineering_log(output, append):
    from trellis.agents.host_files import safe_directory
    with safe_directory(output.parent) as (_, directory_fd):
        flags = os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        fd = os.open(output.name, flags | (os.O_APPEND if append else 0), 0o600, dir_fd=directory_fd)
        with os.fdopen(fd, 'ab' if append else 'wb', buffering=0) as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise ValueError('Engineering readable log requires an unaliased regular file')
            if not append:
                os.ftruncate(stream.fileno(), 0)
            yield stream


def _record(directory, name):
    path = directory / (name + '.json')
    if not path.exists():
        return None
    config = (directory / 'config.json').read_bytes()
    record = json.loads(path.read_bytes())
    if record.get('config_sha256') != digest(config):
        raise ValueError('provider clock receipt differs from its authorized turn')
    return record


def _segments(directory):
    """The first clock is unchanged; resumed transport clocks are append-only."""
    original = (directory / 'config.json').read_bytes()
    config = json.loads(original)
    result, spent, thread = [directory], 0, config.get('binding', {}).get('thread_id', '')
    children = list((directory / 'segments').glob('*'))
    if any(not path.name.isdigit() for path in children):
        raise ValueError('provider transport segments are not an ordered owned history')
    for path in sorted(children, key=lambda p: int(p.name)):
        if path.is_symlink() or not path.name.isdigit() or int(path.name) != len(result) + 1:
            raise ValueError('provider transport segments are not an ordered owned history')
        previous = _record(result[-1], 'finished')
        if previous is None or previous['disposition'] != 'interrupted' or not previous.get('provider_budget'):
            raise ValueError('provider transport resume has no owned budget interruption')
        spent += previous['elapsed_ms']
        thread = previous.get('thread_id') or thread
        current = json.loads((path / 'config.json').read_bytes())
        expected_binding = dict(config['binding'], thread_id=thread)
        if (current.get('root_config_sha256') != digest(original)
                or current.get('previous_finished_sha256') != digest(canonical_json(previous))
                or current['allowance_ms'] != config['allowance_ms'] - spent
                or current['allowance_ms'] <= 0 or current['binding'] != expected_binding):
            raise ValueError('provider transport segment changed its binding or remaining allowance')
        result.append(path)
    return result


def resume_context(directory, *, ordinal, process, runtime, maximum):
    """Check a stopped transport segment before advertising an explicit resume.

    Callers still require the existing exact process/ordinal resume permit.
    Provider errors in writable logs never authorize a transport segment.
    """
    directory = Path(directory)
    if type(ordinal) is not int or ordinal < 1 or ordinal > maximum:
        raise RuntimeError('Provider transport retry limit reached; retain this attempt for recovery')
    if json.loads((directory.parent / 'process.json').read_bytes()) != process:
        raise RuntimeError('Provider continuation belongs to a different owned launcher')
    paths = _segments(directory)
    if len(paths) != ordinal:
        raise RuntimeError('Provider resume ordinal differs from its retained transport history')
    finished, exited = _record(paths[-1], 'finished'), _record(paths[-1], 'exited')
    from trellis.dispatch.process import alive
    if (not finished or not exited or alive(finished['owner'])
            or finished['disposition'] != 'interrupted' or not finished.get('provider_budget')
            or finished.get('detail') != 'Provider exited without a completed turn'):
        raise RuntimeError('Provider continuation is not established; retain its files and clock receipts')
    spent = sum(_record(path, 'finished')['elapsed_ms'] for path in paths)
    config = json.loads((directory / 'config.json').read_bytes())
    if spent >= config['allowance_ms']:
        raise RuntimeError('Worker execution allowance is exhausted; no provider restart is permitted')
    thread = next((receipt.get('thread_id') for path in reversed(paths)
                   if (receipt := _record(path, 'session')) and receipt.get('thread_id')),
                  config['binding'].get('thread_id', ''))
    session = dict(config['binding'], thread_id=thread) if thread else None
    if session is not None:
        from trellis.agent_wrapper.continuation import require_session
        require_session(Path(runtime) / 'worker-session.json', session)
    return dict(ordinal=ordinal, session=session)


def _resume_config(directory, *, allowance_ms, binding, output, startup_seconds, shutdown_seconds):
    execution = directory.parent
    recipe = json.loads((execution / 'recipe.json').read_bytes())
    turn = recipe.get('payload', {}).get('worker_turn') or {}
    if Path(turn.get('execution_record', '')).resolve() != directory.resolve():
        raise RuntimeError('Provider resume does not belong to this owned execution')
    paused = json.loads((execution / 'provider-pause.json').read_bytes())
    process = json.loads((execution / 'process.json').read_bytes())
    permit = json.loads((execution / 'provider-resume.json').read_bytes())
    if (paused['process'] != process or permit != dict(ordinal=paused['ordinal'], process=process)
            or paused.get('resume_allowed') is not True):
        raise RuntimeError('Provider resume requires the exact explicit process and pause permit')
    policy = recipe['payload']['request']['worker_submission_policy']
    context = resume_context(directory, ordinal=paused['ordinal'], process=process,
        runtime=turn.get('session_runtime', recipe['payload']['runtime_root']), maximum=policy['max_infrastructure_retries'])
    original = (directory / 'config.json').read_bytes()
    initial = json.loads(original)
    expected = dict(initial['binding'], thread_id=(context['session'] or {}).get('thread_id', ''))
    if (allowance_ms != initial['allowance_ms'] or binding != expected
            or str(output) != initial['output']):
        raise RuntimeError('Provider resume changed its original task allowance or provider binding')
    paths = _segments(directory)
    spent = sum(_record(path, 'finished')['elapsed_ms'] for path in paths)
    destination = directory / 'segments' / str(context['ordinal'] + 1)
    if destination.exists():
        raise RuntimeError('Provider transport segment already prepared; adopt it instead of relaunching')
    config = dict(version=1, allowance_ms=initial['allowance_ms'] - spent,
        binding=expected, output=str(output), startup_seconds=startup_seconds,
        shutdown_seconds=shutdown_seconds, marker='trellis-provider-start:' + secrets.token_hex(24),
        append_output=True,
        root_config_sha256=digest(original), previous_finished_sha256=digest(canonical_json(_record(paths[-1], 'finished'))),
        resume_permit=permit)
    destination.mkdir(parents=True)
    atomic(destination / 'config.json', canonical_json(config))
    return dict(config, segment_directory=str(destination))


def prepare(directory: Path, *, allowance_ms: int, binding: dict, output: Path,
            startup_seconds: float = 3600, shutdown_seconds: float = 120) -> dict:
    if type(allowance_ms) is not int or allowance_ms <= 0:
        raise ValueError('provider clock requires a positive remaining allowance')
    directory.mkdir(parents=True, exist_ok=True)
    config = dict(version=1, allowance_ms=allowance_ms, binding=binding,
                  output=str(output), startup_seconds=startup_seconds,
                  shutdown_seconds=shutdown_seconds, marker='trellis-provider-start:' + secrets.token_hex(24))
    path = directory / 'config.json'
    if path.exists():
        # Re-entering the wrapper cannot grant another turn or reset its clock.
        if not (directory.parent / 'provider-resume.json').exists():
            raise RuntimeError('provider turn already prepared; adopt its owned launcher instead of relaunching')
        return _resume_config(directory, allowance_ms=allowance_ms, binding=binding, output=output,
            startup_seconds=startup_seconds, shutdown_seconds=shutdown_seconds)
    atomic(path, canonical_json(config))
    return config


def read(directory: Path, name: str) -> dict | None:
    directory = Path(directory)
    if not (directory / 'config.json').exists():
        return None
    # Segment-local reads are for the currently running Codex wrapper. The
    # coordinator reads the root and sees one summed execution allowance.
    config = json.loads((directory / 'config.json').read_bytes())
    paths = [directory] if 'root_config_sha256' in config else _segments(directory)
    if name == 'started':
        return _record(paths[0], name)
    if name == 'session':
        return next((record for path in reversed(paths) if (record := _record(path, name))), None)
    record = _record(paths[-1], name)
    if record is None or len(paths) == 1:
        return record
    record = dict(record, config_sha256=digest((directory / 'config.json').read_bytes()), transport_segments=len(paths))
    if name == 'finished':
        record['elapsed_ms'] += sum(_record(path, name)['elapsed_ms'] for path in paths[:-1])
        record['started_at_ms'] = _record(paths[0], 'started')['started_at_ms']
        record['thread_id'] = record.get('thread_id') or json.loads((paths[-1] / 'config.json').read_bytes())['binding'].get('thread_id', '')
    return record


def monitor(directory: Path, command: list[str], *, engineering=False) -> int:
    import fcntl
    lock = (directory / 'monitor.lock').open('a')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if (directory / 'owner.json').exists():
        raise RuntimeError('provider clock already launched; its execution cannot be repeated')
    config_bytes = (directory / 'config.json').read_bytes()
    config = json.loads(config_bytes)
    from trellis.dispatch.process import identity
    common = dict(config_sha256=digest(config_bytes), owner=identity(os.getpid()))
    import ctypes
    from trellis.dispatch.process import reap_descendants
    if ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) != 0:
        raise RuntimeError('cannot establish provider descendant ownership')
    atomic(directory / 'owner.json', canonical_json(common))
    started = terminal = None
    thread = ''
    provider_budget = ''
    buffer = b''
    dropping = False
    startup_deadline = time.monotonic_ns() + int(config['startup_seconds'] * 1e9)
    shutdown_deadline = None
    terminated = False
    persistence = ThreadPoolExecutor(max_workers=1, thread_name_prefix='provider-clock-receipts')
    writes = []
    def save(name, value):
        # Persistence must never delay observation of provider timing events.
        writes.append(persistence.submit(atomic, directory / (name + '.json'), canonical_json(value)))

    def finish(disposition, now, detail=''):
        nonlocal terminal, shutdown_deadline
        if terminal is not None:
            return
        elapsed = max(0, (now - started['monotonic_ns']) // 1_000_000) if started else 0
        if disposition == 'interrupted' and detail == 'Provider exited without a completed turn' and elapsed >= config['allowance_ms']:
            disposition = 'expired'
        if disposition != 'completed':
            # Freeze/kill the owned tree, including Codex's separate session,
            # before publishing exhaustion. There is no post-deadline edit grace.
            reap_descendants()
        terminal = dict(**common, disposition=disposition, elapsed_ms=elapsed,
                        thread_id=thread, detail=detail, monotonic_ns=now,
                        started_at_ms=started['started_at_ms'] if started else None,
                        provider_budget=provider_budget if disposition == 'interrupted' else '')
        # Commit the execution winner before any cleanup or native operation.
        save('finished', terminal)
        shutdown_deadline = time.monotonic_ns() + int(config['shutdown_seconds'] * 1e9)

    def line(data, now):
        nonlocal started, thread, provider_budget
        if data == config['marker'].encode() and started is None and terminal is None:
            started = dict(**common, monotonic_ns=now, started_at_ms=time.time_ns() // 1_000_000,
                           allowance_ms=config['allowance_ms'])
            save('started', started)
            return
        if started is None or terminal is not None:
            return
        try:
            event = json.loads(data)
        except (ValueError, UnicodeDecodeError):
            if not engineering:
                from trellis.provider_budget import budget_exhaustion_detail
                provider_budget = budget_exhaustion_detail(output=data.decode(errors='replace')) or provider_budget
            return
        if not isinstance(event, dict):
            return
        if not engineering and event.get('type') in ('error', 'turn.failed'):
            from trellis.provider_budget import budget_exhaustion_detail
            provider_budget = budget_exhaustion_detail(output=json.dumps(event)) or provider_budget
        if event.get('type') == 'thread.started':
            actual = event.get('thread_id')
            expected = config['binding'].get('thread_id')
            if not isinstance(actual, str) or not actual or (expected and expected != actual) or (thread and thread != actual):
                finish('interrupted', now, 'Provider conversation differs from the authorized continuation')
            else:
                thread = actual
                save('session', dict(**common, thread_id=thread))
        elif event.get('type') == 'turn.completed':
            if not thread:
                finish('interrupted', now, 'Provider completed without a conversation identity')
            elif (now - started['monotonic_ns']) // 1_000_000 <= config['allowance_ms']:
                finish('completed', now)
            else:
                finish('expired', now)

    from contextlib import nullcontext
    from trellis.pv_execution import owned_process
    process_context = (owned_process(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                       if engineering else nullcontext(subprocess.Popen(command, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, start_new_session=True)))
    try:
        with process_context as proc:
            selector = None
            try:
                selector = selectors.DefaultSelector()
                selector.register(proc.stdout, selectors.EVENT_READ)
                output = Path(config['output'])
                if not engineering:
                    output.parent.mkdir(parents=True, exist_ok=True)
                chunks = queue.Queue(maxsize=128)
                dropped = 0
                log_errors = []
                def copy_log():
                    try:
                        log_context = (_engineering_log(output, config.get('append_output')) if engineering
                                       else output.open('ab' if config.get('append_output') else 'wb', buffering=0))
                        with log_context as log:
                            while (data := chunks.get()) is not None:
                                log.write(data)
                    except BaseException as exc:
                        log_errors.append('log_write_failed' if engineering else str(exc))
                copier = threading.Thread(target=copy_log, name='provider-readable-log', daemon=True)
                copier.start()
                while selector.get_map():
                    now = time.monotonic_ns()
                    deadline = shutdown_deadline if terminal else (
                        started['monotonic_ns'] + (config['allowance_ms'] + 1) * 1_000_000
                        if started else startup_deadline)
                    # Drain already-available provider events before declaring expiry.
                    ready = selector.select(max(0, min(.1, (deadline - now) / 1e9)))
                    drained = 0
                    while ready and drained < 128:
                        key, _ = ready[0]
                        data = os.read(key.fd, 65536)
                        observed = time.monotonic_ns()
                        drained += 1
                        if not data:
                            selector.unregister(key.fileobj)
                            ready = selector.select(0) if selector.get_map() else []
                            continue
                        try:
                            chunks.put_nowait(data)
                        except queue.Full:
                            dropped += len(data)
                        for part in data.splitlines(keepends=True):
                            if not dropping:
                                buffer += part
                            if part.endswith(b'\n'):
                                if not dropping:
                                    line(buffer.rstrip(b'\r\n'), observed)
                                buffer, dropping = b'', False
                            elif len(buffer) > 2 * 1024 * 1024:
                                # Large tool output is irrelevant to top-level completion.
                                buffer, dropping = b'', True
                        ready = selector.select(0)
                    now = time.monotonic_ns()
                    if terminal is None and now >= deadline:
                        finish('expired' if started else 'interrupted', now,
                               '' if started else 'Provider did not start before the startup limit')
                    if terminal and terminal['disposition'] != 'completed' and not terminated:
                        terminated = True
                        try:
                            os.killpg(proc.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                    if shutdown_deadline is not None and now >= shutdown_deadline:
                        break
                if terminal is None:
                    finish('interrupted', time.monotonic_ns(), 'Provider exited without a completed turn')
                try:
                    code = proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    code = proc.wait()
                reap_descendants()
                for write in writes:
                    write.result()
                # Logs are advisory. Slow storage cannot extend or debit execution.
                try:
                    chunks.put(None, timeout=2)
                except queue.Full:
                    log_errors.append('log_queue_undrained' if engineering else
                                      "Readable log storage did not drain; timing receipts are retained")
                copier.join(timeout=2)
                atomic(directory / 'exited.json', canonical_json(dict(**common, returncode=code,
                    dropped_log_bytes=dropped, log_errors=log_errors)))
                return code if code else (0 if terminal['disposition'] == 'completed' else 124)
            finally:
                if engineering:
                    # Drain before receipt/log joins. Repeated cancellation remains
                    # pending until the owned descendant tree has been stopped.
                    mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
                    try:
                        reap_descendants()
                    finally:
                        signal.pthread_sigmask(signal.SIG_SETMASK, mask)
                if selector is not None:
                    selector.close()
                if not engineering:
                    persistence.shutdown(wait=True)
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGKILL)
                    proc.wait()
                reap_descendants()
    finally:
        if engineering:
            persistence.shutdown(wait=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--engineering-owned', action='store_true')
    parser.add_argument('directory', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    return monitor(args.directory, command, engineering=args.engineering_owned)


if __name__ == '__main__':
    sys.exit(main())
