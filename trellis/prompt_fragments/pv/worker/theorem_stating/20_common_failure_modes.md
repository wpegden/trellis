## Common theorem-stating failure modes

- Do not put `\section`, free top-level prose, or multiple top-level claims in an ordinary node `.tex` file.
- If a model fact needs to be named for reuse, make it a real lemma node over the pinned defs rather than disguising it as a theorem-like result it is not.
- Definitions use `def`, `abbrev`, or `noncomputable def`; proof-bearing nodes use `theorem` or `lemma`. The `ExtractionModel` defs are given and pinned — you do not author them.
- In targeted work, return `needs_restructure` when the real fix is a specific broader restructure outside the authorized impact region. Use `stuck` only when you cannot yet name the honest broader fix.

### Substantiveness failure modes (avoid by construction)

A node fails the per-node substantiveness rubric if its NL content does not represent a genuine, non-duplicative obligation the verification needs. Avoid these patterns when authoring nodes:

- **Wrapper.** Node content is a thin renaming or repackaging of another node's content. Proof of one from the other is one line. Merge the content into the node it duplicates, or retarget consumers so they no longer use the wrapper.
- **Vacuous spec.** A `Spec`/`Correctness` postcondition (`⦃ ret => P ret ⦄`) whose `P` is so weak the triple is trivially true (e.g. `True` where the contract requires a congruence and a bound). State the obligation `GOAL.md` actually requires, at full strength.
- **Under-strength statement.** A goal theorem whose preconditions are narrower, or whose postcondition is weaker, than the `GOAL.md` contract — it under-verifies the function. Carry exactly the input conditions the function needs and the full output property.
- **Over-fragmentation.** A single `GOAL.md` obligation split into nodes so fine-grained that no individual node carries weight. Coarsen the decomposition.
- **Restating a model fact.** A node whose content merely re-asserts what a pinned `ExtractionModel` def already says, with no proof obligation. Cite the model def where it is used instead.

See `SUBSTANTIVENESS.md` at the project root for the canonical rubric.

### Statement-writing discipline

- The Lean signature must encode every domain assumption the `GOAL.md` contract carries — the precondition regime that keeps the arithmetic in range, the input bounds — and no narrower.
- Don't summarize the contract; write the preconditions and the postcondition out at full strength over the pinned model def.
- A Decide target's witness subject belongs in an explicit top-level binder of its goal theorem, carrying one property per target. The kernel derives the disproof certificate's goal from the accepted bytes by instantiating that binder; a subject quantified inside a conjunction admits no derivation, leaving the target disprovable with no source-confirmed witness.
- Where a model type admits values its Rust type cannot, author a validity definition naming the Rust domain and state the theorem relative to it, justified from the pinned Rust source. A validity definition used as a Rust-domain guard must be one CamelCase identifier beginning `RustValid`, without underscores.
- Where a reachable primitive is an opaque extraction axiom, stage an under-model assumption for the Rust-guaranteed behavior the statement needs and cite its Rust-docs corpus locator.
