# Proof-Formalization Worker Skill (Isabelle/HOL backend)

This skill applies only in the `proof_formalization` phase.

The emitted prompt is authoritative. Treat the request payload, scope
description, and checker command in the prompt as the full workflow contract.

## Isar Workflow

- read the active goal state first: the `fixes`/`assumes`/`shows` of the
  principal command, the imports, and nearby helper nodes
- write the proof as structured Isar: a principal-command `proof … qed` whose
  intermediate `have`/`show` steps name the facts the argument turns on
- work it as Draft -> Sketch -> Prove: use `sketch` to emit the `proof … qed`
  skeleton with one `show` per leaf goal, then close each leaf
- run `sledgehammer` on a leaf and ship the method it reports; harvest in this
  order: a one-word `auto`/`simp`/`blast`/`force` first, then `metis`/`meson`
  with the facts sledgehammer names; treat an `smt` suggestion as a probe that
  tells you which facts matter and rephrase it as a harvested method
- build on the `HOL`/`Main` library over re-deriving standard material

## Fact discovery first

Search from inside the prover session before inventing helper statements.

- `find_theorems` finds existing facts by the shape of their conclusion or by a
  constant they mention, e.g. `find_theorems "_ + _ = _ + _"` or
  `find_theorems name: "comm" "_ * _"`
- `find_consts` finds a constant by its type
- `sledgehammer` searches the library for a proof of the current goal

Search one shape at a time. If a query is broad, give it time before giving up.

## Isabelle Build Hygiene

- drive the warm Isabelle server for the edit-check-fix inner loop: it
  re-checks only the part of the theory you changed, and the node you are
  actively proving is pre-warmed
- the warm server is a fast advisory pre-check; the deterministic worker check
  is the only sign-off, and a full session build is the fallback when the warm
  server cannot help (it falls back automatically, so a long check is expected)
- use the deterministic `check.py …` command from the prompt for the actual
  acceptance gate

## Common Failure Modes

- chasing broad search without reducing the current goal
- changing declarations or files that the prompt did not authorize
- skipping the deterministic checker before writing the handoff
