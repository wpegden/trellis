## Disproving a Decide target

The live polarity of this Decide target is Disprove, so the obligation is
`<name>__Refutation`, the kernel-pinned exact negation `¬T`.

The refutation statement is byte-pinned and kernel-authored. This phase's
deliverable is its `.tex`: state the counterexample in prose — the input value,
the branch it reaches, and the contract clause it violates — at the soundness
lane's detail floor.

When the disproof is witness-specific, state the witness as a separate helper
node with one principal theorem applying the exact predicate from `T` to the
concrete witness, paired with its own `.tex` statement and proof. Import it from
`<name>__Refutation` and derive `¬T` through it in prose.

Lean closure of both nodes and the witness sidecar are proof-formalization work
and are recorded as owed.
