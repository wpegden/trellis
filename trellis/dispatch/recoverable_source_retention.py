"""Opt-in failed-worker cleanup; requires deployed source_recovery readers."""
from pathlib import Path

from . import auxiliary_retention as engine, retained_files as retained, source_recovery

OUTCOMES = frozenset(('rejected', 'stuck', 'needs_restructure', 'target_false_under_model',
                     'timed_out', 'cancelled', 'launch_failed', 'stale'))


def roots(directory):
    for surface in ('Tablet', 'reference'):
        yield directory, directory / 'repo', surface


def retire(collector, directory, observed, *, apply=False, runtime_admission=None):
    _, directory = retained.owner_directory(collector.root, Path(directory))
    ineligible = dict(status='ineligible', removed=0, removed_bytes=0,
                      removed_allocated_bytes=0, retained_files=0)
    if not directory.name.isdigit():
        return ineligible
    identity = observed.get(directory.name, {})
    status = identity.get('status', {})
    outcome = status.get('outcome', {})
    if (status.get('status') != 'resolved'
            or not isinstance(outcome, dict) or outcome.get('kind') not in OUTCOMES):
        return ineligible
    index = directory / source_recovery.INDEX
    from .git_retention import _canonical_objects
    _canonical_objects(collector.repo)
    existed = index.exists() or index.is_symlink()
    def admission(witness):
        # Deployment compatibility is checked off-lock before preparing work and
        # again at every actual deletion/completion lease. It is not persisted
        # as ownership: a later compatible deployment may resume the same index.
        runtime_admission()
        return None, runtime_admission
    result = engine._retire(collector, directory, observed, apply=apply,
        roots_fn=roots, format=source_recovery.RETIREMENT_FORMAT, receipt_name=source_recovery.RECEIPT,
        git_surfaces=('Tablet', 'reference'), file_surfaces=(),
        admission=admission if runtime_admission is not None else None,
        prepare_recovery=lambda record: source_recovery.prepare_index(collector, directory, record))
    result['new_recovery_index_allocated_bytes'] = (
        retained.regular_path(index).stat().st_blocks * 512 if not existed and index.exists() else 0)
    return result
