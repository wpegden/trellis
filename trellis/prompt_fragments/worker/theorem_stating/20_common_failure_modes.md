## Common theorem-stating failure modes

- Do not put `\section`, free top-level prose, or multiple top-level claims in an ordinary node `.tex` file.
- If a concept is being introduced, make it a real definition node rather than disguising it as a theorem-like result.
- Definitions use `def`, `abbrev`, or `noncomputable def`; proof-bearing nodes use `theorem` or `lemma`.
- In targeted work, return `needs_restructure` when the real fix is a specific broader restructure outside the authorized impact region. Use `stuck` only when you cannot yet name the honest broader fix.

### Substantiveness failure modes (avoid by construction)

A node fails the per-node substantiveness rubric if its NL content does not represent a valid AND meaningful decomposition of the paper's proof. Avoid these patterns when authoring nodes:

- **Wrapper.** Node content is a thin renaming or repackaging of another node's content. Proof of one from the other is one line. Merge the content into the node it duplicates, or retarget consumers so they no longer use the wrapper. Only delete an existing node when the kernel-authored scope/checker allows deletion.
- **Vacuous existential.** Statement of the form `∃ x, P(x)` where the body `P` is so weak that the existence is trivial. Often arises when extracting a witness from a paper claim mechanically. State the substantive claim the paper actually proves.
- **Underspecified definition.** Definition that names a concept but omits properties the paper uses elsewhere. Downstream proofs that need those properties become impossible. Include the properties the paper uses.
- **Over-fragmentation.** A single paper claim split into nodes so fine-grained that no individual node carries semantic weight. Coarsen the decomposition.
- **Restating an axiom or hypothesis.** A node whose content is identical to a paper-level assumption with no new content. Cite the assumption directly where it is used; if an existing node has become redundant, retarget its consumers and let automatic orphan cleanup remove it when unsupported.

The exception to non-redundancy: an aggregator following trivially from several *meaningfully different* cases is acceptable. The aggregator and the cases all count as substantive even though the aggregator's proof from the cases is trivial.

See `SUBSTANTIVENESS.md` at the project root for the canonical rubric.

### Statement-writing discipline

- Lean signature must encode every domain assumption the paper's statement carries, including ones the paper's prose leaves implicit.
- Don't summarize the paper's (explicit and implicit) hypotheses; write them out (and if necessary, correct them).
