"""Deterministic agent for the real native/checker/launcher integration test.

Only provider output is simulated. Source edits and incremental checks execute
inside the production compiler-confined worker sandbox; bridge acceptance is real.
"""
import contextlib
from dataclasses import replace
import json
from pathlib import Path
import subprocess
import sys

from trellis.runtime import bridge
from trellis.runtime.bridge_protocol import BridgeCliRequest
from trellis.agent_wrapper.protocol import SingleAgentResponse, PanelExecutionResponse
from trellis.sandbox import wrap_command

payload = json.load(sys.stdin)
retry_test = payload.pop('fixture_retry', False)
migration_test = payload.pop('fixture_migration', False)
coarse_test = payload.pop('fixture_coarse', False)
scoped_test = payload.pop('fixture_scoped', False)
submission_test = payload.pop('fixture_submission', False)
ordinary_refresh = payload.pop('fixture_ordinary_refresh', None)
request = payload['request']
kind = request['kind'].lower()
node = request.get('active_node')


original_home = bridge.seed_burst_home
original_execute = bridge.execute_agent_request

def home(runtime, role, **kwargs):
    if submission_test and kind == 'worker':
        return original_home(runtime, role, **kwargs)
    directory = Path(runtime) / 'fixture-home' / role
    directory.mkdir(parents=True, exist_ok=True)
    return directory


def execute(single, **kwargs):
    if kind == 'review':
        tasks = request['parallel_dispatch']['tasks']
        assigned = {n for t in tasks if not t['scope']['exclusive'] or t['status']['status'] != 'resolved'
                    for n in t['scope']['nodes']}
        assignments = []
        goals = request.get('coarse_goals') or {}
        audit_done = bool(goals.get('goals', {}).get('A', {}).get('history_head'))
        if scoped_test and not audit_done and not request['parallel_dispatch'].get('verifier_jobs') and not goals.get('pending_audits'):
            raw_path = single.work_dir / '.trellis/staging/fixture-review.json'
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            raw_path.write_text(json.dumps(dict(assignments=[], action=dict(decision='continue', reset='none',
                reason='Request scoped advice for A.', next_mode='local', allow_new_obligations=True, must_close_active=False,
                audit_request=dict(coarse_nodes=['A'], reason_kind='approach', reason='Check the direct proof route for A.')))))
            return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
                burst_role=single.burst_role, ok=True, raw_path=str(raw_path))
        for name in ('A', 'B'):
            if name in assigned or (scoped_test and name == 'A' and not audit_done):
                continue
            ordinary = coarse_test or (submission_test and name == 'A') or (ordinary_refresh and name == 'B')
            assignments.append(dict(id=name, profile='sol-xhigh' if ordinary else 'luna-high', compiler='managed', task_kind='ordinary' if ordinary else 'lean_closure',
                review=dict(decision='continue', reason='Close the independent fixture theorem.', comments='',
                    task_blocker_ids=[], override_blocker_ids=[], next_active=name, next_mode='restructure' if ordinary_refresh and name == 'B' else 'local', reset='none',
                    **(dict(next_active_coarse=name) if coarse_test or (submission_test and name == 'A')
                       or (ordinary_refresh and name == 'B') else {}),
                    **(dict(authorized_node_ids=['B', 'D'] if ordinary_refresh in ('delete', 'bad-consumer') else ['B'])
                       if ordinary_refresh and name == 'B' else {}),
                    difficulty_updates={}, allow_new_obligations=False, must_close_active=not (ordinary_refresh == 'helpers' and name == 'B'),
                    clear_human_input=False, paper_focus_ranges=[dict(start_line=1, end_line=3, reason='Fixture theorem')],
                    paper_grounding=dict(consulted_cited_ranges=True, basis_summary='The fixture asserts True.'))))
        raw_path = single.work_dir / '.trellis/staging/fixture-review.json'
        raw_path.parent.mkdir(parents=True, exist_ok=True)
        raw_path.write_text(json.dumps(dict(assignments=assignments)))
        return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
            burst_role=single.burst_role, ok=True, raw_path=str(raw_path))
    if kind in ('stuckmathaudit', 'stuck_math_audit') and scoped_test:
        import time
        assert request['scoped_audit']['goals'] == ['A']
        assert (single.work_dir / 'Tablet/H.lean').exists(), 'attested Lean dependency was omitted'
        assert not (single.work_dir / 'Tablet/B.lean').exists(), 'unbound source is presented as current evidence'
        # Require actual overlap: B must reach model work while A's evidence
        # is reserved. No production scheduling or mathematical rule is mocked.
        started = time.time()
        dispatch = Path(payload['runtime_root']).parent.parent
        deadline = time.monotonic() + 60
        while not list(dispatch.glob('[0-9]*/repo/.trellis/staging/fixture-agent-start.json')):
            if time.monotonic() >= deadline:
                raise AssertionError('independent B did not start while the scoped audit was running')
            time.sleep(.25)
        (single.work_dir / '.trellis/staging').mkdir(parents=True, exist_ok=True)
        (single.work_dir / '.trellis/staging/fixture-audit-overlap.json').write_text(json.dumps(dict(started=started, finished=time.time())))
        audit = dict(
                report='## Claim being audited\n' + 'The supplied A statement is True and its direct proof needs only trivial. The shared premise H is available and unchanged. ' * 3,
                tasks=[dict(id='close-a', title='Close A', body='Prove the existing statement with trivial and run the authoritative checker.')], probe_paths=[],
                memory_operations=[dict(op='add', type='interface-decision', coarse_node='A', title='A needs no decomposition',
                    body='The current statement is True; a direct trivial proof suffices without changing any statement or shared premise.')])
        raw_path = single.work_dir / '.trellis/staging/fixture-audit.json'
        raw_path.parent.mkdir(parents=True, exist_ok=True)
        raw_path.write_text(json.dumps(audit))
        return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
            burst_role=single.burst_role, ok=True, payload=audit, raw_path=str(raw_path))
    assert kind == 'worker' and node in ('A', 'B')
    if submission_test:
        import time
        staging = single.work_dir / '.trellis/staging'
        staging.mkdir(parents=True, exist_ok=True)
        started = staging/'fixture-agent-start.json'
        if not started.exists():
            started.write_text(json.dumps(time.time()))
        if node == 'A' and payload['worker_turn']['turn'] == 1:
            # Isolated canary deliberately supplies a bad proof on the first
            # submission, so real host rejection and exact-thread repair run.
            single = replace(single, prompt=single.prompt + '\n\n'
                'Integration canary instruction for this first submission only: replace the sorry in '
                'Tablet/A.lean with exact (0 : Nat), even though it does not prove True. '
                'Do not repair or compile it in this turn. Write the normal valid result JSON, '
                'run the supplied submit command and end your turn. The host must catch this '
                'deliberate error and return diagnostics; then repair it on that correction turn.')
        return original_execute(single, **kwargs)
    if retry_test and request['task_execution']['task_id'] == 1:
        import time
        (single.work_dir / '.trellis/staging').mkdir(parents=True, exist_ok=True)
        (single.work_dir / '.trellis/staging/fixture-agent-start.json').write_text(json.dumps(time.time()))
        time.sleep(2)
        return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
            burst_role=single.burst_role, ok=False, error='fixture transport interruption')
    if migration_test and request.get('parallel_scope', {}).get('exclusive'):
        # The inherited serial task finishes with no source change; the next
        # reviewer can then dispatch both independent toy theorems together.
        return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
            burst_role=single.burst_role, ok=True,
            payload=dict(outcome='valid', summary='Inherited task completed without source changes.',
                         comments='', system_feedback='', deleted_nodes=[]))
    code = '''
import json, subprocess, time
from pathlib import Path
node = __ACTIVE_NODE__
p = Path('Tablet') / (node + '.lean')
if __ORDINARY__ == 'helpers':
    prefix = __PREFIX__
    aux, zero = prefix + 'Aux', prefix + 'Zero'
    Path('Tablet/' + zero + '.lean').write_text('import Tablet.H\\n\\n-- [TABLET NODE: ' + zero + ']\\ndef ' + zero + ' : Nat :=\\n-- BODY\\n  0\\n')
    Path('Tablet/' + zero + '.tex').write_text('\\\\begin{definition}The fixture zero is zero.\\\\end{definition}\\n')
    Path('Tablet/' + aux + '.lean').write_text('import Tablet.H\\n\\n-- [TABLET NODE: ' + aux + ']\\ntheorem ' + aux + ' : True := by\\n-- BODY\\n  trivial\\n')
    Path('Tablet/' + aux + '.tex').write_text('\\\\begin{theorem}True.\\\\end{theorem}\\n\\\\begin{proof}Trivial.\\\\end{proof}\\n')
    p.write_text('import Tablet.' + aux + '\\nimport Tablet.' + zero + '\\n' + p.read_text().replace('sorry', 'exact (fun (_ : ' + zero + ' = 0) => ' + aux + ') rfl -- trivial'))
elif __ORDINARY__ == 'bad-consumer':
    p.write_text(p.read_text().replace('sorry', 'trivial'))
    definition = Path('Tablet/D.lean')
    original = definition.read_text()
    assert '-- BODY\\n  0' in original
    definition.write_text(original.replace('-- BODY\\n  0', '-- BODY\\n  1'))
elif __ORDINARY__ == 'delete':
    p.write_text(p.read_text().replace('import Tablet.D\\n', '').replace('sorry', 'trivial'))
    Path('Tablet/D.lean').unlink()
    Path('Tablet/D.tex').unlink()
elif __IMPORTS__:
    assert 'import Tablet.H' not in p.read_text()
    p.write_text('import Tablet.H\\n' + p.read_text().replace('sorry', 'exact (fun (_ : fixtureZero = 0) => True.intro) H'))
else:
    p.write_text(p.read_text().replace('sorry', 'trivial'))
Path('.trellis/staging/fixture-agent-start.json').write_text(json.dumps(time.time()))
time.sleep(2)
result = subprocess.run(['.trellis/scripts/check-job', 'incremental', node, '--timeout', '90'], capture_output=True, text=True)
Path('.trellis/staging/fixture-check.json').write_text(json.dumps(dict(code=result.returncode, stdout=result.stdout, stderr=result.stderr)))
assert result.returncode == 0, result.stdout + result.stderr
'''.replace('__ACTIVE_NODE__', repr(node)).replace('__IMPORTS__', repr(payload.get('fixture_imports', False))).replace('__ORDINARY__', repr(ordinary_refresh if node == 'B' else None)).replace('__PREFIX__', repr(next(iter(request.get('parallel_scope', {}).get('helper_prefixes', [])), '')))
    command = wrap_command(['/usr/bin/python3', '-c', code], sandbox=single.sandbox,
                           work_dir=single.work_dir, burst_home=single.burst_home)
    result = subprocess.run(command, cwd=single.work_dir, capture_output=True, text=True, timeout=150)
    (single.work_dir / '.trellis/fixture-provider.log').write_text(result.stdout + result.stderr)
    if result.returncode:
        raise RuntimeError('confined fixture worker failed: ' + result.stderr[-4000:])
    metadata = {}
    if ordinary_refresh == 'helpers' and node == 'B':
        prefix = request['parallel_scope']['helper_prefixes'][0]
        metadata = dict(target_claim_updates={prefix + 'Aux': [], prefix + 'Zero': []},
                        difficulty_updates={prefix + 'Aux': 'easy', prefix + 'Zero': 'easy'})
    return SingleAgentResponse(request_id=single.request_id, cycle=single.cycle, kind=single.kind,
        burst_role=single.burst_role, ok=True,
        payload=dict(outcome='valid', summary=f'Closed {node}.', comments='', system_feedback='',
                     deleted_nodes=['D'] if ordinary_refresh == 'delete' and node == 'B' else [], **metadata))


def panel(panel_request, **kwargs):
    # Exercise the production panel wrapper/normalizer with deterministic
    # fixture judgments. No provider call or live-run approval is performed.
    members = []
    for member in panel_request.members:
        verdict = dict(overall='APPROVE', summary='The fixture theorem is True.', comments='')
        if kind == 'sound':
            verdict['node'] = request.get('verify_nodes', [node])[0]
            verdict['soundness'] = dict(decision='SOUND', explanation='Trivial proof.')
        elif request.get('substantiveness_verify_nodes'):
            verdict['substantiveness'] = dict(decision='PASS', verdicts=[
                dict(node=n, verdict='Pass') for n in request['substantiveness_verify_nodes']])
        else:
            verdict['correspondence'] = dict(decision='PASS', issues=[])
            verdict['paper_faithfulness'] = dict(decision='PASS', issues=[])
        raw_path = member.work_dir / '.trellis/staging' / f'fixture-{member.request_id}.json'
        raw_path.parent.mkdir(parents=True, exist_ok=True)
        raw_path.write_text(json.dumps(verdict))
        members.append(SingleAgentResponse(request_id=member.request_id, cycle=member.cycle,
            kind=member.kind, burst_role=member.burst_role, ok=True, payload=verdict, raw_path=str(raw_path)))
    return PanelExecutionResponse(request_id=panel_request.request_id, cycle=panel_request.cycle,
        kind=panel_request.kind, member_responses=members)


bridge.seed_burst_home = home
bridge.execute_agent_request = execute
bridge.execute_panel_raw = panel
with contextlib.redirect_stdout(sys.stderr):
    response = bridge.handle_bridge_request(BridgeCliRequest.from_dict(payload))
json.dump(response, sys.stdout)
