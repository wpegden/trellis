# local_closure_smoke fixture

Synthetic Tablet for the Patch A local-closure probe (plan §5.9 Tier 3).

## Building it

`.lake/` is gitignored, so a fresh checkout has no `.olean` tree and the
tests that probe this fixture cannot run. Build it once — the fixture
deliberately avoids Mathlib, so no `lake exe cache get` is needed and only
stdlib oleans are required:

```
(cd kernel/tests/fixtures/local_closure_smoke && lake build)
```

The cargo tests live in `kernel/tests/local_closure_smoke.rs` (guard-checked:
they run on every `cargo test`) and `kernel/tests/semantic_fingerprint_smoke.rs`
(`#[ignore]`d: they mutate a `Tablet/*.lean` and rebuild). The guard-checked
ones are the regression harness for two resolved local-closure soundness holes,
so with the fixture unbuilt they FAIL rather than pass vacuously. A lane with
no Lean toolchain opts out explicitly:

```
TRELLIS_ALLOW_FIXTURE_SKIP=1 cargo test -p trellis-kernel --test local_closure_smoke
```

`scripts/run_local_closure_smoke_tests.sh` does the build plus the `--ignored`
lane in one step.

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
