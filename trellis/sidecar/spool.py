"""Daemon side of the sidecar spool (§1.3).

Directory vocabulary under ``<runtime>/sidecar/spool/``::

    pending/           daemon -> kernel (published closures; kernel claims by rename)
    claimed/           kernel-private   (mid-boundary processing)
    applied/           kernel -> daemon (terminal, verdict appended)
    rejected/          kernel -> daemon (terminal, verdict appended)
    outcomes/          daemon -> kernel (SPENT generations; kernel claims by rename)
    claimed_outcomes/  kernel-private   (mid-boundary processing)
    outcomes_consumed/ kernel -> daemon (terminal, verdict appended)
    inflight/          daemon-private   (record under construction)
    abandoned/         daemon-private   (inflight leftovers from a crash)

Ownership protocol (risk 16): the daemon writes only INTO ``pending/``
and ``outcomes/`` — and only by atomic rename out of ``inflight/`` —
and only READS the terminal dirs; the kernel alone moves files out of
``pending/`` / ``outcomes/``. A crash can therefore never publish a
half-written record: a record is published exactly by the final
``os.replace``.

The two daemon->kernel lanes carry opposite news about the same queue
entry. ``pending/`` says "this generation produced a proof, close the
node"; ``outcomes/`` says "this generation had its one attempt and it
did not close the node, so the entry is spent". Both are keyed by
``(node, entry_seq)`` and both are consumed at the kernel's boundary.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterator, List, Optional, Set


@dataclass(frozen=True)
class SpoolDirs:
    pending: Path
    claimed: Path
    applied: Path
    rejected: Path
    inflight: Path
    abandoned: Path
    outcomes: Path
    outcomes_consumed: Path


def spool_dirs(runtime_root: Path) -> SpoolDirs:
    spool = Path(runtime_root) / "sidecar" / "spool"
    return SpoolDirs(
        pending=spool / "pending",
        claimed=spool / "claimed",
        applied=spool / "applied",
        rejected=spool / "rejected",
        inflight=spool / "inflight",
        abandoned=spool / "abandoned",
        outcomes=spool / "outcomes",
        outcomes_consumed=spool / "outcomes_consumed",
    )


def ensure_spool_dirs(dirs: SpoolDirs) -> None:
    for path in (
        dirs.pending,
        dirs.claimed,
        dirs.applied,
        dirs.rejected,
        dirs.inflight,
        dirs.abandoned,
        dirs.outcomes,
        dirs.outcomes_consumed,
    ):
        path.mkdir(parents=True, exist_ok=True)


def attempt_file_name(attempt_id: str) -> str:
    return f"attempt-{attempt_id}.json"


def outcome_file_name(attempt_id: str) -> str:
    return f"outcome-{attempt_id}.json"


def publish_attempt(dirs: SpoolDirs, record: Dict[str, Any]) -> Path:
    """Publish a SUCCESS record: write into ``inflight/`` then atomic
    ``os.replace`` into ``pending/`` (same filesystem). Only successes
    are ever published — failures stay in the daemon's ledger/tried-set
    (the kernel has no use for them; K-D8 keeps it byte-inert when
    there is nothing to apply)."""
    attempt_id = str(record.get("attempt_id", ""))
    if not attempt_id:
        raise ValueError("attempt record missing attempt_id")
    name = attempt_file_name(attempt_id)
    staged = dirs.inflight / name
    staged.write_text(
        json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    published = dirs.pending / name
    os.replace(staged, published)
    return published


def publish_outcome(dirs: SpoolDirs, record: Dict[str, Any]) -> Path:
    """Publish a SPENT-GENERATION record: write into ``inflight/`` then
    atomic ``os.replace`` into ``outcomes/`` (same filesystem), exactly
    like ``publish_attempt``.

    Published for every outcome that permanently consumes a queue-entry
    generation and did NOT close the node. A ``success`` is never
    published here — its closure is already on its way through
    ``pending/``, and expiring its entry would make the kernel's apply
    gate reject that closure as ``not_queued``."""
    attempt_id = str(record.get("attempt_id", ""))
    if not attempt_id:
        raise ValueError("outcome record missing attempt_id")
    name = outcome_file_name(attempt_id)
    dirs.inflight.mkdir(parents=True, exist_ok=True)
    dirs.outcomes.mkdir(parents=True, exist_ok=True)
    staged = dirs.inflight / name
    staged.write_text(
        json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    published = dirs.outcomes / name
    os.replace(staged, published)
    return published


def sweep_inflight_to_abandoned(
    dirs: SpoolDirs, keep_attempt_ids: Optional[Set[str]] = None
) -> List[Path]:
    """Startup crash recovery (E§4.5): anything still in ``inflight/``
    was mid-construction when the daemon died. Never publish it —
    move to ``abandoned/`` for forensics.

    ``keep_attempt_ids`` are the attempts the starting daemon has just
    ADOPTED — their children are still RUNNING, so a record of theirs in
    ``inflight/`` is not crash debris but a publish in progress: this
    function would move it out from under the child's ``os.replace``,
    turning a successful closure into an ``error``. Both record kinds
    are named after the attempt (``attempt-<id>.json`` /
    ``outcome-<id>.json``), so one substring test spares both."""
    moved: List[Path] = []
    if not dirs.inflight.is_dir():
        return moved
    keep = set(keep_attempt_ids or ())
    for path in sorted(dirs.inflight.glob("*.json")):
        if any(attempt_id and attempt_id in path.name for attempt_id in keep):
            continue
        dest = dirs.abandoned / path.name
        try:
            os.replace(path, dest)
            moved.append(dest)
        except OSError:
            continue
    return moved


def iter_terminal_records(dirs: SpoolDirs) -> Iterator[Dict[str, Any]]:
    """Yield applied/rejected records (with their kernel-appended
    ``verdict``) for tried-set / ledger bookkeeping."""
    for directory, outcome in ((dirs.applied, "applied"), (dirs.rejected, "rejected")):
        if not directory.is_dir():
            continue
        for path in sorted(directory.glob("*.json")):
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            if isinstance(record, dict):
                record.setdefault("verdict", {}).setdefault("outcome", outcome)
                yield record


def pending_attempt_ids(dirs: SpoolDirs) -> List[str]:
    if not dirs.pending.is_dir():
        return []
    out = []
    for path in sorted(dirs.pending.glob("attempt-*.json")):
        out.append(path.stem[len("attempt-") :])
    return out


def pending_nodes(dirs: SpoolDirs) -> Set[str]:
    """Nodes with a published closure still awaiting kernel ingest —
    anything in ``pending/`` OR ``claimed/``.

    The kernel applies at most one closure per boundary, so a success can
    sit in ``pending`` for several cycles while its queue entry is still
    exported as ``ready``. Re-attempting such a node burns a grunt slot to
    produce a duplicate record the eligibility gate then refuses, so the
    assignment pass skips it. Pending is transient in both directions —
    the record leaves for ``applied`` or ``rejected`` — so this can never
    park a node permanently.

    BOTH LANES, deliberately: this is the daemon-side mirror of the
    kernel's ``sidecar::nodes_awaiting_closure_ingest``, which iterates
    ``pending`` and ``claimed`` together, and the two must agree or the
    skip they implement has a hole. ``claimed/`` is where the kernel
    parks a record it has taken but not yet dispositioned, so with one
    apply per boundary EVERY closure spends at least one boundary there,
    and a crash mid-boundary leaves it there until the next sweep.
    Reading ``pending/`` alone made that window invisible to the daemon,
    which then re-assigned the node — the duplicate this skip exists to
    prevent, and a class that has already fired in production."""
    out: Set[str] = set()
    for directory in (dirs.pending, dirs.claimed):
        if not directory.is_dir():
            continue
        for path in sorted(directory.glob("attempt-*.json")):
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                continue
            node = record.get("node") if isinstance(record, dict) else None
            if isinstance(node, str) and node:
                out.add(node)
    return out


def read_candidates(runtime_root: Path) -> Optional[Dict[str, Any]]:
    """Read the kernel-exported ``candidates.json`` (or None). The
    daemon treats it as read-only truth for eligibility and NEVER
    derives eligibility from ``protocol_state.json``."""
    path = Path(runtime_root) / "sidecar" / "candidates.json"
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def candidates_mtime(runtime_root: Path) -> Optional[float]:
    path = Path(runtime_root) / "sidecar" / "candidates.json"
    try:
        return path.stat().st_mtime
    except OSError:
        return None


def export_is_stale(
    runtime_root: Path,
    stale_after_seconds: float,
    now: Optional[float] = None,
) -> bool:
    """Staleness signal (E§4.2, relocated here from the retired
    scheduler): an export older than the configured window means the
    supervisor is down or wedged. The manager then neither ASSIGNS nor
    CANCELS from the frozen view (audit A2) — reaping continues."""
    import time as _time

    mtime = candidates_mtime(runtime_root)
    if mtime is None:
        return True
    now = _time.time() if now is None else now
    return (now - mtime) > stale_after_seconds
