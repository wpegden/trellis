"""Host witness hydration remains inside managed admission and process ownership."""
import json
import sys
from types import SimpleNamespace

import pytest

from trellis.checker import admission, host_response
from trellis.checker.protocol import ProtocolError


@pytest.mark.parametrize('cancel', [False, True])
def test_fixed_host_adapter_holds_admission_until_namespace_exit(tmp_path, monkeypatch, cancel):
    from trellis.runtime import kernel_cli
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    runtime = tmp_path / 'runtime'
    runtime.mkdir()
    response = {'kind': 'worker', 'status': 'Ok'}
    (runtime / 'host-response.json').write_text(json.dumps(response))
    code = "import json,sys,time; p=json.load(sys.stdin); assert p['allow_compilation']; "
    code += "time.sleep(60)" if cancel else "print(json.dumps({'status':'dispatch_ok','output':p['response']}))"
    monkeypatch.setattr(kernel_cli, 'kernel_cli_command', lambda: [sys.executable, '-c', code])
    events = []
    class Service:
        def _remember(self, process):
            self.process = process
            assert len(scheduler.active) == 1
            scheduler.track_process(process.pid)
            events.append('started')
            return {'pid': process.pid}
        def _forget(self, identity):
            assert self.process.poll() is not None
            assert len(scheduler.active) == 1, 'capacity released before process cleanup'
            scheduler.track_process(identity['pid'], remove=True)
            events.append('reaped')
    if cancel:
        def cancelled():
            raise ProtocolError('cancelled', 'fixture cancellation')
        monkeypatch.setattr(host_response, 'check_cancelled', cancelled)
        with pytest.raises(ProtocolError, match='fixture cancellation'):
            host_response.enrich(SimpleNamespace(runtime_root=runtime), Service())
    else:
        assert host_response.enrich(SimpleNamespace(runtime_root=runtime), Service()) == response
    assert events == ['started', 'reaped']
    assert not scheduler.active and not scheduler.cleanup_failed
