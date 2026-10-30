Assess Modeling completion against the source, statements, dependencies and current lane evidence. Inspect `.trellis/scratch/modeling-completion-check.json` and its provenance reports when present. The supervisor runs the full tablet and provenance checks before completion; the audit sandbox has no checker socket.

Return `modeling_accept` when the conditions hold, or `modeling_revise` with the required repair. Record the assessment in `scratchpad_replace`; keep `new_tasks` and `task_modifications` empty. A preparation-rebuild disposition ends this generation and requires fresh preparation.

{{contract_json}}
