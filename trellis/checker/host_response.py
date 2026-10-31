"""Fixed host enrichment entry point; no command or path is accepted on wire."""
import json
import os
import subprocess
import tempfile
import threading
import time

from .admission import admitted, check_cancelled
from .protocol import ProtocolError


@admitted
def enrich(workspace, service):
    from trellis.checker import admission
    from trellis.runtime.kernel_cli import kernel_cli_command
    from trellis.memory_guard import MemoryGuard
    if service is None:
        raise ProtocolError('internal_error', 'host response enrichment requires managed compiler admission')
    root = workspace.runtime_root
    source = root / 'host-response.json'
    if source.is_symlink() or not source.is_file():
        raise ProtocolError('malformed_request', 'no host-prepared response in this task runtime')
    response = json.loads(source.read_text())
    payload = json.dumps(dict(action='dispatch_enrich_response', root=str(root),
                              response=response, allow_compilation=True)).encode()
    # This is a trusted host adapter, not an agent shell. The outer PID
    # namespace owns the complete native/approved-runner process tree; the
    # pinned Rust runner retains its ordinary separate sandbox and trust gate.
    command = ['/usr/bin/bwrap', '--bind', '/', '/', '--unshare-pid', '--die-with-parent',
               '--new-session', '--proc', '/proc', '--', *kernel_cli_command()]
    guard = (MemoryGuard(admission.ACTIVE.execution_limit(), admission.ACTIVE.execution_floor())
             if admission.ACTIVE else MemoryGuard.from_gib(0, float(os.environ.get('TRELLIS_CHECK_JOBS_FLOOR_GIB', '6'))))
    deadline = time.monotonic() + 360
    with tempfile.TemporaryFile() as stdin, tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        stdin.write(payload); stdin.seek(0)
        process = subprocess.Popen(admission.ACTIVE.process_command(command) if admission.ACTIVE else command,
                                   stdin=stdin, stdout=out, stderr=err)
        identity = None
        try:
            identity = service._remember(process)
            while process.poll() is None:
                check_cancelled()
                if time.monotonic() >= deadline:
                    raise ProtocolError('resource_exhausted', 'host witness execution exceeded its deadline')
                if admission.ACTIVE is not None:
                    failure = admission.ACTIVE.memory_stop(guard, process.pid)
                    if failure and admission.ACTIVE.active[threading.get_ident()].get('controlled'):
                        raise failure
                    reason = failure.reason if failure else None
                else:
                    reason = guard.check(process.pid)
                if reason:
                    raise ProtocolError('resource_exhausted', reason)
                if out.tell() + err.tell() > 64 * 1024 * 1024:
                    raise ProtocolError('resource_exhausted', 'host witness execution exceeded its output limit')
                try:
                    process.wait(timeout=.5)
                except subprocess.TimeoutExpired:
                    pass
            if admission.ACTIVE is not None:
                failure = admission.ACTIVE.memory_stop(guard, process.pid)
                if failure and admission.ACTIVE.active[threading.get_ident()].get('controlled'):
                    raise failure
                if failure:
                    raise ProtocolError('resource_exhausted', failure.reason)
                admission.ACTIVE.check_carrier(process)
            out.seek(0); err.seek(0)
            result = json.loads(out.read())
            if process.returncode or result.get('status') != 'dispatch_ok':
                raise ProtocolError('internal_error', result.get('message') or err.read().decode(errors='replace')[-4000:])
            return result['output']
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
            if identity:
                service._forget(identity)
