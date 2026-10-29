"""Production checker launch defaults for fresh and resumed parallel runs."""
import json
import os

import pytest

from trellis.checker.server import configure_managed_jobs


@pytest.mark.parametrize('durable,explicit,expected', [(None, None, '4'), (6, None, '6'), (6, '3', '3')])
def test_parallel_launch_enables_shared_admission(tmp_path, monkeypatch, durable, explicit, expected):
    monkeypatch.delenv('TRELLIS_CHECK_JOBS', raising=False)
    monkeypatch.delenv('TRELLIS_CHECK_JOBS_PARALLELISM', raising=False)
    config = tmp_path / 'config.json'
    config.write_text(json.dumps({'parallel_dispatch': {'enabled': True, 'compiler_slots': 4}}))
    (tmp_path / 'runtime_metadata.json').write_text(json.dumps({'config_path': str(config)}))
    if durable is not None:
        (tmp_path / 'protocol_state.json').write_text(json.dumps({'dispatch': {'config': {'compiler_slots': durable}}}))
    if explicit is not None:
        monkeypatch.setenv('TRELLIS_CHECK_JOBS_PARALLELISM', explicit)
    configure_managed_jobs(tmp_path)
    assert os.environ['TRELLIS_CHECK_JOBS'] == '1'
    assert os.environ['TRELLIS_CHECK_JOBS_PARALLELISM'] == expected


def test_serial_launch_retains_explicit_settings(tmp_path, monkeypatch):
    monkeypatch.delenv('TRELLIS_CHECK_JOBS', raising=False)
    monkeypatch.delenv('TRELLIS_CHECK_JOBS_PARALLELISM', raising=False)
    config = tmp_path / 'config.json'
    config.write_text('{}')
    (tmp_path / 'runtime_metadata.json').write_text(json.dumps({'config_path': str(config)}))
    configure_managed_jobs(tmp_path)
    assert 'TRELLIS_CHECK_JOBS' not in os.environ
    assert 'TRELLIS_CHECK_JOBS_PARALLELISM' not in os.environ
