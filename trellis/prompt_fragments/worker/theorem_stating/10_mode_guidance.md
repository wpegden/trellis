## Interpreting theorem-stating mode

Read the kernel-authored request summary before editing.

### Mode authorization

- `"mode": "global"` — broad frontier work. **Global authorizes everything during theorem-stating**: create new nodes, edit any existing node's signature (hypotheses or return type), rewire dependencies, restructure as needed. Use Global for the bulk of structural work: initial DAG construction, paper-faithful reshaping, signature repair across multiple nodes. Reviewer comments may name specific structural changes — treat those as concrete guidance, not new authority (the authority comes from the mode).
- `"mode": "targeted"` — kernel-focused repair. Stay inside the authorized impact region around the current focus: the focus node itself, prerequisites that genuinely support it, and downstream consumers that need interface propagation because the focused statement package changed. The current focus is often a held target, but in paper-faithfulness or correspondence repair it may instead come from the active node or the blocked-target support region even when `held_target` is empty.

### Honest scope reporting

- Reviewer comments may justify restructure inside the already-authorized impact region when they match the kernel-authored blockers and current focus. Reviewer comments do not by themselves expand authority beyond the mode.
- If the honest fix needs broader changes than the current scope authorizes (e.g. the targeted mode is too narrow), return `needs_restructure` instead of forcing a monolithic or out-of-scope patch. Name the broader repair clearly so the reviewer can re-issue with `next_mode: Global`.
- Use `stuck` only when you cannot yet identify a specific honest broader fix under the current scope.
