-- [TABLET NODE: DefForgeLetRec]
import Tablet.Preamble

/-- FIX 2 coverage (Definition-kind, `let rec`-bound): a pure
    Definition-kind node whose body binds a reserved-shaped auxiliary via
    `let rec`. Lean lifts `let rec _aux := …` to the real constant
    `DefForgeLetRec._aux`. This is the residual surface: the full closure
    probe never runs on a Definition-kind node (the kernel `probe_candidates`
    loop excludes it), and the Rust text backstop cannot see `let rec`
    binders, so only the universal `--scan-only` owner-file scan catches it.
    The binder is a `letId` under a `letRecDecl` ancestor — the same AST
    path the scan walks for `where` binders, exercised here via the distinct
    `let rec` surface. (`_`-prefixed shape: a reserved `eq_N` let-rec binder
    collides with Lean's generated equation lemma, so we use `_aux`.) -/
def DefForgeLetRec (n : Nat) : Nat :=
  let rec _aux (m : Nat) : Nat := m + 1
  _aux n
