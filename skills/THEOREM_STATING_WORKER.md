# Theorem-Stating Worker Skill

This skill applies only in the `theorem_stating` phase.

The emitted prompt is authoritative. Treat the request payload, scope
description, and checker command in the prompt as the full workflow contract.

## Goals

- build a paper-faithful tablet DAG
- prefer real intermediate steps that should make later Lean formalization easier
- avoid gratuitous helpers or duplicated Mathlib concepts

## Verification Order

- theorem-stating is checked in this order:
  - paper-faithfulness on paper-target coverage
  - per-node substantiveness (paper-substantive decomposition)
  - node-level Lean-vs-TeX correspondence
  - NL proof soundness on the active proof target
- if paper-faithfulness fails, later verification does not clear the cycle
- if substantiveness fails, correspondence and soundness do not clear the cycle
- if correspondence fails or splits, soundness does not clear the cycle

## Definitions And Imports

- prefer existing Mathlib definitions over project wrappers
- every real project definition should be its own node with matching `.lean` and `.tex`
- do not let theorem/lemma/corollary nodes act as hidden definitions
- `Tablet/Preamble.lean` is for imports only
- never use `import Mathlib`; import specific submodules only

## Loogle First

Use the local Loogle server before inventing project definitions or guessing
import paths.

```bash
bash .trellis/runtime/src/scripts/loogle_json.sh "Submodule.span"
bash .trellis/runtime/src/scripts/loogle_json.sh "Nat.choose"
```

Search one concept at a time. If a query is cold or broad, wait longer before
giving up.

```bash
bash .trellis/runtime/src/scripts/loogle_json.sh --timeout 120 "Submodule.span"
```

## NL Proof Standard

- rigorous, not sketch-level
- at least as detailed as the relevant part of the paper
- cite imported child nodes with `\noderef{name}`

## Lean Build Hygiene

- use `lake build Tablet.NodeName` (multiple targets allowed) for inner-loop iteration on a single node — fastest path, caches oleans
- prefer `lake env lean <scratch-file>` for scratch declaration and import probes
- use the deterministic `check.py ...` command from the prompt for the actual
  acceptance gate
- use the repo-local scratch directory named in the prompt rather than system temp files

## Output Discipline

- run the exact checker command given in the prompt before writing the handoff
- wait for that checker command to finish before writing the done marker
