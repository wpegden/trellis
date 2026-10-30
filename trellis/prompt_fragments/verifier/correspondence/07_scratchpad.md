## Correspondence scratch workspace

Write Lean probes for correspondence questions in the writable request-local scratch directory. Keep canonical files read-only.

```bash
cd {{repo_path}}
.trellis/scripts/check-job scratch {{correspondence_scratch_path}}/probe.lean
```

If Lake reports a package URL change or deletion failure, report the diagnostic in `$HOME/.trellis/git-preflight/` and label cached-artifact probes advisory until the required current-source verification succeeds.
