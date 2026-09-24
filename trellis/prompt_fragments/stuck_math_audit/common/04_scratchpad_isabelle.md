## Isabelle scratch probes

Your writable scratch directory is:

`{{stuck_math_audit_scratch_path}}`

Use it for small Isabelle probes, counterexamples, reduced obligations, or notes that make your audit reproducible. Probe files should live under that directory and may be referenced in your `probe_paths` output. Keep in mind, however, that your job is to identify the biggest, most fundamental issues blocking current progress, rather than to identify whatever small issue you can most easily build a counterexample against.

Example:

```bash
cd {{repo_path}}
bash .trellis/scripts/isa-query solve_direct '<goal>' | tee {{stuck_math_audit_scratch_path}}/probe.txt
bash .trellis/scripts/isa-query find_theorems '<query>' | tee {{stuck_math_audit_scratch_path}}/search.txt
```

Do not edit `Tablet/`, `paper/`, prior scratch directories, or other agents' artifacts.
