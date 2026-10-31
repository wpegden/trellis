## Adjudicating an extracted-model refutation (trust protocol v1)

The challenge is carried in `need_input_audit.refutation_carrier` as the typed
`{target, refutation, evidence_refs}` triple. It may originate in a worker
`target_false_under_model` response or in retained reviewer/Sound evidence.
Re-derive the claimed refutation against that exact target in the byte-pinned
extracted Lean model. If it does not hold, set `under_model_ruling: "reject"`
and explain the failed derivation.

The carrier, reviewer prose, and every Sound result are routing evidence only.
They are never a statement binding or a polarity decision. A `SOUND` result does
not certify the refutation. Only the kernel's sole polarity mutator and the
conditional-theorem protocol can supply those authorities.

If the witness does falsify `T`, this audit owns the model proof. Then classify
the witness `x0` by its own properties; the classification selects the route.

- Admitted by the Rust language, and constructible on a well-provisioned
  present-day machine — `under_model_ruling: "bug"` with
  `set_live_polarity: "disprove"`: the kernel-computed exact negation becomes
  the live node.
- Excluded by a Rust language invariant the extraction dropped —
  `under_model_ruling: "deviation"`. Name the violated guarantee in your
  reasoning; the kernel routes the pair to disprove and records the guarantee
  as a route hint. Extraction divergence on a buildable input uses the
  seam-repair lane.
- Admitted by the Rust language, and physically unrealizable under the
  eligibility rubric's standard — `under_model_ruling: "bug"` with
  `set_live_polarity` left empty, and file a `conditional_theorem_proposal`
  (see that section): a Lean condition under the target's registered binders
  that excludes the recorded witness class, the hardware property the class
  demands in `rationale`, `evidence.disproof_sha256` as the proposal section
  describes, and `concrete_counterexample_arguments` naming the witness. The proposal keeps both unconditional sides open; the kernel
  seals `C → T`, the inhabitation of `C`, and the exclusion of the witness as
  Lean obligations, and the human gate ratifies the condition.

A size, count, or index exceeding a bound the language guarantees is an excluded
witness; scale the language permits is realizability, judged by the rubric.

Where more than one classification fits, the route that leaves more of `T`
established is the correct one.

`under_model_candidate_invariant` must be empty. Never describe a
model-admissible witness as a real Rust execution, allocation, or source
counterexample unless the target's frozen validation method produces the
corresponding authenticated source result.
