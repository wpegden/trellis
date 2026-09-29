## Proof-formalization operating pattern

Start by reading the active goal state carefully before editing. Check the current theorem statement, local hypotheses, imports, nearby helper lemmas, and any reviewer comments before inventing new structure.

Treat the current tablet DAG as the search boundary by default. Prefer using existing imported support, nearby helper lemmas, and small interface repairs over wandering into unrelated parts of the repo.

For the edit-compile-fix inner loop, run `.trellis/scripts/incremental-check Tablet.NodeName` first: it drives a warm Lean server that re-elaborates only the part of the proof you changed, so a late edit in a long proof returns errors in a fraction of the `lake build` time, and on the node you are actively proving the server is pre-warmed so the first call is fast too. It is a fast advisory pre-check: green is necessary but not sufficient, and the deterministic worker check remains the only sign-off. When it cannot help — it is unavailable, or a node is too large for the warm server — it falls back to `lake build` automatically, so a check that runs long is expected, not an error. Use `lake build Tablet.NodeName` (or several `Tablet.X Tablet.Y` targets at once) to confirm a green and as the fallback; it caches the resulting `.olean` for the next compile. For one-off scratch experiments outside `Tablet/`, `lake env lean .trellis/scratch/foo.lean` runs Lean directly without writing an olean.

If Lake reports a package URL change or deletion failure, report the diagnostic in `$HOME/.trellis/git-preflight/` and label cached-artifact probes advisory until the required current-source verification succeeds.

The full deterministic worker check (`trellis-worker-result`) is the authoritative sign-off gate: in addition to compiling the transitive Tablet closure and extracting semantic payloads, it enforces the kernel-supplied rules for this cycle (allowed scope, contract fields, structural invariants, and any cycle-specific gates). Run it once you believe the proof is ready to submit.

If you know or discover that a node's `.tex` proof is still incomplete, put or leave `SKETCH:` as the first nonblank line of its `proof` block when the current request and `FILESPEC.md` permit that marker for the node. The kernel marks `SKETCH:` proof bodies as `SketchAutoFail` until the marker is removed.

When a build runs long, weigh whether a different proof approach or a decomposition would serve better. Long builds slow every later iteration, and in the extreme a single heavy elaboration can load the machine enough to disrupt the run.

Before raising `maxHeartbeats`, find where the budget goes — `set_option diagnostics true` reports the counts, or replace a bare `simp` in a hot block with `simp?` and take its `simp only` suggestion — then narrow the expensive step. When a raise is still needed, size it from a measured `#count_heartbeats in` count with only modest headroom.

When a growing proof slows down or times out in a previously-green section, consider wrapping completed Prop-goal blocks with `as_aux_lemma =>` (e.g. `have h : T := by as_aux_lemma => exact …`) — the sealed proof term stops being re-traversed as the proof grows.
