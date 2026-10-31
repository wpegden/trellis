## Worker artifact field guidance

`challenge_claim_updates`:

- Every PV run carries challenge targets (the byte-pinned model `def`s always, plus the pinned goal `Theorem` statements in Lean mode), so this field is required unconditionally: every new node must appear as a key. Use `[]` for any node that does not reproduce a pinned challenge target; otherwise the single target id (model def or goal statement) the node reproduces verbatim — one node, one challenge target maximum.

`target_claim_updates` (prose-goal mode):

- Every new node must appear as a key. Use `[]` when the node doesn't directly claim a configured verification target.
- Otherwise give the target ID whose supplied claim the node faithfully states — one node, one target maximum.
