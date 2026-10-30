## Pending node retirement (audit-ordered)

{{node_retirement_block}}

These are nodes the audit has decided are doing more harm than good, they should be removed so the run can have a fresh start at the hard proof obligations. Dispatch this as the next worker task: `dispatch_node_retirement=true` on a `decision=continue, reset=none` Continue with `next_mode=restructure` or `coarse_restructure`, `allow_new_obligations=false`, `must_close_active=false`, and `authorized_node_ids` covering every listed node plus any surviving consumers to repair.

To decline, set `node_retirement_decline_reason`; the decline is recorded and surfaced to the next audit.
