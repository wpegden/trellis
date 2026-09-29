## Shared proof blocks

`audit_contract.shared_proof_blocks.regions` reports runs of identical — or identical-up-to-local-renaming — consecutive proof lines that appear in more than one node, above a fixed length. It is a mechanical text scan, computed fresh each burst. Each region names the nodes sharing a block, the representative's `block_lines`, the ranking input `recoverable = block_lines × (n_nodes − 1)`, and any overlapping `nested_alternatives`.

Rank by descending `recoverable`, preferring the smaller block on a tie. It orders candidates and nothing else: it is neither a threshold below which a region is unworthy nor a ceiling on what extraction recovers, since collapsing a shared block frequently makes surrounding machinery dead as well. There is no minimum block size, and no class of region you must refuse to propose — a Low-confidence proposal is a legitimate output.

For a nested chain, propose one level only: the representative, unless an alternative states a cleaner lemma.

`post_block_dependence` estimates how much strengthening a region needs. Names bound inside the block and used after it, or context-consuming closers, suggest the helper must return more intermediate facts; name the likely ones in the `hint`. The signal is unsound in both directions — tactics consume context invisibly, and a textual reference may be trivial to remove — so rank heavier dependence lower without treating it as disqualifying.

The technique that succeeds is strengthening the helper's conclusion past any single parent's immediate goal, so it returns the intermediate facts every converted parent needs and each parent finishes its own tail. A first attempt that only packages the visibly duplicated subgoal usually converts one parent and strands the rest.

Assign confidence from match quality and dependence, never from size:

- **High** — exact match, little post-block dependence.
- **Medium** — exact match with meaningful dependence, or an alpha-equivalent match with little.
- **Low** — alpha-equivalent match with substantial or unclear dependence.

The nodes sharing a block need not be related; name the prospective lemma for what it proves rather than for where it was found.

A completed task may have converted only some of its parents. Compare `swept_parents` against the declared set: `Completed` means at least two converted, not that every copy is gone. Re-detection then reports the leftover region alongside the helper that was just created, and `has_lemma_shaped_member: true` marks that case. Do not propose a second extraction over the leftovers — it is rejected as a semantic duplicate, and no task kind converts a leftover parent to the existing helper. Leftover copies are accepted debt.
