## Worker Scratch Workspace

A repo-local worker scratch workspace is available at `{{scratch_workspace_path}}`.

Scratch workspace status for this request: {{scratch_workspace_status_text}}.

The scaffold files are:
- `{{scratch_readme_path}}`
- `{{scratch_notes_path}}`
- `{{scratch_example_path}}`

Use this workspace for temporary notes, Lean experiments, and one-off helper files. It is not canonical tablet state and does not change the kernel-authored request, contract, or checker. Scratch Lean under `Tablet/` would cause rejection, as all Lean under `Tablet` must live in nodes.
