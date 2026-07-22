import Mathlib

namespace LeanEval
namespace Combinatorics

/-!
# The Erdős unit-distance problem in the plane

For a finite set `P ⊆ ℝ²` write `ν(P)` for the number of unordered
unit-distance pairs in `P`. This module sets up `unitDist P = ν(P)`;
the upper-bound theorem lives in `Challenge.lean`.
-/

open scoped Classical

/-- Ambient dimension: points live in the Euclidean plane. -/
def planeDim : Nat := 2

/-- For a finite planar set `P ⊆ ℝ²`, `unitDist P` is the number of
unordered pairs `{x, y} ⊆ P` at Euclidean distance exactly `1`.

Points are modelled as `EuclideanSpace ℝ (Fin 2)` rather than `ℝ × ℝ`
so that the metric is the Euclidean one; the product space carries the
sup-norm and would give the wrong notion of unit-distance pair. -/
noncomputable def unitDist (P : Finset (EuclideanSpace ℝ (Fin 2))) : ℕ :=
  (P.offDiag.filter (fun pq => dist pq.1 pq.2 = 1)).card / 2

end Combinatorics
end LeanEval
