# local_closure_smoke fixture

Synthetic Tablet for the Patch A local-closure probe (plan §5.9 Tier 3).

## Why this is gated `#[ignore]`

The fixture's `.lean` files import a Mathlib symbol (`True.intro`-equivalent
trivial proofs are fine, but the boundary-helper test relies on the same
`isTabletConst` filter the real probe uses, which inspects the elaborated
environment). Building this requires the operator to:

1. Run `lake exe cache get` to download the Mathlib oleans for the pinned
   toolchain (or vendor them).
2. Run `lake build` to compile `Tablet.Helper`, `Tablet.Closed`,
   `Tablet.UsesHelper`, and `Tablet.ActiveSorry`.
3. Stand up a CheckerServer pointed at this fixture's runtime root.
4. Set `TRELLIS_CHECKER_SOCKET` and run
   `cargo test -p trellis-kernel local_closure_smoke -- --ignored`
   from the kernel crate.

The cargo tests live in `kernel/tests/local_closure_smoke.rs`.

## Files

- `lakefile.lean` — minimal lake config; pins to the same toolchain the
  live runs use.
- `lean-toolchain` — pinned toolchain.
- `Tablet/Preamble.lean` — empty preamble (no Mathlib imports needed for
  our trivial proofs).
- `Tablet/Helper.lean` — open helper (`theorem Helper : True := by sorry`).
- `Tablet/Closed.lean` — sorry-free, no Tablet deps (`theorem Closed : True := trivial`).
- `Tablet/UsesHelper.lean` — `import Tablet.Helper`; proof leans on `Helper`
  by name. Demonstrates the boundary-cut semantics: even though `Helper`
  carries `sorryAx`, the local-closure probe should report `kernel_axioms`
  as a subset of the canonical four (because we hit `Helper` under
  `ProofMayAssumeTheorems` and stop at its statement).
- `Tablet/ActiveSorry.lean` — sorry-free at the byte level NOT — has an
  active `sorry`. Probe should report `sorryAx` in `kernel_axioms` and a
  rejection from the gate.
- `Tablet/ReservedArtifactDef.lean` — authored definition whose reserved
  generated theorem `ReservedArtifactDef.congr_simp` is forced by the next
  fixture.
- `Tablet/UsesReservedArtifact.lean` — explicitly references
  `ReservedArtifactDef.congr_simp`. The probe must transparent-walk that
  generated theorem, record `ReservedArtifactDef` as a strict definition dep,
  and never record the generated theorem as a boundary node.

## Test expectations (per plan §5.9 Tier 3)

| Node             | Expected `status` | Expected `kernel_axioms`          | Expected `boundary_theorems` |
|------------------|-------------------|------------------------------------|------------------------------|
| `Closed`         | `ok`              | ⊆ `{Classical.choice, propext, ...}` | empty                        |
| `UsesHelper`     | `ok`              | ⊆ canonical four                  | `{Helper}`                   |
| `UsesReservedArtifact` | `ok`         | ⊆ canonical four                  | no `*.congr_simp` entry      |
| `ActiveSorry`    | `ok`              | ∋ `sorryAx`                       | empty                        |
| (fictitious)     | `ok` w/ errors    | empty                              | empty                        |

The fictitious-name case verifies the wrapper's fail-closed diagnostic when
the script can't find the requested declaration.
