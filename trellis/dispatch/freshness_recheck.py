"""One fresh host check when a successful legacy result cannot be published.

This never reuses a stale verdict. The original round remains immutable and
the next round must perform ordinary current-state acceptance and publication.
The durable limit is per sealed submission, including coordinator restarts.
"""
from __future__ import annotations

import json

from . import acceptance_receipts as receipts, candidate

MARKER = 'freshness-recheck.json'


def superseded(directory, identity):
    """Validate a retained transition before skipping its original round."""
    path = directory / MARKER
    if not path.exists():
        return False
    marker = json.loads(receipts.regular(path))
    saved = json.loads(receipts.regular(directory / 'completed.json'))
    original = saved['key']
    current = marker.get('current_key', {})
    if (marker.get('version') != 1 or marker.get('kind') != 'fresh_current_context_check'
            or marker.get('candidate_sha256') != identity
            or original.get('candidate_sha256') != identity
            or marker.get('original_key') != original
            or marker.get('completed_sha256') != receipts.regular_digest(directory / 'completed.json')
            or marker.get('next_round') != int(directory.name) + 1
            or original.get('acceptance_sha256') == current.get('acceptance_sha256')
            or dict(original, acceptance_sha256=current.get('acceptance_sha256'),
                    implementation_identity=current.get('implementation_identity')) != current):
        raise candidate.CandidateError('freshness recheck receipt changed')
    actual, _ = receipts.authenticate(directory, original)
    if actual != saved:
        raise candidate.CandidateError('superseded host completion changed')
    return True


def authorize(driver, task_id, identity, directory, checked, *, apply=True):
    """Called after ordinary reuse/refresh refused, under finalization_gate."""
    from .driver import acceptance_identity

    with driver.native.locked():
        driver.require_no_halt()
        driver.native._require_recovered_publication()
        state = driver.state()
        task = state['dispatch']['tasks'][str(task_id)]
        if task['status'] != dict(status='checking', candidate_sha256=identity):
            return False
        captured = json.loads(receipts.regular(directory / 'canonical.json'))
        original = captured.get('key')
        if (not original or checked.get('response', {}).get('status') != 'Ok'
                or checked.get('response', {}).get('outcome') != 'Valid'
                or captured.get('config_sha256') != candidate.digest(driver.config_path.read_bytes())):
            return False
        actual, authenticated = receipts.authenticate(directory, original)
        saved = json.loads(receipts.regular(directory / 'completed.json'))
        if actual != saved or authenticated != checked:
            raise candidate.CandidateError('host completion changed before freshness recheck')
        # Any previous transition for this candidate consumes the budget, even
        # after a crash, another infrastructure retry, or an implementation update.
        for previous in directory.parent.iterdir():
            if previous.name.isdigit() and (previous / MARKER).exists():
                marker = json.loads(receipts.regular(previous / MARKER))
                if marker.get('candidate_sha256') == identity:
                    superseded(previous, identity)
                    return False
        source = driver.native.observe_sources(state, driver.repo)
        private = driver.native._call(dict(action='dispatch_acceptance_state', state=state,
            task_id=task_id, graph=source['graph']))
        current = receipts.key(task_id, identity, task, acceptance_identity(private), source)
        # Implementation drift is permitted only because the new round performs
        # a full check. Source, authority, task, binding and submission stay exact.
        if (original['acceptance_sha256'] == current['acceptance_sha256']
                or dict(original, acceptance_sha256=current['acceptance_sha256'],
                        implementation_identity=current['implementation_identity']) != current):
            return False
        transition = dict(version=1,
            kind='fresh_current_context_check', candidate_sha256=identity,
            original_key=original, current_key=current,
            completed_sha256=receipts.regular_digest(directory / 'completed.json'),
            next_round=int(directory.name) + 1,
            reason='Successful legacy check retained; publication context changed and reuse was unavailable.')
        if apply:
            receipts.write_once(directory / MARKER, transition)
        return True


def legacy_skip(directory, identity):
    """Explicit stopped-run recovery for older drivers' round-skip interface.

Call only while owning the coordinator, with its pause retained. This is a
fresh-context transition, not a failed-host retry or a worker failure event.
The old driver already skips this filename before adopting a process, and its
exact-key cache refuses the superseded round. No proof result is authorized.
"""
    if not superseded(directory, identity):
        raise candidate.CandidateError('legacy round skip lacks authenticated freshness transition')
    receipts.write_once(directory / 'retry-authorized.json', dict(version=1,
        kind='fresh_current_context_check', candidate_sha256=identity,
        freshness_recheck_sha256=receipts.regular_digest(directory / MARKER)))
