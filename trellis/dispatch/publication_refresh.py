"""Refresh a publication decision from authenticated host observations.

Environment capture runs before the publication lock. The native refresh is
compiler-free and still enters the ordinary current publication transaction.
Original completion files are never rewritten.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

from . import acceptance_receipts as receipts, candidate


def refresh_stuck(directory, checked, *, current_key, private, repo, kernel):
    """Recheck a sealed closure failure against current native policy.

    A Stuck attempt publishes none of its partial edits or proof observations.
    Authenticate the completed check, then reconstruct its restricted report
    on canonical sources through the existing compiler-free native checker.
    The driver still applies the ordinary current failure/retry transition.
    """
    directory = Path(directory)
    request = private['in_flight_request']
    response = checked.get('response') or {}
    if (not checked.get('ok') or response.get('status') != 'Ok'
            or response.get('outcome') != 'Stuck'
            or (request.get('task_execution') or {}).get('task_kind') != 'lean_closure'):
        return dict(status='cold', reason='not a checked closure Stuck report')
    try:
        completed_raw = receipts.regular(directory / 'completed.json')
        old_key = json.loads(completed_raw)['key']
        # No old observation or verdict becomes authority. The current native
        # constructor revalidates the sealed report, so a producer upgrade may
        # change its policy/context while task, candidate and all inputs stay exact.
        if dict(old_key, acceptance_sha256=current_key['acceptance_sha256'],
                implementation_identity=current_key['implementation_identity']) != current_key:
            return dict(status='cold', reason='candidate, source, or task changed')
        if receipts.reusable(directory, old_key) != checked:
            return dict(status='cold', reason='original completed acceptance changed')
        sealed = directory.parents[1] / 'candidate'
        if 'submission_turn' in old_key:
            sealed = directory.parents[1] / 'turns' / str(old_key['submission_turn']) / 'candidate'
        record, identity = candidate.read(sealed)
        if identity != old_key['candidate_sha256']:
            return dict(status='cold', reason='sealed candidate changed')
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError) as error:
        return dict(status='cold', reason='Stuck refresh evidence unavailable: ' + str(error)[:240])
    # Canonical sources are unchanged. The candidate's partial edits stay in
    # its sealed directory and are never supplied to this failure constructor.
    result = kernel(dict(action='check_unchanged_closure_stuck', repo_path=str(repo),
        request=request, baseline=private['live'], raw_payload=record['response']))
    if result.get('status') != 'dispatch_ok':
        raise RuntimeError('native Stuck refresh returned an unexpected response')
    output = result.get('output')
    if output is None:
        return dict(status='cold', reason='current native closure Stuck contract is not eligible')
    response = output.get('response') or {}
    if (not output.get('ok') or response.get('status') != 'Ok'
            or response.get('outcome') != 'Stuck' or response.get('acceptance_check_passed')
            or response.get('request_id') != request['id'] or response.get('cycle') != request['cycle']):
        raise RuntimeError('native Stuck refresh returned an inconsistent failure response')
    output['candidate_sha256'] = identity
    receipt = dict(version=1, kind='closure_stuck_current_context', donor_round=directory.name,
        donor_completion_sha256=candidate.digest(completed_raw), current_key=current_key, output=output)
    receipt_sha = candidate.digest(candidate.canonical_json(receipt))
    receipts.write_once(directory / 'refreshes' / (receipt_sha + '.json'), receipt)
    return dict(status='refreshed', output=output, refresh_receipt_sha256=receipt_sha)


def _capture(runtime, owners, *, task_runtime=None):
    from trellis.atomic_actions.checker_client import (
        CheckerRpcError, _next_request_id, _round_trip,
    )
    from trellis.checker.workspaces import register, revoke

    runtime = Path(runtime)
    socket = runtime / 'sockets/checker.sock'
    token = os.environ.get('TRELLIS_CHECKER_TOKEN')
    if task_runtime is not None:
        capability = register(runtime, task_runtime, lifetime=None)
        token = capability['TRELLIS_CHECKER_TOKEN']
    try:
        try:
            return _round_trip(socket, dict(op='capture_replay_environment',
                request_id=_next_request_id(), auth_token=token,
                node_names=sorted(owners), timeout_secs=180),
                timeout_secs=3965, inherit_environment=False)
        except CheckerRpcError as error:
            return dict(environment=None, cold_reason=str(error)[:240])
    finally:
        if task_runtime is not None:
            try:
                from .resources import drain_channels
                drain_channels(capability)
            finally:
                revoke(runtime, token)


def prepare(runtime, directory, checked):
    """Authenticate the closed round and observe current environments unlocked.

    Missing legacy evidence is a cold miss. This does not start another worker
    turn or mutate the completed round's source, request, or verdict.
    """
    directory = Path(directory)
    try:
        response = checked.get('response') or {}
        if response.get('status') != 'Ok' or response.get('outcome') != 'Valid':
            return dict(status='cold', reason='original acceptance was not Valid')
        if not checked.get('acceptance_context_sha256'):
            return dict(status='cold', reason='original context binding unavailable')
        completed_raw = receipts.regular(directory / 'completed.json')
        completed = json.loads(completed_raw)
        if receipts.reusable(directory, completed['key']) != checked:
            return dict(status='cold', reason='original completed acceptance changed')
        context_raw = receipts.regular(directory / 'runtime/acceptance-context.json')
        if candidate.digest(context_raw) != checked['acceptance_context_sha256']:
            return dict(status='cold', reason='original acceptance context changed')
        context = json.loads(context_raw)
        environments = dict(baseline=context.get('baseline_environment'), final=checked.get('final_environment'))
        for phase, original in environments.items():
            if (not isinstance(original, dict) or original.get('version') != 1
                    or not isinstance(original.get('environment_sha256s'), dict)
                    or not original['environment_sha256s']):
                return dict(status='cold', reason=phase + ' observation environment unavailable')
        current = {}
        for phase, original in environments.items():
            result = _capture(runtime, original['environment_sha256s'],
                task_runtime=directory / 'runtime' if phase == 'final' else None)
            environment = result.get('environment')
            if not isinstance(environment, dict):
                return dict(status='cold', reason=phase + ' current environment unavailable',
                    capture_reason=str(result.get('cold_reason', ''))[:240])
            current[phase + '_environment'] = environment
        record, identity = candidate.read(
            directory.parents[1] / 'turns' / str(completed['key']['submission_turn']) / 'candidate'
            if 'submission_turn' in completed['key'] else directory.parents[1] / 'candidate')
        if identity != completed['key']['candidate_sha256']:
            return dict(status='cold', reason='sealed candidate changed')
        from .replay_donation import validation_fact_context
        return dict(status='prepared', original_key=completed['key'],
            donor_completion_sha256=candidate.digest(completed_raw),
            original_acceptance_context=context, original_checked=checked,
            raw_payload=record['response'],
            validation_fact_context=validation_fact_context(directory / 'runtime'), **current)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError) as error:
        return dict(status='cold', reason='refresh preparation unavailable: ' + str(error)[:240])


def apply(directory, prepared, *, current_key, request, repo, kernel):
    """Under the publication lock, rebuild only the current native decision."""
    directory = Path(directory)
    if prepared.get('status') != 'prepared':
        return prepared
    old_key = prepared['original_key']
    if dict(old_key, acceptance_sha256=current_key['acceptance_sha256']) != current_key:
        return dict(status='cold', reason='candidate, source, task, or implementation changed')
    if candidate.digest(receipts.regular(directory / 'completed.json')) != prepared['donor_completion_sha256']:
        return dict(status='cold', reason='original completion changed before refresh')
    from .publication_artifacts import lexical_authority
    fields = ('original_acceptance_context', 'original_checked', 'raw_payload',
              'validation_fact_context', 'baseline_environment', 'final_environment')
    result = kernel(dict(action='try_refresh_trellis_worker_result',
        repo_path=str(lexical_authority(directory / 'repo')),
        baseline_repo_path=str(repo), request=request,
        **{name: prepared[name] for name in fields}))
    if result.get('status') != 'try_refresh_trellis_worker_result_ok':
        raise RuntimeError('native publication refresh returned an unexpected response')
    result = result['output']
    if result.get('status') == 'cold':
        return result
    output = result.get('output') or {}
    if (result.get('status') != 'refreshed' or not isinstance(result.get('refresh_receipt'), dict)
            or not output.get('ok') or output.get('response', {}).get('outcome') != 'Valid'):
        raise RuntimeError('native publication refresh returned an incomplete decision')
    receipt = dict(version=1, donor_round=directory.name,
        donor_completion_sha256=prepared['donor_completion_sha256'], current_key=current_key,
        native=result['refresh_receipt'], output=output)
    identity = candidate.digest(candidate.canonical_json(receipt))
    receipts.write_once(directory / 'refreshes' / (identity + '.json'), receipt)
    return dict(status='refreshed', output=output, refresh_receipt_sha256=identity)
