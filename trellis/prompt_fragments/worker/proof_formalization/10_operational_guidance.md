## Proof-formalization operating pattern

Start by reading the active goal state carefully before editing. Check the current theorem statement, local hypotheses, imports, nearby helper lemmas, and any reviewer comments before inventing new structure.

Treat the current tablet DAG as the search boundary by default. Prefer using existing imported support, nearby helper lemmas, and small interface repairs over wandering into unrelated parts of the repo.

For the edit-compile-fix inner loop, use `lake build Tablet.NodeName` (or pass several `Tablet.X Tablet.Y` targets at once) — this is the fastest reliable way to surface Lean error messages while iterating on a proof, and it caches the resulting `.olean` for the next compile. For one-off scratch experiments outside `Tablet/`, `lake env lean .trellis/scratch/foo.lean` runs Lean directly without writing an olean.

The full deterministic worker check (`trellis-worker-result`) is the authoritative sign-off gate: in addition to compiling the transitive Tablet closure and extracting semantic payloads, it enforces the kernel-supplied rules for this cycle (allowed scope, contract fields, structural invariants, and any cycle-specific gates). Run it once you believe the proof is ready to submit.

If you know or discover that a node's `.tex` proof is still incomplete, put or leave `SKETCH:` as the first nonblank line of its `proof` block when the current request and `FILESPEC.md` permit that marker for the node. The kernel marks `SKETCH:` proof bodies as `SketchAutoFail` until the marker is removed.
