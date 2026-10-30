import asyncio
import json
from pathlib import Path

import pytest

from trellis.dispatch.__main__ import verify_checker, sidecar_fence
from trellis.dispatch.coordination import Coordinator
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.native import NativeLedger


@pytest.mark.parametrize('status', [dict(pong=True), dict(pong=True, managed_jobs=False, compiler_slots=2),
                                  dict(pong=True, managed_jobs=True, compiler_slots=1)])
def test_launch_refuses_a_checker_with_wrong_admission(status, tmp_path):
    with pytest.raises(RuntimeError, match='2 compiler slots'):
        verify_checker(tmp_path, 2, ping=lambda *a, **kw: status)
    good = dict(pong=True, managed_jobs=True, compiler_slots=2)
    assert verify_checker(tmp_path, 2, ping=lambda *a, **kw: good) == good


def test_coordinator_excludes_legacy_sidecar_for_its_lifetime(tmp_path):
    with sidecar_fence(tmp_path):
        with pytest.raises(RuntimeError, match='legacy sidecar'):
            with sidecar_fence(tmp_path):
                pytest.fail('second legacy scheduler acquired ownership')
    with sidecar_fence(tmp_path):
        pass


def test_transport_budget_survives_private_workspaces_and_completion_replay(tmp_path, monkeypatch):
    monkeypatch.setenv('TRELLIS_VERIFIER_ARTIFACT_RETRY_BUDGET', '1')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime, driver.repo = tmp_path / 'runtime', tmp_path / 'repo'
    driver.repo.mkdir()
    driver.native = NativeLedger(driver.runtime)
    driver.root = driver.native.root
    def completion(id, status):
        directory = driver.root / f'verifier-{id}'
        (directory / 'runtime').mkdir(parents=True)
        (directory / 'runtime/protocol_state.json').write_text(json.dumps({'in_flight_request': {'kind': 'Sound'}}))
        response = dict(status=status)
        driver._record_transport(directory, response)
        driver._record_transport(directory, response)
    completion(1, 'Malformed')
    assert not driver.pausing()
    completion(2, 'Ok')
    completion(3, 'Malformed')
    assert not driver.pausing()
    completion(4, 'Malformed')
    assert driver.pausing()
    assert json.loads((driver.root / 'transport.json').read_text())['streaks']['sound'] == 2


def test_pause_drains_assigned_tasks_without_filling_new_slots():
    from test_dispatch_coordination import Driver, tick
    async def run():
        d = Driver()
        original = d.snapshot
        async def snapshot():
            return dict(await original(), pausing=True, reviewer_runnable=False)
        d.snapshot = snapshot
        c = Coordinator(d)
        await tick(c)
        assert not c.closed and len(c.runners) == 2
        for task in d.tasks.values():
            task['status'] = {'status': 'resolved'}
        await c.tick()
        assert not c.closed, 'native resolution must not abandon owned delivery waiters'
        from trellis.dispatch.coordination import Completion
        for task_id, future in d.done.items():
            future.set_result(Completion(task_id=task_id, candidate_sha256='already-resolved'))
        await asyncio.gather(*c.runners.values())
        await c.tick()
        assert c.closed and d.review_count == 0
        await c.shutdown()
    asyncio.run(run())


def test_operational_pause_is_not_copied_into_task_inputs(tmp_path):
    from trellis.dispatch.workspace import _copy_tree, _EXCLUDED
    repo, private = tmp_path / 'canonical', tmp_path / 'private'
    repo.mkdir()
    (repo / '.trellis-stop-after-checkpoint').write_text('operator pause')
    (repo / 'paper.tex').write_text('source evidence')
    _copy_tree(repo, private, exclude=_EXCLUDED)
    assert not (private / '.trellis-stop-after-checkpoint').exists()
    assert (private / 'paper.tex').read_text() == 'source evidence'


@pytest.mark.parametrize('marker', ['system_feedback_halt.json', 'checker_disagreement_halt.json'])
def test_halt_blocks_scheduling_launch_and_publication_without_losing_evidence(tmp_path, marker):
    from contextlib import nullcontext
    from types import SimpleNamespace
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime = driver.root = driver.repo = tmp_path
    driver.publisher = SimpleNamespace(locked=nullcontext)
    halt = tmp_path / marker
    # Even malformed markers must stop work, as the ordinary bridge does.
    halt.write_text('retained diagnostic')
    for operation in (
        driver._snapshot,
        driver._start_review,
        lambda: driver._routing({'kind': 'route'}),
        lambda: driver._permit(None, {}),
        lambda: driver._process(tmp_path / 'new-attempt', compiler='managed', seconds=3600),
        lambda: driver._dispatch_review({'request_id': 1}, {'kind': 'parallel_review', 'assignments': []}),
        lambda: driver._reject_candidate(1, 'digest', 'scope mismatch'),
        lambda: driver._publish(1, 'digest', tmp_path / 'acceptance', {}),
    ):
        with pytest.raises(RuntimeError, match='halt marker present'):
            operation()
        assert halt.read_text() == 'retained diagnostic'
    assert not (tmp_path / 'new-attempt').exists()
    halt.unlink()
    driver.require_no_halt()


@pytest.mark.parametrize('marker', ['system_feedback_halt.json', 'checker_disagreement_halt.json'])
def test_resume_cannot_clear_pause_or_start_work_while_halted(tmp_path, monkeypatch, marker):
    from trellis.dispatch import __main__ as launch
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime = tmp_path
    monkeypatch.setattr(launch, 'ProductionDriver', lambda _: driver)
    halt = tmp_path / marker
    halt.write_text('{}')
    pause = tmp_path / 'pause_request.json'
    pause.write_text('{"kind":"parallel_infrastructure"}')
    with pytest.raises(RuntimeError, match='halt marker present'):
        launch.main(['resume', str(tmp_path)])
    assert pause.read_text() == '{"kind":"parallel_infrastructure"}'
    assert halt.read_text() == '{}'
