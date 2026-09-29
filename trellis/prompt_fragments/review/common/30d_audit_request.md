## `audit_request` — call for a fresh StuckMathAudit

Use this to dispatch a StuckMathAudit immediately, ahead of the automatic re-audit interval, when the formalization is up against a genuine structural problem: the current approach has a flaw you cannot resolve from the reviewer seat, or an existing audit report or plan is itself wrong. Reserve it for problems at the level of the decomposition, approach, or audit quality: request an audit when the obstruction is structural — the same bar as a worker's `needs_restructure` (an already-broken decomposition).

Emit `audit_request={reason_kind, reason}` on a non-acting `decision=continue, reset=none` Continue with every action field at default (no `authorized_nodes`, no `next_active`, no blocker actions, no coarse-anchor change). Set:

- `reason_kind`: `approach` when the current formalization approach has a problem you cannot resolve; `suspect_report` when an existing audit report or plan should be re-examined.
- `reason`: a concise statement of the problem, at most 1000 characters; for `suspect_report` name the report or plan and the locus (node / task / paper reference) you want re-examined.

The kernel dispatches the audit this turn. The dispatched auditor runs the ordinary structural-blocker audit; for `suspect_report` it scrutinizes the prior plan in `previous_audit_plan_snapshot` and overturns or replaces it where its claims do not hold.

Forwarding a worker's request: when `request_summary.pending_worker_audit_request` is present, a worker has advisorily asked for an audit. Forward it by emitting your own `audit_request` (carry the worker's `reason_kind` and reason, refined with your reviewer view); proceed with ordinary routing to decline it.

`request_summary.audit_request_admissible` is true exactly when an on-demand audit can fire right now (the current phase admits a StuckMathAudit, no other audit lane is in flight, and the request cooldown has elapsed). In Cleanup, request another audit round via `cleanup_request_reaudit` on a Done decision instead.
