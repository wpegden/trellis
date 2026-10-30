import Lean

/-!
Diagnostic axiom inventory for native preparation, including partial elaboration.
Uses the ordinary Lean frontend and Lean.collectAxioms. It issues no certificates
and changes neither the closure checker nor its theorem boundaries.

Declarations created by an erroneous command, synthetic recovery declarations,
and their local dependents are pending, never positive compatibility evidence.
Imported declarations are examined only through dependencies of local constants.
-/
open Lean Lean.Elab

private def localConstants (env : Environment) : Array (Name × ConstantInfo) :=
  env.checked.get.constants.foldStage2 (fun xs n c => xs.push (n, c)) #[]

private def dependencies (ci : ConstantInfo) : Array Name := Id.run do
  let mut names := ci.type.getUsedConstants
  if let some value := ci.value? (allowOpaque := true) then
    names := names ++ value.getUsedConstants
  if let .inductInfo info := ci then
    names := names ++ info.ctors.toArray
  return names

private def synthetic (ci : ConstantInfo) : Bool :=
  ci.type.hasSyntheticSorry || (ci.value? (allowOpaque := true)).any Expr.hasSyntheticSorry

private def inspect (source moduleName : String) : IO Json := do
  let input ← IO.FS.readFile source
  let inputCtx := Parser.mkInputContext input source
  let opts := ({} : Options).setBool `Elab.async false |>.setBool `autoImplicit false
  let (header, parserState, messages) ← Parser.parseHeader inputCtx
  let (env, messages) ← processHeader header opts messages inputCtx
    (mainModule := moduleName.toName)
  if messages.hasErrors then
    for message in messages.toList do
      IO.eprintln (← message.toString)
    throw <| IO.userError "Cannot inspect axioms: module header/imports failed"
  let mut state : Frontend.State := {
    commandState := Command.mkState env messages opts
    parserState, cmdPos := parserState.pos }
  let mut seen : NameSet := {}
  let mut pending : NameSet := {}
  let mut locations : NameMap Nat := {}
  let mut errorLines : Array Nat := #[]
  repeat
    -- Per-command messages identify failed commands without modifying normal
    -- elaboration recovery. Disabling async ensures each command has finished.
    state := { state with commandState.messages := {} }
    let (done, next) ← (Frontend.processCommand.run { inputCtx }).run state
    state := next
    let pos := state.commands.back!.getPos?.getD state.cmdPos
    let line := (inputCtx.fileMap.toPosition pos).line
    let failed := state.commandState.messages.hasErrors
    if failed then errorLines := errorLines.push line
    for entry in localConstants state.commandState.env do
      let (name, ci) := entry
      if !seen.contains name then
        seen := seen.insert name
        locations := locations.insert name line
        if failed || synthetic ci then pending := pending.insert name
    if done then break
  let env := state.commandState.env
  let constants := (localConstants env).qsort (fun a b => Name.lt a.1 b.1)
  -- A later error-free command can refer to a recovered earlier declaration.
  -- Propagate uncertainty through local type/value/inductive dependencies.
  repeat
    let mut changed := false
    for (name, ci) in constants do
      if !pending.contains name && (dependencies ci).any pending.contains then
        pending := pending.insert name
        changed := true
    unless changed do break
  let action : CoreM (Array Json) := do
    constants.mapM fun (name, _) => do
      let unresolved := pending.contains name
      let axioms ← if unresolved then pure #[] else Lean.collectAxioms name
      return Json.mkObj [
        ("name", toJson name.toString),
        ("line", toJson ((locations.find? name).getD 0)),
        ("pending", toJson unresolved),
        ("axioms", toJson (axioms.map Name.toString))]
  let (rows, _) ← action.toIO { fileName := source, fileMap := inputCtx.fileMap } { env }
  return Json.mkObj [
    ("schema", toJson "trellis-preparation-axioms/v1"),
    ("module", toJson moduleName),
    ("complete", toJson (errorLines.isEmpty && pending.isEmpty)),
    ("error_lines", toJson errorLines),
    ("declarations", toJson rows)]

unsafe def main (args : List String) : IO UInt32 := do
  match args with
  | [source, moduleName, output] =>
    initSearchPath (← findSysroot)
    enableInitializersExecution
    try
      -- Like the ordinary frontend, elaborate on a Lean task thread. Running
      -- directly inside `lean --run` uses the smaller process-main stack and
      -- can overflow on source the ordinary frontend handles successfully.
      -- Command elaboration stays synchronous for precise error attribution.
      let task ← (inspect source moduleName).asTask
      IO.FS.writeFile output (← IO.ofExcept task.get).compress
      return 0
    catch e =>
      IO.eprintln e.toString
      return 2
  | _ =>
    IO.eprintln "Usage: lean --run lean_preparation_axioms.lean SOURCE MODULE OUTPUT"
    return 2
