## Helper-node decomposition

If direct closure inside one file is becoming brittle or opaque, prefer meaningful helper decomposition over flailing inside one oversized proof.

New or changed helper nodes will still need to satisfy the project's substantiveness, Lean-TeX correspondence, and NL soundness invariants, but importantly, soundness is waived for nodes that are already Lean-closed. Note that adding helper nodes is legal even under `scope_contract.allow_new_obligations=false` so long as the new helpers are Lean-closed. In particular, consider decomposing into already-closed helper nodes (following FILESPEC.md) when encountering heartbeat/elaboration complexity issues in lean builds.
