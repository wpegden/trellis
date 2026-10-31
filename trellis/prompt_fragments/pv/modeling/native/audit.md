Assess the modeling completion conditions against the source, statements, and dependencies. Handoff findings are diagnostic evidence; verification scope and current approvals are determined by the shared kernel. Identify any unmet modeling condition in the current artifacts.

For final completion, inspect `.trellis/scratch/modeling-completion-check.json` and the associated `.trellis/scratch/provenance-*/provenance.json` reports alongside the current source, manifest, and model support. The supervisor runs the full tablet check, including native extraction provenance and model composition, before requesting the final audit and again before accepting completion. These reports provide diagnostic evidence; they do not replace your semantic assessment or the kernel's current approvals. Route and preparation-reopening audits can occur before a completion report exists.

The audit sandbox is read-only for authored artifacts and has no checker socket. The manifest's `provenance_check_command` and full-tablet checker commands require the authoring or supervisor environment; do not run them from this audit. If the available evidence leaves a concrete provenance or composition concern unresolved, return `modeling_revise` with that concern so the reviewer can commission the appropriate check or repair through the existing workflow. A fresh extraction check is available through that workflow when needed.

Return modeling_accept when the completion conditions hold, or modeling_revise with the unmet condition and supporting evidence. Distinguish authored-node repairs from defects requiring preparation to reopen. Put the assessment in scratchpad_replace; new_tasks and task_modifications are empty arrays.

{{contract_json}}

For reopening preparation, assess whether the current extraction basis must be abandoned and whether the proposed repair is adequate. Approval ends this runtime. The managed native launcher returns to ordinary preparation with the candidate and accepted decision retained, then bootstraps a fresh runtime after the required reviews. Earlier artifacts remain read-only references and carry no artifact verification approvals.

Check consequential scope and authoring decisions against the recorded route approvals in the structured request and preparation manifest. If a consequential choice escaped review, return modeling_revise and require modeling-route review before completion.
