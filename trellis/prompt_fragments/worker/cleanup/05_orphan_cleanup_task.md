This cleanup task is deterministic structural hygiene.

Repair current orphaned nodes by removing them or attaching real supporting imports.

Legal outcomes: `valid`, `invalid`.

List removed nodes in `deleted_nodes`.

Cleanup-preserving edits:

- Delete current orphan nodes only.
- Retained-node edits are limited to orphan import lines.
