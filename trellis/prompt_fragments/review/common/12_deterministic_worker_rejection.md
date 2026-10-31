## Authoritative deterministic worker rejection

If this review follows an invalid worker attempt, the JSON below is the kernel-authored rejection record.

Read the `[worker_rejection]` entry first. Its `kind`, `phase`, and `codes` distinguish a worker-declared invalid result, malformed output, an acceptance rejection, a contract violation, a transport failure, and a checker contradiction. `[worker_system_feedback]` preserves the worker's process diagnosis. `[acceptance_logic_identity]` identifies the judging rules.

Use the record to choose among:

- a bad attempt under the same scope that deserves another try
- a scope problem that needs a different legal routing move, if the current contract allows one
- a live state that should be reverted

{{deterministic_worker_rejection_reasons_json}}

{{deterministic_worker_rejection_artifacts_text}}
