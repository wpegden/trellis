"""Exercise installed Codex tool dispatch without inference or external APIs."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import threading
from contextlib import nullcontext
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from trellis.adapters import ProviderConfig
from trellis.agents.codex_headless import build_script, build_launcher_script
from trellis.checker.admission import Admission
from trellis.checker.jobs import JobService, register_workspace
from trellis.config import SandboxConfig
from trellis.dispatch.confinement import data_view
from trellis.host_runtime import worker_elan_home
from trellis.sandbox import wrap_command


@pytest.mark.parametrize('model', ['gpt-5.6-sol', 'gpt-5.6-luna'])
@pytest.mark.parametrize('backend', ['legacy', 'native'])
def test_confined_provider_tools_fresh_and_resumed(tmp_path, monkeypatch, model, backend):
    from trellis import check_job
    from test_check_jobs_lean import make_repo
    codex = shutil.which('codex')
    helper = os.environ.get('TRELLIS_TEST_BINDFS') or shutil.which('bindfs')
    native = os.environ.get('TRELLIS_TEST_COMPILER_BWRAP')
    lean = worker_elan_home() / 'toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean'
    if not codex or not lean.is_file():
        pytest.skip('requires installed Codex and the pinned Lean toolchain')
    if backend == 'legacy' and (not helper or not Path('/dev/fuse').exists()):
        pytest.skip('legacy recipe requires FUSE')
    if backend == 'native' and not native:
        pytest.skip('requires the provisioned native noexec host helper')
    repo = make_repo(tmp_path)
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    local_kernel = repo / '.trellis/runtime/bin/trellis_runtime_cli'
    local_kernel.parent.mkdir(parents=True)
    shutil.copy('/usr/bin/true', local_kernel)
    monkeypatch.setenv('TRELLIS_TRELLIS_KERNEL_CMD', '/usr/bin/true')
    monkeypatch.setenv('TRELLIS_AGENT_COMPILER_CAPABILITY', 'managed')
    monkeypatch.setenv('TRELLIS_LEAN_PARALLELISM', '1')
    check_job.provision(repo / '.trellis/scripts')
    shutil.copy(check_job.__file__, repo / 'client.py')
    home = tmp_path / 'home'; home.mkdir()
    # A prior burst can plant apparent provider installs pointing at Lean.
    # These must never enlarge the next burst's executable mounts or PATH.
    for prefix in ('.trellis-npm', '.local/share/npm-global'):
        provider_bin = home / prefix / 'bin'; provider_bin.mkdir(parents=True)
        if model.endswith('luna'):
            # Include the libraries too, while leaving actual Codex unshadowed;
            # denial must not merely result from missing executable libraries.
            (provider_bin / 'agy').symlink_to(lean.parent.parent, target_is_directory=True)
        else:
            for provider in ('codex', 'claude', 'gemini', 'agy'):
                (provider_bin / provider).symlink_to(lean)
    logs = repo / '.trellis/logs'; logs.mkdir()
    catalog = repo / 'provider-models.json'
    # Bundled metadata is sufficient for this offline protocol test. Production
    # exports the effective catalog, including custom caller configuration.
    exported = subprocess.run([codex, 'debug', 'models', '--bundled'], capture_output=True, text=True, check=True)
    catalog.write_text(exported.stdout)
    body = f'''
from pathlib import Path
import ctypes as C, subprocess, shutil
import client
Path('Tablet/Example.lean').write_text('example : 1 = 1 := by rfl\\n')
for native in [{str(lean)!r}, 'Tablet/copied-lean']:
 if native == 'Tablet/copied-lean': shutil.copy({str(lean)!r}, native)
 try: subprocess.run([str(Path(native).resolve()), '--version'], check=True)
 except PermissionError: pass
 else: raise AssertionError('unmanaged Lean executed')
lib=C.CDLL(None);lib.mmap.restype=C.c_void_p
assert lib.mmap(None,4096,7,0x22,-1,0)==C.c_void_p(-1).value
job=client.check('incremental', 'Example', timeout=30)
assert job['state']=='succeeded',job
print('confined-tools-and-managed-check-ok')
'''
    (repo / 'probe.py').write_text(body)
    command = shlex.join(['/usr/bin/python3', str(repo / 'probe.py')])
    requests = []
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args): pass
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            requests.append(request)
            if len(requests) % 2:
                item = dict(type='function_call', id=f'fc_{len(requests)}', call_id=f'call_{len(requests)}',
                    name='exec_command', arguments=json.dumps(dict(cmd=command, max_output_tokens=500, yield_time_ms=10000)), status='completed')
            else:
                item = dict(type='message', id=f'msg_{len(requests)}', role='assistant', status='completed',
                            content=[dict(type='output_text', text='Offline probe complete.')])
            response = dict(id=f'resp_{len(requests)}', object='response', status='completed', output=[item],
                            usage=dict(input_tokens=1, output_tokens=1, total_tokens=2))
            events = [dict(type='response.created', response=dict(response, status='in_progress', output=[])),
                      dict(type='response.output_item.added', output_index=0, item=item),
                      dict(type='response.output_item.done', output_index=0, item=item),
                      dict(type='response.completed', response=response)]
            data = ''.join('event: '+e['type']+'\ndata: '+json.dumps(e)+'\n\n' for e in events).encode()
            self.send_response(200); self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    runtime = tmp_path / 'runtime'
    channel = register_workspace(runtime, repo, scope='offline-provider-tools')
    for key, value in channel.items(): monkeypatch.setenv(key, value)
    service = JobService(runtime, scheduler=Admission(1)); service.start()
    extra = ['-c', f'model_catalog_json={json.dumps(str(catalog))}',
             '-c', 'model_provider="fixture"',
             '-c', 'model_providers.fixture={name="Fixture",base_url="http://127.0.0.1:'+
                    str(server.server_port)+'/v1",wire_api="responses",requires_openai_auth=false}',
             '-c', 'features.apps=false', '-c', 'features.enable_request_compression=false',
             '-c', 'features.responses_websockets=false', '-c', 'features.responses_websockets_v2=false',
             '-c', 'web_search="disabled"']
    try:
        with (data_view(tmp_path / 'mounts', bindfs=helper) if backend == 'legacy' else nullcontext()) as view:
            if backend == 'legacy':
                monkeypatch.setenv('TRELLIS_COMPILER_DATA_VIEW', str(view))
                monkeypatch.delenv('TRELLIS_COMPILER_BWRAP', raising=False)
            else:
                monkeypatch.setenv('TRELLIS_COMPILER_BWRAP', native)
                monkeypatch.delenv('TRELLIS_COMPILER_DATA_VIEW', raising=False)
            # Agent configuration must not make discovery read a host-only
            # catalog through a symlink before the provider boundary applies.
            from trellis.agents.codex_tool_mode import direct_tool_catalog
            outside = tmp_path / 'host-only-models.json'; outside.write_text(exported.stdout)
            link = repo / 'external-models.json'; link.symlink_to(outside)
            with pytest.raises(RuntimeError, match='catalog export failed'):
                direct_tool_catalog(ProviderConfig('codex', model=model,
                    extra_args=['-c', f'model_catalog_json={json.dumps(str(link))}']), home, repo)
            assert not (home / '.codex/trellis-direct-models.json').exists()
            thread_id = ''
            for attempt in ('fresh', 'resume'):
                prompt = logs / f'{attempt}-prompt.txt'; prompt.write_text('Offline test; no inference.')
                started, exited = logs / f'{attempt}.started', logs / f'{attempt}.exit'
                script = build_script(ProviderConfig('codex', model=model, extra_args=extra),
                    prompt_file=prompt, start_file=started, exit_file=exited, work_dir=repo,
                    burst_home=home, log_prefix=attempt, resume_thread_id=thread_id)
                argv = wrap_command(['/bin/bash', str(script)], sandbox=SandboxConfig(enabled=True, backend='bwrap'),
                                    work_dir=repo, burst_home=home, role='worker')
                launcher = build_launcher_script(script_path=script, launch_cmd=argv, log_dir=logs, log_prefix=attempt)
                result = subprocess.run(['/bin/bash', str(launcher)], capture_output=True, text=True, timeout=50)
                assert result.returncode == 0, result.stderr
                output = (logs / f'{attempt}-output.log').read_text()
                assert started.exists() and exited.read_text().strip() == '0', output
                items = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
                commands = [item['item'] for item in items if item['type']=='item.completed' and item['item']['type']=='command_execution']
                assert any(c['exit_code']==0 and 'confined-tools-and-managed-check-ok' in c['aggregated_output'] for c in commands), output
                thread_id = next(item['thread_id'] for item in items if item['type']=='thread.started')
                assert 'code-mode host' not in output
                assert (repo / 'Tablet/Example.lean').read_text() == 'example : 1 = 1 := by rfl\n'
            assert len(requests)==4
            # Responses Lite embeds the schema in input rather than top-level tools.
            assert 'exec_command' in json.dumps(requests[0])
            with service.lock:
                assert service.db.execute('SELECT count(*) FROM jobs').fetchone()[0] == 1  # the identical resumed check reuses its job
    finally:
        service.shutdown(); server.shutdown(); thread.join(timeout=5)
