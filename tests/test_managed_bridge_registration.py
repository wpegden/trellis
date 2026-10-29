import hashlib
import json
import os
from types import SimpleNamespace

import pytest

from trellis.runtime import bridge
from trellis.runtime.bridge_protocol import BridgeCliRequest


@pytest.mark.parametrize('kind', ['paper', 'corr', 'sound', 'review', 'audit', 'stuck_math_audit'])
@pytest.mark.parametrize('isabelle', [False, True])
def test_nonworker_dispatch_registers_only_lean_workspaces(tmp_path, monkeypatch, kind, isabelle):
    repo, runtime = tmp_path/'repo', tmp_path/'runtime'
    repo.mkdir()
    monkeypatch.setenv('TRELLIS_CHECK_JOBS', '1')
    for key in ['TRELLIS_CHECKER_TOKEN', 'TRELLIS_CHECK_JOBS_SOCKET', 'TRELLIS_CHECK_JOBS_TOKEN']:
        monkeypatch.setenv(key, '')
    monkeypatch.setattr(bridge, 'load_config', lambda _: SimpleNamespace(repo_path=repo))
    monkeypatch.setattr(bridge, 'validate_commissioned_operation', lambda *a, **k: None)
    monkeypatch.setattr('trellis.sandbox.repo_targets_isabelle', lambda _: isabelle)
    def dispatch(**kwargs):
        token = os.environ['TRELLIS_CHECK_JOBS_TOKEN']
        assert bool(token) is not isabelle
        if not isabelle:
            key = hashlib.sha256(token.encode()).hexdigest()
            cap = json.loads((runtime/'checker-state/jobs/capabilities'/f'{key}.json').read_text())
            assert cap['repo'] == str(repo)
            assert cap['owner']['pid'] == os.getpid()
            assert cap['priority'] == 'interactive'
        return {'dispatched': kind}
    monkeypatch.setattr(bridge, '_handle_'+kind, dispatch)
    result = bridge.handle_bridge_request(BridgeCliRequest(
        config_path=tmp_path/'config.json', runtime_root=runtime,
        request={'kind': kind, 'id': 1, 'cycle': 1, 'runtime_support_required': False}))
    assert result == {'dispatched': kind}
