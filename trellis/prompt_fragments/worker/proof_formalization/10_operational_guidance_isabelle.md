## Proof-formalization operating pattern

Start by reading the active goal state carefully before editing. Check the current statement, the `fixes`/`assumes`/`shows` of the principal command, the `imports`, nearby helper nodes, and any reviewer comments before inventing new structure.

Treat the current tablet DAG as the search boundary by default. Prefer using existing imported support, nearby helper nodes, and small interface repairs over wandering into unrelated parts of the repo.

Write the proof as structured Isar. Read the active goal state first, then build the proof as a principal-command `proof … qed` whose intermediate `have`/`show` steps name the facts the argument turns on, so the structure of the argument is visible and each step is checked on its own.

Work the proof in three moves:

- **Draft.** Lay out the claim and the shape of the argument: the `fixes`/`assumes`/`shows`, the case split or induction, and the intermediate facts each leaf will establish.
- **Skeleton.** Read the active goal state and write the `proof … qed` skeleton by hand: `proof -` (or `proof (induction n)` / `proof (cases …)`), then one `have`/`show` per subgoal the goal state presents, each naming the fact that leaf establishes, with each leaf left for the prove step. Writing it by hand keeps the structure tied to the goals you actually see.
- **Prove.** Close each leaf. Run `sledgehammer` on the leaf to find a proof, then ship the method it reports. Harvest in this order: take a one-word `auto`, `simp`, `blast`, or `force` first; then `metis`/`meson` with the facts sledgehammer names. Treat an `smt` suggestion as a probe that tells you which facts matter, and rephrase it as one of the harvested methods.

For the edit-compile-fix inner loop, run `bash .trellis/scripts/incremental-check Tablet.<Node>` first: it drives the warm Isabelle server, re-checking only the part of the theory you changed, so a late edit in a long proof returns errors quickly, and the node you are actively proving is pre-warmed so the first call is fast too. **Every `.thy` you write or edit must come back clean from this check before you submit — a node that fails to compile is rejected.** It is a fast advisory pre-check: green is necessary but not sufficient, and the deterministic worker check remains the only sign-off. When the warm server cannot help — it is unavailable, or a theory is too large for it — it reports that and points you at `isabelle build`; confirm a final green there, so a check that runs long is expected, not an error.

The full deterministic worker check (`trellis-worker-result`) is the authoritative sign-off gate: in addition to building the transitive Tablet closure and extracting the per-node trust-basis certificate, it enforces the kernel-supplied rules for this cycle (allowed scope, contract fields, structural invariants, and any cycle-specific gates). Run it once you believe the proof is ready to submit.

If you know or discover that a node's `.tex` proof is still incomplete, put or leave `SKETCH:` as the first nonblank line of its `proof` block when the current request and the file spec permit that marker for the node. The kernel marks `SKETCH:` proof bodies as `SketchAutoFail` until the marker is removed. The Isabelle analogue inside the `.thy` itself is to leave the proof as `sorry`, which marks the node as still open.
