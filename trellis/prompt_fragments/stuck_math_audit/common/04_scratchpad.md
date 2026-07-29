## Lean scratch probes

Your writable scratch directory is:

`{{stuck_math_audit_scratch_path}}`

Use it for small Lean probes, counterexamples, reduced obligations, or notes that make your audit reproducible. Probe files should live under that directory and may be referenced in your `probe_paths` output. Keep in mind, however, that your job is to identify the biggest, most fundamental issues blocking current progress, rather than to identify whatever small issue you can most easily build a counterexample against.

Example:

```bash
cd {{repo_path}}
cat > {{stuck_math_audit_scratch_path}}/probe.lean <<'LEAN'
import Tablet.Preamble

-- Minimal probe for the claim under audit.
LEAN
lake env lean {{stuck_math_audit_scratch_path}}/probe.lean
```

Do not edit `Tablet/`, `paper/`, prior scratch directories, or other agents' artifacts.
