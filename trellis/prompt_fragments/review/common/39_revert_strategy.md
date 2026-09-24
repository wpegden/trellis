## When `reset = last_commit` is the best move

Use `last_commit` when the current live state is a bad direction and another continuation would mostly spend tokens undoing or routing around unaccepted changes. Typical signs are:

- repeated invalid attempts on the same broken live delta
- a worker that drifted far from the intended focus
- a branch shape that is now harder to repair than to discard

Do not use revert reflexively. Prefer continuation when the live state is basically sound and just needs a sharper next assignment.
