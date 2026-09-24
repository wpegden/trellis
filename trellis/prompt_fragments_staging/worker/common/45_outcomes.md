## Worker outcome guidance

Use `valid` when the repository edits are a serious acceptable attempt under the current worker scope.

Use `invalid` when you made a concrete attempt under the current scope, but the result is still wrong, incomplete, malformed, or checker-rejected.

Use `needs_restructure` when the existing decomposition is wrong for the active task and the current set of allowed nodes doesn't allow you to fix it.

Use `stuck` when you have been unable to make any progress but not because of scope limitations (when scope is the problem, you return `needs_restructure`). This is the outlet valve to be used if, for example, the paper being formalized has a genuine unfixable mathematical error.

The kernel rolls the tablet back to the request baseline when you return `stuck`, `needs_restructure`, `invalid`, or `malformed` — your in-burst tablet edits do not survive into the next worker's baseline. Before the rollback the kernel snapshots your tablet WIP into `last_invalid/Tablet/` so the next worker can inspect what you tried (alongside your `comments` and `summary` in the metadata). You don't need to manually revert tablet edits before returning a non-valid outcome; the rollback is automatic and uniform.

The scratch workspace lives outside the tablet and is preserved across worker bursts regardless of outcome — it is the canonical handoff channel for partial proofs, attempted lemmas, experiments, or WIP notes you want carried forward. Save to scratch when you have something concrete that would help the next worker; there is no obligation if nothing materially useful surfaced.

Note: when a DAG restructure is needed and your authorized scope is sufficient to start doing the heavy lifting, you should get to work on that heavy lifting.
