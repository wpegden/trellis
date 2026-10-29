"""Disposable private history indexes after their complete owner has resolved.

Fresh workspaces copy canonical burst history and reconstruct parallel chat rows
from retained execution/chat evidence. No future handoff reads the old private
index. Advisory scratch/audit trees and actual chats are deliberately excluded.
"""
from pathlib import Path

FORMAT = 'trellis-terminal-history-retirement/1'
OWNER_FORMAT = 'trellis-terminal-history-retirement/2'
RECEIPT = 'history-indexes-retired.json'


def eligible_paths(directory):
    from .storage import _attempt_directories
    for attempt in _attempt_directories(Path(directory)):
        for repo in (attempt / 'repo', attempt / 'repo/.trellis/supervisor/repo'):
            path = repo / '.trellis/logs/burst-history.jsonl'
            if path.exists() or path.is_symlink():
                yield path
