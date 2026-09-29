## Correspondence-repair task

The correspondence verifier found that one node's `.tex` statement and its Lean statement do not say the same thing. This burst brings the prose into agreement with the Lean.

The node is the active node and the only entry in the authorized node list. The verifier's finding is in the request's blocker detail.

### What to produce

An edited **statement block** in that node's `.tex` — the text above `\begin{proof}` — saying exactly what the Lean declaration says.

The Lean is ground truth here: it is machine-checked and closed. Where the two disagree, the prose is what moves.

The node's `.lean` file and its `.tex` proof block both stay byte-identical. The statement block is the whole edit.
