## Loogle helper

Use the local Loogle server helper to help with lean work.

Helper path: `{{loogle_helper_path}}`

One query at a time; Do not launch many Loogle queries in parallel.

Examples:

```bash
bash {{loogle_helper_path}} "Submodule.span"
bash {{loogle_helper_path}} "Nat.choose"
bash {{loogle_helper_path}} "Real.exp_neg"
```

Cold or broad queries can take several seconds. Waiting on Loogle is fine. There is a built-in timeout of 60 seconds; you can simply wait for the command to return or timeout.

If the helper still seems unavailable after a reasonable retry, fall back to direct Lean scratch checks or repository grep rather than treating Loogle as mandatory.
