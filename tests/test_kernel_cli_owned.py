"""Synthetic native/hook ownership through the production invocation helper."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

import pytest

from trellis.runtime import kernel_cli as cli


ROOT = Path(__file__).resolve().parents[1]


def controller(tmp_path, *, mode='normal', setup='', cleanup='', fail_drain=False):
    entered, late, receipt = [tmp_path / name for name in ('entered', 'late', 'receipt.json')]
    hook = 'import time\nfrom pathlib import Path\ntime.sleep(1.7)\nPath(' + repr(str(late)) + ').write_text("late commit")\ntime.sleep(10)'
    kernel = tmp_path / 'kernel.py'
    kernel.write_text('import sys,json,subprocess,time\nfrom pathlib import Path\njson.load(sys.stdin)\n'
        + 'subprocess.Popen([sys.executable,"-I","-S","-c",' + repr(hook)
        + '],start_new_session=True,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n'
        + 'Path(' + repr(str(entered)) + ').touch()\n'
        + ('time.sleep(10)\n' if mode in ('cancel', 'timeout') else '')
        + ('print(\'{"error":"synthetic refusal"}\');raise SystemExit(2)\n' if mode == 'failure'
           else 'print(\'{"status":"ok"}\')\n'))
    driver = tmp_path / 'driver.py'
    body = '''import ctypes,json,os,signal,sys
from pathlib import Path
sys.path.insert(0, ROOT)
from trellis.runtime import kernel_cli as cli
libc=ctypes.CDLL(None)
def subreaper():
 value=ctypes.c_int();assert libc.prctl(37,ctypes.byref(value),0,0,0)==0;return value.value
previous=subreaper()
handlers={s:signal.getsignal(s) for s in (signal.SIGTERM,signal.SIGINT)}
cli.kernel_cli_command=lambda:[sys.executable,'-I','-S',KERNEL]
'''.replace('ROOT', repr(str(ROOT))).replace('KERNEL', repr(str(kernel)))
    if fail_drain:
        body += '''from trellis.dispatch import process
original=process.reap_descendants
def failed_drain():
 original();raise RuntimeError('synthetic drain receipt failure')
process.reap_descendants=failed_drain
'''
    body += setup + '\n'
    body += '''try:
 result=cli.run_kernel_cli({'action':'synthetic'},owned=True,timeout_seconds=TIMEOUT)
 report={'result':result}
except BaseException as error:
 report={'error':type(error).__name__,'quiescent':getattr(error,'kernel_quiescent',False)}
finally:
 CLEANUP
report['subreaper_restored']=subreaper()==previous
report['handlers_restored']=all(signal.getsignal(s)==h for s,h in handlers.items())
Path(RECEIPT).write_text(json.dumps(report))
'''.replace('TIMEOUT', '1' if mode == 'timeout' else '5').replace('CLEANUP', cleanup or 'pass').replace('RECEIPT', repr(str(receipt)))
    driver.write_text(body)
    process = subprocess.Popen([sys.executable, '-I', '-S', str(driver)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if mode == 'cancel':
        deadline = time.monotonic() + 5
        while not entered.exists() and time.monotonic() < deadline and process.poll() is None:
            time.sleep(.01)
        assert entered.exists(), 'synthetic native command did not start'
        process.send_signal(signal.SIGTERM)
    try:
        assert process.wait(timeout=10) == 0
    finally:
        if process.poll() is None:
            process.kill(); process.wait()
    report = json.loads(receipt.read_bytes())
    time.sleep(1.8)
    assert not late.exists(), 'a helper remained able to commit after native closure'
    assert report['subreaper_restored'] and report['handlers_restored']
    return report, entered


@pytest.mark.parametrize('mode', ['normal', 'failure', 'timeout', 'cancel'])
def test_owned_native_call_drains_escaped_hook_before_return(tmp_path, mode):
    report, entered = controller(tmp_path, mode=mode)
    assert entered.exists()
    if mode == 'normal':
        assert report['result'] == {'status': 'ok'}
    else:
        expected = {'failure': 'KernelCliError', 'timeout': 'TimeoutExpired', 'cancel': 'KeyboardInterrupt'}
        assert report['error'] == expected[mode] and report['quiescent']


def test_cancellation_preserves_original_error_when_drain_receipt_fails(tmp_path):
    report, _ = controller(tmp_path, mode='cancel', fail_drain=True)
    assert report['error'] == 'KeyboardInterrupt' and not report['quiescent']


def test_preexisting_child_is_refused_and_survives(tmp_path):
    report, entered = controller(tmp_path,
        setup="import subprocess\nunrelated=subprocess.Popen([sys.executable,'-c','import time;time.sleep(10)'])",
        cleanup="report['unrelated_survived']=unrelated.poll() is None;unrelated.terminate();unrelated.wait()")
    assert report['error'] == 'KernelCliError' and report['unrelated_survived'] and not entered.exists()


def test_other_os_thread_refuses_before_spawn(tmp_path):
    source, library = tmp_path / 'thread.c', tmp_path / 'thread.so'
    source.write_text('#include <pthread.h>\n#include <unistd.h>\n#include <stdatomic.h>\n'
        'static pthread_t t;static atomic_int done;\n'
        'static void *run(void *p){while(!atomic_load(&done))usleep(1000);return 0;}\n'
        'int start(void){return pthread_create(&t,0,run,0);}\n'
        'void stop(void){atomic_store(&done,1);pthread_join(t,0);}\n')
    subprocess.run(['/usr/bin/cc', '-shared', '-fPIC', '-pthread', str(source), '-o', str(library)], check=True)
    report, entered = controller(tmp_path, setup='lib=ctypes.CDLL(' + repr(str(library)) + ')\nassert lib.start()==0', cleanup='lib.stop()')
    assert report['error'] == 'KernelCliError' and not entered.exists()


def test_owned_empty_sigterm_is_never_retried(monkeypatch):
    calls = []
    def once(payload, **kwargs):
        calls.append((payload, kwargs))
        return subprocess.CompletedProcess([], -15, '', '')
    monkeypatch.setattr(cli, '_run_kernel_cli_once', once)
    with pytest.raises(cli.KernelCliError) as result:
        cli.run_kernel_cli({'action': 'synthetic'}, owned=True, timeout_seconds=1)
    assert len(calls) == 1 and result.value.kernel_quiescent


def test_descriptor_forwarding_requires_owned_scope():
    with pytest.raises(cli.KernelCliError, match='owned kernel'):
        cli.run_kernel_cli({'action': 'synthetic'}, pass_fds=(17,))
