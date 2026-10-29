"""Optional scheduling groups; never an authority for semantic observations."""
from __future__ import annotations

from pathlib import Path
from typing import Sequence


def plan_groups(repo: Path, nodes: Sequence[str]) -> list[tuple[str, list[str]]]:
    """Choose requested roots whose source imports suggest useful sharing.

    The collector imports ONLY the chosen root and validates exact principal
    ownership in Lean's actual environment. Missing members fall back to single
    probes. Source parsing here cannot authorize an observation or cache entry.
    """
    from trellis.atomic_actions.observations import SourceClosureSnapshot

    try:
        snapshot = SourceClosureSnapshot(repo)
        closures = {}
        memberships = 0
        for node in nodes:
            closure = set(snapshot.materialization_order([node]))
            memberships += len(closure)
            if memberships > snapshot.MAX_CLOSURE_MEMBERS:
                return []
            closures[node] = closure
        if not snapshot.is_current():
            return []
    except (OSError, ValueError, RecursionError):
        return []
    except Exception:
        # Includes bounded-snapshot overflow: optimization is always optional.
        return []
    remaining = set(nodes)
    groups = []
    while remaining:
        root = max((node for node in nodes if node in remaining),
                   key=lambda node: len(closures[node] & remaining))
        members = [node for node in nodes if node in remaining and node in closures[root]]
        if len(members) < 2:
            break
        groups.append((root, members))
        remaining.difference_update(members)
    return groups
