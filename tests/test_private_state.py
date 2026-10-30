import hashlib
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis import private_state as private
from trellis.history_artifacts import encode_shared_state
from trellis.dispatch.candidate import canonical_json


def test_plain_and_compact_have_exact_same_logical_value_without_aliases():
    state = {'a': [{'long': 'x' * 200}] * 4, 'integer': 2**64 - 1}
    assert private.decode_private_state(state) is state
    decoded = private.decode_private_state(encode_shared_state({'state': state}))
    assert decoded == state
    decoded['a'][0]['long'] = 'changed'
    assert decoded['a'][1]['long'] == 'x' * 200


@pytest.mark.parametrize('change', ['unknown_format', 'extra', 'nonobject', 'dangling'])
def test_malformed_carrier_is_not_plain_or_empty(change):
    carrier = encode_shared_state({'state': {}})
    if change == 'unknown_format': carrier['$format'] = 'unknown'
    if change == 'extra': carrier['checkpoint'] = {}
    if change == 'nonobject': carrier['state'] = []
    if change == 'dangling': carrier['state'] = {'$r': 'missing'}
    with pytest.raises(ValueError): private.decode_private_state(carrier)


def test_small_snapshot_uses_unchanged_plain_bytes_and_never_overwrites(tmp_path):
    path = tmp_path / 'protocol_state.json'
    state = {'cycle': 3, 'text': 'λ'}
    private.write_private_state(path, state, kernel=lambda _: pytest.fail('small snapshot needs no RPC'))
    assert path.read_bytes() == canonical_json(state)
    with pytest.raises(FileExistsError): private.write_private_state(path, {'changed': True})
    assert path.read_bytes() == canonical_json(state)


def test_large_writer_validates_physical_receipt(tmp_path):
    path = tmp_path / 'protocol_state.json'
    state = {'a': ['long repeated value' * 20] * 400}
    def kernel(request):
        assert request == dict(action='write_private_state', path=str(path), state=state)
        raw = canonical_json(encode_shared_state({'state': state}))
        path.write_bytes(raw)
        return dict(status='dispatch_ok', output=dict(status='written', bytes=len(raw),
                    sha256=hashlib.sha256(raw).hexdigest()))
    private.write_private_state(path, state, kernel=kernel)
    assert private.read_private_state(path) == state
    assert path.stat().st_size < len(canonical_json(state)) / 10


def test_float_fallback_is_exact_existing_python_codec_in_child(tmp_path):
    path = tmp_path / 'protocol_state.json'
    state = {'a': ['long repeated value' * 20] * 400, 'float': [1.0, -0.0, 1e100]}
    private.write_private_state(path, state, kernel=lambda _: dict(status='dispatch_ok', output={'status':'unsupported_float'}))
    assert path.read_bytes() == canonical_json(encode_shared_state({'state': state}))
    assert private.read_private_state(path) == state


@pytest.mark.parametrize('response', [dict(status='error', message='native failed'),
    dict(status='dispatch_ok', output=dict(status='unsupported_other'))])
def test_other_native_errors_never_fall_back(tmp_path, response):
    path = tmp_path / 'protocol_state.json'
    with pytest.raises(RuntimeError):
        private.write_private_state(path, {'large': 'x'*70000}, kernel=lambda _: response)
    assert not path.exists()


def test_readonly_status_consumers_decode_compact_snapshot(tmp_path, monkeypatch):
    from trellis import stall_analyzer, viewer_adapter, cli_monitor
    path = tmp_path / 'protocol_state.json'
    state = dict(cycle=4, stage='Worker', in_flight_request=dict(id=8, kind='Worker'))
    path.write_bytes(canonical_json(encode_shared_state({'state': state})))
    assert stall_analyzer._read_json(path) == state
    assert viewer_adapter._read_json(path) == state
    monkeypatch.setattr(viewer_adapter, '_load_runtime_info', lambda _: SimpleNamespace(protocol_state_path=path))
    assert cli_monitor._read_protocol_state(tmp_path) == state


def test_viewer_private_projection_reads_the_same_request_from_both_formats(tmp_path):
    import importlib.util
    spec = importlib.util.spec_from_file_location('private_projection',
        Path(__file__).resolve().parents[1] / 'viewer/parallel_state.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    state = dict(phase='ProofFormalization', cycle=7, stage='Worker',
                 in_flight_request=dict(id=19, kind='Worker', cycle=7, active_node='A'))
    path = tmp_path / 'protocol_state.json'
    path.write_bytes(canonical_json(state))
    plain = module.project(path)
    path.write_bytes(canonical_json(encode_shared_state({'state': state})))
    assert module.project(path) == plain
    assert plain['in_flight_request']['id'] == 19


def test_float_fallback_ignores_old_package_in_callers_current_directory(tmp_path, monkeypatch):
    decoy = tmp_path / 'old-checkout'
    (decoy / 'trellis').mkdir(parents=True)
    (decoy / 'trellis/__init__.py').write_text('raise RuntimeError("OLD_PACKAGE_IMPORTED")')
    monkeypatch.chdir(decoy)
    path = tmp_path / 'protocol_state.json'
    state = {'long': 'x' * 70000, 'float': 1.0}
    private.write_private_state(path, state, kernel=lambda _: dict(status='dispatch_ok',
        output={'status': 'unsupported_float'}))
    assert path.read_bytes() == canonical_json(encode_shared_state({'state': state}))
