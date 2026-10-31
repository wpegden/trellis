Assess the proposed disposition against the stage criteria and the complete candidate diff. Require a source-grounded preservation argument, using notes or code comments and established dependencies; sampled comparisons are supporting evidence. Compare changes with smaller adequate alternatives, including downstream modeling and preservation work.

For consequential scope or authoring choices without a completed route review, return `decision: review` with the tradeoff to examine. Otherwise, acceptance requires `route_compliance: verified` when route decisions exist, confirming the implementation respects their scope and authoring limits, or `not_needed` when no consequential choice requires review.

After successful extraction, record diagnosed missing operation/observation meanings and their source evidence in `support_obligations`. Lean failures may pass to modeling when those missing definitions account for all diagnostics; translation defects require repair. The kernel determines the authored DAG's verifier requirements.

Assess the downstream work committed by this handoff, including retained concrete instances and unused interface requirements. Apply the route-review criterion to consequential modeling choices implicit in the extraction boundary, even when their implementation is deferred. Routine missing bindings may still pass to Modeling without a route review; do not require complete model authoring during preparation.

Check whether unresolved names refer to genuinely absent model definitions or to declarations already present in the extraction. Invalid forward references and cyclic generated imports require adaptation or extraction-setup repair before handoff; they are not missing-support obligations. The export report records mechanical packaging failures and their dependency paths.

The export's axiom compatibility report records dependencies of successfully elaborated declarations, including imported assumptions and proofs inserted by elaboration. Unapproved dependencies require adaptation or setup repair before freezing. Discovery does not approve an assumption. Pending coverage from failed declarations and their dependents remains part of the diagnosed support handoff; complete closure checking still follows Modeling. Removing unused source support is an adaptation only when its preservation argument covers every retained GOAL observation and concrete instantiation.

Assess setup reopening on its proposed changes and goal coverage. Accept when the stage criteria hold, or return actionable reasons for revision.
