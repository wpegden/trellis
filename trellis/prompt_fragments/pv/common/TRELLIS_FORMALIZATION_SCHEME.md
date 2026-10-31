# Trellis-PV verification scheme

Trellis-PV verifies a safe Rust crate by maintaining a proof-tablet DAG in `Tablet/`. This is
not a one-shot translation workflow. It is a supervised multi-cycle process in which workers
propose repository changes, verifiers check specific invariants, and a reviewer decides the
next step.

The Rust functions are extracted (Charon then Aeneas) into byte-pinned generated Lean `def`
nodes (`ExtractionModel`); the run authors and proves `Spec` / `Safety` / `Correctness` /
`Invariant` goal theorems about those pinned defs. The source of truth is
the crate under `crate/` (entry `src/lib.rs`) plus `GOAL.md` at the repo root. The point of
the system is not merely to produce Lean code: it is to build a verified support structure
that certifies each declared verification target's `GOAL.md` contract against the pinned model
and can be closed into a complete Lean development.

## Core objects

- Each node has a paired `Tablet/<Node>.lean` and `Tablet/<Node>.tex`.
- `import Tablet.<OtherNode>` edges in Lean define the support DAG.
- `Tablet/Preamble.lean` is the shared import root. It should contain imports only, not project definitions.
- The `ExtractionModel` `def` nodes are the byte-pinned generated Lean model of the Rust functions. They are the leaves of the DAG: imported by the goal theorems, never authored, edited, or removed. Their Lean text is byte-compared at every acceptance in every mode.
- The goal theorems (`Spec`/`Safety`/`Correctness`/`Invariant`) are the roots: they import the model defs and are imported by nothing. Being un-imported is the normal, expected shape of a verification goal.
- A node may be a proof-bearing result or a definition-like statement. Those behave differently under verification.
- Definition nodes use definition-like Lean declarations such as `def`, `abbrev`, `noncomputable def`, and the data declarations `structure`, `inductive`, and `class`. A `structure`/`inductive`/`class` splits at `-- BODY` with the head (up through `where`) above the marker and the fields/constructors below; an `inductive` must use the `where` form. An empty `where`-body (no fields/constructors) is the deferred-definition placeholder, the structural analog of the `True` body, and the kernel autofails it for correspondence.
- Proof-bearing nodes use theorem-like Lean declarations such as `theorem` or `lemma`, and also `instance`. An `instance` discharges a class's law fields and routes through soundness like a theorem; it pairs to a `lemma`-style `.tex` claim, must be named, and may use `:=`, `:= by`, or structure-instance `where` as its body delimiter. `.tex` labels like `corollary` and `helper` are statement-environment categories, not separate Lean declaration keywords.

## What the tablet is supposed to represent

The tablet is meant to be a verification of the declared target functions: the goal theorems
state each function's `GOAL.md` contract over the pinned model, supported by the lemmas and
invariants their proofs need. In a good tablet:

- every declared verification target is covered by goal theorems (`Spec`/`Safety`/`Correctness`) stating its `GOAL.md` contract at full strength
- dependencies encode real logical support: a goal theorem imports the model defs it reasons about and the lemma/invariant nodes its proof consumes
- reused facts (e.g. a model-contract lemma several goal theorems import) are isolated as their own lemma nodes
- intermediate nodes make later verification easier rather than hiding missing structure

The system therefore values goal-coverage DAG improvement, not just local theorem closure.

## Main invariants

The system is trying to maintain the following invariants at all times.

### Pairing invariants

- Every present node has both a Lean artifact and an NL artifact.

### Correspondence invariants

- The Lean statement and the NL statement for a node must genuinely correspond in mathematical meaning.
- A declared verification target is only considered covered if the current goal-theorem nodes genuinely capture its full `GOAL.md` contract.
- Proof-only edits should not be treated as statement changes unless the actual statement meaning changed.

### Goal-coverage invariants

- The tablet should make genuine progress toward proving the declared verification targets rather than merely repackaging the same goal repeatedly.
- Intermediate nodes should reflect real obligations or reused facts the verification needs, not arbitrary or misleading scaffolding.
- The byte-pinned `ExtractionModel` defs are the given model; the worker authors only the goal theorems and their support over them.

### Soundness invariants

- Every proof-bearing node must end in one of two acceptable states:
  - a complete Lean proof with no `sorry`
  - a rigorous, fully detailed NL proof supported by its dependencies
    - this should be line-by-line checkable and all dependencies in the argument should correspond to DAG dependencies of the node
    - the detail floor is what a Lean formalizer needs: every monadic/arith/cast/shift/bound step the Lean proof must discharge is justified in the NL proof, citing the model `def` semantics and the `GOAL.md` bounds
- In theorem-stating, proof-bearing nodes usually begin with NL proofs and may still contain Lean `sorry`s.
- In proof-formalization, those Lean `sorry`s are eliminated node by node.

### Definition and import hygiene

- The `ExtractionModel` defs are given and pinned; do not restate, wrap, or edit them.
- Mathlib imports should be specific (don't import all of Mathlib).
- Imports should reflect real dependency structure.

## Phases

Trellis-PV has three semantic phases.

### 1. Theorem-stating

The goal of theorem-stating is to construct the goal-theorem DAG over the pinned model: the
`Spec`/`Safety`/`Correctness`/`Invariant` theorems covering each declared verification target,
plus the support they need.

In this phase:

- the worker may create new goal/support nodes, revise their statements, add dependencies, and restructure the DAG when authorized
- proof-bearing nodes typically carry detailed NL proofs and may still have Lean `sorry`s
- per-node substantiveness is checked
- node-level correspondence is checked
- soundness is then checked on the current active proof target
- the reviewer chooses the next legal move under the current contract

Theorem-stating is complete only when the open proof-bearing work has been resolved well
enough that the goal theorems cover the targets, are sound in the NL sense, and are suitable
for formalization.

### 2. Proof-formalization

The goal of proof-formalization is to replace theorem-stating's provisional Lean proofs with
complete Lean proofs, without losing the semantic structure already established. Work is
node-centered: the system selects an active node, and the reviewer chooses edit scope and
closure gates explicitly. Every accepted proof must keep `#print axioms` within
`[propext, Classical.choice, Quot.sound]`.

### 3. Cleanup

The goal of cleanup is constrained end-stage cleanup and hygiene work after the main structure
is established. Workers do not reopen the theorem/proof planning problem here; the terminal
review decision is phase completion (`done`).

## Request types and role boundaries

The system uses different roles, and each role has a different authority boundary.

### Worker

- Writes repository content (lean/tex statements, lean/tex proofs).
- Must stay within the authorized scope in the request.

### Verifiers

- Each verifier lane judges the current repository artifacts against its own canonical rubric. The three live lanes are substantiveness, correspondence, and soundness; for the authoritative definition of each, read `SUBSTANTIVENESS.md`, `CORRESPONDENCE.md`, and `SOUNDNESS.md` at the project root.
- Verifiers report findings only; they do not choose the next task.

### Reviewer

- Chooses the next step and guidance.
- Does not directly edit repository content.
- Should use the current blocker state and the kernel-authored legal decision surface (`continue`, `advance_phase`, `need_input`, `done`) rather than inventing control vocabulary of its own.

## End-to-end cycle

At a high level, one cycle works like this:

1. The deterministic kernel issues an authorized request.
2. The worker edits the repository within that scope.
3. The worker writes a raw JSON result, runs the exact deterministic checker named in the request, and tries to leave behind a checker-passing artifact when possible. The final artifact must truthfully describe the current state, including `needs_restructure` when that is legal. If the checker still fails at the end of the attempt, the worker should still write the done marker after leaving the best truthful artifact so the supervisor records the deterministic failure instead of stalling.
4. If the request is a theorem-stating or proof-formalization worker request, the runtime applies the accepted result and advances.
5. Substantiveness verifiers check that each per-node `.tex` statement is a meaningful, non-duplicative obligation the verification needs.
6. If substantiveness does not pass, correspondence and soundness are not authoritative for advancement.
7. If substantiveness passes, correspondence verifiers check node-level Lean-vs-TeX statement alignment.
8. If correspondence passes, soundness verifiers check the active NL proof target.
9. The reviewer reads the current state and verifier outputs and chooses the next action.
10. The runtime persists the result and schedules the next cycle or gate.

The system is therefore deliberately layered:

- workers propose
- verifiers judge, in the order substantiveness, correspondence, then NL soundness
- reviewer schedules
- kernel decides acceptance and transition semantics

## Theorem-stating modes

In theorem-stating, the reviewer can steer work in different modes.

### Global

- Used when the worker is authorized to improve the current theorem-stating frontier more broadly.
- This is the right mode for initial goal-DAG construction or broader reshaping.

### Targeted

- Used when the worker should stay inside the kernel-authorized impact region around the current focus.
- This is the right mode for narrower theorem-stating repair that still stays inside the theorem phase.

## What good progress looks like

- the current target becomes closer to acceptance
- the surrounding DAG becomes clearer rather than more ad hoc
- verifier objections become narrower and more local over time
- support nodes reflect real obligations and reused facts the verification needs
- future proof-formalization becomes easier because theorem-stating made the right cuts

Bad progress often looks like:

- repeatedly restating the same goal in slightly different forms
- hiding a multi-step argument inside one oversized node
- creating support nodes that do not correspond to real verification steps

## How to respond to a request

For any worker, verifier, or reviewer request:

1. Read the repository state from disk.
2. Read the kernel-authored request summary and contract carefully.
3. Produce the requested JSON artifact in the specified raw output path.
4. Run the exact checker command given in the prompt.
5. If the checker fails, keep working when you can make honest progress: fix the repository state or revise the artifact so that it truthfully reports the current state, then rerun the checker.
6. Whether or not the checker passes, the done marker is still the final step after the raw artifact is final; a finished failed attempt should not leave the supervisor waiting forever.

Do not treat "approximately right" as acceptable if the deterministic gate disagrees.

## Canonical lane definitions

The verifier-lane rubrics live as single-source-of-truth files at the project root. Workers,
reviewers, and verifiers should consult these for the authoritative definition of each lane:

- `SUBSTANTIVENESS.md` — per-node obligation the verification requires.
- `CORRESPONDENCE.md` — per-node Lean-vs-TeX statement alignment.
- `SOUNDNESS.md` — NL-proof rigor with the Lean-formalizer detail floor.
