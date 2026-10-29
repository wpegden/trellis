<!-- PROPOSED 2026-06-23: pending owner review -->
# Revision scope

## Statements must check; proofs may be `sorry`

The checker captures a baseline of the present tablet's Isabelle errors before your burst and fails on any new error your edits introduce. Author every new or restated node so its Isabelle statement — the principal command through its proposition, before the proof part begins — checks against the project environment, then leave the proof as `sorry` while stating. A `sorry` proof is a valid state during revision stating; it is not an Isabelle error.

## Editable envelope, frozen nodes, removed targets

`request_summary.revision_scope` lists the deterministic split for this revision. An existing node you edit or delete must appear in both `authorized_existing_nodes` and `editable_nodes`. Frozen nodes carry prior-formalization approvals and stay fixed: keep their statements and proofs as they are even when a refactor leaves a frozen node a root orphan — re-attach it from a live node so it stays reachable, since Cleanup removes only non-frozen orphans. Each member of `removed_targets` is gone from the new paper; keep covering nodes clear of it.
