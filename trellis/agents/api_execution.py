"""Run API submission turns under Trellis's existing owned provider clock."""
from dataclasses import asdict
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import uuid

from trellis.adapters import BurstResult, ProviderConfig
from trellis.agent_wrapper import provider_clock
from trellis.agent_wrapper.continuation import require_session, session_binding
from trellis.chat_history import ensure_chat_file_link
from trellis.config import SandboxConfig
from trellis.agents.host_files import safe_directory, atomic_text


def run_monitored(config, prompt, **kwargs):
    start = time.monotonic()
    session_record = kwargs.get('session_record')
    home = kwargs.get('burst_home')
    timeout = kwargs.get('burst_timeout', 7200)
    thread = kwargs.get('exact_resume_thread_id', '')
    if not session_record or not home or timeout <= 0:
        raise ValueError('worker submission requires retained context and a positive remaining allowance')
    binding = session_binding(config, home, thread or 'pending')
    if thread:
        require_session(session_record, binding)
    binding['thread_id'] = thread
    work_dir = kwargs['work_dir'].resolve()
    prefix = kwargs.get('artifact_prefix') or kwargs.get('role', 'worker')
    log_dir = kwargs.get('log_dir') or work_dir / '.trellis/logs/bursts'
    output = ensure_chat_file_link(work_dir, log_dir=log_dir, artifact_prefix=prefix,
        role=kwargs.get('role', 'worker'), log_filename=prefix + '-output.log', canonical_name='output.log')
    root = kwargs.get('execution_record') or Path(session_record).parent / (prefix + '-execution')
    clock = provider_clock.prepare(root, allowance_ms=int(timeout * 1000), binding=binding,
        output=output, startup_seconds=kwargs.get('startup_timeout', 3600))
    segment = Path(clock.get('segment_directory', root))
    offset = output.stat().st_size if clock.get('append_output') and output.exists() else 0
    request_name = 'api-request-' + uuid.uuid4().hex + '.json'
    result_path = segment / (request_name + '.result')
    wire = {key: str(value) if isinstance(value, Path) else asdict(value) if isinstance(value, SandboxConfig) else value
            for key, value in kwargs.items()}
    # The outer clock, not an in-process wall-clock check, owns completion.
    wire['burst_timeout'] = clock['allowance_ms'] / 1000
    wire['_events_only'] = True
    with safe_directory(segment) as (directory, fd):
        request_path = atomic_text(directory, fd, request_name, json.dumps(dict(
            config=asdict(config), prompt=prompt, kwargs=wire, marker=clock['marker'], result=str(result_path))))
    env = dict(os.environ)
    source = str(Path(__file__).resolve().parents[2])
    env['PYTHONPATH'] = source + (os.pathsep + env['PYTHONPATH'] if env.get('PYTHONPATH') else '')
    with (segment / 'monitor.stderr').open('w') as errors:
        proc = subprocess.Popen([sys.executable, '-m', 'trellis.agent_wrapper.provider_clock', str(segment), '--',
            sys.executable, '-m', 'trellis.agents.api_backend', str(request_path)],
            env=env, stdout=subprocess.DEVNULL, stderr=errors, start_new_session=True)
        try:
            code = proc.wait()
        except BaseException:
            # SIGINT lets the monitor's finally block reap separate tool sessions.
            os.killpg(proc.pid, signal.SIGINT)
            proc.wait(timeout=30)
            raise
    timing = provider_clock.read(root, 'finished')
    session = provider_clock.read(root, 'session') or {}
    output_text = output.read_text(errors='replace') if output.exists() else ''
    if result_path.exists():
        raw = json.loads(result_path.read_text())
        raw['transcript_path'] = Path(raw['transcript_path'])
        result = BurstResult(**raw)
    else:
        from trellis.agents.api_backend import _add_usage
        usage = dict(provider=config.provider, model=config.model, requests=0,
            input_tokens=0, output_tokens=0, cached_input_tokens=0, reasoning_tokens=0, total_tokens=0)
        for line in output_text.encode()[offset:].decode(errors='replace').splitlines():
            try:
                event = json.loads(line)
                if event.get('type') == 'usage':
                    _add_usage(usage, event['usage'])
                    usage['requests'] += 1
            except (ValueError, TypeError, KeyError):
                continue
        result = BurstResult(False, code, output_text, time.monotonic() - start, usage=usage)
    result.provider_execution = timing
    result.provider_session_id = session.get('thread_id', '')
    result.provider_turn_completed = bool(timing and timing['disposition'] == 'completed')
    result.ok = code == 0 and result.provider_turn_completed
    result.exit_code = code
    result.captured_output = output_text
    result.transcript_path = output
    result.duration_seconds = time.monotonic() - start
    if not result.ok and not result.error:
        result.error = ('Worker execution allowance exhausted; retain available files for host checking'
                        if timing and timing['disposition'] == 'expired' else
                        'Worker turn did not complete; retain its conversation and files for recovery')
    from trellis.agents.tmux_backend import append_cost_ledger
    append_cost_ledger(work_dir, provider=config.provider, role=kwargs.get('role', 'worker'),
        scope=kwargs.get('session_scope', ''), model=config.model, usage=result.usage, cost_usd=None,
        duration_seconds=result.duration_seconds, attempts=1, ok=result.ok, reason=result.error or 'completed',
        session_id=result.provider_session_id, message_count=(result.usage or {}).get('requests'), extra={'backend': 'api'})
    return result


def child_main():
    request = json.loads(Path(sys.argv[1]).read_text())
    kwargs = request['kwargs']
    for name in ('work_dir', 'log_dir', 'done_file', 'burst_home', 'session_record', 'execution_record'):
        if kwargs.get(name):
            kwargs[name] = Path(kwargs[name])
    if kwargs.get('sandbox') is not None:
        kwargs['sandbox'] = SandboxConfig(**kwargs['sandbox'])
    print(request['marker'], flush=True)
    from trellis.agents.api_backend import _run
    result = _run(ProviderConfig(**request['config']), request['prompt'], **kwargs)
    raw = asdict(result)
    raw['transcript_path'] = str(raw['transcript_path'])
    path = Path(request['result'])
    with safe_directory(path.parent) as (directory, fd):
        atomic_text(directory, fd, path.name, json.dumps(raw))
    return 0 if result.ok else 1
