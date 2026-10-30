import copy
from pathlib import Path

import pytest

from trellis.adapters import ProviderConfig
from trellis.runtime import bridge


def task(task_id=1):
    return dict(kind="Worker", phase="ProofFormalization", id=10, cycle=20,
                task_execution=dict(task_id=task_id, profile="luna-high", compiler="managed", task_kind="lean_closure",
                    binding=dict(provider="codex", model="gpt-6-luna", reasoning_effort="high",
                                 timeout_seconds=600, max_timeout_seconds=3600, allowed_tasks=["lean_closure"])))


def test_worker_sessions_are_isolated_by_attempt_with_stable_restart_identity():
    provider = ProviderConfig(provider="codex", model="gpt-6-luna", effort="high")
    a, b = task(1), task(2)
    assert bridge._session_scope(a, provider, "worker") != bridge._session_scope(b, provider, "worker")
    assert bridge._session_scope(a, provider, "worker") == bridge._session_scope(copy.deepcopy(a), provider, "worker")


def test_explicit_profile_cannot_be_overridden_by_legacy_ab_selection(monkeypatch):
    monkeypatch.setattr(bridge, "_maybe_apply_worker_model_ab", lambda **kw: pytest.fail("explicit profile overridden"))
    provider, decision = bridge._worker_execution_provider(task(), Path("."))
    assert (provider.model, provider.effort, decision) == ("gpt-6-luna", "high", None)


@pytest.mark.parametrize("seconds", [0, -1, True, "600", 3601])
def test_task_timeout_is_checked_against_launch_profile(seconds):
    request = task(); request["task_execution"]["binding"]["timeout_seconds"] = seconds
    with pytest.raises(bridge.BridgeError, match="timeout"):
        bridge._task_execution(request)


def test_worker_model_menu_cannot_select_verifier_membership():
    request = task(); request["kind"] = "Sound"
    with pytest.raises(bridge.BridgeError, match="worker request"):
        bridge._task_execution(request)


def test_tmux_sessions_do_not_collide_for_private_runtime_basenames(tmp_path):
    a = tmp_path / "run-runtime/dispatch/1/runtime"
    b = tmp_path / "run-runtime/dispatch/2/runtime"
    other = tmp_path / "other/run-runtime/dispatch/1/runtime"
    assert len({bridge._runtime_session_namespace(p) for p in (a, b, other)}) == 3
    assert bridge._runtime_session_namespace(a) == bridge._runtime_session_namespace(a)


def test_weak_profile_cannot_launch_ordinary_or_nl_only_work():
    request = task()
    request["task_execution"]["task_kind"] = "ordinary"
    with pytest.raises(bridge.BridgeError, match="not permitted"):
        bridge._task_execution(request)
    request = task()
    request["task_execution"]["compiler"] = "none"
    with pytest.raises(bridge.BridgeError, match="managed compiler"):
        bridge._task_execution(request)


def test_nl_only_prompt_gives_json_check_without_compiler_instructions(tmp_path):
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt
    contract = _worker_contract(authorized_nodes=['N'], phase='proof_formalization', active_node='N')
    contract['artifact_prompt_view']['json_check_command_template'].append('--raw-only')
    contract['prompt_fragments'].append('worker/proof_formalization/10_operational_guidance.md')
    acceptance = _worker_acceptance('proof_local', ['N'])
    prompt = build_worker_prompt(
        request=dict(project_invariants=_project_invariants(), phase='proof_formalization',
            active_node='N', mode='local', worker_contract=contract, worker_acceptance=acceptance,
            task_execution=dict(compiler='none', task_kind='ordinary')),
        worker_gate={'worker_acceptance': acceptance}, repo_path=tmp_path,
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json',
        reviewer_handoff_status_text='Reviewer notes unavailable: .trellis/scratch/plan.md')
    assert 'This task cannot compile' in prompt
    assert 'the host performs authoritative acceptance' in prompt
    assert '--raw-only' in prompt
    assert '.trellis/scripts/incremental-check' not in prompt
    assert str(tmp_path/'acceptance.json') not in prompt
    assert 'Reviewer notes unavailable: .trellis/scratch/plan.md' in prompt


def test_parallel_worker_prompt_shows_reserved_scope_and_helper_prefix(tmp_path):
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt
    contract = _worker_contract(authorized_nodes=['N'], phase='proof_formalization', active_node='N')
    acceptance = _worker_acceptance('proof_local', ['N'])
    prompt = build_worker_prompt(
        request=dict(project_invariants=_project_invariants(), phase='proof_formalization',
            active_node='N', mode='local', worker_contract=contract, worker_acceptance=acceptance,
            parallel_scope=dict(nodes=['N'], helper_prefixes=['P27_proof_'], shared_writes=[], exclusive=False)),
        worker_gate={'worker_acceptance': acceptance}, repo_path=tmp_path,
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json')
    assert 'P27_proof_' in prompt
    assert 'Report a needed scope change to the reviewer' in prompt


def test_worker_receives_complete_evidence_grant_matching_host_scope(tmp_path):
    import json
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.dispatch.candidate import CandidateError, check_scope
    from trellis.runtime.bridge_prompts import build_worker_prompt

    nodes = [f'Model{i}' for i in range(6)]
    paths = [f'reference/model-evidence/current-{i}.txt' for i in range(10)]
    scope = dict(nodes=nodes, helper_prefixes=['P116_T1_'], shared_writes=paths, exclusive=True)
    contract = _worker_contract(authorized_nodes=nodes, phase='proof_formalization', active_node=nodes[0])
    acceptance = _worker_acceptance('proof_local', nodes)
    request = dict(project_invariants=_project_invariants(), phase='proof_formalization',
        active_node=nodes[0], mode='local', worker_contract=contract, worker_acceptance=acceptance,
        parallel_scope=scope, reviewer_comments='Also create P116_T1_unreserved.txt.')
    original = copy.deepcopy(request)
    prompt = build_worker_prompt(request=request, worker_gate={'worker_acceptance': acceptance},
        repo_path=tmp_path, raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json')
    section = prompt.split('## Assigned edit scope', 1)[1]
    delivered = json.loads(section.split('```json\n', 1)[1].split('\n```', 1)[0])
    assert delivered == scope  # Every node and all ten exact paths survive rendering.
    assert request == original
    assert 'Evidence files require an exact `shared_writes` entry in an exclusive assignment.' in section
    assert 'Reviewer comments and node prefixes do not grant additional paths' in section
    assert 'request a revised assignment if needed evidence paths are missing' in section

    def check(paths, *, exclusive=delivered['exclusive']):
        check_scope(dict(changes=dict.fromkeys(paths), base_manifest={}),
            nodes=delivered['nodes'], helper_prefixes=delivered['helper_prefixes'],
            shared_paths=delivered['shared_writes'], exclusive=exclusive)

    check(paths)
    check(['Tablet/P116_T1_Helper.lean'])
    with pytest.raises(CandidateError, match='unreserved source'):
        check(['reference/model-evidence/P116_T1_unreserved.txt'])
    with pytest.raises(CandidateError, match='unreserved source'):
        check([paths[-1]], exclusive=False)


@pytest.mark.parametrize('kind,compiler', [('lean_closure', 'managed'), ('ordinary', 'managed'), ('ordinary', 'none')])
def test_parallel_retry_prompt_recovers_report_and_source_for_every_capability(tmp_path, kind, compiler):
    import json
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt

    contract = _worker_contract(authorized_nodes=['N'], phase='proof_formalization', active_node='N')
    contract['prompt_fragments'].append('worker/common/31_last_invalid.md')
    acceptance = _worker_acceptance('proof_local', ['N'])
    request = dict(project_invariants=_project_invariants(), phase='proof_formalization',
        active_node='N', mode='local', worker_contract=contract, worker_acceptance=acceptance,
        parallel_scope=dict(nodes=['N'], helper_prefixes=[], shared_writes=[], exclusive=False),
        task_execution=dict(compiler=compiler, task_kind=kind))
    original = copy.deepcopy(request)
    kwargs = dict(request=request, worker_gate={'worker_acceptance': acceptance}, repo_path=tmp_path,
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json')
    assert '## Previous attempt for this task' not in build_worker_prompt(**kwargs)
    saved = tmp_path/'last_invalid/Tablet/N.lean'
    saved.parent.mkdir(parents=True); saved.write_text('prior proof')
    (tmp_path/'last_invalid/metadata.json').write_text(json.dumps(dict(
        previous_task='7', request_id=786, outcome='Invalid', invalid_kind='checker_rejected',
        summary='Closed the assigned node.', comments='Final certification failed on another node.',
        deterministic_rejection_reasons=['checker identity unavailable'])))
    prompt = build_worker_prompt(**kwargs)
    assert 'Before solving this task again, inspect the previous worker' in prompt
    assert 'Closed the assigned node.' in prompt
    assert 'Final certification failed on another node.' in prompt
    assert 'checker identity unavailable' in prompt
    assert '"outcome": "Invalid"' in prompt
    assert str(saved) in prompt and str(tmp_path/'last_invalid/metadata.json') in prompt
    assert 'not an acceptance result' in prompt
    assert 'does not expand your edit scope' in prompt
    assert '.trellis-history/worker_state/last_invalid' not in prompt
    assert request == original


def test_parallel_retry_prompt_bounds_report_and_handles_missing_metadata(tmp_path):
    import json
    from trellis.runtime.bridge_prompts import _parallel_retry_prompt_context
    root = tmp_path/'last_invalid'; root.mkdir()
    context = _parallel_retry_prompt_context(tmp_path, {})
    assert 'metadata is unavailable' in context['parallel_retry_summary']
    (root/'metadata.json').write_text(json.dumps(dict(
        summary='s'*30000, comments='c'*30000, deterministic_rejection_reasons=['r'*8000]*20)))
    context = _parallel_retry_prompt_context(tmp_path, {})
    assert len(context['parallel_retry_summary']) < 10000
    assert '[see last_invalid/metadata.json]' in context['parallel_retry_summary']
    assert 'more_rejection_reasons' in context['parallel_retry_summary']


def test_parallel_reviewer_prompt_replaces_sidecar_and_checks_the_envelope(tmp_path):
    from test_runtime_bridge import _project_invariants, _review_contract
    from trellis.dispatch.config import DEFAULTS
    from trellis.runtime.bridge_prompts import build_review_prompt
    contract = _review_contract(blocker_choices=[])
    contract['prompt_fragments'].insert(2, 'review/common/30g_sidecar_grunts.md')
    from trellis.dispatch.closure_history import by_node
    history = by_node({'1': dict(id=1, launched_at_ms=1,
        binding=dict(provider='openai', model='historical-model'),
        status=dict(status='resolved', outcome=dict(kind='stale')),
        assignment=dict(profile='retired-profile', task_kind='lean_closure', scope=dict(nodes=['PriorNode'])))})
    prompt = build_review_prompt(
        request=dict(project_invariants=_project_invariants(), phase='theorem_stating',
            review_contract=contract, parallel_dispatch=dict(profiles=DEFAULTS['profiles'],
                available_slots=4, compiler_slots=2, verifier_slots=8, helper_prefix='P27_<assignment-id>_',
                closure_attempts_by_node=history, closure_candidates_without_recorded_launches=['FreshNode'])),
        repo_path=tmp_path, runtime_root=tmp_path/'runtime', raw_output_path=tmp_path/'result.json',
        done_path=tmp_path/'done', context_json_path=tmp_path/'context.json')
    assert 'trellis.dispatch.review --context' in prompt
    assert 'grunt_add' not in prompt
    assert 'sol-xhigh' in prompt and 'luna-high' in prompt
    # Aggregate prompt previews may elide nested values; the complete request
    # lookup stays in the delivered prose and the full rendered context is saved.
    full_path = (tmp_path/'result.json').with_suffix('.full-prompt.txt')
    full_prompt = full_path.read_text() if full_path.exists() else prompt
    assert 'profile_token_cost_comparison' in full_prompt and '15.42' in full_prompt
    assert 'Prefer inexpensive profiles for speculative attempts' not in prompt
    assert 'historical-model' in full_prompt and 'PriorNode' in full_prompt and 'FreshNode' in full_prompt
    assert '"native_outcomes": {' in full_prompt and '"stale": 1' in full_prompt
    assert '.parallel_dispatch.closure_attempts_by_node["NodeId"]' in prompt
    assert str(tmp_path/'result.request.json') in prompt
    assert 'Keep total worker slots fully utilized' in prompt
    assert 'high confidence' in prompt and 'current reservations and dependencies' in prompt
    assert 'closing every Lean proof is not required' in prompt
    assert 'These tasks cannot plan, decompose' in prompt
    assert 'allow_new_obligations: false' in prompt
    assert 'must_close_active: true' in prompt
    assert 'authorized_node_ids: []' in prompt
    assert "name that path in the assignment's `review.comments`" in prompt
    assert "assignment's `reviewer_comments`" not in prompt


def test_parallel_reviewer_advertised_command_runs_from_burst_without_pythonpath(tmp_path):
    import json
    import os
    import shlex
    import subprocess
    from test_runtime_bridge import _project_invariants, _review_contract
    from trellis.dispatch.config import DEFAULTS
    from trellis.runtime.bridge_prompts import build_review_prompt

    repo = tmp_path / "review burst 'quoted'"
    source = repo / '.trellis/runtime/src'
    source.mkdir(parents=True)
    (source / 'trellis').symlink_to(Path(__file__).resolve().parents[1] / 'trellis', target_is_directory=True)
    context_path, raw_path = repo / 'context.json', repo / 'result.json'
    request = dict(project_invariants=_project_invariants(), phase='theorem_stating',
                   review_contract=_review_contract(blocker_choices=[]),
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles']))
    context_path.write_text(json.dumps(request))
    prompt = build_review_prompt(request=request, repo_path=repo, runtime_root=repo/'runtime',
        raw_output_path=raw_path, done_path=repo/'done', context_json_path=context_path)
    delivery = json.loads(prompt.split('## Kernel-authored artifact delivery contract')[1]
                          .split('```json\n')[1].split('\n```')[0])
    command = shlex.split(delivery['json_check_command'])
    env = dict(os.environ, PYTHONPATH=str(tmp_path/'missing-source'), PYTHONDONTWRITEBYTECODE='1')
    for raw, valid in [(dict(assignments=[], reason='Waiting for the active task'), True),
                       (dict(decision='continue', reason='ordinary fields at top level'), False)]:
        raw_path.write_text(json.dumps(raw))
        result = subprocess.run(command, cwd=repo, env=env, text=True, capture_output=True, timeout=30)
        assert result.returncode == (0 if valid else 1), result.stderr
        assert json.loads(result.stdout)['ok'] is valid


@pytest.mark.parametrize('parallel', [False, True])
@pytest.mark.parametrize('pointer', ['shared/91_structured_request_pointer.md',
                                    'pv/shared/91_structured_request_pointer.md'])
def test_reviewer_schema_references_distinguish_parallel_envelope(tmp_path, parallel, pointer):
    import json
    from test_runtime_bridge import _project_invariants, _review_contract
    from trellis.dispatch.config import DEFAULTS
    from trellis.runtime.bridge_prompts import build_review_prompt

    contract = _review_contract(blocker_choices=[])
    contract['prompt_fragments'].append(pointer)
    request = dict(project_invariants=_project_invariants(), phase='theorem_stating', review_contract=contract)
    if parallel:
        request['parallel_dispatch'] = dict(profiles=DEFAULTS['profiles'])
    original = copy.deepcopy(request)
    prompt = build_review_prompt(request=request, repo_path=tmp_path, runtime_root=tmp_path/'runtime',
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        context_json_path=tmp_path/'context.json')
    delivery = json.loads(prompt.split('## Kernel-authored artifact delivery contract')[1]
                          .split('```json\n')[1].split('\n```')[0])
    schema_path = '.review_contract.artifact_contract.prompt_schema_example'
    if parallel:
        assert delivery['required_json_shape_ref'].startswith('Parallel assignments above;')
        assert f'inner `review` or `action` schema `{schema_path}`' in prompt
        assert "The ordinary response schema below applies to each assignment's `review` or the envelope's `action`." in prompt
        assert 'Return `{"assignments": [], "reason": "..."}` when no useful work' in prompt
        assert 'required_json_shape' not in delivery
    else:
        assert delivery['required_json_shape_ref'] == 'artifact_contract.prompt_schema_example in the role contract above'
        assert f'response schema `{schema_path}`.' in prompt
        assert 'Parallel assignments' not in prompt
    assert request == original


@pytest.mark.parametrize('pv', [False, True])
@pytest.mark.parametrize('closure', [False, True])
def test_reviewer_scope_advice_respects_worker_outcome_permission(tmp_path, closure, pv):
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt

    contract = _worker_contract(authorized_nodes=['N'], phase='proof_formalization', active_node='N',
        reviewer_comments='Repair the statement and introduce a supporting helper.')
    if closure:
        contract['allowed_outcomes'] = ['valid', 'invalid', 'stuck']
        contract['needs_restructure_contract']['allowed'] = False
        contract['prompt_schema_example']['outcome'] = 'valid / invalid / stuck'
    contract['prompt_fragments'] += ['worker/proof_formalization/05_scope_local.md',
                                    'worker/proof_formalization/20_helper_decomposition.md',
                                    'shared/25_filespec.md']
    acceptance = _worker_acceptance('proof_local', ['N'])
    request = dict(project_invariants=_project_invariants(), phase='proof_formalization',
        active_node='N', mode='local', worker_contract=contract, worker_acceptance=acceptance,
        task_execution=dict(compiler='managed', task_kind='lean_closure' if closure else 'ordinary'))
    original = copy.deepcopy(request)
    # Existing runs retain their repository FILESPEC across runtime upgrades.
    # Task130 cited this generic advice to distinguish adding a new option
    # from changing an existing one. The task prompt must settle both cases.
    old_filespec = (
        'A standalone `set_option maxHeartbeats N` above the TABLET NODE marker '
        'is outside the hashed signature and can be retuned later.\n'
    )
    (tmp_path / 'FILESPEC.md').write_text(old_filespec)
    if pv:
        (tmp_path / 'modeling-input').mkdir()
        (tmp_path / 'modeling-input/manifest.json').write_text('{}\n')
    prompt = build_worker_prompt(request=request, worker_gate={'worker_acceptance': acceptance},
        repo_path=tmp_path, raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json')
    advice = prompt.split('## Reviewer comments', 1)[1].split('\n## ', 1)[0]
    assert 'Repair the statement and introduce a supporting helper.' in advice
    assert request == original
    assert (tmp_path / 'FILESPEC.md').read_text() == old_filespec
    if closure:
        assert 'Preserve the existing preamble options for this closure-only task.' in prompt
        assert 'Do not add, remove, or change any preamble option' in prompt
        assert 'introducing a preamble `set_option maxHeartbeats` where none existed is forbidden' in prompt
        assert 'A term-local `set_option maxHeartbeats N in ...` wholly below `-- BODY` is allowed' in prompt
        assert 'the preamble restriction does not ban proof-body option expressions' in prompt
        assert 'Generic FILESPEC heartbeat-placement advice does not grant this task permission' in prompt
        assert 'Placement outside the hashed statement does not permit the edit.' in prompt
        assert 'standalone `set_option maxHeartbeats N` in the preamble' not in prompt
        assert 'local proof steps and intermediate facts inside the allowed body' in advice
        assert 'return `needs_restructure`; otherwise return `stuck`' not in advice
        assert 'after sustained implementation and checking, report `stuck`' in prompt
        assert 'Preserve the `-- BODY` marker, existing imports, preamble options, statement, TeX, and every other node' in prompt
        assert '`local` authorizes edits' not in prompt
        assert '## Interpreting theorem-stating mode' not in prompt
        assert '## Helper decomposition' not in prompt
        assert 'return `needs_restructure` instead of forcing' not in prompt
    else:
        assert ('standalone `set_option maxHeartbeats N` in the preamble' in prompt) is (not pv)
        assert 'Preserve the existing preamble options for this closure-only task.' not in prompt
        assert 'Do not add, remove, or change any preamble option' not in prompt
        assert 'the preamble restriction does not ban proof-body option expressions' not in prompt
        assert 'If the contract allows `needs_restructure` and you can name a specific broader repair' in advice
        assert 'return `needs_restructure`; otherwise return `stuck`' in advice
        assert contract['needs_restructure_contract']['allowed'] is True
        assert 'needs_restructure' in contract['allowed_outcomes']
        assert 'Use `needs_restructure` only when the contract allows it' in prompt
        assert '`local` authorizes edits' in prompt


@pytest.mark.parametrize('model', ['gpt-6-luna', 'gpt-6.1-sol'])
@pytest.mark.parametrize('remaining', [None, 1195240])
def test_closure_prompt_delivers_sustained_work_and_actual_allowance(tmp_path, model, remaining):
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt
    contract = _worker_contract(authorized_nodes=['N'], phase='proof_formalization', active_node='N')
    acceptance = _worker_acceptance('proof_local', ['N'])
    request = dict(project_invariants=_project_invariants(), phase='proof_formalization',
        active_node='N', mode='local', worker_contract=contract, worker_acceptance=acceptance,
        task_execution=dict(compiler='managed', task_kind='lean_closure',
            binding=dict(model=model, timeout_seconds=3600)))
    before = copy.deepcopy(request)
    prompt = build_worker_prompt(request=request, worker_gate={'worker_acceptance': acceptance},
        repo_path=tmp_path, raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json', remaining_allowance_ms=remaining)
    assert prompt.startswith('Close the assigned Lean theorem.')
    seconds = (remaining if remaining is not None else 3600000) / 1000
    assert f'{seconds:g} seconds' in prompt
    assert 'Turn the mathematical argument into a concrete Lean proof attempt early' in prompt
    assert 'local intermediate facts proved with `have`' in prompt
    assert 'Read the resulting goal or error, revise the proof, and check again' in prompt
    assert 'near the end of the allowance after sustained implementation and checking' in prompt
    assert 'deleted_nodes: []' in prompt
    assert 'no advisory Lean check is required' not in prompt
    assert 'needed scope change' not in prompt
    assert request == before


@pytest.mark.parametrize('pv', [False, True])
@pytest.mark.parametrize('phase', ['proof_formalization', 'theorem_stating'])
@pytest.mark.parametrize('capability', ['closure', 'nl'])
def test_restricted_prompt_drops_incompatible_phase_and_authoring_guidance(tmp_path, phase, capability, pv):
    from test_runtime_bridge import _project_invariants, _worker_contract, _worker_acceptance
    from trellis.runtime.bridge_prompts import build_worker_prompt
    contract = _worker_contract(authorized_nodes=['N'], phase=phase, active_node='N')
    contract['prompt_fragments'] += [
        'worker/proof_formalization/05_scope_local.md',
        'worker/proof_formalization/10_operational_guidance.md',
        'worker/proof_formalization/20_helper_decomposition.md',
        'worker/theorem_stating/05_after_soundness_review.md',
        'worker/theorem_stating/10_mode_guidance.md',
        'worker/common/21_deviations.md', 'worker/common/38_new_node_difficulty.md',
        'worker/common/45_outcomes.md', 'worker/common/46_audit_request.md',
        'worker/common/31_last_invalid.md', 'worker/common/31_scratchpad.md',
    ]
    if pv:
        contract['prompt_fragments'] += [
            'pv/common/18_source_of_truth.md', 'pv/canonical/SOUNDNESS.md',
            'pv/canonical/SUBSTANTIVENESS.md', 'pv/canonical/EXTRACTION_MODEL.md',
            'pv/canonical/FAITHFULNESS.md', 'pv/worker/common/71_target_false_trust_v1.md',
        ]
    acceptance = _worker_acceptance('proof_local', ['N'])
    closure = capability == 'closure'
    if not closure:
        contract['artifact_prompt_view']['json_check_command_template'].append('--raw-only')
    prompt = build_worker_prompt(
        request=dict(project_invariants=_project_invariants(), phase=phase, active_node='N', mode='local',
            worker_contract=contract, worker_acceptance=acceptance,
            task_execution=dict(compiler='managed' if closure else 'none',
                                task_kind='lean_closure' if closure else 'ordinary')),
        worker_gate={'worker_acceptance': acceptance}, repo_path=tmp_path,
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done',
        acceptance_context_path=tmp_path/'acceptance.json')
    for heading in ['Proof-formalization operating pattern', 'Helper decomposition', 'Worker artifact field guidance']:
        assert heading not in prompt
    assert 'preserved across worker bursts regardless of outcome' not in prompt
    assert 'canonical handoff channel' not in prompt
    assert '.trellis-history/worker_state/last_invalid' not in prompt
    assert 'Run `json_check_command`' in prompt
    assert 'create `done_path`' in prompt
    if closure:
        assert 'proof body below `-- BODY`' in prompt
        assert '.trellis/scripts/incremental-check Tablet.NodeName' in prompt
        assert '## Worker outcome guidance' not in prompt
    else:
        assert 'You may read Lean files' in prompt
        assert '.trellis/scripts/incremental-check' not in prompt
        assert 'scratch is not automatically copied to a later task' in prompt
        assert '## Worker scratch workspace' not in prompt
        assert 'scratch/README.md' not in prompt
        if pv:
            assert '## Source of truth' in prompt
            assert 'EXTRACTION_MODEL.md' in prompt or 'Extraction-model' in prompt
            assert 'concrete witness' in prompt


@pytest.mark.parametrize('pv', [False, True])
@pytest.mark.parametrize('parallel', [False, True])
def test_reviewer_target_orientation_matches_dispatch_mode(tmp_path, pv, parallel):
    from test_runtime_bridge import _project_invariants, _review_contract
    from trellis.dispatch.config import DEFAULTS
    from trellis.runtime.bridge_prompts import build_review_prompt
    contract = _review_contract(blocker_choices=[])
    contract['prompt_fragments'].append(('pv/review/' if pv else 'review/common/') + '33b_theorem_target_orientation.md')
    request = dict(project_invariants=_project_invariants(), phase='theorem_stating', review_contract=contract)
    if parallel:
        request['parallel_dispatch'] = dict(profiles=DEFAULTS['profiles'], available_slots=4,
            compiler_slots=2, verifier_slots=8, helper_prefix='P27_<assignment-id>_')
    prompt = build_review_prompt(request=request, repo_path=tmp_path, runtime_root=tmp_path/'runtime',
        raw_output_path=tmp_path/'result.json', done_path=tmp_path/'done', context_json_path=tmp_path/'context.json')
    if parallel:
        assert 'well-decomposed while clearing verifier blockers' in prompt
        assert 'using free slots for compatible work' in prompt
    else:
        assert 'well-decomposed before any proofs are seriously attempted' in prompt


@pytest.mark.parametrize('valid', [True, False])
def test_advertised_reviewer_checker_validates_parallel_envelope_and_embedded_review(tmp_path, monkeypatch, capsys, valid):
    import json
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch.review import validate
    context = dict(phase='proof_formalization', current_present_nodes=['A'], current_deps={'A': []},
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    embedded = dict(decision='continue')
    raw = dict(assignments=[dict(id='left', profile='sol-xhigh', compiler='managed',
                                task_kind='ordinary', review=embedded)], reason='independent work')
    calls = []
    def normalize(payload, *, review_request, worker_assignment, assignment_task_kind):
        assert payload == embedded  # The old path incorrectly checked the outer envelope.
        assert review_request == context
        assert worker_assignment is True
        calls.append(payload)
        return {'response': {'decision': 'Continue'}} if valid else {'errors': ['missing next_active']}
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data', normalize)
    monkeypatch.setattr(checking, '_kernel_response', lambda *a, **kw:
                        dict(ok=True, errors=[], data={'errors': {}}))
    raw_path, context_path = tmp_path/'raw.json', tmp_path/'context.json'
    raw_path.write_text(json.dumps(raw)); context_path.write_text(json.dumps(context))
    expected = validate(raw, context)
    actual = checking.validate_json_artifact('trellis-reviewer-result', raw_path, context_json=context_path, repo=tmp_path)
    assert actual == expected and actual['ok'] is valid
    assert checking._artifact_main('trellis-reviewer-result', [str(raw_path), '--context-json', str(context_path), '--repo', str(tmp_path)]) == (0 if valid else 1)
    output = capsys.readouterr().out
    assert ('OK:' if valid else 'left: missing next_active') in output
    assert len(calls) == 3


def test_parallel_assignment_rejection_preserves_peer_and_action_uses_ordinary_check(monkeypatch):
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch.review import normalize, validate
    context = dict(phase='proof_formalization', current_present_nodes=['A'], current_deps={'A': []},
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    bad = dict(decision='continue', dismissed_tasks=[dict(id='task-1', reason='Completed')])
    good = dict(decision='continue')
    calls = []

    def kernel(payload, *, expected_status):
        if payload['action'] == 'check_dispatch_scopes':
            assert [item['id'] for item in payload['input']['assignments']] == ['good']
            return dict(ok=True, errors=[], data={'errors': {}})
        assert payload['action'] == 'check_trellis_reviewer_result'
        calls.append(payload)
        if payload['worker_assignment'] and payload['raw_payload'].get('dismissed_tasks'):
            return dict(ok=False, errors=['assignment review cannot include dismissed_tasks'], data=None)
        return dict(ok=True, errors=[], data=dict(response=payload['raw_payload']))

    monkeypatch.setattr(checking, '_kernel_response', kernel)
    raw = dict(assignments=[dict(id=id, profile='sol-xhigh', compiler='managed', task_kind='ordinary', review=review)
                            for id, review in [('bad', bad), ('good', good)]])
    checked = validate(raw, context)
    assert checked['ok'] is False
    assert checked['errors'] == ['bad: assignment review cannot include dismissed_tasks']
    # The bridge retains the same per-item result for a native partial receipt.
    delivered = normalize(raw, context)
    assert delivered == checked['data']
    assert delivered['assignments'][0]['preparation_error']
    assert delivered['assignments'][1]['preparation_error'] is None
    assert delivered['assignments'][1]['review'] == good
    action = validate(dict(action=bad), context)
    assert action['ok'] and action['data']['action'] == bad
    assert calls[-1]['worker_assignment'] is False


@pytest.mark.parametrize('envelope', ['action', 'assignment', 'verifier_action'])
def test_retirement_worker_route_agrees_across_bridge_and_both_selfchecks(tmp_path, monkeypatch, capsys, envelope):
    import json
    import sys
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch import review
    context = dict(phase='proof_formalization', current_present_nodes=['Old', 'Consumer'],
                   current_deps={'Old': [], 'Consumer': ['Old']},
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    response = dict(decision='Continue', dispatch_node_retirement=envelope != 'verifier_action',
                    next_mode='Restructure', next_active='Consumer', next_active_coarse='ActiveGoal',
                    authorized_nodes=['Old', 'Consumer'])
    # Ordinary normalization and native scope checking remain separate authorities.
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data',
                        lambda raw, **kw: dict(response=raw))
    monkeypatch.setattr(checking, '_kernel_response',
                        lambda *a, **kw: dict(ok=True, errors=[], data={'errors': {}}))
    raw = (dict(assignments=[dict(id='retire', profile='sol-xhigh', compiler='managed',
                                 task_kind='ordinary', review=response)])
           if envelope == 'assignment' else dict(assignments=[], action=response))
    raw_path, context_path = tmp_path / 'raw.json', tmp_path / 'context.json'
    raw_path.write_text(json.dumps(raw)); context_path.write_text(json.dumps(context))
    expected_ok = envelope != 'action'
    advertised = checking.validate_json_artifact('trellis-reviewer-result', raw_path,
                                                context_json=context_path, repo=tmp_path)
    assert advertised['ok'] is expected_ok
    monkeypatch.setattr(sys, 'argv', ['review', '--context', str(context_path), '--raw', str(raw_path)])
    assert review.main() == (0 if expected_ok else 1)
    assert json.loads(capsys.readouterr().out)['ok'] is expected_ok
    if expected_ok:
        normalized = review.normalize(raw, context)
        assert normalized == advertised['data']
        if envelope == 'assignment':
            assert normalized['assignments'][0]['review'] == response
            assert normalized['assignments'][0]['preparation_error'] is None
    else:
        assert 'ordinary managed assignment review, not action' in advertised['errors'][0]
        with pytest.raises(ValueError, match='node retirement dispatches a worker'):
            review.normalize(raw, context)


@pytest.mark.parametrize('envelope', ['action', 'assignment', 'unmanaged_assignment', 'audit_action'])
def test_assumption_authoring_route_agrees_across_bridge_and_both_selfchecks(tmp_path, monkeypatch, capsys, envelope):
    import json
    import sys
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch import review
    context = dict(phase='theorem_stating', current_present_nodes=['Assumptions'],
                   current_deps={'Assumptions': []},
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    response = dict(decision='Continue', reset='None', next_active=None, authorized_nodes=[],
                    assumption_authoring_request=envelope != 'audit_action')
    if envelope == 'audit_action':
        response['audit_request'] = dict(reason_kind='approach', reason='Assess the premise')
    # This regression tests the common envelope guard, not native premise eligibility.
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data',
                        lambda raw, **kw: dict(response=raw))
    monkeypatch.setattr(checking, '_kernel_response',
                        lambda *a, **kw: dict(ok=True, errors=[], data={'errors': {}}))
    raw = (dict(assignments=[dict(id='author', profile='sol-xhigh',
                                 compiler='none' if envelope == 'unmanaged_assignment' else 'managed',
                                 task_kind='ordinary', review=response)])
           if envelope in ('assignment', 'unmanaged_assignment') else dict(assignments=[], action=response))
    raw_path, context_path = tmp_path / 'raw.json', tmp_path / 'context.json'
    raw_path.write_text(json.dumps(raw)); context_path.write_text(json.dumps(context))
    expected_ok = envelope not in ('action', 'unmanaged_assignment')
    advertised = checking.validate_json_artifact('trellis-reviewer-result', raw_path,
                                                context_json=context_path, repo=tmp_path)
    assert advertised['ok'] is expected_ok
    monkeypatch.setattr(sys, 'argv', ['review', '--context', str(context_path), '--raw', str(raw_path)])
    assert review.main() == (0 if expected_ok else 1)
    assert json.loads(capsys.readouterr().out)['ok'] is expected_ok
    if expected_ok:
        assert review.normalize(raw, context) == advertised['data']
    elif envelope == 'unmanaged_assignment':
        assert 'compiler=managed' in advertised['errors'][0]
        assert review.normalize(raw, context)['assignments'][0]['preparation_error']
    else:
        assert 'ordinary managed assignment review, not action' in advertised['errors'][0]
        with pytest.raises(ValueError, match='assumption authoring dispatches a worker'):
            review.normalize(raw, context)


def test_frozen_scope_feedback_is_shared_by_advertised_checks_but_never_changes_delivery(tmp_path, monkeypatch, capsys):
    import json
    import sys
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch import review
    occupied = dict(id=17, status={'status': 'running'}, scope=dict(nodes=['Ancestor'],
        helper_prefixes=[], shared_writes=[], exclusive=False), report={'large': 'omit'})
    context = dict(phase='proof_formalization', active_node='Ancestor',
        current_present_nodes=['Ancestor', 'Helper', 'Independent'],
        current_deps={'Ancestor': ['Helper'], 'Helper': [], 'Independent': []},
        parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[occupied]),
        large_contract={'do_not_copy': 'unrelated context'})
    raw = dict(assignments=[dict(id=id, profile='sol-xhigh', compiler='managed', task_kind='ordinary',
        review=dict(next_active='Ancestor', next_mode='Restructure', authorized_nodes=[node]))
        for id, node in [('repair', 'Helper'), ('independent', 'Independent')]])
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data',
        lambda payload, **kw: dict(response=payload))
    calls = []
    def kernel(payload, *, expected_status):
        assert expected_status == 'dispatch_ok'
        assert payload['action'] == 'check_dispatch_scopes'
        assert 'large_contract' not in payload['input']
        assert 'report' not in payload['input']['tasks'][0]
        assert len(payload['input']['assignments']) == 2
        calls.append(payload)
        return dict(ok=True, errors=[], data={'errors': {
            'repair': 'authorized edit scope conflicts with task 17'}})
    monkeypatch.setattr(checking, '_kernel_response', kernel)
    raw_path, context_path = tmp_path/'raw.json', tmp_path/'context.json'
    raw_path.write_text(json.dumps(raw)); context_path.write_text(json.dumps(context))
    checked = review.validate(raw, context)
    assert checked['errors'] == ['repair: authorized edit scope conflicts with task 17']
    assert not checked['ok']
    assert checking._artifact_main('trellis-reviewer-result', [str(raw_path), '--context-json', str(context_path)]) == 1
    monkeypatch.setattr(sys, 'argv', ['review', '--raw', str(raw_path), '--context', str(context_path)])
    assert review.main() == 1
    assert len(calls) == 3  # One compact batch preflight per advertised check.
    before = len(calls)
    delivered = review.normalize(raw, context)
    assert delivered == checked['data'] and len(calls) == before
    assert all(item['preparation_error'] is None for item in delivered['assignments'])
    # Current admission, after task17 resolves or a peer fails, sees the intact batch.
    assert len(delivered['assignments']) == 2
    assert 'authorized edit scope conflicts with task 17' in capsys.readouterr().out


def test_missing_frozen_scope_context_is_not_guessed(monkeypatch):
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch.review import validate, normalize
    context = dict(parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    raw = dict(assignments=[dict(id='one', profile='sol-xhigh', compiler='managed', task_kind='ordinary', review={})])
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data', lambda *a, **kw: dict(response={}))
    monkeypatch.setattr(checking, '_kernel_response', lambda *a, **kw: pytest.fail('missing context must not invoke native preflight'))
    assert 'requires phase, current_present_nodes, current_deps and parallel_dispatch.tasks' in validate(raw, context)['errors'][0]
    assert normalize(raw, context)['assignments'][0]['preparation_error'] is None


@pytest.mark.parametrize('paths,changes,error', [
    (['reference/model-evidence/new-probe.rs'], {}, None),
    ([], {}, None),
    (['reference/model-evidence/new-probe.rs'], {'compiler': 'none'}, 'compiler=managed'),
    (['reference/model-evidence/new-probe.rs'], {'phase': 'proof_formalization'}, 'ordinary Modeling'),
    (['reference/model-evidence/new-probe.rs'], {'model_preservation_enabled': False}, 'model preservation'),
    (['reference/model-evidence/../GOAL.md'], {}, 'normalized file paths'),
    (['reference/model-evidence/a//b'], {}, 'normalized file paths'),
    (['reference/model-evidence/a', 'reference/model-evidence/a'], {}, 'repeat'),
    (['reference/APPROVED_AXIOMS.json'], {}, 'normalized file paths'),
    (['reference/model-evidence/' + str(n) for n in range(65)], {}, 'at most 64'),
])
def test_parallel_evidence_contract_is_exact_and_modeling_only(paths, changes, error):
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch.review import normalize
    context = dict(phase='modeling', model_preservation_enabled=True,
                   parallel_dispatch=dict(profiles=DEFAULTS['profiles']))
    item = dict(id='evidence', profile='sol-xhigh', compiler='managed', task_kind='ordinary',
                review={}, evidence_paths=paths)
    for key, value in changes.items():
        (item if key == 'compiler' else context)[key] = value
    checked = normalize(dict(assignments=[item]), context,
                        normalize_review=lambda raw, **kw: dict(response=raw))['assignments'][0]
    if error:
        assert error in checked['preparation_error']
    else:
        assert checked['preparation_error'] is None
        if paths:
            assert checked['evidence_paths'] == paths
        else:
            assert 'evidence_paths' not in checked, 'old wire output remains unchanged'


def test_evidence_reviewer_preflight_carries_exact_exclusive_request(monkeypatch):
    from trellis import checking
    from trellis.dispatch.config import DEFAULTS
    from trellis.dispatch.review import validate
    path = 'reference/model-evidence/new.rs'
    context = dict(phase='modeling', model_preservation_enabled=True, current_present_nodes=['A'],
                   current_deps={'A': []}, parallel_dispatch=dict(profiles=DEFAULTS['profiles'], tasks=[]))
    monkeypatch.setattr(checking, 'normalize_trellis_reviewer_result_data', lambda raw, **kw: dict(response=raw))
    def check(payload, **kw):
        assert payload['input']['assignments'][0]['evidence_paths'] == [path]
        return dict(ok=True, errors=[], data=dict(errors={'evidence': 'scope conflicts with task 8'}))
    monkeypatch.setattr(checking, '_kernel_response', check)
    result = validate(dict(assignments=[dict(id='evidence', profile='sol-xhigh', compiler='managed',
        task_kind='ordinary', evidence_paths=[path], review={})]), context)
    assert result['errors'] == ['evidence: scope conflicts with task 8']
    assert result['data']['assignments'][0]['preparation_error'] is None
