"""Redundant launch and bridge inputs after the complete owner has resolved.

These files serve launch, continuation and checking within the same owner.
Assignment/carrier seals remain for committed batch recovery. Staging raw
responses remain for future reviewer handoffs; all other evidence is untouched.
"""
from pathlib import Path

FORMAT = 'trellis-terminal-execution-input-retirement/1'
OWNER_FORMAT = 'trellis-terminal-execution-input-retirement/2'
RECEIPT = 'execution-inputs-retired.json'


def eligible_paths(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(Path(directory)):
        for path in (attempt / 'dispatch-request.json', attempt / 'execution/recipe.json',
                     attempt / 'runtime/acceptance-context.json'):
            if path.exists() or path.is_symlink():
                yield path
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            bridge = repo / '.trellis/runtime/runtime'
            for parent, pattern in ((bridge / 'staging', '*.request.json'),
                                    (bridge / 'private', '*.acceptance.json')):
                if any(path.is_symlink() for path in (parent, *parent.parents)):
                    raise ValueError('bridge inputs cannot traverse a symlink')
                yield from sorted(parent.glob(pattern))
