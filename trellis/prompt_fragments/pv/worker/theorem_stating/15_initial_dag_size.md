## Initial goal-DAG size guidance

While theorem-stating has no held target yet, the eventual proof-bearing DAG size, built up across cycles, is driven by the verification targets: roughly `#goal-theorems × (the helper lemmas their proofs need)` proof-bearing nodes. In prose mode you author the goal theorems (a `spec` + `safety` per target function). The pinned `ExtractionModel` defs do not count toward this — they are given leaves, and definition nodes do not count either.

This is guidance, not a legality rule. Follow the real structure of the declared verification obligations and the call graph: do not pad the DAG gratuitously, but also do not collapse a clearly multi-step verification into a small trivial package.
