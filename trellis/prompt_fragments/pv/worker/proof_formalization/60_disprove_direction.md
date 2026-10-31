## Disproving a Decide target

This is the formalization-phase completion of a Decide target already stated on
the Disprove side.

The live polarity of this Decide target is Disprove, so the work is to close
`<name>__Refutation`, the kernel-pinned exact negation `¬T` of the goal
statement.

The refutation statement is byte-pinned and kernel-authored; your `.tex` gloss
states the counterexample in prose (the input value that makes `<f>` panic or
return `r`, and the contract clause it violates).

When the disproof is witness-specific, land the witness as a separate,
Lean-closed helper node as well as closing `<name>__Refutation`:

- Give the helper one principal theorem whose statement applies the exact
  predicate from `T` to the concrete witness and proves that predicate false.
- Import that helper from `<name>__Refutation` and derive `¬T` through the
  helper. The unrestricted refutation's checked dependency closure must
  therefore contain the named witness theorem; an unrelated witness theorem
  is rejected.
- Give the helper a paired `.tex` statement and complete proof under the normal
  node rules. Do not place the witness theorem as an unregistered auxiliary in
  the refutation node.

After both nodes close, write exactly
`Tablet/<name>__Refutation.trust-witness.json` as canonical JSON: UTF-8, sorted
object keys, no insignificant whitespace, and no trailing newline. Its closed
shape is:

```json
{"schema":"trellis-campaign-witness-report/v1","source_encoding_descriptor":{"encoding":"<the concrete source-level representation of this witness>"},"target_id":"<the exact configured challenge-target id>","witness_refutation_node_id":"<the exact helper node id>","witness_term":<the concrete witness as JSON>}
```

Use the exact target id and node id already present in the request/repository;
do not guess or hash them. `witness_term` must denote the same Lean witness as
the helper theorem. `source_encoding_descriptor` is only a candidate encoding
for the seed-pinned Rust validator; it does not assert Rust admissibility or
source truth. Describe the concrete Rust value/type construction faithfully.
If no finite concrete witness exists, do not create this sidecar: close the
general `¬T` proof only, and the trust kernel will use the registered
not-defined-for-claim-shape path.
