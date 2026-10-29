"""Recoverable host copies after the complete worker task has resolved.

Cross-owner retries/findings still need the OUTER worker repo and candidates.
Host acceptance/finalization and nested supervisor source copies serve only
same-task checking. Remove only exact canonical-Git matches from those copies.
"""
from pathlib import Path

from . import auxiliary_retention, retained_files

FORMAT = 'trellis-terminal-worker-checkout-retirement/1'
RECEIPT = 'worker-checkouts-retired.json'


def roots(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(directory):
        repos = [attempt / 'repo/.trellis/supervisor/repo']
        relative = attempt.relative_to(directory)
        if relative.parts and relative.parts[0] == 'acceptance':
            repos.insert(0, attempt / 'repo')
        for repo in repos:
            for surface in ('Tablet', 'reference'):
                yield attempt, repo, surface


def retire(collector, directory, observed, *, apply=False):
    _, directory = retained_files.owner_directory(collector.root, Path(directory))
    if retained_files.owner_identity(directory)['kind'] != 'worker':
        return dict(status='nonworker_preserved', removed=0, removed_bytes=0,
                    removed_allocated_bytes=0, retained_files=0)
    return auxiliary_retention._retire(collector, directory, observed, apply=apply,
        roots_fn=roots, format=FORMAT, receipt_name=RECEIPT,
        git_surfaces=('Tablet', 'reference'), file_surfaces=())
