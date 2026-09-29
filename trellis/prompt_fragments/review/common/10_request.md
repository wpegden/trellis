## Kernel-authored reviewer request

{{request_summary_json}}

When `retry_outcome_kind` is not `None`, this review was reached after a non-success worker attempt. Use that retry context to decide whether to allow another try, request human input, or revert to the last accepted checkpoint if that reset is allowed.
