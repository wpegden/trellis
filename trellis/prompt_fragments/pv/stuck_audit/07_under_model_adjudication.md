## Adjudicating an under-model claim

A worker reported `target_false_under_model`: it concluded that the bound
Decide target `T` is false under the Aeneas Lean model and supplied a witness.
Its disproof, route opinion, and reasoning are in `need_input_audit`.

### 1. Verify the disproof

Re-derive that the witness `x0` falsifies `T` against the extracted Aeneas
model. A failed re-derivation yields `under_model_ruling: "reject"` and routes
the target back to ordinary proving.

### 2. Rule witness realizability

The accepted extraction scope in `modeling-input/manifest.json` is part of the
model's meaning. When your derivation depends on a property of the extraction
environment that the scope fixes but the model leaves abstract, rule
`deviation` and name that property as `under_model_candidate_invariant` (claim
class environment), so the assumptions lane can admit it and the derivation can
be completed under it. A derivation that holds only in an environment the scope
excludes refutes nothing about the scoped program.

Decide whether `x0` corresponds to a real, constructible, runnable Rust input.
Use the Empirical Rust check to build and run the crate in scratch.

- `under_model_ruling: "bug"` means `x0` runs in real Rust and exhibits the
  violation. Flip polarity to Disprove and plan any boundary-contract
  obligations needed by the disproof.
- `under_model_ruling: "deviation"` means Rust construction requires violating
  a language invariant absent from the model. Authorize an assumption and name
  the candidate invariant in `under_model_candidate_invariant`. The target
  parks pending assumption authoring and human ratification.

This lane is the authority for the ruling and any polarity flip.
