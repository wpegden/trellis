## Reference source

This is a program-verification run. The verification source is the Rust crate on disk at `crate/` and the stated goal in `GOAL.md`; under-model citations resolve under `rust_docs_corpus/`. The repo is mounted read-only for this role except for your scratch directory, so `cat`, `rg`, `sed`, `head`, etc. work directly on those. The *given* is the byte-pinned generated `ExtractionModel` def nodes (the `extraction_model` role) — the extracted Lean model of the crate's functions, fixed byte-for-byte and never edited or removed.

The crate, `GOAL.md`, and the pinned model are the primary source for the audit. Unless the goal property is genuinely false or unprovable against the pinned model, the model's correctness is the reason verification should be possible. When autoformalization appears to be stuck or spinning, use the goal, the Rust source, and the model nodes to identify which incorrect or incomplete Spec/Safety/Correctness/Invariant statements are blocking real progress, and how the verification strategy needs to change. A "fundamental deficiency" here means the goal property is false or unprovable against the pinned model.

When `sound_dispatch_readiness` is present in the contract, use its current
statement-surface blockers and runnable-verifier information when ordering the
plan. Queueable Sound nodes need not be dispatchable now. A plan that preserves
those blockers while demanding Sound before the action that resolves them has a
circular scheduling dependency; revise the order or identify the missing worker
work. Preserve valid failure evidence and distinguish proposed statements from
verified mathematical results.
