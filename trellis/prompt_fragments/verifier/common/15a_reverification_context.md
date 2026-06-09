## Re-verification context

This Sound target was previously approved by your lane. The kernel is re-issuing it because the soundness fingerprint changed: either the target's own NL body changed (`own_tex_changed`), or one or more dep statements changed (`deps_changed`).

Facts only — the kernel does not instruct you how to weight this evidence. You must judge the current files and the current kernel-authored request. The prior accepted-lane explanation for THIS target is included verbatim under `prior_lane_evidence` (keyed by LaneId). The cross-target most-recent finding from your lane remains in the `previous_own_findings` block above.

Dep hashes are truncated to 12 hex characters for display. To inspect actual prior content (rather than the hash), use the git-access hint in the JSON block — the repository is mounted read-only inside this sandbox and `cycle-N` tags exist for every committed cycle.

{{reverification_context_json}}
