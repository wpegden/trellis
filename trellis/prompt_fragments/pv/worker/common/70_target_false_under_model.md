## When the target is false under the Aeneas model

The goal theorem `T` is proved over byte-pinned Lean `def`s that Aeneas
extracted from the Rust crate. That model is sometimes a strict UNDER-model of
real Rust: it admits Lean states real Rust forbids. When that happens, a `T`
that is true for every real-Rust input can be FALSE against the looser Lean
model — you cannot prove it (false in the model), and you must not force a proof.

If — and only if — you conclude `T` is false under the model (not merely hard),
emit `outcome: "target_false_under_model"` and report:

- `under_model_disproof`: the witness `x0` that falsifies `T` against the Lean
  model, plus precisely why it breaks the spec (which contract clause, and how
  the extracted `def` mis-behaves on `x0`).
- `under_model_route_opinion`: your read of the route —
  - `"flip"` if `x0` is a real, constructible Rust input (a genuine bug ⇒ `T`
    should be disproved), or
  - `"model-deviation"` if `x0` is only reachable by violating a Rust language
    invariant Aeneas dropped (e.g. a slice longer than `isize::MAX`), so the
    faithful fix is to assume that invariant.
- `under_model_reasoning`: how you reached the disproof and the route opinion.

You author NO tablet edit and you CANNOT mint an assumption. You only report the
witness and your opinion. The auditor is the sole adjudicator: it re-verifies
your disproof in the audit lane and rules the route (bug → the worker proves
`¬T`; model-deviation → an assumption is authorized and a human ratifies it).

Reserve this outcome for a genuine under-model falsification. If the goal is
merely hard, or you are unsure whether `x0` is reachable, keep proving (or use
`stuck` / `needs_restructure`).
