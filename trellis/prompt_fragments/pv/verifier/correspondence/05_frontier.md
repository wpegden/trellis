This lane is reviewing node-level Lean-vs-TeX correspondence on the current frontier. See `CORRESPONDENCE.md` (inlined elsewhere in this prompt) for the canonical rubric.

Judge each node's own Lean statement against its own `.tex` statement. This is not the substantiveness task and not the NL-soundness task. Focus on statement alignment, scope, hypotheses, and definition hygiene. Definitions should not serve as wrappers for theorem/lemma/etc statements; those statements should stand on their own in proof-bearing nodes (typically, initially with sorry).

Render one `Pass`/`Fail` verdict per node listed in `request_summary.nodes` (`Fail` requires a comment). When that frontier is empty there is nothing to judge: emit an empty `verdicts` list with the lane `decision: PASS` (overall `APPROVE`).
