## Available blocker ids

When the live blocker count exceeds the inline threshold, the index table below shows only the actionable subset (blockers touching the current `active_node` / `held_target`) and the full structured blocker list is written to a sidecar file (the path is shown in the block). Use the sidecar IDs only for actions you are taking in this transition. Omitted blockers remain live and will resurface; there is no complete-partition requirement.

{{blocker_choices_summary}}
