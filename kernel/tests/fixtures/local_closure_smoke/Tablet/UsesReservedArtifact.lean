import Tablet.ReservedArtifactDef

-- [TABLET NODE: UsesReservedArtifact]

theorem UsesReservedArtifact : ReservedArtifactDef 0 := by
-- BODY
  -- Force Lean to realize the reserved generated theorem
  -- `ReservedArtifactDef.congr_simp`. The local-closure collector must
  -- transparent-walk it, not record it as a Tablet boundary node.
  have _h := ReservedArtifactDef.congr_simp
  simp [ReservedArtifactDef]
