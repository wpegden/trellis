## Kernel-authored artifact delivery contract

{{artifact_delivery_json}}

Write your best truthful JSON outcome permitted by the role contract to `raw_output_path`. Backslashes inside JSON strings must be escaped. Do not print the JSON to stdout.

Use an optional `system_feedback` string for a tooling or process defect requiring operator repair. Feedback is recorded privately for the operator and may halt the supervisor when configured. Keep process-visible findings in the ordinary response fields.

Run `json_check_command` and repair the raw artifact until it passes.

{{acceptance_check_block}}

With the artifact final, create `done_path` as the final filesystem change. Completion requires this marker even when the permitted outcome reports unfinished work.
