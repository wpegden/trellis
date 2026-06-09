## `global_repair_request` — audit-gated cone extension

Use ONLY when no choice of `(next_active, next_mode)` inside the current coarse cone covers the edit you need, and the edit does NOT touch any protected statement node (paper-target covering nodes + their type-surface closure). This remains legal and intended on retry reviews such as `Stuck` or `NeedsRestructure`.

**Step A (this cycle)**: emit `global_repair_request={proposed_extension_node_ids: [...], reason: "..."}` with `decision=continue`, `reset=none`, every action field default (no authorized_nodes, no next_active, no task_blockers). The kernel routes to a StuckMathAudit burst that confirms or declines the request. For process-routing/scope impasses, use this global-repair route.

**Step B (audit cycle, automatic)**: the auditor either approves a subset of the dep-neighborhood of your proposal, or declines with a reason surfaced via `latest_global_repair_audit_decline_reason` on the next Review request.

**Step C (post-approval cycle)**: with `pending_global_repair_grant` visible on the request, emit a normal Restructure/CoarseRestructure Continue with `consume_global_repair_grant=true` and `authorized_nodes` drawn from `grant.approved_extension_nodes` (in addition to the usual envelope). The kernel exempts those nodes from the cone check. If the review is still in retry context, consuming the grant dispatches the retry worker with the widened authorization and preserves retry accounting. The active coarse anchor stays unchanged.

Rate limit: at most one Step A per `stuck_math_audit_dispatch_cooldown_cycles` while a prior grant is pending or a recent decline reason is in scope.

Anchor lock: a Step C burst does NOT reset the anchor staleness counter. Bursts that broaden the shallow-closed-coarse regression set will pin the anchor until the regression is repaired (or the starvation guard fires).
