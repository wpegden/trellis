"""Host-owned proof observations followed by current-source finalization.

An owned stage never returns a worker acceptance decision. Its immutable host
completion authorizes only typed observations, which native finalization must
reconcile with the current request, merged sources and imported environment.
The driver retains semantic reservations and serializes finalization separately.
"""
from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path
import sys

from trellis.private_state import parse_private_state

from . import acceptance_receipts as receipts, candidate
from .publication import atomic

_COMMAND = ['-m', 'trellis.dispatch.staged_acceptance']
_FILES = receipts._RECEIPTS
_STABLE_KEY_FIELDS = ('task_id', 'candidate_sha256', 'submission_turn',
                      'task_binding_sha256')


def _layout(directory):
    directory = Path(directory).absolute()
    if (directory.name.isdigit() and directory.parent.name == 'finalization'
            and directory.parent.parent.name.isdigit()
            and directory.parent.parent.parent.name == 'acceptance'):
        owned = directory.parent.parent
        stage = 'finalize'
    elif directory.name.isdigit() and directory.parent.name == 'acceptance':
        owned, stage = directory, 'owned'
    else:
        raise candidate.CandidateError('not an isolated staged acceptance workspace')
    task = owned.parent.parent
    if not task.name.isdigit() or task.parent.name != 'dispatch':
        raise candidate.CandidateError('staged acceptance has no host task binding')
    return directory, owned, task, stage


def _sealed(task, key):
    turn = key.get('submission_turn')
    if turn is not None and (type(turn) is not int or turn < 1):
        raise candidate.CandidateError('staged acceptance has an invalid submission ordinal')
    return task / 'turns' / str(turn) / 'candidate' if turn is not None else task / 'candidate'


def authenticate_execution(directory, expected_key):
    """Authenticate preparation and a reaped host launch, without granting facts."""
    from .driver import acceptance_identity
    from .process import alive

    directory, _owned, task, stage = _layout(directory)
    if str(expected_key['task_id']) != task.name:
        raise candidate.CandidateError('staged acceptance belongs to a different task')
    sealed = _sealed(task, expected_key)
    record, identity = candidate.read(sealed)
    if (identity != expected_key['candidate_sha256'] or record['task_id'] != expected_key['task_id']
            or record.get('submission', {}).get('turn') != expected_key.get('submission_turn')):
        raise candidate.CandidateError('staged acceptance candidate changed')
    raw = {name: receipts.regular(directory / name) for name in _FILES}
    values = {name: parse_private_state(data) if name == 'runtime/protocol_state.json' else json.loads(data)
              for name, data in raw.items() if name != 'execution/response.json'}
    captured, prepared = values['canonical.json'], values['prepared.json']
    if (captured.get('key') != expected_key
            or captured['source']['input_hashes'] != expected_key['input_hashes']
            or captured['source'].get('authority_inputs') != expected_key.get('authority_input_hashes')
            or captured['acceptance_sha256'] != expected_key['acceptance_sha256']):
        raise candidate.CandidateError('staged acceptance capture changed')
    private = values['runtime/protocol_state.json']
    if (prepared['identity'] != candidate.digest(candidate.canonical_json(dict(state=private,
            sources=values['base-manifest.json'], config_sha256=captured['config_sha256'])))
            or acceptance_identity(private) != expected_key['acceptance_sha256']):
        raise candidate.CandidateError('staged acceptance preparation changed')
    process = values['execution/process.json']
    if values['execution/permit.json'] != process or values['execution/cleanup.json'] != process or alive(process):
        raise candidate.CandidateError('staged acceptance launch/cleanup is not complete')
    result = values['execution/result.json']
    if type(result.get('returncode')) is not int or result.get('response_sha256') != candidate.digest(raw['execution/response.json']):
        raise candidate.CandidateError('staged acceptance completion changed')
    recipe = values['execution/recipe.json']
    payload = recipe['payload']
    if (recipe['command'][1:] != _COMMAND or payload.get('stage') != stage
            or payload.get('config_path') != str(directory / 'repo/trellis.config.json')
            or payload.get('runtime_root') != str(directory / 'runtime')
            or payload.get('candidate') != str(sealed)
            or payload.get('captured_inputs') != expected_key['input_hashes']
            or payload.get('source_graph') != captured['source']['graph']):
        raise candidate.CandidateError('staged acceptance launch was not authorized for these inputs')
    checker = recipe.get('checker', {})
    if (checker.get('runtime') != str(task.parent.parent)
            or checker.get('task_runtime') != str(directory / 'runtime')
            or checker.get('compiler') != 'managed'
            or checker.get('scope') != str(directory.relative_to(task.parent))):
        raise candidate.CandidateError('staged acceptance lacks its authorized host compiler binding')
    return raw, values


def authenticate(directory, expected_key, *, _reviewed_finalizer=False):
    """Bind facts to an authorized, completed and reaped host process."""
    raw, values = authenticate_execution(directory, expected_key)
    directory, _owned, task, stage = _layout(directory)
    identity = expected_key["candidate_sha256"]
    captured = values["canonical.json"]
    private = values["runtime/protocol_state.json"]
    if values["execution/result.json"]["returncode"] != 0:
        raise candidate.CandidateError("staged acceptance completion changed")
    output = json.loads(raw['execution/response.json'])
    if output.get('stage') != stage or output.get('candidate_sha256') != identity:
        raise candidate.CandidateError('staged acceptance output belongs to a different stage or candidate')
    allowed = {'facts', 'rejected', 'unsupported', 'cold'} if stage == 'owned' else {'checked', 'cold', 'unsupported'}
    if output.get('status') not in allowed:
        raise candidate.CandidateError('staged acceptance output has an unknown status')
    if stage == 'owned':
        if any(name in output for name in ('response', 'outcome', 'acceptance_check_passed', 'output')):
            raise candidate.CandidateError('owned proof observations cannot be a worker decision')
        if output['status'] in ('facts', 'rejected') and not isinstance(output.get('facts'), dict):
            raise candidate.CandidateError('owned proof observations are missing')
    elif output['status'] == 'checked' and not isinstance(output.get('output', {}).get('response'), dict):
        raise candidate.CandidateError('current staged finalization has no worker decision')
    if 'context_sha256' in output:
        raw['runtime/staged-context.json'] = receipts.regular(directory / 'runtime/staged-context.json')
        if candidate.digest(raw['runtime/staged-context.json']) != output['context_sha256']:
            raise candidate.CandidateError('staged acceptance context changed')
    if output.get('checked_sources') != receipts.checked_sources(directory / 'repo'):
        raise candidate.CandidateError('staged acceptance sources changed')
    if stage == 'finalize':
        raw['staged-donor.json'] = receipts.regular(directory / 'staged-donor.json')
        # Historical Cold is evidence for a bounded fresh finalizer, never an
        # old acceptance decision or permission to install from its workspace.
        consumer = expected_key.get('implementation_identity')
        historical = consumer if consumer != receipts.implementation_identity() and (
            output['status'] == 'cold' or _reviewed_finalizer and reviewed_positive_finalizer(
                consumer, output, values['runtime/protocol_state.json'])) else None
        _previous, marker, _completed = donor(directory / 'runtime', historical_consumer=historical)
        if output.get('donor_completion_sha256') != marker['donor_completion_sha256']:
            raise candidate.CandidateError('staged finalization donor changed')
    elif captured.get('legacy_donor') is not None:
        from .legacy_acceptance import resolve
        raw['legacy-owner-donor.json'] = receipts.regular(directory / 'legacy-owner-donor.json')
        producer = expected_key.get('implementation_identity')
        historical = producer if producer != receipts.implementation_identity() else None
        _previous, marker, _completed = resolve(directory / 'runtime', historical_consumer=historical)
        if any(marker.get(k) != captured['legacy_donor'].get(k)
               for k in ('donor_round', 'donor_completion_sha256')):
            raise candidate.CandidateError('staged legacy artifact donor changed')
    retry_marker = directory / 'replay-cache-donor.json'
    if stage == 'owned' and retry_marker.exists():
        from .staged_driver import owned_retry_donor
        raw['replay-cache-donor.json'] = receipts.regular(retry_marker)
        owned_retry_donor(directory / 'runtime')
    return dict(version=1, stage=stage, key=expected_key,
                files={name: candidate.digest(data) for name, data in raw.items()}), output


def complete(directory, output):
    captured = json.loads(receipts.regular(Path(directory) / 'canonical.json'))
    receipt, actual = authenticate(directory, captured['key'])
    if output != actual:
        raise candidate.CandidateError('staged result differs from the host completion')
    receipts.write_once(Path(directory) / 'staged-completed.json', receipt)


def reusable(directory, expected_key):
    """Corrupt or unfinished stages stay unavailable; never infer fresh facts."""
    try:
        saved = json.loads(receipts.regular(Path(directory) / 'staged-completed.json'))
        if saved.get('key') != expected_key or (Path(directory) / 'retry-authorized.json').exists():
            return None
        receipt, output = authenticate(directory, expected_key)
        return output if receipt == saved else None
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return None


def reviewed_positive_finalizer(producer, output, private):
    policy = json.loads(Path(__file__).with_name('compatible_staged_producers.json').read_text())
    request = private.get('in_flight_request') or {}
    response = output.get('output', {}).get('response', {})
    return (policy.get('version') == 1 and producer in policy.get('positive_math_finalizers', [])
        and output.get('status') == 'checked' and private.get('phase') == 'ProofFormalization'
        and request.get('acceptance_logic_identity') == producer
        and not request.get('is_pv') and not request.get('modeling_draft')
        and not request.get('node_role') and not request.get('configured_challenge_targets')
        and response.get('status') == 'Ok' and response.get('outcome') == 'Valid'
        and response.get('acceptance_check_passed') is True
        and not response.get('transport_failure') and not response.get('preflight_error')
        and not response.get('deterministic_rejection_reasons'))


def reusable_finalizer(directory, expected_key):
    """Authenticate an immutable positive historical completion; never rerun it.

    This separate consumer only permits reviewed finalizer provenance. Current
    source/authority/transition checks still precede every publication.
    """
    ordinary = reusable(directory, expected_key)
    if ordinary is not None:
        return ordinary
    try:
        if _layout(directory)[3] != 'finalize' or (Path(directory) / 'retry-authorized.json').exists():
            return None
        saved = json.loads(receipts.regular(Path(directory) / 'staged-completed.json'))
        if saved.get('key') != expected_key:
            return None
        receipt, output = authenticate(directory, expected_key, _reviewed_finalizer=True)
        private = parse_private_state(receipts.regular(Path(directory) / 'runtime/protocol_state.json'))
        return output if receipt == saved and reviewed_positive_finalizer(
            expected_key.get('implementation_identity'), output, private) else None
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return None


def reviewed_owned_producers():
    policy = json.loads(Path(__file__).with_name('compatible_staged_producers.json').read_text())
    if policy.get('version') != 1:
        raise RuntimeError('unsupported staged producer compatibility policy')
    return frozenset(policy['positive_math_owned'])


def _producer_decision(directory, current, previous, completed, checked, *, historical_consumer=None):
    """Explicit reviewed raw-fact compatibility; original evidence stays exact."""
    producer = completed['key'].get('implementation_identity')
    consumer = current['key'].get('implementation_identity')
    if producer == consumer:
        return None
    old_request = parse_private_state(receipts.regular(directory.parent.parent / 'runtime/protocol_state.json')).get('in_flight_request') or {}
    new_request = parse_private_state(receipts.regular(directory / 'runtime/protocol_state.json')).get('in_flight_request') or {}
    facts = checked.get('facts') or {}
    authority = completed['key'].get('authority_input_hashes')
    def math_pf(request):
        return (str(request.get('phase')).lower().replace('_', '') == 'proofformalization'
                and str(request.get('kind')).lower() == 'worker'
                and request.get('is_pv', False) is False and not request.get('modeling_draft')
                and not request.get('node_role') and not request.get('configured_challenge_targets'))
    consumer_matches = consumer == receipts.implementation_identity()
    if historical_consumer is not None:
        consumer_matches = consumer == historical_consumer and consumer in reviewed_owned_producers()
    if (producer not in reviewed_owned_producers()
            or not consumer_matches
            or old_request.get('acceptance_logic_identity') != producer
            or new_request.get('acceptance_logic_identity') != consumer
            or not math_pf(old_request) or not math_pf(new_request)
            or checked.get('status') != 'facts' or facts.get('owner_ok') is not True
            or facts.get('schema_version') != 1 or facts.get('acceptance_logic_identity') != producer
            or not isinstance(authority, dict) or not authority
            or authority != current['key'].get('authority_input_hashes')):
        raise candidate.CandidateError('staged owner producer is not compatible with current finalization')
    return dict(version=1, kind='reviewed_staged_owner_producer',
                producer_identity=producer, consumer_identity=consumer)


def prepare_donor(directory, checked):
    """Bind a nested finalizer to its immutable, reaped proof stage."""
    directory, owned, _task, stage = _layout(directory)
    if stage != 'finalize':
        raise candidate.CandidateError('proof donor requires a finalization workspace')
    raw = receipts.regular(owned / 'staged-completed.json')
    completed = json.loads(raw)
    if reusable(owned, completed['key']) != checked or checked.get('status') not in ('facts', 'rejected'):
        raise candidate.CandidateError('proof donor has no authenticated completed observations')
    current = json.loads(receipts.regular(directory / 'canonical.json'))
    previous = json.loads(receipts.regular(owned / 'canonical.json'))
    if (any(current['key'].get(k) != completed['key'].get(k) for k in _STABLE_KEY_FIELDS)
            or current['config_sha256'] != previous['config_sha256']):
        raise candidate.CandidateError('proof donor candidate, task, configuration or implementation changed')
    marker = dict(version=1, donor_round=owned.name,
        donor_completion_sha256=candidate.digest(raw),
        current_key_sha256=candidate.digest(candidate.canonical_json(current['key'])),
        owner_nodes=checked['facts']['owner_nodes'])
    decision = _producer_decision(directory, current, previous, completed, checked)
    if decision is not None:
        marker['producer_compatibility'] = decision
    receipts.write_once(directory / 'staged-donor.json', marker)
    return marker


def donor(runtime, *, historical_consumer=None):
    """Resolve only the containing proof stage, never a caller-selected path."""
    runtime = Path(runtime).absolute()
    directory, owned, _task, stage = _layout(runtime.parent)
    if runtime.name != 'runtime' or stage != 'finalize':
        raise candidate.CandidateError('not a staged finalization runtime')
    marker = json.loads(receipts.regular(directory / 'staged-donor.json'))
    completed_raw = receipts.regular(owned / 'staged-completed.json')
    completed = json.loads(completed_raw)
    captured = json.loads(receipts.regular(directory / 'canonical.json'))
    previous = json.loads(receipts.regular(owned / 'canonical.json'))
    if (marker.get('version') != 1 or marker.get('donor_round') != owned.name
            or marker.get('donor_completion_sha256') != candidate.digest(completed_raw)
            or marker.get('current_key_sha256') != candidate.digest(candidate.canonical_json(captured['key']))
            or any(captured['key'].get(k) != completed['key'].get(k) for k in _STABLE_KEY_FIELDS)
            or captured['config_sha256'] != previous['config_sha256']):
        raise candidate.CandidateError('staged finalization donor binding changed')
    # The marker is created only after full authentication. Bind the output
    # again at use time; the donor remains host-owned and immutable throughout.
    raw = receipts.regular(owned / 'execution/response.json')
    if candidate.digest(raw) != completed['files']['execution/response.json']:
        raise candidate.CandidateError('staged proof observations changed')
    checked = json.loads(raw)
    decision = _producer_decision(directory, captured, previous, completed, checked,
                                  historical_consumer=historical_consumer)
    if marker.get('producer_compatibility') != decision:
        raise candidate.CandidateError('staged owner producer compatibility binding changed')
    if marker.get('owner_nodes') != checked.get('facts', {}).get('owner_nodes'):
        raise candidate.CandidateError('staged proof artifact roots changed')
    return owned, marker, completed


def _native(payload):
    from trellis.runtime.kernel_cli import run_kernel_cli
    result = run_kernel_cli(payload)
    if result.get('status') != 'dispatch_ok' or not isinstance(result.get('output'), dict):
        raise RuntimeError('native staged acceptance returned an unexpected response')
    return result['output']


def _capture_environment(owners):
    from trellis.atomic_actions.checker_client import _next_request_id, _round_trip
    return _round_trip(Path(os.environ['TRELLIS_CHECKER_SOCKET']),
        dict(op='capture_replay_environment', request_id=_next_request_id(),
             node_names=owners, timeout_secs=180), timeout_secs=3965)


def merge_delta(sealed, repo, captured):
    """Merge only sealed writes; native finalization checks the fact read set.

    Check every own before-byte before writing anything. Unrelated canonical
    sources remain current; no old full workspace or generated index is copied.
    """
    record, identity = candidate.read(sealed)
    for path, change in record['changes'].items():
        data = candidate.regular_bytes(repo, path)
        actual = None if data is None else candidate.digest(data)
        if actual != change['before'] or captured.get(path) != change['before']:
            raise candidate.CandidateError('candidate source conflicts with current work: ' + path)
        if path in ('Tablet/INDEX.md', 'Tablet/README.md'):
            raise candidate.CandidateError('sealed proof delta contains generated support: ' + path)
    for path, change in record['changes'].items():
        destination = Path(repo) / path
        if change['after'] is None:
            destination.unlink()
        else:
            atomic(destination, candidate.regular_bytes(Path(sealed) / 'files', path))
    return record, identity


def check(payload):
    from trellis.checker.resource_failure import host_scope
    with host_scope(payload['runtime_root']):
        return _check(payload)


def _check(payload):
    from trellis.checker.resource_failure import require_healthy, memory_phase
    from trellis.config import load_config
    from trellis.supervisor_workspace import sync_supervisor_workspace, propagate_tablet_back_to_worker
    from .replay_donation import install_via_checker, validation_fact_context

    runtime = Path(payload['runtime_root'])
    _directory, _owned, _task, stage = _layout(runtime.parent)
    if payload.get('stage') != stage:
        raise candidate.CandidateError('staged acceptance launch has the wrong stage')
    require_healthy(runtime)
    config = load_config(Path(payload['config_path']))
    repo = config.repo_path
    private = parse_private_state(receipts.regular(runtime / 'protocol_state.json'))
    record, identity = candidate.read(payload['candidate'])
    memory_phase(runtime, 'baseline', identity)
    facts, donor_hash = None, None
    if stage == 'finalize':
        previous, marker, _completed = donor(runtime)
        original = json.loads(receipts.regular(previous / 'execution/response.json'))
        facts = original.get('facts')
        donor_hash = marker['donor_completion_sha256']

    def finish(value, context_sha=None):
        require_healthy(runtime)
        result = dict(value, stage=stage, candidate_sha256=identity)
        if donor_hash is not None:
            result['donor_completion_sha256'] = donor_hash
        if context_sha is not None:
            result['context_sha256'] = context_sha
        result['checked_sources'] = receipts.checked_sources(repo)
        atomic(runtime / 'staged-result.json', candidate.canonical_json(result))
        return result

    authority = Path(sync_supervisor_workspace(repo)['authoritative_repo_path']).resolve()
    context_payload = dict(action='prepare_staged_worker_context', repo_path=str(authority),
        request=private['in_flight_request'], finalization=stage == 'finalize')
    for name, value in (('paper_source_path', config.workflow.paper_tex_path),
                        ('goal_prose_path', getattr(config, 'goal_file', None))):
        if value is not None:
            context_payload[name] = str(value)
    if stage == 'finalize':
        context_payload.update(owner_facts=facts, source_graph=payload['source_graph'], current_private=private)
    prepared = _native(context_payload)
    if prepared.get('status') == 'unsupported':
        return finish(prepared)
    if prepared.get('status') != 'prepared' or not isinstance(prepared.get('context'), dict):
        raise RuntimeError('native staged acceptance did not prepare a context')
    context = prepared['context']
    context_bytes = candidate.canonical_json(context)
    receipts.write_once(runtime / 'staged-context.json', context)
    context_sha = candidate.digest(context_bytes)
    propagate_tablet_back_to_worker(authority, repo, mirror_build=False)
    try:
        # Owned checks observe this freshly captured baseline themselves.
        # Only sealed writes require their original before-bytes; unrelated
        # sibling publications are neither copied back nor accepted as facts.
        merge_delta(payload['candidate'], repo, payload['captured_inputs'])
    except candidate.CandidateError as error:
        return finish(dict(status='cold', reason=str(error)), context_sha)
    memory_phase(runtime, 'candidate', identity)
    authority = Path(sync_supervisor_workspace(repo)['authoritative_repo_path']).resolve()
    fact_context = validation_fact_context(runtime)
    call = dict(repo_path=str(authority), acceptance_context=context, raw_payload=record['response'])
    if fact_context is not None:
        call['fact_context'] = fact_context
    installation = None
    if stage == 'owned':
        if any((runtime.parent / name).exists() for name in ('legacy-owner-donor.json', 'replay-cache-donor.json')):
            installation = install_via_checker(runtime)  # Exact imported artifacts only, after the fresh source merge.
        call.update(action='check_staged_owned_proof', source_graph=payload['source_graph'])
    else:
        positive = isinstance(facts, dict) and facts.get('owner_ok') is True
        installation = install_via_checker(runtime) if positive else None
        owners = facts.get('owner_nodes', []) if isinstance(facts, dict) else []
        capture = _capture_environment(owners) if positive and owners else {}
        environment = capture.get('environment')
        if positive and owners and environment is None:
            detail = str(capture.get('cold_reason') or '')[:240]
            failure = dict(status='cold', reason='current proof environment unavailable' + (': ' + detail if detail else ''),
                           replay_donation=installation,
                           environment_capture={key: capture[key] for key in
                               ('measurement', 'failure_class', 'failure_detail') if key in capture})
            deterministic_skips = {'no portable replay attestation', 'source or replay attestation changed',
                                   'import environment changed', 'artifact or source changed after copy',
                                   'dependency replay unavailable'}
            if (installation and installation.get('status') == 'cold'
                    and (installation.get('failure_class') == 'saved_owner_binding'
                         or (installation.get('skipped') and set(installation['skipped']) <= deterministic_skips
                             and str(installation.get('reason', '')).startswith('no replay artifacts retained:')))):
                failure['reason'] += '; replay donation: ' + str(installation.get('reason') or installation.get('skipped'))[:240]
            elif capture.get('failure_class') != 'environment_binding':
                failure['retry_class'] = 'finalization_environment_capture'
            return finish(failure, context_sha)
        call.update(action='finalize_staged_worker', current_private=private,
                    source_graph=payload['source_graph'], owner_facts=facts, current_environment=environment)
    result = _native(call)
    if installation is not None:
        # Native facts carry the complete observation-time environment. Keep
        # transfer diagnostics without duplicating its large file inventory.
        result['replay_donation'] = {key: value for key, value in installation.items()
                                    if key not in ('artifact_files', 'environment_sha256s')}
    propagate_tablet_back_to_worker(authority, repo, mirror_build=False)
    if stage == 'finalize' and result.get('status') == 'checked':
        from .enrichment import enrich
        response = result['output']['response']
        response['kind'] = 'worker'
        result['output']['response'] = enrich(runtime, response)
    return finish(result, context_sha)


def main():
    payload = json.load(sys.stdin)
    with contextlib.redirect_stdout(sys.stderr):
        result = check(payload)
    json.dump(result, sys.stdout)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
