import Lean

/-!
Trusted mechanical statement derivation. The target proof is parsed but never
elaborated or installed. All output is a proposition, not a proof certificate.
Run only through the checker's existing confined Lean execution boundary.
-/
open Lean Elab Meta

private def parse (category : Name) (text : String) : TermElabM Syntax := do
  let ictx := Parser.mkInputContext text "<statement-transform>"
  let parser := Parser.andthenFn Parser.whitespace (Parser.categoryParserFnImpl category)
  let env ← getEnv
  let pmctx : Parser.ParserModuleContext := {
    env, options := ← getOptions, currNamespace := ← getCurrNamespace, openDecls := ← getOpenDecls
  }
  let state := parser.run ictx pmctx (Parser.getTokenTable env) (Parser.mkParserState text)
  if !state.allErrors.isEmpty then throwError "{state.toErrorMsg ictx}"
  unless ictx.atEnd state.pos do throwError "{(state.mkError "end of input").toErrorMsg ictx}"
  return state.stxStack.back

private def closedProp (e : Expr) : TermElabM Expr := do
  Term.synthesizeSyntheticMVarsNoPostponing
  let e ← instantiateMVars e
  if e.hasMVar || e.hasFVar || e.hasSorry then
    throwError "statement transformation requires a closed, fully elaborated proposition without sorry"
  unless ← isProp e do throwError "statement transformation requires Prop"
  checkWithKernel e
  return e

private def render (registeredEnv : Environment) (e : Expr) : TermElabM String := do
  let e ← closedProp e
  -- Signature/condition elaboration can create temporary matcher definitions.
  -- They are not part of the registered context and cannot escape in source.
  -- Lean's own delta expansion closes over these definitions; do not rewrite
  -- printed private names or add temporary declarations to the target context.
  let portable ← closedProp (← deltaExpand e (fun name => !registeredEnv.contains name))
  for name in portable.getUsedConstants do
    unless registeredEnv.contains name do
      throwError "generated proposition retains a temporary declaration: {name}"
  unless ← withOptions (fun o => o.setBool `smartUnfolding false) (withTransparency .all (isDefEq e portable)) do
    throwError "closing temporary declarations changed the generated proposition"
  let text ← withOptions (fun o => o.setBool `pp.all true |>.setBool `pp.universes true) do
    return (← ppExpr portable).pretty
  -- Re-elaborate using only the registered context, so a temporary matcher
  -- accidentally retained by a printer can never pass the round-trip check.
  withEnv registeredEnv do
    let roundtrip ← closedProp (← Term.elabTermEnsuringType (← parse `term text) (mkSort .zero))
    unless ← isDefEq portable roundtrip do
      throwError "generated proposition changed meaning on re-elaboration: {text} (read back as {roundtrip})"
  return text

-- Only syntax-level leading forall introduces condition parameters. An arrow
-- (including a reducible expression whose value is a forall) stays in P.
private partial def withDomain (ty : Syntax) (xs : Array Expr)
    (k : Array Expr → Syntax → TermElabM Json) : TermElabM Json := do
  match ty with
  | `(($body)) => withDomain body xs k
  | `(forall $bs* : $t, $body) =>
    let bs ← bs.mapM fun b => liftMacroM <| Term.expandSimpleBinderWithType t b
    Term.elabBinders bs fun ys => withDomain body (xs ++ ys) k
  | `(forall $bs*, $body) =>
    Term.elabBinders bs fun ys => withDomain body (xs ++ ys) k
  | _ => k xs ty

private def derive (input : Json) : TermElabM Json := do
  let field (key : String) : TermElabM String :=
    match input.getObjValAs? String key with
    | .ok value => pure value
    | .error e => throwError "{e}"
  let registeredEnv ← getEnv
  let source ← field "statement"
  let expectedName ← field "name"
  -- The accepted FILESPEC slice ends at the proof boundary. A dummy body is
  -- supplied solely to Lean's parser; it is never elaborated.
  let declaration ← parse `command (source ++ "\n  sorry")
  let (declId, binders, ty) ← match declaration with
    | `(command| theorem $id:declId $bs* : $ty := $_) => pure (id, bs, ty)
    | _ => throwError "expected one theorem signature at the FILESPEC proof boundary"
  unless declId.raw[0].getId.toString == expectedName do
    throwError "theorem name does not match registered target"
  unless declId.raw[1].isNone do
    throwError "explicit declaration universes are not supported for Decide statements"
  Term.elabBinders binders fun xs => do
    let original ← Term.elabTermEnsuringType ty (mkSort .zero)
    let target ← closedProp (← mkForallFVars xs original)
    let negative ← render registeredEnv (mkApp (mkConst ``Not) target)
    let mut fields := [("refutation_type", toJson negative)]
    if let .ok condition := input.getObjValAs? String "condition" then
      let obligations ← withDomain ty xs fun domain body => do
        let proposition ← Term.elabTermEnsuringType body (mkSort .zero)
        let reconstructed ← closedProp (← mkForallFVars domain proposition)
        unless ← isDefEq target reconstructed do
          throwError "condition binder context does not preserve the registered target type"
        let condition ← Term.elabTermEnsuringType (← parse `term condition) (mkSort .zero)
        Term.synthesizeSyntheticMVarsNoPostponing
        let condition ← instantiateMVars condition
        -- Preserve the existing nontrivial-condition policy using elaborated
        -- syntax, leaving substantive relevance/non-vacuity to correspondence.
        if condition.isConstOf ``True || condition.isConstOf ``False then
          throwError "conditional restriction must not be True or False"
        if (condition.isAppOfArity ``Eq 3 || condition.isAppOfArity ``Iff 2)
            && condition.getAppArgs[condition.getAppNumArgs - 2]! == condition.getAppArgs.back! then
          throwError "conditional restriction must not be reflexive"
        let proof ← render registeredEnv (← mkForallFVars domain (← mkArrow condition proposition))
        let mut inhabited := condition
        for x in domain.reverse do
          inhabited ← mkAppM ``Exists #[← mkLambdaFVars #[x] inhabited]
        let inhabitedText ← render registeredEnv inhabited
        let mut out := [("proof_type", toJson proof), ("inhabited_type", toJson inhabitedText)]
        if let .ok arguments := input.getObjValAs? (Array String) "arguments" then
          unless arguments.size == domain.size do
            throwError "counterexample argument count differs from registered domain"
          let predicate ← mkLambdaFVars domain condition
          let mut applied := predicate
          for argument in arguments do
            let .forallE _ argType _ _ ← whnf (← inferType applied)
              | throwError "counterexample arguments exceed registered domain"
            let value ← Term.elabTermEnsuringType (← parse `term argument) argType
            applied := (mkApp applied value).headBeta
          out := out ++ [("excluded_type", toJson (← render registeredEnv (mkApp (mkConst ``Not) applied)))]
        return Json.mkObj out
      fields := fields ++ [("conditional", obligations)]
    else
      -- Keep the existing refutation closure restriction. Supporting new
      -- implicit/universe forms is a separate semantics decision.
      for x in xs do
        unless (← x.fvarId!.getDecl).binderInfo == .default do
          throwError "implicit and instance declaration binders are not supported for Decide refutations"
    return Json.mkObj fields

unsafe def main (args : List String) : IO UInt32 := do
  try
    let input ← IO.ofExcept <| Json.parse (args.headD "")
    let context ← IO.ofExcept <| input.getObjValAs? String "context"
    initSearchPath (← findSysroot)
    enableInitializersExecution
    let inputCtx := Parser.mkInputContext context "<registered-context>"
    let (header, parserState, messages) ← Parser.parseHeader inputCtx
    let (env, messages) ← processHeader header {} messages inputCtx
    if messages.hasErrors then
      throw <| IO.userError (String.intercalate "\n" (← messages.toList.mapM fun m => m.data.toString))
    let front ← IO.processCommands inputCtx parserState (Command.mkState env messages {})
    if front.commandState.messages.hasErrors then
      throw <| IO.userError (String.intercalate "\n" (← front.commandState.messages.toList.mapM fun m => m.data.toString))
    let action := Command.runTermElabM fun _ => derive input
    let ctx : Command.Context := { fileName := inputCtx.fileName, fileMap := inputCtx.fileMap, snap? := none, cancelTk? := none }
    match ← (action ctx).run front.commandState |>.toIO' with
    | .error e => throw <| IO.userError (← e.toMessageData.toString)
    | .ok (out, state) =>
      if state.messages.hasErrors then
        throw <| IO.userError (String.intercalate "\n" (← state.messages.toList.mapM fun m => m.data.toString))
      IO.println <| (Json.mkObj [("status", toJson "ok"), ("result", out)]).compress
      return (0 : UInt32)
  catch e =>
    IO.println <| (Json.mkObj [("status", toJson "invalid_statement"), ("error", toJson e.toString)]).compress
    return (1 : UInt32)
