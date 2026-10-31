## Kernel-authored artifact delivery contract

{{artifact_delivery_json}}

Write your best truthful JSON outcome permitted by the role contract to `raw_output_path`. Backslashes inside JSON strings must be escaped. Do not print the JSON to stdout.

Use an optional `system_feedback` string for a tooling or process defect requiring operator repair. Feedback is recorded privately for the operator and may halt the supervisor when configured. Keep process-visible findings in the ordinary response fields.

Run `submit_command` and fix any errors it reports. When it reports Submitted, end your turn. The host checks your submission. Success finishes the attempt. If correction is allowed, you receive diagnostics in this conversation and can fix and submit again within your remaining allowance.
