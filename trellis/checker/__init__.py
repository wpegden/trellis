"""Unified checker server (Step 1 — server scaffolding only).

The supervisor-side checker server centralises all `lake`/`lean` invocations
that today fan out across worker bursts and supervisor pre-warm paths. The
unified architecture eliminates the cache-divergence cheat-detection layer,
because the only deterministic compile target is the supervisor workspace.

This package is feature-flagged off in production: it is callable via
`python3 -m trellis.checker.server <runtime_root>` for isolated testing,
but no live runtime currently launches it. The client-side cutover (worker
`check.py` routing to the socket) lands in a follow-up PR.

Module layout:

- ``trellis.checker.protocol`` — typed JSON request/response envelopes plus
  validation (node-name regex, length caps, path containment). The single
  source of truth for the node-name regex.
- ``trellis.checker.sync`` — fingerprint-cached `worker_repo` → `supervisor_repo`
  copy. Replaces ``supervisor_workspace._copy_file_if_needed`` with an
  ``O_NOFOLLOW``-aware path that rejects symlinks and ``st_nlink > 1`` sources.
- ``trellis.checker.server`` — UNIX-socket dispatcher. Acceptor thread,
  per-connection handler, workspace ``RLock``-guarded sync+lake, append-only
  request log.
"""
