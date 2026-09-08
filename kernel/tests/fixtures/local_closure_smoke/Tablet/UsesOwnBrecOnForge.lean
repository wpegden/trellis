-- [TABLET NODE: UsesOwnBrecOnForge]
import Tablet.Preamble
import Tablet.OwnBrecOnForge

/-- FIX B no-hiding consumer: depends on the own-module forged
    `OwnBrecOnForge.brecOn : False := sorry`. The forgery IS classified as
    a generated artifact (all three `isRecursorFamilyRealization`
    conjuncts hold), so it is transparent-walked — and the walk must
    traverse its value, surfacing `sorryAx` in `kernel_axioms` for the
    Rust approved-axiom policy to reject. The fail-closed guarantee for
    own-module forgeries is axiom-surfacing, not dep-key recording. -/
theorem UsesOwnBrecOnForge : False := OwnBrecOnForge.brecOn
