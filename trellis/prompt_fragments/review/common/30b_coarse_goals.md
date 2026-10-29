## Active coarse goals

`coarse_goals` lists the active commitments, their plans, progress, and pending audits. For an ordinary proof assignment, set `next_active_coarse` to the goal it serves, then choose `next_active` and its edit scope within that goal's support cone. `kernel_hinted_next_active_coarse_nodes` includes active goals and any additional goals the available capacity permits. Selecting another goal does not replace an unfinished commitment.

Finish active goals. Do not park or switch them out to revisit later. A goal releases its place when its shallow work and relevant verification are complete and its workers have finished. An unfinished goal may disappear only through an authorized structural repair that actually eliminates the obsolete node. Stalls, resets, and waiting for checks do not release its place.

Shared support is allowed. The dispatcher checks actual task conflicts and prerequisites; an unrelated cone's verifier backlog does not prevent ready work here. Closure-only assignments retain their broader eligibility and need no coarse-goal place.

For a structural problem or suspect prior report in one or more active goals, use `audit_request` with `coarse_nodes: ["GoalA", "GoalB"]`, `reason_kind`, and `reason` in the envelope's `action`: `decision=continue`, `reset=none`, with other action fields at default. Use `reason_kind=approach` or `suspect_report`; state the problem concisely. The shared audit queue waits for conflicting work; compatible work may continue. Omit `coarse_nodes` for a run-wide audit, including decisions needing global process authority.

Use per-goal plans and prior reports for mathematical findings and necessary repairs. Do not defer useful compatible work solely because a plan or an earlier reviewer says to wait. Schedule it using the current dependencies, scopes, and gates.

`request_summary.audit_request_admissible` governs run-wide audits, including their cooldown. Scoped requests use the shared queue and do not interrupt work already running. A worker audit request is advisory: forward its reason and choose the relevant active goals when appropriate. In Cleanup use `cleanup_request_reaudit` on a Done decision.

Recent per-goal reports are listed in `.trellis/coarse-audit-history/index.json` when present. To read an older report by hash, use `git show HEAD:.trellis-history/coarse-audits/<sha256>.json`. Historical reports are advice about their recorded versions; recheck their claims against current sources.

To dismiss a per-goal plan or individual tasks, use a separate non-acting `action` with `decision=continue`, `reset=none`, `next_active_coarse` naming that goal, and `dismiss_audit_plan` or `dismissed_tasks`. Wait for that goal’s current attempts and any queued or running audit to finish first. Dismissal changes the advice; it does not release the goal’s place.

Non-acting actions still include every required ordinary response field. Use `next_active=""`, `next_mode="local"`, empty blocker, authorization, and difficulty fields, `allow_new_obligations=true`, and `must_close_active=false`.
