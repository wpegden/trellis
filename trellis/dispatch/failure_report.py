"""Native delivery of empty closure reports, without a proof reservation.

Current-producer PF results reconcile this report bookkeeping natively.
Older checks and exclusive routing seeds retain finalization-gate sequencing.
No candidate source, artifact or successful proof observation is installed.
"""
import json

from . import acceptance, acceptance_receipts, candidate
from .publication import atomic


def request_review(payload, task, raw):
    """Only the authenticated worker-declared closure failure changes policy."""
    response = payload['response']
    if (task['assignment'].get('task_kind') == 'lean_closure'
            and isinstance(raw, dict) and raw.get('outcome') == 'stuck'
            and response.get('status') == 'Ok' and response.get('outcome') == 'Stuck'
            and not response.get('transport_failure') and not response.get('preflight_error')
            and not response.get('acceptance_check_passed')):
        payload['closure_stuck_to_review'] = True
        payload['submitted_outcome'] = 'Stuck'


def _candidate(driver, task_id, identity):
    state = driver.state()
    task = state['dispatch']['tasks'][str(task_id)]
    if task['status'] != dict(status='checking', candidate_sha256=identity):
        return state, task, None
    if (state['phase'] != 'ProofFormalization' or task['assignment']['task_kind'] != 'lean_closure'
            or task['assignment']['compiler'] != 'managed'):
        return state, task, None
    sealed = driver._submission_directory(task) / 'candidate'
    record, actual = candidate.read(sealed)
    if actual != identity or record['task_id'] != task_id:
        raise candidate.CandidateError('empty report differs from its native Checking candidate')
    attempt = task.get('worker_attempt')
    if attempt is not None and (record.get('submission') or {}).get('turn') != attempt['turn']:
        raise candidate.CandidateError('empty report belongs to a different submission turn')
    if attempt is not None and record['submission'].get('contract_sha256') != attempt['contract_sha256']:
        raise candidate.CandidateError('empty report submission contract changed')
    raw = record.get('response')
    if record['changes'] or not isinstance(raw, dict) or raw.get('outcome') != 'stuck':
        return state, task, None
    return state, task, record


def eligible(driver, task_id, identity):
    """Pure early selection; delivery repeats all bindings after gate wait."""
    root = driver.root / str(task_id)
    if not (root / 'candidate/candidate.json').exists() and not any(root.glob('turns/*/candidate/candidate.json')):
        return False
    with driver.native.locked():
        driver.native._require_recovered_publication()
        # Existing check processes/receipts keep their ordinary adoption path.
        if any((driver.root / str(task_id) / 'acceptance').glob('*/canonical.json')):
            return False
        return _candidate(driver, task_id, identity)[2] is not None


def reconciliation_capture(private):
    """Compact host-only compatibility fact, derived while capturing inputs.

    This is not proof evidence. Absent/older captures keep report sequencing;
    eligibility must not reread a potentially enormous private protocol state.
    """
    request = private.get('in_flight_request') or {}
    supported = (private.get('phase') == 'ProofFormalization'
        and private.get('stage') == 'Worker' and bool(private.get('worker_execution'))
        and request.get('assumption_authoring') is None
        and not (private.get('live') or {}).get('modeling_draft')
        and request.get('kind') == 'Worker'
        and (request.get('worker_acceptance') or {}).get('validation_kind') in
            ('ProofEasy', 'ProofLocal', 'ProofRestructure'))
    return dict(version=1, supported=supported,
                implementation_identity=request.get('acceptance_logic_identity'))


def _concurrent(driver, state):
    """Do not invalidate an older retained check or a global routing seed.

    This selection runs under the canonical lock, as does acceptance capture.
    New checks therefore capture either before or after this native report.
    Historical receipts are only inspected, never promoted as current evidence.
    """
    if state.get('phase') != 'ProofFormalization' or state['dispatch'].get('exclusive_workers'):
        return False
    producer = acceptance_receipts.implementation_identity()
    for task in state['dispatch']['tasks'].values():
        if task['status']['status'] != 'checking':
            continue
        root = driver.root / str(task['id']) / 'acceptance'
        paths = list(root.glob('*/canonical.json')) + list(root.glob('*/finalization/*/canonical.json'))
        if not paths:
            continue  # No captured result to invalidate; future capture owns this lock.
        assignment = task['assignment']
        if (assignment.get('compiler') != 'managed'
                or assignment.get('task_kind') not in ('ordinary', 'lean_closure')
                or assignment.get('scope', {}).get('shared_writes')):
            return False
        record, identity = candidate.read(driver._submission_directory(task) / 'candidate')
        if identity != task['status'].get('candidate_sha256') or record['task_id'] != task['id']:
            raise candidate.CandidateError('retained check candidate changed before empty report')
        if (record.get('response') or {}).get('outcome') != 'valid':
            return False  # Only Valid→Valid/Invalid normalization is covered.
        for path in paths:
            captured = json.loads(acceptance_receipts.regular(path))
            if captured.get('key', {}).get('implementation_identity') != producer:
                return False
            if not captured.get('staged_plan'):
                # Nonexclusive legacy checking can outlive the finalizer gate,
                # including real audit delivery. Keep its existing sequencing.
                admitted = driver.acceptance_queue.active.get(task['id'])
                if not (assignment.get('scope', {}).get('exclusive')
                        or admitted is not None and admitted.exclusive):
                    return False
            compatibility = captured.get('report_reconciliation')
            if compatibility != dict(version=1, supported=True, implementation_identity=producer):
                return False
    return True


def deliver(driver, task_id, identity, *, concurrent=False):
    """Short native commit, or False to retain the existing gated path."""
    driver.require_no_halt()
    with driver.native.locked():
        driver.native._require_recovered_publication()
        state, task, record = _candidate(driver, task_id, identity)
        if concurrent and not _concurrent(driver, state):
            return False
        if task['status'] != dict(status='checking', candidate_sha256=identity):
            return True  # Another current native disposition already won.
        if record is None:
            return False
        source = driver.native.observe_sources(state, driver.repo)
        # A report about old dependency bytes stays historical. No prefix
        # equivalence can turn that old failure into a current proof judgment.
        try:
            acceptance.validate_overlay(record, driver.repo, source['input_hashes'])
        except candidate.CandidateError as error:
            driver.native._call(dict(action='dispatch_control', root=str(driver.runtime), event=dict(
                kind='attempt_stopping', task_id=task_id,
                outcome=dict(kind='stale', reason=str(error)))))
            return True
        scope = task['assignment']['scope']
        try:
            candidate.check_scope(record, nodes=scope['nodes'], helper_prefixes=scope['helper_prefixes'],
                shared_paths=scope['shared_writes'], exclusive=scope['exclusive'], compiler='managed')
            candidate.check_lean_closure(driver._submission_directory(task) / 'candidate',
                driver.repo, next(iter(scope['nodes'])))
        except candidate.CandidateError:
            return False  # Ordinary scope rejection retains its existing diagnostics/budget.
        # The existing native constructor independently checks the restricted
        # raw contract, scope/source shape and current policy without a checker.
        private = driver.native._call(dict(action='dispatch_acceptance_state', state=state,
            task_id=task_id, graph=source['graph']))
        result = driver.native._call(dict(action='check_unchanged_closure_stuck', repo_path=str(driver.repo),
            request=private['in_flight_request'], baseline=private['live'], raw_payload=record['response']))
        # NativeLedger._call unwraps dispatch_ok output, including a cold null.
        if result is None:
            return False
        response = result.get('response') or {}
        request = private['in_flight_request']
        if (not result.get('ok') or response.get('status') != 'Ok' or response.get('outcome') != 'Stuck'
                or response.get('acceptance_check_passed') or response.get('request_id') != request['id']
                or response.get('cycle') != request['cycle']):
            raise RuntimeError('native empty Stuck report returned an inconsistent failure')
        # Fresh byte guards after native source parsing; source writers remain
        # excluded by the canonical lock, not by a mathematical read lease.
        current = driver.native.observe_sources(state, driver.repo)
        if current != source:
            raise candidate.CandidateError('source changed during native empty report validation')
        payload = dict(task_id=task_id, candidate_sha256=identity, base_state_sha256='',
            decided_at_ms=driver.now_ms(), inputs=source['input_hashes'],
            proof_body_rebase_inputs=source.get('proof_body_rebase_inputs', {}), actual_scope=scope,
            graph_before=source['graph'], graph_after=source['graph'], response=response,
            submitted_outcome='Stuck', closure_stuck_to_review=True)
        payload = driver.native.prepare_publication(payload)
        receipt = dict(version=1, kind='empty_closure_stuck', candidate_sha256=identity,
            input_sha256=candidate.digest(candidate.canonical_json(source['input_hashes'])),
            checked=result, publication=payload)
        receipt_hash = candidate.digest(candidate.canonical_json(receipt))
        acceptance_receipts.write_once(driver.root / str(task_id) / 'reports' / (receipt_hash + '.json'), receipt)
        driver.require_no_halt()
        if task.get('worker_attempt') is not None:
            atomic(driver._submission_directory(task) / 'feedback.json', candidate.canonical_json(payload))
        driver.native.commit_failure(payload)
        return True
