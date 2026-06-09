# Soundness

A node *N* is **sound** iff the natural-language proof in *N*'s `.tex` rigorously establishes *N*'s `.tex` statement by a detailed, line-by-line checkable proof from explicitly cited support nodes. Soundness requires proofs that are clear and highly detailed, matching and using hypotheses exactly as stated, and thus providing a strong roadmap for formalization in Lean. As such, the detail level in the paper is a floor on the detail one expects in a proof that passes soundness.

Every cross-node natural-language proof dependency must be cited with `\noderef{NodeName}` in the proof block. The cited node's statement is the dependency being used; the cited node's proof is not part of the dependency. Do not rely on an unstated or implicit support node, or hypotheses/parameters that belong only to the proof of a dependency rather than its statement. A proof that appeals to another theorem-like node's hypotheses by citation fails soundness (see FILESPEC).

A proof whose first nonblank line is exactly `SKETCH:` is marked incomplete by the kernel until the marker is removed.
