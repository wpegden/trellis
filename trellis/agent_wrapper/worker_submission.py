"""Shared delivery/continuation mechanics; mathematical checking stays on the host."""
from dataclasses import replace
import hashlib
import json
import re
import shlex
from pathlib import Path

from trellis.agent_wrapper.continuation import session_binding
from trellis.dispatch.candidate import canonical_json, digest
from trellis.dispatch.publication import atomic


def native_digest(value):
    # Native serde_json hashes UTF-8 rather than ASCII-escaped JSON.
    return digest(json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode())


def engineering_artifact(native, turn, ordinal=0):
    suffix = f'-transport-{ordinal}' if ordinal else ''
    return f"trellis_engineering_{native['id']}_submission-{turn}{suffix}.json"


def engineering_check_name(submission):
    ordinal = submission.get('transport_ordinal', 0)
    suffix = f'-transport-{ordinal}' if ordinal else ''
    return f"{submission['engineering_request']['id']}-{submission['turn']}{suffix}"


def engineering_turn(single, submission):
    """Compare delivery with the current kernel-issued ready turn."""
    from trellis.git_trust import read_engineering_json
    from trellis.agents.engineering_exec import open_regular
    native = submission['engineering_request']
    path = Path(submission['native_runtime']) / 'protocol_state.json'
    state, _ = read_engineering_json(path, limit=64 * 1024 * 1024)
    from trellis.runtime.kernel_cli import run_kernel_cli
    authorized = run_kernel_cli(dict(action='engineering_ready_turn', state=state))['turn']
    if any(submission.get(key) != value for key, value in authorized.items()):
        raise RuntimeError('Engineering turn differs from native authorization')
    attempt = submission['engineering_attempt']
    contract = native['engineering']
    profile = contract['profile']
    ordinal = submission.get('transport_ordinal', 0)
    continuation = attempt['phase']['phase'] == 'transport_ready'
    artifact = engineering_artifact(native, attempt['turn'], ordinal)
    kind = ('preparation-performance-result' if contract['purpose'] == 'performance' else
            'preparation-audit-result' if contract['purpose'].endswith('review') else 'preparation-worker-result')
    if (submission.get('transport_resume')
            or type(ordinal) is not int or ordinal != attempt.get('transport_retries', 0)
            or bool(submission.get('transport_continuation')) != continuation
            or submission['remaining_ms'] <= 0 or attempt['turn'] > attempt['policy']['max_submissions']
            or single.kind != 'engineering' or single.burst_role != 'engineering'
            or single.request_id != f"engineering-{native['id']}-{contract['purpose']}"
            or single.cycle != native['cycle'] or single.artifact is None
            or single.artifact.canonical_name != artifact or single.artifact.phase != 'engineering' or single.artifact.kind != kind
            or (single.provider.provider, single.provider.model, single.provider.effort) !=
               (profile['provider'], profile['model'], profile['reasoning_effort'])
            or single.provider.extra_args or single.burst_home is None
            or single.lane.kind != 'engineering'):
        raise RuntimeError('Engineering turn differs from current native authorization')
    if (native_digest(contract['artifact_contract']) != contract['artifact_contract_sha256']
            or contract['artifact_contract']['purpose'] != contract['purpose']):
        raise RuntimeError('Engineering artifact contract differs from native authorization')
    helper = Path(__file__).resolve().parents[1] / 'engineering_check.py'
    with open_regular(helper) as stream:
        helper_bytes = stream.read()
    if hashlib.sha256(helper_bytes).hexdigest() != contract['checker_sha256']:
        raise RuntimeError('Engineering checker differs from native authorization')
    if attempt['turn'] == 1 and not continuation:
        if attempt.get('session') or attempt['phase'].get('feedback_sha256') or submission.get('feedback'):
            raise RuntimeError('Initial engineering assignment contains continuation context')
    elif continuation and (not attempt.get('session') or attempt['last_interruption']['turn'] != attempt['turn']):
        raise RuntimeError('Engineering transport lost its retained conversation')
    if attempt['turn'] > 1 and (not attempt.get('session') or native_digest(submission['feedback']) != submission['feedback_sha256']
          or submission['feedback'].get('outcome') != 'malformed'):
        raise RuntimeError('Engineering correction lacks its checked rejection')
    return contract, helper_bytes


def _immutable_text(path, text):
    from trellis.agents.host_files import safe_directory, atomic_text
    from trellis.agents.engineering_exec import open_regular
    with safe_directory(path.parent) as (directory, fd):
        try:
            with open_regular(path) as stream:
                existing = stream.read()
        except FileNotFoundError:
            atomic_text(directory, fd, path.name, text)
        else:
            if existing != text.encode('utf-8'):
                raise RuntimeError('Engineering turn publication changed retained bytes')


def _engineering_check_command(single, submission, contract, helper_bytes):
    from trellis.agent_wrapper.executor import prepare_artifact_paths
    paths = prepare_artifact_paths(single.state_dir, single.work_dir, single.artifact.canonical_name)
    directory = single.work_dir / '.trellis/worker-checks'
    name = engineering_check_name(submission)
    helper, rules, script = directory / 'engineering-check.py', directory / (name + '.json'), directory / (name + '.sh')
    _immutable_text(helper, helper_bytes.decode('utf-8'))
    _immutable_text(rules, json.dumps(contract['artifact_contract'], sort_keys=True, separators=(',', ':'), ensure_ascii=False))
    command = ['/usr/bin/python3', '-I', '-S', str(helper), str(rules), contract['artifact_contract_sha256'],
               contract['checker_sha256'], str(paths.raw), str(single.work_dir), str(paths.done)]
    files=[helper,rules,script]
    if contract['purpose']=='performance' and 'reset_budget' in contract['artifact_contract']['performance_fields']:
        from trellis.agents.engineering_exec import file_identity
        budget=single.work_dir/'performance-budget.json'
        command+=['--performance-budget-sha256',file_identity(budget)['sha256']]
        files.append(budget)
    _immutable_text(script, '#!/bin/sh\nset -eu\nexec ' + shlex.join(command) + ' "$@"\n')
    return 'sh ' + shlex.quote(str(script.relative_to(single.work_dir))), files


def prepare_turn(single, submission, runtime, *, task_kind=None):
    engineering = single.kind == 'engineering'
    if engineering != (task_kind == 'engineering'):
        raise RuntimeError('Engineering turn requires its explicit submission domain')
    if engineering:
        contract, helper_bytes = engineering_turn(single, submission)
        check_command, checker_files = _engineering_check_command(single, submission, contract, helper_bytes)
    turn = submission['turn']
    session = submission.get('session')
    transport_resume = submission.get('transport_resume')
    if transport_resume is not None:
        resumed_session = transport_resume['session']
        if session is not None and resumed_session != session:
            raise RuntimeError('worker transport resume changed its original conversation binding')
        session = resumed_session
    thread = session.get('thread_id', '') if session else ''
    if (turn > 1 or session is not None) and (
            not thread or session != session_binding(single.provider, single.burst_home, thread)):
        raise RuntimeError('worker continuation lost its original provider/model/conversation binding')
    prompt = single.prompt
    if turn > 1:
        feedback = submission['feedback']
        if native_digest(feedback) != submission['feedback_sha256']:
            raise RuntimeError('worker correction feedback differs from the native authorization')
        if engineering:
            diagnostics = feedback['diagnostics']
            if (not isinstance(diagnostics, list) or not diagnostics
                    or any(not isinstance(d, dict) or set(d) != {'path', 'rule'}
                           or not isinstance(d['path'], str) or not re.fullmatch(r'[A-Za-z0-9_.\[\]-]{1,256}', d['path'])
                           or d['rule'] not in {'required', 'closed_fields', 'type', 'nonempty', 'enum', 'stale', 'failed_check'}
                           for d in diagnostics)):
                raise RuntimeError('Engineering correction diagnostics are invalid')
            build_outputs = {
                'source.target': 'Remove target/ build output directories from source/ before resubmitting.',
                'source.__pycache__': 'Remove __pycache__/ build output directories from source/ before resubmitting.',
            }
            details = '\n'.join(d['path'] + ': ' + d['rule']
                + ('\n' + build_outputs[d['path']] if d['rule'] == 'closed_fields' and d['path'] in build_outputs else '')
                for d in diagnostics)
        else:
            response = feedback['response']
            details = '\n'.join(filter(None, [response.get('comments', ''),
                *response.get('deterministic_rejection_reasons', [])]))
        path = single.work_dir / '.trellis/check-results' / f'submission-{turn - 1}.txt'
        from trellis.agents.host_files import safe_directory, atomic_text
        with safe_directory(path.parent) as (directory, fd):
            atomic_text(directory, fd, path.name, details)
        from trellis.agent_wrapper.executor import prepare_artifact_paths
        artifacts = prepare_artifact_paths(single.state_dir, single.work_dir, single.artifact.canonical_name)
        bound = re.fullmatch(r'trellis_(?:worker|engineering)_(\d+)_submission-\d+(?:-transport-\d+)?\.json', single.artifact.canonical_name)
        if bound is None:
            raise RuntimeError('worker correction has no bound submission artifact')
        if engineering:
            prompt = (f'Submission {turn - 1} failed checking.\n{details}\n\n'
                      f'Correct the retained files, write {artifacts.raw.relative_to(single.work_dir)}, '
                      f'run `{check_command}`, and end your turn.')
        else:
            prompt = (f'Submission {turn - 1} failed authoritative checking.\n\n'
                  f'{details[:12000]}\n\nSaved checker feedback: {path.relative_to(single.work_dir)}\n\n'
                  'Your files and edit scope are retained. Fix the failure, write your result JSON to '
                  f'{artifacts.raw.relative_to(single.work_dir)}, then run '
                  f'sh .trellis/worker-checks/{bound.group(1)}.sh and end your turn. '
                  'The host will check your work; if it passes, you are finished.')
    if submission.get('transport_continuation'):
        from trellis.agent_wrapper.executor import prepare_artifact_paths
        artifacts = prepare_artifact_paths(single.state_dir, single.work_dir, single.artifact.canonical_name)
        bound = re.fullmatch(r'trellis_(?:worker|engineering)_(\d+)_submission-\d+-transport-\d+\.json', single.artifact.canonical_name)
        if bound is None or not thread:
            raise RuntimeError('transport continuation requires the original thread and owned artifact')
        if engineering:
            continuation = 'The provider connection ended before this submission completed. '
            prompt = continuation + (prompt if turn > 1 else
                f'Continue the retained work. Write {artifacts.raw.relative_to(single.work_dir)}, '
                f'run `{check_command}`, and end your turn.')
        else:
            prompt = ('Your previous provider connection ended before completing this submission. '
                  'Your source files, conversation, scope and remaining execution allowance are retained. '
                  'This is the same submission; no host proof verdict was issued for the interrupted output. '
                  'Continue your unfinished work. Write the required result JSON (including deleted_nodes) to '
                  f'{artifacts.raw.relative_to(single.work_dir)}, then run '
                  f'sh .trellis/worker-checks/{bound.group(1)}.sh and end your turn after Submitted. '
                  'Ordinary host acceptance still follows.')
    if task_kind == 'lean_closure' and (turn > 1 or submission.get('transport_continuation')):
        prompt += (
            f"\n\nThis continuation starts with {submission['remaining_ms'] / 1000:g} seconds "
            "of remaining execution allowance. Continue concrete Lean proof development in the "
            "assigned body: prove local intermediate facts, check candidates, and use the errors "
            "to revise your approach. Work until the theorem closes or the remaining allowance "
            "is nearly spent, reserving time for the required submission."
        )
    if engineering and turn == 1 and not submission.get('transport_continuation'):
        from trellis.agent_wrapper.executor import prepare_artifact_paths
        paths = prepare_artifact_paths(single.state_dir, single.work_dir, single.artifact.canonical_name)
        prompt += f'\nWrite {paths.raw.relative_to(single.work_dir)}, run `{check_command}`, and end your turn.'
    prepared = replace(single, prompt=prompt, require_turn_completion=True,
        timeout_seconds=submission['remaining_ms'] / 1000,
        resume_thread_id=thread, session_record=Path(runtime) / 'worker-session.json',
        execution_record=Path(submission['execution_record']), transport_resume=transport_resume)
    if engineering:
        prepared = replace(prepared, fresh=not bool(thread))
        from trellis.agents.engineering_exec import file_identity
        from trellis.agents.engineering_context import turn_directories
        record = dict(request=prepared.to_dict(), submission=submission,
                      checker_files={str(path): file_identity(path) for path in checker_files},
                      directories=turn_directories(single.work_dir, single.burst_home))
        path = prepared.execution_record.parent / 'engineering-request.json'
        _immutable_text(path, json.dumps(record, sort_keys=True, indent=2) + '\n')
    return prepared


def capture_delivery(response, *, request, submission):
    """Preserve delivery even at exact exhaustion; no semantic compile here.

    The caller still drains execution before sealing sources. An exhausted turn
    with parseable output is checked once, but cannot receive another turn.
    """
    raw, error = None, ''
    try:
        if response.raw_path is not None and response.raw_path.is_file():
            raw = json.loads(response.raw_path.read_text())
        elif isinstance(response.payload, dict):
            raw = response.payload
        if not isinstance(raw, dict):
            raise ValueError('Submission must contain a JSON object')
    except (OSError, ValueError) as exc:
        error = str(exc)
        raw = None
    timing = response.provider_execution
    return dict(kind='worker_submission', request_id=request['id'], cycle=request['cycle'],
        turn=submission['turn'], raw=raw, delivery_error=error,
        provider_execution=timing, provider_session_id=response.provider_session_id,
        provider_turn_completed=response.provider_turn_completed, transport_error=response.error)
