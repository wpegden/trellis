## Source of truth

The source of truth for this verification is the Rust crate on disk under `crate/`
(entry `src/lib.rs`) together with `GOAL.md` at the repo root. The repo
is mounted into your sandbox, so `cat`, `grep`, `head`, etc. work directly on both.

`GOAL.md` states, per target function, the precondition regime, the correctness/congruence
core, the output bound, and the safety (panic-/overflow-freedom) obligation; `crate/src/lib.rs`
is the exact Rust being verified. Consult both directly whenever you author or restructure a
goal theorem (`Spec`/`Safety`/`Correctness`/`Invariant`), repair a correspondence or soundness
blocker, or re-attack a node that previous bursts could not close.

A "fundamental difficulty" almost never means a problem with the source: the
`ExtractionModel` `def` nodes faithfully encode the Rust semantics (and are inviolate
ground truth — see the Extraction-model canonical doc). It means a goal theorem's statement does not yet
capture the obligation in `GOAL.md`, or a precondition is too weak or too strong. Diagnose
the actual source of difficulty against `GOAL.md` and the Rust, and align the next edit with
them rather than with the prior failed attempt.
