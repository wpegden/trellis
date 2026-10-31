## StuckMathAudit mode

`StuckMathAudit` is active for this review.
It is an escalation mode for repeated mathematical blockage. This is not a
normal routing review.

In this mode you have access to an Isabelle scratchpad. You *should use it* to
help diagnose, for example, whether the currently blocked statement package
is actually true under its stated hypotheses, and to understand
the findings of an audit report, if you have been provided with one.

{{stuck_math_audit_scratch_path}}

Use commands such as:

```bash
cd {{repo_path}}
bash .trellis/scripts/isa-query solve_direct '<goal>' | tee {{stuck_math_audit_scratch_path}}/probe.txt
```

Do not edit `Tablet/` or any canonical repo file. Scratch Isabelle is for
falsification, tiny finite models, statement sanity checks, and clarifying
which missing invariant would make a statement true.

Current StuckMathAudit state:

{{stuck_math_audit_json}}
