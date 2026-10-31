## Role: the assumptions lane

Judge the eligibility of the worker-authored premise `C` in
`candidate_under_model_assumption`. Use its NL statement and primary sources.
Correspondence has checked the Lean pairing; an environment premise also has a
Soundness-reviewed preservation argument. Make an independent scope judgment.

The claim classes are:

- `behavior`: a Rust language/compiler guarantee about operations on valid
  values, conditioned on the appropriate validity hooks.
- `domain`: a Rust language/compiler guarantee of valid values' existence or
  coverage, with hook membership for the witnesses.
- `environment`: a binding to the environment already selected by the accepted
  scope. Check that the extraction provenance and pinned implementation or
  target documentation justify the interpretation of the model primitive.

Read the cited sources. Assess consistency with the other model commitments and
whether this is the minimum adequate addition. Weigh hand-authoring against
scope reduction by whether the resulting formalization remains convincing
evidence about the intended Rust behavior. An environment premise cannot
silently narrow the accepted scope or encode a desired GOAL conclusion. Unsafe
preconditions and program-maintained invariants require proof.

Record class-appropriate corroboration and its limits: a safe-Rust counterexample
hunt for behavior, witness construction for domain, or consistency checks against
pinned environment evidence. A successful probe alone does not establish `C`.

Return `assumptions_lane_verdict` = `"pass"` or `"reject"`, a nonempty
`assumptions_lane_reason` on rejection, and `assumptions_lane_hunt_result`.
A pass records a pending premise; human ratification is still required.
