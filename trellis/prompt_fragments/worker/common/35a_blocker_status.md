## Live blocker status (situational awareness; contract controls legality)

The guidance above describes the concrete task for this burst. The block below is the live blocker set the verifier lanes are currently tracking, provided for situational awareness. The actionable subset (when present) shows the blockers most directly tied to the active node and its direct-dep neighborhood.

When the live blocker count is small, every blocker is listed inline. When the count exceeds the inline threshold, only the actionable subset is inline and the full structured blocker list is written to a sidecar file (path shown in the block); read it with `jq` when a reviewer comment references a blocker not in the inline subset.

{{blocker_status_block}}
