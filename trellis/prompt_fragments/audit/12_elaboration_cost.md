## Elaboration cost

`audit_contract.node_elaboration_cost` gives each node's measured cost: `heartbeats`, `walltime_ms`, `peak_rss_kib`, and `olean_size_bytes`.

`heartbeats` counts the elaboration work a node's declarations perform, in the units `set_option maxHeartbeats` bounds. It can slightly overshoot the count `maxHeartbeats` enforcement uses but is deterministic, so it orders nodes by cost more reliably than wall time does.

`max_heartbeats_override` appears when a node grants itself more than Lean's default budget of 200000; a node without one builds under the default.

A node whose `heartbeats` runs orders of magnitude above the corpus may be a candidate for decomposition.

A node missing from the map has no measurement for the content it has now; treat its cost as unknown.
