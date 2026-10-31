from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.adapters import ProviderConfig
from trellis.agent_wrapper.continuation import session_binding
from trellis.agent_wrapper.protocol import AgentLane, ArtifactSpec, SingleAgentRequest
from trellis.agent_wrapper.worker_submission import prepare_turn


def single_request(root):
    return SingleAgentRequest(request_id='7', cycle=2, kind='worker', burst_role='worker',
        provider=ProviderConfig(provider='codex', model='test', effort='high'),
        prompt='original assignment', work_dir=root, state_dir=root / '.trellis',
        session_name='test', lane=AgentLane(kind='worker'), timeout_seconds=60,
        burst_home=root / 'home', artifact=ArtifactSpec('trellis_worker_7_submission-1.json', 'worker'))


def test_memory_feedback_resumes_same_thread_with_bound_checkin_and_remaining_allowance(tmp_path):
    from trellis.agent_wrapper.worker_submission import native_digest
    from test_checker_serial_recovery import pool, retry, stop
    state = retry(pool(tmp_path))
    stop(state); stop(state)
    details = str(state.outcome('resource_rejected', 'serial_own_cap_exhausted'))
    single = single_request(tmp_path)
    session = session_binding(single.provider, single.burst_home, 'original-thread')
    feedback = dict(response=dict(comments=details, deterministic_rejection_reasons=[]))
    contract = dict(turn=2, remaining_ms=12000, session=session, feedback=feedback,
                    feedback_sha256=native_digest(feedback), execution_record=str(tmp_path / 'clock'))
    result = prepare_turn(single, contract, tmp_path / 'runtime')
    assert result.resume_thread_id == 'original-thread'
    assert result.timeout_seconds == 12
    assert details in result.prompt
    assert 'sh .trellis/worker-checks/7.sh' in result.prompt
    assert (tmp_path / '.trellis/check-results/submission-1.txt').read_text() == details


@pytest.mark.parametrize('thread', ['', 'retained-thread'])
def test_first_submission_transport_resume_keeps_exact_conversation(tmp_path, thread):
    single = single_request(tmp_path)
    session = session_binding(single.provider, single.burst_home, thread) if thread else None
    transport = dict(ordinal=1, session=session)
    contract = dict(turn=1, remaining_ms=60000, session=None, transport_resume=transport,
                    execution_record=str(tmp_path / 'clock'))
    result = prepare_turn(single, contract, tmp_path / 'runtime')
    assert result.resume_thread_id == thread
    assert result.transport_resume == transport
    assert result.prompt == single.prompt
    assert result.timeout_seconds == 60  # The owned clock subtracts prior segments.


@pytest.mark.parametrize('change', [dict(model='other'), dict(home='/other'), dict(provider='other')])
def test_transport_resume_cannot_change_provider_binding(tmp_path, change):
    single = single_request(tmp_path)
    session = session_binding(single.provider, single.burst_home, 'retained-thread')
    contract = dict(turn=1, remaining_ms=60000, session=None,
        transport_resume=dict(ordinal=1, session=dict(session, **change)),
        execution_record=str(tmp_path / 'clock'))
    with pytest.raises(RuntimeError, match='binding'):
        prepare_turn(single, contract, tmp_path / 'runtime')


@pytest.mark.parametrize('generic', [False, True])
@pytest.mark.parametrize('closure', [False, True])
def test_first_submission_transport_resume_preserves_source_scratch_and_baseline(tmp_path, monkeypatch, generic, closure):
    from trellis.runtime import bridge
    source = tmp_path / 'Tablet/A.lean'
    source.parent.mkdir()
    source.write_text('retained partial proof')
    scratch = tmp_path / '.trellis/scratch/notes.md'
    scratch.parent.mkdir(parents=True)
    scratch.write_text('retained reasoning')
    runtime = tmp_path / 'runtime'
    private = bridge._bridge_private_state_dir(tmp_path, runtime)
    private.mkdir(parents=True)
    baseline = private / 'trellis_worker_7_result.acceptance.json'
    baseline.write_text('{"before_snapshot":{"original":"baseline"}}')
    original = baseline.read_bytes()
    request = dict(kind='worker', id=7, cycle=2, phase='theorem_stating', fresh_context=True,
        task_execution=dict(task_id=1, profile='sol-high', compiler='managed',
            task_kind='lean_closure' if closure else 'ordinary', binding=dict(provider='codex',
            model='gpt-6.1-sol', reasoning_effort='high', timeout_seconds=60,
            max_timeout_seconds=3600, allowed_tasks=['ordinary', 'lean_closure'])),
        worker_submission_policy=dict(version=1, max_submissions=3, max_infrastructure_retries=2))
    single = single_request(tmp_path)
    session = session_binding(single.provider, single.burst_home, 'retained-thread')
    submission = dict(turn=1, remaining_ms=60000, session=None,
        transport_resume=dict(ordinal=1, session=session), execution_record=str(tmp_path / 'clock'))
    if generic:
        submission.pop('transport_resume')
        submission.update(session=session, transport_ordinal=1, transport_continuation=True, retain_baseline=True)
    def forbidden(*args, **kwargs):
        pytest.fail('transport resume replaced the existing worker baseline or sources')
    monkeypatch.setattr(bridge, '_restore_active_worker_base_via_kernel', forbidden)
    monkeypatch.setattr(bridge, 'propagate_tablet_back_to_worker', forbidden)
    monkeypatch.setattr(bridge, 'build_trellis_worker_acceptance_context', forbidden)
    monkeypatch.setattr(bridge, 'sync_supervisor_workspace', lambda _: {'authoritative_repo_path': str(tmp_path)})
    monkeypatch.setattr(bridge, 'build_worker_prompt', lambda **_: 'original assignment')
    monkeypatch.setattr(bridge, '_policy', lambda _: None)
    monkeypatch.setattr(bridge, '_theorem_initial_dag_size_guidance', lambda _: '')
    monkeypatch.setattr(bridge, '_worker_execution_provider', lambda *_: (single.provider, None))
    monkeypatch.setattr(bridge, '_worker_burst_role', lambda _: 'worker')
    from dataclasses import replace
    monkeypatch.setattr(bridge, '_single_request_common', lambda **kw: replace(single, artifact=kw['artifact']))
    def execute(actual, **kwargs):
        assert actual.transport_resume == submission.get('transport_resume')
        assert actual.resume_thread_id == 'retained-thread'
        assert ('Continue concrete Lean proof development' in actual.prompt) == (generic and closure)
        if generic and closure:
            assert '60 seconds' in actual.prompt
        assert source.read_text() == 'retained partial proof'
        assert scratch.read_text() == 'retained reasoning'
        return SimpleNamespace(raw_path=None, payload={'outcome': 'Stuck'}, provider_execution={},
            provider_session_id='retained-thread', provider_turn_completed=True, error='')
    monkeypatch.setattr(bridge, 'execute_agent_request', execute)
    config = SimpleNamespace(repo_path=tmp_path, workflow=SimpleNamespace(paper_tex_path=None))
    response = bridge._handle_worker(config=config, runtime_root=runtime, request=request, submission=submission)
    assert response['kind'] == 'worker_submission', response
    import json
    assert json.loads(baseline.read_bytes()) == json.loads(original)
