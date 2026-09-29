## Previous worker attempt

Metadata for the immediately previous worker attempt — including its `outcome`, the worker's own `summary` and `comments`, and any deterministic rejection reasons — is preserved at `{{last_invalid_metadata_path}}`. Read it before retrying so you do not recapitulate the same failure or miss the prior worker's diagnosis.

The previous worker's `Tablet/` working tree at the moment its burst ended — its WIP, whether the burst exited via Invalid, Malformed, Stuck, or NeedsRestructure, or exited Valid and was then rejected by a kernel rule — is preserved as a snapshot at `{{last_invalid_path}}`. The kernel rolls the live `Tablet/` back to the request baseline before issuing this new request, so the snapshot is the only place that WIP survives.{{last_invalid_scratch_handoff_text}}

These artifacts are diagnostic only. They are not the live tablet state, and they do not expand your authority beyond the current kernel-authored request and checker.
