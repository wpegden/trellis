## Scratch workspace

You can keep notes at `{{scratch_workspace_path}}` (writable; repo files are read-only).

To pass a note to a worker, name its repo-relative `.trellis/scratch/...` file in the reviewer comments. Referenced files are copied to that worker and retained when its context is fresh.
