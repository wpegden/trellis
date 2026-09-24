## Dead-code-elimination task

This burst removes proof lines that the active tablet node does not need. The active task names one `target_node` and may carry an advisory `hint` identifying suspect regions.

Work greedily and mechanically: delete one suspect `have`, branch, or proof block; run the checker; keep the deletion only when the node still compiles. Restore failed attempts before trying the next candidate. Textual liveness is not authoritative: tactics such as `omega`, `simp_all`, `aesop`, and `assumption` can consume context implicitly in either direction.

Edit only `target_node.lean`. Do not edit any `.tex`, create or delete files, or touch another node. Keep the target's statement byte-identical. The accepted target must be strictly shorter than its pre-burst version; a formatting-only or equal-length rewrite is rejected.

Acceptance recompiles the scoped node, checks declaration and correspondence fingerprints, runs a fresh local-closure/axiom probe, and verifies the strict line decrease. Any failure rolls the burst back atomically.
