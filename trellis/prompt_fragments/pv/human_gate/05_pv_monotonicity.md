# Protected spec re-approval — monotonicity review

A program-verification spec-role node (`Spec`, `Safety`, `Correctness`, or `Invariant`)
has had its statement changed after approval. Any such change reopens the bound theorems
and raises this gate; the kernel cannot decide whether the new statement implies the old
one, so this is a human judgment. Completion stays blocked until you re-approve.

The question to answer for each reopened node: **is the new statement a strengthening or
a weakening of the approved one?**

- A **strengthening** proves more: a weaker precondition (a wider input domain), a
  stronger postcondition (an added conjunct, an equality in place of a bound, a specific
  value in place of membership), or a no-fail guarantee over more operations. A
  strengthening is safe to re-approve.
- A **weakening** proves less: a stronger precondition (a narrowed input domain), a
  weaker postcondition (a dropped conjunct, a bound relaxed to a looser one, a constraint
  replaced by `True`), or a no-fail guarantee dropped on some operation. A weakening
  certifies less than the approved spec and is the cheat this gate exists to catch.
  Withhold re-approval unless the weakening is intended and justified.

The per-node bullets surfaced with this gate (`protected_reapproval_nodes` and the
fingerprint-axis diff) name what changed between the approved statement and the current
one. Read each as a precondition change or a postcondition change and classify its
direction. A change that is part strengthening and part weakening is a weakening for this
gate. When the direction is unclear from the diff, treat it as a weakening and withhold
re-approval until the change is clarified.

A pure precondition narrowing that keeps the same shape (a constant pushed up inside one
hypothesis) can read as a small edit while being a real weakening; check whether the new
precondition excludes inputs the function is intended to handle.
