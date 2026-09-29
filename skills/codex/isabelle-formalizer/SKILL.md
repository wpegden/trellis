---
name: isabelle-formalizer
description: Formalize LaTeX manuscript fragments into Isabelle/HOL. Use for theorem translation, proof planning, library fact search, naming, and checked .thy edits.
---

# Isabelle manuscript formalizer

## Goal
Translate `.tex` mathematics into checked Isabelle/HOL with readable structured Isar proofs, minimal imports, and trustworthy oracle-free closure.

## Workflow
1. Read the manuscript fragment first. Extract every hypothesis, quantifier, domain, coercion, and type-class assumption explicitly. Isabelle's types are load-bearing: pin `nat` vs `int` vs `real` up front, and remember that `nat` subtraction truncates at 0.
2. Search existing library facts before writing proof code with the discovery helper `bash .trellis/scripts/isa-query <cmd> '<query>'` (`cmd` ∈ `find_theorems` / `find_consts` / `solve_direct` / `sledgehammer` / `thm_oracles`), run from the repo root — it runs them in the tablet's warm base context and prints `.thy`-ready results in a few seconds (sledgehammer longer).
   - `find_theorems` finds facts by a conclusion/subterm pattern (dummies `_`, schematics `?x`), by a constant they mention, by `name:`, or by rule role (`intro`/`elim`/`dest`/`solves`/`simp:`); criteria combine and a leading `-` negates. It sees the loaded theories only.
   - `find_consts` finds a constant by its type, e.g. `find_consts "_ list => nat"`.
   - `solve_direct` reports whether an existing library fact already closes the goal.
   - `sledgehammer` searches the whole visible library for a proof of a leaf goal and reports the facts and method that close it. Force lemmas with `sledgehammer (add: l1 l2)`; widen with `[timeout = 120, max_facts = 100]` or `[provers = vampire z3]`. `try0` runs the internal methods; `try` adds sledgehammer + nitpick/quickcheck.
3. Build on `Main`, the `HOL` session, and the sibling tablet nodes the node imports; lean on the library freely.
4. Work each proof in three moves:
   - State the principal command's signature: the `fixes`/`assumes`/`shows`, the case split or induction, and the intermediate facts each leaf will establish.
   - Lay out the structured skeleton by hand: a `proof - ... qed` (or `proof (induction n)` / `proof (cases ...)`) whose `have`/`show` steps name the facts the argument turns on, with each leaf left for the prove step.
   - Close each leaf: run `sledgehammer` on it and ship the method it reports.
   Re-check after each nontrivial change by driving the warm Isabelle server: it re-checks only the part you changed and the active node is pre-warmed, so the first call is fast. It is a fast advisory pre-check (green is necessary but not sufficient; the deterministic worker check is the only sign-off), and it falls back to a full session build automatically, so a check that runs long is expected. To compile-test in your own scratch session, parent it on the prewarmed base and build with the shared system heaps (`isabelle build -b -o system_heaps=true`); it loads warm in seconds.
5. When a statement might be false, run `quickcheck` (fast random/exhaustive testing) and `nitpick` (finite model finder) first: a counterexample means the statement is mis-stated, which is far cheaper to learn now than after a long proof search. For a counting goal whose predicate is not executable, an independent enumerator (small `n`) checked against the closed form validates the statement just as well.
6. `find_theorems` and a bare `sledgehammer` are for discovery; ship the concrete fact list and method they yield.
7. A finished proof is `sorry`-free. If blocked, isolate the exact missing lemma and state it precisely; a deliberate skeleton with `sorry` leaves builds only under `-o quick_and_dirty=true` (the default `quick_and_dirty=false` build treats a `sorry` as an error).

## Trustworthy (oracle-free) proofs
The final theorem must rest on the kernel alone, so harvest each `sledgehammer` result in this order and ship the first that works:
- a one-word `auto`, `simp`, `blast`, `force`, or `fastforce`;
- then `metis` or `meson` with the facts `sledgehammer` names — these replay through the kernel and are correct by construction;
- then, for linear arithmetic `arith`, and for ring/field equalities `algebra`.
Treat an `smt` suggestion as a probe: it tells you which facts matter, so re-derive the leaf with `metis`/`meson` and those facts. A `metis`/`meson` proof has the smallest trusted basis and stays stable across solver and version changes.
Confirm cleanliness on the finished theorem with `thm_oracles` (or `Thm_Deps.all_oracles` in ML/batch): its output is empty for a kernel-checked proof, and any entry (a `skip_proof` from a leftover `sorry`, or a solver name) marks a leaf still to re-derive or decompose.

## Search guidance
- Use `find_theorems` when you know the shape or types of the target fact better than its name; query one shape at a time.
- Query by conclusion-shape, constant, or rule role (`intro`/`elim`/`dest` against the current goal).
- Treat search results as a search aid, not a stable API.
- Verify every fact name against the library before using it.

## Formalization conventions
- Write structured Isar: a principal-command `proof ... qed` whose intermediate `have`/`show` steps name the facts the argument turns on, so each step is checked on its own and the argument is auditable. A trailing one-line method on a leaf is the natural close.
- Keep the top-level structure in the framework connectives `==>`, `!!`, and `==` — premises as `[| P; Q |] ==> R`, or the `fixes`/`assumes`/`shows` form that names each premise — and use the HOL connectives `-->`, `ALL`, `=` inside the individual propositions.
- Match the library's naming and notation: the generated facts `T.simps`/`T.induct`/`T.cases`, `f.simps`/`f.induct`, `d_def`, and `p.intros`/`p.induct`/`p.cases`, and qualified locale/class names. Run `print_theorems` after a `definition`/`datatype`/`fun`/`inductive` to see what was generated.
- A `definition` is opaque: unfold it with its `_def` fact (`simp add: foo_def` or `unfolding foo_def`). Use `simp add:`/`del:`/`only:` to scope the simpset, and add `[simp]` deliberately.
- Choose the specification command to fit the content: `definition`/`abbreviation` for non-recursive constants, `fun`/`primrec` for recursive functions, `datatype` for free types, `inductive` for least predicates/sets, and `locale`/`class` for abstract structures.
- Keep the `imports` clause to the session roots (`Main`, `HOL` and its dotted descendants, `Complex_Main`) and the sibling-node imports the node needs; the warm base already provides the heavy library, so most nodes need only `imports Tablet_Preamble`.
- Preserve manuscript provenance in a comment, e.g. `(* Paper Lemma 2.3 *)`.
- Prefer canonical HOL abstractions over paper-local encodings.

## Output contract
When finishing a task, report:
- files changed
- new definitions/lemmas/theorems added
- remaining blockers, if any
- exact check/build command run and whether it passed
- the oracle status of any closed proof (`thm_oracles` empty)

## Lean to Isabelle quick map
For a worker fluent in Lean 4 / mathlib, the idioms map as follows:
- `Loogle`, `exact?`, `apply?`, `rw?` -> `find_theorems` / `find_consts` / `solve_direct` / `sledgehammer`
- `decide`, `#eval` -> `value`; `slim_check` -> `quickcheck` / `nitpick`
- `ring`, `field_simp` -> `algebra` (and `simp add: algebra_simps`); `linarith`, `omega`, `positivity` -> `arith`; `norm_num` -> `simp`
- `aesop`, `tauto` -> `auto` / `blast` / `force` / `fastforce`
- `rcases`, `obtain`, `rintro` -> `obtain ... where` / `cases` / `rule`/`erule`
- `calc` -> Isar `also` / `finally` with `...`
- `induction ... generalizing` -> `induction ... arbitrary:`; a custom recursor -> `induction ... rule: f.induct`
- a `by ...` tactic proof -> a structured `proof ... qed`; `unfold`/`simp only` -> `unfolding` / `simp only:` / `subst`
- `theorem foo : T := by` / `-- BODY` / `import Mathlib` -> `theory Tablet_Foo imports Tablet_Preamble begin theorem foo: "T" proof - ... qed end`
