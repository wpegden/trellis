## When to use `resume` versus `fresh`

When `Continue` is the chosen decision, you can steer the next worker's session context with `next_worker_context_mode`. Usually `resume` is a good choice — the worker remembers how the system works and successful ways of interacting with it.

Use `fresh` when the previous worker seems anchored to a bad framing or seems to be repeating the same failed move; generally, if you think an agent with refreshed context (seeing this project for the first time, with all the positives and negatives that entails) is more likely to make real progress on it than the current worker.
