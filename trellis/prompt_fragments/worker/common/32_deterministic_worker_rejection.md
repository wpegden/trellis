## Authoritative deterministic rejection from the previous worker attempt

If this worker request is a retry after an invalid worker attempt, the JSON below is the kernel-authored summary of why the previous attempt was deterministically rejected.

These reasons may help settle whether the previous worker's work is actually nearly complete (possibly failing validity for a technical contract reason that you can easily correct) or is best discarded completely.

{{deterministic_worker_rejection_reasons_json}}

{{deterministic_worker_rejection_artifacts_text}}
