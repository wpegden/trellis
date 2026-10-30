"""Reviewed producer compatibility; never a replacement acceptance verdict.

The fixed-proof producers are clean, deployed 9b384f49, 0e437b72 and ef05a3b6.
Their completed proof checks retain their original receipts. The current native transition,
semantic source guards and certificate reconciliation still authorize adoption.
The separate Modeling producer category can only reach its native exact-source
Modeling route; it never authorizes general fixed-proof reuse.
The theorem-stating category reaches only its exact-source positive context
route, preserving PV operands and the current native structural transition.
"""
from __future__ import annotations

import json
from pathlib import Path

from . import acceptance_receipts, candidate


def reviewed_producers(*, modeling=False, theorem_stating=False):
    policy = json.loads(Path(__file__).with_name('compatible_acceptance_producers.json').read_text())
    if policy.get('version') != 1:
        raise RuntimeError('unsupported acceptance producer compatibility policy')
    category = ('theorem_stating_identities' if theorem_stating else
                'modeling_identities' if modeling else 'identities')
    return frozenset(policy.get(category, []))


def prepare(directory, captured, current_key, original_private, current_private, checked, current_source):
    """Authenticate the old completion and derive a separate adoption request.

    Return None for incompatible provenance/carriers. Corrupt authenticated
    evidence remains an error. The caller records the returned decision and
    passes the derived source view through the ordinary native rebase gates.
    """
    old_key = captured['key']
    producer, consumer = old_key.get('implementation_identity'), current_key.get('implementation_identity')
    modeling = (original_private.get('phase') == current_private.get('phase') == 'Modeling')
    def stating_lane(state):
        execution = state.get('worker_execution') or {}
        request = state.get('in_flight_request') or {}
        return (state.get('phase') == 'TheoremStating'
                and execution.get('task_kind') == 'ordinary'
                and execution.get('compiler') == 'managed'
                and (request.get('worker_acceptance') or {}).get('validation_kind') == 'TheoremTargeted')
    theorem_stating = stating_lane(original_private) and stating_lane(current_private)
    if (producer not in reviewed_producers(modeling=modeling, theorem_stating=theorem_stating) or producer == consumer
            or consumer != acceptance_receipts.implementation_identity()):
        return None
    old_request = original_private.get('in_flight_request') or {}
    current_request = current_private.get('in_flight_request') or {}
    if (old_request.get('acceptance_logic_identity') != producer
            or current_request.get('acceptance_logic_identity') != consumer):
        return None
    response = checked.get('response') or {}
    if (response.get('status') != 'Ok' or response.get('outcome') != 'Valid'
            or response.get('acceptance_check_passed') is not True
            or response.get('transport_failure') or response.get('preflight_error')):
        return None
    allowed = {'implementation_identity', 'acceptance_sha256', 'input_hashes', 'authority_input_hashes'}
    if ({key: value for key, value in old_key.items() if key not in allowed}
            != {key: value for key, value in current_key.items() if key not in allowed}):
        return None
    if acceptance_receipts.reusable(directory, old_key) != checked:
        raise candidate.CandidateError('original completion changed before producer compatibility review')

    # Old canonical captures omitted host-only paths, but their authenticated
    # final inventories include them. Read the original evidence, never fill a
    # missing old binding from the current deployment.
    inventories = checked.get('checked_sources') or {}
    maps = [inventories.get(name) for name in ('inputs', 'authority_inputs')]
    if any(not isinstance(inputs, dict) for inputs in maps):
        return None
    carriers = [{path: digest for path, digest in inputs.items() if path.startswith('.trellis/')}
                for inputs in maps]
    original_authority = carriers[0]
    if (not original_authority or carriers[1] != original_authority
            or original_authority != current_source.get('authority_inputs')
            or original_authority != current_key.get('authority_input_hashes')):
        return None
    if ('authority_inputs' in captured['source']
            and captured['source']['authority_inputs'] != original_authority):
        return None
    if ('authority_input_hashes' in old_key and old_key['authority_input_hashes'] != original_authority):
        return None
    digest = lambda value: candidate.digest(candidate.canonical_json(value))
    decision = dict(version=1, kind='reviewed_acceptance_producer',
        producer_identity=producer, consumer_identity=consumer,
        original_completion_sha256=acceptance_receipts.regular_digest(Path(directory) / 'completed.json'),
        original_key_sha256=digest(old_key), current_key_sha256=digest(current_key),
        authority_inputs_sha256=digest(original_authority))
    return dict(decision=decision,
                original_source=dict(captured['source'], authority_inputs=original_authority))
