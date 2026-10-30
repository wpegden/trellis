from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path

from scripts import pv_modeling


def make_preparation(root: Path) -> Path:
    state = root / '.trellis/modeling/state.json'
    state.parent.mkdir(parents=True)
    state.write_text(json.dumps({'stage': 'adaptation', 'status': 'active',
                                 'sequence': 12, 'next': {'role': 'reviewer'}}))
    return state


def test_status_tracks_lock_owner_and_pause_without_a_native_supervisor(tmp_path, capsys):
    state = make_preparation(tmp_path)
    lock = state.with_name('runner.lock')
    pause = state.with_name('pause')
    # State left behind by a dead driver must not count as a live process.
    assert pv_modeling.runner_status(tmp_path)['state'] == 'preparation_stopped'
    with lock.open('w') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        result = pv_modeling.runner_status(tmp_path)
        assert result['state'] == 'preparing'
        assert result['controller'] == 'preparation'
        assert result['wrapper_pid'] == os.getpid()
        assert result['preparation']['role'] == 'reviewer'
        assert result['resumable'] is False
        assert result['launch_env'] is None
        before = {p.name: p.read_bytes() for p in state.parent.iterdir()}
        pv_modeling._main(['status', str(tmp_path)])
        assert json.loads(capsys.readouterr().out) == result
        assert {p.name: p.read_bytes() for p in state.parent.iterdir()} == before
        pause.touch()
        assert pv_modeling.runner_status(tmp_path)['state'] == 'preparation_pausing'
    assert pv_modeling.runner_status(tmp_path)['state'] == 'preparation_paused'
    pause.unlink()
    assert pv_modeling.runner_status(tmp_path)['state'] == 'preparation_stopped'
    state.write_text(json.dumps({'status': 'prepared'}))
    assert pv_modeling.runner_status(tmp_path)['state'] == 'prepared'


def test_status_does_not_borrow_another_preparation_driver(tmp_path):
    first, second = tmp_path / 'first', tmp_path / 'second'
    first_state = make_preparation(first)
    make_preparation(second)
    with first_state.with_name('runner.lock').open('w') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert pv_modeling.runner_status(first)['state'] == 'preparing'
        assert pv_modeling.runner_status(second)['state'] == 'preparation_stopped'
    assert pv_modeling.runner_status(first)['state'] == 'preparation_stopped'
