-- [TABLET NODE: ForgeWhere]
import Tablet.Preamble

/-- FIX 2 surface (where-bound): a `where`-bound auxiliary. Lean lifts
    `where _helper := …` to the real constant `ForgeWhere._helper`. The
    binder is nested in the declVal, NOT a top-level declId, so neither the
    Rust line-scanner nor a naive declId walk sees it; the parse walk
    collects `letId` names under a `letRecDecl` ancestor. (A reserved
    `eq_N` where-binder collides with Lean's generated equation lemma, so we
    use the `_`-prefixed shape, which builds.) -/
def ForgeWhere (n : Nat) : Nat := _helper n where
  _helper (m : Nat) : Nat := m + 1
