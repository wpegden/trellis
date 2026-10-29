"""Authenticate completed legacy checks as artifact donors, never verdicts.

The new owned stage observes its own leaves under current rules. Portable
replay installs only exact current source/import environments after overlay;
current finalization still performs shared checks and certificate issuance.
"""
from __future__ import annotations

import json
from pathlib import Path
from trellis.private_state import parse_private_state

from . import acceptance_receipts as receipts, candidate
from .acceptance_compatibility import reviewed_producers

MARKER = 'legacy-owner-donor.json'
_REBINDABLE = {'implementation_identity', 'acceptance_sha256', 'input_hashes', 'authority_input_hashes'}


def _read(path):
    raw = receipts.regular(path)
    return parse_private_state(raw) if Path(path).name == 'protocol_state.json' else json.loads(raw)


def _digest(value):
    return candidate.digest(candidate.canonical_json(value))


def _carriers(checked):
    inventories = checked.get('checked_sources') or {}
    maps = [inventories.get(name) for name in ('inputs', 'authority_inputs')]
    if any(not isinstance(inputs, dict) for inputs in maps):
        raise candidate.CandidateError('legacy completion lacks checked authority inventories')
    maps = [{path: digest for path, digest in inputs.items() if path.startswith('.trellis/')}
            for inputs in maps]
    if not maps[0] or maps[0] != maps[1]:
        raise candidate.CandidateError('legacy completion has inconsistent authority carriers')
    return maps[0]


def select(task_directory, candidate_sha256):
    """Select the latest matching full completion; unfinished work stays parked."""
    task = Path(task_directory).absolute()
    if task.parent.name != 'dispatch' or not task.name.isdigit():
        raise candidate.CandidateError('legacy donor requires a host task directory')
    rounds = task / 'acceptance'
    if not rounds.is_dir():
        return None
    for directory in sorted((path for path in rounds.iterdir() if path.name.isdigit()),
                            key=lambda path: int(path.name), reverse=True):
        if not (directory / 'canonical.json').exists():
            continue
        captured = _read(directory / 'canonical.json')
        if 'staged_plan' in captured:
            continue
        key = captured.get('key') or {}
        if key.get('candidate_sha256') != candidate_sha256:
            continue
        if not (directory / 'completed.json').exists():
            return None  # Never bypass a surviving original launcher.
        producer = key.get('implementation_identity')
        if producer not in reviewed_producers() | {receipts.implementation_identity()}:
            return None
        checked = receipts.reusable(directory, key)
        if checked is None:
            raise candidate.CandidateError('legacy completion is not authenticated and reaped')
        response = checked.get('response') or {}
        if (response.get('status') != 'Ok' or response.get('outcome') != 'Valid'
                or response.get('acceptance_check_passed') is not True
                or response.get('transport_failure') or response.get('preflight_error')):
            return None
        original = _read(directory / 'runtime/protocol_state.json')
        if (original.get('in_flight_request') or {}).get('acceptance_logic_identity') != producer:
            raise candidate.CandidateError('legacy request and completion producers differ')
        return dict(donor_round=directory.name,
            donor_completion_sha256=receipts.regular_digest(directory / 'completed.json'),
            key=key, config_sha256=captured['config_sha256'], source=captured['source'],
            authority_inputs=_carriers(checked))
    return None


def _global_inputs(source, known):
    result = {}
    for path, digest in source['input_hashes'].items():
        if path in ('Tablet/INDEX.md', 'Tablet/README.md'):
            continue
        relative = Path(path)
        node = relative.stem if relative.parent == Path('Tablet') and relative.suffix in ('.lean', '.tex') else None
        if node in known and node not in {'Preamble', 'Axioms', 'Assumptions', 'header'}:
            continue
        result[path] = digest
    return result


def prepare(owned_directory, donor):
    """Bind a new owned stage to an older immutable completion in the same task."""
    directory = Path(owned_directory).absolute()
    if not directory.name.isdigit() or directory.parent.name != 'acceptance':
        raise candidate.CandidateError('legacy artifact donation requires an owned acceptance stage')
    task = directory.parent.parent
    if task.parent.name != 'dispatch' or not task.name.isdigit():
        raise candidate.CandidateError('legacy artifact donation has no host task binding')
    ordinal = donor.get('donor_round')
    if not isinstance(ordinal, str) or not ordinal.isdigit() or int(ordinal) >= int(directory.name):
        raise candidate.CandidateError('legacy artifact donor must be an earlier round')
    previous = directory.parent / ordinal
    if receipts.regular_digest(previous / 'completed.json') != donor['donor_completion_sha256']:
        raise candidate.CandidateError('legacy completion changed before donation preparation')
    completed = _read(previous / 'completed.json')
    if completed['key'] != donor['key']:
        raise candidate.CandidateError('legacy donor key changed')
    for name in ('canonical.json', 'execution/response.json'):
        if receipts.regular_digest(previous / name) != completed['files'][name]:
            raise candidate.CandidateError('legacy donor evidence changed')
    original = _read(previous / 'canonical.json')
    checked = _read(previous / 'execution/response.json')
    if (original['source'] != donor['source']
            or original['config_sha256'] != donor['config_sha256']
            or original['key'] != donor['key']
            or _carriers(checked) != donor['authority_inputs']):
        raise candidate.CandidateError('legacy donor descriptor differs from authenticated evidence')
    current = _read(directory / 'canonical.json')
    key = current['key']
    producer, consumer = donor['key']['implementation_identity'], key['implementation_identity']
    if (consumer != receipts.implementation_identity()
            or producer not in reviewed_producers() | {consumer}
            or {k: v for k, v in key.items() if k not in _REBINDABLE}
                != {k: v for k, v in donor['key'].items() if k not in _REBINDABLE}
            or str(key['task_id']) != task.name
            or current['config_sha256'] != donor['config_sha256']
            or current['source'].get('authority_inputs') != donor['authority_inputs']
            or key.get('authority_input_hashes') != donor['authority_inputs']):
        raise candidate.CandidateError('legacy task, candidate, configuration or authority binding changed')
    turn = key.get('submission_turn')
    sealed = task / 'turns' / str(turn) / 'candidate' if turn is not None else task / 'candidate'
    record, identity = candidate.read(sealed)
    if identity != key['candidate_sha256'] or record['task_id'] != key['task_id']:
        raise candidate.CandidateError('legacy donation candidate changed')
    private = _read(directory / 'runtime/protocol_state.json')
    request = private.get('in_flight_request') or {}
    if request.get('acceptance_logic_identity') != consumer:
        raise candidate.CandidateError('current owned request and producer differ')
    owners = set((request.get('worker_acceptance') or {}).get('authorized_nodes', []))
    owners.update((request.get('worker_context') or {}).get('authorized_nodes', []))
    if request.get('active_node'):
        owners.add(request['active_node'])
    for path, change in record['changes'].items():
        path = Path(path)
        if path.parent == Path('Tablet') and path.suffix == '.lean':
            if change['after'] is None:
                owners.discard(path.stem)
            else:
                owners.add(path.stem)
    if not owners:
        raise candidate.CandidateError('legacy donation has no surviving proof owners')
    known = set(donor['source']['graph']['edges']) | set(current['source']['graph']['edges']) | owners
    if _global_inputs(donor['source'], known) != _global_inputs(current['source'], known):
        raise candidate.CandidateError('legacy global semantic inputs changed')
    marker = dict(version=1, donor_round=ordinal,
        donor_completion_sha256=donor['donor_completion_sha256'], current_key_sha256=_digest(key),
        producer_identity=producer, consumer_identity=consumer, owner_nodes=sorted(owners))
    receipts.write_once(directory / MARKER, marker)
    return marker


def resolve(runtime, *, historical_consumer=None):
    """Resolve only this task's fixed old host donor, with unchanged evidence."""
    runtime = Path(runtime).absolute()
    directory = runtime.parent
    if runtime.name != 'runtime' or directory.parent.name != 'acceptance' or not directory.name.isdigit():
        raise candidate.CandidateError('not a legacy artifact donation workspace')
    task = directory.parent.parent
    if task.parent.name != 'dispatch' or not task.name.isdigit():
        raise candidate.CandidateError('legacy artifact donation has no host task binding')
    marker = _read(directory / MARKER)
    ordinal = marker.get('donor_round')
    if (marker.get('version') != 1 or not isinstance(ordinal, str) or not ordinal.isdigit()
            or int(ordinal) >= int(directory.name)):
        raise candidate.CandidateError('legacy artifact donor ordinal changed')
    previous = directory.parent / ordinal
    completed_raw = receipts.regular(previous / 'completed.json')
    completed = json.loads(completed_raw)
    captured = _read(directory / 'canonical.json')
    consumer_matches = marker['consumer_identity'] == receipts.implementation_identity()
    if historical_consumer is not None:
        # Only authentication of an already completed staged receipt supplies
        # this argument. Live artifact installation still requires this build.
        from .staged_acceptance import reviewed_owned_producers
        consumer_matches = (historical_consumer == marker['consumer_identity']
                            and historical_consumer in reviewed_owned_producers())
    if (not consumer_matches
            or marker['producer_identity'] not in reviewed_producers() | {marker['consumer_identity']}
            or str(captured['key']['task_id']) != task.name
            or candidate.digest(completed_raw) != marker['donor_completion_sha256']
            or _digest(captured['key']) != marker['current_key_sha256']
            or completed['key']['implementation_identity'] != marker['producer_identity']
            or captured['key']['implementation_identity'] != marker['consumer_identity']):
        raise candidate.CandidateError('legacy artifact donation binding changed')
    # Full authentication ran before the marker was created. Bind the exact
    # original observations again; artifact bytes are checked by portable replay.
    if receipts.regular_digest(previous / 'execution/response.json') != completed['files']['execution/response.json']:
        raise candidate.CandidateError('legacy donor response changed')
    return previous, marker, completed
