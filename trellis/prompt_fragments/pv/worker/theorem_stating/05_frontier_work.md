## Theorem-stating frontier work

This request is ordinary frontier work on the goal DAG — not the first request, and not a direct repair of a live substantiveness, correspondence, soundness, or faithfulness blocker. Use it to make real progress: state the `GOAL.md` obligations for the target functions as honest `Spec`/`Safety`/`Correctness` theorems over the pinned model, and build the supporting helper-lemma structure those proofs need.

Theorem-stating is expected to span many cycles. Your goal is to build a DAG that is a robust and rigorous foundation for the autoformalization process, with the `GOAL.md` obligations as targets.

The run stays in theorem-stating until all verifier lanes are passing for each node. You author the goal theorems in this mode, so each authored goal theorem and each helper you add is reviewed on substantiveness, correspondence, soundness, and faithfulness to `GOAL.md`. SOUNDNESS is waived when a node is closed in `.lean` relative to its imports, so drive toward a DAG in which:
- Each goal theorem is either closed in `.lean` relative to its imports (including any new nodes you add), or has an NL proof that will pass the SOUNDNESS verifier lane.
- Each node states an obligation faithful to `GOAL.md` and passes the SUBSTANTIVENESS and CORRESPONDENCE lanes.

Making a DAG that is a strong foundation for the formalization process involves making intelligent, robust choices about decomposition. A DAG that does not isolate re-used facts and definitions into individual nodes will result in a wasteful formalization process.

- Isolate reused facts as their own lemma nodes (a model-contract lemma several goal theorems import; see the helper-node policy for the worked example).
- Intermediate nodes can be useful even if they will not be used in multiple places, so long as they meaningfully decompose a proof obligation.
- Aim for a support graph that later workers and verifiers can understand and extend honestly.

You do not need to cover every target on this cycle. Each NL proof is individually audited for soundness. The `SKETCH:` placeholder (first line of an NL proof, repaired on a later cycle) is permitted only for new nodes in the first theorem-stating cycle; on any later cycle every NL proof you provide must be a complete, sound argument.
