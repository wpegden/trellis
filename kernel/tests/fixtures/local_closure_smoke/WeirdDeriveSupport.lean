import Lean

/- Operator-owned fixture support for a third-party-style deriving handler.
   Its generated declaration deliberately lives outside the owner's namespace,
   so only exact module metadata can attribute it correctly. -/
class WeirdDerive (α : Type) : Prop where
  witness : True

open Lean Elab Command in
private def weirdDerivingHandler (typeNames : Array Name) : CommandElabM Bool := do
  let #[_] := typeNames | return false
  let declName := mkIdent `CobaltMoth922
  elabCommand (← `(theorem $declName : True := True.intro))
  return true

initialize Lean.Elab.registerDerivingHandler ``WeirdDerive weirdDerivingHandler

/- Rename-invariance twin of `WeirdDerive`. It differs from the handler
   above in exactly one respect: the name of the declaration it generates.
   Classification must therefore be identical for both — if it is not,
   something has reintroduced name-based authority. The generated name is
   placed under an unrelated namespace, the shape that previously changed
   the legacy dependency key. -/
class WeirdDeriveRenamed (α : Type) : Prop where
  witness : True

open Lean Elab Command in
private def weirdDerivingHandlerRenamed (typeNames : Array Name) : CommandElabM Bool := do
  let #[_] := typeNames | return false
  let declName := mkIdent `Alien.NeonTapir77
  elabCommand (← `(theorem $declName : True := True.intro))
  return true

initialize Lean.Elab.registerDerivingHandler ``WeirdDeriveRenamed weirdDerivingHandlerRenamed
