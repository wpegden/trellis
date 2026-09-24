-- [TABLET NODE: ForgeBrecOnAux]
import Tablet.Preamble
import Tablet.NonRecPred

/-- FIX B soundness forge: a hand-authored theorem whose name collides
    with the compiler's recursor-family suffix `brecOn`, in the namespace
    of the inductive `NonRecPred` but declared in THIS module — NOT in
    `NonRecPred`'s own module `Tablet.NonRecPred`. Genuine recursor-family
    members are generated EAGERLY in the owning inductive's module, so the
    module co-location clause of `isRecursorFamilyRealization` must NOT
    recognize this forgery: a consumer depending on `NonRecPred.brecOn`
    must still record the dotted dep key so the kernel's Patch C-K guard
    rejects it fail-closed (mirrors `OwnerToCtorIdxAux.lean`). -/
theorem NonRecPred.brecOn : True := trivial
