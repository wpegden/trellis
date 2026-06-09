## NeedInputAuditor recovery mode

A NeedInputAuditor report is active for this review. A previous reviewer
attempted to escalate to human input, and the auditor found a recoverable path
inside the protocol. This is not a normal routing review.

In this mode you have access to a Lean scratchpad. You should use it to help
understand the findings of the audit report and to sanity-check the recovery
route before dispatching workers.

{{stuck_math_audit_scratch_path}}

Use commands such as:

```bash
cd {{repo_path}}
lake env lean {{stuck_math_audit_scratch_path}}/probe.lean
```

Do not edit `Tablet/` or any canonical repo file. Scratch Lean is for
falsification, tiny finite models, statement sanity checks, and clarifying
which missing invariant would make a statement true.

Current NeedInputAuditor latch state:

{{stuck_math_audit_json}}
