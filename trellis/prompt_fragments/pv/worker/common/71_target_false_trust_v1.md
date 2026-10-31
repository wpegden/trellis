## When a target is false in the extracted model (trust protocol v1)

If — and only if — you have a concrete witness `x0` that falsifies the current
Decide target `T` in the byte-pinned extracted Lean model, emit
`outcome: "target_false_under_model"`. Report `x0` and the exact failed
predicate in `under_model_disproof`, use `under_model_route_opinion` only as a
short diagnostic hint, and put the derivation in `under_model_reasoning`.

Do not decide whether this is a real-Rust counterexample. Do not run a proxy,
invent a source encoding, propose an axiom, or weaken `T`. The kernel enacts
the flip to Disprove only on a recorded station verdict: the audit lane
solicits the structured `witness_proposal`, the source-assessment station
replays it against real Rust, and the recorded verdict enacts the flip
(exact source counterexample), routes to Lane-1 model repair (source-model
mismatch), or bounces the witness (candidate rejected); the audit's
classification of the witness selects the remaining recorded routes. The first
flip refusal is the expected leg of the refuse→station→re-propose loop. The
unrestricted extracted-model refutation always remains visible.

On the later Disprove task, follow the Disprove instructions to make the
concrete witness a separately checked helper dependency and to emit the exact
canonical `.trust-witness.json` report. Do not emit that report during this
diagnostic `target_false_under_model` response, and do not emit one for a
theorem-level disproof that has no finite concrete witness.

These routes are claim-specific and apply equally to primary and supporting
Decide targets. In reports and TeX, say `refuted in the extracted model` until
the frozen source-validation method has independently established the source
result. Model carrier admissibility alone is not evidence that Rust allocated,
constructed, or executed the witness. An unexecutably large symbolic witness
therefore remains model evidence plus a reason for the source-validation
outcome; it is never itself a reproduced Rust counterexample.
