This review follows a `target_false_under_model` worker attempt.

The worker is reporting that the Decide target `T` is FALSE under the Aeneas
Lean model: it found a witness `x0` that falsifies `T` against the extracted
model, and gave an opinion on the route — a real bug (the witness is a genuine
Rust input, so `T` should be disproved) versus a model-deviation (the witness
is only reachable by violating a Rust language invariant Aeneas dropped, so the
faithful fix is to assume that invariant). The worker authored no tablet edit
and could not mint an assumption — it only reported the witness.

You are not the adjudicator. Forward this claim to the auditor by emitting
`NeedInput`: the auditor is the sole authority that verifies the disproof in
the audit lane and rules the route (bug → disprove, model-deviation → authorize
an assumption). The worker's disproof, route opinion, and reasoning are in
`request_summary.latest_worker_rationale` — restate the witness and the route
question in your `reason` so the auditor inherits the localization.

Do not attempt to flip polarity, author a refutation, or widen scope yourself.
