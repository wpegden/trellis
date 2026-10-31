## Author the under-model assumption C

The auditor named candidate premise `C`
(`authored_assumption_contract.candidate_invariant`). Author the assumption
needed by the run-authored statement; the assumptions lane gates it.

Author these paired blocks:

- `Tablet/Assumptions.lean`: one block from
  `-- BEGIN UNDERMODEL ASSUMPTION <authored_assumption_id>` to
  `-- END UNDERMODEL ASSUMPTION <authored_assumption_id>`. Its body is the
  verbatim `axiom <name> : <type>` over the real model API.
- `Tablet/Assumptions.tex`: the paired block from
  `% BEGIN UNDERMODEL ASSUMPTION <authored_assumption_id>` to
  `% END UNDERMODEL ASSUMPTION <authored_assumption_id>`. Its body is the
  matching NL statement. For an environment premise, include the standard
  `preservation` section here: its claim, source-grounded proof, and a separate
  `goalproof` application for each pursued GOAL, with primary citations.

State property axioms through their Rust-validity/correspondence hook.
Every quantified variable of an over-approximating container type must sit
under that type's hook applied to it; a `domain` claim's existential witness
must assert hook membership. The accepted theorem statement and support DAG
stay unchanged during this authoring burst.

Emit JSON metadata only:

- `authored_assumption_id`: stable ASCII block id using letters, digits, `_`,
  `-`, or `.`.
- `authored_axiom_name`: namespace-qualified Lean axiom name for `C`.
- `authored_citation_locator`: primary sources establishing `C`: Rust-docs
  locators for language guarantees; accepted scope, extraction provenance, and
  pinned implementation or target documentation for environment bindings.
- `authored_rust_justification`: why the locator guarantees `C` and matches the
  Lean axiom.
- `authored_claim_class`: `behavior` for hook-conditioned facts about
  operations on valid values, or `domain` for language-guaranteed
  existence/coverage of valid values; `environment` for a missing binding to
  the extraction environment already selected by the accepted scope.

An environment binding applies to both goal polarities without changing the
goal statements. Explain why the model primitive represents the selected source
environment and why the binding preserves the relevant behavior. Executable
probes corroborate this argument. Keep required primary evidence under
`reference/model-evidence/` and cite it with `\evidenceref{path}`. Correspondence
checks the paired statement, Soundness checks this argument, and the assumptions
lane checks eligibility before human ratification.
