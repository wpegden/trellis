"""Durable orchestration for owned facts and serialized current finalization.

The caller retains the native owner/import reservation throughout this flow.
Only the existing driver owns compiler leases, pauses and publication policy.
"""
from __future__ import annotations

import json
from pathlib import Path
from trellis.private_state import parse_private_state
import re
import sys
import tempfile

from . import acceptance_receipts as receipts, candidate, failure_report, staged_acceptance as stages
from .publication import atomic
from .retained_files import present


def _read(path):
    raw = receipts.regular(path)
    return parse_private_state(raw) if Path(path).name == 'protocol_state.json' else json.loads(raw)


def _current(driver, task_id, identity):
    """Caller holds the canonical lock; no compiler call occurs here."""
    driver.require_no_halt()
    driver.native._require_recovered_publication()
    from .telemetry import phase
    measurements = getattr(driver.native, 'measurements', None)
    with phase(measurements, 'acceptance.state_read'):
        state = driver.state()
    task = state['dispatch']['tasks'][str(task_id)]
    if task['status'] != dict(status='checking', candidate_sha256=identity):
        return None
    with phase(measurements, 'acceptance.source_observation'):
        source = driver.native.observe_sources(state, driver.repo)
    private = driver.native._call(dict(action='dispatch_acceptance_state', state=state,
        task_id=task_id, graph=source['graph']))
    return task, source, private


def _plan(driver, task, source, private, identity):
    sealed = driver._submission_directory(task) / 'candidate'
    record, actual = candidate.read(sealed)
    if actual != identity:
        raise candidate.CandidateError('staged reservation candidate changed')
    if any(not path.startswith('Tablet/') for path in record['changes']):
        return dict(status='unsupported', reason='staged proof scope requires only Tablet sources')
    # Native admission consumes source text, not compiler artifacts. This
    # temporary merged view never modifies canonical or sealed source files.
    with tempfile.TemporaryDirectory(prefix='trellis-staged-plan-') as temporary:
        merged = Path(temporary)
        (merged / 'Tablet').mkdir()
        before = {}
        for path, digest in source['manifest'].items():
            if not path.startswith('Tablet/'):
                continue
            data = candidate.regular_bytes(driver.repo, path)
            if data is None or candidate.digest(data) != digest:
                raise candidate.CandidateError('staged admission source changed: ' + path)
            target = merged / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
            if len(Path(path).parts) == 2:
                before[Path(path).name] = digest
        config = candidate.regular_bytes(driver.repo, 'trellis.config.json')
        if config is not None:
            (merged / 'trellis.config.json').write_bytes(config)
        stages.merge_delta(sealed, merged, source['input_hashes'])
        result = driver.native._call(dict(action='plan_staged_worker', repo_path=str(merged),
            current_private=private, source_graph=source['graph'], before_snapshot=before,
            edit_scope=task['assignment']['scope'], raw_payload=record['response']))
        if result.get('status') not in ('planned', 'unsupported'):
            raise RuntimeError('native staged admission returned an unexpected plan')
        return result


def completed_modeling_fallback(directory):
    """Recognize only the authenticated, reaped pre-overlay decline."""
    captured = _read(directory / 'canonical.json')
    if 'staged_plan' not in captured:
        return False
    response = directory / 'execution/response.json'
    if not present(response):
        return False
    hint = _read(response)
    if hint.get('status') != 'unsupported' or hint.get('fallback_class') != 'ordinary_modeling_baseline':
        return False
    if stages.reusable(directory, captured['key']) != hint:
        raise candidate.CandidateError('staged Modeling fallback evidence changed')
    return True


def _ordinary_modeling_fallback(driver, task, source, private, identity):
    """A completed pre-overlay baseline may decline staged observation reuse.

    No candidate verdict or proof facts exist. Re-admit the unchanged packet
    under the ordinary exclusive footprint instead of misclassifying existing
    baseline errors or repeatedly starting the unsupported staged lane.
    """
    if private.get('phase') != 'Modeling':
        return False
    from .driver import acceptance_identity
    current = receipts.key(task['id'], identity, task, acceptance_identity(private), source)
    for directory in reversed(_directories(driver.root / str(task['id']) / 'acceptance')):
        response = directory / 'execution/response.json'
        if not present(response):
            continue
        hint = _read(response)
        if hint.get('status') != 'unsupported' or hint.get('fallback_class') != 'ordinary_modeling_baseline':
            continue
        captured = _read(directory / 'canonical.json')
        previous = captured['key']
        # This is only a conservative admission hint. Changed inputs/config
        # are checked afresh by the full gate; no old observation is accepted.
        fields = ('version', 'task_id', 'candidate_sha256', 'submission_turn',
                  'task_binding_sha256', 'implementation_identity')
        if any(previous.get(field) != current.get(field) for field in fields):
            continue
        return completed_modeling_fallback(directory)
    return False


def plan(driver, task_id, identity):
    """Pure current+sealed admission plan; unsupported does no staged work."""
    with driver.native.locked():
        captured = _current(driver, task_id, identity)
        if captured is None:
            return None
        if _ordinary_modeling_fallback(driver, *captured, identity):
            return dict(status='unsupported', reason='ordinary Modeling baseline checking required')
        if _legacy_rounds(driver, task_id, identity) and _select_legacy(driver, task_id, identity) is None:
            return dict(status='unsupported', reason='original host acceptance requires adoption before staged reuse')
        return _plan(driver, *captured, identity)


def _within_reservation(driver, task_id, native_plan):
    admission = _read(driver.root / str(task_id) / 'acceptance-admission.json')
    reserved = admission['footprint']
    if reserved.get('exclusive'):
        return  # Adopted legacy work conservatively owns every source read.
    reads = set(reserved['reads']) | set(reserved['writes'])
    if (not set(native_plan['read_nodes']) <= reads
            or not set(native_plan['write_nodes']) <= set(reserved['writes'])):
        raise candidate.CandidateError('staged proof inputs exceed the retained source reservation')


def _prepare(driver, task_id, identity, directory, *, owned=None, checked=None):
    from .driver import acceptance_identity
    with driver.native.locked():
        captured = _current(driver, task_id, identity)
        if captured is None:
            return None
        task, source, private = captured
        native_plan = _plan(driver, task, source, private, identity)
        if native_plan['status'] != 'planned':
            return native_plan
        _within_reservation(driver, task_id, native_plan)
        legacy = _select_legacy(driver, task_id, identity) if owned is None else None
        if owned is None and _legacy_rounds(driver, task_id, identity) and legacy is None:
            return dict(status='unsupported', reason='original host acceptance is not an authenticated artifact donor')
        driver._prepare(directory, private, host_only=True)
        key = receipts.key(task_id, identity, task, acceptance_identity(private), source)
        capture = dict(key=key, source=source,
            acceptance_sha256=key['acceptance_sha256'], staged_plan=native_plan,
            report_reconciliation=failure_report.reconciliation_capture(private),
            config_sha256=candidate.digest(driver.config_path.read_bytes()))
        if legacy is not None:
            capture['legacy_donor'] = {k: legacy[k] for k in ('donor_round', 'donor_completion_sha256')}
        receipts.write_once(directory / 'canonical.json', capture)
    if owned is not None:
        if directory.parent.parent != owned:
            raise candidate.CandidateError('finalization is outside its owned proof stage')
        stages.prepare_donor(directory, checked)
    elif legacy is not None:
        from .legacy_acceptance import prepare
        prepare(directory, legacy)
    if owned is None:
        _seed_owned_retry_artifacts(directory)
    return dict(status='prepared', directory=directory)


def _directories(root):
    return sorted((p for p in root.iterdir() if p.name.isdigit()), key=lambda p: int(p.name)) if root.exists() else []


def _legacy_rounds(driver, task_id, identity):
    result = []
    for directory in _directories(driver.root / str(task_id) / 'acceptance'):
        path = directory / 'canonical.json'
        if path.exists():
            captured = _read(path)
            if captured.get('key', {}).get('candidate_sha256') == identity and 'staged_plan' not in captured:
                result.append(directory)
    return result


def _select_legacy(driver, task_id, identity):
    if not _legacy_rounds(driver, task_id, identity):
        return None
    from .legacy_acceptance import select
    return select(driver.root / str(task_id), identity)


def _ensure_legacy_donor(driver, directory, task_id, identity):
    """Recover an interrupted marker preparation before granting a compiler."""
    captured = _read(directory / 'canonical.json')
    required = captured.get('legacy_donor')
    if required is None:
        return
    from .legacy_acceptance import prepare, resolve
    marker = directory / 'legacy-owner-donor.json'
    if not marker.exists():
        legacy = _select_legacy(driver, task_id, identity)
        if legacy is None or any(legacy.get(k) != v for k, v in required.items()):
            raise candidate.CandidateError('staged legacy donor preparation is incomplete; retain its evidence')
        prepare(directory, legacy)
    _previous, actual, _completed = resolve(directory / 'runtime')
    if any(actual.get(k) != v for k, v in required.items()):
        raise candidate.CandidateError('staged legacy donor reference changed')


def _payload(driver, directory, task_id, identity, stage):
    # Reconstruct the sealed path from this stage's immutable preparation, not
    # from a newer repair turn or an altered mutable task report.
    captured = _read(directory / 'canonical.json')
    key = captured['key']
    if key['task_id'] != task_id or key['candidate_sha256'] != identity:
        raise candidate.CandidateError('staged process belongs to a different submission')
    sealed = stages._sealed(driver.root / str(task_id), key)
    task = driver.state()['dispatch']['tasks'][str(task_id)]
    return dict(stage=stage, config_path=str(directory / 'repo/trellis.config.json'),
        runtime_root=str(directory / 'runtime'), candidate=str(sealed),
        captured_inputs=captured['source']['input_hashes'], source_graph=captured['source']['graph'],
        prepared_config_sha256=_read(directory / 'prepared.json').get('prepared_config_sha256'),
        proof_body_rebase_inputs=task['assignment'].get('proof_body_rebase_inputs', {}))


async def _execute(driver, directory, task_id, identity, stage):
    from .driver import _run_blocking
    key = _read(directory / 'canonical.json')['key']
    completed = await _run_blocking(stages.reusable_finalizer if stage == 'finalize' else stages.reusable, directory, key)
    if completed is not None:
        return completed
    if (directory / 'staged-completed.json').exists():
        raise candidate.CandidateError('completed staged acceptance changed; retain its evidence')
    if stage == 'owned':
        await _run_blocking(_ensure_legacy_donor, driver, directory, task_id, identity)
    recipe = directory / 'execution/recipe.json'
    payload = (_read(recipe)['payload'] if recipe.exists() else
               await _run_blocking(_payload, driver, directory, task_id, identity, stage))
    process, record = await _run_blocking(driver._process, directory, compiler='managed', seconds=None,
        command=[sys.executable, '-m', 'trellis.dispatch.staged_acceptance'], payload=payload,
        capacity_recovery=type(key.get('submission_turn')) is int and key['submission_turn'] > 0)
    if not (process.root / 'permit.json').exists():
        await _run_blocking(driver._permit, process, record)
    result = await driver._wait(process, record, host_check=True)
    await _run_blocking(process.stop_and_reap, record)
    await _run_blocking(stages.complete, directory, result)
    return result


def _require_facts(directory, checked):
    if checked.get('status') not in ('facts', 'rejected'):
        reason = checked.get('reason') or '; '.join(checked.get('errors', [])) or checked.get('status')
        raise RuntimeError(f'staged proof check is unavailable; submission retained: {reason}; evidence: {directory}')
    plan = _read(directory / 'canonical.json')['staged_plan']
    if not set(checked['facts']['read_nodes']) <= set(plan['read_nodes']):
        raise candidate.CandidateError('observed proof reads exceed the admitted native plan; facts retained')


def _requires_fresh_owner(directory):
    """An unreviewed producer cannot supply facts to the current finalizer.

    Retain its evidence and recheck the sealed candidate under the ordinary
    infrastructure budget. In particular, do not deliver an old finalizer's
    resource diagnostics as a current worker verdict.
    """
    producer = _read(directory / 'canonical.json')['key'].get('implementation_identity')
    return bool(producer and producer != receipts.implementation_identity()
                and producer not in stages.reviewed_owned_producers())


def _owned_capture_completion(directory):
    """Authenticate drained Cold evidence, including after a durable retry grant."""
    captured = _read(directory / 'canonical.json')
    receipt, checked = stages.authenticate(directory, captured['key'])
    if receipt != _read(directory / 'staged-completed.json'):
        raise candidate.CandidateError('owned capture completion changed')
    if (checked.get('stage') != 'owned' or checked.get('status') != 'cold'
            or 'facts' in checked):
        raise candidate.CandidateError('owned capture retry requires Cold without proof facts')
    typed = checked.get('retry_class') == 'owned_environment_capture'
    # Older hosts erased the capture cause. A new implementation may retry
    # this exact failure using a fresh native plan; it donates no observations.
    old = (checked.get('retry_class') is None
           and captured['key'].get('implementation_identity') != receipts.implementation_identity()
           and (checked.get('reason') == 'staged owner environment unavailable before successful observations'
                or (checked.get('capture_phase') == 'after_observations'
                    and checked.get('reason') == 'staged owner environment unavailable (after_observations): '
                        'import resolution changed before initial content binding')))
    if not (typed or old) or not _sealed_authored_inputs_match(directory, captured, checked):
        raise candidate.CandidateError('owned capture retry does not match retained source evidence')
    return captured, receipt, checked


def _retryable_owned(directory, checked):
    if checked.get('stage') != 'owned' or checked.get('status') != 'cold':
        return False
    try:
        return _owned_capture_completion(directory)[2] == checked
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def _metadata_scope_retry(directory, checked):
    """A changed implementation may recheck a retained metadata-scope decline.

    This donates no observations and consumes the normal native retry budget.
    The sealed submission and captured inputs must still authenticate exactly.
    """
    if (checked.get('stage') != 'owned' or checked.get('status') != 'cold'
            or checked.get('reason') != 'unsupported staged structural or trust payload'):
        return False
    try:
        captured = _read(directory / 'canonical.json')
        producer = captured['key'].get('implementation_identity')
        return bool(producer and producer != receipts.implementation_identity()
                    and stages.reusable(directory, captured['key']) == checked
                    and _sealed_authored_inputs_match(directory, captured, checked))
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def _owned_retry_donor(directory, previous):
    """Existing artifact installer only; neither a fact nor a verdict donor."""
    current = _read(directory / 'canonical.json')
    captured, completed, _checked = _owned_capture_completion(previous)
    event = _read(previous / 'retry-authorized.json')
    key = current['key']
    if (event.get('kind') != 'bounded_infrastructure_retry'
            or event.get('event', {}).get('kind') != 'check_retry'
            or event['event'].get('task_id') != key['task_id']
            or event['event'].get('candidate_sha256') != key['candidate_sha256']
            or any(key.get(name) != captured['key'].get(name) for name in stages._STABLE_KEY_FIELDS)
            or current['config_sha256'] != captured['config_sha256']
            or current['source'].get('authority_inputs') != captured['source'].get('authority_inputs')):
        raise candidate.CandidateError('owned retry artifact donor binding changed')
    marker = dict(version=1, donor_kind='owned_capture_retry', donor_round=previous.name,
        donor_completion_sha256=receipts.regular_digest(previous / 'staged-completed.json'),
        current_key_sha256=candidate.digest(candidate.canonical_json(key)),
        owner_nodes=current['staged_plan']['owner_nodes'])
    return marker, completed


def _seed_owned_retry_artifacts(directory):
    for previous in reversed(_directories(directory.parent)[-receipts._PROBE_SEED_MAX_DONORS:]):
        if (int(previous.name) >= int(directory.name) or not (previous / 'retry-authorized.json').exists()
                or not (previous / 'staged-completed.json').exists()):
            continue
        try:
            marker, _completed = _owned_retry_donor(directory, previous)
        except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
            continue  # Optional cache miss; the fresh native observations still run.
        if (directory / 'legacy-owner-donor.json').exists() and not _portable_owned_artifacts(previous, marker['owner_nodes']):
            continue  # Do not shadow a useful legacy donor with an incomplete cache.
        receipts.write_once(directory / 'replay-cache-donor.json', marker)
        return


def _portable_owned_artifacts(directory, owners):
    """Selection hint only; the installer still verifies full imported bytes."""
    from trellis.atomic_actions import observations as obs
    from .publication_artifacts import lexical_authority
    try:
        repo = lexical_authority(directory / 'repo')
        snapshot = obs.SourceClosureSnapshot(repo)
        return bool(owners) and all(
            isinstance((obs._read_olean_provenance(repo, node).get('kernel_replay') or {}).get('portable_environment_sha256'), str)
            and obs.olean_is_content_current(repo, node, source_snapshot=snapshot)
            and obs.olean_has_current_kernel_replay(repo, node) for node in owners)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def owned_retry_donor(runtime):
    directory, _owned, _task, stage = stages._layout(Path(runtime).parent)
    marker = _read(directory / 'replay-cache-donor.json')
    ordinal = marker.get('donor_round')
    if (stage != 'owned' or Path(runtime).name != 'runtime' or not isinstance(ordinal, str)
            or not ordinal.isdigit() or int(ordinal) >= int(directory.name)):
        raise candidate.CandidateError('invalid owned retry artifact donor ordinal')
    previous = directory.parent / ordinal
    expected, completed = _owned_retry_donor(directory, previous)
    if marker != expected:
        raise candidate.CandidateError('owned retry artifact donor changed')
    return previous, marker, completed


def _flatten(checked):
    return dict(checked['output'], candidate_sha256=checked['candidate_sha256'],
                checked_sources=checked['checked_sources'])


def _normalization_projection_retry(directory, captured, final, authenticated_output=None):
    """Recognize the drained support-sync failure, without accepting its work."""
    if (captured['key'].get('implementation_identity') not in stages.reviewed_owned_producers()
            or receipts.implementation_identity() == captured['key'].get('implementation_identity')
            or final.get('retry_class') is not None
            or final.get('reason') != 'staged normalization changed source; owned facts remain retained'
            or (final.get('replay_donation') or {}).get('status') != 'installed'):
        return False
    try:
        if (authenticated_output if authenticated_output is not None else stages.reusable(directory, captured['key'])) != final:
            return False
        return _sealed_authored_inputs_match(directory, captured, final)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def _owned_alias_projection_retry(directory, captured, final, authenticated_output=None):
    """Retry the known mixed-witness defect only after exact retained proof."""
    affected = {
        'trellis-worker-acceptance-v2:sha256:bb19564229a19e67703e65ebdcd7347713313ca3596b96f94812ad07a3c4f46f',
        'trellis-worker-acceptance-v2:sha256:06705d6e854df0a232b06e1db67964ee0a53d8090055355a212fd5271e70b545',
    }
    producer = captured['key'].get('implementation_identity')
    donation = final.get('replay_donation') or {}
    reason = donation.get('reason', '')
    captures = donation.get('captures')
    if (producer not in affected or producer == receipts.implementation_identity()
            or final.get('retry_class') is not None
            or donation.get('status') != 'cold' or donation.get('failure_class') != 'saved_owner_binding'
            or donation.get('copied_nodes') != [] or donation.get('covered_nodes') != []
            or not isinstance(reason, str) or not reason.startswith('saved owner imported environment changed: ')
            or final.get('reason') != 'current proof environment unavailable: import inventory unavailable or oversized; replay donation: ' + reason
            or not isinstance(captures, list) or len(captures) != 1
            or not isinstance(captures[0], dict) or captures[0].get('status') != 'captured'):
        return False
    try:
        if ((authenticated_output if authenticated_output is not None else stages.reusable(directory, captured['key'])) != final
                or not _sealed_authored_inputs_match(directory, captured, final)):
            return False
        from trellis.checker import portable_replay as portable
        from .publication_artifacts import lexical_authority
        from .replay_donation import recover_owned_alias
        environment = portable.environment_for(lexical_authority(directory / 'repo'),
                                               captures[0]['environment_sha256'])
        commitments, _relocation = recover_owned_alias(directory / 'runtime', portable.EnvironmentView(environment),
                                                       historical_consumer=producer)
        # This authorizes only an ordinary bounded CheckRetry. It neither
        # changes the saved facts nor replaces the fresh finalizer's checks.
        return bool(commitments)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def _sealed_authored_inputs_match(directory, captured, final):
    """Recovery may refresh generated docs, never authored or host input bytes."""
    try:
        _directory, _owned, task, _stage = stages._layout(directory)
        record, identity = candidate.read(stages._sealed(task, captured['key']))
        if identity != captured['key']['candidate_sha256']:
            return False
        expected = dict(_read(directory / 'base-manifest.json'))
        inputs = dict(captured['source']['input_hashes'])
        for path, change in record['changes'].items():
            if expected.get(path) != change['before'] or inputs.get(path) != change['before']:
                return False
            for inventory in (expected, inputs):
                if change['after'] is None:
                    inventory.pop(path, None)
                else:
                    inventory[path] = change['after']
        authority = captured['key']['authority_input_hashes']
        if not authority or authority != captured['source']['authority_inputs']:
            return False
        inputs.update(authority)
        inputs['trellis.config.json'] = _read(directory / 'prepared.json')['prepared_config_sha256']
        checked = final['checked_sources']
        if any(not isinstance(checked.get(name), dict) for name in ('repo', 'authority', 'inputs', 'authority_inputs')):
            return False
        # Only these two deterministic projections can be repaired. Header,
        # statements, imported sources, toolchain and host checker inputs stay
        # exact. Full source/environment checks run again in the new finalizer.
        def authored(inventory):
            return {path: digest for path, digest in inventory.items()
                    if path not in ('Tablet/INDEX.md', 'Tablet/README.md')}
        if any(authored(checked[name]) != authored(expected) for name in ('repo', 'authority')):
            return False
        if authored(checked['inputs']) != authored(inputs):
            return False
        from .publication_artifacts import lexical_authority
        marker = lexical_authority(directory / 'repo') / '.trellis-supervisor.json'
        if marker.exists():
            inputs['.trellis-supervisor.json'] = receipts.regular_digest(marker)
        return authored(checked['authority_inputs']) == authored(inputs)
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return False


def _retryable_finalization(directory, final):
    return _classify_finalization_retry(directory, final)


def _authenticate_finalization_retry(directory):
    """Read-only adoption preflight; only this wrapper supplies authenticated output."""
    captured = _read(directory / 'canonical.json')
    receipt, final = stages.authenticate(directory, captured['key'])
    saved = directory / 'staged-completed.json'
    if saved.exists() and _read(saved) != receipt:
        raise candidate.CandidateError('staged completion receipt changed')
    if not _classify_finalization_retry(directory, final, final):
        raise candidate.CandidateError('finalization is not a recoverable infrastructure Cold')
    return receipt, final


def _classify_finalization_retry(directory, final, authenticated_output=None):
    """Only authenticated finalizer infrastructure, never semantic staleness."""
    if final.get('status') != 'cold' or final.get('stage') != 'finalize':
        return False
    if final.get('retry_class') == 'finalization_environment_capture':
        return True
    # b7 predates the typed classification. Permit its exact missing-manifest
    # failure once a reviewed implementation update can actually repair it.
    captured = _read(directory / 'canonical.json')
    producer = captured['key'].get('implementation_identity')
    if _normalization_projection_retry(directory, captured, final, authenticated_output):
        return True
    if _owned_alias_projection_retry(directory, captured, final, authenticated_output):
        return True
    donation = final.get('replay_donation') or {}
    # The donor stores at most 240 diagnostic characters. Reconstruct only
    # this owned stage's expected missing-manifest path before truncating it.
    prefix = "[Errno 2] No such file or directory: '" + str(
        directory.parent.parent / 'repo/.trellis/supervisor/repo/.trellis/kernel-replay-environments') + '/'
    detail = str(donation.get('reason', ''))
    manifest = detail[len(prefix):len(prefix) + 64] if detail.startswith(prefix) else ''
    missing_manifest = (re.fullmatch('[0-9a-f]{64}', manifest) is not None
                        and detail == (prefix + manifest + ".json'")[:240])
    return (producer in stages.reviewed_owned_producers()
            and producer != receipts.implementation_identity()
            and final.get('reason') == 'current proof environment unavailable: import inventory unavailable or oversized'
            and donation.get('status') == 'cold'
            and donation.get('copied_nodes') == [] and donation.get('covered_nodes') == []
            and donation.get('captures') == [] and missing_manifest)


def _supersede_unlaunched_finalization(directory):
    captured = _read(directory / 'canonical.json')
    producer = captured['key'].get('implementation_identity')
    consumer = receipts.implementation_identity()
    if ((directory / 'staged-completed.json').exists()
            or any((directory / 'execution' / name).exists()
                   for name in ('process.json', 'permit.json', 'result.json', 'cleanup.json'))):
        return False  # A launched process must finish under its original binding.
    if not (directory / 'staged-donor.json').exists():
        # Preparation stopped before binding the donor; no check ran. Preserve
        # this partial capture and prepare again against current canonical state.
        reason = 'finalization preparation did not finish before launch'
    elif producer != consumer and producer in stages.reviewed_owned_producers():
        reason = 'implementation changed before finalization launch'
    else:
        return False
    receipts.write_once(directory / 'finalization-retry.json', dict(version=1,
        finalization=int(directory.name), reason=reason,
        producer_identity=producer, consumer_identity=consumer))
    return True


async def run(driver, task_id, identity):
    """Return False only before staged work starts; preserve completed proof facts."""
    from .driver import HostCheckFailure, _run_blocking
    attempts = driver.root / str(task_id) / 'acceptance'
    owned = None
    for previous in _directories(attempts):
        capture = previous / 'canonical.json'
        if not capture.exists():
            continue
        saved = _read(capture)
        if (saved.get('key', {}).get('candidate_sha256') == identity
                and 'staged_plan' in saved and not (previous / 'retry-authorized.json').exists()):
            owned = previous
    if owned is None:
        # _prepare captures current state and checks support, reservation and
        # any legacy donor under the native lock before creating a workspace.
        # Admission's earlier plan cannot authorize this fresh preparation.
        owned = attempts / str(max((int(p.name) for p in _directories(attempts)), default=0) + 1)
        prepared = await _run_blocking(_prepare, driver, task_id, identity, owned)
        if prepared is None:
            return True
        if prepared['status'] != 'prepared':
            return False  # Still no workspace/process/fact receipt was created.
    try:
        checked = await _execute(driver, owned, task_id, identity, 'owned')
    except HostCheckFailure as error:
        if await driver._handle_check_failure(task_id, identity, owned, str(error)):
            return True
        return await run(driver, task_id, identity)  # Existing durable retry budget bounds this path.
    if await _run_blocking(_requires_fresh_owner, owned):
        await _run_blocking(driver._retry_check, task_id, identity, owned,
            'Retained owner observations require fresh checking under the current implementation; '
            'old host outcomes establish no current candidate verdict. Candidate and evidence retained.')
        return await run(driver, task_id, identity)
    if (await _run_blocking(_retryable_owned, owned, checked)
            or await _run_blocking(_metadata_scope_retry, owned, checked)):
        await _run_blocking(driver._retry_check, task_id, identity, owned, checked['reason'])
        return await run(driver, task_id, identity)  # No facts exist; fresh plan/observations are required.
    if checked.get('status') == 'unsupported' and checked.get('fallback_class') == 'ordinary_modeling_baseline':
        admitted = driver.acceptance_queue.active.get(task_id)
        return False if admitted and admitted.exclusive else 'readmit'
    _require_facts(owned, checked)
    # Keep owner/import reservations while waiting; no semantic lock upgrade.
    # The owned compiler process has been reaped before this separate gate.
    async with driver.finalization_gate:
        finalizations = owned / 'finalization'
        rows = _directories(finalizations)
        current = next((p for p in reversed(rows) if (p / 'canonical.json').exists()
                        and not (p / 'finalization-retry.json').exists()
                        and not (p / 'retry-authorized.json').exists()), None)
        while True:
            if current is not None and await _run_blocking(_supersede_unlaunched_finalization, current):
                current = None
            if current is None:
                current = finalizations / str(max((int(p.name) for p in rows), default=0) + 1)
                prepared = await _run_blocking(_prepare, driver, task_id, identity, current,
                                               owned=owned, checked=checked)
                if prepared is None:
                    return True
                if prepared['status'] != 'prepared':
                    raise RuntimeError('current staged finalization is unsupported; proof facts retained: '
                                       + str(prepared.get('reason', 'current native contract changed')))
                rows.append(current)
            try:
                final = await _execute(driver, current, task_id, identity, 'finalize')
            except HostCheckFailure as error:
                if await driver._handle_check_failure(task_id, identity, current, str(error)):
                    return True
                current = None
                continue  # The owned proof receipt remains the same donor.
            if final.get('status') != 'checked':
                if await _run_blocking(_retryable_finalization, current, final):
                    await _run_blocking(driver._retry_check, task_id, identity, current,
                                       str(final.get('reason') or 'finalization environment capture unavailable'))
                    current = None
                    continue  # Retain the completed owned stage and its exact facts.
                raise RuntimeError('current staged finalization is unavailable; proof facts retained: '
                                   + str(final.get('reason') or final.get('errors') or final.get('status'))
                                   + '; evidence: ' + str(current))
            published = await _run_blocking(driver._publish, task_id, identity, current, _flatten(final), staged=True)
            if published is True:
                return True
            if published is None:
                raise RuntimeError('staged publication requires explicit current finalization; proof facts retained')
            await _run_blocking(receipts.write_once, current / 'finalization-retry.json',
                               dict(version=1, finalization=int(current.name),
                                    reason='current publication inputs changed'))
            current = None  # Refresh current consumers only, retaining owned facts.
