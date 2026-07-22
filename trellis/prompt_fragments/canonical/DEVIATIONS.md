# DEVIATIONS

A deviation is an explicit, authorized departure from the paper which is still compatible with end-to-end faithful formalization of the paper targets.

Deviations allow only minor changes (e.g., to constants in proofs) that can be easily absorbed by later proof steps and simplify the step modeled by the deviation — not completely different proofs of paper results.

A deviation must live in one TeX-only reference file. It must state the departure, name the affected nodes, and give a rigorous argument that the formalization returns to a paper-faithful step.

We authorize a deviation only when the return argument is clear enough for later substantiveness checks to rely on it. Constant changes, strengthened hypotheses, weakened conclusions, or alternate intermediate statements need a concrete explanation of where the difference is absorbed.

A deviation must be valid on its own: its return-to-faithful argument may not rely on any other authorized deviation. Conversely, it cannot invalidate the return-to-faithful argument of any other deviation. Intertwined deviations should exist in one combined deviation file.

Deviations are *unusual*, not normal operation. Following the paper's precise path is always preferred; deviate only when there is a clear reason.

A node file passing substantiveness may claim only authorized deviations. Substantiveness checks the node, the paper, and the claimed authorized deviations together.
