# Trellis-PV verification scheme (verifier reference)

Trellis-PV verifies a safe Rust crate by maintaining a proof-tablet DAG in `Tablet/`. This is
not a one-shot translation workflow. It is a supervised multi-cycle process in which workers
propose repository changes, verifiers check specific invariants, and a reviewer decides the
next step.

The Rust functions are extracted (Charon then Aeneas) into byte-pinned generated Lean `def`
nodes (`ExtractionModel`); the run authors and proves `Spec` / `Safety` / `Correctness` /
`Invariant` goal theorems about those pinned defs. The source of truth is
the crate under `crate/` plus `GOAL.md`. This is the verifier-facing trim of the full scheme.
It drops reviewer-only mode-machinery; for the full reference, see the worker/reviewer copy.

## Core objects

- Each node has a paired `Tablet/<Node>.lean` and `Tablet/<Node>.tex`.
- `import Tablet.<OtherNode>` edges in Lean define the support DAG.
- `Tablet/Preamble.lean` is the shared import root. It should contain imports only, not project definitions.
- The `ExtractionModel` `def` nodes are the byte-pinned generated Lean model of the Rust functions: the DAG leaves, imported by the goal theorems, never authored or edited.
- The goal theorems (`Spec`/`Safety`/`Correctness`/`Invariant`) are the roots: they import the model defs and are imported by nothing. Being un-imported is the normal, expected shape of a verification goal.
- A node may be a proof-bearing result or a definition-like statement. Those behave differently under verification.
- Definition nodes use definition-like Lean declarations such as `def`, `abbrev`, `noncomputable def`, and the data declarations `structure`, `inductive`, and `class` (whose fields/constructors sit below `-- BODY`, split off by a `where` delimiter). An empty `where`-body is the deferred-definition placeholder and is kernel-autofailed for correspondence, like a `True` body.
- Proof-bearing nodes use theorem-like Lean declarations such as `theorem`, `lemma`, and `instance` (an instance discharges a class's law fields and pairs to a `lemma`-style `.tex` claim). `.tex` labels like `corollary` and `helper` are statement-environment categories, not separate Lean declaration keywords.

## What the tablet is supposed to represent

The tablet is meant to be a verification of the declared target functions. In a good tablet:

- every declared verification target is covered by goal theorems stating its `GOAL.md` contract at full strength over the pinned model
- intermediate nodes correspond to real obligations or reused facts the verification needs, not arbitrary scaffolding
- the support DAG mirrors the actual logical structure of each proof
- the `ExtractionModel` defs are given and pinned, never authored
- proof-bearing nodes have either a complete Lean proof or a rigorous NL proof at the detail a Lean formalizer needs

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

### Soundness invariants

- Every proof-bearing node must end in one of two acceptable states:
  - a complete Lean proof with no `sorry`
  - a rigorous, fully detailed NL proof supported by its dependencies
    - this should be line-by-line checkable and all dependencies in the argument should correspond to DAG dependencies of the node
    - the detail floor is what a Lean formalizer needs: every monadic/arith/cast/shift/bound step the Lean proof must discharge is justified in the NL proof, citing the model `def` semantics and the `GOAL.md` bounds
- In theorem-stating, proof-bearing nodes usually begin with NL proofs and may still contain Lean `sorry`s.
- In proof-formalization, those Lean `sorry`s are eliminated node by node.

### Definition and import hygiene

- The `ExtractionModel` defs are given and pinned; they are not restated, wrapped, or edited.
- Mathlib imports should be specific (don't import all of Mathlib).
- Imports should reflect real dependency structure.

## Phases

Trellis-PV has three semantic phases.

### 1. Theorem-stating

The goal of theorem-stating is to construct the goal-theorem DAG over the pinned model. In this
phase, proof-bearing nodes typically carry detailed NL proofs and may still have Lean `sorry`s.
Per-node substantiveness is checked first, then node-level correspondence, and finally soundness
on the current active proof target.

### 2. Proof-formalization

The goal of proof-formalization is to replace theorem-stating's provisional Lean proofs with
complete Lean proofs, without losing the semantic structure already established. Work is
node-centered.

### 3. Cleanup

The goal of cleanup is constrained end-stage cleanup and hygiene work after the main structure
is established. Workers do not reopen the theorem/proof planning problem here.

## Verifier role boundaries

Each verifier lane judges the current repository artifacts against its own canonical rubric. The
three live lanes are substantiveness, correspondence, and soundness; for the authoritative
definition of each, read `SUBSTANTIVENESS.md`, `CORRESPONDENCE.md`, and `SOUNDNESS.md` at the
project root.

Verifiers report findings only; they do not choose the next task. Reviewers do that under the
kernel-authored decision surface; verifiers report Pass / Fail (and per-node verdicts where the
lane operates per-node) against the current artifacts and let the reviewer schedule the next move.

## How to respond to a request

For any verifier request:

1. Read the repository state from disk.
2. Read the kernel-authored request summary and contract carefully.
3. Produce the requested JSON artifact in the specified raw output path.
4. Run the exact checker command given in the prompt.
5. If the checker fails, keep working when you can make honest progress: revise the artifact so that it truthfully reports the current state, then rerun the checker.
6. Whether or not the checker passes, the done marker is still the final step after the raw artifact is final; a finished failed attempt should not leave the supervisor waiting forever.

Do not treat "approximately right" as acceptable if the deterministic gate disagrees.

## Canonical lane definitions

The verifier-lane rubrics live as single-source-of-truth files at the project root. Verifiers
should consult the relevant file for the authoritative definition of their lane:

- `SUBSTANTIVENESS.md` — per-node obligation the verification requires.
- `CORRESPONDENCE.md` — per-node Lean-vs-TeX statement alignment.
- `SOUNDNESS.md` — NL-proof rigor with the Lean-formalizer detail floor.
