# Trellis formalization scheme

Trellis formalizes a natural-language mathematical paper into Lean by maintaining a proof-tablet DAG in `Tablet/`. This is not a one-shot translation workflow. It is a supervised multi-cycle process in which workers propose repository changes, verifiers check specific invariants, and a reviewer decides the next step.

The point of the system is not merely to produce Lean code. The point is to build a paper-faithful, verifiable support structure that carries the full mathematical content of the paper and can eventually be closed into a complete Lean development.

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

- the major subarguments of the paper are represented explicitly
- dependencies encode real logical support
- intermediate nodes make later verification easier rather than hiding missing structure
- the DAG gives a faithful scaffold for eventual Lean formalization

The system therefore values paper-faithful DAG improvement, not just local theorem closure.

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

The goal of theorem-stating is to construct a faithful, useful support DAG for the paper.

In this phase:

- the worker may create new nodes, revise node statements, add dependencies, and restructure the DAG when authorized
- proof-bearing nodes typically carry detailed NL proofs and may still have Lean `sorry`s
- paper-faithfulness is checked first
- then node-level correspondence is checked
- soundness is then checked on the current active proof target
- the reviewer chooses the next legal move under the current contract

The reviewer should prefer decompositions that match the actual subarguments of the paper. If a target is under-decomposed, the right next step is often broader authorized theorem work rather than repeated local polishing of one oversized node.

Theorem-stating is complete only when the relevant proof-bearing open work has been resolved well enough that the resulting DAG is faithful, sound in the NL sense, and suitable for formalization.

### 2. Proof-formalization

The goal of proof-formalization is to replace theorem-stating's provisional Lean proofs with complete Lean proofs, without losing the semantic structure already established.

In this phase:

- work is node-centered
- the system selects an active node
- the reviewer chooses edit scope and closure gates explicitly
- broader proof refactors or new nearby support require explicit authorization

Proof-formalization does not mean “forget the theorem-stating structure.” It means closing the Lean side of the already-vetted DAG, or enriching that DAG's structure in authorized ways to make it more amenable to formalization.

### 3. Cleanup

The goal of cleanup is to do constrained end-stage cleanup and hygiene work after the main semantic structure has already been established.

In this phase:

- workers do not reopen the theorem/proof planning problem
- cleanup work stays inside the cleanup contract for the current request
- the terminal review decision is phase completion (`done`), not another semantic phase design choice

## Request types and role boundaries

The system uses different roles, and each role has a different authority boundary.

### Worker

- Writes repository content (lean/tex statements, lean/tex proofs).
- Must stay within the authorized scope in the request.

### Verifiers

- Each verifier lane judges the current repository artifacts against its own canonical rubric. The four lanes are paper-faithfulness, substantiveness, correspondence, and soundness; for the authoritative definition of each, read `FAITHFULNESS.md`, `SUBSTANTIVENESS.md`, `CORRESPONDENCE.md`, and `SOUNDNESS.md` at the project root.
- Verifiers report findings only; they do not choose the next task.

### Reviewer

- Chooses the next step and guidance.
- Does not directly edit repository content.
- Should use the current blocker state and the kernel-authored legal decision surface (`continue`, `advance_phase`, `need_input`, `done`) rather than inventing control vocabulary of its own.

### Outside expert

- Enters at explicit expert-gate moments.
- Provides external semantic approval that DAG-paper target paper-faithfulness is genuine.

## End-to-end cycle

At a high level, one cycle works like this:

1. The deterministic kernel issues an authorized request.
2. The worker edits the repository within that scope.
3. The worker writes a raw JSON result, runs the exact deterministic checker named in the request, and tries to leave behind a checker-passing artifact when possible. The final artifact may honestly report any worker outcome allowed by the current contract, including `needs_restructure` when that is legal. The point is that it must truthfully describe the current state. If the checker still fails at the end of the attempt, the worker should still write the done marker after leaving the best truthful artifact so the supervisor can record the deterministic failure instead of stalling.
4. If the request is a theorem-stating or proof-formalization worker request, the runtime applies the accepted result and advances.
5. Paper-faithfulness verifiers check whether each current paper target is collectively covered by the relevant node statements.
6. If paper-faithfulness does not pass, substantiveness, correspondence, and soundness do not clear the cycle for advancement.
7. If paper-faithfulness passes, substantiveness verifiers check that each per-node `.tex` statement is a meaningful decomposition step.
8. If substantiveness does not pass, correspondence and soundness are not authoritative for advancement.
9. If substantiveness passes, correspondence verifiers check node-level Lean-vs-TeX statement alignment.
10. If correspondence does not pass, soundness is not authoritative for advancement.
11. If correspondence passes, soundness verifiers check the active NL proof target.
10. The reviewer reads the current state and verifier outputs and chooses the next action.
11. The runtime persists the result and schedules the next cycle or gate.

The system is therefore deliberately layered:

- workers propose
- verifiers judge, in the fixed order paper-faithfulness, substantiveness, correspondence, then NL soundness
- reviewer schedules
- kernel decides acceptance and transition semantics

## Theorem-stating modes

In theorem-stating, the reviewer can steer work in different modes.

### Global

- Used when the worker is authorized to improve the current theorem-stating frontier more broadly.
- This is the right mode for initial DAG construction or broader paper-faithful reshaping.

### Targeted

- Used when the worker should stay inside the kernel-authorized impact region around the current focus.
- The current focus is often a held target, but it can also be an active node or blocked-target support region.
- This is the right mode for narrower theorem-stating repair that still stays inside the theorem phase.

## What good progress looks like

- the current target becomes closer to acceptance
- the surrounding DAG becomes clearer rather than more ad hoc
- verifier objections become narrower and more local over time
- support nodes reflect real mathematical structure in the paper
- future proof-formalization becomes easier because theorem-stating made the right cuts

Bad progress often looks like:

- repeatedly restating the same target in slightly different forms
- hiding a multi-step argument inside one oversized node
- creating support nodes that do not correspond to real mathematical steps

## What agents should do when deciding between local repair and decomposition

When a proof or target is obviously composite, prefer decomposition unless there is a strong reason not to.

Signs that decomposition is needed:

- the NL proof refers to several distinct cases or subarguments
- one node is carrying too many mathematically separate responsibilities
- soundness criticism keeps pointing to missing intermediate claims
- the node's dependencies do not expose the real support structure
- a reviewer can clearly name smaller supporting facts that should exist as nodes

A monolithic node is acceptable only when it is genuinely the right representation of the paper's structure, not just the cheapest legal response under the contract.

## How to respond to a request

For any worker, verifier, or reviewer request:

1. Read the repository state from disk.
2. Read the kernel-authored request summary and contract carefully.
3. Produce the requested JSON artifact in the specified raw output path.
4. Run the exact checker command given in the prompt.
5. If the checker fails, keep working when you can make honest progress: fix the repository state or revise the artifact so that it truthfully reports the current state, then rerun the checker.
6. Whether or not the checker passes, the done marker is still the final step after the raw artifact is final; a finished failed attempt should not leave the supervisor waiting forever.

Do not treat “approximately right” as acceptable if the deterministic gate disagrees.

## How to use this file

The short scheme fragment that appears near the start of every prompt is only a reminder. This file is the fuller reference for the intended end-to-end behavior of Trellis.

If the prompt contract is terse, use this document to recover the larger purpose:

- build a faithful support DAG
- preserve the role boundaries
- let deterministic contracts, not prompt improvisation, decide acceptance
- optimize for the eventual full formalization of the paper, not only for a locally legal next step

## Canonical lane definitions

The four verifier-lane rubrics live as single-source-of-truth files at the project root. Workers, reviewers, and verifiers should consult these for the authoritative definition of each lane:

- `FAITHFULNESS.md` — paper-target faithfulness via the covering set.
- `SUBSTANTIVENESS.md` — per-node paper-substantive decomposition.
- `CORRESPONDENCE.md` — per-node Lean-vs-TeX statement alignment.
- `SOUNDNESS.md` — NL-proof rigor with paper-detail-as-floor.
