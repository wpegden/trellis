"""Fresh staged publication bindings, authored exclusively by the host.

A completed finalizer checks current dependency bytes, not merely launch-time
prefixes. Its authenticated start inventory can therefore replace the launch
binding for readonly dependencies. This does not rebase owned source edits or
permit a worker to select its own acceptance basis.
"""
from __future__ import annotations

import json
from trellis.private_state import parse_private_state
from pathlib import Path

from . import acceptance_receipts as receipts, candidate, staged_acceptance as stages


def _completed(directory, captured, checked):
    from .staged_driver import _flatten
    completed = stages.reusable_finalizer(directory, captured['key'])
    if completed is None or completed.get('status') != 'checked' or _flatten(completed) != checked:
        raise candidate.CandidateError('completed finalization changed before publication')
    return completed


def compatible(directory, captured, current_key, private, checked, source):
    """Select a reviewed positive finalizer without widening owner/legacy policy."""
    previous = captured['key']
    old_private = parse_private_state(receipts.regular(Path(directory) / 'runtime/protocol_state.json'))
    completed = _completed(directory, captured, checked)
    if (not stages.reviewed_positive_finalizer(previous.get('implementation_identity'), completed, old_private)
            or current_key.get('implementation_identity') != receipts.implementation_identity()
            or private.get('phase') != 'ProofFormalization'
            or (private.get('in_flight_request') or {}).get('acceptance_logic_identity') != current_key['implementation_identity']):
        return None
    permitted = {'implementation_identity', 'acceptance_sha256'}
    if ({k: v for k, v in previous.items() if k not in permitted}
            != {k: v for k, v in current_key.items() if k not in permitted}
            or captured['source']['input_hashes'] != source['input_hashes']
            or captured['source'].get('authority_inputs') != source.get('authority_inputs')):
        return None
    return dict(original_source=captured['source'], decision=dict(version=1,
        kind='reviewed_positive_staged_finalizer', producer_identity=previous['implementation_identity'],
        consumer_identity=current_key['implementation_identity'], original_completion_sha256=
        receipts.regular_digest(Path(directory) / 'staged-completed.json')))


def fresh_basis(directory, captured, current_key, private, source, checked, task):
    """Bind exact current pre-overlay inputs to an authenticated completed check."""
    response = checked.get('response') or {}
    request = private.get('in_flight_request') or {}
    if (private.get('phase') != 'ProofFormalization' or request.get('is_pv')
            or response.get('status') != 'Ok' or response.get('outcome') != 'Valid'
            or response.get('acceptance_check_passed') is not True
            or response.get('transport_failure') or response.get('preflight_error')):
        return None
    _completed(directory, captured, checked)
    previous = captured['key']
    for name in ('task_id', 'candidate_sha256', 'submission_turn', 'task_binding_sha256'):
        if previous.get(name) != current_key.get(name):
            raise candidate.CandidateError('fresh finalization task binding changed')
    if (captured['source']['input_hashes'] != source['input_hashes']
            or captured['source'].get('authority_inputs') != source.get('authority_inputs')):
        return None
    # Native hashes sorted JSON Value (UTF-8, without ASCII escaping). Only
    # string maps are used, never archived/hydrated request representations.
    digest = lambda value: candidate.digest(json.dumps(value, sort_keys=True,
        separators=(',', ':'), ensure_ascii=False).encode())
    assignment = task['assignment']
    return dict(assignment_id=assignment['assignment_id'],
        assignment_inputs_sha256=digest(assignment['input_hashes']),
        inputs_sha256=digest(source['input_hashes']),
        completion_sha256=receipts.regular_digest(Path(directory) / 'staged-completed.json'))
