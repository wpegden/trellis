## Target orientation (theorem-stating)

The supervisor's goal is proving the configured paper target. Theorem-stating builds the target-support DAG that proof-formalization will later close. Your job here is to keep that DAG paper-faithful and well-decomposed before any proofs are seriously attempted.

In theorem-stating, broader theorem work — including creating new helper nodes and editing the signatures of existing nodes — happens by re-issuing the worker in `next_mode: Global` with explicit comments naming the structural change you want. `Global` authorizes broad theorem-stating edits: new helpers, signature edits, dependency rewiring. After cycle 1, it still does not authorize new proof-bearing nodes with `SKETCH:` NL proofs; workers must either provide complete NL proofs expected to pass strict soundness verification or avoid creating those nodes.

Pivoting to an unrelated node whose failure is not what's blocking the current focus is almost never the right move. Those nodes will still be there later, and will often be easier once the current statement package is settled.
