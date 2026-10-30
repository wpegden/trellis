import asyncio
import copy
import json
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.dispatch import candidate, failure_report
from trellis.dispatch.acceptance_queue import AcceptanceQueue, Footprint
from trellis.dispatch.driver import ProductionDriver


def fixture(tmp_path, *, submission=False, raw=None, changes=False):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = tmp_path / 'dispatch'
    driver.repo = tmp_path / 'repo'
    driver.runtime = tmp_path / 'runtime'
    (driver.repo / 'Tablet').mkdir(parents=True)
    source = driver.repo / 'Tablet/A.lean'
    source.write_text('import H\n-- BODY\ntheorem A : True := by sorry\n')
    (driver.repo / 'Tablet/H.lean').write_text('theorem H : True := by trivial\n')
    before = candidate.source_manifest(driver.repo)
    if changes:
        source.write_text(source.read_text().replace('sorry', 'trivial'))
    task = dict(id=2, assignment=dict(task_kind='lean_closure', compiler='managed',
        scope=dict(nodes=['A'], helper_prefixes=[], shared_writes=[], exclusive=False)))
    if submission:
        task['worker_attempt'] = dict(turn=1, contract_sha256='a'*64)
    sealed = driver._submission_directory(task) / 'candidate'
    identity = candidate.seal(driver.repo, sealed, task_id=2, base_manifest=before, inputs=before,
        response=raw or dict(outcome='stuck', summary='Need a supporting construction'),
        submission=dict(turn=1, contract_sha256='a'*64) if submission else None)
    task['status'] = dict(status='checking', candidate_sha256=identity)
    state = dict(phase='ProofFormalization', dispatch=dict(tasks={'2': task}))
    driver.state = lambda: copy.deepcopy(state)
    driver.now_ms = lambda: 100
    driver.require_no_halt = lambda: None
    driver.acceptance_queue = AcceptanceQueue()
    driver.finalization_gate = asyncio.Lock()
    calls = []
    source_view = lambda: dict(input_hashes=candidate.source_manifest(driver.repo), graph=dict(edges={}))
    def native(payload):
        calls.append(payload)
        if payload['action'] == 'dispatch_acceptance_state':
            return dict(in_flight_request=dict(id=9, cycle=4), live=dict(present_nodes=['A', 'H']))
        if payload['action'] == 'check_unchanged_closure_stuck':
            return dict(ok=True, response=dict(kind='worker', status='Ok', outcome='Stuck',
                request_id=9, cycle=4, summary=payload['raw_payload']['summary']))
        if payload['action'] == 'dispatch_control':
            task['status'] = dict(status='stopping', outcome=payload['event']['outcome'])
            return {}
        raise AssertionError(payload)
    def commit(payload):
        calls.append(dict(action='commit_failure', payload=payload))
        task['status'] = dict(status='stopping', outcome=dict(kind='stuck'))
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: source_view(), _call=native,
        prepare_publication=lambda p: dict(p, base_state_sha256='b'*64), commit_failure=commit)
    return driver, task, identity, calls


@pytest.mark.parametrize('submission', [False, True])
def test_empty_report_current_native_delivery_retains_evidence_and_slot(tmp_path, submission):
    driver, task, identity, calls = fixture(tmp_path, submission=submission)
    sealed = driver._submission_directory(task) / 'candidate/candidate.json'
    old = sealed.read_bytes()
    assert failure_report.eligible(driver, 2, identity)
    assert failure_report.deliver(driver, 2, identity)
    assert sealed.read_bytes() == old
    assert task['status']['status'] == 'stopping', 'only normal cleanup releases the worker slot'
    commits = [c['payload'] for c in calls if c['action'] == 'commit_failure']
    assert len(commits) == 1 and commits[0]['closure_stuck_to_review'] is True
    assert commits[0]['graph_before'] == commits[0]['graph_after']
    assert len(list((driver.root/'2/reports').glob('*.json'))) == 1
    assert not (driver.root/'2/acceptance').exists()
    assert failure_report.deliver(driver, 2, identity), 'late callback observes terminal current disposition'
    assert len([c for c in calls if c['action'] == 'commit_failure']) == 1


def test_changed_dependency_is_stale_without_native_check_or_proof_claim(tmp_path):
    driver, task, identity, calls = fixture(tmp_path)
    assert failure_report.eligible(driver, 2, identity)
    (driver.repo/'Tablet/H.lean').write_text('theorem H : True := by sorry\n')
    assert failure_report.deliver(driver, 2, identity)
    assert task['status']['outcome']['kind'] == 'stale'
    assert 'Tablet/H.lean' in task['status']['outcome']['reason']
    assert [c['action'] for c in calls] == ['dispatch_control']


@pytest.mark.parametrize('reason', ['partial', 'ordinary', 'active_round', 'valid', 'contract'])
def test_unselected_reports_keep_ordinary_eligibility(tmp_path, reason):
    driver, task, identity, _ = fixture(tmp_path, changes=reason == 'partial', submission=reason == 'contract',
        raw=dict(outcome='valid', summary='done') if reason == 'valid' else None)
    if reason == 'ordinary': task['assignment']['task_kind'] = 'ordinary'
    if reason == 'active_round':
        path = driver.root/'2/acceptance/1/canonical.json'; path.parent.mkdir(parents=True);path.write_text('{}')
    if reason == 'contract':
        task['worker_attempt']['contract_sha256'] = 'c'*64
        with pytest.raises(candidate.CandidateError, match='contract'):
            failure_report.eligible(driver, 2, identity)
    else:
        assert not failure_report.eligible(driver, 2, identity)


def test_native_unsupported_report_falls_back_and_corrupt_native_result_fails(tmp_path):
    driver, task, identity, _ = fixture(tmp_path)
    original = driver.native._call
    driver.native._call = lambda p: None if p['action'] == 'check_unchanged_closure_stuck' else original(p)
    assert failure_report.deliver(driver, 2, identity) is False
    driver.native._call = lambda p: dict(ok=True, response=dict(outcome='Valid')) if p['action'] == 'check_unchanged_closure_stuck' else original(p)
    with pytest.raises(RuntimeError, match='inconsistent failure'):
        failure_report.deliver(driver, 2, identity)
    assert task['status']['status'] == 'checking'


def test_driver_empty_report_does_not_convoy_independent_owner_or_release_true_conflict(tmp_path):
    async def scenario():
        driver, task, identity, calls = fixture(tmp_path)
        from trellis.dispatch.coordination import Coordinator
        coordinator = Coordinator(driver)
        owner_started = asyncio.Event(); dependent_started = asyncio.Event()
        driver._acceptance_footprint = lambda i, _: Footprint(frozenset(['A'] if i == 4 else ['H']), frozenset([str(i)]))
        async def integrate(i, _):
            (dependent_started if i == 4 else owner_started).set()
            async with driver.finalization_gate:
                calls.append(dict(action='positive_publication', task=i))
        driver._integrate_reserved = integrate
        async with driver.acceptance_queue.reserve(1, Footprint(frozenset(['H']), frozenset(['A']))):
            async with driver.finalization_gate:
                report = coordinator._start(driver.integrate(2, identity), handoff=(2, 'checking'))
                # The actual report commits while the positive finalizer owns
                # its long gate; no mathematical reservation is taken.
                await asyncio.wait_for(report, 2)
                assert coordinator._handoffs_ready(), 'report delivery must not hold retry preparation behind a finalizer'
                positive = asyncio.create_task(driver.integrate(3, 'positive'))
                dependent = asyncio.create_task(driver.integrate(4, 'dependent'))
                await asyncio.wait_for(owner_started.wait(), 2)
                assert not dependent_started.is_set()
                assert 2 not in driver.acceptance_queue.active
                assert all(t[0] != 2 for t in driver.acceptance_queue.pending)
                assert task['status']['status'] == 'stopping'
            await asyncio.wait_for(report, 2)
            await asyncio.wait_for(positive, 2)
            assert not dependent_started.is_set()
        await asyncio.wait_for(dependent, 2)
        assert [c['task'] for c in calls if c['action'] == 'positive_publication'] == [3, 4]
    asyncio.run(scenario())


@pytest.mark.parametrize('raw', ['valid', 'stuck'])
def test_host_normalization_does_not_turn_valid_submission_into_worker_declared_stuck(raw):
    payload = dict(submitted_outcome=raw.title(), response=dict(status='Ok', outcome='Stuck'))
    failure_report.request_review(payload, dict(assignment=dict(task_kind='lean_closure')), dict(outcome=raw))
    assert payload['submitted_outcome'] == raw.title()
    assert bool(payload.get('closure_stuck_to_review')) == (raw == 'stuck')


@pytest.mark.parametrize('barrier', ['old_owner', 'old_finalizer', 'unknown', 'exclusive_seed'])
def test_retained_checks_keep_report_behind_gate(tmp_path, monkeypatch, barrier):
    from trellis.dispatch import acceptance_receipts
    monkeypatch.setattr(acceptance_receipts, 'implementation_identity', lambda: 'current')
    async def scenario():
        driver, task, identity, calls = fixture(tmp_path)
        state = driver.state()
        if barrier == 'exclusive_seed':
            state['dispatch']['exclusive_workers'] = {'3': {}}
        else:
            path = retained_check(driver, state)
            if barrier == 'old_finalizer':
                path = path.parent / 'finalization/1/canonical.json'
                path.parent.mkdir(parents=True)
            path.write_bytes(candidate.canonical_json(
                {} if barrier == 'unknown' else dict(key=dict(implementation_identity='old'))))
        driver.state = lambda: copy.deepcopy(state)
        async with driver.finalization_gate:
            report = asyncio.create_task(driver.integrate(2, identity))
            async def queued():
                while not driver.finalization_gate._waiters:
                    if report.done(): await report; raise AssertionError('lost compatibility fence')
                    await asyncio.sleep(.001)
            await asyncio.wait_for(queued(), 2)
            assert not any(c['action'] == 'commit_failure' for c in calls)
        await asyncio.wait_for(report, 2)
        assert sum(c['action'] == 'commit_failure' for c in calls) == 1
    asyncio.run(scenario())


def retained_check(driver, state, *, raw='valid'):
    from trellis.dispatch import acceptance_receipts
    task = dict(id=3, assignment=dict(compiler='managed', task_kind='ordinary', scope=dict(shared_writes=[], exclusive=True)))
    before = candidate.source_manifest(driver.repo)
    sealed = driver._submission_directory(task) / 'candidate'
    identity = candidate.seal(driver.repo, sealed, task_id=3, base_manifest=before, inputs=before,
                              response=dict(outcome=raw))
    task['status'] = dict(status='checking', candidate_sha256=identity)
    state['dispatch']['tasks']['3'] = task
    path = driver.root / '3/acceptance/1/canonical.json'; path.parent.mkdir(parents=True)
    private = dict(phase='ProofFormalization', stage='Worker', worker_execution={'task_id': 3},
                   live={}, in_flight_request=dict(kind='Worker', acceptance_logic_identity='current',
                   worker_acceptance=dict(validation_kind='ProofLocal')))
    path.write_bytes(candidate.canonical_json(dict(key=dict(implementation_identity='current'),
        report_reconciliation=failure_report.reconciliation_capture(private))))
    (path.parent / 'runtime').mkdir()
    (path.parent / 'runtime/protocol_state.json').write_bytes(candidate.canonical_json(private))
    return path


def test_current_checks_allow_report_but_corrupt_host_record_does_not(tmp_path, monkeypatch):
    from trellis.dispatch import acceptance_receipts
    monkeypatch.setattr(acceptance_receipts, 'implementation_identity', lambda: 'current')
    driver, _, _, _ = fixture(tmp_path)
    state = driver.state(); path = retained_check(driver, state)
    # No full-state read is needed once the host capture exists.
    (path.parent / 'runtime/protocol_state.json').write_text('not loaded')
    assert failure_report._concurrent(driver, state)
    path.write_text('{')
    with pytest.raises(ValueError): failure_report._concurrent(driver, state)


@pytest.mark.parametrize('raw', ['stuck', 'needs_restructure', 'target_false_under_model', 'invalid'])
def test_other_retained_outcomes_keep_gate(tmp_path, monkeypatch, raw):
    from trellis.dispatch import acceptance_receipts
    monkeypatch.setattr(acceptance_receipts, 'implementation_identity', lambda: 'current')
    driver, _, _, _ = fixture(tmp_path)
    state = driver.state(); retained_check(driver, state, raw=raw)
    assert not failure_report._concurrent(driver, state)


@pytest.mark.parametrize('change', ['source_kind', 'assumption', 'producer', 'candidate', 'shared'])
def test_retained_context_guards(tmp_path, monkeypatch, change):
    from trellis.dispatch import acceptance_receipts
    import json
    monkeypatch.setattr(acceptance_receipts, 'implementation_identity', lambda: 'current')
    driver, _, _, _ = fixture(tmp_path)
    state = driver.state(); path = retained_check(driver, state)
    private_path = path.parent / 'runtime/protocol_state.json'
    private = json.loads(private_path.read_text())
    if change == 'source_kind': private['in_flight_request']['worker_acceptance']['validation_kind'] = 'ProofCoarseRestructure'
    if change == 'assumption': private['in_flight_request']['assumption_authoring'] = {}
    if change == 'producer': private['in_flight_request']['acceptance_logic_identity'] = 'old'
    if change == 'shared': state['dispatch']['tasks']['3']['assignment']['scope']['shared_writes'] = ['reference/pin']
    if change == 'candidate':
        state['dispatch']['tasks']['3']['status']['candidate_sha256'] = 'different'
        with pytest.raises(candidate.CandidateError): failure_report._concurrent(driver, state)
        return
    capture = json.loads(path.read_text())
    capture['report_reconciliation'] = failure_report.reconciliation_capture(private)
    path.write_bytes(candidate.canonical_json(capture))
    assert not failure_report._concurrent(driver, state)


def test_ordinary_capture_requires_actual_exclusive_admission(tmp_path, monkeypatch):
    from trellis.dispatch import acceptance_receipts
    monkeypatch.setattr(acceptance_receipts, 'implementation_identity', lambda: 'current')
    driver, _, _, _ = fixture(tmp_path)
    state = driver.state(); path = retained_check(driver, state)
    state['dispatch']['tasks']['3']['assignment']['scope']['exclusive'] = False
    assert not failure_report._concurrent(driver, state)
    driver.acceptance_queue.active[3] = Footprint(frozenset(), frozenset(), exclusive=True)
    assert failure_report._concurrent(driver, state)
    driver.acceptance_queue.active.clear()
    captured = json.loads(path.read_text()); captured['staged_plan'] = {'status': 'planned'}
    path.write_bytes(candidate.canonical_json(captured))
    assert failure_report._concurrent(driver, state)
