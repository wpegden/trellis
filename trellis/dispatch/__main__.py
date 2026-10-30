"""Production launch and stopped-checkpoint migration for parallel dispatch."""
from __future__ import annotations

import argparse
import asyncio
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from trellis.atomic_actions.checker_client import client_ping
from trellis.config import load_config
from .candidate import canonical_json
from .confinement import NATIVE_BWRAP_ENV, MANAGED_AGENT_CHECKS_ENV
from .coordination import Coordinator
from .driver import ProductionDriver
from .publication import atomic
from .storage import execution_directories


def verify_checker(runtime, slots, *, ping=client_ping):
    status = ping(Path(runtime) / 'sockets/checker.sock', timeout_secs=5)
    if not status.get('pong') or not status.get('managed_jobs') or status.get('compiler_slots') != slots:
        raise RuntimeError(f'parallel dispatch requires the central checker with managed jobs and {slots} compiler slots; '
                           'restart the checker with TRELLIS_CHECK_JOBS=1 and '
                           f'TRELLIS_CHECK_JOBS_PARALLELISM={slots} at a drained checkpoint')
    return status


def ensure_checker(runtime, slots):
    """Adopt the run's singleton service; never replace a live checker."""
    socket = runtime / 'sockets/checker.sock'
    if len(os.fsencode(socket)) >= 108:
        raise RuntimeError(f'checker socket path exceeds the Linux limit; choose a shorter runtime path: {socket}')
    if socket.exists():
        return verify_checker(runtime, slots)
    log = runtime / 'checker-state/parallel-launch.log'
    log.parent.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, TRELLIS_CHECK_JOBS='1', TRELLIS_CHECK_JOBS_PARALLELISM=str(slots))
    with log.open('ab') as stream:
        process = subprocess.Popen([sys.executable, '-m', 'trellis.checker.server', str(runtime)],
            env=environment, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream, start_new_session=True)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if socket.exists():
            return verify_checker(runtime, slots)
        if process.poll() is not None:
            break
        time.sleep(.1)
    raise RuntimeError(f'central checker did not start; inspect {log}')


@contextmanager
def sidecar_fence(runtime):
    """Refuse to start while a closure-sidecar daemon from a pre-retirement
    install still holds its pid lock under this runtime root."""
    path = runtime / 'sidecar/daemon.pid'
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError('legacy sidecar is still running; finish its drain before parallel dispatch') from None
        yield


async def serve(driver):
    coordinator = Coordinator(driver)
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, lambda: asyncio.create_task(coordinator.shutdown()))
    try:
        await coordinator.run()
    except Exception as exc:
        # Preserve a visible operational stop even when the failure was in a
        # native transition or preparation, rather than an agent subprocess.
        driver._pause('parallel_infrastructure', str(exc))
        atomic(driver.root / 'operational-stop.json', canonical_json(dict(reason=str(exc))))
        raise
    finally:
        await coordinator.shutdown()
        for sig in (signal.SIGINT, signal.SIGTERM):
            loop.remove_signal_handler(sig)


def provider_resume_permits(root):
    """Validate live pauses before issuing any permits; retain historical evidence."""
    from .process import alive
    permits = []
    for directory in root.iterdir():
        if not directory.name.startswith(('review-', 'verifier-', 'exclusive-')) and not directory.name.isdigit():
            continue
        for execution in execution_directories(directory):
            paused = execution / 'provider-pause.json'
            if not paused.exists():
                continue
            value = json.loads(paused.read_text())
            cleanup = execution / 'cleanup.json'
            if (cleanup.exists() and json.loads(cleanup.read_text()) == value['process']
                    and not alive(value['process'])):
                continue
            if value.get('resume_allowed') is False:
                raise RuntimeError('provider continuation unavailable; ordinary resume cannot restart '
                    'this attempt. Retain its files and receipts for explicit recovery: '
                    f'{value["pause"]["reason"]}')
            permits.append((execution, value))
    return permits


def configure_pipeline(driver, settings, token):
    from trellis.runtime.bridge import _unregister_burst_token
    try:
        raw = json.loads(driver.config_path.read_text()).get('parallel_dispatch') or {}
        if 'max_in_flight_tasks' not in raw:
            raise RuntimeError('configure-pipeline requires explicit max_in_flight_tasks (or null to disable) in the launch configuration')
        with driver.native.own_coordinator(), sidecar_fence(driver.runtime), driver.native.locked():
            driver.native._require_recovered_publication()
            current = driver.native._call(dict(action='dispatch_control', root=str(driver.runtime), event=None))
            driver.native._call(dict(action='dispatch_control', root=str(driver.runtime), event=dict(
                kind='execution_pipeline_changed',
                expected_max_in_flight_tasks=current['config'].get('max_in_flight_tasks'),
                max_in_flight_tasks=settings.get('max_in_flight_tasks'))))
        print('Execution pipeline policy recorded at the drained checkpoint.')
        return 0
    finally:
        _unregister_burst_token(driver.runtime, token=token)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['enable', 'configure-submissions', 'configure-pipeline', 'run', 'resume', 'status'])
    parser.add_argument('runtime', type=Path)
    args = parser.parse_args(argv)
    driver = ProductionDriver(args.runtime)
    if args.action == 'status':
        print(json.dumps(driver.native.snapshot(), indent=2))
        return 0
    driver.require_no_halt()
    config = load_config(driver.config_path)
    settings = config.parallel_dispatch
    if not settings:
        raise RuntimeError('set parallel_dispatch.enabled in the launch configuration before enabling or running')
    from trellis.sandbox import repo_targets_isabelle
    if repo_targets_isabelle(config.repo_path):
        raise RuntimeError('parallel dispatch currently requires Lean; this backend has no verified compiler drain')
    if not config.sandbox.enabled:
        raise RuntimeError('parallel dispatch requires the sandbox')
    os.environ[MANAGED_AGENT_CHECKS_ENV] = '1'
    os.environ.pop(NATIVE_BWRAP_ENV, None)
    os.environ.pop('TRELLIS_COMPILER_DATA_VIEW', None)
    os.environ.setdefault('TRELLIS_RUNTIME_CHECKPOINT_HOOK', str(Path(__file__).resolve().parents[2] / 'scripts/trellis_git_checkpoint_hook.sh'))
    # HumanGate Step uses the ordinary bridge to consume the viewer signal.
    # Direct parallel launches need the same default as scripts/trellis.sh.
    if not os.environ.get('TRELLIS_RUNTIME_BRIDGE_CMD', '').strip():
        os.environ['TRELLIS_RUNTIME_BRIDGE_CMD'] = str(Path(__file__).resolve().parents[2] / 'scripts/trellis_bridge.sh')
    os.environ['TRELLIS_KERNEL_CACHE_ROOT'] = str(driver.runtime)
    os.environ['TRELLIS_CHECK_JOBS'] = '1'
    from trellis.runtime.bridge import _mint_burst_token, _register_burst_token
    token = _mint_burst_token()
    _register_burst_token(driver.runtime, token=token, burst_id='parallel-supervisor',
                          kind='supervisor', request_id=0, cycle=driver.state()['cycle'])
    os.environ['TRELLIS_CHECKER_TOKEN'] = token
    os.environ['TRELLIS_CHECKER_SOCKET'] = str(driver.runtime / 'sockets/checker.sock')
    if args.action == 'configure-submissions':
        # Native migration owns both locks; the CLI is safe without this wrapper.
        if 'worker_submission_policy' not in (json.loads(driver.config_path.read_text()).get('parallel_dispatch') or {}):
            raise RuntimeError('configure-submissions requires an explicit worker_submission_policy in the launch configuration')
        driver.native._call(dict(action='dispatch_configure_worker_submissions', root=str(driver.runtime),
                                 policy=settings.get('worker_submission_policy')))
        print('Worker submission policy recorded for future tasks at the drained checkpoint.')
        return 0
    if args.action == 'configure-pipeline':
        return configure_pipeline(driver, settings, token)
    with driver.native.own_coordinator():
        if args.action == 'enable':
            if driver.state().get('dispatch') is not None:
                raise RuntimeError('parallel dispatch is already enabled; use run to adopt it')
            # Check every host prerequisite before changing the protocol state.
            ensure_checker(driver.runtime, settings['compiler_slots'])
            with driver.native.locked():
                driver.native._call(dict(action='dispatch_enable', root=str(driver.runtime), config=settings))
            print('Parallel dispatch enabled at the stopped checkpoint. Start with scripts/trellis.sh run.')
            return 0
        from .config import resume_settings
        durable = (driver.state().get('dispatch') or {}).get('config') or {}
        settings = resume_settings(settings, durable,
            json.loads(driver.config_path.read_text()).get('parallel_dispatch') or {})
        if durable != settings:
            raise RuntimeError('launch profiles/capacity differ from the durable dispatch configuration; restore the enabled configuration')
        with sidecar_fence(driver.runtime):
            ensure_checker(driver.runtime, settings['compiler_slots'])
            # Feed the existing native observation pool from verified capacity.
            # This is client concurrency; central checker admission still owns Lean.
            if not os.environ.get('TRELLIS_CHECK_WORK_PARALLELISM', '').strip():
                os.environ['TRELLIS_CHECK_WORK_PARALLELISM'] = str(settings['compiler_slots'])
            if args.action == 'resume':
                driver.require_no_halt()
                # Explicit operator control, never a human mathematical gate.
                pause_path = driver.runtime / 'pause_request.json'
                pause = json.loads(pause_path.read_text()) if pause_path.exists() else {}
                if pause.get('kind') == 'parallel_infrastructure' and pause.get('detail', {}).get('process'):
                    execution = Path(pause.get('detail', {}).get('process', ''))
                    # Host acceptance is repeatable on a new captured base;
                    # agent execution is never silently repeated under one ID.
                    parts = execution.relative_to(driver.root).parts if execution.is_relative_to(driver.root) else ()
                    if len(parts) != 4 or not parts[0].isdigit() or parts[1] != 'acceptance' or parts[3] != 'execution':
                        raise RuntimeError('repair/cancel the failed agent attempt through native dispatch before resuming; it cannot be silently relaunched')
                    result = json.loads((execution / 'result.json').read_text())
                    if result['returncode'] == 0:
                        raise RuntimeError('acceptance failure has no failed process receipt; preserve the evidence')
                    from .process import AttemptProcess
                    AttemptProcess(execution).stop_and_reap(json.loads((execution / 'process.json').read_text()))
                    atomic(execution.parent / 'retry-authorized.json', canonical_json(dict(
                        failed_result=result, resumed_at=time.time())))
                # Validate every pause before issuing any permits.
                for execution, value in provider_resume_permits(driver.root):
                    atomic(execution / 'provider-resume.json', canonical_json(dict(
                        ordinal=value['ordinal'], process=value['process'])))
                if pause_path.exists():
                    atomic(driver.root / 'resumes' / f'{time.time_ns()}.json', pause_path.read_bytes())
                    pause_path.unlink()
                stopped = driver.root / 'operational-stop.json'
                if stopped.exists():
                    atomic(driver.root / 'resumes' / f'{time.time_ns()}-stop.json', stopped.read_bytes())
                    stopped.unlink()
                transport = driver.root / 'transport.json'
                if transport.exists():
                    value = json.loads(transport.read_text())
                    value.pop('pause', None)
                    value['streaks'] = {}
                    atomic(transport, canonical_json(value))
                (driver.repo / '.trellis-stop-after-checkpoint').unlink(missing_ok=True)
            driver.environment.update(os.environ)
            atomic(driver.root / 'coordinator.json', canonical_json(dict(pid=os.getpid(), started_at=time.time())))
            asyncio.run(serve(driver))
    if driver.state().get('pv_revision_handoff'):
        # Direct run/resume callers need the same idempotent continuation as
        # trellis.sh. Release coordinator ownership before entering revision.
        from scripts.pv_native_create import continue_native
        continue_native(driver.runtime)
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (RuntimeError, ValueError, OSError) as exc:
        print(f'trellis-dispatch: {exc}', file=sys.stderr)
        raise SystemExit(1)
