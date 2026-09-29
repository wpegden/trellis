## Extract-helper task

This burst splits one proof-internal helper lemma out of a single oversized node, bringing the published repository closer to mathlib's file-size distribution.

The worker context names the parent in `cleanup_active_target_node`, the ordinal and hint in `cleanup_active_task_kind`, and the audit rationale in `cleanup_active_rationale`.

A batched dispatch lists `[parent_node, hint]` entries in `cleanup_active_batch`. Produce one helper for each, following everything below per parent: each parent gets its own new helper pair, keeps its statement byte-identical, and ends shorter than it started. Each helper is cited by its own parent's proof. Acceptance takes the burst whole, so every parent needs its extraction to stand on its own.

### What to produce

1. **Two new files per parent, for one helper node each**: `<Helper>.lean` and `<Helper>.tex`. Choose a descriptive, mathlib-idiomatic name. Follow FILESPEC — imports above the `-- [TABLET NODE: <Helper>]` marker, exactly one `-- BODY` line.
2. **A closed helper**: it compiles and is sorry-free in this same burst.
3. **A helper `.tex`** stating the helper, in the register you would use for any other node.
4. **An edited parent `.lean`** that imports the helper and uses it in place of the extracted reasoning. Everything from the parent's node marker to its `-- BODY` line stays byte-identical.
5. **Reasoning moved out of the parent, not just referenced from it.** Cut one coherent, nameable block — a chunk large enough that the resulting lemma is worth stating on its own. The parent ends up shorter than it started. The hint proposes a candidate cut. You have the node compiled and can measure the result, which the audit could not. Judge a candidate by how much it shrinks the parent and how well the resulting lemma stands on its own, and take the best one you find.
6. **Explicit newborn claims**: include every helper as a key in `target_claim_updates` with value `[]`, and do the same in `challenge_claim_updates` when that field is configured.

The parent's `.tex` proof block may cite `\noderef{<Helper>}`; its statement block stays byte-identical.

Acceptance reads the parent's post-burst closure record and looks for the helper among its boundary theorems, so the extracted lemma needs to be genuinely used by the parent's proof.

Acceptance requires exactly one fresh source/`.tex` pair per parent, one changed parent per helper, a byte-identical parent declaration slice and manuscript statement, strict parent shrinkage, same-burst helper closure, and genuine parent use. A batch satisfies these obligations independently for every member and lands atomically.
