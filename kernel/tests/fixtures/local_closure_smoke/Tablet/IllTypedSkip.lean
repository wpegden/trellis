-- [TABLET NODE: IllTypedSkip]
import Lean
import Tablet.Preamble

/- P0 regression fixture: elaboration succeeds and writes an `.olean` only
because this declaration-scoped option disables `Lean.addDecl`'s kernel call.
The authoritative acceptance build must reject the resulting artifact during
its independent `leanchecker` replay. -/
set_option debug.skipKernelTC true in
run_cmd
  Lean.Elab.Command.liftCoreM <| Lean.addDecl <| .thmDecl {
    name := `IllTypedSkip
    levelParams := []
    type := .const ``False []
    value := .const ``True.intro []
  }
