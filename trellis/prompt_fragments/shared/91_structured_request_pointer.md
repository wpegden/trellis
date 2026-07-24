## Full structured request file

The kernel-authored JSON blocks above are the prompt-rendered view of your request. Long arrays and large objects may be truncated:

- Truncated arrays end with a string element `"... [truncated; N more items in full request file]"`.
- Truncated objects gain a `"__truncated__"` key with the same hint.

The complete structured request — every field, every list, every object key in full — is on disk at:

`{{structured_request_path}}`

Read it directly when you need an item that was truncated, or when you want fields the prompt summary does not surface. Role-neutral entry points:

```
jq 'keys' {{structured_request_path}}
jq '. | with_entries(.value |= length)' {{structured_request_path}}
```

The first lists every top-level field (your request's full schema). The second prints the size of each top-level value, which is a quick way to spot which fields were truncated above.

Once you know the field you want, drill in normally — e.g. `jq '.<field>' {{structured_request_path}}`. The on-disk file is authoritative. Do not modify it.

### On-disk schema vs the prompt-rendered request_summary

The prompt's `request_summary` block above is a **purpose-built abbreviation** for the renderer. Its top-level keys (`phase`, `nodes`, `targets`, `blocked_targets`, `scenario`, ...) do **not** match the on-disk file's top-level keys, which use the full `WrapperRequest` schema. If you `jq` against `{{structured_request_path}}` expecting `request_summary` keys, you will get empty results. Use the on-disk names below:

| Prompt `request_summary` key | On-disk `WrapperRequest` key(s) |
| --- | --- |
| `phase` | `phase` (same) |
| `nodes` (correspondence) | `corr_verify_nodes` (also `verify_nodes`, denormalized superset) |
| `nodes` (substantiveness) | `substantiveness_verify_nodes` |
| `targets` (paper) | `paper_verify_targets` (also `verify_targets`, denormalized superset) |
| `blocked_targets` | `blocked_targets` (same) |
| `scenario` | (synthetic; not in on-disk file) |
| (no abbrev. for lane bindings) | `corr_verify_lane_bindings`, `paper_verify_lane_bindings`, `sound_verify_lane_bindings` |
| (no abbrev. for verifier-lane set) | `verify_lanes` |

When you want the authoritative verifier frontier for your lane, query the on-disk keys directly:

```
jq '.corr_verify_nodes' {{structured_request_path}}        # correspondence frontier
jq '.corr_verify_lane_bindings' {{structured_request_path}} # which lane covers which subset
jq '.substantiveness_verify_nodes' {{structured_request_path}} # per-node Paper / Substantiveness frontier
jq '.paper_verify_targets' {{structured_request_path}}     # target-package Paper frontier
jq '.sound_verify_node' {{structured_request_path}}        # current Soundness focus
```

The prompt's `request_summary` is sufficient for the common case (which nodes / targets is this verifier judging?); reach for the on-disk schema only when you need lane bindings or the broader request context.
