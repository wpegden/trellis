## Authoritative deterministic worker rejection

If this review follows an invalid worker attempt, the JSON below is the kernel-authored summary of why the last worker attempt was deterministically rejected.

Use these reasons before trusting the worker's own summary of what happened. When deciding the next step, distinguish:

- a bad attempt under the same scope that deserves another try
- a scope problem that needs a different legal routing move, if the current contract allows one
- a live state that should be reverted

{{deterministic_worker_rejection_reasons_json}}

{{deterministic_worker_rejection_artifacts_text}}
