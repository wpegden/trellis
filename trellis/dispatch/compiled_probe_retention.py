"""Regenerable checker programs belonging to a terminal private workspace.

Only CheckerServer's own CompiledClosure/SemanticProbe reads these files. Fresh
workspaces and retry donors do not consume predecessor compiled-probe trees.
Pinned source collectors, checker observations and arbitrary siblings remain.
This scope is terminal-only: partial retirement is not a usable compiler cache.
"""
from pathlib import Path
import re

FORMAT = 'trellis-terminal-compiled-probe-retirement/1'
OWNER_FORMAT = 'trellis-terminal-compiled-probe-retirement/2'
RECEIPT = 'compiled-probes-retired.json'
_KEY = re.compile(r'[a-f0-9]{64}')


def eligible_paths(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(Path(directory)):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            for kind, prefix, runner in (('closure', 'TrellisClosure', 'RunClosure.lean'),
                                         ('semantic', 'TrellisSemantic', 'RunSemantic.lean')):
                root = repo / f'.trellis/{kind}-collectors/compiled-v1'
                if any(path.is_symlink() for path in (root, *root.parents)):
                    raise ValueError('compiled probes cannot traverse a symlink')
                if not root.exists():
                    continue
                for cache in sorted(root.iterdir()):
                    if not _KEY.fullmatch(cache.name):
                        continue
                    if cache.is_symlink():
                        raise ValueError('compiled probe directory cannot be a symlink')
                    if not cache.is_dir():
                        continue
                    module = f'{prefix}_{cache.name}'
                    # Enumerate by the immutable pathname contract, not the
                    # receipt: a previous cleanup may already have removed it.
                    for name in (f'{module}.lean', f'{module}.olean',
                                 f'{module}.olean.server', f'{module}.olean.private',
                                 runner, 'receipt.json'):
                        path = cache / name
                        if path.exists() or path.is_symlink():
                            yield path
