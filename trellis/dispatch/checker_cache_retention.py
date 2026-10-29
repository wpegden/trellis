"""Disposable observations in fully resolved private host-check runtimes.

CheckerServer recomputes these optional caches on a miss. Probe donation only
uses earlier sibling rounds of the same task, so an individual finished round
is insufficient: the ordinary whole-owner retirement guards remain required.
Statement-transform seeds, logs, source, receipts and canonical caches remain.
"""
from pathlib import Path
import re

FORMAT = 'trellis-terminal-checker-cache-retirement/1'
OWNER_FORMAT = 'trellis-terminal-checker-cache-retirement/2'
RECEIPT = 'checker-caches-retired.json'
_KEY = re.compile(r'[a-f0-9]{64}(?:-principal-[a-f0-9]{64})?'
                  r'(?:-premises-[a-f0-9]{64})?(?:-noax)?\.json')
_NAMESPACES = ('semantic-payloads', 'local-closure-axioms-cache', 'print-axioms-cache')


def eligible_paths(directory):
    from .storage import _numbered_directories
    directory = Path(directory)
    # Only worker-owned host attempts have this contract. Typed reviewer and
    # verifier roots, arbitrary siblings and the outer worker are excluded.
    if not directory.name.isdigit():
        return
    for attempt in _numbered_directories(directory / 'acceptance'):
        for host in (attempt, *_numbered_directories(attempt / 'finalization')):
            state = host / 'runtime/checker-state'
            if any(path.is_symlink() for path in (state, *state.parents)):
                raise ValueError('private checker caches cannot traverse a symlink')
            fingerprint = state / 'sync-fingerprints.json'
            if fingerprint.exists() or fingerprint.is_symlink():
                yield fingerprint
            for namespace in _NAMESPACES:
                root = state / namespace
                if root.is_symlink():
                    raise ValueError('private checker cache directory cannot be a symlink')
                if not root.is_dir():
                    continue
                for path in sorted(root.iterdir()):
                    if _KEY.fullmatch(path.name):
                        yield path
