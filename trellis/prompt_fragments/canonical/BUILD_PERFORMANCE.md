# Build Performance

The reference for a Lean build that is slow, hits `maxHeartbeats`, or exhausts memory. Techniques here apply inside a node's proof body, preamble, and scratch experiments; `FILESPEC.md` governs file shape. Escalation order: measure, narrow the named cost center, seal completed blocks, peel closed helpers, decompose.

## Measure first

The expensive step is usually one small span, and the counters name it. Attach a profiler before editing.

- `set_option diagnostics true in <declaration>` prints subsystem counters after elaboration (Lean suggests this option itself at a deterministic timeout). Reading them: high *unfolded declarations* / *unfolded reducible declarations* means definitional unfolding is the sink; high instance counts under `synthInstance` mean typeclass search; high *isDefEq heuristic applications* mean unification; counters tagged `[kernel]` are kernel-side reduction. `set_option diagnostics.threshold N` hides counters below `N`.
- `set_option profiler true` prints `<step> took <time>` wall-clock lines for elaboration steps over the reporting threshold (100 ms; lower it with `set_option profiler.threshold <ms>`). This attributes cost to a tactic or elaborator by name.
- `#count_heartbeats in <declaration>` (available with Mathlib) reports heartbeat consumption in `maxHeartbeats` units and, when over budget, prints a `Try this:` with a sufficient `set_option maxHeartbeats` value. `#count_heartbeats! in` runs the measurement 10 times and reports min/max/standard deviation — use it when one count looks noisy.
- `set_option trace.profiler true in` attaches times to the elaboration trace tree, locating the slow subterm within a tactic; `set_option trace.profiler.output <file>` exports the trace as Firefox-Profiler-loadable JSON when the tree is too big to read inline.

Heartbeats count elaborator work deterministically, independent of machine load, so a heartbeat profile taken in a scratch file transfers to the checked build.

## Cost centers

Symptom, then the concrete move.

**Bare `simp` in a hot block.** `simp` searches the full default simp set on every call. Run `simp?` at the call site and accept the printed `simp only [...]`; the closed list elaborates faster and stays stable as the library evolves. `simpa?` serves `simpa` the same way. A cheap `simp only` pre-pass that shrinks the goal also cheapens whatever automation follows it.

**Typeclass-search blowup.** Confirm with the diagnostics instance counters, then watch the search with `set_option trace.Meta.synthInstance true in` on the declaration — long backtracking chains of failing candidates name the problematic class. `#synth <Class> <args>` reproduces one search in isolation. Fixes: bind the instance once with `haveI`/`letI` so later uses become lookups; register it file-wide as `@[local instance]` (optionally with a priority, `@[local instance 2000]`) in the preamble; pass instance arguments explicitly at the expensive call sites.

**Expensive `isDefEq` unification — the hidden cost center.** The profile shows elaboration time with no single guilty tactic, and diagnostics shows large isDefEq counts. Typical sources: `convert`, whose congruence search runs defeq checks across mismatched subterms; `_` placeholders and implicit arguments whose inference forces deep unfolding; `rw`/`exact` against terms whose heads agree only after heavy reduction. Moves: fill the `_`s and implicit arguments the profiler implicates with explicit terms; pin the expected form up front with `show` so unification meets the stated type; replace `convert` with `refine` plus explicit `congr`/`ext` steps that name where the mismatch lives.

**Kernel reduction: `decide` and term-level `rfl`.** These make the kernel evaluate the goal, and kernel typechecking runs outside the heartbeat budget — the failure mode is unbounded time and memory on a `decide`/`rfl` over a recursive function or a large literal, surfacing after elaboration looks fine (lean4#5321). `norm_num` proves numeric goals with small certificate terms; `simp` with the relevant equation lemmas computes at the elaborator level, under the budget. Reserve `decide` for small closed goals whose decision procedure you know evaluates in a few steps.

**`omega` and `grind` scope.** `omega` consumes every `Nat`/`Int` linear fact in the local context, so in a fat context `clear` unrelated hypotheses before calling it. `grind` additionally searches the `@[grind]`-annotated library; `grind only [...]` pins its lemma set the way `simp only` does.

## Non-monotone elaboration (lean4#5108)

Adding a correct step can push an earlier, previously-green block past the budget. Mechanism: a tactic proof elaborates into one growing term, and later steps that depend on earlier ones traverse and substitute the earlier proof terms during metavariable instantiation — per-step cost grows with the accumulated term size, quadratically over the proof and worse when dependent terms nest. Lean abstracts a proof into auxiliary lemmas only when the whole declaration completes, so mid-proof the entire term stays live.

The fix is sealing completed `Prop`-goal blocks with `as_aux_lemma =>` (in core since Lean 4.18):

```lean
have hkey : ∀ x, P x → Q x := by
  as_aux_lemma =>
    intro x hx
    long_completed_tactic_block
```

`as_aux_lemma => tac` runs `tac`, wraps the finished proof term as a compiler-generated auxiliary lemma (a `_proof_N`-shaped constant), and leaves a small constant reference in the main term, so later steps stop re-traversing it. Constraints: the block must close its goal completely, and the goal must be a `Prop` — anything else fails loudly at wrap time. The generated constants are reserved-shaped names the local-closure probes transparent-walk (see `FILESPEC.md`), so sealing is invisible to node acceptance. Apply it to the largest completed `have`/`obtain` blocks, starting from the top of the proof, whenever a growing proof slows in sections that used to be fast.

## `maxHeartbeats` discipline

- The budget is per-declaration; the default is 200000, and `0` disables the check. In a Tablet node the option is a standalone preamble line above the node marker (`FILESPEC.md`); in scratch files, `set_option maxHeartbeats N in` scopes a raise to one declaration.
- Set it from measurement: run `#count_heartbeats in` and take its suggested value — the `2^k * 200000` steps carry headroom, which matters because counts drift as mathlib and the support cone move.
- Known budget escapes: equation-lemma generation for a definition runs at the default whatever the file sets (lean4#11546), and kernel reduction sits outside the budget (above) — a raise fixes neither.
- A raised ceiling recurs as cost on every future rebuild of the node, and in Trellis a preamble `maxHeartbeats` at or above 2,000,000 — or a file over 3,000 lines — classifies the node as **giant**: the warm incremental-check server skips it, and olean prewarm passes it by. Narrowing the expensive step and lowering the ceiling back below the threshold restores both services.

## Structural fixes

- **Peel closed helpers.** Move completed material into Lean-closed helper nodes (`FILESPEC.md` governs their shape): each helper elaborates once and caches as an olean, the active node shrinks, and every later iteration cheapens. Closed helpers are waived from soundness verification, and adding them is legal even under `scope_contract.allow_new_obligations=false`.
- **In-file auxiliaries.** Declarations inside `namespace NodeName ... end` below `-- BODY` are private to the node by design; use them for lemmas only this node needs. When a second node needs the fact, promote it to a registered helper node.
- **Full decomposition.** A node's `.tex` proof length is the available predictor of its Lean elaboration cost; above roughly 130 lines of proof TeX, prefer the complete-decomposition workflow — a faithful split of the whole proof into meaningfully simpler nodes. When the current gates preclude the needed new nodes, report `NeedsRestructure` with suggested nodes so the reviewer can widen them.

## Memory

Elaboration RSS is driven by the mechanisms above — accumulated proof terms (the re-traversal mechanism), kernel reduction on `decide`/`rfl` goals (the unbounded case) — plus long-lived servers that reaccumulate state while reprocessing large declarations (lean4#6753; `LEAN_NUM_THREADS=1` caps that growth by serializing elaboration). The warning sign: a single `lean` process climbing steadily through the multi-GB range on one declaration, well past the import-closure baseline.

When a build's failure line reports it was stopped at a memory budget, treat that report as a diagnosis of the node, on par with a timeout you observed yourself: shrink its elaboration by the techniques above.

## Trellis tooling for the loop

- `incremental-check Tablet.NodeName` — the warm advisory checker for the edit-compile-fix loop. A node under the giant thresholds is served warm; a giant falls back to `lake build` automatically, with the advisory naming which signal tripped.
- Scratch experiments — `lake env lean .trellis/scratch/foo.lean`. Reproduce the slow step there with the node's imports and preamble options, iterate with the measurement options enabled, and port the fast version back. This is also where an `in`-scoped `maxHeartbeats` raise belongs while experimenting.
- Loogle — shape-search for the intended library lemma; a proof citing the right lemma elaborates cheaper than automation re-deriving it. Invocation details are in the role fragments.
