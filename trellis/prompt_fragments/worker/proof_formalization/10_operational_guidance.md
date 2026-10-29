## Proof-formalization operating pattern

Start by reading the active goal state carefully before editing. Check the current theorem statement, local hypotheses, imports, nearby helper lemmas, and any reviewer comments before inventing new structure.

Treat the current tablet DAG as the search boundary by default. Prefer using existing imported support, nearby helper lemmas, and small interface repairs over wandering into unrelated parts of the repo.

For the edit-compile-fix inner loop, run `.trellis/scripts/incremental-check Tablet.NodeName` first. It reuses a warm Lean server when possible and runs a managed Lake build when needed. Confirm with `.trellis/scripts/check-job build Tablet.NodeName`; use `.trellis/scripts/check-job scratch .trellis/scratch/foo.lean` for scratch experiments. Checks print a job ID: after a disconnect, repeat the same command or use `.trellis/scripts/check-job wait JOB_ID` to reconnect. These checks are advisory; the deterministic worker check remains the sign-off gate.

If Lake reports a package URL change or deletion failure, report the diagnostic in `$HOME/.trellis/git-preflight/` and label cached-artifact probes advisory until the required current-source verification succeeds.

The full deterministic worker check (`trellis-worker-result`) is the authoritative sign-off gate: in addition to compiling the transitive Tablet closure and extracting semantic payloads, it enforces the kernel-supplied rules for this cycle (allowed scope, contract fields, structural invariants, and any cycle-specific gates). Run it yourself only when the artifact delivery contract provides `acceptance_check_command`, using that exact command when ready to submit. If the contract provides only `submit_command`, run that command and end your turn when it reports Submitted; the host performs the full acceptance check. Do not construct an additional full-check command from private context paths.

If you know or discover that a node's `.tex` proof is still incomplete, put or leave `SKETCH:` as the first nonblank line of its `proof` block when the current request and `FILESPEC.md` permit that marker for the node. The kernel marks `SKETCH:` proof bodies as `SketchAutoFail` until the marker is removed.

When a build runs long, weigh whether a different proof approach or a decomposition would serve better. Long builds slow every later iteration, and in the extreme a single heavy elaboration can load the machine enough to disrupt the run.

Before raising `maxHeartbeats`, find where the budget goes — `set_option diagnostics true` reports the counts, or replace a bare `simp` in a hot block with `simp?` and take its `simp only` suggestion — then narrow the expensive step. When a raise is still needed, size it from a measured `#count_heartbeats in` count with only modest headroom.

When a growing proof slows down or times out in a previously-green section, consider wrapping completed Prop-goal blocks with `as_aux_lemma =>` (e.g. `have h : T := by as_aux_lemma => exact …`) — the sealed proof term stops being re-traversed as the proof grows.
