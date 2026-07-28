# Theorem-Stating Reviewer Skill

This skill applies only in the `theorem_stating` phase.

The emitted prompt is authoritative. Treat the request payload, blocker lists,
and checker command as the full workflow contract.

## What To Optimize

- a paper-faithful DAG of statements
- rigorous NL proofs from imported child statements
- structure that should make later Lean formalization easier

## Verification Order

- theorem-stating review sits after:
  - paper-faithfulness on paper-target coverage
  - then per-node substantiveness
  - then node-level correspondence
  - then NL proof soundness on the active proof target
- if paper-faithfulness fails or splits, later verification does not clear the cycle
- if substantiveness fails, correspondence and soundness do not clear the cycle
- if correspondence fails or splits, soundness does not clear the cycle
- use the verifier reasons to decide whether the next step is local repair, restructure, escalation, or revert

## Good Review Habits

- keep feedback concrete and local to the actual unresolved slice
- separate paper-faithfulness issues from per-node substantiveness issues from node-level correspondence issues, and separate all of those from proof-structure issues
- prefer guidance that names the missing intermediate claims or dependency links
- when verifiers disagree, weigh the mathematical reasoning rather than the vote count

## Output Discipline

- use the exact deterministic checker command named in the prompt
- do not invent alternate blocker partitions or decision vocabularies
- let the kernel-authored contract determine what decisions are legal
