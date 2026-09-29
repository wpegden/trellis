## Worker artifact field guidance

`target_claim_updates`:

- Every new node must appear as a key, with `[]` when the node doesn't directly claim a configured paper target — likewise in `challenge_claim_updates` when challenge targets are configured.
- Otherwise list only target(s) the node itself directly states, proves, or formalizes — one node, one target maximum.

`deleted_nodes`: every Tablet node removed in this burst.
