"""Restartable per-attempt launcher with a durable launch-before-exec gate.

Each attempt owns a separate tmux server. A process that survived an operator
restart is adopted by PID/start-time/boot identity, never relaunched by name.
The launcher holds an exclusive file lock across its entire lifetime.
"""
from __future__ import annotations

import fcntl
import ctypes
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import stat
import subprocess
import sys
import time

from trellis.incremental_check import _process_identity
from .candidate import canonical_json
from .publication import atomic


def identity(pid):
    value = _process_identity(pid)
    return dict(pid=value['pid'], start_ticks=int(value['start_ticks']), boot_id=value['boot_id'])


def alive(record):
    try:
        text = (Path('/proc') / str(record['pid']) / 'stat').read_text()
        return text[text.rfind(')') + 2:].split()[0] != 'Z' and identity(record['pid']) == record
    except (OSError, ValueError, KeyError):
        return False


def socket_name(root):
    return 'trellis-task-' + hashlib.sha256(str(Path(root).resolve()).encode()).hexdigest()[:24]


def _load_recipe(root):
    """Load the launch input once; optionally bind its exact bytes for display."""
    path = root / 'recipe.json'
    with path.open('rb') as stream:
        try:
            before = os.fstat(stream.fileno())
        except OSError:
            before = None
        raw = stream.read()
        recipe = json.loads(raw.decode('utf-8'))
        digest = signature = None
        try:
            from trellis.checker.provider_progress import _signature
            after = os.fstat(stream.fileno())
            current = path.stat(follow_symlinks=False)
            task = recipe.get('payload', {}).get('request', {}).get('task_execution', {})
            if (task.get('task_kind') == 'lean_closure' and task.get('compiler') == 'managed'
                    and before is not None
                    and all(stat.S_ISREG(value.st_mode) and value.st_nlink == 1
                            for value in (before, after, current))
                    and _signature(before) == _signature(after) == _signature(current)
                    and len(raw) == before.st_size):
                digest = hashlib.sha256(raw).hexdigest()
                signature = _signature(before)
        except (OSError, ValueError, TypeError, AttributeError):
            pass  # Optional timing metadata cannot prevent a valid launch.
    return recipe, digest, signature


def _engineering_recipe(recipe):
    # The native request is the domain authority, including on cleanup.
    return recipe.get('payload', {}).get('request', {}).get('kind') == 'Engineering'


def _stop_for_reaping(fd, fields, *, timeout=3):
    signal.pidfd_send_signal(fd, signal.SIGSTOP)
    deadline = time.monotonic() + timeout
    while fields()[0] not in ('T', 't', 'Z'):
        if time.monotonic() >= deadline:
            # A filesystem wait can defer SIGSTOP while remaining killable.
            # Kill this checked descendant by pidfd; after its exit any
            # surviving children are adopted by our subreaper and rescanned.
            signal.pidfd_send_signal(fd, signal.SIGKILL)
            if not select.select([fd], [], [], timeout)[0]:
                raise RuntimeError('attempt descendant did not exit; retain its slot')
            return False
        time.sleep(.01)
    return True


def reap_descendants(*, direct_child=None):
    """The launcher is a subreaper: even double-forked host helpers belong here.

    Freeze parents before visiting their children so the tree cannot grow
    behind the traversal. PID descriptors and checked parentage keep signals
    bound to this launcher's descendants. Compiler-service processes live
    outside this tree and require separate capability revocation/drain.
    """
    owned = {}
    def children(parent):
        result = set()
        for thread in (Path('/proc')/str(parent)/'task').glob('*'):
            try:
                result.update(int(p) for p in (thread/'children').read_text().split())
            except FileNotFoundError:
                continue
        return result
    def freeze(pid, parent):
        if pid in owned:
            return
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            def fields():
                raw = (Path('/proc')/str(pid)/'stat').read_text()
                return raw[raw.rfind(')')+2:].split()
            state = fields()
            if int(state[1]) != parent:
                return
            if not _stop_for_reaping(fd, fields):
                return
            owned[pid] = fd
            fd = None
            for child_pid in children(pid):
                freeze(child_pid, pid)
        except (FileNotFoundError, ProcessLookupError):
            pass
        finally:
            if fd is not None:
                os.close(fd)
    try:
        # Children that exited while being frozen may have adopted descendants
        # into this subreaper. Scan until every current child has been visited.
        while True:
            pending = children(os.getpid()) - owned.keys()
            if not pending:
                break
            for pid in pending:
                freeze(pid, os.getpid())
        for fd in owned.values():
            try:
                signal.pidfd_send_signal(fd, signal.SIGKILL)
            except ProcessLookupError:
                pass
        # A retained Popen owns its direct child's exit status. Capture it
        # after containing the tree, before the generic adopted-child drain.
        if direct_child is not None:
            direct_child.wait()
        while True:
            try:
                os.waitpid(-1, 0)
            except ChildProcessError:
                break
    finally:
        for fd in owned.values():
            os.close(fd)


class AttemptProcess:
    def __init__(self, root):
        self.root = Path(root).resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.child = None

    def start_or_adopt(self, command, payload, *, environment=None, checker=None, confinement=None, timeout=10):
        # A recipe contains no API or checker credentials; those are inherited
        # or minted inside the host launcher, never stored in agent-readable data.
        path = self.root / 'recipe.json'
        recipe_value = dict(command=command, payload=payload)
        if checker is not None:
            recipe_value['checker'] = checker
        if confinement is not None:
            from .confinement import confinement_recipe
            previous = json.loads(path.read_text()).get('confinement') if path.exists() else None
            recipe_value['confinement'] = confinement_recipe(confinement, previous,
                os.environ if environment is None else environment)
        recipe = canonical_json(recipe_value)
        if path.exists() and path.read_bytes() != recipe:
            raise RuntimeError('attempt identity already has a different launch recipe')
        if not path.exists():
            atomic(path, recipe)
        launch_path = self.root / 'process.json'
        if launch_path.exists():
            record = json.loads(launch_path.read_text())
            if alive(record):
                return record
            if (self.root / 'permit.json').exists():
                # Even an empty/missing result after a real launch is an outcome
                # for this attempt, not authorization for a second execution.
                return record
        log = (self.root / 'launcher.log').open('ab')
        try:
            self.child = subprocess.Popen([sys.executable, '-m', 'trellis.dispatch.process', str(self.root)],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                env=environment, start_new_session=True, close_fds=True)
        finally:
            log.close()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if launch_path.exists():
                record = json.loads(launch_path.read_text())
                if alive(record):
                    return record
            if self.child.poll() is not None:
                break
            time.sleep(.02)
        raise RuntimeError('attempt launcher did not establish its durable identity; inspect launcher.log')

    def permit(self, record):
        # The caller first commits the kernel Launched event with this identity.
        if not alive(record):
            raise RuntimeError('attempt launcher exited before launch was committed')
        atomic(self.root / 'permit.json', canonical_json(record))

    def result(self):
        path = self.root / 'result.json'
        return json.loads(path.read_text()) if path.exists() else None

    def stop_and_reap(self, record, *, timeout=5):
        from . import resources
        resources.revoke(self.root)
        try:
            engineering = _engineering_recipe(_load_recipe(self.root)[0])
        except (OSError, ValueError, TypeError, AttributeError):
            engineering = None
        # Exact per-attempt server; never the canonical run's shared tmux server.
        if engineering is False:
            subprocess.run(['tmux', '-L', socket_name(self.root), 'kill-server'],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if alive(record):
            fd = os.pidfd_open(record['pid'])
            try:
                if alive(record):
                    signal.pidfd_send_signal(fd, signal.SIGTERM)
                    if not select.select([fd], [], [], timeout)[0]:
                        signal.pidfd_send_signal(fd, signal.SIGKILL)
                        if not select.select([fd], [], [], timeout)[0]:
                            raise RuntimeError('attempt launcher did not terminate; retain its slot')
            finally:
                os.close(fd)
        if self.child is not None:
            self.child.wait(timeout=timeout)
        # A provider could have created its server between the first kill and
        # the launcher's termination. Check again only after new launch is gone.
        if engineering is False:
            subprocess.run(['tmux', '-L', socket_name(self.root), 'kill-server'],
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        cleanup = self.root / 'cleanup.json'
        if not cleanup.exists() or json.loads(cleanup.read_text()) != record:
            raise RuntimeError('attempt descendant cleanup was not acknowledged; retain its slot and recovery evidence')
        # An in-flight launch could have issued its channels between the first
        # revoke and termination. Read the durable record again after reaping.
        resources.drain(self.root)
        if engineering is None:
            raise RuntimeError('Attempt domain is unavailable; retain its execution evidence')


def main():
    root = Path(sys.argv[1]).resolve()
    with (root / 'launcher.lock').open('a+b') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 0
        if (root / 'permit.json').exists():
            return 0
        record = identity(os.getpid())
        if ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
            raise RuntimeError('cannot establish attempt descendant ownership')
        def terminate(_signum, _frame):
            raise SystemExit(143)
        signal.signal(signal.SIGTERM, terminate)
        view_lease = None
        try:
            atomic(root / 'process.json', canonical_json(record))
            while not (root / 'permit.json').exists():
                time.sleep(.05)
            if json.loads((root / 'permit.json').read_text()) != record:
                raise RuntimeError('launch permit belongs to a different process identity')
            loaded_recipe = _load_recipe(root)
            recipe = loaded_recipe[0]
            engineering = _engineering_recipe(recipe)
            env = dict(os.environ)
            if engineering:
                env.pop('TRELLIS_TMUX_SOCKET', None)
            else:
                env['TRELLIS_TMUX_SOCKET'] = socket_name(root)
            if recipe.get('checker') is not None:
                from . import resources
                channels = ('TRELLIS_CHECK_JOBS_SOCKET', 'TRELLIS_CHECK_JOBS_TOKEN')
                if not recipe['checker'].get('serial'):
                    channels += ('TRELLIS_CHECKER_SOCKET', 'TRELLIS_CHECKER_TOKEN')
                for name in channels:
                    env.pop(name, None)
                env.update(resources.grant(root, recipe['checker'], loaded_recipe=loaded_recipe))
                env['TRELLIS_SERIAL_RUNTIME_ROOT' if recipe['checker'].get('serial') else 'TRELLIS_DISPATCH_RUNTIME_ROOT'] = str(Path(recipe['checker']['runtime']).resolve())
                env['TRELLIS_CHECK_JOBS'] = '1'
                env['TRELLIS_AGENT_COMPILER_CAPABILITY'] = recipe['checker']['compiler']
            if recipe.get('confinement') is not None:
                from .confinement import (PersistentDataView, NATIVE_BWRAP_ENV,
                                          MANAGED_AGENT_CHECKS_ENV, require_native_helper)
                confinement = recipe['confinement']
                env.pop('TRELLIS_COMPILER_DATA_VIEW', None)
                env.pop(NATIVE_BWRAP_ENV, None)
                env.pop(MANAGED_AGENT_CHECKS_ENV, None)
                if isinstance(confinement, str):
                    view_lease = PersistentDataView(confinement).use()
                    env['TRELLIS_COMPILER_DATA_VIEW'] = str(view_lease.__enter__())
                elif isinstance(confinement, dict) and confinement.get('kind') == 'native_noexec':
                    env[NATIVE_BWRAP_ENV] = str(require_native_helper(confinement['helper']))
                elif isinstance(confinement, dict) and confinement.get('kind') == 'managed_commands':
                    env[MANAGED_AGENT_CHECKS_ENV] = '1'
                else:
                    raise RuntimeError('unknown attempt compiler confinement; retain its recipe')
            ordinal = 0
            payload = recipe['payload']
            while True:
                # The parent records Launched before this code can invoke any provider.
                # Block TERM across Popen and assignment to close the orphan window.
                signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM})
                try:
                    parent = os.getpid()
                    def child_setup():
                        if ctypes.CDLL(None).prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
                            os._exit(126)
                        signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
                    child = subprocess.Popen(recipe['command'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                             stderr=sys.stderr, env=env, close_fds=True, preexec_fn=child_setup)
                finally:
                    signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
                output, _ = child.communicate(canonical_json(payload))
                if engineering or child.returncode != 3:
                    break
                # Provider allowance pauses are not mathematical failures.
                # Keep this launcher/slot, archive each response and await an
                # explicit host resume permit before invoking the provider again.
                pause = json.loads(output)
                if pause.get('pause', {}).get('kind') != 'provider_budget':
                    break
                reap_descendants()
                ordinal += 1
                paused = dict(ordinal=ordinal, process=record, pause=pause['pause'])
                turn = recipe['payload'].get('worker_turn')
                def continuation():
                    from trellis.agent_wrapper.provider_clock import resume_context
                    return resume_context(Path(turn['execution_record']), ordinal=ordinal, process=record,
                        runtime=turn.get('session_runtime', recipe['payload']['runtime_root']),
                        maximum=recipe['payload']['request']['worker_submission_policy']['max_infrastructure_retries'])
                if turn is not None:
                    try:
                        continuation()
                        paused['resume_allowed'] = True
                    except (OSError, ValueError, RuntimeError) as exc:
                        paused['resume_allowed'] = False
                        paused['pause']['reason'] += '; continuation unavailable: ' + str(exc)
                atomic(root / 'provider-pauses' / f'{ordinal}.json', canonical_json(paused))
                atomic(root / 'provider-pause.json', canonical_json(paused))
                while True:
                    resume = root / 'provider-resume.json'
                    if resume.exists() and json.loads(resume.read_text()) == dict(ordinal=ordinal, process=record):
                        if turn is not None:
                            if not paused['resume_allowed']:
                                time.sleep(.2)
                                continue
                            try:
                                context = continuation()
                            except (OSError, ValueError, RuntimeError) as exc:
                                paused['resume_allowed'] = False
                                paused['pause']['reason'] += '; continuation unavailable: ' + str(exc)
                                atomic(root / 'provider-pause.json', canonical_json(paused))
                                continue
                            payload = dict(recipe['payload'], worker_turn=dict(turn, transport_resume=context))
                        break
                    time.sleep(.2)

        finally:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            reap_descendants()
            if view_lease is not None:
                view_lease.__exit__(None, None, None)
            atomic(root / 'cleanup.json', canonical_json(record))
        # The output is host evidence, outside all agent-writable mounts.
        atomic(root / 'response.json', output)
        atomic(root / 'result.json', canonical_json(dict(returncode=child.returncode,
            response_sha256=hashlib.sha256(output).hexdigest())))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
