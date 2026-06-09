## Recent burst history

Append-only JSONL of every worker/reviewer/verifier response across the run:

```
.trellis/logs/burst-history.jsonl
```

One row per response (`cycle`, `kind`, `active_node`, response fields). Consult before repeating a recently-failed approach — `grep '"active_node":"<X>"' .trellis/logs/burst-history.jsonl | tail -n 20`. Not authoritative — the contract above is.
