## Previous worker attempt

Metadata for the immediately previous worker attempt — including its `outcome`, the worker's own `summary` and `comments`, and any deterministic rejection reasons — is preserved at `{{last_invalid_metadata_path}}`. Read it before retrying so you do not recapitulate the same failure or miss the prior worker's diagnosis.

The previous worker's `Tablet/` and `reference/` source trees at the moment its burst ended — its WIP, whether the burst exited via Invalid, Malformed, Stuck, or NeedsRestructure, or exited Valid and was then rejected by a kernel rule — are preserved as a snapshot at `{{last_invalid_path}}`. The kernel restores both live trees to the request baseline before issuing this new request, so the snapshot is the place to recover that work, including supporting evidence. Its metadata records which trees were present; older snapshots may contain only `Tablet/`.{{last_invalid_scratch_handoff_text}}

These artifacts are diagnostic only. They are not the live tablet state, and they do not expand your authority beyond the current kernel-authored request and checker.
