## Isabelle library-search helper

Use the local Isabelle query helper to help with formalization work.

The request compatibility field for the search-helper path is `{{loogle_helper_path}}`. On this backend, run the Isabelle query helper as shown below.

One query at a time; do not launch many library queries in parallel.

Examples:

```bash
bash .trellis/scripts/isa-query find_theorems 'name: add_comm'  # {{loogle_helper_path}}
bash .trellis/scripts/isa-query find_consts '"_ list => nat"'  # {{loogle_helper_path}}
bash .trellis/scripts/isa-query solve_direct '(n::nat) + 0 = n'  # {{loogle_helper_path}}
```

Cold or broad queries can take several seconds. Waiting on a query is fine. There is a built-in timeout; you can simply wait for the command to return or time out.

If the helper still seems unavailable after a reasonable retry, fall back to direct Isabelle scratch checks or repository search rather than treating the helper as mandatory.
