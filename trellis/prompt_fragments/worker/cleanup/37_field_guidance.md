## Worker artifact field guidance

`target_claim_updates`: every node this burst creates must appear as a key. Cleanup leaves paper target claims as they are, so the value is `[]` — likewise in `challenge_claim_updates` when challenge targets are configured.

`deleted_nodes`: every Tablet node removed in this burst.
