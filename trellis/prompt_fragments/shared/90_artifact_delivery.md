## Kernel-authored artifact delivery contract

{{artifact_delivery_json}}

Write valid JSON to `{{raw_output_path}}`. Backslashes inside JSON strings must be escaped.

Your task is not finished until you have written the best truthful raw artifact you can and then written the done marker as the final step.

The raw JSON may honestly report whatever outcome or decision is allowed by the current kernel-authored contract and the repository state.

If the task/setup seems impossible, inconsistent, or poorly supported at the system or tooling level, you may additionally include a short `system_feedback` string in the JSON artifact. The supervisor appends that field to a private host-side log that other agents cannot read. Use `system_feedback` for testing/debugging notes about prompts, tooling, or workflow. Operators will read it later. Do not use it in place of normal process-visible fields like `summary` or `comments`.

{{acceptance_check_block}}

At a bare minimum, finish with raw JSON that passes raw artifact validator:

`{{json_check_command}}`

Do not write the done marker until this raw JSON validator passes. Agents should always be willing to satisfy this minimum requirement; otherwise the system stalls.

Write the done marker file `{{done_path}}` only after the raw artifact is final and the raw JSON validator is passing. The done marker is always the last step.

Do not print the JSON to stdout.
