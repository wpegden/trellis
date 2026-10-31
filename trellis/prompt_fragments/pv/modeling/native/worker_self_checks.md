Before returning `valid`, check the edited statements against correspondence, goal-faithfulness, and substantiveness, and check preservation arguments against soundness. Model definitions must be complete. Unfinished arguments use `SKETCH:`; unfinished Lean proofs use `sorry`. Follow the shared target-support, scope, and file-format contract.

Use `check.py tablet` for structure and elaboration checks. `check.py node` additionally requires proof closure and rejects compiler `sorry` warnings, including those from dependencies.
