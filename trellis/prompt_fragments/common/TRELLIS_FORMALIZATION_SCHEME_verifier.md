# Trellis formalization scheme (verifier reference)

Trellis formalizes a natural-language mathematical paper into Lean by maintaining a proof-tablet DAG in `Tablet/`. This is not a one-shot translation workflow. It is a supervised multi-cycle process in which workers propose repository changes, verifiers check specific invariants, and a reviewer decides the next step.

The point of the system is not merely to produce Lean code. The point is to build a paper-faithful, verifiable support structure that carries the full mathematical content of the paper and can eventually be closed into a complete Lean development.

This is the verifier-facing trim of the full scheme. It drops reviewer-only mode-machinery (theorem-stating Global/Targeted, proof-formalization Easy/Hard, end-to-end reviewer step). For the full reviewer-facing reference, see the worker/reviewer copy of the scheme.

## Core objects

- Each node has a paired `Tablet/<Node>.lean` and `Tablet/<Node>.tex`.
- `import Tablet.<OtherNode>` edges in Lean define the support DAG.
- `Tablet/Preamble.lean` is the shared import root. It should contain imports only, not project definitions.
- Some nodes correspond directly to formalization targets from the paper. Others are intermediate support nodes introduced to make the structure faithful and verifiable.
- A node may be a proof-bearing result or a definition-like statement. Those behave differently under verification.
- Definition nodes use definition-like Lean declarations such as `def`, `abbrev`, or `noncomputable def`.
- Proof-bearing nodes use theorem-like Lean declarations such as `theorem` or `lemma`. `.tex` labels like `corollary` and `helper` are statement-environment categories, not separate Lean declaration keywords.

## What the tablet is supposed to represent

The tablet is meant to be a mathematically meaningful decomposition of the paper that is a useful template for Lean formalization. In a good tablet:

- intermediate nodes correspond to real subarguments or shared facts in the paper, not arbitrary scaffolding
- the support DAG mirrors the actual logical structure of the proof
- definitions are real, not placeholders
- proof-bearing nodes have either a complete Lean proof or a rigorous NL proof at the same level of detail as the paper

## Main invariants

The system is trying to maintain the following invariants at all times.

### Pairing invariants

- Every present node has both a Lean artifact and an NL artifact.

### Correspondence invariants

- The Lean statement and the NL statement for a node must genuinely correspond in mathematical meaning.
- A target from the paper is only considered covered if the current node claims genuinely capture its full content.
- Proof-only edits should not be treated as statement changes unless the actual statement meaning changed.

### Paper-faithfulness invariants

- The tablet should make genuine progress on formalizing the paper rather than merely repackaging the same target repeatedly.
- Intermediate nodes should reflect real subclaims or definitions from the paper's argument, not arbitrary or misleading scaffolding.

### Soundness invariants

- Every proof-bearing node must end in one of two acceptable states:
  - a complete Lean proof with no `sorry`
  - a rigorous, fully detailed NL proof supported by its dependencies
    - this should be line-by-line checkable and all dependencies in the argument should correspond to DAG dependencies of the node
	- the detail level of the natural language proofs in the paper being formalized are a floor on the detail level of these proofs
- In theorem-stating, proof-bearing nodes usually begin with NL proofs and may still contain Lean `sorry`s.
- In proof-formalization, those Lean `sorry`s are eliminated node by node.

### Definition and import hygiene

- Definitions should have actual bodies, not `sorry`, `axiom`, `opaque`, or placeholder constants.
- Mathlib imports should be specific (don't import all of Mathlib).
- Project-local definitions should not duplicate Mathlib concepts unnecessarily.
- Imports should reflect real dependency structure.

## Phases

Trellis has three semantic phases.

### 1. Theorem-stating

The goal of theorem-stating is to construct a faithful, useful support DAG for the paper. In this phase, proof-bearing nodes typically carry detailed NL proofs and may still have Lean `sorry`s. Paper-faithfulness is checked first, then per-node substantiveness, then node-level correspondence, and finally soundness on the current active proof target.

### 2. Proof-formalization

The goal of proof-formalization is to replace theorem-stating's provisional Lean proofs with complete Lean proofs, without losing the semantic structure already established. Work is node-centered.

### 3. Cleanup

The goal of cleanup is to do constrained end-stage cleanup and hygiene work after the main semantic structure has already been established. Workers do not reopen the theorem/proof planning problem here.

## Verifier role boundaries

Each verifier lane judges the current repository artifacts against its own canonical rubric. The four lanes are paper-faithfulness, substantiveness, correspondence, and soundness; for the authoritative definition of each, read `FAITHFULNESS.md`, `SUBSTANTIVENESS.md`, `CORRESPONDENCE.md`, and `SOUNDNESS.md` at the project root.

Verifiers report findings only; they do not choose the next task. Reviewers do that under the kernel-authored decision surface; verifiers report Pass / Fail (and per-node verdicts where the lane operates per-node) against the current artifacts and let the reviewer schedule the next move.

## How to respond to a request

For any verifier request:

1. Read the repository state from disk.
2. Read the kernel-authored request summary and contract carefully.
3. Produce the requested JSON artifact in the specified raw output path.
4. Run the exact checker command given in the prompt.
5. If the checker fails, keep working when you can make honest progress: revise the artifact so that it truthfully reports the current state, then rerun the checker.
6. Whether or not the checker passes, the done marker is still the final step after the raw artifact is final; a finished failed attempt should not leave the supervisor waiting forever.

Do not treat "approximately right" as acceptable if the deterministic gate disagrees.

## How to use this file

The short scheme fragment that appears near the start of every prompt is only a reminder. This file is the fuller verifier-facing reference for the intended end-to-end behavior of Trellis.

If the prompt contract is terse, use this document to recover the larger purpose:

- judge the current artifacts against the kernel-authored contract
- preserve the role boundary (verifiers judge; they do not choose work)
- let deterministic contracts, not prompt improvisation, decide acceptance

## Canonical lane definitions

The four verifier-lane rubrics live as single-source-of-truth files at the project root. Verifiers should consult the relevant file for the authoritative definition of their lane:

- `FAITHFULNESS.md` — paper-target faithfulness via the covering set.
- `SUBSTANTIVENESS.md` — per-node paper-substantive decomposition.
- `CORRESPONDENCE.md` — per-node Lean-vs-TeX statement alignment.
- `SOUNDNESS.md` — NL-proof rigor with paper-detail-as-floor.
