## Lint-fix task

The worker context carries either one task in `cleanup_active_target_node` and `cleanup_active_task_kind.warning_text`, or a homogeneous batch in `cleanup_active_batch`. Each batch entry is `[target_node, warning_text]`, and the authorized source scope is exactly those targets.

For every member, edit its source file and remove the named diagnostic. Keep the declaration signature and correspondence fingerprint invariant. Preserve the tablet file set and every `.tex` file.

Acceptance captures the named diagnostic immediately before the burst and requires a scoped source edit plus its disappearance afterward. It checks every batch member atomically. If a diagnostic is already absent when you begin, return `invalid` and identify the stale task in `comments`.
