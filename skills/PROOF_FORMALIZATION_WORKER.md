# Proof-Formalization Worker Skill

This skill applies only in the `proof_formalization` phase.

The emitted prompt is authoritative. Treat the request payload, scope
description, and checker command in the prompt as the full workflow contract.

## Lean Workflow

- start with simple automation first: `simp`, `aesop`, `norm_num`, `ring`,
  `omega`, `linarith`, `positivity`, `exact?`, `apply?`
- break long proofs into named intermediate claims
- use `calc` chains for algebraic and order-sensitive arguments
- prefer Mathlib lemmas and definitions over project-local wrappers when possible

## Loogle First

Use the local Loogle server before inventing helper statements or guessing
import paths.

```bash
bash .trellis/runtime/src/scripts/loogle_json.sh "Real.exp_neg"
bash .trellis/runtime/src/scripts/loogle_json.sh "Submodule.span"
```

Search one concept at a time. If a query is cold or broad, wait longer before
giving up.

```bash
bash .trellis/runtime/src/scripts/loogle_json.sh --timeout 120 "Submodule.span"
```

## Lean Build Hygiene

- use `lake build Tablet.NodeName` (multiple targets allowed) for inner-loop iteration on a single node — fastest path, caches oleans
- prefer `lake env lean <scratch-file>` for scratch declaration and import probes
- use the deterministic `check.py ...` command from the prompt for the actual
  acceptance gate
- you normally do not need `lake update` or `lake exe cache get` during a cycle

## Common Failure Modes

- chasing broad search without reducing the current goal
- changing declarations or files that the prompt did not authorize
- skipping the deterministic checker before writing the handoff
