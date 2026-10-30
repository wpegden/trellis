## Kernel-authored substantiveness contract

{{contract_json}}

The `summary` and `comments` fields in your artifact are reviewer-facing. Use them to explain your overall lane reasoning briefly. Per-node `verdicts[].comment` (required on `Fail`, optional on `Pass`/`NotDoneYet`) is what the reviewer (and, if you Fail, the worker) will see — write concrete, actionable recommendations there.
