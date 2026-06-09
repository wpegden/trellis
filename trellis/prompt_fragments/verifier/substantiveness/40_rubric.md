## Kernel-authored substantiveness rubric

{{rubric_json}}

### Output schema reminder

Your artifact has the `substantiveness_result_v1` shape:

```
{
  "substantiveness": {
    "decision": "PASS" | "FAIL",
    "verdicts": [
      {"node": "<NodeName>", "verdict": "Pass"},
      {"node": "<NodeName>", "verdict": "Fail", "comment": "concrete next-step recommendation"},
      {"node": "<NodeName>", "verdict": "NotDoneYet"}
    ]
  },
  "overall": "APPROVE" | "REJECT",
  "summary": "lane-level summary",
  "comments": "optional"
}
```

Per-node verdicts:

- `Pass` — you read the node carefully and it satisfies both clauses of the substantiveness rubric.
- `Fail` — you read the node carefully and it fails clause 1 or clause 2. **Must include `comment` with a concrete next-step recommendation.**
- `NotDoneYet` — you did not have time to read the node carefully. Comment is optional.

Every node listed in `request_summary.nodes` should appear in `verdicts[]` with an explicit verdict. **A node omitted from `verdicts[]` is treated as `NotDoneYet` by default** — do not rely on omission to mark Pass.

Set `decision: PASS` iff every verdict is `Pass` or `NotDoneYet`; otherwise `decision: FAIL`. NotDoneYet entries do not by themselves cause a Fail decision.
