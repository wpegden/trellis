"""Supervisor-side active-node prewarm server (the "hybrid" design).

A persistent, supervisor-owned, NON-AUTHORITATIVE ``lake env lean --server``
that keeps EXACTLY ONE node warm: the supervisor's current active node. Its
sole purpose is to pay the cold elaboration of a (possibly giant) active node
OFF the worker's clock, before/around burst dispatch, so the worker's VERY
FIRST ``incremental-check`` on that node is already warm.

This is the supervisor-side complement to the in-burst broker in
``trellis.incremental_check`` (the general engine that survives across calls
WITHIN a burst). The two compose:

  worker ``incremental-check <node>`` preference order
    1. ``<node>`` is the active node AND the supervisor warm server is
       reachable -> use it (fast first call, cold cost already paid);
    2. else the in-burst broker (warm across calls within the burst);
    3. else ``lake build``.
  Every step is failure-open.

Shape that fixes the NibblePlus hang
------------------------------------
The parked full cross-burst preview service held the WHOLE import cone as live
open documents and LRU-evicted in-flight dependencies under a memory budget;
on the ~16-minute giant node that wedged. This service is deliberately the
configuration the standalone study completed that node with:

  * SINGLE node only — exactly one ``didOpen``, never the cone as live docs;
  * dependencies resolved from OLEANS via the normal ``lake env lean --server``
    LEAN_PATH, never held open;
  * NO pool, NO LRU eviction (structurally impossible to evict an in-flight
    dependency because no dependency is ever an open document).

Reuse, not reinvention
----------------------
The LSP plumbing, server-reliability config (``LEAN_STACK_SIZE_KB``,
``LEAN_NUM_THREADS=1``, unlimited stack), the ``set_option`` injection with
diagnostic-offset correction, the fileProgress-terminal wait with the
``saw_processing`` latch (the false-early-complete fix), ``sorry``-as-INFO
(mirroring ``lake build``, which exits 0 on a ``sorry`` warning),
and foreign-URI handling all come straight from ``trellis.incremental_check``
(the hardened in-burst broker). This module adds only: single-node warm
lifecycle, one-node content-keyed coherence, and a unix-socket front end.

Correctness boundary (non-negotiable)
-------------------------------------
Advisory only. The supervisor NEVER reads these diagnostics for acceptance.
The deterministic worker check (``trellis.checking`` ``check_node`` /
``check_tablet`` via the checker socket) is the SOLE sign-off gate. Worst case
is a false green wasting worker time; the real checker then rejects.
"""

from __future__ import annotations

from trellis.active_node_prewarm.config import ActiveNodePrewarmConfig
from trellis.active_node_prewarm.server import ActiveNodePrewarmServer

__all__ = ["ActiveNodePrewarmConfig", "ActiveNodePrewarmServer"]
