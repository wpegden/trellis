# Soundness (program-verification)

This is a program-verification run. The detail floor for soundness is
**what a Lean formalization of this node will
need to discharge**: every monadic step (`ok` / `Result.fail` / `Result.div` branching),
every arithmetic, cast, shift, and bound step the Lean proof must carry has to be
justified in the NL proof. A node passes soundness only when its `.tex` proof is a
detailed, line-by-line checkable derivation at that floor — a proof a Lean formalizer can
follow without inventing a missing step.

The available hypotheses are the cited support nodes' `.tex` statements, exactly as
stated. Three of those carry pinned content:
- **`ExtractionModel` def nodes** (the `extraction_model` role): the inviolate generated
  Lean declarations of the Rust functions (see the Extraction-model canonical doc). Their
  semantics — the `Result`-monad control flow, the failure branches, the cast/shift/modulus
  operations they encode — are the ground facts the proof reasons from. A step that asserts a
  property of a modeled operation must follow from that pinned declaration's actual behavior,
  not from an informal gloss of "what the function does."
- **`Spec` nodes**: precondition/postcondition contracts. When the proof appeals to a
  Spec, treat its postcondition `P ret` (under its preconditions) as a stated hypothesis.
- **The `Assumptions` node** (the `under_model_assumptions` role): staged, lane-gated
  axioms about opaque boundary primitives, pending human ratification. Each
  marker-delimited `axiom` statement is an available hypothesis; ratification status is
  outside this lane's scope.

Judge against the bounds and constants the goal carries (the `GOAL.md` output bounds, the
domain constants the model fixes): a bound or modulus step is justified only when the NL
proof shows the arithmetic that establishes it, citing the model `def` semantics and the
relevant Spec/Invariant. A proof that waves at "the obvious bound" without the arithmetic
fails — that is exactly the step the Lean proof cannot skip. Exception: a closed numeric claim over fixed pinned constants is at-floor once stated exactly; Lean discharges it by `decide`.

Every cross-node dependency must be cited with `\noderef{NodeName}` in the proof block.
The cited node's statement is the dependency; its proof is not. Do not rely on an
unstated support node or on hypotheses that belong only to a dependency's proof rather
than its statement.

A proof whose first nonblank line is exactly `SKETCH:` is marked incomplete by the kernel
until the marker is removed.
