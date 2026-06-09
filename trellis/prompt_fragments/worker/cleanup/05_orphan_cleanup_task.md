This cleanup task is for deterministic structural hygiene.

Repair the tablet so that current orphaned nodes are either removed or attached by real supporting imports.

In cleanup, only `valid` and `invalid` are legal outcomes. Do not return `stuck` or `needs_restructure`.

For each current orphaned node, either remove it, or attach it by adding it as a real `import Tablet.<Orphan>` dependency of an existing supported consumer node. On retained nodes, limit edits to the import changes needed for that attachment. Do not make any other retained-node edits.
