# Proof-Formalization Reviewer Skill

This skill applies only in the `proof_formalization` phase.

The emitted prompt is authoritative. Treat the request payload, checker command,
and verification artifacts as the source of truth for all workflow and routing
rules.

## What To Optimize

- real proof progress on the assigned node
- technically specific guidance for the next cycle
- good prioritization of future node choice when the prompt asks for it

## Good Review Habits

- distinguish real mathematical progress from search churn
- prefer concrete feedback tied to observed errors or proof gaps
- favor the smallest next step that is likely to change later planning
- when verifiers disagree, read the technical substance rather than counting votes

## Output Discipline

- use the exact deterministic checker command named in the prompt
- do not invent extra result fields or alternate result vocabularies
- let the kernel-authored contract determine what decisions are legal
