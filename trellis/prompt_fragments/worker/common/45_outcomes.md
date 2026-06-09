## Worker outcome guidance

Use `valid` when the repository edits are a serious acceptable attempt under the current worker scope.

Use `invalid` when you made a concrete attempt under the current scope, but the result is still wrong, incomplete, malformed, or checker-rejected.

Use `stuck` when the proof is too hard to close in one attempt under the current scope. The kernel may retry, and the reviewer may broaden scope.

Use `needs_restructure` when the existing decomposition is wrong for the active task and the current set of allowed nodes doesn't allow you to fix it — for example, an imported helper's statement is too weak, the active node's signature or `.tex` needs to change, or a sibling node needs repair first. (It is not for "I want to break this task into smaller pieces", but for cases where the existing decomposition is already broken somehow.)

The kernel rolls the tablet back to the request baseline when you return `stuck`, `needs_restructure`, `invalid`, or `malformed` — your in-burst tablet edits do not survive into the next worker's baseline. Before the rollback the kernel snapshots your tablet WIP into `last_invalid/Tablet/` so the next worker can inspect what you tried (alongside your `comments` and `summary` in the metadata). You don't need to manually revert tablet edits before returning a non-valid outcome; the rollback is automatic and uniform.

The scratch workspace lives outside the tablet and is preserved across worker bursts regardless of outcome — it is the canonical handoff channel for partial proofs, attempted lemmas, experiments, or WIP notes you want carried forward. Save to scratch when you have something concrete that would help the next worker; there is no obligation if nothing materially useful surfaced.

Note: when a restructure is needed and your authorized scope is sufficient to start doing the heavy lifting, you should do it.

## Soundness Self-Check Before `valid`

If you edit or add a proof-bearing `.tex` node without a `SKETCH:` marker and return `valid`, you are claiming the relevant NL proof is expected to pass the soundness verifier. In particular, before returning `valid`, adversarially audit the proof against `SOUNDNESS.md`; where the current request and `FILESPEC.md` permit a `SKETCH:` marker for that node, if you are not able to write the proof at a level of detail that survives your adversarial audits and that you are confident will pass a strict soundness verifier lane, leave or add a `SKETCH:` marker in the file.
