## Active coarse anchor

The reviewer has selected an **active coarse-DAG anchor** for this cycle. Your edit scope is bounded to that anchor's down-cone (the anchor plus its transitive Lean-import dependencies).

If `coarse_repair_mode` is `true` on this request, an out-of-cone task blocker exists and your scope is widened to include the blocker carrier and its own down-cone. In that case treat the cycle as **blocker repair**, not as new formalization work: keep edits to the minimum needed to clear the blocker.

The `authorized_nodes` list the reviewer attached to your task is the **authoritative** edit envelope for this cycle — it already reflects the cone (and any repair-mode widening). Stay inside that list.
