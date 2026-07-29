## Active coarse anchor (ProofFormalization only)

In ProofFormalization the kernel maintains an **active coarse-DAG anchor** — a single coarse node that scopes this cycle's work. While an anchor is set, every `next_active` choice you make must lie in the **anchor's down-cone** (the anchor plus its transitive Lean-import dependencies), with one exception described below. This is enforced; out-of-cone `next_active` will be rejected.

### When the anchor changes

The anchor is **locked** until BOTH of the following hold:

1. it is shallowly-closed-from-coarse (every transitive non-coarse dep is present and closed), AND
2. there are no outstanding global blockers.

When the lock opens on a clean unlock (`coarse_anchor_starvation_unlocked = false`), you must advance the anchor by setting `next_active_coarse` to one of the candidates. The kernel rejects leaving it empty. Set `next_active` in the same response — the new anchor itself if it is open, otherwise a node in its cone.

A **starvation escape** also opens the lock after `cycles_in_coarse_repair_mode >= stuck_coarse_repair_threshold` (see below) — this prevents you from being trapped by a runaway blocker chain. When that happens the request payload sets `coarse_anchor_starvation_unlocked = true`, which is your signal that switching to a different anchor is **encouraged**: the current anchor's repair work has been spinning for the threshold number of cycles without clean closure, and another coarse goal may be easier to make progress on. Clean unlocks (anchor reached shallow closure with no blockers) leave the flag `false` — those are routine "next coarse goal" transitions, no special framing needed.

### Repair-mode widening (`coarse_repair_mode = true`)

If `coarse_repair_mode` is `true`, at least one task-blocker carrier lies **outside** the current anchor's down-cone. The legal `next_active` set is then widened to include every task-blocker carrier and its own down-cone. Your job during repair-mode cycles is **repair these blockers, not new formalization work** — keep `next_mode` and `authorized_nodes` tight to what the blocker needs.

Signature changes on any coarse-DAG node still require `next_mode = coarse_restructure` regardless of the anchor, but `coarse_restructure` does **not** relax the cone-membership constraint above: you can signature-edit the current anchor or a coarse blocker in the widened set, but not an arbitrary out-of-cone coarse node.

### Picking the first anchor

When `active_coarse_node` is `none` (post-cone-clean-of-anchor recovery, or legacy state from before phase-entry seeding) the kernel surfaces every open coarse node as a candidate in `kernel_hinted_next_active_coarse_nodes`. **You must set `next_active_coarse` in this case** — the kernel rejects Continue responses that leave the anchor unset whenever a coarse DAG exists. Choose the one that best matches the natural sequence of theorems you want to prove first — typically the most depended-upon open coarse node whose dependencies are themselves tractable.

If `kernel_hinted_next_active_coarse_nodes` is empty AND `active_coarse_node` is set, the anchor is locked: leave `next_active_coarse` empty and focus on the cone (or, in repair-mode, the widened set).

### When the anchor lock blocks repair

If the active coarse anchor is shallow-closed but `ever_shallow_coarse_closed_regressed` is non-empty, the kernel rejects `next_active_coarse` mutations until the regressed nodes regain shallow closure. The regressed set is the natural target for the next worker burst. If those nodes lie outside the current cone, see the section on `global_repair_request` below for the audit-gated extension mechanism.
