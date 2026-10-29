"""Real native/checker report-during-finalization canary; providers are simulated.

Run from a matching source root with its attested kernel outside /tmp:
  python3 tests/fixtures/parallel_dispatch/empty_report.py FRESH_BASE ordinary|staged
The fixture only delays delivery at real process boundaries. It never changes
canonical state, receipts, verdicts or source after checking.
"""
import asyncio
import hashlib
import json
import os
from pathlib import Path
import sys
import time

mode = sys.argv.pop(2)
assert mode in ('ordinary', 'staged')
source = Path.cwd()
fixture = source / 'tests/fixtures/parallel_dispatch/run.py'
bootstrap = fixture.read_text().split('\nclass FixtureProcess(')[0]
sys.argv.append('--staged-helpers')  # Real shared Root topology + ordinary B helpers.
base_arg = Path(sys.argv[1]).resolve()
provider_path = base_arg.parent / (base_arg.name + '-provider.py')
provider = (source / 'tests/fixtures/parallel_dispatch/provider.py').read_text()
needle = "    assert kind == 'worker' and node in ('A', 'B')\n"
assert provider.count(needle) == 1
provider = provider.replace(needle, needle + '''    if node == 'A':
        return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
            burst_role=single.burst_role, ok=True, payload=dict(outcome='stuck',
            summary='Need a supporting construction.', comments='', system_feedback='', deleted_nodes=[]))
''')
if mode == 'ordinary':
    # Ordinary workers may preserve an existing owner's difficulty explicitly.
    # Staged PF supports metadata only for new helpers, so this legal no-op
    # selects the real full gate without overriding staged admission.
    needle = "difficulty_updates={prefix + 'Aux': 'easy', prefix + 'Zero': 'easy'})"
    assert provider.count(needle) == 1
    provider = provider.replace(needle,
        "difficulty_updates={prefix + 'Aux': 'easy', prefix + 'Zero': 'easy', 'B': 'hard'})")
provider_path.write_text(provider)
exec(compile(bootstrap, str(fixture), 'exec'), globals())
from trellis.dispatch.acceptance_queue import Footprint

trace = []
def note(kind, **values):
    trace.append(dict(kind=kind, at=time.time(), **values))
    (base / 'empty-report-trace.json').write_text(json.dumps(trace, indent=2) + '\n')

async def until(predicate, explanation):
    deadline = time.monotonic() + 180
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError(explanation)
        await asyncio.sleep(.05)

class Process(AttemptProcess):
    def start_or_adopt(self, command, payload, **kwargs):
        if command[-1] == 'trellis.runtime.bridge_cli':
            command = [sys.executable, str(provider_path)]
            payload = dict(payload, fixture_ordinary_refresh='helpers')
        return super().start_or_adopt(command, payload, **kwargs)

class Driver(ProductionDriver):
    async def start_review(self):
        # Bound provider work to the original two candidates, while retaining
        # production snapshot/archive/Route housekeeping throughout B's check.
        if any(t['status']['status'] == 'resolved'
               for t in self.state()['dispatch']['tasks'].values()):
            return None
        return await super().start_review()
    def _acceptance_footprint(self, task_id, identity):
        result = super()._acceptance_footprint(task_id, identity)
        if task_id == 2 and mode == 'ordinary' and result is not None:
            # Exercise the existing conservative full-gate path, never weaken
            # the real native source/read scope to create overlap artificially.
            return Footprint(result.reads, result.writes, exclusive=True)
        return result
    async def integrate(self, task_id, identity):
        if task_id == 1:
            await until(lambda: (check_dir() / 'execution/permit.json').exists()
                        and self.finalization_gate.locked(), 'B did not capture under finalization gate')
            note('report_delivery_started', gate_locked=self.finalization_gate.locked())
        return await super().integrate(task_id, identity)
    async def _wait(self, process, record, **kwargs):
        result = await super()._wait(process, record, **kwargs)
        if process.root == check_dir() / 'execution':
            assert self.finalization_gate.locked()
            note('checked_result_retained', response_sha256=digest(process.root / 'response.json'))
            await until(lambda: self.state()['dispatch']['tasks']['1']['status']['status'] == 'resolved',
                        'empty report did not commit/reap while B held gate')
            status = await asyncio.to_thread(self._snapshot)
            assert not status['tasks']['1'].get('request_archivable', False)
            assert not status.get('routing_pending')
            note('report_reaped_before_publication', gate_locked=self.finalization_gate.locked(),
                 housekeeping=True, last_clean_commit=self.state().get('last_clean_commit'))
        return result

def check_dir():
    directory = runtime / 'dispatch/2/acceptance/1'
    return directory / 'finalization/1' if mode == 'staged' else directory

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

async def run():
    driver = Driver(runtime, process_factory=Process)
    coordinator = Coordinator(driver)
    runner = asyncio.create_task(coordinator.run())
    try:
        await until(lambda: runner.done() or len(driver.state()['dispatch']['tasks']) == 2
                    and all(t['status']['status'] == 'resolved'
                            for t in driver.state()['dispatch']['tasks'].values()), 'canary did not resolve')
        if runner.done(): await runner
        state = driver.state(); tasks = state['dispatch']['tasks']
        assert tasks['1']['status']['outcome']['kind'] == 'stuck'
        assert tasks['1'].get('pending_retry') is None
        assert tasks['2']['status']['outcome']['kind'] == 'accepted'
        assert not (runtime / 'dispatch/1/acceptance').exists()
        assert not (runtime / 'dispatch/1/acceptance-admission.json').exists()
        assert len(list((runtime / 'dispatch/2/acceptance').glob('*/canonical.json'))) == 1
        if mode == 'staged':
            assert len(list((runtime / 'dispatch/2/acceptance/1/finalization').glob('*/canonical.json'))) == 1
        else:
            assert 'staged_plan' not in json.loads((check_dir() / 'canonical.json').read_text())
            assert not (check_dir() / 'finalization').exists()
            assert state['node_difficulty']['B'] == 'Hard'
        assert not list((runtime / 'dispatch/2/acceptance').glob('*/rejections/*.json'))
        assert {'A', 'B', 'Root'} <= set(state['live']['present_nodes'])
        helpers = set(tasks['2']['assignment']['scope']['helper_prefixes'])
        actual = {node for node in state['live']['present_nodes'] if any(node.startswith(p) for p in helpers)}
        assert len(actual) == 2 and actual <= set(state['deps']['B'])
        assert (repo / 'Tablet/A.lean').read_text().count('sorry') == 1
        checked = next(row for row in trace if row['kind'] == 'checked_result_retained')
        assert digest(check_dir() / 'execution/response.json') == checked['response_sha256']
        assert any(row['kind'] == 'report_reaped_before_publication' and row['gate_locked'] for row in trace)
        assert not (runtime / 'dispatch/publication/pending.json').exists()
        result = dict(passed=True, mode=mode, trace=trace, publications=state['dispatch']['publications'],
                      helper_nodes=sorted(actual), provider='deterministic fixture; no paid provider claims')
        (base / 'empty-report-passed.json').write_text(json.dumps(result, indent=2) + '\n')
        print('PASS', mode, flush=True)
    finally:
        await coordinator.shutdown()
        runner.cancel()
        await asyncio.gather(runner, return_exceptions=True)

asyncio.run(run())
