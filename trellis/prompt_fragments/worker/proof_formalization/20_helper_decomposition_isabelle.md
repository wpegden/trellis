## Helper-node decomposition

If direct closure inside one file is becoming brittle or opaque, prefer meaningful helper decomposition over flailing inside one oversized proof. A node's whole `.thy` re-checks on every build, so once it grows large each iteration is slow; peeling self-contained, already-closed parts into helper nodes keeps builds fast — each helper checks and caches once while the active node you iterate on stays small.

New or changed helper nodes will still need to satisfy the project's substantiveness, statement-TeX correspondence, and NL soundness invariants, but importantly, soundness is waived for nodes that are already proved (no `sorry`). Note that adding helper nodes is legal even under `scope_contract.allow_new_obligations=false` so long as the new helpers are proved (no `sorry`). In particular, consider decomposing into already-proved helper nodes (following the file spec) when encountering performance or complexity issues in Isabelle builds.
