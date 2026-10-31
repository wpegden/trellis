-- [TABLET NODE: UsesForgeBrecOnAux]
import Tablet.Preamble
import Tablet.ForgeBrecOnAux

/-- FIX B soundness consumer: depends on the CROSS-MODULE forged
    `NonRecPred.brecOn` (see `ForgeBrecOnAux.lean`). The recursor-family
    clause's suffix + inductive-parent conjuncts both match the forgery;
    ONLY the module co-location clause keeps it out of the generated-
    artifact classification. The probe must RECORD the dotted
    `NonRecPred.brecOn` dep key (not transparent-walk it away) so the
    Rust C-K guard can reject it as a private auxiliary — mirrors
    `UsesOwnerToCtorIdx.lean`'s fail-OPEN regression pin. -/
theorem UsesForgeBrecOnAux : True := NonRecPred.brecOn
