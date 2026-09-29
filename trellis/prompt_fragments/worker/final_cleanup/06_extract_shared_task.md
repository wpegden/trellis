## Extract-shared task

This burst replaces one duplicated proof block across an unbounded declared parent set with exactly one new helper. The active task's `target_node` plus `co_parents` are the complete edit envelope. Convert as many declared parents as you can, and at least two.

Create exactly one fresh FILESPEC-conformant `<Helper>.lean`/`<Helper>.tex` pair. Give the lemma a descriptive mathematical name and make it closed, sorry-free, and genuinely used by at least two converted parents. The parents may be unrelated theorems that merely share an argument: name the lemma for what it proves, not for the nodes it came from.

Include the helper as a key in `target_claim_updates` with value `[]`, and do the same in `challenge_claim_updates` when that field is configured.

For every parent you convert:

- keep its declaration and `.tex` statement byte-identical;
- import and use the helper in place of the duplicated reasoning;
- make its `.lean` file strictly shorter.

Leave any parent you cannot convert exactly as you found it. An unconverted parent must be byte-identical, not partially edited: every changed parent is closure-checked and must shrink, so one abandoned edit rejects the whole burst.

The helper's conclusion usually must be stronger than any single parent's immediate use of the block. Carry the intermediate facts that the parents' different tails need, possibly as a conjunction or structured result. Do not abandon strengthening after one attempt; at width it is the difference between converting a couple of parents and converting all of them.

Acceptance measures the outcome, not the helper in isolation:

`sum(post lines of changed parents) + helper lines < sum(pre lines of changed parents)`.

There is no helper-size bound. A helper that must carry strengthened conclusions is legitimately larger than the block it replaces; judge the net sum instead. Acceptance also preserves the tablet's previous largest-file ceiling, runs a scoped build, and obtains fresh closure records for the helper and every changed parent.
