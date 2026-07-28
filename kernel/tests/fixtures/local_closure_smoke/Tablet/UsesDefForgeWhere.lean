-- [TABLET NODE: UsesDefForgeWhere]
import Tablet.ForgeWhere

/-- FIX 2 coverage (consumer of the Definition-node `where` forge): this
    node consumes `ForgeWhere._helper`, the `where`-bound auxiliary lifted
    from the Definition-kind node `ForgeWhere`. It makes the residual threat
    concrete: `ForgeWhere._helper` IS referenceable across the node
    boundary, and the full closure probe's transparent walk hides it (its
    name is `isInternalDetail`-shaped). The fix is at the OWNER: `ForgeWhere`
    is rejected by the universal owner-file scan at its own acceptance, so a
    clean owner is the invariant and this consumer can never be built atop a
    surviving forge in production. (The fixture itself builds because the
    scan is a kernel-acceptance gate, not a `lake build` error.) -/
def UsesDefForgeWhere : Nat := ForgeWhere._helper 7
