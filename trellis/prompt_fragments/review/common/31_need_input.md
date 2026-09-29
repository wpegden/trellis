## `NEED_INPUT`

Reserve `NEED_INPUT` for cases where the paper being formalized appears to have fundamental gaps that cannot be closed or worked around within the formalization scope.

## `NEED_INPUT` mechanics

For `NEED_INPUT`, write `reason` as the authoritative short summary the
NeedInputAuditor should start from. Use `comments` only for additional detail.

`NEED_INPUT` will escalate to an audit agent that will decide whether human review is truly needed or whether there is a path to get formalization back on track.

- Leave `task_blocker_ids`, `reset_blocker_ids`, and `request_sound_verifier_node_ids` empty. The kernel rejects a NeedInput response that carries any blocker action. After the human responds you will be re-issued with `human_input_outstanding=true` and can adjudicate blockers there.
- Leave `next_active` empty and keep `next_mode` equal to the current request mode.

If you also use a whole-state `reset` (`last_commit` or `last_clean`), all blocker-id lists must be empty.
